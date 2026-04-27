use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::stream::Stream;
use serde::Serialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

use crate::config::RUNNER_API_PORT;
use crate::health_cache::{CachedRunnerHealth, RecentCrashSummary, RunnerStatus, UiErrorSummary};
use crate::sdk_features::{SDK_FEATURES, SDK_FEATURE_DOC_URL};
use crate::state::SharedState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub runner: RunnerHealth,
    pub ports: PortsHealth,
    pub watchdog: WatchdogHealth,
    pub build: BuildHealth,
    pub expo: ExpoHealth,
    pub supervisor: SupervisorInfo,
    /// Multi-runner status array (includes all managed runners).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runners: Vec<RunnerInstanceHealth>,
    /// SDK feature inventory baked at compile time. Lets test drivers tell
    /// in one round-trip whether the running supervisor binary's bundled
    /// `@qontinui/ui-bridge` SDK includes a feature they need; an absent
    /// entry means the binary predates that feature's SDK release.
    /// See `crate::sdk_features` for the source of truth.
    #[serde(rename = "sdkFeatures")]
    pub sdk_features: Vec<&'static str>,
    /// Documentation URL describing what each `sdkFeatures` entry means.
    #[serde(rename = "sdkFeaturesDocUrl")]
    pub sdk_features_doc_url: &'static str,
    /// Identifier for the embedded frontend bundle this supervisor is
    /// currently serving. Stable across the life of the supervisor process,
    /// changes when the supervisor binary is rebuilt with a fresh
    /// `dist/index.html` embedded. Connected dashboard tabs read this from
    /// the SSE stream and compare against the `<meta name="build-id">` that
    /// was injected at HTML serve time so a rebuild can prompt them to
    /// refresh.
    #[serde(rename = "buildId")]
    pub build_id: String,
}

#[derive(Serialize, Clone)]
pub struct RunnerInstanceHealth {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub is_primary: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub api_responding: bool,
    pub watchdog_status: WatchdogHealth,
    /// UI-level error reported by the runner's /health endpoint (Phase 3J.1).
    /// `None` when the runner reports no error or when the runner is too old
    /// to include the `ui_error` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_error: Option<UiErrorSummary>,
    /// Most recent Rust crash dump surfaced by the runner's /health endpoint.
    /// `None` when the runner has no fresh dump on disk or predates the
    /// crash-dump scanner (post-3J follow-up). Distinct from `ui_error`:
    /// non-unwinding panics abort the process before the React boundary sees
    /// them, so this is the only signal for that class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_crash: Option<RecentCrashSummary>,
    /// Supervisor-derived status (healthy / degraded / errored / offline /
    /// starting). Combines process liveness with the runner's self-reported
    /// `ui_error` + `derived_status` + `recent_crash`.
    pub derived_status: RunnerStatus,
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
}

#[derive(Serialize)]
pub struct PortsHealth {
    pub api_port: PortStatus,
}

#[derive(Serialize)]
pub struct PortStatus {
    pub port: u16,
    pub in_use: bool,
}

#[derive(Serialize, Clone)]
pub struct WatchdogHealth {
    pub enabled: bool,
    pub restart_attempts: u32,
    pub last_restart_at: Option<String>,
    pub disabled_reason: Option<String>,
    pub crash_count: usize,
}

impl WatchdogHealth {
    /// Return a static "disabled" value for the ambient auto-restart
    /// watchdog. That module was removed; the supervisor no longer tries to
    /// resurrect a runner that has gone unhealthy. A separate first-healthy
    /// watchdog (see `process::manager::watch_first_healthy`) runs per spawn
    /// and kills children that never bind their HTTP API — it does not
    /// surface here because it's ephemeral per-start, not ambient.
    /// This keeps the JSON shape stable for API consumers.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            restart_attempts: 0,
            last_restart_at: None,
            disabled_reason: Some(
                "no ambient auto-restart (first-healthy watchdog runs per spawn)".to_string(),
            ),
            crash_count: 0,
        }
    }
}

