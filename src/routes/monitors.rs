//! Spawn-monitor placement config.
//!
//! Read by `forward_window_position_env` in `process::manager` when
//! spawning a non-primary runner: the supervisor picks the next enabled
//! monitor in round-robin order and exports its rect as the
//! `QONTINUI_WINDOW_X/Y/WIDTH/HEIGHT` env vars. The runner reads these at
//! window-build time (see `qontinui-runner/src-tauri/src/main.rs`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::settings::{self, MonitorConfig};
use crate::state::SharedState;

#[derive(Serialize)]
pub struct SpawnMonitorsResponse {
    pub monitors: Vec<MonitorConfig>,
    pub next_index: usize,
}

#[derive(Deserialize)]
pub struct PutSpawnMonitorsRequest {
    pub monitors: Vec<MonitorConfig>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// GET /spawn-monitors — current monitor placement config + the next index
/// the round-robin will hand out (modulo enabled count).
pub async fn list_spawn_monitors(State(state): State<SharedState>) -> Json<SpawnMonitorsResponse> {
    use std::sync::atomic::Ordering;
    let monitors = state.spawn_monitors.read().await.clone();
    let next_index = state.next_monitor_index.load(Ordering::Relaxed);
    Json(SpawnMonitorsResponse {
        monitors,
        next_index,
    })
}

/// PUT /spawn-monitors — replace the full list and persist to disk.
/// Resets the round-robin counter to 0 so the next spawn lands on the first
/// enabled entry of the new list.
pub async fn put_spawn_monitors(
    State(state): State<SharedState>,
    Json(body): Json<PutSpawnMonitorsRequest>,
) -> Result<Json<SpawnMonitorsResponse>, (StatusCode, Json<ErrorResponse>)> {
    use std::sync::atomic::Ordering;

    for (i, m) in body.monitors.iter().enumerate() {
        if m.label.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("monitor[{i}]: label must not be empty"),
                }),
            ));
        }
        if m.width == 0 || m.height == 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("monitor[{i}] '{}': width/height must be > 0", m.label),
                }),
            ));
        }
    }

    let path = settings::settings_path(&state.config);
    if let Err(e) = settings::save_spawn_monitors(&path, &body.monitors) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to save settings: {e}"),
            }),
        ));
    }

    *state.spawn_monitors.write().await = body.monitors.clone();
    state.next_monitor_index.store(0, Ordering::Relaxed);

    Ok(Json(SpawnMonitorsResponse {
        monitors: body.monitors,
        next_index: 0,
    }))
}
