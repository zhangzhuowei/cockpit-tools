use base64::Engine as _;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::models::cursor::{CursorAccount, CursorAccountIndex, CursorImportPayload, CursorSandUsage};
use crate::modules::{account, logger};

const ACCOUNTS_INDEX_FILE: &str = "cursor_accounts.json";
const ACCOUNTS_DIR: &str = "cursor_accounts";
const CURSOR_QUOTA_ALERT_COOLDOWN_SECONDS: i64 = 10 * 60;
const CURSOR_ACCESS_TOKEN_REFRESH_THRESHOLD_SECONDS: i64 = 5 * 60;

lazy_static::lazy_static! {
    static ref CURSOR_ACCOUNT_INDEX_LOCK: Mutex<()> = Mutex::new(());
    static ref CURSOR_QUOTA_ALERT_LAST_SENT: Mutex<HashMap<String, i64>> = Mutex::new(HashMap::new());
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn normalize_status_value(value: Option<&str>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_lowercase())
        }
    })
}

fn is_banned_status(value: Option<&str>) -> bool {
    matches!(
        normalize_status_value(value).as_deref(),
        Some("banned") | Some("ban") | Some("forbidden")
    )
}

fn is_banned_reason(value: Option<&str>) -> bool {
    let Some(reason) = normalize_status_value(value) else {
        return false;
    };
    reason.contains("banned")
        || reason.contains("forbidden")
        || reason.contains("suspended")
        || reason.contains("disabled")
        || reason.contains("封禁")
        || reason.contains("禁用")
}

pub(crate) fn is_banned_account(account: &CursorAccount) -> bool {
    is_banned_status(account.status.as_deref())
        || is_banned_reason(account.status_reason.as_deref())
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

fn get_data_dir() -> Result<PathBuf, String> {
    account::get_data_dir()
}

fn get_accounts_dir() -> Result<PathBuf, String> {
    let base = get_data_dir()?;
    let dir = base.join(ACCOUNTS_DIR);
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("创建 Cursor 账号目录失败: {}", e))?;
    }
    Ok(dir)
}

fn get_accounts_index_path() -> Result<PathBuf, String> {
    Ok(get_data_dir()?.join(ACCOUNTS_INDEX_FILE))
}

fn normalize_account_id(account_id: &str) -> Result<String, String> {
    let trimmed = account_id.trim();
    if trimmed.is_empty() {
        return Err("账号 ID 不能为空".to_string());
    }

    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err("账号 ID 非法，包含路径字符".to_string());
    }

    let valid = trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.');
    if !valid {
        return Err("账号 ID 非法，仅允许字母/数字/._-".to_string());
    }

    Ok(trimmed.to_string())
}

fn resolve_account_file_path(account_id: &str) -> Result<PathBuf, String> {
    let normalized = normalize_account_id(account_id)?;
    Ok(get_accounts_dir()?.join(format!("{}.json", normalized)))
}

// ---------------------------------------------------------------------------
// Account file operations
// ---------------------------------------------------------------------------

pub fn load_account(account_id: &str) -> Option<CursorAccount> {
    let account_path = resolve_account_file_path(account_id).ok()?;
    if !account_path.exists() {
        return None;
    }
    let content = fs::read_to_string(&account_path).ok()?;
    match crate::modules::secure_account_storage::deserialize_account_file::<CursorAccount>(
        &account_path,
        &content,
    ) {
        Ok((account, needs_rotation)) => {
            if needs_rotation {
                let account_for_rewrite = account.clone();
                crate::modules::deferred_account_rewrite::schedule_account_rewrite_if_unchanged(
                    "cursor",
                    account_for_rewrite.id.clone(),
                    account_path.clone(),
                    content.as_bytes(),
                    move || {
                        crate::modules::secure_account_storage::serialize_account_file(
                            "cursor",
                            &account_for_rewrite,
                        )
                    },
                );
            }
            Some(account)
        }
        Err(_) => {
            let backup = cursor_account_recovery::detail_backup(&account_path)?;
            let content = fs::read_to_string(backup).ok()?;
            let restored = decode_recovery_detail(&content).ok()?;
            (restored.id == account_id).then_some(restored)
        }
    }
}

fn save_account_file(account: &CursorAccount) -> Result<(), String> {
    let path = resolve_account_file_path(account.id.as_str())?;
    crate::modules::secure_account_storage::write_account_preserving_switch_time(&path, "cursor", account)
        .map_err(|e| format!("保存账号失败: {}", e))
}

// Record credential-switch success separately from refresh activity.
pub fn touch_last_used(account_id: &str) -> Result<(), String> {
    let mut account = load_account(account_id)
        .ok_or_else(|| format!("Cursor account not found: {}", account_id))?;
    account.last_switched_at = Some(now_ts());
    save_account_file(&account)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Index operations
// ---------------------------------------------------------------------------

fn delete_account_file(account_id: &str) -> Result<(), String> {
    let path = resolve_account_file_path(account_id)?;
    let _lock = crate::modules::account_file_crypto::lock_account_file_for_write(&path)?;
    crate::modules::atomic_write::remove_file_locked(&path).map(|_| ())
}

include!("../../../crates/cockpit-core/src/modules/cursor_account_index_flow.rs");

fn refresh_summary(index: &mut CursorAccountIndex, account: &CursorAccount) {
    if let Some(summary) = index.accounts.iter_mut().find(|item| item.id == account.id) {
        *summary = account.summary();
        return;
    }
    index.accounts.push(account.summary());
}

fn upsert_account_record(account: CursorAccount) -> Result<CursorAccount, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let mut index = load_account_index_checked()?;
    save_account_file(&account)?;
    refresh_summary(&mut index, &account);
    save_account_index(&index)?;
    Ok(account)
}

fn persist_quota_query_error(account_id: &str, message: &str) {
    let Ok(_lock) = CURSOR_ACCOUNT_INDEX_LOCK.lock() else {
        return;
    };
    let Some(mut account) = load_account(account_id) else {
        return;
    };
    account.quota_query_last_error = Some(message.to_string());
    account.quota_query_last_error_at = Some(chrono::Utc::now().timestamp_millis());
    let _ = save_account_file(&account);
}

// ---------------------------------------------------------------------------
// Identity helpers
// ---------------------------------------------------------------------------

fn normalize_non_empty(value: Option<&str>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_email_identity(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value).and_then(|raw| {
        let lowered = raw.to_lowercase();
        if lowered.contains('@') {
            Some(lowered)
        } else {
            None
        }
    })
}

fn normalize_token_identity(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value)
}

fn normalize_auth_identity(value: Option<&str>) -> Option<String> {
    normalize_non_empty(value)
}

/// `auth0|user_xxx`（JWT sub / state.vscdb）与 `user_xxx`（GetUserMeta.workosId）指向同一用户，
/// 比较时只看 `|` 之后的部分。
fn auth_identity_key(value: &str) -> String {
    value
        .trim()
        .rsplit('|')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}

pub(crate) fn auth_ids_match(left: &str, right: &str) -> bool {
    auth_identity_key(left) == auth_identity_key(right)
}

/// 刷新时以 token 里的真实身份校正 `auth_id`。
/// 旧版本在 state.vscdb 错位时会把别的账号的 authId 写进记录，导致同一账号出现两条、
/// 且因 auth_id 不同无法自动去重；这里修正后，下一次列表加载就能合并。
fn reconcile_account_auth_id(account: &mut CursorAccount, workos_id: Option<&str>) {
    let Some(identity) = extract_auth_id_from_access_token(account.access_token.as_str())
        .or_else(|| normalize_non_empty(workos_id))
    else {
        return;
    };

    let needs_update = account
        .auth_id
        .as_deref()
        .map(|current| !auth_ids_match(current, identity.as_str()))
        .unwrap_or(true);
    if !needs_update {
        return;
    }

    if let Some(previous) = account.auth_id.as_deref() {
        logger::log_warn(&format!(
            "[Cursor Refresh] auth_id 与 token 身份不一致，已按 token 修正: id={}, previous={}, actual={}",
            account.id, previous, identity
        ));
    }
    account.auth_id = Some(identity.clone());
    upsert_cursor_auth_raw_string(account, "authId", Some(identity));
}

