use std::path::Path;

const USAGE: &str = "\
Usage:
  aero run <file.aero | package dir>   compile and run
  aero build [file.aero | dir]        compile to a standalone exe (AOT)
  aero new <name>                    create a new package skeleton
  aero test [file.aero]              run tests (default: all in tests/)";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("run") => match args.get(2) {
            Some(path) => cmd_run(path),
            None => {
                eprintln!("missing argument.\n{USAGE}");
                2
            }
        },
        Some("build") => match args.get(2) {
            Some(p) => cmd_build(p),
            None => cmd_build("."),
        },
        Some("new") => match args.get(2) {
            Some(name) => cmd_new(name),
            None => {
                eprintln!("missing package name.\naero new <name>");
                2
            }
        },
        Some("test") => match args.get(2) {
            Some(file) => cmd_test_file(file),
            None => cmd_test_dir("tests"),
        },
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

/// `aero run <file|dir>`: files take the single-file pipeline; directories (or those with Aero.toml) use the package flow.
fn cmd_run(arg: &str) -> u8 {
    let p = Path::new(arg);
    if p.is_dir() || p.join("Aero.toml").exists() {
        match aero_pm::run_package(p) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }
    } else {
        match run_file(arg) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }
    }
}

/// `aero build [file.aero | dir]`: AOT-compile to a standalone executable.
/// - Package dir: outputs to `<pkg root>/target/aero/<pkg name>.exe`;
/// - Single file: outputs to `<file name>.exe` next to the source.
fn cmd_build(arg: &str) -> u8 {
    let p = Path::new(arg);
    if p.is_file() {
        let exe = p.with_extension("exe");
        return match aero_ir::aot::compile_file_to_exe(p, &exe) {
            Ok(()) => {
                println!("build succeeded: {}", exe.display());
                0
            }
            Err(e) => {
                eprintln!("build failed: {e}");
                1
            }
        };
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
    match aero_pm::build::compile_package(p, &exe) {
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

fn run_file(path: &str) -> Result<(), aero_ir::AeroError> {
    let source = std::fs::read_to_string(path).map_err(|e| aero_ir::AeroError {
        phase: "IO",
        line: 0,
        col: 0,
        msg: format!("cannot read file {path}: {e}"),
    })?;
    aero_ir::run_source(&source)
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
