//! Multi-runner management routes.
//!
//! New endpoints for managing multiple runner instances independently.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use axum::extract::Query;
use axum::response::sse::{Event, KeepAlive, Sse};
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::config::RunnerConfig;
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager;
use crate::settings;
use crate::state::{ManagedRunner, SharedState};
use std::sync::Arc;
use tracing::info;
#[cfg(windows)]
use tracing::warn;

#[derive(Deserialize)]
pub struct AddRunnerRequest {
    pub name: String,
    pub port: u16,
    #[serde(default)]
    pub server_mode: Option<bool>,
    #[serde(default)]
    pub restate_ingress_port: Option<u16>,
    #[serde(default)]
    pub restate_admin_port: Option<u16>,
    #[serde(default)]
    pub restate_service_port: Option<u16>,
    #[serde(default)]
    pub external_restate_admin_url: Option<String>,
    #[serde(default)]
    pub external_restate_ingress_url: Option<String>,
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
    // Snapshot of ui_error + derived_status lives on the health cache, which
    // the background refresher updates every ~2s by GETting /health on each
    // runner. We index into it by id rather than re-issuing HTTP here so the
    // listing stays cheap (~100µs) regardless of fleet size.
    let cached_snapshots = state.cached_runner_health.read().await;
    let mut result = Vec::new();

