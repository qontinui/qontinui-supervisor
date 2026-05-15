use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::state::SharedState;

/// One row in the supervisor's HTTP API manifest. Returned by `GET /help`.
///
/// Sourced from the "API Endpoints" section of `CLAUDE.md`. The list is
/// intentionally static (compiled into the binary) so the manifest reflects
/// what the build supports — not whatever the docs file on disk happens to
/// say at runtime. When you add or remove a route, update this list AND
/// `CLAUDE.md` together.
#[derive(serde::Serialize)]
pub struct EndpointEntry {
    pub method: &'static str,
    pub path: &'static str,
    pub summary: &'static str,
}

pub static ENDPOINT_MANIFEST: &[EndpointEntry] = &[
    // Health & Dashboard
    EndpointEntry {
        method: "GET",
        path: "/",
        summary: "React SPA dashboard",
    },
    EndpointEntry {
        method: "GET",
        path: "/health",
        summary: "Comprehensive status (runners, build, expo)",
    },
    EndpointEntry {
        method: "GET",
        path: "/health/stream",
        summary: "SSE stream of real-time health data",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor/restart",
        summary: "Self-restart supervisor (runners are left running)",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor/shutdown",
        summary: "Graceful supervisor shutdown",
    },
    EndpointEntry {
        method: "GET",
        path: "/help",
        summary: "List of supervisor HTTP endpoints",
    },
    // Runner Management
    EndpointEntry {
        method: "GET",
        path: "/runners",
        summary: "List all runners with status",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners",
        summary: "Add a runner config to the registry",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/spawn-test",
        summary: "Spawn ephemeral test runner on next free port",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/spawn-named",
        summary: "Spawn persistent named runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/purge-stale",
        summary: "Remove runners whose processes are no longer alive",
    },
    EndpointEntry {
        method: "DELETE",
        path: "/runners/{id}",
        summary: "Remove a runner from the registry",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/start",
        summary: "Start a runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/stop",
        summary: "Stop a runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/restart",
        summary: "Restart a runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/protect",
        summary: "Toggle protection on a runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/watchdog",
        summary: "Control watchdog for a specific runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/rebuild-and-restart",
        summary: "Rebuild then restart a non-primary runner",
    },
    EndpointEntry {
        method: "GET",
        path: "/runners/{id}/logs",
        summary: "Log history for a specific runner",
    },
    EndpointEntry {
        method: "GET",
        path: "/runners/{id}/logs/stream",
        summary: "SSE log stream for a specific runner",
    },
    EndpointEntry {
        method: "GET",
        path: "/runners/{id}/early-log",
        summary: "Per-spawn early-death log file content",
    },
    EndpointEntry {
        method: "GET",
        path: "/runners/{id}/crash-summary",
        summary: "Crash summary for a stopped runner",
    },
    EndpointEntry {
        method: "GET",
        path: "/runners/{id}/crash-dump",
        summary: "Full crash dump payload",
    },
    EndpointEntry {
        method: "GET",
        path: "/runners/{id}/ui-bridge/{*path}",
        summary: "Proxy GET UI Bridge requests to a runner",
    },
    EndpointEntry {
        method: "POST",
        path: "/runners/{id}/ui-bridge/{*path}",
        summary: "Proxy POST UI Bridge requests to a runner",
    },
    // Parallel Build Pool
    EndpointEntry {
        method: "POST",
        path: "/build/submit",
        summary: "Submit a build action against a worktree (Row 2 Phase 3)",
    },
    EndpointEntry {
        method: "GET",
        path: "/build/{id}/status",
        summary: "Get the status of a submitted build",
    },
    EndpointEntry {
        method: "GET",
        path: "/builds",
        summary: "Snapshot of the parallel build pool",
    },
    EndpointEntry {
        method: "GET",
        path: "/builds/{slot_id}/log",
        summary: "Latest combined build log for a slot",
    },
    EndpointEntry {
        method: "GET",
        path: "/builds/{slot_id}/last-build-stderr",
        summary: "Latest stderr output for a slot",
    },
    EndpointEntry {
        method: "DELETE",
        path: "/builds/caches",
        summary: "Clear build caches across all pool slots",
    },
    EndpointEntry {
        method: "POST",
        path: "/build/reset",
        summary: "Reset build state",
    },
    // Logs
    EndpointEntry {
        method: "GET",
        path: "/logs/history",
        summary: "Recent log entries from circular buffer",
    },
    EndpointEntry {
        method: "GET",
        path: "/logs/build/history",
        summary: "Recent entries from the build-only buffer",
    },
    EndpointEntry {
        method: "GET",
        path: "/logs/stream",
        summary: "SSE stream of real-time log events",
    },
    EndpointEntry {
        method: "GET",
        path: "/logs/file/{type}",
        summary: "Read .dev-logs/ files",
    },
    EndpointEntry {
        method: "GET",
        path: "/logs/files",
        summary: "List available log files",
    },
    // Expo
    EndpointEntry {
        method: "POST",
        path: "/expo/start",
        summary: "Start Expo dev server",
    },
    EndpointEntry {
        method: "POST",
        path: "/expo/stop",
        summary: "Stop Expo dev server",
    },
    EndpointEntry {
        method: "GET",
        path: "/expo/status",
        summary: "Expo running state, PID, port, configured flag",
    },
    EndpointEntry {
        method: "GET",
        path: "/expo/logs/stream",
        summary: "SSE stream filtered to Expo log source",
    },
    // Velocity (HTTP Span Tracing)
    EndpointEntry {
        method: "POST",
        path: "/velocity/ingest",
        summary: "Ingest HTTP span data",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity/summary",
        summary: "Aggregated latency summary (P50/P95/P99)",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity/endpoints",
        summary: "Per-endpoint latency breakdown",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity/slow",
        summary: "Slowest requests",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity/timeline",
        summary: "Latency over time",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity/compare",
        summary: "Before/after comparison",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity/trace/{request_id}",
        summary: "Detailed trace for a single request",
    },
    // Velocity Tests
    EndpointEntry {
        method: "POST",
        path: "/velocity-tests/start",
        summary: "Start a velocity test run",
    },
    EndpointEntry {
        method: "POST",
        path: "/velocity-tests/stop",
        summary: "Stop a running test",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity-tests/status",
        summary: "Current test status",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity-tests/runs",
        summary: "List past runs",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity-tests/runs/{id}",
        summary: "Get a specific run",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity-tests/trend",
        summary: "Performance trend across runs",
    },
    // Velocity Improvement
    EndpointEntry {
        method: "POST",
        path: "/velocity-improvement/start",
        summary: "Start improvement analysis",
    },
    EndpointEntry {
        method: "POST",
        path: "/velocity-improvement/stop",
        summary: "Stop running analysis",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity-improvement/status",
        summary: "Current analysis status",
    },
    EndpointEntry {
        method: "GET",
        path: "/velocity-improvement/history",
        summary: "Past improvement results",
    },
    // Evaluation
    EndpointEntry {
        method: "POST",
        path: "/eval/start",
        summary: "Start an evaluation run",
    },
    EndpointEntry {
        method: "POST",
        path: "/eval/stop",
        summary: "Stop a running evaluation",
    },
    EndpointEntry {
        method: "GET",
        path: "/eval/status",
        summary: "Current evaluation status",
    },
    EndpointEntry {
        method: "POST",
        path: "/eval/continuous/start",
        summary: "Start continuous evaluation",
    },
    EndpointEntry {
        method: "POST",
        path: "/eval/continuous/stop",
        summary: "Stop continuous evaluation",
    },
    EndpointEntry {
        method: "GET",
        path: "/eval/runs",
        summary: "List past evaluation runs",
    },
    EndpointEntry {
        method: "GET",
        path: "/eval/runs/{id}",
        summary: "Get a specific evaluation run",
    },
    EndpointEntry {
        method: "GET",
        path: "/eval/test-suite",
        summary: "List test prompts",
    },
    EndpointEntry {
        method: "POST",
        path: "/eval/test-suite",
        summary: "Add a test prompt",
    },
    EndpointEntry {
        method: "PUT",
        path: "/eval/test-suite/{id}",
        summary: "Update a test prompt",
    },
    EndpointEntry {
        method: "DELETE",
        path: "/eval/test-suite/{id}",
        summary: "Delete a test prompt",
    },
    // AI Provider/Model Config
    EndpointEntry {
        method: "GET",
        path: "/ai/provider",
        summary: "Get current AI provider/model selection",
    },
    EndpointEntry {
        method: "POST",
        path: "/ai/provider",
        summary: "Set AI provider/model",
    },
    EndpointEntry {
        method: "GET",
        path: "/ai/models",
        summary: "List available AI models",
    },
    // Proxies
    EndpointEntry {
        method: "GET",
        path: "/ui-bridge/{*path}",
        summary: "Proxy GET UI Bridge requests to runner port 9876",
    },
    EndpointEntry {
        method: "POST",
        path: "/ui-bridge/{*path}",
        summary: "Proxy POST UI Bridge requests to runner port 9876",
    },
    EndpointEntry {
        method: "GET",
        path: "/runner-api/{*path}",
        summary: "Proxy GET runner-API requests to runner port 9876",
    },
    EndpointEntry {
        method: "POST",
        path: "/runner-api/{*path}",
        summary: "Proxy POST runner-API requests to runner port 9876",
    },
    EndpointEntry {
        method: "POST",
        path: "/graphql",
        summary: "Proxy GraphQL queries to runner",
    },
    EndpointEntry {
        method: "GET",
        path: "/graphql/ws",
        summary: "Proxy GraphQL WebSocket subscriptions to runner",
    },
    EndpointEntry {
        method: "GET",
        path: "/web-fleet",
        summary: "Proxy listing of qontinui-web fleet",
    },
    // Supervisor Bridge
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/commands/stream",
        summary: "SSE stream of pending commands",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/commands",
        summary: "Submit a command response",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/heartbeat",
        summary: "Dashboard heartbeat",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/boot-id",
        summary: "Stable installation boot id",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/health",
        summary: "Bridge health status",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/status",
        summary: "Bridge status alias",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/control/snapshot",
        summary: "Full snapshot of dashboard UI",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/control/elements",
        summary: "List dashboard UI elements",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/element/{id}/action",
        summary: "Execute action on dashboard element",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/discover",
        summary: "Trigger element discovery",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/control/console-errors",
        summary: "Get console errors from dashboard",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/page/evaluate",
        summary: "Evaluate JS in dashboard webview",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/page/navigate",
        summary: "Navigate dashboard page",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/page/refresh",
        summary: "Refresh dashboard page",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/network/stubs",
        summary: "Register a fetch stub",
    },
    EndpointEntry {
        method: "GET",
        path: "/supervisor-bridge/control/network/stubs",
        summary: "List active stubs",
    },
    EndpointEntry {
        method: "DELETE",
        path: "/supervisor-bridge/control/network/stubs",
        summary: "Clear all stubs",
    },
    EndpointEntry {
        method: "DELETE",
        path: "/supervisor-bridge/control/network/stubs/{id}",
        summary: "Remove one stub by id",
    },
    EndpointEntry {
        method: "POST",
        path: "/supervisor-bridge/control/network/verify-stub",
        summary: "Non-consuming stub verification",
    },
    // Diagnostics
    EndpointEntry {
        method: "GET",
        path: "/diagnostics",
        summary: "Build/restart event history",
    },
    EndpointEntry {
        method: "POST",
        path: "/diagnostics/clear",
        summary: "Clear diagnostic events",
    },
    // Test login
    EndpointEntry {
        method: "GET",
        path: "/test-login",
        summary: "Get test login credentials",
    },
    EndpointEntry {
        method: "POST",
        path: "/test-login",
        summary: "Set test login credentials",
    },
    EndpointEntry {
        method: "DELETE",
        path: "/test-login",
        summary: "Clear test login credentials",
    },
    // LKG coverage helper
    EndpointEntry {
        method: "GET",
        path: "/lkg/coverage",
        summary: "Single-call LKG coverage check for agents",
    },
    // WebSocket
    EndpointEntry {
        method: "GET",
        path: "/ws",
        summary: "WebSocket endpoint",
    },
    // Legacy single-runner endpoints
    EndpointEntry {
        method: "POST",
        path: "/runner/stop",
        summary: "Stop runner (legacy single-runner endpoint)",
    },
    EndpointEntry {
        method: "GET",
        path: "/runner/stop",
        summary: "Stop runner (legacy single-runner endpoint)",
    },
    EndpointEntry {
        method: "POST",
        path: "/runner/restart",
        summary: "Restart runner (legacy single-runner endpoint)",
    },
    EndpointEntry {
        method: "POST",
        path: "/runner/watchdog",
        summary: "Control watchdog (legacy single-runner endpoint)",
    },
    EndpointEntry {
        method: "POST",
        path: "/runner/fix-and-rebuild",
        summary: "Fix errors and rebuild (legacy single-runner endpoint)",
    },
    // Debug-gated
    EndpointEntry {
        method: "POST",
        path: "/control/dev/emit-build-id",
        summary: "Inject synthetic buildId into /health/stream (debug-only)",
    },
];

