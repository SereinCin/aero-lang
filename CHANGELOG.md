# Changelog

All notable changes to Aero are documented in this file.

## [1.2.4] - 2026-10-03

### Added — P1 bare-metal line
- **Bare-metal target support** (P1.1). `*-unknown-none` triples
  (`x86_64-unknown-none`, `aarch64-unknown-none`, `riscv64-unknown-none`)
  now produce freestanding ELF `.o` files that link with `ld.lld` without any
  libc. Verified on all three backends end-to-end.
- **`#[entry]` entry point** (P1.2). Marks the kernel/OS entry function so
  freestanding mode has a well-defined start symbol instead of looking for
  `main`. Attribute chain `HirFn → emit_object → codegen` fully propagates.
- **`#[no_mangle]`** consumed in codegen — emits the function with its plain
  name so the linker / bootloader can find it.
- **`#[link_arg]` plumbed through** — custom linker arguments pass from CLI
  (`--link-arg`) all the way to `ld.lld`.
- **`volatile`, `atomic`, `fence` builtins** (P1.3). `volatile_load` /
  `volatile_store` generate LLVM volatile loads and stores; `atomic_*` emit
  `atomicrmw` / `cmpxchg`; `fence` / `fence_acq` / `fence_rel` emit the
  corresponding LLVM fences.
- **`asm!` inline assembly** (P1.4). Parser → HIR → lower → typecheck →
  codegen. Constraints validated, illegal `memory` constraint converted to a
  clear error telling the user to write `~{memory}`. The codegen bypasses
  inkwell's `build_indirect_call` (which crashes on Windows with empty args)
  and calls `LLVMBuildCall2` directly. `-arch arm64` is now split into
  individual `Command::arg` calls so Rust's `Command` API passes the linker
  args through correctly on iOS too.
- **`static` / `static mut` global variables** (P1.5). Module-scope storage
  with thread-local and section placement support.

### Fixed
- **riscv64-unknown-none segfault** on Windows. inkwell `target-riscv` was
  missing from features and `Target::initialize_riscv` was never called in
  the AOT initializer; both added.
- **Android cross-linker**: `--defsym=_snprintf=snprintf` now skipped for
  Android (renaming happens in emit_object, same as Mach-O), and `snprintf`
  resolved directly from bionic libc. Removed the redundant `-lc` flag.
- **iOS shared library**: linker arguments were concatenated into single
  strings (`"-arch arm64"`, `"-isysroot /path"`). `Command::arg` does not
  shell-split, so clang received `"-arch arm64"` as one argument and died.
  Split into individual args.
- **Android NDK / CI**: upgraded `android-actions/setup-android` to v4
  (v3 failed with the obsolete `tools` package), pinned NDK 27, added
  `libpolly-22-dev` required by `llvm-sys 221`, and switched LLVM install to
  the `apt.llvm.org` script (Ubuntu noble ships only up to LLVM 20). `brew
  link llvm@22 --force` added on macOS to handle the keg-only formula.

### Changed
- Cross-compile smoke tests (iOS `.dylib`, Android `.so`) green again on CI.
  The two workflows — broken since 2026-09-26 over a mix of distro package
  drift, brew keg-only defaults, and a Rust `Command` footgun — now pass.

### Notes
- This is the first release where Aero can compile a freestanding bare-metal
  kernel from end to end. Follow-up work (P1.6) will strip unconditional libc
  symbol declarations from freestanding IR and do module pruning on the
  standard library; those are not part of this release.
- `aero-v1.2.4-windows-x86_64.zip` is published with this release.

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
