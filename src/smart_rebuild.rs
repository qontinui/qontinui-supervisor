use chrono::{DateTime, Utc};
use serde::Serialize;
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{error, info, warn};

use crate::build_monitor;
use crate::code_activity;
use crate::config::{
    SMART_REBUILD_CHECK_INTERVAL_SECS, SMART_REBUILD_FIX_TIMEOUT_SECS,
    SMART_REBUILD_MAX_FIX_ATTEMPTS, SMART_REBUILD_QUIET_PERIOD_SECS,
    SMART_REBUILD_RETRY_COOLDOWN_SECS,
};
use crate::diagnostics::DiagnosticEventKind;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager;
use crate::state::SharedState;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SmartRebuildPhase {
    Idle,
    WaitingForQuiet,
    StoppingRunner,
    Building,
    FixingWithAi { attempt: u32 },
    Restarting,
    Failed { reason: String },
}

pub struct SmartRebuildState {
    pub enabled: bool,
    pub phase: SmartRebuildPhase,
    pub last_successful_build_mtime: Option<SystemTime>,
    pub current_attempt: u32,
    pub last_build_error: Option<String>,
    pub total_rebuilds: u32,
    pub last_rebuild_at: Option<DateTime<Utc>>,
    pub last_failed_at: Option<DateTime<Utc>>,
}

impl SmartRebuildState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            phase: SmartRebuildPhase::Idle,
            last_successful_build_mtime: None,
            current_attempt: 0,
            last_build_error: None,
            total_rebuilds: 0,
            last_rebuild_at: None,
            last_failed_at: None,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self.phase, SmartRebuildPhase::Idle)
    }
}

impl Default for SmartRebuildState {
    fn default() -> Self {
        Self::new(false)
    }
}

/// Spawn the background source watcher that polls for file changes and triggers rebuilds.
pub fn spawn_source_watcher(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Smart rebuild source watcher started");
        let interval = Duration::from_secs(SMART_REBUILD_CHECK_INTERVAL_SECS);

        loop {
            tokio::time::sleep(interval).await;

            // Guard: enabled
            let (enabled, is_idle, is_failed, last_failed_at) = {
                let sr = state.smart_rebuild.read().await;
                (
                    sr.enabled,
                    sr.is_idle(),
                    matches!(sr.phase, SmartRebuildPhase::Failed { .. }),
                    sr.last_failed_at,
                )
            };
            if !enabled {
                continue;
            }

            // Auto-retry: if in Failed state and cooldown has elapsed, reset to Idle
            if is_failed {
                let cooldown_elapsed = last_failed_at
                    .map(|t| (chrono::Utc::now() - t).num_seconds() >= SMART_REBUILD_RETRY_COOLDOWN_SECS)
                    .unwrap_or(true);
                let editing = code_activity::is_code_being_edited(
                    &state.config.project_dir,
                    SMART_REBUILD_QUIET_PERIOD_SECS,
                );
                let external_claude = code_activity::is_external_claude_session().await;

                if cooldown_elapsed && !editing && !external_claude {
                    info!("Smart rebuild: retry cooldown elapsed, resetting from Failed to retry");
                    state
                        .logs
                        .emit(
                            LogSource::SmartRebuild,
                            LogLevel::Info,
                            "Retry cooldown elapsed, retrying smart rebuild",
                        )
                        .await;
                    state
                        .diagnostics
                        .write()
                        .await
                        .emit(DiagnosticEventKind::SmartRebuildStarted {
                            trigger: "auto_retry".to_string(),
                        });
                    let mut sr = state.smart_rebuild.write().await;
                    sr.phase = SmartRebuildPhase::Idle;
                    sr.current_attempt = 0;
                    sr.last_build_error = None;
                    // Clear last_successful_build_mtime to force rebuild even if no new changes
                    sr.last_successful_build_mtime = None;
                }
                continue;
            }

            if !is_idle {
                continue;
            }

            // Guard: no build in progress
            {
                let build = state.build.read().await;
                if build.build_in_progress {
                    continue;
                }
            }

            // Guard: no workflow loop running
            {
                let wl = state.workflow_loop.read().await;
                if wl.running {
                    continue;
                }
            }

            // Guard: no manual stop/restart in progress
            {
                let runner = state.runner.read().await;
                if runner.stop_requested || runner.restart_requested {
                    continue;
                }
            }

            // Check for source modifications
            let last_mtime = code_activity::get_last_source_modification(&state.config.project_dir);
            let last_mtime = match last_mtime {
                Some(t) => t,
                None => continue,
            };

            // Compare against last successful build mtime
            let last_build_mtime = {
                let sr = state.smart_rebuild.read().await;
                sr.last_successful_build_mtime
            };
            if let Some(build_mtime) = last_build_mtime {
                if last_mtime <= build_mtime {
                    continue; // No new changes since last build
                }
            }

            // Check quiet period — wait for files to stop changing
            let elapsed = SystemTime::now()
                .duration_since(last_mtime)
                .unwrap_or(Duration::from_secs(0));
            if (elapsed.as_secs() as i64) < SMART_REBUILD_QUIET_PERIOD_SECS {
                let mut sr = state.smart_rebuild.write().await;
                if sr.is_idle() {
                    sr.phase = SmartRebuildPhase::WaitingForQuiet;
                }
                continue;
            }

            // Quiet period elapsed — trigger rebuild
            info!("Smart rebuild: source changes detected, triggering rebuild");
            state
                .logs
                .emit(
                    LogSource::SmartRebuild,
                    LogLevel::Info,
                    "Source changes detected after quiet period, triggering smart rebuild",
                )
                .await;

            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::SmartRebuildStarted {
                    trigger: "source_change".to_string(),
                });

            let state_clone = state.clone();
            tokio::spawn(async move {
                run_smart_rebuild(&state_clone).await;
            });
        }
    })
}

