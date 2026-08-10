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

fn init_target() {
    INIT_TARGET.call_once(|| {
        // The JIT path (create_jit_execution_engine) also initializes natively
        // internally and does not trigger the LLVM 22.1.8 Windows static-lib crash.
        let _ = Target::initialize_native(&InitializationConfig::default());
    });
}

/// Write a compiled module to a COFF object file.
fn emit_object(module: &Module, obj_path: &Path) -> Result<(), AeroError> {
    init_target();
    if std::env::var("AERO_AOT_DEBUG").is_ok() {
        eprintln!("[aot-dbg] init_target done");
    }
    let triple = TargetTriple::create("x86_64-pc-windows-gnu");
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
            OptimizationLevel::None,
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
    if let Err(e) = module.run_passes("default<O2>", &tm, PassBuilderOptions::create()) {
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

/// Link an object file into an executable with the system linker.
///
/// The linker is chosen via the `AERO_LINKER` env var, defaulting to `gcc`.
/// gcc automatically brings the CRT startup objects, default libs (UCRT) and
/// the console subsystem, so a COFF object file containing `main` is enough.
fn link(obj_path: &Path, exe_path: &Path, libs: &[String], lib_paths: &[String]) -> Result<(), AeroError> {
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
    cmd.arg("-o").arg(exe_path);
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
pub fn compile_to_exe(source: &str, exe_path: &Path) -> Result<(), AeroError> {
    compile_to_exe_linked(source, exe_path, &[], &[])
}

/// AOT compilation with link config (FFI): extra libs and search paths from the `[link]` section.
pub fn compile_to_exe_linked(
    source: &str,
    exe_path: &Path,
    libs: &[String],
    lib_paths: &[String],
) -> Result<(), AeroError> {
    if let Some(parent) = exe_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AeroError {
            phase: "AOT",
            line: 0,
            col: 0,
            msg: format!("cannot create output directory {}: {e}", parent.display()),
        })?;
    }
    let context = Context::create();
    let module = crate::compile_pipeline(&context, source)?;
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
    let exe_tmp = tmp_dir.join("aero_out.exe");
    let emit_result = emit_object(&module, &obj_path);
    // LLVM 22.1.8 official Windows static-lib bug: after target init, Builder-built
    // module/Context disposal (LLVMDisposeModule / LLVMContextDispose) crashes
    // (0xC0000005; probes show IR-parsed modules are unaffected). The object file is written,
    // so module/context are done — leak them and let process exit reclaim, avoiding the crash.
    std::mem::forget(module);
    std::mem::forget(context);
    emit_result?;
    link(&obj_path, &exe_tmp, libs, lib_paths)?;
    // Copy from the ASCII temp dir back to the target path (supports non-ASCII paths)
    std::fs::copy(&exe_tmp, exe_path).map_err(|e| AeroError {
        phase: "AOT",
        line: 0,
        col: 0,
        msg: format!("cannot write output file {}: {e}", exe_path.display()),
    })?;
    Ok(())
}

/// Compile a single file to an executable (convenience entry; output at exe_path).
pub fn compile_file_to_exe(source_file: &Path, exe_path: &Path) -> Result<(), AeroError> {
    let source = std::fs::read_to_string(source_file).map_err(|e| AeroError {
        phase: "IO",
        line: 0,
        col: 0,
        msg: format!("cannot read file {}: {e}", source_file.display()),
    })?;
    compile_to_exe(&source, exe_path)
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
