use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Kill processes listening on a specific port using netstat + taskkill.
pub async fn kill_by_port(port: u16) -> anyhow::Result<bool> {
    // Find PIDs using the port via netstat
    let output = Command::new("cmd")
        .args([
            "/C",
            &format!("netstat -ano | findstr :{} | findstr LISTENING", port),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut killed_any = false;

    for line in stdout.lines() {
        // netstat output format: TCP    0.0.0.0:9876    0.0.0.0:0    LISTENING    12345
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(pid_str) = parts.last() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid > 0 {
                    info!("Killing PID {} on port {}", pid, port);
                    let kill_output = Command::new("taskkill")
                        .args(["/F", "/PID", &pid.to_string()])
                        .creation_flags(CREATE_NO_WINDOW)
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .output()
                        .await?;

                    if kill_output.status.success() {
                        killed_any = true;
                    }
                }
            }
        }
    }

    Ok(killed_any)
}

/// Clean up orphaned cargo/rustc processes that may be left from a previous build.
pub async fn cleanup_orphaned_build_processes() {
    // Kill orphaned cargo.exe processes (but not the one running *us*)
    let our_pid = std::process::id();

    for proc_name in &["cargo.exe", "rustc.exe"] {
        // Use WMIC to find processes - more reliable than tasklist parsing
        let output = Command::new("cmd")
            .args([
                "/C",
                &format!(
                    "wmic process where \"name='{}'\" get ProcessId /format:list",
                    proc_name
                ),
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;

        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Some(pid_str) = line.strip_prefix("ProcessId=") {
                    if let Ok(pid) = pid_str.trim().parse::<u32>() {
                        if pid != our_pid && pid > 0 {
                            debug!("Cleaning up orphaned {} (PID {})", proc_name, pid);
                            let _ = Command::new("taskkill")
                                .args(["/F", "/PID", &pid.to_string()])
                                .creation_flags(CREATE_NO_WINDOW)
                                .stdout(Stdio::piped())
                                .stderr(Stdio::piped())
                                .output()
                                .await;
                        }
                    }
                }
            }
        }
    }
}

