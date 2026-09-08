use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tauri::{AppHandle, Emitter};

use crate::models::cursor::CursorAccount;
use crate::modules::{cursor_account, cursor_oauth, cursor_switch_history, logger};

#[tauri::command]
pub fn list_cursor_accounts() -> Result<Vec<CursorAccount>, String> {
    cursor_account::list_accounts_checked()
}

#[tauri::command]
pub fn delete_cursor_account(account_id: String) -> Result<(), String> {
    cursor_account::remove_account(&account_id)
}

#[tauri::command]
pub fn delete_cursor_accounts(account_ids: Vec<String>) -> Result<(), String> {
    cursor_account::remove_accounts(&account_ids)
}

#[tauri::command]
pub fn import_cursor_from_json(json_content: String) -> Result<Vec<CursorAccount>, String> {
    cursor_account::import_from_json(&json_content)
}

#[tauri::command]
pub fn import_cursor_from_local(app: AppHandle) -> Result<Vec<CursorAccount>, String> {
    match cursor_account::import_from_local()? {
        Some(account) => {
            let _ = crate::modules::tray::update_tray_menu(&app);
            Ok(vec![account])
        }
        None => Err("未找到本地 Cursor 登录信息".to_string()),
    }
}

#[tauri::command]
pub fn export_cursor_accounts(account_ids: Vec<String>) -> Result<String, String> {
    cursor_account::export_accounts(&account_ids)
}

#[tauri::command]
pub async fn refresh_cursor_token(
    app: AppHandle,
    account_id: String,
) -> Result<CursorAccount, String> {
    let started_at = Instant::now();
    logger::log_info(&format!(
        "[Cursor Command] 手动刷新账号开始: account_id={}",
        account_id
    ));

    match cursor_account::refresh_account_async(&account_id).await {
        Ok(account) => {
            run_cursor_post_refresh_checks(&app).await;
            let _ = crate::modules::tray::update_tray_menu(&app);
            logger::log_info(&format!(
                "[Cursor Command] 刷新完成: account_id={}, email={}, elapsed={}ms",
                account.id,
                account.email,
                started_at.elapsed().as_millis()
            ));
            Ok(account)
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Command] 刷新失败: account_id={}, elapsed={}ms, error={}",
                account_id,
                started_at.elapsed().as_millis(),
                err
            ));
            Err(err)
        }
    }
}

#[tauri::command]
pub async fn refresh_all_cursor_tokens(app: AppHandle) -> Result<i32, String> {
    let started_at = Instant::now();
    logger::log_info("[Cursor Command] 批量刷新开始");

    let results = cursor_account::refresh_all_tokens().await?;
    let success_count = results.iter().filter(|(_, r)| r.is_ok()).count();

    if success_count > 0 {
        run_cursor_post_refresh_checks(&app).await;
    }

    let _ = crate::modules::tray::update_tray_menu(&app);
    logger::log_info(&format!(
        "[Cursor Command] 批量刷新完成: success={}, elapsed={}ms",
        success_count,
        started_at.elapsed().as_millis()
    ));
    Ok(success_count as i32)
}

#[tauri::command]
pub fn add_cursor_account_with_token(
    app: AppHandle,
    access_token: String,
) -> Result<CursorAccount, String> {
    let access_token = cursor_account::normalize_import_access_token(&access_token);
    if access_token.is_empty() {
        return Err("access_token 不能为空".to_string());
    }
    cursor_account::validate_import_access_token(&access_token, None)?;

    let email = "unknown".to_string();
    let payload = crate::models::cursor::CursorImportPayload {
        email,
        auth_id: None,
        name: None,
        access_token,
        refresh_token: None,
        membership_type: None,
        subscription_status: None,
        sign_up_type: None,
        cursor_auth_raw: None,
        cursor_usage_raw: None,
        status: None,
        status_reason: None,
    };
    let account = cursor_account::upsert_account(payload)?;
    let _ = crate::modules::tray::update_tray_menu(&app);
    Ok(account)
}

