//! AOT compilation: LLVM IR → object file (.obj) → system linker → standalone executable.
//!
//! Aero's first step toward self-reproduction: no longer depending on JIT or the aero
//! toolchain — a standalone native binary is produced.
//!
//! Linker selection: `gcc` by default (MinGW UCRT64, handles CRT startup, default
//! libs and the console subsystem); override with the `AERO_LINKER` env var.
//!
//! Note: with the official LLVM 22.1.8 Windows static libs, explicit `initialize_x86`
//! initialization crashes Builder-built modules at write_to_file/disposal (0xC0000005;
//! probes confirm the initialize_native path is fine), so we always use host-native init.

use std::path::Path;
use std::process::Command;
use std::sync::Once;

use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;

use crate::AeroError;

/// Host backend initialized once (LLVM global state).
static INIT_TARGET: Once = Once::new();

/// Optimization level for AOT native codegen. Maps to an LLVM pass pipeline
/// (`default<O0>`–`default<O3>`) and the target machine's `OptimizationLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
}

impl Default for OptLevel {
    fn default() -> Self {
        OptLevel::O2
    }
}

impl OptLevel {
    /// The LLVM pass pipeline string consumed by `module.run_passes(...)`.
    pub fn pass_string(self) -> &'static str {
        match self {
            OptLevel::O0 => "default<O0>",
            OptLevel::O1 => "default<O1>",
            OptLevel::O2 => "default<O2>",
            OptLevel::O3 => "default<O3>",
        }
    }

    /// The LLVM `OptimizationLevel` used when creating the target machine.
    pub(crate) fn inkwell(self) -> OptimizationLevel {
        match self {
            OptLevel::O0 => OptimizationLevel::None,
            OptLevel::O1 => OptimizationLevel::Less,
            OptLevel::O2 => OptimizationLevel::Default,
            OptLevel::O3 => OptimizationLevel::Aggressive,
        }
    }
}

fn init_target() {
    INIT_TARGET.call_once(|| {
        // The JIT path (create_jit_execution_engine) also initializes natively
        // internally and does not trigger the LLVM 22.1.8 Windows static-lib crash.
        let _ = Target::initialize_native(&InitializationConfig::default());
    });
}

/// Host target triple (detected from compile-time constants).
///
/// The GNU-vs-MSVC and apple-vs-Linux vendor strings matter: the object file
/// format (COFF/ELF/Mach-O) and the system linker both key off this. Known
/// combos are spelled out explicitly; anything unexpected falls back to a
/// best-effort `arch-vendor-os` triple.
pub fn host_target_triple() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "windows") => "x86_64-pc-windows-gnu",
        ("aarch64", "windows") => "aarch64-pc-windows-gnu",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        (arch, os) => {
            // Fallback: construct a reasonable triple from the host constants.
            // This covers other platforms (e.g. i686, arm, freebsd) at the cost
            // of a less-common vendor string.
            let vendor = match os {
                "linux" => "unknown",
                "windows" => "pc",
                "macos" => "apple",
                _ => "unknown",
            };
            // Leak a Box for the static lifetime; called once so it's fine.
            Box::leak(format!("{arch}-{vendor}-{os}").into_boxed_str())
        }
    }
}