/// Return the PID of the first process found LISTENING on `port`, or `None`
/// if the port is idle. Uses the same `netstat -ano | findstr LISTENING`
/// parse as `kill_by_port` but does not kill anything. Used by the health
/// cache to recover a runner's PID after a supervisor restart re-discovers
/// it (the supervisor lost the PID when it shut down; the process is still
/// the same one bound to the port).
pub async fn find_pid_on_port(port: u16) -> Option<u32> {
    let output = Command::new("cmd")
        .args([
            "/C",
            &format!("netstat -ano | findstr :{} | findstr LISTENING", port),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(pid_str) = parts.last() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid > 0 {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Kill a process by its PID using taskkill.
pub async fn kill_by_pid(pid: u32) -> anyhow::Result<bool> {
    let output = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let success = output.status.success();
    if success {
        info!("taskkill: killed PID {}", pid);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("taskkill PID {}: {}", pid, stderr.trim());
    }

    Ok(success)
}

/// Return the WebView2 user data folder for a given runner id.
///
/// - The primary runner uses the default path `com.qontinui.runner\EBWebView`
///   (unchanged) so existing auth, terminal layouts, and other local state
///   are preserved.
/// - All other runners (named secondaries, temp `test-*` runners) get a
///   per-runner subdirectory `com.qontinui.runner\EBWebView-{id}` so their
///   localStorage, IndexedDB, cookies, and WebView2 caches are isolated from
///   the primary and from each other. This is what the runner process sees
///   via the `WEBVIEW2_USER_DATA_FOLDER` env var.
///
/// Returns `None` if `LOCALAPPDATA` is not set (non-Windows or broken env).
pub fn webview2_user_data_folder(runner_id: &str, is_primary: bool) -> Option<std::path::PathBuf> {
    let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
    let base = std::path::PathBuf::from(local_app_data).join("com.qontinui.runner");
    if is_primary {
        Some(base.join("EBWebView"))
    } else {
        // Sanitize id for filesystem use (alphanumerics, dash, underscore only).
        let safe_id: String = runner_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Some(base.join(format!("EBWebView-{}", safe_id)))
    }
}

/// Remove a non-primary runner's WebView2 data folder. Used when a temp
/// runner is deleted so its state doesn't accumulate on disk.
///
/// Refuses to touch the primary runner's folder as a safety check — always
/// pass `is_primary: false`. Returns `Ok(false)` if the folder didn't exist
/// (nothing to clean up).
pub async fn remove_webview2_user_data_folder(
    runner_id: &str,
    is_primary: bool,
) -> anyhow::Result<bool> {
    if is_primary {
        anyhow::bail!("refusing to remove the primary runner's WebView2 data folder");
    }
    let Some(folder) = webview2_user_data_folder(runner_id, false) else {
        return Ok(false);
    };
    if !folder.exists() {
        return Ok(false);
    }
    match tokio::fs::remove_dir_all(&folder).await {
        Ok(()) => {
            info!(
                "Removed WebView2 data folder for runner '{}' at {:?}",
                runner_id, folder
            );
            Ok(true)
        }
        Err(e) => {
            warn!(
                "Failed to remove WebView2 data folder for runner '{}' at {:?}: {}",
                runner_id, folder, e
            );
            Err(e.into())
        }
    }
}

/// Remove per-instance app-data directories for a non-primary runner.
///
/// The runner's `crate::instance::scope_path()` helper writes per-runner
/// dev logs, macros, prompts, playwright tests, contexts, and Restate journals
/// under an `instance-<sanitized_name>` subdirectory of several base locations.
/// When a temp runner is deleted we clean these up so disk usage doesn't grow
/// unbounded.
///
/// `runner_name` must be the `managed.config.name` value that the supervisor
/// passed to the runner as `QONTINUI_INSTANCE_NAME` — not the runner's id.
///
/// Refuses to touch anything for primary runners as a safety check.
pub async fn remove_runner_app_data_dirs(
    runner_name: &str,
    is_primary: bool,
) -> anyhow::Result<u32> {
    if is_primary {
        anyhow::bail!("refusing to remove the primary runner's app data dirs");
    }

    // Mirror runner's sanitize() in src-tauri/src/instance.rs.
    let safe_name: String = runner_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let subdir = format!("instance-{}", safe_name);

    let local_app_data = std::env::var("LOCALAPPDATA")
        .ok()
        .map(std::path::PathBuf::from);
    let roaming_app_data = std::env::var("APPDATA").ok().map(std::path::PathBuf::from);

    // Each runner path helper applies scope_path at a slightly different
    // nesting level. Enumerate every known landing spot so all per-instance
    // state gets cleaned up.
    let candidates: Vec<std::path::PathBuf> = [
        // dev-logs: base is `.../qontinui-runner/dev-logs`, so the instance
        // dir lands inside dev-logs (see src/paths.rs::get_dev_logs_dir).
        local_app_data
            .as_ref()
            .map(|p| p.join("qontinui-runner").join("dev-logs").join(&subdir)),
        // macros, ai_workflows: base is `.../qontinui-runner`.
        local_app_data
            .as_ref()
            .map(|p| p.join("qontinui-runner").join(&subdir)),
        // prompts, playwright, contexts: base is `.../com.qontinui.runner`.
        local_app_data
            .as_ref()
            .map(|p| p.join("com.qontinui.runner").join(&subdir)),
        roaming_app_data
            .as_ref()
            .map(|p| p.join("com.qontinui.runner").join(&subdir)),
        // Restate journal: base is dirs::data_dir()/qontinui-runner/restate/data.
        roaming_app_data.as_ref().map(|p| {
            p.join("qontinui-runner")
                .join("restate")
                .join("data")
                .join(&subdir)
        }),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut removed = 0u32;
    for path in candidates {
        if !path.exists() {
            continue;
        }
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                info!(
                    "Removed per-instance app data for runner '{}' at {:?}",
                    runner_name, path
                );
                removed += 1;
            }
            Err(e) => {
                warn!(
                    "Failed to remove per-instance app data at {:?}: {}",
                    path, e
                );
            }
        }
    }
    Ok(removed)
}
