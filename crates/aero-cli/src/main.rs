mod diag;
mod lsp;

use std::path::Path;
use std::process::Command;

const USAGE: &str = "\
Usage:
  aero run <file.aero | package dir>   compile and run
  aero run <file.aero> -O0|-O1|-O2|-O3  run with a specific optimization level
  aero build [file.aero | dir]        compile to a standalone exe (AOT)
  aero build <file.aero> -O0|-O1|-O2|-O3  build with a specific optimization level
  aero build <file> --target <triple>  cross-compile for a target (e.g. x86_64-unknown-linux-gnu)
  aero build <file> --shared [--target <triple>]  build a shared library (.so/.dll/.dylib)
  aero build <file> --shared --target aarch64-linux-android [--ndk <path>]  build an Android .so
  aero build <file> --shared --target aarch64-apple-ios  build an iOS .dylib (macOS + Xcode only)
  aero build <file> --pyext [--py-module <name>] [--py-home <prefix>]  build a Python C extension
  aero build <file> --cpp            build a C++ binding: shared library + <name>.hpp header
  aero new <name>                    create a new package skeleton
  aero check <file.aero | dir>       compile-check (parse/type/borrow/LLVM verify), no run
  aero test [file.aero]              run tests (default: all in tests/)
  aero fmt <file.aero>            format a source file in place
  aero fmt --check <file.aero>    check formatting without writing
  aero fmt <f> --width 100 --indent 2   set max width / indent (rustfmt-style)
  aero bench <file.aero>             run benchmarks (bench_ functions) with timing
  aero bench <file> --iterations 1e7 --samples 3   tune iterations / samples
  aero clippy <file.aero>            run the static linter (100+ rules)
  aero cov <file.aero>               compile with coverage instrumentation, run,
                                  then print a statement-coverage report
  aero --lsp | aero lsp             run the LSP server (stdin/stdout JSON-RPC 2.0;
                                  diagnostics, completion, go-to-definition, hover)
  aero lock [dir]                  resolve deps and (re)generate Aero.lock
  aero install [name]              install a package from the GitHub ecosystem
                                 (no arg: interactive picker; <name>: install that package)
  aero publish <dir>               publish a library package to the registry
  aero ls                          list packages/versions in the registry
  aero registry                    print the registry location";

/// Split an optional trailing `-O<n>` optimization flag from a path argument.
/// Returns `(path, opt_level)`; opt defaults to O2.
fn parse_opt_flag(arg: &str) -> (&str, aero_ir::aot::OptLevel) {
    // Find the last whitespace-separated token; if it looks like `-O<n>`, use it.
    let trimmed = arg.trim_end();
    if let Some(idx) = trimmed.rfind(' ') {
        let (head, tail) = trimmed.split_at(idx + 1);
        if let Some(opt) = match_opt_flag(tail) {
            return (head.trim_end(), opt);
        }
    }
    if let Some(opt) = match_opt_flag(trimmed) {
        return ("", opt);
    }
    (trimmed, aero_ir::aot::OptLevel::default())
}

/// Map a `-O<n>` token to an `OptLevel`; returns None if `s` is not a valid flag.
fn match_opt_flag(s: &str) -> Option<aero_ir::aot::OptLevel> {
    match s {
        "-O0" => Some(aero_ir::aot::OptLevel::O0),
        "-O1" => Some(aero_ir::aot::OptLevel::O1),
        "-O2" => Some(aero_ir::aot::OptLevel::O2),
        "-O3" => Some(aero_ir::aot::OptLevel::O3),
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("run") => {
            let joined = args[2..].join(" ");
            if joined.trim().is_empty() {
                eprintln!("missing argument.\n{USAGE}");
                2
            } else {
                cmd_run(&joined)
            }
        }
        Some("build") => {
            let joined = args[2..].join(" ");
            if joined.trim().is_empty() {
                cmd_build(".")
            } else {
                cmd_build(&joined)
            }
        }
        Some("new") => match args.get(2) {
            Some(name) => cmd_new(name),
            None => {
                eprintln!("missing package name.\naero new <name>");
                2
            }
        },
        Some("check") => match args.get(2) {
            Some(target) => cmd_check(target),
            None => {
                eprintln!("missing target.\naero check <file.aero | dir>");
                2
            }
        },
        Some("test") => match args.get(2) {
            Some(file) => cmd_test_file(file),
            None => cmd_test_dir("tests"),
        },
        Some("bench") => {
            let joined = args[2..].join(" ");
            if joined.trim().is_empty() {
                eprintln!("missing file.\naero bench <file.aero> [--iterations N] [--samples N]");
                2
            } else {
                cmd_bench(&joined)
            }
        }
        Some("fmt") => {
            let joined = args[2..].join(" ");
            if joined.trim().is_empty() {
                eprintln!("missing file.\naero fmt <file.aero> [--check] [--width N] [--indent N]");
                2
            } else {
                cmd_fmt(&joined)
            }
        }
        Some("--lsp") | Some("-lsp") | Some("lsp") => lsp::run_lsp(),
        Some("clippy") => match args.get(2) {
            Some(file) => cmd_clippy(file),
            None => {
                eprintln!("missing file.\naero clippy <file.aero>");
                2
            }
        },
        Some("cov") => match args.get(2) {
            Some(file) => cmd_cov(file),
            None => {
                eprintln!("missing file.\naero cov <file.aero>");
                2
            }
        },
        Some("lock") => cmd_lock(args.get(2).map(String::as_str)),
        Some("install") => cmd_install(args.get(2).map(String::as_str)),
        Some("publish") => match args.get(2) {
            Some(dir) => cmd_publish(dir),
            None => {
                eprintln!("missing dir.\naero publish <dir>");
                2
            }
        },
        Some("ls") => cmd_ls(),
        Some("registry") => cmd_registry(),
        Some(other) => {
            eprintln!("unknown command: {other}\n{USAGE}");
            2
        }
        None => {
            eprintln!("{USAGE}");
            2
        }
    };
    // The official LLVM 22.1.8 Windows static libs crash when the process exits normally
    // and global LLVM state is disposed (0xC0000005 / 0xC0000374; reproduced by probes).
    // Terminate via the exit code to skip disposal; stdout is line-buffered and flushed.
    std::process::exit(i32::from(code));
}

