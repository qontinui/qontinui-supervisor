use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use serde::Serialize;
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::log_capture::LogSource;
use crate::state::{SharedState, SseConnectionGuard};

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
///
/// Terminates on `state.shutdown_signal()` so axum's graceful drain can
/// complete promptly when the supervisor is asked to exit.
pub async fn logs_stream(
    State(state): State<SharedState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.logs.subscribe();
    let stream = BroadcastStream::new(rx);

    // Track this connection in `state.active_sse_connections`. Captured
    // by-move into the per-event closure below so it lives exactly as long
    // as the stream — drop happens when axum tears down the response.
    let conn_guard = SseConnectionGuard::new(state.active_sse_connections.clone());

    let event_stream = stream.filter_map(move |result| {
        // Hold the guard for every yielded event so the stream owns it.
        let _hold = &conn_guard;
        match result {
            Ok(entry) if entry.source == LogSource::Expo => {
                let data = serde_json::to_string(&entry).unwrap_or_default();
                Some(Ok(Event::default().event("log").data(data)))
            }
            _ => None,
        }
    });

    let shutdown_state = state.clone();
    let shutdown = Box::pin(async move { shutdown_state.shutdown_signal().await });
    let event_stream = futures::StreamExt::take_until(event_stream, shutdown);

    Sse::new(event_stream).keep_alive(KeepAlive::default())
}
