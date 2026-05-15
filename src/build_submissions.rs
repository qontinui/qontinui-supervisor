//! Generalized build pool — Row 2 Phase 3.
//!
//! Extends the supervisor build pool from "build the runner binary for
//! spawn-test" to "execute a submitted build action against a submitted
//! worktree state."
//!
//! See `plans/2026-05-14-fleet-topology-and-build-pool-design.md` §3.5
//! for the design intent. This module owns:
//!
//! * [`BuildKind`] / [`BuildStatus`] / [`BuildSubmission`] — the data
//!   shape of a submitted build.
//! * [`BuildSubmissionStore`] — bounded in-memory map keyed by `Uuid`,
//!   evicting oldest terminal submissions when over cap.
//! * [`submit`] — accepts a submission, reserves a permit on the existing
//!   `build_pool.permits` semaphore from a background task, runs cargo,
//!   captures stdout/stderr tails, and updates the submission entry.
//! * The HTTP surface (`POST /build/submit`, `GET /build/:id/status`)
//!   lives in `routes/build_submit.rs` to keep this module
//!   storage-and-execution-focused.
//!
//! Cache-hit short-circuit (bazel-remote AC lookup before dispatch) is
//! intentionally out of scope. That arrives with Row 10 Items 5-7
//! (Wave 5). This phase only dispatches — every submission produces a
//! cargo invocation.
//!
//! ## Slot accounting
//!
//! Reuses `state.build_pool.permits` (the same semaphore as
//! `spawn-test`). Generalized submissions and spawn-test builds share
//! the same slot budget. Per-slot `CARGO_TARGET_DIR` isolation is NOT
//! used here — submissions point at an externally-managed worktree
//! with its own `target/`. Phase 5 (per-machine cap enforcement) sizes
//! the semaphore from `MachineBudget.max_concurrent_builds`; this
//! phase keeps the existing config-driven sizing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::cache_key::{self, CanonicalDiffKey};
use crate::cache_telemetry::CacheEvent;
use crate::log_capture::{LogLevel, LogSource};
use crate::reapi::{ActionResult, Digest as ReapiDigest, OutputFile};
use crate::state::SharedState;

use sha2::{Digest as _, Sha256};

/// What cargo command to run for a submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildKind {
    /// `cargo check` — fast, no codegen.
    Check,
    /// `cargo build` (default profile = dev).
    Build,
    /// `cargo build --release`.
    Release,
    /// `cargo test --no-run` (compile-only).
    Test,
}

impl BuildKind {
    pub fn cargo_args(self) -> Vec<&'static str> {
        match self {
            BuildKind::Check => vec!["check"],
            BuildKind::Build => vec!["build"],
            BuildKind::Release => vec!["build", "--release"],
            BuildKind::Test => vec!["test", "--no-run"],
        }
    }
}

/// Lifecycle of a submitted build.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BuildStatus {
    /// Waiting for a build-pool permit.
    Queued,
    /// Cargo invocation in flight.
    Running { started_at: DateTime<Utc> },
    /// Cargo returned exit code 0.
    Succeeded {
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        duration_secs: f64,
    },
    /// Cargo returned non-zero exit code or failed to spawn.
    Failed {
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        duration_secs: f64,
        exit_code: Option<i32>,
        error: String,
    },
}

impl BuildStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BuildStatus::Succeeded { .. } | BuildStatus::Failed { .. }
        )
    }
}

/// One submitted build. Identified by `id` and queryable via
/// `GET /build/:id/status`.
#[derive(Debug, Clone, Serialize)]
pub struct BuildSubmission {
    pub id: Uuid,
    pub worktree_path: PathBuf,
    pub build_kind: BuildKind,
    pub agent_id: Option<String>,
    pub package: Option<String>,
    pub features: Vec<String>,
    /// Base ref the content-hash diff is computed against. `None` ⇒
    /// `canonical_diff_hash.py` defaults to the first commit on HEAD.
    /// Production callers should pass `origin/main` (or the agent's
    /// branch point) so the diff is the agent's *change set*.
    pub base_ref: Option<String>,
    pub submitted_at: DateTime<Utc>,
    pub status: BuildStatus,
    /// Row 10 Item 5 — the composed canonical-diff cache key + its
    /// dimensions. `None` if key derivation failed (cache disabled for
    /// this build; it still builds normally).
    pub cache_key: Option<CanonicalDiffKey>,
    /// `hit` | `miss` | `disabled` | `populated` | `populate_failed` —
    /// the content-addressed cache lifecycle for this submission.
    pub cache_outcome: Option<String>,
    /// True when this submission was satisfied entirely from the
    /// content-addressed cache — no cargo ran, no build permit consumed.
    pub cache_hit: bool,
    /// Last ~500 lines of cargo stdout. Populated on completion.
    pub stdout_tail: Vec<String>,
    /// Last ~500 lines of cargo stderr. Populated on completion.
    pub stderr_tail: Vec<String>,
}

