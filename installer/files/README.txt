============================================
  Aero Programming Language v1.2.1
  Windows 64-bit (Portable)
============================================

Aero is a systems programming language that combines the
performance of C with the safety of Rust, featuring:
AOT compilation, generics, arena memory management,
tensor operations, matmul, FFI, and more.

NEW IN 1.2.1
----------------------------------------
  - Editor diagnostics work. The LSP publishDiagnostics
    payload is valid JSON now, so VS Code underlines compiler
    errors and warnings while you type.
  - aero --version and aero --help print their output again.
  - Paths containing spaces are accepted by aero run,
    aero build and aero check.
  - Two crashes fixed: dynamic indexing into an arena, and
    returning a struct or tuple from the last expression of
    a function.
  - Over-allocating an arena reports an arena out-of-bounds
    error instead of the misleading "overflowed its stack".
  - Clearer diagnostics for builtin name collisions, unknown
    types, self receivers, and HashMap keys other than i64.

NEW IN 1.2.0
----------------------------------------
  - aero install: one-command ecosystem packages
    (51 packages: web, database, serialization, ...)
  - aero check: compile-only validation
  - Shared library builds (#[export], aero build --shared)
  - C++ bindings (aero build --cpp) and Python extensions

GitHub: https://github.com/SereinCin/aero-lang
Ecosystem: https://github.com/SereinCin/Aero-packages


SYSTEM REQUIREMENTS
----------------------------------------
  - Windows 10 or Windows 11 (64-bit)
  - No dependencies required (static build)


INSTALLATION
----------------------------------------

  1. Extract this ZIP to any folder (e.g. C:\Aero)

  2. Double-click install.bat (no admin required)
     - It adds the Aero folder to your USER PATH.
     - It is safe: only your user PATH is changed, and it
       will not truncate an existing long PATH.

  3. Close the installer window, then open a NEW terminal:
     - Command Prompt / PowerShell: open a brand-new window
     - VS Code: fully restart it (File > Exit, then reopen).
       Opening a new terminal inside an already-running VS
       Code is NOT enough - VS Code only sees environment
       variables that existed when it started.

  4. Type: aero --help


USAGE
----------------------------------------

  Run a .aero file:
     aero run file.aero

  Compile to standalone exe:
     aero build file.aero

  Compile-check only:
     aero check file.aero

  Install ecosystem packages:
     aero install aero-web

  Run benchmarks / tests / lint:
     aero bench file.aero
     aero test file.aero
     aero clippy file.aero

  Format code:
     aero fmt file.aero

  Create new project:
     aero new project-name

  List ecosystem packages:
     aero install


VS CODE EXTENSION (Optional)
----------------------------------------
  Install aero-lang-1.2.1.vsix for syntax highlighting and
  compiler diagnostics. Drag the .vsix file into the VS Code
  Extensions panel.

  After installing, set "aero.lsp.executablePath" to the full
  path of aero.exe to enable diagnostics, hover and
  go-to-definition.

  Note: The extension is offline-only, not on the Marketplace.


UPDATE
----------------------------------------
  1. Download the latest ZIP from GitHub Releases
  2. Extract and overwrite the old files
  3. Run install.bat again


UNINSTALL
----------------------------------------
  1. Double-click uninstall.bat
  2. Delete the Aero folder


FILES
----------------------------------------
  aero.exe        Compiler executable (59 MB)
  install.bat     Installation script
  uninstall.bat   Uninstallation script
  update.bat      Update guide
  README.txt      This file
  bin\            Helper scripts (aero_cmd.bat, aero_env.bat)
  plugins\        Bundled standard plugins


TROUBLESHOOTING
----------------------------------------

  Q: I ran install.bat but "aero" is still not recognized
     in the VS Code terminal.
  A: Fully restart VS Code (File > Exit, then reopen it).
     A new terminal inside an already-running VS Code does
     NOT pick up the new PATH - only a restarted VS Code does.

  Q: I ran install.bat but "aero" is not recognized in a
     new Command Prompt window.
  A: Make sure you opened a brand-new window AFTER running
     install.bat (an old window still has the old PATH).
     Re-run install.bat and confirm it prints [SUCCESS].
     You can also verify the PATH contains the Aero folder:
        echo %PATH%

  Q: How do I install Aero without install.bat?
  A: Manually add the Aero folder to your User PATH:
     System Properties > Advanced > Environment Variables >
     User variables > Path > Edit > New > paste the Aero
     folder path > OK > OK.

  Q: Diagnostics do not show up in VS Code.
  A: Version 1.2.1 is the first release where the language
     server sends valid diagnostics. Make sure the extension
     is aero-lang-1.2.1.vsix or newer, and that
     "aero.lsp.executablePath" points at this aero.exe.


============================================
  Aero 1.2.1 - Windows 64-bit
============================================
