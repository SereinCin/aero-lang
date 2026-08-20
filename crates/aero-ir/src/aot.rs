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
/// Cross-compile backends (aarch64/arm/x86) initialized once each.
static INIT_AARCH64: Once = Once::new();
static INIT_ARM: Once = Once::new();
static INIT_X86: Once = Once::new();

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

/// The CPU arch component of a target triple (the leading `arch` field), used to
/// pick the matching LLVM backend for cross-compilation. The vendor/OS/abi fields
/// do not affect which LLVM backend is required (e.g. `aarch64-linux-android`
/// and `aarch64-unknown-linux-gnu` both use the AArch64 backend).
fn triple_arch(triple: &str) -> &str {
    triple.split('-').next().unwrap_or("")
}

/// Initialize the LLVM backend required to emit/object-link a module for
/// `triple`. The host backend is always initialized; cross targets (Android
/// `aarch64`/`arm`/`i686`) are registered lazily and once. LLVM requires
/// the backend to be registered before `Target::from_triple` succeeds — without
/// this, `aero build --target aarch64-linux-android` fails with "No available
/// targets are compatible with this triple".
///
/// The host arch is deliberately skipped: `initialize_native` already registered
/// it, and re-initializing e.g. the x86 backend explicitly crashes Builder-built
/// modules on the LLVM 22.1.8 Windows static libs (0xC0000005, see top-of-file).
fn init_target_for(triple: &str) {
    init_target();
    if triple_is_host_arch(triple) {
        return;
    }
    match triple_arch(triple) {
        "aarch64" => INIT_AARCH64.call_once(|| {
            let _ = Target::initialize_aarch64(&InitializationConfig::default());
        }),
        "arm" | "armv7" => INIT_ARM.call_once(|| {
            let _ = Target::initialize_arm(&InitializationConfig::default());
        }),
        "i686" | "i386" | "x86_64" => INIT_X86.call_once(|| {
            // The `x86` backend covers both x86 (i386/i686) and x86_64 targets.
            let _ = Target::initialize_x86(&InitializationConfig::default());
        }),
        _ => {}
    }
}

/// Whether `triple` targets the same CPU arch as the build host. When
/// cross-compiling, the host CPU name/features must not be passed to the target
/// machine (an x86_64 host CPU string is invalid for an aarch64 target), so the
/// emitter falls back to generic ("") CPU/features for foreign archs.
///
/// iOS triples are always treated as cross targets even when the arch matches
/// the host (e.g. an Apple Silicon Mac building `aarch64-apple-ios`): iOS
/// device/simulator codegen must not inherit desktop-Mac CPU features.
fn triple_is_host_arch(triple: &str) -> bool {
    if triple.contains("ios") {
        return false;
    }
    triple_arch(triple) == std::env::consts::ARCH
}

/// Whether `triple` targets Darwin (macOS/iOS). Darwin emits Mach-O object
/// files, whose symbol naming, shared-library flag (`-dynamiclib`) and runtime
/// (`libSystem`) all differ from COFF/ELF.
fn is_macho_triple(triple: &str) -> bool {
    triple.contains("darwin") || triple.contains("apple")
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
    init_target_for(triple_str);
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] init_target_for done");
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
    // Mach-O symbol naming: LLVM prepends an underscore to every exported
    // global on Darwin. The codegen emits the Windows CRT name `_snprintf`;
    // on Mach-O that would become `__snprintf`, but libSystem only exports
    // `_snprintf` (the C name `snprintf` + prefix). Rename it to `snprintf` so
    // the emitted Mach-O symbol is `_snprintf`, resolving against libSystem.
    if is_macho_triple(triple_str) {
        if let Some(f) = module.get_function("_snprintf") {
            f.as_global_value().set_name("snprintf");
        }
    }
    // Only pass host CPU/features when the triple targets the host arch; a foreign
    // arch (e.g. aarch64 from an x86_64 host) has no valid host-CPU mapping, so
    // LLVM falls back to the target's generic CPU ("") for codegen.
    //
    // NOTE: get_host_cpu_name/features return owned LLVMStrings whose Drop calls
    // LLVMDisposeMessage. Dropping them on the LLVM 22.1.8 Windows static libs
    // crashes (0xC0000005, same class as the module/context disposal bug), so they
    // must be kept alive for the whole function and leaked, as the original code did.
    let (cpu, features) = if triple_is_host_arch(triple_str) {
        let host_cpu = TargetMachine::get_host_cpu_name();
        let host_features = TargetMachine::get_host_cpu_features();
        let cpu = host_cpu.to_string();
        let features = host_features.to_string();
        std::mem::forget(host_cpu);
        std::mem::forget(host_features);
        (cpu, features)
    } else {
        (String::new(), String::new())
    };
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] cpu/features done");
    }
    let tm = target
        .create_target_machine(
            &triple,
            &cpu,
            &features,
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
///
/// Platform adjustments driven by `target`:
/// - COFF (Windows): add `-Wl,--stack,67108864` (large main-thread stack reserve).
/// - ELF (Linux/Android): the codegen emits the Windows CRT name `_snprintf`;
///   bionic/glibc export `snprintf`, so `--defsym=_snprintf=snprintf` aliases it.
fn link(
    obj_path: &Path,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    shared: bool,
    extra_args: &[String],
    target: &str,
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
        if is_macho_triple(target) {
            // Darwin/clang uses `-dynamiclib` for shared libraries; `-shared`
            // is a GNU-ld flag that clang rejects on Apple platforms.
            cmd.arg("-dynamiclib");
        } else {
            cmd.arg("-shared");
        }
    }
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.arg("-o").arg(out_path);
    let coff = target.contains("windows");
    if coff {
        // Give the executable a large main-thread stack reserve. By default MinGW/LLD
        // links with a ~1MB stack, so deep recursion (>~50k frames) overflows and
        // aborts the whole process. Bumping this to 64MB makes high-intensity
        // recursive code robust without changing the language semantics.
        cmd.arg("-Wl,--stack,67108864");
    } else if (target.contains("linux") || target.contains("android"))
        && !is_macho_triple(target)
    {
        // The string-runtime calls `_snprintf` (the Windows CRT export name). On
        // ELF targets the symbol is `snprintf`; alias it so Android/Linux .so and
        // executable links resolve (GNU ld / lld both support --defsym). Mach-O
        // is excluded: `_snprintf` is renamed to `snprintf` in emit_object, so
        // ld64/lld resolve `_snprintf` against libSystem directly.
        cmd.arg("-Wl,--defsym=_snprintf=snprintf");
    }
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
    compile_to_out(source, exe_path, libs, lib_paths, opt, target, false, true, None, &[])
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
    compile_to_out(source, out_path, libs, lib_paths, opt, target, true, false, None, extra_args)
}

