//! Tree-sitter–based "currently-edited symbol" watcher daemon (Phase 4.1
//! of `plans/2026-05-21-coordination-improvements.md`).
//!
//! On startup the daemon:
//!
//! 1. Reads its config (env vars + sensible defaults — see [`WatcherConfig`]).
//! 2. Resolves the local device id from `~/.qontinui/machine.json`'s
//!    `device_id` (canonical), falling back to the legacy `machine_id`.
//!    If missing, runs in NO-COORD mode (extracts + logs, never POSTs).
//! 3. Spawns one `notify::RecommendedWatcher` per configured watch_dir.
//! 4. Loops: for each filtered save event, re-extracts symbols with
//!    tree-sitter, compares to the last-known set, and emits
//!    acquire/release HTTP calls to `{coord_url}/claims/{acquire,release}`
//!    for the diff.
//!
//! Resource_key shape: `<repo>:<relative-file-path>:<symbol-name>` —
//! the same shape Phase 4.2 documented in the coord enum's doc comment.

pub mod coord_client;
pub mod diff;
pub mod file_watch;
pub mod tree_sitter_extract;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use coord_client::{
    acquire_best_effort, release_best_effort, ClaimRequestWire, CoordTransport, HttpTransport,
    NoOpTransport,
};
use diff::{SymbolDiff, SymbolMemory};
use file_watch::{spawn_watchers, SaveEvent};
use tree_sitter_extract::{extract_symbols, Symbol};

/// Default coord URL when neither env nor profiles file provides one.
/// Mirrors the local-dev convention (`http://localhost:9870`).
const DEFAULT_COORD_URL: &str = "http://localhost:9870";

/// Env var to override the coord URL.
const ENV_COORD_URL: &str = "QONTINUI_COORD_URL";

/// Default idle TTL (seconds): if a symbol claim doesn't fire a re-acquire
/// within this many seconds, the daemon proactively releases it. Matches
/// the coord TTL so the only difference between "daemon idle" and "daemon
/// crashed" is whether the audit row gets a clean release vs. an expiry.
const DEFAULT_IDLE_TTL_SECS: u64 = 300;

/// Default file extensions to watch.
const DEFAULT_EXTENSIONS: &[&str] = &["rs", "ts", "tsx", "py"];

/// Effective runtime config, materialized from CLI args + env + defaults.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub watch_dirs: Vec<PathBuf>,
    pub coord_url: String,
    pub idle_ttl: Duration,
    pub extensions: Vec<String>,
}

impl WatcherConfig {
    /// Build a config from raw CLI input. CLI values take precedence; env
    /// vars fill in defaults; constants are the floor.
    pub fn build(watch_dirs: Vec<PathBuf>, idle_ttl_seconds: Option<u64>) -> Self {
        let coord_url = std::env::var(ENV_COORD_URL)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_COORD_URL.to_string());
        let idle_ttl = Duration::from_secs(idle_ttl_seconds.unwrap_or(DEFAULT_IDLE_TTL_SECS));
        let extensions = DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect();
        Self {
            watch_dirs,
            coord_url,
            idle_ttl,
            extensions,
        }
    }
}

/// Minimal subset of `~/.qontinui/machine.json` the watcher needs.
///
/// The canonical post-unified-devices field is `device_id`; the live file
/// carries `device_id`, NOT `machine_id` (older hosts used `machine_id`).
/// BOTH fields are `Option` so deserialization does not fail outright on a
/// `device_id`-only file — a required-`machine_id` struct previously made
/// `load_machine_id` return `None` and silently dropped the watcher into
/// NO-COORD mode on every modern host (fixed 2026-06-08, matching the
/// `fleet.rs` #90 / `dev_action/ingest.rs` #89 pattern).
#[derive(Debug, Clone, Deserialize)]
struct MachineFile {
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    machine_id: Option<String>,
}

/// Read `~/.qontinui/machine.json` and return the device id (UUID string).
/// Prefers the canonical `device_id`, falling back to the legacy
/// `machine_id`. Returns `None` if the file is missing/unreadable/malformed
/// or carries neither field — caller falls back to NO-COORD mode.
pub fn load_machine_id() -> Option<String> {
    let path = dirs::home_dir()?.join(".qontinui").join("machine.json");
    read_machine_id(&path)
}

/// Path-parameterized core of [`load_machine_id`] (testable without touching
/// the real home directory). Prefers `device_id`, falls back to `machine_id`.
fn read_machine_id(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let parsed: MachineFile = serde_json::from_slice(&bytes).ok()?;
    parsed.device_id.or(parsed.machine_id)
}

/// Walk up the directory tree from `start` looking for a `.git` directory
/// or file (git worktrees use `.git` files). Returns the path of the dir
/// containing the `.git` entry, or `None` if none found by filesystem root.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cursor = start;
    loop {
        let git = cursor.join(".git");
        if git.exists() {
            return Some(cursor.to_path_buf());
        }
        cursor = cursor.parent()?;
    }
}

