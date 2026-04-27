use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use tokio::time::{sleep, Duration};
use tracing::debug;

use crate::routes::health::build_health_response;
use crate::state::SharedState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: SharedState) {
    // Send initial health snapshot
    if let Ok(json) = serde_json::to_string(&build_health_response(&state).await) {
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    let mut rx = state.health_tx.subscribe();
    // Use the latched shutdown helper instead of a raw broadcast subscribe:
    // the helper resolves immediately if the WS connects *after* shutdown
    // already fired, which protects axum's graceful drain from being held
    // open by a freshly-arrived WebSocket. Without the latch, a late-
    // connecting WS would race into `shutdown_rx.recv().await` and miss the
    // already-broadcast shutdown event, holding the listener alive.
    let shutdown_state = state.clone();
    let shutdown_fut = async move { shutdown_state.shutdown_signal().await };
    tokio::pin!(shutdown_fut);

    loop {
        tokio::select! {
            // Shutdown notification — tell client and close
            _ = &mut shutdown_fut => {
                let _ = socket.send(Message::Text(r#"{"type":"shutdown"}"#.into())).await;
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            // Health change notification — debounce 100ms then send
            result = rx.recv() => {
                match result {
                    Ok(()) => {
                        // Debounce: drain any queued notifications within 100ms
                        sleep(Duration::from_millis(100)).await;
                        while rx.try_recv().is_ok() {}

                        let response = build_health_response(&state).await;
                        if let Ok(json) = serde_json::to_string(&response) {
                            if socket.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            // Client messages — handle ping/pong/close
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // Ignore text/binary from client
                }
            }
        }
    }

    debug!("WebSocket client disconnected");
}