    for managed in &runners {
        let runner = managed.runner.read().await;
        let watchdog = managed.watchdog.read().await;
        let cached = managed.cached_health.read().await;
        let is_protected = managed.is_protected().await;

        // A runner is "up" if either the supervisor is tracking it as running
        // OR its API is responding (e.g. spawned externally by the runner's instance manager).
        let effectively_running = runner.running || cached.runner_responding;

        // Expose the per-runner log/event URLs so callers don't have to
        // reverse-engineer them from the server routes — the per-runner
        // `logs` endpoint is the only place that captures the runner's own
        // stdout/stderr (unlike /logs/history, which is the supervisor's
        // own buffer and does not include runner logs).
        let logs_url = format!("/runners/{}/logs", managed.config.id);
        let logs_stream_url = format!("/runners/{}/logs/stream", managed.config.id);

        let snapshot = cached_snapshots.iter().find(|c| c.id == managed.config.id);
        let ui_error = snapshot.and_then(|c| c.ui_error.clone());
        let recent_crash = snapshot.and_then(|c| c.recent_crash.clone());
        let derived_status = snapshot
            .map(|c| c.derived_status.clone())
            .unwrap_or_default();

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
            "logs_url": logs_url,
            "logs_stream_url": logs_stream_url,
            "ui_error": ui_error,
            "recent_crash": recent_crash,
            "derived_status": derived_status,
            "watchdog": {
                "enabled": watchdog.enabled,
                "restart_attempts": watchdog.restart_attempts,
                "last_restart_at": watchdog.last_restart_at.map(|t| t.to_rfc3339()),
                "disabled_reason": watchdog.disabled_reason.clone(),
                "crash_count": watchdog.crash_history.len(),
            }
        }));
    }
    drop(cached_snapshots);

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

    let server_mode = body.server_mode.unwrap_or(false);

    // Check for port conflicts, allocate the Restate triple (when server_mode),
    // build the RunnerConfig, and insert — all under a single write lock so
    // allocation is race-free against concurrent registrations.
    let runner_config = {
        let mut runners = state.runners.write().await;
        for existing in runners.values() {
            if existing.config.port == body.port {
                return Err(SupervisorError::Validation(format!(
                    "Port {} is already in use by runner '{}'",
                    body.port, existing.config.name
                )));
            }
        }

        // Snapshot existing configs so the Restate allocator can see current
        // assignments under this same write lock.
        let existing_configs: Vec<RunnerConfig> =
            runners.values().map(|r| r.config.clone()).collect();
        let resolved = crate::process::restate_port::resolve_ports(
            &existing_configs,
            server_mode,
            body.restate_ingress_port,
            body.restate_admin_port,
            body.restate_service_port,
            body.external_restate_admin_url.clone(),
            body.external_restate_ingress_url.clone(),
        )
        .map_err(SupervisorError::Validation)?;

        let runner_config = RunnerConfig {
            id: id.clone(),
            name: name.clone(),
            port: body.port,
            is_primary: false,
            protected: true,
            server_mode,
            restate_ingress_port: resolved.ingress_port,
            restate_admin_port: resolved.admin_port,
            restate_service_port: resolved.service_port,
            external_restate_admin_url: resolved.external_admin_url,
            external_restate_ingress_url: resolved.external_ingress_url,
            extra_env: Default::default(),
        };

        let managed = Arc::new(ManagedRunner::new_with_log_dir(
            runner_config.clone(),
            state.config.watchdog_enabled_at_start,
            state.config.log_dir.as_deref(),
        ));
        runners.insert(id.clone(), managed);
        runner_config
    };

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

    // Preserve logs in the stopped-runners cache before dropping the
    // ManagedRunner so post-mortem debugging still works via
    // `?include_stopped=true` on the logs endpoint.
    {
        let snapshot = crate::process::stopped_cache::snapshot_from_managed(
            managed.as_ref(),
            None,
            crate::process::stopped_cache::StopReason::GracefulStop,
        )
        .await;
        let mut cache = state.stopped_runners.write().await;
        crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
    }

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
        if let Err(e) = crate::process::windows::remove_webview2_user_data_folder(&id, false).await
        {
            warn!(
                "Failed to remove WebView2 data folder for runner '{}': {}",
                id, e
            );
        }

        // Also remove per-instance app data dirs (dev-logs, restate journal,
        // macros, prompts, playwright, contexts) that the runner writes under
        // an `instance-<name>` subdirectory. The runner sees the env var
        // `QONTINUI_INSTANCE_NAME = <managed.config.name>`, so cleanup keys
        // off the name, not the id.
        if let Err(e) = crate::process::windows::remove_runner_app_data_dirs(&name, false).await {
            warn!(
                "Failed to remove per-instance app data for runner '{}': {}",
                name, e
            );
        }
    }

    // Clean up the per-runner exe copy for temp runners to prevent disk bloat.
    if manager::is_temp_runner(&id) {
        let exe_copy = state.config.runner_exe_copy_path(&id);
        if exe_copy.exists() {
            if let Err(e) = std::fs::remove_file(&exe_copy) {
                warn!("Failed to remove runner exe copy {:?}: {}", exe_copy, e);
            } else {
                info!("Removed runner exe copy {:?}", exe_copy);
            }
        }
        let pdb_copy = exe_copy.with_extension("pdb");
        if pdb_copy.exists() {
            let _ = std::fs::remove_file(&pdb_copy);
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

/// POST /runners/purge-stale — remove all stopped test runners from the registry.
///
/// Test runners that crash without an explicit stop call (or whose stop response
/// was lost) accumulate in the in-memory registry and block port allocation.
/// This endpoint finds every `test-*` runner that is not currently running and
/// removes it, freeing its port for reuse. Also kills any orphaned process still
/// bound to the port.
pub async fn purge_stale(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    let purged_triples = purge_stale_test_runners_core(&state).await;
    let count = purged_triples.len();
    let purged: Vec<serde_json::Value> = purged_triples
        .iter()
        .map(|(id, name, port)| json!({ "id": id, "name": name, "port": port }))
        .collect();

    if count > 0 {
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!("Purged {} stale test runner(s)", count),
            )
            .await;
    }

    Ok(Json(json!({
        "purged": count,
        "runners": purged,
    })))
}

/// Shared implementation behind `POST /runners/purge-stale` and the periodic
/// `reap_stale_test_runners` background task in `main.rs`. Returns the
/// `(id, name, port)` of every test runner that was evicted — placeholders
/// whose process never became responsive, plus zombies where the process died
/// but the in-memory registry still said "running".
pub async fn purge_stale_test_runners_core(state: &SharedState) -> Vec<(String, String, u16)> {
    let runners = state.get_all_runners().await;
    let mut purged: Vec<(String, String, u16)> = Vec::new();

    for managed in &runners {
        if !manager::is_temp_runner(&managed.config.id) {
            continue;
        }
        let is_running = {
            let runner = managed.runner.read().await;
            runner.running
        };
        if is_running {
            // Try to detect zombie: process is gone but state says running.
            // If nothing is actually listening on the port, treat it as stale.
            let port_alive = crate::process::port::is_port_in_use(managed.config.port);
            if port_alive {
                continue; // genuinely running, skip
            }
            // Port is free but state says running — it crashed. Fix state.
            {
                let mut runner = managed.runner.write().await;
                runner.running = false;
                runner.pid = None;
            }
        }

        let id = managed.config.id.clone();
        let name = managed.config.name.clone();
        let port = managed.config.port;

        // Best-effort kill anything still on the port
        let _ = crate::process::windows::kill_by_port(port).await;

        // Preserve logs for post-mortem. purge-stale only targets test runners
        // whose process is already dead, so mark them `Crashed`.
        {
            let snapshot = crate::process::stopped_cache::snapshot_from_managed(
                managed.as_ref(),
                None,
                crate::process::stopped_cache::StopReason::Crashed,
            )
            .await;
            let mut cache = state.stopped_runners.write().await;
            crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
        }

        // Remove from registry
        {
            let mut runners_map = state.runners.write().await;
            runners_map.remove(&id);
        }

        // Best-effort cleanup of on-disk data
        #[cfg(windows)]
        {
            if let Err(e) =
                crate::process::windows::remove_webview2_user_data_folder(&id, false).await
            {
                warn!(
                    "purge-stale: failed to remove WebView2 data for '{}': {}",
                    id, e
                );
            }
            if let Err(e) = crate::process::windows::remove_runner_app_data_dirs(&name, false).await
            {
                warn!(
                    "purge-stale: failed to remove app data for '{}': {}",
                    name, e
                );
            }
        }

        // Clean up the per-runner exe copy to prevent disk bloat.
        let exe_copy = state.config.runner_exe_copy_path(&id);
        if exe_copy.exists() {
            if let Err(e) = std::fs::remove_file(&exe_copy) {
                warn!(
                    "purge-stale: failed to remove exe copy {:?}: {}",
                    exe_copy, e
                );
            } else {
                info!("purge-stale: removed exe copy {:?}", exe_copy);
            }
        }
        let pdb_copy = exe_copy.with_extension("pdb");
        if pdb_copy.exists() {
            let _ = std::fs::remove_file(&pdb_copy);
        }

        info!(
            "purge-stale: removed test runner '{}' (port {})",
            name, port
        );
        purged.push((id, name, port));
    }

    purged
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
    /// Optional hint identifying who requested this build (e.g. an agent name).
    /// Recorded with the active build info for `GET /builds` visibility.
    #[serde(default)]
    pub requester_id: Option<String>,
    /// Maximum seconds to wait in the build queue before giving up with 504.
    /// Only meaningful when `rebuild: true` and all slots are busy.
    /// `None` means wait indefinitely (the default).
    #[serde(default)]
    pub queue_timeout_secs: Option<u64>,
    /// Optional post-spawn health probe window in milliseconds.
    ///
    /// After the child process is spawned, the supervisor polls
    /// `GET http://localhost:{port}/health` every 200ms until it succeeds or
    /// this many milliseconds elapse. Defaults to 6000 (6 seconds).
    ///
    /// Set to `0` to skip the probe entirely (legacy behavior — return
    /// immediately after spawn without verifying the child bound its API
    /// port).
    ///
    /// Outcomes:
    /// - Probe succeeds: response includes `health_probe_ms` (elapsed ms).
    /// - Probe times out and child is alive: returns HTTP 502
    ///   `runner_started_but_unresponsive` and stops/cleans the runner.
    /// - Probe times out and child has died: returns HTTP 500
    ///   `runner_died_during_startup` and stops/cleans the runner.
    #[serde(default = "default_health_probe_timeout")]
    pub health_probe_timeout_ms: u64,
    #[serde(default)]
    pub server_mode: Option<bool>,
    #[serde(default)]
    pub restate_ingress_port: Option<u16>,
    #[serde(default)]
    pub restate_admin_port: Option<u16>,
    #[serde(default)]
    pub restate_service_port: Option<u16>,
    #[serde(default)]
    pub external_restate_admin_url: Option<String>,
    #[serde(default)]
    pub external_restate_ingress_url: Option<String>,
    /// Additional environment variables forwarded to the spawned test runner.
    /// Applied after the supervisor's hardcoded envs so callers can override
    /// (e.g. pointing `QONTINUI_API_URL` at a fake backend) or inject
    /// feature flags like `QONTINUI_SCRIPTED_OUTPUT=1` without requiring
    /// the supervisor itself to have the var in its environment.
    ///
    /// Temp runners are ephemeral — this is not persisted across supervisor
    /// restarts.
    #[serde(default)]
    pub extra_env: std::collections::HashMap<String, String>,
}

fn default_health_probe_timeout() -> u64 {
    6000
}

fn default_wait_timeout() -> u64 {
    120
}

#[derive(Deserialize)]
pub struct SpawnNamedRequest {
    pub name: String,
    pub port: Option<u16>,
    #[serde(default)]
    pub rebuild: bool,
    #[serde(default)]
    pub wait: bool,
    #[serde(default = "default_wait_timeout")]
    pub wait_timeout_secs: u64,
    pub requester_id: Option<String>,
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub server_mode: Option<bool>,
    #[serde(default)]
    pub restate_ingress_port: Option<u16>,
    #[serde(default)]
    pub restate_admin_port: Option<u16>,
    #[serde(default)]
    pub restate_service_port: Option<u16>,
    #[serde(default)]
    pub external_restate_admin_url: Option<String>,
    #[serde(default)]
    pub external_restate_ingress_url: Option<String>,
}

/// POST /runners/spawn-test — spawn an ephemeral test runner on the next available port.
///
/// Automatically picks a free port (9877-9899), creates a temporary runner,
/// starts it, and returns the connection details. The runner is not protected
/// and not persisted to settings — it gets cleaned up on primary restart or
/// explicit stop.
///
/// ## Queue behavior when `rebuild: true`
///
/// The supervisor runs a fixed pool of N parallel cargo builds (default 3).
/// When all slots are busy and a caller sets `rebuild: true`:
///
/// - **Default (blocking):** The HTTP request is held open until a slot frees,
///   then the build and spawn proceed. Pass `queue_timeout_secs` to bound the wait.
/// - **`X-Queue-Mode: no-wait` header:** Returns immediately with 503 and a body
///   describing current build slot activity and the caller's queue position.
pub async fn spawn_test(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<SpawnTestRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    let no_wait = headers
        .get("X-Queue-Mode")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("no-wait"))
        .unwrap_or(false);

    // Atomically reserve a free port AND insert a placeholder ManagedRunner
    // into the registry under a single write lock. Without this, two
    // concurrent `spawn-test` calls with `rebuild: true` would both scan the
    // port set (under read lock) before either insertion happened, and both
    // would pick the same free port — the second spawn would then collide.
    //
    // We insert the ManagedRunner before the (potentially long) build so
    // subsequent scanners see the reservation. If the build fails, the
    // placeholder is removed.
    //
    // We also keep an Arc to the ManagedRunner for the rest of this handler
    // so the eventual start call uses it directly, bypassing an id-based
    // registry lookup. Without that, a transient "Runner not found" 404 is
    // possible when a concurrent path (reaper, stop_all, failed probe on a
    // sibling spawn) removes our id between insertion and start. Smoke tests
    // hit this ~1 in 10 times under load.
    let (id, port, managed) = {
        let mut runners = state.runners.write().await;
        let used_ports: std::collections::HashSet<u16> =
            runners.values().map(|r| r.config.port).collect();
        let port = (9877..=9899)
            .find(|p| !used_ports.contains(p))
            .ok_or_else(|| {
                SupervisorError::Validation(
                    "No available ports in range 9877-9899. Stop some test runners first."
                        .to_string(),
                )
            })?;
        let id = format!("test-{}", uuid_simple());
        let name = format!("test-{}", port);

        let server_mode = body.server_mode.unwrap_or(false);
        let existing_configs: Vec<RunnerConfig> =
            runners.values().map(|r| r.config.clone()).collect();
        let resolved = crate::process::restate_port::resolve_ports(
            &existing_configs,
            server_mode,
            body.restate_ingress_port,
            body.restate_admin_port,
            body.restate_service_port,
            body.external_restate_admin_url.clone(),
            body.external_restate_ingress_url.clone(),
        )
        .map_err(SupervisorError::Validation)?;

        let runner_config = RunnerConfig {
            id: id.clone(),
            name,
            port,
            is_primary: false,
            protected: true,
            server_mode,
            restate_ingress_port: resolved.ingress_port,
            restate_admin_port: resolved.admin_port,
            restate_service_port: resolved.service_port,
            external_restate_admin_url: resolved.external_admin_url,
            external_restate_ingress_url: resolved.external_ingress_url,
            extra_env: body.extra_env.clone(),
        };
        let managed = Arc::new(ManagedRunner::new_with_log_dir(
            runner_config,
            false,
            state.config.log_dir.as_deref(),
        ));
        runners.insert(id.clone(), managed.clone());
        (id, port, managed)
    };

    // Rebuild if requested
    if body.rebuild {
        // If caller opted out of waiting AND all slots are currently busy,
        // return 503 with queue info instead of blocking.
        if no_wait && state.build_pool.permits.available_permits() == 0 {
            let snap = state.build_pool.snapshot().await;
            let now = chrono::Utc::now();
            let mut min_elapsed_secs: Option<f64> = None;
            let active: Vec<serde_json::Value> = snap
                .iter()
                .filter_map(|(id, _td, info)| {
                    info.as_ref().map(|i| {
                        let elapsed = (now - i.started_at).num_seconds().max(0) as f64;
                        min_elapsed_secs = Some(match min_elapsed_secs {
                            Some(cur) => cur.min(elapsed),
                            None => elapsed,
                        });
                        json!({
                            "slot": id,
                            "started_at": i.started_at.to_rfc3339(),
                            "elapsed_secs": elapsed as i64,
                            "requester_id": i.requester_id,
                            "rebuild_kind": i.rebuild_kind,
                        })
                    })
                })
                .collect();
            // Compute estimated_wait_secs from slot history.
            let mut sum: f64 = 0.0;
            let mut count: usize = 0;
            for slot in &state.build_pool.slots {
                let h = slot.history.read().await;
                for d in &h.recent_durations_secs {
                    sum += *d;
                    count += 1;
                }
            }
            let avg = if count > 0 {
                Some(sum / count as f64)
            } else {
                None
            };
            let estimated_wait_secs = avg.map(|a| (a - min_elapsed_secs.unwrap_or(0.0)).max(0.0));

            let queued = state
                .build_pool
                .queue_depth
                .load(std::sync::atomic::Ordering::Relaxed);
            return Err(SupervisorError::BuildPoolFull {
                queue_position: queued + 1,
                active_builds: active,
                estimated_wait_secs,
            });
        }

        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Rebuilding runner before spawning test runner on port {} (requester={:?})",
                    port, body.requester_id
                ),
            )
            .await;

        // Run the build, optionally bounded by a queue timeout.
        // On any failure (build error, timeout, etc.), remove the placeholder
        // we reserved above so the port doesn't leak.
        let build_fut =
            crate::build_monitor::run_cargo_build_with_requester(&state, body.requester_id.clone());
        let build_result = match body.queue_timeout_secs {
            Some(secs) => {
                let timeout = std::time::Duration::from_secs(secs);
                match tokio::time::timeout(timeout, build_fut).await {
                    Ok(r) => r,
                    Err(_) => Err(SupervisorError::Timeout(format!(
                        "Build queue timeout: waited {}s for a build slot",
                        secs
                    ))),
                }
            }
            None => build_fut.await,
        };
        if let Err(e) = build_result {
            // Release the placeholder port reservation.
            let mut runners = state.runners.write().await;
            runners.remove(&id);
            return Err(e);
        }
    }

    // Check that a runner binary exists in some build slot (or at the legacy path).
    // Without this check, a fresh supervisor would succeed the request only to
    // fail inside `start_runner_by_id` with a less helpful error. If the check
    // fails, remove the placeholder we reserved above so the port frees up.
    if manager::resolve_source_exe(&state).await.is_err() {
        let mut runners = state.runners.write().await;
        runners.remove(&id);
        return Err(SupervisorError::Process(
            "Runner binary not found in any build slot. Use rebuild: true to build it first."
                .to_string(),
        ));
    }

    let name = format!("test-{}", port);

    // Start the runner using the Arc captured at insertion time. This avoids
    // the id-based lookup in `start_runner_by_id` which can race with
    // concurrent paths that remove the id from the registry (e.g. a sibling
    // spawn's failed health probe, stop_all_temp_runners, the reaper).
    // `start_managed_runner` also re-inserts the Arc if the id went missing,
    // so the subsequent health probe and /runners lookups still work.
    if let Err(e) = manager::start_managed_runner(&state, &managed).await {
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

    // --- Post-spawn health probe ---
    //
    // Quick verification that the child actually bound its API port. The child
    // process may stay alive (e.g. a lifecycle task keeps ticking) even when
    // axum failed to bind — without this probe, callers would poll /health for
    // minutes before realizing the runner is dead.
    //
    // - `health_probe_timeout_ms == 0` skips the probe entirely (escape hatch).
    // - On success: continue to the existing flow with `health_probe_ms` in the
    //   response body.
    // - On timeout + child alive: return 502 `runner_started_but_unresponsive`,
    //   stop the runner, and let the next spawn try fresh.
    // - On timeout + child dead: return 500 `runner_died_during_startup`, stop,
    //   and clean up.
    let mut health_probe_ms: Option<u64> = None;
    let mut probed_git_sha: Option<String> = None;
    if body.health_probe_timeout_ms > 0 {
        match probe_runner_health(&state, &id, port, body.health_probe_timeout_ms).await {
            ProbeOutcome::Healthy {
                elapsed_ms,
                git_sha,
            } => {
                health_probe_ms = Some(elapsed_ms);
                probed_git_sha = git_sha;
            }
            ProbeOutcome::Failed {
                elapsed_ms,
                child_alive,
                pid,
                recent_logs,
            } => {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Test runner '{}' failed health probe after {}ms (alive={}); stopping.",
                            name, elapsed_ms, child_alive
                        ),
                    )
                    .await;

                // Snapshot for post-mortem BEFORE removal. stop_runner_by_id
                // will also snapshot (as GracefulStop) for test- runners, but
                // we overwrite with Crashed here since a health-probe failure
                // is the more accurate reason.
                if let Some(managed) = state.get_runner(&id).await {
                    let snapshot = crate::process::stopped_cache::snapshot_from_managed(
                        managed.as_ref(),
                        None,
                        crate::process::stopped_cache::StopReason::Crashed,
                    )
                    .await;
                    let mut cache = state.stopped_runners.write().await;
                    crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
                }

                // Stop & clean up so we don't leak a zombie.
                let _ = manager::stop_runner_by_id(&state, &id).await;
                {
                    let mut runners = state.runners.write().await;
                    runners.remove(&id);
                }
                state.notify_health_change();

                let (status, error_kind) = if child_alive {
                    (
                        axum::http::StatusCode::BAD_GATEWAY,
                        "runner_started_but_unresponsive",
                    )
                } else {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "runner_died_during_startup",
                    )
                };

                let body = json!({
                    "error": error_kind,
                    "id": id,
                    "pid": pid,
                    "port": port,
                    "elapsed_ms": elapsed_ms,
                    "recent_logs": recent_logs,
                });

                return Ok((status, Json(body)).into_response());
            }
        }
    }

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
                    let clear_url =
                        format!("http://localhost:{}/ui-bridge/control/clear-storage", port);
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

    // Resolve the source exe to report its metadata so callers can detect
    // stale binaries ("this binary is older than my last commit").
    let exe_meta = crate::process::manager::resolve_source_exe(&state)
        .await
        .ok()
        .and_then(|p| crate::process::manager::binary_meta(&p));

    // Determine if the slot we used for the binary has a stale frontend
    // baked in. We consult `last_successful_slot` (the slot whose exe the
    // spawn-start path copies) — if that slot's `frontend_stale` is true, the
    // caller's runner is running with a potentially-old UI.
    let (frontend_stale, stale_slot_id) = resolve_frontend_stale_for_spawn(&state).await;

    let mut resp = json!({
        "id": id,
        "name": name,
        "port": port,
        "status": if body.wait { if healthy { "healthy" } else { "timeout" } } else { "started" },
        "wait_ms": wait_ms,
        "api_url": format!("http://localhost:{}", port),
        "ui_bridge_url": format!("http://localhost:{}/ui-bridge", port),
        // Per-runner log endpoints — these capture the runner's own
        // stdout/stderr. /logs/history on the supervisor has the
        // supervisor's own buffer and does NOT include runner logs, so
        // agents testing runner internals should hit these instead.
        "logs_url": format!("/runners/{}/logs", id),
        "logs_stream_url": format!("/runners/{}/logs/stream", id),
        "message": if body.wait && healthy {
            format!("Test runner ready on port {} ({}ms)", port, wait_ms)
        } else if body.wait {
            format!("Test runner spawned on port {} but did not become healthy within {}s", port, body.wait_timeout_secs)
        } else {
            format!("Test runner spawned on port {}", port)
        }
    });
    if let Some(meta) = exe_meta {
        resp["binary_mtime"] = json!(meta.binary_mtime);
        resp["binary_size_bytes"] = json!(meta.binary_size_bytes);
    }
    if let Some(ms) = health_probe_ms {
        resp["health_probe_ms"] = json!(ms);
    }
    if let Some(sha) = probed_git_sha {
        resp["git_sha"] = json!(sha);
    }

    // If the runner's binary came from a slot with a stale frontend, surface
    // a loud top-level warning + header so calling agents see it immediately.
    if frontend_stale {
        let stale_msg = match stale_slot_id {
            Some(sid) => format!(
                "frontend_stale: slot {} embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.",
                sid
            ),
            None => "frontend_stale: the active build slot embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.".to_string(),
        };
        resp["warnings"] = json!([stale_msg]);
        resp["frontend_stale"] = json!(true);

        let mut response = (axum::http::StatusCode::OK, Json(resp)).into_response();
        response
            .headers_mut()
            .insert("X-Frontend-Stale", "1".parse().unwrap());
        return Ok(response);
    }

    Ok(Json(resp).into_response())
}