/// Bounded in-memory submission store.
///
/// Terminal submissions (Succeeded / Failed) get evicted oldest-first
/// when the map exceeds `cap`. In-flight submissions (Queued / Running)
/// are never evicted — they're load-bearing.
pub struct BuildSubmissionStore {
    pub submissions: RwLock<HashMap<Uuid, Arc<RwLock<BuildSubmission>>>>,
    pub cap: usize,
}

impl BuildSubmissionStore {
    pub fn new(cap: usize) -> Self {
        Self {
            submissions: RwLock::new(HashMap::new()),
            cap: cap.max(16),
        }
    }

    pub async fn insert(&self, sub: BuildSubmission) -> Arc<RwLock<BuildSubmission>> {
        let id = sub.id;
        let arc = Arc::new(RwLock::new(sub));
        let mut map = self.submissions.write().await;
        map.insert(id, arc.clone());

        // Cap-enforcement: if over cap, evict oldest *terminal* entries.
        // Collect (id, submitted_at, is_terminal) into a vec, sort, and
        // remove the oldest terminals until we're under cap. Avoids
        // borrowing issues by iterating after the snapshot.
        if map.len() > self.cap {
            let mut candidates: Vec<(Uuid, DateTime<Utc>, bool)> = Vec::with_capacity(map.len());
            for (k, v) in map.iter() {
                let v = v.read().await;
                candidates.push((*k, v.submitted_at, v.status.is_terminal()));
            }
            candidates.sort_by_key(|(_, ts, _)| *ts);
            let to_remove = map.len() - self.cap;
            let mut removed = 0;
            for (k, _, is_term) in candidates {
                if removed >= to_remove {
                    break;
                }
                if is_term {
                    map.remove(&k);
                    removed += 1;
                }
            }
        }
        arc
    }

    pub async fn get(&self, id: &Uuid) -> Option<Arc<RwLock<BuildSubmission>>> {
        self.submissions.read().await.get(id).cloned()
    }
}

/// Submit a new build. Returns the assigned `Uuid` immediately and
/// spawns a background task that runs the cargo invocation when a
/// build-pool permit is available.
pub async fn submit(
    state: SharedState,
    worktree_path: PathBuf,
    build_kind: BuildKind,
    agent_id: Option<String>,
    package: Option<String>,
    features: Vec<String>,
    base_ref: Option<String>,
) -> Result<Uuid, String> {
    // Validate the worktree path exists and looks like a cargo workspace.
    let cargo_toml = worktree_path.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Err(format!(
            "worktree_path {:?} does not contain Cargo.toml — \
             /build/submit requires a cargo workspace root",
            worktree_path
        ));
    }

    let id = Uuid::new_v4();
    let now = Utc::now();
    let submission = BuildSubmission {
        id,
        worktree_path: worktree_path.clone(),
        build_kind,
        agent_id: agent_id.clone(),
        package: package.clone(),
        features: features.clone(),
        base_ref: base_ref.clone(),
        submitted_at: now,
        status: BuildStatus::Queued,
        cache_key: None,
        cache_outcome: None,
        cache_hit: false,
        stdout_tail: Vec::new(),
        stderr_tail: Vec::new(),
    };

    let store = state.build_submissions.clone();
    let arc = store.insert(submission).await;

    // Background task: acquire permit, run cargo, update submission.
    // Independent of the HTTP handler's task — the handler returns
    // immediately with the id.
    let state_bg = state.clone();
    let arc_bg = arc.clone();
    tokio::spawn(async move {
        run_submission(state_bg, arc_bg).await;
    });

    Ok(id)
}

