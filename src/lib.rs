// Library crate for integration tests.
// main.rs has its own mod declarations; this re-exports all modules.

pub mod bazel_remote;
/// Pure rendering of a failed build's stderr into a diagnosable error. Split
/// out of `build_monitor` so the Linux-only merge gate can test it — a build
/// error that does not name its own cause is a defect the gate must catch.
pub mod build_diagnostics;
pub mod build_monitor;
pub mod build_submissions;
pub mod cache_key;
pub mod cache_telemetry;
pub mod ci_runner_lifecycle;
pub mod ci_runner_probe;
pub mod config;
pub mod dev_action;
pub mod diagnostics;
pub mod error;
pub mod evaluation;
pub mod expo;
pub mod footprint;
// Row 2 Phase 1 (fleet topology): CPU/RAM/disk detection + the
// `max_concurrent_builds` budget POST. Exposed here (not only in `main.rs`) so
// `resource_sample` can reuse its machine-identity / coord-URL resolution and
// so both are reachable from tests.
pub mod fleet;
pub mod fs_atomic;
pub mod git_provenance;
pub mod health_cache;
pub mod log_capture;
pub mod otel;
pub mod pii_scrub;
pub mod process;
pub mod reapi;
/// §A2 of plan `2026-08-02-fleet-resource-telemetry-and-ci-allocation`: turns
/// the footprint snapshot into a `lane='host'`, `source='supervisor'` resource
/// sample and publishes it to coord. Best-effort — a coord outage never touches
/// the build lane.
pub mod resource_sample;
pub mod routes;
pub mod sdk_features;
pub mod server;
pub mod settings;
pub mod spawn_worktree;
pub mod state;
// Phase 4.1 (`plans/2026-05-21-coordination-improvements.md`): per-machine
// tree-sitter symbol watcher daemon. Reports `ClaimKind::Symbol` claims to
// coord via the existing `/claims/{acquire,release}` endpoints. Shipped as
// a separate binary (`src/bin/symbol_watcher.rs`) but the module lives in
// the library crate so integration tests can drive it via
// `SymbolWatcher::with_transport(...)` + `MockTransport`.
pub mod symbol_watcher;
pub mod trace_propagation;
pub mod velocity;
pub mod velocity_improvement;
pub mod velocity_layer;
pub mod velocity_tests;
#[cfg(windows)]
pub mod webview;
pub mod wsl_util;
