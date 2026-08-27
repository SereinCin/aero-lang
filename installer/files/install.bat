@echo off
title Aero 1.2.0 Installer

set "DIR=%~dp0"
REM Remove the trailing backslash so the PATH entry is clean (e.g. C:\Aero)
set "DIR=%DIR:~0,-1%"

echo.
echo ============================================
echo   Aero 1.2.0 Installer
echo ============================================
echo.
echo Install folder: %DIR%

if not exist "%DIR%\aero.exe" (
    echo.
    echo [ERROR] aero.exe not found in this folder.
    echo Please make sure install.bat is in the same folder as aero.exe.
    echo.
    pause
    exit /b 1
)

echo.
echo Adding Aero to User PATH...

REM Use PowerShell to edit ONLY the User PATH.
REM (setx truncates PATH at 1024 chars and copies the system PATH into the
REM  user PATH, which can break other programs. PowerShell avoids both.)
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$dir = '%DIR%';" ^
  "$p = [Environment]::GetEnvironmentVariable('Path','User');" ^
  "if ($p -split ';' -contains $dir) { Write-Host '[OK] Aero is already in PATH.' }" ^
  "else { $add = if ([string]::IsNullOrEmpty($p)) { $dir } else { $dir + ';' + $p }; [Environment]::SetEnvironmentVariable('Path', $add, 'User'); Write-Host '[SUCCESS] Aero added to PATH.' }"

if errorlevel 1 (
    echo.
    echo [FAILED] Please add manually:
    echo   1. Open System Properties - Advanced - Environment Variables
    echo   2. Find "Path" in User variables, click Edit
    echo   3. Add: %DIR%
    echo   4. Click OK
    echo.
)

echo.
echo ============================================
echo   Installation complete!
echo   Aero 1.2.0
echo.
echo   IMPORTANT: The PATH change only affects NEW terminals.
echo   Close this window, then either:
echo     - open a brand-new Command Prompt / PowerShell, or
echo     - fully restart VS Code (File - Exit, then reopen).
echo       Opening a new terminal inside a running VS Code is
echo       NOT enough.
echo.
echo   Then type:
echo     aero --help
echo   Install ecosystem packages:
echo     aero install aero-web
echo ============================================
echo.
pause
