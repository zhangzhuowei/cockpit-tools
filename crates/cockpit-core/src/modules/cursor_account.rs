use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::models::cursor::{CursorAccount, CursorAccountIndex, CursorImportPayload};
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

pub fn accounts_index_path_string() -> Result<String, String> {
    Ok(get_accounts_index_path()?.to_string_lossy().to_string())
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
    crate::modules::atomic_write::parse_json_with_auto_restore(&account_path, &content).ok()
}

fn save_account_file(account: &CursorAccount) -> Result<(), String> {
    let path = resolve_account_file_path(account.id.as_str())?;
    let content =
        serde_json::to_string_pretty(account).map_err(|e| format!("序列化账号失败: {}", e))?;
    crate::modules::atomic_write::write_string_atomic(&path, &content)
        .map_err(|e| format!("保存账号失败: {}", e))
}

fn delete_account_file(account_id: &str) -> Result<(), String> {
    let path = resolve_account_file_path(account_id)?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| format!("删除账号文件失败: {}", e))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Index operations
// ---------------------------------------------------------------------------

fn load_account_index() -> CursorAccountIndex {
    let path = match get_accounts_index_path() {
        Ok(p) => p,
        Err(_) => return CursorAccountIndex::new(),
    };

    if !path.exists() {
        return CursorAccountIndex::new();
    }

    match fs::read_to_string(path.as_path()) {
        Ok(content) => match crate::modules::atomic_write::parse_json_with_auto_restore::<
            CursorAccountIndex,
        >(&path, &content)
        {
            Ok(index) => index,
            Err(err) => {
                logger::log_warn(&format!(
                    "[Cursor Account] 账号索引解析失败，使用空索引兜底: path={}, error={}",
                    path.display(),
                    err
                ));
                CursorAccountIndex::new()
            }
        },
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 读取账号索引失败，使用空索引兜底: path={}, error={}",
                path.display(),
                err
            ));
            CursorAccountIndex::new()
        }
    }
}

fn load_account_index_checked() -> Result<CursorAccountIndex, String> {
    let path = get_accounts_index_path()?;
    if !path.exists() {
        return Ok(CursorAccountIndex::new());
    }

    let content = match fs::read_to_string(path.as_path()) {
        Ok(content) => content,
        Err(err) => {
            if !collect_account_ids_from_directory().is_empty() {
                logger::log_warn(&format!(
                    "[Cursor Account] 读取账号索引失败，将按账号目录补扫恢复: path={}, error={}",
                    path.display(),
                    err
                ));
                return Ok(CursorAccountIndex::new());
            }
            return Err(format!("读取账号索引失败: {}", err));
        }
    };

    if content.trim().is_empty() {
        return Ok(CursorAccountIndex::new());
    }

    match crate::modules::atomic_write::parse_json_with_auto_restore::<CursorAccountIndex>(
        &path, &content,
    ) {
        Ok(index) => Ok(index),
        Err(err) => {
            if !collect_account_ids_from_directory().is_empty() {
                logger::log_warn(&format!(
                    "[Cursor Account] 账号索引解析失败，将按账号目录补扫恢复: path={}, error={}",
                    path.display(),
                    err
                ));
                return Ok(CursorAccountIndex::new());
            }
            Err(crate::error::file_corrupted_error(
                ACCOUNTS_INDEX_FILE,
                &path.to_string_lossy(),
                &err.to_string(),
            ))
        }
    }
}

fn save_account_index(index: &CursorAccountIndex) -> Result<(), String> {
    let path = get_accounts_index_path()?;
    let content =
        serde_json::to_string_pretty(index).map_err(|e| format!("序列化账号索引失败: {}", e))?;
    crate::modules::atomic_write::write_string_atomic(&path, &content)
        .map_err(|e| format!("写入账号索引失败: {}", e))
}

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
    let mut index = load_account_index();
    save_account_file(&account)?;
    refresh_summary(&mut index, &account);
    save_account_index(&index)?;
    Ok(account)
}