/// Compute the repo-relative path for `file`, scoped to `repo_root`.
/// On Windows the result uses forward slashes for resource_key stability
/// across operating systems (so MSI + spaceship can collide on the same
/// key regardless of separator).
fn relative_repo_path(repo_root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(repo_root).unwrap_or(file);
    rel.to_string_lossy().replace('\\', "/")
}

/// Resource key for coord: `<repo>:<rel-file>:<symbol>`.
pub fn make_resource_key(repo: &str, rel_file: &str, symbol: &str) -> String {
    format!("{repo}:{rel_file}:{symbol}")
}

/// Per-symbol bookkeeping. `last_emitted_at` is used by the idle-TTL
/// sweeper to release stale claims.
#[derive(Debug, Clone)]
struct ActiveClaim {
    repo: String,
    rel_file: String,
    symbol_name: String,
    last_acquired_at: std::time::Instant,
}

impl ActiveClaim {
    fn resource_key(&self) -> String {
        make_resource_key(&self.repo, &self.rel_file, &self.symbol_name)
    }
}

/// The daemon. Owns the watchers, the symbol memory, the transport, the
/// active-claim map. Constructed via [`SymbolWatcher::new`], driven via
/// [`SymbolWatcher::run`].
pub struct SymbolWatcher {
    config: WatcherConfig,
    machine_id: String,
    transport: Arc<dyn CoordTransport>,
    /// `None` means no-coord mode — visible to logs only.
    no_coord_mode: bool,
    memory: Arc<Mutex<SymbolMemory>>,
    /// Active claims keyed by resource_key.
    active: Arc<Mutex<HashMap<String, ActiveClaim>>>,
}

