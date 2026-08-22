# Aero

A systems programming language that aims for Python-level development speed with C++-level performance, plus native support for AI computing.

## Quick Start

### Windows
1. Download `Aero-1.2.0-win64.zip` from [Releases](https://github.com/SereinCin/aero-lang/releases)
2. Extract to any folder, double-click `install.bat`
3. Open a new cmd window and run `aero --help`

### Linux
```bash
curl -sSL https://github.com/SereinCin/aero-lang/releases/download/v1.2.0/install_linux.sh | sh
```

### Build from Source
```bash
# Requirements: Rust 1.97+, LLVM 22
cargo build --release
./target/release/aero run examples/hello.aero
```

## Pipeline

```
Source -> Lex -> Parse -> HIR (type inference + borrow check) -> LLVM IR -> JIT / AOT executable
```

## Example

```
print("Hello from Aero!\n");

fn fib(n: i64) -> i64 {
    if (n <= 1) { return n; }
    return fib(n - 1) + fib(n - 2);
}

let i = 0;
while (i < 10) {
    print("fib("); print(i); print(") = "); print(fib(i)); print("\n");
    i = i + 1;
}
```

## Features

| Feature | Status |
|---------|--------|
| AOT / JIT compilation | Stable |
| LLVM IR codegen (inkwell) | Stable |
| Type inference + generics | Stable |
| Ownership + borrow checking | Stable |
| Tensor operations + matmul | Stable |
| C FFI (extern "C") | Stable |
| Vec, HashMap, String, LinkedList | Stable |
| ADTs (enum), traits, operator overloading | Stable |
| Package manager (aero-pm) | Stable |
| LSP server, formatter, linter | Stable |
| Benchmark framework (aero bench) | Stable |
| Linux x86_64 / aarch64 | New in 1.1.2 |
| `aero install` (GitHub ecosystem) | New in 1.2.0 |

## Crates

| Crate      | Role                                              |
| ---------- | ------------------------------------------------- |
| `aero-lex` | Tokenizer                                         |
| `aero-parse` | Parser (AST)                                    |
| `aero-hir` | Name resolution, type inference, borrow checking  |
| `aero-ir`  | LLVM IR codegen, JIT execution, AOT compilation   |
| `aero-pm`  | Package manager (Aero.toml, dependency graph, tests, benchmarks) |
| `aero-cli` | CLI: `aero run/build/new/test/bench/clippy`        |
| `aero-std` | Standard library (Option, Result, HashMap, etc.)   |
| `aero-fmt` | Code formatter                                    |
| `aero-clippy` | Static linter (100+ rules)                     |

## Toolchain Requirements

- Rust 1.97+ (edition 2024)
- LLVM 22 (llvm-sys 221); set `LLVM_SYS_221_PREFIX` to the LLVM prefix directory
- MinGW UCRT64 `gcc` on PATH (Windows, used as the AOT linker)

## Build & Test

```bash
cargo build --release
cargo test
```

## CLI

```
aero run <file.aero | package-dir>    compile and execute
aero build [file.aero | dir]          compile to a standalone executable (AOT)
aero new <name>                       create a new package skeleton
aero test [file.aero]                 run tests
aero bench <file.aero>                run benchmarks
aero fmt <file.aero>                  format code
aero clippy <file.aero>               static analysis
```

## License

MIT