/// `aero run <file|dir> [-O<n>] [--target <triple>]`: files take the single-file pipeline; directories (or those with Aero.toml) use the package flow.
fn cmd_run(arg: &str) -> u8 {
    let flags = parse_build_flags(arg);
    let p = Path::new(flags.path);
    if p.is_dir() || p.join("Aero.toml").exists() {
        match aero_pm::run_package(p) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }
    } else {
        run_file(flags.path, flags.opt, flags.triple)
    }
}

/// `aero build [file.aero | dir] [-O0|-O1|-O2|-O3] [--target <triple>] [--shared] [--pyext ...]`:
/// AOT-compile to a standalone executable, a shared library (`--shared`), or a
/// Python C extension (`--pyext`, `.pyd`/`.so` with CPython glue).
/// - Package dir: outputs to `<pkg root>/target/aero/<pkg name>.exe`;
/// - Single file: outputs to `<file name>.exe` (or `.so`/`.dll`/`.dylib` with `--shared`).
fn cmd_build(arg: &str) -> u8 {
    // Peel an optional `--target <triple>`, `--shared`/`--pyext`, `--py-module`
    // and `-O<n>` flags.
    let flags = parse_build_flags(arg);
    let p = Path::new(flags.path);
    if p.is_file() {
        let source = match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                let err = aero_ir::AeroError {
                    phase: "IO",
                    line: 0,
                    col: 0,
                    msg: format!("cannot read file {}: {e}", p.display()),
                };
                eprint!("{}", diag::render_error("", flags.path, &err));
                return 1;
            }
        };
        if flags.pyext {
            return cmd_pyext(p, &source, &flags);
        }
        if flags.cpp {
            return cmd_cpp(p, &source, &flags);
        }
        if flags.shared {
            if flags.triple.contains("android") {
                return cmd_android_shared(p, &source, &flags);
            }
            // Only `ios` triples take the Xcode toolchain path; a macOS host
            // triple (`*-apple-darwin`) still uses the plain shared build below.
            if flags.triple.contains("ios") {
                return cmd_ios_shared(p, &source, &flags);
            }
            let ext = shared_ext(flags.triple);
            let out = p.with_extension(ext);
            let extra: Vec<String> = Vec::new();
            return match aero_ir::aot::compile_to_shared(&source, &out, &[], &[], flags.opt, flags.triple, &extra) {
                Ok(()) => {
                    println!("{}", out.display());
                    0
                }
                Err(e) => {
                    eprint!("{}", diag::render_error(&source, flags.path, &e));
                    1
                }
            };
        }
        let out = p.with_extension("exe");
        // Fold config-affecting env into the cache key (own value with its own
        // key) so a stale exe is never reused across AERO_LTO/AERO_DUMP_IR switches.
        let env_ctx: Vec<(&str, String)> = ["AERO_LTO", "AERO_DUMP_IR", "AERO_DEBUG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (*k, v)))
            .collect();
        let env_refs: Vec<(&str, &str)> = env_ctx
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        return match aero_ir::aot::compile_to_exe_cached(&source, &out, &[], &[], flags.opt, &env_refs, flags.triple) {
            Ok(hit) => {
                println!(
                    "{}",
                    out.display(),
                );
                let _ = hit;
                0
            }
            Err(e) => {
                eprint!("{}", diag::render_error(&source, flags.path, &e));
                1
            }
        };
    }
    if flags.shared || flags.pyext {
        eprintln!("`--shared`/`--pyext` is only supported for single files in this version");
        return 2;
    }
    let manifest = match aero_pm::graph::load_manifest(p) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("build failed: {e}");
            return 1;
        }
    };
    let out_dir = p.join("target").join("aero");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create output directory {}: {e}", out_dir.display());
        return 1;
    }
    let exe = out_dir.join(format!("{}.exe", manifest.name));
    match aero_pm::build::compile_package(p, &exe, flags.triple) {
        Ok(()) => {
            println!("build succeeded: {}", exe.display());
            0
        }
        Err(e) => {
            eprintln!("build failed: {e}");
            1
        }
    }
}

/// Parsed `aero build`/`aero run` flags (path, triple, opt level, mode).
struct BuildFlags<'a> {
    path: &'a str,
    triple: &'a str,
    opt: aero_ir::aot::OptLevel,
    /// `--shared`: build a shared library (`.so`/`.dll`/`.dylib`)
    shared: bool,
    /// `--pyext`: build a Python C extension (`.pyd`/`.so`) with CPython glue
    pyext: bool,
    /// `--cpp`: build a C++ binding — shared library + `<name>.hpp` header
    /// declaring every `#[export]` function as `extern "C"`
    cpp: bool,
    /// `--py-module <name>`: Python module name (defaults to the file stem)
    py_module: Option<&'a str>,
    /// `--py-home <prefix>`: Python install prefix (defaults to `PYTHON_HOME` /
    /// probing the `python` executable)
    py_home: Option<&'a str>,
    /// `--ndk <path>`: Android NDK root (defaults to `ANDROID_NDK_HOME` /
    /// probing default install locations)
    ndk: Option<&'a str>,
}

/// Parse `--target <triple>`, `--shared`, `--pyext`, `--py-module`,
/// `--py-home`, `--ndk` and `-O<n>` flags from a build/run argument string.
/// The triple defaults to the host triple when not specified.
fn parse_build_flags(arg: &str) -> BuildFlags<'_> {
    let mut triple = aero_ir::aot::host_target_triple();
    let mut opt = aero_ir::aot::OptLevel::default();
    let mut shared = false;
    let mut pyext = false;
    let mut cpp = false;
    let mut py_module: Option<&str> = None;
    let mut py_home: Option<&str> = None;
    let mut ndk: Option<&str> = None;
    let mut path = "";
    let toks: Vec<&str> = arg.split_whitespace().collect();
    let mut i = 0usize;
    while i < toks.len() {
        match toks[i] {
            "--target" => {
                i += 1;
                if i < toks.len() {
                    triple = toks[i];
                }
            }
            "--shared" => shared = true,
            "--pyext" => pyext = true,
            "--cpp" => cpp = true,
            "--py-module" => {
                i += 1;
                if i < toks.len() {
                    py_module = Some(toks[i]);
                }
            }
            "--py-home" => {
                i += 1;
                if i < toks.len() {
                    py_home = Some(toks[i]);
                }
            }
            "--ndk" => {
                i += 1;
                if i < toks.len() {
                    ndk = Some(toks[i]);
                }
            }
            "-O0" => opt = aero_ir::aot::OptLevel::O0,
            "-O1" => opt = aero_ir::aot::OptLevel::O1,
            "-O2" => opt = aero_ir::aot::OptLevel::O2,
            "-O3" => opt = aero_ir::aot::OptLevel::O3,
            other => {
                if path.is_empty() {
                    path = other;
                }
            }
        }
        i += 1;
    }
    BuildFlags {
        path,
        triple,
        opt,
        shared,
        pyext,
        cpp,
        py_module,
        py_home,
        ndk,
    }
}

