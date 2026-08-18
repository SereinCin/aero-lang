//! Standardized micro-benchmark framework: `aero bench`.
//!
//! Convention: functions named with the `bench_` prefix are benchmark cases
//! (no params, no meaningful return). Each case is AOT-compiled once into a
//! standalone executable whose `main` runs the function in a tight loop
//! `iterations` times; the executable is then launched `samples` times and the
//! **fastest** run is kept (it best reflects steady-state, amortizing OS
//! process-spawn overhead across the iteration count). Per-case results report
//! total time, ns/op and ops/sec.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use aero_parse::ast::{Program, Stmt};

use crate::graph::PmError;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Tunables for a benchmark run.
#[derive(Debug, Clone, Copy)]
pub struct BenchConfig {
    /// Iterations per executable run (the bench body is called this many times).
    pub iterations: u64,
    /// How many times to run the executable; the fastest (minimum) total wins.
    pub samples: usize,
}

impl Default for BenchConfig {
    fn default() -> Self {
        BenchConfig { iterations: 1_000_000, samples: 5 }
    }
}

/// Result of a single benchmark case.
#[derive(Debug, Clone)]
pub struct BenchResult {
    pub name: String,
    pub iterations: u64,
    /// Fastest total wall-clock time (ns) across all samples.
    pub total_ns: u128,
    /// Wall-clock time per iteration (ns), process spawn amortized over `iterations`.
    pub ns_per_iter: f64,
    /// Iterations per second (throughput).
    pub iters_per_sec: f64,
}

/// The set of all benchmark results for one source file.
#[derive(Debug, Clone)]
pub struct BenchReport {
    pub results: Vec<BenchResult>,
}

impl BenchReport {
    /// A fixed-width table suitable for terminal output.
    pub fn text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<24} {:>12} {:>12} {:>12} {:>16}\n",
            "benchmark", "iterations", "total ms", "ns/op", "op/s"
        ));
        for r in &self.results {
            out.push_str(&format!(
                "{:<24} {:>12} {:>12.3} {:>12.3} {:>16.2}\n",
                r.name,
                r.iterations,
                r.total_ns as f64 / 1e6,
                r.ns_per_iter,
                r.iters_per_sec,
            ));
        }
        out
    }
}

/// Collect all `bench_` function names (no params) from the AST.
pub fn collect_benches(program: &Program) -> Vec<String> {
    let mut benches = Vec::new();
    for stmt in &program.stmts {
        if let Stmt::FnDef { name, params, .. } = stmt {
            if name.starts_with("bench_") && params.is_empty() {
                benches.push(name.clone());
            }
        }
    }
    benches
}

/// Parse source and collect benchmark function names.
pub fn collect_benches_from_source(source: &str) -> Result<Vec<String>, PmError> {
    let program = aero_parse::parse_source(source)
        .map_err(|e| PmError::new(format!("syntax error: {}:{}: {}", e.line, e.col, e.msg)))?;
    Ok(collect_benches(&program))
}

/// Build the AOT source for one bench: original code plus a tight loop that
/// calls `bench()` `iterations` times. The loop counter uses a unique name so it
/// cannot collide with user identifiers.
fn harness(bench: &str, source: &str, iterations: u64, tid: u32) -> String {
    let mut out = source.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "let _bench_i{tid} = {iterations};\n\
         while (_bench_i{tid} > 0) {{\n    \
             {bench}();\n    \
             _bench_i{tid} = _bench_i{tid} - 1;\n\
         }}\n"
    ));
    out
}

/// Compile + run one benchmark case, returning its fastest-sample timing.
fn run_one_bench(bench: &str, source: &str, cfg: &BenchConfig) -> Result<BenchResult, PmError> {
    let tid = COUNTER.fetch_add(1, Ordering::SeqCst);
    let src = harness(bench, source, cfg.iterations, tid);

    let path: PathBuf = std::env::temp_dir()
        .join(format!("aero_bench_src_{}_{}.aero", std::process::id(), tid));
    std::fs::write(&path, src.as_bytes()).map_err(|e| {
        PmError::new(format!("cannot write bench source: {e}"))
    })?;

    let exe: PathBuf = std::env::temp_dir()
        .join(format!("aero_bench_exe_{}_{}.exe", std::process::id(), tid));
    let _ = std::fs::remove_file(&exe);

    aero_ir::aot::compile_to_exe_linked(
        &src,
        &exe,
        &[],
        &[],
        aero_ir::aot::OptLevel::default(),
        aero_ir::aot::host_target_triple(),
    )
    .map_err(|e| PmError::new(format!("bench `{bench}` failed to compile: {}", e.msg)))?;
    let _ = std::fs::remove_file(&path);

    let mut best_ns = u128::MAX;
    for _ in 0..cfg.samples.max(1) {
        let t = Instant::now();
        let status = Command::new(&exe).output();
        let elapsed = t.elapsed().as_nanos();
        if let Err(e) = status {
            let _ = std::fs::remove_file(&exe);
            return Err(PmError::new(format!("cannot run bench `{bench}`: {e}")));
        }
        if elapsed < best_ns {
            best_ns = elapsed;
        }
    }
    let _ = std::fs::remove_file(&exe);

    let iters = cfg.iterations.max(1) as f64;
    let ns_per_iter = best_ns as f64 / iters;
    let iters_per_sec = if best_ns > 0 { 1e9 * iters / best_ns as f64 } else { 0.0 };
    Ok(BenchResult {
        name: bench.to_string(),
        iterations: cfg.iterations,
        total_ns: best_ns,
        ns_per_iter,
        iters_per_sec,
    })
}

/// Run all benchmark cases in the source.
pub fn run_bench_source(source: &str, cfg: &BenchConfig) -> Result<BenchReport, PmError> {
    let benches = collect_benches_from_source(source)?;
    if benches.is_empty() {
        return Err(PmError::new(
            "no benchmark functions found (names must start with `bench_`)",
        ));
    }
    let mut results = Vec::with_capacity(benches.len());
    for b in &benches {
        results.push(run_one_bench(b, source, cfg)?);
    }
    Ok(BenchReport { results })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_finds_bench_fns() {
        let program = aero_parse::parse_source(
            "fn bench_sum() { }\nfn helper() { }\nfn bench_mul() { }\nfn test_x() { }",
        )
        .unwrap();
        assert_eq!(collect_benches(&program), vec!["bench_sum", "bench_mul"]);
    }

    #[test]
    fn harness_builds_tight_loop() {
        let src = "fn bench_add() { let a = 1 + 2; }\n";
        let out = harness("bench_add", src, 100, 7);
        assert!(out.contains("let _bench_i7 = 100;"));
        assert!(out.contains("while (_bench_i7 > 0) {"));
        assert!(out.contains("_bench_i7 = _bench_i7 - 1;"));
        // The synthesized source must still parse.
        let program = aero_parse::parse_source(&out).unwrap();
        assert_eq!(collect_benches(&program), vec!["bench_add"]);
    }

    #[test]
    fn report_text_has_header_and_rows() {
        let report = BenchReport {
            results: vec![BenchResult {
                name: "bench_x".into(),
                iterations: 1_000_000,
                total_ns: 12_345_678,
                ns_per_iter: 12.345678,
                iters_per_sec: 81_000_000.0,
            }],
        };
        let text = report.text();
        assert!(text.starts_with("benchmark"));
        assert!(text.contains("bench_x"));
        assert!(text.contains("12.346"));
    }
}