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
use crate::state::{ManagedRunner, SharedState, SseConnectionGuard};
use qontinui_types::wire::runner_kind::RunnerKind;
use std::sync::Arc;
use tracing::{info, warn};

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

/// Body for `POST /runners/{id}/rebuild-and-restart` (Item E).
///
/// The rebuild-and-restart cycle is one round-trip for callers that today
/// have to combine `POST /runners/{id}/stop` + a build + `POST .../start`
/// manually. Refuses to operate on the primary or any user-managed runner
/// (the supervisor never starts/stops those unprompted).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RebuildAndRestartRequest {
    /// Optional source label, recorded in diagnostic events.
    #[serde(default)]
    pub source: String,
    /// Reserved for future "force unprotect" semantics. Currently a no-op
    /// since the rebuild-and-restart cycle goes through `stop_runner_by_id`
    /// which already respects protection.
    #[serde(default)]
    #[allow(dead_code)]
    pub force: bool,
    /// If true, after starting the runner block until its `/health` returns
    /// 200 OK or `wait_timeout_secs` elapses.
    #[serde(default)]
    pub wait: bool,
    /// Maximum seconds to wait when `wait` is true. Defaults to 120s.
    #[serde(default)]
    pub wait_timeout_secs: Option<u64>,
}

fn default_source_manual() -> String {
    "manual".to_string()
}

/// Build the `build_result` JSON object surfaced on spawn-test / spawn-named
/// success responses (item A of the supervisor cleanup plan).
///
/// `attempted` is the request's `rebuild` bit. `succeeded` is `None` when
/// `!attempted` (no build was run, so neither true nor false applies);
/// otherwise it is `Some(true)` for a successful build or `Some(false)` for
/// a failure that the caller chose to swallow via `allow_stale_fallback`.
/// `reused_stale` is true only when a build failure was swallowed AND we
/// resolved an exe to fall back to. `error` carries the cargo error string
/// on failure. `slot_id` is `state.build_pool.last_successful_slot` read
/// after the build attempt — for stale-fallback runs this points at the
/// slot whose exe we are reusing. `meta` describes the actual binary that
/// will run (mtime / size / age in seconds).
///
/// `state` is a discriminator added 2026-05-10 so JSON-driven clients can
/// switch on a single field instead of decoding the (`attempted`,
/// `succeeded`, `reused_stale`) cross-product:
///
/// | state          | attempted | succeeded   | reused_stale |
/// |----------------|-----------|-------------|--------------|
/// | `reused`       | false     | None        | false        |
/// | `built`        | true      | Some(true)  | false        |
/// | `failed`       | true      | Some(false) | false        |
/// | `reused_stale` | true      | Some(false) | true         |
///
/// The legacy fields stay populated unchanged for backward compatibility.
pub fn build_result_json(
    attempted: bool,
    succeeded: Option<bool>,
    reused_stale: bool,
    error: Option<&str>,
    slot_id: Option<usize>,
    meta: Option<&crate::process::manager::BinaryMeta>,
) -> serde_json::Value {
    let state = build_result_state(attempted, succeeded, reused_stale);
    let mut obj = json!({
        "state": state,
        "attempted": attempted,
        "succeeded": succeeded,
        "reused_stale": reused_stale,
        "error": error,
        "slot_id": slot_id,
    });
    if let Some(m) = meta {
        obj["binary_mtime"] = json!(m.binary_mtime);
        obj["binary_size_bytes"] = json!(m.binary_size_bytes);
        obj["binary_age_secs"] = json!(m.binary_age_secs);
    } else {
        obj["binary_mtime"] = serde_json::Value::Null;
        obj["binary_size_bytes"] = serde_json::Value::Null;
        obj["binary_age_secs"] = serde_json::Value::Null;
    }
    obj
}

