============================================
  Aero Programming Language v1.2.0
  Windows 64-bit (Portable)
============================================

Aero is a systems programming language that combines the
performance of C with the safety of Rust, featuring:
AOT compilation, generics, arena memory management,
tensor operations, matmul, FFI, and more.

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

  3. Open a new Command Prompt window

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
  Install aero-lang-1.2.0.vsix for syntax highlighting.
  Drag the .vsix file into VS Code Extensions panel.

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


============================================
  Aero 1.2.0 - Windows 64-bit
============================================
