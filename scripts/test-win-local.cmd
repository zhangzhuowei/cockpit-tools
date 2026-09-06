@echo off
setlocal

call "G:\software\Microsoft Visual Studio18Professional\18\Professional\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 (
  echo [test-win-local] vcvars64.bat failed
  exit /b 1
)

set "PATH=C:\Users\Administrator\.cargo\bin;G:\software\go\bin;%PATH%"
set "COCKPIT_SKIP_CLIPROXY_BUILD=1"
cd /d "%~dp0.."

REM The cockpit-tools lib test binary needs the Tauri/WebView2 runtime and fails to
REM load here (STATUS_ENTRYPOINT_NOT_FOUND); core logic tests run in cockpit-core.
cargo test -p cockpit-core --lib %*
exit /b %errorlevel%
