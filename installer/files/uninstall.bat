@echo off
title Aero 1.2.0 Uninstaller

set "DIR=%~dp0"

echo.
echo ============================================
echo   Aero 1.2.0 Uninstaller
echo ============================================
echo.
echo Removing Aero from PATH...
echo   %DIR%

powershell -Command ^
$p=[Environment]::GetEnvironmentVariable('Path','User'); ^
$n=($p -split ';' ^| Where-Object {$_ -ne '' -and $_ -ne '%DIR%'}) -join ';'; ^
[Environment]::SetEnvironmentVariable('Path',$n,'User')

if %errorlevel% equ 0 (
    echo [SUCCESS] Aero removed from PATH.
) else (
    echo [FAILED] Please remove manually from User PATH.
)

echo.
echo You can now delete this folder.
echo.
pause