async fn run_submission(state: SharedState, sub_arc: Arc<RwLock<BuildSubmission>>) {
    let (worktree_path, build_kind, agent_id, package, features, base_ref) = {
        let sub = sub_arc.read().await;
        (
            sub.worktree_path.clone(),
            sub.build_kind,
            sub.agent_id.clone(),
            sub.package.clone(),
            sub.features.clone(),
            sub.base_ref.clone(),
        )
    };

    // ---- Row 10 Item 5: derive the canonical-diff cache key. -------
    // Best-effort: a derivation failure disables the cache for this
    // build but never fails it.
    let cache_started = std::time::Instant::now();
    let key = derive_cache_key(&state, &worktree_path, build_kind, base_ref.as_deref()).await;
    {
        let mut sub = sub_arc.write().await;
        sub.cache_key = key.clone();
        sub.cache_outcome = Some(if key.is_some() { "miss" } else { "disabled" }.into());
    }

    // ---- Row 10 Item 6: AC lookup before dispatch. ----------------
    // A hit materializes the cached artifact into the worktree and
    // short-circuits the build entirely — no permit consumed, no cargo.
    if let Some(k) = &key {
        if let Some(ar) = state.bazel_remote.ac_get(&k.key).await {
            if try_materialize_hit(&state, &worktree_path, &ar).await {
                let elapsed = cache_started.elapsed().as_secs_f64();
                let now = Utc::now();
                {
                    let mut sub = sub_arc.write().await;
                    sub.cache_hit = true;
                    sub.cache_outcome = Some("hit".into());
                    sub.status = BuildStatus::Succeeded {
                        started_at: now,
                        finished_at: now,
                        duration_secs: elapsed,
                    };
                }
                state
                    .cache_telemetry
                    .record(CacheEvent {
                        worker: agent_id.clone().unwrap_or_else(|| "unknown".into()),
                        repo: repo_label(&worktree_path),
                        profile: cache_key::profile_for(build_kind),
                        ac_hit: true,
                        ac_write_ok: None,
                        timestamp_would_reuse: false,
                    })
                    .await;
                state
                    .logs
                    .emit(
                        LogSource::Build,
                        LogLevel::Info,
                        format!(
                            "[/build/submit] CACHE HIT key={} — skipped build, \
                             materialized artifact in {:.3}s (agent={:?})",
                            &k.key[..16.min(k.key.len())],
                            elapsed,
                            agent_id
                        ),
                    )
                    .await;
                return;
            }
        }
    }

    // Acquire permit on the shared build pool. Held across the entire
    // cargo invocation. Released on drop at the end of this fn.
    state
        .build_pool
        .queue_depth
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let permit_result = state.build_pool.permits.clone().acquire_owned().await;
    state
        .build_pool
        .queue_depth
        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let _permit = match permit_result {
        Ok(p) => p,
        Err(_) => {
            let mut sub = sub_arc.write().await;
            let now = Utc::now();
            sub.status = BuildStatus::Failed {
                started_at: now,
                finished_at: now,
                duration_secs: 0.0,
                exit_code: None,
                error: "build pool semaphore closed".to_string(),
            };
            return;
        }
    };

    let started_at = Utc::now();
    {
        let mut sub = sub_arc.write().await;
        sub.status = BuildStatus::Running { started_at };
    }

    state
        .logs
        .emit(
            LogSource::Build,
            LogLevel::Info,
            format!(
                "[/build/submit] running cargo {:?} in {:?} (agent={:?})",
                build_kind, worktree_path, agent_id
            ),
        )
        .await;

    let mut args: Vec<String> = build_kind
        .cargo_args()
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(p) = &package {
        args.push("-p".to_string());
        args.push(p.clone());
    }
    for f in &features {
        args.push("--features".to_string());
        args.push(f.clone());
    }
    args.push("--locked".to_string());

    let cargo_bin = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(&cargo_bin);
    cmd.args(&args)
        .current_dir(&worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Inherit sccache + path-remap env from the supervisor's shell. The
    // .cargo/config.toml inside the worktree adds --remap-path-prefix
    // (Row 2 Phase 3) automatically; this command path doesn't override it.

    let spawn_result = cmd.spawn();
    let mut child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            let now = Utc::now();
            let mut sub = sub_arc.write().await;
            sub.status = BuildStatus::Failed {
                started_at,
                finished_at: now,
                duration_secs: (now - started_at).num_milliseconds() as f64 / 1000.0,
                exit_code: None,
                error: format!("failed to spawn cargo: {e}"),
            };
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Bounded ring buffers (~500 lines each) for tail capture.
    const TAIL_CAP: usize = 500;
    let stdout_handle = tokio::spawn(async move {
        let mut buf = Vec::with_capacity(TAIL_CAP);
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if buf.len() == TAIL_CAP {
                buf.remove(0);
            }
            buf.push(line);
        }
        buf
    });
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::with_capacity(TAIL_CAP);
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if buf.len() == TAIL_CAP {
                buf.remove(0);
            }
            buf.push(line);
        }
        buf
    });

    let exit_status = child.wait().await;
    let stdout_tail = stdout_handle.await.unwrap_or_default();
    let stderr_tail = stderr_handle.await.unwrap_or_default();

    let finished_at = Utc::now();
    let duration_secs = (finished_at - started_at).num_milliseconds() as f64 / 1000.0;
    let succeeded;
    {
        let mut sub = sub_arc.write().await;
        sub.stdout_tail = stdout_tail;
        sub.stderr_tail = stderr_tail;
        sub.status = match exit_status {
            Ok(s) if s.success() => {
                succeeded = true;
                BuildStatus::Succeeded {
                    started_at,
                    finished_at,
                    duration_secs,
                }
            }
            Ok(s) => {
                succeeded = false;
                BuildStatus::Failed {
                    started_at,
                    finished_at,
                    duration_secs,
                    exit_code: s.code(),
                    error: format!("cargo exited with status {s}"),
                }
            }
            Err(e) => {
                succeeded = false;
                BuildStatus::Failed {
                    started_at,
                    finished_at,
                    duration_secs,
                    exit_code: None,
                    error: format!("failed to await cargo: {e}"),
                }
            }
        };
    }

    // ---- Row 10 Item 7: populate the content-addressed cache. ------
    // Only on a green build, and only if Item 5 produced a key. Writes
    // CAS blobs first, then a validated REAPI ActionResult to AC (so
    // bazel-remote's re-enabled HTTP AC validation passes). Best-effort
    // and never mutates the build's success — a cache-write failure is
    // logged and recorded, the build still Succeeded.
    let write_ok = if succeeded {
        if let Some(k) = &key {
            let r = populate_cache(
                &state,
                &worktree_path,
                build_kind,
                package.as_deref(),
                started_at,
                k,
            )
            .await;
            let mut sub = sub_arc.write().await;
            sub.cache_outcome = Some(if r { "populated" } else { "populate_failed" }.into());
            Some(r)
        } else {
            None
        }
    } else {
        None
    };

    // Dual-write shadow telemetry (tracker step B). `/build/submit` had
    // no prior reuse path, so `timestamp_would_reuse` is honestly
    // `false` here — every hash hit is pure cross-worktree uplift the
    // legacy timestamp heuristic would have missed.
    if key.is_some() {
        state
            .cache_telemetry
            .record(CacheEvent {
                worker: agent_id.clone().unwrap_or_else(|| "unknown".into()),
                repo: repo_label(&worktree_path),
                profile: cache_key::profile_for(build_kind),
                ac_hit: false,
                ac_write_ok: write_ok,
                timestamp_would_reuse: false,
            })
            .await;
    }
}