/// Handler for `GET /help`. Returns the static endpoint manifest as JSON.
pub async fn help_handler() -> impl axum::response::IntoResponse {
    axum::Json(serde_json::json!({ "endpoints": ENDPOINT_MANIFEST }))
}

/// Translate a panic in any axum handler into a 500 JSON response that
/// mirrors `SupervisorError::IntoResponse` (see `src/error.rs`). Without
/// this, a handler panic propagates through `tower::Service::poll_ready` /
/// `call`, axum drops the connection, and clients see `connection reset by
/// peer` with no logging — extremely hostile to debug.
///
/// We can't push a structured entry to `state.logs` here because the
/// `tower-http` `CatchPanicLayer` doesn't have access to `SharedState`. The
/// `tracing::error!` line is enough — every log surface in the supervisor
/// already subscribes to the global tracing subscriber, so the entry shows
/// up in `--log-file` / `--log-dir` output and on the console.
pub fn supervisor_panic_handler(
    err: Box<dyn std::any::Any + Send + 'static>,
) -> axum::response::Response {
    let message = if let Some(s) = err.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    };

    tracing::error!(handler_panic = true, %message, "supervisor handler panicked");

    let body = serde_json::json!({
        "error": format!("supervisor handler panicked: {}", message),
        "panicked": true,
    });
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(body),
    )
        .into_response()
}

