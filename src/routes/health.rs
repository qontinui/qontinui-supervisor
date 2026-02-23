use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::Serialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

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

/// Determine the overall health status string based on runner, API, and build state.
/// This is a pure function extracted for testability.
pub fn determine_overall_status(
    runner_running: bool,
    api_responding: bool,
    build_in_progress: bool,
) -> &'static str {
    if runner_running && api_responding {
        "healthy"
    } else if runner_running && !api_responding {
        "degraded"
    } else if build_in_progress {
        "building"
    } else {
        "stopped"
    }
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

    let overall_status =
        determine_overall_status(runner.running, api_responding, build.build_in_progress);

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

/// GET /health/stream — SSE stream that pushes health updates every 3s.
/// Only emits an event when the serialized health JSON changes from the previous tick.
pub async fn health_stream(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let interval = IntervalStream::new(tokio::time::interval(Duration::from_secs(3)));
    let mut last_json = String::new();

    let stream = interval.map(move |_| {
        let state = state.clone();
        let health = {
            // Sync-safe: use try_read on each lock to avoid blocking the stream
            let runner = state.runner.try_read();
            let watchdog = state.watchdog.try_read();
            let build = state.build.try_read();
            let ai = state.ai.try_read();
            let ca = state.code_activity.try_read();
            let expo = state.expo.try_read();
            let cached = state.cached_health.try_read();

            // If any lock is contended, skip this tick
            if runner.is_err()
                || watchdog.is_err()
                || build.is_err()
                || ai.is_err()
                || ca.is_err()
                || expo.is_err()
                || cached.is_err()
            {
                return Ok(Event::default().comment("keepalive"));
            }

            let runner = runner.unwrap();
            let watchdog = watchdog.unwrap();
            let build = build.unwrap();
            let ai = ai.unwrap();
            let ca = ca.unwrap();
            let expo = expo.unwrap();
            let cached = cached.unwrap();

            let api_responding = cached.runner_responding;
            let api_in_use = cached.runner_port_open;
            let vite_in_use = cached.vite_port_open;

            let overall_status =
                determine_overall_status(runner.running, api_responding, build.build_in_progress);

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
        };

        let json = serde_json::to_string(&health).unwrap_or_default();
        if json == last_json {
            Ok(Event::default().comment("keepalive"))
        } else {
            last_json = json.clone();
            Ok(Event::default().event("health").data(json))
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_healthy_when_runner_running_and_api_responding() {
        assert_eq!(determine_overall_status(true, true, false), "healthy");
    }

    #[test]
    fn test_healthy_even_when_building() {
        // runner_running + api_responding takes precedence over build_in_progress
        assert_eq!(determine_overall_status(true, true, true), "healthy");
    }

    #[test]
    fn test_degraded_when_runner_running_but_api_not_responding() {
        assert_eq!(determine_overall_status(true, false, false), "degraded");
    }

    #[test]
    fn test_degraded_when_runner_running_api_down_and_building() {
        // runner_running + !api_responding => degraded, regardless of build state
        assert_eq!(determine_overall_status(true, false, true), "degraded");
    }

    #[test]
    fn test_building_when_runner_not_running_and_build_in_progress() {
        assert_eq!(determine_overall_status(false, false, true), "building");
    }

    #[test]
    fn test_stopped_when_nothing_running() {
        assert_eq!(determine_overall_status(false, false, false), "stopped");
    }

    #[test]
    fn test_stopped_when_runner_not_running_and_api_responding() {
        // Edge case: api_responding but runner not running (stale port?)
        // runner.running is false, so falls through to build/stopped checks
        assert_eq!(determine_overall_status(false, true, false), "stopped");
    }

    #[test]
    fn test_health_response_serializes_to_json() {
        let response = HealthResponse {
            status: "healthy".to_string(),
            runner: RunnerHealth {
                running: true,
                pid: Some(1234),
                started_at: None,
                api_responding: true,
                mode: "dev".to_string(),
            },
            ports: PortsHealth {
                api_port: PortStatus {
                    port: 9876,
                    in_use: true,
                },
                vite_port: Some(PortStatus {
                    port: 1420,
                    in_use: true,
                }),
            },
            watchdog: WatchdogHealth {
                enabled: true,
                restart_attempts: 0,
                last_restart_at: None,
                disabled_reason: None,
                crash_count: 0,
            },
            build: BuildHealth {
                in_progress: false,
                error_detected: false,
                last_error: None,
                last_build_at: None,
            },
            ai: AiHealth {
                ai_running: false,
                ai_provider: "claude".to_string(),
                ai_model: "opus".to_string(),
                auto_debug_enabled: false,
            },
            code_activity: CodeActivityHealth {
                code_being_edited: false,
                external_claude_session: false,
                pending_debug: false,
                pending_debug_reason: None,
            },
            expo: ExpoHealth {
                running: false,
                pid: None,
                port: 8081,
                configured: false,
            },
            supervisor: SupervisorInfo {
                version: "0.1.0".to_string(),
                dev_mode: true,
                project_dir: "/tmp/test".to_string(),
            },
        };

        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"running\":true"));
        assert!(json.contains("\"pid\":1234"));
        assert!(json.contains("\"api_responding\":true"));
        assert!(json.contains("\"mode\":\"dev\""));
    }

    #[test]
    fn test_port_status_values() {
        let port_status = PortStatus {
            port: RUNNER_API_PORT,
            in_use: false,
        };
        assert_eq!(port_status.port, 9876);
        assert!(!port_status.in_use);
    }
}
