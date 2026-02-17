use chrono::Utc;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{error, info, warn};

use super::db::EvalDb;
use super::{EvalResult, EvalRunSummary};
use crate::config::RUNNER_API_PORT;
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;

/// Generate a workflow via the runner API and return (task_run_id, workflow_id, workflow_json).
async fn generate_workflow_for_eval(
    http_client: &reqwest::Client,
    prompt: &str,
) -> anyhow::Result<(String, String, String)> {
    let runner_url = format!("http://127.0.0.1:{}", RUNNER_API_PORT);

    // Start async generation — runner expects both "prompt" and "description" fields
    let resp = http_client
        .post(format!("{}/unified-workflows/generate-async", runner_url))
        .json(&serde_json::json!({
            "prompt": prompt,
            "description": prompt,
        }))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("generate-async returned {}: {}", status, body);
    }

    // Response: {"success": true, "data": {"task_run_id": "...", "meta_workflow_id": "..."}}
    let gen_resp: serde_json::Value = resp.json().await?;
    let data = &gen_resp["data"];
    let task_run_id = data["task_run_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No task_run_id in generate-async response"))?
        .to_string();
    let workflow_id = data["meta_workflow_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No meta_workflow_id in generate-async response"))?
        .to_string();

    info!(
        "Started generation task_run_id={}, workflow_id={}",
        task_run_id, workflow_id
    );

    // Poll until complete (up to 10 minutes)
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Workflow generation timed out after 10 minutes");
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let state_resp = http_client
            .get(format!(
                "{}/task-runs/{}/workflow-state",
                runner_url, task_run_id
            ))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;

        match state_resp {
            Ok(resp) if resp.status().is_success() => {
                let wf_state: serde_json::Value = resp.json().await?;
                let is_complete = wf_state["is_complete"].as_bool().unwrap_or(false);
                let is_stopped = wf_state["is_stopped"].as_bool().unwrap_or(false);

                if is_complete || is_stopped {
                    if is_stopped {
                        anyhow::bail!("Workflow generation was stopped");
                    }

                    // Fetch the generated workflow JSON
                    // The meta-workflow wraps the generation — the actual generated workflow
                    // is the one we need to score. Fetch it from unified-workflows endpoint.
                    let wf_resp = http_client
                        .get(format!("{}/unified-workflows/{}", runner_url, workflow_id))
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await?;

                    if !wf_resp.status().is_success() {
                        anyhow::bail!(
                            "Failed to fetch workflow {}: {}",
                            workflow_id,
                            wf_resp.status()
                        );
                    }

                    // Response is {"success": true, "data": {...workflow...}}
                    let wf_body: serde_json::Value = wf_resp.json().await?;
                    let wf_data = &wf_body["data"];

                    // The workflow JSON for scoring is the data object
                    let workflow_json = serde_json::to_string_pretty(wf_data)?;
                    return Ok((task_run_id, workflow_id, workflow_json));
                }
            }
            Ok(resp) => {
                warn!("Workflow state poll returned status {}", resp.status());
            }
            Err(e) => {
                warn!("Workflow state poll error: {}", e);
            }
        }
    }
}

