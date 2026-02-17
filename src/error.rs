use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum SupervisorError {
    #[error("Runner is not running")]
    RunnerNotRunning,

    #[error("Runner is already running")]
    RunnerAlreadyRunning,

    #[error("Build in progress")]
    BuildInProgress,

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
        let status = match &self {
            SupervisorError::RunnerNotRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerAlreadyRunning => StatusCode::CONFLICT,
            SupervisorError::BuildInProgress => StatusCode::CONFLICT,
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
