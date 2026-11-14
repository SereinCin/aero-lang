# Aero 编译器嵌入与 ABI 参考

> 对应版本：Aero 1.2.6
> 来源：`crates/aero-ir/src/aot.rs`、`crates/aero-ir/src/lib.rs`、`crates/aero-cli/src/main.rs`、`crates/aero-hir/src/lower.rs`、`crates/aero-parse/src/parser.rs`。

---

## 1. CLI 构建参数

```
aero build <file.aero | dir> [flags]
```

| 参数 | 语义 |
|---|---|
| `-O0` / `-O1` / `-O2` / `-O3` | 优化级别，默认 `O2` |
| `--target <triple>` | 目标三元组，如 `x86_64-unknown-linux-gnu` |
| `--emit-obj` | 只产出 `.o` 目标文件，不链接 |
| `--shared` | 产出动态库（`.so` / `.dll` / `.dylib`） |
| `--pyext` | 产出 Python C 扩展（`.pyd` / `.so`） |
| `--py-module <name>` | Python 扩展模块名，需与产物文件名一致 |
| `--py-home <prefix>` | 指定 CPython 前缀，用于定位 `python3xx.lib` |
| `--cpp` | 产出共享库 + `<name>.hpp` 头文件 |
| `--ndk <path>` | Android NDK 工具链路径（配合 `--target *-android`） |
| `--link-arg <arg>` | 追加链接器参数（可重复） |

其他子命令：

```
aero run   <file|pkg> [-O0..-O3]
aero check <file|pkg>       解析/类型/借用/LLVM 校验，不执行
aero test  [file]           默认跑 tests/ 下全部
aero bench <file> [--iterations N] [--samples N]
aero fmt   <file> [--check] [--width N] [--indent N]
aero clippy <file>
aero cov   <file>
aero lock  [dir]
aero install [name]
aero publish <dir>
aero ls
aero registry
aero lsp | --lsp
```

### 1.1 跨平台链接约束（1.2.6 新增）

LLVM 可以为任意受支持的 triple 生成目标文件，但**生成最终可执行文件/动态库需要宿主机链接器认识目标 OS 的 ABI**。

| 场景 | 结果 |
|---|---|
| `--target` 等于宿主 triple | 允许 |
| `*-none`（freestanding） | 允许 |
| `*-android` | 允许（需 `--ndk`） |
| 与宿主同 OS 家族（Windows→Windows、Linux→Linux） | 允许 |
| 跨 OS（Windows→Linux、Linux→Windows、→macOS） | **报错，退出码 1** |

报错原文：

```
error[AOT]: cannot cross-link from `x86_64-pc-windows-gnu` to `x86_64-unknown-linux-gnu`:
host linker (windows) has no CRT/startup for `linux`.
Use `--emit-obj` for a freestanding object file, or install a cross-toolchain and set AERO_LINKER.
```

需要跨平台产物时用 `--emit-obj` 拿 `.o`，自行链接：

```
aero build app.aero --target x86_64-unknown-linux-gnu --emit-obj
```

> 1.2.5 及之前的行为是**静默忽略** `--target` 的 OS 部分、直接产出 PE 文件，1.2.6 起改为硬失败。

---

## 2. 导出符号（从 Aero 到 C）

### 2.1 属性

| 属性 | 作用 |
|---|---|
| `#[export]` | 设为外部可见，并标记 DLL export（`--shared` 时进入动态符号表） |
| `#[no_mangle]` | 保留原名称，不做名字修饰 |
| `#[py_export]` | 等价于 `#[export]`，并生成 CPython 包装、方法表与 `PyInit_<module>` |

```aero
#[export]
#[no_mangle]
fn add(a: i64, b: i64) -> i64 {
    a + b
}
```

`#[export]` 的函数签名会在 lowering 阶段校验 C ABI 兼容性（`crates/aero-hir/src/lower.rs`）。

`#[py_export]` 的签名会额外校验可转换为 Python 对象的类型。

### 2.2 从 Aero 调用 C

```aero
extern "C" fn puts(s: str) -> i32;
extern "C" fn printf(fmt: str) -> i32;
```

外部符号名默认等于函数名；可用 `= "sym"` 指定真实符号：

```aero
extern "C" fn my_puts(s: str) -> i32 = "puts";
```

