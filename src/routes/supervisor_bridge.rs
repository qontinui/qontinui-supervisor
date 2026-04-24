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
use std::sync::Arc;
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
    created_at: Instant,
}

#[derive(Debug)]
struct CommandResult {
    success: bool,
    result: serde_json::Value,
    error: Option<String>,
}

impl CommandRelay {
    pub fn new() -> Arc<Self> {
        let relay = Arc::new(Self {
            pending: RwLock::new(HashMap::new()),
            subscribers: RwLock::new(HashMap::new()),
            heartbeats: RwLock::new(HashMap::new()),
        });

        // Spawn a background task to reap stale pending commands (30s TTL)
        let relay_reaper = relay.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15));
            loop {
                interval.tick().await;
                let mut pending = relay_reaper.pending.write().await;
                let before = pending.len();
                pending.retain(|_id, cmd| cmd.created_at.elapsed() < Duration::from_secs(30));
                let reaped = before - pending.len();
                if reaped > 0 {
                    debug!("Supervisor bridge: reaped {reaped} stale pending commands");
                }
            }
        });

        relay
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
                created_at: Instant::now(),
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
    /// Optional navigation mode (F1): `"hard"` (default, full reload) or
    /// `"soft"` (SPA-friendly `pushState` + synthetic events). Any other
    /// value is rejected with a 400.
    #[serde(default)]
    mode: Option<String>,
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

    // Cleanup guard: removes subscriber and heartbeat when SSE stream is dropped
    let relay_cleanup = state.command_relay.clone();
    let tab_id_cleanup = tab_id.clone();
    let cleanup_stream = futures::stream::once(async move {
        // This runs when the upstream stream ends (browser disconnects).
        relay_cleanup
            .subscribers
            .write()
            .await
            .remove(&tab_id_cleanup);
        relay_cleanup
            .heartbeats
            .write()
            .await
            .remove(&tab_id_cleanup);
        debug!("Supervisor bridge: tab {tab_id_cleanup} disconnected, cleaned up");
        Ok::<_, Infallible>(Event::default().comment("disconnect"))
    });

    let stream = initial.chain(command_stream).chain(cleanup_stream);

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
        // Another tab may have already fulfilled this command — benign duplicate
        (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "duplicate": true})),
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
///
/// Accepts an optional `mode` field (`"hard"` | `"soft"`). Default `"hard"`
/// preserves pre-F1 behaviour (full webview reload). `"soft"` performs a
/// `history.pushState` + synthetic `popstate`/`ui-bridge:navigate` event pair
/// so SPA routers pick up the change without wiping injected window state.
/// Any other value is rejected with 400.
pub async fn page_navigate(
    State(state): State<SharedState>,
    Json(body): Json<NavigateBody>,
) -> impl IntoResponse {
    let mode = match body.mode.as_deref() {
        None | Some("hard") => "hard",
        Some("soft") => "soft",
        Some(other) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("invalid mode `{}` (expected \"hard\" or \"soft\")", other),
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                })),
            )
                .into_response();
        }
    };

    let payload = serde_json::json!({
        "url": body.url,
        "mode": mode,
    });

    match state
        .command_relay
        .queue_command("pageNavigate", payload)
        .await
    {
        Ok(mut data) => {
            // Defensive: guarantee the audit fields are present even if the
            // browser-side handler omits them.
            if let Some(obj) = data.as_object_mut() {
                obj.entry("mode".to_string())
                    .or_insert_with(|| serde_json::Value::String(mode.to_string()));
                obj.entry("hard".to_string())
                    .or_insert_with(|| serde_json::Value::Bool(mode == "hard"));
            }
            success_response(data).into_response()
        }
        Err((status, json)) => (status, json).into_response(),
    }
}

// ============================================================================
// F2 — Network stub registry
// ============================================================================
//
// Four endpoints mirror the runner's
// `/ui-bridge/control/network/stubs[/id]` shape. Validation runs locally
// so we can return structured 400s without paying the browser IPC round-
// trip; the SDK-side `validateStubRequest` in
// `packages/ui-bridge/src/network/stubs.ts` is the source of truth for
// semantics and this layer must stay in lockstep with it.

