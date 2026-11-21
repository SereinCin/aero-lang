@echo off
title Aero 1.2.6 Uninstaller

set "DIR=%~dp0"
REM install.bat stores the path without a trailing backslash, so strip it here
REM too; comparing "H:\Aero\" against "H:\Aero" never matches.
set "DIR=%DIR:~0,-1%"

echo.
echo ============================================
echo   Aero 1.2.6 Uninstaller
echo ============================================
echo.
echo Removing Aero from PATH...
echo   %DIR%

powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$dir = '%DIR%'.TrimEnd('\');" ^
  "$p = [Environment]::GetEnvironmentVariable('Path','User');" ^
  "$items = if ([string]::IsNullOrEmpty($p)) { @() } else { @($p -split ';') };" ^
  "$keep = @($items | Where-Object { $_ -ne '' -and $_.TrimEnd('\') -ne $dir });" ^
  "$removed = $items.Count - $keep.Count;" ^
  "if ($removed -gt 0) {" ^
  "  [Environment]::SetEnvironmentVariable('Path', ($keep -join ';'), 'User');" ^
  "  $w = if ($removed -eq 1) { 'entry' } else { 'entries' };" ^
  "  Write-Host ('[SUCCESS] Removed ' + $removed + ' PATH ' + $w + '.'); exit 0" ^
  "} else {" ^
  "  Write-Host '[INFO] Aero was not on the User PATH; nothing to remove.'; exit 2" ^
  "}"

if errorlevel 2 (
    echo.
    echo Checked the User PATH only. If Aero was added to the System PATH
    echo manually, remove %DIR% from there as well.
) else if errorlevel 1 (
    echo.
    echo [FAILED] Could not update PATH. Remove %DIR% manually.
)

echo.
echo You can now delete this folder.
echo.
pause