/// AOT compilation to a Python C extension (`.pyd` on Windows / `.so` on Unix).
///
/// Same shared-library pipeline as [`compile_to_shared`], plus the CPython glue
/// (wrappers + method table + `PyInit_<module>`) for every `#[py_export]`
/// function. `spec.module` must match the output file stem (`<name>.pyd` →
/// `PyInit_<name>`). The linker resolves the CPython API against the import
/// library passed via `libs`/`lib_paths` (e.g. `python313.lib`).
pub fn compile_to_pyext(
    source: &str,
    out_path: &Path,
    libs: &[String],
    lib_paths: &[String],
    opt: OptLevel,
    target: &str,
    spec: &crate::PyExtSpec,
) -> Result<(), AeroError> {
    compile_to_out(
        source,
        out_path,
        libs,
        lib_paths,
        opt,
        target,
        true,
        false,
        Some(spec),
        &[],
    )
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
    py_ext: Option<&crate::PyExtSpec>,
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
    let module = crate::compile_pipeline_emit(&context, source, emit_main, py_ext)?;
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
    // The intermediate output keeps the *final file name* so that the emitted
    // artifact records the right identity: gcc/clang bake the `-o` file name
    // into the output (a PE DLL's internal name, an ELF soname / Mach-O
    // install_name). Using `aero_out.lib` here made the DLL identify itself as
    // `aero_out.lib` even after being copied to `cpp_bind.dll`, so the loader
    // could not find it at runtime (0xC0000135). Reuse `out_path`'s file name
    // (the temp *directory* still keeps the link in ASCII space).
    let out_name = out_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| if shared { "aero_out.lib" } else { "aero_out.exe" }.to_string());
    let out_tmp = tmp_dir.join(&out_name);
    let emit_result = emit_object(&module, &obj_path, opt, target);
    // LLVM 22.1.8 official Windows static-lib bug: after target init, Builder-built
    // module/Context disposal (LLVMDisposeModule / LLVMContextDispose) crashes
    // (0xC0000005; probes show IR-parsed modules are unaffected). The object file is written,
    // so module/context are done — leak them and let process exit reclaim, avoiding the crash.
    std::mem::forget(module);
    std::mem::forget(context);
    emit_result?;
    link(&obj_path, &out_tmp, libs, lib_paths, shared, extra_args, target)?;
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

    #[test]
    fn triple_arch_parses_android_abis() {
        assert_eq!(triple_arch("aarch64-linux-android"), "aarch64");
        assert_eq!(triple_arch("armv7-linux-androideabi"), "armv7");
        assert_eq!(triple_arch("arm-linux-androideabi"), "arm");
        assert_eq!(triple_arch("x86_64-linux-android"), "x86_64");
        assert_eq!(triple_arch("i686-linux-android"), "i686");
        assert_eq!(triple_arch("x86_64-pc-windows-gnu"), "x86_64");
    }

    /// Cross-compiling to an Android AArch64 target must produce an ELF object
    /// file with the exported symbol visible — the core of `aero build --shared
    /// --target aarch64-linux-android`. This runs the full pipeline (source →
    /// IR → object) without needing an NDK installed (only linking requires it).
    #[test]
    fn emits_android_aarch64_object_with_export() {
        let src = "#[export]\nfn add(a: i64, b: i64) -> i64 { return a + b; }\n#[export]\nfn double(x: f64) -> f64 { return x * 2.0; }\nprint(\"x\");\n";
        let triples = ["aarch64-linux-android", "armv7-linux-androideabi", "x86_64-linux-android", "i686-linux-android"];
        for t in triples {
            let context = Context::create();
            let module = crate::compile_pipeline_emit(&context, src, false, None).expect("pipeline");
            let tmp = std::env::temp_dir().join(format!("aero_android_{}_test.o", t.replace('-', "_")));
            let ok = emit_object(&module, &tmp, OptLevel::O2, t);
            assert!(ok.is_ok(), "{t} object emit failed: {:?}", ok.err());
            // Every Android target must yield a genuine ELF with the exports.
            let bytes = std::fs::read(&tmp).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp);
            assert!(
                bytes.len() > 4 && &bytes[0..4] == b"\x7fELF",
                "{t} object is not ELF ({} bytes)",
                bytes.len()
            );
            let has_add = bytes.windows(4).any(|w| w == b"add\0" || w == b"add");
            let has_double = bytes.windows(7).any(|w| w == b"double\0");
            assert!(has_add, "{t}: exported symbol `add` missing from object strtab");
            assert!(has_double, "{t}: exported symbol `double` missing from object strtab");
            let _ = module;
        }
    }

    /// M5: cross-compiling to an iOS target must produce a 64-bit little-endian
    /// Mach-O object (magic `0xfeedfacf` → bytes `cf fa ed fe`) with the exported
    /// symbol visible, and the string-runtime `_snprintf` must be renamed to
    /// `snprintf` (Mach-O prepends `_`, so `__snprintf` would not resolve against
    /// libSystem's `_snprintf`). No Xcode/macOS needed — only object emission.
    #[test]
    fn emits_ios_macho_object_with_export() {
        let src = "#[export]\nfn add(a: i64, b: i64) -> i64 { return a + b; }\n#[export]\nfn double(x: f64) -> f64 { return x * 2.0; }\nprint(\"x\");\n";
        let triples = [
            "aarch64-apple-ios",
            "x86_64-apple-ios",
            "aarch64-apple-ios-sim",
        ];
        for t in triples {
            let context = Context::create();
            let module = crate::compile_pipeline_emit(&context, src, false, None).expect("pipeline");
            let tmp = std::env::temp_dir().join(format!("aero_ios_{}_test.o", t.replace('-', "_")));
            let ok = emit_object(&module, &tmp, OptLevel::O2, t);
            assert!(ok.is_ok(), "{t} object emit failed: {:?}", ok.err());
            let bytes = std::fs::read(&tmp).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp);
            // 64-bit little-endian Mach-O magic
            assert!(
                bytes.len() > 4 && &bytes[0..4] == b"\xcf\xfa\xed\xfe",
                "{t} object is not 64-bit little-endian Mach-O ({} bytes)",
                bytes.len()
            );
            let has_add = bytes.windows(4).any(|w| w == b"add\0");
            let has_double = bytes.windows(7).any(|w| w == b"double\0");
            assert!(has_add, "{t}: exported symbol `add` missing from Mach-O strtab");
            assert!(has_double, "{t}: exported symbol `double` missing from Mach-O strtab");
            // The runtime `_snprintf` must have been renamed to `snprintf`: the
            // Mach-O backend prepends `_`, so the emitted symbol is `_snprintf`
            // (matching libSystem). A double underscore (`__snprintf`, the name
            // the Windows-CRT `_snprintf` would get) must NOT appear.
            let has_double_underscored = bytes.windows(11).any(|w| w == b"__snprintf\0");
            assert!(!has_double_underscored, "{t}: `_snprintf` was not renamed to `snprintf` (found `__snprintf`)");
            let _ = module;
        }
    }

    /// M2: a `#[py_export]` function with a `String` (bytes) parameter/return must
    /// emit the CPython glue: the `y#` ParseTuple format unit, the
    /// `PyBytes_FromStringAndSize` builder call, and the `PyInit_<name>` entry.
    /// This exercises the full glue path without needing a Python install.
    #[test]
    fn py_export_bytes_glue_is_emitted() {
        init_target();
        let context = Context::create();
        let src = "#[py_export]\nfn bytes_len(b: String) -> i64 { return b.len(); }\n#[py_export]\nfn bytes_echo(b: String) -> String { return b; }\n";
        let spec = crate::PyExtSpec {
            module: "pyb",
            api_version: 1013,
            windows: true,
        };
        let module = crate::compile_pipeline_emit(&context, src, false, Some(&spec)).expect("pipeline");
        let llvm_ir = module.print_to_string();
        let ir = llvm_ir.to_string_lossy().into_owned();
        // Keep the LLVMString alive: dropping it calls LLVMDisposeMessage, which
        // crashes (0xC0000005) on the LLVM 22.1.8 Windows static libs.
        std::mem::forget(llvm_ir);
        assert!(ir.contains("PyBytes_FromStringAndSize"), "missing PyBytes glue:\n{ir}");
        assert!(ir.contains("PyInit_pyb"), "missing PyInit entry:\n{ir}");
        // The "y#" ParseTuple unit is baked into the per-wrapper format strings.
        assert!(ir.contains("y#"), "missing y# bytes format:\n{ir}");
        let _ = module;
    }
}
