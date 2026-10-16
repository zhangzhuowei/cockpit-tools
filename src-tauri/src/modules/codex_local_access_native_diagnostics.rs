// Native management and diagnostics share the host's bounded, redacted persistence.
use crate::modules::codex_native_api::diagnostics::{
    DiagnosticContext as NativeDiagnosticContext, ManagementRequest as NativeManagementRequest,
};

static NATIVE_DIAGNOSTIC_EVENTS: std::sync::LazyLock<
    Mutex<std::collections::VecDeque<CodexLocalAccessRequestDetail>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::VecDeque::new()));
static NATIVE_DIAGNOSTIC_WORKERS: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(4)));
static NATIVE_DIAGNOSTIC_DROPPED: AtomicU64 = AtomicU64::new(0);
static NATIVE_SPEED_REVISION: std::sync::LazyLock<TokioMutex<(u64, String)>> =
    std::sync::LazyLock::new(|| TokioMutex::new((0, String::new())));

pub(crate) fn native_request_payload_logging_enabled() -> bool {
    REQUEST_PAYLOAD_LOGGING_ENABLED.load(Ordering::SeqCst)
}

fn native_diagnostic_detail(
    context: NativeDiagnosticContext,
    capture_payloads: bool,
) -> CodexLocalAccessRequestDetail {
    let mut detail = CodexLocalAccessRequestDetail {
        request_id: context.request_id,
        client_identity_captured: context.client_identity_captured,
        client_session_id: context.client_session_id,
        client_thread_id: context.client_thread_id,
        client_identity_metadata_captured: context.client_identity_metadata_captured,
        client_installation_id: context.client_installation_id,
        client_chatgpt_account_id: context.client_chatgpt_account_id,
        first_response_ms: context.first_response_ms,
        failure_phase: context.failure_phase,
        captured_at_ms: context.started_at_ms,
        truncated: context.truncated
            || context.attempts.len() > REQUEST_DIAGNOSTICS_MAX_ATTEMPTS
            || context.payloads.len() > REQUEST_DIAGNOSTICS_MAX_PAYLOADS,
        attempts: context
            .attempts
            .into_iter()
            .take(REQUEST_DIAGNOSTICS_MAX_ATTEMPTS)
            .map(|item| CodexLocalAccessRequestAttempt {
                sequence: item.sequence,
                upstream_identity_captured: item.upstream_identity_captured,
                upstream_session_id: item.upstream_session_id,
                upstream_thread_id: item.upstream_thread_id,
                upstream_identity_metadata_captured: item.upstream_identity_metadata_captured,
                upstream_installation_id: item.upstream_installation_id,
                upstream_chatgpt_account_id: item.upstream_chatgpt_account_id,
                account_identity: item.account_identity,
                account_id: item.account_id,
                account_email: item.account_email,
                model_id: item.model_id,
                transport: item.transport,
                started_at_ms: item.started_at_ms,
                latency_ms: item.latency_ms,
                status: item.status,
                success: item.success,
                error_category: item.error_category,
                error_message: item.error_message,
                failure_phase: item.failure_phase,
            })
            .collect(),
        payloads: if capture_payloads {
            context
                .payloads
                .into_iter()
                .take(REQUEST_DIAGNOSTICS_MAX_PAYLOADS)
                .filter_map(|item| {
                    let stage = match item.stage.as_str() {
                        "client" | "client_request" => "client",
                        "upstream" | "upstream_request" => "upstream",
                        _ => return None,
                    };
                    // Never mark an unredacted, truncated string as trusted. The host normalizer
                    // parses/redacts complete JSON; other formats and oversized input are omitted.
                    let oversized = item.body.len() > 256 * 1024;
                    Some(CodexLocalAccessRequestPayload {
                        stage: stage.into(),
                        attempt_sequence: item.attempt_sequence,
                        transport: item.transport,
                        content_type: item.content_type,
                        headers: item.headers,
                        body: if oversized {
                            "\"[omitted: input exceeds diagnostic limit]\"".into()
                        } else {
                            item.body
                        },
                        truncated: false,
                        original_bytes: item.original_bytes,
                        sha256: String::new(),
                    })
                })
                .collect()
        } else {
            Vec::new()
        },
    };
    detail = normalize_request_diagnostics(detail);
    detail
}

