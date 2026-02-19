use axum::extract::State;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;
use crate::velocity_improvement::{
    VelocityImprovementConfig, VelocityImprovementHistory, VelocityImprovementPhase,
    VelocityImprovementStatus,
};
use crate::velocity_tests::db::VelocityTestDb;

// ============================================================================
// State
// ============================================================================

pub struct ViRouteState {
    pub db: Arc<VelocityTestDb>,
    #[allow(dead_code)]
    pub dev_logs_dir: PathBuf,
    pub supervisor: SharedState,
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, serde::Serialize)]
pub struct MessageResponse {
    pub ok: bool,
    pub message: String,
}

// ============================================================================
// Routes
// ============================================================================

pub fn velocity_improvement_routes(dev_logs_dir: PathBuf, supervisor: SharedState) -> Router {
    let db = match VelocityTestDb::new(&dev_logs_dir) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!(
                "Failed to initialize velocity test database for improvement loop: {}",
                e
            );
            return Router::new();
        }
    };

    let state = Arc::new(ViRouteState {
        db: Arc::new(db),
        dev_logs_dir,
        supervisor,
    });

    Router::new()
        .route("/velocity-improvement/start", post(start_handler))
        .route("/velocity-improvement/stop", post(stop_handler))
        .route("/velocity-improvement/status", get(status_handler))
        .route("/velocity-improvement/history", get(history_handler))
        .with_state(state)
}

// ============================================================================
// Handlers
// ============================================================================

async fn start_handler(
    State(state): State<Arc<ViRouteState>>,
    Json(config): Json<VelocityImprovementConfig>,
) -> Json<MessageResponse> {
    // Create stop channel
    let (stop_tx, stop_rx) = watch::channel(false);

    // Atomically check if running and set running = true
    {
        let mut vi = state.supervisor.velocity_improvement.write().await;
        if vi.running {
            return Json(MessageResponse {
                ok: false,
                message: "Velocity improvement loop is already running".to_string(),
            });
        }
        vi.running = true;
        vi.phase = VelocityImprovementPhase::RunningTests;
        vi.current_iteration = 0;
        vi.max_iterations = config.max_iterations;
        vi.target_score = config.target_score;
        vi.started_at = Some(chrono::Utc::now());
        vi.error = None;
        vi.iterations.clear();
        vi.stop_tx = Some(stop_tx);
    }

    state
        .supervisor
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Velocity improvement loop started: max_iterations={}, target_score={:.0}",
                config.max_iterations, config.target_score
            ),
        )
        .await;

    let db = state.db.clone();
    let supervisor = state.supervisor.clone();

    tokio::spawn(async move {
        crate::velocity_improvement::run_velocity_improvement_loop(db, supervisor, config, stop_rx)
            .await;
    });

    Json(MessageResponse {
        ok: true,
        message: "Velocity improvement loop started".to_string(),
    })
}

async fn stop_handler(State(state): State<Arc<ViRouteState>>) -> Json<MessageResponse> {
    let mut vi = state.supervisor.velocity_improvement.write().await;
    if !vi.running {
        return Json(MessageResponse {
            ok: false,
            message: "Velocity improvement loop is not running".to_string(),
        });
    }

    if let Some(tx) = vi.stop_tx.take() {
        let _ = tx.send(true);
    }

    Json(MessageResponse {
        ok: true,
        message: "Stop signal sent".to_string(),
    })
}

async fn status_handler(State(state): State<Arc<ViRouteState>>) -> Json<VelocityImprovementStatus> {
    let vi = state.supervisor.velocity_improvement.read().await;
    Json(VelocityImprovementStatus {
        running: vi.running,
        phase: vi.phase.clone(),
        current_iteration: vi.current_iteration,
        max_iterations: vi.max_iterations,
        target_score: vi.target_score,
        started_at: vi.started_at.map(|dt| dt.to_rfc3339()),
        error: vi.error.clone(),
    })
}

async fn history_handler(
    State(state): State<Arc<ViRouteState>>,
) -> Json<VelocityImprovementHistory> {
    let vi = state.supervisor.velocity_improvement.read().await;
    Json(VelocityImprovementHistory {
        iterations: vi.iterations.clone(),
    })
}
