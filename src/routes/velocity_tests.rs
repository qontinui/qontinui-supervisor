use axum::extract::{Path, State};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

use crate::state::SharedState;
use crate::velocity_tests::db::VelocityTestDb;
use crate::velocity_tests::{
    VelocityTestRunWithResults, VelocityTestStatus, VelocityTestTrendPoint,
};

// ============================================================================
// State
// ============================================================================

pub struct VtRouteState {
    pub db: Arc<VelocityTestDb>,
    #[allow(dead_code)]
    pub dev_logs_dir: PathBuf,
    pub supervisor: SharedState,
}

// ============================================================================
// Request/Response types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct TrendQuery {
    pub limit: Option<i64>,
}

// ============================================================================
// Routes
// ============================================================================

pub fn velocity_test_routes(dev_logs_dir: PathBuf, supervisor: SharedState) -> Router {
    let db = match VelocityTestDb::new(&dev_logs_dir) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("Failed to initialize velocity test database: {}", e);
            return Router::new();
        }
    };

    let state = Arc::new(VtRouteState {
        db: Arc::new(db),
        dev_logs_dir,
        supervisor,
    });

    Router::new()
        .route("/velocity-tests/start", post(start_handler))
        .route("/velocity-tests/stop", post(stop_handler))
        .route("/velocity-tests/status", get(status_handler))
        .route("/velocity-tests/runs", get(list_runs_handler))
        .route("/velocity-tests/runs/{id}", get(get_run_handler))
        .route("/velocity-tests/trend", get(trend_handler))
        .with_state(state)
}

// ============================================================================
// Handlers
// ============================================================================

async fn start_handler(State(state): State<Arc<VtRouteState>>) -> Json<MessageResponse> {
    // Create stop channel
    let (stop_tx, stop_rx) = watch::channel(false);

    // Atomically check if running and set running = true + store stop sender
    {
        let mut vt = state.supervisor.velocity_tests.write().await;
        if vt.running {
            return Json(MessageResponse {
                ok: false,
                message: "Velocity tests already running".to_string(),
            });
        }
        vt.running = true;
        vt.stop_tx = Some(stop_tx);
    }

    let db = state.db.clone();
    let supervisor = state.supervisor.clone();

    tokio::spawn(async move {
        crate::velocity_tests::engine::run_velocity_tests(db, supervisor, stop_rx).await;
    });

    Json(MessageResponse {
        ok: true,
        message: "Velocity tests started".to_string(),
    })
}

async fn stop_handler(State(state): State<Arc<VtRouteState>>) -> Json<MessageResponse> {
    let mut vt = state.supervisor.velocity_tests.write().await;
    if !vt.running {
        return Json(MessageResponse {
            ok: false,
            message: "No velocity tests in progress".to_string(),
        });
    }

    if let Some(tx) = vt.stop_tx.take() {
        let _ = tx.send(true);
    }

    Json(MessageResponse {
        ok: true,
        message: "Stop signal sent".to_string(),
    })
}

async fn status_handler(State(state): State<Arc<VtRouteState>>) -> Json<VelocityTestStatus> {
    let vt = state.supervisor.velocity_tests.read().await;
    Json(VelocityTestStatus {
        running: vt.running,
        current_run_id: vt.current_run_id.clone(),
        current_test_index: vt.current_test_index,
        total_tests: vt.total_tests,
    })
}

async fn list_runs_handler(
    State(state): State<Arc<VtRouteState>>,
) -> Json<Vec<crate::velocity_tests::VelocityTestRun>> {
    match state.db.list_runs() {
        Ok(runs) => Json(runs),
        Err(e) => {
            tracing::error!("Failed to list velocity test runs: {}", e);
            Json(Vec::new())
        }
    }
}

async fn get_run_handler(
    State(state): State<Arc<VtRouteState>>,
    Path(id): Path<String>,
) -> Json<Option<VelocityTestRunWithResults>> {
    let run = match state.db.get_run(&id) {
        Ok(Some(r)) => r,
        Ok(None) => return Json(None),
        Err(e) => {
            tracing::error!("Failed to get velocity test run: {}", e);
            return Json(None);
        }
    };

    let results = match state.db.get_results_for_run(&id) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get velocity test results: {}", e);
            Vec::new()
        }
    };

    Json(Some(VelocityTestRunWithResults { run, results }))
}

async fn trend_handler(
    State(state): State<Arc<VtRouteState>>,
    axum::extract::Query(query): axum::extract::Query<TrendQuery>,
) -> Json<Vec<VelocityTestTrendPoint>> {
    let limit = query.limit.unwrap_or(20);
    match state.db.get_trend(limit) {
        Ok(points) => Json(points),
        Err(e) => {
            tracing::error!("Failed to get velocity test trend: {}", e);
            Json(Vec::new())
        }
    }
}
