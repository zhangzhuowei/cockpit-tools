use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::modules;

const HISTORY_FILE: &str = "cursor_switch_history.json";
const MAX_HISTORY_ITEMS: usize = 200;

lazy_static::lazy_static! {
    static ref HISTORY_LOCK: Mutex<()> = Mutex::new(());
}

/// 一次 Cursor 切号记录。`reason` 取值：`manual`（用户点击）、`auto`（额度耗尽自动切换）、
/// `quota_alert`（配额预警弹窗里的快捷切换）、`tray`（托盘菜单）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorSwitchHistoryItem {
    pub id: String,
    pub timestamp: i64,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_email: Option<String>,
    pub to_account_id: String,
    pub to_email: String,
    /// 自动切换时命中阈值的额度池及其剩余百分比。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub low_metrics: Vec<CursorSwitchLowMetric>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<i32>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorSwitchLowMetric {
    pub label: String,
    pub left_percent: i32,
}

fn history_path() -> Result<PathBuf, String> {
    Ok(modules::account::get_data_dir()?.join(HISTORY_FILE))
}

pub fn load_history() -> Result<Vec<CursorSwitchHistoryItem>, String> {
    let path = history_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(&path).map_err(|e| format!("读取 Cursor 切号记录失败: {}", e))?;
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<Vec<CursorSwitchHistoryItem>>(&content) {
        Ok(items) => Ok(items),
        Err(error) => {
            let _ = modules::atomic_write::quarantine_file(&path, "invalid-json");
            modules::logger::log_warn(&format!(
                "Cursor 切号记录解析失败，已隔离并使用空记录: path={}, error={}",
                path.display(),
                error
            ));
            Ok(Vec::new())
        }
    }
}

fn save_history(items: &[CursorSwitchHistoryItem]) -> Result<(), String> {
    let path = history_path()?;
    let content = serde_json::to_string_pretty(items)
        .map_err(|e| format!("序列化 Cursor 切号记录失败: {}", e))?;
    modules::atomic_write::write_string_atomic(&path, &content)
        .map_err(|e| format!("保存 Cursor 切号记录失败: {}", e))
}

pub fn add_history_item(item: CursorSwitchHistoryItem) -> Result<(), String> {
    let _lock = HISTORY_LOCK.lock().map_err(|_| "获取切号记录锁失败")?;
    let mut existing = load_history().unwrap_or_default();
    existing.retain(|x| x.id != item.id);
    existing.push(item);
    existing.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    existing.truncate(MAX_HISTORY_ITEMS);
    save_history(&existing)
}

pub fn clear_history() -> Result<(), String> {
    let _lock = HISTORY_LOCK.lock().map_err(|_| "获取切号记录锁失败")?;
    save_history(&[])
}

/// 便捷构造：记录一次切换（成功或失败）。失败时不影响主流程，只写日志。
pub fn record_switch(
    reason: &str,
    from: Option<(&str, &str)>,
    to_account_id: &str,
    to_email: &str,
    low_metrics: Vec<CursorSwitchLowMetric>,
    threshold: Option<i32>,
    result: &Result<(), String>,
) {
    let item = CursorSwitchHistoryItem {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().timestamp(),
        reason: reason.to_string(),
        from_account_id: from.map(|(id, _)| id.to_string()),
        from_email: from.map(|(_, email)| email.to_string()),
        to_account_id: to_account_id.to_string(),
        to_email: to_email.to_string(),
        low_metrics,
        threshold,
        success: result.is_ok(),
        error: result.as_ref().err().cloned(),
    };
    if let Err(err) = add_history_item(item) {
        modules::logger::log_warn(&format!("[Cursor Switch] 写入切号记录失败: {}", err));
    }
}
