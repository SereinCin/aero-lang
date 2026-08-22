//! 从 GitHub Release 拉取生态包并安装（`aero install`）。
//!
//! 保持工具链零依赖、离线自包含：网络下载复用系统自带 `curl`，SHA-256 校验用
//! `certutil`（Windows）/ `sha256sum`（POSIX），zip 解压用 bsdtar（Windows 10+
//! 内置 `tar`）/ `unzip`（POSIX）。`packages.json` 用本文件内置的最小 JSON 解析器，
//! 不引入任何第三方 crate。
//!
//! 数据流：
//! 1. 下载注册表索引 `packages.json`（默认 GitHub Release `latest/download`）；
//! 2. 匹配/选择目标包；
//! 3. 下载该包的 zip 附件 → 校验 SHA-256；
//! 4. 解压到 `<project>/deps/<name>/`；
//! 5. 把 `name = { path = "deps/<name>" }` 写回 `Aero.toml` 的 `[dependencies]`。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::graph::PmError;
use crate::manifest::{add_path_dependency, load_manifest_from};
use crate::semver::{Requirement, Version};

/// 默认索引地址：指向 GitHub Release 的 `latest` 附件，自动跟随重定向。
pub const DEFAULT_INDEX_URL: &str =
    "https://github.com/SereinCin/Aero-packages/releases/latest/download/packages.json";

/// 一个远程包条目（来自 packages.json）。
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub download_url: String,
    /// SHA-256（小写十六进制，packages.json 提供）。
    pub checksum: String,
    /// 可选：包要求的最低工具链版本（`requires_aero`，可能缺失）。
    pub requires_aero: Option<String>,
}

/// `aero install` 的安装结果。
#[derive(Debug, Clone)]
pub struct InstallReport {
    pub name: String,
    pub version: String,
    /// 解压后的包目录（`<project>/deps/<name>`）。
    pub dir: PathBuf,
    /// 已写入 `[dependencies]` 的键。
    pub dep_key: String,
}

impl fmt::Display for InstallReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "installed {}@{} → {}",
            self.name,
            self.version,
            self.dir.display()
        )
    }
}

/// 从远程索引 URL 下载 `packages.json` 并解析全部条目。
///
/// 索引地址可用 `AERO_INDEX_URL` 环境变量覆盖（便于切换镜像 / 指定 tag）。
pub fn fetch_index(url: &str) -> Result<Vec<IndexEntry>, PmError> {
    let text = download_text(url, "packages.json")?;
    parse_index(&text)
}

/// 解析 packages.json 文本。结构为
/// `{ "schema_version": "...", "packages": [ {name, version, description, author, download_url, checksum, requires_aero?} ] }`。
pub fn parse_index(text: &str) -> Result<Vec<IndexEntry>, PmError> {
    let json = JsonParser::parse(text)?;
    let root = json.as_obj().ok_or_else(|| PmError::new("packages.json: root must be an object"))?;
    let pkgs = root
        .get("packages")
        .and_then(|v| v.as_arr())
        .ok_or_else(|| PmError::new("packages.json: missing `packages` array"))?;
    let mut out = Vec::with_capacity(pkgs.len());
    for (i, item) in pkgs.iter().enumerate() {
        let obj = item.as_obj().ok_or_else(|| {
            PmError::new(format!("packages.json: packages[{i}] is not an object"))
        })?;
        let name = get_str(obj, "name", i)?;
        let version = get_str(obj, "version", i)?;
        let download_url = get_str(obj, "download_url", i)?;
        let checksum = get_str(obj, "checksum", i)?;
        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let requires_aero = obj
            .get("requires_aero")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        out.push(IndexEntry {
            name,
            version,
            description,
            download_url,
            checksum,
            requires_aero,
        });
    }
    Ok(out)
}

fn get_str(obj: &BTreeMap<String, Json>, key: &str, i: usize) -> Result<String, PmError> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| PmError::new(format!("packages.json: packages[{i}] missing `{key}`")))
}

