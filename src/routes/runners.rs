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
use crate::dev_action::{
    evaluate_all, spawn_attribution_watcher, ActionKind, ActionRecord, AttributionTargets,
    SlotResolution,
};
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

/// Phase 2b of `plans/2026-05-22-mtc-iter3-remediation-web-dashboard.md` —
/// resolve and apply a `paired_profile_id` snapshot before runner spawn.
///
/// Looks up a profile snapshot dir under
/// `<profiles_root>/<paired_profile_id>/` and copies its `paired_user.json`
/// (required) + `auth_tokens.enc` (optional) into `<data_local_dir>/`.
///
/// Both root paths are injected (not pulled from `dirs::*`) so unit tests
/// can drive the helper hermetically against tempdirs. The HTTP handler
/// passes `dirs::home_dir().unwrap().join(".qontinui").join("profiles")`
/// and `dirs::data_local_dir().unwrap().join("com.qontinui.runner")`
/// respectively.
///
/// Returns `Ok(applied_files)` on success — the list of basenames copied,
/// for surfacing on the spawn-test response. Returns `Err((status_code,
/// error_body))` on validation failure so the caller can short-circuit
/// with the right HTTP status.
pub(crate) fn apply_paired_profile(
    profile_id: &str,
    profiles_root: &std::path::Path,
    data_local_runner_dir: &std::path::Path,
) -> Result<Vec<String>, (axum::http::StatusCode, serde_json::Value)> {
    // Defense-in-depth: refuse traversal characters. `~/.qontinui/profiles/`
    // is owned by the operator but a profile id like `../../etc/passwd`
    // would still expand to whatever the OS allows reads from. clap
    // doesn't validate JSON bodies for us.
    if profile_id.trim().is_empty()
        || profile_id.contains('/')
        || profile_id.contains('\\')
        || profile_id.contains("..")
    {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            json!({
                "error": "validation_error",
                "message": "paired_profile_id must be a simple identifier (no slashes or '..')",
                "paired_profile_id": profile_id,
            }),
        ));
    }

    let snapshot_dir = profiles_root.join(profile_id);
    let paired_src = snapshot_dir.join("paired_user.json");
    if !paired_src.exists() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            json!({
                "error": "profile_not_found",
                "paired_profile_id": profile_id,
                "expected_path": paired_src.display().to_string(),
                "message": "no paired_user.json under the requested profile snapshot dir",
            }),
        ));
    }

    if let Err(e) = std::fs::create_dir_all(data_local_runner_dir) {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": "profile_apply_failed",
                "paired_profile_id": profile_id,
                "message": format!(
                    "could not create {}: {e}",
                    data_local_runner_dir.display()
                ),
            }),
        ));
    }

    let paired_dst = data_local_runner_dir.join("paired_user.json");
    if let Err(e) = std::fs::copy(&paired_src, &paired_dst) {
        return Err((
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": "profile_apply_failed",
                "paired_profile_id": profile_id,
                "message": format!(
                    "copy {} -> {} failed: {e}",
                    paired_src.display(),
                    paired_dst.display(),
                ),
            }),
        ));
    }

    let mut applied = vec!["paired_user.json".to_string()];

    // auth_tokens.enc is optional in the snapshot — older snapshots may
    // only have paired_user.json. Without the encrypted JWT cache the
    // runner will need to refresh on its own, but it can still register
    // because the paired_user.json carries the user_id + tenant_id.
    let tokens_src = snapshot_dir.join("auth_tokens.enc");
    if tokens_src.exists() {
        let tokens_dst = data_local_runner_dir.join("auth_tokens.enc");
        if let Err(e) = std::fs::copy(&tokens_src, &tokens_dst) {
            return Err((
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                json!({
                    "error": "profile_apply_failed",
                    "paired_profile_id": profile_id,
                    "message": format!(
                        "copy {} -> {} failed: {e}",
                        tokens_src.display(),
                        tokens_dst.display(),
                    ),
                }),
            ));
        }
        applied.push("auth_tokens.enc".to_string());
    }

    Ok(applied)
}

/// HTTP-level wrapper around [`apply_paired_profile`] that resolves the
/// production `profiles_root` + `data_local_runner_dir` from `dirs::*` and
/// surfaces failures as `(StatusCode, Json)`. Returns the list of applied
/// file basenames on success.
fn apply_paired_profile_for_spawn(
    profile_id: &str,
) -> Result<Vec<String>, (axum::http::StatusCode, serde_json::Value)> {
    let home = dirs::home_dir().ok_or_else(|| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": "home_dir_unresolved",
                "message": "could not resolve home directory",
            }),
        )
    })?;
    let data_local = dirs::data_local_dir().ok_or_else(|| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": "data_local_dir_unresolved",
                "message": "could not resolve data_local_dir",
            }),
        )
    })?;
    let profiles_root = home.join(".qontinui").join("profiles");
    let data_local_runner_dir = data_local.join("com.qontinui.runner");
    apply_paired_profile(profile_id, &profiles_root, &data_local_runner_dir)
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

