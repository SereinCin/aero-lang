@echo off
title Aero 1.2.0 Installer

set "DIR=%~dp0"

echo.
echo ============================================
echo   Aero 1.2.0 Installer
echo ============================================
echo.
echo Install folder: %DIR%

if not exist "%DIR%aero.exe" (
    echo.
    echo [ERROR] aero.exe not found in this folder.
    echo Please make sure install.bat is in the same folder as aero.exe.
    echo.
    pause
    exit /b 1
)

echo %PATH%|findstr /i "%DIR%" >nul 2>&1
if %errorlevel% equ 0 (
    echo.
    echo [OK] Aero is already in PATH.
    echo.
) else (
    echo.
    echo Adding Aero to PATH...
    setx PATH "%DIR%;%PATH%" >nul 2>&1
    if %errorlevel% equ 0 (
        echo [SUCCESS] Aero added to PATH.
    ) else (
        echo [FAILED] Please add manually:
        echo   1. Open System Properties - Advanced - Environment Variables
        echo   2. Find "Path" in User variables, click Edit
        echo   3. Add: %DIR%
        echo   4. Click OK
    )
    echo.
)

echo.
echo ============================================
echo   Installation complete!
echo   Aero 1.2.0
echo.
echo   Open a new cmd window and type:
echo     aero --help
echo   Install ecosystem packages:
echo     aero install aero-web
echo ============================================
echo.
pause