/// Fire-and-forget only optional diagnostics; usage persistence retains its existing guarantee.
fn native_record_diagnostic(mut context: NativeDiagnosticContext) {
    if context.request_id.trim().is_empty() || context.request_id.len() > 256 {
        return;
    }
    let capture = native_request_payload_logging_enabled();
    if !capture {
        context.payloads.clear();
    }
    let Ok(permit) = NATIVE_DIAGNOSTIC_WORKERS.clone().try_acquire_owned() else {
        NATIVE_DIAGNOSTIC_DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let payload_epoch = REQUEST_DIAGNOSTICS_PAYLOAD_EPOCH.load(Ordering::SeqCst);
    let log_epoch = REQUEST_LOG_WRITE_EPOCH.load(Ordering::SeqCst);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let detail = native_diagnostic_detail(context, capture);
        if log_epoch != REQUEST_LOG_WRITE_EPOCH.load(Ordering::SeqCst)
            || detail.captured_at_ms <= REQUEST_LOG_RECORDS_CLEARED_BEFORE.load(Ordering::SeqCst)
        {
            return;
        }
        queue_request_diagnostics_at_epoch(detail.clone(), payload_epoch, log_epoch);
        if let Ok(mut events) = NATIVE_DIAGNOSTIC_EVENTS.lock() {
            if events.len() >= 32 {
                events.pop_front();
                NATIVE_DIAGNOSTIC_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
            events.push_back(detail);
        }
    });
}

