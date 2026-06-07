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
    /// Spawn-test outcome, when this submission was created via
    /// [`submit_spawn`] (`POST /runners/spawn-test`). Carries the exact HTTP
    /// status + body the synchronous spawn-test path would have returned —
    /// build provenance, runner id/port, exe metadata, frontend-stale, etc.
    /// `None` for plain `/build/submit` cargo submissions and until the spawn
    /// build reaches a terminal state. See Item 6 of
    /// `2026-06-03-ui-bridge-verification-friction-improvements`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn: Option<SpawnOutcome>,
    /// Terminal outcome of a *detached* HTTP-action submission created via
    /// [`submit_detached`] — the same state-machine seam as [`submit_spawn`] but
    /// with no runner/port. Used by `POST /runner/fix-and-rebuild` so a 10-20min
    /// live-tree rebuild survives the caller disconnecting: the handler returns
    /// 202 with this submission's id, the build runs to completion (provenance
    /// sidecar + LKG promotion + slot state) in a detached task, and the result
    /// is recoverable via `GET /build/:id/status`. `None` for spawn/cargo
    /// submissions and until the action reaches a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detached: Option<DetachedOutcome>,
}

/// Terminal outcome of a detached HTTP-action submission ([`submit_detached`]).
/// Mirrors [`SpawnOutcome`] minus the runner port — carries the exact HTTP
/// status + body the synchronous handler would have returned, recoverable via
/// `GET /build/:id/status` after the caller's connection is gone.
#[derive(Debug, Clone, Serialize)]
pub struct DetachedOutcome {
    /// HTTP status the synchronous path would have returned (200, 500, …).
    pub http_status: u16,
    /// The full response body.
    pub body: serde_json::Value,
}