/// Write a compiled module to a COFF/ELF/Mach-O object file.
fn emit_object(module: &Module, obj_path: &Path, opt: OptLevel, triple_str: &str) -> Result<(), AeroError> {
    init_target();
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] init_target done");
    }
    let triple = TargetTriple::create(triple_str);
    let target = Target::from_triple(&triple).map_err(|e| AeroError {
        phase: "AOT",
        line: 0,
        col: 0,
        msg: format!("cannot get target backend {triple}: {e}"),
    })?;
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] from_triple done");
    }
    let cpu = TargetMachine::get_host_cpu_name();
    let features = TargetMachine::get_host_cpu_features();
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] cpu/features done");
    }
    let tm = target
        .create_target_machine(
            &triple,
            &cpu.to_string(),
            &features.to_string(),
            opt.inkwell(),
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| AeroError {
            phase: "AOT",
            line: 0,
            col: 0,
            msg: "failed to create TargetMachine".to_string(),
        })?;
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] target machine created");
    }
    // Run LLVM optimization passes (default<O2>): codegen emits unoptimized IR (every
    // variable lives in memory); without this, AOT binaries run ~15-25x slower than
    // optimized C/Rust. run_passes is stable on the LLVM 22.1.8 Windows static libs
    // (the LLVMVerifyModule crash does not affect it).
    //
    // LTO: Aero compiles the whole program to a single module already, so cross-CU
    // inlining is implicit. AERO_LTO=1 turns on extra whole-program global passes
    // (GlobalOpt, IPSCCP, ConstantMerge) on top of the baseline pipeline, tightening
    // interprocedural optimization for release builds.
    let mut pipeline = opt.pass_string().to_string();
    if opt != OptLevel::O0 && std::env::var("AERO_LTO").map(|v| v == "1").unwrap_or(false) {
        pipeline = format!("{pipeline},globalopt,ipsccp,constmerge");
        eprintln!("[aot-dbg] LTO pipeline: {pipeline}");
    }
    if let Err(e) = module.run_passes(&pipeline, &tm, PassBuilderOptions::create()) {
        let s = e.to_string();
        std::mem::forget(e);
        return Err(AeroError {
            phase: "AOT",
            line: 0,
            col: 0,
            msg: format!("LLVM optimization failed: {s}"),
        });
    }
    let write_result = tm.write_to_file(module, FileType::Object, obj_path);
    // LLVM 22.1.8 Windows static-lib bug: after target initialization, disposing LLVM
    // objects (LLVMDisposeTargetMachine / LLVMDisposeMessage) crashes (0xC0000005).
    // The object file is already written (or errored); leak these objects so no path disposes them.
    std::mem::forget(tm);
    std::mem::forget(cpu);
    std::mem::forget(features);
    std::mem::forget(triple);
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] write_to_file done");
    }
    write_result.map_err(|e| {
        let s = e.to_string();
        std::mem::forget(e);
        AeroError {
            phase: "AOT",
            line: 0,
            col: 0,
            msg: format!("failed to generate object file: {s}"),
        }
    })?;
    Ok(())
}

/// Link an object file into an executable (or shared library) with the system linker.
///
/// The linker is chosen via the `AERO_LINKER` env var, defaulting to `gcc`.
/// gcc automatically brings the CRT startup objects, default libs (UCRT) and
/// the console subsystem, so a COFF object file containing `main` is enough.
/// `shared=true` adds `-shared` (dynamic-library output: `.so`/`.dll`/`.dylib`);
/// `extra_args` are appended verbatim (used for cross-toolchains, e.g. NDK).
fn link(
    obj_path: &Path,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    shared: bool,
    extra_args: &[String],
) -> Result<(), AeroError> {
    let linker = std::env::var("AERO_LINKER").unwrap_or_else(|_| "gcc".to_string());
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] link: spawning `{linker}`");
    }
    let mut cmd = Command::new(&linker);
    cmd.arg(obj_path);
    for lp in lib_paths {
        cmd.arg("-L").arg(lp);
    }
    for l in libs {
        cmd.arg(format!("-l{l}"));
    }
    if shared {
        cmd.arg("-shared");
    }
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg("-o").arg(out_path);
    // Give the executable a large main-thread stack reserve. By default MinGW/LLD
    // links with a ~1MB stack, so deep recursion (>~50k frames) overflows and
    // aborts the whole process. Bumping this to 64MB makes high-intensity
    // recursive code robust without changing the language semantics.
    cmd.arg("-Wl,--stack,67108864");
    let out = cmd
        .output()
        .map_err(|e| AeroError {
            phase: "link",
            line: 0,
            col: 0,
            msg: format!(
                "cannot start linker `{linker}`: {e} (set the AERO_LINKER env var to point at a linker)"
            ),
        })?;
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] link: process returned, status={}", out.status);
    }
    if !out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(AeroError {
            phase: "link",
            line: 0,
            col: 0,
            msg: format!("link failed ({linker}):\n{stdout}{stderr}"),
        });
    }
    Ok(())
}