/// Shared-library file extension (without dot) for a target triple (`dll`/`so`/`dylib`).
fn shared_ext(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "dll"
    } else if triple.contains("darwin") {
        "dylib"
    } else {
        "so"
    }
}

/// Resolved Android NDK: root dir + the LLVM prebuilt toolchain path.
struct NdkEnv {
    root: std::path::PathBuf,
    prebuilt: std::path::PathBuf,
}

/// Locate the Android NDK: `--ndk <path>` first, then `ANDROID_NDK_HOME`,
/// then probe common install locations (`%LOCALAPPDATA%\Android\Sdk\ndk`,
/// `~/Android/Sdk/ndk`, `C:\Android\Sdk\ndk`, …). Returns the newest NDK when a
/// directory contains multiple version folders.
fn find_ndk(flag: Option<&str>) -> Option<NdkEnv> {
    // Candidate NDK roots: explicit flag, env, then default install dirs.
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = flag {
        candidates.push(std::path::PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("ANDROID_NDK_HOME") {
        candidates.push(std::path::PathBuf::from(p));
    }
    if let Ok(h) = std::env::var("ANDROID_HOME") {
        candidates.push(std::path::Path::new(&h).join("ndk"));
    }
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        candidates.push(std::path::Path::new(&l).join("Android").join("Sdk").join("ndk"));
    }
    if let Ok(h) = std::env::var("USERPROFILE") {
        candidates.push(std::path::Path::new(&h).join("Android").join("Sdk").join("ndk"));
    }
    if let Ok(h) = std::env::var("HOME") {
        candidates.push(std::path::Path::new(&h).join("Android").join("Sdk").join("ndk"));
        candidates.push(std::path::Path::new(&h).join("android-ndk"));
    }
    for base in &candidates {
        if let Some(prebuilt) = resolve_ndk_prebuilt(base) {
            return Some(NdkEnv {
                root: base.clone(),
                prebuilt,
            });
        }
        // base may be a parent containing multiple NDK versions (e.g. <sdk>/ndk/).
        if let Ok(rd) = std::fs::read_dir(base) {
            let mut versions: Vec<std::path::PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| resolve_ndk_prebuilt(p).is_some())
                .collect();
            versions.sort();
            if let Some(ndk) = versions.pop() {
                return Some(NdkEnv {
                    root: ndk.clone(),
                    prebuilt: resolve_ndk_prebuilt(&ndk).expect("checked above"),
                });
            }
        }
    }
    None
}

/// Find the LLVM prebuilt toolchain inside an NDK root
/// (`toolchains/llvm/prebuilt/<host>`); returns None if `root` is not an NDK.
fn resolve_ndk_prebuilt(root: &Path) -> Option<std::path::PathBuf> {
    let host = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("macos", "aarch64") => "darwin-arm64",
        _ => return None,
    };
    let prebuilt = root.join("toolchains").join("llvm").join("prebuilt").join(host);
    if prebuilt.join("bin").join(clang_bin_name()).is_file() {
        Some(prebuilt)
    } else {
        None
    }
}

/// NDK clang driver binary name (`.exe` on Windows).
fn clang_bin_name() -> &'static str {
    if std::env::consts::OS == "windows" {
        "clang.exe"
    } else {
        "clang"
    }
}

/// Map an Android target triple to the NDK clang `--target` flag (the ABI part
/// of the triple, e.g. `aarch64-linux-android` → `aarch64-linux-android`).
fn android_abi(triple: &str) -> &str {
    if triple.starts_with("armv7") {
        "armv7a-linux-androideabi"
    } else if triple.starts_with("arm") {
        "armv7a-linux-androideabi"
    } else if triple.starts_with("x86_64") {
        "x86_64-linux-android"
    } else if triple.starts_with("i686") || triple.starts_with("i386") {
        "i686-linux-android"
    } else {
        "aarch64-linux-android"
    }
}

/// `aero build --shared --target <android-triple> [--ndk <path>] <file>`:
/// cross-compile an Aero file whose `#[export]` functions become visible
/// C-ABI symbols in a shared `lib<name>.so` for Android, linking with the NDK
/// clang + sysroot. This is what a host App links against via JNI / FFI.
fn cmd_android_shared(p: &Path, source: &str, flags: &BuildFlags<'_>) -> u8 {
    let ndk = match find_ndk(flags.ndk) {
        Some(n) => n,
        None => {
            eprintln!(
                "cannot locate Android NDK; pass --ndk <path> or set ANDROID_NDK_HOME"
            );
            return 1;
        }
    };
    let linker = ndk.prebuilt.join("bin").join(clang_bin_name());
    let sysroot = ndk.prebuilt.join("sysroot");
    let abi = android_abi(flags.triple);
    // Android 21 (Android 5.0) is the minimum API the NDK still supports for
    // 64-bit targets; codegen uses bionic libc calls only, no version gate.
    let api = 21;
    let extra = vec![
        format!("--target={abi}{api}"),
        format!("--sysroot={}", sysroot.display()),
        // bionic requires position-independent code for shared libraries
        "-fPIC".to_string(),
        // Android logging API (used by aero-rt/__android_log_print)
        "-llog".to_string(),
    ];
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aero".to_string());
    // Android convention: shared libs are named lib<name>.so.
    let out = p.with_file_name(format!("lib{stem}.so"));
    // Route the NDK clang through AERO_LINKER (aot::link honours it).
    unsafe {
        std::env::set_var("AERO_LINKER", &linker);
    }
    match aero_ir::aot::compile_to_shared(source, &out, &[], &[], flags.opt, flags.triple, &extra) {
        Ok(()) => {
            println!("{}", out.display());
            0
        }
        Err(e) => {
            eprint!("{}", diag::render_error(source, flags.path, &e));
            1
        }
    }
}

