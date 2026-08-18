//! `Aero.lock` 依赖锁定文件：读取、生成与基于 FNV-1a 的内容校验。
//!
//! 锁文件用于可复现构建：把已解析的精确版本钉住（而不是每次重新找“最高匹配”），
//! 并用校验和保证包内容未被篡改。

use std::path::Path;

use crate::graph::PmError;
use crate::semver::Version;

/// 依赖来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceKind {
    /// 路径依赖（`[dependencies].x = { path = "..." }`）。
    #[default]
    Path,
    /// 来自注册表的依赖（`[dependencies].x = { version = "..." }`）。
    Registry,
}

/// 锁文件中的单条记录。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LockEntry {
    pub name: String,
    pub version: Version,
    pub kind: SourceKind,
    /// 路径来源时记录其相对路径；注册表来源为空字符串。
    pub path: String,
    /// 内容校验和（FNV-1a 64，小写十六进制）。
    pub checksum: String,
}

/// 整个锁文件。
#[derive(Debug, Clone, Default)]
pub struct Lockfile {
    /// 按依赖名排序（便于比对时稳定遍历）。
    pub entries: Vec<LockEntry>,
}

impl Lockfile {
    pub fn new() -> Lockfile {
        Lockfile { entries: Vec::new() }
    }

    /// 读取并解析 `dir/Aero.lock`；文件不存在或解析失败视为无锁文件（返回默认空）。
    pub fn load(dir: &Path) -> Lockfile {
        match std::fs::read_to_string(dir.join("Aero.lock")) {
            Ok(text) => Self::parse(&text).unwrap_or_default(),
            Err(_) => Lockfile::new(),
        }
    }

    /// 解析锁文件文本。
    pub fn parse(text: &str) -> Result<Lockfile, PmError> {
        let mut lock = Lockfile::new();
        let mut cur: Option<LockEntry> = None;
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line == "[[package]]" {
                if let Some(prev) = cur.replace(LockEntry::default()) {
                    lock.push(prev)?;
                }
                continue;
            }
            let entry = match cur.as_mut() {
                Some(e) => e,
                None => {
                    // 顶部头的 `version = <schema>` 行：容忍并忽略。
                    if line.starts_with("version") {
                        continue;
                    }
                    return Err(lock_err(idx, format!("key outside `[[package]]`: {line}")));
                }
            };
            let (key, val) = line.split_once('=').ok_or_else(|| {
                lock_err(idx, format!("malformed line: {line}"))
            })?;
            let key = key.trim();
            let val = val.trim().trim_matches('"');
            match key {
                "name" => entry.name = val.to_string(),
                "version" => {
                    entry.version = Version::parse(val).ok_or_else(|| {
                        lock_err(idx, format!("invalid version `{val}`"))
                    })?
                }
                "source" => {
                    entry.kind = match val {
                        "path" => SourceKind::Path,
                        "registry" => SourceKind::Registry,
                        other => {
                            return Err(lock_err(idx, format!("unknown source `{other}`")))
                        }
                    }
                }
                "path" => entry.path = val.to_string(),
                "checksum" => entry.checksum = val.to_string(),
                _ => {}
            }
        }
        if let Some(prev) = cur.take() {
            lock.push(prev)?;
        }
        Ok(lock)
    }

    fn push(&mut self, entry: LockEntry) -> Result<(), PmError> {
        if entry.name.is_empty() || entry.checksum.is_empty() {
            return Err(PmError::new("corrupt lockfile: [[package]] missing name/checksum"));
        }
        self.entries.push(entry);
        Ok(())
    }

    /// 序列化为锁文件文本。
    pub fn render(&self) -> String {
        let mut out = String::from("# Aero.lock\nversion = 3\n\n");
        let mut entries: Vec<&LockEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in entries {
            out.push_str("[[package]]\n");
            out.push_str(&format!("name = \"{}\"\n", e.name));
            out.push_str(&format!("version = \"{}\"\n", e.version));
            match e.kind {
                SourceKind::Registry => out.push_str("source = \"registry\"\n"),
                SourceKind::Path => {
                    out.push_str("source = \"path\"\n");
                    out.push_str(&format!("path = \"{}\"\n", e.path));
                }
            }
            out.push_str(&format!("checksum = \"{}\"\n\n", e.checksum));
        }
        out
    }

    /// 写锁文件到 `dir/Aero.lock`。
    pub fn save(&self, dir: &Path) -> Result<(), PmError> {
        std::fs::write(dir.join("Aero.lock"), self.render())
            .map_err(|e| PmError::new(format!("cannot write Aero.lock: {e}")))
    }

    /// 查找某个依赖名；（重新）为解析结果钉一个版本入口。
    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

/// FNV-1a 64 内容校验和（小写十六进制）。用于锁定包内容的完整性。
pub fn fnv_checksum(content: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for b in content.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{:016x}", hash)
}

fn lock_err(line: usize, msg: String) -> PmError {
    PmError::new(format!("Aero.lock:{}: {msg}", line + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lockfile_roundtrip() {
        let mut lock = Lockfile::new();
        lock.entries.push(LockEntry {
            name: "liba".into(),
            version: Version::parse("1.0.0").unwrap(),
            kind: SourceKind::Path,
            path: "../liba".into(),
            checksum: fnv_checksum("fn a(){}"),
        });
        lock.entries.push(LockEntry {
            name: "libz".into(),
            version: Version::parse("2.3.4").unwrap(),
            kind: SourceKind::Registry,
            path: String::new(),
            checksum: fnv_checksum("fn z(){}"),
        });
        let text = lock.render();
        let parsed = Lockfile::parse(&text).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        let a = parsed.get("liba").unwrap();
        assert_eq!(a.version.to_string(), "1.0.0");
        assert_eq!(a.kind, SourceKind::Path);
        assert_eq!(a.path, "../liba");
        let z = parsed.get("libz").unwrap();
        assert_eq!(z.kind, SourceKind::Registry);
    }

    #[test]
    fn parse_tolerates_corrupt_as_default() {
        // parse 对错误应返回 Err；load 对坏文件返回空锁以减少阻碍
        assert!(Lockfile::parse("[garbage").is_err());
        let dir = std::env::temp_dir().join("no_such_aero_lock_dir");
        assert!(Lockfile::load(&dir).entries.is_empty());
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv_checksum("hello"), "a430d84680aabd0b");
        assert_eq!(fnv_checksum(""), "cbf29ce484222325");
        assert_ne!(fnv_checksum("a"), fnv_checksum("b"));
    }

    #[test]
    fn render_sorted_by_name() {
        let mut lock = Lockfile::new();
        lock.entries.push(LockEntry {
            name: "zeta".into(),
            version: Version::parse("1.0.0").unwrap(),
            kind: SourceKind::Registry,
            path: String::new(),
            checksum: "ab".into(),
        });
        lock.entries.push(LockEntry {
            name: "alpha".into(),
            version: Version::parse("1.0.0").unwrap(),
            kind: SourceKind::Registry,
            path: String::new(),
            checksum: "cd".into(),
        });
        let text = lock.render();
        let ia = text.find("alpha").unwrap();
        let iz = text.find("zeta").unwrap();
        assert!(ia < iz);
    }
}