# Aero 语言 API 参考

> 对应版本：Aero 1.2.6
> 本文只收录已实现并经过验证的接口；未实现或仅部分实现的能力列在「已知限制」里。

---

## 1. 类型系统

| 类型 | 说明 |
|---|---|
| `i32` / `i64` | 有符号整数；整数字面量默认 `i64` |
| `f32` / `f64` | 浮点；浮点字面量默认 `f64` |
| `bool` | 布尔 |
| `char` | Unicode 码位（32 位） |
| `str` | 不可变 C 字符串（`i8*`，NUL 结尾） |
| `String` | 堆分配可增长字符串，布局 `{ data: i8*, len: i64, cap: i64 }`，`data[len]` 恒为 NUL |
| `Vec<T>` | 堆分配可增长向量，布局同 `String` |
| `Box<T>` | 单指针智能指针，`Box::new(v)` 分配并拷贝 |
| `[T; N]` | 编译期定长数组 |
| `(T1, T2, ...)` | 元组 |
| `&T` / `&mut T` | 引用，支持可选命名生命周期 `'a` |
| `*T` | 裸指针 |
| `arena(N)` | 字节容量为 N 的 arena 分配器 |
| `tensor<D1xD2x...>` | 编译期固定形状的张量 |
| `struct` / `union` / `enum` | 名义类型；`union` 字段共享存储，布局取最大字段 |
| `Name<Arg...>` | 泛型结构体/枚举实例，如 `Vec<i64>`、`Maybe<i64>` |
| `dyn Trait` | trait object（胖指针 `{ data, vtable }`） |
| `(T1, ...) -> R` | 函数签名 |

---

## 2. 内置函数（builtin）

直接由编译器识别、无需导入。返回 `void` 表示该调用只能作为语句使用。

### 2.1 断言

```aero
assert(cond: bool) -> void
assert_eq(a: i64, b: i64) -> void
```

### 2.2 字符串（`str`）

```aero
len(s: str) -> i64
substr(s: str, start: i64, end: i64) -> str   // 截取 [start, end)，end 是下标不是长度
str_to_int(s: str) -> i64
str_contains(haystack: str, needle: str) -> bool
str_find(haystack: str, needle: str) -> i64
str_cmp(a: str, b: str) -> i64
str_free(s: str) -> void
int_to_str(n: i64) -> str
format(fmt: str, ...) -> str      // printf 风格，返回新分配的 str
```

`str_find` 返回首次出现位置；`str_cmp` 返回 `<0` / `0` / `>0`。

### 2.3 UTF-8

```aero
utf8_len(s: str) -> i64      // 码位数量，不是字节数
utf8_at(s: str, i: i64) -> i64  // 第 i 个码位；越界返回 -1
```

### 2.4 数值转换

```aero
int_to_f64(n: i64) -> f64
```

> **注意**：`as` 关键字在 Aero 中只用于 `expr as dyn Trait` 装箱，**不是**通用数值转换。`f64 -> i64` 目前没有内置函数。
> 打印 64 位整数请用 `%lld`，不要用 `%d`：
> ```aero
> print("%lld\n", big_value);
> ```
> 1.2.6 起 `print` 会自动把 `%d/%i/%u/%x/%X/%o` 重写为 64 位长度修饰符，但显式写 `%lld` 更清晰。

### 2.5 文件、参数、环境、时间

```aero
read_file(path: str) -> str
write_file(path: str, content: str) -> i64
arg_count() -> i64             // 不含程序名；无额外参数时为 0
arg(i: i64) -> str
rand() -> i64
time() -> i64                 // Unix 秒
get_env(name: str) -> str     // 未设置时返回 ""
set_env(name: str, value: str) -> bool
has_env(name: str) -> bool
file_exists(path: str) -> bool
```

### 2.6 哈希

```aero
str_hash(s: str) -> i64    // FNV-1a
hash_i64(n: i64) -> i64    // splitmix64
```

### 2.7 裸机与并发原语

```aero
volatile_load_i32(p: *i32) -> i32
volatile_store_i32(p: *i32, v: i32) -> void
atomic_load_acquire_i32(p: *i32) -> i32
atomic_store_release_i32(p: *i32, v: i32) -> void
atomic_cmpxchg_i32(p: *i32, expected: i32, desired: i32) -> i32
atomic_fence_acqrel() -> void
```