/// Map an iOS target triple to the clang `-arch` value.
fn ios_arch(triple: &str) -> &'static str {
    if triple.starts_with("x86_64") {
        "x86_64"
    } else {
        "arm64"
    }
}

/// Resolve the Xcode toolchain for a given iOS SDK (`iphoneos` for devices,
/// `iphonesimulator` for the simulator): the clang binary and the SDK sysroot.
/// Returns `(clang, sysroot)`.
fn find_xcode(sdk: &str) -> Option<(String, String)> {
    let clang = Command::new("xcrun")
        .args(["--sdk", sdk, "-f", "clang"])
        .output()
        .ok()?;
    if !clang.status.success() {
        return None;
    }
    let clang = String::from_utf8_lossy(&clang.stdout).trim().to_string();
    let sysroot = Command::new("xcrun")
        .args(["--sdk", sdk, "--show-sdk-path"])
        .output()
        .ok()?;
    if !sysroot.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&sysroot.stdout).trim().to_string();
    if clang.is_empty() || sysroot.is_empty() {
        return None;
    }
    Some((clang, sysroot))
}

/// `aero build --shared --target <ios-triple> <file>`: cross-compile an Aero file
/// whose `#[export]` functions become C-ABI symbols in a `lib<name>.dylib` for
/// iOS, using the Xcode toolchain (xcrun + SDK sysroot). Requires macOS with
/// Xcode installed — this is only exercisable on CI / a Mac.
fn cmd_ios_shared(p: &Path, source: &str, flags: &BuildFlags<'_>) -> u8 {
    // Device triples (aarch64-apple-ios) use the iphoneos SDK; simulator
    // triples (*-apple-ios-sim, x86_64-apple-ios) use iphonesimulator.
    let sdk = if flags.triple.contains("-sim") || flags.triple.starts_with("x86_64") {
        "iphonesimulator"
    } else {
        "iphoneos"
    };
    let (clang, sysroot) = match find_xcode(sdk) {
        Some(t) => t,
        None => {
            eprintln!(
                "cannot locate Xcode toolchain for SDK `{sdk}`; iOS cross-compilation requires macOS with Xcode (xcrun --sdk {sdk} --show-sdk-path)"
            );
            return 1;
        }
    };
    let arch = ios_arch(flags.triple);
    let extra = vec![
        format!("-arch {arch}"),
        format!("-isysroot {sysroot}"),
        "-fPIC".to_string(),
        // iOS 13 is a safe deployment floor for both devices and the simulator.
        "-mios-version-min=13.0".to_string(),
    ];
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aero".to_string());
    // Darwin convention: dynamic libraries are named lib<name>.dylib.
    let out = p.with_file_name(format!("lib{stem}.dylib"));
    // Route the Xcode clang through AERO_LINKER (aot::link honours it).
    unsafe {
        std::env::set_var("AERO_LINKER", &clang);
    }
    match aero_ir::aot::compile_to_shared(source, &out, &[], &[], flags.opt, flags.triple, &extra) {
        Ok(()) => {
            println!("{}", out.display());
            0
        }
        Err(e) => {
            eprint!("{}", diag::render_error(source, flags.path, &e));
            1
        }
    }
}

/// `aero build --cpp <file>`: build a C++ binding for an Aero file. Produces a
/// shared library (`<name>.dll`/`.so`/`.dylib`) containing every `#[export]`
/// function as a visible C-ABI symbol, plus a `<name>.hpp` header declaring
/// them `extern "C"` — a C++ project `#include`s the header, links the library
/// and calls the Aero functions directly. Android/iOS targets reuse the same
/// cross toolchains as `--shared`.
fn cmd_cpp(p: &Path, source: &str, flags: &BuildFlags<'_>) -> u8 {
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "aero".to_string());
    // 1. Collect exported signatures and write the C++ header.
    let exports = match aero_ir::cpp::collect_cpp_exports(source) {
        Ok(e) => e,
        Err(e) => {
            eprint!("{}", diag::render_error(source, flags.path, &e));
            return 1;
        }
    };
    let header = aero_ir::cpp::cpp_header(&exports, &stem);
    let hpp = p.with_extension("hpp");
    if let Err(e) = std::fs::write(&hpp, header) {
        eprintln!("cannot write header {}: {e}", hpp.display());
        return 1;
    }
    // 2. Build the shared library. Route cross targets (Android/iOS) through
    // their toolchains, exactly like `--shared`.
    if flags.triple.contains("android") {
        let code = cmd_android_shared(p, source, flags);
        if code == 0 {
            println!("{}", hpp.display());
        }
        return code;
    }
    if flags.triple.contains("ios") {
        let code = cmd_ios_shared(p, source, flags);
        if code == 0 {
            println!("{}", hpp.display());
        }
        return code;
    }
    let ext = shared_ext(flags.triple);
    let out = p.with_extension(ext);
    let extra: Vec<String> = Vec::new();
    match aero_ir::aot::compile_to_shared(source, &out, &[], &[], flags.opt, flags.triple, &extra) {
        Ok(()) => {
            println!("{}", out.display());
            println!("{}", hpp.display());
            0
        }
        Err(e) => {
            eprint!("{}", diag::render_error(source, flags.path, &e));
            1
        }
    }
}

/// Detected Python installation (prefix + version).
struct PyEnv {
    prefix: std::path::PathBuf,
    major: u32,
    minor: u32,
}

