use chrono::Utc;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

use crate::config::{AI_DEBUG_COOLDOWN_SECS, AI_MODELS, RUNNER_API_PORT};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;

/// Schedule an AI debug session, deferring if code is being edited or external Claude is running.
pub async fn schedule_debug(state: &SharedState, reason: &str) {
    let auto_enabled = {
        let ai = state.ai.read().await;
        ai.auto_debug_enabled
    };

    if !auto_enabled {
        return;
    }

    let (editing, external) = {
        let ca = state.code_activity.read().await;
        (ca.code_being_edited, ca.external_claude_session)
    };

    if editing || external {
        // Defer the debug session
        let defer_reason = if editing {
            "Code being edited"
        } else {
            "External Claude session detected"
        };
        info!("Deferring AI debug ({}): {}", defer_reason, reason);
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!("AI debug deferred ({}), reason: {}", defer_reason, reason),
            )
            .await;

        let mut ca = state.code_activity.write().await;
        ca.pending_debug = true;
        ca.pending_debug_reason = Some(reason.to_string());
        return;
    }

    // Try to spawn immediately
    if let Err(e) = spawn_ai_debug(state, Some(reason)).await {
        warn!("Failed to schedule AI debug: {}", e);
    }
}

/// Spawn an AI debug session. Returns error if guards prevent spawning.
pub async fn spawn_ai_debug(
    state: &SharedState,
    reason: Option<&str>,
) -> Result<(), SupervisorError> {
    // Guard: already running
    {
        let ai = state.ai.read().await;
        if ai.running {
            return Err(SupervisorError::Other(
                "AI debug session already running".to_string(),
            ));
        }
    }

    // Guard: cooldown
    {
        let ai = state.ai.read().await;
        if let Some(last) = ai.last_debug_at {
            let elapsed = (Utc::now() - last).num_seconds();
            if elapsed < AI_DEBUG_COOLDOWN_SECS {
                return Err(SupervisorError::Other(format!(
                    "AI debug cooldown: {}s remaining",
                    AI_DEBUG_COOLDOWN_SECS - elapsed
                )));
            }
        }
    }

    // Guard: code being edited
    {
        let ca = state.code_activity.read().await;
        if ca.code_being_edited {
            return Err(SupervisorError::Other(
                "Code is being edited, deferring debug".to_string(),
            ));
        }
        if ca.external_claude_session {
            return Err(SupervisorError::Other(
                "External Claude session detected, deferring debug".to_string(),
            ));
        }
    }

    // Get provider/model config
    let (provider, model_key, model_id) = {
        let ai = state.ai.read().await;
        let provider = ai.provider.clone();
        let model_key = ai.model.clone();
        let model_id = resolve_model_id(&provider, &model_key)
            .unwrap_or_else(|| "claude-opus-4-6".to_string());
        (provider, model_key, model_id)
    };

    // Build the debug prompt
    let prompt = build_debug_prompt(state, reason).await;

    info!(
        "Spawning AI debug session: provider={}, model={} ({})",
        provider, model_key, model_id
    );
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Starting AI debug session ({}/{}): {}",
                provider,
                model_key,
                reason.unwrap_or("manual trigger")
            ),
        )
        .await;

    // Write prompt to temp file
    let temp_dir = std::env::temp_dir();
    let prompt_file = temp_dir.join("qontinui-debug-prompt.md");
    if let Err(e) = tokio::fs::write(&prompt_file, &prompt).await {
        return Err(SupervisorError::Other(format!(
            "Failed to write prompt file: {}",
            e
        )));
    }

    // Spawn the AI process
    let child = match provider.as_str() {
        "claude" => spawn_claude_process(&prompt_file, &model_id).await?,
        "gemini" => spawn_gemini_process(&prompt, &model_id).await?,
        _ => {
            return Err(SupervisorError::Other(format!(
                "Unknown provider: {}",
                provider
            )));
        }
    };

    // Update state
    {
        let mut ai = state.ai.write().await;
        ai.process = Some(child);
        ai.running = true;
        ai.session_started_at = Some(Utc::now());
        ai.last_debug_at = Some(Utc::now());
        ai.output_buffer.clear();
    }

    state.notify_health_change();

    // Spawn output capture task
    spawn_output_capture(state.clone());

    Ok(())
}

