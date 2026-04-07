//! Multi-runner management routes.
//!
//! New endpoints for managing multiple runner instances independently.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::config::RunnerConfig;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager;
use crate::settings;
use crate::state::{ManagedRunner, SharedState};
use std::sync::Arc;
#[cfg(windows)]
use tracing::warn;

#[derive(Deserialize)]
pub struct AddRunnerRequest {
    pub name: String,
    pub port: u16,
}

#[derive(Deserialize)]
pub struct RestartRunnerRequest {
    #[serde(default)]
    pub rebuild: bool,
    /// Source of the restart request. Defaults to "manual".
    #[serde(default = "default_source_manual")]
    pub source: String,
    /// Force restart even if the runner is protected.
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, Default)]
pub struct StopRunnerRequest {
    /// Force stop even if the runner is protected.
    #[serde(default)]
    pub force: bool,
}

fn default_source_manual() -> String {
    "manual".to_string()
}

#[derive(Deserialize)]
pub struct WatchdogRunnerRequest {
    pub enabled: bool,
    #[serde(default)]
    pub reset_attempts: bool,
}

/// GET /runners — list all runners with status.
pub async fn list_runners(
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    let runners = state.get_all_runners().await;
    let mut result = Vec::new();

    for managed in &runners {
        let runner = managed.runner.read().await;
        let watchdog = managed.watchdog.read().await;
        let cached = managed.cached_health.read().await;
        let is_protected = managed.is_protected().await;

        // A runner is "up" if either the supervisor is tracking it as running
        // OR its API is responding (e.g. spawned externally by the runner's instance manager).
        let effectively_running = runner.running || cached.runner_responding;

        result.push(json!({
            "id": managed.config.id,
            "name": managed.config.name,
            "port": managed.config.port,
            "is_primary": managed.config.is_primary,
            "protected": is_protected,
            "running": effectively_running,
            "pid": runner.pid,
            "started_at": runner.started_at.map(|t| t.to_rfc3339()),
            "api_responding": cached.runner_responding,
            "watchdog": {
                "enabled": watchdog.enabled,
                "restart_attempts": watchdog.restart_attempts,
                "last_restart_at": watchdog.last_restart_at.map(|t| t.to_rfc3339()),
                "disabled_reason": watchdog.disabled_reason.clone(),
                "crash_count": watchdog.crash_history.len(),
            }
        }));
    }

    Ok(Json(json!(result)))
}

/// POST /runners — add a new runner config.
pub async fn add_runner(
    State(state): State<SharedState>,
    Json(body): Json<AddRunnerRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Validate name
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(SupervisorError::Validation(
            "Runner name must not be empty".to_string(),
        ));
    }
    if name.len() > 64 {
        return Err(SupervisorError::Validation(
            "Runner name must be 64 characters or fewer".to_string(),
        ));
    }

    if body.port < 1024 {
        return Err(SupervisorError::Validation(
            "Port must be >= 1024".to_string(),
        ));
    }

    // Generate a unique ID
    let id = format!("runner-{}", uuid_simple());

    let runner_config = RunnerConfig {
        id: id.clone(),
        name: name.clone(),
        port: body.port,
        is_primary: false,
        protected: true,
    };

    // Check for port conflicts and insert under a single write lock to avoid TOCTOU race.
    let managed = Arc::new(ManagedRunner::new(
        runner_config.clone(),
        state.config.watchdog_enabled_at_start,
    ));
    {
        let mut runners = state.runners.write().await;
        for existing in runners.values() {
            if existing.config.port == body.port {
                return Err(SupervisorError::Validation(format!(
                    "Port {} is already in use by runner '{}'",
                    body.port, existing.config.name
                )));
            }
        }
        runners.insert(id.clone(), managed);
    }

    // Persist to settings
    let path = settings::settings_path(&state.config);
    settings::add_runner(&path, &runner_config);

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Added runner '{}' (id: {}, port: {})", name, id, body.port),
        )
        .await;

    Ok(Json(json!({
        "id": id,
        "name": name,
        "port": body.port,
        "message": "Runner added successfully"
    })))
}