`void` 的三个（两个 store 加 `atomic_fence_acqrel`）只能作为语句调用。

指针来源只有一个：`extern "C"` 声明返回的指针，或者 `#[export]` 函数的 `*i32` 参数。
`&x` 得到的是引用而不是裸指针，不能直接喂给这些函数。

```aero
extern "C" fn malloc(n: i64) -> *i32 = "malloc";

let p = malloc(4);
volatile_store_i32(p, 5);
print("%d\n", volatile_load_i32(p));   // 5
```

### 2.8 张量

归约（返回值是张量的元素类型，不是固定 `f64`）：

```aero
sum(t: tensor) -> elem    // 所有元素之和
mean(t: tensor) -> elem   // 算术平均
max(t: tensor) -> elem    // 最大元素
min(t: tensor) -> elem    // 最小元素
```

`elem` 为张量创建时指定的元素类型。`tensor(2, 3)` 的元素类型是 `i64`，
所以求出来的是整数；`tensor<f64>(2, 3)` 求出来的才是浮点。整数张量上
`mean` 走整数除法（`[1, 2]` 的 mean 是 `1`，不是 `1.5`）。
```aero
let a = tensor(2, 3);
a[0][0] = 1; a[0][1] = 2; a[0][2] = 3;
a[1][0] = 4; a[1][1] = 5; a[1][2] = 6;
print("%d\n", sum(a));    // 21
print("%d\n", mean(a));   // 3
```

矩阵乘法：

```aero
matmul(a: tensor, b: tensor) -> tensor   // 二维张量相乘，结果形状为 rows(a) x cols(b)
```

逐元素：

```aero
tensor_add(a: tensor, b: tensor) -> tensor
tensor_sub(a: tensor, b: tensor) -> tensor
tensor_mul(a: tensor, b: tensor) -> tensor
tensor_div(a: tensor, b: tensor) -> tensor
tensor_neg(a: tensor) -> tensor
```

BLAS Level-1（`elem` 表示张量元素类型）：

```aero
blas_dot(x: tensor, y: tensor) -> elem        // 点积 x·y
blas_nrm2(x: tensor) -> elem                  // sqrt(sum(x*x))
blas_asum(x: tensor) -> elem                  // sum(|x|)
blas_amax(x: tensor) -> i64                   // |x| 最大元素的下标，0 基
blas_scal(alpha: elem, x: tensor) -> tensor   // 返回 alpha*x 的新张量
blas_axpy(alpha: elem, x: tensor, y: tensor) -> tensor  // 返回 alpha*x + y 的新张量
```

`blas_scal` / `blas_axpy` **不修改入参**，结果放在返回值里：

```aero
let x = tensor<f64>(3);
x[0] = 1.0; x[1] = 2.0; x[2] = 3.0;
let scaled = blas_scal(2.0, x);
print("%f\n", scaled[2]);   // 6.000000，x[2] 仍是 3.0
```

`blas_amax` 返回的是下标而不是数值，取最大值本身请写 `x[blas_amax(x)]`。

张量字面量：

```aero
let a = tensor(3, 4);            // 3x4，元素类型默认 i64
let b = tensor<f64>(3, 4);       // 显式元素类型
a[0][1] = 7;                     // 按 [行][列] 写入
```

> 张量支持：形状由编译期维度决定；逐元素四则、`matmul`、BLAS Level-1、四个归约均已实装。
> **没有**自动微分（autograd）、`transpose`、切片和广播。

### 2.9 内联汇编

```aero
asm("nop", "");
asm("", "~{memory}");           // 编译屏障（bench 使用）
```

第一个参数是汇编模板，第二个是约束字符串。注意是 `asm(...)` 调用形式，**不是** `asm!(...)`。

---

## 3. 标准库包（`aero-registry`，1.1.0）

### aero-collections