fn persist_quota_query_error(account_id: &str, message: &str) {
    let Some(mut account) = load_account(account_id) else {
        return;
    };
    account.quota_query_last_error = Some(message.to_string());
    account.quota_query_last_error_at = Some(chrono::Utc::now().timestamp_millis());
    let _ = upsert_account_record(account);
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

fn auth_ids_match(left: &str, right: &str) -> bool {
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

fn extract_auth_id_from_access_token(access_token: &str) -> Option<String> {
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

fn resolve_account_auth_id(account: &CursorAccount) -> Option<String> {
    normalize_auth_identity(account.auth_id.as_deref())
        .or_else(|| extract_auth_id_from_raw_value(account.cursor_auth_raw.as_ref()))
        .or_else(|| extract_auth_id_from_access_token(account.access_token.as_str()))
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

fn accounts_are_duplicates(left: &CursorAccount, right: &CursorAccount) -> bool {
    let left_auth_id = resolve_account_auth_id(left);
    let right_auth_id = resolve_account_auth_id(right);
    if let (Some(left_auth), Some(right_auth)) = (left_auth_id.as_ref(), right_auth_id.as_ref()) {
        return auth_ids_match(left_auth, right_auth);
    }
    if left_auth_id.is_some() || right_auth_id.is_some() {
        return false;
    }

    let left_email = normalize_email_identity(Some(left.email.as_str()));
    let right_email = normalize_email_identity(Some(right.email.as_str()));
    let left_token = normalize_token_identity(Some(left.access_token.as_str()));
    let right_token = normalize_token_identity(Some(right.access_token.as_str()));

    let email_conflict = matches!(
        (left_email.as_ref(), right_email.as_ref()),
        (Some(l), Some(r)) if l != r
    );
    if email_conflict {
        return false;
    }

    let email_match = matches!(
        (left_email.as_ref(), right_email.as_ref()),
        (Some(l), Some(r)) if l == r
    );
    let token_match = matches!(
        (left_token.as_ref(), right_token.as_ref()),
        (Some(l), Some(r)) if l == r
    );

    email_match || token_match
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

fn collect_account_ids_from_directory() -> Vec<String> {
    let accounts_dir = match get_accounts_dir() {
        Ok(dir) => dir,
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 获取账号目录失败，跳过目录补扫: {}",
                err
            ));
            return Vec::new();
        }
    };

    let entries = match fs::read_dir(&accounts_dir) {
        Ok(value) => value,
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Account] 读取账号目录失败，跳过目录补扫: path={}, error={}",
                accounts_dir.display(),
                err
            ));
            return Vec::new();
        }
    };

    let mut ids = Vec::new();
    for entry in entries {
        let Ok(item) = entry else {
            continue;
        };
        let path = item.path();
        if !path.is_file() {
            continue;
        }

        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        if !is_json {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(account_id) = normalize_account_id(stem) else {
            logger::log_warn(&format!(
                "[Cursor Account] 检测到非法账号文件名，已忽略: file={}",
                path.display()
            ));
            continue;
        };
        ids.push(account_id);
    }

    ids.sort();
    ids.dedup();
    ids
}

