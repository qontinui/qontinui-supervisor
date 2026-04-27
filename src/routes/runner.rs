use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::diagnostics::RestartSource;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::health_probe::{wait_for_runner_healthy_default, HealthProbeFailure};
use crate::process::manager;
use crate::state::SharedState;

/// Query string for endpoints that opt-OUT of the post-spawn health wait.
///
/// Default is `wait=true` (poll up to 30s for the runner's `/health` to come
/// up before returning 200). Pass `?wait=false` to get the legacy
/// fire-and-forget behavior (200 the moment the OS process spawns, even if
/// it's hung).
#[derive(Deserialize, Default)]
pub struct StartWaitQuery {
    #[serde(default = "default_wait_true")]
    pub wait: bool,
}

fn default_wait_true() -> bool {
    true
}

/// Build the standard 503 body for a runner that started but didn't bind
/// its API port within the wait budget. Shared between the legacy
/// `/runner/restart` and the per-runner `/runners/{id}/start` so both
/// endpoints expose an identical contract.
pub(crate) fn unhealthy_after_start_response(
    runner_id: &str,
    failure: HealthProbeFailure,
) -> Response {
    let body = serde_json::json!({
        "error": "runner_unhealthy_after_start",
        "runner_id": runner_id,
        "elapsed_ms": failure.elapsed_ms,
        "recent_logs": failure.recent_logs,
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

#[derive(Deserialize)]
pub struct RestartRequest {
    #[serde(default)]
    pub rebuild: bool,
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize)]
pub struct WatchdogRequest {
    pub enabled: bool,
    #[serde(default)]
    pub reset_attempts: bool,
}

#[derive(Deserialize, Default)]
pub struct StopRequest {
    #[serde(default)]
    pub force: bool,
}

pub async fn stop_runner(
    State(state): State<SharedState>,
    body: Option<Json<StopRequest>>,
) -> Result<impl IntoResponse, SupervisorError> {
    let force = body.map(|b| b.force).unwrap_or(false);
    manager::stop_runner(&state, force).await?;

    Ok(Json(serde_json::json!({
        "status": "stopped",
        "message": "Runner stopped successfully"
    })))
}

pub async fn restart_runner(
    State(state): State<SharedState>,
    Query(wait_q): Query<StartWaitQuery>,
    Json(body): Json<RestartRequest>,
) -> Result<Response, SupervisorError> {
    let rebuild = body.rebuild;

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Restart requested (rebuild: {})", rebuild),
        )
        .await;

    manager::restart_runner(&state, rebuild, RestartSource::Manual, body.force).await?;

    // Port-bind verification: the manager call above returns the moment the
    // OS process is spawned, but the runner may still be pre-bind (or hung).
    // Poll its `/health` for up to 30s so callers can distinguish a healthy
    // restart from a wedged one. `wait=false` opts out (legacy behavior).
    if wait_q.wait {
        // Look up the primary runner id so the probe targets the right
        // managed entry. The legacy endpoint always restarts the primary,
        // so this mirrors `manager::restart_runner`'s lookup.
        if let Some(primary) = state.get_primary().await {
            let runner_id = primary.config.id.clone();
            if let Err(failure) = wait_for_runner_healthy_default(&state, &runner_id).await {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Runner '{}' did not become healthy {}ms after restart",
                            runner_id, failure.elapsed_ms
                        ),
                    )
                    .await;
                return Ok(unhealthy_after_start_response(&runner_id, failure));
            }
        }
    }

    Ok(Json(serde_json::json!({
        "status": "restarted",
        "message": format!("Runner restarted successfully{}", if rebuild { " (with rebuild)" } else { "" })
    }))
    .into_response())
}

pub async fn control_watchdog(
    State(state): State<SharedState>,
    Json(body): Json<WatchdogRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Update the primary runner's managed watchdog (used by the watchdog bg task)
    let response = if let Some(primary) = state.get_primary().await {
        let mut wd = primary.watchdog.write().await;
        wd.enabled = body.enabled;

        if body.reset_attempts {
            wd.restart_attempts = 0;
            wd.disabled_reason = None;
            wd.crash_history.clear();
        }

        serde_json::json!({
            "watchdog": {
                "enabled": wd.enabled,
                "restart_attempts": wd.restart_attempts,
            }
        })
    } else {
        // Fallback to legacy state if no managed runners
        let mut wd = state.watchdog.write().await;
        wd.enabled = body.enabled;

        if body.reset_attempts {
            wd.restart_attempts = 0;
            wd.disabled_reason = None;
            wd.crash_history.clear();
        }

        serde_json::json!({
            "watchdog": {
                "enabled": wd.enabled,
                "restart_attempts": wd.restart_attempts,
            }
        })
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Watchdog {} {}",
                if body.enabled { "enabled" } else { "disabled" },
                if body.reset_attempts {
                    "(attempts reset)"
                } else {
                    ""
                }
            ),
        )
        .await;

    state.notify_health_change();

    Ok(Json(response))
}