/// Label a worktree by its repo dir name for telemetry breakdown.
fn repo_label(worktree: &std::path::Path) -> String {
    worktree
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into())
}

/// Item 5 wrapper: resolve the script + derive the key. Logs (but
/// swallows) failures — the cache is an accelerator, never a gate.
async fn derive_cache_key(
    state: &SharedState,
    worktree: &std::path::Path,
    kind: BuildKind,
    base_ref: Option<&str>,
) -> Option<CanonicalDiffKey> {
    let script = cache_key::resolve_script(&state.config.project_dir, worktree)?;
    match cache_key::compute(&script, worktree, base_ref, kind).await {
        Ok(k) => Some(k),
        Err(e) => {
            state
                .logs
                .emit(
                    LogSource::Build,
                    LogLevel::Warn,
                    format!("[/build/submit] cache-key derivation failed (cache off for this build): {e}"),
                )
                .await;
            None
        }
    }
}

/// Item 6 hit path: download every `ActionResult` output file from CAS
/// and write it to its worktree-relative path. Returns false (→ treat
/// as miss, build normally) if any blob is missing or unwritable, so a
/// partial cache never produces a corrupt worktree.
async fn try_materialize_hit(
    state: &SharedState,
    worktree: &std::path::Path,
    ar: &ActionResult,
) -> bool {
    if ar.output_files.is_empty() {
        return false;
    }
    for of in &ar.output_files {
        let bytes = match state.bazel_remote.cas_get(&of.digest.hash).await {
            Some(b) => b,
            None => return false,
        };
        if bytes.len() as i64 != of.digest.size_bytes {
            return false;
        }
        let dest = worktree.join(&of.path);
        if let Some(parent) = dest.parent() {
            if tokio::fs::create_dir_all(parent).await.is_err() {
                return false;
            }
        }
        if tokio::fs::write(&dest, &bytes).await.is_err() {
            return false;
        }
        #[cfg(unix)]
        if of.is_executable {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).await;
        }
    }
    true
}