```aero
struct Stack
struct Queue

fn stack_new() -> Stack
fn stack_push(s: &mut Stack, val: i64) -> void
fn stack_pop(s: &mut Stack) -> i64
fn stack_peek(s: &mut Stack) -> i64
fn stack_is_empty(s: &mut Stack) -> bool
fn stack_len(s: &mut Stack) -> i64

fn queue_new() -> Queue
fn queue_enqueue(q: &mut Queue, val: i64) -> void
fn queue_dequeue(q: &mut Queue) -> i64
fn queue_is_empty(q: &mut Queue) -> bool
fn queue_len(q: &mut Queue) -> i64
```

### aero-crypto

```aero
fn hex_encode(data: str, len: i64) -> str
fn sha256(input: str) -> str
fn sha256_hash(input: str) -> str
fn xor_obfuscate(data: str, key: str) -> str
fn constant_time_eq(a: str, b: str) -> bool
```

### aero-csv

```aero
fn csv_count_lines(s: str) -> i64
fn csv_get_record(s: str, n: i64) -> str
fn csv_count_fields(record: str) -> i64
fn csv_get_field(record: str, n: i64) -> str
```

### aero-http（libcurl FFI）

```aero
struct HttpResponse

fn http_get(url: str) -> HttpResponse
fn http_post(url: str, body: str) -> HttpResponse
fn http_ok(resp: HttpResponse) -> bool
fn http_body(resp: HttpResponse) -> str
```

依赖系统 `libcurl`，链接时需 `[link] libs = ["curl"]`。

### aero-io

```aero
fn read_text_file(path: str) -> str
fn path_exists(path: str) -> bool
fn file_extension(path: str) -> str
fn basename(path: str) -> str
```

### aero-json

```aero
struct JsonValue

fn parse_json(s: str) -> JsonValue
fn json_get_int(s: str, key: str) -> i64
fn json_get_str(s: str, key: str) -> str
fn json_number(n: i64) -> str
```

### aero-logger

```aero
fn log_error(msg: str) -> void
fn log_warn(msg: str) -> void
fn log_info(msg: str) -> void
fn log_debug(msg: str) -> void
fn log(level: str, msg: str) -> void
fn log_separator() -> void
fn log_kv(key: str, value: str) -> void
fn log_section(title: str) -> void
```

### aero-net（TCP 客户端）

```aero
struct TcpSocket

fn net_init() -> i32
fn net_cleanup() -> void
fn tcp_create() -> TcpSocket
fn tcp_connect(sock: &TcpSocket, ip: str, port: i32) -> i32
fn tcp_send(sock: &TcpSocket, data: str) -> i32
fn tcp_recv(sock: &TcpSocket, bufsize: i64) -> str
fn tcp_close(sock: &TcpSocket) -> void
```

**只有客户端能力**：本包未提供 `bind` / `listen` / `accept`，无法直接做 TCP 服务端。

### aero-regex（POSIX regex FFI）

```aero
struct Regex
struct RegexMatch

fn regex_compile(pattern: str) -> Regex
fn regex_is_match(re: &Regex, text: str) -> bool
fn regex_find(re: &Regex, text: str) -> RegexMatch
fn regex_free(re: &Regex) -> void
fn regex_test(pattern: str, text: str) -> bool
fn regex_group(text: str, m: RegexMatch) -> str
```

### aero-test

```aero
fn assert_true(cond: bool, msg: str) -> void
fn assert_eq_i64(got: i64, expected: i64, msg: str) -> void
fn assert_eq_i32(got: i32, expected: i32, msg: str) -> void
fn assert_eq_str(got: str, expected: str, msg: str) -> void
fn test_group(name: str) -> void
fn test_summary(passed: i64, failed: i64) -> void
```

### aero-time

```aero
fn format_timestamp(secs: i64) -> str
fn format_duration(secs: i64) -> str
fn format_bytes(bytes: i64) -> str
fn timestamp() -> str
```

### aero-toml

```aero
struct TomlValue

fn toml_parse_line(line: str) -> TomlValue
fn toml_is_section(line: str) -> bool
fn toml_is_comment(line: str) -> bool
```

---

## 4. 语法要点