/// Stop a running AI debug session.
pub async fn stop_ai_debug(state: &SharedState) -> Result<(), SupervisorError> {
    let mut ai = state.ai.write().await;
    if !ai.running {
        return Err(SupervisorError::Other(
            "No AI debug session running".to_string(),
        ));
    }

    if let Some(ref mut child) = ai.process {
        let _ = child.kill().await;
        info!("AI debug session killed");
    }

    ai.process = None;
    ai.running = false;
    ai.session_started_at = None;
    drop(ai);

    state.notify_health_change();

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "AI debug session stopped",
        )
        .await;

    Ok(())
}

/// Build the debug prompt with context from logs, git, and runner state.
async fn build_debug_prompt(state: &SharedState, reason: Option<&str>) -> String {
    let mut sections = Vec::new();

    sections.push("# AI Debug Session\n".to_string());
    sections.push(format!(
        "**Trigger:** {}\n",
        reason.unwrap_or("Manual trigger")
    ));
    sections.push(
        "**Constraint:** Do NOT explore the filesystem beyond reading specific files mentioned in error messages. Focus on diagnosing and suggesting fixes.\n".to_string()
    );

    // Recent runner logs
    let log_lines = get_recent_runner_logs(state).await;
    if !log_lines.is_empty() {
        sections.push("## Recent Runner Logs (last 100 lines)\n".to_string());
        sections.push(format!("```\n{}\n```\n", log_lines));
    }

    // Build error output
    {
        let build = state.build.read().await;
        if build.build_error_detected {
            if let Some(ref err) = build.last_build_error {
                sections.push("## Build Error\n".to_string());
                sections.push(format!("```\n{}\n```\n", err));
            }
        }
    }

    // Recent git changes
    let git_info = get_git_info(state).await;
    if !git_info.is_empty() {
        sections.push("## Recent Git Changes\n".to_string());
        sections.push(format!("```\n{}\n```\n", git_info));
    }

    // Running task context from runner API
    let task_info = get_running_tasks().await;
    if !task_info.is_empty() {
        sections.push("## Running Tasks\n".to_string());
        sections.push(format!("```\n{}\n```\n", task_info));
    }

    sections.join("\n")
}

/// Get recent runner log lines from the dev-logs file.
async fn get_recent_runner_logs(state: &SharedState) -> String {
    let log_path = state.config.dev_logs_dir.join("runner-tauri.log");
    if !log_path.exists() {
        return String::new();
    }

    // Read the file as bytes, strip null bytes (UTF-16LE handling)
    match tokio::fs::read(&log_path).await {
        Ok(bytes) => {
            let cleaned: Vec<u8> = bytes.into_iter().filter(|b| *b != 0).collect();
            let content = String::from_utf8_lossy(&cleaned);
            let lines: Vec<&str> = content.lines().collect();
            let start = if lines.len() > 100 {
                lines.len() - 100
            } else {
                0
            };
            lines[start..].join("\n")
        }
        Err(_) => String::new(),
    }
}

