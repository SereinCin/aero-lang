//! Package building: merge dependency sources in topological order into one executable source.
//!
//! Merge rules:
//! - Dependency libraries (`src/lib.aero`) may only contain declarations (functions, types,
//!   constants, traits, impls, modules, imports); executable statements are rejected
//!   (libraries have no side effects at load time);
//! - The root package `src/main.aero` statements are appended last (the main block);

use aero_parse::ast::Stmt;

use crate::graph::{resolve_with_lock, load_manifest, CrateSource, PmError};

/// Resolve the tree, persist `Aero.lock`, and log a friendly summary line.
fn resolve_and_lock(root: &std::path::Path) -> Result<Vec<CrateSource>, PmError> {
    let res = resolve_with_lock(root)?;
    if !res.lock.entries.is_empty() {
        res.lock.save(root)?;
    }
    Ok(res.crates)
}

/// Merge the dependency tree into a single source text.
pub fn merge_source(crates: &[CrateSource]) -> Result<String, PmError> {
    let mut out = String::new();
    for cs in crates {
        if cs.is_root {
            continue;
        }
        if let Some(lib) = &cs.lib_source {
            reject_top_level_stmts(cs, lib)?;
            out.push_str(&format!("// ===== dependency library: {} =====\n", cs.name));
            out.push_str(lib);
            if !lib.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    let root = crates.iter().find(|c| c.is_root).ok_or_else(|| {
        PmError::new("dependency tree lacks the root package".to_string())
    })?;
    let main = root.main_source.as_ref().ok_or_else(|| {
        PmError::new(format!("root package {} lacks src/main.aero", root.name))
    })?;
    out.push_str(&format!("// ===== root package: {} (main) =====\n", root.name));
    out.push_str(main);
    Ok(out)
}

/// Library files may only contain function definitions: any non-`FnDef` top-level statement is an error.
fn reject_top_level_stmts(cs: &CrateSource, lib: &str) -> Result<(), PmError> {
    let program = aero_parse::parse_source(lib).map_err(|e| {
        PmError::new(format!("library {} syntax error: {}:{}: {}", cs.name, e.line, e.col, e.msg))
    })?;
    for stmt in &program.stmts {
        // Libraries may contain any declaration with no load-time side effects:
        // functions, types, constants, traits, impls, modules and imports.
        // Executable statements (`let`, `print`, expressions, ...) are rejected.
        match stmt {
            Stmt::FnDef { .. }
            | Stmt::StructDef { .. }
            | Stmt::UnionDef { .. }
            | Stmt::ConstDef { .. }
            | Stmt::EnumDef { .. }
            | Stmt::TraitDef { .. }
            | Stmt::ImplBlock { .. }
            | Stmt::ModDef { .. }
            | Stmt::UseDecl { .. }
            | Stmt::Pub(..) => continue,
            _ => {}
        }
        let sp = stmt.span();
        return Err(PmError::new(format!(
            "library {} may only contain declarations; non-declaration statement at line {}",
            cs.name, sp.line
        )));
    }
    Ok(())
}

pub fn build_source(root: &std::path::Path) -> Result<String, PmError> {
    let crates = resolve_and_lock(root)?;
    merge_source(&crates)
}

/// Resolve from the root directory and merge; returns the merged source.
pub fn build_package(root: &std::path::Path) -> Result<(), PmError> {
    let merged = build_source(root)?;
    aero_ir::check_source(&merged).map_err(|e| PmError::new(e.to_string()))
}

/// Collect the deduplicated `[link]` config (libs + lib paths) across every crate
/// in the resolved dependency tree.
fn collect_link_config(crates: &[CrateSource]) -> Result<(Vec<String>, Vec<String>), PmError> {
    let mut libs: Vec<String> = Vec::new();
    let mut lib_paths: Vec<String> = Vec::new();
    for cs in crates {
        let m = load_manifest(&cs.root)?;
        for l in &m.link_libs {
            if !libs.contains(l) {
                libs.push(l.clone());
            }
        }
        for p in &m.link_paths {
            if !lib_paths.contains(p) {
                lib_paths.push(p.clone());
            }
        }
    }
    Ok((libs, lib_paths))
}

/// Build the package: resolve deps -> merge -> full compile check (LLVM IR + verify), no execution.
/// Compile the package to a standalone executable (AOT): resolve -> merge -> LLVM IR -> link.
pub fn compile_package(root: &std::path::Path, out_exe: &std::path::Path, target: &str) -> Result<(), PmError> {
    let crates = resolve_and_lock(root)?;
    let merged = merge_source(&crates)?;
    let (libs, lib_paths) = collect_link_config(&crates)?;
    aero_ir::aot::compile_to_exe_linked(&merged, out_exe, &libs, &lib_paths, aero_ir::aot::OptLevel::default(), target)
        .map_err(|e| PmError::new(e.to_string()))
}

/// Link config (the `[link]` section) merges declarations from the root package and all deps.
pub fn run_package(root: &std::path::Path) -> Result<(), PmError> {
    let crates = resolve_and_lock(root)?;
    let merged = merge_source(&crates)?;
    let (libs, lib_paths) = collect_link_config(&crates)?;
    if libs.is_empty() && lib_paths.is_empty() {
        // Pure package: run via the in-process JIT.
        return aero_ir::run_source(&merged).map_err(|e| PmError::new(e.to_string()));
    }
    // FFI package: the JIT cannot resolve external C library symbols (OpenSSL,
    // Winsock2, libcurl, ...) at runtime, so AOT-compile to an exe (same output
    // location as `aero build`) and run it as a child process instead.
    let root_manifest = load_manifest(root)?;
    let out_dir = root.join("target").join("aero");
    std::fs::create_dir_all(&out_dir).map_err(|e| {
        PmError::new(format!("cannot create output directory {}: {e}", out_dir.display()))
    })?;
    let exe = out_dir.join(format!("{}.exe", root_manifest.name));
    aero_ir::aot::compile_to_exe_linked(
        &merged,
        &exe,
        &libs,
        &lib_paths,
        aero_ir::aot::OptLevel::default(),
        aero_ir::aot::host_target_triple(),
    )
    .map_err(|e| PmError::new(e.to_string()))?;
    let status = std::process::Command::new(&exe)
        .status()
        .map_err(|e| PmError::new(format!("failed to run {}: {e}", exe.display())))?;
    if !status.success() {
        return Err(PmError::new(format!(
            "program {} exited with {}",
            exe.display(),
            status
                .code()
                .map(|c| format!("code {c}"))
                .unwrap_or_else(|| "a signal".to_string())
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aero_pm_build_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_pkg(dir: &std::path::Path, name: &str, deps: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut toml = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
        if !deps.is_empty() {
            toml.push_str("\n[dependencies]\n");
            for (n, p) in deps {
                toml.push_str(&format!("{n} = {{ path = \"{p}\" }}\n"));
            }
        }
        std::fs::write(dir.join("Aero.toml"), toml).unwrap();
        std::fs::create_dir_all(&dir.join("src")).unwrap();
        if name.starts_with("lib") {
            std::fs::write(dir.join("src").join("lib.aero"), "fn lib_fn() -> i64 { return 1; }\n").unwrap();
        } else {
            std::fs::write(dir.join("src").join("main.aero"), "print(1);\n").unwrap();
        }
    }

    #[test]
    fn merge_orders_deps_before_root() {
        let d = tmpdir("merge");
        write_pkg(&d.join("liba"), "liba", &[]);
        write_pkg(&d.join("app"), "app", &[("liba", "../liba")]);
        let merged = build_source(&d.join("app")).unwrap();
        assert!(merged.contains("dependency library: liba"));
        assert!(merged.find("liba").unwrap() < merged.find("app").unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn lib_with_top_level_stmt_rejected() {
        let d = tmpdir("badlib");
        write_pkg(&d.join("liba"), "liba", &[]);
        // Overwrite lib.aero with a top-level statement
        std::fs::write(
            d.join("liba").join("src").join("lib.aero"),
            "fn lib_fn() -> i64 { return 1; }\nprint(1);\n",
        )
        .unwrap();
        write_pkg(&d.join("app"), "app", &[("liba", "../liba")]);
        let err = build_source(&d.join("app")).unwrap_err();
        assert!(err.msg.contains("non-declaration statement"), "got: {}", err.msg);
        let _ = std::fs::remove_dir_all(&d);
    }
}