/// DELETE /runners/{id} — remove a runner config (must be stopped).
pub async fn remove_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, SupervisorError> {
    let managed = state
        .get_runner(&id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(id.clone()))?;

    if managed.config.is_primary {
        return Err(SupervisorError::Validation(
            "Cannot remove the primary runner".to_string(),
        ));
    }

    {
        let runner = managed.runner.read().await;
        if runner.running {
            return Err(SupervisorError::Validation(
                "Runner must be stopped before removal. Call POST /runners/{id}/stop first."
                    .to_string(),
            ));
        }
    }

    let name = managed.config.name.clone();

    {
        let mut runners = state.runners.write().await;
        runners.remove(&id);
    }

    let path = settings::settings_path(&state.config);
    settings::remove_runner(&path, &id);

    // Best-effort: remove the runner's isolated WebView2 data folder so its
    // localStorage, cookies, and caches don't accumulate on disk. Primary
    // runners are never reached here (the is_primary check above returns
    // early), so this always targets a non-primary folder.
    #[cfg(windows)]
    {
        if let Err(e) =
            crate::process::windows::remove_webview2_user_data_folder(&id, false).await
        {
            warn!(
                "Failed to remove WebView2 data folder for runner '{}': {}",
                id, e
            );
        }
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Removed runner '{}' (id: {})", name, id),
        )
        .await;

    Ok(Json(json!({
        "status": "removed",
        "message": format!("Runner '{}' removed", name)
    })))
}

/// POST /runners/{id}/start — start a specific runner.
pub async fn start_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, SupervisorError> {
    manager::start_runner_by_id(&state, &id).await?;

    Ok(Json(json!({
        "status": "started",
        "message": format!("Runner '{}' started", id)
    })))
}

/// POST /runners/{id}/stop — stop a specific runner.
pub async fn stop_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: Option<Json<StopRunnerRequest>>,
) -> Result<impl IntoResponse, SupervisorError> {
    let _force = body.map(|b| b.force).unwrap_or(false);

    manager::stop_runner_by_id(&state, &id).await?;

    Ok(Json(json!({
        "status": "stopped",
        "message": format!("Runner '{}' stopped", id)
    })))
}

/// POST /runners/{id}/restart — restart a specific runner.
/// Protected runners require `force: true` in the request body.
pub async fn restart_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RestartRunnerRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    let source = match body.source.as_str() {
        "workflow_loop" => crate::diagnostics::RestartSource::WorkflowLoop,
        "watchdog" => crate::diagnostics::RestartSource::Watchdog,
        _ => crate::diagnostics::RestartSource::Manual,
    };

    manager::restart_runner_by_id(&state, &id, body.rebuild, source, body.force).await?;

    Ok(Json(json!({
        "status": "restarted",
        "message": format!("Runner '{}' restarted", id)
    })))
}

/// POST /runners/{id}/watchdog — control per-runner watchdog.
pub async fn control_runner_watchdog(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<WatchdogRunnerRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    let managed = state
        .get_runner(&id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(id.clone()))?;

    let response = {
        let mut wd = managed.watchdog.write().await;
        wd.enabled = body.enabled;

        if body.reset_attempts {
            wd.restart_attempts = 0;
            wd.disabled_reason = None;
            wd.crash_history.clear();
        }

        json!({
            "watchdog": {
                "enabled": wd.enabled,
                "restart_attempts": wd.restart_attempts,
            }
        })
    };

    state.notify_health_change();

    Ok(Json(response))
}

#[derive(Deserialize)]
pub struct ProtectRunnerRequest {
    pub protected: bool,
}