#[derive(Serialize)]
pub struct BuildHealth {
    pub in_progress: bool,
    /// Number of build pool slots currently available for new builds.
    /// 0 means the pool is saturated; >0 means new `spawn-test {rebuild: true}`
    /// calls will begin immediately without queuing.
    pub available_slots: usize,
    pub error_detected: bool,
    pub last_error: Option<String>,
    pub last_build_at: Option<String>,
    /// True when at least one build slot embeds a stale frontend because its
    /// most recent `npm run build` failed but a cargo build proceeded using a
    /// prior `dist/` snapshot. Clears when a subsequent npm build on that
    /// slot succeeds.
    pub frontend_stale_any: bool,
    /// Last-known-good runner binary metadata. `None` until the first
    /// successful build (or after a fresh checkout where `target-pool/lkg/`
    /// doesn't exist). Agents deciding whether to fall back to the LKG when
    /// their own build fails should compare `built_at` (RFC3339) against the
    /// mtime of every file they've changed. If `built_at` is later than the
    /// max file mtime, the LKG already contains those changes and is safe to
    /// run via `POST /runners/spawn-test {use_lkg: true}`. Otherwise the LKG
    /// predates the changes and would silently run stale code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lkg: Option<LkgHealth>,
}

#[derive(Serialize)]
pub struct LkgHealth {
    /// RFC3339 wall-clock time the LKG build completed. THIS is the value
    /// agents compare against `mtime(changed files)` to decide LKG safety.
    pub built_at: String,
    /// Pool slot the LKG exe was copied from at build time. Informational —
    /// the LKG file lives at a fixed path independent of slot state.
    pub source_slot: usize,
    /// Byte size of the LKG exe. Useful for spotting truncated copies.
    pub exe_size: u64,
}

#[derive(Serialize)]
pub struct SupervisorInfo {
    pub version: String,
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
    } else if api_responding {
        "external"
    } else if build_in_progress {
        "building"
    } else {
        "stopped"
    }
}

/// Build runner instance health from the cached snapshot (sync-safe for SSE).
fn build_sse_runners(state: &SharedState) -> Vec<RunnerInstanceHealth> {
    match state.cached_runner_health.try_read() {
        Ok(snapshots) => snapshots
            .iter()
            .map(|r: &CachedRunnerHealth| RunnerInstanceHealth {
                id: r.id.clone(),
                name: r.name.clone(),
                port: r.port,
                is_primary: r.is_primary,
                running: r.running,
                pid: r.pid,
                started_at: None, // Not cached — use GET /runners for full detail
                api_responding: r.api_responding,
                watchdog_status: WatchdogHealth::disabled(),
                ui_error: r.ui_error.clone(),
                recent_crash: r.recent_crash.clone(),
                derived_status: r.derived_status.clone(),
            })
            .collect(),
        Err(_) => Vec::new(), // Lock contended, skip this tick
    }
}

pub async fn health(State(state): State<SharedState>) -> Json<HealthResponse> {
    Json(build_health_response(&state).await)
}

