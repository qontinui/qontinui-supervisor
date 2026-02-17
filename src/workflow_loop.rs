use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::ai_debug::resolve_model_id;
use crate::config::{
    RUNNER_API_PORT, WORKFLOW_LOOP_FIX_TIMEOUT_SECS, WORKFLOW_LOOP_MAX_ITERATIONS_DEFAULT,
    WORKFLOW_LOOP_POLL_INTERVAL_SECS, WORKFLOW_LOOP_RUNNER_HEALTH_POLL_SECS,
    WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS,
};
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::log_capture::{LogLevel, LogSource};
use crate::process::port::is_runner_responding;
use crate::state::SharedState;

// --- Configuration types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowLoopConfig {
    /// Workflow ID for simple mode (required when phases is absent)
    pub workflow_id: Option<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Exit strategy for simple mode (required when phases is absent)
    pub exit_strategy: Option<ExitStrategy>,
    pub between_iterations: BetweenIterations,
    /// Pipeline mode configuration (mutually exclusive with simple mode)
    pub phases: Option<PipelinePhases>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelinePhases {
    /// Optional: generate workflow from a description
    pub build: Option<BuildPhaseConfig>,
    /// Fallback workflow ID if build phase is absent or hasn't run yet
    pub execute_workflow_id: Option<String>,
    /// Reflection config (always enabled in pipeline mode)
    #[serde(default)]
    pub reflect: ReflectPhaseConfig,
    /// Optional: spawn Claude Code to implement reflection findings
    pub implement_fixes: Option<ImplementFixesConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPhaseConfig {
    /// Prompt/description for workflow generation (constant across iterations)
    pub description: String,
    /// Optional additional context for the builder
    pub context: Option<String>,
    /// Context IDs to pass to the generator
    pub context_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReflectPhaseConfig {
    pub reflection_workflow_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementFixesConfig {
    /// AI provider override (defaults to supervisor's current provider)
    pub provider: Option<String>,
    /// AI model override (defaults to supervisor's current model)
    pub model: Option<String>,
    /// Extra instructions for the fix agent
    pub additional_context: Option<String>,
    /// Max seconds to wait for Claude to finish
    #[serde(default = "default_fix_timeout")]
    pub timeout_secs: u64,
}

fn default_fix_timeout() -> u64 {
    WORKFLOW_LOOP_FIX_TIMEOUT_SECS
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
    BuildingWorkflow,
    RunningWorkflow,
    Reflecting,
    ImplementingFixes,
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
    /// Workflow ID generated by the build phase (pipeline mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_workflow_id: Option<String>,
    /// Task run ID for the reflection phase (pipeline mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reflection_task_run_id: Option<String>,
    /// Number of fixes found by reflection (pipeline mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_count: Option<u32>,
    /// Whether fixes were implemented (pipeline mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixes_implemented: Option<bool>,
    /// Whether the builder will re-run next iteration (pipeline mode)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rebuild_triggered: Option<bool>,
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

    // Runner returns {"success": true, "data": [...fixes...]}
    let count = json
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| arr.len() as u64)
        .or_else(|| {
            json.get("fix_count")
                .or_else(|| json.get("count"))
                .and_then(|v| v.as_u64())
        })
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

// --- Pipeline helpers ---

/// Fix types that indicate the workflow itself needs to be regenerated
const REBUILD_FIX_TYPES: &[&str] = &[
    "workflow_step_rewrite",
    "instruction_clarification",
    "context_addition",
];

pub fn should_rebuild(fixes: &[serde_json::Value]) -> bool {
    fixes.iter().any(|fix| {
        fix.get("fix_type")
            .and_then(|v| v.as_str())
            .map(|t| REBUILD_FIX_TYPES.iter().any(|rt| t.contains(rt)))
            .unwrap_or(false)
    })
}

/// Generate a workflow via the runner's async generator endpoint.
/// Returns the generated workflow ID.
async fn generate_workflow(
    client: &reqwest::Client,
    build_config: &BuildPhaseConfig,
    stop_rx: &watch::Receiver<bool>,
) -> Result<String, String> {
    let url = runner_url("/unified-workflows/generate-async");

    let mut body = serde_json::json!({
        "description": build_config.description,
        "max_fix_iterations": 3,
    });
    if let Some(ctx) = &build_config.context {
        body["inline_context"] = serde_json::json!(ctx);
    }
    if let Some(ids) = &build_config.context_ids {
        body["context_ids"] = serde_json::json!(ids);
    }

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Failed to call generate-async: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("generate-async failed ({}): {}", status, text));
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse generate-async response: {}", e))?;

    // Runner wraps in ApiResponse: {"success": true, "data": {"task_run_id": "...", ...}}
    let data = json.get("data").unwrap_or(&json);
    let task_run_id = data
        .get("task_run_id")
        .or_else(|| data.get("id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "No task_run_id in generate-async response".to_string())?
        .to_string();

    // Poll until the meta-workflow completes
    poll_until_complete(client, &task_run_id, stop_rx).await?;

    // Fetch result data to get the generated workflow ID
    let result_url = runner_url(&format!("/task-runs/{}/result-data", task_run_id));
    let result_resp = client
        .get(&result_url)
        .send()
        .await
        .map_err(|e| format!("Failed to fetch result-data: {}", e))?;

    if !result_resp.status().is_success() {
        return Err(format!("result-data returned {}", result_resp.status()));
    }

    let result_json: serde_json::Value = result_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse result-data: {}", e))?;

    result_json
        .get("generated_workflow_id")
        .or_else(|| result_json.get("workflow_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "No generated_workflow_id in result-data".to_string())
}

/// Run reflection and return (reflection_task_run_id, fix_count, fixes).
async fn run_reflection(
    client: &reqwest::Client,
    task_run_id: &str,
    _reflect_config: &ReflectPhaseConfig,
    stop_rx: &watch::Receiver<bool>,
    state: &SharedState,
) -> Result<(String, u32, Vec<serde_json::Value>), String> {
    // Trigger reflection (reuse existing helper)
    let reflection_id = match trigger_reflection(client, task_run_id).await? {
        ReflectionTriggerResult::Started { task_run_id } => task_run_id,
        ReflectionTriggerResult::AlreadyRunning => {
            // Find the auto-triggered reflection
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

    // Fetch fixes
    let fixes_url = runner_url(&format!("/task-runs/{}/reflection-fixes", reflection_id));
    let fixes_resp = client.get(&fixes_url).send().await;

    let (fix_count, fixes) = match fixes_resp {
        Ok(r) if r.status().is_success() => {
            let json: serde_json::Value = r
                .json()
                .await
                .map_err(|e| format!("Failed to parse reflection fixes: {}", e))?;

            // Runner returns {"success": true, "data": [...fixes...]}
            let fixes_arr = json
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let count = fixes_arr.len() as u32;

            (count, fixes_arr)
        }
        _ => {
            // Fallback: use count_new_reflection_fixes
            let count = count_new_reflection_fixes(client, &reflection_id).await?;
            (count, Vec::new())
        }
    };

    Ok((reflection_id, fix_count, fixes))
}

/// Build a prompt for the fix agent containing the reflection findings.
pub fn build_fix_prompt(fixes: &[serde_json::Value], additional_context: Option<&str>) -> String {
    let fixes_json = serde_json::to_string_pretty(fixes).unwrap_or_else(|_| "[]".to_string());

    let mut prompt = format!(
        "You are an autonomous code fix agent. A workflow was executed and a reflection phase \
         identified the following issues that need to be fixed.\n\n\
         ## Reflection Fixes\n\n\
         ```json\n{}\n```\n\n\
         ## Instructions\n\n\
         1. Read the relevant source files mentioned in the fixes\n\
         2. Apply each fix carefully\n\
         3. Verify your changes make sense in context\n\
         4. Do NOT create new files unless absolutely necessary\n\
         5. Focus on the specific issues identified — do not refactor unrelated code\n",
        fixes_json
    );

    if let Some(ctx) = additional_context {
        prompt.push_str(&format!("\n## Additional Context\n\n{}\n", ctx));
    }

    prompt
}

/// Spawn a Claude Code session to implement the reflection fixes.
/// Returns Ok(true) if fixes were successfully applied.
async fn implement_fixes(
    state: &SharedState,
    client: &reqwest::Client,
    reflection_task_run_id: &str,
    fix_count: u32,
    fixes: &[serde_json::Value],
    fix_config: &ImplementFixesConfig,
    stop_rx: &watch::Receiver<bool>,
) -> Result<bool, String> {
    // If we don't have fixes JSON, try to fetch them
    let fixes_to_use = if fixes.is_empty() {
        let url = runner_url(&format!(
            "/task-runs/{}/reflection-fixes",
            reflection_task_run_id
        ));
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                let json: serde_json::Value = r.json().await.unwrap_or_default();
                json.get("data")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            }
            _ => {
                // Fallback: fetch raw output
                let output_url = runner_url(&format!(
                    "/task-runs/{}/output?tail_chars=10000",
                    reflection_task_run_id
                ));
                let output_text = match client.get(&output_url).send().await {
                    Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
                    _ => String::new(),
                };
                vec![serde_json::json!({
                    "raw_output": output_text,
                    "fix_count": fix_count,
                })]
            }
        }
    } else {
        fixes.to_vec()
    };

    // Build the prompt
    let prompt = build_fix_prompt(&fixes_to_use, fix_config.additional_context.as_deref());

    // Write prompt to temp file
    let prompt_path = std::env::temp_dir().join("qontinui-fix-prompt.md");
    tokio::fs::write(&prompt_path, &prompt)
        .await
        .map_err(|e| format!("Failed to write fix prompt: {}", e))?;

    // Resolve provider/model
    let model_id = if let (Some(provider), Some(model)) = (&fix_config.provider, &fix_config.model)
    {
        resolve_model_id(provider, model).unwrap_or_else(|| "claude-opus-4-6".to_string())
    } else {
        let ai = state.ai.read().await;
        resolve_model_id(&ai.provider, &ai.model).unwrap_or_else(|| "claude-opus-4-6".to_string())
    };

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!(
                "Spawning fix agent (model={}, fixes={})...",
                model_id, fix_count
            ),
        )
        .await;

    // Spawn claude --print
    let mut child = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt_path.display().to_string(),
            "--permission-mode",
            "bypassPermissions",
            "--output-format",
            "text",
            "--model",
            &model_id,
        ])
        .env_remove("CLAUDECODE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn Claude CLI for fixes: {}", e))?;

    // Wait for completion with timeout and stop signal
    let timeout = Duration::from_secs(fix_config.timeout_secs);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if *stop_rx.borrow() {
            let _ = child.kill().await;
            return Err("Stop requested during fix implementation".to_string());
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited
                if status.success() {
                    state
                        .logs
                        .emit(
                            LogSource::WorkflowLoop,
                            LogLevel::Info,
                            "Fix agent completed successfully",
                        )
                        .await;

                    // Capture output for logging
                    if let Some(stdout) = child.stdout.take() {
                        use tokio::io::AsyncReadExt;
                        let mut buf = String::new();
                        let mut reader = tokio::io::BufReader::new(stdout);
                        let _ = reader.read_to_string(&mut buf).await;
                        if !buf.is_empty() {
                            let truncated = if buf.len() > 2000 {
                                format!("{}... [truncated]", &buf[..2000])
                            } else {
                                buf
                            };
                            state
                                .logs
                                .emit(
                                    LogSource::WorkflowLoop,
                                    LogLevel::Debug,
                                    format!("Fix agent output: {}", truncated),
                                )
                                .await;
                        }
                    }

                    return Ok(true);
                } else {
                    let code = status.code().unwrap_or(-1);
                    warn!("Fix agent exited with code {}", code);
                    return Ok(false);
                }
            }
            Ok(None) => {
                // Still running
                if tokio::time::Instant::now() >= deadline {
                    warn!("Fix agent timed out after {}s", fix_config.timeout_secs);
                    let _ = child.kill().await;
                    return Err(format!(
                        "Fix agent timed out after {}s",
                        fix_config.timeout_secs
                    ));
                }
                sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                return Err(format!("Error checking fix agent status: {}", e));
            }
        }
    }
}

