use tauri::webview::cookie::{Cookie, SameSite};
use tauri::{AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder};

use crate::modules::cursor_account;
use crate::modules::logger;

const CURSOR_DASHBOARD_URL: &str = "https://cursor.com/dashboard";
const CURSOR_COOKIE_URL: &str = "https://cursor.com/";

fn webview_label(account_id: &str) -> String {
    format!("cursor-webview-{}", account_id)
}

/// 用账号的 token 直接打开已登录的 cursor.com Dashboard。
///
/// 网页会话（type=web）token 无法用于桌面端切号，但它本身就是 cursor.com 的登录 Cookie，
/// 所以在这里能派上用场；session 类型的 token 同样能登录网页。每个账号使用独立的
/// WebView 数据目录，Cookie 互不串扰。窗口先打开空白页、写入 Cookie，再导航到 Dashboard，
/// 避免首个请求抢在 Cookie 之前发出而落到登录页。
#[tauri::command]
pub async fn open_cursor_webview(app: AppHandle, account_id: String) -> Result<(), String> {
    let account = cursor_account::load_account(&account_id)
        .ok_or_else(|| format!("账号不存在：{}", account_id))?;
    let cookie_value = cursor_account::build_session_cookie_value(&account.access_token)
        .ok_or_else(|| "无法从 token 解析 WorkOS 用户 ID，无法生成网页登录 Cookie".to_string())?;

    let label = webview_label(&account_id);
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_focus();
        return Ok(());
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("获取应用数据目录失败：{}", e))?
        .join("webviews")
        .join("cursor")
        .join(&account_id);
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("创建 webview 数据目录失败：{}", e))?;

    let title = format!("Cursor Dashboard - {}", account.email);
    let app_for_thread = app.clone();
    let label_for_thread = label.clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<tauri::WebviewWindow, tauri::Error>>();
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

    let window = rx
        .await
        .map_err(|_| "创建窗口任务已取消".to_string())?
        .map_err(|e| format!("创建网页窗口失败：{}", e))?;

    let cookie = Cookie::build((cursor_account::CURSOR_SESSION_COOKIE_NAME, cookie_value))
        .domain("cursor.com")
        .path("/")
        .secure(true)
        .http_only(true)
        .same_site(SameSite::Lax)
        .build();
    // Cookie 接口在 Windows 上不能在同步上下文里等待，这里是 async command，可直接调用。
    window
        .set_cookie(cookie)
        .map_err(|e| format!("写入登录 Cookie 失败：{}", e))?;

    let dashboard: Url = CURSOR_DASHBOARD_URL
        .parse()
        .map_err(|e| format!("Dashboard 地址无效：{}", e))?;
    window
        .navigate(dashboard)
        .map_err(|e| format!("打开 Dashboard 失败：{}", e))?;

    logger::log_info(&format!(
        "[Cursor WebView] 已打开账号网页会话: id={}, email={}, cookie_url={}",
        account.id, account.email, CURSOR_COOKIE_URL
    ));
    Ok(())
}
