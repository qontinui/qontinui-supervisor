use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum SupervisorError {
    #[error("Runner is not running")]
    RunnerNotRunning,

    #[error("Runner is already running")]
    RunnerAlreadyRunning,

    #[error("Runner not found: {0}")]
    RunnerNotFound(String),

    /// Legacy variant — retained for call sites in `process/manager.rs` and
    /// `routes/runner.rs` that report "build currently in progress". With the
    /// parallel build pool, this is derived from "any slot busy".
    #[error("Build in progress")]
    BuildInProgress,

    /// All build pool slots are busy AND the caller opted out of waiting via
    /// `X-Queue-Mode: no-wait`. Body carries queue position and active build
    /// slot info for the caller to decide whether to retry or skip.
    #[error("Build pool full: all slots busy")]
    BuildPoolFull {
        queue_position: usize,
        active_builds: Vec<serde_json::Value>,
    },

    #[error("Build failed: {0}")]
    BuildFailed(String),

    #[error("Process error: {0}")]
    Process(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Workflow loop is already running")]
    WorkflowLoopAlreadyRunning,

    #[error("Workflow loop is not running")]
    WorkflowLoopNotRunning,

    #[error("Runner API error: {0}")]
    RunnerApi(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("{0}")]
    Other(String),
}

impl IntoResponse for SupervisorError {
    fn into_response(self) -> Response {
        match &self {
            SupervisorError::BuildPoolFull {
                queue_position,
                active_builds,
            } => {
                let body = serde_json::json!({
                    "error": "build_pool_full",
                    "message": self.to_string(),
                    "queue_position": queue_position,
                    "active_builds": active_builds,
                });
                return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response();
            }
            _ => {}
        }

        let status = match &self {
            SupervisorError::RunnerNotRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerAlreadyRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerNotFound(_) => StatusCode::NOT_FOUND,
            SupervisorError::BuildInProgress => StatusCode::CONFLICT,
            SupervisorError::BuildPoolFull { .. } => StatusCode::SERVICE_UNAVAILABLE,
            SupervisorError::WorkflowLoopAlreadyRunning => StatusCode::CONFLICT,
            SupervisorError::WorkflowLoopNotRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerApi(_) => StatusCode::BAD_GATEWAY,
            SupervisorError::BuildFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SupervisorError::Process(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SupervisorError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            SupervisorError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            SupervisorError::Validation(_) => StatusCode::BAD_REQUEST,
            SupervisorError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({
            "error": self.to_string(),
        });

        (status, axum::Json(body)).into_response()
    }
}
