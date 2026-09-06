@echo off
setlocal

call "G:\software\Microsoft Visual Studio18Professional\18\Professional\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 (
  echo [check-win-local] vcvars64.bat failed
  exit /b 1
)

set "PATH=C:\Users\Administrator\.cargo\bin;G:\software\go\bin;%PATH%"
set "COCKPIT_SKIP_CLIPROXY_BUILD=1"
cd /d "%~dp0.."

echo [check-win-local] cargo check cockpit-core
cargo check -p cockpit-core
if errorlevel 1 exit /b 1

echo [check-win-local] cargo check cockpit-tools
cargo check -p cockpit-tools
exit /b %errorlevel%
