//! Cascade detection event routes.
//!
//! Proxies cascade detection events from the runner so the dashboard
//! can display real-time cascade activity (backend fallback chain,
//! hit/miss, timing).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use futures::stream::Stream;
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;
use tracing::debug;

use crate::state::SharedState;

/// Port where the runner serves cascade events.
const RUNNER_API_PORT: u16 = 9876;

/// Polling interval for cascade events from the runner.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// GET /cascade/events — recent cascade events (JSON array).
///
/// Fetches from the runner and returns the snapshot.
pub async fn events(State(state): State<SharedState>) -> impl IntoResponse {
    let url = format!("http://127.0.0.1:{}/cascade/events", RUNNER_API_PORT);
    let client = &state.http_client;

    match client
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_else(|_| "[]".into());
            (StatusCode::OK, body).into_response()
        }
        Ok(resp) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("runner returned {}", resp.status()),
            })),
        )
            .into_response(),
        Err(e) => {
            debug!("Cascade events fetch failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "runner not responding",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// GET /cascade/stream — SSE stream of cascade events.
///
/// Polls the runner `/cascade/events` every second and pushes new
/// events as SSE messages. Uses the timestamp of the last forwarded
/// event as a cursor so that upstream buffer wraps (deque eviction)
/// do not cause missed or duplicated events.
pub async fn stream(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let client = state.http_client.clone();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);

    tokio::spawn(async move {
        let url = format!("http://127.0.0.1:{}/cascade/events", RUNNER_API_PORT);
        let mut last_timestamp: Option<String> = None;

        loop {
            // Poll for new events
            if let Ok(resp) = client
                .get(&url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(text) = resp.text().await {
                        if let Ok(events) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                            // Forward only events whose timestamp is strictly
                            // greater than the last one we sent.
                            for evt in &events {
                                let ts =
                                    evt.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");

                                let is_new = match &last_timestamp {
                                    Some(last) => ts > last.as_str(),
                                    None => true,
                                };

                                if is_new && !ts.is_empty() {
                                    let data = serde_json::to_string(evt).unwrap_or_default();
                                    let sse_event = Event::default().event("cascade").data(data);
                                    if tx.send(Ok(sse_event)).await.is_err() {
                                        return; // Client disconnected
                                    }
                                    last_timestamp = Some(ts.to_owned());
                                }
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}
