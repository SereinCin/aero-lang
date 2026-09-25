# Changelog

All notable changes to Aero are documented in this file.

## [1.2.1] - 2026-09-26

### Fixed
- Arena indexing with a runtime index no longer crashes the process. The
  compiler emitted an unchecked load, so `a.get(i)` with a computed `i` died
  with an access violation instead of reporting anything.
- A function that declares a struct or tuple return type but never returns
  no longer crashes codegen; a zero-initialized aggregate is produced instead.
- Allocating past the end of an arena now reports an arena out-of-bounds error.
  It previously surfaced as "overflowed its stack", which pointed at the wrong
  subsystem.
- Paths containing spaces are accepted by `aero run`, `aero build` and
  `aero check`. The driver split its argument on whitespace before parsing it.
- `aero --version` and `aero --help` work again. Both flags were treated as
  input file names and failed with "no such file".
- LSP: `textDocument/publishDiagnostics` is now well-formed JSON. The message
  was closed incorrectly, so editors discarded every diagnostics push and never
  showed a compile error even though the server produced them.
- Diagnostic wording: a user function shadowing a builtin, an unknown type name,
  and a `self` used as a non-receiver now report the actual problem (D1-D3).
- `HashMap` with a key type other than `i64` reports a readable error naming the
  key type instead of an internal type mismatch.

### Changed
- Documentation aligned with the implementation: operator overloading requires
  the `RHS` and `Output` type arguments (`impl Add<Vec2, Vec2> for Vec2`), and
  `str` has no method syntax — string operations go through the free functions
  (`len(s)`, `substr(s, a, b)`, `str_contains(h, n)`, ...).
- Windows installer package bumped to 1.2.1: new `aero.exe`, version banners in
  `install.bat` / `uninstall.bat` / `update.bat`, and an updated `README.txt`.

### Notes
- `aero-v1.2.1-windows-x86_64.zip` is published with this release. Linux and
  macOS packages are not built yet; use the 1.2.0 installers until those targets
  are rebuilt.

## [1.2.0] - 2026-08-22

### Added
- `aero install` (GitHub ecosystem): fetches and installs Aero packages from the
  GitHub ecosystem repository with recursive dependency resolution, SHA256
  checksum verification, and idempotent installs.
- Version isolation: the package index URL is bound to the toolchain version so
  each Aero release pulls the matching ecosystem packages.
- `#[export]` and shared-library builds (`aero build --shared`).
- M0 milestone: ecosystem packages for the standard library (aero-std),
  networking (aero-tcp/aero-http/aero-web), data (aero-redis/aero-sqlite),
  and crypto (aero-crypto).

### Changed
- Package manager (`aero-pm`) now ships 51 ecosystem packages.
- Native builds and release artifacts for Linux x86_64, macOS x86_64/arm64,
  and Windows x86_64.
- `aero install` handles multi-package installs (e.g. aero-web then aero-sqlite)
  without missing dependencies.

### Fixed
- `aero install` correctly inserts dependencies into `Aero.toml` even when
  `[dependencies]` is the last table in the file.
- `pack.sh` generates valid JSON for the dependency tree (no parse failures).
- `aero-sqlite` ships self-contained `libsqlite3.a` with relative FFI paths so
  installs work on any machine without a C toolchain.
- Installer package naming `aero-<version>-windows-x86_64.zip` with SHA256
  checksums published on the release page.

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
