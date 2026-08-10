//! Package dependency graph: resolve Aero.toml path deps, topological sort, cycle detection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::manifest::{parse_manifest, Manifest};

/// Package management error (with a readable message).
#[derive(Debug, Clone)]
pub struct PmError {
    pub msg: String,
}

impl PmError {
    pub fn new(msg: impl Into<String>) -> Self {
        PmError { msg: msg.into() }
    }
}

impl std::fmt::Display for PmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for PmError {}

/// Parsed result of a single crate.
#[derive(Debug, Clone)]
pub struct CrateSource {
    pub name: String,
    /// Package root directory (contains Aero.toml)
    pub root: PathBuf,
    /// Whether this is the root package
    pub is_root: bool,
    /// Library source (`src/lib.aero`, function definitions only), provided by dependency packages
    pub lib_source: Option<String>,
    /// Executable source (`src/main.aero`), provided by the root package
    pub main_source: Option<String>,
}

/// Resolve the whole dependency tree from the root; returns crates in topological order (deps first).
pub fn resolve(root: &Path) -> Result<Vec<CrateSource>, PmError> {
    let mut graph: HashMap<String, CrateSource> = HashMap::new();
    let mut visiting: Vec<String> = Vec::new();
    let root_manifest = load_manifest(root)?;

    fn visit(
        name: &str,
        root: &Path,
        is_root: bool,
        graph: &mut HashMap<String, CrateSource>,
        visiting: &mut Vec<String>,
    ) -> Result<(), PmError> {
        if let Some(existing) = graph.get(name) {
            // Already resolved: if the existing entry is not the root but the current one is, keep the root; else skip
            if is_root && !existing.is_root {
                return Ok(());
            }
            return Ok(());
        }
        if visiting.contains(&name.to_string()) {
            return Err(PmError::new(format!("dependency cycle: {}", visiting.join(" -> "))));
        }
        visiting.push(name.to_string());
        let manifest = load_manifest(root)?;
        for dep in &manifest.deps {
            let dep_root = normalize_path(&root.join(&dep.path))?;
            visit(&dep.name, &dep_root, false, graph, visiting)?;
        }
        visiting.pop();

        let crate_src = read_crate(name, root, is_root)?;
        graph.insert(name.to_string(), crate_src);
        Ok(())
    }

    visit(&root_manifest.name, root, true, &mut graph, &mut visiting)?;

    // Topological sort (deps first): repeat Kahn until empty
    let mut order: Vec<CrateSource> = Vec::new();
    let mut done: HashMap<String, bool> = HashMap::new();
    let mut remaining: Vec<&CrateSource> = graph.values().collect();
    while !remaining.is_empty() {
        let mut progressed = false;
        let mut next: Vec<&CrateSource> = Vec::new();
        for cs in &remaining {
            let manifest = load_manifest(&cs.root)?;
            let all_deps_done = manifest
                .deps
                .iter()
                .all(|d| done.get(&d.name).copied().unwrap_or(false));
            if all_deps_done {
                done.insert(cs.name.clone(), true);
                order.push((*cs).clone());
                progressed = true;
            } else {
                next.push(cs);
            }
        }
        if !progressed {
            // Missing deps (references not in the tree) or leftover cycle
            let names: Vec<&str> = next.iter().map(|c| c.name.as_str()).collect();
            return Err(PmError::new(format!("unresolvable dependencies: {}", names.join(", "))));
        }
        remaining = next;
    }

    // Root package goes last (non-root order preserves the topological order)
    order.sort_by_key(|c| c.is_root);
    Ok(order)
}

/// Read and parse the Aero.toml in a directory.
pub fn load_manifest(root: &Path) -> Result<Manifest, PmError> {
    let path = root.join("Aero.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        PmError::new(format!("cannot read {}: {}", path.display(), e))
    })?;
    parse_manifest(&text).map_err(|e| PmError::new(format!("{}:{}: {}", e.line, e.col, e.msg)))
}

