use axum::extract::{Path, State};
use axum::response::Json;
use axum::routing::{delete, get, post, put};
use axum::Router;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

use crate::evaluation::db::EvalDb;
use crate::evaluation::{self, EvalRunWithResults, EvalStatus, TestPrompt};
use crate::state::SharedState;

// ============================================================================
// State
// ============================================================================

pub struct EvalState {
    pub db: Arc<EvalDb>,
    #[allow(dead_code)]
    pub dev_logs_dir: PathBuf,
    pub supervisor: SharedState,
}

// ============================================================================
// Request/Response types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub prompt_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ContinuousStartRequest {
    pub interval_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub ok: bool,
    pub message: String,
}

// ============================================================================
// Routes
// ============================================================================

pub fn eval_routes(dev_logs_dir: PathBuf, supervisor: SharedState) -> Router {
    let db = match EvalDb::new(&dev_logs_dir) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("Failed to initialize eval database: {}", e);
            return Router::new();
        }
    };

    let state = Arc::new(EvalState {
        db: Arc::new(db),
        dev_logs_dir,
        supervisor,
    });

    Router::new()
        .route("/eval/status", get(status_handler))
        .route("/eval/start", post(start_handler))
        .route("/eval/stop", post(stop_handler))
        .route("/eval/continuous/start", post(continuous_start_handler))
        .route("/eval/continuous/stop", post(continuous_stop_handler))
        .route("/eval/runs", get(list_runs_handler))
        .route("/eval/runs/{id}", get(get_run_handler))
        .route(
            "/eval/runs/{id}/compare/{baseline_id}",
            get(compare_handler),
        )
        .route("/eval/test-suite", get(list_test_suite_handler))
        .route("/eval/test-suite", post(add_test_prompt_handler))
        .route("/eval/test-suite/{id}", put(update_test_prompt_handler))
        .route("/eval/test-suite/{id}", delete(delete_test_prompt_handler))
        .with_state(state)
}

// ============================================================================
// Handlers
// ============================================================================

async fn status_handler(State(state): State<Arc<EvalState>>) -> Json<EvalStatus> {
    let eval = state.supervisor.evaluation.read().await;
    Json(EvalStatus {
        running: eval.running,
        current_run_id: eval.current_run_id.clone(),
        continuous_mode: eval.continuous_mode,
        continuous_interval_secs: eval.continuous_interval_secs,
        current_prompt_index: eval.current_prompt_index,
        total_prompts: eval.total_prompts,
    })
}

async fn start_handler(
    State(state): State<Arc<EvalState>>,
    Json(body): Json<StartRequest>,
) -> Json<MessageResponse> {
    // Check if already running
    {
        let eval = state.supervisor.evaluation.read().await;
        if eval.running {
            return Json(MessageResponse {
                ok: false,
                message: "Eval run already in progress".to_string(),
            });
        }
    }

    // Create stop channel
    let (stop_tx, stop_rx) = watch::channel(false);

    // Store stop sender
    {
        let mut eval = state.supervisor.evaluation.write().await;
        eval.stop_tx = Some(stop_tx);
    }

    let db = state.db.clone();
    let supervisor = state.supervisor.clone();
    let prompt_ids = body.prompt_ids;

    tokio::spawn(async move {
        evaluation::engine::run_eval(db, supervisor, prompt_ids, stop_rx).await;
    });

    Json(MessageResponse {
        ok: true,
        message: "Eval run started".to_string(),
    })
}

async fn stop_handler(State(state): State<Arc<EvalState>>) -> Json<MessageResponse> {
    let mut eval = state.supervisor.evaluation.write().await;
    if !eval.running {
        return Json(MessageResponse {
            ok: false,
            message: "No eval run in progress".to_string(),
        });
    }

    if let Some(tx) = eval.stop_tx.take() {
        let _ = tx.send(true);
    }

    Json(MessageResponse {
        ok: true,
        message: "Stop signal sent".to_string(),
    })
}

async fn continuous_start_handler(
    State(state): State<Arc<EvalState>>,
    Json(body): Json<ContinuousStartRequest>,
) -> Json<MessageResponse> {
    {
        let eval = state.supervisor.evaluation.read().await;
        if eval.running {
            return Json(MessageResponse {
                ok: false,
                message: "Eval run already in progress".to_string(),
            });
        }
    }

    let interval_secs = body.interval_secs.unwrap_or(3600);

    let (stop_tx, stop_rx) = watch::channel(false);

    {
        let mut eval = state.supervisor.evaluation.write().await;
        eval.continuous_mode = true;
        eval.continuous_interval_secs = interval_secs;
        eval.stop_tx = Some(stop_tx);
    }

    let db = state.db.clone();
    let supervisor = state.supervisor.clone();

    tokio::spawn(async move {
        evaluation::engine::run_continuous(db, supervisor, interval_secs, stop_rx).await;
    });

    Json(MessageResponse {
        ok: true,
        message: format!("Continuous eval started with {}s interval", interval_secs),
    })
}