fn normalize_account_index(index: &mut CursorAccountIndex) -> Vec<CursorAccount> {
    let mut loaded_accounts = Vec::new();
    let mut seen_account_ids = HashSet::new();
    let mut seen_summary_ids = HashSet::new();

    for summary in &index.accounts {
        if !seen_summary_ids.insert(summary.id.clone()) {
            continue;
        }
        if let Some(account) = load_account(&summary.id) {
            if seen_account_ids.insert(account.id.clone()) {
                loaded_accounts.push(account);
            }
        }
    }

    let mut recovered_count = 0usize;
    for account_id in collect_account_ids_from_directory() {
        if seen_account_ids.contains(&account_id) {
            continue;
        }
        if let Some(account) = load_account(&account_id) {
            if seen_account_ids.insert(account.id.clone()) {
                if !seen_summary_ids.contains(&account_id) {
                    recovered_count += 1;
                }
                loaded_accounts.push(account);
            }
        }
    }
    if recovered_count > 0 {
        logger::log_warn(&format!(
            "[Cursor Account] 检测到索引缺失，已从账号目录恢复 {} 个账号",
            recovered_count
        ));
    }

    if loaded_accounts.len() <= 1 {
        index.accounts = loaded_accounts
            .iter()
            .map(|account| account.summary())
            .collect();
        return loaded_accounts;
    }

    let mut parents: Vec<usize> = (0..loaded_accounts.len()).collect();

    fn find(parents: &mut [usize], idx: usize) -> usize {
        let parent = parents[idx];
        if parent == idx {
            return idx;
        }
        let root = find(parents, parent);
        parents[idx] = root;
        root
    }

    fn union(parents: &mut [usize], left: usize, right: usize) {
        let left_root = find(parents, left);
        let right_root = find(parents, right);
        if left_root != right_root {
            parents[right_root] = left_root;
        }
    }

    let total = loaded_accounts.len();
    for left in 0..total {
        for right in (left + 1)..total {
            if accounts_are_duplicates(&loaded_accounts[left], &loaded_accounts[right]) {
                union(&mut parents, left, right);
            }
        }
    }

    let mut grouped: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..total {
        let root = find(&mut parents, idx);
        grouped.entry(root).or_default().push(idx);
    }

    let mut processed_roots = HashSet::new();
    let mut normalized_accounts = Vec::new();
    let mut removed_ids = Vec::new();
    for idx in 0..total {
        let root = find(&mut parents, idx);
        if !processed_roots.insert(root) {
            continue;
        }
        let Some(group) = grouped.get(&root) else {
            continue;
        };

        if group.len() == 1 {
            normalized_accounts.push(loaded_accounts[group[0]].clone());
            continue;
        }

        let primary_idx = choose_primary_account_index(group, &loaded_accounts);
        let mut primary = loaded_accounts[primary_idx].clone();
        for member in group {
            if *member == primary_idx {
                continue;
            }
            merge_duplicate_account(&mut primary, &loaded_accounts[*member]);
            removed_ids.push(loaded_accounts[*member].id.clone());
        }

        normalized_accounts.push(primary);
    }

    if !removed_ids.is_empty() {
        for account in &normalized_accounts {
            if let Err(err) = save_account_file(account) {
                logger::log_warn(&format!(
                    "[Cursor Account] 保存去重账号失败: id={}, error={}",
                    account.id, err
                ));
            }
        }
        for account_id in &removed_ids {
            if let Err(err) = delete_account_file(account_id) {
                logger::log_warn(&format!(
                    "[Cursor Account] 删除重复账号文件失败: id={}, error={}",
                    account_id, err
                ));
            }
        }
        logger::log_warn(&format!(
            "[Cursor Account] 检测到重复账号并已合并: removed_ids={}",
            removed_ids.join(",")
        ));
    }

    index.accounts = normalized_accounts
        .iter()
        .map(|account| account.summary())
        .collect();
    normalized_accounts
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

pub fn list_accounts() -> Vec<CursorAccount> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut index = load_account_index();
    let accounts = normalize_account_index(&mut index);
    if let Err(err) = save_account_index(&index) {
        logger::log_warn(&format!("[Cursor Account] 保存账号索引失败: {}", err));
    }
    accounts
}