/// Inspect the build pool and return `(frontend_stale, slot_id)` describing
/// the slot whose binary a caller of `spawn-test` / `spawn-named` will pick
/// up. Prefers `last_successful_slot`; if that isn't set, reports true if
/// any slot is stale (conservative — we can't pinpoint which slot's exe the
/// resolver will choose from disk).
async fn resolve_frontend_stale_for_spawn(state: &SharedState) -> (bool, Option<usize>) {
    let last_successful = *state.build_pool.last_successful_slot.read().await;
    if let Some(sid) = last_successful {
        if let Some(slot) = state.build_pool.slots.iter().find(|s| s.id == sid) {
            let stale = *slot.frontend_stale.read().await;
            return (stale, Some(sid));
        }
    }
    // Fall back to "any slot stale" — conservative but honest.
    let any = state.build_pool.any_slot_has_stale_frontend().await;
    (any, None)
}

/// Outcome of the post-spawn health probe.
enum ProbeOutcome {
    Healthy {
        elapsed_ms: u64,
        /// Git SHA embedded in the runner binary (from build.rs). `None` if
        /// the runner didn't include it in its /health response (old binary,
        /// or unparseable body).
        git_sha: Option<String>,
    },
    Failed {
        elapsed_ms: u64,
        child_alive: bool,
        pid: Option<u32>,
        recent_logs: Vec<String>,
    },
}

