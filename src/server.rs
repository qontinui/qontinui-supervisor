use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};

use crate::state::SharedState;

pub fn build_router(state: SharedState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // Health
        .route("/health", get(crate::routes::health::health))
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
        .layer(cors)
        .with_state(state)
}