#[tauri::command]
pub async fn update_cursor_account_tags(
    account_id: String,
    tags: Vec<String>,
) -> Result<CursorAccount, String> {
    cursor_account::update_account_tags(&account_id, tags)
}

#[tauri::command]
pub fn get_cursor_accounts_index_path() -> Result<String, String> {
    cursor_account::accounts_index_path_string()
}

#[tauri::command]
pub fn cursor_oauth_login_start() -> Result<cursor_oauth::CursorOAuthStartResponse, String> {
    logger::log_info("[Cursor Command] OAuth 登录开始");
    cursor_oauth::start_login()
}

#[tauri::command]
pub async fn cursor_oauth_login_complete(
    app: AppHandle,
    login_id: String,
) -> Result<CursorAccount, String> {
    logger::log_info(&format!(
        "[Cursor Command] OAuth 等待完成: login_id={}",
        login_id
    ));
    let payload = cursor_oauth::complete_login(&login_id).await?;
    let mut account = cursor_account::upsert_account(payload)?;

    match cursor_account::refresh_account_async(&account.id).await {
        Ok(refreshed) => account = refreshed,
        Err(e) => {
            logger::log_warn(&format!("[Cursor OAuth] 登录后自动刷新配额失败: {}", e));
        }
    }

    let _ = crate::modules::tray::update_tray_menu(&app);
    logger::log_info(&format!(
        "[Cursor Command] OAuth 登录完成: account_id={}, email={}",
        account.id, account.email
    ));
    Ok(account)
}

#[tauri::command]
pub fn cursor_oauth_login_cancel(login_id: Option<String>) -> Result<(), String> {
    logger::log_info(&format!(
        "[Cursor Command] OAuth 取消: login_id={}",
        login_id.as_deref().unwrap_or("<none>")
    ));
    cursor_oauth::cancel_login(login_id.as_deref())
}

#[tauri::command]
pub async fn inject_cursor_account(app: AppHandle, account_id: String) -> Result<String, String> {
    switch_cursor_account(&app, &account_id, "manual").await
}

#[tauri::command]
pub fn list_cursor_switch_history(
) -> Result<Vec<cursor_switch_history::CursorSwitchHistoryItem>, String> {
    cursor_switch_history::load_history()
}

#[tauri::command]
pub fn clear_cursor_switch_history() -> Result<(), String> {
    cursor_switch_history::clear_history()
}

#[tauri::command]
pub async fn get_cursor_usage_breakdown(
    account_id: String,
) -> Result<cursor_account::CursorUsageBreakdown, String> {
    cursor_account::fetch_usage_breakdown(&account_id).await
}

#[tauri::command]
pub async fn get_cursor_hard_limit(
    account_id: String,
) -> Result<cursor_account::CursorHardLimit, String> {
    cursor_account::fetch_hard_limit(&account_id).await
}

/// 设置按需使用开关与上限后立刻刷新该账号，让卡片上的"按需使用"行反映新状态。
#[tauri::command]
pub async fn set_cursor_hard_limit(
    app: AppHandle,
    account_id: String,
    hard_limit_dollars: f64,
    no_usage_based_allowed: bool,
) -> Result<CursorAccount, String> {
    cursor_account::update_hard_limit(&account_id, hard_limit_dollars, no_usage_based_allowed)
        .await?;
    let refreshed = cursor_account::refresh_account_async(&account_id).await?;
    let _ = crate::modules::tray::update_tray_menu(&app);
    Ok(refreshed)
}

/// 手动切号与自动切号共用的完整流程：关 Cursor → 写凭据 → 记当前账号 → 重启 Cursor，
/// 并把结果写入切号历史。
async fn switch_cursor_account(
    app: &AppHandle,
    account_id: &str,
    reason: &str,
) -> Result<String, String> {
    switch_cursor_account_recorded(app, account_id, reason, Vec::new(), None).await
}