/// `aero build --pyext <file>`: compile an Aero file whose `#[py_export]`
/// functions get auto-generated CPython glue into a CPython extension module
/// (`<name>.pyd` on Windows, `<name>.so` elsewhere), linking the Python import
/// library.
fn cmd_pyext(p: &Path, source: &str, flags: &BuildFlags<'_>) -> u8 {
    // Module name: --py-module, else the file stem. Must match the file name
    // (`<name>.pyd` ↔ `PyInit_<name>` for `import <name>`).
    let module_name = flags
        .py_module
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            p.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "aero".to_string())
        });
    if !valid_pkg_name(&module_name) {
        eprintln!("invalid Python module name: `{module_name}`");
        return 2;
    }
    let pyenv = match find_python(flags.py_home) {
        Some(e) => e,
        None => {
            eprintln!("cannot locate Python; set --py-home <prefix> or PYTHON_HOME");
            return 1;
        }
    };
    let (libs, lib_paths) = python_link_libs(&pyenv, flags.triple);
    let ext = if flags.triple.contains("windows") {
        "pyd"
    } else {
        "so"
    };
    let out = p.with_file_name(format!("{module_name}.{ext}"));
    // PYTHON_API_VERSION = 1000 + minor (CPython modsupport.h; e.g. 3.13 → 1013).
    let api_version = 1000 + pyenv.minor;
    let spec = aero_ir::PyExtSpec {
        module: &module_name,
        api_version,
        windows: flags.triple.contains("windows"),
    };
    match aero_ir::aot::compile_to_pyext(
        source,
        &out,
        &libs,
        &lib_paths,
        flags.opt,
        flags.triple,
        &spec,
    ) {
        Ok(()) => {
            println!("{}", out.display());
            0
        }
        Err(e) => {
            eprint!("{}", diag::render_error(source, flags.path, &e));
            1
        }
    }
}

/// Locate a Python installation: `--py-home <prefix>` / `PYTHON_HOME` first,
/// else probe the `python` executable for its prefix and version.
fn find_python(py_home: Option<&str>) -> Option<PyEnv> {
    if let Some(h) = py_home.or(std::env::var("PYTHON_HOME").ok().as_deref()) {
        let prefix = Path::new(h);
        if let Some((major, minor)) = detect_version_in_prefix(prefix) {
            return Some(PyEnv {
                prefix: prefix.to_path_buf(),
                major,
                minor,
            });
        }
        // Fall back to probing a `python` executable inside the prefix.
        let exe = prefix.join(if std::env::consts::OS == "windows" {
            "python.exe"
        } else {
            "bin/python3"
        });
        if let Some(e) = probe_python_exec(exe.to_str()?) {
            return Some(e);
        }
    }
    probe_python_exec(if std::env::consts::OS == "windows" {
        "python"
    } else {
        "python3"
    })
}

/// Detect the Python (major, minor) version from the import libraries in a
/// prefix: `libs/python313.lib` (Windows) or `lib/libpython3.13.*` (Unix).
fn detect_version_in_prefix(prefix: &Path) -> Option<(u32, u32)> {
    for dir in ["libs", "lib"] {
        let d = prefix.join(dir);
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(v) = parse_python_lib_name(&name) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Parse a Python import-library name into `(major, minor)`:
/// `python313.lib`, `python3.13.lib`, `libpython3.13.so`, `libpython3.13.dylib`.
fn parse_python_lib_name(name: &str) -> Option<(u32, u32)> {
    let s = name.to_ascii_lowercase();
    let nums = if let Some(rest) = s.strip_prefix("libpython") {
        rest.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>()
    } else if let Some(rest) = s.strip_prefix("python") {
        rest.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>()
    } else {
        return None;
    };
    let mut parts = nums.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    if let Some(minor_s) = parts.next() {
        if !minor_s.is_empty() {
            return Some((major, minor_s.parse().ok()?));
        }
    }
    // `python313.lib`: first char is the major, the rest the minor.
    if nums.len() >= 2 {
        let major = nums.chars().next()?.to_digit(10)?;
        let minor: u32 = nums[1..].parse().ok()?;
        return Some((major, minor));
    }
    None
}

/// Query a Python executable for its prefix + version via `python -c`.
fn probe_python_exec(exe: &str) -> Option<PyEnv> {
    let out = std::process::Command::new(exe)
        .args([
            "-c",
            "import sys; print(sys.prefix); print(sys.version_info.major); print(sys.version_info.minor)",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let prefix = std::path::PathBuf::from(lines.next()?.trim());
    let major: u32 = lines.next()?.trim().parse().ok()?;
    let minor: u32 = lines.next()?.trim().parse().ok()?;
    Some(PyEnv { prefix, major, minor })
}

/// Link configuration for a Python extension: `-L <prefix>/libs -l python313`
/// (Windows) or `-L <prefix>/lib -l python3.13` (Unix). On Windows the import
/// library is a MinGW-compatible one generated from the DLL (see
/// [`ensure_mingw_python_import_lib`]), since MinGW ld cannot consume the MSVC
/// `python313.lib` shipped with the official python.org builds.
fn python_link_libs(env: &PyEnv, triple: &str) -> (Vec<String>, Vec<String>) {
    if triple.contains("windows") {
        let lib = format!("python{}{}", env.major, env.minor);
        let dir = ensure_mingw_python_import_lib(env);
        (vec![lib], vec![dir.to_string_lossy().into_owned()])
    } else {
        let lib = format!("python{}.{}", env.major, env.minor);
        (
            vec![lib],
            vec![env.prefix.join("lib").to_string_lossy().into_owned()],
        )
    }
}

/// Generate a MinGW-compatible import library (`libpython3xx.a`) for the given
/// Python DLL, cached under `<TEMP>/aero_pyimport/<dll-key>`. The official
/// python.org Windows builds ship an MSVC `.lib` that MinGW's ld rejects, so we
/// list the DLL exports with `objdump -p` and rebuild the import library with
/// `dlltool`. The cache key folds the DLL path, size and mtime, so the import
/// library is only regenerated when the interpreter changes.
fn ensure_mingw_python_import_lib(env: &PyEnv) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let dll = env.prefix.join(format!("python{}{}.dll", env.major, env.minor));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dll.to_string_lossy().hash(&mut hasher);
    if let Ok(md) = std::fs::metadata(&dll) {
        md.len().hash(&mut hasher);
        if let Ok(t) = md.modified() {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                .hash(&mut hasher);
        }
    }
    let dir = std::env::temp_dir().join("aero_pyimport").join(format!("{:016x}", hasher.finish()));
    let out_lib = dir.join(format!("libpython{}{}.a", env.major, env.minor));
    if out_lib.exists() {
        return dir;
    }
    let names = std::process::Command::new("objdump")
        .arg("-p")
        .arg(&dll)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let toks: Vec<&str> = l.trim().split_whitespace().collect();
                    // Export name-pointer rows look like "[   0] +base[   1]
                    // 0000 PY_TIMEOUT_MAX". The ordinal brackets are space-padded
                    // (< 1000), so the token count varies; the name is always the
                    // LAST token with a hex hint field right before it. Address
                    // rows ("... 004adaf8 Export RVA") are skipped because their
                    // second-to-last token is not hex.
                    if toks.len() < 3 || !toks[0].starts_with('[') || !l.contains("+base[") {
                        return None;
                    }
                    let prev = toks[toks.len() - 2];
                    let name = toks[toks.len() - 1];
                    if prev.len() < 1 || !prev.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return None;
                    }
                    if name.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return None;
                    }
                    Some(name.to_string())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if names.is_empty() {
        // Fall back to the MSVC libs dir (linking will fail with a clear error).
        return env.prefix.join("libs");
    }
    let _ = std::fs::create_dir_all(&dir);
    let def = dir.join("python.def");
    let mut def_text = format!("LIBRARY python{}{}\nEXPORTS\n", env.major, env.minor);
    for n in &names {
        def_text.push_str(&format!("  {n}\n"));
    }
    if std::fs::write(&def, def_text).is_err() {
        return env.prefix.join("libs");
    }
    let _ = std::process::Command::new("dlltool")
        .args([
            "--dllname",
            dll.file_name().and_then(|s| s.to_str()).unwrap_or_default(),
            "--input-def",
            def.to_str().unwrap_or_default(),
            "--output-lib",
            out_lib.to_str().unwrap_or_default(),
        ])
        .output();
    dir
}

/// `aero new <name>`: generate a package skeleton.
fn cmd_new(name: &str) -> u8 {
    if !valid_pkg_name(name) {
        eprintln!("invalid package name: `{name}` (letters/digits/underscore only, must not start with a digit)");
        return 2;
    }
    let root = Path::new(name);
    if root.exists() {
        eprintln!("directory already exists: {name}");
        return 2;
    }
    let src = root.join("src");
    if std::fs::create_dir_all(&src).is_err() {
        eprintln!("cannot create directory {name}/src");
        return 1;
    }
    let toml = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.1\"\nedition = \"2024\"\n"
    );
    let main = "print(\"Hello, Aero!\\n\");\n";
    if std::fs::write(root.join("Aero.toml"), toml).is_err()
        || std::fs::write(src.join("main.aero"), main).is_err()
    {
        eprintln!("failed to write skeleton files");
        return 1;
    }
    println!("created package {name} (Aero.toml + src/main.aero)");
    0
}

