use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::state::SharedState;

#[derive(Serialize)]
pub struct SmartRebuildStatusResponse {
    pub enabled: bool,
    pub phase: crate::smart_rebuild::SmartRebuildPhase,
    pub current_attempt: u32,
    pub last_build_error: Option<String>,
    pub total_rebuilds: u32,
    pub last_rebuild_at: Option<String>,
}

#[derive(Deserialize)]
pub struct EnableRequest {
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub status: String,
    pub message: String,
}

/// GET /smart-rebuild/status
pub async fn status(State(state): State<SharedState>) -> Json<SmartRebuildStatusResponse> {
    let sr = state.smart_rebuild.read().await;
    Json(SmartRebuildStatusResponse {
        enabled: sr.enabled,
        phase: sr.phase.clone(),
        current_attempt: sr.current_attempt,
        last_build_error: sr.last_build_error.clone(),
        total_rebuilds: sr.total_rebuilds,
        last_rebuild_at: sr.last_rebuild_at.map(|t| t.to_rfc3339()),
    })
}

/// POST /smart-rebuild/enable — body: {"enabled": bool}
pub async fn enable(
    State(state): State<SharedState>,
    Json(body): Json<EnableRequest>,
) -> Json<ActionResponse> {
    let mut sr = state.smart_rebuild.write().await;
    sr.enabled = body.enabled;

    // Initialize last_successful_build_mtime on first enable so we don't
    // immediately rebuild existing code
    if body.enabled && sr.last_successful_build_mtime.is_none() {
        let mtime = crate::code_activity::get_last_source_modification(&state.config.project_dir);
        sr.last_successful_build_mtime = mtime;
    }

    let action = if body.enabled { "enabled" } else { "disabled" };
    tracing::info!("Smart rebuild {}", action);
    state
        .logs
        .emit(
            crate::log_capture::LogSource::SmartRebuild,
            crate::log_capture::LogLevel::Info,
            format!("Smart rebuild {}", action),
        )
        .await;

    Json(ActionResponse {
        status: action.to_string(),
        message: format!("Smart rebuild {}", action),
    })
}

/// POST /smart-rebuild/trigger — manually trigger a rebuild
pub async fn trigger(State(state): State<SharedState>) -> Json<ActionResponse> {
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::smart_rebuild::trigger_smart_rebuild(&state_clone).await;
    });

    Json(ActionResponse {
        status: "triggered".to_string(),
        message: "Smart rebuild triggered".to_string(),
    })
}

/// POST /smart-rebuild/stop — cancel an in-progress rebuild
pub async fn stop(State(state): State<SharedState>) -> Json<ActionResponse> {
    crate::smart_rebuild::cancel_smart_rebuild(&state).await;

    Json(ActionResponse {
        status: "stopped".to_string(),
        message: "Smart rebuild cancelled".to_string(),
    })
}
