//! Supervisor Bridge — Command relay for the supervisor's own React dashboard.
//!
//! Implements the same protocol as the UI Bridge SDK's CommandRelay:
//! - SSE endpoint delivers commands to the browser tab
//! - POST endpoint receives command results from the browser
//! - Control endpoints queue commands and await browser responses
//!
//! This lets external tools (AI agents, workflows, etc.) inspect and interact
//! with the supervisor dashboard UI the same way they do with the runner or web frontend.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json};
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tracing::debug;
use uuid::Uuid;

use crate::state::SharedState;

// ============================================================================
// Command Relay
// ============================================================================

/// Server-side command relay for the supervisor dashboard.
pub struct CommandRelay {
    /// Pending commands awaiting browser response: commandId -> sender
    pending: RwLock<HashMap<String, PendingCommand>>,
    /// SSE subscribers: tabId -> broadcast sender for delivering commands
    subscribers: RwLock<HashMap<String, broadcast::Sender<String>>>,
    /// Heartbeat tracking: tabId -> last heartbeat instant
    heartbeats: RwLock<HashMap<String, Instant>>,
}

struct PendingCommand {
    tx: oneshot::Sender<CommandResult>,
    _created_at: Instant,
}

#[derive(Debug)]
struct CommandResult {
    success: bool,
    result: serde_json::Value,
    error: Option<String>,
}

impl Default for CommandRelay {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRelay {
    pub fn new() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            subscribers: RwLock::new(HashMap::new()),
            heartbeats: RwLock::new(HashMap::new()),
        }
    }

    /// Queue a command and wait for the browser to execute it.
    async fn queue_command(
        &self,
        action: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
        let command_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();

        // Insert pending command
        self.pending.write().await.insert(
            command_id.clone(),
            PendingCommand {
                tx,
                _created_at: Instant::now(),
            },
        );

        // Build command message
        let command_json = serde_json::json!({
            "commandId": command_id,
            "action": action,
            "payload": payload,
            "timestamp": chrono::Utc::now().timestamp_millis(),
        });
        let msg = serde_json::to_string(&command_json).unwrap_or_default();

        // Broadcast to all SSE subscribers
        let subs = self.subscribers.read().await;
        if subs.is_empty() {
            drop(subs);
            self.pending.write().await.remove(&command_id);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "success": false,
                    "error": "No browser tab connected to supervisor dashboard. Open http://localhost:9875/ in a browser.",
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                })),
            ));
        }
        let mut sent = 0;
        for sender in subs.values() {
            if sender.send(msg.clone()).is_ok() {
                sent += 1;
            }
        }
        drop(subs);

        if sent == 0 {
            self.pending.write().await.remove(&command_id);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "success": false,
                    "error": "Browser tab(s) connected but not listening. Try refreshing the supervisor dashboard.",
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                })),
            ));
        }

        debug!("Supervisor bridge: sent command {action} ({command_id}) to {sent} tab(s)");

        // Await response with timeout (8s matches SDK default)
        match tokio::time::timeout(Duration::from_secs(8), rx).await {
            Ok(Ok(result)) => {
                if result.success {
                    Ok(result.result)
                } else {
                    Err((
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(serde_json::json!({
                            "success": false,
                            "error": result.error.unwrap_or_else(|| "Command failed".to_string()),
                            "timestamp": chrono::Utc::now().timestamp_millis(),
                        })),
                    ))
                }
            }
            Ok(Err(_)) => {
                self.pending.write().await.remove(&command_id);
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Command channel closed unexpectedly",
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    })),
                ))
            }
            Err(_) => {
                self.pending.write().await.remove(&command_id);
                Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(serde_json::json!({
                        "success": false,
                        "error": "Browser did not respond within 8 seconds",
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    })),
                ))
            }
        }
    }
}

// ============================================================================
// Request / Response types
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResponseBody {
    command_id: String,
    success: bool,
    result: Option<serde_json::Value>,
    error: Option<String>,
    #[allow(dead_code)]
    tab_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatBody {
    #[allow(dead_code)]
    timestamp: Option<i64>,
    tab_id: Option<String>,
}

#[derive(Deserialize)]
pub struct StreamQuery {
    #[serde(rename = "tabId")]
    tab_id: Option<String>,
}

#[derive(Deserialize)]
pub struct ActionBody {
    action: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Deserialize)]
pub struct EvaluateBody {
    expression: String,
}

#[derive(Deserialize)]
pub struct NavigateBody {
    url: String,
}

#[derive(Deserialize)]
pub struct DiscoverBody {
    #[serde(default)]
    interactive_only: bool,
}

#[derive(Serialize)]
struct ApiResponse {
    success: bool,
    data: serde_json::Value,
    timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    _meta: Option<serde_json::Value>,
}

fn success_response(data: serde_json::Value) -> Json<ApiResponse> {
    Json(ApiResponse {
        success: true,
        data,
        timestamp: chrono::Utc::now().timestamp_millis(),
        _meta: None,
    })
}

// ============================================================================
// SSE: Command Stream
// ============================================================================

