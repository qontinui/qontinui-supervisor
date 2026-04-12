use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::state::SharedState;

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

    // Build main stateful routes, then apply state to get Router<()>
    let main_routes = Router::new()
        // Dashboard
        .route("/", get(crate::routes::dashboard::index))
        // Health
        .route("/health", get(crate::routes::health::health))
        .route("/health/stream", get(crate::routes::health::health_stream))
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
        // AI debug
        .route("/ai/debug", post(crate::routes::ai::debug))
        .route("/ai/auto-debug", post(crate::routes::ai::auto_debug))
        .route("/ai/status", get(crate::routes::ai::status))
        .route("/ai/stop", post(crate::routes::ai::stop))
        .route("/ai/provider", get(crate::routes::ai::get_provider))
        .route("/ai/provider", post(crate::routes::ai::set_provider))
        .route("/ai/models", get(crate::routes::ai::models))
        .route("/ai/output/stream", get(crate::routes::ai::output_stream))
        // Claude backward-compat aliases
        .route("/claude/debug", post(crate::routes::ai::debug))
        .route("/claude/status", get(crate::routes::ai::status))
        .route("/claude/stop", post(crate::routes::ai::stop))
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
        .route(
            "/supervisor-bridge/health",
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
        // WebSocket
        .route("/ws", get(crate::routes::ws::ws_handler))
        // Workflow loop
        .route(
            "/workflow-loop/start",
            post(crate::routes::workflow_loop::start),
        )
        .route(
            "/workflow-loop/stop",
            post(crate::routes::workflow_loop::stop),
        )
        .route(
            "/workflow-loop/status",
            get(crate::routes::workflow_loop::status),
        )
        .route(
            "/workflow-loop/history",
            get(crate::routes::workflow_loop::history),
        )
        .route(
            "/workflow-loop/loops",
            get(crate::routes::workflow_loop::list_loops),
        )
        .route(
            "/workflow-loop/stream",
            get(crate::routes::workflow_loop::stream),
        )
        .route(
            "/workflow-loop/signal-restart",
            post(crate::routes::workflow_loop::signal_restart),
        )
        .route(
            "/workflow-loop/checkpoints/{task_run_id}",
            get(crate::routes::workflow_loop::get_checkpoints),
        )
        .route(
            "/workflow-loop/breakpoints/{task_run_id}",
            get(crate::routes::workflow_loop::get_breakpoints),
        )
        .route(
            "/workflow-loop/breakpoints/{task_run_id}/{snapshot_id}",
            get(crate::routes::workflow_loop::get_breakpoint_detail),
        )
        .route(
            "/workflow-loop/breakpoints/{task_run_id}/{snapshot_id}/resume",
            post(crate::routes::workflow_loop::resume_breakpoint),
        )
        // Cascade detection events
        .route("/cascade/events", get(crate::routes::cascade::events))
        .route("/cascade/stream", get(crate::routes::cascade::stream))
        // Diagnostics
        .route(
            "/diagnostics",
            get(crate::routes::diagnostics::get_diagnostics),
        )
        .route(
            "/diagnostics/clear",
            post(crate::routes::diagnostics::clear_diagnostics),
        )
        // Smart rebuild
        .route(
            "/smart-rebuild/status",
            get(crate::routes::smart_rebuild::status),
        )
        .route(
            "/smart-rebuild/enable",
            post(crate::routes::smart_rebuild::enable),
        )
        .route(
            "/smart-rebuild/trigger",
            post(crate::routes::smart_rebuild::trigger),
        )
        .route(
            "/smart-rebuild/stop",
            post(crate::routes::smart_rebuild::stop),
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
            "/test-login",
            post(crate::routes::runners::set_test_login)
                .get(crate::routes::runners::get_test_login)
                .delete(crate::routes::runners::clear_test_login),
        )
        .route("/builds", get(crate::routes::runners::list_builds))
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
            "/runners/{id}/watchdog",
            post(crate::routes::runners::control_runner_watchdog),
        )
        .route(
            "/runners/{id}/protect",
            post(crate::routes::runners::protect_runner),
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
        .merge(crate::routes::dashboard::spa_routes())
        .layer(TraceLayer::new_for_http())
        .layer(cors)
}
