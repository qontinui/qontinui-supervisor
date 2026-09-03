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

    /// The runner reported it is NOT safe to stop (or could not be asked), and
    /// the caller did not pass `force: true`. Mapped to `409 CONFLICT`: the
    /// request is well-formed, the runner's state is not.
    ///
    /// This is the refusal that makes the long-advertised `force` field on
    /// `POST /runners/{id}/stop` and `POST /runners/{id}/restart` real — see
    /// [`crate::restart_readiness`] for what was inert before it and why every
    /// UNKNOWN resolves here rather than to "safe".
    ///
    /// Boxed for the same reason `InsufficientDisk` and `UnverifiedExe` are:
    /// every `Result<_, SupervisorError>` in the crate pays for the largest
    /// variant (`clippy::result_large_err`).
    #[error("{}", .0.message)]
    RestartUnsafe(Box<crate::restart_readiness::RefusalDetail>),

    /// A start was refused because the runner's port is already held by a
    /// LIVE `qontinui-runner` process. Spawning a second runner against a held
    /// port is the OPEN finding S2 (double-spawn, measured on PID 247696 —
    /// `plans/2026-08-30-mobile-account-usage-relay-503-runner-runtime-starvation.md`
    /// §S2): the restart funnel used to skip the stop for a wedged adopted
    /// primary (`running == false` while the process owned the port) and go
    /// straight to `start_managed_runner`, whose only guard was that same
    /// flag. This variant is the belt to the stop-predicate fix's braces
    /// (`manager::restart_should_stop`): it closes S2 for EVERY caller of
    /// `start_managed_runner`, including the operator route. Mapped to
    /// `409 CONFLICT` like its neighbours — the request is fine, the on-box
    /// state is not. Recovery is a stop (or a restart, which now stops
    /// first) of the runner that holds the port.
    #[error(
        "Refusing to start a runner on port {port}: a live qontinui-runner process          (PID {pid}) already holds it. Starting a second runner against a held port          is the S2 double-spawn; stop (or restart) that runner first."
    )]
    PortHeldByLiveRunner { port: u16, pid: u32 },

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

        if let SupervisorError::RestartUnsafe(detail) = self {
            // The payload is already the exact wire shape (built in
            // `restart_readiness::decide`) so the body a caller reads and the
            // body the log line carries can never drift.
            return (StatusCode::CONFLICT, detail.payload.clone());
        }

        if let SupervisorError::PortHeldByLiveRunner { port, pid } = self {
            // Typed body: the caller can branch on `error` and read WHICH
            // process holds the port without scraping prose.
            let body = serde_json::json!({
                "error": "port_held_by_live_runner",
                "message": self.to_string(),
                "port": port,
                "pid": pid,
                "recovery": [
                    "POST /runners/{id}/stop",
                    "POST /runners/{id}/restart",
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
            SupervisorError::RestartUnsafe(_) => StatusCode::CONFLICT,
            SupervisorError::PortHeldByLiveRunner { .. } => StatusCode::CONFLICT,
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

#[cfg(test)]
mod tests {
    use super::SupervisorError;
    use axum::http::StatusCode;

    /// `PortHeldByLiveRunner` maps to 409 with a typed body naming the port
    /// and the holder, so a caller can branch on it rather than scrape prose.
    #[test]
    fn port_held_by_live_runner_maps_to_conflict_with_a_typed_body() {
        let err = SupervisorError::PortHeldByLiveRunner {
            port: 9876,
            pid: 247696,
        };
        let (status, body) = err.to_status_body();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "port_held_by_live_runner");
        assert_eq!(body["port"], 9876);
        assert_eq!(body["pid"], 247696);
        let message = body["message"].as_str().unwrap();
        assert!(message.contains("port 9876"), "{message}");
        assert!(message.contains("PID 247696"), "{message}");
        assert!(body["recovery"].is_array());
    }
}