/// Run a single evaluation pass over all enabled test prompts.
pub async fn run_eval(
    db: Arc<EvalDb>,
    state: SharedState,
    prompt_ids: Option<Vec<String>>,
    stop_rx: watch::Receiver<bool>,
) {
    let run_id = uuid::Uuid::new_v4().to_string();

    // Load test prompts
    let prompts = match db.list_enabled_test_prompts() {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to load test prompts: {}", e);
            return;
        }
    };

    // Filter to requested prompt IDs if specified
    let prompts: Vec<_> = if let Some(ref ids) = prompt_ids {
        prompts
            .into_iter()
            .filter(|p| ids.contains(&p.id))
            .collect()
    } else {
        prompts
    };

    if prompts.is_empty() {
        warn!("No enabled test prompts to evaluate");
        return;
    }

    let total = prompts.len() as i64;

    // Create run record
    let run = EvalRunSummary {
        id: run_id.clone(),
        mode: "on_demand".to_string(),
        status: "running".to_string(),
        prompts_total: total,
        prompts_completed: 0,
        avg_overall_score: None,
        avg_structural: None,
        avg_command_accuracy: None,
        avg_phase_flow: None,
        avg_step_completeness: None,
        avg_prompt_quality: None,
        avg_determinism: None,
        error: None,
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
    };

    if let Err(e) = db.insert_eval_run(&run) {
        error!("Failed to create eval run: {}", e);
        return;
    }

    // Update in-memory state
    {
        let mut eval = state.evaluation.write().await;
        eval.running = true;
        eval.current_run_id = Some(run_id.clone());
        eval.current_prompt_index = 0;
        eval.total_prompts = prompts.len();
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Eval benchmark started: run_id={}, prompts={}",
                run_id,
                prompts.len()
            ),
        )
        .await;

    let http_client = state.http_client.clone();

    for (i, test_prompt) in prompts.iter().enumerate() {
        // Check for cancellation
        if *stop_rx.borrow() {
            info!("Eval run cancelled at prompt {}/{}", i, prompts.len());
            let _ = db.complete_eval_run(&run_id, "cancelled", None);
            break;
        }

        // Update progress
        {
            let mut eval = state.evaluation.write().await;
            eval.current_prompt_index = i;
        }

        let result_started = Utc::now().to_rfc3339();
        let gen_start = std::time::Instant::now();

        info!(
            "Evaluating prompt {}/{}: '{}'",
            i + 1,
            prompts.len(),
            test_prompt.id
        );

        // Generate workflow
        let gen_result = generate_workflow_for_eval(&http_client, &test_prompt.prompt).await;

        let gen_duration = gen_start.elapsed().as_millis() as i64;

        match gen_result {
            Ok((task_run_id, workflow_id, workflow_json)) => {
                // Score the workflow
                let score_start = std::time::Instant::now();
                let score_result =
                    super::judge::score_workflow(&state, test_prompt, &workflow_json).await;
                let score_duration = score_start.elapsed().as_millis() as i64;

                let result = match score_result {
                    Ok(scores) => EvalResult {
                        id: 0,
                        run_id: run_id.clone(),
                        test_prompt_id: test_prompt.id.clone(),
                        generated_workflow_json: Some(workflow_json),
                        task_run_id: Some(task_run_id),
                        workflow_id: Some(workflow_id),
                        structural_correctness: Some(scores.structural_correctness.score),
                        command_accuracy: Some(scores.command_accuracy.score),
                        phase_flow_logic: Some(scores.phase_flow_logic.score),
                        step_completeness: Some(scores.step_completeness.score),
                        prompt_quality: Some(scores.prompt_quality.score),
                        determinism: Some(scores.determinism.score),
                        overall_score: Some(scores.overall()),
                        score_rationales: serde_json::to_string(&scores).ok(),
                        generation_error: None,
                        scoring_error: None,
                        generation_duration_ms: Some(gen_duration),
                        scoring_duration_ms: Some(score_duration),
                        started_at: result_started,
                        completed_at: Some(Utc::now().to_rfc3339()),
                    },
                    Err(e) => {
                        warn!("Scoring failed for '{}': {}", test_prompt.id, e);
                        EvalResult {
                            id: 0,
                            run_id: run_id.clone(),
                            test_prompt_id: test_prompt.id.clone(),
                            generated_workflow_json: Some(workflow_json),
                            task_run_id: Some(task_run_id),
                            workflow_id: Some(workflow_id),
                            structural_correctness: None,
                            command_accuracy: None,
                            phase_flow_logic: None,
                            step_completeness: None,
                            prompt_quality: None,
                            determinism: None,
                            overall_score: None,
                            score_rationales: None,
                            generation_error: None,
                            scoring_error: Some(e.to_string()),
                            generation_duration_ms: Some(gen_duration),
                            scoring_duration_ms: Some(score_duration),
                            started_at: result_started,
                            completed_at: Some(Utc::now().to_rfc3339()),
                        }
                    }
                };

                let _ = db.insert_eval_result(&result);
            }
            Err(e) => {
                warn!("Generation failed for '{}': {}", test_prompt.id, e);
                let result = EvalResult {
                    id: 0,
                    run_id: run_id.clone(),
                    test_prompt_id: test_prompt.id.clone(),
                    generated_workflow_json: None,
                    task_run_id: None,
                    workflow_id: None,
                    structural_correctness: None,
                    command_accuracy: None,
                    phase_flow_logic: None,
                    step_completeness: None,
                    prompt_quality: None,
                    determinism: None,
                    overall_score: None,
                    score_rationales: None,
                    generation_error: Some(e.to_string()),
                    scoring_error: None,
                    generation_duration_ms: Some(gen_duration),
                    scoring_duration_ms: None,
                    started_at: result_started,
                    completed_at: Some(Utc::now().to_rfc3339()),
                };
                let _ = db.insert_eval_result(&result);
            }
        }

        // Update progress in DB
        let _ = db.update_eval_run_progress(&run_id, (i + 1) as i64);
    }

    // Complete the run (unless it was cancelled above)
    if !*stop_rx.borrow() {
        let _ = db.complete_eval_run(&run_id, "completed", None);
    }

    // Clear in-memory state
    {
        let mut eval = state.evaluation.write().await;
        eval.running = false;
        eval.current_run_id = None;
        eval.current_prompt_index = 0;
        eval.total_prompts = 0;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Eval benchmark completed: run_id={}", run_id),
        )
        .await;
}

/// Run eval in a continuous loop with configurable interval.
pub async fn run_continuous(
    db: Arc<EvalDb>,
    state: SharedState,
    interval_secs: u64,
    stop_rx: watch::Receiver<bool>,
) {
    info!(
        "Starting continuous eval mode with {}s interval",
        interval_secs
    );

    loop {
        if *stop_rx.borrow() {
            info!("Continuous eval stopped");
            break;
        }

        // Run one eval pass
        let inner_stop_rx = stop_rx.clone();
        run_eval(db.clone(), state.clone(), None, inner_stop_rx).await;

        // Sleep for interval, checking stop signal
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(interval_secs);
        loop {
            if *stop_rx.borrow() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // Re-check the stop signal
            let _ = stop_rx.has_changed();
        }
    }

    // Clear in-memory state
    {
        let mut eval = state.evaluation.write().await;
        eval.running = false;
        eval.continuous_mode = false;
        eval.current_run_id = None;
    }
}