/// Manually trigger a smart rebuild, skipping file change detection.
pub async fn trigger_smart_rebuild(state: &SharedState) {
    // Guard: not already running
    {
        let sr = state.smart_rebuild.read().await;
        if !sr.is_idle() && !matches!(sr.phase, SmartRebuildPhase::Failed { .. }) {
            warn!("Smart rebuild: already in progress, skipping manual trigger");
            return;
        }
    }

    info!("Smart rebuild: manually triggered");
    state
        .logs
        .emit(
            LogSource::SmartRebuild,
            LogLevel::Info,
            "Smart rebuild manually triggered",
        )
        .await;

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::SmartRebuildStarted {
            trigger: "manual".to_string(),
        });

    run_smart_rebuild(state).await;
}

/// Execute the full smart rebuild flow: stop → build → (fix if needed) → restart.
async fn run_smart_rebuild(state: &SharedState) {
    let rebuild_start = std::time::Instant::now();

    // Reset attempt counter
    {
        let mut sr = state.smart_rebuild.write().await;
        sr.current_attempt = 0;
        sr.last_build_error = None;
    }

    // Phase: StoppingRunner — stop unprotected runners for rebuild.
    // Protected runners are left running (they use their own binary copy).
    // Record which runners are running so we only restart those after rebuild.
    let (was_running, protected_skipped): (Vec<String>, Vec<String>) = {
        let runners = state.get_all_runners().await;
        let mut ids = Vec::new();
        let mut skipped = Vec::new();
        for managed in &runners {
            if managed.runner.read().await.running {
                if managed.is_protected().await {
                    skipped.push(managed.config.name.clone());
                } else {
                    ids.push(managed.config.id.clone());
                }
            }
        }
        (ids, skipped)
    };
    if !protected_skipped.is_empty() {
        state
            .logs
            .emit(
                LogSource::SmartRebuild,
                LogLevel::Info,
                format!(
                    "Skipping protected runner(s): {} — they will not be stopped for rebuild",
                    protected_skipped.join(", ")
                ),
            )
            .await;
    }
    if !was_running.is_empty() {
        {
            let mut sr = state.smart_rebuild.write().await;
            sr.phase = SmartRebuildPhase::StoppingRunner;
        }
        state
            .logs
            .emit(
                LogSource::SmartRebuild,
                LogLevel::Info,
                format!("Stopping {} runner(s) for rebuild", was_running.len()),
            )
            .await;

        // Stop only unprotected runners individually
        for runner_id in &was_running {
            if let Err(e) = manager::stop_runner_by_id(state, runner_id).await {
                error!(
                    "Smart rebuild: failed to stop runner '{}': {}",
                    runner_id, e
                );
                set_failed(
                    state,
                    format!("Failed to stop runner '{}': {}", runner_id, e),
                )
                .await;
                return;
            }
        }
    }

    // Build loop with AI fix attempts
    let mut attempt = 0u32;
    loop {
        attempt += 1;

        // Phase: Building
        {
            let mut sr = state.smart_rebuild.write().await;
            sr.phase = SmartRebuildPhase::Building;
            sr.current_attempt = attempt;
        }

        state
            .logs
            .emit(
                LogSource::SmartRebuild,
                LogLevel::Info,
                format!("Building (attempt {})", attempt),
            )
            .await;

        match build_monitor::run_cargo_build(state).await {
            Ok(()) => {
                info!("Smart rebuild: build succeeded on attempt {}", attempt);
                break; // Build succeeded
            }
            Err(e) => {
                let error_msg = e.to_string();
                warn!(
                    "Smart rebuild: build failed on attempt {}: {}",
                    attempt, error_msg
                );

                state.diagnostics.write().await.emit(
                    DiagnosticEventKind::SmartRebuildBuildFailed {
                        attempt,
                        error: error_msg.clone(),
                    },
                );

                // Get full stderr from build state
                let full_stderr = {
                    let build = state.build.read().await;
                    build
                        .last_build_stderr
                        .clone()
                        .unwrap_or_else(|| error_msg.clone())
                };

                {
                    let mut sr = state.smart_rebuild.write().await;
                    sr.last_build_error = Some(full_stderr.clone());
                }

                if attempt >= SMART_REBUILD_MAX_FIX_ATTEMPTS {
                    let reason = format!(
                        "Build failed after {} attempts, last error: {}",
                        attempt,
                        error_msg.chars().take(200).collect::<String>()
                    );
                    set_failed(state, reason.clone()).await;
                    state
                        .diagnostics
                        .write()
                        .await
                        .emit(DiagnosticEventKind::SmartRebuildFailed { reason });
                    return;
                }

                // Phase: FixingWithAi
                {
                    let mut sr = state.smart_rebuild.write().await;
                    sr.phase = SmartRebuildPhase::FixingWithAi { attempt };
                }

                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::SmartRebuildAiFix { attempt });

                state
                    .logs
                    .emit(
                        LogSource::SmartRebuild,
                        LogLevel::Info,
                        format!(
                            "Spawning AI fix session (attempt {}/{})",
                            attempt, SMART_REBUILD_MAX_FIX_ATTEMPTS
                        ),
                    )
                    .await;

                if let Err(e) = run_ai_fix(state, &full_stderr).await {
                    let reason = format!("AI fix failed on attempt {}: {}", attempt, e);
                    warn!("Smart rebuild: {}", reason);
                    state
                        .logs
                        .emit(LogSource::SmartRebuild, LogLevel::Warn, &reason)
                        .await;
                    // Continue to next attempt — the AI might have partially fixed things
                }
            }
        }
    }

    // Phase: Restarting — restart all runners that were running before rebuild
    {
        let mut sr = state.smart_rebuild.write().await;
        sr.phase = SmartRebuildPhase::Restarting;
    }

    state
        .logs
        .emit(
            LogSource::SmartRebuild,
            LogLevel::Info,
            format!(
                "Build succeeded, restarting {} previously-running runner(s)",
                was_running.len()
            ),
        )
        .await;

    // Restart only runners that were running before the rebuild.
    // Primary first, then non-primary with 2s delay.
    let runners = state.get_all_runners().await;
    let mut start_errors = Vec::new();

    // Start primary first (if it was running)
    for managed in &runners {
        if managed.config.is_primary && was_running.contains(&managed.config.id) {
            if let Err(e) = manager::start_runner_by_id(state, &managed.config.id).await {
                start_errors.push(format!("'{}': {}", managed.config.name, e));
            }
        }
    }

    // Then non-primary with 2s delay (only those that were running)
    for managed in &runners {
        if !managed.config.is_primary && was_running.contains(&managed.config.id) {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Err(e) = manager::start_runner_by_id(state, &managed.config.id).await {
                start_errors.push(format!("'{}': {}", managed.config.name, e));
            }
        }
    }

    if !start_errors.is_empty() {
        let reason = format!("Failed to restart runners: {}", start_errors.join("; "));
        error!("Smart rebuild: {}", reason);
        set_failed(state, reason.clone()).await;
        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::SmartRebuildFailed { reason });
    } else {
        // Wait for primary runner to become healthy
        let healthy = wait_for_runner_healthy(state, 60).await;
        if !healthy {
            warn!("Smart rebuild: runners started but primary not healthy after 60s");
            state
                .logs
                .emit(
                    LogSource::SmartRebuild,
                    LogLevel::Warn,
                    "Runners started but primary not healthy after 60s",
                )
                .await;
        }

        let duration_secs = rebuild_start.elapsed().as_secs_f64();

        // Update state
        let new_mtime = code_activity::get_last_source_modification(&state.config.project_dir);
        {
            let mut sr = state.smart_rebuild.write().await;
            sr.phase = SmartRebuildPhase::Idle;
            sr.last_successful_build_mtime = new_mtime;
            sr.total_rebuilds += 1;
            sr.last_rebuild_at = Some(Utc::now());
            sr.current_attempt = 0;
            sr.last_build_error = None;
        }

        info!(
            "Smart rebuild: completed successfully in {:.1}s ({} attempts)",
            duration_secs, attempt
        );
        state
            .logs
            .emit(
                LogSource::SmartRebuild,
                LogLevel::Info,
                format!(
                    "Smart rebuild completed in {:.1}s ({} attempts)",
                    duration_secs, attempt
                ),
            )
            .await;

        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::SmartRebuildCompleted {
                total_attempts: attempt,
                duration_secs,
            });

        // Spawn build error monitor for the new runner
        build_monitor::spawn_build_error_monitor(state.clone());
    }
}

