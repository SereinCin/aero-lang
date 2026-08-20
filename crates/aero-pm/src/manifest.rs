//! Aero.toml manifest parsing: a hand-written mini-TOML parser.
//!
//! Supports the TOML subset Aero.toml needs:
//! - `[package]` / `[dependencies]` tables
//! - strings / integers / booleans
//! - inline tables (`foo = { path = "../bar" }`)
//! - `#` line comments
//!
//! No external TOML crate: the toolchain stays self-contained (buildable offline).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::semver::Requirement;

/// Manifest parsing error.
#[derive(Debug, Clone)]
pub struct ManifestError {
    pub line: u32,
    pub col: u32,
    pub msg: String,
}

/// 读取并解析一个目录里的 `Aero.toml`（供注册表 / 构建流程复用）。
pub fn load_manifest_from(dir: &Path) -> Result<Manifest, crate::graph::PmError> {
    let path = dir.join("Aero.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| crate::graph::PmError::new(format!("cannot read {}: {}", path.display(), e)))?;
    parse_manifest(&text)
        .map_err(|e| crate::graph::PmError::new(format!("{}:{}: {}", e.line, e.col, e.msg)))
}

/// 把一条路径依赖 `name = { path = "<rel>" }` 写入 `dir/Aero.toml` 的
/// `[dependencies]` 表（用于 `aero install` 安装后回写）。
///
/// - 已存在同名依赖 → 返回错误（不重复写入）；
/// - `[dependencies]` 不存在 → 在文件末尾追加该表；
/// - 保留原有注释与其它表格内容不变（基于行的文本写入，不重排）。
pub fn add_path_dependency(dir: &Path, name: &str, rel: &str) -> Result<(), crate::graph::PmError> {
    let path = dir.join("Aero.toml");
    if !path.is_file() {
        return Err(crate::graph::PmError::new(format!(
            "cannot add dependency: {} does not exist",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| {
        crate::graph::PmError::new(format!("cannot read {}: {}", path.display(), e))
    })?;

    // 解析现有依赖，检测冲突
    let manifest = parse_manifest(&text).map_err(|e| {
        crate::graph::PmError::new(format!("{}:{}: {}", e.line, e.col, e.msg))
    })?;
    if manifest.deps.iter().any(|d| d.name == name) {
        return Err(crate::graph::PmError::new(format!(
            "dependency `{name}` already declared in {}",
            path.display()
        )));
    }

    let line = format!("{name} = {{ path = \"{rel}\" }}");
    let lines: Vec<&str> = text.lines().collect();

    // 找到 `[dependencies]` 表头的行号（0 基）。表头形如 `[dependencies]`，
    // 不匹配 `[dependencies.xxx]` 子表。
    let mut dep_idx: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t == "[dependencies]" {
            dep_idx = Some(i);
            break;
        }
    }

    let mut out: String = String::new();
    match dep_idx {
        Some(idx) => {
            // 在 `[dependencies]` 段内追加：找到该段结束位置（下一段表头或 EOF），
            // 在段内最后一个非空行后插入新行。
            let mut end = lines.len();
            for i in (idx + 1)..lines.len() {
                let t = lines[i].trim();
                if t.starts_with('[') {
                    end = i;
                    break;
                }
            }
            // 检查段内是否已有内容行（跳过空行/注释）来决定是否补空行
            let has_content = lines[idx + 1..end]
                .iter()
                .any(|l| !l.trim().is_empty() && !l.trim().starts_with('#'));
            // 在段内最后一个内容行后插入新行。注意 `end == lines.len()` 时
            // 用 `i == end` 判定会永不成立（i 最大为 len-1），必须用 `end - 1`。
            for (i, l) in lines.iter().enumerate() {
                out.push_str(l);
                out.push('\n');
                if i == end - 1 {
                    if has_content {
                        out.push('\n');
                    }
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
        None => {
            // 末尾追加 [dependencies] 表
            for l in &lines {
                out.push_str(l);
                out.push('\n');
            }
            if !text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("\n[dependencies]\n");
            out.push_str(&line);
            out.push('\n');
        }
    }

    std::fs::write(&path, out).map_err(|e| {
        crate::graph::PmError::new(format!("cannot write {}: {}", path.display(), e))
    })
}

/// A parsed dependency entry.
///
/// - `{ path = "..." }` / 字符串形式 → 路径依赖（`path` 有效，`version_req` 为空）；
/// - `{ version = "..." }` → 注册表依赖（`path` 为空，`version_req` 有效）；
/// - `{ path = "...", version = "..." }` → 路径依赖同时钉定版本（`version_req` 也记录）。
#[derive(Debug, Clone)]
pub struct Dep {
    pub name: String,
    /// 路径依赖的相对路径；注册表依赖为空。
    pub path: PathBuf,
    /// 语义化版本需求；路径依赖通常为空。
    pub version_req: Option<Requirement>,
}

/// An Aero package manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub edition: String,
    /// Path dependencies in `[dependencies]` (in declaration order).
    pub deps: Vec<Dep>,
    /// Link library names from `[link]` (passed to the linker as `-l<lib>`, e.g. `user32`).
    pub link_libs: Vec<String>,
    /// Link library search paths from `[link]` (passed to the linker as `-L<path>`).
    pub link_paths: Vec<String>,
}

/// Parse `Aero.toml` text.
pub fn parse_manifest(text: &str) -> Result<Manifest, ManifestError> {
    let toml = TomlParser::new(text).parse_document()?;
    // Extract [package]
    let package = toml
        .get("package")
        .ok_or_else(|| ManifestError {
            line: 0,
            col: 0,
            msg: "missing `[package]` table".to_string(),
        })?;
    let package = package.as_table().ok_or_else(|| ManifestError {
        line: 0,
        col: 0,
        msg: "`[package]` must be a table".to_string(),
    })?;
    let name = req_str(package, "name", "`[package].name` missing")?;
    let version = req_str(package, "version", "`[package].version` missing")?;
    let edition = opt_str(package, "edition").unwrap_or_else(|| "2024".to_string());

    // Extract [dependencies]
    let mut deps = Vec::new();
    if let Some(deps_tbl) = toml.get("dependencies") {
        let deps_tbl = deps_tbl.as_table().ok_or_else(|| ManifestError {
            line: 0,
            col: 0,
            msg: "`[dependencies]` must be a table".to_string(),
        })?;
        for (dep_name, val) in deps_tbl {
            let (path, version_req) = match val {
                // 字符串形式保持兼容：视为路径依赖
                TomlValue::Str(s) => (PathBuf::from(s), None),
                TomlValue::Table(t) => {
                    let path = match t.get("path") {
                        Some(TomlValue::Str(s)) => Some(PathBuf::from(s)),
                        _ => None,
                    };
                    let version_req = match t.get("version") {
                        Some(TomlValue::Str(s)) => {
                            match Requirement::parse(s) {
                                Some(r) => Some(r),
                                None => {
                                    return Err(ManifestError {
                                        line: 0,
                                        col: 0,
                                        msg: format!(
                                            "dependency `{dep_name}` has invalid version requirement `{s}`"
                                        ),
                                    })
                                }
                            }
                        }
                        _ => None,
                    };
                    match (path, version_req) {
                        // 仅版本需求 → 注册表依赖
                        (None, Some(req)) => (PathBuf::new(), Some(req)),
                        // 仅路径 / 路径+版本 → 路径依赖
                        (Some(p), req) => (p, req),
                        (None, None) => {
                            return Err(ManifestError {
                                line: 0,
                                col: 0,
                                msg: format!(
                                    "dependency `{dep_name}` needs `path = \"...\"` and/or `version = \"...\"`"
                                ),
                            })
                        }
                    }
                }
                _ => {
                    return Err(ManifestError {
                        line: 0,
                        col: 0,
                        msg: format!("dependency `{dep_name}` value must be a string or inline table"),
                    })
                }
            };
            deps.push(Dep {
                name: dep_name.clone(),
                path,
                version_req,
            });
        }
    }

    // Extract [link]: FFI link configuration (Campaign 5)
    let mut link_libs = Vec::new();
    let mut link_paths = Vec::new();
    if let Some(link_tbl) = toml.get("link") {
        let link_tbl = link_tbl.as_table().ok_or_else(|| ManifestError {
            line: 0,
            col: 0,
            msg: "`[link]` must be a table".to_string(),
        })?;
        if let Some(v) = link_tbl.get("libs") {
            link_libs = str_array(v, "`[link].libs` must be a string array (e.g. libs = [\"user32\"])")?;
        }
        if let Some(v) = link_tbl.get("lib_paths") {
            link_paths = str_array(v, "`[link].lib_paths` must be a string array (e.g. lib_paths = [\"C:/libs\"])")?;
        }
    }

    Ok(Manifest {
        name,
        version,
        edition,
        deps,
        link_libs,
        link_paths,
    })
}

/// Extract a string array from a TomlValue.
fn str_array(v: &TomlValue, err: &str) -> Result<Vec<String>, ManifestError> {
    match v {
        TomlValue::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    TomlValue::Str(s) => out.push(s.clone()),
                    _ => {
                        return Err(ManifestError {
                            line: 0,
                            col: 0,
                            msg: err.to_string(),
                        })
                    }
                }
            }
            Ok(out)
        }
        _ => Err(ManifestError {
            line: 0,
            col: 0,
            msg: err.to_string(),
        }),
    }
}

fn req_str<'a>(t: &'a BTreeMap<String, TomlValue>, key: &str, err: &str) -> Result<String, ManifestError> {
    match t.get(key) {
        Some(TomlValue::Str(s)) => Ok(s.clone()),
        _ => Err(ManifestError {
            line: 0,
            col: 0,
            msg: err.to_string(),
        }),
    }
}

fn opt_str(t: &BTreeMap<String, TomlValue>, key: &str) -> Option<String> {
    match t.get(key) {
        Some(TomlValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// mini-TOML value model.
#[derive(Debug, Clone, PartialEq)]
enum TomlValue {
    Str(String),
    Int(i64),
    Bool(bool),
    Table(BTreeMap<String, TomlValue>),
    /// Array: `[a, b, c]` (element types need not match)
    Array(Vec<TomlValue>),
}

impl TomlValue {
    fn as_table(&self) -> Option<&BTreeMap<String, TomlValue>> {
        match self {
            TomlValue::Table(t) => Some(t),
            _ => None,
        }
    }
}

struct TomlParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

/// A table: path + key/value pairs (grouped by the nearest table header).
struct TableEntry {
    path: Vec<String>,
    kvs: Vec<(String, TomlValue)>,
}

impl<'a> TomlParser<'a> {
    fn new(src: &'a str) -> Self {
        // Skip the UTF-8 BOM (Windows editors / Set-Content often write EF BB BF)
        let pos = if src.as_bytes().starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else {
            0
        };
        TomlParser {
            src,
            bytes: src.as_bytes(),
            pos,
            line: 1,
            col: 1,
        }
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ManifestError> {
        Err(ManifestError {
            line: self.line,
            col: self.col,
            msg: msg.into(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\r' | b'\n' => {
                    self.bump();
                }
                b'#' => {
                    // Comment to end of line
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn parse_document(mut self) -> Result<BTreeMap<String, TomlValue>, ManifestError> {
        let mut tables: Vec<TableEntry> = Vec::new();
        let mut cur: Vec<String> = Vec::new();
        loop {
            self.skip_ws();
            if self.eof() {
                break;
            }
            match self.peek() {
                Some(b'[') => {
                    self.bump();
                    let path = self.parse_table_path()?;
                    cur = path;
                }
                _ => {
                    let key = self.parse_key()?;
                    self.skip_ws();
                    self.expect(b'=')?;
                    let val = self.parse_value()?;
                    tables.push(TableEntry {
                        path: cur.clone(),
                        kvs: vec![(key, val)],
                    });
                }
            }
        }

        // Assemble: merge by path
        let mut root: BTreeMap<String, TomlValue> = BTreeMap::new();
        for entry in tables {
            let mut node = &mut root;
            for seg in &entry.path {
                let next = node
                    .entry(seg.clone())
                    .or_insert_with(|| TomlValue::Table(BTreeMap::new()));
                match next {
                    TomlValue::Table(t) => node = t,
                    _ => {
                        return Err(ManifestError {
                            line: self.line,
                            col: self.col,
                            msg: format!("`{seg}` is already occupied by a non-table value"),
                        })
                    }
                }
            }
            for (k, v) in entry.kvs {
                if node.insert(k, v).is_some() {
                    return Err(ManifestError {
                        line: self.line,
                        col: self.col,
                        msg: "duplicate key".to_string(),
                    });
                }
            }
        }
        Ok(root)
    }

    /// Parse a `[a.b.c]` table header; returns the path segments.
    fn parse_table_path(&mut self) -> Result<Vec<String>, ManifestError> {
        let mut path = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b']') => {
                    self.bump();
                    if path.is_empty() {
                        return self.err("empty table header `[]`");
                    }
                    return Ok(path);
                }
                Some(b',') | Some(b'.') => {
                    self.bump();
                }
                _ => {
                    let key = self.parse_key()?;
                    path.push(key);
                }
            }
        }
    }

    fn parse_key(&mut self) -> Result<String, ManifestError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => {
                    self.bump();
                }
                _ => break,
            }
        }
        if self.pos == start {
            return self.err("expected a key name");
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn expect(&mut self, b: u8) -> Result<(), ManifestError> {
        if self.peek() == Some(b) {
            self.bump();
            Ok(())
        } else {
            self.err(format!("expected `{}`", b as char))
        }
    }

    fn parse_value(&mut self) -> Result<TomlValue, ManifestError> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => self.parse_string().map(TomlValue::Str),
            Some(b'{') => self.parse_inline_table(),
            Some(b'[') => self.parse_array(),
            Some(b't') => self.parse_word(b"true").map(|_| TomlValue::Bool(true)),
            Some(b'f') => self.parse_word(b"false").map(|_| TomlValue::Bool(false)),
            Some(b'0'..=b'9') | Some(b'-') => self.parse_int().map(TomlValue::Int),
            _ => self.err("expected a value (string/integer/boolean/array/inline table)"),
        }
    }

    fn parse_word(&mut self, word: &[u8]) -> Result<(), ManifestError> {
        for w in word {
            if self.peek() != Some(*w) {
                return self.err("invalid value");
            }
            self.bump();
        }
        Ok(())
    }

    fn parse_int(&mut self) -> Result<i64, ManifestError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        let mut digits = 0usize;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                digits += 1;
                self.bump();
            } else {
                break;
            }
        }
        if digits == 0 {
            return self.err("expected a digit");
        }
        self.src[start..self.pos]
            .parse::<i64>()
            .map_err(|_| ManifestError {
                line: self.line,
                col: self.col,
                msg: "integer out of range".to_string(),
            })
    }

    fn parse_string(&mut self) -> Result<String, ManifestError> {
        // Opening quote already consumed
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.bump();
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.bump();
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.bump();
                    match self.peek() {
                        Some(b'n') => {
                            out.push('\n');
                            self.bump();
                        }
                        Some(b't') => {
                            out.push('\t');
                            self.bump();
                        }
                        Some(b'r') => {
                            out.push('\r');
                            self.bump();
                        }
                        Some(b'"') => {
                            out.push('"');
                            self.bump();
                        }
                        Some(b'\\') => {
                            out.push('\\');
                            self.bump();
                        }
                        _ => return self.err("unknown escape sequence"),
                    }
                }
                Some(c) => {
                    out.push(c as char);
                    self.bump();
                }
            }
        }
    }

    /// Parse an array: `[v1, v2, ...]` (leading `[` already consumed).
    fn parse_array(&mut self) -> Result<TomlValue, ManifestError> {
        self.bump(); // [
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b']') => {
                    self.bump();
                    return Ok(TomlValue::Array(items));
                }
                None => return self.err("unterminated array"),
                _ => {
                    let val = self.parse_value()?;
                    items.push(val);
                    self.skip_ws();
                    if self.peek() == Some(b',') {
                        self.bump();
                    }
                }
            }
        }
    }

    fn parse_inline_table(&mut self) -> Result<TomlValue, ManifestError> {
        self.bump(); // {
        let mut table = BTreeMap::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'}') => {
                    self.bump();
                    return Ok(TomlValue::Table(table));
                }
                None => return self.err("unterminated inline table"),
                _ => {
                    let key = self.parse_key()?;
                    self.skip_ws();
                    self.expect(b'=')?;
                    let val = self.parse_value()?;
                    if table.insert(key, val).is_some() {
                        return self.err("duplicate key");
                    }
                    self.skip_ws();
                    if self.peek() == Some(b',') {
                        self.bump();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_manifest() {
        let m = parse_manifest("[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
        assert_eq!(m.name, "demo");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.edition, "2024");
        assert!(m.deps.is_empty());
    }

    #[test]
    fn parses_path_dependency() {
        let m = parse_manifest(
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n\n[dependencies]\nlibx = { path = \"../libx\" }\nliby = \"libs/y\"\n",
        )
        .unwrap();
        assert_eq!(m.deps.len(), 2);
        assert_eq!(m.deps[0].name, "libx");
        assert_eq!(m.deps[0].path, PathBuf::from("../libx"));
        assert_eq!(m.deps[1].name, "liby");
    }

    #[test]
    fn comments_and_blank_lines_ok() {
        let m = parse_manifest("# top comment\n[package]  # trailing comment\nname = \"a\"  \nversion = \"0.0.1\"\n").unwrap();
        assert_eq!(m.name, "a");
    }

    #[test]
    fn missing_package_rejected() {
        assert!(parse_manifest("name = \"x\"\n").is_err());
    }

    #[test]
    fn missing_name_rejected() {
        assert!(parse_manifest("[package]\nversion = \"1\"\n").is_err());
    }

    #[test]
    fn version_only_dep_is_registry_dep() {
        let m = parse_manifest("[package]\nname=\"a\"\nversion=\"1\"\n[dependencies]\nx = { version = \"^1.2\" }\n")
            .unwrap();
        assert_eq!(m.deps.len(), 1);
        assert!(m.deps[0].version_req.is_some());
        assert!(m.deps[0].path.as_os_str().is_empty());
    }

    #[test]
    fn dep_without_path_or_version_rejected() {
        let err = parse_manifest("[package]\nname=\"a\"\nversion=\"1\"\n[dependencies]\nx = { }\n")
            .unwrap_err();
        assert!(err.msg.contains("path"), "got: {}", err.msg);
    }

    #[test]
    fn invalid_version_requirement_rejected() {
        let err = parse_manifest("[package]\nname=\"a\"\nversion=\"1\"\n[dependencies]\nx = { version = \"not-a-version\" }\n")
            .unwrap_err();
        assert!(err.msg.contains("invalid version"), "got: {}", err.msg);
    }

    #[test]
    fn unterminated_string_rejected() {
        assert!(parse_manifest("[package]\nname = \"abc\n").is_err());
    }

    #[test]
    fn utf8_bom_tolerated() {
        // Simulate a BOM-prefixed file (common from Windows editors)
        let src = "\u{FEFF}[package]\nname = \"bom\"\nversion = \"1\"\n";
        let m = parse_manifest(src).unwrap();
        assert_eq!(m.name, "bom");
    }

    #[test]
    fn escaped_string_decoded() {
        let m = parse_manifest("[package]\nname = \"a\\n\\t\\\"b\\\"\"\nversion = \"1\"\n").unwrap();
        assert_eq!(m.name, "a\n\t\"b\"");
    }

    #[test]
    fn parses_link_section() {
        let m = parse_manifest(
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n\n[link]\nlibs = [\"user32\", \"kernel32\"]\nlib_paths = [\"C:/libs\"]\n",
        )
        .unwrap();
        assert_eq!(m.link_libs, vec!["user32".to_string(), "kernel32".to_string()]);
        assert_eq!(m.link_paths, vec!["C:/libs".to_string()]);
    }

    #[test]
    fn link_section_optional() {
        let m = parse_manifest("[package]\nname = \"plain\"\nversion = \"1\"\n").unwrap();
        assert!(m.link_libs.is_empty());
        assert!(m.link_paths.is_empty());
    }

    #[test]
    fn link_libs_must_be_string_array() {
        let err = parse_manifest("[package]\nname = \"a\"\nversion = \"1\"\n\n[link]\nlibs = \"user32\"\n")
            .unwrap_err();
        assert!(err.msg.contains("array"), "got: {}", err.msg);
    }

    #[test]
    fn add_dependency_appends_new_table() {
        let dir = std::env::temp_dir().join(format!("aero_pm_adddep_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        add_path_dependency(&dir, "aero-base64", "deps/aero-base64").unwrap();
        let text = std::fs::read_to_string(dir.join("Aero.toml")).unwrap();
        assert!(text.contains("[dependencies]"), "got: {text}");
        assert!(text.contains("aero-base64 = { path = \"deps/aero-base64\" }"), "got: {text}");
        let m = parse_manifest(&text).unwrap();
        assert_eq!(m.deps.len(), 1);
        assert_eq!(m.deps[0].name, "aero-base64");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_dependency_merges_into_existing_table() {
        let dir = std::env::temp_dir().join(format!("aero_pm_adddep2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlibx = { path = \"../libx\" }\n\n[link]\nlibs = []\n",
        )
        .unwrap();
        add_path_dependency(&dir, "aero-hex", "deps/aero-hex").unwrap();
        let text = std::fs::read_to_string(dir.join("Aero.toml")).unwrap();
        assert!(text.contains("aero-hex = { path = \"deps/aero-hex\" }"), "got: {text}");
        // 原有依赖与 link 表保留
        assert!(text.contains("libx = { path = \"../libx\" }"), "got: {text}");
        assert!(text.contains("[link]"), "got: {text}");
        let m = parse_manifest(&text).unwrap();
        assert_eq!(m.deps.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_dependency_appends_into_eof_table() {
        // `[dependencies]` 段位于文件末尾（第二次 `aero install` 的典型场景）：
        // 插入必须命中段末，不能因 `end == lines.len()` 而丢失新行。
        let dir = std::env::temp_dir().join(format!("aero_pm_adddep4_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\naero-base64 = { path = \"deps/aero-base64\" }\n",
        )
        .unwrap();
        add_path_dependency(&dir, "aero-hex", "deps/aero-hex").unwrap();
        let text = std::fs::read_to_string(dir.join("Aero.toml")).unwrap();
        assert!(
            text.contains("aero-hex = { path = \"deps/aero-hex\" }"),
            "got:\n{text}"
        );
        assert!(
            text.contains("aero-base64 = { path = \"deps/aero-base64\" }"),
            "got:\n{text}"
        );
        let m = parse_manifest(&text).unwrap();
        assert_eq!(m.deps.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_dependency_rejects_duplicate() {
        let dir = std::env::temp_dir().join(format!("aero_pm_adddep3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlibx = { path = \"../libx\" }\n",
        )
        .unwrap();
        let err = add_path_dependency(&dir, "libx", "deps/libx").unwrap_err();
        assert!(err.msg.contains("already declared"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