/// Poll `GET http://localhost:{port}/health` every 200ms until it succeeds or
/// `timeout_ms` elapses. On failure, also reports whether the child process
/// is still alive and snapshots the last 20 log lines from the runner's
/// per-runner buffer.
async fn probe_runner_health(
    state: &SharedState,
    runner_id: &str,
    port: u16,
    timeout_ms: u64,
) -> ProbeOutcome {
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let interval = std::time::Duration::from_millis(200);
    let start = std::time::Instant::now();
    let url = format!("http://localhost:{}/health", port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .unwrap_or_default();

    loop {
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                // Extract gitSha from the /health body while we have it
                // (best-effort; an old runner won't include the field).
                let git_sha = resp
                    .text()
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                    .and_then(|v| {
                        v.get("data")
                            .and_then(|d| d.get("gitSha"))
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                    });
                return ProbeOutcome::Healthy {
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    git_sha,
                };
            }
        }

        if start.elapsed() >= timeout {
            break;
        }
        tokio::time::sleep(interval).await;
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;

    // Determine if the child process is still alive and capture its pid.
    // try_wait() returns Ok(Some(_)) if the child has exited, Ok(None) if
    // still running, Err(_) on a system error (treated as "unknown — assume
    // dead" so we report the more honest 500).
    let (child_alive, pid) = if let Some(managed) = state.get_runner(runner_id).await {
        let mut runner = managed.runner.write().await;
        let pid = runner.pid;
        let alive = match runner.process.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        (alive, pid)
    } else {
        (false, None)
    };

    let recent_logs = recent_runner_log_lines(state, runner_id, 20).await;

    ProbeOutcome::Failed {
        elapsed_ms,
        child_alive,
        pid,
        recent_logs,
    }
}