/// Map the (attempted, succeeded, reused_stale) cross-product to a stable
/// discriminator string. Used inside `build_result_json` and by tests.
fn build_result_state(
    attempted: bool,
    succeeded: Option<bool>,
    reused_stale: bool,
) -> &'static str {
    match (attempted, succeeded, reused_stale) {
        (false, _, _) => "reused",
        (true, Some(true), _) => "built",
        (true, Some(false), true) => "reused_stale",
        (true, Some(false), false) => "failed",
        // `attempted=true, succeeded=None` shouldn't happen — every build
        // path either resolves to true/false or returns an error before
        // calling build_result_json. Surface it as "failed" rather than
        // panicking on what's effectively a programmer error elsewhere.
        (true, None, _) => "failed",
    }
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

        // Phase 2b: startup-panic telemetry. `recent_panic` is the structured
        // parse of the runner's `runner-panic.log`, populated by
        // `monitor_runner_process_exit` when the runner exits non-zero and a
        // fresh panic log is on disk. Distinct from `recent_crash` (which is
        // the on-runtime WebView2 crash dump surfaced by the runner's own
        // /health endpoint). The dashboard renders this as a red "Panic"
        // badge on the runner row.
        let recent_panic = managed.recent_panic.read().await.clone();

        // Phase 2c (Item 9): stale-binary detection. `None` is the normal case
        // (running binary is newer than or equal to the newest slot within a
        // 30s jitter threshold). `Some` means a fresher build is sitting in a
        // pool slot — the dashboard surfaces this as a yellow "stale binary"
        // badge prompting the user to restart. File-stat only, swallows I/O
        // errors — this is strictly informational and must never block the
        // listing.
        let stale_binary = manager::stale_binary_for_runner(&state, &managed.config).await;

        result.push(json!({
            "id": managed.config.id,
            "name": managed.config.name,
            "port": managed.config.port,
            "kind": managed.config.kind(),
            "protected": is_protected,
            "running": effectively_running,
            "pid": runner.pid,
            "started_at": runner.started_at.map(|t| t.to_rfc3339()),
            "api_responding": cached.runner_responding,
            "logs_url": logs_url,
            "logs_stream_url": logs_stream_url,
            "ui_error": ui_error,
            "recent_crash": recent_crash,
            "recent_panic": recent_panic,
            "stale_binary": stale_binary,
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
            kind: RunnerKind::External,
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

    if managed.config.kind().is_primary() {
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
        let exe_copy = state.config.runner_exe_copy_path(&managed.config);
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
    // Explicit user request — purge regardless of build-pool state.
    let purged_triples = purge_stale_test_runners_core(&state, false).await;
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
///
/// `respect_active_builds`:
/// - `false` — caller explicitly asked to purge regardless of state (route
///   handler `POST /runners/purge-stale` is the canonical example: the user
///   knows what they're doing).
/// - `true` — caller is a periodic sweep; skip pre-running placeholders
///   (`running=false`) when ANY build slot is busy, because that placeholder
///   is overwhelmingly likely to be the one the active build is feeding.
///   Cold cargo builds can run 10-15 min on a fresh checkout, far longer
///   than the sweep cadence; reaping the placeholder mid-build leaves the
///   build orphaned and the spawn-test request without a runner.
pub async fn purge_stale_test_runners_core(
    state: &SharedState,
    respect_active_builds: bool,
) -> Vec<(String, String, u16)> {
    let any_build_active = if respect_active_builds {
        state
            .build_pool
            .slots
            .iter()
            .any(|s| s.busy.try_read().map(|g| g.is_some()).unwrap_or(true))
    } else {
        false
    };

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
        // Active-build grace: a `running=false` placeholder during an active
        // build is the spawn-test request that triggered that build. Skip
        // reaping; the next sweep will pick it up if the build never produces
        // a healthy runner. Only zombies (`running=true` with a dead port)
        // continue to fall through.
        if respect_active_builds && !is_running && any_build_active {
            continue;
        }
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
        #[cfg(target_os = "windows")]
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
        let exe_copy = state.config.runner_exe_copy_path(&managed.config);
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
///
/// **Port-bind verification.** After the OS process spawns successfully,
/// this endpoint polls the runner's `/health` for up to 30s (500ms cadence)
/// before returning 200. If the runner stays alive but never binds the API
/// port within the budget, the response is **503** with body
/// `{error: "runner_unhealthy_after_start", elapsed_ms, recent_logs}`.
///
/// Pass `?wait=false` to opt out and get the legacy fire-and-forget 200
/// response the moment the process spawns (for callers that do their own
/// readiness probing).
pub async fn start_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(wait_q): Query<crate::routes::runner::StartWaitQuery>,
) -> Result<axum::response::Response, SupervisorError> {
    manager::start_runner_by_id(&state, &id).await?;

    if wait_q.wait {
        if let Err(failure) =
            crate::process::health_probe::wait_for_runner_healthy_default(&state, &id).await
        {
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Warn,
                    format!(
                        "Runner '{}' did not become healthy {}ms after start",
                        id, failure.elapsed_ms
                    ),
                )
                .await;
            return Ok(crate::routes::runner::unhealthy_after_start_response(
                &id, failure,
            ));
        }
    }

    Ok(Json(json!({
        "status": "started",
        "message": format!("Runner '{}' started", id)
    }))
    .into_response())
}

/// POST /runners/{id}/stop — stop a specific runner.
pub async fn stop_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: Option<Json<StopRunnerRequest>>,
) -> Result<impl IntoResponse, SupervisorError> {
    let _force = body.map(|b| b.force).unwrap_or(false);

    // Capture the early-log path BEFORE stopping — for test-* runners the
    // stop also removes them from the registry, after which we can no longer
    // look up the path. We delete the file AFTER stop succeeds because the
    // stop is the explicit "user is done diagnosing" signal.
    let early_log_path = if let Some(managed) = state.get_runner(&id).await {
        managed.early_log_path.read().await.clone()
    } else {
        None
    };

    manager::stop_runner_by_id(&state, &id).await?;

    // Explicit user-facing /stop is the only path that deletes the
    // early-log file. The spawn-test failed-probe cleanup path also calls
    // stop_runner_by_id, but it does NOT route through here — it goes
    // directly through the manager — so the early-log file is preserved
    // there (which is what we want for post-mortem debugging). Files for
    // runners that died on their own and were reaped also persist. Best
    // effort: missing-file errors are silently swallowed by delete_early_log.
    if let Some(path) = early_log_path {
        crate::process::early_log::delete_early_log(&path);
    }

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

/// POST /runners/{id}/rebuild-and-restart — stop → cargo build → start, in
/// one round-trip.
///
/// Refuses to act on the primary runner. On build failure, returns the
/// cargo error directly (no automatic stale-fallback — callers who want
/// that should pair `spawn-test {allow_stale_fallback: true}` with their
/// own restart logic).
pub async fn rebuild_and_restart_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RebuildAndRestartRequest>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    let outcome = manager::rebuild_and_restart_by_id(&state, &id, body).await?;
    Ok(Json(outcome))
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
    /// Pin this runner to the last-known-good (LKG) binary instead of the
    /// freshest slot exe. Use this when your own build failed but you want
    /// to test a runner whose changes you know are *already in the LKG*.
    ///
    /// **Caller's responsibility:** before setting this to `true`, compare
    /// `mtime(your changed files)` against `lkg.built_at` from `GET /health`.
    /// If any changed file's mtime is later than `lkg.built_at`, the LKG
    /// binary does NOT contain your changes — pinning to it would silently
    /// run stale code. The supervisor does not enforce this comparison.
    ///
    /// Mutually meaningful with `rebuild`:
    /// - `{rebuild: false, use_lkg: true}`: skip the build, use LKG.
    /// - `{rebuild: true, use_lkg: true}`: build first; if the build
    ///   succeeds, the new exe becomes the LKG and is what you'll run.
    ///   If the build fails, the request fails as usual — `use_lkg` is not
    ///   an automatic build-failure fallback.
    /// - `{rebuild: false, use_lkg: false}` (default): use the freshest
    ///   slot exe (`resolve_source_exe` ordering).
    #[serde(default)]
    pub use_lkg: bool,
    /// When true, return HTTP 503 if the spawned runner would embed a stale
    /// frontend (`frontend_stale: true` for any reason — build failure or
    /// `src/` newer than `dist/`). Default false: stale-frontend spawns
    /// proceed but the response carries `frontend_stale: true` + a
    /// `frontend_stale_reason` so callers can branch.
    #[serde(default)]
    pub frontend_strict: bool,
    /// When true and a `rebuild: true` cargo build fails, fall through to
    /// the existing exe-resolution chain and spawn the runner using the
    /// previous slot exe. The `build_result` field of the response carries
    /// `succeeded: false`, `reused_stale: true`, and the cargo error text so
    /// callers can choose to ignore stale-binary risk for fast diagnostics.
    ///
    /// When false (default), a build failure short-circuits with HTTP 500
    /// and no spawn is attempted. See item A of the supervisor cleanup plan
    /// for the rationale.
    #[serde(default)]
    pub allow_stale_fallback: bool,
}

fn default_health_probe_timeout() -> u64 {
    // 6s was too aggressive: the runner's PG bootstrap (apply_canonical_schema
    // → ensure_tables → run_migrations) is wrapped in 30s per-stage timeouts
    // on the runner side. If the supervisor probe gives up at 6s, it kills
    // a runner that's still legitimately initializing — and we never see the
    // runner's own diagnostic timeout fire. 60s is enough to let a slow GIN
    // index build complete on a busy DB AND to surface the runner's own
    // pg_stat_activity dump if a stage genuinely hangs.
    60_000
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
    /// Mirror of `SpawnTestRequest::allow_stale_fallback`. Defaults to false:
    /// build failures short-circuit with HTTP 500 and no spawn happens.
    #[serde(default)]
    pub allow_stale_fallback: bool,
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
            kind: RunnerKind::Temp { id: id.clone() },
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

    // Build-result tracking for the response. Populated by the rebuild
    // branch below; surfaced via the `build_result` JSON field. When
    // `body.rebuild` is false, `attempted` stays false and `succeeded`
    // stays None.
    let mut build_attempted = false;
    let mut build_succeeded: Option<bool> = None;
    let mut build_error: Option<String> = None;
    let mut build_reused_stale = false;

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
        // we reserved above so the port doesn't leak — UNLESS the caller
        // opted into `allow_stale_fallback`, in which case we keep the
        // placeholder and fall through to spawn from the previous slot exe.
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
        match build_result {
            Ok(()) => {
                build_attempted = true;
                build_succeeded = Some(true);
            }
            Err(e) => {
                if body.allow_stale_fallback {
                    let err_str = e.to_string();
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Warn,
                            format!("spawn-test stale fallback engaged: {}", err_str),
                        )
                        .await;
                    build_attempted = true;
                    build_succeeded = Some(false);
                    build_error = Some(err_str);
                    // Fall through to exe-resolution + spawn below. If the
                    // resolution fails, we'll surface 500 there.
                } else {
                    // Release the placeholder port reservation.
                    let mut runners = state.runners.write().await;
                    runners.remove(&id);
                    return Err(e);
                }
            }
        }
    }

    // If the caller asked to pin this runner to the LKG binary, resolve the
    // path now (before starting) and stash it on the ManagedRunner. The
    // override takes precedence over the slot-resolution chain inside
    // `start_exe_mode_for_runner`. The check intentionally happens AFTER any
    // optional rebuild so a `{rebuild: true, use_lkg: true}` call uses the
    // freshly-rebuilt binary as its LKG (the rebuild updated LKG on success).
    if body.use_lkg {
        match manager::resolve_lkg_exe(&state).await {
            Ok(lkg_path) => {
                let mut slot = managed.source_exe_override.write().await;
                *slot = Some(lkg_path.clone());
                let lkg_info = state.build_pool.last_known_good.read().await.clone();
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        format!(
                            "Pinning test runner '{}' to LKG binary {:?} (built_at={:?}, source_slot={:?})",
                            id,
                            lkg_path,
                            lkg_info.as_ref().map(|i| i.built_at.to_rfc3339()),
                            lkg_info.as_ref().map(|i| i.source_slot),
                        ),
                    )
                    .await;
            }
            Err(e) => {
                let mut runners = state.runners.write().await;
                runners.remove(&id);
                return Err(e);
            }
        }
    }

    // Check that a runner binary exists in some build slot (or at the legacy path).
    // Without this check, a fresh supervisor would succeed the request only to
    // fail inside `start_runner_by_id` with a less helpful error. If the check
    // fails, remove the placeholder we reserved above so the port frees up.
    // Skip when use_lkg pinned an override above — we already verified that path.
    if !body.use_lkg && manager::resolve_source_exe(&state).await.is_err() {
        let mut runners = state.runners.write().await;
        runners.remove(&id);
        return Err(SupervisorError::Process(
            "Runner binary not found in any build slot. Use rebuild: true to build it first, or use_lkg: true if a previous build's LKG copy is acceptable."
                .to_string(),
        ));
    }

    // If we got here after a failed build with `allow_stale_fallback: true`,
    // the previous slot exe survived the resolve_source_exe check above —
    // mark the build_result as reusing-stale so callers can detect it.
    if matches!(build_succeeded, Some(false)) {
        build_reused_stale = true;
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
                recent_panic,
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

                // Capture the per-spawn early-log path BEFORE cleanup so we
                // can surface it in the error response. The file itself is
                // explicitly preserved across this cleanup path (it's the
                // whole point of the early-death capture) — only the
                // explicit user-facing /stop endpoint deletes it.
                let early_log_path = if let Some(managed) = state.get_runner(&id).await {
                    managed
                        .early_log_path
                        .read()
                        .await
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                } else {
                    None
                };

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
                    // Phase 2b: structured startup panic from
                    // `runner-panic.log`, when the runner died during
                    // startup via a Rust panic (e.g. axum router build,
                    // DB connect, Tauri builder). `null` if the child is
                    // hung-but-alive or exited for a non-panic reason.
                    "recent_panic": recent_panic,
                    // Per-spawn captured stdout/stderr file. Survives
                    // cleanup of the dead runner — read this path on disk
                    // for the full child output when `recent_logs` is
                    // truncated. `null` if the supervisor failed to open
                    // the file (rare; out-of-disk, perms).
                    "early_log_path": early_log_path,
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
    // stale binaries ("this binary is older than my last commit"). When the
    // runner was pinned to LKG, prefer the LKG path's metadata over the
    // freshest slot's so the response describes what's actually running.
    let exe_meta = if body.use_lkg {
        crate::process::manager::resolve_lkg_exe(&state)
            .await
            .ok()
            .and_then(|p| crate::process::manager::binary_meta(&p))
    } else {
        crate::process::manager::resolve_source_exe(&state)
            .await
            .ok()
            .and_then(|p| crate::process::manager::binary_meta(&p))
    };

    // Determine if the slot we used for the binary has a stale frontend
    // baked in (build failure) OR if `src/` is newer than `dist/index.html`
    // (someone forgot to `npm run build`). Either way, the runner ships with
    // a UI that doesn't reflect current source.
    let (frontend_stale, stale_reason, stale_slot_id) =
        resolve_frontend_stale_for_spawn(&state).await;

    // frontend_strict: short-circuit with 503 before reporting success when
    // the caller has explicitly opted into strict-frontend enforcement.
    if frontend_stale && body.frontend_strict {
        // Stop the runner we just spawned — strict mode means "don't ship a
        // stale UI to the caller."
        let _ = manager::stop_runner_by_id(&state, &id).await;
        {
            let mut runners = state.runners.write().await;
            runners.remove(&id);
        }
        state.notify_health_change();
        let reason_str = stale_reason.map(|r| r.as_str()).unwrap_or("unknown");
        let body = json!({
            "error": "frontend_stale",
            "message": format!(
                "frontend_strict: refusing to spawn — frontend dist is stale (reason={}). \
                 Run `cd qontinui-runner && npm run build` (or fix the build error) and retry, \
                 or pass {{\"frontend_strict\": false}} to override.",
                reason_str
            ),
            "frontend_stale": true,
            "frontend_stale_reason": reason_str,
            "stale_slot_id": stale_slot_id,
        });
        return Ok((axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
    }

    // Item A: assemble the build_result JSON object. `slot_id` reads
    // last_successful_slot AFTER the build (or after the no-op when
    // !attempted) so stale-fallback runs report the slot whose exe is
    // being reused.
    let post_build_slot_id = *state.build_pool.last_successful_slot.read().await;
    let build_result = build_result_json(
        build_attempted,
        build_succeeded,
        build_reused_stale,
        build_error.as_deref(),
        post_build_slot_id,
        exe_meta.as_ref(),
    );

    // Item B: assemble the auth_state JSON object.
    let auto_login_configured = state.test_auto_login.read().await.is_some();
    let auth_state_json = {
        let last_auth = managed.last_auth_result.read().await.clone();
        match last_auth {
            Some(r) => json!({
                "auto_login_configured": auto_login_configured,
                "auto_login_attempted": r.attempted,
                "auto_login_succeeded": r.succeeded,
                "last_login_attempt_at": r.attempt_at.to_rfc3339(),
                "rate_limit_hint": r.rate_limit_hint,
            }),
            None => json!({
                "auto_login_configured": auto_login_configured,
                "auto_login_attempted": serde_json::Value::Null,
                "auto_login_succeeded": serde_json::Value::Null,
                "last_login_attempt_at": serde_json::Value::Null,
                "rate_limit_hint": serde_json::Value::Null,
            }),
        }
    };

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
        "build_result": build_result,
        "auth_state": auth_state_json,
        "message": if body.wait && healthy {
            format!("Test runner ready on port {} ({}ms)", port, wait_ms)
        } else if body.wait {
            format!("Test runner spawned on port {} but did not become healthy within {}s", port, body.wait_timeout_secs)
        } else {
            format!("Test runner spawned on port {}", port)
        }
    });
    if let Some(ms) = health_probe_ms {
        resp["health_probe_ms"] = json!(ms);
    }
    if let Some(sha) = probed_git_sha {
        resp["git_sha"] = json!(sha);
    }
    if body.use_lkg {
        resp["used_lkg"] = json!(true);
        if let Some(info) = state.build_pool.last_known_good.read().await.clone() {
            resp["lkg"] = json!({
                "built_at": info.built_at.to_rfc3339(),
                "source_slot": info.source_slot,
                "exe_size": info.exe_size,
            });
        }
    }

    // Always emit `frontend_stale` so callers can branch on a predictable
    // field shape rather than needing missing-field handling. When stale,
    // also emit `frontend_stale_reason` for diagnostic surfaces and keep
    // the existing `warnings` entry + `X-Frontend-Stale` header for
    // backward compat with anyone parsing those today.
    resp["frontend_stale"] = json!(frontend_stale);
    if frontend_stale {
        let reason_str = stale_reason.map(|r| r.as_str()).unwrap_or("unknown");
        resp["frontend_stale_reason"] = json!(reason_str);
        let stale_msg = match stale_reason {
            Some(FrontendStaleReason::BuildFailed) => match stale_slot_id {
                Some(sid) => format!(
                    "frontend_stale: slot {} embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.",
                    sid
                ),
                None => "frontend_stale: the active build slot embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.".to_string(),
            },
            Some(FrontendStaleReason::SrcDrift) => "frontend_stale: src/**/*.{ts,tsx,css,json,html} is newer than dist/index.html — the runner embeds a UI that doesn't reflect current source. Run `cd qontinui-runner && npm run build` to refresh.".to_string(),
            Some(FrontendStaleReason::DistMissing) => "frontend_stale: dist/index.html is missing — likely a concurrent external `npm run build` wiped dist/, or an npm-exit-0 empty-output regression. Run `cd qontinui-runner && npm run build` to rebuild.".to_string(),
            None => "frontend_stale: the runner may embed a stale frontend dist (reason unavailable).".to_string(),
        };
        resp["warnings"] = json!([stale_msg]);

        let mut response = (axum::http::StatusCode::OK, Json(resp)).into_response();
        response
            .headers_mut()
            .insert("X-Frontend-Stale", "1".parse().unwrap());
        return Ok(response);
    }

    Ok(Json(resp).into_response())
}

