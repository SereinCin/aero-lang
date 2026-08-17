//! Package dependency graph: resolve path + registry deps, topological sort, cycle detection,
//! and reproducible `Aero.lock` generation.
//!
//! Resolution rules:
//! - Path deps (`{ path = "..." }` / string form) resolve relative to the declaring package.
//! - Registry deps (`{ version = "..." }`) resolve against the local registry
//!   (`$AERO_REGISTRY` or `~/.aero/registry`), preferring the highest version that satisfies
//!   the requirement unless `Aero.lock` already pins an in-range version.
//! - The resolved graph is written to `Aero.lock` (exact versions + FNV checksums) for
//!   reproducible builds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::lock::{fnv_checksum, LockEntry, Lockfile, SourceKind};
use crate::manifest::{load_manifest_from, Manifest};
use crate::registry::Registry;
use crate::semver::Version;

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
    /// Package version (from `[package].version`).
    pub version: String,
    /// Whether this is the root package
    pub is_root: bool,
    /// Library source (`src/lib.aero`, function definitions only), provided by dependency packages
    pub lib_source: Option<String>,
    /// Executable source (`src/main.aero`), provided by the root package
    pub main_source: Option<String>,
}

/// The outcome of a dependency resolution: the crate order plus the generated lockfile.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// Crates in topological order (deps first, root last).
    pub crates: Vec<CrateSource>,
    /// The lockfile to write (all resolved deps, exact versions + checksums).
    pub lock: Lockfile,
}

/// Resolve the whole dependency tree from the root; returns crates in topological order (deps first).
pub fn resolve(root: &Path) -> Result<Vec<CrateSource>, PmError> {
    Ok(resolve_with_lock(root)?.crates)
}

/// Resolve the dependency tree and also return the lockfile to persist.
pub fn resolve_with_lock(root: &Path) -> Result<Resolution, PmError> {
    Resolver::new(root).run()
}

/// Resolve against an explicitly chosen registry (used by tests and the registry API).
pub fn resolve_with_registry(root: &Path, registry: Registry) -> Result<Resolution, PmError> {
    Resolver::new_with(root, registry).run()
}

/// Pins we remember while walking deps (name → source + version).
#[derive(Debug, Clone)]
struct Pin {
    version: Version,
    kind: SourceKind,
    /// Path source text (Path kind) or empty (Registry kind).
    path: String,
}

struct Resolver<'a> {
    root: &'a Path,
    registry: Registry,
    lock: Lockfile,
    graph: HashMap<String, CrateSource>,
    pins: HashMap<String, Pin>,
    visiting: Vec<String>,
}