/// Normalize a path (collapse `.`/`..`). Leading `..` in relative paths is kept (legal parent).
fn normalize_path(p: &Path) -> Result<PathBuf, PmError> {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                if parts.last().map(|s| s != "..").unwrap_or(false) {
                    parts.pop();
                } else {
                    parts.push("..".into());
                }
            }
            other => parts.push(other.as_os_str().to_owned()),
        }
    }
    let mut out = PathBuf::new();
    for part in parts {
        out.push(part);
    }
    Ok(out)
}

/// Read a crate source file (library and executable entry).
fn read_crate(name: &str, root: &Path, is_root: bool) -> Result<CrateSource, PmError> {
    let lib_path = root.join("src").join("lib.aero");
    let main_path = root.join("src").join("main.aero");
    let lib_source = if is_root {
        None
    } else if lib_path.exists() {
        Some(read_utf8(&lib_path)?)
    } else if main_path.exists() {
        // Dependencies may also provide main.aero (used as a library; only function definitions are taken)
        Some(read_utf8(&main_path)?)
    } else {
        return Err(PmError::new(format!(
            "dependency `{name}` lacks `src/lib.aero` ({} does not exist)",
            lib_path.display()
        )));
    };
    let main_source = if is_root {
        if main_path.exists() {
            Some(read_utf8(&main_path)?)
        } else {
            return Err(PmError::new(format!(
                "root package lacks `src/main.aero` ({} does not exist)",
                main_path.display()
            )));
        }
    } else {
        None
    };
    Ok(CrateSource {
        name: name.to_string(),
        root: root.to_path_buf(),
        is_root,
        lib_source,
        main_source,
    })
}

fn read_utf8(path: &Path) -> Result<String, PmError> {
    std::fs::read_to_string(path)
        .map_err(|e| PmError::new(format!("cannot read {}: {}", path.display(), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aero_pm_test_{}_{}",
            std::process::id(),
            tag
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pkg(dir: &Path, name: &str, deps: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut toml = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
        if !deps.is_empty() {
            toml.push_str("\n[dependencies]\n");
            for (n, p) in deps {
                toml.push_str(&format!("{n} = {{ path = \"{p}\" }}\n"));
            }
        }
        std::fs::write(dir.join("Aero.toml"), toml).unwrap();
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        if name.starts_with("lib") {
            std::fs::write(src.join("lib.aero"), "fn lib_fn() -> i64 { return 1; }\n").unwrap();
        } else {
            std::fs::write(src.join("main.aero"), "print(1);\n").unwrap();
        }
    }

    #[test]
    fn resolve_single_root() {
        let d = tmpdir("single");
        write_pkg(&d, "app", &[]);
        let crates = resolve(&d).unwrap();
        assert_eq!(crates.len(), 1);
        assert_eq!(crates[0].name, "app");
        assert!(crates[0].is_root);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_deps_topological() {
        let d = tmpdir("deps");
        write_pkg(&d.join("liba"), "liba", &[]);
        write_pkg(&d.join("libb"), "libb", &[("liba", "../liba")]);
        write_pkg(&d.join("app"), "app", &[("libb", "../libb")]);
        let crates = resolve(&d.join("app")).unwrap();
        assert_eq!(crates.len(), 3);
        // Topological order: liba before libb, root app last
        assert_eq!(crates[0].name, "liba");
        assert_eq!(crates[1].name, "libb");
        assert_eq!(crates[2].name, "app");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_cycle_rejected() {
        let d = tmpdir("cycle");
        write_pkg(&d.join("liba"), "liba", &[("libb", "../libb")]);
        write_pkg(&d.join("libb"), "libb", &[("liba", "../liba")]);
        let err = resolve(&d.join("liba")).unwrap_err();
        assert!(err.msg.contains("cycle"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_manifest_rejected() {
        let d = tmpdir("missing");
        let err = resolve(&d).unwrap_err();
        assert!(err.msg.contains("Aero.toml"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&d);
    }
}
