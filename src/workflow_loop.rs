use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::{
    RUNNER_API_PORT, WORKFLOW_LOOP_MAX_ITERATIONS_DEFAULT, WORKFLOW_LOOP_POLL_INTERVAL_SECS,
    WORKFLOW_LOOP_RUNNER_HEALTH_POLL_SECS, WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS,
};
use crate::log_capture::{LogLevel, LogSource};
use crate::process::port::is_runner_responding;
use crate::state::SharedState;

// --- Configuration types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowLoopConfig {
    pub workflow_id: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    pub exit_strategy: ExitStrategy,
    pub between_iterations: BetweenIterations,
}

fn default_max_iterations() -> u32 {
    WORKFLOW_LOOP_MAX_ITERATIONS_DEFAULT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitStrategy {
    Reflection {
        reflection_workflow_id: Option<String>,
    },
    WorkflowVerification,
    FixedIterations,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BetweenIterations {
    RestartRunner { rebuild: bool },
    RestartOnSignal { rebuild: bool },
    WaitHealthy,
    None,
}

// --- State types ---

pub struct WorkflowLoopState {
    pub running: bool,
    pub config: Option<WorkflowLoopConfig>,
    pub current_iteration: u32,
    pub phase: LoopPhase,
    pub started_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub iteration_results: Vec<IterationResult>,
    pub stop_tx: Option<watch::Sender<bool>>,
    pub restart_signaled: bool,
}

impl WorkflowLoopState {
    pub fn new() -> Self {
        Self {
            running: false,
            config: None,
            current_iteration: 0,
            phase: LoopPhase::Idle,
            started_at: None,
            error: None,
            iteration_results: Vec::new(),
            stop_tx: None,
            restart_signaled: false,
        }
    }

    pub fn to_status(&self) -> WorkflowLoopStatus {
        WorkflowLoopStatus {
            running: self.running,
            config: self.config.clone(),
            current_iteration: self.current_iteration,
            phase: self.phase.clone(),
            started_at: self.started_at,
            error: self.error.clone(),
            iteration_count: self.iteration_results.len() as u32,
            restart_signaled: self.restart_signaled,
        }
    }
}

impl Default for WorkflowLoopState {
    fn default() -> Self {
        Self::new()
    }
}

// --- Read-only status for API responses ---

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowLoopStatus {
    pub running: bool,
    pub config: Option<WorkflowLoopConfig>,
    pub current_iteration: u32,
    pub phase: LoopPhase,
    pub started_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub iteration_count: u32,
    pub restart_signaled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LoopPhase {
    Idle,
    RunningWorkflow,
    EvaluatingExit,
    BetweenIterations,
    WaitingForRunner,
    Complete,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct IterationResult {
    pub iteration: u32,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub task_run_id: String,
    pub exit_check: ExitCheckResult,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExitCheckResult {
    pub should_exit: bool,
    pub reason: String,
}

#[derive(Debug)]
enum ReflectionTriggerResult {
    Started { task_run_id: String },
    AlreadyRunning,
}

// --- Runner API client helpers ---

fn runner_url(path: &str) -> String {
    format!("http://127.0.0.1:{}{}", RUNNER_API_PORT, path)
}

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build reqwest client")
}

async fn start_workflow(client: &reqwest::Client, workflow_id: &str) -> Result<String, String> {
    let url = runner_url(&format!("/unified-workflows/{}/run", workflow_id));
    let resp = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to start workflow: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to start workflow ({}): {}", status, body));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse workflow response: {}", e))?;

    json.get("task_run_id")
        .or_else(|| json.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No task_run_id in workflow response".to_string())
}

async fn poll_until_complete(
    client: &reqwest::Client,
    task_run_id: &str,
    stop_rx: &watch::Receiver<bool>,
) -> Result<serde_json::Value, String> {
    let url = runner_url(&format!("/task-runs/{}/workflow-state", task_run_id));
    let interval = Duration::from_secs(WORKFLOW_LOOP_POLL_INTERVAL_SECS);

    loop {
        if *stop_rx.borrow() {
            return Err("Stop requested".to_string());
        }

        let resp = client.get(&url).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value =
                    r.json().await.map_err(|e| format!("Parse error: {}", e))?;

                let is_complete = json
                    .get("is_complete")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if is_complete {
                    return Ok(json);
                }
            }
            Ok(r) => {
                let status = r.status();
                if status.as_u16() == 404 {
                    // Task run might not have started yet, keep polling
                } else {
                    warn!("Unexpected status polling workflow state: {}", status);
                }
            }
            Err(e) => {
                warn!("Error polling workflow state: {}", e);
            }
        }

        sleep(interval).await;
    }
}

async fn trigger_reflection(
    client: &reqwest::Client,
    task_run_id: &str,
) -> Result<ReflectionTriggerResult, String> {
    let url = runner_url(&format!("/reflection/trigger/{}", task_run_id));
    let resp = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to trigger reflection: {}", e))?;

    if resp.status().as_u16() == 409 {
        return Ok(ReflectionTriggerResult::AlreadyRunning);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Failed to trigger reflection ({}): {}",
            status, body
        ));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse reflection response: {}", e))?;

    let task_run_id = json
        .get("task_run_id")
        .or_else(|| json.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default();

    Ok(ReflectionTriggerResult::Started { task_run_id })
}

async fn find_reflection_for_source(
    client: &reqwest::Client,
    source_task_run_id: &str,
) -> Result<Option<String>, String> {
    let url = runner_url("/task-runs");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to list task runs: {}", e))?;

    if !resp.status().is_success() {
        return Err("Failed to list task runs".to_string());
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse task runs: {}", e))?;

    // Look for a reflection task run that references our source
    let task_runs = json
        .as_array()
        .or_else(|| json.get("task_runs").and_then(|v| v.as_array()));

    if let Some(runs) = task_runs {
        for run in runs {
            let is_reflection = run
                .get("is_reflection")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let source_id = run
                .get("reflection_source_task_run_id")
                .and_then(|v| v.as_str());

            if is_reflection && source_id == Some(source_task_run_id) {
                if let Some(id) = run.get("id").and_then(|v| v.as_str()) {
                    return Ok(Some(id.to_string()));
                }
            }
        }
    }

    Ok(None)
}

async fn count_new_reflection_fixes(
    client: &reqwest::Client,
    reflection_task_run_id: &str,
) -> Result<u32, String> {
    let url = runner_url(&format!(
        "/task-runs/{}/reflection-fixes",
        reflection_task_run_id
    ));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to get reflection fixes: {}", e))?;

    if !resp.status().is_success() {
        // If endpoint doesn't exist, fall back to checking output
        let alt_url = runner_url(&format!(
            "/task-runs/{}/output?tail_chars=5000",
            reflection_task_run_id
        ));
        let alt_resp = client.get(&alt_url).send().await;
        if let Ok(r) = alt_resp {
            if r.status().is_success() {
                let text = r.text().await.unwrap_or_default();
                // Heuristic: count lines mentioning "fix" in the output
                let fix_count = text
                    .lines()
                    .filter(|l| {
                        let lower = l.to_lowercase();
                        lower.contains("fixed") || lower.contains("fix applied")
                    })
                    .count();
                return Ok(fix_count as u32);
            }
        }
        return Ok(0);
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse reflection fixes: {}", e))?;

    let count = json
        .get("fix_count")
        .or_else(|| json.get("count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    Ok(count as u32)
}

async fn wait_for_runner_healthy(timeout_secs: u64) -> bool {
    let deadline = Duration::from_secs(timeout_secs);
    let interval = Duration::from_secs(WORKFLOW_LOOP_RUNNER_HEALTH_POLL_SECS);

    let result = tokio::time::timeout(deadline, async {
        loop {
            if is_runner_responding(RUNNER_API_PORT).await {
                return true;
            }
            sleep(interval).await;
        }
    })
    .await;

    result.unwrap_or(false)
}

// --- State update helpers ---

async fn update_phase(state: &SharedState, phase: LoopPhase) {
    let mut wl = state.workflow_loop.write().await;
    wl.phase = phase;
}

async fn set_error(state: &SharedState, msg: &str) {
    {
        let mut wl = state.workflow_loop.write().await;
        wl.error = Some(msg.to_string());
        wl.phase = LoopPhase::Error;
        wl.running = false;
    }
    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Error,
            format!("Workflow loop error: {}", msg),
        )
        .await;
}

// --- Exit evaluators ---

async fn evaluate_reflection_exit(
    client: &reqwest::Client,
    task_run_id: &str,
    _reflection_workflow_id: &Option<String>,
    stop_rx: &watch::Receiver<bool>,
    state: &SharedState,
) -> Result<ExitCheckResult, String> {
    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            "Evaluating exit: triggering reflection...",
        )
        .await;

    // Try to trigger reflection
    let reflection_id = match trigger_reflection(client, task_run_id).await? {
        ReflectionTriggerResult::Started { task_run_id } => task_run_id,
        ReflectionTriggerResult::AlreadyRunning => {
            // Find the auto-triggered reflection
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    "Reflection already running (auto-triggered), finding it...",
                )
                .await;

            let mut found = None;
            for _ in 0..10 {
                if *stop_rx.borrow() {
                    return Err("Stop requested".to_string());
                }
                if let Ok(Some(id)) = find_reflection_for_source(client, task_run_id).await {
                    found = Some(id);
                    break;
                }
                sleep(Duration::from_secs(2)).await;
            }
            found.ok_or_else(|| "Could not find auto-triggered reflection task run".to_string())?
        }
    };

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!("Waiting for reflection {} to complete...", reflection_id),
        )
        .await;

    // Poll until reflection completes
    poll_until_complete(client, &reflection_id, stop_rx).await?;

    // Count fixes
    let fix_count = count_new_reflection_fixes(client, &reflection_id).await?;

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!("Reflection found {} new fix(es)", fix_count),
        )
        .await;

    if fix_count == 0 {
        Ok(ExitCheckResult {
            should_exit: true,
            reason: "Reflection found 0 new fixes — clean".to_string(),
        })
    } else {
        Ok(ExitCheckResult {
            should_exit: false,
            reason: format!("Reflection found {} new fix(es) — continuing", fix_count),
        })
    }
}