/// Terminal outcome of a spawn-test build submission. Mirrors the
/// synchronous `POST /runners/spawn-test` response so a `GET /build/:id/status`
/// poll can recover the full result after a supervisor restart killed the
/// original long-poll connection (Item 6).
#[derive(Debug, Clone, Serialize)]
pub struct SpawnOutcome {
    /// HTTP status the synchronous path would have returned (200, 503, …).
    pub http_status: u16,
    /// Reserved temp-runner port (echoed up-front in the 202 so callers can
    /// target the runner before the build finishes).
    pub port: u16,
    /// The full response body (same shape as the sync 200/5xx body).
    pub body: serde_json::Value,
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
        spawn: None,
        detached: None,
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

/// Register a spawn-test build submission and drive it through the SAME
/// state machine as `/build/submit`, so the supervisor has exactly one build
/// state machine (Item 6 of
/// `2026-06-03-ui-bridge-verification-friction-improvements`).
///
/// `exec` is the spawn build+spawn+probe+assemble future. It runs in a
/// detached background task; its `(http_status, body)` result is stored on
/// the submission's terminal state alongside the reserved `port`. The
/// submission moves `Queued → Running → Succeeded/Failed` exactly like a
/// cargo submission:
///
/// - The future is awaited under the `Running` state.
/// - A `2xx` http_status records `Succeeded`; anything else records `Failed`
///   (build/spawn/frontend error). `error` carries a short reason pulled from
///   the body so `GET /build/:id/status` is useful without parsing `spawn`.
///
/// Returns the submission id immediately. The async spawn-test path returns a
/// 202 with this id + `port`; the sync path awaits terminal via
/// [`await_terminal`] and unwraps `spawn`.
pub fn submit_spawn<F>(
    store: Arc<BuildSubmissionStore>,
    worktree_path: PathBuf,
    agent_id: Option<String>,
    port: u16,
    exec: F,
) -> (Uuid, Arc<RwLock<BuildSubmission>>)
where
    F: std::future::Future<Output = (u16, serde_json::Value, Vec<String>)> + Send + 'static,
{
    let id = Uuid::new_v4();
    let now = Utc::now();
    let submission = BuildSubmission {
        id,
        worktree_path,
        build_kind: BuildKind::Build,
        agent_id,
        package: None,
        features: vec!["custom-protocol".to_string()],
        base_ref: None,
        submitted_at: now,
        status: BuildStatus::Queued,
        cache_key: None,
        cache_outcome: None,
        cache_hit: false,
        stdout_tail: Vec::new(),
        stderr_tail: Vec::new(),
        spawn: None,
        detached: None,
    };

    // Insert synchronously-enough: callers await this fn (it's not async, so
    // we block the insert onto the same task via a dedicated runtime handle is
    // overkill). Instead we insert from the background task BEFORE running
    // exec, and return the Arc the caller already holds. To keep the id
    // queryable the instant this returns, we build the Arc here and insert it
    // into the store from the spawned task's first step.
    let arc = Arc::new(RwLock::new(submission));
    let arc_ret = arc.clone();
    let store_bg = store.clone();
    tokio::spawn(async move {
        // Register in the store so `GET /build/:id/status` can find it.
        {
            let mut map = store_bg.submissions.write().await;
            map.insert(id, arc.clone());
        }
        let started_at = Utc::now();
        {
            let mut sub = arc.write().await;
            sub.status = BuildStatus::Running { started_at };
        }

        let (http_status, mut body, stderr_tail) = exec.await;
        // `build_id` = this submission's id. Inject it into the stored outcome
        // body so BOTH the sync path (which returns this stored body) and the
        // async poll (`GET /build/:id/status`) expose `build_id` = submission_id
        // — one build-identity abstraction, no second token. Only inject when
        // the body is a JSON object (every spawn-test outcome is).
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "build_id".to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
        let finished_at = Utc::now();
        let duration_secs = (finished_at - started_at).num_milliseconds() as f64 / 1000.0;
        let succeeded = (200..300).contains(&http_status);

        let mut sub = arc.write().await;
        // Issue 3 fix part 1: the spawn build's full cargo stderr (split into
        // lines by the exec future) flows into the submission's `stderr_tail`
        // so `GET /build/:id/status` returns the REAL compiler error on a
        // cargo-pool build failure — the cargo-pool path previously never fed
        // this field, leaving it empty (`stderr_tail: []`).
        sub.stderr_tail = stderr_tail;
        sub.spawn = Some(SpawnOutcome {
            http_status,
            port,
            body: body.clone(),
        });
        sub.status = if succeeded {
            BuildStatus::Succeeded {
                started_at,
                finished_at,
                duration_secs,
            }
        } else {
            // Pull a short reason from the body (build_result.error / error /
            // message) so the terminal status is self-describing.
            let reason = body
                .get("build_result")
                .and_then(|br| br.get("error"))
                .and_then(|e| e.as_str())
                .or_else(|| body.get("error").and_then(|e| e.as_str()))
                .or_else(|| body.get("message").and_then(|m| m.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("spawn-test returned HTTP {http_status}"));
            BuildStatus::Failed {
                started_at,
                finished_at,
                duration_secs,
                exit_code: None,
                error: reason,
            }
        };
    });

    (id, arc_ret)
}

/// Register a *detached* HTTP-action submission and drive it through the SAME
/// state machine as [`submit_spawn`] / `/build/submit`, but without a runner or
/// port. Used by `POST /runner/fix-and-rebuild`: the `exec` future is the full
/// live-tree rebuild (cargo build + provenance sidecar + LKG promotion + slot
/// bookkeeping) and runs in a *detached* background task so a client disconnect
/// (E2E timeout, `curl -m 30`, Ctrl-C) can no longer cancel it mid-flight.
///
/// The submission moves `Queued → Running → Succeeded/Failed` exactly like
/// [`submit_spawn`]: the future is awaited under `Running`, a `2xx` http_status
/// records `Succeeded`, anything else `Failed` with a short reason pulled from
/// the body. The `(http_status, body)` result is stored under
/// [`BuildSubmission::detached`] and recoverable via `GET /build/:id/status`.
///
/// Returns the submission id + Arc immediately. The handler returns a 202 with
/// the id; there is intentionally no synchronous await path (the whole point is
/// detachment), but callers that want to block can [`await_terminal`] the Arc.
pub fn submit_detached<F>(
    store: Arc<BuildSubmissionStore>,
    worktree_path: PathBuf,
    agent_id: Option<String>,
    exec: F,
) -> (Uuid, Arc<RwLock<BuildSubmission>>)
where
    F: std::future::Future<Output = (u16, serde_json::Value)> + Send + 'static,
{
    let id = Uuid::new_v4();
    let now = Utc::now();
    let submission = BuildSubmission {
        id,
        worktree_path,
        build_kind: BuildKind::Build,
        agent_id,
        package: None,
        features: vec!["custom-protocol".to_string()],
        base_ref: None,
        submitted_at: now,
        status: BuildStatus::Queued,
        cache_key: None,
        cache_outcome: None,
        cache_hit: false,
        stdout_tail: Vec::new(),
        stderr_tail: Vec::new(),
        spawn: None,
        detached: None,
    };

    let arc = Arc::new(RwLock::new(submission));
    let arc_ret = arc.clone();
    let store_bg = store.clone();
    tokio::spawn(async move {
        // Register in the store so `GET /build/:id/status` can find it.
        {
            let mut map = store_bg.submissions.write().await;
            map.insert(id, arc.clone());
        }
        let started_at = Utc::now();
        {
            let mut sub = arc.write().await;
            sub.status = BuildStatus::Running { started_at };
        }

        let (http_status, mut body) = exec.await;
        // Inject `build_id` = submission id so the stored body self-identifies,
        // mirroring submit_spawn.
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "build_id".to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
        let finished_at = Utc::now();
        let duration_secs = (finished_at - started_at).num_milliseconds() as f64 / 1000.0;
        let succeeded = (200..300).contains(&http_status);

        let mut sub = arc.write().await;
        sub.detached = Some(DetachedOutcome {
            http_status,
            body: body.clone(),
        });
        sub.status = if succeeded {
            BuildStatus::Succeeded {
                started_at,
                finished_at,
                duration_secs,
            }
        } else {
            let reason = body
                .get("error")
                .and_then(|e| e.as_str())
                .or_else(|| body.get("message").and_then(|m| m.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("detached action returned HTTP {http_status}"));
            BuildStatus::Failed {
                started_at,
                finished_at,
                duration_secs,
                exit_code: None,
                error: reason,
            }
        };
    });

    (id, arc_ret)
}

/// Block until `sub` reaches a terminal `BuildStatus`. Polls the submission's
/// own lock every 100ms — the spawn build/probe is the long pole (seconds to
/// minutes), so a coarse poll is fine and avoids threading a notifier through
/// the state machine. Used by the SYNCHRONOUS spawn-test path so both sync and
/// async requests flow through the same submission.
pub async fn await_terminal(sub: &Arc<RwLock<BuildSubmission>>) {
    loop {
        if sub.read().await.status.is_terminal() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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
            spawn: None,
            detached: None,
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
                spawn: None,
                detached: None,
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
                spawn: None,
                detached: None,
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
            spawn: None,
            detached: None,
        };
        store.insert(extra).await;
        // 17 entries, all in-flight — no evictions possible.
        assert_eq!(store.submissions.read().await.len(), 17);
        // Confirm none of the originals were dropped.
        for id in ids {
            assert!(store.get(&id).await.is_some());
        }
    }

    // ---------- spawn submission state machine (Item 6) ----------

    #[tokio::test]
    async fn submit_spawn_reaches_terminal_succeeded_and_stores_outcome() {
        let store = Arc::new(BuildSubmissionStore::new(100));
        // Fake spawn exec: returns a 200 + a body shaped like the real
        // spawn-test response. No cargo, no runner — exercises the state
        // machine + outcome plumbing only.
        let (id, arc) = submit_spawn(
            store.clone(),
            PathBuf::from("/tmp/runner/src-tauri"),
            Some("agent-x".into()),
            9881,
            async {
                (
                    200,
                    serde_json::json!({
                        "id": "test-abc",
                        "port": 9881,
                        "status": "started",
                        "build_result": { "state": "built", "succeeded": true },
                    }),
                    Vec::new(),
                )
            },
        );

        // The id is queryable in the store (registered by the bg task).
        await_terminal(&arc).await;
        let found = store
            .get(&id)
            .await
            .expect("submission registered in store");
        let sub = found.read().await;
        assert!(sub.status.is_terminal());
        assert!(matches!(sub.status, BuildStatus::Succeeded { .. }));
        let outcome = sub.spawn.as_ref().expect("spawn outcome recorded");
        assert_eq!(outcome.http_status, 200);
        assert_eq!(outcome.port, 9881);
        assert_eq!(outcome.body["id"], "test-abc");
        assert_eq!(outcome.body["build_result"]["succeeded"], true);
        // build_id is injected into the stored outcome body = submission id.
        assert_eq!(
            outcome.body["build_id"],
            id.to_string(),
            "stored spawn outcome body must carry build_id = submission_id"
        );
    }

    #[tokio::test]
    async fn submit_spawn_non_2xx_records_failed_with_reason() {
        let store = Arc::new(BuildSubmissionStore::new(100));
        let (_id, arc) = submit_spawn(
            store.clone(),
            PathBuf::from("/tmp/runner/src-tauri"),
            None,
            9882,
            async {
                (
                    500,
                    serde_json::json!({
                        "build_result": {
                            "state": "failed",
                            "succeeded": false,
                            "error": "error[E0277]: trait bound not satisfied",
                        },
                    }),
                    vec![
                        "   Compiling qontinui-runner v0.1.0".to_string(),
                        "error[E0277]: trait bound not satisfied".to_string(),
                    ],
                )
            },
        );

        await_terminal(&arc).await;
        let sub = arc.read().await;
        match &sub.status {
            BuildStatus::Failed { error, .. } => {
                assert!(
                    error.contains("E0277"),
                    "failed status should carry the build error reason; got {error}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let outcome = sub
            .spawn
            .as_ref()
            .expect("spawn outcome recorded even on failure");
        assert_eq!(outcome.http_status, 500);
        // Issue 3 fix part 1: the full stderr the exec future surfaced lands in
        // the submission's `stderr_tail` (was empty on the cargo-pool path).
        assert!(
            sub.stderr_tail.iter().any(|l| l.contains("E0277")),
            "spawn failure must populate stderr_tail with the real cargo stderr; got {:?}",
            sub.stderr_tail
        );
    }

    #[tokio::test]
    async fn spawn_outcome_serializes_only_when_present() {
        // A plain cargo submission must NOT serialize a `spawn` field (it's
        // `skip_serializing_if = "Option::is_none"`); a spawn submission must.
        let cargo = BuildSubmission {
            id: Uuid::new_v4(),
            worktree_path: PathBuf::from("/tmp/x"),
            build_kind: BuildKind::Build,
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
            spawn: None,
            detached: None,
        };
        let v = serde_json::to_value(&cargo).unwrap();
        assert!(
            v.get("spawn").is_none(),
            "cargo submission must omit `spawn`"
        );

        let mut spawn_sub = cargo.clone();
        spawn_sub.spawn = Some(SpawnOutcome {
            http_status: 200,
            port: 9881,
            body: serde_json::json!({"id": "test-abc"}),
        });
        let v = serde_json::to_value(&spawn_sub).unwrap();
        assert_eq!(v["spawn"]["http_status"], 200);
        assert_eq!(v["spawn"]["port"], 9881);
    }

    // ---------- detached submission state machine (fix-and-rebuild) ----------

    #[tokio::test]
    async fn submit_detached_reaches_terminal_succeeded_and_stores_outcome() {
        let store = Arc::new(BuildSubmissionStore::new(100));
        let (id, arc) = submit_detached(
            store.clone(),
            PathBuf::from("/tmp/runner/src-tauri"),
            Some("agent-y".into()),
            async {
                (
                    200,
                    serde_json::json!({"status": "ok", "message": "Runner rebuilt"}),
                )
            },
        );

        await_terminal(&arc).await;
        let found = store
            .get(&id)
            .await
            .expect("detached submission registered in store");
        let sub = found.read().await;
        assert!(matches!(sub.status, BuildStatus::Succeeded { .. }));
        let outcome = sub.detached.as_ref().expect("detached outcome recorded");
        assert_eq!(outcome.http_status, 200);
        assert_eq!(outcome.body["status"], "ok");
        // build_id injected = submission id.
        assert_eq!(outcome.body["build_id"], id.to_string());
        // A detached submission carries no spawn outcome.
        assert!(sub.spawn.is_none());
    }

    #[tokio::test]
    async fn submit_detached_non_2xx_records_failed_with_reason() {
        let store = Arc::new(BuildSubmissionStore::new(100));
        let (_id, arc) =
            submit_detached(store.clone(), PathBuf::from("/tmp/runner"), None, async {
                (
                    500,
                    serde_json::json!({"status": "error", "error": "cargo exited with status 101"}),
                )
            });

        await_terminal(&arc).await;
        let sub = arc.read().await;
        match &sub.status {
            BuildStatus::Failed { error, .. } => assert!(
                error.contains("cargo exited"),
                "failed status must carry the error reason; got {error}"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(sub.detached.as_ref().unwrap().http_status, 500);
    }

    /// The whole point of detachment: the exec future runs to completion even
    /// when the caller drops the returned Arc immediately (simulating an HTTP
    /// client disconnect — nothing awaits the build). A side-channel flag is set
    /// from inside the exec future; after a brief yield it must be observed set.
    #[tokio::test]
    async fn submit_detached_completes_after_caller_drops_handle() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let store = Arc::new(BuildSubmissionStore::new(100));
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in = ran.clone();
        let (id, arc) = submit_detached(
            store.clone(),
            PathBuf::from("/tmp/runner"),
            None,
            async move {
                ran_in.store(true, Ordering::SeqCst);
                (200, serde_json::json!({"status": "ok"}))
            },
        );
        // Drop the handle the handler would have returned — as if the HTTP
        // connection went away. The detached task is independent.
        drop(arc);

        // Poll the store (not the dropped Arc) until terminal. The bg task
        // registers the submission as its first step, so retry briefly until it
        // appears — registration is concurrent with this read.
        let mut stored = None;
        for _ in 0..200 {
            if let Some(a) = store.get(&id).await {
                stored = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let stored = stored.expect("submission registered in store despite dropped handle");
        await_terminal(&stored).await;
        assert!(
            ran.load(Ordering::SeqCst),
            "detached exec future must run to completion even with no awaiter"
        );
        assert!(stored.read().await.status.is_terminal());
    }

    #[tokio::test]
    async fn detached_outcome_serializes_only_when_present() {
        let mut sub = BuildSubmission {
            id: Uuid::new_v4(),
            worktree_path: PathBuf::from("/tmp/x"),
            build_kind: BuildKind::Build,
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
            spawn: None,
            detached: None,
        };
        let v = serde_json::to_value(&sub).unwrap();
        assert!(
            v.get("detached").is_none(),
            "must omit `detached` when None"
        );

        sub.detached = Some(DetachedOutcome {
            http_status: 200,
            body: serde_json::json!({"status": "ok"}),
        });
        let v = serde_json::to_value(&sub).unwrap();
        assert_eq!(v["detached"]["http_status"], 200);
        assert_eq!(v["detached"]["body"]["status"], "ok");
    }
}