/// 按需求选择要安装的包。`query` 为空返回全部（调用方决定交互）；非空时精确
/// 匹配名字，否则做不区分大小写的子串匹配（返回所有候选，供调用方选择）。
pub fn select_packages<'a>(
    entries: &'a [IndexEntry],
    query: &str,
) -> Vec<&'a IndexEntry> {
    let q = query.trim();
    if q.is_empty() {
        return entries.iter().collect();
    }
    if let Some(e) = entries.iter().find(|e| e.name == q) {
        return vec![e];
    }
    let ql = q.to_lowercase();
    entries
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&ql))
        .collect()
}

/// 主入口：`aero install [query]`。
///
/// - `query` 为空：列出全部可安装包，交互选择；
/// - `query` 精确命中一个：直接安装；
/// - `query` 命中多个：列出候选，交互选择。
///
/// 安装会把目标包的**整棵依赖树**递归装进 `deps/`：每个包的 `Aero.toml`
/// 里的路径依赖（如 `path = "../aero-tcp"`）在解压到 `deps/<name>/` 后，
/// 相对路径恰好解析到 `deps/aero-tcp`，所以依赖图天然闭合。
///
/// `toolchain` 是本机工具链版本号（如 `1.2.0`），用于 `requires_aero` 兼容性过滤。
pub fn install_package(
    project_dir: &Path,
    query: Option<&str>,
    toolchain: &str,
) -> Result<InstallReport, PmError> {
    // 必须在包根目录运行（要有 Aero.toml，install 需要写回 [dependencies]）
    if !project_dir.join("Aero.toml").is_file() {
        return Err(PmError::new(format!(
            "no Aero.toml in {} — run `aero install` inside a package root (create one with `aero new <name>`)",
            project_dir.display()
        )));
    }
    let index_url = std::env::var("AERO_INDEX_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_INDEX_URL.to_string());

    println!("fetching index: {index_url}");
    let entries = fetch_index(&index_url)?;
    if entries.is_empty() {
        return Err(PmError::new("remote index has no packages"));
    }

    let query = query.unwrap_or("").trim().to_string();
    let mut cands = select_packages(&entries, &query);
    // 若精确命中唯一候选，直接安装；否则按 version 降序排候选。
    cands.sort_by(|a, b| {
        let va = Version::parse(&a.version).unwrap_or_default();
        let vb = Version::parse(&b.version).unwrap_or_default();
        vb.cmp(&va)
    });

    let pick: IndexEntry = if query.is_empty() || cands.len() > 1 {
        // 交互选择
        let list: Vec<&IndexEntry> = if query.is_empty() {
            entries.iter().collect()
        } else {
            cands
        };
        pick_interactive(&list, &query)?.clone()
    } else {
        cands[0].clone()
    };

    // 幂等：目标包已在 Aero.toml 的 [dependencies] 声明 → 视为已安装。
    // 用户重复运行 `aero install <name>` 不应报错，也不应重复下载。
    {
        let manifest = crate::graph::load_manifest(project_dir)?;
        if manifest.deps.iter().any(|d| d.name == pick.name) {
            println!(
                "{}@{} is already declared in Aero.toml — nothing to install",
                pick.name, pick.version
            );
            return Ok(InstallReport {
                name: pick.name.clone(),
                version: pick.version.clone(),
                dir: project_dir.join("deps").join(&pick.name),
                dep_key: pick.name.clone(),
            });
        }
    }

    // 递归安装依赖树（主包 + 全部传递依赖）
    let mut installed: Vec<String> = Vec::new();
    let mut visiting: Vec<String> = Vec::new();
    install_recursive(project_dir, &entries, &pick, toolchain, &mut installed, &mut visiting)?;

    // 写回 [dependencies]：只写顶层包（依赖树通过其 Aero.toml 的路径引用自动闭合）
    let rel = format!("deps/{}", pick.name);
    add_path_dependency(project_dir, &pick.name, &rel)?;

    Ok(InstallReport {
        name: pick.name.clone(),
        version: pick.version.clone(),
        dir: project_dir.join("deps").join(&pick.name),
        dep_key: pick.name.clone(),
    })
}

/// 递归安装一个包及其路径依赖。`installed` 是已完成目录，`visiting` 用于环检测。
fn install_recursive(
    project_dir: &Path,
    entries: &[IndexEntry],
    entry: &IndexEntry,
    toolchain: &str,
    installed: &mut Vec<String>,
    visiting: &mut Vec<String>,
) -> Result<(), PmError> {
    // 环检测
    if visiting.iter().any(|n| n == &entry.name) {
        return Err(PmError::new(format!(
            "dependency cycle: {} -> {}",
            visiting.join(" -> "),
            entry.name
        )));
    }
    // 已装则跳过
    if installed.iter().any(|n| n == &entry.name) {
        return Ok(());
    }
    // 目标目录已存在（用户先前装过）→ 视为满足，跳过，不覆盖
    let dest = project_dir.join("deps").join(&entry.name);
    if dest.exists() {
        println!("  {}: already present at {}, skipping", entry.name, dest.display());
        installed.push(entry.name.clone());
        return Ok(());
    }

    // 兼容性过滤：requires_aero 存在时校验工具链版本。
    if let Some(req) = &entry.requires_aero {
        check_compat(req, toolchain)?;
    }

    visiting.push(entry.name.clone());

    // 下载 → 校验 → 解压
    let zip_path = download_zip(entry)?;
    verify_sha256(&zip_path, &entry.checksum)?;
    let tmp_unzip = project_dir.join("deps").join(format!(".{}-tmp", entry.name));
    let _ = std::fs::remove_dir_all(&tmp_unzip);
    unzip(&zip_path, &tmp_unzip)?;
    let _ = std::fs::remove_file(&zip_path);

    // 解压产物可能是目录根直接是包（Aero.toml + src/lib.aero），也可能带一层包裹目录。
    let src_root = locate_package_root(&tmp_unzip)?;
    let manifest = load_manifest_from(&src_root)?;
    if manifest.name != entry.name {
        return Err(PmError::new(format!(
            "package `{}` inside the archive does not match requested `{}`",
            manifest.name, entry.name
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            PmError::new(format!("cannot create {}: {e}", parent.display()))
        })?;
    }
    std::fs::rename(src_root, &dest).map_err(|e| {
        PmError::new(format!("cannot move package into {}: {e}", dest.display()))
    })?;
    let _ = std::fs::remove_dir_all(&tmp_unzip);
    println!("  installed {}@{}", entry.name, entry.version);
    installed.push(entry.name.clone());

    // 递归安装本包声明的路径依赖（来自其 Aero.toml）
    for dep in &manifest.deps {
        if dep.path.as_os_str().is_empty() {
            // 版本依赖（registry）：跳过——aero install 面向 path 依赖树。
            continue;
        }
        // 从 index 找同名包；找不到则报错（依赖缺失不可静默）
        let dep_entry = entries.iter().find(|e| e.name == dep.name).ok_or_else(|| {
            PmError::new(format!(
                "dependency `{}` of `{}` is not in the remote index",
                dep.name, entry.name
            ))
        })?;
        install_recursive(project_dir, entries, dep_entry, toolchain, installed, visiting)?;
    }
    visiting.pop();
    Ok(())
}

/// `requires_aero` 兼容性检查：工具链版本满足需求（如 `>=1.2.0`）则通过。
///
/// `requires_aero` 是版本需求字符串（pack.sh 生成 `>=<tag版本>`），必须用
/// [`Requirement`] 解析匹配，不能用 [`Version`]（它不接受比较符前缀）。
fn check_compat(require: &str, toolchain: &str) -> Result<(), PmError> {
    let req = Requirement::parse(require)
        .ok_or_else(|| PmError::new(format!("invalid requires_aero `{require}`")))?;
    let tc = Version::parse(toolchain)
        .ok_or_else(|| PmError::new(format!("invalid toolchain version `{toolchain}`")))?;
    if !req.matches(&tc) {
        return Err(PmError::new(format!(
            "package requires aero {require}, but this toolchain is {toolchain}"
        )));
    }
    Ok(())
}

/// 列出候选，读一行 stdin 选择（零依赖交互）。
fn pick_interactive<'a>(list: &'a [&'a IndexEntry], query: &str) -> Result<&'a IndexEntry, PmError> {
    use std::io::Write;
    if list.len() == 1 {
        return Ok(list[0]);
    }
    let header = if query.is_empty() {
        "select a package to install:".to_string()
    } else {
        format!("multiple packages match `{query}`; select one:")
    };
    println!("{header}");
    for (i, e) in list.iter().enumerate() {
        println!("  {}. {} {} — {}", i + 1, e.name, e.version, e.description);
    }
    print!("> ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Err(PmError::new("failed to read selection"));
    }
    let sel: usize = line
        .trim()
        .parse()
        .map_err(|_| PmError::new("invalid selection (enter a number)"))?;
    if sel == 0 || sel > list.len() {
        return Err(PmError::new("selection out of range"));
    }
    Ok(list[sel - 1])
}

/// 下载 zip 附件到临时目录，返回本地路径。
fn download_zip(entry: &IndexEntry) -> Result<PathBuf, PmError> {
    let dir = std::env::temp_dir().join("aero-install");
    std::fs::create_dir_all(&dir)
        .map_err(|e| PmError::new(format!("cannot create temp dir {dir:?}: {e}")))?;
    let file = dir.join(format!("{}-{}.zip", entry.name, entry.version));
    println!("downloading {}@{} ...", entry.name, entry.version);
    download_file(&entry.download_url, &file)?;
    Ok(file)
}

/// 下载文本（curl；Windows 10+ 自带）。
pub fn download_text(url: &str, _hint: &str) -> Result<String, PmError> {
    let bytes = download_bytes(url)?;
    String::from_utf8(bytes).map_err(|_| PmError::new("downloaded content is not UTF-8"))
}

/// 下载文件到 `dest`。优先级：
/// 1. `curl -fsSL` 直连 URL；
/// 2. GitHub API fallback（`github.com` 直连被墙时，改走 `api.github.com`
///    `releases/assets/{id}` 下载，绕开被阻断的 302 跳转）；
/// 3. PowerShell `Invoke-WebRequest`（Windows）/ `wget`（POSIX）。
pub fn download_file(url: &str, dest: &Path) -> Result<(), PmError> {
    // 1) curl 直连
    if curl_download(url, dest) {
        return Ok(());
    }
    // 2) GitHub API fallback（仅针对 github.com release 资产 URL）
    if let Some(api_url) = github_api_asset_url(url) {
        println!("  (github.com direct blocked; trying api.github.com ...)");
        let _ = std::fs::remove_file(dest);
        if curl_download(&api_url, dest) {
            return Ok(());
        }
        let _ = std::fs::remove_file(dest);
    }
    // 3) Windows 回退：Invoke-WebRequest
    if std::env::consts::OS == "windows" {
        let script = format!(
            "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; Invoke-WebRequest -Uri '{}' -OutFile '{}' -UseBasicParsing",
            url.replace('\'', "''"),
            dest.to_string_lossy().replace('\'', "''")
        );
        if run_ok(
            "powershell",
            &["-NoProfile", "-NonInteractive", "-Command", &script],
        ) {
            if dest.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
                return Ok(());
            }
            let _ = std::fs::remove_file(dest);
        }
    }
    // 4) POSIX 回退：wget
    if run_ok("wget", &["-q", "-O", dest.to_str().unwrap_or_default(), url]) {
        if dest.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
            return Ok(());
        }
        let _ = std::fs::remove_file(dest);
    }
    Err(PmError::new(format!("cannot download {url} (curl/wget/powershell unavailable or failed)")))
}

/// 用 curl 下载（返回是否成功且产物非空）。
fn curl_download(url: &str, dest: &Path) -> bool {
    // 对 api.github.com 附上 User-Agent / Accept（GitHub API 要求 UA，Accept
    // octet-stream 令资产接口直接返回文件体）
    let mut args: Vec<String> = vec!["-fsSL".into()];
    if url.contains("api.github.com") {
        args.push("-H".into());
        args.push("User-Agent: aero-install".into());
        args.push("-H".into());
        args.push("Accept: application/octet-stream".into());
    }
    args.push("-o".into());
    args.push(dest.to_string_lossy().into_owned());
    args.push(url.to_string());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if !run_ok("curl", &arg_refs) {
        let _ = std::fs::remove_file(dest);
        return false;
    }
    if dest.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        let _ = std::fs::remove_file(dest);
        return false;
    }
    true
}