impl<'a> Resolver<'a> {
    fn new(root: &'a Path) -> Resolver<'a> {
        Resolver::new_with(root, Registry::locate())
    }

    fn new_with(root: &'a Path, registry: Registry) -> Resolver<'a> {
        Resolver {
            root,
            registry,
            lock: Lockfile::load(root),
            graph: HashMap::new(),
            pins: HashMap::new(),
            visiting: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Resolution, PmError> {
        let root_manifest = load_manifest_from(self.root)?;
        self.visit(&root_manifest.name, self.root, true)?;

        // Topological sort (deps first): repeat Kahn until empty
        let mut order: Vec<CrateSource> = Vec::new();
        let mut done: HashMap<String, bool> = HashMap::new();
        let mut remaining: Vec<CrateSource> = self.graph.values().cloned().collect();
        while !remaining.is_empty() {
            let mut progressed = false;
            let mut next: Vec<CrateSource> = Vec::new();
            for cs in &remaining {
                let manifest = load_manifest_from(&cs.root)?;
                let all_deps_done = manifest
                    .deps
                    .iter()
                    .all(|d| done.get(&d.name).copied().unwrap_or(false));
                if all_deps_done {
                    done.insert(cs.name.clone(), true);
                    order.push(cs.clone());
                    progressed = true;
                } else {
                    next.push(cs.clone());
                }
            }
            if !progressed {
                let names: Vec<&str> = next.iter().map(|c| c.name.as_str()).collect();
                return Err(PmError::new(format!("unresolvable dependencies: {}", names.join(", "))));
            }
            remaining = next;
        }

        // Root package goes last (non-root order preserves the topological order)
        order.sort_by_key(|c| c.is_root);

        // Build the lockfile from resolved deps
        let mut lock = Lockfile::new();
        for cs in &order {
            if cs.is_root {
                continue;
            }
            let manifest = load_manifest_from(&cs.root)?;
            let version = Version::parse(&manifest.version).unwrap_or_else(|| {
                Version { major: 0, minor: 0, patch: 0, pre: Vec::new() }
            });
            let pin = self.pins.get(&cs.name).cloned().unwrap_or(Pin {
                version: version.clone(),
                kind: SourceKind::Registry,
                path: String::new(),
            });
            let lib = cs
                .lib_source
                .as_deref()
                .unwrap_or("");
            lock.entries.push(LockEntry {
                name: cs.name.clone(),
                version: pin.version,
                kind: pin.kind,
                path: pin.path,
                checksum: fnv_checksum(lib),
            });
        }
        lock.entries.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Resolution { crates: order, lock })
    }

    fn visit(&mut self, name: &str, root: &Path, is_root: bool) -> Result<(), PmError> {
        if let Some(existing) = self.graph.get(name) {
            // Already resolved.
            let _ = existing;
            if is_root && !self.graph.get(name).map(|c| c.is_root).unwrap_or(false) {
                return Ok(());
            }
            return Ok(());
        }
        if self.visiting.contains(&name.to_string()) {
            return Err(PmError::new(format!(
                "dependency cycle: {} -> {name}",
                self.visiting.join(" -> ")
            )));
        }
        self.visiting.push(name.to_string());
        let manifest = load_manifest_from(root)?;
        for dep in &manifest.deps {
            if dep.version_req.is_some() && dep.path.as_os_str().is_empty() {
                // Registry dependency
                let pkg_root = self.resolve_registry_dep(dep.name.clone(), manifest.clone())?;
                self.visit(&dep.name, &pkg_root, false)?;
            } else {
                // Path dependency
                let dep_root = normalize_path(&root.join(&dep.path))?;
                self.pins.insert(
                    dep.name.clone(),
                    Pin {
                        version: self.dep_version(&dep.name, &dep_root).unwrap_or_else(|| {
                            Version { major: 0, minor: 0, patch: 0, pre: Vec::new() }
                        }),
                        kind: SourceKind::Path,
                        path: dep.path.to_string_lossy().into_owned(),
                    },
                );
                self.visit(&dep.name, &dep_root, false)?;
            }
        }
        self.visiting.pop();

        let crate_src = read_crate(name, root, is_root, &manifest)?;
        self.graph.insert(name.to_string(), crate_src);
        Ok(())
    }

    /// Resolve a registry dependency: honor a lockfile pin when it satisfies the requirement,
    /// otherwise pick the highest matching version from the registry.
    fn resolve_registry_dep(&mut self, name: String, manifest: Manifest) -> Result<PathBuf, PmError> {
        let dep = manifest
            .deps
            .iter()
            .find(|d| d.name == name)
            .expect("dep looked up from its own manifest");
        let req = dep
            .version_req
            .as_ref()
            .expect("registry dep must carry a version requirement");

        // 1) Honor an in-range lockfile pin for reproducibility.
        if let Some(ent) = self.lock.get(&dep.name) {
            if req.matches(&ent.version) {
                let dir = self.registry.root().join(&dep.name).join(ent.version.to_string());
                if dir.join("Aero.toml").exists() {
                    self.pins.insert(
                        dep.name.clone(),
                        Pin { version: ent.version.clone(), kind: SourceKind::Registry, path: String::new() },
                    );
                    return Ok(dir);
                }
            }
        }

        // 2) Resolve highest matching version from the registry.
        let rc = self.registry.resolve(&dep.name, req).ok_or_else(|| {
            PmError::new(format!(
                "cannot resolve dependency `{}@{req}` from registry `{}` \
                 (set AERO_REGISTRY or publish the package first)",
                dep.name,
                self.registry.root().display()
            ))
        })?;
        self.pins.insert(
            dep.name.clone(),
            Pin { version: rc.version.clone(), kind: SourceKind::Registry, path: String::new() },
        );
        Ok(rc.root)
    }

    fn dep_version(&self, _name: &str, root: &Path) -> Option<Version> {
        let m = load_manifest_from(root).ok()?;
        Version::parse(&m.version)
    }
}

/// Read and parse the Aero.toml in a directory.
pub fn load_manifest(root: &Path) -> Result<Manifest, PmError> {
    load_manifest_from(root)
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
fn read_crate(name: &str, root: &Path, is_root: bool, manifest: &Manifest) -> Result<CrateSource, PmError> {
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
        version: manifest.version.clone(),
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

    fn write_pkg(dir: &Path, name: &str, version: &str, deps: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut toml = format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n");
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

    /// Write a package ready to be published into a registry dir.
    fn write_registry_pkg(reg: &Path, name: &str, version: &str, body: &str) {
        let dir = reg.join(name).join(version);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Aero.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("src").join("lib.aero"), body).unwrap();
    }

    #[test]
    fn resolve_single_root() {
        let d = tmpdir("single");
        write_pkg(&d, "app", "0.1.0", &[]);
        let crates = resolve(&d).unwrap();
        assert_eq!(crates.len(), 1);
        assert_eq!(crates[0].name, "app");
        assert_eq!(crates[0].version, "0.1.0");
        assert!(crates[0].is_root);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_deps_topological() {
        let d = tmpdir("deps");
        write_pkg(&d.join("liba"), "liba", "0.1.0", &[]);
        write_pkg(&d.join("libb"), "libb", "0.1.0", &[("liba", "../liba")]);
        write_pkg(&d.join("app"), "app", "0.1.0", &[("libb", "../libb")]);
        let crates = resolve(&d.join("app")).unwrap();
        assert_eq!(crates.len(), 3);
        assert_eq!(crates[0].name, "liba");
        assert_eq!(crates[1].name, "libb");
        assert_eq!(crates[2].name, "app");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_cycle_rejected() {
        let d = tmpdir("cycle");
        write_pkg(&d.join("liba"), "liba", "0.1.0", &[("libb", "../libb")]);
        write_pkg(&d.join("libb"), "libb", "0.1.0", &[("liba", "../liba")]);
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

    #[test]
    fn registry_dep_resolved_and_locked() {
        let d = tmpdir("regdep");
        let reg = Registry::at(&d.join("registry"));
        write_registry_pkg(&d.join("registry"), "libx", "1.2.0", "fn xa() -> i64 { return 1; }\n");
        write_registry_pkg(&d.join("registry"), "libx", "1.5.0", "fn xa() -> i64 { return 2; }\n");
        write_registry_pkg(&d.join("registry"), "libx", "2.0.0", "fn xa() -> i64 { return 3; }\n");

        // app depends on libx via version requirement
        let app = d.join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            app.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlibx = { version = \"^1.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(
            app.join("src").join("main.aero"),
            "fn _use() { }\nprint(\"%lld\\n\", xa());\n",
        )
        .unwrap();

        let res = resolve_with_registry(&app, reg.clone()).unwrap();
        assert_eq!(res.crates.len(), 2);
        let libx = res.crates.iter().find(|c| c.name == "libx").unwrap();
        // highest ^1.0.0 match = 1.5.0
        assert_eq!(libx.version, "1.5.0");
        let entry = res.lock.get("libx").unwrap();
        assert_eq!(entry.version.to_string(), "1.5.0");
        assert_eq!(entry.kind, SourceKind::Registry);
        assert!(!entry.checksum.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lockfile_pin_is_respected() {
        let d = tmpdir("lockpin");
        let reg = Registry::at(&d.join("registry"));
        write_registry_pkg(&d.join("registry"), "libz", "1.1.0", "fn z() -> i64 { return 1; }\n");
        write_registry_pkg(&d.join("registry"), "libz", "1.9.0", "fn z() -> i64 { return 2; }\n");

        let app = d.join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            app.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlibz = { version = \"^1.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("src").join("main.aero"), "print(1);\n").unwrap();

        // First resolve picks highest (1.9.0). Persist the lock.
        let res = resolve_with_registry(&app, reg.clone()).unwrap();
        res.lock.save(&app).unwrap();
        let crates = resolve_with_registry(&app, reg).unwrap().crates;
        let libz = crates.iter().find(|c| c.name == "libz").unwrap();
        // Respect the pinned version instead of re-max.
        assert_eq!(libz.version, "1.9.0");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn registry_dep_unresolvable_rejected() {
        let d = tmpdir("regmissing");
        let reg = Registry::at(&d.join("registry"));
        let app = d.join("app");
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(
            app.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nghost = { version = \"^1.0.0\" }\n",
        )
        .unwrap();
        std::fs::write(app.join("src").join("main.aero"), "print(1);\n").unwrap();
        let err = resolve_with_registry(&app, reg).unwrap_err();
        assert!(err.msg.contains("cannot resolve"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&d);
    }
}