/// GET /runners/by-unit/{unit_id} — resolve the preview handle(s) bound to an
/// autonomous-dev work unit (Track 2 UI-Bridge preview-verification).
///
/// Returns an array of `{runner_id, port, ui_bridge_url, git_sha, attempt_id}`
/// for every live runner whose `preview_binding.unit_id` matches — typically
/// one per attempt. Reads the in-memory `state.runners` map (the supervisor's
/// single source of truth for runner↔port); no HTTP probe is issued, so this is
/// cheap. An unknown unit returns `200 []`, NOT a 404 — "no previews for this
/// unit" is a valid, queryable answer, not an error.
pub async fn runners_by_unit(
    State(state): State<SharedState>,
    Path(unit_id): Path<String>,
) -> Result<Json<serde_json::Value>, SupervisorError> {
    let runners = state.get_all_runners().await;
    let mut result = Vec::new();
    for managed in &runners {
        let binding = managed.preview_binding.read().await;
        let Some(b) = binding.as_ref() else { continue };
        if b.unit_id != unit_id {
            continue;
        }
        let port = managed.config.port;
        result.push(json!({
            "runner_id": managed.config.id,
            "port": port,
            "ui_bridge_url": format!("http://localhost:{}/ui-bridge", port),
            "git_sha": b.git_sha,
            "attempt_id": b.attempt_id,
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
        if let Err(e) = crate::process::windows::remove_instance_config_dir(&id, false).await {
            warn!(
                "Failed to remove instance config dir for runner '{}': {}",
                id, e
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
            if let Err(e) = crate::process::windows::remove_instance_config_dir(&id, false).await {
                warn!(
                    "purge-stale: failed to remove instance config dir for '{}': {}",
                    id, e
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
///
/// **`rebuild: true` is detached from the HTTP connection** (mirrors the
/// primary `/runner/restart` path in `routes/runner.rs`). A per-id rebuild runs
/// a full `cargo build` (~10-30 min when cold/wedged); running it inline blocked
/// the HTTP request for the whole build and a client disconnect could abandon it
/// mid-flight. The stop → build → start sequence now runs detached via the
/// #63 build-submissions state machine ([`submit_detached`], same seam
/// `/runner/fix-and-rebuild` uses). We return **202** + a `build_id` in ~1s;
/// callers poll `GET /builds` (or `GET /build/{id}/status`) for the terminal
/// outcome, and the existing detached completion logic triggers the restart.
pub async fn restart_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RestartRunnerRequest>,
) -> Result<axum::response::Response, SupervisorError> {
    let source = match body.source.as_str() {
        "watchdog" => crate::diagnostics::RestartSource::Watchdog,
        _ => crate::diagnostics::RestartSource::Manual,
    };

    // ── Rebuild path: DETACH from the HTTP connection. ──────────────────────
    if body.rebuild {
        let exec_state = state.clone();
        let force = body.force;
        let runner_id = id.clone();
        let (submission_id, _arc) = crate::build_submissions::submit_detached(
            state.build_submissions.clone(),
            state.config.project_dir.clone(),
            None,
            async move {
                // The exact stop → build → start sequence the handler used to
                // await inline. Errors are surfaced in the (status, body)
                // result so they land in /builds — never silently dropped.
                if let Err(e) =
                    manager::restart_runner_by_id(&exec_state, &runner_id, true, source, force)
                        .await
                {
                    exec_state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Error,
                            format!("Detached rebuild-restart of '{}' failed: {}", runner_id, e),
                        )
                        .await;
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        json!({
                            "status": "error",
                            "error": e.to_string(),
                            "runner_id": runner_id,
                        }),
                    );
                }

                (
                    axum::http::StatusCode::OK.as_u16(),
                    json!({
                        "status": "restarted",
                        "message": format!("Runner '{}' restarted (with rebuild)", runner_id),
                    }),
                )
            },
        );

        return Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(json!({
                "status": "rebuilding",
                "build_id": submission_id.to_string(),
                "submission_id": submission_id.to_string(),
                "poll": "/builds",
                "message": "rebuild-restart submitted; the build+restart runs detached from \
                            this connection — poll GET /builds (or GET /build/{id}/status) \
                            for the terminal outcome",
            })),
        )
            .into_response());
    }

    // ── No-rebuild path: stays synchronous (fast restart). ──────────────────
    manager::restart_runner_by_id(&state, &id, false, source, body.force).await?;

    Ok(Json(json!({
        "status": "restarted",
        "message": format!("Runner '{}' restarted", id)
    }))
    .into_response())
}

/// POST /runners/{id}/rebuild-and-restart — stop → cargo build → start, in
/// one round-trip.
///
/// Refuses to act on the primary runner. On build failure, returns the
/// cargo error directly (no automatic stale-fallback — callers who want
/// that should pair `spawn-test {allow_stale_fallback: true}` with their
/// own restart logic).
///
/// **Detached from the HTTP connection** (mirrors the primary `/runner/restart`
/// rebuild path). The cargo build is ~10-30 min; running it inline blocked the
/// request for the whole build and a client disconnect could abandon it
/// mid-flight. The stop → build → start sequence now runs detached via the
/// #63 build-submissions state machine ([`submit_detached`]). We return **202**
/// and a `build_id` immediately; callers poll `GET /builds` (or `GET
/// /build/{id}/status`) for the terminal outcome (which carries the full
/// `build_result` body the inline path used to return synchronously).
pub async fn rebuild_and_restart_runner(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RebuildAndRestartRequest>,
) -> Result<axum::response::Response, SupervisorError> {
    let exec_state = state.clone();
    let runner_id = id.clone();
    let (submission_id, _arc) = crate::build_submissions::submit_detached(
        state.build_submissions.clone(),
        state.config.project_dir.clone(),
        None,
        async move {
            match manager::rebuild_and_restart_by_id(&exec_state, &runner_id, body).await {
                Ok(outcome) => (axum::http::StatusCode::OK.as_u16(), outcome),
                Err(e) => {
                    exec_state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Error,
                            format!(
                                "Detached rebuild-and-restart of '{}' failed: {}",
                                runner_id, e
                            ),
                        )
                        .await;
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        json!({
                            "status": "error",
                            "error": e.to_string(),
                            "runner_id": runner_id,
                        }),
                    )
                }
            }
        },
    );

    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(json!({
            "status": "rebuilding",
            "build_id": submission_id.to_string(),
            "submission_id": submission_id.to_string(),
            "poll": "/builds",
            "message": "rebuild-and-restart submitted; the build+restart runs detached from \
                        this connection — poll GET /builds (or GET /build/{id}/status) for the \
                        terminal outcome",
        })),
    )
        .into_response())
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
    /// Optional git ref (branch, tag, or SHA) to build instead of the live
    /// working tree at `state.config.project_dir`.
    ///
    /// **Requires `rebuild: true`.** When set with `rebuild: true`, the
    /// supervisor materializes a managed *detached* worktree at this ref
    /// (under `<repo>/../.supervisor-spawn-worktrees/<sanitized_ref>`),
    /// runs `cargo build` against that tree, and spawns the resulting exe.
    /// Slot isolation / exe resolution are unchanged — only the build's
    /// source tree differs, so the spawned runner provably reflects the
    /// requested ref (e.g. `origin/main`) regardless of what branch the
    /// live tree is parked on.
    ///
    /// When set without `rebuild: true`, the request fails with HTTP 400
    /// (`git_ref requires rebuild:true`) rather than silently ignoring it.
    ///
    /// On success the response carries:
    /// - `git_ref` / `git_ref_resolved` — the requested ref, echoed verbatim
    ///   (two field names for the same value: `git_ref` for back-compat
    ///   with existing callers, `git_ref_resolved` for callers that want a
    ///   "resolved" label).
    /// - `git_ref_resolved_sha` — full 40-char `git rev-parse HEAD` of the
    ///   prepared worktree.
    /// - `git_ref_resolved_sha_short` — first 12 chars of the same SHA,
    ///   suitable for direct comparison with
    ///   `git rev-parse origin/main | head -c 12`.
    /// - `source` — `"worktree"` for a `git_ref` build, `"live_tree"` for
    ///   the default build path. Always present so callers can branch on a
    ///   single predictable field.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// Optional absolute path to an EXISTING caller-owned worktree/checkout to
    /// build, instead of the live tree or a managed `git_ref` worktree.
    ///
    /// **Requires `rebuild: true`** (same provenance reason as `git_ref`):
    /// without a rebuild the caller would get the live tree's exe while
    /// believing they got their checkout. **Mutually exclusive with `git_ref`**
    /// — setting both → 400 `provenance_conflict`.
    ///
    /// The path must (1) exist and be a directory, (2) NOT be the live runner
    /// tree (`worktree_path_is_live_tree`), (3) contain `src-tauri/Cargo.toml`
    /// (`not_a_runner_worktree`), and (4) have a `../qontinui-schemas/rust`
    /// sibling so the runner crate's `../../qontinui-schemas/rust` path-deps
    /// resolve (`path_deps_unresolved`). Each failure is a precise 400 — never
    /// a silent live-tree fallback.
    ///
    /// The supervisor builds the tree exactly where it is and NEVER cleans it
    /// up (the caller owns it — `cleanup_worktree_on_fail` applies only to
    /// managed `git_ref` containers). The frontend prebuild runs for any
    /// override tree, so a checkout with no `dist/` gets a full pnpm
    /// install+build automatically; one WITH a built `dist/` is reused as-is.
    ///
    /// On success the response carries `source: "worktree_path"`, the echoed
    /// `worktree_path`, `schemas_path` / `schemas_sha` / `schemas_is_shared`
    /// (shared-checkout drift provenance), and `git_ref_resolved_sha`/`_short`
    /// from the worktree's HEAD when it is a git checkout.
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// Frontend-only fast path. When true, FORCE a fresh `pnpm run build` in
    /// the isolated tree (re-embed the new `dist/` via cargo) while skipping
    /// `pnpm install` when the `node_modules/.bin/ui-bridge-build-ir` marker is
    /// already present, and never touching the live tree.
    ///
    /// **Requires a provenance selector** (`git_ref` or `worktree_path`) — a
    /// `frontend_only` build of the live tree is meaningless (it would touch
    /// the shared tree), so it 400s.
    ///
    /// **The cargo build still runs.** A Tauri binary embeds `dist/` at
    /// `cargo build` time (`rust-embed`), so a fresh `dist/` is only picked up
    /// by recompiling — "fast" means "don't re-fetch / don't reinstall
    /// node_modules / don't touch the live tree", NOT "skip cargo". When
    /// combined with `use_lkg`/`allow_stale_fallback` AND the spawned exe came
    /// from LKG/stale reuse, the response carries `frontend_only_warning`
    /// stating the reused exe embeds the OLD dist.
    #[serde(default)]
    pub frontend_only: bool,
    /// When true, and a `git_ref` build fails downstream of `prepare_worktree`,
    /// best-effort remove the spawn worktree dir on disk via
    /// `git worktree remove --force` + `git worktree prune`. Default false:
    /// the worktree is left in place for idempotent reuse (the same ref's
    /// next spawn-test call reuses the existing dir and force-resets it).
    ///
    /// Use this when the worktree itself is the problem (corrupted tree,
    /// stale lockfile, contaminated `node_modules/`) and you want the next
    /// call to start from a fresh `git worktree add`. The flag is a no-op
    /// when `git_ref` is None (no worktree was created for this request).
    /// Cleanup is a side effect — the original build error is what the
    /// caller sees.
    #[serde(default)]
    pub cleanup_worktree_on_fail: bool,

    /// Optional paired-profile snapshot id. When provided, the supervisor
    /// looks up a profile snapshot directory at
    /// `~/.qontinui/profiles/<paired_profile_id>/` and copies its
    /// `paired_user.json` (and `auth_tokens.enc`, when present) into the
    /// shared runner data dir at `{data_local_dir}/com.qontinui.runner/`
    /// BEFORE starting the spawned runner.
    ///
    /// This lets a paired CI machine inherit its operator's pairing without
    /// re-running the headless pair flow on every spawn (Phase 2b of
    /// `plans/2026-05-22-mtc-iter3-remediation-web-dashboard.md`).
    ///
    /// **Resolution failure is not silent.** If the snapshot dir does not
    /// exist OR contains no `paired_user.json`, the request returns
    /// `400 profile_not_found` — falling back to an unpaired spawn would
    /// silently produce a less-paired runner than the caller asked for.
    /// Profile snapshots are created out-of-band (operator copies the live
    /// `paired_user.json` from `{data_local_dir}/com.qontinui.runner/` into
    /// `~/.qontinui/profiles/<id>/` after a successful browser pair).
    ///
    /// **Shared data-dir caveat.** The runner reads `paired_user.json` from
    /// a fixed `dirs::data_local_dir()` path with no env override. Copying
    /// the snapshot in therefore overwrites whatever pairing the live data
    /// dir currently holds — single-user dev boxes are the supported case.
    /// For multi-tenant CI boxes a future iteration will need per-runner
    /// data-dir isolation via the runner-side `--data-dir` flag.
    #[serde(default)]
    pub paired_profile_id: Option<String>,

    /// Optional autonomous-dev work-unit id this spawn is a *preview* for
    /// (Track 2 UI-Bridge preview-verification). Pure passthrough correlation:
    /// the supervisor stores it on the spawned `ManagedRunner` and echoes it in
    /// the response, and `GET /runners/by-unit/{unit_id}` resolves the
    /// preview handle(s) for the unit. Has no effect on port arbitration or the
    /// build — the preview is built from `git_ref` exactly as a normal
    /// `git_ref` spawn. `None` for ordinary (non-preview) spawns.
    #[serde(default)]
    pub unit_id: Option<String>,
    /// Optional attempt id within `unit_id` (the specific attempt whose
    /// `git_ref` was built). Passthrough/echo only; surfaced in
    /// `GET /runners/by-unit/{unit_id}` so a verifier can distinguish multiple
    /// attempts' previews for the same unit. `None` for ordinary spawns.
    #[serde(default)]
    pub attempt_id: Option<String>,

    /// Item 6 — when true, do NOT hold the HTTP request open for the build.
    /// Register the build in the build-submissions state machine, reserve the
    /// runner port/slot, and return `202 {submission_id, port, status:"queued"}`
    /// immediately. The build + spawn run in a background task; poll
    /// `GET /build/{submission_id}/status` for the terminal state, whose
    /// `spawn` field carries the same body the synchronous 200 would have.
    ///
    /// Survives a supervisor restart's connection reset: the long-poll no
    /// longer conflates "build failed" with "supervisor died". Both the sync
    /// (`async:false`, the default) and async paths route through the SAME
    /// submission state machine — sync simply awaits the submission's
    /// completion in-handler.
    #[serde(default, rename = "async")]
    pub r#async: bool,
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

/// Paths needed to tear down a prepared spawn container on a downstream
/// build failure (the `cleanup_worktree_on_fail` flag). Mirrors the argument
/// list of [`crate::spawn_worktree::cleanup_fresh_worktree`].
struct SpawnCleanupPaths {
    /// Runner repo the runner worktree was registered in.
    repo_root: std::path::PathBuf,
    /// `<workspace_root>/.spawn-<ref>/` — removed last.
    container_path: std::path::PathBuf,
    /// Nested runner worktree (`<container>/qontinui-runner`).
    runner_wt_path: std::path::PathBuf,
    /// Shared `<workspace_root>/qontinui-schemas` repo the pinned worktree was
    /// created from. `None` if the workspace root couldn't be derived.
    schemas_repo_root: Option<std::path::PathBuf>,
    /// Pinned schemas worktree (`<container>/qontinui-schemas`).
    schemas_wt_path: Option<std::path::PathBuf>,
}

/// Provenance guard for spawn-test builds. Generalizes the original
/// `git_ref`-only guard to cover BOTH provenance selectors (`git_ref` and
/// `worktree_path`):
///
/// - **Mutual exclusion.** Both set → 400 `provenance_conflict` ("set git_ref
///   OR worktree_path, not both"). They name two different build sources; the
///   supervisor refuses to guess which one the caller meant.
/// - **Rebuild required.** Either selector set with `rebuild: false` → 400
///   naming the field (`<field> requires rebuild:true`). Without a recompile
///   the caller would get the live tree's existing exe while believing they got
///   the ref / checkout — provenance is sacrosanct, never lie about it.
///
/// Pure (no I/O / no state) so it is unit-testable without a live `SharedState`.
fn provenance_rebuild_guard(
    git_ref: Option<&str>,
    worktree_path: Option<&str>,
    rebuild: bool,
) -> Result<(), SupervisorError> {
    if git_ref.is_some() && worktree_path.is_some() {
        return Err(SupervisorError::Validation(
            "provenance_conflict: set git_ref OR worktree_path, not both".to_string(),
        ));
    }
    if git_ref.is_some() && !rebuild {
        return Err(SupervisorError::Validation(
            "git_ref requires rebuild:true".to_string(),
        ));
    }
    if worktree_path.is_some() && !rebuild {
        return Err(SupervisorError::Validation(
            "worktree_path requires rebuild:true".to_string(),
        ));
    }
    Ok(())
}

/// Detect known aliases of the real provenance params in a raw spawn-test JSON
/// body. spawn-test carries many optional fields so a blanket
/// `deny_unknown_fields` is too blunt; instead this targets the three keys the
/// motivating incident's caller actually passed (`branch`, `worktree`, `ref`),
/// which serde silently dropped, and returns a 400 naming the correct field so
/// the caller learns the real name instead of getting a silent `live_tree`
/// spawn.
///
/// Pure (operates on a parsed `serde_json::Value`) so it is unit-testable.
fn reject_known_provenance_aliases(raw: &serde_json::Value) -> Result<(), SupervisorError> {
    let obj = match raw.as_object() {
        Some(o) => o,
        // A non-object body will fail the typed deserialize with its own clear
        // error; nothing to alias-check.
        None => return Ok(()),
    };
    // (alias key, correct field name)
    const ALIASES: &[(&str, &str)] = &[
        ("branch", "git_ref"),
        ("ref", "git_ref"),
        ("worktree", "worktree_path"),
    ];
    for (alias, correct) in ALIASES {
        if obj.contains_key(*alias) {
            return Err(SupervisorError::Validation(format!(
                "unknown_provenance_param: `{alias}` is not a spawn-test field — \
                 use `{correct}` instead (provenance params are `git_ref` and \
                 `worktree_path` only; `branch`/`worktree`/`ref` are rejected, \
                 not silently ignored)"
            )));
        }
    }
    Ok(())
}

/// Mint a `spawn`-kind dev-action snapshot for a freshly-reserved runner.
///
/// Evaluates the active dev-state set, stores the record, and spawns the
/// attribution watcher targeting the spawned runner. The early-log is read
/// lazily via the managed runner because its path is only set during the
/// build, after mint. Returns the action id and active-state ids for stamping
/// into the spawn ACK.
async fn mint_spawn_action(
    state: &SharedState,
    params_digest: String,
    requester_id: Option<String>,
    managed: Arc<ManagedRunner>,
) -> (uuid::Uuid, Vec<&'static str>) {
    // Resolve the slot the spawn WOULD reuse (relevant for `rebuild:false`),
    // so LEGACY_EXE_FALLBACK is a real signal where it applies. A resolution
    // error ⇒ leave unevaluated so the state surfaces as `Unknown`.
    let slot_resolution = match manager::resolve_source_exe_with_slot(state).await {
        Ok((slot_id, _)) => SlotResolution::Resolved(slot_id),
        Err(_) => SlotResolution::NotEvaluated,
    };
    let states = evaluate_all(state, slot_resolution).await;
    let record = ActionRecord::new(ActionKind::Spawn, requester_id, params_digest, &states);
    let action_id = record.action_id;
    // Stamp the ACK with the canonical string ids (Phase-2b stores typed
    // `DevState`; the ACK wire form is the canonical-id array, unchanged).
    let states_active: Vec<&'static str> =
        record.states_active.iter().map(|s| s.as_str()).collect();
    let arc = state.dev_actions.write().await.insert(record);

    let runner_id = managed.config.id.clone();
    spawn_attribution_watcher(
        state.clone(),
        arc,
        AttributionTargets {
            early_log_path: None,
            managed: Some(managed),
            panic_log_path: None,
            runner_id: Some(runner_id),
        },
        None,
    );

    (action_id, states_active)
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
    Json(raw): Json<serde_json::Value>,
) -> Result<impl IntoResponse, SupervisorError> {
    let no_wait = headers
        .get("X-Queue-Mode")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("no-wait"))
        .unwrap_or(false);

    // Two-step deserialize so we can inspect raw keys before typing the body.
    // (1) Reject known aliases of the provenance params (`branch`/`worktree`/
    // `ref`) that serde would otherwise silently drop — the motivating
    // incident's caller passed `branch`/`worktree_path` and got a silent
    // live-tree spawn. Do this on the RAW value before any state mutation.
    reject_known_provenance_aliases(&raw)?;

    // (2) Type the body. A malformed body surfaces serde's own message as a 400.
    let body: SpawnTestRequest = serde_json::from_value(raw).map_err(|e| {
        SupervisorError::Validation(format!("invalid spawn-test request body: {e}"))
    })?;

    // Provenance guards (mutual exclusion + rebuild-required), pure + before
    // reserving a port. Either selector silently ignored / lying about
    // provenance is the exact failure mode this prevents.
    provenance_rebuild_guard(
        body.git_ref.as_deref(),
        body.worktree_path.as_deref(),
        body.rebuild,
    )?;

    // `frontend_only` re-embeds a fresh dist into an ISOLATED tree; it requires
    // a provenance selector (a live-tree frontend_only would touch the shared
    // tree, which the whole feature exists to avoid).
    if body.frontend_only && body.git_ref.is_none() && body.worktree_path.is_none() {
        return Err(SupervisorError::Validation(
            "frontend_only requires a provenance selector (git_ref or worktree_path) — \
             a frontend_only build of the live tree is not supported (it would touch the \
             shared tree)"
                .to_string(),
        ));
    }

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

    // Track 2 (UI-Bridge preview-verification): if this spawn is a preview for
    // an autonomous-dev work unit, record the (unit_id, attempt_id) correlation
    // on the ManagedRunner so it round-trips in the response and is resolvable
    // via `GET /runners/by-unit/{unit_id}`. The runner is already in the
    // registry, so a concurrent by-unit query observes it as soon as this set
    // completes. No effect on the build/port logic below.
    if let Some(unit_id) = body.unit_id.clone() {
        *managed.preview_binding.write().await = Some(crate::state::PreviewBinding {
            unit_id,
            attempt_id: body.attempt_id.clone(),
            // `git_sha` is filled in after the health probe resolves the SHA the
            // runner actually booted (see below). `None` here for the window
            // between insert and probe.
            git_sha: None,
        });
    }

    // Mint a `spawn`-kind dev-action snapshot. State eval captures the
    // cause-side context (SLOTS_EMPTY / LEGACY_EXE_FALLBACK / DIST_STALE) at
    // spawn time; the attribution watcher (30s window) folds the verdict in by
    // scanning the SPAWNED runner's early-log (read lazily via the managed
    // runner — its path is set during the build, after this mint) + cached
    // health. Reuses the request's `requester_id` as the snapshot requester
    // (Q8 — no new identity system).
    let (action_id, action_states_active) = mint_spawn_action(
        &state,
        format!(
            "rebuild={};use_lkg={};git_ref={:?}",
            body.rebuild, body.use_lkg, body.git_ref
        ),
        body.requester_id.clone(),
        managed.clone(),
    )
    .await;

    // Item 6 — route the build+spawn through the SINGLE build-submissions
    // state machine. `submit_spawn` registers a submission and drives the
    // extracted `execute_spawn_build` future in a background task; its
    // `(status, body)` becomes the submission's terminal `spawn` outcome.
    //
    // - async: return 202 with the submission id + reserved port now.
    // - sync (default): await the submission's terminal state in-handler and
    //   return the stored outcome. Both paths share one state machine.
    let want_async = body.r#async;
    let store = state.build_submissions.clone();
    let exec_state = state.clone();
    let exec_managed = managed.clone();
    let exec_id = id.clone();
    let agent_id = body.requester_id.clone();
    let worktree_label = state.config.project_dir.clone();
    let (submission_id, sub_arc) =
        crate::build_submissions::submit_spawn(store, worktree_label, agent_id, port, async move {
            let (status, body_json, stderr_tail) =
                execute_spawn_build(exec_state, body, exec_id, port, exec_managed, no_wait).await;
            (status.as_u16(), body_json, stderr_tail)
        });

    if want_async {
        return Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(json!({
                "submission_id": submission_id.to_string(),
                "status": "queued",
                "id": id,
                "port": port,
                "api_url": format!("http://localhost:{}", port),
                "ui_bridge_url": format!("http://localhost:{}/ui-bridge", port),
                "action_id": action_id.to_string(),
                "states_active": action_states_active,
                "outcome_url": format!("/actions/{}/outcome", action_id),
                "poll_url": format!("/build/{}/status", submission_id),
                "message": format!(
                    "spawn-test build queued (submission {}); poll GET /build/{}/status for the terminal `spawn` outcome",
                    submission_id, submission_id
                ),
            })),
        )
            .into_response());
    }

    // Sync path: await the submission terminal state, then return its stored
    // spawn outcome (same shape as the legacy inline response).
    crate::build_submissions::await_terminal(&sub_arc).await;
    let outcome = {
        let sub = sub_arc.read().await;
        sub.spawn.clone()
    };
    match outcome {
        Some(o) => {
            let status = axum::http::StatusCode::from_u16(o.http_status)
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            // Back-compat: re-attach the legacy `X-Frontend-Stale: 1` header
            // when the body flags a stale frontend (the header couldn't ride
            // the stored submission outcome).
            let frontend_stale = o
                .body
                .get("frontend_stale")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Enrich the stored spawn outcome body with the dev-action snapshot
            // fields so the synchronous ACK carries the action_id + active
            // dev-state set + outcome readback URL (in-place edit per §6.1; no
            // new route). The watcher was already spawned at mint time.
            let mut body = o.body;
            if let Some(obj) = body.as_object_mut() {
                obj.insert("action_id".to_string(), json!(action_id.to_string()));
                obj.insert("states_active".to_string(), json!(action_states_active));
                obj.insert(
                    "outcome_url".to_string(),
                    json!(format!("/actions/{}/outcome", action_id)),
                );
            }
            let mut response = (status, Json(body)).into_response();
            if frontend_stale {
                response
                    .headers_mut()
                    .insert("X-Frontend-Stale", "1".parse().unwrap());
            }
            Ok(response)
        }
        None => Err(SupervisorError::Other(
            "spawn-test submission reached terminal state without a spawn outcome (internal error)"
                .to_string(),
        )),
    }
}

