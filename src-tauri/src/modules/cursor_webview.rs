use tauri::webview::cookie::{Cookie, SameSite};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::models::cursor::CursorAccount;
use crate::modules::cursor_account;
use crate::modules::cursor_oauth;
use crate::modules::logger;

const CURSOR_DASHBOARD_URL: &str = "https://cursor.com/dashboard";

fn webview_label(account_id: &str) -> String {
    format!("cursor-webview-{}", account_id)
}

/// 取得（或创建）某账号的独立 WebView 窗口，并确保 cursor.com 的登录 Cookie 已写入。
///
/// 窗口先打开空白页、写 Cookie，再由调用方决定导航到哪里，避免首个请求抢在 Cookie 之前
/// 发出而落到登录页。每个账号使用独立的 WebView 数据目录，Cookie 互不串扰。
async fn ensure_account_webview(
    app: &AppHandle,
    account: &CursorAccount,
) -> Result<WebviewWindow, String> {
    let cookie_value = cursor_account::build_session_cookie_value(&account.access_token)
        .ok_or_else(|| "无法从 token 解析 WorkOS 用户 ID，无法生成网页登录 Cookie".to_string())?;

    let label = webview_label(&account.id);
    let window = if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        existing
    } else {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("获取应用数据目录失败：{}", e))?
            .join("webviews")
            .join("cursor")
            .join(&account.id);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("创建 webview 数据目录失败：{}", e))?;

        let title = format!("Cursor Dashboard - {}", account.email);
        let app_for_thread = app.clone();
        let label_for_thread = label.clone();
        let (tx, rx) =
            tokio::sync::oneshot::channel::<Result<tauri::WebviewWindow, tauri::Error>>();
        // Windows 上 WebView2 窗口必须在主线程创建，否则白屏且无法关闭。
        app.run_on_main_thread(move || {
            let builder = WebviewWindowBuilder::new(
                &app_for_thread,
                &label_for_thread,
                WebviewUrl::External("about:blank".parse().unwrap()),
            )
            .title(title)
            .data_directory(data_dir)
            .inner_size(1280.0, 860.0)
            .min_inner_size(900.0, 600.0)
            .center()
            .focused(true);
            let _ = tx.send(builder.build());
        })
        .map_err(|e| format!("调度主线程创建窗口失败：{}", e))?;

        rx.await
            .map_err(|_| "创建窗口任务已取消".to_string())?
            .map_err(|e| format!("创建网页窗口失败：{}", e))?
    };

    let cookie = Cookie::build((cursor_account::CURSOR_SESSION_COOKIE_NAME, cookie_value))
        .domain("cursor.com")
        .path("/")
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Lax)
        .build();
    // Cookie 接口在 Windows 上不能在同步上下文里等待；这里始终从 async command 进入。
    window
        .set_cookie(cookie)
        .map_err(|e| format!("写入登录 Cookie 失败：{}", e))?;
    Ok(window)
}

fn navigate(window: &WebviewWindow, url: &str, what: &str) -> Result<(), String> {
    let parsed: Url = url
        .parse()
        .map_err(|e| format!("{} 地址无效：{}", what, e))?;
    window
        .navigate(parsed)
        .map_err(|e| format!("打开 {} 失败：{}", what, e))
}

/// 用账号的 token 直接打开已登录的 cursor.com Dashboard。
///
/// 网页会话（type=web）token 无法用于桌面端切号，但它本身就是 cursor.com 的登录 Cookie，
/// 所以在这里能派上用场；session 类型的 token 同样能登录网页。
#[tauri::command]
pub async fn open_cursor_webview(app: AppHandle, account_id: String) -> Result<(), String> {
    let account = cursor_account::load_account(&account_id)
        .ok_or_else(|| format!("账号不存在：{}", account_id))?;
    let window = ensure_account_webview(&app, &account).await?;
    navigate(&window, CURSOR_DASHBOARD_URL, "Dashboard")?;
    logger::log_info(&format!(
        "[Cursor WebView] 已打开账号网页会话: id={}, email={}",
        account.id, account.email
    ));
    Ok(())
}

/// 借助该账号已登录的网页会话，为它换取桌面端 session token。
///
/// Cursor 桌面端登录 = 浏览器里已登录 → 打开 loginDeepControl 确认页 → 点 "Yes, Log In" →
/// 客户端轮询 auth/poll 拿到 session 类型的 access/refresh token。这里把确认页开在带有该账号
/// Cookie 的内嵌窗口里，确认后走同一套轮询，拿到的 token 写回同一账号（auth_id 相同会合并），
/// 之后该账号就能正常切号。用户需要在弹出的窗口里点一次确认。
#[tauri::command]
pub async fn cursor_webview_desktop_login(
    app: AppHandle,
    account_id: String,
) -> Result<CursorAccount, String> {
    let account = cursor_account::load_account(&account_id)
        .ok_or_else(|| format!("账号不存在：{}", account_id))?;
    let window = ensure_account_webview(&app, &account).await?;

    let start = cursor_oauth::start_login()?;
    navigate(&window, &start.verification_uri, "Cursor 登录确认页")?;
    logger::log_info(&format!(
        "[Cursor WebView] 已在网页会话中打开桌面登录确认页: id={}, login_id={}",
        account.id, start.login_id
    ));

    let payload = match cursor_oauth::complete_login(&start.login_id).await {
        Ok(payload) => payload,
        Err(err) => {
            let _ = cursor_oauth::cancel_login(Some(&start.login_id));
            return Err(err);
        }
    };

    // 网页会话对应的是这个账号，但确认页理论上可能被切到别的账号；身份不一致时不覆盖。
    let payload_auth = payload
        .auth_id
        .clone()
        .or_else(|| cursor_account::extract_auth_id_from_access_token(&payload.access_token));
    let current_auth = cursor_account::resolve_account_auth_id(&account);
    if let (Some(got), Some(expected)) = (payload_auth.as_deref(), current_auth.as_deref()) {
        if !cursor_account::auth_ids_match(got, expected) {
            return Err(format!(
                "确认页登录的账号（{}）与当前账号（{}）不一致，已取消，未修改任何数据",
                payload.email, account.email
            ));
        }
    }

    let saved = cursor_account::upsert_account(payload)?;
    let refreshed = match cursor_account::refresh_account_async(&saved.id).await {
        Ok(account) => account,
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor WebView] 桌面登录后刷新配额失败（token 已保存）: id={}, error={}",
                saved.id, err
            ));
            saved
        }
    };

    let _ = navigate(&window, CURSOR_DASHBOARD_URL, "Dashboard");
    let _ = crate::modules::tray::update_tray_menu(&app);
    logger::log_info(&format!(
        "[Cursor WebView] 已通过网页会话获取桌面 session token: id={}, email={}",
        refreshed.id, refreshed.email
    ));
    Ok(refreshed)
}
