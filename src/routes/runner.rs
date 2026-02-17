use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::diagnostics::RestartSource;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct RestartRequest {
    #[serde(default)]
    pub rebuild: bool,
}

#[derive(Deserialize)]
pub struct WatchdogRequest {
    pub enabled: bool,
    #[serde(default)]
    pub reset_attempts: bool,
}

pub async fn stop_runner(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    manager::stop_runner(&state).await?;

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

    manager::restart_runner(&state, rebuild, RestartSource::Manual).await?;

    Ok(Json(serde_json::json!({
        "status": "restarted",
        "message": format!("Runner restarted successfully{}", if rebuild { " (with rebuild)" } else { "" })
    })))
}

pub async fn control_watchdog(
    State(state): State<SharedState>,
    Json(body): Json<WatchdogRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    let response = {
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

    // Stop runner first
    {
        let runner = state.runner.read().await;
        if runner.running {
            drop(runner);
            let _ = manager::stop_runner(&state).await;
        }
    }

    // Spawn replacement process
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&remaining_args);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
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
