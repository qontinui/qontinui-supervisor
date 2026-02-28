use chrono::DateTime;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::ai_debug;
use crate::config::CODE_CHECK_RETRY_INTERVAL_SECS;
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;

/// File extensions to consider as source code.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "json", "toml", "css", "html", "py", "md",
];

/// Directory names to skip during scanning.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "dist", ".git"];

/// Scan source directories under the runner project for the most recently modified file.
pub fn get_last_source_modification(project_dir: &Path) -> Option<SystemTime> {
    // project_dir is src-tauri, parent is qontinui-runner
    let runner_root = project_dir.parent().unwrap_or(project_dir);

    let scan_dirs = [
        project_dir.join("src"), // Rust source (src-tauri/src/)
        runner_root.join("src"), // TypeScript/React frontend (src/)
    ];

    let mut latest: Option<SystemTime> = None;

    for dir in &scan_dirs {
        if dir.exists() {
            scan_directory_recursive(dir, &mut latest);
        }
    }

    latest
}

/// Recursively scan a directory for source files, tracking the most recent modification time.
fn scan_directory_recursive(dir: &Path, latest: &mut Option<SystemTime>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if SKIP_DIRS.contains(&dir_name) {
                continue;
            }
            scan_directory_recursive(&path, latest);
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !SOURCE_EXTENSIONS.contains(&ext) {
                continue;
            }
            if let Ok(metadata) = path.metadata() {
                if let Ok(modified) = metadata.modified() {
                    match latest {
                        Some(current) if modified > *current => *latest = Some(modified),
                        None => *latest = Some(modified),
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Check if source code was modified within the quiet period.
pub fn is_code_being_edited(project_dir: &Path, quiet_period_secs: i64) -> bool {
    if let Some(last_mod) = get_last_source_modification(project_dir) {
        let elapsed = SystemTime::now()
            .duration_since(last_mod)
            .unwrap_or(Duration::from_secs(u64::MAX));
        elapsed.as_secs() < quiet_period_secs as u64
    } else {
        false
    }
}

/// Check if an external Claude Code session is running.
/// On Windows, uses tasklist to look for node.exe processes with "claude" in the window title.
#[cfg(windows)]
pub async fn is_external_claude_session() -> bool {
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/C", "tasklist /FI \"IMAGENAME eq node.exe\" /V /FO CSV"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let output = cmd.output().await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
            // Look for "claude" in the window title / command info columns
            stdout.lines().any(|line| {
                // Skip the CSV header line
                if line.starts_with("\"image name\"") || line.starts_with("\"image") {
                    return false;
                }
                line.contains("claude")
            })
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
pub async fn is_external_claude_session() -> bool {
    // On non-Windows, check for claude/node processes via ps
    let output = tokio::process::Command::new("ps")
        .args(["aux"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_lowercase();
            stdout.lines().any(|line| line.contains("claude"))
        }
        Err(_) => false,
    }
}

/// Background task that periodically checks code activity and external Claude sessions.
/// When a pending debug request exists and the quiet period has elapsed, triggers AI debug.
pub fn spawn_code_activity_monitor(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("Code activity monitor started");
        let interval = Duration::from_secs(CODE_CHECK_RETRY_INTERVAL_SECS);

        loop {
            sleep(interval).await;

            let project_dir = state.config.project_dir.clone();
            let quiet_period = crate::config::CODE_QUIET_PERIOD_SECS;

            // Check code being edited
            let editing = is_code_being_edited(&project_dir, quiet_period);
            let external_claude = is_external_claude_session().await;

            // Update code activity state
            let (pending, reason) = {
                let mut ca = state.code_activity.write().await;
                ca.code_being_edited = editing;
                ca.external_claude_session = external_claude;

                // Update last code change timestamp from filesystem
                if let Some(last_mod) = get_last_source_modification(&project_dir) {
                    if let Ok(duration) = last_mod.duration_since(SystemTime::UNIX_EPOCH) {
                        let secs = duration.as_secs() as i64;
                        let nanos = duration.subsec_nanos();
                        if let Some(dt) = DateTime::from_timestamp(secs, nanos) {
                            ca.last_code_change_at = Some(dt);
                        }
                    }
                }

                (ca.pending_debug, ca.pending_debug_reason.clone())
            };

            state.notify_health_change();

            debug!(
                "Code activity: editing={}, external_claude={}, pending_debug={}",
                editing, external_claude, pending
            );

            // If there's a pending debug and conditions are met, trigger it
            if pending && !editing && !external_claude {
                info!("Quiet period elapsed and no external Claude session — triggering pending debug");
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        "Code quiet period elapsed, triggering deferred AI debug session",
                    )
                    .await;

                // Clear pending state
                {
                    let mut ca = state.code_activity.write().await;
                    ca.pending_debug = false;
                    ca.pending_debug_reason = None;
                }

                let reason_str = reason.unwrap_or_else(|| "Deferred debug".to_string());
                if let Err(e) = ai_debug::spawn_ai_debug(&state, Some(&reason_str)).await {
                    warn!("Failed to trigger deferred AI debug: {}", e);
                }
            }
        }
    })
}