/// Set the smart rebuild phase to Failed and record the timestamp for retry cooldown.
async fn set_failed(state: &SharedState, reason: String) {
    state
        .logs
        .emit(
            LogSource::SmartRebuild,
            LogLevel::Error,
            format!("Smart rebuild failed: {}", reason),
        )
        .await;
    let mut sr = state.smart_rebuild.write().await;
    sr.phase = SmartRebuildPhase::Failed {
        reason: reason.clone(),
    };
    sr.last_failed_at = Some(Utc::now());
}

/// Spawn Claude CLI to fix compilation errors.
async fn run_ai_fix(state: &SharedState, build_stderr: &str) -> Result<(), String> {
    let prompt = build_compilation_fix_prompt(build_stderr, &state.config.project_dir);

    // Write prompt to temp file
    let prompt_file = std::env::temp_dir().join("qontinui-smart-rebuild-prompt.md");
    tokio::fs::write(&prompt_file, &prompt)
        .await
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    // Spawn Claude CLI
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args([
        "--print",
        &prompt_file.display().to_string(),
        "--permission-mode",
        "bypassPermissions",
        "--output-format",
        "text",
        "--model",
        "claude-opus-4-6",
    ])
    .env_remove("CLAUDECODE")
    .current_dir(&state.config.project_dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn Claude CLI: {}", e))?;

    // Capture stdout/stderr for logging
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let state_clone = state.clone();
    let log_handle = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                state_clone
                    .logs
                    .emit(LogSource::SmartRebuild, LogLevel::Debug, &line)
                    .await;
            }
        }
    });

    let state_clone2 = state.clone();
    let stderr_handle = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                state_clone2
                    .logs
                    .emit(LogSource::SmartRebuild, LogLevel::Debug, &line)
                    .await;
            }
        }
    });

    // Wait with timeout
    let timeout = Duration::from_secs(SMART_REBUILD_FIX_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let _ = log_handle.await;
            let _ = stderr_handle.await;
            if status.success() {
                Ok(())
            } else {
                Err(format!("Claude CLI exited with status: {}", status))
            }
        }
        Ok(Err(e)) => {
            let _ = child.kill().await;
            Err(format!("Claude CLI process error: {}", e))
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(format!(
                "Claude CLI timed out after {}s",
                SMART_REBUILD_FIX_TIMEOUT_SECS
            ))
        }
    }
}