/// Reason a `frontend_stale: true` was raised.
///
/// - `BuildFailed`: the slot-level flag set when `npm run build` errored
///   (or a defense-in-depth check in `build_monitor` flagged the slot).
/// - `SrcDrift`: mtime-based check (any
///   `src/**/*.{ts,tsx,css,json,html}` newer than `dist/index.html`).
/// - `DistMissing`: `dist/index.html` does not exist on disk at all. This
///   used to be silently treated as "not stale" by the per-spawn walker —
///   exactly the silent-success bug in
///   `supervisor-frontend-build-silent-success.md`. Surface it loudly
///   instead so callers learn the runner will serve `asset not found:
///   index.html`.
///
/// Reported back to callers via `frontend_stale_reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrontendStaleReason {
    BuildFailed,
    SrcDrift,
    DistMissing,
}

impl FrontendStaleReason {
    fn as_str(&self) -> &'static str {
        match self {
            FrontendStaleReason::BuildFailed => "build_failed",
            FrontendStaleReason::SrcDrift => "src_newer_than_dist",
            FrontendStaleReason::DistMissing => "dist_missing",
        }
    }
}

/// Inspect the build pool and the on-disk dist mtime; return
/// `(stale, reason, slot_id)` describing the spawn-test/spawn-named caller's
/// effective frontend freshness. `stale` is the OR of:
///
/// 1. The slot-level `frontend_stale` flag (set by `build_monitor` when the
///    most recent `npm run build` for that slot errored). Prefers
///    `last_successful_slot`; falls back to "any slot stale" otherwise.
/// 2. mtime drift: the newest mtime under `qontinui-runner/src/` is greater
///    than `qontinui-runner/dist/index.html`'s mtime. This catches the common
///    case where someone edited a `.tsx` file but never ran `npm run build`,
///    so the dist on disk doesn't reflect current source.
///
/// `reason` distinguishes the two for diagnostic surfaces. `BuildFailed` wins
/// when both are true (the error is more actionable than the drift). `slot_id`
/// is populated only for the build-failure path; mtime-drift is per-checkout,
/// not per-slot.
async fn resolve_frontend_stale_for_spawn(
    state: &SharedState,
) -> (bool, Option<FrontendStaleReason>, Option<usize>) {
    // (1) Build-failure flag first — most actionable signal.
    let last_successful = *state.build_pool.last_successful_slot.read().await;
    if let Some(sid) = last_successful {
        if let Some(slot) = state.build_pool.slots.iter().find(|s| s.id == sid) {
            if *slot.frontend_stale.read().await {
                return (true, Some(FrontendStaleReason::BuildFailed), Some(sid));
            }
        }
    } else if state.build_pool.any_slot_has_stale_frontend().await {
        return (true, Some(FrontendStaleReason::BuildFailed), None);
    }

    // (2) On-disk dist/ check — handles two cases:
    //   - `dist/index.html` is missing entirely (the silent-success bug:
    //     prior to 2026-05-11 this returned "not stale" and the runner
    //     went on to serve `asset not found: index.html`).
    //   - mtime drift: newest mtime under `qontinui-runner/src/` is later
    //     than `qontinui-runner/dist/index.html`.
    // project_dir is `qontinui-runner/src-tauri/`, so the runner root is
    // its parent.
    let runner_root = match state.config.project_dir.parent() {
        Some(p) => p.to_path_buf(),
        None => return (false, None, None),
    };
    if let Some(reason) = check_dist_freshness(&runner_root).await {
        return (true, Some(reason), None);
    }

    (false, None, None)
}

