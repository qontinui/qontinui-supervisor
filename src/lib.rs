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
pub mod external_volume;
pub mod footprint;
pub mod fs_atomic;
pub mod git_provenance;
pub mod health_cache;
pub mod log_capture;
pub mod otel;
pub mod pii_scrub;
pub mod process;
/// Pure build-provenance classification, `include!`d by `build.rs` so the
/// stamping rules and the reading rules are one source (and so a build
/// script's logic is covered by the merge-blocking CI gate).
pub mod provenance_stamp;
pub mod reapi;
/// The restart-readiness gate: the supervisor consults the runner's own
/// `GET /restart-readiness` verdict before stopping or restarting it, and the
/// long-advertised (and until now inert) `force` field is the override. Plan
/// `2026-08-29-no-single-answer-to-is-it-safe-to-restart-the-runner`, Phase 3.
pub mod restart_readiness;
pub mod routes;
/// S3-backend degrade guard for the supervisor's own in-process cargo spawns
/// (`plans/2026-08-04-landed-infra-fixes-not-in-effect-on-this-machine.md`
/// Phase 1.3).
pub mod sccache_guard;
pub mod sdk_features;
/// The supervisor's own build commit, stamped by `build.rs` and surfaced on
/// `/health` (same plan, Phase 2.3).
pub mod self_provenance;
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