/// 若 `url` 是 `github.com/{owner}/{repo}/releases/{latest|download/<tag>}/{asset}`
/// 形式的资产 URL，返回可用的 `api.github.com` 下载地址；否则返回 None。
///
/// 原理：github.com 资产地址会 302 到 objects.githubusercontent.com，这条链路
/// 在部分网络（如国内）被阻断；而 `api.github.com/releases/assets/{id}` 加
/// `Accept: application/octet-stream` 可直接返回文件体，且 api.github.com 通常
/// 可直连。资产 id 通过 `GET /repos/{o}/{r}/releases/tags/{tag}`（或 `latest`）
/// 查询 `assets[].name → id` 得到。
fn github_api_asset_url(url: &str) -> Option<String> {
    let u = url.trim_end_matches('/');
    let rest = u.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(4, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let kind = parts.next()?;
    let asset = parts.next()?;
    if kind != "releases" {
        return None;
    }
    // 剩余段形如 `download/v1.2.0/<asset>` 或 `latest/download/<asset>`
    let (tag_part, asset_name) = if let Some(a) = asset.strip_prefix("latest/download/") {
        ("latest", a.to_string())
    } else if let Some(a) = asset.strip_prefix("download/") {
        let mut it = a.splitn(2, '/');
        let t = it.next()?;
        let f = it.next()?;
        if t.is_empty() || f.is_empty() {
            return None;
        }
        (t, f.to_string())
    } else {
        return None;
    };

    // 1) 查询 release，拿资产 id（需要访问 api.github.com 两次）。
    //    注意：`latest` 不是真实 tag，`releases/tags/latest` 是无效端点，
    //    必须走 `releases/latest`。
    let (list_url, recheck) = if tag_part == "latest" {
        (
            format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
            false,
        )
    } else {
        (
            format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{tag_part}"),
            true,
        )
    };
    let list_body = download_bytes_via_curl(&list_url).ok()?;
    let list_text = String::from_utf8(list_body).ok()?;
    let json = JsonParser::parse(&list_text).ok()?;
    let obj = json.as_obj()?;
    let assets = obj.get("assets")?.as_arr()?;
    for a in assets {
        let aobj = a.as_obj()?;
        let name = aobj.get("name")?.as_str()?;
        if name == asset_name {
            let id = aobj.get("id")?.as_id_str()?;
            return Some(format!(
                "https://api.github.com/repos/{owner}/{repo}/releases/assets/{id}"
            ));
        }
    }
    // tag 查询未命中时，回退到 `releases/latest` 端点（比如 tag 名写错或
    // 实际是 latest release）。
    if recheck {
        let list_url = format!(
            "https://api.github.com/repos/{owner}/{repo}/releases/latest"
        );
        let list_body = download_bytes_via_curl(&list_url).ok()?;
        let list_text = String::from_utf8(list_body).ok()?;
        let json = JsonParser::parse(&list_text).ok()?;
        let obj = json.as_obj()?;
        let assets = obj.get("assets")?.as_arr()?;
        for a in assets {
            let aobj = a.as_obj()?;
            let name = aobj.get("name")?.as_str()?;
            if name == asset_name {
                let id = aobj.get("id")?.as_id_str()?;
                return Some(format!(
                    "https://api.github.com/repos/{owner}/{repo}/releases/assets/{id}"
                ));
            }
        }
    }
    None
}

/// 用 curl 下载字节（仅用于解析 GitHub API 响应，附带必要请求头）。
fn download_bytes_via_curl(url: &str) -> Result<Vec<u8>, PmError> {
    let dir = std::env::temp_dir().join("aero-install");
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join(format!("ghapi-{}.json", std::process::id()));
    let mut args: Vec<String> = vec![
        "-fsSL".into(),
        "-H".into(),
        "User-Agent: aero-install".into(),
    ];
    args.push("-o".into());
    args.push(tmp.to_string_lossy().into_owned());
    args.push(url.to_string());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    if !run_ok("curl", &arg_refs) {
        let _ = std::fs::remove_file(&tmp);
        return Err(PmError::new(format!("curl failed for {url}")));
    }
    let bytes = std::fs::read(&tmp)
        .map_err(|e| PmError::new(format!("cannot read temp file: {e}")))?;
    let _ = std::fs::remove_file(&tmp);
    if bytes.is_empty() {
        return Err(PmError::new(format!("empty response from {url}")));
    }
    Ok(bytes)
}

fn download_bytes(url: &str) -> Result<Vec<u8>, PmError> {
    let dir = std::env::temp_dir().join("aero-install");
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join(format!("dl-{}.bin", std::process::id()));
    download_file(url, &tmp)?;
    let bytes = std::fs::read(&tmp)
        .map_err(|e| PmError::new(format!("cannot read downloaded file: {e}")))?;
    let _ = std::fs::remove_file(&tmp);
    Ok(bytes)
}

/// 校验文件 SHA-256（小写十六进制，无空格）。优先 certutil（Windows），
/// 回退 sha256sum（POSIX）。
fn verify_sha256(path: &Path, expected: &str) -> Result<(), PmError> {
    let actual = if std::env::consts::OS == "windows" {
        let out = Command::new("certutil")
            .args(["-hashfile", path.to_str().unwrap_or_default(), "SHA256"])
            .output()
            .map_err(|e| PmError::new(format!("certutil unavailable: {e}")))?;
        if !out.status.success() {
            return Err(PmError::new("certutil failed to compute SHA-256"));
        }
        // certutil 输出（中文/英文系统不同）：从中提取全部 ASCII hex 字符，
        // 拼接后取前 64 位。这样与语言无关、与空格/CR 无关。
        extract_hex64(&out.stdout).ok_or_else(|| {
            PmError::new(format!(
                "cannot parse certutil output:\n{}",
                String::from_utf8_lossy(&out.stdout)
            ))
        })?
    } else {
        let out = Command::new("sha256sum")
            .arg(path)
            .output()
            .map_err(|e| PmError::new(format!("sha256sum unavailable: {e}")))?;
        if !out.status.success() {
            return Err(PmError::new("sha256sum failed"));
        }
        extract_hex64(&out.stdout).ok_or_else(|| {
            PmError::new("cannot parse sha256sum output")
        })?
    };
    let exp = expected.trim().to_lowercase();
    if actual == exp {
        Ok(())
    } else {
        Err(PmError::new(format!(
            "checksum mismatch for {}:\n  expected {exp}\n  actual   {actual}",
            path.display()
        )))
    }
}

/// 从命令输出中提取 64 位 ASCII hex（SHA-256 长度）。哈希通常连续打印为
/// 一行 64 个 hex 字符；路径等其它文本中的 hex 字符会被分隔符打断，所以
/// 找「第一个连续 64 位 hex 串」而不是全量拼接（后者会混入路径字符）。
fn extract_hex64(bytes: &[u8]) -> Option<String> {
    let mut run = String::new();
    for &b in bytes {
        if b.is_ascii_hexdigit() {
            run.push(b.to_ascii_lowercase() as char);
            if run.len() == 64 {
                return Some(run);
            }
        } else {
            run.clear();
        }
    }
    None
}

/// 解压 zip 到目标目录。优先 bsdtar（Windows 10+ 自带，支持 zip），
/// 回退 `unzip`（POSIX）。
fn unzip(zip: &Path, dest: &Path) -> Result<(), PmError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| PmError::new(format!("cannot create {}: {e}", dest.display())))?;
    // bsdtar（Windows 自带 tar.exe 即 bsdtar，能解 zip）
    if run_ok(
        "tar",
        &["-xf", zip.to_str().unwrap_or_default(), "-C", dest.to_str().unwrap_or_default()],
    ) {
        return Ok(());
    }
    // unzip（POSIX）
    if run_ok(
        "unzip",
        &[
            "-q",
            "-o",
            zip.to_str().unwrap_or_default(),
            "-d",
            dest.to_str().unwrap_or_default(),
        ],
    ) {
        return Ok(());
    }
    // Windows 回退：Expand-Archive
    if std::env::consts::OS == "windows" {
        let script = format!(
            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
            zip.to_string_lossy().replace('\'', "''"),
            dest.to_string_lossy().replace('\'', "''")
        );
        if run_ok("powershell", &["-NoProfile", "-NonInteractive", "-Command", &script]) {
            return Ok(());
        }
    }
    Err(PmError::new(format!(
        "cannot extract {} (no tar/unzip/Expand-Archive available)",
        zip.display()
    )))
}

