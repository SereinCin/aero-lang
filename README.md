# Aero

A systems programming language that aims for Python-level development speed with C++-level performance, plus native support for AI computing.

## Pipeline

```
Source -> Lex -> Parse -> HIR (type inference + borrow check) -> LLVM IR -> JIT / AOT executable
```

## Crates

| Crate      | Role                                              |
| ---------- | ------------------------------------------------- |
| `aero-lex` | Tokenizer                                         |
| `aero-parse` | Parser (AST)                                    |
| `aero-hir` | Name resolution, type inference, borrow checking  |
| `aero-ir`  | LLVM IR codegen, JIT execution, AOT compilation   |
| `aero-pm`  | Package manager (Aero.toml, dependency graph, tests) |
| `aero-cli` | CLI: `aero run/build/new/test`                    |

## Toolchain Requirements

- Rust 1.97+ (edition 2024)
- LLVM 22 (llvm-sys 221); set `LLVM_SYS_221_PREFIX` to the LLVM prefix directory
- MinGW UCRT64 `gcc` on `PATH` (used as the AOT linker)

## Build & Run

- `scripts\build.bat` — build the compiler
- `scripts\run.bat` — run `examples\hello.aero` through the LLVM JIT
- `cargo test` — run the full test suite

## CLI

```
aero run <file.aero | package-dir>    compile and execute
aero build [file.aero | dir]          compile to a standalone executable (AOT)
aero new <name>                       create a new package skeleton
aero test [file.aero]                 run tests (default: all in tests/)
```

## FFI

External C functions are declared with `extern "C"` (no body); the symbol name defaults to the function name and can be overridden with `= "c_symbol"`. Libraries are linked via the `[link]` section of `Aero.toml`:

```toml
[link]
libs = ["kernel32"]   # passed to the linker as -lkernel32
lib_paths = ["."]     # extra library search paths (-L)
```

## License

MIT