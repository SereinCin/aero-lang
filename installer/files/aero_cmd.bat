@echo off
REM Aero Command Prompt
REM Opens a dedicated cmd window with Aero tools in PATH.
REM Start menu shortcut points here.

set "AERO_HOME=%~dp0"
set "PATH=%AERO_HOME%;%PATH%"

echo.
echo  Aero 1.0.0 - Aero Programming Language
echo  Type "aero --help" for usage.
echo.

cd /d "%USERPROFILE%"

cmd /k "set AERO_HOME=%AERO_HOME% && set PATH=%PATH%"