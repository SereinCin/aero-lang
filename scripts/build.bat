@echo off
rem Aero: build all crates (lex/parse/ir/cli) with LLVM 22 bindings.
rem Double-click this after changing source code.
setlocal
set "LLVM_SYS_221_PREFIX=D:\Scripts\LLVM\clang+llvm-22.1.8-x86_64-pc-windows-msvc"
set "PATH=D:\Scripts\LLVM\clang+llvm-22.1.8-x86_64-pc-windows-msvc\bin;%PATH%"
cd /d "%~dp0"
cargo build
if errorlevel 1 (
    echo.
    echo BUILD FAILED - see errors above.
    pause
    exit /b 1
)
echo.
echo Build OK. Now you can run: run-aero.bat
pause