async fn continuous_stop_handler(State(state): State<Arc<EvalState>>) -> Json<MessageResponse> {
    let mut eval = state.supervisor.evaluation.write().await;
    if !eval.continuous_mode {
        return Json(MessageResponse {
            ok: false,
            message: "Continuous mode not active".to_string(),
        });
    }

    if let Some(tx) = eval.stop_tx.take() {
        let _ = tx.send(true);
    }
    eval.continuous_mode = false;

    Json(MessageResponse {
        ok: true,
        message: "Continuous eval stopping".to_string(),
    })
}

async fn list_runs_handler(
    State(state): State<Arc<EvalState>>,
) -> Json<Vec<evaluation::EvalRunSummary>> {
    match evaluation::queries::list_runs(&state.db) {
        Ok(runs) => Json(runs),
        Err(e) => {
            tracing::error!("Failed to list eval runs: {}", e);
            Json(Vec::new())
        }
    }
}

async fn get_run_handler(
    State(state): State<Arc<EvalState>>,
    Path(id): Path<String>,
) -> Json<Option<EvalRunWithResults>> {
    let run = match state.db.get_eval_run(&id) {
        Ok(Some(r)) => r,
        Ok(None) => return Json(None),
        Err(e) => {
            tracing::error!("Failed to get eval run: {}", e);
            return Json(None);
        }
    };

    let results = match state.db.get_results_for_run(&id) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get eval results: {}", e);
            Vec::new()
        }
    };

    Json(Some(EvalRunWithResults { run, results }))
}

async fn compare_handler(
    State(state): State<Arc<EvalState>>,
    Path((id, baseline_id)): Path<(String, String)>,
) -> Json<Option<evaluation::CompareReport>> {
    match evaluation::queries::compare_runs(&state.db, &id, &baseline_id) {
        Ok(report) => Json(Some(report)),
        Err(e) => {
            tracing::error!("Failed to compare runs: {}", e);
            Json(None)
        }
    }
}

async fn list_test_suite_handler(State(state): State<Arc<EvalState>>) -> Json<Vec<TestPrompt>> {
    match state.db.list_test_prompts() {
        Ok(prompts) => Json(prompts),
        Err(e) => {
            tracing::error!("Failed to list test prompts: {}", e);
            Json(Vec::new())
        }
    }
}

async fn add_test_prompt_handler(
    State(state): State<Arc<EvalState>>,
    Json(mut prompt): Json<TestPrompt>,
) -> Json<MessageResponse> {
    let now = Utc::now().to_rfc3339();
    if prompt.created_at.is_empty() {
        prompt.created_at = now.clone();
    }
    if prompt.updated_at.is_empty() {
        prompt.updated_at = now;
    }

    match state.db.insert_test_prompt(&prompt) {
        Ok(()) => Json(MessageResponse {
            ok: true,
            message: format!("Test prompt '{}' added", prompt.id),
        }),
        Err(e) => Json(MessageResponse {
            ok: false,
            message: format!("Failed to add test prompt: {}", e),
        }),
    }
}

async fn update_test_prompt_handler(
    State(state): State<Arc<EvalState>>,
    Path(id): Path<String>,
    Json(mut prompt): Json<TestPrompt>,
) -> Json<MessageResponse> {
    prompt.updated_at = Utc::now().to_rfc3339();

    match state.db.update_test_prompt(&id, &prompt) {
        Ok(true) => Json(MessageResponse {
            ok: true,
            message: format!("Test prompt '{}' updated", id),
        }),
        Ok(false) => Json(MessageResponse {
            ok: false,
            message: format!("Test prompt '{}' not found", id),
        }),
        Err(e) => Json(MessageResponse {
            ok: false,
            message: format!("Failed to update: {}", e),
        }),
    }
}

async fn delete_test_prompt_handler(
    State(state): State<Arc<EvalState>>,
    Path(id): Path<String>,
) -> Json<MessageResponse> {
    match state.db.delete_test_prompt(&id) {
        Ok(true) => Json(MessageResponse {
            ok: true,
            message: format!("Test prompt '{}' deleted", id),
        }),
        Ok(false) => Json(MessageResponse {
            ok: false,
            message: format!("Test prompt '{}' not found", id),
        }),
        Err(e) => Json(MessageResponse {
            ok: false,
            message: format!("Failed to delete: {}", e),
        }),
    }
}