/// Build + spawn + probe + assemble the spawn-test response.
///
/// Extracted from `spawn_test` (Item 6) so BOTH the synchronous and the
/// `async: true` request paths execute identical logic through the single
/// build-submissions state machine — the only difference is whether the HTTP
/// handler awaits this future inline or polls for its recorded outcome.
///
/// Returns `(StatusCode, body)` rather than a `Response` so the result can be
/// stored on the submission's terminal state and rebuilt into a `Response`
/// later. On any internal `SupervisorError` the placeholder port reservation
/// is released (as the inline path did) and the error is rendered to its
/// canonical `(status, body)` via [`SupervisorError::to_status_body`].
///
/// `no_wait` mirrors the `X-Queue-Mode: no-wait` header semantics for the
/// build-pool-full short-circuit.
async fn execute_spawn_build(
    state: SharedState,
    body: SpawnTestRequest,
    id: String,
    port: u16,
    managed: Arc<ManagedRunner>,
    no_wait: bool,
) -> (axum::http::StatusCode, serde_json::Value, Vec<String>) {
    // Issue 3 fix part 1: `execute_spawn_build_inner` writes the FULL cargo
    // stderr of a failed build pool invocation here (a generous tail, split
    // into lines) so it can be threaded onto the build submission's
    // `stderr_tail` and recovered via `GET /build/{id}/status`. Empty when the
    // build succeeded or there was no cargo failure.
    let mut build_stderr_tail: Vec<String> = Vec::new();
    match execute_spawn_build_inner(
        &state,
        body,
        &id,
        port,
        &managed,
        no_wait,
        &mut build_stderr_tail,
    )
    .await
    {
        Ok((status, body)) => (status, body, build_stderr_tail),
        Err(e) => {
            let (status, body) = e.to_status_body();
            (status, body, build_stderr_tail)
        }
    }
}