/// 定位解压后的包根：优先直接含 `Aero.toml` 的目录，否则找唯一的子目录。
fn locate_package_root(dir: &Path) -> Result<PathBuf, PmError> {
    if dir.join("Aero.toml").is_file() {
        return Ok(dir.to_path_buf());
    }
    let sub: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| PmError::new(format!("cannot list {}: {e}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    for p in &sub {
        if p.join("Aero.toml").is_file() {
            return Ok(p.clone());
        }
    }
    Err(PmError::new(format!(
        "archive does not contain an Aero package (no Aero.toml found under {})",
        dir.display()
    )))
}

fn run_ok(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 最小 JSON 解析器（仅覆盖 packages.json 需要的子集：对象/数组/字符串/数字/
// 布尔/null）。零依赖，与 manifest.rs 的手写 TOML 风格一致。
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    fn as_obj(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Json::Obj(m) => Some(m),
            _ => None,
        }
    }
    fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    /// 数字或数字字符串 → 字符串（GitHub API 的资产 id 是 JSON 数字）。
    fn as_id_str(&self) -> Option<String> {
        match self {
            Json::Num(n) if n.fract() == 0.0 => Some(format!("{:.0}", n)),
            Json::Str(s) => Some(s.clone()),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn parse(text: &str) -> Result<Json, PmError> {
        let mut p = JsonParser {
            src: text.as_bytes(),
            pos: 0,
        };
        // 跳过 UTF-8 BOM（Windows 工具常写入 EF BB BF）
        if p.src.starts_with(&[0xEF, 0xBB, 0xBF]) {
            p.pos = 3;
        }
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.pos != p.src.len() {
            return Err(PmError::new("packages.json: trailing data after JSON value"));
        }
        Ok(v)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b' ' | b'\t' | b'\r' | b'\n' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn value(&mut self) -> Result<Json, PmError> {
        self.skip_ws();
        match self.src.get(self.pos) {
            None => Err(PmError::new("packages.json: unexpected end of input")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::Str),
            Some(b't') => self.word("true").map(|_| Json::Bool(true)),
            Some(b'f') => self.word("false").map(|_| Json::Bool(false)),
            Some(b'n') => self.word("null").map(|_| Json::Null),
            Some(c) if c.is_ascii_digit() || *c == b'-' => self.number(),
            Some(c) => Err(PmError::new(format!(
                "packages.json: unexpected character `{}` at byte {}",
                *c as char, self.pos
            ))),
        }
    }

    fn object(&mut self) -> Result<Json, PmError> {
        self.pos += 1; // {
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.src.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Json::Obj(map));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.src.get(self.pos) != Some(&b':') {
                return Err(PmError::new("packages.json: expected `:` in object"));
            }
            self.pos += 1;
            let val = self.value()?;
            map.insert(key, val);
            self.skip_ws();
            match self.src.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(map));
                }
                _ => return Err(PmError::new("packages.json: expected `,` or `}` in object")),
            }
        }
    }

    fn array(&mut self) -> Result<Json, PmError> {
        self.pos += 1; // [
        let mut arr = Vec::new();
        self.skip_ws();
        if self.src.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Json::Arr(arr));
        }
        loop {
            arr.push(self.value()?);
            self.skip_ws();
            match self.src.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(arr));
                }
                _ => return Err(PmError::new("packages.json: expected `,` or `]` in array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, PmError> {
        if self.src.get(self.pos) != Some(&b'"') {
            return Err(PmError::new("packages.json: expected string"));
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.src.get(self.pos) {
                None => return Err(PmError::new("packages.json: unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.src.get(self.pos) {
                        Some(b'"') => {
                            out.push('"');
                            self.pos += 1;
                        }
                        Some(b'\\') => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        Some(b'/') => {
                            out.push('/');
                            self.pos += 1;
                        }
                        Some(b'n') => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        Some(b'r') => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        Some(b'b') => {
                            out.push('\u{0008}');
                            self.pos += 1;
                        }
                        Some(b'f') => {
                            out.push('\u{000C}');
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            let hi = self.hex4()?;
                            // 仅处理 BMP 码位（packages.json 全为 ASCII 描述，可忽略代理对）
                            if let Some(c) = char::from_u32(hi) {
                                out.push(c);
                            }
                        }
                        _ => return Err(PmError::new("packages.json: bad escape sequence")),
                    }
                }
                Some(_) => {
                    let ch = self.src[self.pos];
                    if ch < 0x80 {
                        out.push(ch as char);
                        self.pos += 1;
                    } else {
                        // 逐字节收集一个完整的 UTF-8 序列（连续 >=0x80 的字节）
                        let start = self.pos;
                        while self.pos < self.src.len() && self.src[self.pos] >= 0x80 {
                            self.pos += 1;
                        }
                        match std::str::from_utf8(&self.src[start..self.pos]) {
                            Ok(s) => out.push_str(s),
                            Err(_) => {
                                return Err(PmError::new(
                                    "packages.json: invalid UTF-8 in string",
                                ))
                            }
                        }
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, PmError> {
        if self.pos + 4 > self.src.len() {
            return Err(PmError::new("packages.json: bad \\u escape"));
        }
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self.src[self.pos];
            let d = (b as char)
                .to_digit(16)
                .ok_or_else(|| PmError::new("packages.json: bad \\u escape"))?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    fn word(&mut self, w: &str) -> Result<(), PmError> {
        if self.src.get(self.pos..self.pos + w.len()) != Some(w.as_bytes()) {
            return Err(PmError::new(format!("packages.json: expected `{w}`")));
        }
        self.pos += w.len();
        Ok(())
    }

    fn number(&mut self) -> Result<Json, PmError> {
        let start = self.pos;
        if self.src.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while self.pos < self.src.len()
            && (self.src[self.pos].is_ascii_digit()
                || self.src[self.pos] == b'.'
                || self.src[self.pos] == b'e'
                || self.src[self.pos] == b'E'
                || self.src[self.pos] == b'+'
                || self.src[self.pos] == b'-')
        {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| PmError::new("packages.json: bad number"))?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| PmError::new(format!("packages.json: bad number `{text}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
  "schema_version": "1.0",
  "packages": [
    {
      "name": "aero-base64",
      "version": "1.2.0",
      "description": "Base64 编解码",
      "author": "Aero Team",
      "download_url": "https://github.com/SereinCin/Aero-packages/releases/download/v1.2.0/aero-base64.zip",
      "checksum": "2d40346b1b45ecfd8c7bff6f6f84b8f9563ca6e2f012cd3606e68b97fe5dc371"
    },
    {
      "name": "aero-hex",
      "version": "1.2.0",
      "description": "Hex 编解码",
      "author": "Aero Team",
      "download_url": "https://github.com/SereinCin/Aero-packages/releases/download/v1.2.0/aero-hex.zip",
      "checksum": "bc9aaf67c04f802871ac530f58536b5ca71d28ce7b015b98447d44b918619648",
      "requires_aero": "1.2.0"
    }
  ]
}"#;

    #[test]
    fn parses_sample_index() {
        let entries = parse_index(SAMPLE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "aero-base64");
        assert_eq!(entries[0].checksum.len(), 64);
        assert!(entries[0].requires_aero.is_none());
        assert_eq!(entries[1].name, "aero-hex");
        assert_eq!(entries[1].requires_aero.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn select_exact_and_fuzzy() {
        let entries = parse_index(SAMPLE).unwrap();
        assert_eq!(select_packages(&entries, "aero-base64").len(), 1);
        assert_eq!(select_packages(&entries, "hex").len(), 1);
        assert_eq!(select_packages(&entries, "nope").len(), 0);
        assert_eq!(select_packages(&entries, "").len(), 2);
    }

    #[test]
    fn compat_check() {
        // requires_aero 是版本需求（pack.sh 生成 `>=<tag版本>`）
        assert!(check_compat(">=1.2.0", "1.2.0").is_ok());
        assert!(check_compat(">=1.2.0", "1.3.0").is_ok());
        assert!(check_compat(">=1.2.0", "1.1.2").is_err());
        assert!(check_compat(">=2.0.0", "1.2.0").is_err());
        // 裸版本 / 无约束也可解析
        assert!(check_compat("1.2.0", "1.2.0").is_ok());
        assert!(check_compat("*", "0.1.0").is_ok());
    }

    #[test]
    fn utf8_description_parsed() {
        let entries = parse_index(SAMPLE).unwrap();
        assert_eq!(entries[0].description, "Base64 编解码");
    }
}