/// Item 7 populate path: discover the build's primary artifact(s),
/// `PUT /cas/<sha>` each, then `PUT /ac/<key>` a validated REAPI
/// `ActionResult`. CAS-before-AC ordering is required: bazel-remote's
/// re-enabled validator rejects an AC entry whose digests aren't
/// already in CAS.
async fn populate_cache(
    state: &SharedState,
    worktree: &std::path::Path,
    kind: BuildKind,
    package: Option<&str>,
    built_after: DateTime<Utc>,
    key: &CanonicalDiffKey,
) -> bool {
    let artifacts = discover_artifacts(worktree, kind, package, built_after);
    if artifacts.is_empty() {
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Warn,
                format!(
                    "[/build/submit] no artifact discovered under target/{} — AC not populated",
                    cache_key::target_subdir(kind)
                ),
            )
            .await;
        return false;
    }
    let mut output_files = Vec::with_capacity(artifacts.len());
    for (rel, abs, is_exec) in &artifacts {
        let bytes = match tokio::fs::read(abs).await {
            Ok(b) => b,
            Err(_) => return false,
        };
        let sha = {
            let mut h = Sha256::new();
            h.update(&bytes);
            hex::encode(h.finalize())
        };
        let size = bytes.len() as i64;
        if !state.bazel_remote.cas_put(&sha, bytes).await {
            return false;
        }
        output_files.push(OutputFile {
            path: rel.clone(),
            digest: ReapiDigest {
                hash: sha,
                size_bytes: size,
            },
            is_executable: *is_exec,
        });
    }
    let ar = ActionResult {
        output_files,
        exit_code: 0,
    };
    let ok = state.bazel_remote.ac_put(&key.key, &ar).await;
    if ok {
        state
            .logs
            .emit(
                LogSource::Build,
                LogLevel::Info,
                format!(
                    "[/build/submit] AC populated key={} ({} artifact(s))",
                    &key.key[..16.min(key.key.len())],
                    ar.output_files.len()
                ),
            )
            .await;
    }
    ok
}

/// Discover the build's output artifact(s). Name-based v1: the package
/// (or worktree dir) binary under `target/<debug|release>/`, plus a
/// `.exe` on Windows. Only files modified at/after `built_after` are
/// taken, so a stale binary from a prior profile isn't cached as this
/// build's output. Multi-binary / cdylib crates are out of scope for
/// v1 (Item 8 can switch to `cargo build --message-format=json` for
/// exhaustive artifact capture).
fn discover_artifacts(
    worktree: &std::path::Path,
    kind: BuildKind,
    package: Option<&str>,
    built_after: DateTime<Utc>,
) -> Vec<(String, std::path::PathBuf, bool)> {
    let sub = cache_key::target_subdir(kind);
    let dir = worktree.join("target").join(sub);
    let name = package
        .map(|s| s.to_string())
        .or_else(|| {
            worktree
                .file_name()
                .map(|n| n.to_string_lossy().replace('-', "_"))
        })
        .unwrap_or_default();
    if name.is_empty() {
        return Vec::new();
    }
    let candidates = [
        format!("{name}.exe"),
        name.clone(),
        format!("{}.exe", name.replace('_', "-")),
        name.replace('_', "-"),
    ];
    let cutoff: std::time::SystemTime = built_after.into();
    let mut out = Vec::new();
    for c in candidates.iter() {
        let abs = dir.join(c);
        if !abs.is_file() {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&abs) {
            // Allow a small clock-skew grace: artifact mtime must be at
            // least as new as build start (minus 2s tolerance).
            if let Ok(mtime) = meta.modified() {
                let grace = cutoff - std::time::Duration::from_secs(2);
                if mtime < grace {
                    continue;
                }
            }
        }
        let rel = format!("target/{sub}/{c}");
        out.push((rel, abs, true));
        break; // primary binary only in v1
    }
    out
}