/// `aero check <file.aero | dir>`: compile-check only (parse -> type -> borrow
/// -> LLVM IR verify), without running.
///
/// - 单个 `.aero` 文件：直接检查该文件；
/// - 包根（有 `Aero.toml` 且含 `src/main.aero`）：解析整棵依赖树后整体检查；
/// - 库 crate（有 `Aero.toml` 但只有 `src/lib.aero`）：检查其库源文件。
fn cmd_check(target: &str) -> u8 {
    let p = Path::new(target);
    // 目录或包根：走 aero-pm 的 package 流程（解析依赖树再整体检查）
    if p.is_dir() || p.join("Aero.toml").exists() {
        // 纯库 crate（无 src/main.aero）：检查 src/lib.aero
        if !p.join("src").join("main.aero").exists() {
            let lib = p.join("src").join("lib.aero");
            if lib.is_file() {
                return check_file(&lib);
            }
        }
        return match aero_pm::build_package(p) {
            Ok(()) => {
                println!("check passed: {}", p.display());
                0
            }
            Err(e) => {
                eprintln!("check failed: {e}");
                1
            }
        };
    }
    check_file(p)
}

/// 检查单个 `.aero` 源文件（编译检查，不执行）。
fn check_file(p: &Path) -> u8 {
    let target = p.to_string_lossy();
    let source = match std::fs::read_to_string(p) {
        Ok(s) => s,
        Err(e) => {
            let err = aero_ir::AeroError {
                phase: "IO",
                line: 0,
                col: 0,
                msg: format!("cannot read file {}: {e}", p.display()),
            };
            eprint!("{}", diag::render_error("", &target, &err));
            return 1;
        }
    };
    match aero_ir::check_source(&source) {
        Ok(()) => {
            println!("check passed: {}", p.display());
            0
        }
        Err(e) => {
            eprint!("{}", diag::render_error(&source, &target, &e));
            1
        }
    }
}

/// `aero test <file>`: run the tests in a single file.
fn cmd_test_file(file: &str) -> u8 {
    let source = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {file}: {e}");
            return 1;
        }
    };
    match aero_pm::run_tests_source(&source) {
        Ok(report) => {
            print_test_report(&report);
            if report.passed == report.total {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("tests failed: {e}");
            1
        }
    }
}

/// `aero bench <file.aero> [--iterations N] [--samples N]`
///
/// Run every `bench_` function in the file. Each case is AOT-compiled once,
/// then executed `samples` times; the fastest run is reported as ns/op and op/s.
fn cmd_bench(arg: &str) -> u8 {
    let mut file: Option<String> = None;
    let mut cfg = aero_pm::BenchConfig::default();
    let toks: Vec<&str> = arg.split_whitespace().collect();
    let mut i = 0usize;
    let mut opt_err: Option<&str> = None;
    while i < toks.len() {
        match toks[i] {
            "--iterations" => match toks.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                Some(n) if n >= 1 => {
                    cfg.iterations = n;
                    i += 1;
                }
                _ => opt_err = Some("--iterations needs a positive integer"),
            },
            "--samples" => match toks.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                Some(n) if n >= 1 => {
                    cfg.samples = n;
                    i += 1;
                }
                _ => opt_err = Some("--samples needs a positive integer"),
            },
            other => {
                if file.is_none() {
                    file = Some(other.to_string());
                }
            }
        }
        if opt_err.is_some() {
            break;
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("aero bench expects exactly one source file.");
        return 2;
    };
    if let Some(e) = opt_err {
        eprintln!("{e}");
        return 2;
    }
    let source = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {file}: {e}");
            return 1;
        }
    };
    match aero_pm::run_bench_source(&source, &cfg) {
        Ok(report) => {
            print!("{}", report.text());
            0
        }
        Err(e) => {
            eprintln!("bench: {e}");
            1
        }
    }
}