/// Maximum stderr bytes to include in the AI prompt (50 KB).
const MAX_STDERR_LEN: usize = 50_000;

/// Strip ANSI escape sequences from text.
fn strip_ansi_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1B' {
            // Consume the escape sequence
            match chars.peek() {
                Some('[') => {
                    // CSI sequence: ESC [ ... final_byte
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        if ch.is_ascii_alphabetic() || ch == '@' || ch == '~' {
                            chars.next();
                            break;
                        }
                        chars.next();
                    }
                }
                Some(']') => {
                    // OSC sequence: ESC ] ... (BEL or ESC \)
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        if ch == '\x07' {
                            chars.next();
                            break;
                        }
                        if ch == '\x1B' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                _ => {
                    // Single-char escape — skip the next char
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Build a focused prompt for fixing compilation errors.
///
/// Sanitizes inputs: canonicalizes the project path, strips ANSI escape codes,
/// and truncates stderr to [`MAX_STDERR_LEN`] bytes.
fn build_compilation_fix_prompt(stderr: &str, project_dir: &std::path::Path) -> String {
    // Canonicalize path to prevent traversal in the prompt
    let canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());

    // Strip ANSI codes and truncate
    let clean = strip_ansi_codes(stderr);
    let truncated = if clean.len() > MAX_STDERR_LEN {
        let cut = &clean[..MAX_STDERR_LEN];
        format!(
            "{cut}\n\n... (truncated — {total} bytes total)",
            total = clean.len()
        )
    } else {
        clean
    };

    format!(
        r#"# Fix Compilation Errors

The Rust project at `{}` has compilation errors. Fix them.

## Cargo Build Output

```
{}
```

## Instructions

1. Read the error messages carefully
2. Fix ONLY the compilation errors — do NOT refactor or change unrelated code
3. Make minimal changes to get the build passing
4. Do NOT add new features or change behavior
5. Do NOT explore the filesystem beyond reading files mentioned in the errors
"#,
        canonical.display(),
        truncated
    )
}

/// Wait for the runner to become healthy (API responding).
async fn wait_for_runner_healthy(state: &SharedState, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_secs(2);

    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;

        let cached = state.cached_health.read().await;
        if cached.runner_responding {
            return true;
        }
    }

    false
}