pub async fn build_health_response(state: &SharedState) -> HealthResponse {
    let build = state.build.read().await;
    let expo = state.expo.read().await;
    let frontend_stale_any = state.build_pool.any_slot_has_stale_frontend().await;
    let lkg = state
        .build_pool
        .last_known_good
        .read()
        .await
        .as_ref()
        .map(|info| LkgHealth {
            built_at: info.built_at.to_rfc3339(),
            source_slot: info.source_slot,
            exe_size: info.exe_size,
        });

    // Read from background health cache instead of live port checks (~100µs vs ~3s)
    let cached = state.cached_health.read().await;
    let api_responding = cached.runner_responding;
    let api_in_use = cached.runner_port_open;
    drop(cached);

    // Use primary ManagedRunner state (not the legacy state.runner which is never
    // updated for user-managed runners, causing "stopped" even when the runner is UP).
    let (primary_running, primary_pid, primary_started_at) =
        if let Some(primary) = state.get_primary().await {
            let pr = primary.runner.read().await;
            (pr.running, pr.pid, pr.started_at)
        } else {
            // Fallback to legacy state.runner if no managed primary exists
            let runner = state.runner.read().await;
            (runner.running, runner.pid, runner.started_at)
        };

    let overall_status =
        determine_overall_status(primary_running, api_responding, build.build_in_progress);

    // Build multi-runner status array. The `watchdog_status` field reports the
    // ambient auto-restart watchdog, which was removed; the first-healthy
    // watchdog in `process::manager` is per-spawn and not surfaced here.
    //
    // ui_error + derived_status come from the background health-cache refresher
    // (which GETs each runner's /health every 2s). We index into that snapshot
    // by runner id to avoid issuing another round of HTTP calls on the hot path.
    let managed_runners = state.get_all_runners().await;
    let cached_snapshots = state.cached_runner_health.read().await;
    let mut runners_health = Vec::new();
    for managed in &managed_runners {
        let mr = managed.runner.read().await;
        let mc = managed.cached_health.read().await;
        let cached = cached_snapshots.iter().find(|c| c.id == managed.config.id);
        runners_health.push(RunnerInstanceHealth {
            id: managed.config.id.clone(),
            name: managed.config.name.clone(),
            port: managed.config.port,
            is_primary: managed.config.is_primary,
            running: mr.running,
            pid: mr.pid,
            started_at: mr.started_at.map(|t| t.to_rfc3339()),
            api_responding: mc.runner_responding,
            watchdog_status: WatchdogHealth::disabled(),
            ui_error: cached.and_then(|c| c.ui_error.clone()),
            recent_crash: cached.and_then(|c| c.recent_crash.clone()),
            derived_status: cached.map(|c| c.derived_status.clone()).unwrap_or_default(),
        });
    }
    drop(cached_snapshots);

    HealthResponse {
        status: overall_status.to_string(),
        runner: RunnerHealth {
            running: primary_running,
            pid: primary_pid,
            started_at: primary_started_at.map(|t| t.to_rfc3339()),
            api_responding,
        },
        ports: PortsHealth {
            api_port: PortStatus {
                port: RUNNER_API_PORT,
                in_use: api_in_use,
            },
        },
        watchdog: WatchdogHealth::disabled(),
        build: BuildHealth {
            in_progress: build.build_in_progress,
            available_slots: state.build_pool.permits.available_permits(),
            error_detected: build.build_error_detected,
            last_error: build.last_build_error.clone(),
            last_build_at: build.last_build_at.map(|t| t.to_rfc3339()),
            frontend_stale_any,
            lkg,
        },
        expo: ExpoHealth {
            running: expo.running,
            pid: expo.pid,
            port: expo.port,
            configured: state.config.expo_dir.is_some(),
        },
        supervisor: SupervisorInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            project_dir: state.config.project_dir.display().to_string(),
        },
        runners: runners_health,
        sdk_features: SDK_FEATURES.to_vec(),
        sdk_features_doc_url: SDK_FEATURE_DOC_URL,
        build_id: state.build_id.clone(),
    }
}