// ----------------------------------------------------------------
// Tests
// ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_kind_serializes_snake_case() {
        let s = serde_json::to_string(&BuildKind::Check).unwrap();
        assert_eq!(s, "\"check\"");
        let s = serde_json::to_string(&BuildKind::Release).unwrap();
        assert_eq!(s, "\"release\"");
    }

    #[test]
    fn build_kind_cargo_args() {
        assert_eq!(BuildKind::Check.cargo_args(), vec!["check"]);
        assert_eq!(BuildKind::Build.cargo_args(), vec!["build"]);
        assert_eq!(BuildKind::Release.cargo_args(), vec!["build", "--release"]);
        assert_eq!(BuildKind::Test.cargo_args(), vec!["test", "--no-run"]);
    }

    #[test]
    fn build_status_terminal_predicate() {
        let now = Utc::now();
        assert!(!BuildStatus::Queued.is_terminal());
        assert!(!BuildStatus::Running { started_at: now }.is_terminal());
        assert!(BuildStatus::Succeeded {
            started_at: now,
            finished_at: now,
            duration_secs: 0.0
        }
        .is_terminal());
        assert!(BuildStatus::Failed {
            started_at: now,
            finished_at: now,
            duration_secs: 0.0,
            exit_code: Some(1),
            error: "x".to_string()
        }
        .is_terminal());
    }

    #[tokio::test]
    async fn store_inserts_and_retrieves() {
        let store = BuildSubmissionStore::new(100);
        let id = Uuid::new_v4();
        let sub = BuildSubmission {
            id,
            worktree_path: PathBuf::from("/tmp/x"),
            build_kind: BuildKind::Check,
            agent_id: None,
            package: None,
            features: vec![],
            base_ref: None,
            submitted_at: Utc::now(),
            status: BuildStatus::Queued,
            cache_key: None,
            cache_outcome: None,
            cache_hit: false,
            stdout_tail: vec![],
            stderr_tail: vec![],
        };
        store.insert(sub).await;
        let found = store.get(&id).await;
        assert!(found.is_some());
    }

    #[tokio::test]
    async fn store_evicts_oldest_terminal_when_over_cap() {
        let store = BuildSubmissionStore::new(16); // cap floored at 16
                                                   // Insert 17 terminal entries; the cap is 16, so one must evict.
        let now = Utc::now();
        for i in 0..17u32 {
            let sub = BuildSubmission {
                id: Uuid::new_v4(),
                worktree_path: PathBuf::from("/tmp/x"),
                build_kind: BuildKind::Check,
                agent_id: None,
                package: None,
                features: vec![],
                base_ref: None,
                submitted_at: now + chrono::Duration::milliseconds(i as i64),
                status: BuildStatus::Succeeded {
                    started_at: now,
                    finished_at: now,
                    duration_secs: 0.0,
                },
                cache_key: None,
                cache_outcome: None,
                cache_hit: false,
                stdout_tail: vec![],
                stderr_tail: vec![],
            };
            store.insert(sub).await;
        }
        assert_eq!(store.submissions.read().await.len(), 16);
    }

    #[tokio::test]
    async fn store_does_not_evict_in_flight() {
        let store = BuildSubmissionStore::new(16);
        let now = Utc::now();
        // First, fill with 16 queued entries (none terminal).
        let mut ids = Vec::new();
        for i in 0..16u32 {
            let id = Uuid::new_v4();
            ids.push(id);
            let sub = BuildSubmission {
                id,
                worktree_path: PathBuf::from("/tmp/x"),
                build_kind: BuildKind::Check,
                agent_id: None,
                package: None,
                features: vec![],
                base_ref: None,
                submitted_at: now + chrono::Duration::milliseconds(i as i64),
                status: BuildStatus::Queued,
                cache_key: None,
                cache_outcome: None,
                cache_hit: false,
                stdout_tail: vec![],
                stderr_tail: vec![],
            };
            store.insert(sub).await;
        }
        // Now add one more queued — cap would force eviction, but every
        // entry is in-flight so nothing terminal to evict. Map grows.
        let extra = BuildSubmission {
            id: Uuid::new_v4(),
            worktree_path: PathBuf::from("/tmp/x"),
            build_kind: BuildKind::Check,
            agent_id: None,
            package: None,
            features: vec![],
            base_ref: None,
            submitted_at: now + chrono::Duration::milliseconds(100),
            status: BuildStatus::Queued,
            cache_key: None,
            cache_outcome: None,
            cache_hit: false,
            stdout_tail: vec![],
            stderr_tail: vec![],
        };
        store.insert(extra).await;
        // 17 entries, all in-flight — no evictions possible.
        assert_eq!(store.submissions.read().await.len(), 17);
        // Confirm none of the originals were dropped.
        for id in ids {
            assert!(store.get(&id).await.is_some());
        }
    }
}
