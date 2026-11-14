# Aero

Aero is for the one function in your Python script that takes 2.5 seconds.

It looks like Python — no `fn main`, top-level statements — and compiles through
LLVM to native code. The output can be a standalone executable, a C shared
library, or a Python C extension you can `pip install`.

[中文文档](https://serein.ren/cn/aero/docs/) · [README 中文版](README.zh-CN.md)

[![release](https://img.shields.io/github/v/release/SereinCin/aero-lang?label=release)](https://github.com/SereinCin/aero-lang/releases)
[![license](https://img.shields.io/github/license/SereinCin/aero-lang)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-windows%20%7C%20linux%20%7C%20macos-lightgrey)](#install)
[![docs](https://img.shields.io/badge/docs-serein.ren-2f6feb)](https://serein.ren/en/aero/docs/)

---

```aero
fn fib(n: i64) -> i64 {
    if (n <= 1) { return n; }
    return fib(n - 1) + fib(n - 2);
}

let i = 0;
while (i < 20) {
    print("%d ", fib(i));
    i = i + 1;
}
```

```console
$ aero run hello.aero      # JIT, ~90ms cold start
$ aero build hello.aero    # 137KB standalone exe
```

Same mandelbrot (N=2000), same Ryzen 9 9950X3D: Aero 151ms, C 157ms, Rust 158ms,
Go 169ms. All four print the same `alive=1588053`.

We are not trying to replace Rust. Rust has an ecosystem, a compiler team, and a
decade of work behind it; we don't.

What Aero does today is narrow on purpose: **turn a numeric kernel into a small
native library, and call it from Python or C.**

## What doesn't work yet

Written up front so you don't spend two hours debugging first:

- Closures **do not capture** variables. It's a compile error, not a silent bug — but the message is bad
- `mod foo from "..."` (multi-file import) **doesn't exist**. The 1.2.5 changelog claimed it did. Our mistake
- `HashMap` keys must be `i64`
- A C function **cannot return `char*`**, and structs **cannot be passed by value**. Handle-based libraries (sqlite3, zlib) work; SDL/X11-style ones don't
- No async, no threads, no macros, no function pointers
- `Tensor` is `matmul` only. Autograd and the operator library aren't wired up

## Install

**Windows** — grab `aero-v1.2.5-windows-x86_64.zip`, extract, run `install.bat`,
then open a **new** terminal:

```console
aero --version
```

**Linux / macOS**

```bash
curl -sSL https://github.com/SereinCin/aero-lang/releases/download/v1.2.5/install_linux.sh | sh
aero --version
```

**From source** — Rust 1.97+, LLVM 22, and MinGW UCRT64 gcc on Windows:

```bash
cargo build --release
./target/release/aero run examples/hello.aero
```

## What works

| | |
|---|---|
| Compilation | JIT (`aero run`), AOT (`aero build`), LLVM 22 backend via inkwell |
| Types | `i32` `i64` `f32` `f64` `bool` `str`, structs, single-payload enums, generics, traits |
| Memory | Borrow checking (`&` / `&mut`, NLL), arena allocation freed at scope exit |
| Collections | `Vec`, `HashMap` (i64 keys), `BTreeMap`, `BTreeSet`, `LinkedList`, `String` |
| Interop | `extern "C"` to call C, `#[export]` to be called from C, `--pyext` for Python extensions, `--cpp` for C++ bindings |
| Toolchain | Package manager, `fmt`, `clippy`, `cov`, `bench`, LSP, VS Code extension |
| Targets | Windows / Linux / macOS, x86_64 and arm64; three `*-unknown-none` bare-metal targets |

Experimental — don't build anything you care about on these: bare metal, tensor,
`--cpp` bindings, module support beyond inline `mod name { }`.

## Layout

```
crates/aero-lex      lexer
crates/aero-parse    parser (AST)
crates/aero-hir      name resolution, type inference, borrow checking
crates/aero-ir       LLVM IR codegen, JIT, AOT
crates/aero-pm       package manager (Aero.toml, dep graph, tests, benchmarks)
crates/aero-cli      CLI and LSP
crates/aero-std      standard library, written in Aero
crates/aero-fmt      formatter
crates/aero-clippy   linter
```

## CLI

```
aero run <file.aero | package-dir>    compile and run (JIT)
aero build [file.aero | dir]          compile to a standalone executable (AOT)
aero check <file.aero>                parse / type-check / borrow-check only
aero new <name>                       create a package skeleton
aero test [file.aero]                 run tests
aero bench <file.aero>                run benchmarks
aero fmt <file.aero>                  format
aero clippy <file.aero>               static analysis
```

## How this is built

Four people plus AI. The AI handles scaffolding, boilerplate, first drafts, and
routine refactoring — roughly 55% of shipped code. The other 45% is language and
compiler design, architecture decisions, review, debugging, and verification,
and that's the team's.

Everything merged goes through a vulnerability scan, a semantic consistency
check, and a benchmark re-run. AI suggestions that break a safety guarantee, a
performance budget, or an architectural decision get rejected outright.

This is in the README on purpose. It's not a disclaimer — it tells you where to
look when you read the code.

## Contributing

- Questions, ideas, "I can't get it to build" → [Discussions](https://github.com/SereinCin/aero-lang/discussions)
- Reproducible bugs → [Issues](https://github.com/SereinCin/aero-lang/issues) (the template asks for `aero --version`)
- Who owns what → [AUTHORS.md](AUTHORS.md)
- How to send a patch → [CONTRIBUTING.md](CONTRIBUTING.md)

## Supporters

Thanks to everyone backing Aero on Aifadian.

<img src="assets/supporters/ay_xy.jpg" alt="Ay_Xy" width="56" height="56" style="border-radius:50%"> **Ay_Xy**

## License

MIT