/// GET /supervisor-bridge/commands/stream?tabId=xxx
///
/// SSE endpoint. The browser's CommandRelayListener connects here to receive commands.
pub async fn commands_stream(
    State(state): State<SharedState>,
    Query(query): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let tab_id = query.tab_id.unwrap_or_else(|| Uuid::new_v4().to_string());

    let (tx, rx) = broadcast::channel::<String>(64);

    // Register this subscriber
    state
        .command_relay
        .subscribers
        .write()
        .await
        .insert(tab_id.clone(), tx);

    debug!("Supervisor bridge: tab {tab_id} connected to command stream");

    // Initial connected event
    let connected_msg = serde_json::json!({"type": "connected", "tabId": tab_id}).to_string();
    let initial =
        futures::stream::once(
            async move { Ok::<_, Infallible>(Event::default().data(connected_msg)) },
        );

    // Command events from the broadcast channel
    let command_stream = BroadcastStream::new(rx).filter_map(|result| async {
        match result {
            Ok(msg) => Some(Ok(Event::default().data(msg))),
            Err(_) => None, // Skip lagged messages
        }
    });

    let stream = initial.chain(command_stream);

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ============================================================================
// POST: Command Response
// ============================================================================

/// POST /supervisor-bridge/commands
///
/// Browser sends command execution results here.
pub async fn command_response(
    State(state): State<SharedState>,
    Json(body): Json<CommandResponseBody>,
) -> impl IntoResponse {
    let mut pending = state.command_relay.pending.write().await;
    if let Some(cmd) = pending.remove(&body.command_id) {
        let result = CommandResult {
            success: body.success,
            result: body.result.unwrap_or(serde_json::Value::Null),
            error: body.error,
        };
        let _ = cmd.tx.send(result);
        (StatusCode::OK, Json(serde_json::json!({"ok": true})))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "error": "Unknown commandId"})),
        )
    }
}

// ============================================================================
// POST: Heartbeat
// ============================================================================

/// POST /supervisor-bridge/heartbeat
pub async fn heartbeat(
    State(state): State<SharedState>,
    Json(body): Json<HeartbeatBody>,
) -> impl IntoResponse {
    if let Some(tab_id) = body.tab_id {
        state
            .command_relay
            .heartbeats
            .write()
            .await
            .insert(tab_id, Instant::now());
    }
    Json(serde_json::json!({"ok": true}))
}

// ============================================================================
// Control Endpoints
// ============================================================================

/// GET /supervisor-bridge/control/snapshot
pub async fn snapshot(State(state): State<SharedState>) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("getControlSnapshot", serde_json::json!({}))
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// GET /supervisor-bridge/control/elements
pub async fn elements(State(state): State<SharedState>) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("getControlSnapshot", serde_json::json!({}))
        .await
    {
        Ok(data) => {
            // Extract just the elements array from the snapshot
            let elements = data
                .get("elements")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![]));
            success_response(serde_json::json!({"elements": elements})).into_response()
        }
        Err((status, json)) => (status, json).into_response(),
    }
}

/// POST /supervisor-bridge/control/element/{id}/action
pub async fn element_action(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<ActionBody>,
) -> impl IntoResponse {
    let payload = serde_json::json!({
        "id": id,
        "request": {
            "action": body.action,
            "params": body.params,
        }
    });
    match state
        .command_relay
        .queue_command("executeElementAction", payload)
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// POST /supervisor-bridge/control/discover
pub async fn discover(
    State(state): State<SharedState>,
    Json(body): Json<DiscoverBody>,
) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command(
            "discover",
            serde_json::json!({"interactive_only": body.interactive_only}),
        )
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// GET /supervisor-bridge/control/console-errors
pub async fn console_errors(State(state): State<SharedState>) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("getConsoleErrors", serde_json::json!({}))
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// POST /supervisor-bridge/control/page/evaluate
pub async fn page_evaluate(
    State(state): State<SharedState>,
    Json(body): Json<EvaluateBody>,
) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command(
            "pageEvaluate",
            serde_json::json!({"expression": body.expression}),
        )
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// POST /supervisor-bridge/control/page/navigate
pub async fn page_navigate(
    State(state): State<SharedState>,
    Json(body): Json<NavigateBody>,
) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("pageNavigate", serde_json::json!({"url": body.url}))
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// POST /supervisor-bridge/control/page/refresh
pub async fn page_refresh(State(state): State<SharedState>) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("pageRefresh", serde_json::json!({}))
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

// ============================================================================
// Health / Diagnostics
// ============================================================================

/// GET /supervisor-bridge/health
pub async fn bridge_health(State(state): State<SharedState>) -> impl IntoResponse {
    let subs = state.command_relay.subscribers.read().await;
    let connected_tabs: Vec<String> = subs.keys().cloned().collect();
    let tab_count = connected_tabs.len();
    drop(subs);

    let pending = state.command_relay.pending.read().await;
    let pending_count = pending.len();
    drop(pending);

    let heartbeats = state.command_relay.heartbeats.read().await;
    let responsive = heartbeats
        .values()
        .any(|t| t.elapsed() < Duration::from_secs(30));
    let last_heartbeat_ms_ago = heartbeats
        .values()
        .map(|t| t.elapsed().as_millis() as u64)
        .min();
    drop(heartbeats);

    Json(serde_json::json!({
        "success": true,
        "data": {
            "connected_tabs": connected_tabs,
            "tab_count": tab_count,
            "pending_commands": pending_count,
            "responsive": responsive,
            "last_heartbeat_ms_ago": last_heartbeat_ms_ago,
        },
        "timestamp": chrono::Utc::now().timestamp_millis(),
    }))
}