async fn switch_cursor_account_recorded(
    app: &AppHandle,
    account_id: &str,
    reason: &str,
    low_metrics: Vec<cursor_switch_history::CursorSwitchLowMetric>,
    threshold: Option<i32>,
) -> Result<String, String> {
    let accounts = cursor_account::list_accounts();
    let from = cursor_account::resolve_current_account_id(&accounts)
        .and_then(|id| accounts.iter().find(|account| account.id == id))
        .map(|account| (account.id.clone(), account.email.clone()));
    let target_email = accounts
        .iter()
        .find(|account| account.id == account_id)
        .map(|account| account.email.clone())
        .unwrap_or_default();

    let result = switch_cursor_account_inner(app, account_id, reason).await;

    cursor_switch_history::record_switch(
        reason,
        from.as_ref().map(|(id, email)| (id.as_str(), email.as_str())),
        account_id,
        &target_email,
        low_metrics,
        threshold,
        &result.as_ref().map(|_| ()).map_err(|e| e.clone()),
    );
    result
}

async fn switch_cursor_account_inner(
    app: &AppHandle,
    account_id: &str,
    reason: &str,
) -> Result<String, String> {
    let started_at = Instant::now();
    logger::log_info(&format!(
        "[Cursor Switch] 开始切换账号: account_id={}, reason={}",
        account_id, reason
    ));

    let account = cursor_account::load_account(account_id)
        .ok_or_else(|| format!("Cursor account not found: {}", account_id))?;
    // 网页会话 token 写进去也只会让 Cursor 弹登录，先拦住，别白白重启一次 Cursor。
    cursor_account::ensure_token_usable_for_desktop(&account)?;
    let account_id = account_id.to_string();

    // 必须先关掉正在运行的默认 Cursor 再写 state.vscdb：Cursor 退出时会把内存里的
    // 旧账号 token 回写，若后面启动失败提前返回，切换会被静默还原。
    let default_dir = crate::modules::cursor_instance::get_default_cursor_user_data_dir()?
        .to_string_lossy()
        .to_string();
    crate::modules::cursor_instance::close_cursor(&[default_dir.clone()], 20)?;
    if crate::modules::cursor_instance::is_cursor_running_for_dir(&default_dir) {
        return Err(
            "仍有 Cursor 进程占用默认用户目录，未写入账号。请完全退出 Cursor（含系统托盘图标）后重试。"
                .to_string(),
        );
    }

    cursor_account::inject_to_cursor(account_id.as_str())?;
    crate::modules::provider_current_state::set_current_account_id(
        "cursor",
        Some(account_id.as_str()),
    )?;

    if let Err(err) = crate::modules::cursor_instance::update_default_settings(
        Some(Some(account_id.clone())),
        None,
        Some(false),
    ) {
        logger::log_warn(&format!("更新 Cursor 默认实例绑定账号失败: {}", err));
    }

    let launch_warning =
        match crate::commands::cursor_instance::cursor_start_instance("__default__".to_string())
            .await
        {
            Ok(_) => None,
            Err(err) => {
                if err.starts_with("APP_PATH_NOT_FOUND:") || err.contains("启动 Cursor 失败") {
                    logger::log_warn(&format!("Cursor 默认实例启动失败: {}", err));
                    if err.starts_with("APP_PATH_NOT_FOUND:") || err.contains("APP_PATH_NOT_FOUND:")
                    {
                        let _ = app.emit(
                            "app:path_missing",
                            serde_json::json!({ "app": "cursor", "retry": { "kind": "default" } }),
                        );
                    }
                    Some(err)
                } else {
                    // 注入已经完成，让前端以真实状态为准，不再乐观标记。
                    logger::log_warn(&format!(
                        "[Cursor Switch] 注入完成但启动异常: account_id={}, error={}",
                        account_id, err
                    ));
                    return Err(err);
                }
            }
        };

    let _ = crate::modules::tray::update_tray_menu(app);

    if let Some(err) = launch_warning {
        logger::log_warn(&format!(
            "[Cursor Switch] 切号完成但启动失败: account_id={}, email={}, elapsed={}ms, error={}",
            account.id,
            account.email,
            started_at.elapsed().as_millis(),
            err
        ));
        Ok(format!("切换完成，但 Cursor 启动失败: {}", err))
    } else {
        logger::log_info(&format!(
            "[Cursor Switch] 切号成功: account_id={}, email={}, elapsed={}ms",
            account.id,
            account.email,
            started_at.elapsed().as_millis()
        ));
        Ok(format!("切换完成: {}", account.email))
    }
}