fn evaluate_verification_exit(workflow_state: &serde_json::Value) -> ExitCheckResult {
    // Check the inner verification loop iteration count
    // If the workflow passed verification on its first inner iteration,
    // it means nothing needed fixing — we can exit
    let inner_iterations = workflow_state
        .get("verification_iterations")
        .or_else(|| workflow_state.get("iteration_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1);

    if inner_iterations <= 1 {
        ExitCheckResult {
            should_exit: true,
            reason: "Workflow passed verification on first iteration — nothing to fix".to_string(),
        }
    } else {
        ExitCheckResult {
            should_exit: false,
            reason: format!(
                "Workflow needed {} inner iterations — may have more issues",
                inner_iterations
            ),
        }
    }
}

// --- Core loop ---

pub async fn run_loop(state: SharedState, stop_rx: watch::Receiver<bool>) {
    let config = {
        let wl = state.workflow_loop.read().await;
        match &wl.config {
            Some(c) => c.clone(),
            None => {
                set_error(&state, "No config set").await;
                return;
            }
        }
    };

    let client = build_client();

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!(
                "Starting workflow loop: workflow={}, max_iterations={}, exit_strategy={:?}",
                config.workflow_id, config.max_iterations, config.exit_strategy
            ),
        )
        .await;

    for iteration in 1..=config.max_iterations {
        // Check stop
        if *stop_rx.borrow() {
            info!(
                "Workflow loop stop requested before iteration {}",
                iteration
            );
            update_phase(&state, LoopPhase::Stopped).await;
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    format!("Workflow loop stopped before iteration {}", iteration),
                )
                .await;
            break;
        }

        // Update iteration counter
        {
            let mut wl = state.workflow_loop.write().await;
            wl.current_iteration = iteration;
        }

        let iter_start = Utc::now();

        state
            .logs
            .emit(
                LogSource::WorkflowLoop,
                LogLevel::Info,
                format!("=== Iteration {}/{} ===", iteration, config.max_iterations),
            )
            .await;

        // Phase 1: Run workflow
        update_phase(&state, LoopPhase::RunningWorkflow).await;

        let task_run_id = match start_workflow(&client, &config.workflow_id).await {
            Ok(id) => {
                state
                    .logs
                    .emit(
                        LogSource::WorkflowLoop,
                        LogLevel::Info,
                        format!("Workflow started: task_run_id={}", id),
                    )
                    .await;
                id
            }
            Err(e) => {
                set_error(&state, &format!("Failed to start workflow: {}", e)).await;
                return;
            }
        };

        // Phase 2: Poll until complete
        let workflow_state = match poll_until_complete(&client, &task_run_id, &stop_rx).await {
            Ok(ws) => {
                state
                    .logs
                    .emit(
                        LogSource::WorkflowLoop,
                        LogLevel::Info,
                        "Workflow completed",
                    )
                    .await;
                ws
            }
            Err(e) if e == "Stop requested" => {
                update_phase(&state, LoopPhase::Stopped).await;
                state
                    .logs
                    .emit(
                        LogSource::WorkflowLoop,
                        LogLevel::Info,
                        "Workflow loop stopped during workflow execution",
                    )
                    .await;
                break;
            }
            Err(e) => {
                set_error(&state, &format!("Error polling workflow: {}", e)).await;
                return;
            }
        };

        // Phase 3: Evaluate exit condition
        update_phase(&state, LoopPhase::EvaluatingExit).await;

        let exit_check = match &config.exit_strategy {
            ExitStrategy::Reflection {
                reflection_workflow_id,
            } => {
                match evaluate_reflection_exit(
                    &client,
                    &task_run_id,
                    reflection_workflow_id,
                    &stop_rx,
                    &state,
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) if e == "Stop requested" => {
                        update_phase(&state, LoopPhase::Stopped).await;
                        break;
                    }
                    Err(e) => {
                        set_error(&state, &format!("Reflection evaluation error: {}", e)).await;
                        return;
                    }
                }
            }
            ExitStrategy::WorkflowVerification => evaluate_verification_exit(&workflow_state),
            ExitStrategy::FixedIterations => {
                if iteration >= config.max_iterations {
                    ExitCheckResult {
                        should_exit: true,
                        reason: format!("Reached max iterations ({})", config.max_iterations),
                    }
                } else {
                    ExitCheckResult {
                        should_exit: false,
                        reason: format!(
                            "Iteration {}/{} complete",
                            iteration, config.max_iterations
                        ),
                    }
                }
            }
        };

        state
            .logs
            .emit(
                LogSource::WorkflowLoop,
                LogLevel::Info,
                format!("Exit check: {}", exit_check.reason),
            )
            .await;

        // Record iteration result
        let iter_result = IterationResult {
            iteration,
            started_at: iter_start,
            completed_at: Utc::now(),
            task_run_id: task_run_id.clone(),
            exit_check: exit_check.clone(),
        };

        {
            let mut wl = state.workflow_loop.write().await;
            wl.iteration_results.push(iter_result);
        }

        // Exit if condition met
        if exit_check.should_exit {
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    format!(
                        "Loop complete after {} iteration(s): {}",
                        iteration, exit_check.reason
                    ),
                )
                .await;
            update_phase(&state, LoopPhase::Complete).await;
            break;
        }

        // Phase 4: Between-iterations action
        if iteration < config.max_iterations {
            update_phase(&state, LoopPhase::BetweenIterations).await;

            match &config.between_iterations {
                BetweenIterations::RestartRunner { rebuild } => {
                    state
                        .logs
                        .emit(
                            LogSource::WorkflowLoop,
                            LogLevel::Info,
                            format!(
                                "Restarting runner (rebuild={}) between iterations...",
                                rebuild
                            ),
                        )
                        .await;

                    if let Err(e) = crate::process::manager::restart_runner(&state, *rebuild).await
                    {
                        set_error(&state, &format!("Failed to restart runner: {}", e)).await;
                        return;
                    }

                    // Wait for runner to be healthy
                    update_phase(&state, LoopPhase::WaitingForRunner).await;
                    state
                        .logs
                        .emit(
                            LogSource::WorkflowLoop,
                            LogLevel::Info,
                            "Waiting for runner API to be healthy...",
                        )
                        .await;

                    if !wait_for_runner_healthy(WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS).await {
                        set_error(&state, "Runner did not become healthy after restart").await;
                        return;
                    }

                    state
                        .logs
                        .emit(
                            LogSource::WorkflowLoop,
                            LogLevel::Info,
                            "Runner is healthy, continuing to next iteration",
                        )
                        .await;
                }
                BetweenIterations::RestartOnSignal { rebuild } => {
                    // Check and clear the signal flag
                    let signaled = {
                        let mut wl = state.workflow_loop.write().await;
                        let was = wl.restart_signaled;
                        wl.restart_signaled = false;
                        was
                    };

                    if signaled {
                        state
                            .logs
                            .emit(
                                LogSource::WorkflowLoop,
                                LogLevel::Info,
                                format!(
                                    "Restart signaled — restarting runner (rebuild={})...",
                                    rebuild
                                ),
                            )
                            .await;

                        if let Err(e) =
                            crate::process::manager::restart_runner(&state, *rebuild).await
                        {
                            set_error(&state, &format!("Failed to restart runner: {}", e)).await;
                            return;
                        }

                        update_phase(&state, LoopPhase::WaitingForRunner).await;
                        state
                            .logs
                            .emit(
                                LogSource::WorkflowLoop,
                                LogLevel::Info,
                                "Waiting for runner API to be healthy...",
                            )
                            .await;

                        if !wait_for_runner_healthy(WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS).await
                        {
                            set_error(&state, "Runner did not become healthy after restart").await;
                            return;
                        }

                        state
                            .logs
                            .emit(
                                LogSource::WorkflowLoop,
                                LogLevel::Info,
                                "Runner is healthy, continuing to next iteration",
                            )
                            .await;
                    } else {
                        state
                            .logs
                            .emit(
                                LogSource::WorkflowLoop,
                                LogLevel::Info,
                                "No restart signal — skipping runner restart",
                            )
                            .await;
                    }
                }
                BetweenIterations::WaitHealthy => {
                    update_phase(&state, LoopPhase::WaitingForRunner).await;
                    state
                        .logs
                        .emit(
                            LogSource::WorkflowLoop,
                            LogLevel::Info,
                            "Waiting for runner to be healthy...",
                        )
                        .await;

                    if !wait_for_runner_healthy(WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS).await {
                        set_error(&state, "Runner is not healthy").await;
                        return;
                    }
                }
                BetweenIterations::None => {
                    state
                        .logs
                        .emit(
                            LogSource::WorkflowLoop,
                            LogLevel::Debug,
                            "No between-iterations action",
                        )
                        .await;
                }
            }
        }

        // If we're at max iterations and didn't exit, mark complete
        if iteration == config.max_iterations {
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Warn,
                    format!(
                        "Reached max iterations ({}) without exit condition",
                        config.max_iterations
                    ),
                )
                .await;
            update_phase(&state, LoopPhase::Complete).await;
        }
    }

    // Mark as not running
    {
        let mut wl = state.workflow_loop.write().await;
        wl.running = false;
        // Keep phase as-is (Complete, Stopped, or Error)
    }

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            "Workflow loop finished",
        )
        .await;
}