/// `aero test`: run all .aero files under tests/.
fn cmd_test_dir(dir: &str) -> u8 {
    let d = Path::new(dir);
    if !d.is_dir() {
        eprintln!("test directory `{dir}` not found");
        return 2;
    }
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "aero").unwrap_or(false) {
                files.push(p);
            }
        }
    }
    files.sort();
    if files.is_empty() {
        eprintln!("no .aero test files in `{dir}`");
        return 2;
    }
    let mut all_ok = true;
    let mut total = 0usize;
    let mut passed = 0usize;
    for f in &files {
        let label = f.display().to_string();
        println!("=== {label} ===");
        match std::fs::read_to_string(f) {
            Ok(src) => match aero_pm::run_tests_source(&src) {
                Ok(report) => {
                    total += report.total;
                    passed += report.passed;
                    all_ok &= report.passed == report.total;
                }
                Err(e) => {
                    eprintln!("{label}: {e}");
                    all_ok = false;
                }
            },
            Err(e) => {
                eprintln!("cannot read {label}: {e}");
                all_ok = false;
            }
        }
    }
    println!("\n{total} tests: {passed} passed, {} failed", total - passed);
    if all_ok {
        0
    } else {
        1
    }
}

fn print_test_report(report: &aero_pm::TestReport) {
    for r in &report.results {
        if r.passed {
            println!("PASS  {}", r.name);
        } else {
            println!("FAIL  {}", r.name);
            if !r.output.is_empty() {
                for line in r.output.lines() {
                    println!("      | {line}");
                }
            }
        }
    }
    println!("{}", report.summary());
}

fn run_file(path: &str, opt: aero_ir::aot::OptLevel, _target: &str) -> u8 {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            let err = aero_ir::AeroError {
                phase: "IO",
                line: 0,
                col: 0,
                msg: format!("cannot read file {path}: {e}"),
            };
            eprint!("{}", diag::render_error("", path, &err));
            return 1;
        }
    };
    match aero_ir::run_source_opt(&source, opt) {
        Ok(()) => 0,
        Err(e) => {
            eprint!("{}", diag::render_error(&source, path, &e));
            1
        }
    }
}

/// `aero fmt [--check] <file.aero> [--width N] [--indent N]`
///
/// Format a source file in place (default), or with `--check` just verify it is
/// already formatted. `--width` sets the max line width and `--indent` the
/// spaces per block level (rustfmt-style knobs).
fn cmd_fmt(arg: &str) -> u8 {
    let (check, file, opts, err) = parse_fmt_args(arg);
    if let Some(e) = err {
        eprintln!("{file}: {e}");
        return 1;
    }
    let source = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {file}: {e}");
            return 1;
        }
    };
    let formatted = match aero_fmt::format_with(&source, &opts) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{file}: {e}");
            return 1;
        }
    };
    if check {
        if formatted == source {
            println!("{file} is formatted");
            0
        } else {
            eprintln!("{file} is not formatted (run `aero fmt {file}`)");
            1
        }
    } else if let Err(e) = std::fs::write(&file, formatted) {
        eprintln!("cannot write {file}: {e}");
        1
    } else {
        println!("formatted {file}");
        0
    }
}

/// Split `aero fmt` arguments into `(check, path, options, error)`. The first
/// non-option word is the file path; `--width`/`--indent` configure the
/// formatter. Returns `(_, _, _, Some(msg))` on bad option values.
fn parse_fmt_args(arg: &str) -> (bool, String, aero_fmt::FmtOptions, Option<String>) {
    let mut check = false;
    let mut opts = aero_fmt::FmtOptions::default();
    let mut path = String::new();
    let toks: Vec<&str> = arg.split_whitespace().collect();
    let mut i = 0usize;
    while i < toks.len() {
        match toks[i] {
            "--check" => check = true,
            "--width" => match toks.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                Some(n) if n >= 20 => {
                    opts.max_width = n;
                    i += 1;
                }
                _ => return (check, if path.is_empty() { "--width".into() } else { path }, opts, Some("--width needs an integer >= 20".into())),
            },
            "--indent" => match toks.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                Some(n) if n >= 1 => {
                    opts.indent = n;
                    i += 1;
                }
                _ => return (check, if path.is_empty() { "--indent".into() } else { path }, opts, Some("--indent needs an integer >= 1".into())),
            },
            other => {
                if path.is_empty() {
                    path = other.to_string();
                }
            }
        }
        i += 1;
    }
    if path.is_empty() {
        return (check, path, opts, Some("missing file path".into()));
    }
    (check, path, opts, None)
}

/// `aero clippy <file.aero>`: run the static linter and print all findings.
fn cmd_clippy(file: &str) -> u8 {
    let source = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {file}: {e}");
            return 1;
        }
    };
    let diags = aero_clippy::lint(&source);
    if diags.is_empty() {
        println!("{file}: no issues found");
        return 0;
    }
    for d in &diags {
        let cat = d.category.as_str();
        let msg = if d.message.is_empty() {
            "issue".to_string()
        } else {
            format!("[{cat}] {}", d.message)
        };
        let dg = diag::Diag {
            severity: d.severity.as_str().to_string(),
            code: (!d.code.is_empty()).then(|| d.code.clone()),
            line: d.line,
            col: d.col,
            msg,
            hint: d.suggestion.clone(),
        };
        eprint!("{}", diag::render(&dg, &source, file));
    }
    if diags.iter().any(|d| d.severity == aero_clippy::Severity::Error) {
        1
    } else {
        0
    }
}