/// POST /runners/{id}/protect — toggle protection on a runner.
/// Protected runners cannot be stopped or restarted by smart rebuild, watchdog, or workflow loop.
pub async fn protect_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<ProtectRunnerRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    let managed = state
        .get_runner(&id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(id.clone()))?;

    let name = managed.config.name.clone();

    // Persist to settings FIRST — if this fails, runtime state stays unchanged (no TOCTOU).
    let settings_path = settings::settings_path(&state.config);
    let mut settings = settings::load_settings(&settings_path);
    if let Some(cfg) = settings.runners.iter_mut().find(|r| r.id == id) {
        cfg.protected = body.protected;
        if let Err(e) = settings::try_save_settings(&settings_path, &settings) {
            return Err(SupervisorError::Other(format!(
                "Failed to persist protection setting: {e}"
            )));
        }
    }

    // Persistence succeeded — now update the runtime protection flag
    {
        let mut protected = managed.protected.write().await;
        *protected = body.protected;
    }

    let action = if body.protected {
        "protected"
    } else {
        "unprotected"
    };
    state
        .logs
        .emit(
            crate::log_capture::LogSource::Supervisor,
            crate::log_capture::LogLevel::Info,
            format!("Runner '{}' is now {}", name, action),
        )
        .await;

    state.notify_health_change();

    Ok(Json(json!({
        "status": action,
        "protected": body.protected,
        "message": format!("Runner '{}' is now {}", name, action)
    })))
}

/// GET /runners/{id}/ui-bridge/{*path} — proxy UI Bridge to specific runner.
pub async fn proxy_ui_bridge(
    State(state): State<SharedState>,
    Path((id, path)): Path<(String, String)>,
    req: axum::extract::Request,
) -> axum::response::Response {
    let managed = match state.get_runner(&id).await {
        Some(m) => m,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Runner '{}' not found", id)})),
            )
                .into_response();
        }
    };

    // Check runner health
    let cached = managed.cached_health.read().await;
    if !cached.runner_responding {
        drop(cached);
        return (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("Runner '{}' is not responding", managed.config.name),
                "runner_port": managed.config.port,
            })),
        )
            .into_response();
    }
    drop(cached);

    let port = managed.config.port;
    let method = req.method().clone();
    let uri = req.uri().clone();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();

    let target_url = format!("http://127.0.0.1:{}/ui-bridge/{}{}", port, path, query);

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Failed to read request body: {e}")})),
            )
                .into_response();
        }
    };

    let mut outgoing = state
        .http_client
        .request(method, &target_url)
        .timeout(std::time::Duration::from_secs(15));
    if let Some(ct) = content_type {
        outgoing = outgoing.header("content-type", ct);
    }
    if !body_bytes.is_empty() {
        outgoing = outgoing.body(body_bytes);
    }

    match outgoing.send().await {
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let resp_ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            match resp.bytes().await {
                Ok(bytes) => {
                    let mut builder = axum::response::Response::builder().status(status);
                    if let Some(ct) = resp_ct {
                        builder = builder.header("content-type", ct);
                    }
                    builder
                        .body(axum::body::Body::from(bytes))
                        .unwrap_or_else(|_| {
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        })
                }
                Err(e) => (
                    axum::http::StatusCode::BAD_GATEWAY,
                    Json(json!({"error": format!("Failed to read runner response: {e}")})),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            let msg = format!("Runner proxy error: {e}");
            (
                axum::http::StatusCode::BAD_GATEWAY,
                Json(json!({"error": msg})),
            )
                .into_response()
        }
    }
}

// --- Spawn test runner ---

#[derive(Deserialize)]
pub struct SpawnTestRequest {
    /// Whether to rebuild before spawning. Default: false (uses existing binary).
    #[serde(default)]
    pub rebuild: bool,
    /// If true, block until the runner's API is healthy before returning.
    /// Polls the runner's /health endpoint every 2s, up to `wait_timeout_secs`.
    /// Default: false (return immediately after process spawn).
    #[serde(default)]
    pub wait: bool,
    /// Maximum seconds to wait for the runner to become healthy.
    /// Only used when `wait` is true. Default: 120 seconds.
    #[serde(default = "default_wait_timeout")]
    pub wait_timeout_secs: u64,
}

fn default_wait_timeout() -> u64 {
    120
}

