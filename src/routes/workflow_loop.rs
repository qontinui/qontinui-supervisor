use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream::unfold;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::watch;

use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;
use crate::workflow_loop::{self, LoopPhase, WorkflowLoopConfig};

/// POST /workflow-loop/start
pub async fn start(
    State(state): State<SharedState>,
    Json(config): Json<WorkflowLoopConfig>,
) -> Result<impl IntoResponse, SupervisorError> {
    // Validate config: pipeline mode vs simple mode
    if let Some(phases) = &config.phases {
        // Pipeline mode: need either build or execute_workflow_id
        if phases.build.is_none() && phases.execute_workflow_id.is_none() {
            return Err(SupervisorError::Validation(
                "Pipeline mode requires either a build phase or execute_workflow_id".to_string(),
            ));
        }
    } else {
        // Simple mode: workflow_id and exit_strategy required
        if config.workflow_id.is_none() {
            return Err(SupervisorError::Validation(
                "workflow_id is required in simple mode (when phases is absent)".to_string(),
            ));
        }
        if config.exit_strategy.is_none() {
            return Err(SupervisorError::Validation(
                "exit_strategy is required in simple mode (when phases is absent)".to_string(),
            ));
        }
    }

    // Guard: not already running
    {
        let wl = state.workflow_loop.read().await;
        if wl.running {
            return Err(SupervisorError::WorkflowLoopAlreadyRunning);
        }
    }

    // Create stop channel
    let (stop_tx, stop_rx) = watch::channel(false);

    let mode = if config.phases.is_some() {
        "pipeline"
    } else {
        "simple"
    };
    let display_id = config
        .workflow_id
        .clone()
        .or_else(|| {
            config
                .phases
                .as_ref()
                .and_then(|p| p.execute_workflow_id.clone())
        })
        .unwrap_or_else(|| "(generated)".to_string());

    // Initialize state
    {
        let mut wl = state.workflow_loop.write().await;
        wl.running = true;
        wl.config = Some(config.clone());
        wl.current_iteration = 0;
        wl.phase = LoopPhase::RunningWorkflow;
        wl.started_at = Some(chrono::Utc::now());
        wl.error = None;
        wl.iteration_results.clear();
        wl.stop_tx = Some(stop_tx);
    }

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!(
                "Workflow loop started: mode={}, workflow={}, max_iterations={}",
                mode, display_id, config.max_iterations
            ),
        )
        .await;

    // Spawn the loop as a background task
    let state_clone = state.clone();
    tokio::spawn(async move {
        workflow_loop::run_loop(state_clone, stop_rx).await;
    });

    Ok(Json(serde_json::json!({
        "status": "started",
        "mode": mode,
        "workflow_id": display_id,
        "max_iterations": config.max_iterations,
    })))
}

/// POST /workflow-loop/stop
pub async fn stop(State(state): State<SharedState>) -> Result<impl IntoResponse, SupervisorError> {
    let stop_tx = {
        let wl = state.workflow_loop.read().await;
        if !wl.running {
            return Err(SupervisorError::WorkflowLoopNotRunning);
        }
        wl.stop_tx.clone()
    };

    if let Some(tx) = stop_tx {
        let _ = tx.send(true);
    }

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            "Workflow loop stop requested",
        )
        .await;

    Ok(Json(serde_json::json!({
        "status": "stop_requested",
        "message": "Current workflow will complete, then loop will stop"
    })))
}

/// GET /workflow-loop/status
pub async fn status(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    let wl = state.workflow_loop.read().await;
    let status = wl.to_status();
    Ok(Json(status))
}

/// GET /workflow-loop/history
pub async fn history(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    let wl = state.workflow_loop.read().await;
    let results = wl.iteration_results.clone();
    Ok(Json(serde_json::json!({
        "iterations": results,
        "total": results.len(),
    })))
}

/// POST /workflow-loop/signal-restart
pub async fn signal_restart(
    State(state): State<SharedState>,
) -> Result<impl IntoResponse, SupervisorError> {
    let mut wl = state.workflow_loop.write().await;
    if !wl.running {
        return Err(SupervisorError::WorkflowLoopNotRunning);
    }
    wl.restart_signaled = true;
    drop(wl);

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            "Restart signal received from workflow",
        )
        .await;

    Ok(Json(serde_json::json!({
        "status": "signaled",
        "message": "Runner will restart between iterations"
    })))
}

struct StreamState {
    shared: SharedState,
    last_phase: LoopPhase,
    last_iteration: u32,
    sent_initial: bool,
}

/// GET /workflow-loop/stream — SSE stream of loop status changes
pub async fn stream(
    State(state): State<SharedState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let initial = StreamState {
        shared: state,
        last_phase: LoopPhase::Idle,
        last_iteration: 0,
        sent_initial: false,
    };

    let event_stream = unfold(initial, |mut ss| async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let wl = ss.shared.workflow_loop.read().await;
            let status = wl.to_status();
            drop(wl);

            let phase_changed = status.phase != ss.last_phase;
            let iteration_changed = status.current_iteration != ss.last_iteration;

            if !ss.sent_initial || phase_changed || iteration_changed {
                ss.last_phase = status.phase.clone();
                ss.last_iteration = status.current_iteration;
                ss.sent_initial = true;

                let data = serde_json::to_string(&status).unwrap_or_default();
                let event = Ok(Event::default().event("status").data(data));
                return Some((event, ss));
            }
        }
    });

    Sse::new(event_stream).keep_alive(KeepAlive::default())
}