/// Get recent git log and diff stat.
async fn get_git_info(state: &SharedState) -> String {
    let project_dir = &state.config.project_dir;
    let runner_root = project_dir.parent().unwrap_or(project_dir);

    let mut info = String::new();

    // git log --oneline -5
    let mut git_log_cmd = tokio::process::Command::new("git");
    git_log_cmd
        .args(["log", "--oneline", "-5"])
        .current_dir(runner_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    git_log_cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    if let Ok(output) = git_log_cmd.output().await {
        if output.status.success() {
            info.push_str("Recent commits:\n");
            info.push_str(&String::from_utf8_lossy(&output.stdout));
            info.push('\n');
        }
    }

    // git diff --stat
    let mut git_diff_cmd = tokio::process::Command::new("git");
    git_diff_cmd
        .args(["diff", "--stat"])
        .current_dir(runner_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    git_diff_cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    if let Ok(output) = git_diff_cmd.output().await {
        if output.status.success() {
            let diff = String::from_utf8_lossy(&output.stdout);
            if !diff.trim().is_empty() {
                info.push_str("Uncommitted changes:\n");
                info.push_str(&diff);
            }
        }
    }

    info
}

/// Query the runner API for running tasks.
async fn get_running_tasks() -> String {
    let url = format!("http://127.0.0.1:{}/task-runs/running", RUNNER_API_PORT);
    match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.text().await.unwrap_or_default(),
        _ => String::new(),
    }
}

/// Spawn a Claude CLI process.
async fn spawn_claude_process(
    prompt_file: &std::path::Path,
    model_id: &str,
) -> Result<tokio::process::Child, SupervisorError> {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([
        "--print",
        &prompt_file.display().to_string(),
        "--permission-mode",
        "bypassPermissions",
        "--output-format",
        "text",
        "--model",
        model_id,
    ])
    .env_remove("CLAUDECODE")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = cmd
        .spawn()
        .map_err(|e| SupervisorError::Process(format!("Failed to spawn Claude CLI: {}", e)))?;

    Ok(child)
}

/// Spawn a Gemini CLI process via PowerShell.
async fn spawn_gemini_process(
    prompt: &str,
    model_id: &str,
) -> Result<tokio::process::Child, SupervisorError> {
    // Write a PowerShell script that pipes the prompt to gemini
    let temp_dir = std::env::temp_dir();
    let script_path = temp_dir.join("qontinui-gemini-debug.ps1");
    let prompt_file = temp_dir.join("qontinui-debug-prompt.md");

    let script_content = format!(
        "Get-Content -Raw '{}' | gemini --yolo -o text -m '{}'",
        prompt_file.display(),
        model_id
    );

    tokio::fs::write(&prompt_file, prompt)
        .await
        .map_err(|e| SupervisorError::Other(format!("Failed to write prompt: {}", e)))?;

    tokio::fs::write(&script_path, &script_content)
        .await
        .map_err(|e| SupervisorError::Other(format!("Failed to write script: {}", e)))?;

    let mut cmd = tokio::process::Command::new("powershell.exe");
    cmd.args([
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        &script_path.display().to_string(),
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = cmd
        .spawn()
        .map_err(|e| SupervisorError::Process(format!("Failed to spawn Gemini CLI: {}", e)))?;

    Ok(child)
}

/// Spawn a background task to capture AI process output.
fn spawn_output_capture(state: SharedState) {
    tokio::spawn(async move {
        // Take the process stdout/stderr handles
        let (stdout, stderr) = {
            let mut ai = state.ai.write().await;
            let child = match ai.process.as_mut() {
                Some(c) => c,
                None => return,
            };
            (child.stdout.take(), child.stderr.take())
        };

        let state_stdout = state.clone();
        let state_stderr = state.clone();

        // Spawn stdout reader
        let stdout_handle = stdout.map(|stdout| {
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut ai = state_stdout.ai.write().await;
                    ai.push_output("stdout", line);
                }
            })
        });

        // Spawn stderr reader
        let stderr_handle = stderr.map(|stderr| {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut ai = state_stderr.ai.write().await;
                    ai.push_output("stderr", line);
                }
            })
        });

        // Wait for both to finish
        if let Some(h) = stdout_handle {
            let _ = h.await;
        }
        if let Some(h) = stderr_handle {
            let _ = h.await;
        }

        // Wait for process exit — take child out to avoid holding lock across await
        let child = {
            let mut ai = state.ai.write().await;
            ai.process.take()
        };
        if let Some(mut child) = child {
            let _ = child.wait().await;
        }
        {
            let mut ai = state.ai.write().await;
            ai.running = false;
            ai.session_started_at = None;
        }

        state.notify_health_change();

        info!("AI debug session completed");
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                "AI debug session completed",
            )
            .await;
    });
}

/// Resolve a model key to its full model ID.
pub fn resolve_model_id(provider: &str, key: &str) -> Option<String> {
    AI_MODELS
        .iter()
        .find(|(p, k, _, _)| *p == provider && *k == key)
        .map(|(_, _, id, _)| id.to_string())
}