fn is_valid_stub_method(m: &str) -> bool {
    matches!(m, "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "*")
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StubResponseBody {
    #[serde(default)]
    pub status: Option<i64>,
    // `headers` is only inspected via the raw JSON that's forwarded to the
    // SDK, but we accept it here for shape validation and to let the test
    // module assert the camelCase / snake_case contract.
    #[serde(default)]
    #[allow(dead_code)]
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub body_json: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StubRequestBody {
    pub url_pattern: Option<String>,
    #[serde(default)]
    pub method: Option<String>,
    pub response: Option<StubResponseBody>,
    #[serde(default)]
    pub times: Option<serde_json::Value>,
}

impl StubRequestBody {
    pub fn validate(&self) -> Result<(), String> {
        let pattern = self
            .url_pattern
            .as_deref()
            .ok_or("urlPattern is required")?;
        if pattern.is_empty() {
            return Err("urlPattern must be non-empty".into());
        }
        if let Some(m) = self.method.as_deref() {
            if !is_valid_stub_method(m) {
                return Err(format!(
                    "method must be one of GET|POST|PUT|DELETE|PATCH|*, got \"{}\"",
                    m
                ));
            }
        }
        let response = self.response.as_ref().ok_or("response is required")?;
        if let Some(status) = response.status {
            if !(100..=599).contains(&status) {
                return Err(format!("status must be in 100-599, got {}", status));
            }
        }
        match (response.body.is_some(), response.body_json.is_some()) {
            (false, false) => {
                return Err("exactly one of response.body or response.bodyJson is required".into())
            }
            (true, true) => {
                return Err("response.body and response.bodyJson are mutually exclusive".into())
            }
            _ => {}
        }
        if let Some(times) = &self.times {
            let ok = match times {
                serde_json::Value::String(s) => s == "always",
                serde_json::Value::Number(n) => n.as_i64().is_some_and(|i| i >= 1),
                _ => false,
            };
            if !ok {
                return Err(format!(
                    "times must be \"always\" or a positive integer, got {}",
                    times
                ));
            }
        }
        Ok(())
    }
}

/// POST /supervisor-bridge/control/network/stubs
pub async fn register_network_stub(
    State(state): State<SharedState>,
    Json(raw): Json<serde_json::Value>,
) -> impl IntoResponse {
    let req: StubRequestBody = match serde_json::from_value(raw.clone()) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "success": false,
                    "error": format!("invalid stub body: {}", e),
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                })),
            )
                .into_response();
        }
    };
    if let Err(msg) = req.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "success": false,
                "error": msg,
                "timestamp": chrono::Utc::now().timestamp_millis(),
            })),
        )
            .into_response();
    }
    match state
        .command_relay
        .queue_command("registerNetworkStub", raw)
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// GET /supervisor-bridge/control/network/stubs
pub async fn list_network_stubs(State(state): State<SharedState>) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("listNetworkStubs", serde_json::json!({}))
        .await
    {
        Ok(data) => success_response(data).into_response(),
        Err((status, json)) => (status, json).into_response(),
    }
}

/// DELETE /supervisor-bridge/control/network/stubs/:id
pub async fn delete_network_stub(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("deleteNetworkStub", serde_json::json!({ "id": id }))
        .await
    {
        Ok(data) => {
            if data.get("code").and_then(|c| c.as_str()) == Some("NOT_FOUND") {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "success": false,
                        "error": format!("stub {} not found", id),
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    })),
                )
                    .into_response();
            }
            success_response(data).into_response()
        }
        Err((status, json)) => (status, json).into_response(),
    }
}

