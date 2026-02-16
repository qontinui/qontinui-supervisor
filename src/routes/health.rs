use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::config::{RUNNER_API_PORT, RUNNER_VITE_PORT};
use crate::process::port::{is_port_in_use, is_runner_responding};
use crate::state::SharedState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub runner: RunnerHealth,
    pub ports: PortsHealth,
    pub watchdog: WatchdogHealth,
    pub build: BuildHealth,
    pub supervisor: SupervisorInfo,
}

#[derive(Serialize)]
pub struct RunnerHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub api_responding: bool,
    pub mode: String,
}

#[derive(Serialize)]
pub struct PortsHealth {
    pub api_port: PortStatus,
    pub vite_port: Option<PortStatus>,
}

#[derive(Serialize)]
pub struct PortStatus {
    pub port: u16,
    pub in_use: bool,
}

#[derive(Serialize)]
pub struct WatchdogHealth {
    pub enabled: bool,
    pub restart_attempts: u32,
    pub last_restart_at: Option<String>,
    pub disabled_reason: Option<String>,
    pub crash_count: usize,
}

#[derive(Serialize)]
pub struct BuildHealth {
    pub in_progress: bool,
    pub error_detected: bool,
    pub last_error: Option<String>,
    pub last_build_at: Option<String>,
}

#[derive(Serialize)]
pub struct SupervisorInfo {
    pub version: String,
    pub dev_mode: bool,
    pub project_dir: String,
}

pub async fn health(State(state): State<SharedState>) -> Json<HealthResponse> {
    let runner = state.runner.read().await;
    let watchdog = state.watchdog.read().await;
    let build = state.build.read().await;

    let api_responding = is_runner_responding(RUNNER_API_PORT).await;
    let api_in_use = is_port_in_use(RUNNER_API_PORT);
    let vite_in_use = is_port_in_use(RUNNER_VITE_PORT);

    let overall_status = if runner.running && api_responding {
        "healthy"
    } else if runner.running && !api_responding {
        "degraded"
    } else if build.build_in_progress {
        "building"
    } else {
        "stopped"
    };

    let response = HealthResponse {
        status: overall_status.to_string(),
        runner: RunnerHealth {
            running: runner.running,
            pid: runner.pid,
            started_at: runner.started_at.map(|t| t.to_rfc3339()),
            api_responding,
            mode: if state.config.dev_mode {
                "dev".to_string()
            } else {
                "exe".to_string()
            },
        },
        ports: PortsHealth {
            api_port: PortStatus {
                port: RUNNER_API_PORT,
                in_use: api_in_use,
            },
            vite_port: if state.config.dev_mode {
                Some(PortStatus {
                    port: RUNNER_VITE_PORT,
                    in_use: vite_in_use,
                })
            } else {
                None
            },
        },
        watchdog: WatchdogHealth {
            enabled: watchdog.enabled,
            restart_attempts: watchdog.restart_attempts,
            last_restart_at: watchdog.last_restart_at.map(|t| t.to_rfc3339()),
            disabled_reason: watchdog.disabled_reason.clone(),
            crash_count: watchdog.crash_history.len(),
        },
        build: BuildHealth {
            in_progress: build.build_in_progress,
            error_detected: build.build_error_detected,
            last_error: build.last_build_error.clone(),
            last_build_at: build.last_build_at.map(|t| t.to_rfc3339()),
        },
        supervisor: SupervisorInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            dev_mode: state.config.dev_mode,
            project_dir: state.config.project_dir.display().to_string(),
        },
    };

    Json(response)
}