pub fn list_accounts_checked() -> Result<Vec<CursorAccount>, String> {
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;
    let mut index = load_account_index_checked()?;
    let accounts = normalize_account_index(&mut index);
    if let Err(err) = save_account_index(&index) {
        logger::log_warn(&format!("[Cursor Account] 保存账号索引失败: {}", err));
    }
    Ok(accounts)
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
    let _lock = CURSOR_ACCOUNT_INDEX_LOCK
        .lock()
        .map_err(|_| "获取 Cursor 账号锁失败".to_string())?;

    let now = now_ts();
    let mut index = load_account_index();
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
    let tags = existing.as_ref().and_then(|acc| acc.tags.clone());
    let created_at = existing.as_ref().map(|acc| acc.created_at).unwrap_or(now);

    let mut account = existing.unwrap_or(CursorAccount {
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
        status: payload.status.clone(),
        status_reason: payload.status_reason.clone(),
        quota_query_last_error: None,
        quota_query_last_error_at: None,
        quota_query_auth_failures: None,
        usage_updated_at: None,
        created_at,
        last_used: now,
    });

    apply_payload(&mut account, payload, incoming_auth_id);
    account.id = account_id;
    account.created_at = created_at;
    account.quota_query_last_error = None;
    account.quota_query_last_error_at = None;
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
    let mut index = load_account_index();
    index.accounts.retain(|item| item.id != account_id);
    save_account_index(&index)?;
    delete_account_file(account_id)?;
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

fn read_vscdb_item(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .and_then(|v| {
        let trimmed = v.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

pub fn read_local_cursor_auth() -> Result<Option<CursorImportPayload>, String> {
    let db_path = get_default_cursor_state_db_path()?;
    if !db_path.exists() {
        return Ok(None);
    }

    let conn = Connection::open(&db_path)
        .map_err(|e| format!("打开 Cursor 本地数据库失败({}): {}", db_path.display(), e))?;

    let access_token = match read_vscdb_item(&conn, "cursorAuth/accessToken") {
        Some(t) => t,
        None => return Ok(None),
    };

    let email = read_vscdb_item(&conn, "cursorAuth/cachedEmail").unwrap_or_default();
    if email.is_empty() {
        return Ok(None);
    }

    let refresh_token = read_vscdb_item(&conn, "cursorAuth/refreshToken");
    // token 的 sub 才是当前实际生效的身份；cursorAuth/authId 可能是上一个账号的残留值。
    let auth_id = extract_auth_id_from_access_token(access_token.as_str())
        .or_else(|| read_vscdb_item(&conn, "cursorAuth/authId"));
    let membership_type = read_vscdb_item(&conn, "cursorAuth/stripeMembershipType");
    let subscription_status = read_vscdb_item(&conn, "cursorAuth/stripeSubscriptionStatus");
    let sign_up_type = read_vscdb_item(&conn, "cursorAuth/cachedSignUpType");

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
    let account = upsert_account(payload)?;
    logger::log_info(&format!(
        "[Cursor Account] 从本地导入成功: id={}, email={}",
        account.id, account.email
    ));
    Ok(Some(account))
}

// ---------------------------------------------------------------------------
// Inject (write auth fields back to Cursor's state.vscdb)
// ---------------------------------------------------------------------------

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
fn resolve_refresh_token_for_injection(account: &CursorAccount) -> Option<String> {
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

fn touch_account_last_used(account_id: &str) {
    let Some(mut account) = load_account(account_id) else {
        return;
    };
    account.last_used = now_ts();
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

const CURSOR_AUTH_ERROR: &str = "Cursor 会话已过期或未认证，请重新导入账号";
/// 连续这么多次刷新在 Bearer 与 Cookie 两条鉴权路径上都被 401/403 拒绝，才把账号标成 error。
/// 单次失败可能只是网络抖动或 token 刚过期、下一轮就会由 refresh_token 续上。
const CURSOR_AUTH_FAILURE_MARK_THRESHOLD: u32 = 3;
const CURSOR_AUTH_FAILURE_STATUS_REASON: &str =
    "token auth failed repeatedly (HTTP 401/403); re-login or update the token";

const CURSOR_USAGE_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
const CURSOR_SAND_USAGE_STATUS_URL: &str = "https://cursor.com/api/dashboard/get-sand-usage-status";
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

fn build_cursor_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))
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

fn build_session_cookie(access_token: &str) -> Option<String> {
    let user_id = extract_workos_user_id(access_token)?;
    Some(format!(
        "WorkosCursorSessionToken={}%3A%3A{}",
        user_id, access_token
    ))
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
        .map_err(|e| format!("请求 Cursor token 刷新接口失败: {}", e))?;

    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor token 刷新响应失败: {}", e))?;

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
        .map_err(|e| format!("请求 Cursor user meta 失败: {}", e))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if status != 200 {
        return Err(format!("Cursor user meta API 返回异常状态码: {}", status));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor user meta 响应失败: {}", e))?;

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
        .map_err(|e| format!("请求 Cursor full stripe profile 失败: {}", e))?;

    let full_status = full_response.status().as_u16();
    if full_status == 401 || full_status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if full_status == 200 {
        let body = full_response
            .text()
            .await
            .map_err(|e| format!("读取 Cursor full stripe profile 响应失败: {}", e))?;
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
        .map_err(|e| format!("请求 Cursor stripe profile 失败: {}", e))?;

    let fallback_status = fallback_response.status().as_u16();
    if fallback_status == 401 || fallback_status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if fallback_status != 200 {
        return Ok(None);
    }

    let body = fallback_response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor stripe profile 响应失败: {}", e))?;

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
        .map_err(|e| format!("请求 Cursor usage API 失败: {}", e))?;

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
        .map_err(|e| format!("读取 Cursor usage 响应失败: {}", e))?;

    serde_json::from_str::<serde_json::Value>(&body)
        .map_err(|e| format!("解析 Cursor usage JSON 失败: {}", e))
}

fn attach_usage_object(usage: &mut Value, key: &str, value: Value) {
    if let Some(obj) = usage.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
}

// Grok Bot 是附加信息，不能拖慢主配额刷新；比全局 15s 更短，且两次尝试合计不超过主请求。
const CURSOR_SAND_USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// 上一轮的 Grok Bot 数据只在其周重置点还没过去时才值得沿用，否则宁可不显示。
fn reusable_previous_sand(previous: Option<Value>) -> Option<Value> {
    let value = previous?;
    let reset_ts = grok_bot_from_object(Some(&value))
        .reset_ts
        .or_else(|| grok_bot_from_object(value.get("status")).reset_ts)
        .or_else(|| grok_bot_from_object(value.get("usage")).reset_ts)?;
    (reset_ts > now_ts()).then_some(value)
}

async fn fetch_sand_usage_via_dashboard(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Value, String> {
    let cookie = build_session_cookie(access_token)
        .ok_or_else(|| "无法从 accessToken 解析 WorkOS 用户 ID".to_string())?;

    let response = client
        .post(CURSOR_SAND_USAGE_STATUS_URL)
        .timeout(CURSOR_SAND_USAGE_TIMEOUT)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Cookie", &cookie)
        .header("Origin", "https://cursor.com")
        .header("Referer", "https://cursor.com/dashboard")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
        )
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("请求 Cursor Grok Bot 用量接口失败: {}", e))?;

    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(CURSOR_AUTH_ERROR.to_string());
    }
    if status != 200 {
        return Err(format!(
            "Cursor Grok Bot 用量接口返回异常状态码: {}",
            status
        ));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor Grok Bot 用量响应失败: {}", e))?;

    serde_json::from_str::<Value>(&body)
        .map_err(|e| format!("解析 Cursor Grok Bot 用量 JSON 失败: {}", e))
}

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
        .map_err(|e| format!("请求 Cursor Grok Bot RPC 失败: {}", e))?;

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

    let body = response
        .text()
        .await
        .map_err(|e| format!("读取 Cursor Grok Bot RPC 响应失败: {}", e))?;

    serde_json::from_str::<Value>(&body)
        .map_err(|e| format!("解析 Cursor Grok Bot RPC JSON 失败: {}", e))
}

async fn fetch_optional_sand_usage(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Option<Value> {
    match fetch_sand_usage_via_dashboard(client, access_token).await {
        Ok(value) if value.is_object() => return Some(value),
        Ok(_) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] Grok Bot 用量响应不是对象: id={}",
                account_id
            ));
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] Grok Bot Dashboard 用量拉取失败: id={}, error={}",
                account_id, err
            ));
        }
    }

    match fetch_sand_usage_via_rpc(client, access_token).await {
        Ok(value) if value.is_object() => Some(value),
        Ok(_) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] Grok Bot RPC 用量响应不是对象: id={}",
                account_id
            ));
            None
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] Grok Bot RPC 用量拉取失败: id={}, error={}",
                account_id, err
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Refresh (updates our own account storage + fetches usage from official APIs)
// ---------------------------------------------------------------------------

