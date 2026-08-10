//! aero-pm: Aero package manager and standardized test runner (Campaign 4).
//!
//! - [`manifest`]: `Aero.toml` manifest parsing (hand-written mini-TOML)
//! - [`graph`]: path dependency resolution, topological sort, cycle detection
//! - [`build`]: merge dependency sources in topological order into one source
//! - [`test`]: test collection (`test_` prefix) with subprocess-isolated runner

pub mod build;
pub mod graph;
pub mod manifest;
pub mod test;

pub use build::{build_package, build_source, run_package};
pub use graph::{resolve, CrateSource, PmError};
pub use manifest::{parse_manifest, Dep, Manifest, ManifestError};
pub use test::{collect_tests, run_tests_source, TestReport, TestResult};