/// DELETE /supervisor-bridge/control/network/stubs
pub async fn clear_network_stubs(State(state): State<SharedState>) -> impl IntoResponse {
    match state
        .command_relay
        .queue_command("clearNetworkStubs", serde_json::json!({}))
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
        "uiBridge": {
            "appId": "qontinui-supervisor-dashboard",
            "appName": "Qontinui Supervisor",
            "appType": "dashboard",
            "framework": "react",
            "capabilities": ["control"],
        },
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

#[cfg(test)]
mod page_navigate_mode_tests {
    //! F1: back-compat + new-mode unit tests for `NavigateBody` deserialization.
    //!
    //! The full handler touches `SharedState` so end-to-end testing lives in
    //! integration tests. These tests exercise the JSON shape + mode
    //! normalization semantics used by the handler.

    use super::NavigateBody;

    fn parse(body: &str) -> NavigateBody {
        serde_json::from_str(body).expect("NavigateBody should deserialize")
    }

    #[test]
    fn legacy_body_without_mode_parses() {
        let b = parse(r#"{"url":"/fleet"}"#);
        assert_eq!(b.url, "/fleet");
        assert_eq!(b.mode, None);
    }

    #[test]
    fn body_with_soft_mode_parses() {
        let b = parse(r#"{"url":"/fleet","mode":"soft"}"#);
        assert_eq!(b.url, "/fleet");
        assert_eq!(b.mode.as_deref(), Some("soft"));
    }

    #[test]
    fn body_with_hard_mode_parses() {
        let b = parse(r#"{"url":"/fleet","mode":"hard"}"#);
        assert_eq!(b.mode.as_deref(), Some("hard"));
    }

    #[test]
    fn body_with_unknown_mode_still_deserializes() {
        // The router-layer validation in `page_navigate` rejects unknown
        // modes with a 400; the struct itself accepts any string so we can
        // surface a clean error message instead of a serde rejection.
        let b = parse(r#"{"url":"/fleet","mode":"spa"}"#);
        assert_eq!(b.mode.as_deref(), Some("spa"));
    }
}

#[cfg(test)]
mod stub_request_tests {
    //! F2: deserialization + validation tests for `StubRequestBody` /
    //! `StubResponseBody`. Mirrors the runner-side
    //! `stubs::stub_request_tests` so the two layers can't drift.

    use super::{StubRequestBody, StubResponseBody};

    fn parse(body: &str) -> StubRequestBody {
        serde_json::from_str(body).expect("StubRequestBody must deserialize")
    }

    #[test]
    fn minimal_valid_body() {
        parse(r#"{"urlPattern":"/foo","response":{"body":"hi"}}"#)
            .validate()
            .expect("valid");
    }

    #[test]
    fn body_json_camel_case_wire_key() {
        let req = parse(r#"{"urlPattern":"/foo","response":{"bodyJson":{"a":1}}}"#);
        let resp: &StubResponseBody = req.response.as_ref().expect("response");
        assert!(resp.body_json.is_some());
        assert!(resp.body.is_none());
        req.validate().expect("valid");
    }

    #[test]
    fn missing_url_pattern() {
        let req = parse(r#"{"response":{"body":"x"}}"#);
        assert!(req.validate().unwrap_err().contains("urlPattern"));
    }

    #[test]
    fn unknown_method() {
        let req = parse(r#"{"urlPattern":"/f","method":"HEAD","response":{"body":"x"}}"#);
        assert!(req.validate().unwrap_err().contains("method"));
    }

    #[test]
    fn wildcard_method_accepted() {
        parse(r#"{"urlPattern":"/f","method":"*","response":{"body":"x"}}"#)
            .validate()
            .expect("valid");
    }

    #[test]
    fn both_bodies_rejected() {
        let req = parse(r#"{"urlPattern":"/f","response":{"body":"x","bodyJson":{"a":1}}}"#);
        assert!(req.validate().unwrap_err().contains("mutually exclusive"));
    }

    #[test]
    fn neither_body_rejected() {
        let req = parse(r#"{"urlPattern":"/f","response":{}}"#);
        assert!(req.validate().unwrap_err().contains("bodyJson"));
    }

    #[test]
    fn status_out_of_range() {
        let req = parse(r#"{"urlPattern":"/f","response":{"body":"x","status":42}}"#);
        assert!(req.validate().unwrap_err().contains("100-599"));
    }

    #[test]
    fn times_always_accepted() {
        parse(r#"{"urlPattern":"/f","response":{"body":"x"},"times":"always"}"#)
            .validate()
            .expect("valid");
    }

    #[test]
    fn times_positive_int_accepted() {
        parse(r#"{"urlPattern":"/f","response":{"body":"x"},"times":5}"#)
            .validate()
            .expect("valid");
    }

    #[test]
    fn times_zero_rejected() {
        let req = parse(r#"{"urlPattern":"/f","response":{"body":"x"},"times":0}"#);
        assert!(req.validate().unwrap_err().contains("positive integer"));
    }

    #[test]
    fn times_unknown_string_rejected() {
        let req = parse(r#"{"urlPattern":"/f","response":{"body":"x"},"times":"once"}"#);
        assert!(req.validate().unwrap_err().contains("always"));
    }

    #[test]
    fn response_headers_survive_roundtrip() {
        let req = parse(r#"{"urlPattern":"/f","response":{"body":"x","headers":{"x-a":"1"}}}"#);
        req.validate().expect("valid");
        let headers = req.response.unwrap().headers.unwrap();
        assert_eq!(headers.get("x-a").unwrap().as_str(), Some("1"));
    }
}
