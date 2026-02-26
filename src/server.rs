use axum::routing::{get, post};
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
        // Logs
        .route("/logs/history", get(crate::routes::logs::log_history))
        .route("/logs/stream", get(crate::routes::logs::log_stream))
        .route("/logs/file/{type}", get(crate::routes::logs::log_file))
        .route("/logs/files", get(crate::routes::logs::log_files))
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
        // Dev-start orchestration
        .route(
            "/dev-start/backend",
            post(crate::routes::dev_start::backend),
        )
        .route(
            "/dev-start/backend/stop",
            post(crate::routes::dev_start::backend_stop),
        )
        .route(
            "/dev-start/frontend",
            post(crate::routes::dev_start::frontend),
        )
        .route(
            "/dev-start/frontend/stop",
            post(crate::routes::dev_start::frontend_stop),
        )
        .route(
            "/dev-start/frontend/clear-cache",
            post(crate::routes::dev_start::frontend_clear_cache),
        )
        .route("/dev-start/docker", post(crate::routes::dev_start::docker))
        .route(
            "/dev-start/docker/stop",
            post(crate::routes::dev_start::docker_stop),
        )
        .route("/dev-start/all", post(crate::routes::dev_start::all))
        .route("/dev-start/stop", post(crate::routes::dev_start::stop))
        .route("/dev-start/clean", post(crate::routes::dev_start::clean))
        .route("/dev-start/fresh", post(crate::routes::dev_start::fresh))
        .route(
            "/dev-start/migrate",
            post(crate::routes::dev_start::migrate),
        )
        .route("/dev-start/status", get(crate::routes::dev_start::status))
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
            "/workflow-loop/stream",
            get(crate::routes::workflow_loop::stream),
        )
        .route(
            "/workflow-loop/signal-restart",
            post(crate::routes::workflow_loop::signal_restart),
        )
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