// ---------------------------------------------------------------------------
// Post-refresh checks: auto switch (mode=auto) or quota alert (mode=notify)
// ---------------------------------------------------------------------------

static CURSOR_POST_REFRESH_CHECK_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, serde::Serialize)]
pub struct CursorAutoSwitchedPayload {
    pub from_account_id: String,
    pub from_email: String,
    pub to_account_id: String,
    pub to_email: String,
    pub threshold: i32,
    pub low_models: Vec<String>,
    pub triggered_at: i64,
}

fn notify_cursor_auto_switched(app: &AppHandle, payload: &CursorAutoSwitchedPayload) {
    let _ = app.emit("cursor:auto-switched", payload);

    let locale = crate::modules::config::get_user_config().language;
    let title = crate::modules::i18n::translate(&locale, "cursor.autoSwitch.notifyTitle", &[]);
    let models = payload.low_models.join(", ");
    let threshold = payload.threshold.to_string();
    let body = crate::modules::i18n::translate(
        &locale,
        "cursor.autoSwitch.notifyBody",
        &[
            ("from", payload.from_email.as_str()),
            ("to", payload.to_email.as_str()),
            ("threshold", threshold.as_str()),
            ("models", models.as_str()),
        ],
    );
    crate::modules::account::send_native_notification_text(&title, &body);
}

/// 刷新配额后的后置检查。自动切号模式下命中阈值就切到最优账号；未切（模式为仅提示、
/// 没有候选、切换失败）则退回到配额预警通知，保证用户至少能收到提示。
pub(crate) async fn run_cursor_post_refresh_checks(app: &AppHandle) {
    if CURSOR_POST_REFRESH_CHECK_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        logger::log_info("[AutoSwitch][Cursor] 后置检查进行中，跳过本次执行");
        return;
    }

    let mut switched = false;
    match cursor_account::pick_auto_switch_target_if_needed() {
        Ok(Some(plan)) => {
            let target_id = plan.target.id.clone();
            let history_metrics = plan
                .low_metrics
                .iter()
                .map(|(label, left)| cursor_switch_history::CursorSwitchLowMetric {
                    label: label.clone(),
                    left_percent: *left,
                })
                .collect();
            match switch_cursor_account_recorded(
                app,
                &target_id,
                "auto",
                history_metrics,
                Some(plan.threshold),
            )
            .await
            {
                Ok(_) => {
                    cursor_account::mark_auto_switch_performed();
                    switched = true;
                    let payload = CursorAutoSwitchedPayload {
                        from_account_id: plan.current.id.clone(),
                        from_email: plan.current.email.clone(),
                        to_account_id: plan.target.id.clone(),
                        to_email: plan.target.email.clone(),
                        threshold: plan.threshold,
                        low_models: plan.low_metrics.iter().map(|(name, _)| name.clone()).collect(),
                        triggered_at: chrono::Utc::now().timestamp(),
                    };
                    logger::log_info(&format!(
                        "[AutoSwitch][Cursor] 自动切号完成: from={}, to={}, threshold={}%, low={:?}",
                        payload.from_email, payload.to_email, payload.threshold, plan.low_metrics
                    ));
                    notify_cursor_auto_switched(app, &payload);
                }
                Err(err) => {
                    logger::log_warn(&format!(
                        "[AutoSwitch][Cursor] 自动切号失败: target_id={}, error={}",
                        target_id, err
                    ));
                }
            }
        }
        Ok(None) => {}
        Err(err) => {
            logger::log_warn(&format!("[AutoSwitch][Cursor] 自动切号检查失败: {}", err));
        }
    }

    if !switched {
        if let Err(err) = cursor_account::run_quota_alert_if_needed() {
            logger::log_warn(&format!("[QuotaAlert][Cursor] 预警检查失败: {}", err));
        }
    }

    CURSOR_POST_REFRESH_CHECK_IN_PROGRESS.store(false, Ordering::SeqCst);
}