/// RAII guard that removes the temp directory on any exit path.
struct TmpDirCleanup(std::path::PathBuf);

impl Drop for TmpDirCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One-stop: source → LLVM IR → .obj → link → standalone executable.
///
/// Both the object file and the intermediate exe live in an ASCII temp directory
/// (msys gcc cannot handle non-ASCII paths); copied back on success.
pub fn compile_to_exe(source: &str, exe_path: &Path, target: &str) -> Result<(), AeroError> {
    compile_to_exe_linked(source, exe_path, &[], &[], OptLevel::default(), target)
}

/// AOT compilation with link config (FFI): extra libs and search paths from the `[link]` section.
pub fn compile_to_exe_linked(
    source: &str,
    exe_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
) -> Result<(), AeroError> {
    compile_to_out(source, exe_path, libs, lib_paths, opt, target, false, true, &[])
}

/// AOT compilation to a shared library (`-shared` output: `.so`/`.dll`/`.dylib`).
///
/// `#[export]` functions become visible C-ABI symbols in the dynamic symbol table;
/// the top-level `main` is kept but hidden (see [`crate::compile_pipeline_emit`]).
/// `extra_args` are appended to the linker command (cross-toolchains, e.g. NDK).
pub fn compile_to_shared(
    source: &str,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
    extra_args: &[String],
) -> Result<(), AeroError> {
    compile_to_out(source, out_path, libs, lib_paths, opt, target, true, false, extra_args)
}

/// Shared "IR → obj → link" pipeline used by both executable and shared-library builds.
#[allow(clippy::too_many_arguments)]
fn compile_to_out(
    source: &str,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
    shared: bool,
    emit_main: bool,
    extra_args: &[String],
) -> Result<(), AeroError> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AeroError {
            phase: "AOT",
            line: 0,
            col: 0,
            msg: format!("cannot create output directory {}: {e}", parent.display()),
        })?;
    }
    let context = Context::create();
    let module = crate::compile_pipeline_emit(&context, source, emit_main)?;
    if std::env::var("AERO_DUMP_IR").is_ok() {
        let s = module.print_to_string();
        println!("{s}");
        std::mem::forget(s);
    }
    // Intermediate artifacts go to an ASCII temp dir: msys gcc cannot handle non-ASCII
    // paths (e.g. Chinese dirs garble), so copy back after a successful link.
    let tmp_dir = std::env::temp_dir().join(format!("aero_link_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).map_err(|e| AeroError {
        phase: "AOT",
        line: 0,
        col: 0,
        msg: format!("cannot create temp directory {}: {e}", tmp_dir.display()),
    })?;
    let _cleanup = TmpDirCleanup(tmp_dir.clone());
    let obj_path = tmp_dir.join("aero_out.obj");
    let out_tmp = tmp_dir.join(if shared { "aero_out.lib" } else { "aero_out.exe" });
    let emit_result = emit_object(&module, &obj_path, opt, target);
    // LLVM 22.1.8 official Windows static-lib bug: after target init, Builder-built
    // module/Context disposal (LLVMDisposeModule / LLVMContextDispose) crashes
    // (0xC0000005; probes show IR-parsed modules are unaffected). The object file is written,
    // so module/context are done — leak them and let process exit reclaim, avoiding the crash.
    std::mem::forget(module);
    std::mem::forget(context);
    emit_result?;
    link(&obj_path, &out_tmp, libs, lib_paths, shared, extra_args)?;
    // Copy from the ASCII temp dir back to the target path (supports non-ASCII paths)
    std::fs::copy(&out_tmp, out_path).map_err(|e| AeroError {
        phase: "AOT",
        line: 0,
        col: 0,
        msg: format!("cannot write output file {}: {e}", out_path.display()),
    })?;
    Ok(())
}

