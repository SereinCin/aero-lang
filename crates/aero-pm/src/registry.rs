//! 本地目录注册表：`<registry>/<name>/<version>/`。
//!
//! 零依赖、离线自包含：注册表就是一个普通目录，每个包一个以版本命名的子目录，
//! 内含 `Aero.toml` 与 `src/lib.aero`。注册表根目录通过 `AERO_REGISTRY` 环境变量
//! 指定，缺省为 `$HOME/.aero/registry`（即用户目录）。

use std::path::{Path, PathBuf};

use crate::graph::PmError;
use crate::semver::{Requirement, Version};

/// 本地注册表句柄。
#[derive(Debug, Clone)]
pub struct Registry {
    root: PathBuf,
}

/// 注册表中某个已发布包（版本 + 目录）。
#[derive(Debug, Clone)]
pub struct RegistryCrate {
    pub name: String,
    pub version: Version,
    /// 该版本的实际目录（`<registry>/<name>/<version>`）。
    pub root: PathBuf,
}

impl Registry {
    /// 按 `AERO_REGISTRY` → `$HOME/.aero/registry` 的顺序定位注册表根目录。
    pub fn locate() -> Registry {
        let root = if let Ok(p) = std::env::var("AERO_REGISTRY") {
            if !p.trim().is_empty() {
                PathBuf::from(p)
            } else {
                default_root()
            }
        } else {
            default_root()
        };
        Registry { root }
    }

    /// 显式指向一个注册表目录。
    pub fn at(root: impl Into<PathBuf>) -> Registry {
        Registry { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 列出注册表中某包的全部已发布版本（目录不存在计为空）。
    pub fn versions(&self, name: &str) -> Vec<RegistryCrate> {
        let pkg_dir = self.root.join(name);
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&pkg_dir) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            let version_dir = entry.path();
            let digest = match version_dir.file_name().and_then(|n| n.to_str()) {
                Some(d) => d.to_string(),
                None => continue,
            };
            if let Some(ver) = Version::parse(&digest) {
                if version_dir.is_dir() && version_dir.join("Aero.toml").exists() {
                    out.push(RegistryCrate { name: name.to_string(), version: ver, root: version_dir });
                }
            }
        }
        out.sort_by(|a, b| a.version.cmp(&b.version));
        out
    }

    /// 列出注册表中全部包名（按名升序）。目录为空返回空列表。
    pub fn packages(&self) -> Vec<String> {
        let mut names = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => return names,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                if n == "src" {
                    continue;
                }
                names.push(n.to_string());
            }
        }
        names.sort();
        names
    }

    /// 按需求选择版本最高的已发布包。
    pub fn resolve(&self, name: &str, req: &Requirement) -> Option<RegistryCrate> {
        let cands = self.versions(name);
        let versions: Vec<Version> = cands.iter().map(|c| c.version.clone()).collect();
        let picked = req.select_highest(&versions)?;
        cands.into_iter().find(|c| &c.version == picked)
    }

    /// 精准定位某一版本。
    pub fn resolve_exact(&self, name: &str, ver: &Version) -> Option<RegistryCrate> {
        self.versions(name).into_iter().find(|c| &c.version == ver)
    }

    /// 把一个已完成发布布局的包（`Aero.toml` + `src/lib.aero`）复制进注册表。
    /// 以该包清单中的 name/version 为键；若该版本已存在则拒绝覆盖。
    pub fn publish(&self, src: &Path) -> Result<RegistryCrate, PmError> {
        let manifest = crate::manifest::load_manifest_from(src)?;
        let ver = Version::parse(&manifest.version).ok_or_else(|| {
            PmError::new(format!("package `{}` has invalid version `{}`", manifest.name, manifest.version))
        })?;
        // 发布包必须是库（只有 src/lib.aero），可执行入口无意义。
        let lib_src = src.join("src").join("lib.aero");
        if !lib_src.exists() {
            return Err(PmError::new(format!(
                "cannot publish `{}`: package lacks `src/lib.aero`",
                src.display()
            )));
        }
        let dest = self.root.join(&manifest.name).join(ver.to_string());
        if dest.exists() {
            return Err(PmError::new(format!(
                "registry already has `{}@{ver}` at {}",
                manifest.name,
                dest.display()
            )));
        }
        std::fs::create_dir_all(&dest).map_err(|e| {
            PmError::new(format!("cannot create registry dir {}: {e}", dest.display()))
        })?;
        copy_dir(&src.join("src"), &dest.join("src"))?;
        copy_file(&src.join("Aero.toml"), &dest.join("Aero.toml"))?;
        Ok(RegistryCrate { name: manifest.name, version: ver, root: dest })
    }

    /// 删除注册表中的某包（`name@version` 精确）。
    pub fn remove(&self, name: &str, ver: &Version) -> Result<(), PmError> {
        let dir = self.root.join(name).join(ver.to_string());
        if !dir.exists() {
            return Err(PmError::new(format!(
                "registry has no `{name}@{ver}` at {}",
                dir.display()
            )));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|e| PmError::new(format!("cannot remove {}: {e}", dir.display())))
    }
}

