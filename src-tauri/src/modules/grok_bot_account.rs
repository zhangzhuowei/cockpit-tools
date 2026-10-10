//! 把 Cockpit 里已有的 Cursor session JWT 写入本机 Grok Bot。
//!
//! Grok Bot 的登录态就是 Cursor 凭据：`sand-secrets.json` 的 `cursor-accounts`
//! 里每个槽位存 OSCrypt 加密后的 access / refresh / profile。切号流程与
//! Sand-Relay-Desktop 一致：退出 Grok Bot → 写槽位并设 active → 再拉起。
//! 不涉及 Box Relay / 直连 Sand。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit};
#[cfg(test)]
use aes_gcm::Nonce;
#[cfg(any(target_os = "windows", target_os = "macos"))]
use base64::{engine::general_purpose, Engine as _};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::models::cursor::CursorAccount;
use crate::modules::{atomic_write, cursor_account, logger};

const V10_PREFIX: &[u8] = b"v10";
const SLOT_PREFIX: &[u8] = b"sand-account-slot\x00";
const QUIT_WAIT_SECS: u64 = 15;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static SWITCH_LOCK: Mutex<()> = Mutex::new(());

pub fn switch_local_grok_bot_with_cursor_account(account_id: &str) -> Result<String, String> {
    let _guard = SWITCH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let account = cursor_account::list_accounts()
        .into_iter()
        .find(|item| item.id == account_id)
        .ok_or_else(|| format!("找不到 Cursor 账号 {}", account_id))?;

    let prepared = prepare_cursor_credentials(&account)?;
    let secrets_path = grok_secrets_path()?;
    if !secrets_path.is_file() {
        return Err(format!(
            "未找到 Grok Bot 账号存储 {}；请先安装并登录过一次 Grok Bot",
            secrets_path.display()
        ));
    }

    let exe = find_grok_bot_launch_target();
    quit_grok_bot()?;
    std::thread::sleep(Duration::from_secs(1));
    if grok_bot_is_running() {
        return Err("Grok Bot 仍在运行，未修改登录数据；请先手动退出后再试".to_string());
    }

    let original = fs::read(&secrets_path).map_err(|err| {
        format!(
            "读取 Grok Bot 登录数据失败: path={}, error={}",
            secrets_path.display(),
            err
        )
    })?;
    backup_secrets(&secrets_path);

    let user_data_dir = grok_user_data_dir()?;
    let write_result = (|| {
        let (mut root, mut accounts) = read_secrets(&secrets_path)?;
        let profile = serde_json::to_string(&json!({
            "authId": prepared.sub,
            "email": prepared.email,
        }))
        .map_err(|err| format!("序列化 Grok Bot 账号资料失败: {}", err))?;
        let encrypted = oscrypt_encrypt(
            &[
                prepared.access_jwt.clone(),
                prepared.refresh_jwt.clone(),
                profile,
            ],
            &user_data_dir,
        )?;
        let target = upsert_cursor_account_slot(
            &mut accounts,
            &prepared.slot,
            &prepared.legacy_slot,
            &encrypted[0],
            &encrypted[1],
            &encrypted[2],
        );
        write_secrets(&secrets_path, &mut root, &accounts)?;
        Ok::<String, String>(target)
    })();

    let target = match write_result {
        Ok(slot) => slot,
        Err(err) => {
            if let Err(restore_err) = fs::write(&secrets_path, &original) {
                logger::log_warn(&format!(
                    "[Grok Bot] 回滚 sand-secrets.json 失败: {}",
                    restore_err
                ));
            }
            return Err(format!("切换 Grok Bot 账号失败：{}", err));
        }
    };

    logger::log_info(&format!(
        "[Grok Bot] sand-secrets.json 已更新，active → {}…",
        target.chars().take(16).collect::<String>()
    ));

    match launch_grok_bot(exe.as_deref()) {
        Ok(()) => Ok(format!(
            "已将本地 Grok Bot 切换为 {} 并重新启动",
            prepared.email
        )),
        Err(launch_err) => {
            logger::log_warn(&format!(
                "[Grok Bot] 启动失败：{}；登录数据已切换，请手动打开 Grok Bot",
                launch_err
            ));
            Ok(format!(
                "已将本地 Grok Bot 切换为 {}。自动启动失败（{}），请手动打开 Grok Bot",
                prepared.email, launch_err
            ))
        }
    }
}

