use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum SupervisorError {
    #[error("Runner is not running")]
    RunnerNotRunning,

    #[error("Runner is already running")]
    RunnerAlreadyRunning,

    #[error("Runner not found: {0}")]
    RunnerNotFound(String),

    /// Legacy variant — retained for call sites in `process/manager.rs` and
    /// `routes/runner.rs` that report "build currently in progress". With the
    /// parallel build pool, this is derived from "any slot busy".
    #[error("Build in progress")]
    BuildInProgress,

    /// All build pool slots are busy AND the caller opted out of waiting via
    /// `X-Queue-Mode: no-wait`. Body carries queue position and active build
    /// slot info for the caller to decide whether to retry or skip.
    #[error("Build pool full: all slots busy")]
    BuildPoolFull {
        queue_position: usize,
        active_builds: Vec<serde_json::Value>,
        /// Rough hint for how many seconds the caller should wait before
        /// retrying. Computed as `avg_build_duration - min(elapsed)` over
        /// active slots. `None` when no history exists yet.
        estimated_wait_secs: Option<f64>,
    },

    #[error("Build failed: {0}")]
    BuildFailed(String),

    /// The pre-permit disk guard refused a build because free disk fell below
    /// `QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB`. Mapped to `507 Insufficient
    /// Storage`. The body embeds the cached footprint snapshot and names both
    /// prune endpoints so the caller can reclaim space and retry. No build-pool
    /// permit / slot is consumed — the refusal happens BEFORE acquisition.
    #[error("Insufficient disk: {free_bytes} bytes free, need at least {required_bytes}")]
    InsufficientDisk {
        free_bytes: u64,
        required_bytes: u64,
        /// Cached footprint snapshot (may be `null` if no snapshot computed yet).
        footprint: Box<Option<serde_json::Value>>,
    },

    /// The runner exe that resolution landed on carries no usable build
    /// identity (or a foreign one), so the supervisor cannot show it is the
    /// code the caller asked for — and it refused to start it rather than
    /// produce a false green. Raised only for the non-pool artifact resolved
    /// through cargo's target-dir precedence; see
    /// [`crate::process::manager::unverified_exe_gate`] for the scope and the
    /// two opt-outs. Mapped to `409 CONFLICT`: the request is fine, the
    /// on-disk state is not.
    ///
    /// Boxed so the whole `SupervisorError` stays small — every
    /// `Result<_, SupervisorError>` in the crate pays for the largest variant
    /// (`clippy::result_large_err`), same reason `InsufficientDisk` boxes its
    /// footprint snapshot.
    #[error("{}", .0.detail)]
    UnverifiedExe(Box<UnverifiedExeInfo>),

    #[error("Process error: {0}")]
    Process(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    /// A build was cancelled — typically because a newer restart targeting the
    /// same runner pre-empted the in-flight build via its per-slot
    /// [`CancellationToken`](tokio_util::sync::CancellationToken). Mapped to
    /// `409 CONFLICT`: the request didn't fail, it was deliberately superseded.
    // wired into the build path in a follow-up (see PR #74)
    #[allow(dead_code)]
    #[error("Cancelled: {0}")]
    Cancelled(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Runner API error: {0}")]
    RunnerApi(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("{0}")]
    Other(String),
}

/// Payload of [`SupervisorError::UnverifiedExe`] — everything an operator needs
/// to see WHICH artifact was refused and why, without scraping the prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedExeInfo {
    /// Absolute path of the artifact that would have been launched.
    pub path: String,
    /// RFC3339 mtime of that artifact; `None` when unreadable.
    pub mtime: Option<String>,
    /// Recorded build SHA, `None` when there is no provenance record.
    pub build_sha: Option<String>,
    /// Recorded build source (`live_tree`/`origin_main`/`override`), `None`
    /// when there is no provenance record.
    pub build_source: Option<String>,
    /// Which cargo target-dir precedence level produced the path.
    pub target_dir_source: String,
    /// Full human-readable reason, including the recovery and the opt-ins.
    pub detail: String,
}

impl SupervisorError {
    /// Render this error to the same `(StatusCode, JSON body)` pair that
    /// [`IntoResponse`] would, but as values rather than a `Response`.
    ///
    /// Lets callers that route an error through a non-HTTP boundary (e.g. a
    /// build-submission terminal state recorded by a background task — Item 6)
    /// reproduce the exact wire shape the synchronous path returns, then build
    /// the `Response` later. `IntoResponse::into_response` is implemented in
    /// terms of this so the two can never drift.
    pub fn to_status_body(&self) -> (StatusCode, serde_json::Value) {
        if let SupervisorError::BuildPoolFull {
            queue_position,
            active_builds,
            estimated_wait_secs,
        } = self
        {
            let mut body = serde_json::json!({
                "error": "build_pool_full",
                "message": self.to_string(),
                "queue_position": queue_position,
                "active_builds": active_builds,
            });
            if let Some(w) = estimated_wait_secs {
                body.as_object_mut()
                    .unwrap()
                    .insert("estimated_wait_secs".to_string(), serde_json::json!(w));
            }
            return (StatusCode::SERVICE_UNAVAILABLE, body);
        }

        if let SupervisorError::InsufficientDisk {
            free_bytes,
            required_bytes,
            footprint,
        } = self
        {
            let body = serde_json::json!({
                "error": "insufficient_disk",
                "message": self.to_string(),
                "free_bytes": free_bytes,
                "required_bytes": required_bytes,
                "footprint": footprint.as_ref().clone(),
                // Name both prune endpoints so the caller can reclaim space.
                "prune_endpoints": [
                    "DELETE /spawn-worktrees?older_than_hours=<h>",
                    "POST /builds/slots/{id}/clean",
                ],
            });
            return (StatusCode::INSUFFICIENT_STORAGE, body);
        }

        if let SupervisorError::UnverifiedExe(info) = self {
            let UnverifiedExeInfo {
                path,
                mtime,
                build_sha,
                build_source,
                target_dir_source,
                detail,
            } = info.as_ref();
            // Typed body: the caller can branch on `error` and read the exact
            // artifact that was refused without scraping prose.
            let body = serde_json::json!({
                "error": "unverified_runner_exe",
                "message": detail,
                "resolved_exe": path,
                "resolved_exe_mtime": mtime,
                "build_sha": build_sha,
                "build_source": build_source,
                "target_dir_source": target_dir_source,
                "opt_ins": [
                    "spawn-test/spawn-named body: {\"allow_stale_fallback\": true}",
                    "supervisor env: QONTINUI_SUPERVISOR_ALLOW_UNVERIFIED_EXE=1",
                ],
                "recovery": [
                    "POST /runners/spawn-test {\"rebuild\": true}",
                    "POST /runner/fix-and-rebuild",
                ],
            });
            return (StatusCode::CONFLICT, body);
        }

        let status = match self {
            SupervisorError::RunnerNotRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerAlreadyRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerNotFound(_) => StatusCode::NOT_FOUND,
            SupervisorError::BuildInProgress => StatusCode::CONFLICT,
            SupervisorError::BuildPoolFull { .. } => StatusCode::SERVICE_UNAVAILABLE,
            SupervisorError::InsufficientDisk { .. } => StatusCode::INSUFFICIENT_STORAGE,
            SupervisorError::UnverifiedExe(_) => StatusCode::CONFLICT,
            SupervisorError::RunnerApi(_) => StatusCode::BAD_GATEWAY,
            SupervisorError::BuildFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SupervisorError::Process(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SupervisorError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            SupervisorError::Cancelled(_) => StatusCode::CONFLICT,
            SupervisorError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SupervisorError::Validation(_) => StatusCode::BAD_REQUEST,
            SupervisorError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = serde_json::json!({ "error": self.to_string() });
        (status, body)
    }
}

impl IntoResponse for SupervisorError {
    fn into_response(self) -> Response {
        // Delegate to `to_status_body` so the wire shape can never drift
        // between the two paths.
        let (status, body) = self.to_status_body();
        (status, axum::Json(body)).into_response()
    }
}
