# Aero Programming Language

Aero is an LLVM-based statically typed systems programming language that combines the high performance of C with memory safety inspired by Rust's design.

## Features

- **Ahead-of-Time (AOT) Compilation** — Compile directly to standalone executables
- **Generic System** — Generic functions, structs, enums, and traits with static dispatch
- **Region-based Memory Management** — Arena allocators for controlled allocation
- **Ownership Model** — Rust-style ownership, borrowing, and move semantics
- **Native Tensor Operations** — Built-in tensor types with element-wise ops, matrix multiplication, and BLAS Level-1 subprograms
- **C FFI** — Call C libraries directly via `extern "C"` declarations with `[link]` support
- **Lightweight Static Distribution** — Self-contained Windows executable, no runtime dependencies

## Quick Start (Windows)

1. Download the latest portable release from the [Releases page](https://github.com/SereinCin/aero-lang/releases)
2. Extract the archive and run `install.bat` to complete installation
3. Open a new terminal and use the `aero` command

```
aero run hello.aero
aero build hello.aero
```

## Documentation

Complete bilingual tutorials (Chinese and English) are maintained in a separate [documentation repository](https://github.com/SereinCin/aero-book).

## VS Code Extension

An offline syntax highlighting plugin is available as a `.vsix` file. Install it by dragging the file into VS Code. This plugin is not published on the VS Code Marketplace.

## Repository Structure

This repository contains the compiler source code, binary packages, and build scripts. All learning materials are hosted in the dedicated documentation repository.

## License

This project is licensed under the [MIT License](LICENSE).