/// Cancel an in-progress smart rebuild, resetting to Idle.
pub async fn cancel_smart_rebuild(state: &SharedState) {
    {
        let mut sr = state.smart_rebuild.write().await;
        if sr.is_idle() {
            return;
        }
        info!("Smart rebuild: cancelled");
        sr.phase = SmartRebuildPhase::Idle;
        sr.current_attempt = 0;
        sr.last_build_error = None;
    } // write lock released before the await

    state
        .logs
        .emit(
            LogSource::SmartRebuild,
            LogLevel::Info,
            "Smart rebuild cancelled",
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smart_rebuild_state_new_defaults() {
        let state = SmartRebuildState::new(false);
        assert!(!state.enabled);
        assert!(state.is_idle());
        assert!(state.last_successful_build_mtime.is_none());
        assert_eq!(state.current_attempt, 0);
        assert!(state.last_build_error.is_none());
        assert_eq!(state.total_rebuilds, 0);
        assert!(state.last_rebuild_at.is_none());
        assert!(state.last_failed_at.is_none());
    }

    #[test]
    fn test_smart_rebuild_state_new_enabled() {
        let state = SmartRebuildState::new(true);
        assert!(state.enabled);
    }

    #[test]
    fn test_smart_rebuild_state_default() {
        let state = SmartRebuildState::default();
        assert!(!state.enabled);
        assert!(state.is_idle());
    }

    #[test]
    fn test_is_idle_for_each_phase() {
        let mut state = SmartRebuildState::new(false);
        assert!(state.is_idle());

        state.phase = SmartRebuildPhase::WaitingForQuiet;
        assert!(!state.is_idle());

        state.phase = SmartRebuildPhase::StoppingRunner;
        assert!(!state.is_idle());

        state.phase = SmartRebuildPhase::Building;
        assert!(!state.is_idle());

        state.phase = SmartRebuildPhase::FixingWithAi { attempt: 1 };
        assert!(!state.is_idle());

        state.phase = SmartRebuildPhase::Restarting;
        assert!(!state.is_idle());

        state.phase = SmartRebuildPhase::Failed {
            reason: "test".to_string(),
        };
        assert!(!state.is_idle());
    }

    #[test]
    fn test_build_compilation_fix_prompt() {
        let stderr = "error[E0308]: mismatched types";
        let dir = std::path::Path::new("/tmp/test/src-tauri");
        let prompt = build_compilation_fix_prompt(stderr, dir);
        assert!(prompt.contains("error[E0308]"));
        assert!(prompt.contains("/tmp/test/src-tauri"));
        assert!(prompt.contains("Fix Compilation Errors"));
        assert!(prompt.contains("do NOT refactor"));
    }

    #[test]
    fn test_phase_serialization() {
        let phase = SmartRebuildPhase::Idle;
        let json = serde_json::to_string(&phase).unwrap();
        assert_eq!(json, "\"idle\"");

        let phase = SmartRebuildPhase::FixingWithAi { attempt: 2 };
        let json = serde_json::to_string(&phase).unwrap();
        assert!(json.contains("fixing_with_ai"));
        assert!(json.contains("2"));
    }
}
