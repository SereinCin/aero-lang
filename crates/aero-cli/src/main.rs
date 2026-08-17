mod diag;
mod lsp;

use std::path::Path;

const USAGE: &str = "\
Usage:
  aero run <file.aero | package dir>   compile and run
  aero run <file.aero> -O0|-O1|-O2|-O3  run with a specific optimization level
  aero build [file.aero | dir]        compile to a standalone exe (AOT)
  aero build <file.aero> -O0|-O1|-O2|-O3  build with a specific optimization level
  aero build <file> --target <triple>  cross-compile for a target (e.g. x86_64-unknown-linux-gnu)
  aero new <name>                    create a new package skeleton
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
    let (path, triple, opt) = parse_build_flags(arg);
    let p = Path::new(path);
    if p.is_dir() || p.join("Aero.toml").exists() {
        match aero_pm::run_package(p) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }
    } else {
        run_file(path, opt, triple)
    }
}

/// `aero build [file.aero | dir] [-O0|-O1|-O2|-O3] [--target <triple>]`: AOT-compile to a standalone executable.
/// - Package dir: outputs to `<pkg root>/target/aero/<pkg name>.exe`;
/// - Single file: outputs to `<file name>.exe` next to the source.
fn cmd_build(arg: &str) -> u8 {
    // Peel an optional `--target <triple>` and `-O<n>`.
    let (path, triple, opt) = parse_build_flags(arg);
    let p = Path::new(path);
    if p.is_file() {
        let exe = p.with_extension("exe");
        let source = match std::fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                let err = aero_ir::AeroError {
                    phase: "IO",
                    line: 0,
                    col: 0,
                    msg: format!("cannot read file {}: {e}", p.display()),
                };
                eprint!("{}", diag::render_error("", path, &err));
                return 1;
            }
        };
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
        return match aero_ir::aot::compile_to_exe_cached(&source, &exe, &[], &[], opt, &env_refs, triple) {
            Ok(hit) => {
                println!(
                    "{}{}",
                    exe.display(),
                    if hit { " (cached)" } else { "" }
                );
                0
            }
            Err(e) => {
                eprint!("{}", diag::render_error(&source, path, &e));
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
    match aero_pm::build::compile_package(p, &exe, triple) {
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

/// Parse `--target <triple>` and `-O<n>` flags from a build/run argument string.
/// Returns `(path_or_dir, triple, opt_level)`. The triple defaults to the host
/// triple when not specified.
fn parse_build_flags(arg: &str) -> (&str, &str, aero_ir::aot::OptLevel) {
    let mut triple = aero_ir::aot::host_target_triple();
    let mut opt = aero_ir::aot::OptLevel::default();
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
    (path, triple, opt)
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