fn decode_access_token_payload(access_token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload_b64.len() % 4 {
        2 => format!("{}==", payload_b64),
        3 => format!("{}=", payload_b64),
        _ => payload_b64,
    };

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub(crate) fn extract_auth_id_from_access_token(access_token: &str) -> Option<String> {
    let value = decode_access_token_payload(access_token)?;
    normalize_non_empty(value.get("sub").and_then(|raw| raw.as_str()))
}

fn extract_access_token_exp(access_token: &str) -> Option<i64> {
    let value = decode_access_token_payload(access_token)?;
    value.get("exp").and_then(|raw| raw.as_i64())
}

/// 用户粘贴的 token 常带着来源痕迹：`Bearer ` 前缀、浏览器 Cookie 的
/// `WorkosCursorSessionToken=user_xxx::<jwt>`（或 URL 编码的 `%3A%3A`）、首尾引号。
/// 这些都能正确解析出 Auth ID，但作为 Bearer 发出去就是 401，因此在保存前统一剥掉。
pub(crate) fn normalize_import_access_token(raw: &str) -> String {
    let mut token = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim();

    if token.len() > 7 && token[..7].eq_ignore_ascii_case("bearer ") {
        token = token[7..].trim();
    }
    if let Some(rest) = token
        .strip_prefix("WorkosCursorSessionToken=")
        .or_else(|| token.strip_prefix("workoscursorsessiontoken="))
    {
        token = rest.trim();
    }

    let decoded = token.replace("%3A%3A", "::").replace("%3a%3a", "::");
    let token = decoded.rsplit("::").next().unwrap_or(decoded.as_str());
    token.trim().to_string()
}

/// 导入前校验：必须是 JWT；已过期且没有 refresh_token 的 token 保存下去也只会一直
/// "配额查询失败"，不如直接告诉用户重新获取。
pub(crate) fn validate_import_access_token(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(), String> {
    if access_token.split('.').count() < 3 || decode_access_token_payload(access_token).is_none() {
        return Err(
            "access_token 不是有效的 Cursor JWT，请粘贴 Cursor 的 accessToken（以 eyJ 开头），而不是 Cookie 或 refresh_token"
                .to_string(),
        );
    }

    let Some(exp) = extract_access_token_exp(access_token) else {
        return Ok(());
    };
    let now = now_ts();
    if exp <= now && normalize_non_empty(refresh_token).is_none() {
        let expired_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0)
            .map(|value| value.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| exp.to_string());
        return Err(format!(
            "access_token 已于 {} 过期，且没有 refresh_token 可续期；请重新获取 token 或改用 OAuth 登录",
            expired_at
        ));
    }
    Ok(())
}

fn access_token_needs_refresh(access_token: &str) -> bool {
    let Some(exp) = extract_access_token_exp(access_token) else {
        return true;
    };
    exp <= now_ts() + CURSOR_ACCESS_TOKEN_REFRESH_THRESHOLD_SECONDS
}

fn extract_auth_id_from_raw_value(raw: Option<&Value>) -> Option<String> {
    let obj = raw.and_then(|value| value.as_object())?;

    normalize_auth_identity(
        obj.get("authId")
            .and_then(|value| value.as_str())
            .or_else(|| obj.get("auth_id").and_then(|value| value.as_str()))
            .or_else(|| obj.get("workosId").and_then(|value| value.as_str()))
            .or_else(|| obj.get("workos_id").and_then(|value| value.as_str())),
    )
}

fn resolve_payload_auth_id(payload: &CursorImportPayload) -> Option<String> {
    normalize_auth_identity(payload.auth_id.as_deref())
        .or_else(|| extract_auth_id_from_raw_value(payload.cursor_auth_raw.as_ref()))
        .or_else(|| extract_auth_id_from_access_token(payload.access_token.as_str()))
}

pub(crate) fn resolve_account_auth_id(account: &CursorAccount) -> Option<String> {
    normalize_auth_identity(account.auth_id.as_deref())
        .or_else(|| extract_auth_id_from_raw_value(account.cursor_auth_raw.as_ref()))
        .or_else(|| extract_auth_id_from_access_token(account.access_token.as_str()))
}

fn cursor_auth_ids_match(existing: &str, incoming: &str) -> bool {
    if existing == incoming {
        return true;
    }
    // Cursor exposes the bare WorkOS user ID as well as qualified token subjects.
    let (bare, qualified) = if existing.starts_with("user_") {
        (existing, incoming)
    } else {
        (incoming, existing)
    };
    bare.starts_with("user_")
        && ["workos|", "auth0|"]
            .into_iter()
            .any(|prefix| qualified.strip_prefix(prefix) == Some(bare))
}

fn cursor_auth_raw_object_mut(account: &mut CursorAccount) -> &mut serde_json::Map<String, Value> {
    if !matches!(account.cursor_auth_raw, Some(Value::Object(_))) {
        account.cursor_auth_raw = Some(Value::Object(serde_json::Map::new()));
    }

    match account.cursor_auth_raw.as_mut() {
        Some(Value::Object(obj)) => obj,
        _ => unreachable!("cursor_auth_raw 应始终为对象"),
    }
}

fn upsert_cursor_auth_raw_string(account: &mut CursorAccount, key: &str, value: Option<String>) {
    let Some(text) = normalize_non_empty(value.as_deref()) else {
        return;
    };
    cursor_auth_raw_object_mut(account).insert(key.to_string(), Value::String(text));
}

fn upsert_cursor_auth_raw_bool(account: &mut CursorAccount, key: &str, value: Option<bool>) {
    let Some(flag) = value else {
        return;
    };
    cursor_auth_raw_object_mut(account).insert(key.to_string(), Value::Bool(flag));
}

fn normalize_cursor_sign_up_type(value: Option<&str>) -> Option<String> {
    let raw = normalize_non_empty(value)?;
    match raw.as_str() {
        "SIGN_UP_TYPE_AUTH_0" => Some("Auth_0".to_string()),
        "SIGN_UP_TYPE_GOOGLE" => Some("Google".to_string()),
        "SIGN_UP_TYPE_GITHUB" => Some("Github".to_string()),
        "SIGN_UP_TYPE_WORKOS" => Some("WorkOS".to_string()),
        _ => Some(raw),
    }
}

// ---------------------------------------------------------------------------
// Merge helpers
// ---------------------------------------------------------------------------

fn merge_string_list(
    primary: Option<Vec<String>>,
    secondary: Option<Vec<String>>,
) -> Option<Vec<String>> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();

    for source in [primary, secondary] {
        if let Some(values) = source {
            for value in values {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let key = trimmed.to_lowercase();
                if seen.insert(key) {
                    merged.push(trimmed.to_string());
                }
            }
        }
    }

    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

fn fill_if_empty_string(target: &mut String, source: &str) {
    if target.trim().is_empty() {
        let incoming = source.trim();
        if !incoming.is_empty() {
            *target = incoming.to_string();
        }
    }
}

fn fill_if_none<T: Clone>(target: &mut Option<T>, source: &Option<T>) {
    if target.is_none() {
        *target = source.clone();
    }
}

fn merge_duplicate_account(primary: &mut CursorAccount, duplicate: &CursorAccount) {
    fill_if_empty_string(&mut primary.email, duplicate.email.as_str());
    fill_if_empty_string(&mut primary.access_token, duplicate.access_token.as_str());

    fill_if_none(&mut primary.auth_id, &duplicate.auth_id);
    fill_if_none(&mut primary.name, &duplicate.name);
    fill_if_none(&mut primary.refresh_token, &duplicate.refresh_token);
    fill_if_none(&mut primary.membership_type, &duplicate.membership_type);
    fill_if_none(
        &mut primary.subscription_status,
        &duplicate.subscription_status,
    );
    fill_if_none(&mut primary.sign_up_type, &duplicate.sign_up_type);
    fill_if_none(&mut primary.cursor_auth_raw, &duplicate.cursor_auth_raw);
    fill_if_none(&mut primary.cursor_usage_raw, &duplicate.cursor_usage_raw);
    fill_if_none(&mut primary.status, &duplicate.status);
    fill_if_none(&mut primary.status_reason, &duplicate.status_reason);

    primary.tags = merge_string_list(primary.tags.clone(), duplicate.tags.clone());
    primary.created_at = primary.created_at.min(duplicate.created_at);
    primary.last_used = primary.last_used.max(duplicate.last_used);
    primary.last_switched_at = primary.last_switched_at.max(duplicate.last_switched_at);
}

fn choose_primary_account_index(group: &[usize], accounts: &[CursorAccount]) -> usize {
    group
        .iter()
        .copied()
        .max_by(|left, right| {
            let left_account = &accounts[*left];
            let right_account = &accounts[*right];
            left_account
                .last_used
                .cmp(&right_account.last_used)
                .then_with(|| right_account.created_at.cmp(&left_account.created_at))
        })
        .unwrap_or(group[0])
}

fn schedule_cursor_recovery(store: IndexStore<CursorAccountIndex>) -> Result<(), String> {
    let directory = get_data_dir()?.join(ACCOUNTS_DIR);
    let mut job = cursor_account_recovery::CursorRecovery::new(
        store, directory, decode_recovery_detail, normalize_account_id, &CURSOR_ACCOUNT_INDEX_LOCK,
    );
    crate::modules::background_maintenance::schedule(
        job.migration_key(),
        move || {
            let result = job.run_batch();
            if matches!(&result, Ok(true) | Err(_)) && job.take_refresh_needed() {
                if let Some(app) = crate::get_app_handle() {
                    use tauri::Emitter;
                    let _ = app.emit("cursor:index-recovered", ());
                }
            }
            result
        },
    );
    Ok(())
}

fn decode_recovery_detail(content: &str) -> Result<CursorAccount, String> {
    serde_json::from_str(content).or_else(|_| {
        crate::modules::secure_account_storage::deserialize_encrypted_snapshot(content)
    })
}

fn cursor_recovery_busy() -> String {
    crate::modules::background_maintenance::busy_message()
}

pub(crate) fn schedule_index_migration() -> Result<(), String> {
    schedule_cursor_recovery(index_store()?)
}

fn merge_json_objects(existing: Option<Value>, incoming: Option<Value>) -> Option<Value> {
    match (existing, incoming) {
        (Some(Value::Object(mut base)), Some(Value::Object(extra))) => {
            for (key, value) in extra {
                base.insert(key, value);
            }
            Some(Value::Object(base))
        }
        (_, Some(value)) => Some(value),
        (existing, None) => existing,
    }
}

/// 导入（本地 / JSON / Token）落到已有账号上时只覆盖 payload 真正携带的字段。
/// 本地导入的 payload 没有 usage、name、status 等，不能把已刷新出来的配额、封禁状态清掉；
/// cursor_auth_raw 做键级合并，保留 refresh 写入的 workosId / isEnterprise 等信息。
fn apply_payload(
    account: &mut CursorAccount,
    payload: CursorImportPayload,
    resolved_auth_id: Option<String>,
) {
    let incoming_email = payload.email.trim().to_string();
    if !incoming_email.is_empty() {
        account.email = incoming_email;
    } else if !account.email.contains('@') {
        account.email.clear();
    }

    let token_changed = normalize_token_identity(Some(payload.access_token.as_str()))
        != normalize_token_identity(Some(account.access_token.as_str()));
    account.access_token = payload.access_token;

    if payload.name.is_some() {
        account.name = payload.name;
    }
    if payload.refresh_token.is_some() {
        account.refresh_token = payload.refresh_token;
    } else if token_changed || account.refresh_token.is_none() {
        // Cursor 桌面端的 refreshToken 就是 session JWT 本身；粘贴 token 添加的账号
        // 补上它，切号写入和 token 保活才有可用的 refresh 凭据。
        account.refresh_token =
            access_token_is_session(&account.access_token).then(|| account.access_token.clone());
    }
    if payload.membership_type.is_some() {
        account.membership_type = payload.membership_type;
    }
    if payload.subscription_status.is_some() {
        account.subscription_status = payload.subscription_status;
    }
    if payload.sign_up_type.is_some() {
        account.sign_up_type = payload.sign_up_type;
    }
    account.cursor_auth_raw =
        merge_json_objects(account.cursor_auth_raw.take(), payload.cursor_auth_raw);
    if payload.cursor_usage_raw.is_some() {
        account.cursor_usage_raw = payload.cursor_usage_raw;
    }
    if let Some(auth_id) = resolved_auth_id {
        account.auth_id = Some(auth_id.clone());
        upsert_cursor_auth_raw_string(account, "authId", Some(auth_id));
    }
    // 换了新凭据时状态需要重新评估；同一 token 重复导入则保留已知的封禁/错误状态。
    if payload.status.is_some() || token_changed {
        account.status = payload.status;
        account.status_reason = payload.status_reason;
    }
    account.last_used = now_ts();
}

pub fn upsert_account(payload: CursorImportPayload) -> Result<CursorAccount, String> {
    upsert_account_with_source(payload, false)
}

fn upsert_account_with_source(
    mut payload: CursorImportPayload,
    from_local: bool,
) -> Result<CursorAccount, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;

    let now = now_ts();
    let mut index = load_account_index_checked()?;
    let incoming_auth_id = resolve_payload_auth_id(&payload);
    let incoming_email = normalize_email_identity(Some(payload.email.as_str()));
    let incoming_token = normalize_token_identity(Some(payload.access_token.as_str()));

    let identity_seed = incoming_auth_id
        .clone()
        .or_else(|| incoming_email.clone())
        .or_else(|| incoming_token.clone())
        .unwrap_or_else(|| "cursor_user".to_string())
        .to_lowercase();
    let generated_id = format!("cursor_{:x}", md5::compute(identity_seed.as_bytes()));

    let account_id = index
        .accounts
        .iter()
        .filter_map(|item| load_account(&item.id))
        .find(|account| {
            let existing_auth_id = resolve_account_auth_id(account);
            if let (Some(existing), Some(incoming)) =
                (existing_auth_id.as_ref(), incoming_auth_id.as_ref())
            {
                return auth_ids_match(existing, incoming);
            }
            if existing_auth_id.is_some() || incoming_auth_id.is_some() {
                return false;
            }

            let existing_email = normalize_email_identity(Some(account.email.as_str()));
            let existing_token = normalize_token_identity(Some(account.access_token.as_str()));
            if let (Some(ex), Some(inc)) = (existing_email.as_ref(), incoming_email.as_ref()) {
                if ex == inc {
                    return true;
                }
            }
            if let (Some(ex), Some(inc)) = (existing_token.as_ref(), incoming_token.as_ref()) {
                if ex == inc {
                    return true;
                }
            }
            false
        })
        .map(|account| account.id)
        .unwrap_or(generated_id);

    let existing = load_account(&account_id);
    let mut resolved_auth_id = incoming_auth_id.clone();
    if let Some(account) = existing.as_ref().filter(|_| from_local) {
        // Cursor's local login cache contains credentials, not the fetched profile or usage.
        if payload.name.is_none() {
            payload.name = account.name.clone();
        }
        if payload.cursor_usage_raw.is_none() {
            payload.cursor_usage_raw = account.cursor_usage_raw.clone();
        }
        if let Some(existing_id) = resolve_account_auth_id(account).filter(|existing| {
            incoming_auth_id
                .as_deref()
                .is_some_and(|incoming| cursor_auth_ids_match(existing, incoming))
        }) {
            resolved_auth_id = Some(existing_id);
        }
    }
    let tags = existing.as_ref().and_then(|acc| acc.tags.clone());
    let created_at = existing.as_ref().map(|acc| acc.created_at).unwrap_or(now);

    let mut account = existing.unwrap_or(CursorAccount {
        last_switched_at: None,
        id: account_id.clone(),
        email: payload.email.clone(),
        auth_id: incoming_auth_id.clone(),
        name: payload.name.clone(),
        tags,
        access_token: payload.access_token.clone(),
        refresh_token: payload.refresh_token.clone(),
        membership_type: payload.membership_type.clone(),
        subscription_status: payload.subscription_status.clone(),
        sign_up_type: payload.sign_up_type.clone(),
        cursor_auth_raw: payload.cursor_auth_raw.clone(),
        cursor_usage_raw: payload.cursor_usage_raw.clone(),
        sand_usage: None,
        status: payload.status.clone(),
        status_reason: payload.status_reason.clone(),
        quota_query_last_error: None,
        quota_query_last_error_at: None,
        quota_query_auth_failures: None,
        usage_updated_at: None,
        created_at,
        last_used: now,
    });

    apply_payload(&mut account, payload, resolved_auth_id);
    account.id = account_id;
    account.created_at = created_at;
    if !from_local {
        account.quota_query_last_error = None;
        account.quota_query_last_error_at = None;
    }
    account.last_used = now;

    save_account_file(&account)?;
    refresh_summary(&mut index, &account);
    save_account_index(&index)?;

    logger::log_info(&format!(
        "Cursor 账号已保存: id={}, email={}",
        account.id, account.email
    ));
    Ok(account)
}

pub fn remove_account(account_id: &str) -> Result<(), String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let path = resolve_account_file_path(account_id)?;
    let _file_lock = crate::modules::account_file_crypto::lock_account_file_for_write(&path)?;
    let mut index = load_account_index_checked()?;
    index.accounts.retain(|item| item.id != account_id);
    index_store()?.replace_with_removals(&index, &[account_id.to_string()])?;
    crate::modules::atomic_write::remove_file_locked(&path)
        .map_err(|error| format!("删除账号文件失败: {error}"))?;
    Ok(())
}

pub fn remove_accounts(account_ids: &[String]) -> Result<(), String> {
    for id in account_ids {
        remove_account(id)?;
    }
    Ok(())
}

pub fn update_account_tags(account_id: &str, tags: Vec<String>) -> Result<CursorAccount, String> {
    let mut account = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    account.tags = Some(tags);
    account.last_used = now_ts();
    let updated = account.clone();
    upsert_account_record(account)?;
    Ok(updated)
}

// ---------------------------------------------------------------------------
// Import / Export
// ---------------------------------------------------------------------------

fn clone_object_value(value: Option<&Value>) -> Option<Value> {
    value.and_then(|raw| {
        if raw.is_object() {
            Some(raw.clone())
        } else {
            None
        }
    })
}

fn extract_string(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = obj.get(*key) {
            if let Some(text) = value.as_str().map(str::trim).filter(|v| !v.is_empty()) {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn payload_from_import_value(raw: Value) -> Result<CursorImportPayload, String> {
    let obj = raw
        .as_object()
        .ok_or_else(|| "Cursor 导入 JSON 必须是对象".to_string())?;

    let email = extract_string(obj, &["email", "cachedEmail", "cursor_email"])
        .ok_or_else(|| "缺少 email 字段".to_string())?;
    let access_token = extract_string(
        obj,
        &[
            "access_token",
            "accessToken",
            "token",
            "cursor_access_token",
        ],
    )
    .map(|raw| normalize_import_access_token(&raw))
    .filter(|token| !token.is_empty())
    .ok_or_else(|| "缺少 access_token 字段".to_string())?;

    let name = extract_string(obj, &["name", "displayName"]);
    let refresh_token = extract_string(
        obj,
        &["refresh_token", "refreshToken", "cursor_refresh_token"],
    );
    validate_import_access_token(access_token.as_str(), refresh_token.as_deref())?;
    let membership_type = extract_string(
        obj,
        &[
            "membership_type",
            "membershipType",
            "stripeMembershipType",
            "plan",
        ],
    );
    let subscription_status = extract_string(
        obj,
        &[
            "subscription_status",
            "subscriptionStatus",
            "stripeSubscriptionStatus",
        ],
    );
    let sign_up_type = extract_string(obj, &["sign_up_type", "signUpType", "cachedSignUpType"]);
    let status = extract_string(obj, &["status"]);
    let status_reason = extract_string(obj, &["status_reason", "statusReason"]);

    let cursor_auth_raw = clone_object_value(obj.get("cursor_auth_raw"))
        .or_else(|| clone_object_value(obj.get("cursorAuthRaw")));
    let cursor_usage_raw = clone_object_value(obj.get("cursor_usage_raw"))
        .or_else(|| clone_object_value(obj.get("cursorUsageRaw")));
    let auth_id = extract_string(obj, &["auth_id", "authId", "workos_id", "workosId"])
        .or_else(|| extract_auth_id_from_raw_value(cursor_auth_raw.as_ref()))
        .or_else(|| extract_auth_id_from_access_token(access_token.as_str()));

    Ok(CursorImportPayload {
        email,
        auth_id,
        name,
        access_token,
        refresh_token,
        membership_type,
        subscription_status,
        sign_up_type,
        cursor_auth_raw,
        cursor_usage_raw,
        status,
        status_reason,
    })
}

fn payloads_from_import_json_value(value: Value) -> Result<Vec<CursorImportPayload>, String> {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Err("导入数组为空".to_string());
            }
            let mut payloads = Vec::with_capacity(items.len());
            for (idx, item) in items.into_iter().enumerate() {
                let payload = payload_from_import_value(item)
                    .map_err(|e| format!("第 {} 条 Cursor 账号解析失败: {}", idx + 1, e))?;
                payloads.push(payload);
            }
            Ok(payloads)
        }
        Value::Object(mut obj) => {
            let object_value = Value::Object(obj.clone());
            if let Ok(payload) = payload_from_import_value(object_value) {
                return Ok(vec![payload]);
            }

            if let Some(accounts) = obj
                .remove("accounts")
                .or_else(|| obj.remove("items"))
                .and_then(|raw| raw.as_array().cloned())
            {
                if accounts.is_empty() {
                    return Err("导入数组为空".to_string());
                }
                let mut payloads = Vec::with_capacity(accounts.len());
                for (idx, item) in accounts.into_iter().enumerate() {
                    let payload = payload_from_import_value(item)
                        .map_err(|e| format!("第 {} 条 Cursor 账号解析失败: {}", idx + 1, e))?;
                    payloads.push(payload);
                }
                return Ok(payloads);
            }

            Err("无法解析 Cursor 导入对象".to_string())
        }
        _ => Err("Cursor 导入 JSON 必须是对象或数组".to_string()),
    }
}

pub fn import_from_json(json_content: &str) -> Result<Vec<CursorAccount>, String> {
    if let Ok(account) = serde_json::from_str::<CursorAccount>(json_content) {
        let saved = upsert_account_record(account)?;
        return Ok(vec![saved]);
    }

    if let Ok(accounts) = serde_json::from_str::<Vec<CursorAccount>>(json_content) {
        let mut result = Vec::new();
        for account in accounts {
            let saved = upsert_account_record(account)?;
            result.push(saved);
        }
        return Ok(result);
    }

    if let Ok(value) = serde_json::from_str::<Value>(json_content) {
        if let Ok(payloads) = payloads_from_import_json_value(value) {
            let mut result = Vec::with_capacity(payloads.len());
            for payload in payloads {
                let saved = upsert_account(payload)?;
                result.push(saved);
            }
            return Ok(result);
        }
    }

    Err("无法解析 JSON 内容".to_string())
}

pub fn export_accounts(account_ids: &[String]) -> Result<String, String> {
    let accounts: Vec<CursorAccount> = account_ids
        .iter()
        .filter_map(|id| load_account(id))
        .collect();
    serde_json::to_string_pretty(&accounts).map_err(|e| format!("序列化失败: {}", e))
}

// ---------------------------------------------------------------------------
// Local import (read from Cursor's state.vscdb)
// ---------------------------------------------------------------------------