impl SymbolWatcher {
    /// Build a [`SymbolWatcher`]. `machine_id`-resolution failure produces
    /// a NO-COORD-mode watcher with a no-op transport instead of erroring.
    pub fn new(config: WatcherConfig) -> anyhow::Result<Self> {
        let (machine_id, transport, no_coord_mode): (String, Arc<dyn CoordTransport>, bool) =
            match load_machine_id() {
                Some(id) => {
                    info!(
                        "symbol_watcher: using machine_id={} coord_url={}",
                        id, config.coord_url
                    );
                    let http = HttpTransport::new(config.coord_url.clone())?;
                    (id, Arc::new(http) as Arc<dyn CoordTransport>, false)
                }
                None => {
                    warn!(
                        "symbol_watcher: ~/.qontinui/machine.json missing or invalid — \
                         running in NO-COORD mode (symbols extracted + logged, no claims sent)"
                    );
                    (
                        "no-coord-mode".to_string(),
                        Arc::new(NoOpTransport) as Arc<dyn CoordTransport>,
                        true,
                    )
                }
            };

        Ok(Self {
            config,
            machine_id,
            transport,
            no_coord_mode,
            memory: Arc::new(Mutex::new(SymbolMemory::new())),
            active: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Construct a [`SymbolWatcher`] with an explicit transport. Used by
    /// tests to inject a `MockTransport`.
    pub fn with_transport(
        config: WatcherConfig,
        machine_id: String,
        transport: Arc<dyn CoordTransport>,
    ) -> Self {
        Self {
            config,
            machine_id,
            transport,
            no_coord_mode: false,
            memory: Arc::new(Mutex::new(SymbolMemory::new())),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    pub fn is_no_coord_mode(&self) -> bool {
        self.no_coord_mode
    }

    /// Run the watcher until shutdown. Spawns file watchers, an idle-TTL
    /// sweeper, and the main event loop. Returns when the channel
    /// closes (i.e. all watchers were dropped — only on explicit stop).
    pub async fn run(self: Arc<Self>) {
        info!(
            "symbol_watcher: starting — watch_dirs={:?} extensions={:?} idle_ttl={:?}s",
            self.config.watch_dirs,
            self.config.extensions,
            self.config.idle_ttl.as_secs()
        );
        let (mut rx, _watchers) =
            spawn_watchers(&self.config.watch_dirs, self.config.extensions.clone());

        // Spawn idle-TTL sweeper. Every IDLE_TTL/4 seconds, scan active
        // claims; any whose `last_acquired_at` is older than IDLE_TTL gets
        // released. Quarter-period cadence keeps lag under 25% of TTL.
        let sweeper = Arc::clone(&self);
        let sweep_interval = self
            .config
            .idle_ttl
            .checked_div(4)
            .unwrap_or(Duration::from_secs(60));
        let sweeper_task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(sweep_interval);
            tick.tick().await; // skip the immediate-first tick
            loop {
                tick.tick().await;
                sweeper.sweep_idle().await;
            }
        });

        while let Some(event) = rx.recv().await {
            self.handle_event(event).await;
        }
        info!("symbol_watcher: file-watch channel closed — shutting down");
        sweeper_task.abort();
    }

    /// Handle a single file-save event. Re-extracts symbols, computes
    /// the diff against memory, and fires acquire/release for the delta.
    /// Exposed `pub` for tests.
    pub async fn handle_event(&self, event: SaveEvent) {
        let path = event.path.clone();

        // Read the file contents. If the read fails (file vanished, perm
        // denied), treat as "all prior symbols removed".
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                debug!(
                    "symbol_watcher: read {} failed: {} — treating as deletion",
                    path.display(),
                    e
                );
                self.handle_deletion(&path).await;
                return;
            }
        };

        // Resolve repo root + relative path. If we can't find a `.git`,
        // use the immediate parent dir's name as the synthetic "repo"
        // so resource_keys still work in tests / non-git trees.
        let (repo_name, rel_file) = match find_repo_root(&path) {
            Some(root) => {
                let repo = root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown-repo")
                    .to_string();
                let rel = relative_repo_path(&root, &path);
                (repo, rel)
            }
            None => {
                let repo = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown-repo")
                    .to_string();
                let rel = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                (repo, rel)
            }
        };

        let new_symbols = extract_symbols(&event.extension, &source);

        let diff: SymbolDiff = {
            let mut mem = self.memory.lock().await;
            mem.update(&path, new_symbols)
        };

        if diff.is_empty() {
            return;
        }

        let metadata_for = |sym: &Symbol| {
            serde_json::json!({
                "file": rel_file.clone(),
                "repo": repo_name.clone(),
                "start_line": sym.start_line,
                "end_line": sym.end_line,
            })
        };

        // Acquire claims for added + modified.
        for sym in diff.added.iter().chain(diff.modified.iter()) {
            let key = make_resource_key(&repo_name, &rel_file, &sym.name);
            let req = ClaimRequestWire::for_symbol(&self.machine_id, &key, metadata_for(sym));
            acquire_best_effort(&*self.transport, req).await;
            self.active.lock().await.insert(
                key.clone(),
                ActiveClaim {
                    repo: repo_name.clone(),
                    rel_file: rel_file.clone(),
                    symbol_name: sym.name.clone(),
                    last_acquired_at: std::time::Instant::now(),
                },
            );
        }

        // Release claims for removed.
        for sym in &diff.removed {
            let key = make_resource_key(&repo_name, &rel_file, &sym.name);
            let req = ClaimRequestWire::for_symbol(&self.machine_id, &key, metadata_for(sym));
            release_best_effort(&*self.transport, req).await;
            self.active.lock().await.remove(&key);
        }
    }

    /// File-deletion handler. Releases every prior claim for this file.
    async fn handle_deletion(&self, path: &PathBuf) {
        let prior: Vec<Symbol> = {
            let mut mem = self.memory.lock().await;
            mem.forget(path)
        };
        if prior.is_empty() {
            return;
        }

        let (repo_name, rel_file) = match find_repo_root(path) {
            Some(root) => {
                let repo = root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown-repo")
                    .to_string();
                let rel = relative_repo_path(&root, path);
                (repo, rel)
            }
            None => return,
        };

        for sym in prior {
            let key = make_resource_key(&repo_name, &rel_file, &sym.name);
            let req = ClaimRequestWire::for_symbol(
                &self.machine_id,
                &key,
                serde_json::json!({
                    "file": rel_file,
                    "repo": repo_name,
                    "reason": "file_deleted",
                }),
            );
            release_best_effort(&*self.transport, req).await;
            self.active.lock().await.remove(&key);
        }
    }

    /// Periodic sweeper: release any active claim whose
    /// `last_acquired_at` is older than `idle_ttl`. Holds the
    /// `active` lock only long enough to take a snapshot, then performs
    /// releases without the lock held (HTTP calls shouldn't block other
    /// event handlers).
    async fn sweep_idle(&self) {
        let now = std::time::Instant::now();
        let stale: Vec<ActiveClaim> = {
            let active = self.active.lock().await;
            active
                .values()
                .filter(|c| now.duration_since(c.last_acquired_at) >= self.config.idle_ttl)
                .cloned()
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        debug!("symbol_watcher: sweeping {} idle claim(s)", stale.len());
        for claim in stale {
            let key = claim.resource_key();
            let req = ClaimRequestWire::for_symbol(
                &self.machine_id,
                &key,
                serde_json::json!({
                    "file": claim.rel_file,
                    "repo": claim.repo,
                    "reason": "idle_ttl_expired",
                }),
            );
            release_best_effort(&*self.transport, req).await;
            self.active.lock().await.remove(&key);
        }
    }

    /// Test-only: snapshot of currently-active resource keys.
    #[cfg(test)]
    pub async fn active_keys(&self) -> Vec<String> {
        let active = self.active.lock().await;
        let mut v: Vec<String> = active.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests;
