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

## String System

Built-in string support over C strings (`str` = `i8*`):

- Concatenation: `"a" + "b"` (compile-time for literals, malloc at runtime)
- Comparison: `==`/`!=` and `str_cmp` (all six operators)
- `len(s)`, indexing `s[i]` (byte value), `substr(s, start, n)`
- `int_to_str(n)`, `str_to_int(s)`, `str_contains(a, b)`, `str_find(a, b)`
- `str_free(s)` releases malloc-allocated string results

## File IO & CLI Arguments (0.1.1)

- `read_file(path) -> str` — read a whole file (empty string on failure)
- `write_file(path, contents) -> i64` — write, returns byte count (-1 on failure)
- `arg_count() -> i64` / `arg(i) -> str` — command-line arguments
  (AOT executables run with `myapp.exe a b c`; JIT runs with no arguments)

## License

MIT