/// Compile a single file to an executable (convenience entry; output at exe_path).
pub fn compile_file_to_exe(
    source_file: &Path,
    exe_path: &Path,
    opt: OptLevel,
    target: &str,
) -> Result<(), AeroError> {
    let source = std::fs::read_to_string(source_file).map_err(|e| AeroError {
        phase: "IO",
        line: 0,
        col: 0,
        msg: format!("cannot read file {}: {e}", source_file.display()),
    })?;
    compile_to_exe_linked(&source, exe_path, &[], &[], opt, target)
}

/// Content-addressable build cache key for a compilation config.
fn cache_key(source: &str, libs: &[String], lib_paths: &[String], opt: OptLevel, target: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    libs.hash(&mut hasher);
    lib_paths.hash(&mut hasher);
    opt.pass_string().hash(&mut hasher);
    target.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Incremental-compilation cache for `aero build <file> <target_dir>`.
///
/// Computes a content hash of (source, link config, opt level). If a previously
/// built exe with the same hash exists in the cache dir, it is copied to the
/// target without re-running LLVM codegen/optimization/link — turning a no-op
/// rebuild into a fast file copy. `env` (e.g. AERO_LTO) is a caller-provided
/// context string folded into the key so caching never leaks across configs.
pub fn compile_to_exe_cached(
    source: &str,
    exe_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    env: &[(&str, &str)],
    target: &str,
) -> Result<bool, AeroError> {
    // Cache dir lives next to the output target (Cargo-style). Fall back to the
    // current directory when the target has no parent.
    let base = exe_path.parent().map(Path::to_path_buf).unwrap_or_else(|| std::path::PathBuf::from("."));
    let cache_dir = base.join(".aero-cache");
    // Salt the key with AERO_LTO/AERO_DUMP_IR so cache-dodging switches never reuse a stale exe.
    let mut ctx = String::new();
    for (k, v) in env {
        ctx.push_str(k);
        ctx.push('=');
        ctx.push_str(v);
        ctx.push('\n');
    }
    let key = format!("{}_{}", cache_key(source, libs, lib_paths, opt, target), {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        use std::hash::Hasher;
        h.write(ctx.as_bytes());
        format!("{:016x}", h.finish())
    });
    let cached = cache_dir.join(format!("{key}.exe"));

    // Hit: copy the cached exe to the target and report a cache hit.
    if cached.is_file() {
        std::fs::copy(&cached, exe_path).map_err(|e| AeroError {
            phase: "AOT",
            line: 0,
            col: 0,
            msg: format!("cannot copy cached exe to {}: {e}", exe_path.display()),
        })?;
        return Ok(true);
    }

    // Miss: build into the cache dir, then copy out.
    std::fs::create_dir_all(&cache_dir).map_err(|e| AeroError {
        phase: "AOT",
        line: 0,
        col: 0,
        msg: format!("cannot create cache dir {}: {e}", cache_dir.display()),
    })?;
    compile_to_exe_linked(source, &cached, libs, lib_paths, opt, target)?;
    std::fs::copy(&cached, exe_path).map_err(|e| AeroError {
        phase: "AOT",
        line: 0,
        col: 0,
        msg: format!("cannot copy built exe to {}: {e}", exe_path.display()),
    })?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_object_requires_valid_source() {
        init_target();
        let context = Context::create();
        // Invalid source should error in compile_pipeline (with a phase name), not panic
        let err = crate::compile_pipeline(&context, "let x = ;").unwrap_err();
        assert!(!err.msg.is_empty());
    }
}
