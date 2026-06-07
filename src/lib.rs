// Library crate for integration tests.
// main.rs has its own mod declarations; this re-exports all modules.

pub mod bazel_remote;
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
pub mod fs_atomic;
pub mod git_provenance;
pub mod health_cache;
pub mod log_capture;
pub mod otel;
pub mod pii_scrub;
pub mod process;
pub mod reapi;
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