/// Inspect `<runner_root>/dist/index.html` against `<runner_root>/src/` and
/// return the appropriate `FrontendStaleReason` (or `None` if the dist on
/// disk genuinely covers the current source).
///
/// Three outcomes:
///
/// - `Some(DistMissing)` — `dist/index.html` doesn't exist on disk. Prior
///   to 2026-05-11 this silently returned "not stale", which let the
///   runner spawn with no embedded frontend and serve `asset not found:
///   index.html` at every route. See
///   `supervisor-frontend-build-silent-success.md`.
/// - `Some(SrcDrift)` — newest mtime under `src/` walks `.ts`, `.tsx`,
///   `.css`, `.json`, `.html` only and is later than the dist mtime —
///   touching unrelated assets shouldn't trip the signal.
/// - `None` — dist exists and is at least as new as every input file we
///   look at, OR the `src/` tree is empty (nothing to be stale relative
///   to).
async fn check_dist_freshness(runner_root: &std::path::Path) -> Option<FrontendStaleReason> {
    use std::time::SystemTime;

    let dist_index = runner_root.join("dist").join("index.html");
    let src_root = runner_root.join("src");

    let dist_mtime = match std::fs::metadata(&dist_index).and_then(|m| m.modified()) {
        Ok(t) => t,
        // Missing dist/index.html IS the stale state we want to catch — the
        // runner about to spawn has no embedded frontend at all. The earlier
        // "no dist yet — let the build fail loudly elsewhere" comment was
        // wrong: nothing else fails loudly, the cargo build embeds the
        // empty dist/ silently and the runner serves
        // `asset not found: index.html`.
        Err(_) => return Some(FrontendStaleReason::DistMissing),
    };

    // Walk synchronously — the supervisor spawn-test path is already async
    // but the walk itself is short (a few hundred files at most). Using
    // `tokio::task::spawn_blocking` to keep the runtime responsive.
    let src_root = src_root.clone();
    let newest_src = tokio::task::spawn_blocking(move || -> Option<SystemTime> {
        let mut newest: Option<SystemTime> = None;
        let mut stack = vec![src_root];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    // Skip node_modules / .git / generated dirs to keep the
                    // walk bounded and avoid false positives from churn in
                    // dependency trees.
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if matches!(name, "node_modules" | ".git" | "dist" | "target") {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext, "ts" | "tsx" | "css" | "json" | "html") {
                        if let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) {
                            newest = Some(match newest {
                                Some(n) if n >= mtime => n,
                                _ => mtime,
                            });
                        }
                    }
                }
            }
        }
        newest
    })
    .await
    .ok()
    .flatten();

    match newest_src {
        Some(n) if n > dist_mtime => Some(FrontendStaleReason::SrcDrift),
        // Either dist is at least as new as every src file, or the src
        // tree is empty / unreadable — nothing to flag.
        _ => None,
    }
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
        /// Parsed startup panic from `runner-panic.log` when the runner
        /// died during startup. `None` if no panic log was found (child
        /// is hung but alive, or exited for a non-panic reason). Boxed
        /// to keep the enum discriminant size balanced — the parsed
        /// struct holds the full panic payload + backtrace preview and
        /// is much larger than the `Healthy` variant.
        recent_panic: Box<Option<crate::process::panic_log::RecentPanic>>,
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

    // Check for a startup panic log. The runner's panic hook writes it
    // synchronously on the panicking thread, so if early init panicked
    // before the health endpoint bound, this file should exist by the
    // time we give up polling.
    let recent_panic = if let Some(managed) = state.get_runner(runner_id).await {
        let dir_opt = managed.panic_log_dir.read().await.clone();
        let path = crate::process::panic_log::resolve_panic_log_path(dir_opt.as_deref());
        let parsed = crate::process::panic_log::parse_panic_file(&path);
        if let Some(p) = parsed.as_ref() {
            // Freshness: a panic log from a prior boot of a re-used runner
            // id would lie here. Match the same 60s window the exit-
            // monitor uses.
            if !crate::process::panic_log::is_fresh(p, chrono::Utc::now()) {
                None
            } else {
                // Stash on the managed runner so subsequent callers of
                // GET /runners see the same record.
                let mut slot = managed.recent_panic.write().await;
                *slot = Some(p.clone());
                Some(p.clone())
            }
        } else {
            None
        }
    } else {
        None
    };

    ProbeOutcome::Failed {
        elapsed_ms,
        child_alive,
        pid,
        recent_logs,
        recent_panic: Box::new(recent_panic),
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
            kind: RunnerKind::Named { name: name.clone() },
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

    // Item A — build_result tracking (mirror of spawn_test).
    let mut build_attempted = false;
    let mut build_succeeded: Option<bool> = None;
    let mut build_error: Option<String> = None;
    let mut build_reused_stale = false;

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

        let build_outcome =
            crate::build_monitor::run_cargo_build_with_requester(&state, body.requester_id.clone())
                .await;
        match build_outcome {
            Ok(()) => {
                build_attempted = true;
                build_succeeded = Some(true);
            }
            Err(e) => {
                if body.allow_stale_fallback {
                    let err_str = e.to_string();
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Warn,
                            format!("spawn-named stale fallback engaged: {}", err_str),
                        )
                        .await;
                    build_attempted = true;
                    build_succeeded = Some(false);
                    build_error = Some(err_str);
                } else {
                    let mut runners = state.runners.write().await;
                    runners.remove(&id);
                    return Err(e);
                }
            }
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

    // After resolve_source_exe succeeded with a failed-but-swallowed build,
    // the previous slot exe is what we'll launch — flag it as stale-reuse.
    if matches!(build_succeeded, Some(false)) {
        build_reused_stale = true;
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

    let (frontend_stale, stale_reason, stale_slot_id) =
        resolve_frontend_stale_for_spawn(&state).await;

    // Item A: assemble build_result alongside the existing top-level
    // binary_mtime/binary_size_bytes (preserved on spawn-named for
    // backward compat).
    let post_build_slot_id = *state.build_pool.last_successful_slot.read().await;
    let build_result = build_result_json(
        build_attempted,
        build_succeeded,
        build_reused_stale,
        build_error.as_deref(),
        post_build_slot_id,
        exe_meta.as_ref(),
    );

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
        "build_result": build_result,
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

    // Symmetric with spawn_test: always emit `frontend_stale`, plus a
    // diagnostic `frontend_stale_reason` when set. (Named runners don't get
    // the `frontend_strict` opt-out yet — they're long-lived, so callers who
    // care can pre-check the response and stop the runner explicitly.)
    resp["frontend_stale"] = json!(frontend_stale);
    if frontend_stale {
        let reason_str = stale_reason.map(|r| r.as_str()).unwrap_or("unknown");
        resp["frontend_stale_reason"] = json!(reason_str);
        let stale_msg = match stale_reason {
            Some(FrontendStaleReason::BuildFailed) => match stale_slot_id {
                Some(sid) => format!(
                    "frontend_stale: slot {} embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.",
                    sid
                ),
                None => "frontend_stale: the active build slot embeds a stale dist/ because the most recent `npm run build` failed. Fix tsc errors and rebuild to refresh.".to_string(),
            },
            Some(FrontendStaleReason::SrcDrift) => "frontend_stale: src/**/*.{ts,tsx,css,json,html} is newer than dist/index.html — the runner embeds a UI that doesn't reflect current source. Run `cd qontinui-runner && npm run build` to refresh.".to_string(),
            Some(FrontendStaleReason::DistMissing) => "frontend_stale: dist/index.html is missing — likely a concurrent external `npm run build` wiped dist/, or an npm-exit-0 empty-output regression. Run `cd qontinui-runner && npm run build` to rebuild.".to_string(),
            None => "frontend_stale: the runner may embed a stale frontend dist (reason unavailable).".to_string(),
        };
        resp["warnings"] = json!([stale_msg]);

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
            "last_error_detail": history_snapshot.last_error_detail,
            // Inline ~1 KiB tail of the most recent FAILED build's stderr.
            // Cleared on subsequent success. Full log: GET /builds/{id}/log.
            "last_error_log": history_snapshot.last_error_log,
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
    let lkg_json = state
        .build_pool
        .last_known_good
        .read()
        .await
        .as_ref()
        .map(|info| {
            json!({
                "built_at": info.built_at.to_rfc3339(),
                "source_slot": info.source_slot,
                "exe_size": info.exe_size,
            })
        });
    Json(json!({
        "pool_size": state.build_pool.slots.len(),
        "available_permits": available,
        "queued": queued,
        "last_successful_slot": last_successful,
        "avg_build_duration_secs": avg_build_duration_secs,
        "any_slot_has_stale_frontend": any_slot_has_stale_frontend,
        "slots": slots_json,
        "lkg": lkg_json,
    }))
}

/// GET /builds/{slot_id}/log — return the in-memory combined log of the
/// most recent build attempt on this slot (success or failure).
///
/// Reads `BuildSlot::last_build_log`, populated by `run_build_inner`. Reset
/// at the start of each build so a reader hitting this endpoint mid-build
/// gets `null` rather than stale bytes from the previous attempt. Persists
/// across builds on the same slot until the next build replaces it; lost on
/// supervisor restart (use `GET /builds/{slot_id}/last-build-stderr` for the
/// disk-persisted failure log).
///
/// Response: `{ slot_id, log: Option<String>, captured_at: Option<String> }`
/// — `captured_at` is the build-completion timestamp in RFC3339, `null` when
/// no build has finished on this slot yet (or one is in flight). 404 when
/// `slot_id` is out of range.
pub async fn slot_build_log(
    State(state): State<SharedState>,
    Path(slot_id): Path<usize>,
) -> impl IntoResponse {
    let slot = match state.build_pool.slots.get(slot_id) {
        Some(s) => s.clone(),
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "slot_not_found",
                    "slot_id": slot_id,
                    "pool_size": state.build_pool.slots.len(),
                })),
            )
                .into_response();
        }
    };
    let captured = slot.last_build_log.read().await.clone();
    let (log, captured_at) = match captured {
        Some((ts, log)) => (Some(log), Some(ts.to_rfc3339())),
        None => (None, None),
    };
    Json(json!({
        "slot_id": slot_id,
        "log": log,
        "captured_at": captured_at,
    }))
    .into_response()
}