pub fn get_default_cursor_data_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().ok_or("无法获取用户主目录")?;
        return Ok(home.join("Library/Application Support/Cursor"));
    }

    #[cfg(target_os = "windows")]
    {
        let appdata =
            std::env::var("APPDATA").map_err(|_| "无法获取 APPDATA 环境变量".to_string())?;
        return Ok(PathBuf::from(appdata).join("Cursor"));
    }

    #[cfg(target_os = "linux")]
    {
        let home = dirs::home_dir().ok_or("无法获取用户主目录")?;
        return Ok(home.join(".config/Cursor"));
    }

    #[allow(unreachable_code)]
    Err("Cursor 账号导入仅支持 macOS、Windows 和 Linux".to_string())
}

pub fn get_default_cursor_state_db_path() -> Result<PathBuf, String> {
    Ok(get_default_cursor_data_dir()?
        .join("User")
        .join("globalStorage")
        .join("state.vscdb"))
}

pub fn read_local_cursor_auth() -> Result<Option<CursorImportPayload>, String> {
    let db_path = get_default_cursor_state_db_path()?;
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(&db_path)
        .map_err(|e| format!("打开 Cursor 本地数据库失败({}): {}", db_path.display(), e))?;
    read_local_cursor_auth_from_conn(&conn)
        .map_err(|error| format!("读取 Cursor 本地登录信息失败({}): {}", db_path.display(), error))
}

fn read_local_cursor_auth_from_conn(
    conn: &Connection,
) -> rusqlite::Result<Option<CursorImportPayload>> {
    // Read all login fields in one snapshot while Cursor may be switching accounts.
    let mut statement = conn.prepare(
        "SELECT key, CAST(value AS TEXT) FROM ItemTable WHERE key IN (
            'cursorAuth/accessToken', 'cursorAuth/cachedEmail', 'cursorAuth/refreshToken',
            'cursorAuth/authId', 'cursorAuth/stripeMembershipType',
            'cursorAuth/stripeSubscriptionStatus', 'cursorAuth/cachedSignUpType')",
    )?;
    let mut fields = HashMap::new();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let value: Option<String> = row.get(1)?;
        if let Some(value) = value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fields.insert(row.get::<_, String>(0)?, value.to_string());
        }
    }
    let access_token = match fields.remove("cursorAuth/accessToken") {
        Some(token) => token,
        None => return Ok(None),
    };
    let Some(email) = fields.remove("cursorAuth/cachedEmail") else {
        return Ok(None);
    };
    let refresh_token = fields.remove("cursorAuth/refreshToken");
    let auth_id = extract_auth_id_from_access_token(&access_token)
        .or_else(|| fields.remove("cursorAuth/authId"));
    let membership_type = fields.remove("cursorAuth/stripeMembershipType");
    let subscription_status = fields.remove("cursorAuth/stripeSubscriptionStatus");
    let sign_up_type = fields.remove("cursorAuth/cachedSignUpType");

    let mut auth_raw = serde_json::Map::new();
    auth_raw.insert(
        "accessToken".to_string(),
        Value::String(access_token.clone()),
    );
    if let Some(ref rt) = refresh_token {
        auth_raw.insert("refreshToken".to_string(), Value::String(rt.clone()));
    }
    if let Some(ref auth_id_value) = auth_id {
        auth_raw.insert("authId".to_string(), Value::String(auth_id_value.clone()));
    }
    auth_raw.insert("cachedEmail".to_string(), Value::String(email.clone()));
    if let Some(ref mt) = membership_type {
        auth_raw.insert(
            "stripeMembershipType".to_string(),
            Value::String(mt.clone()),
        );
    }
    if let Some(ref ss) = subscription_status {
        auth_raw.insert(
            "stripeSubscriptionStatus".to_string(),
            Value::String(ss.clone()),
        );
    }
    if let Some(ref st) = sign_up_type {
        auth_raw.insert("cachedSignUpType".to_string(), Value::String(st.clone()));
    }

    Ok(Some(CursorImportPayload {
        email,
        auth_id,
        name: None,
        access_token,
        refresh_token,
        membership_type,
        subscription_status,
        sign_up_type,
        cursor_auth_raw: Some(Value::Object(auth_raw)),
        cursor_usage_raw: None,
        status: None,
        status_reason: None,
    }))
}

pub fn import_from_local() -> Result<Option<CursorAccount>, String> {
    let payload = match read_local_cursor_auth()? {
        Some(p) => p,
        None => return Ok(None),
    };
    let account = upsert_account_with_source(payload, true)?;
    logger::log_info(&format!(
        "[Cursor Account] 从本地导入成功: id={}, email={}",
        account.id, account.email
    ));
    Ok(Some(account))
}

// ---------------------------------------------------------------------------
// Inject (write auth fields back to Cursor's state.vscdb)
// ---------------------------------------------------------------------------

fn read_vscdb_item(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?1",
        [key],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

fn upsert_vscdb_item(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
        (key, value),
    )
    .map_err(|e| format!("写入 {} 失败: {}", key, e))?;
    Ok(())
}

fn delete_vscdb_item(conn: &Connection, key: &str) -> Result<(), String> {
    conn.execute("DELETE FROM ItemTable WHERE key = ?1", (key,))
        .map_err(|e| format!("删除 {} 失败: {}", key, e))?;
    Ok(())
}

/// 账号没有该字段时删除键，避免把上一个账号的值留在 state.vscdb 里。
fn upsert_or_delete_vscdb_item(
    conn: &Connection,
    key: &str,
    value: Option<&str>,
) -> Result<(), String> {
    match normalize_non_empty(value) {
        Some(text) => upsert_vscdb_item(conn, key, &text),
        None => delete_vscdb_item(conn, key),
    }
}

/// Cursor 桌面端的 `cursorAuth/refreshToken` 就是登录时拿到的 session JWT（与 accessToken 相同），
/// 启动时靠它续期；缺失会被视为未登录。粘贴 token 添加的账号没有单独的 refresh_token，
/// 只要 access token 是 session 类型就用它自己补位。
fn access_token_type(access_token: &str) -> Option<String> {
    decode_access_token_payload(access_token)
        .and_then(|payload| payload.get("type").and_then(|v| v.as_str()).map(str::to_string))
}

fn access_token_is_session(access_token: &str) -> bool {
    access_token_type(access_token)
        .map(|kind| kind.eq_ignore_ascii_case("session"))
        .unwrap_or(false)
}

/// Cursor 桌面端只接受 `type=session` 的 JWT；从浏览器 Cookie 复制来的 `type=web` token
/// 能查配额，但写进 state.vscdb 后 Cursor 会直接弹登录。切号前先拦住，给出可操作的提示。
pub fn ensure_token_usable_for_desktop(account: &CursorAccount) -> Result<(), String> {
    match access_token_type(&account.access_token) {
        Some(kind) if !kind.eq_ignore_ascii_case("session") => Err(format!(
            "账号 {} 的 token 是 {} 类型（网页会话），Cursor 桌面端无法用它登录。请用 OAuth 重新登录该账号，或改用从 Cursor 客户端导出的 session token（user_xxx::eyJ…）。",
            display_email(account),
            kind
        )),
        _ => Ok(()),
    }
}

pub fn is_token_usable_for_desktop(account: &CursorAccount) -> bool {
    ensure_token_usable_for_desktop(account).is_ok()
}

/// 优先级：session 类型的 refresh_token → session 类型的 access_token → 其他 refresh_token。
/// Cursor 桌面端的 refreshToken 槽位只认 session JWT；把 web cookie JWT 或别的 token 放进去
/// 会让它在启动续期时失败。
pub(crate) fn resolve_refresh_token_for_injection(account: &CursorAccount) -> Option<String> {
    let stored = normalize_non_empty(account.refresh_token.as_deref());
    if let Some(token) = stored.as_deref() {
        if access_token_is_session(token) {
            return stored;
        }
    }
    if access_token_is_session(&account.access_token) {
        return Some(account.access_token.clone());
    }
    stored
}

/// Cursor 自己写入的 userId/authId 都是 `auth0|user_xxx`；GetUserMeta 给的 workosId 没有前缀，
/// 写入前统一补齐，避免同一账号在 DB 里出现两种写法。
fn normalize_auth0_user_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with("user_") {
        format!("auth0|{}", trimmed)
    } else {
        trimmed.to_string()
    }
}

/// 切号时删掉的上一账号缓存：套餐/订阅/团队/显示名由 Cursor 启动后用新 token 重新拉取，
/// 写旧值只会造成顶栏套餐串号。
const CURSOR_STALE_PROFILE_KEYS: [&str; 5] = [
    "cursorAuth/cachedScopedProfile",
    "cursorAuth/cachedTeam",
    "cursorAuth/stripeCustomerId",
    "cursorAuth/stripeMembershipType",
    "cursorAuth/stripeSubscriptionStatus",
];

/// 组装切号需要写入的键值。`None` 表示该键应删除。
fn build_auth_key_writes(account: &CursorAccount) -> Vec<(&'static str, Option<String>)> {
    let auth_id = resolve_account_auth_id(account).map(|id| normalize_auth0_user_id(&id));
    let refresh_token = resolve_refresh_token_for_injection(account);
    // Cursor 自己总会写这个键；未知时按邮箱注册处理，与官方默认一致。
    let sign_up_type = normalize_non_empty(account.sign_up_type.as_deref())
        .unwrap_or_else(|| "Auth_0".to_string());

    let mut writes: Vec<(&'static str, Option<String>)> = vec![
        ("cursorAuth/accessToken", Some(account.access_token.clone())),
        ("cursorAuth/refreshToken", refresh_token),
        ("cursorAuth/cachedEmail", Some(account.email.clone())),
        ("cursorAuth/email", Some(account.email.clone())),
        ("cursorAuth/cachedSignUpType", Some(sign_up_type)),
        ("cursorAuth/authId", auth_id.clone()),
        ("cursorAuth/userId", auth_id.clone()),
        ("cursorAuth/cachedUserId", auth_id.clone()),
        ("cursorAuth/stripeMembershipAuthId", auth_id),
        ("cursor.accessToken", Some(account.access_token.clone())),
        ("cursor.email", Some(account.email.clone())),
    ];
    for key in CURSOR_STALE_PROFILE_KEYS {
        writes.push((key, None));
    }
    writes
}

/// Cursor 会用 accessToken/refreshToken/userId 等键判断当前登录身份，切号时必须整组覆盖，
/// 否则 token 是新账号的、身份键还是旧账号的，导致"当前账号"识别错位、本地导入串号。
fn write_account_auth_to_vscdb(conn: &Connection, account: &CursorAccount) -> Result<(), String> {
    for (key, value) in build_auth_key_writes(account) {
        upsert_or_delete_vscdb_item(conn, key, value.as_deref())?;
    }
    verify_vscdb_auth_written(conn, account)
}

/// 写完立刻回读，确认落盘的是目标账号的 token。SQLite 写入本身很少失败，但如果 Cursor
/// 仍在运行并持有连接，可能出现写入被它的事务覆盖的情况，与其让用户看到"切换成功"却
/// 弹登录，不如在这里直接报错。
fn verify_vscdb_auth_written(conn: &Connection, account: &CursorAccount) -> Result<(), String> {
    let stored = read_vscdb_item(conn, "cursorAuth/accessToken").unwrap_or_default();
    if stored == account.access_token {
        return Ok(());
    }
    Err(format!(
        "写入 state.vscdb 后回读校验失败：accessToken 与目标账号 {} 不一致，可能有 Cursor 进程仍在运行并覆盖了写入，请完全退出 Cursor 后重试",
        display_email(account)
    ))
}

/// 部分 Cursor 版本会从 globalStorage/storage.json 恢复登录态；文件存在时同步写入，
/// 避免 state.vscdb 与它不一致被回滚。文件不存在则跳过。
fn write_account_auth_to_storage_json(
    global_storage_dir: &std::path::Path,
    account: &CursorAccount,
) -> Result<(), String> {
    let path = global_storage_dir.join("storage.json");
    if !path.exists() {
        return Ok(());
    }
    let content =
        fs::read_to_string(&path).map_err(|e| format!("读取 storage.json 失败: {}", e))?;
    let mut data = serde_json::from_str::<Value>(&content)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (key, value) in build_auth_key_writes(account) {
        match value {
            Some(text) => {
                data.insert(key.to_string(), Value::String(text));
            }
            None => {
                data.remove(key);
            }
        }
    }
    let serialized = serde_json::to_string_pretty(&Value::Object(data))
        .map_err(|e| format!("序列化 storage.json 失败: {}", e))?;
    crate::modules::atomic_write::write_string_atomic(&path, &serialized)
        .map_err(|e| format!("写入 storage.json 失败: {}", e))
}

/// 切号写入成功后同时更新 last_used 与 last_switched_at。凭据已经写进 Cursor，
/// 这里写失败只记日志，不让切号整体报失败。
fn touch_account_last_used(account_id: &str) {
    let Some(mut account) = load_account(account_id) else {
        return;
    };
    let now = now_ts();
    account.last_used = now;
    account.last_switched_at = Some(now);
    if let Err(err) = upsert_account_record(account) {
        logger::log_warn(&format!(
            "[Cursor Account] 更新 last_used 失败: id={}, error={}",
            account_id, err
        ));
    }
}

pub fn inject_to_cursor(account_id: &str) -> Result<(), String> {
    let account =
        load_account(account_id).ok_or_else(|| format!("Cursor 账号不存在: {}", account_id))?;
    ensure_token_usable_for_desktop(&account)?;
    let db_path = get_default_cursor_state_db_path()?;
    if !db_path.exists() {
        return Err(format!("Cursor state.vscdb 不存在: {}", db_path.display()));
    }

    let conn =
        Connection::open(&db_path).map_err(|e| format!("打开 Cursor 本地数据库失败: {}", e))?;

    write_account_auth_to_vscdb(&conn, &account)?;
    if let Some(dir) = db_path.parent() {
        if let Err(err) = write_account_auth_to_storage_json(dir, &account) {
            logger::log_warn(&format!(
                "[Cursor Account] storage.json 同步失败（state.vscdb 已写入）: id={}, error={}",
                account.id, err
            ));
        }
    }
    touch_account_last_used(account_id);

    logger::log_info(&format!(
        "[Cursor Account] 注入成功: id={}, email={}",
        account.id, account.email
    ));
    Ok(())
}