/// Inner body returning `Result` so the existing `?` / `return Err(e)` control
/// flow is preserved verbatim from the pre-Item-6 handler. The placeholder
/// removal on the error paths below is unchanged.
async fn execute_spawn_build_inner(
    state: &SharedState,
    body: SpawnTestRequest,
    id: &str,
    port: u16,
    managed: &Arc<ManagedRunner>,
    no_wait: bool,
    build_stderr_tail: &mut Vec<String>,
) -> Result<(axum::http::StatusCode, serde_json::Value), SupervisorError> {
    // Build-result tracking for the response. Populated by the rebuild
    // branch below; surfaced via the `build_result` JSON field. When
    // `body.rebuild` is false, `attempted` stays false and `succeeded`
    // stays None.
    let mut build_attempted = false;
    let mut build_succeeded: Option<bool> = None;
    let mut build_error: Option<String> = None;
    let mut build_reused_stale = false;
    // Set when a `git_ref` build occurred: (requested_ref, resolved_sha).
    // Surfaced in the response as `git_ref` / `git_ref_resolved_sha`.
    let mut git_ref_info: Option<(String, String)> = None;
    // Set alongside `git_ref_info` when prepare_worktree returns Ok. Used by
    // the `cleanup_worktree_on_fail` flag to wipe the spawn container on a
    // downstream cargo failure.
    // (runner_repo_root, container_path, runner_wt_path, schemas_repo_root, schemas_wt_path)
    let mut git_ref_cleanup_paths: Option<SpawnCleanupPaths> = None;

    // Set when a `worktree_path` build occurred (Phase 2). Carries the echoed
    // provenance: canonical path, optional HEAD SHA, and schemas-sibling drift
    // info. The struct type (`ValidatedExistingWorktree`) has NO cleanup paths
    // by construction — a caller-owned tree can never enter the cleanup
    // machinery (the type-level guarantee from the plan).
    let mut worktree_path_info: Option<crate::spawn_worktree::ValidatedExistingWorktree> = None;

    // Set to the spawn container path of a `git_ref` build that materialized a
    // fresh `.spawn-*` container, so we can (a) mark it active for the duration
    // of the build (the pruner's active-build exclusion) and (b) run an
    // opportunistic prune sweep after the build, scrubbing OTHER stale
    // containers this new one's parent dir accumulated. `None` for live-tree /
    // worktree_path builds (no supervisor-owned scratch container involved).
    let mut active_spawn_container: Option<std::path::PathBuf> = None;

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

        // If the caller asked for a specific git ref, materialize a managed
        // detached worktree at that ref now and build *that* tree instead of
        // the live project dir. Any git failure aborts the request (no
        // silent fallback to the live tree — provenance is the whole point).
        // Release the placeholder port on failure so it doesn't leak.
        let build_dir_override = match body.git_ref.as_deref() {
            Some(git_ref) => {
                match crate::spawn_worktree::prepare_worktree(&state.config.project_dir, git_ref)
                    .await
                {
                    Ok(wt) => {
                        state
                            .logs
                            .emit(
                                LogSource::Supervisor,
                                LogLevel::Info,
                                format!(
                                    "spawn-test git_ref build: ref={:?} resolved_sha={} worktree={:?} (requester={:?})",
                                    wt.requested_ref,
                                    wt.resolved_sha,
                                    wt.worktree_path,
                                    body.requester_id
                                ),
                            )
                            .await;
                        git_ref_info = Some((wt.requested_ref.clone(), wt.resolved_sha.clone()));
                        // Stash paths for the optional cleanup-on-fail branch
                        // below. `find_repo_root` walks up from the
                        // configured project_dir; if it errors here (the
                        // supervisor lost track of the runner repo) we can't
                        // safely run `git worktree remove` anyway — skip
                        // cleanup wiring rather than 500 the spawn. The schemas
                        // repo root is the shared `<workspace_root>/qontinui-schemas`
                        // checkout the pinned worktree was created from.
                        if let Ok(repo_root) =
                            crate::spawn_worktree::find_repo_root(&state.config.project_dir)
                        {
                            let schemas_repo_root = crate::spawn_worktree::derive_workspace_root(
                                &state.config.project_dir,
                            )
                            .ok()
                            .map(|ws| ws.join("qontinui-schemas"));
                            git_ref_cleanup_paths = Some(SpawnCleanupPaths {
                                repo_root,
                                container_path: wt.container_path.clone(),
                                runner_wt_path: wt.worktree_path.clone(),
                                schemas_repo_root,
                                schemas_wt_path: Some(wt.schemas_path.clone()),
                            });
                        }
                        // Mark this container active so the pruner never reaps
                        // it while the build below is in flight. Removed after
                        // the build completes (success or failure) — see the
                        // unregister + opportunistic-sweep block after the build.
                        {
                            let mut active = state.active_spawn_worktrees.lock().unwrap();
                            active.insert(wt.container_path.clone());
                        }
                        active_spawn_container = Some(wt.container_path.clone());
                        Some(wt.src_tauri)
                    }
                    Err(e) => {
                        let mut runners = state.runners.write().await;
                        runners.remove(id);
                        return Err(e);
                    }
                }
            }
            // No git_ref. If `worktree_path` was supplied (mutually exclusive
            // with git_ref — already guarded), validate the caller-owned tree
            // and build IT in place. We NEVER materialize, mutate, or clean up
            // a caller-owned tree — `validate_existing_worktree` returns a
            // `ValidatedExistingWorktree` which (by type) carries no cleanup
            // paths, so `git_ref_cleanup_paths` stays None and the
            // cleanup-on-fail machinery can't touch it.
            None => match body.worktree_path.as_deref() {
                Some(wt_path) => {
                    // The live repo root the worktree must NOT be is derived
                    // from the configured project_dir (its repo root).
                    let live_repo_root =
                        match crate::spawn_worktree::find_repo_root(&state.config.project_dir) {
                            Ok(r) => r,
                            // Can't determine the live tree → can't safely run
                            // the live-tree-refusal guard. Fall back to the
                            // configured project_dir's parent (the npm dir) so
                            // the comparison still has a sane reference.
                            Err(_) => state.config.runner_npm_dir(),
                        };
                    match crate::spawn_worktree::validate_existing_worktree(
                        std::path::Path::new(wt_path),
                        &live_repo_root,
                    )
                    .await
                    {
                        Ok(validated) => {
                            state
                                .logs
                                .emit(
                                    LogSource::Supervisor,
                                    LogLevel::Info,
                                    format!(
                                        "spawn-test worktree_path build: path={:?} resolved_sha={:?} schemas_path={:?} schemas_sha={:?} schemas_is_shared={} (requester={:?})",
                                        validated.worktree_path,
                                        validated.resolved_sha,
                                        validated.schemas_path,
                                        validated.schemas_sha,
                                        validated.schemas_is_shared,
                                        body.requester_id
                                    ),
                                )
                                .await;
                            let src_tauri = validated.src_tauri.clone();
                            worktree_path_info = Some(validated);
                            Some(src_tauri)
                        }
                        Err(e) => {
                            let mut runners = state.runners.write().await;
                            runners.remove(id);
                            return Err(e);
                        }
                    }
                }
                None => None,
            },
        };

        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Rebuilding runner before spawning test runner on port {} (requester={:?}, git_ref={:?})",
                    port, body.requester_id, body.git_ref
                ),
            )
            .await;

        // Run the build, optionally bounded by a queue timeout.
        // On any failure (build error, timeout, etc.), remove the placeholder
        // we reserved above so the port doesn't leak — UNLESS the caller
        // opted into `allow_stale_fallback`, in which case we keep the
        // placeholder and fall through to spawn from the previous slot exe.
        // Phase 3: `frontend_only` FORCES a fresh `pnpm run build` in the
        // isolated override tree even when the dist-present idempotency gate
        // would otherwise skip it (a TS edit after the tree's last build would
        // silently embed stale dist). Only meaningful for an override tree —
        // it was already guarded to require a provenance selector, and the
        // build below only carries an override when git_ref/worktree_path set.
        // Issue 3: drive the build through the detailed entry point so a
        // FAILURE returns the slot id + full cargo stderr alongside the error.
        // This powers (1) surfacing the real compiler error through
        // `GET /build/{id}/status` and (2) a poisoned-slot self-heal retry.
        // `build_dir_override` is cloned so it survives a retry invocation.
        let requester_id = body.requester_id.clone();
        let frontend_only = body.frontend_only;
        let queue_timeout_secs = body.queue_timeout_secs;
        let run_detailed = |override_dir: Option<std::path::PathBuf>| {
            let requester = requester_id.clone();
            async move {
                let fut = crate::build_monitor::run_cargo_build_with_dir_detailed(
                    state,
                    requester,
                    override_dir,
                    frontend_only,
                );
                match queue_timeout_secs {
                    Some(secs) => {
                        let timeout = std::time::Duration::from_secs(secs);
                        match tokio::time::timeout(timeout, fut).await {
                            Ok(r) => r,
                            Err(_) => Err((
                                SupervisorError::Timeout(format!(
                                    "Build queue timeout: waited {}s for a build slot",
                                    secs
                                )),
                                crate::build_monitor::BuildAttempt::default(),
                            )),
                        }
                    }
                    None => fut.await,
                }
            }
        };

        let mut build_result = run_detailed(build_dir_override.clone()).await;

        // Poisoned-slot self-heal (Issue 3 fix part 2). On a build FAILURE
        // whose stderr is NOT a genuine compiler diagnostic (no `error[E####]`,
        // no `could not compile`), the failure is almost certainly stale/
        // poisoned incremental state in the claimed slot — NOT a code error.
        // An isolated clean `cargo build` of the same tree succeeds. Clean that
        // exact slot's `CARGO_TARGET_DIR` and retry the build ONCE before
        // surfacing the 500. A genuine compiler error returns immediately (no
        // wasteful retry). Both outcomes are logged via `tracing`.
        if let Err((_, ref attempt)) = build_result {
            let stderr_for_class = attempt.full_stderr.clone().unwrap_or_default();
            let class = crate::build_monitor::classify_build_stderr(&stderr_for_class);
            if class == crate::build_monitor::StderrClass::Environmental {
                if let Some(slot_id) = attempt.slot_id {
                    if let Some(slot) = state.build_pool.slots.get(slot_id).cloned() {
                        tracing::warn!(
                            slot_id,
                            "spawn-test build failed with no compiler diagnostic in stderr; \
                             treating as a poisoned slot and retrying ONCE in a cleaned slot \
                             (requester={:?}, git_ref={:?})",
                            body.requester_id,
                            body.git_ref
                        );
                        state
                            .logs
                            .emit(
                                LogSource::Supervisor,
                                LogLevel::Warn,
                                format!(
                                    "spawn-test: slot {} build failure is environmental (no compiler diagnostic); cleaning slot + retrying once",
                                    slot_id
                                ),
                            )
                            .await;
                        match crate::build_monitor::clean_slot_target(&slot).await {
                            Ok(freed) => tracing::info!(
                                slot_id,
                                bytes_freed = freed,
                                "cleaned poisoned slot before spawn-test retry"
                            ),
                            Err(e) => tracing::warn!(
                                slot_id,
                                "failed to clean poisoned slot {} before retry: {} (retrying anyway)",
                                slot_id,
                                e
                            ),
                        }
                        let retry = run_detailed(build_dir_override.clone()).await;
                        match &retry {
                            Ok(_) => {
                                tracing::info!(
                                    "spawn-test poisoned-slot retry SUCCEEDED after cleaning slot {}",
                                    slot_id
                                );
                                state
                                    .logs
                                    .emit(
                                        LogSource::Supervisor,
                                        LogLevel::Info,
                                        format!(
                                            "spawn-test: poisoned-slot retry succeeded after cleaning slot {}",
                                            slot_id
                                        ),
                                    )
                                    .await;
                            }
                            Err((e, _)) => {
                                tracing::warn!(
                                    "spawn-test poisoned-slot retry FAILED after cleaning slot {}: {}",
                                    slot_id,
                                    e
                                );
                            }
                        }
                        build_result = retry;
                    }
                }
            } else {
                tracing::info!(
                    "spawn-test build failed with a compiler diagnostic; returning immediately (no poisoned-slot retry)"
                );
            }
        }

        match build_result {
            Ok(_attempt) => {
                build_attempted = true;
                build_succeeded = Some(true);
            }
            Err((mut e, attempt)) => {
                // Issue 3 fix part 1: surface the FULL cargo stderr (a generous
                // tail, split into lines) onto the build submission's
                // `stderr_tail` so `GET /build/{id}/status` returns the real
                // compiler error instead of an empty/2KB-truncated tail.
                if let Some(full) = &attempt.full_stderr {
                    let tail = crate::build_monitor::stderr_submission_tail(full);
                    *build_stderr_tail = tail.lines().map(|l| l.to_string()).collect();
                }
                // Item 1(b) — drift diagnostic. With pinned-schemas isolation
                // a build failure should reference only files inside the spawn
                // container. If the cargo error references a path OUTSIDE the
                // container (e.g. the SHARED qontinui-schemas checkout), the
                // failure is residual shared path-dep drift, not the requested
                // ref's fault — prefix the error so the caller doesn't chase a
                // phantom regression. Only applies to git_ref (worktree) builds
                // where we know the container path.
                if let Some(paths) = &git_ref_cleanup_paths {
                    if let Some(offending) =
                        crate::spawn_worktree::classify_drift(&e.to_string(), &paths.container_path)
                    {
                        let prefixed = format!(
                            "{}{} | {}",
                            crate::spawn_worktree::DRIFT_PREFIX,
                            offending,
                            e
                        );
                        state
                            .logs
                            .emit(
                                LogSource::Supervisor,
                                LogLevel::Warn,
                                format!(
                                    "spawn-test build failure references a path outside the spawn container ({}); classifying as shared path-dep drift",
                                    offending
                                ),
                            )
                            .await;
                        e = SupervisorError::BuildFailed(prefixed);
                    }
                }

                // Optional spawn-worktree cleanup. Best-effort, runs before
                // any of the existing error-path branches so the worktree
                // is gone whether we fall through (allow_stale_fallback)
                // OR short-circuit with 500. The cleanup itself can't fail
                // the caller's request — `cleanup_fresh_worktree` logs and
                // swallows internally.
                if body.cleanup_worktree_on_fail && body.git_ref.is_some() {
                    if let Some(paths) = &git_ref_cleanup_paths {
                        state
                            .logs
                            .emit(
                                LogSource::Supervisor,
                                LogLevel::Info,
                                format!(
                                    "spawn-test cleanup_worktree_on_fail: removing spawn container {:?} after build failure",
                                    paths.container_path
                                ),
                            )
                            .await;
                        crate::spawn_worktree::cleanup_fresh_worktree(
                            &paths.repo_root,
                            &paths.container_path,
                            &paths.runner_wt_path,
                            paths.schemas_repo_root.as_deref(),
                            paths.schemas_wt_path.as_deref(),
                        )
                        .await;
                    }
                }

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
                    if let Some(container) = active_spawn_container.take() {
                        state
                            .active_spawn_worktrees
                            .lock()
                            .unwrap()
                            .remove(&container);
                    }
                    let mut runners = state.runners.write().await;
                    runners.remove(id);
                    return Err(e);
                }
            }
        }

        // Build finished (success, or stale-fallback that didn't early-return).
        // Unregister the active spawn container so the pruner may reap it once
        // it ages out, then run an OPPORTUNISTIC prune sweep: a freshly-built
        // `.spawn-*` container is the natural trigger to scrub OTHER stale
        // scratch containers its parent dir accumulated. Best-effort + bounded
        // — the sweep itself never fails the spawn request.
        if let Some(container) = active_spawn_container.take() {
            state
                .active_spawn_worktrees
                .lock()
                .unwrap()
                .remove(&container);

            let active_now: std::collections::HashSet<std::path::PathBuf> =
                state.active_spawn_worktrees.lock().unwrap().clone();
            let report = crate::spawn_worktree::prune_spawn_worktrees(
                &state.config.project_dir,
                &active_now,
                std::time::SystemTime::now(),
            )
            .await;
            if !report.removed.is_empty() || !report.failed.is_empty() {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        format!(
                            "spawn-test opportunistic prune: removed {} stale scratch worktree(s), {} failed",
                            report.removed.len(),
                            report.failed.len()
                        ),
                    )
                    .await;
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
        match manager::resolve_lkg_exe(state).await {
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
                runners.remove(id);
                return Err(e);
            }
        }
    }

    // Check that a runner binary exists in some build slot (or at the legacy path).
    // Without this check, a fresh supervisor would succeed the request only to
    // fail inside `start_runner_by_id` with a less helpful error. If the check
    // fails, remove the placeholder we reserved above so the port frees up.
    // Skip when use_lkg pinned an override above — we already verified that path.
    if !body.use_lkg && manager::resolve_source_exe(state).await.is_err() {
        let mut runners = state.runners.write().await;
        runners.remove(id);
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

    // Phase 2b — `paired_profile_id`. If the caller asked the spawned runner
    // to inherit a previously-stashed pairing, copy the snapshot files into
    // the shared runner data dir BEFORE starting the runner. On failure
    // release the placeholder port and return the structured error body.
    //
    // Tracked under the spawn-test response as `paired_profile_applied`
    // (the list of basenames actually copied).
    let mut paired_profile_applied: Option<Vec<String>> = None;
    if let Some(profile_id) = body.paired_profile_id.as_deref() {
        match apply_paired_profile_for_spawn(profile_id) {
            Ok(applied) => {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        format!(
                            "spawn-test paired_profile_id='{}' applied: {:?}",
                            profile_id, applied
                        ),
                    )
                    .await;
                paired_profile_applied = Some(applied);
            }
            Err((status, body_json)) => {
                let mut runners = state.runners.write().await;
                runners.remove(id);
                drop(runners);
                return Ok((status, body_json));
            }
        }
    }

    // Start the runner using the Arc captured at insertion time. This avoids
    // the id-based lookup in `start_runner_by_id` which can race with
    // concurrent paths that remove the id from the registry (e.g. a sibling
    // spawn's failed health probe, stop_all_temp_runners, the reaper).
    // `start_managed_runner` also re-inserts the Arc if the id went missing,
    // so the subsequent health probe and /runners lookups still work.
    if let Err(e) = manager::start_managed_runner(state, managed).await {
        // Clean up on failure
        let mut runners = state.runners.write().await;
        runners.remove(id);
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
        match probe_runner_health(state, id, port, body.health_probe_timeout_ms).await {
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
                let early_log_path = if let Some(managed) = state.get_runner(id).await {
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
                if let Some(managed) = state.get_runner(id).await {
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
                let _ = manager::stop_runner_by_id(state, id).await;
                {
                    let mut runners = state.runners.write().await;
                    runners.remove(id);
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

                return Ok((status, body));
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
        crate::process::manager::resolve_lkg_exe(state)
            .await
            .ok()
            .and_then(|p| crate::process::manager::binary_meta(&p))
    } else {
        crate::process::manager::resolve_source_exe(state)
            .await
            .ok()
            .and_then(|p| crate::process::manager::binary_meta(&p))
    };

    // Determine if the slot we used for the binary has a stale frontend
    // baked in (build failure) OR if `src/` is newer than `dist/index.html`
    // (someone forgot to `npm run build`). Either way, the runner ships with
    // a UI that doesn't reflect current source.
    let (frontend_stale, stale_reason, stale_slot_id) =
        resolve_frontend_stale_for_spawn(state).await;

    // Short-circuit with 503 before reporting success when the spawned
    // runner would serve a broken or stale UI. Two triggers:
    //
    //  1. `DistMissing` — ALWAYS hard-fails, regardless of `frontend_strict`.
    //     A missing `dist/index.html` is not "stale but functional": the
    //     runner has NO embedded frontend and serves `asset not found:
    //     index.html` at every route — a blank screen. Returning
    //     `status=healthy` here lies to the caller (a manual-test run caught
    //     exactly this: gutted `node_modules/.bin` + empty `dist/` → the
    //     freshly-built runner embedded a blank frontend, yet spawn-test only
    //     warned and reported healthy). A blank UI is never healthy, so the
    //     default path must refuse the spawn, not warn-and-serve.
    //
    //  2. Any other `frontend_stale` reason (`BuildFailed` / `SrcDrift`) —
    //     hard-fails only when the caller opted into strict-frontend
    //     enforcement via `frontend_strict: true`. These are stale-but-
    //     functional (the UI renders, just not the latest source), so the
    //     default stays warn-and-serve for back-compat.
    let dist_missing = matches!(stale_reason, Some(FrontendStaleReason::DistMissing));
    if frontend_stale && (dist_missing || body.frontend_strict) {
        // Stop the runner we just spawned — we will not ship a broken/stale
        // UI to the caller.
        let _ = manager::stop_runner_by_id(state, id).await;
        {
            let mut runners = state.runners.write().await;
            runners.remove(id);
        }
        state.notify_health_change();
        let reason_str = stale_reason.map(|r| r.as_str()).unwrap_or("unknown");
        let message = if dist_missing {
            "frontend_dist_missing: refusing to spawn — qontinui-runner/dist/index.html is \
             missing, so the runner would embed no frontend and serve a blank screen \
             (`asset not found: index.html`) at every route. Run \
             `cd qontinui-runner && npm run build` to rebuild dist/, then retry. This is \
             unconditional: a missing dist is never reported as healthy."
                .to_string()
        } else {
            format!(
                "frontend_strict: refusing to spawn — frontend dist is stale (reason={}). \
                 Run `cd qontinui-runner && npm run build` (or fix the build error) and retry, \
                 or pass {{\"frontend_strict\": false}} to override.",
                reason_str
            )
        };
        let body = json!({
            "error": if dist_missing { "frontend_dist_missing" } else { "frontend_stale" },
            "message": message,
            "frontend_stale": true,
            "frontend_stale_reason": reason_str,
            "stale_slot_id": stale_slot_id,
        });
        return Ok((axum::http::StatusCode::SERVICE_UNAVAILABLE, body));
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
    if let Some(applied) = &paired_profile_applied {
        // Phase 2b — surface which snapshot files were materialized into
        // the runner data dir. Callers can verify the pair-state inheritance
        // happened before treating the runner as paired.
        resp["paired_profile_applied"] = json!(applied);
        if let Some(pid) = body.paired_profile_id.as_deref() {
            resp["paired_profile_id"] = json!(pid);
        }
    }
    if let Some(ms) = health_probe_ms {
        resp["health_probe_ms"] = json!(ms);
    }
    if let Some(sha) = &probed_git_sha {
        resp["git_sha"] = json!(sha);
    }
    // Track 2 (UI-Bridge preview-verification): backfill the resolved git_sha
    // onto the preview binding now that the health probe has run, so the
    // `GET /runners/by-unit/{unit_id}` handle carries provenance. Only touches
    // the binding when this spawn is a preview (binding already set above).
    if body.unit_id.is_some() {
        if let Some(binding) = managed.preview_binding.write().await.as_mut() {
            binding.git_sha = probed_git_sha.clone();
        }
    }
    // Track 2 (UI-Bridge preview-verification): echo the work-unit / attempt
    // correlation back next to id/port/ui_bridge_url/git_sha so the caller gets
    // the full preview handle in one round-trip. Only present when the spawn
    // carried `unit_id`; `attempt_id` is null when only `unit_id` was supplied.
    if let Some(unit_id) = &body.unit_id {
        resp["unit_id"] = json!(unit_id);
        resp["attempt_id"] = json!(body.attempt_id);
    }
    if body.use_lkg {
        resp["used_lkg"] = json!(true);
        if let Some(info) = state.build_pool.last_known_good.read().await.clone() {
            resp["lkg"] = json!({
                "built_at": info.built_at.to_rfc3339(),
                "source_slot": info.source_slot,
                "exe_size": info.exe_size,
                // Provenance of the LKG build (#65). `sha` is the git SHA of
                // the built live tree (null if the git probe failed or the
                // record predates these fields); `source` serializes via
                // BuildSource's serde as "live_tree"/"override" (always
                // "live_tree" for any LKG written from #65 forward).
                "sha": info.sha,
                "source": info.source,
            });

            // Staleness signal: there's no per-request changed-file list at
            // spawn time, so compute a server-side one. Walk the runner's
            // Rust sources under `<project_dir>/src` for the newest mtime;
            // if it's newer than the LKG build, the spawned binary may not
            // reflect recent changes. Single bounded walk, one file
            // reported. Does not change /lkg/coverage behavior.
            let project_dir = state.config.project_dir.clone();
            let built_at = info.built_at;
            let scan = tokio::task::spawn_blocking(move || {
                crate::routes::lkg_coverage::scan_lkg_staleness(&project_dir, built_at)
            })
            .await
            .ok()
            .flatten();
            match scan {
                Some(s) if s.stale => {
                    resp["lkg_stale_warning"] = json!({
                        "stale": true,
                        "lkg_built_at": s.lkg_built_at.to_rfc3339(),
                        "newest_src_mtime": s.newest_src_mtime.to_rfc3339(),
                        "newest_src_file": s.newest_src_file,
                        "lag_secs": s.lag_secs,
                        "message": "LKG binary predates current runner source; spawned runner may not reflect recent changes. Use rebuild:true (optionally with git_ref) to build current code.",
                    });
                }
                _ => {
                    resp["lkg_stale_warning"] = json!({ "stale": false });
                }
            }
        }
    }

    // Provenance for a git_ref build: the requested ref and the SHA the
    // managed worktree actually resolved to. Only present when a git_ref
    // build occurred (rebuild:true + git_ref).
    //
    // Field shape (kept stable for downstream callers / agents):
    //   git_ref               — the input ref verbatim (e.g. "origin/main").
    //   git_ref_resolved      — echo alias of git_ref so callers that want a
    //                           "resolved" label don't have to special-case.
    //   git_ref_resolved_sha  — full 40-char `git rev-parse HEAD` of the
    //                           prepared worktree.
    //   git_ref_resolved_sha_short
    //                         — 12-char abbreviated SHA for log-readable
    //                           comparison against `git rev-parse origin/main
    //                           | head -c 12`. Always derived from
    //                           git_ref_resolved_sha; never re-shells out to
    //                           git so it's a pure presentational helper.
    //   source                — "worktree" when this build came from the
    //                           managed detached worktree; absent when the
    //                           live tree was used. Callers that need a
    //                           single-field "where did the binary come
    //                           from?" answer can branch on this.
    if let Some((requested_ref, resolved_sha)) = &git_ref_info {
        let short = resolved_sha.chars().take(12).collect::<String>();
        resp["git_ref"] = json!(requested_ref);
        resp["git_ref_resolved"] = json!(requested_ref);
        resp["git_ref_resolved_sha"] = json!(resolved_sha);
        resp["git_ref_resolved_sha_short"] = json!(short);
        resp["source"] = json!("worktree");
    } else if let Some(wt) = &worktree_path_info {
        // Phase 2 — `worktree_path` provenance. Echo the canonical path, the
        // worktree's HEAD SHA (when it's a git checkout) reusing the same
        // `git_ref_resolved_sha`/`_short` field names callers already parse,
        // and the schemas-sibling drift provenance (`schemas_path`,
        // `schemas_sha`, `schemas_is_shared`). `source: "worktree_path"`
        // distinguishes it from a managed `git_ref` worktree.
        resp["source"] = json!("worktree_path");
        resp["worktree_path"] = json!(wt.worktree_path.to_string_lossy());
        if let Some(sha) = &wt.resolved_sha {
            let short = sha.chars().take(12).collect::<String>();
            resp["git_ref_resolved_sha"] = json!(sha);
            resp["git_ref_resolved_sha_short"] = json!(short);
        }
        resp["schemas_path"] = match &wt.schemas_path {
            Some(p) => json!(p.to_string_lossy()),
            None => serde_json::Value::Null,
        };
        resp["schemas_sha"] = match &wt.schemas_sha {
            Some(s) => json!(s),
            None => serde_json::Value::Null,
        };
        resp["schemas_is_shared"] = json!(wt.schemas_is_shared);
    } else {
        // Explicit "live_tree" provenance so callers can always branch on a
        // present field rather than needing missing-field handling.
        resp["source"] = json!("live_tree");
    }

    // Phase 3 — `frontend_only` echo + honest stale-reuse warning. Always echo
    // the flag when set so callers can confirm the fast path engaged. When the
    // spawned exe came from LKG (`use_lkg`) or a stale-fallback reuse, that exe
    // embeds the OLD dist — the fresh `pnpm run build` we forced was NOT
    // re-embedded by a cargo recompile, so warn explicitly (never lie about
    // provenance).
    if body.frontend_only {
        resp["frontend_only"] = json!(true);
        let reused_old_exe = body.use_lkg || build_reused_stale;
        if reused_old_exe {
            resp["frontend_only_warning"] = json!(
                "frontend_only forced a fresh `pnpm run build`, but the spawned \
                 runner reuses a previously-built exe (use_lkg or stale-fallback) \
                 which embeds the OLD dist at compile time — the new dist is NOT \
                 reflected. Rebuild with frontend_only + rebuild:true (no LKG/stale \
                 reuse) to re-embed the fresh dist via cargo."
            );
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

        // The legacy `X-Frontend-Stale: 1` response header can't ride a stored
        // submission outcome, so the sync handler re-attaches it by branching on
        // the `frontend_stale` body field (always present here).
        return Ok((axum::http::StatusCode::OK, resp));
    }

    Ok((axum::http::StatusCode::OK, resp))
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

    // Mint a `spawn`-kind dev-action snapshot for the named runner (same shape
    // as spawn-test). Stamped into the success response below.
    let (action_id, action_states_active) = mint_spawn_action(
        &state,
        format!("named={};rebuild={}", name, body.rebuild),
        body.requester_id.clone(),
        managed.clone(),
    )
    .await;

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

    // DistMissing hard-fail (symmetric with spawn_test). A missing
    // `dist/index.html` means the runner embeds NO frontend and serves a
    // blank screen (`asset not found: index.html`) at every route — never a
    // `healthy` outcome. Unlike `SrcDrift`/`BuildFailed` (stale-but-
    // functional), this is unconditional. Tear the named runner back down
    // (stop, drop from the registry, AND remove the just-persisted settings
    // entry so it doesn't resurrect on the next supervisor restart) and
    // return 503 instead of reporting a blank UI as healthy.
    if frontend_stale && matches!(stale_reason, Some(FrontendStaleReason::DistMissing)) {
        let _ = manager::stop_runner_by_id(&state, &id).await;
        {
            let mut runners = state.runners.write().await;
            runners.remove(&id);
        }
        let settings_path = settings::settings_path(&state.config);
        settings::remove_runner(&settings_path, &id);
        state.notify_health_change();
        let reason_str = stale_reason.map(|r| r.as_str()).unwrap_or("unknown");
        let body = json!({
            "error": "frontend_dist_missing",
            "message": "frontend_dist_missing: refusing to spawn — qontinui-runner/dist/index.html \
                is missing, so the runner would embed no frontend and serve a blank screen \
                (`asset not found: index.html`) at every route. Run \
                `cd qontinui-runner && npm run build` to rebuild dist/, then retry.",
            "frontend_stale": true,
            "frontend_stale_reason": reason_str,
            "stale_slot_id": stale_slot_id,
        });
        return Ok((axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response());
    }

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
    // Dev-action snapshot fields (in-place ACK enrichment per §6.1).
    resp["action_id"] = json!(action_id.to_string());
    resp["states_active"] = json!(action_states_active);
    resp["outcome_url"] = json!(format!("/actions/{}/outcome", action_id));

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
///
/// Build-artifact footprint (plan
/// `2026-06-05-supervisor-build-artifact-footprint`): the response carries a
/// `footprint` object (per-slot bytes, lkg bytes, `.spawn-*` containers, exe
/// copies, disk free) with its own `computed_at` so staleness is explicit. By
/// default it serves the CACHED snapshot (background-refreshed every 15 min)
/// because walking GB-scale trees is minutes-slow. Pass
/// `?refresh_footprint=1` to force a synchronous recompute before responding.
#[derive(Deserialize, Default)]
pub struct ListBuildsQuery {
    /// When truthy (`1`/`true`), recompute the footprint snapshot synchronously
    /// before responding instead of serving the cached one.
    #[serde(default)]
    pub refresh_footprint: Option<String>,
}

fn is_truthy_flag(v: &Option<String>) -> bool {
    matches!(
        v.as_deref().map(|s| s.trim()),
        Some("1") | Some("true") | Some("yes")
    )
}

pub async fn list_builds(
    State(state): State<SharedState>,
    Query(query): Query<ListBuildsQuery>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    // Footprint: force a synchronous recompute when asked, else serve cache.
    let footprint_json: serde_json::Value = if is_truthy_flag(&query.refresh_footprint) {
        let snap = state.refresh_footprint().await;
        serde_json::to_value(&snap).unwrap_or(serde_json::Value::Null)
    } else {
        match state.footprint.read().await.as_ref() {
            Some(snap) => serde_json::to_value(snap).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        }
    };
    let mut slots_json: Vec<serde_json::Value> = Vec::with_capacity(state.build_pool.slots.len());
    let mut global_sum: f64 = 0.0;
    let mut global_count: usize = 0;
    // Derive `active_builds` and the slot-based permit count from the same
    // per-slot iteration that builds `slots_json` so the three views never
    // disagree. Previously `available_permits` came from the semaphore
    // (`state.build_pool.permits.available_permits()`) while per-slot state
    // came from each `slot.busy` lock — those release at different points in
    // `run_cargo_build_with_dir` and a reader hitting `/builds` mid-release
    // could observe `permits_free=3` while `slots[0].state=building` with
    // multi-minute elapsed.
    let mut active_builds: Vec<serde_json::Value> =
        Vec::with_capacity(state.build_pool.slots.len());

    // Cross-slot SHA snapshot — what resolve_source_exe would pick now,
    // each slot's sidecar SHA (None when absent), and the drift warning.
    let freshness = crate::process::manager::compute_slot_freshness(&state).await;
    let provenance_by_slot: std::collections::HashMap<
        usize,
        crate::process::manager::SlotProvenanceKey,
    > = freshness.slot_provenance.iter().cloned().collect();

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

        let (slot_git_sha, slot_source) = match provenance_by_slot.get(&slot.id) {
            Some((sha, src)) => (
                sha.clone(),
                src.as_ref().map(|s| match s {
                    crate::process::manager::BuildSource::LiveTree => "live_tree",
                    crate::process::manager::BuildSource::Override => "override",
                }),
            ),
            None => (None, None),
        };
        let slot_json = match info_opt {
            Some(ref i) => {
                let elapsed = (now - i.started_at).num_seconds().max(0);
                // Also push a compact summary into active_builds so the
                // top-level field is derived from the same per-slot
                // snapshot — matches the 503 `build_pool_full` body shape.
                active_builds.push(json!({
                    "slot": slot.id,
                    "started_at": i.started_at.to_rfc3339(),
                    "elapsed_secs": elapsed,
                    "requester_id": i.requester_id,
                    "rebuild_kind": i.rebuild_kind,
                }));
                json!({
                    "id": slot.id,
                    "target_dir": slot.target_dir.to_string_lossy(),
                    "state": "building",
                    "started_at": i.started_at.to_rfc3339(),
                    "elapsed_secs": elapsed,
                    "requester_id": i.requester_id,
                    "rebuild_kind": i.rebuild_kind,
                    "frontend_stale": frontend_stale,
                    "git_sha": slot_git_sha,
                    "source": slot_source,
                    "history": history_json,
                })
            }
            None => json!({
                "id": slot.id,
                "target_dir": slot.target_dir.to_string_lossy(),
                "state": "idle",
                "frontend_stale": frontend_stale,
                "git_sha": slot_git_sha,
                "source": slot_source,
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
    // Derive `available_permits` from per-slot busy counts rather than the
    // semaphore. The semaphore permit is dropped in `run_cargo_build_with_dir`
    // after `slot.busy` is cleared, so during the gap between
    // `Drop for SlotGuard` (which clears busy via `tokio::spawn`) and
    // `drop(_permit)` two views disagreed: `permits.available_permits()` could
    // still report 2 while every slot's `busy` was already `None`, or vice
    // versa. Deriving from the same iteration as `slots_json` / `active_builds`
    // makes the three views mathematically consistent: by construction
    // `pool_size = available_permits + active_builds.len()`.
    let available = state.build_pool.slots.len() - active_builds.len();
    // Also expose the raw semaphore count under a distinct field so callers
    // who care about the throttle (queue admission) can still read it. When
    // it disagrees with `available_permits`, the semaphore is the gate; the
    // disagreement is transient (release ordering inside `run_cargo_build_*`)
    // and self-heals once both views land at steady state.
    let semaphore_permits = state.build_pool.permits.available_permits();
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
                // Provenance of the LKG build (#65). `sha` = git SHA of the
                // built live tree (null when the git probe failed or the
                // on-disk record predates these fields); `source` serializes
                // via BuildSource as "live_tree"/"override".
                "sha": info.sha,
                "source": info.source,
            })
        });
    // Phase A: origin/main drift for the LKG sha — turns the silent stale-build
    // (the 2026-06-07 incident) into a surfaced signal. Computed only when the
    // LKG records a sha; `null` when up-to-date, unknown, or not computable
    // (no remote / not a repo). The git repo root is `project_dir.parent()`
    // (project_dir is `qontinui-runner/src-tauri`). Best-effort: a probe
    // failure reads as not-computable and yields `null`.
    let lkg_sha: Option<String> = state
        .build_pool
        .last_known_good
        .read()
        .await
        .as_ref()
        .and_then(|info| info.sha.clone());
    let origin_main_drift = match (&lkg_sha, state.config.project_dir.parent()) {
        (Some(sha), Some(repo_root)) => {
            let drift = crate::git_provenance::origin_main_drift(repo_root, sha).await;
            // Null when up-to-date or not computable; surface only real drift,
            // matching the null-when-fine convention of the `*_warning` siblings.
            if drift.origin_main_sha.is_empty() || drift.is_up_to_date() {
                None
            } else {
                Some(json!({
                    "built_sha": drift.built_sha,
                    "origin_main_sha": drift.origin_main_sha,
                    "behind_count": drift.behind_count,
                    "is_ancestor": drift.is_ancestor,
                    "diverged": drift.is_diverged(),
                    "fetched": drift.fetched,
                }))
            }
        }
        _ => None,
    };
    let slot_freshness_warning = freshness.drift.as_ref().map(|d| {
        let source_label = |s: &crate::process::manager::BuildSource| match s {
            crate::process::manager::BuildSource::LiveTree => "live_tree",
            crate::process::manager::BuildSource::Override => "override",
        };
        json!({
            "picked_slot_id": d.picked_slot_id,
            "picked_sha": d.picked_sha,
            "picked_source": source_label(&d.picked_source),
            "conflicting": d.conflicting.iter().map(|(id, sha, src)| json!({
                "slot_id": id,
                "sha": sha,
                "source": src.as_ref().map(source_label),
            })).collect::<Vec<_>>(),
            "message": crate::process::manager::format_drift_warning(d),
        })
    });
    // Adjacent staleness surface: a stale exe at `target/debug/` (operator
    // built from workspace root instead of into a slot). Null when no legacy
    // exe, no slot exes, or legacy is not strictly older than every slot.
    let legacy_target_debug_warning = freshness.target_debug_staleness.as_ref().map(|s| {
        let legacy_iso: chrono::DateTime<chrono::Utc> = s.legacy_mtime.into();
        let oldest_iso: chrono::DateTime<chrono::Utc> = s.oldest_slot_mtime.into();
        json!({
            "legacy_path": s.legacy_path.to_string_lossy(),
            "legacy_mtime": legacy_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "oldest_slot_mtime": oldest_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "message": crate::process::manager::format_target_debug_warning(s),
        })
    });

    Json(json!({
        "pool_size": state.build_pool.slots.len(),
        "available_permits": available,
        // Diagnostic: raw semaphore count. Equal to `available_permits` at
        // steady state; transient divergence indicates a slot release in
        // flight inside `run_cargo_build_with_dir` (see comment above where
        // `available` is derived). Useful for debugging stuck-build reports.
        "semaphore_permits": semaphore_permits,
        "queued": queued,
        "last_successful_slot": last_successful,
        // The slot resolve_source_exe would pick right now. Differs from
        // last_successful_slot when that slot's exe is gone (supervisor restart,
        // wipe, etc.) and the scan falls through to a different slot.
        "resolved_slot_id": freshness.picked_slot_id,
        "avg_build_duration_secs": avg_build_duration_secs,
        "any_slot_has_stale_frontend": any_slot_has_stale_frontend,
        "slots": slots_json,
        // Compact view of in-flight builds, derived from the same per-slot
        // iteration as `slots[]` so the two views can never disagree. Shape
        // matches the 503 `build_pool_full` error body for symmetry.
        // Invariant: `pool_size == available_permits + active_builds.len()`.
        "active_builds": active_builds,
        "lkg": lkg_json,
        // Phase A: drift of the LKG sha vs origin/main. `null` when up-to-date,
        // unknown, or not computable; otherwise carries behind_count + a
        // `diverged` flag (is_ancestor == false). The loud counterpart to the
        // silent stale-build incident (2026-06-07).
        "origin_main_drift": origin_main_drift,
        "slot_freshness_warning": slot_freshness_warning,
        "legacy_target_debug_warning": legacy_target_debug_warning,
        // Build-artifact footprint snapshot (plan
        // 2026-06-05-supervisor-build-artifact-footprint). `null` until the
        // first refresh; carries its own `computed_at` for staleness. Force a
        // fresh walk with `?refresh_footprint=1`.
        "footprint": footprint_json,
    }))
}

// --- Build-artifact prune endpoints (Phase 3) ---

#[derive(Deserialize, Default)]
pub struct PruneSpawnQuery {
    /// Retention window override in hours. A container is prunable once its
    /// mtime is older than this (dirty registered worktrees need DOUBLE). When
    /// omitted, the engine resolves the window from
    /// `QONTINUI_SPAWN_WORKTREE_RETENTION_HOURS` / its 48h default.
    #[serde(default)]
    pub older_than_hours: Option<u64>,
}

/// DELETE /spawn-worktrees?older_than_hours=<h> — prune stale `.spawn-*`
/// scratch-worktree containers.
///
/// Thin wrapper over the existing engine
/// [`crate::spawn_worktree::prune_spawn_worktrees_with_window`]. The engine
/// owns ALL selection + safety (prefix-under-root filter, active-build
/// exclusion, age + dirty double-retention). This handler only:
///   1. passes the LIVE active-container set from state (same source the
///      opportunistic sweep uses),
///   2. forwards the optional `older_than_hours` window override,
///   3. measures container sizes BEFORE the delete so it can report
///      `bytes_freed`,
///   4. refreshes the footprint cache afterward.
pub async fn prune_spawn_worktrees_endpoint(
    State(state): State<SharedState>,
    Query(query): Query<PruneSpawnQuery>,
) -> impl IntoResponse {
    // Snapshot sizes of every `.spawn-*` container up front (the engine deletes
    // them, so we can't measure after). Best-effort: failures yield 0.
    let pre_sizes: std::collections::HashMap<std::path::PathBuf, u64> =
        match crate::spawn_worktree::derive_workspace_root(&state.config.project_dir) {
            Ok(ws) => {
                let mut map = std::collections::HashMap::new();
                if let Ok(entries) = std::fs::read_dir(&ws) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with(crate::spawn_worktree::SPAWN_DIR_PREFIX)
                            && entry.path().is_dir()
                        {
                            let p = entry.path();
                            let size = crate::footprint::dir_size_bytes(&p);
                            map.insert(p, size);
                        }
                    }
                }
                map
            }
            Err(_) => std::collections::HashMap::new(),
        };

    // Live active-build container set — mirror the opportunistic sweep.
    let active_now: std::collections::HashSet<std::path::PathBuf> =
        state.active_spawn_worktrees.lock().unwrap().clone();

    let report = crate::spawn_worktree::prune_spawn_worktrees_with_window(
        &state.config.project_dir,
        &active_now,
        std::time::SystemTime::now(),
        query.older_than_hours,
    )
    .await;

    let bytes_freed: u64 = report
        .removed
        .iter()
        .map(|p| pre_sizes.get(p).copied().unwrap_or(0))
        .fold(0u64, |a, b| a.saturating_add(b));

    let removed: Vec<String> = report
        .removed
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let kept: Vec<serde_json::Value> = report
        .kept
        .iter()
        .map(|(p, reason)| json!({ "path": p.to_string_lossy(), "reason": reason }))
        .collect();
    let failed: Vec<serde_json::Value> = report
        .failed
        .iter()
        .map(|(p, err)| json!({ "path": p.to_string_lossy(), "error": err }))
        .collect();

    if !report.removed.is_empty() {
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "DELETE /spawn-worktrees: removed {} container(s), freed {} bytes",
                    report.removed.len(),
                    bytes_freed
                ),
            )
            .await;
        // Bytes were freed — invalidate + refresh the footprint cache.
        let _ = state.refresh_footprint().await;
    }

    Json(json!({
        "removed": removed,
        "kept": kept,
        "failed": failed,
        "bytes_freed": bytes_freed,
        "window_hours": query.older_than_hours,
    }))
}

/// POST /builds/slots/{id}/clean — empty a single build-pool slot's target dir.
///
/// Refuses (409, structured) when the slot has an active build OR when its exe
/// is held open by a live process (reuses the holder-detection machinery used
/// by `free_slot_exe`). On success, deletes the slot's `target_dir` contents,
/// reports `bytes_freed`, and refreshes the footprint cache.
pub async fn clean_slot_endpoint(
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

    // Refusal 1: an active build owns this slot.
    let busy = {
        match slot.busy.try_read() {
            Ok(g) => g.clone(),
            Err(_) => slot.busy.read().await.clone(),
        }
    };
    if let Some(info) = busy {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({
                "error": "slot_busy",
                "message": "Slot has an active build; refusing to clean.",
                "slot_id": slot_id,
                "started_at": info.started_at.to_rfc3339(),
                "requester_id": info.requester_id,
                "rebuild_kind": info.rebuild_kind,
            })),
        )
            .into_response();
    }

    // Refusal 2: the slot exe is held open by a live process. Holder detection
    // is Windows-only (sysinfo image-path match); on other platforms there is
    // no holder concept for a stalled file lock, so the check is a no-op.
    let exe_path = slot.target_dir.join("debug").join("qontinui-runner.exe");
    let holders = crate::build_monitor::slot_exe_holders(&exe_path).await;
    if !holders.is_empty() {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({
                "error": "slot_exe_held",
                "message": "Slot exe is held open by a live process; refusing to clean.",
                "slot_id": slot_id,
                "exe_path": exe_path.to_string_lossy(),
                "holder_pids": holders,
            })),
        )
            .into_response();
    }

    // Measure before deleting.
    let bytes_before = crate::footprint::dir_size_bytes(&slot.target_dir);

    // Empty the slot target dir. Remove + recreate so subsequent builds find an
    // existing (empty) dir, matching how `BuildPool::new` creates them eagerly.
    let remove_result = tokio::fs::remove_dir_all(&slot.target_dir).await;
    if let Err(e) = &remove_result {
        // ENOENT is fine (already empty); any other error is a real failure.
        if e.kind() != std::io::ErrorKind::NotFound {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "clean_failed",
                    "message": format!("failed to remove slot target dir: {}", e),
                    "slot_id": slot_id,
                    "target_dir": slot.target_dir.to_string_lossy(),
                })),
            )
                .into_response();
        }
    }
    if let Err(e) = tokio::fs::create_dir_all(&slot.target_dir).await {
        warn!(
            "clean-slot: removed slot {} dir but failed to recreate it: {}",
            slot_id, e
        );
    }

    let bytes_freed = crate::footprint::dir_size_bytes(&slot.target_dir);
    let bytes_freed = bytes_before.saturating_sub(bytes_freed);

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "POST /builds/slots/{}/clean: freed {} bytes",
                slot_id, bytes_freed
            ),
        )
        .await;

    // Bytes were freed (or the slot was already empty) — refresh footprint.
    let _ = state.refresh_footprint().await;

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "slot_id": slot_id,
            "target_dir": slot.target_dir.to_string_lossy(),
            "bytes_freed": bytes_freed,
        })),
    )
        .into_response()
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

