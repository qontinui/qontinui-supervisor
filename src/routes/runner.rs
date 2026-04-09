use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::diagnostics::RestartSource;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager;
use crate::state::SharedState;
use crate::{ai_debug, routes::ai::DebugRequest};

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
    Json(body): Json<RestartRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
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

    Ok(Json(serde_json::json!({
        "status": "restarted",
        "message": format!("Runner restarted successfully{}", if rebuild { " (with rebuild)" } else { "" })
    })))
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
        format!("Cleared {} stuck slot{}", cleared_count, if cleared_count == 1 { "" } else { "s" })
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

/// POST /runner/fix-and-rebuild — Run AI debug, wait for completion, then rebuild the runner.
pub async fn fix_and_rebuild(
    State(state): State<SharedState>,
    Json(body): Json<DebugRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Fix & Rebuild: starting AI debug session",
        )
        .await;

    // 1. Spawn AI debug (same as POST /ai/debug)
    let reason = body.prompt.as_deref().unwrap_or("Fix & Rebuild trigger");
    ai_debug::spawn_ai_debug(&state, Some(reason))
        .await
        .map_err(|e| SupervisorError::Other(format!("Failed to start AI debug: {}", e)))?;

    // 2. Poll until AI session completes (check every 2s, timeout 10 min)
    let timeout = std::time::Duration::from_secs(600);
    let start = std::time::Instant::now();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let still_running = {
            let ai = state.ai.read().await;
            ai.running
        };

        if !still_running {
            break;
        }

        if start.elapsed() > timeout {
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Warn,
                    "Fix & Rebuild: AI session timed out after 10 min, proceeding with rebuild",
                )
                .await;
            // Kill the stuck session
            let _ = ai_debug::stop_ai_debug(&state).await;
            break;
        }
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Fix & Rebuild: AI debug complete, rebuilding runner",
        )
        .await;

    // 3. Build only — the supervisor does not restart user runners
    crate::build_monitor::run_cargo_build(&state).await?;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "AI fix applied and runner rebuilt (restart manually to apply)"
    })))
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