/// Snapshot the last `limit` log lines from a runner's per-runner log buffer.
/// Each line is rendered as `"<level> <message>"` to keep the JSON payload
/// compact (the existing `/runners/{id}/logs` endpoint returns full
/// `LogEntry` records — for a 502/500 diagnostic body we just want
/// human-readable tail lines).
async fn recent_runner_log_lines(
    state: &SharedState,
    runner_id: &str,
    limit: usize,
) -> Vec<String> {
    let Some(managed) = state.get_runner(runner_id).await else {
        return Vec::new();
    };
    let entries = managed.logs.history().await;
    let total = entries.len();
    let start = total.saturating_sub(limit);
    entries[start..]
        .iter()
        .map(|e| {
            let level = match e.level {
                LogLevel::Info => "info",
                LogLevel::Warn => "warn",
                LogLevel::Error => "error",
                LogLevel::Debug => "debug",
            };
            format!("{} {}", level, e.message)
        })
        .collect()
}

// --- Spawn named runner ---

/// POST /runners/spawn-named — spawn a persistent named runner on the next available port.
///
/// Like `spawn-test` but the runner is NOT ephemeral — it is persisted to settings
/// and is NOT auto-cleaned by the reaper. The ID uses a `named-` prefix instead of
/// `test-`, so `is_temp_runner()` returns false.
pub async fn spawn_named(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<SpawnNamedRequest>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Validate name
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(SupervisorError::Validation(
            "Runner name must not be empty".to_string(),
        ));
    }
    if name == "primary" {
        return Err(SupervisorError::Validation(
            "Runner name 'primary' is reserved".to_string(),
        ));
    }
    if name.starts_with("test-") {
        return Err(SupervisorError::Validation(
            "Runner name must not start with 'test-'".to_string(),
        ));
    }

    let no_wait = headers
        .get("X-Queue-Mode")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("no-wait"))
        .unwrap_or(false);

    // Atomically reserve a free port AND insert a placeholder ManagedRunner.
    // Keep an Arc to the ManagedRunner so the later start uses it directly,
    // bypassing the id-based registry lookup. See the matching note in
    // `spawn_test` for why this matters.
    let (id, port, managed) = {
        let mut runners = state.runners.write().await;
        let used_ports: std::collections::HashSet<u16> =
            runners.values().map(|r| r.config.port).collect();
        let port = match body.port {
            Some(p) => {
                if used_ports.contains(&p) {
                    return Err(SupervisorError::Validation(format!(
                        "Port {} is already in use",
                        p
                    )));
                }
                p
            }
            None => (9877..=9899)
                .find(|p| !used_ports.contains(p))
                .ok_or_else(|| {
                    SupervisorError::Validation(
                        "No available ports in range 9877-9899. Stop some runners first."
                            .to_string(),
                    )
                })?,
        };
        let id = format!("named-{}-{}", port, uuid_simple());

        let server_mode = body.server_mode.unwrap_or(false);
        let existing_configs: Vec<RunnerConfig> =
            runners.values().map(|r| r.config.clone()).collect();
        let resolved = crate::process::restate_port::resolve_ports(
            &existing_configs,
            server_mode,
            body.restate_ingress_port,
            body.restate_admin_port,
            body.restate_service_port,
            body.external_restate_admin_url.clone(),
            body.external_restate_ingress_url.clone(),
        )
        .map_err(SupervisorError::Validation)?;

        let runner_config = RunnerConfig {
            id: id.clone(),
            name: name.clone(),
            port,
            is_primary: false,
            protected: body.protected,
            server_mode,
            restate_ingress_port: resolved.ingress_port,
            restate_admin_port: resolved.admin_port,
            restate_service_port: resolved.service_port,
            external_restate_admin_url: resolved.external_admin_url,
            external_restate_ingress_url: resolved.external_ingress_url,
            extra_env: Default::default(),
        };
        let managed = Arc::new(ManagedRunner::new_with_log_dir(
            runner_config,
            false,
            state.config.log_dir.as_deref(),
        ));
        runners.insert(id.clone(), managed.clone());
        (id, port, managed)
    };

    // Rebuild if requested
    if body.rebuild {
        if no_wait && state.build_pool.permits.available_permits() == 0 {
            // Clean up placeholder before returning error
            let mut runners = state.runners.write().await;
            runners.remove(&id);

            let snap = state.build_pool.snapshot().await;
            let now = chrono::Utc::now();
            let mut min_elapsed_secs: Option<f64> = None;
            let active: Vec<serde_json::Value> = snap
                .iter()
                .filter_map(|(slot_id, _td, info)| {
                    info.as_ref().map(|i| {
                        let elapsed = (now - i.started_at).num_seconds().max(0) as f64;
                        min_elapsed_secs = Some(match min_elapsed_secs {
                            Some(cur) => cur.min(elapsed),
                            None => elapsed,
                        });
                        json!({
                            "slot": slot_id,
                            "started_at": i.started_at.to_rfc3339(),
                            "elapsed_secs": elapsed as i64,
                            "requester_id": i.requester_id,
                            "rebuild_kind": i.rebuild_kind,
                        })
                    })
                })
                .collect();
            let mut sum: f64 = 0.0;
            let mut count: usize = 0;
            for slot in &state.build_pool.slots {
                let h = slot.history.read().await;
                for d in &h.recent_durations_secs {
                    sum += *d;
                    count += 1;
                }
            }
            let avg = if count > 0 {
                Some(sum / count as f64)
            } else {
                None
            };
            let estimated_wait_secs = avg.map(|a| (a - min_elapsed_secs.unwrap_or(0.0)).max(0.0));
            let queued = state
                .build_pool
                .queue_depth
                .load(std::sync::atomic::Ordering::Relaxed);
            return Err(SupervisorError::BuildPoolFull {
                queue_position: queued + 1,
                active_builds: active,
                estimated_wait_secs,
            });
        }

        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Rebuilding runner before spawning named runner '{}' on port {} (requester={:?})",
                    name, port, body.requester_id
                ),
            )
            .await;

        let build_result =
            crate::build_monitor::run_cargo_build_with_requester(&state, body.requester_id.clone())
                .await;
        if let Err(e) = build_result {
            let mut runners = state.runners.write().await;
            runners.remove(&id);
            return Err(e);
        }
    }

    // Check that a runner binary exists
    if manager::resolve_source_exe(&state).await.is_err() {
        let mut runners = state.runners.write().await;
        runners.remove(&id);
        return Err(SupervisorError::Process(
            "Runner binary not found in any build slot. Use rebuild: true to build it first."
                .to_string(),
        ));
    }

    // Start the runner using the Arc captured at insertion time (race-free
    // — no re-lookup of `id` which could fail transiently under concurrent
    // load).
    if let Err(e) = manager::start_managed_runner(&state, &managed).await {
        let mut runners = state.runners.write().await;
        runners.remove(&id);
        return Err(e);
    }

    // Persist to settings (named runners survive restarts)
    let settings_path = settings::settings_path(&state.config);
    settings::add_runner(&settings_path, &managed.config);

    // If protected, persist the protection flag via settings
    if body.protected {
        let mut settings = settings::load_settings(&settings_path);
        if let Some(cfg) = settings.runners.iter_mut().find(|r| r.id == id) {
            cfg.protected = true;
            let _ = settings::try_save_settings(&settings_path, &settings);
        }
        // Update runtime protection flag
        if let Some(managed) = state.get_runner(&id).await {
            let mut protected = managed.protected.write().await;
            *protected = true;
        }
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Spawned named runner '{}' on port {} (id: {})",
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
                    "Waiting up to {}s for named runner '{}' to become healthy...",
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
                            "Named runner '{}' did not become healthy within {}s",
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
                            format!("Named runner '{}' is healthy (took {}ms)", name, wait_ms),
                        )
                        .await;
                    break;
                }
                _ => continue,
            }
        }
    }

    // Resolve the source exe to report its metadata
    let exe_meta = crate::process::manager::resolve_source_exe(&state)
        .await
        .ok()
        .and_then(|p| crate::process::manager::binary_meta(&p));

    let (frontend_stale, stale_slot_id) = resolve_frontend_stale_for_spawn(&state).await;

    let mut resp = json!({
        "id": id,
        "name": name,
        "port": port,
        "status": if body.wait { if healthy { "healthy" } else { "timeout" } } else { "started" },
        "wait_ms": wait_ms,
        "api_url": format!("http://localhost:{}", port),
        "ui_bridge_url": format!("http://localhost:{}/ui-bridge", port),
        "logs_url": format!("/runners/{}/logs", id),
        "logs_stream_url": format!("/runners/{}/logs/stream", id),
        "message": if body.wait && healthy {
            format!("Named runner '{}' ready on port {} ({}ms)", name, port, wait_ms)
        } else if body.wait {
            format!("Named runner '{}' spawned on port {} but did not become healthy within {}s", name, port, body.wait_timeout_secs)
        } else {
            format!("Named runner '{}' spawned on port {}", name, port)
        }
    });
    if let Some(meta) = exe_meta {
        resp["binary_mtime"] = json!(meta.binary_mtime);
        resp["binary_size_bytes"] = json!(meta.binary_size_bytes);
    }

    if frontend_stale {
        let stale_msg = match stale_slot_id {
            Some(sid) => format!(
                "frontend_stale: slot {} embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.",
                sid
            ),
            None => "frontend_stale: the active build slot embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.".to_string(),
        };
        resp["warnings"] = json!([stale_msg]);
        resp["frontend_stale"] = json!(true);

        let mut response = (axum::http::StatusCode::OK, Json(resp)).into_response();
        response
            .headers_mut()
            .insert("X-Frontend-Stale", "1".parse().unwrap());
        return Ok(response);
    }

    Ok(Json(resp).into_response())
}