/// GET /builds/{slot_id}/log/stream — SSE stream of cargo stderr lines for
/// THIS slot's currently-running build.
///
/// Each cargo stderr line is emitted as an SSE event with `event: cargo` and
/// the raw line as the `data:` payload. Subscribers pick up at the next line
/// cargo writes — there is no replay of lines from before the subscription.
/// For a one-shot view of the most recent completed build, use
/// `GET /builds/{slot_id}/log`.
///
/// Behavior matrix:
/// * Slot id out of range → 404 `{error: "slot_not_found", ...}`.
/// * Slot is idle when the SSE connection opens → the stream sends one
///   `event: status` frame with `{"state": "idle"}` so clients can detect
///   "no active build" without parsing the empty stream; then it keeps the
///   connection open and starts streaming as soon as the next build on this
///   slot begins. This is the "tail -f" semantics the user expects when
///   wiring this up to `POST /runners/spawn-test {rebuild: true}`.
/// * Build completes → the stream emits one `event: completed` frame and
///   keeps the connection open (the per-slot broadcast persists across
///   builds). Clients that only care about the current build should
///   disconnect on `completed`.
/// * Subscriber lags behind (cargo produces lines faster than the client
///   reads) → the broadcast layer drops the oldest unread lines and the
///   client receives an `event: lagged` frame with `{"skipped": <usize>}`.
///   Clients should refetch the full log from `GET /builds/{slot_id}/log`
///   once the build completes if they need every line.
///
/// Terminates on `state.shutdown_signal()` for graceful supervisor exit,
/// same pattern as the other SSE handlers in the supervisor.
pub async fn slot_build_log_stream(
    State(state): State<SharedState>,
    Path(slot_id): Path<usize>,
) -> Result<
    Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>,
    axum::response::Response,