/// GET /builds/{slot_id}/last-build-stderr — return the persisted cargo
/// stderr from the most recent failed build on this slot.
///
/// Reads `target-pool/slot-{slot_id}/last-build.stderr`. Returns 404 when
/// the slot id is out of range or the file does not exist (no failure has
/// been recorded since the slot dir was provisioned).
pub async fn slot_last_build_stderr(
    State(state): State<SharedState>,
    Path(slot_id): Path<usize>,
) -> impl IntoResponse {
    let slot = match state.build_pool.slots.get(slot_id) {
        Some(s) => s.clone(),
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "slot_not_found",
                    "slot_id": slot_id,
                    "pool_size": state.build_pool.slots.len(),
                })),
            )
                .into_response();
        }
    };
    let path = slot.target_dir.join("last-build.stderr");
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => {
            let mut resp = axum::response::Response::new(axum::body::Body::from(contents));
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            resp
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({
                "error": "no_stderr_recorded",
                "slot_id": slot_id,
                "path": path.to_string_lossy(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "read_failed",
                "slot_id": slot_id,
                "path": path.to_string_lossy(),
                "detail": e.to_string(),
            })),
        )
            .into_response(),
    }
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
        // Phase 2b: include the structured startup panic record when
        // we have one. Optional field — existing consumers that didn't
        // opt in continue to see the same shape.
        let recent_panic = managed.recent_panic.read().await.clone();
        // Phase 2c (Item 9): mirror `stale_binary` from the `/runners` listing
        // so single-runner-drill-down callers (dashboard log pane, CLI) see
        // the same freshness signal without issuing a second request.
        let stale_binary = manager::stale_binary_for_runner(&state, &managed.config).await;
        // Per-spawn early-death log file path, when the runner was started
        // by the supervisor (any spawn flow). `null` for primary runners or
        // runners imported into the registry without a managed start.
        let early_log_path = managed
            .early_log_path
            .read()
            .await
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        return Ok(Json(json!({
            "runner_id": id,
            "entries": entries,
            "count": count,
            "recent_panic": recent_panic,
            "stale_binary": stale_binary,
            "early_log_path": early_log_path,
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
                "recent_panic": snapshot.recent_panic,
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

/// Maximum bytes returned in the `content` field of `GET /runners/{id}/early-log`.
///
/// Early-log files are hard-capped at [`crate::process::early_log::EARLY_LOG_BYTE_CAP`]
/// (256 KiB) at write time, so a clean file will never exceed this. The cap
/// here is defense-in-depth: if an external tool grew the file or a future
/// change raises the writer cap, we still bound the response body.
const EARLY_LOG_RESPONSE_BYTE_CAP: usize = 1024 * 1024;

/// GET /runners/{id}/early-log — return the full per-spawn early-death log
/// for a runner.
///
/// **Why this exists.** The spawn-test failure body includes `recent_logs`
/// (last ~20 lines) and an `early_log_path` filesystem path. The full file
/// content (often 50+ lines including PG migration progress, panic info,
/// the runner's startup banner, etc.) is the diagnostic gold but currently
/// has to be read by hand from disk. This endpoint lets HTTP clients (slash
/// commands, automation, dashboard) fetch it directly.
///
/// **Lookup order.** Live registry first (`state.runners`), then the
/// post-mortem cache (`state.stopped_runners`). Both store the path in
/// `early_log_path`. Returns 404 if neither has the runner OR the path is
/// missing/the file no longer exists on disk.
///
/// **Body cap.** Responses are capped at 1 MB; if the file is larger, only
/// the **last** 1 MB is returned (matches what crash diagnosis cares about).
/// `truncated` is set to `true` in that case.
pub async fn runner_early_log(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    // Live-registry first.
    let path_opt = if let Some(managed) = state.get_runner(&id).await {
        managed.early_log_path.read().await.clone()
    } else {
        // Fall through to the stopped-runner cache. Read lock is held only
        // long enough to clone the PathBuf.
        let cache = state.stopped_runners.read().await;
        match cache.get(&id) {
            Some(snapshot) => snapshot.early_log_path.clone(),
            None => {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": "runner_not_found",
                        "runner_id": id,
                    })),
                )
                    .into_response();
            }
        }
    };

    let path = match path_opt {
        Some(p) => p,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "early_log_unavailable",
                    "reason": "runner has no captured early_log_path (no managed spawn or open failed)",
                    "runner_id": id,
                })),
            )
                .into_response();
        }
    };

    // Read the full file, then truncate to the last EARLY_LOG_RESPONSE_BYTE_CAP
    // bytes if needed. We deliberately read into memory (not stream) because
    // the byte cap is small and the response is JSON, not chunked-text — and
    // streaming would race the writer's in-place truncation in
    // `early_log::truncate_file_in_place`.
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "early_log_unavailable",
                    "reason": format!("file not found at {}", path.display()),
                    "runner_id": id,
                    "path": path.to_string_lossy(),
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "early_log_unavailable",
                    "reason": format!("read failed: {}", e),
                    "runner_id": id,
                    "path": path.to_string_lossy(),
                })),
            )
                .into_response();
        }
    };

    let original_size = raw.len();
    let (content, truncated) = if original_size > EARLY_LOG_RESPONSE_BYTE_CAP {
        // Take the *last* cap bytes. The boundary may land mid-UTF-8 code
        // point (rare for log content, but possible). `char_indices` snaps
        // forward to the next valid boundary so the JSON serializer doesn't
        // bail.
        let want_start = original_size - EARLY_LOG_RESPONSE_BYTE_CAP;
        let snap = raw
            .char_indices()
            .find(|(i, _)| *i >= want_start)
            .map(|(i, _)| i)
            .unwrap_or(original_size);
        (raw[snap..].to_string(), true)
    } else {
        (raw, false)
    };

    Json(json!({
        "runner_id": id,
        "path": path.to_string_lossy(),
        "content": content,
        "size_bytes": original_size,
        "truncated": truncated,
    }))
    .into_response()
}