// --- Build pool visibility ---

/// GET /builds — snapshot of the parallel build pool.
///
/// Returns the pool size, the state of each slot, and the number of callers
/// currently waiting in the queue. Agents use this to decide whether a
/// rebuild request will be quick or will have to wait.
pub async fn list_builds(State(state): State<SharedState>) -> impl IntoResponse {
    let now = chrono::Utc::now();
    let mut slots_json: Vec<serde_json::Value> = Vec::with_capacity(state.build_pool.slots.len());
    let mut global_sum: f64 = 0.0;
    let mut global_count: usize = 0;

    let mut any_slot_has_stale_frontend = false;
    for slot in &state.build_pool.slots {
        let info_opt = match slot.busy.try_read() {
            Ok(g) => g.clone(),
            Err(_) => slot.busy.read().await.clone(),
        };
        let history_snapshot = match slot.history.try_read() {
            Ok(g) => g.clone(),
            Err(_) => slot.history.read().await.clone(),
        };
        let frontend_stale = match slot.frontend_stale.try_read() {
            Ok(g) => *g,
            Err(_) => *slot.frontend_stale.read().await,
        };
        if frontend_stale {
            any_slot_has_stale_frontend = true;
        }
        for d in &history_snapshot.recent_durations_secs {
            global_sum += *d;
            global_count += 1;
        }

        let history_json = json!({
            "recent_samples": history_snapshot.recent_durations_secs.len(),
            "total_builds": history_snapshot.total_builds,
            "successful_builds": history_snapshot.successful_builds,
            "avg_duration_secs": history_snapshot.avg_duration_secs(),
            "p50_duration_secs": history_snapshot.p50_duration_secs(),
            "last_completed_at": history_snapshot.last_completed_at.map(|t| t.to_rfc3339()),
            "last_error": history_snapshot.last_error,
        });

        let slot_json = match info_opt {
            Some(i) => {
                let elapsed = (now - i.started_at).num_seconds().max(0);
                json!({
                    "id": slot.id,
                    "target_dir": slot.target_dir.to_string_lossy(),
                    "state": "building",
                    "started_at": i.started_at.to_rfc3339(),
                    "elapsed_secs": elapsed,
                    "requester_id": i.requester_id,
                    "rebuild_kind": i.rebuild_kind,
                    "frontend_stale": frontend_stale,
                    "history": history_json,
                })
            }
            None => json!({
                "id": slot.id,
                "target_dir": slot.target_dir.to_string_lossy(),
                "state": "idle",
                "frontend_stale": frontend_stale,
                "history": history_json,
            }),
        };
        slots_json.push(slot_json);
    }

    let avg_build_duration_secs: Option<f64> = if global_count > 0 {
        Some(global_sum / global_count as f64)
    } else {
        None
    };

    let queued = state
        .build_pool
        .queue_depth
        .load(std::sync::atomic::Ordering::Relaxed);
    let last_successful = *state.build_pool.last_successful_slot.read().await;
    let available = state.build_pool.permits.available_permits();
    Json(json!({
        "pool_size": state.build_pool.slots.len(),
        "available_permits": available,
        "queued": queued,
        "last_successful_slot": last_successful,
        "avg_build_duration_secs": avg_build_duration_secs,
        "any_slot_has_stale_frontend": any_slot_has_stale_frontend,
        "slots": slots_json,
    }))
}