fn default_root() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".aero").join("registry")
    } else {
        PathBuf::from("aero-registry")
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), PmError> {
    std::fs::create_dir_all(to)
        .map_err(|e| PmError::new(format!("cannot create {}: {e}", to.display())))?;
    for entry in std::fs::read_dir(from)
        .map_err(|e| PmError::new(format!("read {}: {e}", from.display())))?
    {
        let entry = entry.map_err(|e| PmError::new(e.to_string()))?;
        let f = entry.path();
        if f.is_dir() {
            copy_dir(&f, &to.join(entry.file_name()))?;
        } else {
            copy_file(&f, &to.join(entry.file_name()))?;
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<(), PmError> {
    std::fs::copy(from, to).map_err(|e| {
        PmError::new(format!("cannot copy {} → {}: {e}", from.display(), to.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aero_pm_reg_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pkg(dir: &Path, name: &str, version: &str) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Aero.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("src").join("lib.aero"), "fn lib_fn() -> i64 { return 1; }\n").unwrap();
    }

    #[test]
    fn publish_then_resolve_version() {
        let reg_dir = tmpdir("reg");
        let pkg = tmpdir("pkg");
        write_pkg(&pkg, "libx", "1.2.3");
        let reg = Registry::at(&reg_dir);
        reg.publish(&pkg).unwrap();
        assert!(reg.root().join("libx").join("1.2.3").exists());
        assert_eq!(reg.versions("libx").len(), 1);

        // 版本需求解析
        let pick = reg.resolve("libx", &Requirement::parse("^1.0.0").unwrap()).unwrap();
        assert_eq!(pick.version.to_string(), "1.2.3");
        // 不存在则 None
        assert!(reg.resolve("nostalgic", &Requirement::parse("*").unwrap()).is_none());
        let _ = std::fs::remove_dir_all(&reg_dir);
        let _ = std::fs::remove_dir_all(&pkg);
    }

    #[test]
    fn picks_highest_matching_version() {
        let reg_dir = tmpdir("highest");
        for ver in ["0.9.0", "1.0.0", "1.4.5", "2.0.0"] {
            let pkg = tmpdir(&format!("pkg_{ver}"));
            write_pkg(&pkg, "libval", ver);
            Registry::at(&reg_dir).publish(&pkg).unwrap();
            let _ = std::fs::remove_dir_all(&pkg);
        }
        let reg = Registry::at(&reg_dir);
        let pick = reg.resolve("libval", &Requirement::parse("^1.0.0").unwrap()).unwrap();
        assert_eq!(pick.version.to_string(), "1.4.5");
        let _ = std::fs::remove_dir_all(&reg_dir);
    }

    #[test]
    fn publish_rejects_duplicate_version() {
        let reg_dir = tmpdir("dup");
        let pkg = tmpdir("pkgdup");
        write_pkg(&pkg, "liby", "1.0.0");
        let reg = Registry::at(&reg_dir);
        reg.publish(&pkg).unwrap();
        let err = reg.publish(&pkg).unwrap_err();
        assert!(err.msg.contains("already has"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&reg_dir);
        let _ = std::fs::remove_dir_all(&pkg);
    }

    #[test]
    fn publishing_executable_rejected() {
        let reg_dir = tmpdir("exe");
        let pkg = tmpdir("mainpkg");
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(
            pkg.join("Aero.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(pkg.join("src").join("main.aero"), "print(1);\n").unwrap();
        let err = Registry::at(&reg_dir).publish(&pkg).unwrap_err();
        assert!(err.msg.contains("lib.aero"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&reg_dir);
        let _ = std::fs::remove_dir_all(&pkg);
    }
}