fn is_auth_error(message: &str) -> bool {
    message == CURSOR_AUTH_ERROR
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

    let mut usage_refreshed = false;
    match fetch_usage_summary_with_client(&client, &account.access_token).await {
        Ok(mut usage) => {
            if let Some(mt) = usage.get("membershipType").and_then(|v| v.as_str()) {
                if !mt.is_empty() {
                    account.membership_type = Some(mt.to_string());
                }
            }
            let previous_sand = reusable_previous_sand(
                account
                    .cursor_usage_raw
                    .as_ref()
                    .and_then(|raw| raw.get("sandUsage").cloned()),
            );
            if let Some(sand) =
                fetch_optional_sand_usage(&client, &account.access_token, &account.id).await
            {
                attach_usage_object(&mut usage, "sandUsage", sand);
                logger::log_info(&format!(
                    "[Cursor Refresh] Grok Bot 周用量拉取成功: id={}",
                    account.id
                ));
            } else if let Some(sand) = previous_sand {
                attach_usage_object(&mut usage, "sandUsage", sand);
            }
            account.cursor_usage_raw = Some(usage);
            account.quota_query_last_error = None;
            account.quota_query_last_error_at = None;
            clear_auth_failures(&mut account);
            usage_refreshed = true;
            logger::log_info(&format!(
                "[Cursor Refresh] API 配额拉取成功: id={}",
                account.id
            ));
        }
        Err(err) => {
            logger::log_warn(&format!(
                "[Cursor Refresh] API 配额拉取失败: id={}, error={}",
                account.id, err
            ));
            // Cookie 路径与 Bearer 路径同时被拒，才算一次真正的鉴权失败；
            // 只有一条路径失败更可能是接口抖动，不计入。
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
    let updated = account.clone();
    upsert_account_record(account)?;
    logger::log_info(&format!(
        "[Cursor Refresh] 刷新完成: id={}, email={}",
        updated.id, updated.email
    ));
    Ok(updated)
}

pub async fn refresh_account_async(account_id: &str) -> Result<CursorAccount, String> {
    let result = refresh_account_async_once(account_id).await;
    if let Err(err) = &result {
        persist_quota_query_error(account_id, err);
    }
    result
}

pub async fn refresh_all_tokens() -> Result<Vec<(String, Result<CursorAccount, String>)>, String> {
    let accounts = list_accounts();
    let active_accounts: Vec<CursorAccount> = accounts
        .into_iter()
        .filter(|account| !is_banned_account(account))
        .collect();

    let mut results = Vec::with_capacity(active_accounts.len());
    for account in active_accounts {
        let id = account.id.clone();
        let result = refresh_account_async(&id).await;
        results.push((id, result));
    }
    Ok(results)
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
    // 托盘/菜单展示用；core crate 内暂无消费者。
    #[allow(dead_code)]
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

    let plan_value = raw_obj
        .get("individualUsage")
        .and_then(|value| value.as_object())
        .and_then(|value| value.get("plan"))
        .or_else(|| {
            raw_obj
                .get("individual_usage")
                .and_then(|value| value.as_object())
                .and_then(|value| value.get("plan"))
        })
        .or_else(|| raw_obj.get("planUsage"))
        .or_else(|| raw_obj.get("plan_usage"));

    let total_direct = pick_number(plan_value, &["totalPercentUsed", "total_percent_used"]);
    let auto_direct = pick_number(plan_value, &["autoPercentUsed", "auto_percent_used"]);
    let api_direct = pick_number(plan_value, &["apiPercentUsed", "api_percent_used"]);

    let used = pick_number(plan_value, &["used", "totalSpend", "total_spend"]);
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
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    accounts
        .iter()
        .max_by_key(|account| account.last_used)
        .map(|account| account.id.clone())
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

    fn fake_jwt_typed(sub: &str, exp: i64, kind: &str) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = engine.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = engine.encode(
            format!(r#"{{"sub":"{}","exp":{},"type":"{}"}}"#, sub, exp, kind).as_bytes(),
        );
        format!("{}.{}.sig", header, payload)
    }

    fn account_with(access: &str, refresh: Option<&str>) -> CursorAccount {
        CursorAccount {
            id: "cursor_test".to_string(),
            email: "a@example.com".to_string(),
            auth_id: Some("auth0|user_1".to_string()),
            name: None,
            tags: None,
            access_token: access.to_string(),
            refresh_token: refresh.map(str::to_string),
            membership_type: Some("pro".to_string()),
            subscription_status: Some("active".to_string()),
            sign_up_type: None,
            cursor_auth_raw: None,
            cursor_usage_raw: None,
            status: None,
            status_reason: None,
            quota_query_last_error: None,
            quota_query_last_error_at: None,
            quota_query_auth_failures: None,
            usage_updated_at: None,
            created_at: 0,
            last_used: 0,
        }
    }

    fn write_of<'a>(writes: &'a [(&str, Option<String>)], key: &str) -> &'a Option<String> {
        &writes.iter().find(|(k, _)| *k == key).expect("key present").1
    }

    #[test]
    fn injection_uses_session_access_token_as_refresh_token_fallback() {
        let session = fake_jwt_typed("auth0|user_1", 4_000_000_000, "session");
        let writes = build_auth_key_writes(&account_with(&session, None));
        assert_eq!(write_of(&writes, "cursorAuth/refreshToken"), &Some(session.clone()));
        assert_eq!(write_of(&writes, "cursorAuth/accessToken"), &Some(session));
        // Cursor 自己总写 cachedSignUpType；未知时给默认值而不是删键
        assert_eq!(write_of(&writes, "cursorAuth/cachedSignUpType"), &Some("Auth_0".to_string()));
    }

    #[test]
    fn injection_drops_refresh_token_for_non_session_jwt_without_refresh() {
        let web = fake_jwt_typed("auth0|user_1", 4_000_000_000, "web");
        let writes = build_auth_key_writes(&account_with(&web, None));
        assert_eq!(write_of(&writes, "cursorAuth/refreshToken"), &None);
    }

    #[test]
    fn injection_prefers_explicit_refresh_token_and_clears_stale_profile_keys() {
        let session = fake_jwt_typed("auth0|user_1", 4_000_000_000, "session");
        let writes = build_auth_key_writes(&account_with(&session, Some("explicit-refresh")));
        assert_eq!(
            write_of(&writes, "cursorAuth/refreshToken"),
            &Some("explicit-refresh".to_string())
        );
        for key in CURSOR_STALE_PROFILE_KEYS {
            assert_eq!(write_of(&writes, key), &None, "{} should be deleted", key);
        }
        assert!(writes.iter().all(|(k, _)| *k != "cursorAuth/isLoggedIn"));
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