fn native_diagnostic_events() -> Value {
    let events = NATIVE_DIAGNOSTIC_EVENTS
        .lock()
        .map(|mut events| {
            let count = events.len().min(8);
            events
                .drain(..count)
                .filter_map(|mut detail| {
                    if detail.captured_at_ms
                        <= REQUEST_LOG_RECORDS_CLEARED_BEFORE.load(Ordering::SeqCst)
                    {
                        return None;
                    }
                    prepare_request_diagnostic_payloads(
                        &mut detail,
                        native_request_payload_logging_enabled(),
                        REQUEST_DIAGNOSTICS_PAYLOAD_CLEARED_BEFORE.load(Ordering::SeqCst),
                    );
                    Some(detail)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({"events": events, "dropped": NATIVE_DIAGNOSTIC_DROPPED.load(Ordering::Relaxed)})
}

fn native_management_permission(
    caller: &native_contract::Caller,
    request: &NativeManagementRequest,
) -> Result<(), native_contract::ServiceError> {
    if request.requires_internal_key() && caller.id != NATIVE_INTERNAL_CALLER {
        return Err(native_error(
            403,
            "permission_denied",
            "Internal management key required",
        ));
    }
    if matches!(
        request,
        NativeManagementRequest::ServiceTier {
            loopback: false,
            ..
        }
    ) {
        return Err(native_error(
            403,
            "permission_denied",
            "Local management connection required",
        ));
    }
    Ok(())
}

fn native_management_scope(
    collection: &CodexLocalAccessCollection,
    caller: &native_contract::Caller,
) -> Result<Vec<String>, native_contract::ServiceError> {
    let key = native_caller_key(collection, caller)?;
    let mut scope = native_scoped_account_ids(collection, key);
    if caller.id == NATIVE_INTERNAL_CALLER {
        scope.extend(internal_api_account_ids());
    }
    scope.sort();
    scope.dedup();
    Ok(scope)
}

fn native_scoped_reset_ids(
    scope: &[String],
    requested: Vec<String>,
) -> Result<Vec<String>, native_contract::ServiceError> {
    let mut selected = requested
        .into_iter()
        .filter(|id| scope.contains(id))
        .collect::<Vec<_>>();
    selected.sort();
    selected.dedup();
    if selected.is_empty() {
        return Err(native_error(
            404,
            "account_not_found",
            "No matching accounts found",
        ));
    }
    Ok(selected)
}

async fn native_management(
    bridge: &NativeHostBridge,
    caller: &native_contract::Caller,
    request: NativeManagementRequest,
) -> Result<Value, native_contract::ServiceError> {
    native_management_permission(caller, &request)?;
    match request {
        NativeManagementRequest::DiagnosticsConfig { enabled } => {
            // Compatibility ACK for the host's already-persisted setting, not a second writer.
            // Reject stale retries rather than overwrite a newer local preference.
            if enabled != native_request_payload_logging_enabled() {
                return Err(native_error(
                    409,
                    "stale_diagnostic_update",
                    "A newer diagnostic preference is active",
                ));
            }
            Ok(json!({"requestPayloadLogging": enabled}))
        }
        NativeManagementRequest::DiagnosticsEvents => Ok(native_diagnostic_events()),
        NativeManagementRequest::ServiceTier {
            speed, revision, ..
        } => {
            if revision == 0 || !matches!(speed.as_str(), "standard" | "fast") {
                return Err(native_error(
                    400,
                    "invalid_request",
                    "Invalid API service speed",
                ));
            }
            let mut current = NATIVE_SPEED_REVISION.lock().await;
            if revision < current.0 || (revision == current.0 && speed != current.1) {
                return Err(native_error(
                    409,
                    "stale_speed_update",
                    "A newer API service speed is active",
                ));
            }
            let expected = speed.clone();
            let saved = tokio::task::spawn_blocking(
                crate::modules::codex_speed::get_api_service_app_speed_config,
            )
            .await
            .map_err(|_| native_error(503, "service_unavailable", "Cannot read API speed"))?
            .map_err(|_| native_error(503, "service_unavailable", "Cannot read API speed"))?;
            let saved = if saved.speed == CodexAppSpeed::Fast {
                "fast"
            } else {
                "standard"
            };
            if expected != saved {
                return Err(native_error(
                    409,
                    "stale_speed_update",
                    "The persisted API service speed has changed",
                ));
            }
            *current = (revision, speed.clone());
            Ok(json!({"speed": speed, "revision": revision}))
        }
        NativeManagementRequest::Quota => {
            let collection = bridge.collection().await?;
            let scope = native_management_scope(&collection, caller)?;
            let accounts = native_load_accounts(scope.clone()).await?;
            let cooling: HashSet<String> = bridge
                .cooldowns
                .lock()
                .map_err(|_| {
                    native_error(503, "service_unavailable", "Account state is unavailable")
                })?
                .iter()
                .filter(|(_, state)| state.until > Instant::now())
                .map(|(key, _)| key.account.clone())
                .collect();
            Ok(native_quota_summary(
                &scope,
                &accounts,
                &cooling,
                chrono::Utc::now().timestamp(),
            ))
        }
        NativeManagementRequest::ResetAuth { account_ids }
        | NativeManagementRequest::ResetScheduler { account_ids } => {
            let collection = bridge.collection().await?;
            let scope = native_management_scope(&collection, caller)?;
            let selected = native_scoped_reset_ids(&scope, account_ids)?;
            let existing: HashSet<String> = native_load_accounts(selected.clone())
                .await?
                .into_iter()
                .map(|account| account.id)
                .collect();
            let selected = selected
                .into_iter()
                .filter(|id| existing.contains(id))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Err(native_error(
                    404,
                    "account_not_found",
                    "No matching accounts found",
                ));
            }
            bridge
                .cooldowns
                .lock()
                .map_err(|_| {
                    native_error(503, "service_unavailable", "Account state is unavailable")
                })?
                .retain(|key, _| !existing.contains(&key.account));
            bridge
                .bindings
                .lock()
                .await
                .retain(|_, (id, _)| !existing.contains(id));
            if let Ok(mut clients) = bridge.clients.lock() {
                clients.retain_accounts(|account| !existing.contains(account));
            }
            {
                let mut runtime = gateway_runtime().lock().await;
                clear_runtime_account_health(&mut runtime, &selected, false);
                clear_runtime_quota_cooldowns(&mut runtime, &selected);
                runtime
                    .recovery_suppressed_accounts
                    .retain(|id, _| !existing.contains(id));
            }
            emit_local_access_state_updated();
            Ok(
                json!({"status":"ok", "reset":selected.len(), "authReset":selected.len(), "schedulerReset":selected.len(), "accountIds":selected}),
            )
        }
    }
}

fn native_quota_summary(
    scope: &[String],
    accounts: &[CodexAccount],
    cooling: &HashSet<String>,
    now: i64,
) -> Value {
    let by_id = accounts
        .iter()
        .map(|account| (account.id.as_str(), account))
        .collect::<HashMap<_, _>>();
    let mut response = json!({"version":1,"scope":"api_key_account_pool","accountCount":0,"includedAccountCount":0,"missingAccountCount":0,"availableAccountCount":0,"abnormalAccountCount":0,"cooldownAccountCount":0,"stale":false});
    let (mut count, mut included, mut missing, mut available, mut abnormal, mut cooldown) =
        (0, 0, 0, 0, 0, 0);
    let (mut total, mut weekly, mut hourly): (Option<i64>, Option<i64>, Option<i64>) =
        (None, None, None);
    let mut updated_at = 0;
    let mut stale = false;
    let mut plans: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for id in scope {
        let account = by_id.get(id.as_str()).copied();
        let plan = account
            .map(|account| {
                if account.is_api_key_auth() {
                    "API_KEY".into()
                } else {
                    account
                        .plan_type
                        .as_deref()
                        .unwrap_or("UNKNOWN")
                        .trim()
                        .replace(['-', ' '], "_")
                        .to_ascii_uppercase()
                }
            })
            .unwrap_or_else(|| "UNKNOWN".into());
        let plan = plans
            .entry(plan.clone())
            .or_insert_with(|| json!({"plan":plan,"count":0}));
        plan["count"] = json!(plan["count"].as_u64().unwrap_or(0) + 1);
        if account.is_some_and(CodexAccount::is_api_key_auth) {
            continue;
        }
        count += 1;
        let Some(account) = account else {
            missing += 1;
            abnormal += 1;
            continue;
        };
        let Some(quota) = account.quota.as_ref() else {
            missing += 1;
            abnormal += 1;
            continue;
        };
        let mut first = (quota.hourly_window_present != Some(false)
            && (0..=100).contains(&quota.hourly_percentage))
        .then_some((
            i64::from(quota.hourly_percentage),
            quota.hourly_window_minutes.unwrap_or(300),
        ));
        let second = (quota.weekly_window_present != Some(false)
            && (0..=100).contains(&quota.weekly_percentage))
        .then_some((
            i64::from(quota.weekly_percentage),
            quota.weekly_window_minutes.unwrap_or(10080),
        ));
        if second.is_some_and(|(value, _)| value == 0) {
            first = first.map(|(_, duration)| (0, duration));
        }
        let value = match (first, second) {
            (Some(a), Some(b)) => Some(if a.1 <= b.1 { a.0 } else { b.0 }),
            (Some(a), None) | (None, Some(a)) => Some(a.0),
            _ => None,
        };
        let Some(value) = value else {
            missing += 1;
            abnormal += 1;
            continue;
        };
        total = Some(total.unwrap_or(0) + value);
        for (value, minutes) in [first, second].into_iter().flatten() {
            let (field, aggregate) = if minutes >= 5 * 24 * 60 {
                ("weeklyRemainingPercent", &mut weekly)
            } else if minutes > 0 && minutes <= 6 * 60 {
                ("fiveHourRemainingPercent", &mut hourly)
            } else {
                continue;
            };
            *aggregate = Some(aggregate.unwrap_or(0) + value);
            plan[field] = json!(plan[field].as_i64().unwrap_or(0) + value);
        }
        included += 1;
        if cooling.contains(id) {
            cooldown += 1;
        } else if account.requires_reauth || account.quota_error.is_some() {
            abnormal += 1;
        } else {
            available += 1;
        }
        if let Some(at) = account.usage_updated_at {
            updated_at = updated_at.max(at);
            stale |= now.saturating_sub(at) > 900 || at > now;
        } else {
            stale = true;
        }
    }
    for (field, value) in [
        ("accountCount", count),
        ("includedAccountCount", included),
        ("missingAccountCount", missing),
        ("availableAccountCount", available),
        ("abnormalAccountCount", abnormal),
        ("cooldownAccountCount", cooldown),
    ] {
        response[field] = json!(value);
    }
    for (field, value) in [
        ("remainingPercent", total),
        ("weeklyRemainingPercent", weekly),
        ("fiveHourRemainingPercent", hourly),
    ] {
        if let Some(value) = value {
            response[field] = json!(value);
        }
    }
    if updated_at > 0 {
        response["updatedAt"] = json!(updated_at);
    }
    response["stale"] = json!(stale);
    response["plans"] = json!(plans.into_values().collect::<Vec<_>>());
    response
}

#[cfg(test)]
mod native_diagnostic_tests {
    use super::*;
    use crate::modules::codex_native_api::diagnostics::DiagnosticPayload;

    #[test]
    fn management_scope_never_promotes_empty_or_limited_keys() {
        assert!(native_scoped_reset_ids(&[], vec!["outside".into()]).is_err());
        assert_eq!(
            native_scoped_reset_ids(&["inside".into()], vec!["outside".into(), "inside".into()])
                .unwrap(),
            vec!["inside"]
        );
        let public = native_contract::Caller {
            id: "public".into(),
            label: "Public".into(),
        };
        let internal = native_contract::Caller {
            id: NATIVE_INTERNAL_CALLER.into(),
            label: String::new(),
        };
        for request in [
            NativeManagementRequest::DiagnosticsEvents,
            NativeManagementRequest::DiagnosticsConfig { enabled: true },
            NativeManagementRequest::ServiceTier {
                speed: "fast".into(),
                revision: 1,
                loopback: true,
            },
        ] {
            assert_eq!(
                native_management_permission(&public, &request)
                    .unwrap_err()
                    .status,
                403
            );
            assert!(native_management_permission(&internal, &request).is_ok());
        }
        assert_eq!(
            native_management_permission(
                &internal,
                &NativeManagementRequest::ServiceTier {
                    speed: "fast".into(),
                    revision: 1,
                    loopback: false
                }
            )
            .unwrap_err()
            .status,
            403
        );
    }

    #[test]
    fn native_payloads_use_host_redaction_and_disabled_capture_drops_bodies() {
        let context = NativeDiagnosticContext {
            request_id: "request-fixture".into(),
            started_at_ms: 1,
            payloads: vec![DiagnosticPayload {
                stage: "client_request".into(),
                body: r#"{"authorization":"secret","nested":{"api_key":"hidden"},"input":"hello"}"#
                    .into(),
                headers: HashMap::from([
                    ("authorization".into(), "Bearer hidden".into()),
                    ("content-type".into(), "application/json".into()),
                ]),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(native_diagnostic_detail(context.clone(), false)
            .payloads
            .is_empty());
        let result = native_diagnostic_detail(context, true);
        assert_eq!(result.payloads[0].stage, "client");
        assert!(!result.payloads[0].body.contains("secret"));
        assert!(!result.payloads[0].body.contains("hidden"));
        assert!(result.payloads[0].body.contains("hello"));
        assert!(!result.payloads[0].headers.contains_key("authorization"));
        assert_eq!(result.payloads[0].sha256.len(), 64);
    }

    #[test]
    fn native_identity_metadata_survives_body_capture_disabled_and_request_stages_normalize() {
        let context = NativeDiagnosticContext {
            request_id: "identity-fixture".into(),
            client_identity_captured: true,
            client_session_id: Some("raw-session".into()),
            client_thread_id: None,
            attempts: vec![
                crate::modules::codex_native_api::diagnostics::DiagnosticAttempt {
                    sequence: 1,
                    account_id: "a".into(),
                    upstream_identity_captured: true,
                    upstream_session_id: Some("mapped-session".into()),
                    upstream_thread_id: Some("mapped-thread".into()),
                    ..Default::default()
                },
            ],
            payloads: ["client_request", "upstream_request", "upstream_response"]
                .into_iter()
                .map(|stage| DiagnosticPayload {
                    stage: stage.into(),
                    body: r#"{"input":"example","api_key":"never-persist"}"#.into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let enabled = native_diagnostic_detail(context.clone(), true);
        assert_eq!(enabled.payloads.len(), 2);
        assert_eq!(enabled.payloads[0].stage, "client");
        assert_eq!(enabled.payloads[1].stage, "upstream");
        assert!(enabled
            .payloads
            .iter()
            .all(|payload| !payload.body.contains("never-persist")));
        let disabled = native_diagnostic_detail(context, false);
        assert!(disabled.payloads.is_empty());
        assert_eq!(disabled.client_session_id.as_deref(), Some("raw-session"));
        assert!(disabled.client_identity_captured);
        assert_eq!(disabled.client_thread_id, None);
        assert_eq!(
            disabled.attempts[0].upstream_session_id.as_deref(),
            Some("mapped-session")
        );
        assert_eq!(
            disabled.attempts[0].upstream_thread_id.as_deref(),
            Some("mapped-thread")
        );
    }

    #[test]
    fn untrusted_truncated_payloads_cannot_skip_host_redaction() {
        let context = NativeDiagnosticContext {
            request_id: "truncated-fixture".into(),
            payloads: vec![DiagnosticPayload {
                stage: "upstream".into(),
                body: "{\"authorization\":\"do-not-store-secret".into(),
                truncated: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let detail = native_diagnostic_detail(context, true);
        assert!(!detail.payloads[0].body.contains("do-not-store-secret"));
        assert!(detail.payloads[0].truncated);
    }

    #[test]
    fn quota_reply_contains_only_scoped_accounts_and_keeps_missing_evidence() {
        fn account(id: &str, hourly: i32, weekly: i32) -> CodexAccount {
            let mut account = CodexAccount::new(
                id.into(),
                "fixture@example.invalid".into(),
                crate::models::codex::CodexTokens {
                    access_token: "fixture".into(),
                    id_token: "fixture".into(),
                    refresh_token: None,
                },
            );
            account.usage_updated_at = Some(1000);
            account.plan_type = Some("pro".into());
            account.quota = Some(serde_json::from_value(json!({"hourly_percentage":hourly,"weekly_percentage":weekly,"hourly_window_minutes":300,"weekly_window_minutes":10080})).unwrap());
            account
        }
        let result = native_quota_summary(
            &["scoped".into(), "missing".into()],
            &[account("scoped", 80, 0), account("outside", 100, 100)],
            &HashSet::new(),
            1001,
        );
        assert_eq!(result["accountCount"], 2);
        assert_eq!(result["includedAccountCount"], 1);
        assert_eq!(result["missingAccountCount"], 1);
        assert_eq!(result["remainingPercent"], 0);
        assert_eq!(result["fiveHourRemainingPercent"], 0);
        assert_eq!(result["weeklyRemainingPercent"], 0);
        let empty =
            native_quota_summary(&[], &[account("outside", 100, 100)], &HashSet::new(), 1001);
        assert_eq!(empty["accountCount"], 0);
        assert!(empty.get("remainingPercent").is_none());
    }
}
