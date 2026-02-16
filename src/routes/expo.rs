use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use serde::Serialize;
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::log_capture::LogSource;
use crate::state::SharedState;

#[derive(Serialize)]
pub struct ExpoStatusResponse {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: u16,
    pub started_at: Option<String>,
    pub configured: bool,
}

#[derive(Serialize)]
pub struct ExpoActionResponse {
    pub status: String,
    pub message: String,
}

/// POST /expo/start — Start the Expo dev server.
pub async fn start(
    State(state): State<SharedState>,
) -> Result<Json<ExpoActionResponse>, Json<ExpoActionResponse>> {
    match crate::expo::start_expo(&state).await {
        Ok(()) => Ok(Json(ExpoActionResponse {
            status: "started".to_string(),
            message: "Expo dev server started".to_string(),
        })),
        Err(e) => Err(Json(ExpoActionResponse {
            status: "error".to_string(),
            message: e.to_string(),
        })),
    }
}

/// POST /expo/stop — Stop the Expo dev server.
pub async fn stop(
    State(state): State<SharedState>,
) -> Result<Json<ExpoActionResponse>, Json<ExpoActionResponse>> {
    match crate::expo::stop_expo(&state).await {
        Ok(()) => Ok(Json(ExpoActionResponse {
            status: "stopped".to_string(),
            message: "Expo dev server stopped".to_string(),
        })),
        Err(e) => Err(Json(ExpoActionResponse {
            status: "error".to_string(),
            message: e.to_string(),
        })),
    }
}

/// GET /expo/status — Expo running state, PID, port, configured flag.
pub async fn status(State(state): State<SharedState>) -> Json<ExpoStatusResponse> {
    let expo = state.expo.read().await;
    Json(ExpoStatusResponse {
        running: expo.running,
        pid: expo.pid,
        port: expo.port,
        started_at: expo.started_at.map(|t| t.to_rfc3339()),
        configured: state.config.expo_dir.is_some(),
    })
}

/// GET /expo/logs/stream — SSE stream filtered to LogSource::Expo.
pub async fn logs_stream(
    State(state): State<SharedState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.logs.subscribe();
    let stream = BroadcastStream::new(rx);

    let event_stream = stream.filter_map(|result| match result {
        Ok(entry) if entry.source == LogSource::Expo => {
            let data = serde_json::to_string(&entry).unwrap_or_default();
            Some(Ok(Event::default().event("log").data(data)))
        }
        _ => None,
    });

    Sse::new(event_stream).keep_alive(KeepAlive::default())
}