不带函数体的 `extern "C" fn` 只声明，符号在链接期解析。链接库通过 `Aero.toml` 的 `[link]` 段提供：

```toml
[link]
libs = ["curl"]
paths = ["C:/libs/curl/lib"]
```

---

## 3. Rust crate 级 API（`aero-ir`）

供外部 Rust 程序直接调用编译器，不经过 CLI。

```rust
pub enum OptLevel { O0, O1, O2, O3 }   // Default = O2

pub fn host_target_triple() -> &'static str;

pub fn compile_to_exe(
    source: &str,
    exe_path: &Path,
    target: &str,
) -> Result<(), AeroError>;

pub fn compile_to_exe_linked(
    source: &str,
    exe_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
    extra_args: &[String],
    source_dir: Option<&Path>,
) -> Result<(), AeroError>;

pub fn compile_to_shared(
    source: &str,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
    extra_args: &[String],
    source_dir: Option<&Path>,
) -> Result<(), AeroError>;

pub fn compile_to_pyext(
    source: &str,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
    spec: &PyExtSpec,
    source_dir: Option<&Path>,
) -> Result<(), AeroError>;

pub fn compile_to_obj(
    source: &str,
    obj_path: &Path,
    opt: OptLevel,
    target: &str,
    /* 其余参数见函数定义 */
);

pub fn compile_file_to_exe(/* 见函数定义 */);
pub fn compile_to_exe_cached(/* 见函数定义 */);

pub struct PyExtSpec<'a> {
    pub module: &'a str,    // 必须匹配产物文件名 stem
    pub api_version: u32,
    pub windows: bool,      // Windows 下走 __imp_ 间接槽（dllimport 语义）
}
```

位置：`compile_to_exe` / `compile_to_exe_linked` / `compile_to_shared` / `compile_to_pyext` / `compile_to_obj` 在 `crates/aero-ir/src/aot.rs`。

### 3.1 最小嵌入示例

```rust
use std::path::Path;
use aero_ir::aot::{compile_to_exe_linked, host_target_triple, OptLevel};

let src = std::fs::read_to_string("app.aero")?;
compile_to_exe_linked(
    &src,
    Path::new("out/app.exe"),
    &[],                 // libs
    &[],                 // lib paths
    OptLevel::O2,
    host_target_triple(),
    &[],                 // 额外链接器参数
    Some(Path::new(".")),// source_dir：解析相对导入
)?;
```

### 3.2 编译缓存

`compile_to_exe_cached` 会命中 `.aero-cache`，源文件未变时直接复用已有产物。
**改动编译器或切换 `--target` 后请清理缓存**，否则可能拿到旧的（甚至是不同 OS 格式的）产物：

```powershell
Remove-Item -Recurse -Force .\examples\.aero-cache
```

---

## 4. Python 扩展

```powershell
aero build math_ext.aero --pyext --py-module math_ext --py-home "C:\Python313"
```

生成 `math_ext.pyd`，其中：

- 每个 `#[py_export] fn` 生成一个包装函数 + `PyMethodDef`
- 模块初始化函数名固定为 `PyInit_<module>`
- 链接期需要 `python3xx.lib`（Windows）或 `libpython3.xx.so`（Unix）

已知踩点（1.2.5 修复）：

| 问题 | 结论 |
|---|---|
| i64 参数用 `ParseTuple` 格式 `l` | Windows 上 `l` 是 32 位，必须用 `L` |
| `api_version` 用 `1000 + minor` | CPython 3.6+ 固定为 `1013` |
| `bytes -> String` 未补 NUL | 需 `malloc(len + 1)` 并写入 `\0` |

---

## 5. C++ 绑定

```powershell
aero build math_ext.aero --cpp
```

产出共享库 + `<name>.hpp`。头文件由编译器根据 `#[export]` 函数签名生成，可直接 `#include` 并在 C++ 工程中调用。

---

## 6. 限制与注意事项

1. `--shared` / `--pyext` 的产物仍保留 `main`，但在动态库模块中隐藏。
2. 跨 OS 链接被拒绝（见 1.1），只有 `--emit-obj` 与 freestanding/Android 例外。
3. `extern "C"` 声明的 `*i32` / `str` 参数按平台 ABI 传递；`str` 是 NUL 结尾的 `char*`。
4. `PyExtSpec::module` 与输出文件 stem 不一致会导致 `import` 失败（`PyInit_` 名字对不上）。
