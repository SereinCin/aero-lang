# Aero

[English](README.md) · [在线文档](https://serein.ren/cn/aero/docs/)

Aero 是给 Python 里那个跑 2.5 秒的函数用的。

写法接近 Python：没有 `fn main`，顶层直接写语句。经 LLVM 编成本地机器码，
产物可以是独立 exe、C 动态库，或者一个 `pip install` 就能装的 Python C 扩展。

[![release](https://img.shields.io/github/v/release/SereinCin/aero-lang?label=release)](https://github.com/SereinCin/aero-lang/releases)
[![license](https://img.shields.io/github/license/SereinCin/aero-lang)](LICENSE)
[![platforms](https://img.shields.io/badge/platforms-windows%20%7C%20linux%20%7C%20macos-lightgrey)](#install)
[![docs](https://img.shields.io/badge/docs-serein.ren-2f6feb)](https://serein.ren/cn/aero/docs/)

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
$ aero run hello.aero      # JIT，冷启动约 90ms
$ aero build hello.aero    # 约 135KB 单文件 exe
```

同一份 mandelbrot（N=2000），同一台 Ryzen 9 9950X3D：Aero 151ms，C 157ms，
Rust 158ms，Go 169ms。四份代码输出一致（`alive=1588053`）。以上是单次测量；
Aero 自身重复运行落在 147ms 到 194ms 之间。

我们不打算替代 Rust。Rust 有生态、有编译器团队、有十年积累，我们没有。
Aero 现在能做的事很窄：**把一段数值代码变成很小的原生库，然后从 Python 或 C 里调它。**

## 还没做到的

写在前面，省得你花两小时 debug 才发现：

- 闭包**不捕获**外部变量。写了会报错（报得还不太好看，见 [#issues](https://github.com/SereinCin/aero-lang/discussions)）
- `mod foo from "..."` 多文件导入**还没实现**。1.2.5 的 CHANGELOG 曾声称支持，是我们的错
- `HashMap` 的键只能是 `i64`
- C 函数**不能返回 `char*`**，结构体**不能按值传递**。包 sqlite3 / zlib 这种句柄式的可以，包 SDL / X11 那种不行
- 没有 async、没有线程、没有宏、没有函数指针
- Tensor 只有 `matmul`，Autograd 和算子库都还没接

## Install

**Windows** — 下 `aero-v1.2.5-windows-x86_64.zip`，解压，双击 `install.bat`，
然后**新开一个** cmd 窗口：

```console
aero --version
```

**Linux / macOS**

```bash
curl -sSL https://github.com/SereinCin/aero-lang/releases/download/v1.2.5/install_linux.sh | sh
aero --version
```

**从源码构建**（需要 Rust 1.97+、LLVM 22、Windows 上还要 MinGW UCRT64 gcc）

```bash
cargo build --release
./target/release/aero run examples/hello.aero
```

## 现在能用的

| | |
|---|---|
| 编译 | JIT（`aero run`）、AOT（`aero build`）、LLVM 22 后端（inkwell） |
| 类型 | `i32` `i64` `f32` `f64` `bool` `str`、struct、单载荷 enum、泛型、trait |
| 内存 | 借用检查（`&` / `&mut`，支持 NLL）、arena 区域分配、块结束自动回收 |
| 集合 | `Vec`、`HashMap`（键 i64）、`BTreeMap`、`BTreeSet`、`LinkedList`、`String` |
| 互操作 | `extern "C"` 调 C；`#[export]` 导出给 C；`aero build --pyext` 出 Python 扩展；`--cpp` 出 C++ 绑定 |
| 工具链 | 包管理（`aero new/run/build/test`）、`fmt`、`clippy`、`cov`、`bench`、LSP、VS Code 扩展 |
| 目标 | Windows / Linux / macOS x86_64 + arm64；三个 `*-unknown-none` 裸机目标（实验性） |

**实验性、别用在正事上**：裸机目标、tensor、`--cpp` 绑定、`mod` 内联以外的模块能力。

## 代码结构

```
crates/aero-lex      词法
crates/aero-parse    语法（AST）
crates/aero-hir      名称解析、类型推断、借用检查
crates/aero-ir       LLVM IR 生成、JIT、AOT
crates/aero-pm       包管理（Aero.toml、依赖图、测试、基准）
crates/aero-cli      CLI 与 LSP
crates/aero-std      标准库（以 Aero 编写）
crates/aero-fmt      格式化
crates/aero-clippy   静态检查
```

## CLI

```
aero run <file.aero | package-dir>    编译并运行（JIT）
aero build [file.aero | dir]          编译成独立可执行文件（AOT）
aero check <file.aero>                只做语法/类型/借用检查
aero new <name>                       新建包
aero test [file.aero]                 跑测试
aero bench <file.aero>                跑基准
aero fmt <file.aero>                  格式化
aero clippy <file.aero>               静态检查
```

## 这个项目怎么做的

4 个人 + AI。AI 负责脚手架、样板代码、初稿和常规重构，约占 55%；
剩下的 45% 是语言与编译器设计、架构决策、review、调试和验证，由团队做。

合入前每段代码要过一遍漏洞扫描、语义一致性检查和基准复核。
AI 给的方案如果违反安全约束、性能预算或架构决定，直接拒。

这件事我们写在 README 里，不是免责声明——是希望你看代码的时候知道该重点看哪儿。

## 参与

- 问题、想法、用不起来 → [Discussions](https://github.com/SereinCin/aero-lang/discussions)
- 能复现的 bug → [Issues](https://github.com/SereinCin/aero-lang/issues)（模板里要求贴 `aero --version`）
- 谁负责哪块 → [AUTHORS.md](AUTHORS.md)
- 怎么提交 → [CONTRIBUTING.md](CONTRIBUTING.md)

## 支持者

感谢在爱发电支持 Aero 的人。

<img src="assets/supporters/ay_xy.jpg" alt="Ay_Xy" width="56" height="56" style="border-radius:50%"> **Ay_Xy**

## License

MIT