struct PreparedCredentials {
    access_jwt: String,
    refresh_jwt: String,
    sub: String,
    email: String,
    slot: String,
    legacy_slot: String,
}

fn prepare_cursor_credentials(account: &CursorAccount) -> Result<PreparedCredentials, String> {
    cursor_account::ensure_token_usable_for_desktop(account)?;
    let access_jwt = cursor_account::normalize_import_access_token(&account.access_token);
    if access_jwt.split('.').count() < 3 {
        return Err("Token 格式无法识别，需要 Cursor 桌面 session JWT".to_string());
    }
    let refresh_jwt = cursor_account::resolve_refresh_token_for_injection(account)
        .map(|token| cursor_account::normalize_import_access_token(&token))
        .filter(|token| token.split('.').count() >= 3)
        .ok_or_else(|| {
            "这份 token 不含 refresh，无法登录 Grok Bot（切 Cursor 不受影响）。请用 OAuth 重新登录，或粘贴 user_xxx::JWT 形式的桌面会话 token。".to_string()
        })?;
    let sub = cursor_account::extract_auth_id_from_access_token(&access_jwt)
        .or_else(|| {
            account
                .auth_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| "Token 缺少账号标识（sub）".to_string())?;
    let email = {
        let trimmed = account.email.trim();
        if trimmed.is_empty() {
            account.id.clone()
        } else {
            trimmed.to_string()
        }
    };
    Ok(PreparedCredentials {
        slot: account_slot(&sub),
        legacy_slot: legacy_account_slot(&sub),
        access_jwt,
        refresh_jwt,
        sub,
        email,
    })
}

fn account_slot(sub: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SLOT_PREFIX);
    hasher.update(sub.as_bytes());
    hex_encode(&hasher.finalize())
}

fn legacy_account_slot(sub: &str) -> String {
    hex_encode(&Sha256::digest(sub.as_bytes()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{:02x}", byte)).collect()
}

fn grok_user_data_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from);
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        for base in roaming.clone().into_iter().chain(local) {
            let dir = base.join("Grok Bot");
            if dir.join("gateway-descriptor.json").is_file() || dir.join("sand-secrets.json").is_file()
            {
                return Ok(dir);
            }
        }
        return roaming
            .map(|base| base.join("Grok Bot"))
            .ok_or_else(|| "无法获取 APPDATA，找不到 Grok Bot 数据目录".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        return dirs::home_dir()
            .map(|home| home.join("Library/Application Support/Grok Bot"))
            .ok_or_else(|| "无法定位 Grok Bot 数据目录".to_string());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        dirs::home_dir()
            .map(|home| home.join(".config/Grok Bot"))
            .ok_or_else(|| "无法定位 Grok Bot 数据目录".to_string())
    }
}

fn grok_secrets_path() -> Result<PathBuf, String> {
    Ok(grok_user_data_dir()?.join("sand-secrets.json"))
}

fn is_grok_bot_process_name(name: &str) -> bool {
    name.trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .eq_ignore_ascii_case("Grok Bot")
}

fn collect_grok_bot_processes() -> Vec<(u32, Option<PathBuf>)> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );
    let mut entries = Vec::new();
    for (pid, process) in system.processes() {
        let name = process.name().to_string_lossy();
        if !is_grok_bot_process_name(&name) {
            continue;
        }
        entries.push((pid.as_u32(), process.exe().map(Path::to_path_buf)));
    }
    entries.sort_by_key(|(pid, _)| *pid);
    entries.dedup_by_key(|(pid, _)| *pid);
    entries
}

fn grok_bot_is_running() -> bool {
    !collect_grok_bot_processes().is_empty()
}