pub fn build_router(state: SharedState) -> Router {
    // Resolve dev_logs directory for velocity routes
    let dev_logs_dir = state
        .config
        .project_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&state.config.project_dir)
        .join(".dev-logs");
    let _ = std::fs::create_dir_all(&dev_logs_dir);
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Clone state for stateless routes (they need SharedState; main_routes consumes it)
    let eval_state = state.clone();
    let vt_state = state.clone();
    let vi_state = state.clone();
    let spa_state = state.clone();
    // Debug-only endpoints (gated by QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS=1).
    // Always merged into the router; the gate is enforced inside each
    // handler so a misconfiguration returns 403 (well-defined response)
    // rather than 404 (looks like the supervisor binary is too old).
    let dev_state = state.clone();

    // Build main stateful routes, then apply state to get Router<()>
    let main_routes = Router::new()
        // Dashboard
        .route("/", get(crate::routes::dashboard::index))
        // Endpoint manifest
        .route("/help", get(help_handler))
        // Health
        .route("/health", get(crate::routes::health::health))
        .route("/health/stream", get(crate::routes::health::health_stream))
        // LKG coverage helper for agents — see routes/lkg_coverage.rs.
        // Single-call collapse of the manual "is my fix in the LKG?" rule
        // documented under "Last-known-good (LKG) fallback for agents" in
        // CLAUDE.md.
        .route(
            "/lkg/coverage",
            get(crate::routes::lkg_coverage::lkg_coverage),
        )
        // Runner lifecycle
        .route("/runner/stop", post(crate::routes::runner::stop_runner))
        .route(
            "/runner/restart",
            post(crate::routes::runner::restart_runner),
        )
        .route(
            "/runner/watchdog",
            post(crate::routes::runner::control_watchdog),
        )
        .route(
            "/runner/fix-and-rebuild",
            post(crate::routes::runner::fix_and_rebuild),
        )
        // Logs
        .route("/logs/history", get(crate::routes::logs::log_history))
        .route(
            "/logs/build/history",
            get(crate::routes::logs::build_log_history),
        )
        .route("/logs/stream", get(crate::routes::logs::log_stream))
        .route("/logs/file/{type}", get(crate::routes::logs::log_file))
        .route("/logs/files", get(crate::routes::logs::log_files))
        // Build management
        .route("/build/reset", post(crate::routes::runner::reset_build))
        // Supervisor self-restart
        .route(
            "/supervisor/restart",
            post(crate::routes::runner::supervisor_restart),
        )
        // Supervisor graceful shutdown (scriptable alternative to Stop-Process -Force).
        .route(
            "/supervisor/shutdown",
            post(crate::routes::runner::supervisor_shutdown),
        )
        // AI provider/model management
        .route("/ai/provider", get(crate::routes::ai::get_provider))
        .route("/ai/provider", post(crate::routes::ai::set_provider))
        .route("/ai/models", get(crate::routes::ai::models))
        // Dev-start orchestration removed — frontend/backend are managed by the runner directly
        // Supervisor Bridge (command relay for supervisor's own dashboard UI)
        .route(
            "/supervisor-bridge/commands/stream",
            get(crate::routes::supervisor_bridge::commands_stream),
        )
        .route(
            "/supervisor-bridge/commands",
            post(crate::routes::supervisor_bridge::command_response),
        )
        .route(
            "/supervisor-bridge/heartbeat",
            post(crate::routes::supervisor_bridge::heartbeat),
        )
        .route(
            "/supervisor-bridge/boot-id",
            get(crate::routes::supervisor_bridge::boot_id),
        )
        .route(
            "/supervisor-bridge/control/snapshot",
            get(crate::routes::supervisor_bridge::snapshot),
        )
        .route(
            "/supervisor-bridge/control/elements",
            get(crate::routes::supervisor_bridge::elements),
        )
        .route(
            "/supervisor-bridge/control/element/{id}/action",
            post(crate::routes::supervisor_bridge::element_action),
        )
        .route(
            "/supervisor-bridge/control/discover",
            post(crate::routes::supervisor_bridge::discover),
        )
        .route(
            "/supervisor-bridge/control/console-errors",
            get(crate::routes::supervisor_bridge::console_errors),
        )
        .route(
            "/supervisor-bridge/control/page/evaluate",
            post(crate::routes::supervisor_bridge::page_evaluate),
        )
        .route(
            "/supervisor-bridge/control/page/navigate",
            post(crate::routes::supervisor_bridge::page_navigate),
        )
        .route(
            "/supervisor-bridge/control/page/refresh",
            post(crate::routes::supervisor_bridge::page_refresh),
        )
        // F2 — Network stub registry (mirrors runner's /control/network/stubs*)
        .route(
            "/supervisor-bridge/control/network/stubs",
            post(crate::routes::supervisor_bridge::register_network_stub)
                .get(crate::routes::supervisor_bridge::list_network_stubs)
                .delete(crate::routes::supervisor_bridge::clear_network_stubs),
        )
        .route(
            "/supervisor-bridge/control/network/stubs/{id}",
            delete(crate::routes::supervisor_bridge::delete_network_stub),
        )
        // N3 — Non-consuming stub verification
        .route(
            "/supervisor-bridge/control/network/verify-stub",
            post(crate::routes::supervisor_bridge::verify_network_stub),
        )
        .route(
            "/supervisor-bridge/health",
            get(crate::routes::supervisor_bridge::bridge_health),
        )
        .route(
            "/supervisor-bridge/status",
            get(crate::routes::supervisor_bridge::bridge_health),
        )
        // GraphQL proxy (forwards to runner at port 9876)
        .route(
            "/graphql",
            post(crate::routes::graphql_proxy::graphql_proxy),
        )
        .route(
            "/graphql/ws",
            get(crate::routes::graphql_proxy::graphql_ws_proxy),
        )
        // UI Bridge proxy (forwards to runner at port 9876)
        .route(
            "/ui-bridge/{*path}",
            get(crate::routes::ui_bridge::proxy).post(crate::routes::ui_bridge::proxy),
        )
        // Runner API proxy (forwards to runner at port 9876)
        .route(
            "/runner-api/{*path}",
            get(crate::routes::runner_monitor::proxy).post(crate::routes::runner_monitor::proxy),
        )
        // Web Fleet proxy (forwards to the user-supplied qontinui-web backend)
        .route("/web-fleet", get(crate::routes::web_fleet::list_web_fleet))
        // WebSocket
        .route("/ws", get(crate::routes::ws::ws_handler))
        // Diagnostics
        .route(
            "/diagnostics",
            get(crate::routes::diagnostics::get_diagnostics),
        )
        .route(
            "/diagnostics/clear",
            post(crate::routes::diagnostics::clear_diagnostics),
        )
        // Expo
        .route("/expo/start", post(crate::routes::expo::start))
        .route("/expo/stop", post(crate::routes::expo::stop))
        .route("/expo/status", get(crate::routes::expo::status))
        .route("/expo/logs/stream", get(crate::routes::expo::logs_stream))
        // Multi-runner management
        .route("/runners", get(crate::routes::runners::list_runners))
        .route("/runners", post(crate::routes::runners::add_runner))
        .route(
            "/runners/spawn-test",
            post(crate::routes::runners::spawn_test),
        )
        .route(
            "/runners/spawn-named",
            post(crate::routes::runners::spawn_named),
        )
        .route(
            "/test-login",
            post(crate::routes::runners::set_test_login)
                .get(crate::routes::runners::get_test_login)
                .delete(crate::routes::runners::clear_test_login),
        )
        // Row 2 Phase 3 — generalized build pool. `POST /build/submit`
        // accepts an external worktree + cargo command; `GET /build/:id/status`
        // returns the submission state. Shares the build_pool permits with
        // spawn-test (`POST /runners/spawn-test`).
        .route(
            "/build/submit",
            post(crate::routes::build_submit::post_submit),
        )
        .route(
            "/build/{id}/status",
            get(crate::routes::build_submit::get_status),
        )
        .route("/builds", get(crate::routes::runners::list_builds))
        .route(
            "/builds/{slot_id}/log",
            get(crate::routes::runners::slot_build_log),
        )
        .route(
            "/builds/{slot_id}/last-build-stderr",
            get(crate::routes::runners::slot_last_build_stderr),
        )
        .route(
            "/builds/caches",
            delete(crate::routes::runners::clear_build_caches),
        )
        .route(
            "/runners/purge-stale",
            post(crate::routes::runners::purge_stale),
        )
        .route(
            "/runners/{id}",
            delete(crate::routes::runners::remove_runner),
        )
        .route(
            "/runners/{id}/start",
            post(crate::routes::runners::start_runner),
        )
        .route(
            "/runners/{id}/stop",
            post(crate::routes::runners::stop_runner),
        )
        .route(
            "/runners/{id}/restart",
            post(crate::routes::runners::restart_runner),
        )
        .route(
            "/runners/{id}/rebuild-and-restart",
            post(crate::routes::runners::rebuild_and_restart_runner),
        )
        .route(
            "/runners/{id}/watchdog",
            post(crate::routes::runners::control_runner_watchdog),
        )
        .route(
            "/runners/{id}/protect",
            post(crate::routes::runners::protect_runner),
        )
        .route(
            "/runners/{id}/crash-dump",
            get(crate::routes::runners::runner_crash_dump),
        )
        .route(
            "/runners/{id}/early-log",
            get(crate::routes::runners::runner_early_log),
        )
        .route(
            "/runners/{id}/crash-summary",
            get(crate::routes::runners::runner_crash_summary),
        )
        .route(
            "/runners/{id}/logs",
            get(crate::routes::runners::runner_log_history),
        )
        .route(
            "/runners/{id}/logs/stream",
            get(crate::routes::runners::runner_log_stream),
        )
        .route(
            "/runners/{id}/ui-bridge/{*path}",
            get(crate::routes::runners::proxy_ui_bridge)
                .post(crate::routes::runners::proxy_ui_bridge),
        )
        // Spec API (Section 3 / Phase B5c). Mounted at `/spec/...` for
        // parity with the runner's spec_api. Storage root defaults to
        // `<supervisor>/frontend/specs/` (override via QONTINUI_SPECS_ROOT).
        .merge(crate::spec_api::routes())
        .with_state(state);

    // Merge stateless routers (velocity/eval have their own state, SPA has none)
    main_routes
        .merge(crate::routes::velocity::velocity_routes(
            dev_logs_dir.clone(),
        ))
        .merge(crate::routes::evaluation::eval_routes(
            dev_logs_dir.clone(),
            eval_state,
        ))
        .merge(crate::routes::velocity_tests::velocity_test_routes(
            dev_logs_dir.clone(),
            vt_state,
        ))
        .merge(
            crate::routes::velocity_improvement::velocity_improvement_routes(
                dev_logs_dir,
                vi_state,
            ),
        )
        .merge(crate::routes::dashboard::spa_routes(spa_state))
        .merge(crate::routes::dev_endpoints::router(dev_state))
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        // CatchPanicLayer wraps everything below it (tower applies layers
        // bottom-up, so this is the OUTERMOST layer). A panic anywhere in
        // a handler — including a deeper layer — is converted into a 500
        // JSON response mirroring SupervisorError. See
        // `supervisor_panic_handler` for the body shape.
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            supervisor_panic_handler,
        ))
}
