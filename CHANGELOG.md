# Changelog

All notable changes to Aero are documented in this file.

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
- Docker image `aero-lang/aero:1.2.0` for quick-start environments.
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