/// GET /health/stream — SSE stream that pushes health updates every 3s.
/// Only emits an event when the serialized health JSON changes from the previous tick.
///
/// The stream terminates as soon as `state.shutdown_signal()` fires so that
/// `axum::serve(..).with_graceful_shutdown(..)` can complete its drain phase
/// promptly. Without this, the supervisor's own dashboard webview keeps a
/// `/health/stream` connection open indefinitely, the drain never completes,
/// and `POST /supervisor/shutdown` results in a 30+ second hang before the
/// process exits.
pub async fn health_stream(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let interval = IntervalStream::new(tokio::time::interval(Duration::from_secs(3)));
    let mut last_json = String::new();

    // Cap the stream's lifetime at the shutdown signal so axum's graceful
    // drain can release this connection.
    let shutdown_state = state.clone();
    let shutdown = Box::pin(async move { shutdown_state.shutdown_signal().await });

    let stream = interval.map(move |_| {
        let state = state.clone();
        let health = {
            // Sync-safe: use try_read on each lock to avoid blocking the stream
            let build = state.build.try_read();
            let expo = state.expo.try_read();
            let cached = state.cached_health.try_read();
            let runner_snapshots = state.cached_runner_health.try_read();

            // If any lock is contended, skip this tick
            let Ok(build) = build else {
                return Ok(Event::default().comment("keepalive"));
            };
            let Ok(expo) = expo else {
                return Ok(Event::default().comment("keepalive"));
            };
            let Ok(cached) = cached else {
                return Ok(Event::default().comment("keepalive"));
            };

            let api_responding = cached.runner_responding;
            let api_in_use = cached.runner_port_open;

            // Use the primary runner from cached snapshots (not the legacy
            // state.runner which is never updated for user-managed runners).
            let primary_snapshot = runner_snapshots
                .as_ref()
                .ok()
                .and_then(|snaps| snaps.iter().find(|r| r.is_primary));
            let (primary_running, primary_pid) = match primary_snapshot {
                Some(p) => (p.running, p.pid),
                None => {
                    // Fallback to legacy state.runner
                    match state.runner.try_read() {
                        Ok(r) => (r.running, r.pid),
                        Err(_) => (false, None),
                    }
                }
            };

            let overall_status =
                determine_overall_status(primary_running, api_responding, build.build_in_progress);

            // Sync-safe scan: use try_read on each slot's frontend_stale flag.
            // If any slot's lock is contended, skip reporting staleness for
            // that slot on this tick — the flag is a UX nudge, not a hard
            // invariant, and it's fine to miss one tick.
            let frontend_stale_any = state
                .build_pool
                .slots
                .iter()
                .any(|s| s.frontend_stale.try_read().map(|g| *g).unwrap_or(false));

            HealthResponse {
                status: overall_status.to_string(),
                runner: RunnerHealth {
                    running: primary_running,
                    pid: primary_pid,
                    started_at: None, // Not available from cached snapshot
                    api_responding,
                },
                ports: PortsHealth {
                    api_port: PortStatus {
                        port: RUNNER_API_PORT,
                        in_use: api_in_use,
                    },
                },
                watchdog: WatchdogHealth::disabled(),
                build: BuildHealth {
                    in_progress: build.build_in_progress,
                    available_slots: state.build_pool.permits.available_permits(),
                    error_detected: build.build_error_detected,
                    last_error: build.last_build_error.clone(),
                    last_build_at: build.last_build_at.map(|t| t.to_rfc3339()),
                    frontend_stale_any,
                    // SSE path: try_read on the LKG lock — if contended,
                    // skip the field this tick (the next tick will catch up).
                    lkg: state
                        .build_pool
                        .last_known_good
                        .try_read()
                        .ok()
                        .and_then(|g| {
                            g.as_ref().map(|info| LkgHealth {
                                built_at: info.built_at.to_rfc3339(),
                                source_slot: info.source_slot,
                                exe_size: info.exe_size,
                            })
                        }),
                },
                expo: ExpoHealth {
                    running: expo.running,
                    pid: expo.pid,
                    port: expo.port,
                    configured: state.config.expo_dir.is_some(),
                },
                supervisor: SupervisorInfo {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    project_dir: state.config.project_dir.display().to_string(),
                },
                // Read cached runner snapshots (built by background health refresher)
                runners: build_sse_runners(&state),
                sdk_features: SDK_FEATURES.to_vec(),
                sdk_features_doc_url: SDK_FEATURE_DOC_URL,
                build_id: state.build_id.clone(),
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

    let stream = futures::StreamExt::take_until(stream, shutdown);

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
    fn test_external_when_runner_not_tracked_but_api_responding() {
        // User-started (or otherwise externally-managed) runner: supervisor
        // didn't spawn it so `running` is false, but its API is reachable.
        assert_eq!(determine_overall_status(false, true, false), "external");
        assert_eq!(determine_overall_status(false, true, true), "external");
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
            },
            ports: PortsHealth {
                api_port: PortStatus {
                    port: 9876,
                    in_use: true,
                },
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
                available_slots: 0,
                error_detected: false,
                last_error: None,
                last_build_at: None,
                frontend_stale_any: false,
                lkg: None,
            },
            expo: ExpoHealth {
                running: false,
                pid: None,
                port: 8081,
                configured: false,
            },
            supervisor: SupervisorInfo {
                version: "0.1.0".to_string(),
                project_dir: "/tmp/test".to_string(),
            },
            runners: Vec::new(),
            sdk_features: SDK_FEATURES.to_vec(),
            sdk_features_doc_url: SDK_FEATURE_DOC_URL,
            build_id: "2026-04-25T00:00:00+00:00".to_string(),
        };

        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("\"status\":\"healthy\""));
        assert!(json.contains("\"running\":true"));
        assert!(json.contains("\"pid\":1234"));
        assert!(json.contains("\"api_responding\":true"));
        assert!(json.contains("\"sdkFeatures\":["));
        assert!(json.contains("\"softNavigate\""));
        assert!(json.contains("\"sdkFeaturesDocUrl\":\"https://"));
        assert!(json.contains("\"buildId\":\"2026-04-25T00:00:00+00:00\""));
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
