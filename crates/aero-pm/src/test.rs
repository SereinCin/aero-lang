//! Standardized test framework: `aero test`.
//!
//! Convention: functions named with the `test_` prefix are test cases (no params, return void).
//! Each case compiles and runs in its own subprocess (isolating crashes/assertion failures); exit 0 = pass.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use aero_parse::ast::{Program, Stmt};

use crate::graph::PmError;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Result of a single test.
#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    /// Output on failure (stdout+stderr)
    pub output: String,
}

/// Test summary report.
#[derive(Debug, Clone)]
pub struct TestReport {
    pub total: usize,
    pub passed: usize,
    pub results: Vec<TestResult>,
}

impl TestReport {
    /// Human-readable summary text.
    pub fn summary(&self) -> String {
        format!(
            "{} tests: {} passed, {} failed",
            self.total,
            self.passed,
            self.total - self.passed
        )
    }
}

/// Collect all test function names from the AST (`test_` prefix).
pub fn collect_tests(program: &Program) -> Vec<String> {
    let mut tests = Vec::new();
    for stmt in &program.stmts {
        if let Stmt::FnDef { name, params, .. } = stmt {
            if name.starts_with("test_") {
                if !params.is_empty() {
                    // Test functions with parameters are not collected (the runner would report an arity error; skip silently)
                    continue;
                }
                tests.push(name.clone());
            }
        }
    }
    tests
}

/// Parse source and collect test function names.
pub fn collect_tests_from_source(source: &str) -> Result<Vec<String>, PmError> {
    let program = aero_parse::parse_source(source)
        .map_err(|e| PmError::new(format!("syntax error: {}:{}: {}", e.line, e.col, e.msg)))?;
    Ok(collect_tests(&program))
}

/// Synthesize the runnable source for one test: append `test_xxx();` at the end.
/// No AST reordering, so original line numbers and diagnostics stay consistent.
fn synthesize(test: &str, source: &str) -> String {
    let mut out = source.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(test);
    out.push_str("();\n");
    out
}

/// Run one test case in a subprocess.
fn run_one(test: &str, source: &str) -> TestResult {
    let synthesized = synthesize(test, source);
    if synthesized.is_empty() {
        return TestResult {
            name: test.to_string(),
            passed: false,
            output: "internal error: test synthesis failed".to_string(),
        };
    }
    let path: PathBuf = std::env::temp_dir().join(format!(
        "aero_test_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, &synthesized).unwrap();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("aero"));
    let out = Command::new(&exe)
        .args(["run", path.to_str().unwrap()])
        .output();
    let _ = std::fs::remove_file(&path);
    match out {
        Ok(out) => {
            let output = String::from_utf8_lossy(&out.stdout)
                .into_owned()
                .replace("\r\n", "\n")
                + &String::from_utf8_lossy(&out.stderr).into_owned();
            TestResult {
                name: test.to_string(),
                passed: out.status.success(),
                output,
            }
        }
        Err(e) => TestResult {
            name: test.to_string(),
            passed: false,
            output: format!("cannot start the compiler process: {e}"),
        },
    }
}

/// Run all test cases in the source.
pub fn run_tests_source(source: &str) -> Result<TestReport, PmError> {
    let tests = collect_tests_from_source(source)?;
    if tests.is_empty() {
        return Err(PmError::new(
            "no test functions found (names must start with `test_`)",
        ));
    }
    let mut report = TestReport {
        total: tests.len(),
        passed: 0,
        results: Vec::new(),
    };
    for t in &tests {
        let r = run_one(t, source);
        if r.passed {
            report.passed += 1;
        }
        report.results.push(r);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_finds_test_fns() {
        let program = aero_parse::parse_source(
            "fn test_add() { }\nfn helper() { }\nfn test_mul() { }\nfn main_helper() { }",
        )
        .unwrap();
        let tests = collect_tests(&program);
        assert_eq!(tests, vec!["test_add", "test_mul"]);
    }

    #[test]
    fn synthesize_appends_call() {
        // Synthesis = original text + trailing call; line numbers unchanged
        let src = "fn test_a() { assert_eq(1, 1); }";
        let out = synthesize("test_a", src);
        assert!(out.ends_with("test_a();\n"));
        assert!(out.starts_with(src));
        assert_eq!(out.matches("test_a").count(), 2);
        // The synthesized text must re-parse
        let program = aero_parse::parse_source(&out).unwrap();
        assert_eq!(collect_tests(&program), vec!["test_a"]);
    }

    #[test]
    fn synthesize_keeps_line_numbers() {
        // Assertion line numbers match the original file: line 1 is the assert_eq
        let src = "fn test_x() {\n    assert_eq(1, 2);\n}\n";
        let out = synthesize("test_x", src);
        assert_eq!(out.lines().count(), 4); // 3 original lines + 1 call line
        assert!(out.ends_with("test_x();\n"));
    }
}