/// POST /build/reset — force-release every build slot and clear the legacy flag.
///
/// Under the parallel build pool a "stuck build" is most likely a `slot.busy`
/// leak: the `SlotGuard` RAII type in `build_monitor.rs` now prevents this
/// going forward, but this endpoint remains as a forensic / recovery tool.
///
/// We intentionally do NOT force-release semaphore permits. `Semaphore` has
/// no public API for that, and `add_permits` would break the invariant
/// `permits.available + permits.held == pool_size`. If a permit is genuinely
/// stuck (which would require a tokio bug), the only safe recovery is a
/// supervisor restart.
pub async fn reset_build(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    let now = chrono::Utc::now();
    let mut slots_cleared = Vec::with_capacity(state.build_pool.slots.len());
    let mut cleared_count = 0usize;

    for slot in &state.build_pool.slots {
        let mut busy = slot.busy.write().await;
        if let Some(prior) = busy.take() {
            cleared_count += 1;
            let elapsed_secs = (now - prior.started_at).num_seconds().max(0);
            tracing::warn!(
                "Forcibly cleared stuck build slot {}: requester_id={:?}, rebuild_kind={}, elapsed_secs={}",
                slot.id, prior.requester_id, prior.rebuild_kind, elapsed_secs
            );
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Warn,
                    format!(
                        "Build reset: forcibly cleared slot {} (requester_id={:?}, elapsed_secs={})",
                        slot.id, prior.requester_id, elapsed_secs
                    ),
                )
                .await;
            slots_cleared.push(serde_json::json!({
                "id": slot.id,
                "was_busy": true,
                "prior_requester_id": prior.requester_id,
                "rebuild_kind": prior.rebuild_kind,
                "elapsed_secs": elapsed_secs,
            }));
        } else {
            slots_cleared.push(serde_json::json!({
                "id": slot.id,
                "was_busy": false,
            }));
        }
    }

    let legacy_flag_was_set = {
        let mut build = state.build.write().await;
        let was = build.build_in_progress;
        build.build_in_progress = false;
        was
    };

    state.notify_health_change();

    let message = if cleared_count > 0 {
        format!(
            "Cleared {} stuck slot{}",
            cleared_count,
            if cleared_count == 1 { "" } else { "s" }
        )
    } else if legacy_flag_was_set {
        "No slots busy; cleared stale legacy build flag".to_string()
    } else {
        "No stuck slots; legacy flag already clear".to_string()
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Build reset: {}", message),
        )
        .await;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "slots_cleared": slots_cleared,
        "legacy_flag_was_set": legacy_flag_was_set,
        "message": message,
    })))
}

/// POST /runner/fix-and-rebuild — Rebuild the runner.
pub async fn fix_and_rebuild(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Fix & Rebuild: rebuilding runner",
        )
        .await;

    crate::build_monitor::run_cargo_build(&state).await?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "Runner rebuilt (restart manually to apply)"
    })))
}

/// Trigger a graceful shutdown of the supervisor.
///
/// Unlike `Stop-Process -Force` (which uses `TerminateProcess` on Windows and
/// can't be caught), this endpoint routes through the same drain that
/// `Ctrl+C` uses: axum finishes in-flight requests before the process exits,
/// so callers waiting on a long `spawn-test` build see a proper response (or
/// a 503 if they hit us after the drain started) instead of an empty body.
///
/// Fire-and-return: the shutdown signal is dispatched on a `tokio::spawn`ed
/// task so the HTTP response can return in well under 100ms. Without this
/// handoff, the handler awaited the broadcast send acknowledgement before
/// responding, which routinely added ~2s to the round-trip while subscribers
/// drained their channels. The actual shutdown drain still happens — it just
/// runs after the response goes out.
pub async fn supervisor_shutdown(State(state): State<SharedState>) -> Json<serde_json::Value> {
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "HTTP shutdown requested — initiating graceful drain",
        )
        .await;

    // The shutdown_signal task in main.rs races ctrl_c against shutdown_tx;
    // `signal_shutdown` flips the latched flag *and* broadcasts so any
    // handler that subscribes after this point still observes shutdown
    // (broadcast channels don't replay missed messages). Fire it from a
    // spawned task so this handler can return immediately — the response
    // must complete before the drain window closes our listener.
    let state_for_signal = state.clone();
    tokio::spawn(async move {
        state_for_signal.signal_shutdown();
    });

    Json(serde_json::json!({
        "status": "shutting_down",
        "message": "Supervisor graceful shutdown initiated"
    }))
}

/// Self-restart the supervisor process with the same CLI args.
pub async fn supervisor_restart(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Supervisor self-restart requested",
        )
        .await;

    let args = state.config.cli_args.clone();
    let exe = args.first().cloned().unwrap_or_else(|| {
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "qontinui-supervisor".to_string())
    });

    let remaining_args: Vec<String> = args.into_iter().skip(1).collect();

    // Only stop temp runners — user runners survive supervisor restarts
    let _ = manager::stop_all_temp_runners(&state).await;

    // Spawn replacement process
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&remaining_args);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    match cmd.spawn() {
        Ok(_child) => {
            // Give the new process a moment to start
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Send response before exiting
            let response = Json(serde_json::json!({
                "status": "restarting",
                "message": "Supervisor is restarting"
            }));

            // Schedule exit after response is sent
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                std::process::exit(0);
            });

            Ok(response)
        }
        Err(e) => Err(SupervisorError::Process(format!(
            "Failed to spawn replacement supervisor: {}",
            e
        ))),
    }
}