// --- Per-runner log endpoints ---

#[derive(Deserialize)]
pub struct RunnerLogQuery {
    #[serde(default = "default_runner_log_limit")]
    pub limit: usize,
    /// Optional level filter: "info", "warn", "error", "debug".
    pub level: Option<String>,
    /// When true, if the live runner lookup misses, fall back to the
    /// stopped-runners post-mortem cache. Accepts both `include_stopped` and
    /// the camelCase `includeStopped` spelling for convenience.
    #[serde(default, alias = "includeStopped")]
    pub include_stopped: bool,
}

fn default_runner_log_limit() -> usize {
    100
}

/// GET /runners/{id}/logs — return recent log entries for a specific runner.
///
/// With `?include_stopped=true`, falls back to the post-mortem snapshot cache
/// if the runner has been removed from the active registry. The post-mortem
/// response includes extra `stopped_at` / `exit_reason` fields so callers can
/// distinguish live logs from cached ones.
pub async fn runner_log_history(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(query): Query<RunnerLogQuery>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    let limit = query.limit.min(500);

    // Live path: runner is still in the registry.
    if let Some(managed) = state.get_runner(&id).await {
        let entries = managed.logs.history().await;
        let entries: Vec<_> = entries
            .into_iter()
            .rev()
            .filter(|e| level_matches(&query.level, e))
            .take(limit)
            .collect();
        let count = entries.len();
        return Ok(Json(json!({
            "runner_id": id,
            "entries": entries,
            "count": count,
        })));
    }

    // Stopped path: only consulted when explicitly opted in.
    if query.include_stopped {
        let cache = state.stopped_runners.read().await;
        if let Some(snapshot) = cache.get(&id) {
            let entries: Vec<_> = snapshot
                .last_log_lines
                .iter()
                .rev()
                .filter(|e| level_matches(&query.level, e))
                .take(limit)
                .cloned()
                .collect();
            let count = entries.len();
            return Ok(Json(json!({
                "runner_id": id,
                "entries": entries,
                "count": count,
                "stopped_at": snapshot.stopped_at.to_rfc3339(),
                "exit_reason": snapshot.exit_reason,
                "exit_code": snapshot.exit_code,
            })));
        }
    }

    Err(SupervisorError::RunnerNotFound(id))
}