/// `aero cov <file.aero>`: statement-coverage report.
///
/// Compiles the file to an exe with `AERO_COV=1` (instruments every statement
/// with a counter + its source line), runs it in a throwaway temp dir so
/// `__aero_cov_fini` writes `aero.cov.txt` there, then prints a per-line
/// hit report and the overall statement-coverage percentage.
fn cmd_cov(file: &str) -> u8 {
    let source = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {file}: {e}");
            return 1;
        }
    };
    // Collect SOURCE_LINES once for the report.
    let source_lines: Vec<&str> = source.lines().collect();

    // In a fresh work dir so the program's stdout doesn't mix with our report
    // and the cov file never collides with a stale copy in the user's cwd.
    let work = match std::env::temp_dir().canonicalize() {
        Ok(dir) => dir.join(format!("aero_cov_{}", std::process::id())),
        Err(_) => std::path::PathBuf::from(format!("aero_cov_{}", std::process::id())),
    };
    let _ = std::fs::remove_dir_all(&work);
    if let Err(e) = std::fs::create_dir_all(&work) {
        eprintln!("cannot create work dir {}: {e}", work.display());
        return 1;
    }
    let exe = work.join(format!(
        "{}.exe",
        std::path::Path::new(file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("cov_prog")
    ));

    // Enable the instrumentation for this compile. The env flag is read at
    // compile time; the produced exe dumps aero.cov.txt whenever it ends.
    unsafe {
        std::env::set_var("AERO_COV", "1");
    }
    let compile = aero_ir::aot::compile_to_exe_linked(&source, &exe, &[], &[], aero_ir::aot::OptLevel::O0, aero_ir::aot::host_target_triple());
    unsafe {
        std::env::remove_var("AERO_COV");
    }
    if let Err(e) = compile {
        eprint!("{}", diag::render_error(&source, file, &e));
        return 1;
    }

    // Run the instrumented program (cwd = work dir -> cov file lands there).
    let status = match std::process::Command::new(&exe)
        .current_dir(&work)
        .status()
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot run {}: {e}", exe.display());
            let _ = std::fs::remove_dir_all(&work);
            return 1;
        }
    };
    // Program's own output goes to stdout already (inherited); ours below is
    // the report. Only draw attention when the run itself failed.
    if let Some(code) = status.code() {
        if code != 0 {
            eprintln!("program exited with code {code}");
        }
    }

    // Parse `line count` pairs written by __aero_cov_fini.
    let cov_text = match std::fs::read_to_string(work.join("aero.cov.txt")) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("no coverage data: {e}");
            let _ = std::fs::remove_dir_all(&work);
            return 1;
        }
    };
    let _ = std::fs::remove_dir_all(&work);

    let mut hits: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    let mut total_stmts = 0usize;
    let user_max = source_lines.len() as u32;
    for line in cov_text.lines() {
        let mut it = line.split_whitespace();
        let (Some(ln), Some(cnt)) = (it.next(), it.next()) else { continue };
        let (Ok(ln), Ok(cnt)) = (ln.parse::<u32>(), cnt.parse::<u64>()) else { continue };
        // The report only counts statements in the user's source. The prelude's
        // injected std tokens carry their own (out-of-range) line numbers, which
        // would otherwise swamp the denominator.
        if ln == 0 || ln > user_max {
            continue;
        }
        total_stmts += 1;
        hits.insert(ln, cnt);
    }

    let covered = hits.values().filter(|&&c| c > 0).count();
    let pct = if total_stmts == 0 {
        100.0
    } else {
        covered as f64 * 100.0 / total_stmts as f64
    };

    println!("coverage report for {file}: {covered}/{total_stmts} statements covered ({pct:.1}%)");
    println!("{}", "-".repeat(60));
    for (idx, line) in source_lines.iter().enumerate() {
        let lineno = (idx + 1) as u32;
        match hits.get(&lineno) {
            Some(&c) if c > 0 => println!("{:>5} {:>6}x  | {}", lineno, c, line),
            Some(_) => println!("{:>5}        | {}", lineno, line),
            // Lines with no counter slot (blank / comment / braces) are skipped.
            None => {}
        }
    }
    0
}

fn valid_pkg_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `aero lock [dir]`: resolve deps and persist `Aero.lock` for reproducible builds.
fn cmd_lock(dir: Option<&str>) -> u8 {
    let dir = dir
        .filter(|s| !s.trim().is_empty())
        .map(Path::new)
        .unwrap_or(Path::new("."));
    match aero_pm::graph::resolve_with_lock(dir) {
        Ok(res) => {
            if res.lock.entries.is_empty() {
                println!("no dependencies; nothing to lock ({}).", dir.display());
                return 0;
            }
            match res.lock.save(dir) {
                Ok(()) => {
                    let file = dir.join("Aero.lock");
                    println!(
                        "locked {} package(s) → {}",
                        res.lock.entries.len(),
                        file.display()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// `aero install [name]`: install a package from the GitHub ecosystem.
///
/// Downloads the package index (`packages.json`), matches/selects the package,
/// verifies its SHA-256, extracts it into `deps/<name>/` and writes the
/// `[dependencies]` entry back into `Aero.toml`.
fn cmd_install(query: Option<&str>) -> u8 {
    match aero_pm::install_package(Path::new("."), query, env!("CARGO_PKG_VERSION")) {
        Ok(report) => {
            println!("{report}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// `aero publish <dir>`: publish a library package into the registry.
fn cmd_publish(dir: &str) -> u8 {
    let reg = aero_pm::Registry::locate();
    match reg.publish(Path::new(dir)) {
        Ok(c) => {
            println!("published {}@{} → {}", c.name, c.version, reg.root().display());
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// `aero ls`: list every package and its published versions in the registry.
fn cmd_ls() -> u8 {
    let reg = aero_pm::Registry::locate();
    let names = reg.packages();
    if names.is_empty() {
        println!("registry is empty ({})", reg.root().display());
        return 0;
    }
    for name in names {
        let versions: Vec<String> = reg
            .versions(&name)
            .iter()
            .map(|c| c.version.to_string())
            .collect();
        let joined = if versions.is_empty() {
            "(no versions)".to_string()
        } else {
            versions.join(", ")
        };
        println!("{name}: {joined}");
    }
    0
}

/// `aero registry`: print the resolved registry location.
fn cmd_registry() -> u8 {
    let reg = aero_pm::Registry::locate();
    println!("{}", reg.root().display());
    0
}
