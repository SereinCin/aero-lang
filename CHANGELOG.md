# Changelog

All notable changes to Aero are documented in this file.

## [0.1.1] - 2026-08-11

### Added
- File IO builtins (M1.2): `read_file(path) -> str` reads a whole file (empty
  string on failure); `write_file(path, contents) -> i64` writes and returns
  the byte count (-1 on failure). Built on the existing libc bridge.
- Command-line argument builtins (M1.2): `arg_count() -> i64` and
  `arg(i) -> str` (empty string when out of range). The AOT entry point now
  uses the standard C signature `main(argc, argv)`; JIT runs with no arguments.
- Integration tests for file IO and CLI arguments (standalone exe with args).

### Changed
- `main` signature: `main()` -> `main(argc, argv)`.
- Version bump 0.1.0 -> 0.1.1 (workspace, `aero-hir`, and the `aero new`
  package template).

## [0.1.0] - 2026-08-11

### Added
- Initial release: lexer, parser, HIR with type inference and borrow checking,
  LLVM codegen (JIT + AOT), package manager (`aero new/build/run/test`), FFI
  (`extern "C"` + `[link]`), and the string system.
- VS Code extension (`aero-lang`) with syntax highlighting and one-key
  run/build in the integrated terminal.
- Open-sourced on GitHub (`SereinCin/aero-lang`).