fn level_matches(filter: &Option<String>, entry: &crate::log_capture::LogEntry) -> bool {
    match filter {
        Some(level) => {
            let entry_level = serde_json::to_string(&entry.level).unwrap_or_default();
            let entry_level = entry_level.trim_matches('"');
            entry_level == level
        }
        None => true,
    }
}

/// GET /runners/{id}/crash-dump — return the panic stack trace from the
/// stopped-runner post-mortem cache. Returns 404 if the runner was never
/// cached or no panic was detected in its stderr.
pub async fn runner_crash_dump(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    let cache = state.stopped_runners.read().await;
    if let Some(snapshot) = cache.get(&id) {
        if let Some(ref panic_stack) = snapshot.panic_stack {
            return Ok(Json(json!({
                "runner_id": id,
                "panic_stack": panic_stack,
                "stopped_at": snapshot.stopped_at.to_rfc3339(),
                "exit_reason": snapshot.exit_reason,
                "exit_code": snapshot.exit_code,
            })));
        }
    }
    Err(SupervisorError::RunnerNotFound(id))
}

/// GET /runners/{id}/logs/stream — SSE stream of real-time log events for a specific runner.
pub async fn runner_log_stream(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, SupervisorError> {
    let managed = state
        .get_runner(&id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(id.clone()))?;

    let rx = managed.logs.subscribe();
    let stream = BroadcastStream::new(rx);

    let event_stream = stream.filter_map(|result| match result {
        Ok(entry) => {
            let data = serde_json::to_string(&entry).unwrap_or_default();
            Some(Ok(Event::default().event("log").data(data)))
        }
        Err(_) => None,
    });

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

/// DELETE /builds/caches — clean all build slot caches.
///
/// Runs `cargo clean` against each slot's CARGO_TARGET_DIR. Requires no
/// builds to be in progress (otherwise the clean would corrupt a running build).
pub async fn clear_build_caches(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Refuse if any slot is busy
    if state.build_pool.permits.available_permits() < state.build_pool.slots.len() {
        return Err(SupervisorError::BuildInProgress);
    }

    let mut results = Vec::new();
    for slot in &state.build_pool.slots {
        let target_dir = &slot.target_dir;
        let mut cmd = tokio::process::Command::new("cargo");
        cmd.args(["clean", "--target-dir"])
            .arg(target_dir)
            .current_dir(&state.config.project_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = cmd.output().await;

        let success = output.as_ref().map(|o| o.status.success()).unwrap_or(false);
        if !success {
            let stderr = output
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
                .unwrap_or_default();
            tracing::warn!("cargo clean for slot {} failed: {}", slot.id, stderr);
        }
        results.push(json!({
            "slot": slot.id,
            "target_dir": target_dir.to_string_lossy(),
            "cleaned": success,
        }));

        if success {
            state
                .logs
                .emit(
                    LogSource::Build,
                    LogLevel::Info,
                    format!(
                        "Cleaned build cache for slot {} ({:?})",
                        slot.id, target_dir
                    ),
                )
                .await;
        }
    }

    // Also clear last_successful_slot since the binaries are gone
    *state.build_pool.last_successful_slot.write().await = None;

    Ok(Json(json!({
        "status": "ok",
        "slots": results,
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

// ---------------------------------------------------------------------------
// Test auto-login credentials API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetTestLoginRequest {
    pub email: String,
    pub password: String,
}

/// POST /test-login — Set auto-login credentials for future temp runner spawns.
pub async fn set_test_login(
    State(state): State<SharedState>,
    Json(body): Json<SetTestLoginRequest>,
) -> impl IntoResponse {
    if body.email.is_empty() || body.password.is_empty() {
        return Json(json!({"success": false, "error": "email and password required"}));
    }
    *state.test_auto_login.write().await = Some((body.email, body.password));
    info!("Test auto-login credentials configured for future temp runners");
    Json(json!({"success": true, "message": "Auto-login credentials set for temp runners"}))
}

/// GET /test-login — Check if auto-login credentials are configured.
pub async fn get_test_login(State(state): State<SharedState>) -> impl IntoResponse {
    let configured = state.test_auto_login.read().await.is_some();
    Json(json!({"configured": configured}))
}

/// DELETE /test-login — Clear auto-login credentials.
pub async fn clear_test_login(State(state): State<SharedState>) -> impl IntoResponse {
    *state.test_auto_login.write().await = None;
    info!("Test auto-login credentials cleared");
    Json(json!({"success": true, "message": "Auto-login credentials cleared"}))
}
