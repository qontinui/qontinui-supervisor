use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::config::{RUNNER_API_PORT, RUNNER_VITE_PORT};
use crate::state::SharedState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub runner: RunnerHealth,
    pub ports: PortsHealth,
    pub watchdog: WatchdogHealth,
    pub build: BuildHealth,
    pub ai: AiHealth,
    pub code_activity: CodeActivityHealth,
    pub expo: ExpoHealth,
    pub supervisor: SupervisorInfo,
}

#[derive(Serialize)]
pub struct ExpoHealth {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub configured: bool,
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
pub struct AiHealth {
    pub ai_running: bool,
    pub ai_provider: String,
    pub ai_model: String,
    pub auto_debug_enabled: bool,
}

#[derive(Serialize)]
pub struct CodeActivityHealth {
    pub code_being_edited: bool,
    pub external_claude_session: bool,
    pub pending_debug: bool,
    pub pending_debug_reason: Option<String>,
}

#[derive(Serialize)]
pub struct SupervisorInfo {
    pub version: String,
    pub dev_mode: bool,
    pub project_dir: String,
}

pub async fn health(State(state): State<SharedState>) -> Json<HealthResponse> {
    Json(build_health_response(&state).await)
}

pub async fn build_health_response(state: &SharedState) -> HealthResponse {
    let runner = state.runner.read().await;
    let watchdog = state.watchdog.read().await;
    let build = state.build.read().await;
    let ai = state.ai.read().await;
    let ca = state.code_activity.read().await;
    let expo = state.expo.read().await;

    // Read from background health cache instead of live port checks (~100µs vs ~3s)
    let cached = state.cached_health.read().await;
    let api_responding = cached.runner_responding;
    let api_in_use = cached.runner_port_open;
    let vite_in_use = cached.vite_port_open;
    drop(cached);

    let overall_status = if runner.running && api_responding {
        "healthy"
    } else if runner.running && !api_responding {
        "degraded"
    } else if build.build_in_progress {
        "building"
    } else {
        "stopped"
    };

    HealthResponse {
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
        ai: AiHealth {
            ai_running: ai.running,
            ai_provider: ai.provider.clone(),
            ai_model: ai.model.clone(),
            auto_debug_enabled: ai.auto_debug_enabled,
        },
        code_activity: CodeActivityHealth {
            code_being_edited: ca.code_being_edited,
            external_claude_session: ca.external_claude_session,
            pending_debug: ca.pending_debug,
            pending_debug_reason: ca.pending_debug_reason.clone(),
        },
        expo: ExpoHealth {
            running: expo.running,
            pid: expo.pid,
            port: expo.port,
            configured: state.config.expo_dir.is_some(),
        },
        supervisor: SupervisorInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            dev_mode: state.config.dev_mode,
            project_dir: state.config.project_dir.display().to_string(),
        },
    }
}
