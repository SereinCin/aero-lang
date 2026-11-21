# Aero 包管理：注册表与清单协议

> 对应版本：Aero 1.2.6
> 本文档描述 1.2.6 中 `aero-pm` 使用的注册表格式与解析规则。

---

## 1. 本地注册表目录

### 1.1 定位顺序

1. 环境变量 `AERO_REGISTRY`（绝对路径，优先）
2. `$HOME/.aero/registry`（POSIX）
3. Windows：`%USERPROFILE%\.aero\registry`
4. Windows 兜底：`%HOMEDRIVE%%HOMEPATH%\.aero\registry`
5. 全部失败：相对路径 `aero-registry`

> 1.2.5 及之前 Windows 只查 `HOME`，该变量通常不存在，导致 `aero ls` 永远为空。1.2.6 修复。

### 1.2 目录布局

```
<registry root>/
└── <package-name>/
    └── <version>/
        ├── Aero.toml
        └── src/
            └── lib.aero
```

示例（仓库内置的 12 个核心包）：

```
aero-registry/aero-collections/1.1.0/Aero.toml
aero-registry/aero-collections/1.1.0/src/lib.aero
```

查看当前生效的路径：

```
aero registry
```

列出可安装的包与版本：

```
aero ls
```

---

## 2. 远程索引

### 2.1 URL 规则

每个 Aero 版本只拉取**自己那个 tag** 的资产，不使用 `latest`：

```
https://github.com/SereinCin/Aero-packages/releases/download/v<version>/packages.json
```

这样 1.2.0 的用户不会拿到 1.3.0 的包，避免被 `requires_aero` 全拒或误装。

可用 `AERO_INDEX_URL` 覆盖为镜像地址：

```powershell
$env:AERO_INDEX_URL = "https://mirror.example.com/packages.json"
```

### 2.2 `packages.json` 结构

顶层是包条目数组（字段名与 `fetch.rs::IndexEntry` 对应）：

```json
[
  {
    "name": "aero-collections",
    "version": "1.1.0",
    "description": "Stack and Queue built on native Vec",
    "download_url": "https://github.com/SereinCin/Aero-packages/releases/download/v1.2.6/aero-collections-1.1.0.zip",
    "checksum": "3f1c9a...e7",
    "requires_aero": "1.2.0"
  }
]
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | 是 | 包名 |
| `version` | string | 是 | 包版本（semver） |
| `description` | string | 是 | 一行描述 |
| `download_url` | string | 是 | zip 直链 |
| `checksum` | string | 是 | **SHA-256，小写十六进制** |
| `requires_aero` | string | 否 | 最低工具链版本；缺失表示不限制 |

> 1.2.5 发布时曾出现索引只包含 46 个 ecosystem 包、遗漏 12 个 core 包的问题。完整索引应为 **58 个包**（12 core + 46 ecosystem）。

### 2.3 安装流程

```
aero install <name>
aero install            # 无参数：交互式选择
```

1. 按当前工具链版本构造索引 URL（或取 `AERO_INDEX_URL`）
2. 下载并解析 `packages.json`
3. 按 `name` 匹配条目，检查 `requires_aero`
4. 下载 `download_url` 的 zip
5. 计算 zip 的 SHA-256，与 `checksum` 比对；不一致即失败
6. 解压到项目下 `deps/<name>/`
7. 把依赖写回项目 `Aero.toml` 的 `[dependencies]`

成功输出：

```
installed aero-collections@1.1.0 → <project>/deps/aero-collections
```

失败时不会写入 `[dependencies]`，避免留下半成品状态。

### 2.4 发布

```
aero publish <dir>
```

把 `<dir>` 打成 `{name}-{version}.zip` 并上传。发布后需要同步更新 `packages.json`（含重新计算的 `checksum`），否则用户安装会校验失败。

---

## 3. `Aero.toml` 清单

### 3.1 字段

`manifest.rs::Manifest` 解析出的结构：

```rust
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub edition: String,
    pub deps: Vec<Dep>,          // Dep { name, path, version_req }
    pub link_libs: Vec<String>,
    pub link_paths: Vec<String>,
}
```

### 3.2 示例

```toml
[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
aero-json = "1.1.0"
aero-http = "1.1.0"

[link]
libs = ["curl"]
paths = ["C:/libs/curl/lib"]
```

| 段 | 键 | 说明 |
|---|---|---|
| `[package]` | `name` / `version` / `edition` | 必填 |
| `[dependencies]` | `<pkg> = "<semver 要求>"` | 也可写路径依赖 |
| `[link]` | `libs` | 链接的系统库名 |
| `[link]` | `paths` | 库搜索路径 |

### 3.3 锁文件

```
aero lock [dir]
```

解析全部依赖并生成/刷新 `Aero.lock`。仓库不提交 `deps/`，只提交 `Aero.toml` 与 `Aero.lock`。

---

## 4. 包搜索路径

`aero run` / `aero build` 接受包目录时会解析依赖：

1. 项目内 `deps/<name>/`
2. 本地注册表 `<registry root>/<name>/<version>/`
3. 相对路径依赖（`[dependencies]` 中写 `path`）

`aero run` 不带参数时，以当前目录为包根；`aero test` 不带参数时跑 `tests/` 下全部文件。

---

## 5. 基准测试（`aero bench`）

```
aero bench <file.aero> [--iterations N] [--samples N]
```

工作方式：

1. 找出所有以 `bench_` 开头的函数
2. 对每个函数生成一个 harness：原源码 + 一个调用 `iterations` 次的 `while` 循环
3. 循环变量名为 `_bench_i<tid>`，避免与用户标识符冲突
4. 编译并运行，采集 `samples` 次，输出耗时统计

生成的循环形态：

```aero
let _bench_i0 = 10000000;
while (_bench_i0 > 0) {
    bench_my_case();
    asm("", "~{memory}");   // 优化屏障
    _bench_i0 = _bench_i0 - 1;
}
```

**`asm("", "~{memory}")` 是 1.2.6 新增的 black-box 屏障。** 没有它时，O2 的 DCE 会把无副作用的 `bench_` 函数体整个删掉，"测出来"的是空循环的耗时。

> 因此：`bench_` 函数如果返回值且未被使用，必须有可观察副作用（写全局、打印、或依赖屏障），否则仍可能被优化。编写 benchmark 时让被测逻辑写入一个会跨循环累加的量最稳妥。

---

## 6. 已知限制

1. 远程生态仍在迁移中，README 中包管理器状态标注为 **Experimental**：本地注册表可用，远程生态（58 包）已能 install，但覆盖面不完整。
2. `packages.json` 的 `checksum` 必须是 zip 原文件的 SHA-256；重新打包 zip 后必须重算，否则校验失败。
3. 跨版本索引隔离依赖 tag 命名：发布 `vX.Y.Z` 时必须同时上传该 tag 下的 `packages.json` 与所有 zip。