/// POST /runners/spawn-test — spawn an ephemeral test runner on the next available port.
///
/// Automatically picks a free port (9877-9899), creates a temporary runner,
/// starts it, and returns the connection details. The runner is not protected
/// and not persisted to settings — it gets cleaned up on primary restart or
/// explicit stop.
pub async fn spawn_test(
    State(state): State<SharedState>,
    Json(body): Json<SpawnTestRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Find next available port by checking existing runners
    let used_ports: Vec<u16> = {
        let runners = state.runners.read().await;
        runners.values().map(|r| r.config.port).collect()
    };

    let port = (9877..=9899)
        .find(|p| !used_ports.contains(p))
        .ok_or_else(|| {
            SupervisorError::Validation(
                "No available ports in range 9877-9899. Stop some test runners first.".to_string(),
            )
        })?;

    // Rebuild if requested
    if body.rebuild {
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Rebuilding runner before spawning test runner on port {}",
                    port
                ),
            )
            .await;
        crate::build_monitor::run_cargo_build(&state).await?;
    }

    // Check that the exe exists
    let exe_path = state.config.runner_exe_path();
    if !exe_path.exists() {
        return Err(SupervisorError::Process(format!(
            "Runner binary not found at {:?}. Use rebuild: true to build it first.",
            exe_path
        )));
    }

    let id = format!("test-{}", uuid_simple());
    let name = format!("test-{}", port);

    let runner_config = RunnerConfig {
        id: id.clone(),
        name: name.clone(),
        port,
        is_primary: false,
        protected: true,
    };

    // Insert into state (not persisted — ephemeral)
    let managed = Arc::new(ManagedRunner::new(runner_config, false));
    {
        let mut runners = state.runners.write().await;
        runners.insert(id.clone(), managed);
    }

    // Start the runner
    if let Err(e) = manager::start_runner_by_id(&state, &id).await {
        // Clean up on failure
        let mut runners = state.runners.write().await;
        runners.remove(&id);
        return Err(e);
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Spawned test runner '{}' on port {} (id: {})",
                name, port, id
            ),
        )
        .await;

    state.notify_health_change();

    // If wait=true, poll the runner's health endpoint until it responds or times out
    let mut healthy = false;
    let mut wait_ms: u64 = 0;
    if body.wait {
        let timeout = std::time::Duration::from_secs(body.wait_timeout_secs);
        let poll_interval = std::time::Duration::from_secs(2);
        let start = std::time::Instant::now();
        let health_url = format!("http://localhost:{}/health", port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();

        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Waiting up to {}s for test runner '{}' to become healthy...",
                    body.wait_timeout_secs, name
                ),
            )
            .await;

        loop {
            if start.elapsed() >= timeout {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Test runner '{}' did not become healthy within {}s",
                            name, body.wait_timeout_secs
                        ),
                    )
                    .await;
                break;
            }

            tokio::time::sleep(poll_interval).await;

            match client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    healthy = true;
                    wait_ms = start.elapsed().as_millis() as u64;
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Info,
                            format!("Test runner '{}' is healthy (took {}ms)", name, wait_ms),
                        )
                        .await;

                    // Clear localStorage for this port to avoid stale tab state
                    let clear_url = format!(
                        "http://localhost:{}/ui-bridge/control/clear-storage",
                        port
                    );
                    if let Err(e) = client
                        .post(&clear_url)
                        .json(&serde_json::json!({}))
                        .send()
                        .await
                    {
                        tracing::warn!("Failed to clear storage for test runner: {}", e);
                    }

                    break;
                }
                _ => continue,
            }
        }
    }

    Ok(Json(json!({
        "id": id,
        "name": name,
        "port": port,
        "status": if body.wait { if healthy { "healthy" } else { "timeout" } } else { "started" },
        "wait_ms": wait_ms,
        "api_url": format!("http://localhost:{}", port),
        "ui_bridge_url": format!("http://localhost:{}/ui-bridge", port),
        "message": if body.wait && healthy {
            format!("Test runner ready on port {} ({}ms)", port, wait_ms)
        } else if body.wait {
            format!("Test runner spawned on port {} but did not become healthy within {}s", port, body.wait_timeout_secs)
        } else {
            format!("Test runner spawned on port {}", port)
        }
    })))
}

// --- Helpers ---

/// Simple unique ID generator (timestamp + incrementing counter).
fn uuid_simple() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", ts, seq)
}
