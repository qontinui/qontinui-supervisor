use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::ai_debug::resolve_model_id;
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;
use crate::velocity_tests::db::VelocityTestDb;
use crate::velocity_tests::VelocityTestResult;

// ============================================================================
// Config
// ============================================================================

fn default_max_iterations() -> u32 {
    5
}

fn default_target_score() -> f64 {
    80.0
}

fn default_fix_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
pub struct VelocityImprovementConfig {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_target_score")]
    pub target_score: f64,
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(default = "default_fix_timeout")]
    pub fix_timeout_secs: u64,
    #[serde(default)]
    pub restart_backend: bool,
}

// ============================================================================
// State
// ============================================================================

pub struct VelocityImprovementState {
    pub running: bool,
    pub phase: VelocityImprovementPhase,
    pub current_iteration: u32,
    pub max_iterations: u32,
    pub target_score: f64,
    pub started_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub iterations: Vec<VelocityImprovementIteration>,
    pub stop_tx: Option<watch::Sender<bool>>,
}

impl VelocityImprovementState {
    pub fn new() -> Self {
        Self {
            running: false,
            phase: VelocityImprovementPhase::Idle,
            current_iteration: 0,
            max_iterations: 5,
            target_score: 80.0,
            started_at: None,
            error: None,
            iterations: Vec::new(),
            stop_tx: None,
        }
    }
}

impl Default for VelocityImprovementState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Types
// ============================================================================

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VelocityImprovementPhase {
    Idle,
    RunningTests,
    Analyzing,
    Fixing,
    RestartingFrontend,
    WaitingFrontend,
    Complete,
    Stopped,
    Error,
}