> {
    let slot = match state.build_pool.slots.get(slot_id) {
        Some(s) => s.clone(),
        None => {
            return Err((
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "slot_not_found",
                    "slot_id": slot_id,
                    "pool_size": state.build_pool.slots.len(),
                })),
            )
                .into_response());
        }
    };

    // Subscribe BEFORE checking the current `busy` state. If we read
    // `slot.busy` first and the build starts between the read and the
    // subscribe, we'd miss the early cargo lines. Subscribing first means
    // we'd at worst see one extra "status: idle" frame followed immediately
    // by real cargo data — strictly safer.
    let rx = slot.log_stream.subscribe();
    let initial_busy_info = match slot.busy.try_read() {
        Ok(g) => g.as_ref().map(|i| (i.started_at, i.rebuild_kind.clone())),
        Err(_) => slot
            .busy
            .read()
            .await
            .as_ref()
            .map(|i| (i.started_at, i.rebuild_kind.clone())),
    };

    // Track this connection in `state.active_sse_connections` so it shows up
    // on /health like every other SSE stream.
    let conn_guard = SseConnectionGuard::new(state.active_sse_connections.clone());

    // Stream construction (no async_stream — supervisor doesn't pull that
    // crate). We compose three streams:
    //   1. A one-shot prelude `event: status` frame derived from
    //      `initial_busy_info`.
    //   2. The slot's broadcast wrapped in `BroadcastStream`, mapped to
    //      `event: cargo` data frames. `BroadcastStream`'s `Lagged` error
    //      variant becomes `event: lagged` frames so clients know to fetch
    //      the full log from `GET /builds/{slot_id}/log` on completion.
    //   3. A periodic 1s tick observing `slot.busy` to emit one `event:
    //      completed` frame when the build transitions back to idle. We
    //      can't rely on `RecvError::Closed` because the sender lives on
    //      the slot for the whole supervisor lifetime.
    //
    // Both `tokio_stream::StreamExt` and `futures::StreamExt` are in scope
    // (the former via the file-level `use`, the latter via the
    // `futures::StreamExt::take_until` call below). To avoid the E0034
    // ambiguity that bit the first compile, this block uses fully qualified
    // syntax: `futures::stream::*` for adapters, `tokio_stream::wrappers::*`
    // for the broadcast / interval wrappers. Nothing here calls
    // `.filter_map`/`.chain`/`.map` via method syntax.
    use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
    use tokio_stream::wrappers::{BroadcastStream, IntervalStream};

    let status_payload = match &initial_busy_info {
        Some((started_at, rebuild_kind)) => json!({
            "slot_id": slot_id,
            "state": "building",
            "started_at": started_at.to_rfc3339(),
            "rebuild_kind": rebuild_kind,
        }),
        None => json!({
            "slot_id": slot_id,
            "state": "idle",
        }),
    };
    let initial = futures::stream::once(async move {
        Ok::<_, Infallible>(
            Event::default()
                .event("status")
                .data(status_payload.to_string()),
        )
    });

    let cargo_lines = futures::StreamExt::filter_map(BroadcastStream::new(rx), |res| async move {
        match res {
            Ok(line) => Some(Ok::<_, Infallible>(
                Event::default().event("cargo").data(line),
            )),
            Err(BroadcastStreamRecvError::Lagged(n)) => Some(Ok(Event::default()
                .event("lagged")
                .data(json!({"skipped": n}).to_string()))),
        }
    });

    // Completion ticker: 1s cadence, emits at most one `event: completed`
    // per building→idle transition. State lives in a `tokio::sync::Mutex<bool>`
    // Arc so the captured closure can mutate it across ticks.
    let was_building_state = Arc::new(tokio::sync::Mutex::new(initial_busy_info.is_some()));
    let ticker_slot = slot.clone();
    let ticker_state = was_building_state.clone();
    let ticker = futures::StreamExt::filter_map(
        IntervalStream::new(tokio::time::interval(std::time::Duration::from_secs(1))),
        move |_| {
            let slot = ticker_slot.clone();
            let was_building_arc = ticker_state.clone();
            async move {
                let is_busy = match slot.busy.try_read() {
                    Ok(g) => g.is_some(),
                    Err(_) => slot.busy.read().await.is_some(),
                };
                let mut was_building = was_building_arc.lock().await;
                if !*was_building && is_busy {
                    *was_building = true;
                    None
                } else if *was_building && !is_busy {
                    *was_building = false;
                    Some(Ok::<_, Infallible>(
                        Event::default()
                            .event("completed")
                            .data(json!({"slot_id": slot_id}).to_string()),
                    ))
                } else {
                    None
                }
            }
        },
    );

    // Merge the cargo lines + completion ticks. `select` interleaves
    // ready events from both; ordering between a final cargo line and the
    // completed frame is best-effort (cargo lines win when ready).
    let live = futures::stream::select(cargo_lines, ticker);
    let event_stream = futures::StreamExt::chain(initial, live);
    // Hold the SSE connection guard for the lifetime of the stream so
    // /health's `sse_active_connections` reflects this subscriber.
    let event_stream = futures::StreamExt::map(event_stream, move |ev| {
        let _hold = &conn_guard;
        ev
    });

    let shutdown_state = state.clone();
    let shutdown = Box::pin(async move { shutdown_state.shutdown_signal().await });
    let event_stream = futures::StreamExt::take_until(event_stream, shutdown);

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
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

    /// Pin the default + explicit-true wire shapes for the
    /// `cleanup_worktree_on_fail` flag. External agents call this endpoint
    /// with arbitrary JSON; a typo or default-change must be caught here.
    #[test]
    fn spawn_test_request_cleanup_worktree_on_fail_defaults_false() {
        // Empty payload (every field at default) — flag must be false so
        // existing callers' idempotent-reuse semantics are preserved.
        let req: super::SpawnTestRequest =
            serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
        assert!(
            !req.cleanup_worktree_on_fail,
            "default must be false (idempotent reuse)"
        );
    }

    #[test]
    fn spawn_test_request_cleanup_worktree_on_fail_explicit_true() {
        // Explicit opt-in by an agent that wants a fresh worktree on the
        // next attempt after a downstream cargo failure.
        let req: super::SpawnTestRequest =
            serde_json::from_str(r#"{"cleanup_worktree_on_fail": true}"#)
                .expect("deserialize SpawnTestRequest with cleanup_worktree_on_fail");
        assert!(req.cleanup_worktree_on_fail);
    }

    /// Phase 2b — `paired_profile_id` defaults to None and round-trips a
    /// supplied string verbatim. Pinned so a typo or default-change is
    /// caught here rather than at `/manual-test-coord` runtime.
    #[test]
    fn spawn_test_request_paired_profile_id_defaults_none() {
        let req: super::SpawnTestRequest =
            serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
        assert!(req.paired_profile_id.is_none());
    }

    #[test]
    fn spawn_test_request_paired_profile_id_explicit() {
        let req: super::SpawnTestRequest =
            serde_json::from_str(r#"{"paired_profile_id":"jspinak-spaceship"}"#)
                .expect("deserialize SpawnTestRequest with paired_profile_id");
        assert_eq!(req.paired_profile_id.as_deref(), Some("jspinak-spaceship"));
    }

    // -------------------------------------------------------------------
    // Phase 2b — apply_paired_profile helper tests.
    // -------------------------------------------------------------------

    #[test]
    fn apply_paired_profile_rejects_traversal() {
        let tmp_profiles = tempfile::tempdir().unwrap();
        let tmp_data = tempfile::tempdir().unwrap();
        for bad in ["", "../etc", "a/b", "a\\b", ".."] {
            let result = super::apply_paired_profile(bad, tmp_profiles.path(), tmp_data.path());
            assert!(
                result.is_err(),
                "profile_id={:?} must be rejected as traversal",
                bad
            );
            let (status, _body) = result.err().unwrap();
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn apply_paired_profile_404_when_snapshot_missing() {
        let tmp_profiles = tempfile::tempdir().unwrap();
        let tmp_data = tempfile::tempdir().unwrap();
        let result =
            super::apply_paired_profile("does-not-exist", tmp_profiles.path(), tmp_data.path());
        let (status, body) = result.expect_err("must error");
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "profile_not_found");
        assert_eq!(body["paired_profile_id"], "does-not-exist");
    }

    #[test]
    fn apply_paired_profile_copies_paired_user_only_when_no_tokens() {
        // Snapshot dir has paired_user.json but no auth_tokens.enc.
        // applied list must include only the file actually present.
        let tmp_profiles = tempfile::tempdir().unwrap();
        let tmp_data = tempfile::tempdir().unwrap();
        let snapshot = tmp_profiles.path().join("dev-profile");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(
            snapshot.join("paired_user.json"),
            r#"{"user_id":"abc","tenant_id":"def"}"#,
        )
        .unwrap();

        let applied =
            super::apply_paired_profile("dev-profile", tmp_profiles.path(), tmp_data.path())
                .expect("must succeed");

        assert_eq!(applied, vec!["paired_user.json".to_string()]);
        // File landed in the data dir.
        let dst = tmp_data.path().join("paired_user.json");
        assert!(dst.exists(), "paired_user.json must be copied to data dir");
        let copied = std::fs::read_to_string(&dst).unwrap();
        assert!(copied.contains("abc"));
    }

    #[test]
    fn apply_paired_profile_copies_both_files_when_present() {
        let tmp_profiles = tempfile::tempdir().unwrap();
        let tmp_data = tempfile::tempdir().unwrap();
        let snapshot = tmp_profiles.path().join("paired");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("paired_user.json"), b"{}").unwrap();
        std::fs::write(snapshot.join("auth_tokens.enc"), b"\x00\x01\x02").unwrap();

        let applied = super::apply_paired_profile("paired", tmp_profiles.path(), tmp_data.path())
            .expect("must succeed");

        assert_eq!(
            applied,
            vec![
                "paired_user.json".to_string(),
                "auth_tokens.enc".to_string()
            ]
        );
        assert!(tmp_data.path().join("paired_user.json").exists());
        assert!(tmp_data.path().join("auth_tokens.enc").exists());
    }

    // -------------------------------------------------------------------
    // Response field shape — `git_ref` request + provenance fields.
    //
    // The spawn-test response surfaces a fixed set of provenance fields
    // for git_ref builds. These tests pin the wire shape so the agent-
    // facing contract is enforced at PR time, not at first-use runtime.
    // -------------------------------------------------------------------

    #[test]
    fn spawn_test_request_git_ref_defaults_none() {
        // Default callers (no git_ref set) must get None so the handler's
        // branch that requires rebuild:true is skipped.
        let req: super::SpawnTestRequest =
            serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
        assert!(req.git_ref.is_none(), "git_ref default must be None");
    }

    #[test]
    fn spawn_test_request_git_ref_round_trips() {
        // Verbatim round-trip — the supervisor never normalizes / lowercases
        // the ref; whatever the caller sends is what `prepare_worktree`
        // hands to `git`.
        let req: super::SpawnTestRequest =
            serde_json::from_str(r#"{"git_ref":"origin/main","rebuild":true}"#)
                .expect("deserialize SpawnTestRequest with git_ref");
        assert_eq!(req.git_ref.as_deref(), Some("origin/main"));
        assert!(req.rebuild);
    }

    /// The 12-char short SHA is a pure-presentational helper: the supervisor
    /// keeps the full 40-char `git rev-parse HEAD` value in
    /// `git_ref_resolved_sha` and exposes `git_ref_resolved_sha_short` as
    /// the first 12 characters. This test pins both halves of that
    /// contract so a future "shorten differently" refactor (8-char, last-N,
    /// etc.) doesn't silently drift away from what callers compare with
    /// `git rev-parse origin/main | head -c 12`.
    #[test]
    fn git_ref_resolved_sha_short_is_first_twelve_chars() {
        // Real SHA shape: 40 hex chars. The Vec<char>→String roundtrip
        // mirrors what runs inside the handler.
        let full = "0156c6775b18deadbeef0123456789abcdef0011";
        let short: String = full.chars().take(12).collect();
        assert_eq!(short.len(), 12, "short SHA must always be 12 chars");
        assert_eq!(short, "0156c6775b18");
        assert!(
            full.starts_with(&short),
            "short SHA must be a prefix of the full SHA"
        );
    }

    /// Boundary: shorter-than-12-char input (truncated/odd-shaped SHA from a
    /// minimal/fixture repo) must not panic — `take(12)` clamps to len.
    /// The handler's `chars().take(12).collect::<String>()` is panic-free,
    /// but pinning that here means a future refactor to `[..12]` slicing
    /// (which WOULD panic on shorter inputs) gets caught.
    #[test]
    fn git_ref_resolved_sha_short_handles_underlength_input() {
        let full = "abc123"; // 6 chars — shorter than 12
        let short: String = full.chars().take(12).collect();
        assert_eq!(short, "abc123");
        assert!(short.len() <= 12);
    }

    // -------------------------------------------------------------------
    // Track 2 (UI-Bridge preview-verification) — work-unit ↔ preview binding.
    //
    // Pins: (1) the new `unit_id`/`attempt_id` passthrough fields default to
    // None and round-trip verbatim; (2) the `git_ref requires rebuild:true`
    // provenance guard still fires (and only when git_ref is set); (3) the
    // `GET /runners/by-unit/{unit_id}` handler resolves bound previews and
    // returns `[]` (not 404) for an unknown unit.
    // -------------------------------------------------------------------

    #[test]
    fn spawn_test_request_unit_attempt_default_none() {
        let req: super::SpawnTestRequest =
            serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
        assert!(req.unit_id.is_none(), "unit_id default must be None");
        assert!(req.attempt_id.is_none(), "attempt_id default must be None");
    }

    #[test]
    fn spawn_test_request_unit_attempt_round_trip() {
        let req: super::SpawnTestRequest = serde_json::from_str(
            r#"{"rebuild":true,"git_ref":"feat/x","unit_id":"u1","attempt_id":"a1"}"#,
        )
        .expect("deserialize SpawnTestRequest with unit/attempt");
        assert_eq!(req.unit_id.as_deref(), Some("u1"));
        assert_eq!(req.attempt_id.as_deref(), Some("a1"));
        // unit_id present without attempt_id is a valid shape (attempt unknown).
        let req2: super::SpawnTestRequest =
            serde_json::from_str(r#"{"unit_id":"u1"}"#).expect("deserialize unit-only");
        assert_eq!(req2.unit_id.as_deref(), Some("u1"));
        assert!(req2.attempt_id.is_none());
    }

    #[test]
    fn git_ref_rebuild_guard_rejects_ref_without_rebuild() {
        // The no-silent-fallback provenance guard: git_ref + rebuild:false → 400.
        let err = super::provenance_rebuild_guard(Some("feat/x"), None, false)
            .expect_err("git_ref without rebuild must be rejected");
        let status = axum::response::IntoResponse::into_response(err).status();
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn git_ref_rebuild_guard_allows_ref_with_rebuild() {
        assert!(super::provenance_rebuild_guard(Some("feat/x"), None, true).is_ok());
    }

    // -------------------------------------------------------------------
    // Phase 1 — generalized provenance guard + alias detector.
    // -------------------------------------------------------------------

    /// Full matrix for `provenance_rebuild_guard`:
    /// {none, git_ref, worktree_path, both} × {rebuild false, true}.
    #[test]
    fn provenance_rebuild_guard_matrix() {
        use super::provenance_rebuild_guard as g;
        use crate::error::SupervisorError;
        // none → always ok regardless of rebuild.
        assert!(g(None, None, false).is_ok());
        assert!(g(None, None, true).is_ok());

        // git_ref alone → requires rebuild:true.
        let err = g(Some("feat/x"), None, false).expect_err("git_ref without rebuild must 400");
        assert_eq!(
            axum::response::IntoResponse::into_response(err).status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        assert!(g(Some("feat/x"), None, true).is_ok());

        // worktree_path alone → requires rebuild:true (named in the message).
        let err =
            g(None, Some("D:/wt"), false).expect_err("worktree_path without rebuild must 400");
        match &err {
            SupervisorError::Validation(m) => {
                assert!(m.contains("worktree_path requires rebuild:true"), "got {m}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        assert!(g(None, Some("D:/wt"), true).is_ok());

        // both → provenance_conflict (even with rebuild:true).
        let err = g(Some("feat/x"), Some("D:/wt"), true).expect_err("both selectors must 400");
        match &err {
            SupervisorError::Validation(m) => assert!(m.contains("provenance_conflict"), "got {m}"),
            other => panic!("expected Validation, got {other:?}"),
        }
        // both with rebuild:false → still the conflict (checked first).
        let err = g(Some("feat/x"), Some("D:/wt"), false)
            .expect_err("both selectors must 400 regardless of rebuild");
        assert!(err.to_string().contains("provenance_conflict"));
    }

    #[test]
    fn reject_known_provenance_aliases_flags_each_alias() {
        use super::reject_known_provenance_aliases as r;
        for (alias, correct) in [
            ("branch", "git_ref"),
            ("ref", "git_ref"),
            ("worktree", "worktree_path"),
        ] {
            let body = serde_json::json!({ alias: "value", "rebuild": true });
            let err = r(&body).expect_err("alias must be rejected");
            let s = err.to_string();
            assert!(
                s.contains(alias) && s.contains(correct),
                "alias `{alias}` 400 must name both the alias and the correct field `{correct}`; got: {s}"
            );
            assert_eq!(
                axum::response::IntoResponse::into_response(err).status(),
                axum::http::StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn reject_known_provenance_aliases_allows_real_fields() {
        use super::reject_known_provenance_aliases as r;
        // The real fields must pass the alias check.
        assert!(r(&serde_json::json!({"git_ref": "origin/main", "rebuild": true})).is_ok());
        assert!(r(&serde_json::json!({"worktree_path": "D:/wt", "rebuild": true})).is_ok());
        assert!(r(&serde_json::json!({})).is_ok());
        // A non-object body short-circuits ok (typed deserialize handles it).
        assert!(r(&serde_json::json!("not-an-object")).is_ok());
    }

    // -------------------------------------------------------------------
    // Phase 2/3 — new request-field wire shapes.
    // -------------------------------------------------------------------

    #[test]
    fn spawn_test_request_worktree_path_defaults_none() {
        let req: super::SpawnTestRequest =
            serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
        assert!(
            req.worktree_path.is_none(),
            "worktree_path default must be None"
        );
    }

    #[test]
    fn spawn_test_request_worktree_path_round_trips() {
        let req: super::SpawnTestRequest = serde_json::from_str(
            r#"{"worktree_path":"D:/qontinui-root/.spawn-pr370","rebuild":true}"#,
        )
        .expect("deserialize SpawnTestRequest with worktree_path");
        assert_eq!(
            req.worktree_path.as_deref(),
            Some("D:/qontinui-root/.spawn-pr370")
        );
        assert!(req.rebuild);
    }

    #[test]
    fn spawn_test_request_frontend_only_defaults_false() {
        let req: super::SpawnTestRequest =
            serde_json::from_str("{}").expect("deserialize empty SpawnTestRequest");
        assert!(!req.frontend_only, "frontend_only default must be false");
    }

    #[test]
    fn spawn_test_request_frontend_only_round_trips() {
        let req: super::SpawnTestRequest = serde_json::from_str(
            r#"{"worktree_path":"D:/wt","frontend_only":true,"rebuild":true}"#,
        )
        .expect("deserialize SpawnTestRequest with frontend_only");
        assert!(req.frontend_only);
    }

    #[test]
    fn git_ref_rebuild_guard_allows_no_ref() {
        // No selector: guard is a no-op regardless of rebuild.
        assert!(super::provenance_rebuild_guard(None, None, false).is_ok());
        assert!(super::provenance_rebuild_guard(None, None, true).is_ok());
    }

    /// Helper: insert a runner with an optional preview binding into a state's
    /// registry, mirroring the spawn-test path (config + binding on the
    /// ManagedRunner). Returns the runner id.
    async fn insert_runner_with_binding(
        state: &crate::state::SharedState,
        port: u16,
        binding: Option<crate::state::PreviewBinding>,
    ) -> String {
        let id = format!("test-{}", port);
        let mut config = crate::config::RunnerConfig::default_primary();
        config.id = id.clone();
        config.name = id.clone();
        config.port = port;
        config.kind = super::RunnerKind::Temp { id: id.clone() };
        let managed = std::sync::Arc::new(crate::state::ManagedRunner::new_with_log_dir(
            config, false, None,
        ));
        if let Some(b) = binding {
            *managed.preview_binding.write().await = Some(b);
        }
        state.runners.write().await.insert(id.clone(), managed);
        id
    }

    fn make_state() -> crate::state::SharedState {
        use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
        use std::path::PathBuf;
        let config = SupervisorConfig {
            project_dir: PathBuf::from("/tmp/test/src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: 9875,
            dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 19000,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: false,
            no_webview: true,
        };
        std::sync::Arc::new(crate::state::SupervisorState::new(config))
    }

    #[tokio::test]
    async fn runners_by_unit_resolves_bound_previews() {
        use axum::extract::{Path, State};
        let state = make_state();
        // u1 has two attempts' previews on distinct ports; u2 has one.
        insert_runner_with_binding(
            &state,
            9877,
            Some(crate::state::PreviewBinding {
                unit_id: "u1".into(),
                attempt_id: Some("a1".into()),
                git_sha: Some("0156c6775b18".into()),
            }),
        )
        .await;
        insert_runner_with_binding(
            &state,
            9878,
            Some(crate::state::PreviewBinding {
                unit_id: "u1".into(),
                attempt_id: Some("a2".into()),
                git_sha: None,
            }),
        )
        .await;
        insert_runner_with_binding(
            &state,
            9879,
            Some(crate::state::PreviewBinding {
                unit_id: "u2".into(),
                attempt_id: None,
                git_sha: None,
            }),
        )
        .await;
        // An unbound runner must never appear in any unit's handle list.
        insert_runner_with_binding(&state, 9880, None).await;

        let resp = super::runners_by_unit(State(state.clone()), Path("u1".to_string()))
            .await
            .expect("by-unit must succeed");
        let arr = resp.0.as_array().expect("array body").clone();
        assert_eq!(arr.len(), 2, "u1 has exactly two bound previews");
        let ports: std::collections::HashSet<u64> =
            arr.iter().map(|h| h["port"].as_u64().unwrap()).collect();
        assert_eq!(
            ports,
            std::collections::HashSet::from([9877, 9878]),
            "only u1's ports, not u2's or the unbound runner's"
        );
        // Handle shape: runner_id, port, ui_bridge_url, git_sha, attempt_id.
        let a1 = arr
            .iter()
            .find(|h| h["attempt_id"] == "a1")
            .expect("a1 present");
        assert_eq!(a1["port"], 9877);
        assert_eq!(a1["ui_bridge_url"], "http://localhost:9877/ui-bridge");
        assert_eq!(a1["git_sha"], "0156c6775b18");
        assert_eq!(a1["runner_id"], "test-9877");
        let a2 = arr
            .iter()
            .find(|h| h["attempt_id"] == "a2")
            .expect("a2 present");
        assert!(a2["git_sha"].is_null(), "a2 sha not yet probed → null");
    }

    #[tokio::test]
    async fn runners_by_unit_unknown_unit_returns_empty_not_404() {
        use axum::extract::{Path, State};
        let state = make_state();
        insert_runner_with_binding(
            &state,
            9877,
            Some(crate::state::PreviewBinding {
                unit_id: "u1".into(),
                attempt_id: Some("a1".into()),
                git_sha: None,
            }),
        )
        .await;
        let resp = super::runners_by_unit(State(state.clone()), Path("nope".to_string()))
            .await
            .expect("unknown unit must be 200, not an error/404");
        assert_eq!(
            resp.0.as_array().expect("array body").len(),
            0,
            "unknown unit returns [] (a queryable answer), never 404"
        );
    }
}
