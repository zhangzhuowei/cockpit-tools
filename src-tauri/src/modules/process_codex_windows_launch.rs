// Windows Codex 受管实例：注册包解析、包身份启动及实例确认。
/// 按 `CreateProcess` 的解析规则引用单个 Windows 命令行参数。
///
/// Electron 的启动参数常含空格与引号（`--user-data-dir=C:\Users\some user\...`），
/// 拼进 `ProcessStartInfo.Arguments` 时必须按同一套规则转义，否则会被拆成多个参数。
#[cfg(any(test, target_os = "windows"))]
fn quote_windows_command_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return argument.to_string();
    }
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut pending_backslashes = 0usize;
    for ch in argument.chars() {
        match ch {
            '\\' => pending_backslashes += 1,
            '"' => {
                // 引号前的反斜杠要加倍，引号自身再转义一个。
                for _ in 0..(pending_backslashes * 2 + 1) {
                    quoted.push('\\');
                }
                quoted.push('"');
                pending_backslashes = 0;
            }
            _ => {
                for _ in 0..pending_backslashes {
                    quoted.push('\\');
                }
                pending_backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    // 结尾反斜杠必须加倍，否则会把收尾引号转义掉。
    for _ in 0..(pending_backslashes * 2) {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

/// 从商店包启动路径反推包的 `InstallLocation`。
///
/// 启动路径形如 `<InstallLocation>\app\ChatGPT.exe`，因此去掉两级即安装根目录；
/// 非商店路径返回 `None`。
///
/// 这里按文本解析而不是 `Path::parent()`：该函数只处理 Windows 路径，而
/// `Path::parent()` 在非 Windows 主机（单元测试）上不会把 `\` 当分隔符。
#[cfg(any(test, target_os = "windows"))]
fn windowsapps_install_location_from_launch_path(launch_path: &Path) -> Option<String> {
    if !is_windowsapps_launch_path(launch_path) {
        return None;
    }
    let normalized = launch_path.to_string_lossy().replace('/', "\\");
    let trimmed = normalized.trim_end_matches('\\');
    let install_location = trimmed.rsplitn(3, '\\').nth(2)?;
    let text = install_location.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(text)
}

/// 把脚本编码成 `powershell.exe -EncodedCommand` 需要的 UTF-16LE + Base64。
///
/// 内层脚本里既有中文错误文案又有引号与反斜杠，直接用 `-Command` 传会被外层
/// 解析一次、内层再解析一次，`-EncodedCommand` 可以完全绕开这层转义问题。
#[cfg(any(test, target_os = "windows"))]
fn encode_powershell_encoded_command(script: &str) -> String {
    use base64::{engine::general_purpose, Engine as _};
    let mut bytes = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    general_purpose::STANDARD.encode(bytes)
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, serde::Deserialize)]
struct CodexRegisteredLaunch {
    family_name: String,
    app_id: String,
    executable: String,
}

/// Include the publisher ID when resolving an updated package, not just its name.
#[cfg(any(test, target_os = "windows"))]
fn codex_store_package_family_from_path(path: &Path) -> Option<String> {
    let normalized = normalized_windows_path_text(path);
    let directory = normalized
        .split(r"\windowsapps\")
        .nth(1)?
        .split('\\')
        .next()?;
    let (name_version, publisher) = directory.rsplit_once("__")?;
    let name = name_version.split('_').next()?;
    if name.is_empty() || publisher.is_empty() {
        return None;
    }
    Some(format!("{name}_{publisher}"))
}

/// WindowsApps is only a routing hint. Activation itself requires a registered
/// package AND a manifest Application whose executable matches the selected GUI.
#[cfg(any(test, target_os = "windows"))]
fn build_codex_registered_launch_probe(launch_path: &Path) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
function Normalize-LaunchPath([string]$path) {{
  $path = $path.Replace('/', '\')
  if ($path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {{ $path = '\\' + $path.Substring(8) }}
  elseif ($path.StartsWith('\\?\')) {{ $path = $path.Substring(4) }}
  return [IO.Path]::GetFullPath($path).TrimEnd('\')
}}
$target = Normalize-LaunchPath '{exe}'
$storeFamily = '{store_family}'
$packages = @(Get-AppxPackage -ErrorAction Stop | Where-Object {{ $_.InstallLocation }} | Sort-Object Version -Descending)
$matches = @()
foreach ($pkg in $packages) {{
  $root = Normalize-LaunchPath $pkg.InstallLocation
  $exactRoot = $target.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)
  if (-not $exactRoot -and (-not $storeFamily -or $pkg.PackageFamilyName -ine $storeFamily)) {{ continue }}
  $manifest = Get-AppxPackageManifest -Package $pkg -ErrorAction Stop
  foreach ($application in $manifest.Package.Applications.Application) {{
    $relative = [string]$application.Executable
    if (-not $relative -or [IO.Path]::IsPathRooted($relative)) {{ continue }}
    $candidate = [IO.Path]::GetFullPath((Join-Path $root $relative))
    if (-not $candidate.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) {{ continue }}
    $isTarget = $candidate -ieq $target
    $isUpdatedGui = $storeFamily -and $pkg.PackageFamilyName -ieq $storeFamily -and [IO.Path]::GetFileName($candidate) -ieq 'ChatGPT.exe'
    if (($isTarget -or $isUpdatedGui) -and (Test-Path -LiteralPath $candidate -PathType Leaf) -and $application.Id) {{
      $matches += [PSCustomObject]@{{ family_name = [string]$pkg.PackageFamilyName; app_id = [string]$application.Id; executable = $candidate }}
    }}
  }}
  if ($matches.Count -gt 0) {{ break }}
}}
if ($matches.Count -gt 1) {{ throw 'Ambiguous registered Codex GUI applications' }}
if ($matches.Count -eq 1) {{ $matches[0] | ConvertTo-Json -Compress }} else {{ Write-Output 'null' }}"#,
        exe = escape_powershell_single_quoted(&launch_path.to_string_lossy()),
        store_family = escape_powershell_single_quoted(
            &codex_store_package_family_from_path(launch_path).unwrap_or_default()
        ),
    )
}

#[cfg(target_os = "windows")]
fn codex_launch_powershell_output(script: &str) -> Result<String, String> {
    // This helper drains pipes concurrently and bounds both process and pipe waits.
    // Do not log the script: it can contain per-account proxy credentials.
    for executable in windows_powershell_executable_candidates_for_host() {
        let mut command = build_powershell_command(&executable, &["-Command", script]);
        match crate::modules::process_timeout::output_with_timeout(
            &mut command,
            Duration::from_secs(15),
        ) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("PowerShell launch/probe failed: {error}")),
            Ok(output) => {
                if !output.status.success() {
                    return Err(format!(
                        "PowerShell launch/probe failed: status={}, stderr={}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                            .trim()
                            .chars()
                            .take(600)
                            .collect::<String>()
                    ));
                }
                return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }
    }
    Err("PowerShell executable not found".to_string())
}

#[cfg(any(test, target_os = "windows"))]
fn parse_codex_registered_launch(output: &str) -> Result<Option<CodexRegisteredLaunch>, String> {
    let registered: Option<CodexRegisteredLaunch> =
        serde_json::from_str(output.trim_start_matches('\u{feff}'))
            .map_err(|error| format!("Invalid package registration response: {error}"))?;
    if let Some(package) = &registered {
        if package.family_name.trim().is_empty()
            || package.app_id.trim().is_empty()
            || !is_chatgpt_store_gui_exe(Path::new(&package.executable))
        {
            return Err("Invalid registered Codex GUI application".to_string());
        }
    }
    Ok(registered)
}

/// Resolve the selected StartApps entry to its registered GUI executable before
/// activation. A shell alias cannot carry a per-launch environment block.
#[cfg(any(test, target_os = "windows"))]
fn build_codex_default_registered_launch_probe(app_user_model_id: &str) -> Result<String, String> {
    let (family, app_id) = app_user_model_id
        .trim()
        .split_once('!')
        .filter(|(family, app_id)| {
            !family.is_empty() && !app_id.is_empty() && !app_id.contains('!')
        })
        .ok_or_else(|| "Invalid Codex AppUserModelId".to_string())?;
    Ok(format!(
        r#"$ErrorActionPreference = 'Stop'
$family = '{family}'
$appId = '{app_id}'
$pkg = Get-AppxPackage -ErrorAction Stop | Where-Object {{ $_.PackageFamilyName -ieq $family }} | Sort-Object Version -Descending | Select-Object -First 1
if (-not $pkg) {{ throw 'Codex package is not registered' }}
$root = [IO.Path]::GetFullPath($pkg.InstallLocation).TrimEnd('\')
$matches = @()
foreach ($application in (Get-AppxPackageManifest -Package $pkg -ErrorAction Stop).Package.Applications.Application) {{
  if ($application.Id -ne $appId) {{ continue }}
  $relative = [string]$application.Executable
  if (-not $relative -or [IO.Path]::IsPathRooted($relative)) {{ continue }}
  $exe = [IO.Path]::GetFullPath((Join-Path $root $relative))
  if ($exe.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase) -and [IO.Path]::GetFileName($exe) -ieq 'ChatGPT.exe' -and (Test-Path -LiteralPath $exe -PathType Leaf)) {{
    $matches += [PSCustomObject]@{{ family_name = [string]$pkg.PackageFamilyName; app_id = [string]$application.Id; executable = $exe }}
  }}
}}
if ($matches.Count -ne 1) {{ throw 'No unique registered Codex GUI application' }}
$matches[0] | ConvertTo-Json -Compress"#,
        family = escape_powershell_single_quoted(family),
        app_id = escape_powershell_single_quoted(app_id),
    ))
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, PartialEq)]
enum CodexManagedLaunchRoute {
    PackageIdentity,
    DirectExe,
}

#[cfg(any(test, target_os = "windows"))]
fn codex_managed_launch_route(
    configured: &Path,
    registered: Option<&CodexRegisteredLaunch>,
) -> Result<CodexManagedLaunchRoute, String> {
    if registered.is_some() {
        Ok(CodexManagedLaunchRoute::PackageIdentity)
    } else if is_windowsapps_launch_path(configured) {
        Err("No matching registered Codex GUI package".to_string())
    } else {
        Ok(CodexManagedLaunchRoute::DirectExe)
    }
}

/// Use the selected profile exactly once, even if saved extra arguments contain
/// an old --user-data-dir. Both direct and packaged launches share this builder.
#[cfg(any(test, target_os = "windows"))]
fn build_codex_managed_windows_args(
    extra_args: &[String],
    app_user_data_dir: &Path,
) -> Vec<String> {
    let mut args = Vec::new();
    let mut input = build_codex_app_launch_args(extra_args).into_iter();
    while let Some(arg) = input.next() {
        if arg == "--user-data-dir" {
            let _ = input.next();
        } else if !arg.starts_with("--user-data-dir=") {
            args.push(arg);
        }
    }
    args.push(format!(
        "--user-data-dir={}",
        app_user_data_dir.to_string_lossy()
    ));
    args
}

#[cfg(any(test, target_os = "windows"))]
fn build_codex_package_identity_script(
    package: &CodexRegisteredLaunch,
    codex_home: &str,
    app_user_data_dir: &Path,
    extra_args: &[String],
    env: &[(String, String)],
) -> String {
    build_codex_package_launch_script(
        package,
        Some(codex_home),
        Some(app_user_data_dir),
        extra_args,
        env,
    )
}

/// Default and managed instances use the same package-identity launcher. Only
/// managed instances set profile paths; defaults must discard inherited paths.
#[cfg(any(test, target_os = "windows"))]
fn build_codex_package_launch_script(
    package: &CodexRegisteredLaunch,
    codex_home: Option<&str>,
    app_user_data_dir: Option<&Path>,
    extra_args: &[String],
    env: &[(String, String)],
) -> String {
    let mut env_lines = env
        .iter()
        .map(|(key, value)| {
            format!(
                "[Environment]::SetEnvironmentVariable('{}', '{}', 'Process')",
                escape_powershell_single_quoted(key),
                escape_powershell_single_quoted(value)
            )
        })
        .collect::<Vec<_>>();
    // Profile isolation must not be overridable by extra_env.
    for (key, value) in [
        ("CODEX_HOME", codex_home.map(str::to_string)),
        (
            "CODEX_ELECTRON_USER_DATA_PATH",
            app_user_data_dir.map(|path| path.to_string_lossy().into_owned()),
        ),
    ] {
        env_lines.push(match value {
            Some(value) => format!("$env:{key} = '{}'", escape_powershell_single_quoted(&value)),
            None => format!("[Environment]::SetEnvironmentVariable('{key}', $null, 'Process')"),
        });
    }
    let args = match app_user_data_dir {
        Some(path) => build_codex_managed_windows_args(extra_args, path),
        None => build_codex_default_launch_args(extra_args),
    };
    let arguments = args
        .iter()
        .map(|arg| quote_windows_command_argument(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let inner = format!(
        r#"$ErrorActionPreference = 'Stop'
{env}
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = '{exe}'
$psi.UseShellExecute = $false
$psi.Arguments = '{arguments}'
[void][System.Diagnostics.Process]::Start($psi)"#,
        env = env_lines.join("\n"),
        exe = escape_powershell_single_quoted(&package.executable),
        arguments = escape_powershell_single_quoted(&arguments),
    );
    format!(
        r#"$ErrorActionPreference = 'Stop'
$family = '{family}'
$appId = '{app_id}'
$exe = '{exe}'
$pkg = Get-AppxPackage -ErrorAction Stop | Where-Object {{ $_.PackageFamilyName -ieq $family }} | Sort-Object Version -Descending | Select-Object -First 1
if (-not $pkg) {{ throw 'Codex package registration changed; retry launch' }}
$root = [IO.Path]::GetFullPath($pkg.InstallLocation).TrimEnd('\')
$app = @((Get-AppxPackageManifest -Package $pkg -ErrorAction Stop).Package.Applications.Application | Where-Object {{ $_.Id -eq $appId -and $_.Executable -and ([IO.Path]::GetFullPath((Join-Path $root $_.Executable)) -ieq $exe) }})
if ($app.Count -ne 1) {{ throw 'Codex package executable changed; retry launch' }}
# Child processes otherwise break away from the package identity by default.
Invoke-CommandInDesktopPackage -PackageFamilyName $family -AppId $appId -PreventBreakaway -Command "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Args '-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -EncodedCommand {encoded}'"#,
        family = escape_powershell_single_quoted(&package.family_name),
        app_id = escape_powershell_single_quoted(&package.app_id),
        exe = escape_powershell_single_quoted(&package.executable),
        encoded = encode_powershell_encoded_command(&inner),
    )
}

#[cfg(target_os = "windows")]
fn launch_codex_via_package_identity(
    package: &CodexRegisteredLaunch,
    codex_home: &str,
    app_user_data_dir: &Path,
    extra_args: &[String],
    extra_env: &[(String, String)],
) -> Result<(), String> {
    let mut env: Vec<(String, String)> = managed_proxy_env_pairs()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    env.extend_from_slice(extra_env);
    let script = build_codex_package_identity_script(
        package,
        codex_home,
        app_user_data_dir,
        extra_args,
        &env,
    );
    codex_launch_powershell_output(&script).map(|_| ())
}

#[cfg(target_os = "windows")]
fn verify_codex_process_package(pid: u32, expected_family: &str) -> Result<(), String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|error| format!("OpenProcess pid={pid}: {error}"))?;
        let result = (|| {
            let mut size = 0;
            let status = GetPackageFamilyName(handle, &mut size, PWSTR::null());
            if status != ERROR_INSUFFICIENT_BUFFER || size == 0 || size > 32768 {
                return Err(format!(
                    "GetPackageFamilyName pid={pid}: win32={}",
                    status.0
                ));
            }
            let mut buffer = vec![0u16; size as usize];
            let status = GetPackageFamilyName(handle, &mut size, PWSTR(buffer.as_mut_ptr()));
            if status != ERROR_SUCCESS {
                return Err(format!(
                    "GetPackageFamilyName pid={pid}: win32={}",
                    status.0
                ));
            }
            let actual = String::from_utf16_lossy(
                &buffer[..buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len())],
            );
            if !actual.eq_ignore_ascii_case(expected_family) {
                return Err(format!(
                    "Package mismatch pid={pid}: expected={expected_family}, actual={actual}"
                ));
            }
            let mut exit_code = 0;
            GetExitCodeProcess(handle, &mut exit_code).map_err(|error| error.to_string())?;
            if exit_code != 259 {
                return Err(format!("Codex exited pid={pid}: exit_code={exit_code}"));
            }
            Ok(())
        })();
        let _ = CloseHandle(handle);
        result
    }
}

#[cfg(any(test, target_os = "windows"))]
fn codex_managed_launch_candidate(
    entries: &[(u32, Option<String>)],
    target: &str,
    before: &HashSet<u32>,
) -> Option<u32> {
    // Never accept missing/default user-data directories or a pre-existing PID.
    collect_matching_pids_by_user_data_dir(entries, target, false)
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .min()
}

#[cfg(target_os = "windows")]
fn launch_windows_codex_managed_instance(
    configured: &Path,
    codex_home: &str,
    app_user_data_dir: &Path,
    extra_args: &[String],
    extra_env: &[(String, String)],
) -> Result<u32, String> {
    let fail = |path: &Path, stage: &str, error: &str| {
        codex_managed_store_launch_unsafe_error(
            "not attempted for registered Store packages",
            error,
            &format!(
                "launch_path={}; launch_stage={stage}; codex_home={codex_home}",
                path.display()
            ),
        )
    };
    // Query registration even for packages installed outside WindowsApps. Failure
    // to query is not evidence of an unpackaged executable: fail closed.
    if configured
        .to_string_lossy()
        .to_ascii_lowercase()
        .starts_with("shell:")
    {
        return Err(fail(
            configured,
            "registration",
            "Re-detect the executable path; shell aliases cannot isolate managed profiles",
        ));
    }
    let probe = codex_launch_powershell_output(&build_codex_registered_launch_probe(configured))
        .map_err(|error| fail(configured, "registration", &error))?;
    let package = parse_codex_registered_launch(&probe)
        .map_err(|error| fail(configured, "registration", &error))?;
    let route = codex_managed_launch_route(configured, package.as_ref())
        .map_err(|error| fail(configured, "registration", &error))?;
    let launch_path = package
        .as_ref()
        .map(|p| PathBuf::from(&p.executable))
        .unwrap_or_else(|| configured.to_path_buf());
    if normalized_windows_path_text(&launch_path) != normalized_windows_path_text(configured) {
        update_app_path_in_config("codex", &launch_path, &configured.to_string_lossy());
    }
    let expected = launch_path.to_string_lossy();
    let before: HashSet<u32> = collect_codex_process_entries_from_sysinfo_fallback(&expected)
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    let mut child = if route == CodexManagedLaunchRoute::PackageIdentity {
        let package = package.as_ref().expect("registered package route");
        crate::modules::logger::log_info(&format!(
            "[Codex Start] strategy=package-identity family={} app_id={} launch_path={}",
            package.family_name,
            package.app_id,
            launch_path.display()
        ));
        launch_codex_via_package_identity(
            package,
            codex_home,
            app_user_data_dir,
            extra_args,
            extra_env,
        )
        .map_err(|error| fail(&launch_path, "activation", &error))?;
        None
    } else {
        Some(
            spawn_command_with_trace(&mut build_windows_codex_instance_command(
                &launch_path,
                codex_home,
                app_user_data_dir,
                extra_args,
                extra_env,
            ))
            .map_err(|error| format!("Codex launch failed: {error}"))?,
        )
    };
    let target = normalize_path_for_compare(&app_user_data_dir.to_string_lossy());
    let started = Instant::now();
    let mut stable_pid = None;
    let mut stable_since = Instant::now();
    let mut last_error = "No matching managed Codex process".to_string();
    while started.elapsed() < Duration::from_secs(15) {
        if let Some(child) = child.as_mut() {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                return Err(format!(
                    "Codex exited before startup confirmation: {status}"
                ));
            }
        }
        let entries = collect_codex_process_entries_from_sysinfo_fallback(&expected);
        let candidate = codex_managed_launch_candidate(&entries, &target, &before);
        let candidate = candidate.filter(|pid| is_pid_running(*pid));
        if candidate != stable_pid {
            stable_pid = candidate;
            stable_since = Instant::now();
        }
        if let Some(pid) = candidate {
            let identity = package
                .as_ref()
                .map(|p| verify_codex_process_package(pid, &p.family_name))
                .unwrap_or(Ok(()));
            match identity {
                Ok(()) if stable_since.elapsed() >= Duration::from_millis(750) => {
                    crate::modules::logger::log_info(&format!("[Codex Start] managed instance confirmed pid={pid} package={} elapsed_ms={}", package.as_ref().map(|p| p.family_name.as_str()).unwrap_or("unpackaged"), started.elapsed().as_millis()));
                    return Ok(pid);
                }
                Err(error) => {
                    last_error = error;
                    stable_since = Instant::now();
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    crate::modules::logger::log_warn(&format!(
        "[Codex Start] confirmation failed launch_path={} error={last_error}",
        launch_path.display()
    ));
    if package.is_some() {
        Err(fail(&launch_path, "confirmation", &last_error))
    } else {
        Err(format!("Codex startup confirmation failed: {last_error}"))
    }
}

#[cfg(test)]
#[path = "process_codex_windows_launch_tests.rs"]
mod codex_windows_managed_launch_tests;