/// Maximum bytes returned in the `panic_excerpt` field of
/// `GET /runners/{id}/crash-summary`. If the runner-last-panic.txt file is
/// larger than this, only the last [`CRASH_SUMMARY_PANIC_EXCERPT_CAP`]
/// bytes are returned (the tail is what matters for diagnosis).
const CRASH_SUMMARY_PANIC_EXCERPT_CAP: usize = 8 * 1024;

/// Number of recent log lines included in the `last_phase_log` field of
/// the crash-summary response.
const CRASH_SUMMARY_LAST_PHASE_LOG_LINES: usize = 20;

/// GET /runners/{id}/crash-summary — post-mortem diagnostics for a stopped runner.
///
/// **Why this exists.** When a temp runner dies during startup, callers
/// currently have to assemble crash diagnostics from multiple sources
/// (spawn-test response body, `recent_logs`, `early_log_path`,
/// `panic_log_dir`, `stopped_runners` cache). This endpoint centralizes
/// them into a single response.
///
/// **Lookup order.**
/// 1. If `id` is in the live registry AND the runner is currently running
///    (`runner.running == true`) → return 404 with
///    `{"error": "runner_still_alive"}`. Crash-summary is post-mortem only.
/// 2. If `id` is in the stopped-runners cache → assemble the response from
///    the snapshot.
/// 3. Otherwise → 404 with `{"error": "runner_not_found"}`.
///
/// **`panic_excerpt`** is the contents of
/// `<panic_log_dir>/runner-last-panic.txt` if the file exists, else `null`.
/// Capped at [`CRASH_SUMMARY_PANIC_EXCERPT_CAP`] bytes (the *last* bytes if
/// the file is larger).
pub async fn runner_crash_summary(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::http::StatusCode;

    // Rule 1: live + running → not post-mortem yet.
    if let Some(managed) = state.get_runner(&id).await {
        let is_running = managed.runner.read().await.running;
        if is_running {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "runner_still_alive",
                    "runner_id": id,
                })),
            )
                .into_response();
        }
    }

    // Rule 2: stopped-cache lookup. Clone the snapshot under the read lock
    // and then drop the lock before doing async file I/O.
    let snapshot_opt = {
        let cache = state.stopped_runners.read().await;
        cache.get(&id).cloned()
    };

    let snapshot = match snapshot_opt {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "runner_not_found",
                    "runner_id": id,
                })),
            )
                .into_response();
        }
    };

    // Compose `last_phase_log`: the last N log lines as a single newline-joined string.
    let last_phase_log: String = {
        let total = snapshot.last_log_lines.len();
        let start = total.saturating_sub(CRASH_SUMMARY_LAST_PHASE_LOG_LINES);
        snapshot.last_log_lines[start..]
            .iter()
            .map(|entry| {
                format!(
                    "{} [{:?}] [{:?}] {}",
                    entry.timestamp.to_rfc3339(),
                    entry.source,
                    entry.level,
                    entry.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Compose `panic_excerpt`: read <panic_log_dir>/runner-last-panic.txt if present.
    let panic_excerpt: Option<String> = match snapshot.panic_log_dir.as_ref() {
        Some(dir) => {
            let path = dir.join("runner-last-panic.txt");
            match tokio::fs::read_to_string(&path).await {
                Ok(s) => {
                    if s.len() > CRASH_SUMMARY_PANIC_EXCERPT_CAP {
                        // Take the last cap bytes, snapping forward to a UTF-8
                        // boundary so JSON serialization can't fail mid-codepoint.
                        let want_start = s.len() - CRASH_SUMMARY_PANIC_EXCERPT_CAP;
                        let snap = s
                            .char_indices()
                            .find(|(i, _)| *i >= want_start)
                            .map(|(i, _)| i)
                            .unwrap_or(s.len());
                        Some(s[snap..].to_string())
                    } else {
                        Some(s)
                    }
                }
                Err(_) => None,
            }
        }
        None => None,
    };

    // duration_alive_ms = stopped_at - started_at (when both available).
    let duration_alive_ms: Option<i64> = snapshot.started_at.map(|started| {
        let delta = snapshot.stopped_at.signed_duration_since(started);
        delta.num_milliseconds()
    });

    let stop_reason = match snapshot.exit_reason {
        crate::process::stopped_cache::StopReason::GracefulStop => "GracefulStop",
        crate::process::stopped_cache::StopReason::Reaped => "Reaped",
        crate::process::stopped_cache::StopReason::Crashed => "Crashed",
        crate::process::stopped_cache::StopReason::Unknown => "Unknown",
    };

    Json(json!({
        "runner_id": snapshot.id,
        "name": snapshot.name,
        "exit_code": snapshot.exit_code,
        "duration_alive_ms": duration_alive_ms,
        "stopped_at": snapshot.stopped_at.to_rfc3339(),
        "stop_reason": stop_reason,
        "last_phase_log": last_phase_log,
        "panic_excerpt": panic_excerpt,
        "early_log_path": snapshot.early_log_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
    }))
    .into_response()
}

/// GET /runners/{id}/logs/stream — SSE stream of real-time log events for a specific runner.
///
/// Terminates on `state.shutdown_signal()` so axum's graceful drain can
/// complete promptly when the supervisor is asked to exit.
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

    // Track this connection in `state.active_sse_connections`. Captured
    // by-move into the per-event closure below so it lives exactly as long
    // as the stream — drop happens when axum tears down the response.
    let conn_guard = SseConnectionGuard::new(state.active_sse_connections.clone());

    let event_stream = stream.filter_map(move |result| {
        // Hold the guard for every yielded event so the stream owns it.
        let _hold = &conn_guard;
        match result {
            Ok(entry) => {
                let data = serde_json::to_string(&entry).unwrap_or_default();
                Some(Ok(Event::default().event("log").data(data)))
            }
            Err(_) => None,
        }
    });

    let shutdown_state = state.clone();
    let shutdown = Box::pin(async move { shutdown_state.shutdown_signal().await });
    let event_stream = futures::StreamExt::take_until(event_stream, shutdown);

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

#[cfg(test)]
mod tests {
    //! Regression tests for `check_dist_freshness` and the
    //! `FrontendStaleReason` wire format. The motivating bug
    //! (`supervisor-frontend-build-silent-success.md`) was that the old
    //! `check_src_newer_than_dist` returned `false` (= "not stale") on a
    //! missing `dist/index.html`, exactly the case the gate was supposed
    //! to catch. These tests pin the new contract: missing dist surfaces
    //! as `Some(DistMissing)`, src drift as `Some(SrcDrift)`, healthy
    //! state as `None`.
    use super::{check_dist_freshness, FrontendStaleReason};
    use std::fs;
    use tempfile::TempDir;

    fn write_file(path: &std::path::Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir parent");
        }
        fs::write(path, contents).expect("write file");
    }

    #[tokio::test]
    async fn returns_dist_missing_when_dist_index_absent() {
        // Repro of the exact silent-success bug: src/ has files but
        // dist/index.html is gone. Old code returned false (not stale);
        // new code must return Some(DistMissing).
        let tmp = TempDir::new().expect("tempdir");
        write_file(
            &tmp.path().join("src").join("App.tsx"),
            b"export const App = () => null;",
        );

        let result = check_dist_freshness(tmp.path()).await;
        assert_eq!(
            result,
            Some(FrontendStaleReason::DistMissing),
            "missing dist/index.html must be flagged DistMissing, not silently treated as fresh"
        );
    }

    #[tokio::test]
    async fn returns_dist_missing_even_when_src_tree_is_empty() {
        // Edge case: no src files at all, no dist either. Should still
        // flag DistMissing rather than silently returning None — a
        // runner with no embedded frontend is broken regardless of
        // whether src/ has anything to drift.
        let tmp = TempDir::new().expect("tempdir");
        let result = check_dist_freshness(tmp.path()).await;
        assert_eq!(result, Some(FrontendStaleReason::DistMissing));
    }

    #[tokio::test]
    async fn returns_src_drift_when_src_newer_than_dist() {
        // Build dist first, then write src/. The src file's mtime will
        // be at least as new as dist/index.html — sleep is enough on
        // every supported platform to guarantee strict-newer.
        let tmp = TempDir::new().expect("tempdir");
        write_file(&tmp.path().join("dist").join("index.html"), b"<old/>");

        // Filesystem mtime resolution can be coarse (FAT = 2s, HFS = 1s);
        // sleep enough to clear the worst case so the assertion is
        // deterministic.
        std::thread::sleep(std::time::Duration::from_millis(2100));

        write_file(
            &tmp.path().join("src").join("App.tsx"),
            b"export const App = () => 'changed';",
        );

        let result = check_dist_freshness(tmp.path()).await;
        assert_eq!(result, Some(FrontendStaleReason::SrcDrift));
    }

    #[tokio::test]
    async fn returns_none_when_dist_newer_than_src() {
        // Happy path: src/ written, then dist built afterward.
        let tmp = TempDir::new().expect("tempdir");
        write_file(
            &tmp.path().join("src").join("App.tsx"),
            b"export const App = () => null;",
        );

        std::thread::sleep(std::time::Duration::from_millis(2100));

        write_file(
            &tmp.path().join("dist").join("index.html"),
            b"<!doctype html><html/>",
        );

        let result = check_dist_freshness(tmp.path()).await;
        assert_eq!(
            result, None,
            "dist newer than src is the healthy state; must not be flagged"
        );
    }

    #[tokio::test]
    async fn ignores_unrelated_extensions_in_src() {
        // Touching a .md or .png in src/ shouldn't trip the drift signal —
        // the walker only looks at .ts/.tsx/.css/.json/.html.
        let tmp = TempDir::new().expect("tempdir");
        write_file(&tmp.path().join("dist").join("index.html"), b"<built/>");
        std::thread::sleep(std::time::Duration::from_millis(2100));
        write_file(&tmp.path().join("src").join("README.md"), b"# notes");
        write_file(&tmp.path().join("src").join("logo.png"), b"\x89PNG fake");

        let result = check_dist_freshness(tmp.path()).await;
        assert_eq!(
            result, None,
            "non-source extensions must not trigger SrcDrift"
        );
    }

    #[test]
    fn frontend_stale_reason_wire_format_is_stable() {
        // External callers parse the `frontend_stale_reason` string —
        // pin the wire format so a refactor can't silently rename it.
        assert_eq!(FrontendStaleReason::BuildFailed.as_str(), "build_failed");
        assert_eq!(
            FrontendStaleReason::SrcDrift.as_str(),
            "src_newer_than_dist"
        );
        assert_eq!(FrontendStaleReason::DistMissing.as_str(), "dist_missing");
    }
}