// --- Between-iterations action (shared by both loop modes) ---

async fn handle_between_iterations(
    state: &SharedState,
    config: &WorkflowLoopConfig,
    iteration: u32,
) -> Result<(), String> {
    if iteration >= config.max_iterations {
        return Ok(());
    }

    update_phase(state, LoopPhase::BetweenIterations).await;

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

            crate::process::manager::restart_runner(state, *rebuild, RestartSource::WorkflowLoop)
                .await
                .map_err(|e| format!("Failed to restart runner: {}", e))?;

            update_phase(state, LoopPhase::WaitingForRunner).await;
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    "Waiting for runner API to be healthy...",
                )
                .await;

            if !wait_for_runner_healthy(WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS).await {
                return Err("Runner did not become healthy after restart".to_string());
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

                crate::process::manager::restart_runner(
                    state,
                    *rebuild,
                    RestartSource::WorkflowLoop,
                )
                .await
                .map_err(|e| format!("Failed to restart runner: {}", e))?;

                update_phase(state, LoopPhase::WaitingForRunner).await;
                state
                    .logs
                    .emit(
                        LogSource::WorkflowLoop,
                        LogLevel::Info,
                        "Waiting for runner API to be healthy...",
                    )
                    .await;

                if !wait_for_runner_healthy(WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS).await {
                    return Err("Runner did not become healthy after restart".to_string());
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
            update_phase(state, LoopPhase::WaitingForRunner).await;
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    "Waiting for runner to be healthy...",
                )
                .await;

            if !wait_for_runner_healthy(WORKFLOW_LOOP_RUNNER_HEALTH_TIMEOUT_SECS).await {
                return Err("Runner is not healthy".to_string());
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

    Ok(())
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

    if config.phases.is_some() {
        run_pipeline_loop(state, config, stop_rx).await;
    } else {
        run_simple_loop(state, config, stop_rx).await;
    }
}

/// The original simple loop: run workflow, evaluate exit, repeat.
async fn run_simple_loop(
    state: SharedState,
    config: WorkflowLoopConfig,
    stop_rx: watch::Receiver<bool>,
) {
    let client = build_client();
    let workflow_id = config.workflow_id.as_deref().unwrap_or("");
    let exit_strategy = config
        .exit_strategy
        .clone()
        .unwrap_or(ExitStrategy::FixedIterations);

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!(
                "Starting workflow loop: workflow={}, max_iterations={}, exit_strategy={:?}",
                workflow_id, config.max_iterations, exit_strategy
            ),
        )
        .await;

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::LoopStarted {
            workflow_id: workflow_id.to_string(),
            max_iterations: config.max_iterations,
            exit_strategy: format!("{:?}", exit_strategy),
            between_iterations: format!("{:?}", config.between_iterations),
        });

    for iteration in 1..=config.max_iterations {
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
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopStopped { iteration });
            break;
        }

        {
            let mut wl = state.workflow_loop.write().await;
            wl.current_iteration = iteration;
        }

        let iter_start = Utc::now();
        let iter_timer = std::time::Instant::now();

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::IterationStarted {
                iteration,
                max_iterations: config.max_iterations,
            });

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

        let task_run_id = match start_workflow(&client, workflow_id).await {
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
                let msg = format!("Failed to start workflow: {}", e);
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopError {
                        iteration,
                        error: msg.clone(),
                    });
                set_error(&state, &msg).await;
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
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopStopped { iteration });
                break;
            }
            Err(e) => {
                let msg = format!("Error polling workflow: {}", e);
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopError {
                        iteration,
                        error: msg.clone(),
                    });
                set_error(&state, &msg).await;
                return;
            }
        };

        // Phase 3: Evaluate exit condition
        update_phase(&state, LoopPhase::EvaluatingExit).await;

        let exit_check = match &exit_strategy {
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
                        state
                            .diagnostics
                            .write()
                            .await
                            .emit(DiagnosticEventKind::LoopStopped { iteration });
                        update_phase(&state, LoopPhase::Stopped).await;
                        break;
                    }
                    Err(e) => {
                        let msg = format!("Reflection evaluation error: {}", e);
                        state
                            .diagnostics
                            .write()
                            .await
                            .emit(DiagnosticEventKind::LoopError {
                                iteration,
                                error: msg.clone(),
                            });
                        set_error(&state, &msg).await;
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

        let iter_result = IterationResult {
            iteration,
            started_at: iter_start,
            completed_at: Utc::now(),
            task_run_id: task_run_id.clone(),
            exit_check: exit_check.clone(),
            generated_workflow_id: None,
            reflection_task_run_id: None,
            fix_count: None,
            fixes_implemented: None,
            rebuild_triggered: None,
        };

        {
            let mut wl = state.workflow_loop.write().await;
            wl.iteration_results.push(iter_result);
        }

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::IterationCompleted {
                iteration,
                duration_secs: iter_timer.elapsed().as_secs_f64(),
                exit_check_result: exit_check.should_exit,
                exit_check_reason: exit_check.reason.clone(),
            });

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
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopCompleted {
                    iterations_completed: iteration,
                    reason: exit_check.reason.clone(),
                });
            update_phase(&state, LoopPhase::Complete).await;
            break;
        }

        // Between-iterations action
        if let Err(e) = handle_between_iterations(&state, &config, iteration).await {
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopError {
                    iteration,
                    error: e.clone(),
                });
            set_error(&state, &e).await;
            return;
        }

        if iteration == config.max_iterations {
            let reason = format!(
                "Reached max iterations ({}) without exit condition",
                config.max_iterations
            );
            state
                .logs
                .emit(LogSource::WorkflowLoop, LogLevel::Warn, &reason)
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopCompleted {
                    iterations_completed: iteration,
                    reason,
                });
            update_phase(&state, LoopPhase::Complete).await;
        }
    }

    // Mark as not running
    {
        let mut wl = state.workflow_loop.write().await;
        wl.running = false;
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

/// Pipeline loop: Build -> Execute -> Reflect -> Implement Fixes
async fn run_pipeline_loop(
    state: SharedState,
    config: WorkflowLoopConfig,
    stop_rx: watch::Receiver<bool>,
) {
    let client = build_client();
    let phases = config.phases.as_ref().unwrap();

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            format!(
                "Starting pipeline loop: max_iterations={}, build={}, implement_fixes={}",
                config.max_iterations,
                phases.build.is_some(),
                phases.implement_fixes.is_some(),
            ),
        )
        .await;

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::LoopStarted {
            workflow_id: phases
                .execute_workflow_id
                .clone()
                .unwrap_or_else(|| "(generated)".to_string()),
            max_iterations: config.max_iterations,
            exit_strategy: "pipeline".to_string(),
            between_iterations: format!("{:?}", config.between_iterations),
        });

    let mut current_workflow_id: Option<String> = phases.execute_workflow_id.clone();
    let mut rebuild_needed = phases.build.is_some(); // Always build on first iteration if configured

    for iteration in 1..=config.max_iterations {
        if *stop_rx.borrow() {
            info!(
                "Pipeline loop stop requested before iteration {}",
                iteration
            );
            update_phase(&state, LoopPhase::Stopped).await;
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    format!("Pipeline loop stopped before iteration {}", iteration),
                )
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopStopped { iteration });
            break;
        }

        {
            let mut wl = state.workflow_loop.write().await;
            wl.current_iteration = iteration;
        }

        let iter_start = Utc::now();
        let iter_timer = std::time::Instant::now();

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::IterationStarted {
                iteration,
                max_iterations: config.max_iterations,
            });

        state
            .logs
            .emit(
                LogSource::WorkflowLoop,
                LogLevel::Info,
                format!(
                    "=== Pipeline Iteration {}/{} ===",
                    iteration, config.max_iterations
                ),
            )
            .await;

        let mut generated_wf_id: Option<String> = None;
        let mut fixes_applied: Option<bool> = None;
        let mut iter_rebuild_triggered: Option<bool> = None;

        // --- PHASE 1: BUILD (conditional) ---
        if rebuild_needed {
            if let Some(build_config) = &phases.build {
                update_phase(&state, LoopPhase::BuildingWorkflow).await;
                let phase_timer = std::time::Instant::now();

                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::PipelinePhaseStarted {
                        iteration,
                        phase: "build".to_string(),
                    });

                state
                    .logs
                    .emit(
                        LogSource::WorkflowLoop,
                        LogLevel::Info,
                        "Building workflow from description...",
                    )
                    .await;

                match generate_workflow(&client, build_config, &stop_rx).await {
                    Ok(wf_id) => {
                        state
                            .logs
                            .emit(
                                LogSource::WorkflowLoop,
                                LogLevel::Info,
                                format!("Workflow generated: {}", wf_id),
                            )
                            .await;
                        current_workflow_id = Some(wf_id.clone());
                        generated_wf_id = Some(wf_id);
                    }
                    Err(e) if e == "Stop requested" => {
                        update_phase(&state, LoopPhase::Stopped).await;
                        state
                            .diagnostics
                            .write()
                            .await
                            .emit(DiagnosticEventKind::LoopStopped { iteration });
                        break;
                    }
                    Err(e) => {
                        let msg = format!("Build phase failed: {}", e);
                        state
                            .diagnostics
                            .write()
                            .await
                            .emit(DiagnosticEventKind::LoopError {
                                iteration,
                                error: msg.clone(),
                            });
                        set_error(&state, &msg).await;
                        return;
                    }
                }

                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::PipelinePhaseCompleted {
                        iteration,
                        phase: "build".to_string(),
                        duration_secs: phase_timer.elapsed().as_secs_f64(),
                    });
            }
            rebuild_needed = false;
        }

        // --- PHASE 2: EXECUTE ---
        let workflow_id = match &current_workflow_id {
            Some(id) => id.clone(),
            None => {
                let msg = "No workflow ID available for execution (no build phase and no execute_workflow_id)";
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopError {
                        iteration,
                        error: msg.to_string(),
                    });
                set_error(&state, msg).await;
                return;
            }
        };

        update_phase(&state, LoopPhase::RunningWorkflow).await;
        let phase_timer = std::time::Instant::now();

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::PipelinePhaseStarted {
                iteration,
                phase: "execute".to_string(),
            });

        let task_run_id = match start_workflow(&client, &workflow_id).await {
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
                let msg = format!("Failed to start workflow: {}", e);
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopError {
                        iteration,
                        error: msg.clone(),
                    });
                set_error(&state, &msg).await;
                return;
            }
        };

        match poll_until_complete(&client, &task_run_id, &stop_rx).await {
            Ok(_ws) => {
                state
                    .logs
                    .emit(
                        LogSource::WorkflowLoop,
                        LogLevel::Info,
                        "Workflow completed",
                    )
                    .await;
            }
            Err(e) if e == "Stop requested" => {
                update_phase(&state, LoopPhase::Stopped).await;
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopStopped { iteration });
                break;
            }
            Err(e) => {
                let msg = format!("Error polling workflow: {}", e);
                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::LoopError {
                        iteration,
                        error: msg.clone(),
                    });
                set_error(&state, &msg).await;
                return;
            }
        }

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::PipelinePhaseCompleted {
                iteration,
                phase: "execute".to_string(),
                duration_secs: phase_timer.elapsed().as_secs_f64(),
            });

        // --- PHASE 3: REFLECT ---
        update_phase(&state, LoopPhase::Reflecting).await;
        let phase_timer = std::time::Instant::now();

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::PipelinePhaseStarted {
                iteration,
                phase: "reflect".to_string(),
            });

        state
            .logs
            .emit(
                LogSource::WorkflowLoop,
                LogLevel::Info,
                "Running reflection...",
            )
            .await;

        let (refl_id, fix_count, fixes) =
            match run_reflection(&client, &task_run_id, &phases.reflect, &stop_rx, &state).await {
                Ok(result) => result,
                Err(e) if e == "Stop requested" => {
                    update_phase(&state, LoopPhase::Stopped).await;
                    state
                        .diagnostics
                        .write()
                        .await
                        .emit(DiagnosticEventKind::LoopStopped { iteration });
                    break;
                }
                Err(e) => {
                    let msg = format!("Reflection failed: {}", e);
                    state
                        .diagnostics
                        .write()
                        .await
                        .emit(DiagnosticEventKind::LoopError {
                            iteration,
                            error: msg.clone(),
                        });
                    set_error(&state, &msg).await;
                    return;
                }
            };

        let reflection_id = Some(refl_id.clone());
        let iter_fix_count = Some(fix_count);

        state
            .logs
            .emit(
                LogSource::WorkflowLoop,
                LogLevel::Info,
                format!("Reflection found {} fix(es)", fix_count),
            )
            .await;

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::PipelinePhaseCompleted {
                iteration,
                phase: "reflect".to_string(),
                duration_secs: phase_timer.elapsed().as_secs_f64(),
            });

        // Exit if no fixes needed
        let exit_check = if fix_count == 0 {
            ExitCheckResult {
                should_exit: true,
                reason: "Reflection found 0 fixes — clean".to_string(),
            }
        } else {
            ExitCheckResult {
                should_exit: false,
                reason: format!("Reflection found {} fix(es) — continuing", fix_count),
            }
        };

        // --- PHASE 4: IMPLEMENT FIXES (if configured and fixes > 0) ---
        if !exit_check.should_exit {
            if let Some(fix_config) = &phases.implement_fixes {
                update_phase(&state, LoopPhase::ImplementingFixes).await;
                let phase_timer = std::time::Instant::now();

                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::PipelinePhaseStarted {
                        iteration,
                        phase: "implement_fixes".to_string(),
                    });

                match implement_fixes(
                    &state, &client, &refl_id, fix_count, &fixes, fix_config, &stop_rx,
                )
                .await
                {
                    Ok(success) => {
                        fixes_applied = Some(success);
                        let duration = phase_timer.elapsed().as_secs_f64();

                        state.diagnostics.write().await.emit(
                            DiagnosticEventKind::FixesImplemented {
                                iteration,
                                fix_count,
                                duration_secs: duration,
                            },
                        );

                        state.diagnostics.write().await.emit(
                            DiagnosticEventKind::PipelinePhaseCompleted {
                                iteration,
                                phase: "implement_fixes".to_string(),
                                duration_secs: duration,
                            },
                        );

                        // Check if rebuild is needed
                        if success && should_rebuild(&fixes) {
                            rebuild_needed = true;
                            iter_rebuild_triggered = Some(true);

                            state.diagnostics.write().await.emit(
                                DiagnosticEventKind::RebuildTriggered {
                                    iteration,
                                    reason: "Fixes include workflow-structural changes".to_string(),
                                },
                            );

                            state
                                .logs
                                .emit(
                                    LogSource::WorkflowLoop,
                                    LogLevel::Info,
                                    "Rebuild triggered: fixes include workflow-structural changes",
                                )
                                .await;
                        }
                    }
                    Err(e) if e.contains("Stop requested") => {
                        update_phase(&state, LoopPhase::Stopped).await;
                        state
                            .diagnostics
                            .write()
                            .await
                            .emit(DiagnosticEventKind::LoopStopped { iteration });
                        break;
                    }
                    Err(e) => {
                        warn!("Fix implementation failed: {}", e);
                        fixes_applied = Some(false);
                        state
                            .logs
                            .emit(
                                LogSource::WorkflowLoop,
                                LogLevel::Warn,
                                format!("Fix implementation failed: {} — continuing", e),
                            )
                            .await;
                    }
                }
            }
        }

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
            generated_workflow_id: generated_wf_id,
            reflection_task_run_id: reflection_id,
            fix_count: iter_fix_count,
            fixes_implemented: fixes_applied,
            rebuild_triggered: iter_rebuild_triggered,
        };

        {
            let mut wl = state.workflow_loop.write().await;
            wl.iteration_results.push(iter_result);
        }

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::IterationCompleted {
                iteration,
                duration_secs: iter_timer.elapsed().as_secs_f64(),
                exit_check_result: exit_check.should_exit,
                exit_check_reason: exit_check.reason.clone(),
            });

        if exit_check.should_exit {
            state
                .logs
                .emit(
                    LogSource::WorkflowLoop,
                    LogLevel::Info,
                    format!(
                        "Pipeline complete after {} iteration(s): {}",
                        iteration, exit_check.reason
                    ),
                )
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopCompleted {
                    iterations_completed: iteration,
                    reason: exit_check.reason.clone(),
                });
            update_phase(&state, LoopPhase::Complete).await;
            break;
        }

        // Between-iterations action
        if let Err(e) = handle_between_iterations(&state, &config, iteration).await {
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopError {
                    iteration,
                    error: e.clone(),
                });
            set_error(&state, &e).await;
            return;
        }

        if iteration == config.max_iterations {
            let reason = format!(
                "Reached max iterations ({}) without clean reflection",
                config.max_iterations
            );
            state
                .logs
                .emit(LogSource::WorkflowLoop, LogLevel::Warn, &reason)
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::LoopCompleted {
                    iterations_completed: iteration,
                    reason,
                });
            update_phase(&state, LoopPhase::Complete).await;
        }
    }

    // Mark as not running
    {
        let mut wl = state.workflow_loop.write().await;
        wl.running = false;
    }

    state
        .logs
        .emit(
            LogSource::WorkflowLoop,
            LogLevel::Info,
            "Pipeline loop finished",
        )
        .await;
}
