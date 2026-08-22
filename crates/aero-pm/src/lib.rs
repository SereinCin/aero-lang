//! aero-pm: Aero package manager and standardized test runner (Campaign 4).
//!
//! - [`manifest`]: `Aero.toml` manifest parsing (hand-written mini-TOML)
//! - [`graph`]: path/registry dependency resolution, topological sort, cycle detection
//! - [`semver`]: SemVer versions and requirement (range) matching
//! - [`registry`]: local directory registry (`~/ .aero/registry`)
//! - [`lock`]: `Aero.lock` lockfile (read/write + FNV checksums)
//! - [`build`]: merge dependency sources in topological order into one source
//! - [`test`]: test collection (`test_` prefix) with subprocess-isolated runner
//! - [`bench`]: micro-benchmark collection (`bench_` prefix) with AOT + timing

pub mod bench;
pub mod build;
pub mod fetch;
pub mod graph;
pub mod lock;
pub mod manifest;
pub mod registry;
pub mod semver;
pub mod test;

pub use build::{build_package, build_source, run_package};
pub use bench::{run_bench_source, BenchConfig, BenchReport, BenchResult};
pub use fetch::{fetch_index, install_package, parse_index, IndexEntry, InstallReport};
pub use graph::{resolve, CrateSource, PmError};
pub use lock::{fnv_checksum, LockEntry, Lockfile, SourceKind};
pub use manifest::{load_manifest_from, parse_manifest, Dep, Manifest, ManifestError};
pub use registry::{Registry, RegistryCrate};
pub use semver::{Requirement, Version};
pub use test::{collect_tests, run_tests_source, TestReport, TestResult};
