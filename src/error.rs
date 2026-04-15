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
        /// Rough hint for how many seconds the caller should wait before
        /// retrying. Computed as `avg_build_duration - min(elapsed)` over
        /// active slots. `None` when no history exists yet.
        estimated_wait_secs: Option<f64>,
    },

    #[error("Build failed: {0}")]
    BuildFailed(String),

    #[error("Process error: {0}")]
    Process(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Runner API error: {0}")]
    RunnerApi(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("{0}")]
    Other(String),
}

impl IntoResponse for SupervisorError {
    fn into_response(self) -> Response {
        if let SupervisorError::BuildPoolFull {
            queue_position,
            active_builds,
            estimated_wait_secs,
        } = &self
        {
            let mut body = serde_json::json!({
                "error": "build_pool_full",
                "message": self.to_string(),
                "queue_position": queue_position,
                "active_builds": active_builds,
            });
            if let Some(w) = estimated_wait_secs {
                body.as_object_mut()
                    .unwrap()
                    .insert("estimated_wait_secs".to_string(), serde_json::json!(w));
            }
            return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response();
        }

        let status = match &self {
            SupervisorError::RunnerNotRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerAlreadyRunning => StatusCode::CONFLICT,
            SupervisorError::RunnerNotFound(_) => StatusCode::NOT_FOUND,
            SupervisorError::BuildInProgress => StatusCode::CONFLICT,
            SupervisorError::BuildPoolFull { .. } => StatusCode::SERVICE_UNAVAILABLE,
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