fn quit_grok_bot() -> Result<(), String> {
    let pids: Vec<u32> = collect_grok_bot_processes()
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    if pids.is_empty() {
        return Ok(());
    }
    logger::log_info(&format!(
        "[Grok Bot] 请求退出进程: pids={}",
        pids.iter()
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ));

    #[cfg(target_os = "windows")]
    {
        for pid in &pids {
            taskkill_pid(*pid, false);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("osascript")
            .args(["-e", "tell application \"Grok Bot\" to quit"])
            .output();
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        for pid in &pids {
            let _ = Command::new("kill").args(["-15", &pid.to_string()]).output();
        }
    }

    wait_until_grok_bot_exits(QUIT_WAIT_SECS);
    if !grok_bot_is_running() {
        return Ok(());
    }

    let remaining: Vec<u32> = collect_grok_bot_processes()
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    logger::log_info(&format!(
        "[Grok Bot] 15 秒内未退出，改为强制结束: pids={}",
        remaining
            .iter()
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ));
    for pid in remaining {
        crate::modules::process::close_pid(pid, 5)?;
    }
    wait_until_grok_bot_exits(5);
    if grok_bot_is_running() {
        return Err("Grok Bot 在超时后仍未退出，未修改登录数据".to_string());
    }
    Ok(())
}

fn wait_until_grok_bot_exits(timeout_secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while Instant::now() < deadline {
        if !grok_bot_is_running() {
            return;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

#[cfg(target_os = "windows")]
fn taskkill_pid(pid: u32, force: bool) {
    use std::os::windows::process::CommandExt;
    let mut args = vec!["/PID".to_string(), pid.to_string(), "/T".to_string()];
    if force {
        args.push("/F".to_string());
    }
    let _ = Command::new("taskkill")
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output();
}

fn find_grok_bot_launch_target() -> Option<PathBuf> {
    for (_, exe) in collect_grok_bot_processes() {
        if let Some(path) = exe {
            if path.is_file() {
                return Some(path);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        return find_grok_bot_exe_windows();
    }
    #[cfg(target_os = "macos")]
    {
        let app = PathBuf::from("/Applications/Grok Bot.app");
        if app.exists() {
            return Some(app);
        }
        None
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

#[cfg(target_os = "windows")]
fn find_grok_bot_exe_windows() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for key in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(key) {
            let base = PathBuf::from(base);
            candidates.push(base.join("Programs").join("Grok Bot").join("Grok Bot.exe"));
            candidates.push(base.join("Grok Bot").join("Grok Bot.exe"));
        }
    }
    if let Some(found) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(found);
    }

    for key in ["APPDATA", "ProgramData"] {
        if let Some(base) = std::env::var_os(key) {
            let lnk = PathBuf::from(base)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Grok Bot.lnk");
            if let Some(target) = resolve_windows_shortcut_target(&lnk) {
                if target.is_file() {
                    return Some(target);
                }
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn resolve_windows_shortcut_target(lnk: &Path) -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    if !lnk.is_file() {
        return None;
    }
    let escaped = lnk.to_string_lossy().replace('\'', "''");
    let script = format!(
        "(New-Object -ComObject WScript.Shell).CreateShortcut('{}').TargetPath",
        escaped
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let target = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if target.is_empty() {
        None
    } else {
        Some(PathBuf::from(target))
    }
}

fn launch_grok_bot(exe: Option<&Path>) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let exe = exe
            .filter(|path| path.is_file())
            .map(Path::to_path_buf)
            .or_else(find_grok_bot_exe_windows)
            .ok_or_else(|| "未找到 Grok Bot.exe；请先手动打开一次 Grok Bot".to_string())?;
        return launch_grok_bot_windows(&exe);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = exe;
        Command::new("open")
            .args(["-a", "Grok Bot"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("启动 Grok Bot 失败: {}", err))?;
        return Ok(());
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = exe;
        Err("切换本地 Grok Bot 目前仅支持 Windows 和 macOS".to_string())
    }
}

#[cfg(target_os = "windows")]
fn launch_grok_bot_windows(exe: &Path) -> Result<(), String> {
    let mut command = if current_process_is_elevated() {
        let mut cmd = Command::new("explorer.exe");
        cmd.arg(exe);
        cmd
    } else {
        Command::new(exe)
    };
    // 不要带 CREATE_NO_WINDOW：那会把 Grok Bot 窗口一起藏掉。
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map_err(|err| format!("启动 Grok Bot 失败: {}", err))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn current_process_is_elevated() -> bool {
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    unsafe { IsUserAnAdmin().as_bool() }
}

fn backup_secrets(path: &Path) {
    let backup = path.with_extension("json.bak");
    if let Err(err) = fs::copy(path, &backup) {
        logger::log_warn(&format!(
            "[Grok Bot] 备份 sand-secrets.json 失败: {}",
            err
        ));
    }
}

fn read_secrets(path: &Path) -> Result<(Value, Value), String> {
    let text = fs::read_to_string(path).map_err(|err| {
        format!(
            "读取 {} 失败: {}",
            path.display(),
            err
        )
    })?;
    let root: Value = serde_json::from_str(&text)
        .map_err(|err| format!("解析 sand-secrets.json 失败: {}", err))?;
    if !root.is_object() {
        return Err("sand-secrets.json 结构无法识别".to_string());
    }
    let mut accounts = match root.get("cursor-accounts") {
        Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or_else(|_| json!({})),
        Some(value) if value.is_object() => value.clone(),
        _ => json!({ "active": "", "accounts": {} }),
    };
    normalize_accounts_container(&mut accounts);
    Ok((root, accounts))
}

fn write_secrets(path: &Path, root: &mut Value, accounts: &Value) -> Result<(), String> {
    let compact = serde_json::to_string(accounts)
        .map_err(|err| format!("序列化 Grok Bot cursor-accounts 失败: {}", err))?;
    root["cursor-accounts"] = Value::String(compact);
    let encoded = serde_json::to_string_pretty(root)
        .map_err(|err| format!("序列化 sand-secrets.json 失败: {}", err))?;
    serde_json::from_str::<Value>(&encoded)
        .map_err(|err| format!("写入前自检 sand-secrets.json 失败: {}", err))?;
    atomic_write::write_secret_string_atomic(path, &encoded)
}

fn normalize_accounts_container(accounts: &mut Value) {
    if !accounts.is_object() {
        *accounts = json!({ "active": "", "accounts": {} });
        return;
    }
    let obj = accounts.as_object_mut().expect("object just checked");
    match obj.get("accounts") {
        Some(Value::Array(items)) => {
            let mut mapped = Map::new();
            for (index, item) in items.iter().enumerate() {
                mapped.insert(index.to_string(), item.clone());
            }
            obj.insert("accounts".to_string(), Value::Object(mapped));
        }
        Some(Value::Object(_)) => {}
        _ => {
            obj.insert("accounts".to_string(), json!({}));
        }
    }
}

fn upsert_cursor_account_slot(
    accounts: &mut Value,
    slot: &str,
    legacy_slot: &str,
    access_enc: &str,
    refresh_enc: &str,
    profile_enc: &str,
) -> String {
    normalize_accounts_container(accounts);
    let map = accounts
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .expect("accounts map normalized");
    let target = if map.contains_key(legacy_slot) && !map.contains_key(slot) {
        legacy_slot
    } else {
        slot
    };
    map.insert(
        target.to_string(),
        json!({
            "cursor-access-token": access_enc,
            "cursor-refresh-token": refresh_enc,
            "cursor-account-profile": profile_enc,
        }),
    );
    accounts["active"] = Value::String(target.to_string());
    target.to_string()
}

fn oscrypt_encrypt(values: &[String], user_data_dir: &Path) -> Result<Vec<String>, String> {
    #[cfg(target_os = "windows")]
    {
        return oscrypt_encrypt_windows(values, user_data_dir);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = user_data_dir;
        return oscrypt_encrypt_macos(values);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (values, user_data_dir);
        Err("当前平台不支持写入 Grok Bot 账号".to_string())
    }
}

fn encrypt_windows_gcm_v10(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    if key.len() != 32 {
        return Err(format!(
            "Grok Bot Windows OSCrypt 主密钥长度异常：{}（预期 32）",
            key.len()
        ));
    }
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|err| format!("AES-GCM 加密失败: {}", err))?;
    let mut result = Vec::with_capacity(3 + 12 + ciphertext.len());
    result.extend_from_slice(V10_PREFIX);
    result.extend_from_slice(nonce.as_slice());
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

#[cfg(test)]
fn decrypt_windows_gcm_v10(key: &[u8], encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if encrypted.len() < 31 || !encrypted.starts_with(V10_PREFIX) {
        return Err("不是 Windows v10 OSCrypt 数据".to_string());
    }
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Nonce::from_slice(&encrypted[3..15]);
    cipher
        .decrypt(nonce, &encrypted[15..])
        .map_err(|err| format!("AES-GCM 解密失败: {}", err))
}

#[cfg(target_os = "windows")]
fn oscrypt_encrypt_windows(values: &[String], user_data_dir: &Path) -> Result<Vec<String>, String> {
    let key = windows_os_crypt_key(user_data_dir)?;
    values
        .iter()
        .map(|value| {
            let encrypted = encrypt_windows_gcm_v10(&key, value.as_bytes())?;
            Ok(general_purpose::STANDARD.encode(encrypted))
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn windows_os_crypt_key(user_data_dir: &Path) -> Result<Vec<u8>, String> {
    let local_state_path = user_data_dir.join("Local State");
    let content = fs::read_to_string(&local_state_path).map_err(|err| {
        format!(
            "无法读取 Grok Bot Windows OSCrypt 主密钥：{} ({})",
            local_state_path.display(),
            err
        )
    })?;
    let json: Value = serde_json::from_str(&content).map_err(|err| {
        format!(
            "解析 Grok Bot Local State 失败: {}",
            err
        )
    })?;
    let encoded_key = json["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| "Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let wrapped = general_purpose::STANDARD
        .decode(encoded_key)
        .map_err(|err| format!("Base64 解码 encrypted_key 失败: {}", err))?;
    if wrapped.len() < 6 || !wrapped.starts_with(b"DPAPI") {
        let prefix = String::from_utf8_lossy(&wrapped[..wrapped.len().min(5)]);
        return Err(format!(
            "Grok Bot Windows OSCrypt 主密钥格式不受支持（前缀 {}，预期 DPAPI）",
            prefix
        ));
    }
    let key = dpapi_decrypt(&wrapped[5..])?;
    if key.len() != 32 {
        return Err(format!(
            "Grok Bot Windows OSCrypt 主密钥长度异常：{}（预期 32）",
            key.len()
        ));
    }
    Ok(key)
}

#[cfg(target_os = "windows")]
fn dpapi_decrypt(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        CryptUnprotectData(&mut input, None, None, None, None, 0, &mut output).map_err(|_| {
            "Windows DPAPI CryptUnprotectData 解密失败；请确认当前 Windows 用户即安装 Grok Bot 的用户"
                .to_string()
        })?;
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(HLOCAL(output.pbData as *mut _));
        Ok(result)
    }
}

#[cfg(target_os = "macos")]
fn oscrypt_encrypt_macos(values: &[String]) -> Result<Vec<String>, String> {
    use aes::Aes128;
    use cbc::cipher::block_padding::Pkcs7;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    use pbkdf2::pbkdf2_hmac;
    use sha1::Sha1;

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;
    const SALT: &[u8] = b"saltysalt";
    const CBC_IV: [u8; 16] = [b' '; 16];

    let password = macos_safe_storage_password()?;
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), SALT, 1003, &mut key);

    values
        .iter()
        .map(|value| {
            let cipher = Aes128CbcEnc::new_from_slices(&key, &CBC_IV)
                .map_err(|err| format!("初始化 Grok Bot AES-CBC 失败: {}", err))?;
            let mut buf = value.as_bytes().to_vec();
            let msg_len = buf.len();
            let pad_len = 16 - (msg_len % 16);
            buf.resize(msg_len + pad_len, 0);
            let ciphertext = cipher
                .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
                .map_err(|err| format!("AES-CBC 加密失败: {}", err))?
                .to_vec();
            let mut result = Vec::with_capacity(3 + ciphertext.len());
            result.extend_from_slice(V10_PREFIX);
            result.extend_from_slice(&ciphertext);
            Ok(general_purpose::STANDARD.encode(result))
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn macos_safe_storage_password() -> Result<String, String> {
    for service in ["Grok Bot Safe Storage", "Grok Bot"] {
        if let Some(password) = run_command_get_trimmed(
            "security",
            &["find-generic-password", "-w", "-s", service],
        ) {
            return Ok(password);
        }
    }
    Err("无法从钥匙串读取 Grok Bot Safe Storage；请先打开一次 Grok Bot 并允许访问钥匙串".to_string())
}

#[cfg(target_os = "macos")]
fn run_command_get_trimmed(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_slot_matches_sand_relay() {
        let sub = "auth0|user_01000000000000000000000000";
        assert_eq!(
            account_slot(sub),
            "035e432cb602b9b72ed72a2493c17f06ac3b6f7431a557bcc4a7f116b09517aa"
        );
        assert_eq!(
            legacy_account_slot(sub),
            "33ffbc8e3477912e8892f2bdf46c45d3c32195fe7140a4c12a2e0ab2e31fd714"
        );
    }

    #[test]
    fn upsert_reuses_legacy_slot_when_new_slot_absent() {
        let mut accounts = json!({
            "active": "old",
            "accounts": {
                "33ffbc8e3477912e8892f2bdf46c45d3c32195fe7140a4c12a2e0ab2e31fd714": {
                    "cursor-access-token": "old"
                }
            }
        });
        let target = upsert_cursor_account_slot(
            &mut accounts,
            "035e432cb602b9b72ed72a2493c17f06ac3b6f7431a557bcc4a7f116b09517aa",
            "33ffbc8e3477912e8892f2bdf46c45d3c32195fe7140a4c12a2e0ab2e31fd714",
            "acc",
            "ref",
            "prof",
        );
        assert_eq!(
            target,
            "33ffbc8e3477912e8892f2bdf46c45d3c32195fe7140a4c12a2e0ab2e31fd714"
        );
        assert_eq!(accounts["active"], target);
        assert_eq!(
            accounts["accounts"][target]["cursor-refresh-token"],
            "ref"
        );
        assert!(accounts["accounts"]
            .get("035e432cb602b9b72ed72a2493c17f06ac3b6f7431a557bcc4a7f116b09517aa")
            .is_none());
    }

    #[test]
    fn upsert_prefers_new_slot_when_both_exist() {
        let mut accounts = json!({
            "accounts": {
                "legacy": { "cursor-access-token": "old" },
                "newslot": { "cursor-access-token": "older" }
            }
        });
        let target =
            upsert_cursor_account_slot(&mut accounts, "newslot", "legacy", "acc", "ref", "prof");
        assert_eq!(target, "newslot");
        assert_eq!(accounts["accounts"]["newslot"]["cursor-access-token"], "acc");
        assert_eq!(accounts["accounts"]["legacy"]["cursor-access-token"], "old");
    }

    #[test]
    fn windows_v10_roundtrip() {
        let key = [7u8; 32];
        let encrypted = encrypt_windows_gcm_v10(&key, b"hello grok").unwrap();
        assert!(encrypted.starts_with(b"v10"));
        let plain = decrypt_windows_gcm_v10(&key, &encrypted).unwrap();
        assert_eq!(plain, b"hello grok");
    }

    #[test]
    fn grok_bot_name_does_not_match_cli() {
        assert!(is_grok_bot_process_name("Grok Bot"));
        assert!(is_grok_bot_process_name("Grok Bot.exe"));
        assert!(!is_grok_bot_process_name("grok"));
        assert!(!is_grok_bot_process_name("Grok.exe"));
    }
}
