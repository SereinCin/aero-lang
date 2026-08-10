@echo off
rem Aero: run the demo program (examples\hello.aero) via LLVM JIT.
rem Double-click this anytime to see Aero in action.
setlocal
set "PATH=D:\Scripts\LLVM\clang+llvm-22.1.8-x86_64-pc-windows-msvc\bin;%PATH%"
cd /d "%~dp0"
if not exist target\debug\aero.exe (
    echo aero.exe not found. Run build-aero.bat first.
    pause
    exit /b 1
)
target\debug\aero.exe run examples\hello.aero
echo.
echo Exit code: %errorlevel%
pause