impl std::fmt::Display for VelocityImprovementPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::RunningTests => write!(f, "running_tests"),
            Self::Analyzing => write!(f, "analyzing"),
            Self::Fixing => write!(f, "fixing"),
            Self::RestartingFrontend => write!(f, "restarting_frontend"),
            Self::WaitingFrontend => write!(f, "waiting_frontend"),
            Self::Complete => write!(f, "complete"),
            Self::Stopped => write!(f, "stopped"),
            Self::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VelocityImprovementIteration {
    pub iteration: u32,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub run_id: Option<String>,
    pub overall_score: Option<f64>,
    pub per_page_scores: Vec<PageScore>,
    pub fix_applied: bool,
    pub fix_summary: Option<String>,
    pub exit_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PageScore {
    pub name: String,
    pub score: f64,
    pub bottleneck: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VelocityImprovementStatus {
    pub running: bool,
    pub phase: VelocityImprovementPhase,
    pub current_iteration: u32,
    pub max_iterations: u32,
    pub target_score: f64,
    pub started_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VelocityImprovementHistory {
    pub iterations: Vec<VelocityImprovementIteration>,
}

// ============================================================================
// Main loop
// ============================================================================

pub async fn run_velocity_improvement_loop(
    db: Arc<VelocityTestDb>,
    state: SharedState,
    config: VelocityImprovementConfig,
    stop_rx: watch::Receiver<bool>,
) {
    let mut no_improvement_streak: u32 = 0;
    let mut previous_score: Option<f64> = None;

    for iteration in 1..=config.max_iterations {
        if *stop_rx.borrow() {
            set_phase(&state, VelocityImprovementPhase::Stopped).await;
            log(
                &state,
                LogLevel::Info,
                "Velocity improvement stopped by user",
            )
            .await;
            return;
        }

        let iter_started = Utc::now().to_rfc3339();

        // Update state
        {
            let mut vi = state.velocity_improvement.write().await;
            vi.current_iteration = iteration;
        }

        log(
            &state,
            LogLevel::Info,
            format!(
                "=== Velocity Improvement Iteration {}/{} ===",
                iteration, config.max_iterations
            ),
        )
        .await;

        // ------------------------------------------------------------------
        // Phase 1: Run velocity tests
        // ------------------------------------------------------------------
        set_phase(&state, VelocityImprovementPhase::RunningTests).await;
        log(
            &state,
            LogLevel::Info,
            format!("Running velocity tests (iteration {})...", iteration),
        )
        .await;

        // Check that velocity tests aren't already running
        {
            let vt = state.velocity_tests.read().await;
            if vt.running {
                set_error(
                    &state,
                    "Velocity tests are already running from another source",
                )
                .await;
                return;
            }
        }

        // Create a stop channel for velocity tests
        let (vt_stop_tx, vt_stop_rx) = watch::channel(false);

        // Mark velocity tests as running
        {
            let mut vt = state.velocity_tests.write().await;
            vt.running = true;
            vt.stop_tx = Some(vt_stop_tx);
        }

        // Run tests (this blocks until complete)
        let db_clone = db.clone();
        let state_clone = state.clone();

        // Spawn tests as a background task
        let test_handle = tokio::spawn(async move {
            crate::velocity_tests::engine::run_velocity_tests(db_clone, state_clone, vt_stop_rx)
                .await;
        });

        // Poll for completion, checking our stop signal periodically
        loop {
            if *stop_rx.borrow() {
                // Stop signal received — cancel velocity tests
                let mut vt = state.velocity_tests.write().await;
                if let Some(tx) = vt.stop_tx.take() {
                    let _ = tx.send(true);
                }
                let _ = test_handle.await;
                set_phase(&state, VelocityImprovementPhase::Stopped).await;
                log(
                    &state,
                    LogLevel::Info,
                    "Velocity improvement stopped by user",
                )
                .await;
                finalize(&state).await;
                return;
            }

            if test_handle.is_finished() {
                break;
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // ------------------------------------------------------------------
        // Phase 2: Analyze results
        // ------------------------------------------------------------------
        set_phase(&state, VelocityImprovementPhase::Analyzing).await;

        // Find the most recent completed run
        let runs = db.list_runs().unwrap_or_default();
        let latest_run = runs.into_iter().find(|r| r.status == "completed");

        let (run_id, overall_score, per_page_scores, results) = match latest_run {
            Some(run) => {
                let results = db.get_results_for_run(&run.id).unwrap_or_default();
                let page_scores: Vec<PageScore> = results
                    .iter()
                    .map(|r| PageScore {
                        name: r.test_name.clone(),
                        score: r.score.unwrap_or(0.0),
                        bottleneck: r
                            .bottleneck
                            .clone()
                            .unwrap_or_else(|| "Unknown".to_string()),
                    })
                    .collect();
                let score = run.overall_score.unwrap_or(0.0);
                (Some(run.id.clone()), Some(score), page_scores, results)
            }
            None => {
                set_error(&state, "No completed velocity test run found").await;
                return;
            }
        };

        let score = overall_score.unwrap_or(0.0);
        log(
            &state,
            LogLevel::Info,
            format!(
                "Iteration {} score: {:.1} (target: {:.1})",
                iteration, score, config.target_score
            ),
        )
        .await;

        // Check exit conditions
        let exit_reason = check_exit_conditions(
            score,
            config.target_score,
            iteration,
            config.max_iterations,
            previous_score,
            no_improvement_streak,
        );

        // Record iteration
        let mut iter_result = VelocityImprovementIteration {
            iteration,
            started_at: iter_started,
            completed_at: None,
            run_id,
            overall_score: Some(score),
            per_page_scores,
            fix_applied: false,
            fix_summary: None,
            exit_reason: exit_reason.clone(),
        };

        if let Some(reason) = &exit_reason {
            iter_result.completed_at = Some(Utc::now().to_rfc3339());
            {
                let mut vi = state.velocity_improvement.write().await;
                vi.iterations.push(iter_result);
            }
            log(&state, LogLevel::Info, format!("Exiting: {}", reason)).await;
            set_phase(&state, VelocityImprovementPhase::Complete).await;
            finalize(&state).await;
            return;
        }

        // Update stagnation tracking
        if let Some(prev) = previous_score {
            if score <= prev {
                no_improvement_streak += 1;
            } else {
                no_improvement_streak = 0;
            }
        }
        previous_score = Some(score);

        // ------------------------------------------------------------------
        // Phase 3: Fix issues
        // ------------------------------------------------------------------
        set_phase(&state, VelocityImprovementPhase::Fixing).await;

        let prompt =
            build_velocity_fix_prompt(&results, iteration, previous_score, config.target_score);

        let fix_result = spawn_fix_agent(&state, &prompt, &config, &stop_rx).await;

        match fix_result {
            Ok(summary) => {
                iter_result.fix_applied = true;
                iter_result.fix_summary = Some(summary);
            }
            Err(e) => {
                if e.contains("Stop requested") {
                    iter_result.completed_at = Some(Utc::now().to_rfc3339());
                    {
                        let mut vi = state.velocity_improvement.write().await;
                        vi.iterations.push(iter_result);
                    }
                    set_phase(&state, VelocityImprovementPhase::Stopped).await;
                    log(
                        &state,
                        LogLevel::Info,
                        "Velocity improvement stopped by user",
                    )
                    .await;
                    finalize(&state).await;
                    return;
                }
                warn!("Fix agent failed: {}", e);
                iter_result.fix_summary = Some(format!("Fix failed: {}", e));
            }
        }

        iter_result.completed_at = Some(Utc::now().to_rfc3339());
        {
            let mut vi = state.velocity_improvement.write().await;
            vi.iterations.push(iter_result);
        }

        // ------------------------------------------------------------------
        // Phase 4: Restart frontend
        // ------------------------------------------------------------------
        set_phase(&state, VelocityImprovementPhase::RestartingFrontend).await;
        log(&state, LogLevel::Info, "Restarting frontend...").await;

        let restart_result = restart_frontend(&state, config.restart_backend).await;
        if let Err(e) = restart_result {
            warn!("Frontend restart failed: {}", e);
            // Continue anyway — the frontend might still be running with old code
        }

        // ------------------------------------------------------------------
        // Phase 5: Wait for frontend
        // ------------------------------------------------------------------
        set_phase(&state, VelocityImprovementPhase::WaitingFrontend).await;
        log(
            &state,
            LogLevel::Info,
            "Waiting for frontend to become healthy...",
        )
        .await;

        if let Err(e) = wait_for_frontend(&state, &stop_rx).await {
            if e.contains("Stop requested") {
                set_phase(&state, VelocityImprovementPhase::Stopped).await;
                finalize(&state).await;
                return;
            }
            set_error(&state, format!("Frontend health check failed: {}", e)).await;
            return;
        }

        log(
            &state,
            LogLevel::Info,
            "Frontend is healthy, proceeding to next iteration",
        )
        .await;

        // Brief pause before next iteration
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // If we get here, we exhausted max_iterations without meeting target
    set_phase(&state, VelocityImprovementPhase::Complete).await;
    log(
        &state,
        LogLevel::Info,
        "Velocity improvement loop completed (max iterations reached)",
    )
    .await;
    finalize(&state).await;
}

// ============================================================================
// Exit condition checks
// ============================================================================

fn check_exit_conditions(
    score: f64,
    target: f64,
    iteration: u32,
    max_iterations: u32,
    previous_score: Option<f64>,
    no_improvement_streak: u32,
) -> Option<String> {
    if score >= target {
        return Some(format!(
            "Target score reached: {:.1} >= {:.1}",
            score, target
        ));
    }

    if iteration >= max_iterations {
        return Some(format!(
            "Max iterations reached ({}/{})",
            iteration, max_iterations
        ));
    }

    if no_improvement_streak >= 2 {
        return Some(format!(
            "Stagnation: no improvement for {} consecutive iterations",
            no_improvement_streak
        ));
    }

    if let Some(prev) = previous_score {
        if score < prev - 5.0 {
            return Some(format!(
                "Score regression: {:.1} -> {:.1} (decreased by {:.1})",
                prev,
                score,
                prev - score
            ));
        }
    }

    None
}

// ============================================================================
// Prompt builder
// ============================================================================

fn build_velocity_fix_prompt(
    results: &[VelocityTestResult],
    iteration: u32,
    previous_score: Option<f64>,
    target_score: f64,
) -> String {
    let mut prompt = String::new();

    // Section 1: Context
    prompt.push_str("# Frontend Performance Fix Request\n\n");
    prompt.push_str("You are fixing frontend performance issues in `qontinui-web/frontend`.\n");
    prompt.push_str("The current working directory is the `qontinui_parent` directory which contains `qontinui-web/frontend/`.\n\n");

    // Section 2: Current Scores
    prompt.push_str("## Current Scores\n\n");
    prompt.push_str(&format!(
        "**Target score: {:.0}** | **Iteration: {}**\n\n",
        target_score, iteration
    ));
    prompt.push_str("| Page | Score | Bottleneck |\n");
    prompt.push_str("|------|-------|------------|\n");
    for r in results {
        prompt.push_str(&format!(
            "| {} | {:.1} | {} |\n",
            r.test_name,
            r.score.unwrap_or(0.0),
            r.bottleneck.as_deref().unwrap_or("Unknown")
        ));
    }
    prompt.push('\n');

    // Section 3: Per-page Diagnostics
    prompt.push_str("## Per-Page Diagnostics\n\n");
    for r in results {
        let score = r.score.unwrap_or(0.0);
        if score >= target_score {
            continue; // Skip pages already meeting target
        }

        prompt.push_str(&format!(
            "### {} (score: {:.1}, bottleneck: {})\n\n",
            r.test_name,
            score,
            r.bottleneck.as_deref().unwrap_or("Unknown")
        ));

        // Parse diagnostics JSON
        if let Some(diag_str) = &r.diagnostics_json {
            if let Ok(diag) = serde_json::from_str::<serde_json::Value>(diag_str) {
                // Top slow resources
                if let Some(resources) = diag.get("resources").and_then(|r| r.as_array()) {
                    let mut sorted: Vec<_> = resources
                        .iter()
                        .filter_map(|r| {
                            let duration = r.get("duration").and_then(|v| v.as_f64())?;
                            let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let size = r.get("transferSize").and_then(|v| v.as_i64()).unwrap_or(0);
                            let kind = r
                                .get("initiatorType")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            Some((name.to_string(), kind.to_string(), duration, size))
                        })
                        .collect();
                    sorted
                        .sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
                    sorted.truncate(5);

                    if !sorted.is_empty() {
                        prompt.push_str("**Top slow resources:**\n\n");
                        prompt.push_str("| Resource | Type | Duration (ms) | Size (KB) |\n");
                        prompt.push_str("|----------|------|---------------|-----------|\n");
                        for (name, kind, dur, size) in &sorted {
                            // Shorten URL to just filename
                            let short_name = name
                                .rsplit('/')
                                .next()
                                .unwrap_or(name)
                                .split('?')
                                .next()
                                .unwrap_or(name);
                            prompt.push_str(&format!(
                                "| {} | {} | {:.0} | {:.1} |\n",
                                short_name,
                                kind,
                                dur,
                                *size as f64 / 1024.0
                            ));
                        }
                        prompt.push('\n');
                    }
                }

                // Long tasks
                if let Some(long_tasks) = diag.get("longTasks").and_then(|lt| lt.as_array()) {
                    if !long_tasks.is_empty() {
                        let total_ms: f64 = long_tasks
                            .iter()
                            .filter_map(|t| t.get("durationMs").and_then(|v| v.as_f64()))
                            .sum();
                        prompt.push_str(&format!(
                            "**Long tasks:** {} tasks, {:.0}ms total blocking time\n\n",
                            long_tasks.len(),
                            total_ms
                        ));
                    }
                }

                // Script attribution from LoAF
                if let Some(scripts) = diag.get("scriptAttribution").and_then(|s| s.as_array()) {
                    if !scripts.is_empty() {
                        prompt.push_str("**Script attribution (LoAF):**\n\n");
                        prompt.push_str("| Source | Function | Duration (ms) |\n");
                        prompt.push_str("|--------|----------|---------------|\n");
                        for s in scripts.iter().take(5) {
                            let source = s.get("sourceURL").and_then(|v| v.as_str()).unwrap_or("");
                            let short_source = source
                                .rsplit('/')
                                .next()
                                .unwrap_or(source)
                                .split('?')
                                .next()
                                .unwrap_or(source);
                            let func = s
                                .get("sourceFunctionName")
                                .and_then(|v| v.as_str())
                                .unwrap_or("(anonymous)");
                            let dur = s.get("duration").and_then(|v| v.as_i64()).unwrap_or(0);
                            prompt
                                .push_str(&format!("| {} | {} | {} |\n", short_source, func, dur));
                        }
                        prompt.push('\n');
                    }
                }
            }
        }

        // Console errors
        if r.console_errors > 0 {
            prompt.push_str(&format!("**Console errors:** {}\n\n", r.console_errors));
        }
    }

    // Section 4: Previous attempt
    if iteration > 1 {
        if let Some(prev) = previous_score {
            prompt.push_str("## Previous Attempt\n\n");
            prompt.push_str(&format!("Previous iteration scored {:.1}. ", prev));
            let current_avg: f64 =
                results.iter().filter_map(|r| r.score).sum::<f64>() / results.len().max(1) as f64;
            if current_avg > prev {
                prompt.push_str(&format!(
                    "Current score is {:.1} (improved by {:.1}). Continue improving.\n\n",
                    current_avg,
                    current_avg - prev
                ));
            } else {
                prompt.push_str(&format!(
                    "Current score is {:.1} (no improvement). Try a different approach.\n\n",
                    current_avg
                ));
            }
        }
    }

    // Section 5: Bottleneck-specific instructions
    prompt.push_str("## Fix Instructions\n\n");
    prompt.push_str("Apply fixes based on the bottleneck type identified for each page:\n\n");
    prompt.push_str("- **JS Blocking**: Code-split heavy components with `next/dynamic`, defer non-critical scripts, reduce synchronous work in component renders\n");
    prompt.push_str("- **Bundle Heavy**: Reduce imports (use specific subpath imports instead of barrel exports), lazy-load heavy dependencies, check for unnecessary polyfills\n");
    prompt.push_str("- **Render Slow**: Reduce DOM complexity, use `React.memo` for expensive renders, avoid layout thrashing\n");
    prompt.push_str("- **TTFB Slow / Backend Slow**: Skip — this is a backend issue, not fixable from frontend code\n");
    prompt.push_str("- **Network Slow**: Check for unoptimized images, missing compression, or redundant network calls\n\n");

    // Section 6: Constraints
    prompt.push_str("## Constraints\n\n");
    prompt.push_str("- Only modify files under `qontinui-web/frontend/`\n");
    prompt.push_str("- Do NOT modify backend code\n");
    prompt.push_str("- Focus on the highest-impact changes first\n");
    prompt.push_str("- Do NOT add new dependencies unless absolutely necessary\n");
    prompt.push_str("- Make targeted, surgical changes — do not refactor unrelated code\n");

    prompt
}

// ============================================================================
// Fix agent
// ============================================================================

async fn spawn_fix_agent(
    state: &SharedState,
    prompt: &str,
    config: &VelocityImprovementConfig,
    stop_rx: &watch::Receiver<bool>,
) -> Result<String, String> {
    // Write prompt to temp file
    let prompt_path = std::env::temp_dir().join("qontinui-velocity-fix-prompt.md");
    tokio::fs::write(&prompt_path, prompt)
        .await
        .map_err(|e| format!("Failed to write velocity fix prompt: {}", e))?;

    // Resolve provider/model
    let model_id = if let (Some(provider), Some(model)) = (&config.provider, &config.model) {
        resolve_model_id(provider, model).unwrap_or_else(|| "claude-opus-4-6".to_string())
    } else {
        let ai = state.ai.read().await;
        resolve_model_id(&ai.provider, &ai.model).unwrap_or_else(|| "claude-opus-4-6".to_string())
    };

    log(
        state,
        LogLevel::Info,
        format!("Spawning velocity fix agent (model={})...", model_id),
    )
    .await;

    // Spawn claude --print
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([
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
    .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn Claude CLI: {}", e))?;

    // Wait with timeout and stop signal
    let timeout = Duration::from_secs(config.fix_timeout_secs);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if *stop_rx.borrow() {
            let _ = child.kill().await;
            return Err("Stop requested during fix implementation".to_string());
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    log(
                        state,
                        LogLevel::Info,
                        "Velocity fix agent completed successfully",
                    )
                    .await;

                    // Capture output
                    let summary = if let Some(stdout) = child.stdout.take() {
                        use tokio::io::AsyncReadExt;
                        let mut buf = String::new();
                        let mut reader = tokio::io::BufReader::new(stdout);
                        let _ = reader.read_to_string(&mut buf).await;
                        if buf.len() > 500 {
                            format!("{}... [truncated]", &buf[..500])
                        } else if buf.is_empty() {
                            "Fixes applied (no output)".to_string()
                        } else {
                            buf
                        }
                    } else {
                        "Fixes applied".to_string()
                    };

                    return Ok(summary);
                } else {
                    let code = status.code().unwrap_or(-1);
                    warn!("Velocity fix agent exited with code {}", code);
                    return Err(format!("Fix agent exited with code {}", code));
                }
            }
            Ok(None) => {
                // Still running
                if tokio::time::Instant::now() >= deadline {
                    warn!(
                        "Velocity fix agent timed out after {}s",
                        config.fix_timeout_secs
                    );
                    let _ = child.kill().await;
                    return Err(format!(
                        "Fix agent timed out after {}s",
                        config.fix_timeout_secs
                    ));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                return Err(format!("Error checking fix agent status: {}", e));
            }
        }
    }
}

// ============================================================================
// Frontend restart + health check
// ============================================================================

async fn restart_frontend(state: &SharedState, restart_backend: bool) -> Result<(), String> {
    let client = &state.http_client;

    if restart_backend {
        let _ = client
            .post("http://localhost:9875/dev-start/backend")
            .timeout(Duration::from_secs(120))
            .send()
            .await;
    }

    let resp = client
        .post("http://localhost:9875/dev-start/frontend")
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("Frontend restart request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!(
            "Frontend restart returned status {}",
            resp.status()
        ));
    }

    Ok(())
}

async fn wait_for_frontend(
    state: &SharedState,
    stop_rx: &watch::Receiver<bool>,
) -> Result<(), String> {
    let client = &state.http_client;
    let max_wait = Duration::from_secs(120);
    let deadline = tokio::time::Instant::now() + max_wait;

    // Initial delay to let the frontend start
    tokio::time::sleep(Duration::from_secs(5)).await;

    loop {
        if *stop_rx.borrow() {
            return Err("Stop requested while waiting for frontend".to_string());
        }

        if tokio::time::Instant::now() >= deadline {
            return Err("Frontend health check timed out after 120s".to_string());
        }

        match client
            .get("http://localhost:3001")
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {
                info!("Frontend is responding");
                return Ok(());
            }
            _ => {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

async fn set_phase(state: &SharedState, phase: VelocityImprovementPhase) {
    let mut vi = state.velocity_improvement.write().await;
    vi.phase = phase;
}

async fn set_error(state: &SharedState, msg: impl Into<String>) {
    let msg = msg.into();
    error!("Velocity improvement error: {}", msg);
    let mut vi = state.velocity_improvement.write().await;
    vi.phase = VelocityImprovementPhase::Error;
    vi.error = Some(msg.clone());
    vi.running = false;
    vi.stop_tx = None;
}

async fn finalize(state: &SharedState) {
    let mut vi = state.velocity_improvement.write().await;
    vi.running = false;
    vi.stop_tx = None;
}

async fn log(state: &SharedState, level: LogLevel, msg: impl Into<String>) {
    state
        .logs
        .emit(LogSource::Supervisor, level, msg.into())
        .await;
}
