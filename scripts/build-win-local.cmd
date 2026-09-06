@echo off
setlocal

call "G:\software\Microsoft Visual Studio18Professional\18\Professional\VC\Auxiliary\Build\vcvars64.bat"
if errorlevel 1 (
  echo [build-win-local] vcvars64.bat failed
  exit /b 1
)

set "PATH=C:\Users\Administrator\.cargo\bin;G:\software\go\bin;%PATH%"
cd /d "%~dp0.."

echo [build-win-local] cl:
where cl
echo [build-win-local] link:
where link
echo [build-win-local] cargo:
cargo --version
echo [build-win-local] go:
go version

npm run tauri build
exit /b %errorlevel%
