# 用 Aero 写 HTTP API：现状与示例

> 对应版本：Aero 1.2.6
> 依赖：`aero-http` 1.1.0、`aero-net` 1.1.0、`aero-json` 1.1.0
> **重要前提**：当前生态**只提供 HTTP/TCP 客户端能力**，没有 `bind` / `listen` / `accept`，因此不能直接用标准库包写出 HTTP 服务端。本文给出三条可行路径。

---

## 1. 能力现状

| 能力 | 是否可用 | 来源 |
|---|---|---|
| 发 HTTPS 请求（GET/POST） | 可用 | `aero-http`（libcurl FFI） |
| 解析 JSON 响应 | 可用 | `aero-json` |
| TCP 客户端 connect/send/recv | 可用 | `aero-net` |
| TCP 服务端 listen/accept | **不可用** | 无包提供 |
| 把 Aero 函数暴露成 C ABI 供外部服务器调用 | 可用 | `#[export]` + `--shared` |

---

## 2. 路径 A：调用第三方 API（最常用）

`Aero.toml`：

```toml
[package]
name = "api-client"
version = "0.1.0"
edition = "2024"

[dependencies]
aero-http = "1.1.0"
aero-json = "1.1.0"

[link]
libs = ["curl"]
```

`src/main.aero`：

```aero
fn main() {
    let resp = http_get("https://api.example.com/v1/status");

    if (http_ok(resp)) {
        let body = http_body(resp);
        print("%s\n", body);
    } else {
        print("request failed\n");
    }
}
```

POST：

```aero
let resp = http_post("https://api.example.com/v1/items", "{\"name\":\"widget\"}");
```

> `http_ok` / `http_body` 是 `aero-http` 提供的访问器；`HttpResponse` 的内部字段未在包中公开，请只通过这两个函数取值。

---

## 3. 路径 B：把 Aero 逻辑做成 C API，交给外部 HTTP 服务器托管

适用于"核心逻辑用 Aero 写、HTTP 层用 nginx/uvicorn/Node 等现成服务器"的场景。

```aero
// calc.aero
#[export]
#[no_mangle]
fn fib(n: i64) -> i64 {
    if (n < 2) {
        return n;
    }
    let mut a: i64 = 0;
    let mut b: i64 = 1;
    let mut i: i64 = 2;
    while (i <= n) {
        let t = a + b;
        a = b;
        b = t;
        i = i + 1;
    }
    return b;
}
```

构建：

```powershell
aero build calc.aero --shared
```

Windows 产出 `calc.dll`，Linux 产出 `calc.so`。

C 侧调用：

```c
#include <stdint.h>
extern int64_t fib(int64_t n);

int main(void) {
    return (int) fib(40);
}
```

链接：`gcc main.c -L. -lcalc -o app`。

然后在任意 HTTP 框架里 wrap 这个 C 函数即可，无需自己实现 socket 服务端。

---

## 4. 路径 C：自己写 TCP 服务端（需要自行声明系统调用）

`aero-net` 没有 listen/accept，但 Aero 支持 `extern "C"`，可以直接声明 Winsock / POSIX socket API。

> 以下为**参考写法**，仓库中没有任何包或测试覆盖它，需要按平台自行验证。

Aero.toml 中声明链接库：

```toml
[link]
libs = ["ws2_32"]
```

```aero
extern "C" fn WSAStartup(wVersionRequested: i32, lpWSAData: *i32) -> i32;
extern "C" fn socket(af: i32, typ: i32, protocol: i32) -> *i32;
extern "C" fn bind(s: *i32, addr: *i32, namelen: i32) -> i32;
extern "C" fn listen(s: *i32, backlog: i32) -> i32;
extern "C" fn accept(s: *i32, addr: *i32, addrlen: *i32) -> *i32;
extern "C" fn send(s: *i32, buf: str, len: i64, flags: i32) -> i32;
extern "C" fn recv(s: *i32, buf: str, len: i64, flags: i32) -> i32;
extern "C" fn closesocket(s: *i32) -> i32;
```

要点：

1. `AF_INET = 2`、`SOCK_STREAM = 1`、`IPPROTO_TCP = 6`。
2. `sockaddr_in` 需要 16 字节手工布局（`sin_family: u16`、`sin_port: u16`（网络字节序）、`sin_addr: u32`、`sin_zero: [u8; 8]`）。
3. Windows 必须先 `WSAStartup(0x0202, &wsa_data)`。
4. 这是**阻塞单线程**模型，要做并发需要 `CreateThread` / `clone` 的 FFI 声明。

在没有跨平台抽象层之前，不建议用它写生产服务。

---

## 5. 客户端 TCP 示例（`aero-net`）

```aero
fn main() {
    let rc = net_init();
    if (rc != 0) {
        print("net_init failed: %d\n", rc);
        return;
    }

    let sock = tcp_create();
    let c = tcp_connect(&sock, "93.184.216.34", 80);
    if (c == 0) {
        tcp_send(&sock, "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n");
        let resp = tcp_recv(&sock, 4096);
        print("%s\n", resp);
    } else {
        print("connect failed: %d\n", c);
    }

    tcp_close(&sock);
    net_cleanup();
}
```

> 借用写法说明：包签名是 `tcp_connect(sock: &TcpSocket, ...)`，示例中显式写了 `&sock`。若你的编译器版本支持自动借用，可省略 `&`。

---

## 6. 建议路线

| 目标 | 推荐 |
|---|---|
| 调用别人的 API | 路径 A |
| 提供 API 给别的系统调用 | 路径 B（`--shared` + `#[export]`） |
| 独立自托管 HTTP 服务 | 暂不建议；等 `aero-net` 补上 listen/accept，或走路径 B |