| 特性 | 写法 |
|---|---|
| 变量 | `let x: i64 = 1;` / `let mut y = 2;` |
| 函数 | `fn f(a: i64) -> i64 { a + 1 }`；块尾不带 `;` 的表达式就是返回值 |
| 闭包 | `|x: i64| -> i64 { x * x }`（参数和返回类型都必须标注） |
| 逻辑运算 | `&&` / `\|\|` / `!`（`!` 仅用于 `bool`） |
| 按位取反 | `~x`（仅用于整数）；`&` / `\|` / `^` / `<<` / `>>` |
| 无返回值函数 | `fn f() -> void { ... }`，`void` 与省略返回类型等价 |
| match | `match (v) { 0 => { .. } n => { .. } }`；裸标识符小写开头是绑定模式（`n`），大写开头是枚举变体 |
| 结构体 | `struct S { a: i64 }`，`S { a: 1 }` |
| 枚举 | `enum E { A, B(i64) }`，`E::B(3)` |
| trait | `trait T { fn m(this: Self) -> i64; }` + `impl T for S { fn m(this: S) -> i64 { ... } }`（`Self` 只能出现在 trait 声明里，impl 里写具体类型） |
| 泛型 | `fn max<T>(a: T, b: T) -> T`、`struct V<T> { ... }` |
| 模块 | `mod m { ... }` → 调用 `m::f()`；`use m::f;` |
| FFI | `extern "C" fn puts(s: str) -> i32;` |
| 属性 | `#[export]` / `#[no_mangle]` / `#[py_export]` |

---

## 5. 已知限制（1.2.6）

1. **闭包不能捕获外部变量**。捕获外部变量会在编译期直接报错：
   `closures cannot capture outer-scope variables (not yet supported)`。需要用的值请通过参数传进去。
2. **`mod` 的 `from "path"` 导入形式不存在**。仅有内联 `mod m { ... }`；文件模块会被拒绝。
3. **数值转换**只有 `int_to_f64`；没有 `f64_to_int`、`i32_to_i64` 等显式转换内置（整数窄化/扩展在赋值处由类型检查约束）。
4. **无服务端网络原语**：`aero-net` 只有 connect/send/recv。
5. **`String` / `Vec<T>` 的方法集**未在本文件逐一列出；常用方法见 `examples/` 与标准库包源码。
6. **闭包必须标注返回类型**。`|x: i64| x * x` 会报 `closure body returns a value but the closure has no return type; annotate it, ...`（编译器不从闭包体推断返回类型）。
7. **`let` 不支持元组解构，元组也没有 `t.0` 写法**。`let (a, b) = t;` 和 `t.0` 都会报解析错误；请写 `let t = ...;` 再用 `t[0]` / `t[1]` 取值。
8. **类型注解里没有函数指针类型**。`(i64) -> i64` 这种写法会报明确的解析错误；请声明具名 `fn` 并按名字传参（`let f = my_fn; f(x)` 可用）。
9. **数组下标只对常量做静态检查**。`a[5]`（长度 3）是编译错误；运行期下标不做边界检查，越界读取是未定义行为（与 C 一致）。
10. **整数除零/取模零运行期终止**。生成代码会打印 `aero: division by zero (line N)` 并 `abort()`（退出码 3），不再静默返回 0。
11. **整数溢出按二补数回绕**（与 Rust release 一致），不产生诊断。
12. **`match` 是语句，不是表达式**。arm 的块尾值会被丢弃，需要产出值请写 `return` 或赋值。
13. **非 `void` 函数必须每条路径都能返回**。函数体末尾必须 `return` 或是一个尾部表达式；
    每个分支都要覆盖，`if (x > 0) { return 1; }` 这种漏掉 `else` 的写法会报
    `function ... a path through its body reaches the end without returning a value`（1.2.6 之前静默返回 0）。
    `loop { ... }` 内没有 `break`、或 `match` 的所有 arm 都返回并且覆盖了全部变体（或含 `_` / 绑定兜底 arm），才视为不会落到末尾。
14. **`if` / `while` 的条件必须带括号**：`if (cond) { ... }`；`if` 也不是表达式，不能写 `let x = if ...`。
15. **同一作用域内不能重复 `let` 同名变量（没有 shadowing）**：会报 `variable ... is already declared in this scope`；换名或放进内层块。
16. **元组只能整体传递和下标读取**，没有解构、没有 `t.0` 字段写法。
17. **`match` 不做穷尽性检查**：运行时没有 arm 匹配就直接落在 `match` 之后；非 `void` 函数请补兜底 arm 或末尾 `return`。