pub fn inject_to_cursor_at_path(db_path: &std::path::Path, account_id: &str) -> Result<(), String> {
    let account =
        load_account(account_id).ok_or_else(|| format!("Cursor 账号不存在: {}", account_id))?;
    ensure_token_usable_for_desktop(&account)?;
    if !db_path.exists() {
        return Err(format!("Cursor state.vscdb 不存在: {}", db_path.display()));
    }

    let conn =
        Connection::open(db_path).map_err(|e| format!("打开 Cursor 本地数据库失败: {}", e))?;

    write_account_auth_to_vscdb(&conn, &account)?;
    if let Some(dir) = db_path.parent() {
        if let Err(err) = write_account_auth_to_storage_json(dir, &account) {
            logger::log_warn(&format!(
                "[Cursor Account] storage.json 同步失败（state.vscdb 已写入）: id={}, path={}, error={}",
                account.id,
                db_path.display(),
                err
            ));
        }
    }
    if let Err(err) = touch_last_used(account_id) {
        logger::log_warn(&format!(
            "[Cursor Account] 更新 last_switched_at 失败: id={}, error={}",
            account_id, err
        ));
    }

    logger::log_info(&format!(
        "[Cursor Account] 注入成功(自定义路径): id={}, email={}, path={}",
        account.id,
        account.email,
        db_path.display()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Cursor usage API
// ---------------------------------------------------------------------------

pub(super) const CURSOR_AUTH_ERROR: &str = "Cursor 会话已过期或未认证，请重新导入账号";
/// 连续这么多次刷新在 Bearer 与 Cookie 两条鉴权路径上都被 401/403 拒绝，才把账号标成 error。
/// 单次失败可能只是网络抖动或 token 刚过期、下一轮就会由 refresh_token 续上。
const CURSOR_AUTH_FAILURE_MARK_THRESHOLD: u32 = 3;
const CURSOR_AUTH_FAILURE_STATUS_REASON: &str =
    "token auth failed repeatedly (HTTP 401/403); re-login or update the token";

const CURSOR_USAGE_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
const CURSOR_SAND_USAGE_URL: &str = "https://cursor.com/api/dashboard/get-sand-usage-status";
const CURSOR_GET_SAND_USAGE_STATUS_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetSandUsageStatus";
const CURSOR_GET_USER_META_URL: &str = "https://api2.cursor.sh/aiserver.v1.AuthService/GetUserMeta";
const CURSOR_FULL_STRIPE_PROFILE_URL: &str = "https://api2.cursor.sh/auth/full_stripe_profile";
const CURSOR_STRIPE_PROFILE_URL: &str = "https://api2.cursor.sh/auth/stripe_profile";
// 与官方 Cursor 客户端保持一致：使用 api2.cursor.sh/oauth/token 和内置 client_id 交换新 token。
const CURSOR_OAUTH_TOKEN_URL: &str = "https://api2.cursor.sh/oauth/token";
const CURSOR_AUTH_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUserMetaResponse {
    email: Option<String>,
    sign_up_type: Option<String>,
    workos_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorStripeProfileResponse {
    membership_type: Option<String>,
    individual_membership_type: Option<String>,
    subscription_status: Option<String>,
    team_membership_type: Option<String>,
    is_team_member: Option<bool>,
    is_enterprise: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct CursorRefreshTokenResponse {
    #[serde(alias = "accessToken")]
    access_token: Option<String>,
    #[serde(alias = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, alias = "shouldLogout")]
    should_logout: bool,
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(err) = source {
        let detail = err.to_string();
        if !detail.trim().is_empty() && parts.last().map(|item| item != &detail).unwrap_or(true) {
            parts.push(detail);
        }
        source = err.source();
    }
    parts.join(" | caused by: ")
}

pub(super) fn format_reqwest_error(error: &reqwest::Error) -> String {
    let mut tags = Vec::new();
    if error.is_timeout() {
        tags.push("timeout");
    }
    if error.is_connect() {
        tags.push("connect");
    }
    if error.is_request() {
        tags.push("request");
    }

    let detail = format_error_chain(error);
    if tags.is_empty() {
        detail
    } else {
        format!("{} [{}]", detail, tags.join(","))
    }
}

fn build_cursor_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", format_reqwest_error(&e)))
}

fn extract_workos_user_id(jwt: &str) -> Option<String> {
    let value = decode_access_token_payload(jwt)?;
    let sub = value.get("sub")?.as_str()?;
    let user_id = sub.rsplit('|').next().unwrap_or(sub);
    if user_id.starts_with("user_") {
        Some(user_id.to_string())
    } else {
        None
    }
}

pub(crate) fn normalize_pasted_access_token(input: &str) -> Result<String, String> {
    const INVALID_BROWSER_TOKEN: &str = "CURSOR_INVALID_BROWSER_TOKEN";
    let token = input.trim();
    let cookie_value = token.strip_prefix("WorkosCursorSessionToken=");
    let has_session_prefix =
        token.starts_with("user_") && (token.contains("::") || token.contains('%'));
    if cookie_value.is_none() && !has_session_prefix {
        return Ok(token.to_owned());
    }

    let decoded = urlencoding::decode(cookie_value.unwrap_or(token))
        .map_err(|_| INVALID_BROWSER_TOKEN.to_owned())?;
    let (user_id, jwt) = decoded
        .split_once("::")
        .ok_or_else(|| INVALID_BROWSER_TOKEN.to_owned())?;
    if extract_workos_user_id(jwt).as_deref() != Some(user_id)
        || jwt.split('.').count() != 3
        || jwt.split('.').any(str::is_empty)
        || jwt
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(INVALID_BROWSER_TOKEN.to_owned());
    }
    Ok(jwt.to_owned())
}

pub(crate) const CURSOR_SESSION_COOKIE_NAME: &str = "WorkosCursorSessionToken";

/// cursor.com 网页登录 Cookie 的值：`user_xxx%3A%3A<jwt>`（`::` 按站点惯例 URL 编码）。
pub(crate) fn build_session_cookie_value(access_token: &str) -> Option<String> {
    let user_id = extract_workos_user_id(access_token)?;
    Some(format!("{}%3A%3A{}", user_id, access_token))
}

fn build_session_cookie(access_token: &str) -> Option<String> {
    build_session_cookie_value(access_token)
        .map(|value| format!("{}={}", CURSOR_SESSION_COOKIE_NAME, value))
}

fn resolve_membership_from_stripe_profile(profile: &CursorStripeProfileResponse) -> Option<String> {
    let membership = normalize_non_empty(profile.membership_type.as_deref());
    let individual = normalize_non_empty(profile.individual_membership_type.as_deref());

    if let Some(individual_value) = individual.as_ref() {
        if !individual_value.eq_ignore_ascii_case("free")
            && !matches!(
                membership.as_deref(),
                Some(value) if value.eq_ignore_ascii_case("enterprise")
            )
        {
            return Some(individual_value.clone());
        }
    }

    membership.or(individual)
}

async fn exchange_refresh_token_with_client(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CursorRefreshTokenResponse, String> {
    let response = client
        .post(CURSOR_OAUTH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CURSOR_AUTH_CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor token 刷新接口失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| {
        format!(
            "读取 Cursor token 刷新响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    if status == 401 || status == 403 {
        return Err("Cursor refresh token 已过期或无效，请重新导入账号".to_string());
    }
    if status != 200 {
        let detail = body.trim();
        return Err(if detail.is_empty() {
            format!("Cursor token 刷新接口返回异常状态码: {}", status)
        } else {
            format!(
                "Cursor token 刷新接口返回异常状态码: {}, body_len={}",
                status,
                body.len()
            )
        });
    }

    serde_json::from_str::<CursorRefreshTokenResponse>(&body)
        .map_err(|e| format!("解析 Cursor token 刷新响应失败: {}", e))
}

async fn refresh_account_access_token_with_client(
    client: &reqwest::Client,
    account: &mut CursorAccount,
) -> Result<bool, String> {
    let Some(refresh_token) = normalize_non_empty(account.refresh_token.as_deref()) else {
        return Ok(false);
    };

    let response = exchange_refresh_token_with_client(client, refresh_token.as_str()).await?;
    if response.should_logout {
        return Err("Cursor refresh token 已失效，请重新导入账号".to_string());
    }

    let new_access_token = normalize_non_empty(response.access_token.as_deref())
        .ok_or_else(|| "Cursor token 刷新响应缺少 access_token".to_string())?;
    let new_refresh_token =
        normalize_non_empty(response.refresh_token.as_deref()).or(Some(refresh_token));

    account.access_token = new_access_token.clone();
    account.refresh_token = new_refresh_token.clone();
    upsert_cursor_auth_raw_string(account, "accessToken", Some(new_access_token));
    upsert_cursor_auth_raw_string(account, "refreshToken", new_refresh_token);
    Ok(true)
}

async fn fetch_user_meta_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<CursorUserMetaResponse, String> {
    let response = client
        .post(CURSOR_GET_USER_META_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("请求 Cursor user meta 失败: {}", format_reqwest_error(&e)))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if status != 200 {
        return Err(format!("Cursor user meta API 返回异常状态码: {}", status));
    }

    let body = response.text().await.map_err(|e| {
        format!(
            "读取 Cursor user meta 响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    serde_json::from_str::<CursorUserMetaResponse>(&body)
        .map_err(|e| format!("解析 Cursor user meta JSON 失败: {}", e))
}

async fn fetch_stripe_profile_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Option<CursorStripeProfileResponse>, String> {
    let full_response = client
        .get(CURSOR_FULL_STRIPE_PROFILE_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor full stripe profile 失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let full_status = full_response.status().as_u16();
    if full_status == 401 || full_status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if full_status == 200 {
        let body = full_response.text().await.map_err(|e| {
            format!(
                "读取 Cursor full stripe profile 响应失败: {}",
                format_reqwest_error(&e)
            )
        })?;
        let profile = serde_json::from_str::<CursorStripeProfileResponse>(&body)
            .map_err(|e| format!("解析 Cursor full stripe profile JSON 失败: {}", e))?;
        return Ok(Some(profile));
    }

    let fallback_response = client
        .get(CURSOR_STRIPE_PROFILE_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor stripe profile 失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let fallback_status = fallback_response.status().as_u16();
    if fallback_status == 401 || fallback_status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if fallback_status != 200 {
        return Ok(None);
    }

    let body = fallback_response.text().await.map_err(|e| {
        format!(
            "读取 Cursor stripe profile 响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    let parsed = serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| format!("解析 Cursor stripe profile JSON 失败: {}", e))?;

    match parsed {
        Value::Object(_) => serde_json::from_value::<CursorStripeProfileResponse>(parsed)
            .map(Some)
            .map_err(|e| format!("解析 Cursor stripe profile 对象失败: {}", e)),
        Value::String(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(CursorStripeProfileResponse {
                    membership_type: Some("pro".to_string()),
                    individual_membership_type: None,
                    subscription_status: None,
                    team_membership_type: None,
                    is_team_member: None,
                    is_enterprise: None,
                }))
            }
        }
        _ => Ok(None),
    }
}

async fn fetch_usage_summary_with_client(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<serde_json::Value, String> {
    let cookie = build_session_cookie(access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;

    let response = client
        .get(CURSOR_USAGE_SUMMARY_URL)
        .header("Accept", "application/json")
        .header("Cookie", &cookie)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        )
        .send()
        .await
        .map_err(|e| format!("请求 Cursor usage API 失败: {}", format_reqwest_error(&e)))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if status != 200 {
        return Err(format!("Cursor usage API 返回异常状态码: {}", status));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor usage 响应失败: {}", format_reqwest_error(&e)))?;

    serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| format!("解析 Cursor usage JSON 失败: {}", e))
}

// Grok Bot 是附加信息，不能拖慢主配额刷新；比全局 15s 更短，且两次尝试合计不超过主请求。
const CURSOR_SAND_USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

async fn fetch_sand_usage_via_rpc(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, String> {
    let response = client
        .post(CURSOR_GET_SAND_USAGE_STATUS_URL)
        .timeout(CURSOR_SAND_USAGE_TIMEOUT)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| {
            format!(
                "请求 Cursor Grok Bot RPC 失败: {}",
                format_reqwest_error(&e)
            )
        })?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if status != 200 {
        return Err(format!(
            "Cursor Grok Bot RPC 返回异常状态码: {}",
            status
        ));
    }

    let body = response.text().await.map_err(|e| {
        format!(
            "读取 Cursor Grok Bot RPC 响应失败: {}",
            format_reqwest_error(&e)
        )
    })?;

    serde_json::from_str::<Value>(&body)
        .map_err(|e| format!("解析 Cursor Grok Bot RPC JSON 失败: {}", e))
}

// ---------------------------------------------------------------------------
// Dashboard extras: per-model usage breakdown, on-demand spend limit
// ---------------------------------------------------------------------------

const CURSOR_AGGREGATED_USAGE_URL: &str =
    "https://cursor.com/api/dashboard/get-aggregated-usage-events";
const CURSOR_GET_HARD_LIMIT_URL: &str = "https://cursor.com/api/dashboard/get-hard-limit";
const CURSOR_SET_HARD_LIMIT_URL: &str = "https://cursor.com/api/dashboard/set-hard-limit";

/// 带 Cookie 会话的 Dashboard POST；所有 dashboard/* 接口的鉴权与错误处理一致。
async fn dashboard_post_json(
    client: &reqwest::Client,
    access_token: &str,
    url: &str,
    body: Value,
    what: &str,
) -> Result<Value, String> {
    let cookie = build_session_cookie(access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;

    let response = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Cookie", &cookie)
        .header("Origin", "https://cursor.com")
        .header("Referer", "https://cursor.com/dashboard")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        )
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 Cursor {} 失败: {}", what, format_reqwest_error(&e)))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if status != 200 {
        return Err(format!("Cursor {} 返回异常状态码: {}", what, status));
    }

    let text = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor {} 响应失败: {}", what, format_reqwest_error(&e)))?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("解析 Cursor {} JSON 失败: {}", what, e))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CursorModelUsage {
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 该模型本周期的成本（含套餐内抵扣部分）。
    pub total_cents: f64,
    /// 该模型本周期实际按需计费的金额；事件日志不可用时为 None。
    pub charged_cents: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CursorUsageBreakdown {
    pub start_ms: i64,
    pub end_ms: i64,
    pub total_cents: f64,
    /// 本周期按需实付合计；事件日志不可用时为 None。
    pub charged_cents: Option<f64>,
    /// Grok 相关事件（Grok Bot，以及 IDE 内 grok 模型）的按需实付合计。
    pub grok_charged_cents: Option<f64>,
    /// 事件日志是否完整拉取；超过分页上限时为 false，金额为下限。
    pub events_complete: bool,
    pub models: Vec<CursorModelUsage>,
}

fn parse_token_count(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn parse_cents(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

const CURSOR_FILTERED_USAGE_EVENTS_URL: &str =
    "https://cursor.com/api/dashboard/get-filtered-usage-events";
const CURSOR_USAGE_EVENTS_PAGE_SIZE: usize = 500;
const CURSOR_USAGE_EVENTS_MAX_PAGES: usize = 20;

#[derive(Debug, Default)]
struct ChargedUsageSummary {
    by_model: HashMap<String, f64>,
    total_cents: f64,
    grok_cents: f64,
    complete: bool,
    kinds: HashSet<String>,
}

fn is_grok_related_event(model: &str, kind: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let kind = kind.to_ascii_lowercase();
    model.contains("grok") || kind.contains("grok") || kind.contains("sand")
}

/// 逐页拉取本周期的用量事件，按模型汇总 `chargedCents`（按需实付）。
/// 聚合接口只有 totalCents（成本），区分"套餐内"与"按需实付"只能靠事件日志。
async fn fetch_charged_usage(
    client: &reqwest::Client,
    access_token: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<ChargedUsageSummary, String> {
    let mut summary = ChargedUsageSummary::default();
    let mut page = 1usize;
    let mut fetched = 0usize;
    let mut expected_total: Option<usize> = None;

    loop {
        let body = serde_json::json!({
            "teamId": -1,
            "startDate": start_ms.to_string(),
            "endDate": end_ms.to_string(),
            "page": page,
            "pageSize": CURSOR_USAGE_EVENTS_PAGE_SIZE,
        });
        let value = dashboard_post_json(
            client,
            access_token,
            CURSOR_FILTERED_USAGE_EVENTS_URL,
            body,
            "用量事件接口",
        )
        .await?;

        if expected_total.is_none() {
            expected_total = value
                .get("totalUsageEventsCount")
                .and_then(|v| match v {
                    Value::Number(n) => n.as_u64().map(|n| n as usize),
                    Value::String(s) => s.trim().parse::<usize>().ok(),
                    _ => None,
                });
        }

        let events = value
            .get("usageEventsDisplay")
            .or_else(|| value.get("usageEvents"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if events.is_empty() {
            summary.complete = true;
            break;
        }

        for event in &events {
            let model = event
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let kind = event
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !kind.is_empty() {
                summary.kinds.insert(kind.clone());
            }
            let charged = parse_cents(event.get("chargedCents"));
            if charged <= 0.0 {
                continue;
            }
            summary.total_cents += charged;
            *summary.by_model.entry(model.clone()).or_insert(0.0) += charged;
            if is_grok_related_event(&model, &kind) {
                summary.grok_cents += charged;
            }
        }

        fetched += events.len();
        if let Some(total) = expected_total {
            if fetched >= total {
                summary.complete = true;
                break;
            }
        }
        if events.len() < CURSOR_USAGE_EVENTS_PAGE_SIZE {
            summary.complete = true;
            break;
        }
        page += 1;
        if page > CURSOR_USAGE_EVENTS_MAX_PAGES {
            summary.complete = false;
            break;
        }
    }

    if !summary.kinds.is_empty() {
        let mut kinds: Vec<&String> = summary.kinds.iter().collect();
        kinds.sort();
        logger::log_info(&format!(
            "[Cursor Usage] 本周期事件类型: {} (events={}, charged=${:.2}, grok_charged=${:.2})",
            kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "),
            fetched,
            summary.total_cents / 100.0,
            summary.grok_cents / 100.0
        ));
    }
    Ok(summary)
}

/// 当前计费周期的按模型用量。周期取 usage-summary 里的 billingCycleStart/End，缺失时退回最近 30 天。
pub async fn fetch_usage_breakdown(account_id: &str) -> Result<CursorUsageBreakdown, String> {
    let account = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let cycle = account.cursor_usage_raw.as_ref().and_then(|raw| {
        let start = raw
            .get("billingCycleStart")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis());
        let end = raw
            .get("billingCycleEnd")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis());
        start.zip(end)
    });
    let (start_ms, end_ms) = cycle.unwrap_or((now_ms - 30 * 24 * 3600 * 1000, now_ms));

    let client = build_cursor_http_client()?;
    let body = serde_json::json!({
        "teamId": -1,
        "startDate": start_ms.to_string(),
        "endDate": end_ms.to_string(),
    });
    let value = dashboard_post_json(
        &client,
        &account.access_token,
        CURSOR_AGGREGATED_USAGE_URL,
        body,
        "按模型用量接口",
    )
    .await?;

    let mut models: Vec<CursorModelUsage> = value
        .get("aggregations")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let model = item
                        .get("modelIntent")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())?
                        .to_string();
                    Some(CursorModelUsage {
                        model,
                        input_tokens: parse_token_count(item.get("inputTokens")),
                        output_tokens: parse_token_count(item.get("outputTokens")),
                        cache_read_tokens: parse_token_count(item.get("cacheReadTokens")),
                        cache_write_tokens: parse_token_count(item.get("cacheWriteTokens")),
                        total_cents: item
                            .get("totalCents")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0),
                        charged_cents: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // 事件日志是附加信息：拉不到就只给成本，不让整个明细失败。
    let charged = match fetch_charged_usage(&client, &account.access_token, start_ms, end_ms).await
    {
        Ok(summary) => Some(summary),
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Usage] 用量事件拉取失败，按需实付不可用: id={}, error={}",
                account_id, err
            ));
            None
        }
    };
    if let Some(summary) = charged.as_ref() {
        for model in &mut models {
            model.charged_cents = Some(summary.by_model.get(&model.model).copied().unwrap_or(0.0));
        }
        // 事件里出现但聚合接口没有的模型（例如只有按需事件），补一行让实付对得上总数。
        for (model_name, cents) in &summary.by_model {
            if *cents > 0.0 && !models.iter().any(|m| &m.model == model_name) {
                models.push(CursorModelUsage {
                    model: model_name.clone(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    total_cents: 0.0,
                    charged_cents: Some(*cents),
                });
            }
        }
    }

    models.sort_by(|a, b| {
        b.total_cents
            .partial_cmp(&a.total_cents)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.charged_cents
                    .unwrap_or(0.0)
                    .partial_cmp(&a.charged_cents.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                (b.input_tokens + b.output_tokens).cmp(&(a.input_tokens + a.output_tokens))
            })
    });

    let total_cents = value
        .get("totalCostCents")
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| models.iter().map(|m| m.total_cents).sum());

    Ok(CursorUsageBreakdown {
        start_ms,
        end_ms,
        total_cents,
        charged_cents: charged.as_ref().map(|s| s.total_cents),
        grok_charged_cents: charged.as_ref().map(|s| s.grok_cents),
        events_complete: charged.as_ref().map(|s| s.complete).unwrap_or(false),
        models,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CursorHardLimit {
    /// 月度按需花费上限（美元）；None 表示未设置 / 无上限。
    pub hard_limit_dollars: Option<f64>,
    /// true 表示按需使用被关闭。
    pub no_usage_based_allowed: bool,
}

pub async fn fetch_hard_limit(account_id: &str) -> Result<CursorHardLimit, String> {
    let account = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    let client = build_cursor_http_client()?;
    let value = dashboard_post_json(
        &client,
        &account.access_token,
        CURSOR_GET_HARD_LIMIT_URL,
        serde_json::json!({}),
        "按需上限查询接口",
    )
    .await?;

    let hard_limit_dollars = value
        .get("hardLimit")
        .and_then(|v| v.as_f64())
        .filter(|v| *v > 0.0);
    let no_usage_based_allowed = value
        .get("noUsageBasedAllowed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Ok(CursorHardLimit {
        hard_limit_dollars,
        no_usage_based_allowed,
    })
}

/// 设置按需使用开关与月度上限。Cursor 接口以美元整数接收 `hardLimit`。
pub async fn update_hard_limit(
    account_id: &str,
    hard_limit_dollars: f64,
    no_usage_based_allowed: bool,
) -> Result<(), String> {
    if !hard_limit_dollars.is_finite() || hard_limit_dollars < 0.0 {
        return Err("按需上限必须是不小于 0 的数字".to_string());
    }
    let account = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    let client = build_cursor_http_client()?;
    dashboard_post_json(
        &client,
        &account.access_token,
        CURSOR_SET_HARD_LIMIT_URL,
        serde_json::json!({
            "hardLimit": hard_limit_dollars.round() as i64,
            "noUsageBasedAllowed": no_usage_based_allowed,
        }),
        "按需上限设置接口",
    )
    .await?;
    logger::log_info(&format!(
        "[Cursor Account] 按需上限已更新: id={}, hard_limit=${}, disabled={}",
        account_id,
        hard_limit_dollars.round() as i64,
        no_usage_based_allowed
    ));
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorSandUsageResponse {
    usage_percent: Option<f64>,
    included_limit_zero: Option<bool>,
    has_non_zero_included_limit: Option<bool>,
    next_reset_timestamp_utc: Option<String>,
    sand_trial_expires_at: Option<String>,
    sand_trial: Option<Value>,
}

impl CursorSandUsageResponse {
    fn usage(self, now: i64) -> Result<Option<CursorSandUsage>, String> {
        if self.included_limit_zero.is_none()
            && self.has_non_zero_included_limit.is_none()
            && self.sand_trial_expires_at.is_none()
            && self.sand_trial.is_none()
        {
            return Err("Cursor Sand 用量响应缺少额度状态".into());
        }
        let timestamp = |value: Option<&str>| {
            chrono::DateTime::parse_from_rfc3339(value?)
                .ok()
                .map(|date| date.timestamp())
        };
        let next_reset = timestamp(self.next_reset_timestamp_utc.as_deref());
        let included = self
            .included_limit_zero
            .map(|zero| !zero)
            .or(self.has_non_zero_included_limit)
            .unwrap_or(false);
        let trial_expiry = timestamp(self.sand_trial_expires_at.as_deref())
            .or_else(|| self.sand_trial.filter(Value::is_object).and(next_reset));
        let reset_at = if included {
            next_reset
        } else if let Some(expiry) = trial_expiry.filter(|expiry| *expiry > now) {
            Some(expiry)
        } else {
            return Ok(None);
        };
        let percent_used = self
            .usage_percent
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or_else(|| "Cursor Sand 用量响应缺少有效比例".to_string())?;
        Ok(Some(CursorSandUsage {
            percent_used,
            reset_at,
            stale: false,
        }))
    }
}

async fn fetch_sand_usage_with_client(
    client: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
) -> Result<Option<CursorSandUsage>, String> {
    let cookie = build_session_cookie(access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;
    let response = client
        .post(endpoint)
        .header("Accept", "application/json")
        .header("Origin", "https://cursor.com")
        .header("Cookie", cookie)
        .json(&serde_json::json!({}))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|err| format!("请求 Cursor Sand 用量失败: {}", format_reqwest_error(&err)))?;
    let status: CursorSandUsageResponse = response
        .json()
        .await
        .map_err(|err| format!("解析 Cursor Sand 用量失败: {}", format_reqwest_error(&err)))?;
    status.usage(now_ts())
}

/// Cookie 接口失败时改走 Bearer 的 RPC；两条路径都失败才算本次 Grok Bot 用量拉取失败。
async fn fetch_sand_usage_with_fallback(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Option<CursorSandUsage>, String> {
    let dashboard_err =
        match fetch_sand_usage_with_client(client, CURSOR_SAND_USAGE_URL, access_token).await {
            Ok(usage) => return Ok(usage),
            Err(err) => err,
        };
    match fetch_sand_usage_via_rpc(client, access_token).await {
        Ok(value) => serde_json::from_value::<CursorSandUsageResponse>(value)
            .map_err(|err| format!("解析 Cursor Grok Bot RPC 用量失败: {err}"))?
            .usage(now_ts()),
        Err(rpc_err) => Err(format!("{dashboard_err}; RPC: {rpc_err}")),
    }
}

// ---------------------------------------------------------------------------
// Refresh (updates our own account storage + fetches usage from official APIs)
// ---------------------------------------------------------------------------

/// `cursor_usage::dashboard_read` 的 401/403 错误带接口名前缀，按结尾匹配。
fn is_auth_error(message: &str) -> bool {
    message.ends_with(CURSOR_AUTH_ERROR)
}

fn is_auto_marked_auth_failure(account: &CursorAccount) -> bool {
    normalize_status_value(account.status.as_deref()).as_deref() == Some("error")
        && account.status_reason.as_deref() == Some(CURSOR_AUTH_FAILURE_STATUS_REASON)
}

/// 记一次鉴权失败；累计到阈值就把账号标成 error，让它进入"异常账号"筛选、退出切换候选。
fn record_auth_failure(account: &mut CursorAccount) {
    let failures = account.quota_query_auth_failures.unwrap_or(0).saturating_add(1);
    account.quota_query_auth_failures = Some(failures);
    if failures < CURSOR_AUTH_FAILURE_MARK_THRESHOLD || is_banned_account(account) {
        return;
    }
    if !is_auto_marked_auth_failure(account) {
        logger::log_warn(&format!(
            "[Cursor Refresh] 连续 {} 次鉴权失败，标记账号为 error: id={}, email={}",
            failures, account.id, account.email
        ));
    }
    account.status = Some("error".to_string());
    account.status_reason = Some(CURSOR_AUTH_FAILURE_STATUS_REASON.to_string());
}

/// 刷新成功即清零计数；若 error 状态是我们自动标的，也一并撤销（导入时带来的状态不动）。
fn clear_auth_failures(account: &mut CursorAccount) {
    account.quota_query_auth_failures = None;
    if is_auto_marked_auth_failure(account) {
        logger::log_info(&format!(
            "[Cursor Refresh] 鉴权恢复，撤销自动标记的 error 状态: id={}, email={}",
            account.id, account.email
        ));
        account.status = None;
        account.status_reason = None;
    }
}

async fn refresh_account_async_once(account_id: &str) -> Result<CursorAccount, String> {
    let existing = load_account(account_id).ok_or_else(|| "账号不存在".to_string())?;
    logger::log_info(&format!(
        "[Cursor Refresh] 开始刷新账号: id={}, email={}",
        existing.id, existing.email
    ));

    let client = build_cursor_http_client()?;
    let mut account = existing.clone();
    // 老数据里粘贴 token 添加的账号没有 refresh_token，顺手补上 session JWT。
    if account.refresh_token.is_none() && access_token_is_session(&account.access_token) {
        account.refresh_token = Some(account.access_token.clone());
    }

    if access_token_needs_refresh(&account.access_token) {
        match refresh_account_access_token_with_client(&client, &mut account).await {
            Ok(true) => {
                logger::log_info(&format!(
                    "[Cursor Refresh] access token 刷新成功: id={}",
                    account.id
                ));
            }
            Ok(false) => {}
            Err(err) => {
                logger::log_warn(&format!(
                    "[Cursor Refresh] access token 刷新失败，继续使用现有 token: id={}, error={}",
                    account.id, err
                ));
            }
        }
    }

    let mut bearer_auth_rejected = false;
    match fetch_user_meta_with_client(&client, &account.access_token).await {
        Ok(meta) => {
            if let Some(email) = normalize_email_identity(meta.email.as_deref()) {
                account.email = email.clone();
                upsert_cursor_auth_raw_string(&mut account, "cachedEmail", Some(email));
            }

            if let Some(sign_up_type) = normalize_cursor_sign_up_type(meta.sign_up_type.as_deref())
            {
                account.sign_up_type = Some(sign_up_type.clone());
                upsert_cursor_auth_raw_string(&mut account, "cachedSignUpType", Some(sign_up_type));
            }

            upsert_cursor_auth_raw_string(&mut account, "workosId", meta.workos_id.clone());
            reconcile_account_auth_id(&mut account, meta.workos_id.as_deref());

            logger::log_info(&format!(
                "[Cursor Refresh] 用户信息拉取成功: id={}, email={}",
                account.id, account.email
            ));
        }
        Err(err) => {
            bearer_auth_rejected = is_auth_error(&err);
            logger::log_warn(&format!(
                "[Cursor Refresh] 用户信息拉取失败: id={}, error={}",
                account.id, err
            ));
        }
    }
    // 即使用户信息拉取失败，也用 token 自身的身份兜底校正。
    reconcile_account_auth_id(&mut account, None);

    match fetch_stripe_profile_with_client(&client, &account.access_token).await {
        Ok(Some(profile)) => {
            if let Some(membership_type) = resolve_membership_from_stripe_profile(&profile) {
                account.membership_type = Some(membership_type.clone());
                upsert_cursor_auth_raw_string(
                    &mut account,
                    "stripeMembershipType",
                    Some(membership_type),
                );
            }

            let subscription_status = normalize_non_empty(profile.subscription_status.as_deref());
            if let Some(status) = subscription_status.clone() {
                account.subscription_status = Some(status);
            }
            upsert_cursor_auth_raw_string(
                &mut account,
                "stripeSubscriptionStatus",
                subscription_status,
            );
            upsert_cursor_auth_raw_string(
                &mut account,
                "teamMembershipType",
                normalize_non_empty(profile.team_membership_type.as_deref()),
            );
            upsert_cursor_auth_raw_bool(&mut account, "isTeamMember", profile.is_team_member);
            upsert_cursor_auth_raw_bool(&mut account, "isEnterprise", profile.is_enterprise);

            logger::log_info(&format!(
                "[Cursor Refresh] 订阅信息拉取成功: id={}",
                account.id
            ));
        }
        Ok(None) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] 未获取到订阅信息: id={}",
                account.id
            ));
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] 订阅信息拉取失败: id={}, error={}",
                account.id, err
            ));
        }
    }

    let usage_result = refresh_usage_snapshot(&client, &mut account).await;
    let usage_refreshed = usage_result.is_ok();
    match usage_result {
        Ok(()) => {
            account.quota_query_last_error = None;
            account.quota_query_last_error_at = None;
            clear_auth_failures(&mut account);
        }
        Err(err) => {
            logger::log_warn(&format!("[Cursor Refresh] 配额拉取失败: id={}, error={err}", account.id));
            // 用户信息与用量两条请求同时被拒，才算一次真正的鉴权失败；
            // 只有一条失败更可能是接口抖动，不计入。
            if is_auth_error(&err) && bearer_auth_rejected {
                record_auth_failure(&mut account);
            }
            account.quota_query_last_error = Some(err);
            account.quota_query_last_error_at = Some(chrono::Utc::now().timestamp_millis());
        }
    }

    let refreshed_at = now_ts();
    if usage_refreshed {
        account.usage_updated_at = Some(refreshed_at);
    }
    account.last_used = refreshed_at;
    let observed_at = usage_refreshed.then(|| chrono::Utc::now().timestamp_millis());
    let updated = tauri::async_runtime::spawn_blocking(move || commit_cursor_refresh(existing, account, observed_at))
        .await
        .map_err(|error| format!("保存 Cursor 刷新结果失败: {error}"))??;
    logger::log_info(&format!(
        "[Cursor Refresh] 刷新完成: id={}, email={}",
        updated.id, updated.email
    ));
    Ok(updated)
}

async fn refresh_usage_snapshot(
    client: &reqwest::Client,
    account: &mut CursorAccount,
) -> Result<(), String> {
    use super::cursor_usage::{dashboard_read, read_credits};
    use serde_json::json;

    let token = &account.access_token;
    let (plan_result, period_usage) = tokio::join!(
        dashboard_read(client, token, "GetPlanInfo", json!({})),
        dashboard_read(client, token, "GetCurrentPeriodUsage", json!({})),
    );
    let previous = account.cursor_usage_raw.as_ref().unwrap_or(&Value::Null);
    let plan = match &plan_result {
        Ok(plan) => plan,
        Err(err) => {
            logger::log_warn(&format!("[Cursor Refresh] {err}"));
            previous
        }
    };
    let enterprise = account
        .cursor_auth_raw
        .as_ref()
        .and_then(|raw| raw["isEnterprise"].as_bool())
        == Some(true);
    let unified = is_unified_usage(plan, account.membership_type.as_deref());
    let (legacy_summary, sand, credits) = tokio::join!(
        async {
            if unified {
                Ok(None)
            } else {
                fetch_usage_summary_with_client(client, token)
                    .await
                    .map(Some)
            }
        },
        async {
            if unified {
                Ok(None)
            } else {
                fetch_sand_usage_with_fallback(client, token).await
            }
        },
        async {
            if unified && !enterprise {
                Some(read_credits(client, token).await)
            } else {
                None
            }
        },
    );
    let legacy_summary = match legacy_summary {
        Ok(summary) => summary,
        Err(err) => {
            logger::log_warn(&format!("[Cursor Refresh] 旧版用量详情拉取失败: {err}"));
            None
        }
    };
    let legacy_summary = legacy_summary.filter(Value::is_object);
    let mut usage = match (period_usage, legacy_summary.as_ref()) {
        (Ok(usage), _) => usage,
        (Err(err), Some(summary)) => {
            logger::log_warn(&format!("[Cursor Refresh] 使用旧版用量数据: {err}"));
            summary.clone()
        }
        (Err(err), None) => return Err(err),
    };
    if let Some(summary) = &legacy_summary {
        if let Some(mt) = summary["membershipType"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            account.membership_type = Some(mt.to_string());
        }
        // Preserve the legacy personal/team spending settings alongside the
        // desktop's included usage. The client response has no enabled flag.
        for key in [
            "individualUsage",
            "individual_usage",
            "teamUsage",
            "team_usage",
            "limitType",
            "limit_type",
            "isUnlimited",
            "is_unlimited",
            "unifiedIncludedUsage",
            "unified_included_usage",
            "includedUsagePeriod",
            "included_usage_period",
            "includedUsageResetsAt",
            "included_usage_resets_at",
        ] {
            if let Some(value) = summary.get(key) {
                usage[key] = value.clone();
            }
        }
    }
    if let Some(info) = plan.get("planInfo").filter(|info| info.is_object()) {
        usage["planInfo"] = info.clone();
    }
    if let Some(merged) = plan["isMerged"].as_bool() {
        usage["isMerged"] = json!(merged && !enterprise);
    }
    if let Some(credits) = credits {
        usage["credits"] = credits;
    }
    match sand {
        Ok(usage) => account.sand_usage = usage,
        Err(err) => {
            if let Some(usage) = &mut account.sand_usage {
                usage.stale = true;
            }
            logger::log_warn(&format!("[Cursor Refresh] Sand 用量拉取失败: {err}"));
        }
    }
    account.cursor_usage_raw = Some(usage);
    Ok(())
}

fn included_usage_period(raw: &Value) -> Option<&Value> {
    raw.get("planInfo")
        .or_else(|| raw.get("plan_info"))
        .and_then(|info| {
            info.get("includedUsagePeriod")
                .or_else(|| info.get("included_usage_period"))
        })
        .or_else(|| raw.get("includedUsagePeriod").or_else(|| raw.get("included_usage_period")))
}

fn is_monthly_period(period: Option<&Value>) -> bool {
    matches!(
        period.and_then(Value::as_str),
        Some("INCLUDED_USAGE_PERIOD_MONTHLY" | "monthly" | "1")
    ) || period.and_then(Value::as_i64) == Some(1)
}

fn is_unified_usage(raw: &Value, membership: Option<&str>) -> bool {
    let info = raw.get("planInfo").or_else(|| raw.get("plan_info"));
    let period = included_usage_period(raw);
    if raw["isMerged"].as_bool() == Some(true)
        || matches!(
            period.and_then(Value::as_str),
            Some("INCLUDED_USAGE_PERIOD_WEEKLY" | "weekly" | "2")
        )
        || period.and_then(Value::as_i64) == Some(2)
    {
        return true;
    }
    if is_monthly_period(period) {
        return false;
    }
    membership.is_some_and(|tier| {
        let tier = tier.trim().to_ascii_lowercase();
        tier == "super" || tier.starts_with("unified_")
    })
        || ["unifiedIncludedUsage", "unified_included_usage"]
            .iter()
            .any(|key| raw[*key].as_bool() == Some(true))
        || ["includedUsageResetsAt", "included_usage_resets_at"]
            .iter()
            .any(|key| {
                info.and_then(|info| info.get(*key))
                    .or_else(|| raw.get(*key))
                    .is_some_and(|value| !value.is_null())
            })
}

pub(super) struct CursorWeeklyTrayUsage {
    pub(super) used_percent: Option<i32>,
    pub(super) reset_at: Option<i64>,
    pub(super) paid_monthly_cents: Option<i64>,
}

pub(super) fn read_weekly_tray_usage(account: &CursorAccount) -> Option<CursorWeeklyTrayUsage> {
    let raw = account.cursor_usage_raw.as_ref().unwrap_or(&Value::Null);
    if is_monthly_period(included_usage_period(raw))
        || !is_unified_usage(raw, account.membership_type.as_deref())
    {
        return None;
    }
    let plan = raw.get("planInfo").or_else(|| raw.get("plan_info"));
    let reset_keys = ["includedUsageResetsAt", "included_usage_resets_at"];
    let reset_at = plan
        .and_then(|plan| cursor_reset_millis(plan, &reset_keys))
        .or_else(|| cursor_reset_millis(raw, &reset_keys))
        .map(|millis| millis / 1000);
    Some(CursorWeeklyTrayUsage {
        used_percent: read_usage_percent(account).total_used,
        reset_at,
        paid_monthly_cents: super::cursor_usage::json_integer(raw["credits"].get("monthlyUsedCents")),
    })
}

fn commit_cursor_refresh(
    previous: CursorAccount,
    mut refreshed: CursorAccount,
    observed_at: Option<i64>,
) -> Result<CursorAccount, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let current = load_account(&previous.id).ok_or_else(|| "账号不存在".to_string())?;
    // A quota request can finish after deletion, another refresh, or re-login.
    // Keep the account currently in storage if the request used old credentials.
    if current.created_at != previous.created_at
        || current.access_token != previous.access_token
        || current.refresh_token != previous.refresh_token
    {
        return Ok(current);
    }
    refreshed.tags = current.tags;
    refreshed.last_switched_at = current.last_switched_at;
    refreshed.last_used = refreshed.last_used.max(current.last_used);
    let mut index = load_account_index_checked()?;
    save_account_file(&refreshed)?;
    refresh_summary(&mut index, &refreshed);
    save_account_index(&index)?;
    drop(_lock);
    if let Some(observed_at) = observed_at {
        use super::platform_quota_history::{self, Platform};
        platform_quota_history::record(
            Platform::Cursor,
            &refreshed.id,
            refreshed.created_at,
            observed_at,
            &cursor_quota_history_metrics(&refreshed),
        );
    }
    Ok(refreshed)
}

pub async fn refresh_account_async(account_id: &str) -> Result<CursorAccount, String> {
    let result = refresh_account_async_once(account_id).await;
    if let Err(err) = &result {
        persist_quota_query_error(account_id, err);
    }
    result
}

pub async fn refresh_all_tokens() -> Result<Vec<(String, Result<CursorAccount, String>)>, String> {
    let ids = list_accounts()
        .into_iter()
        .filter(|account| !is_banned_account(account))
        .map(|account| account.id)
        .collect();
    Ok(refresh_tokens_for_ids(ids).await)
}

pub async fn refresh_tokens_for_ids(ids: Vec<String>) -> Vec<(String, Result<CursorAccount, String>)> {
    let mut results = Vec::with_capacity(ids.len());
    for id in ids {
        let result = refresh_account_async(&id).await;
        results.push((id, result));
    }
    results
}

// ---------------------------------------------------------------------------
// Quota alert
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct CursorUsagePercent {
    total_used: Option<i32>,
    auto_used: Option<i32>,
    api_used: Option<i32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CursorGrokBotWeekly {
    pub used_percent: Option<i32>,
    pub reset_ts: Option<i64>,
}

fn clamp_percent(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    if value <= 0.0 {
        return 0;
    }
    if value >= 100.0 {
        return 100;
    }
    value.round() as i32
}

fn pick_number(value: Option<&Value>, keys: &[&str]) -> Option<f64> {
    let obj = value?.as_object()?;
    for key in keys {
        let Some(raw) = obj.get(*key) else {
            continue;
        };
        if let Some(n) = raw.as_f64() {
            if n.is_finite() {
                return Some(n);
            }
            continue;
        }
        if let Some(text) = raw.as_str() {
            if let Ok(parsed) = text.trim().parse::<f64>() {
                if parsed.is_finite() {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

fn pick_bool(value: Option<&Value>, keys: &[&str]) -> Option<bool> {
    let obj = value?.as_object()?;
    for key in keys {
        let Some(raw) = obj.get(*key) else {
            continue;
        };
        if let Some(flag) = raw.as_bool() {
            return Some(flag);
        }
        if let Some(text) = raw.as_str() {
            let normalized = text.trim().to_ascii_lowercase();
            if normalized == "true" {
                return Some(true);
            }
            if normalized == "false" {
                return Some(false);
            }
        }
    }
    None
}

fn parse_cursor_timestamp(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(n) = value.as_f64() {
        if !n.is_finite() || n <= 0.0 {
            return None;
        }
        if n >= 1_000_000_000_000.0 {
            return Some((n / 1000.0).round() as i64);
        }
        return Some(n.round() as i64);
    }
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(trimmed) {
            return Some(parsed.timestamp());
        }
        if let Ok(n) = trimmed.parse::<f64>() {
            return parse_cursor_timestamp(Some(&Value::from(n)));
        }
    }
    None
}

fn grok_bot_from_object(value: Option<&Value>) -> CursorGrokBotWeekly {
    let has_included_limit = pick_bool(
        value,
        &[
            "hasNonZeroIncludedLimit",
            "has_non_zero_included_limit",
        ],
    );
    if has_included_limit == Some(false) {
        return CursorGrokBotWeekly::default();
    }

    let percent = pick_number(
        value,
        &[
            "usagePercent",
            "usedPercent",
            "percentUsed",
            "weeklyPercentUsed",
            "weekly_percent_used",
            "grokBotWeeklyPercentUsed",
            "grok_bot_weekly_percent_used",
        ],
    );
    let used = pick_number(value, &["used", "includedUsed", "included_used"]);
    let limit = pick_number(value, &["limit", "includedLimit", "included_limit"]);
    let ratio = match (used, limit) {
        (Some(used_val), Some(limit_val)) if limit_val > 0.0 => {
            Some((used_val / limit_val) * 100.0)
        }
        _ => None,
    };

    let reset_ts = value.and_then(|raw| raw.as_object()).and_then(|obj| {
        [
            "nextResetTimestampUtc",
            "next_reset_timestamp_utc",
            "resetsAt",
            "resetAt",
            "weeklyResetAt",
            "weekly_reset_at",
            "resetTime",
        ]
        .into_iter()
        .find_map(|key| parse_cursor_timestamp(obj.get(key)))
    });

    CursorGrokBotWeekly {
        used_percent: percent.or(ratio).map(clamp_percent),
        reset_ts,
    }
}

pub(crate) fn read_grok_bot_weekly(account: &CursorAccount) -> CursorGrokBotWeekly {
    if let Some(sand) = account
        .sand_usage
        .as_ref()
        .filter(|usage| usage.percent_used.is_finite())
    {
        return CursorGrokBotWeekly {
            used_percent: Some(clamp_percent(sand.percent_used)),
            reset_ts: sand.reset_at,
        };
    }
    // 旧版本把 Grok Bot 用量存在 cursor_usage_raw.sandUsage 里，下次刷新前继续读它。
    let Some(raw) = account.cursor_usage_raw.as_ref() else {
        return CursorGrokBotWeekly::default();
    };
    let Some(raw_obj) = raw.as_object() else {
        return CursorGrokBotWeekly::default();
    };

    let individual = raw_obj
        .get("individualUsage")
        .or_else(|| raw_obj.get("individual_usage"));
    let sand = raw_obj.get("sandUsage");
    let candidates = [
        sand,
        sand.and_then(|value| value.get("status")),
        sand.and_then(|value| value.get("usage")),
        raw_obj.get("grokBot"),
        raw_obj.get("grok_bot"),
        raw_obj.get("weeklyUsage"),
        raw_obj.get("weekly_usage"),
        individual.and_then(|value| value.get("grokBot")),
        individual.and_then(|value| value.get("grok_bot")),
        individual.and_then(|value| value.get("grok")),
        individual.and_then(|value| value.get("weeklyUsage")),
        individual.and_then(|value| value.get("weekly_usage")),
        raw_obj.get("plan").and_then(|value| value.get("grokBot")),
    ];

    for candidate in candidates {
        let parsed = grok_bot_from_object(candidate);
        if parsed.used_percent.is_some() {
            return parsed;
        }
    }

    CursorGrokBotWeekly::default()
}

fn read_usage_percent(account: &CursorAccount) -> CursorUsagePercent {
    let Some(raw) = account.cursor_usage_raw.as_ref() else {
        return CursorUsagePercent::default();
    };

    let raw_obj = match raw.as_object() {
        Some(value) => value,
        None => return CursorUsagePercent::default(),
    };

    let desktop_plan = raw_obj.get("planUsage").or_else(|| raw_obj.get("plan_usage"));
    if is_unified_usage(raw, account.membership_type.as_deref()) {
        return CursorUsagePercent {
            total_used: pick_number(desktop_plan, &["totalPercentUsed", "total_percent_used"])
                .map(clamp_percent),
            ..CursorUsagePercent::default()
        };
    }
    let plan_value = desktop_plan.or_else(|| {
        raw_obj.get("individualUsage")
            .or_else(|| raw_obj.get("individual_usage"))
            .and_then(|value| value.get("plan"))
    });

    let total_direct = pick_number(plan_value, &["totalPercentUsed", "total_percent_used"]);
    let auto_direct = pick_number(plan_value, &["autoPercentUsed", "auto_percent_used"]);
    let api_direct = pick_number(plan_value, &["apiPercentUsed", "api_percent_used"]);

    let used = pick_number(plan_value, &["includedSpend", "included_spend", "used", "totalSpend", "total_spend"]);
    let limit = pick_number(plan_value, &["limit"]);
    let total_ratio = match (used, limit) {
        (Some(used_val), Some(limit_val)) if limit_val > 0.0 => {
            Some((used_val / limit_val) * 100.0)
        }
        _ => None,
    };

    CursorUsagePercent {
        total_used: total_direct.or(total_ratio).map(clamp_percent),
        auto_used: auto_direct.map(clamp_percent),
        api_used: api_direct.map(clamp_percent),
    }
}

/// IDE 额度池（剩余百分比），用于账号推荐、托盘/菜单排序等平均值计算。
pub(crate) fn extract_quota_metrics(account: &CursorAccount) -> Vec<(String, i32)> {
    let usage = read_usage_percent(account);
    let mut metrics = Vec::new();

    if let Some(used) = usage.total_used {
        metrics.push(("Total Usage".to_string(), 100 - used.clamp(0, 100)));
    }
    if let Some(used) = usage.auto_used {
        metrics.push(("Cursor Models".to_string(), 100 - used.clamp(0, 100)));
    }
    if let Some(used) = usage.api_used {
        metrics.push(("Other Models".to_string(), 100 - used.clamp(0, 100)));
    }

    metrics
}

/// 低额度告警检查项：IDE 额度池 + Grok Bot 周用量。
/// Grok Bot 是独立产品，不参与"推荐切换账号"的平均分，否则没用过 Grok Bot 的账号会被无理由抬高。
fn extract_alert_metrics(account: &CursorAccount) -> Vec<(String, i32)> {
    let mut metrics = extract_quota_metrics(account);
    if let Some(used) = read_grok_bot_weekly(account).used_percent {
        metrics.push(("Grok Bot (Weekly)".to_string(), 100 - used.clamp(0, 100)));
    }
    metrics
}

fn cursor_reset_millis(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        let reset = value.get(*key)?;
        if let Some(text) = reset.as_str().map(str::trim) {
            return text
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(text)
                        .ok()
                        .map(|time| time.timestamp_millis())
                });
        }
        reset.as_i64().filter(|value| *value > 0)
    })
}

fn cursor_quota_history_metrics(
    account: &CursorAccount,
) -> Vec<super::platform_quota_history::Metric> {
    use super::platform_quota_history::Metric;

    let Some(raw) = &account.cursor_usage_raw else {
        return Vec::new();
    };
    let plan = raw
        .get("planInfo")
        .or_else(|| raw.get("plan_info"))
        .unwrap_or(&Value::Null);
    let unified = is_unified_usage(raw, account.membership_type.as_deref());
    let reset_at = if unified {
        let keys = ["includedUsageResetsAt", "included_usage_resets_at"];
        cursor_reset_millis(plan, &keys).or_else(|| cursor_reset_millis(raw, &keys))
    } else {
        let keys = ["billingCycleEnd", "billing_cycle_end"];
        cursor_reset_millis(raw, &keys).or_else(|| cursor_reset_millis(plan, &keys))
    };
    let usage = read_usage_percent(account);
    let mut rows = Vec::new();
    for (key, used) in [
        ("total", usage.total_used),
        ("auto", usage.auto_used),
        ("api", usage.api_used),
    ] {
        if let Some(used) = used {
            rows.push(Metric {
                key: key.to_owned(),
                label: key.to_owned(),
                remaining: 100 - used,
                reset_at,
            });
        }
    }
    if !unified {
        let personal = raw
            .get("individualUsage")
            .or_else(|| raw.get("individual_usage"))
            .and_then(|value| value.get("onDemand").or_else(|| value.get("on_demand")));
        let team = raw
            .get("teamUsage")
            .or_else(|| raw.get("team_usage"))
            .and_then(|value| value.get("onDemand").or_else(|| value.get("on_demand")));
        let spending = raw
            .get("spendLimitUsage")
            .or_else(|| raw.get("spend_limit_usage"));
        let on_demand = personal.or(spending);
        let read_enabled = |value: &Value| {
            value.as_bool().or_else(
                || match value.as_str()?.trim().to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                },
            )
        };
        let enabled = raw
            .get("onDemandEnabled")
            .and_then(read_enabled)
            .or_else(|| {
                personal
                    .and_then(|value| value.get("enabled"))
                    .and_then(read_enabled)
            });
        let team_limit = [Some(raw), spending]
            .into_iter()
            .flatten()
            .find_map(|value| {
                value
                    .get("limitType")
                    .or_else(|| value.get("limit_type"))
                    .and_then(Value::as_str)
            })
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("team"));
        let mut used = pick_number(
            on_demand,
            &[
                "used",
                "individualUsed",
                "individual_used",
                "totalSpend",
                "total_spend",
            ],
        );
        let mut limit = pick_number(
            on_demand,
            &[
                "limit",
                "individualLimit",
                "individual_limit",
                "pooledLimit",
                "pooled_limit",
            ],
        );
        if team_limit {
            used = pick_number(team, &["used"])
                .or_else(|| {
                    pick_number(
                        spending,
                        &["pooledUsed", "pooled_used", "overallUsed", "overall_used"],
                    )
                })
                .or(used);
            limit = pick_number(team, &["limit"])
                .or_else(|| {
                    pick_number(
                        spending,
                        &[
                            "pooledLimit",
                            "pooled_limit",
                            "overallLimit",
                            "overall_limit",
                        ],
                    )
                })
                .or(limit);
        }
        if let (Some(used), Some(limit), true) = (
            used,
            limit.filter(|limit| *limit > 0.0),
            enabled != Some(false),
        ) {
            rows.push(Metric {
                key: "on_demand".into(),
                label: "on_demand".into(),
                remaining: 100 - clamp_percent(used / limit * 100.0),
                reset_at: None,
            });
        }
    }
    if let Some(sand) = account
        .sand_usage
        .as_ref()
        .filter(|usage| !usage.stale && usage.percent_used.is_finite())
    {
        rows.push(Metric {
            key: "sand".into(),
            label: "sand".into(),
            remaining: 100 - clamp_percent(sand.percent_used),
            reset_at: sand.reset_at.map(|time| time.saturating_mul(1000)),
        });
    }
    rows
}

fn average_quota_percentage(metrics: &[(String, i32)]) -> f64 {
    if metrics.is_empty() {
        return 0.0;
    }
    let sum: i32 = metrics.iter().map(|(_, pct)| *pct).sum();
    sum as f64 / metrics.len() as f64
}

fn normalize_quota_alert_threshold(value: i32) -> i32 {
    value.clamp(0, 100)
}

pub(crate) fn resolve_current_account_id(accounts: &[CursorAccount]) -> Option<String> {
    if let Ok(Some(local_payload)) = read_local_cursor_auth() {
        let incoming_auth_id = resolve_payload_auth_id(&local_payload);
        let incoming_email = normalize_email_identity(Some(local_payload.email.as_str()));
        let incoming_token = normalize_token_identity(Some(local_payload.access_token.as_str()));

        if let Some(account_id) = accounts
            .iter()
            .find(|account| {
                let existing_auth_id = resolve_account_auth_id(account);
                if let (Some(existing), Some(incoming)) =
                    (existing_auth_id.as_ref(), incoming_auth_id.as_ref())
                {
                    return auth_ids_match(existing, incoming);
                }
                if existing_auth_id.is_some() || incoming_auth_id.is_some() {
                    return false;
                }

                let existing_email = normalize_email_identity(Some(account.email.as_str()));
                let existing_token = normalize_token_identity(Some(account.access_token.as_str()));
                if let (Some(existing), Some(incoming)) =
                    (existing_email.as_ref(), incoming_email.as_ref())
                {
                    if existing == incoming {
                        return true;
                    }
                }
                if let (Some(existing), Some(incoming)) =
                    (existing_token.as_ref(), incoming_token.as_ref())
                {
                    if existing == incoming {
                        return true;
                    }
                }
                false
            })
            .map(|account| account.id.clone())
        {
            return Some(account_id);
        }
    }

    if let Ok(settings) = crate::modules::cursor_instance::load_default_settings() {
        if let Some(bind_id) = settings.bind_account_id {
            let trimmed = bind_id.trim();
            if !trimmed.is_empty() && accounts.iter().any(|account| account.id == trimmed) {
                return Some(trimmed.to_string());
            }
        }
    }

    crate::modules::provider_current_state::resolve_existing_current_account_id(
        "cursor",
        accounts.iter().map(|account| account.id.as_str()),
    )
    .or_else(|| {
        accounts
            .iter()
            .max_by_key(|account| account.last_used)
            .map(|account| account.id.clone())
    })
}

fn pick_quota_alert_recommendation(
    accounts: &[CursorAccount],
    current_id: &str,
) -> Option<CursorAccount> {
    let mut candidates: Vec<CursorAccount> = accounts
        .iter()
        .filter(|account| account.id != current_id)
        .filter(|account| !is_banned_account(account))
        // 刷新报错的账号（token 失效等）不能推荐，哪怕它还留着上次的用量数据。
        .filter(|account| account.quota_query_last_error.is_none())
        .filter(|account| is_token_usable_for_desktop(account))
        .filter(|account| !extract_quota_metrics(account).is_empty())
        .cloned()
        .collect();

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(|a, b| {
        let avg_a = average_quota_percentage(&extract_quota_metrics(a));
        let avg_b = average_quota_percentage(&extract_quota_metrics(b));
        avg_b
            .partial_cmp(&avg_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.last_used.cmp(&b.last_used))
    });

    candidates.into_iter().next()
}

fn display_email(account: &CursorAccount) -> String {
    let trimmed = account.email.trim();
    if trimmed.is_empty() {
        account.id.clone()
    } else {
        trimmed.to_string()
    }
}

fn build_quota_alert_cooldown_key(account_id: &str, threshold: i32) -> String {
    format!("cursor:{}:{}", account_id, threshold)
}

fn should_emit_quota_alert(cooldown_key: &str, now: i64) -> bool {
    let Ok(mut state) = CURSOR_QUOTA_ALERT_LAST_SENT.lock() else {
        return true;
    };

    if let Some(last_sent) = state.get(cooldown_key) {
        if now - *last_sent < CURSOR_QUOTA_ALERT_COOLDOWN_SECONDS {
            return false;
        }
    }

    state.insert(cooldown_key.to_string(), now);
    true
}

fn clear_quota_alert_cooldown(account_id: &str, threshold: i32) {
    if let Ok(mut state) = CURSOR_QUOTA_ALERT_LAST_SENT.lock() {
        state.remove(&build_quota_alert_cooldown_key(account_id, threshold));
    }
}

// ---------------------------------------------------------------------------
// Auto switch
// ---------------------------------------------------------------------------

/// 自动切号会关闭并重启 Cursor，两次之间留出间隔，避免刷新抖动或候选账号也濒临耗尽时来回切。
const CURSOR_AUTO_SWITCH_COOLDOWN_SECONDS: i64 = 10 * 60;

lazy_static::lazy_static! {
    static ref CURSOR_AUTO_SWITCH_LAST_PERFORMED: Mutex<Option<i64>> = Mutex::new(None);
}

#[derive(Debug, Clone)]
pub struct CursorAutoSwitchPlan {
    pub current: CursorAccount,
    pub target: CursorAccount,
    pub threshold: i32,
    pub low_metrics: Vec<(String, i32)>,
}

pub fn is_auto_switch_mode_enabled() -> bool {
    let cfg = crate::modules::config::get_user_config();
    cfg.cursor_quota_alert_enabled
        && crate::modules::config::normalize_cursor_quota_alert_mode(&cfg.cursor_quota_alert_mode)
            == crate::modules::config::CURSOR_QUOTA_ALERT_MODE_AUTO
}

/// 判断当前账号是否命中阈值并挑出切换目标。只看 IDE 额度池（Total / Cursor Models / Other Models），
/// Grok Bot 是独立产品，它耗尽不应该重启 IDE。候选账号要求所有池都在阈值之上，按平均剩余从高到低取第一个。
pub fn pick_auto_switch_target_if_needed() -> Result<Option<CursorAutoSwitchPlan>, String> {
    if !is_auto_switch_mode_enabled() {
        return Ok(None);
    }
    let cfg = crate::modules::config::get_user_config();
    let threshold = normalize_quota_alert_threshold(cfg.cursor_quota_alert_threshold);

    let now = now_ts();
    if let Ok(last) = CURSOR_AUTO_SWITCH_LAST_PERFORMED.lock() {
        if let Some(last_ts) = *last {
            if now - last_ts < CURSOR_AUTO_SWITCH_COOLDOWN_SECONDS {
                logger::log_info(&format!(
                    "[AutoSwitch][Cursor] 冷却中，跳过本次检查: elapsed={}s",
                    now - last_ts
                ));
                return Ok(None);
            }
        }
    }

    let accounts = list_accounts();
    let Some(current_id) = resolve_current_account_id(&accounts) else {
        return Ok(None);
    };
    let Some(current) = accounts.iter().find(|account| account.id == current_id) else {
        return Ok(None);
    };
    if is_banned_account(current) {
        return Ok(None);
    }

    let current_metrics = extract_quota_metrics(current);
    if current_metrics.is_empty() {
        return Ok(None);
    }
    let low_metrics: Vec<(String, i32)> = current_metrics
        .iter()
        .filter(|(_, pct)| *pct <= threshold)
        .cloned()
        .collect();
    if low_metrics.is_empty() {
        return Ok(None);
    }

    let mut candidates: Vec<(&CursorAccount, f64, i32)> = accounts
        .iter()
        .filter(|account| account.id != current_id)
        .filter(|account| !is_banned_account(account))
        .filter(|account| account.quota_query_last_error.is_none())
        .filter(|account| is_token_usable_for_desktop(account))
        .filter_map(|account| {
            let metrics = extract_quota_metrics(account);
            if metrics.is_empty() {
                return None;
            }
            let min_left = metrics.iter().map(|(_, pct)| *pct).min().unwrap_or(0);
            if min_left <= threshold {
                return None;
            }
            Some((account, average_quota_percentage(&metrics), min_left))
        })
        .collect();

    if candidates.is_empty() {
        logger::log_warn(&format!(
            "[AutoSwitch][Cursor] 当前账号命中阈值 (<= {}%)，但没有可切换候选账号: current={}",
            threshold,
            display_email(current)
        ));
        return Ok(None);
    }

    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.last_used.cmp(&b.0.last_used))
    });

    let target = candidates[0].0.clone();
    Ok(Some(CursorAutoSwitchPlan {
        current: current.clone(),
        target,
        threshold,
        low_metrics,
    }))
}

pub fn mark_auto_switch_performed() {
    if let Ok(mut last) = CURSOR_AUTO_SWITCH_LAST_PERFORMED.lock() {
        *last = Some(now_ts());
    }
}

pub fn run_quota_alert_if_needed(
) -> Result<Option<crate::modules::account::QuotaAlertPayload>, String> {
    let cfg = crate::modules::config::get_user_config();
    if !cfg.cursor_quota_alert_enabled {
        return Ok(None);
    }

    let threshold = normalize_quota_alert_threshold(cfg.cursor_quota_alert_threshold);
    let accounts = list_accounts();
    let current_id = match resolve_current_account_id(&accounts) {
        Some(id) => id,
        None => return Ok(None),
    };

    let current = match accounts.iter().find(|account| account.id == current_id) {
        Some(account) => account,
        None => return Ok(None),
    };
    if is_banned_account(current) {
        return Ok(None);
    }

    let metrics = extract_alert_metrics(current);
    if metrics.is_empty() {
        clear_quota_alert_cooldown(&current_id, threshold);
        return Ok(None);
    }

    let low_models: Vec<(String, i32)> = metrics
        .into_iter()
        .filter(|(_, pct)| *pct <= threshold)
        .collect();
    if low_models.is_empty() {
        clear_quota_alert_cooldown(&current_id, threshold);
        return Ok(None);
    }

    let now = chrono::Utc::now().timestamp();
    let cooldown_key = build_quota_alert_cooldown_key(&current_id, threshold);
    if !should_emit_quota_alert(&cooldown_key, now) {
        return Ok(None);
    }

    let recommendation = pick_quota_alert_recommendation(&accounts, &current_id);
    let lowest_percentage = low_models.iter().map(|(_, pct)| *pct).min().unwrap_or(0);
    let payload = crate::modules::account::QuotaAlertPayload {
        platform: "cursor".to_string(),
        current_account_id: current_id,
        current_email: display_email(current),
        threshold,
        threshold_display: None,
        lowest_percentage,
        low_models: low_models.into_iter().map(|(name, _)| name).collect(),
        recommended_account_id: recommendation.as_ref().map(|account| account.id.clone()),
        recommended_email: recommendation.as_ref().map(display_email),
        triggered_at: now,
    };

    crate::modules::account::dispatch_quota_alert(&payload);
    Ok(Some(payload))
}

#[cfg(test)]
struct CursorTestDataDir {
    _directory: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl CursorTestDataDir {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("COCKPIT_TOOLS_TEST_DATA_DIR");
        std::env::set_var("COCKPIT_TOOLS_TEST_DATA_DIR", directory.path());
        Self {
            _directory: directory,
            previous,
        }
    }
}

#[cfg(test)]
impl Drop for CursorTestDataDir {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            std::env::set_var("COCKPIT_TOOLS_TEST_DATA_DIR", value);
        } else {
            std::env::remove_var("COCKPIT_TOOLS_TEST_DATA_DIR");
        }
    }
}

#[cfg(test)]
mod sand_usage_tests {
    use super::*;

    #[test]
    fn super_uses_the_weekly_pool_when_period_metadata_is_missing() {
        let legacy_snapshot = json!({
            "billingCycleEnd": "2026-12-01T00:00:00Z",
            "individualUsage": {"plan": {"totalPercentUsed": 0, "limit": 16159}}
        });
        assert!(is_unified_usage(&legacy_snapshot, Some("super")));
        assert!(!is_unified_usage(&legacy_snapshot, Some("pro_plus")));
        assert!(!is_unified_usage(
            &json!({"planInfo":{"includedUsagePeriod":"INCLUDED_USAGE_PERIOD_MONTHLY"}}),
            Some("super"),
        ));
    }
    use serde_json::json;

    #[test]
    fn pasted_browser_session_token_keeps_the_jwt_and_checks_account_identity() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"auth0|user_fixture"}"#);
        let jwt = format!("e30.{claims}.fixture");
        for input in [
            jwt.clone(),
            format!("user_fixture::{jwt}"),
            format!("user_fixture%3a%3A{jwt}"),
            format!("WorkosCursorSessionToken=user_fixture%3A%3A{jwt}"),
        ] {
            let normalized = normalize_pasted_access_token(&input).unwrap();
            assert_eq!(normalized, jwt);
            assert_eq!(
                build_session_cookie(&normalized).unwrap(),
                format!("WorkosCursorSessionToken=user_fixture%3A%3A{jwt}")
            );
        }
        for input in [
            format!("user_other%3A%3A{jwt}"),
            format!("user_fixture%3A%3A{jwt};other=value"),
            "user_fixture%3A%3A".to_owned(),
        ] {
            assert_eq!(
                normalize_pasted_access_token(&input).unwrap_err(),
                "CURSOR_INVALID_BROWSER_TOKEN"
            );
        }
    }

    #[test]
    fn sand_usage_refresh_cannot_overwrite_reauthorized_deleted_or_reimported_account() {
        let _environment = crate::modules::test_support::env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _data_dir = CursorTestDataDir::new();

        let previous: CursorAccount = serde_json::from_value(json!({
            "id": "fixture", "email": "cursor@example.test", "access_token": "old",
            "created_at": 1, "last_used": 1
        }))
        .unwrap();
        let mut refreshed = previous.clone();
        refreshed.sand_usage = Some(CursorSandUsage {
            percent_used: 12.0,
            reset_at: None,
            stale: false,
        });
        upsert_account_record(previous.clone()).unwrap();
        commit_cursor_refresh(previous.clone(), refreshed.clone(), None).unwrap();
        assert_eq!(
            load_account(&previous.id).unwrap().sand_usage.unwrap().percent_used,
            12.0
        );
        let mut current = previous.clone();
        current.access_token = "reauthorized".into();
        upsert_account_record(current).unwrap();

        let preserved = commit_cursor_refresh(previous.clone(), refreshed.clone(), None).unwrap();
        assert_eq!(preserved.access_token, "reauthorized");
        assert!(load_account(&previous.id).unwrap().sand_usage.is_none());
        remove_account(&previous.id).unwrap();
        assert!(commit_cursor_refresh(previous.clone(), refreshed.clone(), None).is_err());
        assert!(load_account(&previous.id).is_none());

        let mut reimported = previous.clone();
        reimported.created_at = 2;
        upsert_account_record(reimported).unwrap();
        let preserved = commit_cursor_refresh(previous.clone(), refreshed, None).unwrap();
        assert_eq!(preserved.created_at, 2);
        assert!(load_account(&previous.id).unwrap().sand_usage.is_none());
    }

    #[test]
    fn expired_cursor_account_can_be_deleted_without_refreshing_credentials() {
        let _environment = crate::modules::test_support::env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _data_dir = CursorTestDataDir::new();
        let mut account: CursorAccount = serde_json::from_value(json!({
            "id": "expired-fixture", "email": "expired@example.test", "access_token": "expired",
            "created_at": 1, "last_used": 1
        }))
        .unwrap();
        account.quota_query_last_error = Some("HTTP 401 Unauthorized".into());
        upsert_account_record(account.clone()).unwrap();

        assert!(list_accounts_checked()
            .unwrap()
            .iter()
            .any(|row| row.id == account.id));
        remove_accounts(std::slice::from_ref(&account.id)).unwrap();
        assert!(load_account(&account.id).is_none());
        assert!(!load_account_index_checked()
            .unwrap()
            .accounts
            .iter()
            .any(|row| row.id == account.id));
    }

    #[test]
    fn sand_usage_distinguishes_paid_exhausted_trial_and_missing_quota() {
        const RESET: &str = "2026-11-20T12:00:00Z";
        const TRIAL_END: &str = "2026-11-18T12:00:00Z";
        let reset = chrono::DateTime::parse_from_rfc3339(RESET).unwrap().timestamp();
        let trial_end = chrono::DateTime::parse_from_rfc3339(TRIAL_END).unwrap().timestamp();
        let parse = |payload, now| {
            serde_json::from_value::<CursorSandUsageResponse>(payload)
                .unwrap()
                .usage(now)
        };
        for paid in [
            json!({"includedLimitZero": false, "usagePercent": 0, "nextResetTimestampUtc": RESET,
                "sandTrialExpiresAt": TRIAL_END}),
            json!({"hasNonZeroIncludedLimit": true, "usagePercent": 0, "nextResetTimestampUtc": RESET}),
        ] {
            let usage = parse(paid, trial_end).unwrap().unwrap();
            assert_eq!(usage.percent_used, 0.0);
            assert_eq!(usage.reset_at, Some(reset));
        }

        let trial = json!({"includedLimitZero": true, "usagePercent": 100,
            "hasAvailableUsage": false, "sandTrialExpiresAt": TRIAL_END, "nextResetTimestampUtc": RESET});
        let usage = parse(trial.clone(), trial_end - 1).unwrap().unwrap();
        assert_eq!(usage.percent_used, 100.0);
        assert_eq!(usage.reset_at, Some(trial_end));
        assert!(parse(trial, trial_end).unwrap().is_none());
        assert!(parse(json!({"includedLimitZero":true,"usagePercent":0}), trial_end).unwrap().is_none());
        assert!(parse(json!({"includedLimitZero":false}), trial_end).is_err());
        assert!(parse(json!({"error":"missing authentication"}), trial_end).is_err());
    }

    #[tokio::test]
    async fn sand_usage_posts_empty_json_using_cursor_session_cookie() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::post,
            Json, Router,
        };
        let jwt = format!(
            "header.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(br#"{"sub":"workos|user_fixture"}"#)
        );
        let expected_cookie = format!("WorkosCursorSessionToken=user_fixture%3A%3A{jwt}");
        let app = Router::new()
            .route(
                "/usage",
                post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["cookie"], expected_cookie);
                    assert_eq!(headers["origin"], "https://cursor.com");
                    assert_eq!(body, json!({}));
                    Json(json!({"includedLimitZero":false,"usagePercent":13.55}))
                }),
            )
            .route("/failure", post(|| async { StatusCode::UNAUTHORIZED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let usage = fetch_sand_usage_with_client(&client, &format!("http://{addr}/usage"), &jwt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(usage.percent_used, 13.55);
        assert!(!usage.stale);
        assert!(
            fetch_sand_usage_with_client(&client, &format!("http://{addr}/failure"), &jwt)
                .await
                .unwrap_err()
                .contains("401")
        );
        server.abort();
    }
}

#[cfg(test)]
mod session_write_tests {
    use super::*;
    fn account() -> CursorAccount {
        serde_json::from_value(serde_json::json!({
            "id":"new", "email":"new@example.test", "access_token":"header.eyJzdWIiOiJuZXctdXNlciJ9.signature",
            "membership_type":"pro", "created_at":0, "last_used":0
        })).unwrap()
    }
    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        for key in [
            "cursorAuth/refreshToken",
            "cursorAuth/cachedTeam",
            "cursorAuth/stripeMembershipAuthId",
            "unrelated",
        ] {
            upsert_vscdb_item(&conn, key, "old").unwrap();
        }
        conn
    }

    #[test]
    fn cursor_quota_history_preserves_plan_reset_and_only_records_current_sand() {
        let mut account = account();
        account.cursor_usage_raw = Some(serde_json::json!({
            "planInfo": { "includedUsagePeriod": "monthly" },
            "billingCycleEnd": "2026-12-01T00:00:00Z",
            "individualUsage": { "plan": { "totalPercentUsed": 40 }, "onDemand": { "used": 5, "limit": 10, "enabled": true } },
            "teamUsage": { "onDemand": { "used": 20, "limit": 100 } }, "limitType": "team"
        }));
        account.sand_usage = Some(CursorSandUsage {
            percent_used: 22.0,
            reset_at: Some(123),
            stale: true,
        });
        let metrics = cursor_quota_history_metrics(&account);
        assert_eq!(
            metrics
                .iter()
                .map(|row| (row.key.as_str(), row.remaining))
                .collect::<Vec<_>>(),
            [("total", 60), ("on_demand", 80)]
        );
        assert_eq!(
            metrics[0].reset_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        account.sand_usage.as_mut().unwrap().stale = false;
        let metrics = cursor_quota_history_metrics(&account);
        assert_eq!(
            metrics
                .last()
                .map(|row| (row.key.as_str(), row.remaining, row.reset_at)),
            Some(("sand", 78, Some(123_000)))
        );

        account.cursor_usage_raw = Some(serde_json::json!({
            "planInfo": { "includedUsagePeriod": "weekly", "includedUsageResetsAt": "1796083200000" },
            "planUsage": { "totalPercentUsed": 30 }
        }));
        account.sand_usage = None;
        let metrics = cursor_quota_history_metrics(&account);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].remaining, 70);
        assert_eq!(metrics[0].reset_at, Some(1796083200000));
    }

    #[test]
    fn cursor_local_import_binds_credentials_to_the_current_subject_and_preserves_saved_usage() {
        let _environment = crate::modules::test_support::env_lock().lock().unwrap();
        let _data_dir = CursorTestDataDir::new();
        let mut old = account();
        old.id = "old".into();
        old.auth_id = Some("old-user".into());
        old.email = "old@example.test".into();
        old.access_token = "old-token".into();
        upsert_account_record(old.clone()).unwrap();

        let mut current = account();
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"sub":"workos|user_new"}"#);
        current.access_token = format!("header.{claims}.signature");
        current.auth_id = Some("user_new".into());
        current.name = Some("Saved display name".into());
        current.cursor_usage_raw = Some(serde_json::json!({"usagePercent": 20}));
        current.usage_updated_at = Some(50);
        upsert_account_record(current.clone()).unwrap();

        let conn = database();
        conn.execute(
            "INSERT INTO ItemTable VALUES (?1, ?2)",
            ("cursorAuth/accessToken", current.access_token.as_bytes()),
        )
        .unwrap();
        upsert_vscdb_item(&conn, "cursorAuth/authId", "old-user").unwrap();
        upsert_vscdb_item(&conn, "cursorAuth/cachedEmail", &current.email).unwrap();
        let payload = read_local_cursor_auth_from_conn(&conn).unwrap().unwrap();
        assert_eq!(payload.auth_id.as_deref(), Some("workos|user_new"));
        let imported = upsert_account_with_source(payload, true).unwrap();
        assert_eq!(imported.id, current.id);
        assert_eq!(imported.auth_id, current.auth_id);
        assert_eq!(imported.name, current.name);
        assert_eq!(imported.cursor_usage_raw, current.cursor_usage_raw);
        assert_eq!(imported.usage_updated_at, current.usage_updated_at);
        let stored_old = load_account(&old.id).unwrap();
        assert_eq!(stored_old.email, old.email);
        assert_eq!(stored_old.access_token, old.access_token);
    }

    #[test]
    fn cursor_local_login_distinguishes_empty_storage_and_database_errors() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(read_local_cursor_auth_from_conn(&conn).is_err());
        conn.execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        assert!(read_local_cursor_auth_from_conn(&conn).unwrap().is_none());
        upsert_vscdb_item(&conn, "cursorAuth/accessToken", "opaque-token").unwrap();
        upsert_vscdb_item(&conn, "cursorAuth/authId", " legacy-user ").unwrap();
        upsert_vscdb_item(&conn, "cursorAuth/cachedEmail", "legacy@example.test").unwrap();
        let payload = read_local_cursor_auth_from_conn(&conn).unwrap().unwrap();
        assert_eq!(payload.auth_id.as_deref(), Some("legacy-user"));
    }
}

