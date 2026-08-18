@echo off
REM aero_env.bat — Add Aero to the current CMD session's PATH.
REM Usage: call aero_env.bat
REM
REM This script is for users who want to manually set up the
REM environment without running the full Aero Command Prompt.
REM The installer also adds Aero to the system PATH automatically.

set "AERO_HOME=%~dp0"
set "PATH=%AERO_HOME%;%PATH%"

echo Aero 1.0.0 environment loaded.
echo   AERO_HOME=%AERO_HOME%