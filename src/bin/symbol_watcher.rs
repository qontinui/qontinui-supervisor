//! `qontinui-symbol-watcher` — per-machine daemon binary for Phase 4.1
//! of `plans/2026-05-21-coordination-improvements.md`.
//!
//! Watches one or more worktree roots, runs tree-sitter on each save to
//! identify the file's defined symbols, diffs against the last-known set,
//! and pushes the deltas as `ClaimKind::Symbol` claims to coord via
//! the existing `/claims/{acquire,release}` endpoints.
//!
//! Run with:
//!
//! ```bash
//! qontinui-symbol-watcher --watch-dir D:/qontinui-root
//! # or
//! QONTINUI_COORD_URL=http://localhost:9870 qontinui-symbol-watcher \
//!     --watch-dir D:/qontinui-root/qontinui-coord \
//!     --watch-dir D:/qontinui-root/qontinui-supervisor
//! ```
//!
//! `~/.qontinui/machine.json` provides the device id (`device_id`, with a
//! legacy `machine_id` fallback). If missing, the daemon runs in NO-COORD
//! mode (extracts + logs symbols, never POSTs).

use clap::Parser;
use qontinui_supervisor::symbol_watcher::{SymbolWatcher, WatcherConfig};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::error;

#[derive(Parser, Debug)]
#[command(
    name = "qontinui-symbol-watcher",
    about = "Per-machine tree-sitter symbol watcher daemon (Phase 4.1 of \
        2026-05-21-coordination-improvements)"
)]
struct CliArgs {
    /// Watch this directory (recursive). Repeat to watch multiple roots.
    /// Defaults to the current working directory when omitted.
    #[arg(long = "watch-dir")]
    watch_dirs: Vec<PathBuf>,

    /// How long after a save to keep a symbol claimed (seconds).
    /// Defaults to 300s (matches coord's `default_ttl_for(Symbol)`).
    #[arg(long = "idle-ttl-seconds")]
    idle_ttl_seconds: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Standard supervisor-style tracing init — env filter override
    // (`RUST_LOG=qontinui_supervisor::symbol_watcher=debug`) supported.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "qontinui_supervisor::symbol_watcher=info".into());
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .init();

    let args = CliArgs::parse();

    let watch_dirs: Vec<PathBuf> = if args.watch_dirs.is_empty() {
        let cwd = std::env::current_dir()?;
        vec![cwd]
    } else {
        args.watch_dirs
    };

    let config = WatcherConfig::build(watch_dirs, args.idle_ttl_seconds);

    let watcher = match SymbolWatcher::new(config) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            error!("symbol_watcher: construction failed: {e}");
            return Err(e);
        }
    };

    watcher.run().await;
    Ok(())
}