#[cfg(test)]
mod import_token_tests {
    use super::*;

    fn fake_jwt(sub: &str, exp: i64) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = engine.encode(format!(r#"{{"sub":"{}","exp":{}}}"#, sub, exp).as_bytes());
        format!("{}.{}.sig", header, payload)
    }

    #[test]
    fn normalize_strips_bearer_cookie_and_quotes() {
        let jwt = fake_jwt("auth0|user_1", 4_000_000_000);
        assert_eq!(normalize_import_access_token(&format!("Bearer {}", jwt)), jwt);
        assert_eq!(normalize_import_access_token(&format!("\"{}\"", jwt)), jwt);
        assert_eq!(
            normalize_import_access_token(&format!("WorkosCursorSessionToken=user_1::{}", jwt)),
            jwt
        );
        assert_eq!(
            normalize_import_access_token(&format!("user_1%3A%3A{}", jwt)),
            jwt
        );
        assert_eq!(normalize_import_access_token(&format!("  {}  ", jwt)), jwt);
    }

    #[test]
    fn auth_ids_match_ignores_auth0_prefix_and_case() {
        assert!(auth_ids_match("auth0|user_ABC", "user_abc"));
        assert!(auth_ids_match("user_abc", "auth0|user_abc"));
        assert!(!auth_ids_match("auth0|user_abc", "auth0|user_xyz"));
    }

    #[test]
    fn validate_rejects_non_jwt_and_expired_without_refresh() {
        assert!(validate_import_access_token("not-a-jwt", None).is_err());
        assert!(validate_import_access_token("a.b", None).is_err());

        let expired = fake_jwt("auth0|user_1", 1_000);
        assert!(validate_import_access_token(&expired, None).is_err());
        assert!(validate_import_access_token(&expired, Some("refresh")).is_ok());

        let valid = fake_jwt("auth0|user_1", 4_000_000_000);
        assert!(validate_import_access_token(&valid, None).is_ok());
    }
}
