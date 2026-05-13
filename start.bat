@echo off
setlocal
cd /d "%~dp0"

REM 1. Check cargo (use >nul 2>&1 to silence and check errorlevel)
cargo --version >nul 2>&1
if not %ERRORLEVEL%==0 (
  echo [x] Rust ^(cargo^) not found.
  echo     Install from: https://rustup.rs
  echo     After install, open a NEW terminal and re-run this script.
  goto :pause_exit
)
for /f "tokens=2" %%v in ('cargo --version') do set "CARGO_VER=%%v"
echo [i] Rust detected: cargo %CARGO_VER%

REM Need cargo >= 1.85 (for indexmap 2.14+ edition2024 feature)
for /f "tokens=1,2 delims=." %%a in ("%CARGO_VER%") do (
  set "VMAJ=%%a"
  set "VMIN=%%b"
)
set /a "VOK=0"
if %VMAJ% gtr 1 set "VOK=1"
if %VMAJ%==1 if %VMIN% geq 85 set "VOK=1"
if %VOK%==0 (
  echo [x] Rust version too old. Need cargo 1.85 or newer, found %CARGO_VER%.
  echo     Run: rustup update stable
  echo     Or reinstall from https://rustup.rs
  goto :pause_exit
)

REM 2. Check config.toml
if not exist "config.toml" (
  if not exist "config.example.toml" (
    echo [x] config.example.toml missing. Repo incomplete?
    goto :pause_exit
  )
  copy /Y "config.example.toml" "config.toml" >nul
  echo [!] config.toml generated.
  echo     Please edit it to set private_key:
  echo       %CD%\config.toml
  goto :pause_exit
)

REM 3. Build
echo [i] Building release binary ^(first time may take 5-10 minutes^)...
cargo build --release
if not %ERRORLEVEL%==0 (
  echo [x] Build failed.
  goto :pause_exit
)

REM 4. Run
echo.
echo [i] Starting miner
echo.
"target\release\hash-miner.exe" %*

:pause_exit
echo.
echo ===== Press any key to close =====
pause >nul
endlocal
