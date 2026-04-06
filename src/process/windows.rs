use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Kill a process by its image name using taskkill.
pub async fn taskkill_by_name(name: &str, force: bool) -> anyhow::Result<bool> {
    let mut cmd = Command::new("taskkill");
    if force {
        cmd.arg("/F");
    }
    cmd.arg("/IM").arg(name);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd.output().await?;
    let success = output.status.success();

    if success {
        info!("taskkill: killed {}", name);
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!("taskkill {}: {}", name, stderr.trim());
    }

    Ok(success)
}

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

/// Kill processes listening on a port using tree kill (/T flag).
/// This kills the entire process tree rooted at the listening process,
/// which is essential for the Vite dev server where the node.exe grandchild
/// survives parent kills due to the cmd→npm→tauri→node process chain.
pub async fn kill_by_port_tree(port: u16) -> anyhow::Result<bool> {
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
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(pid_str) = parts.last() {
            if let Ok(pid) = pid_str.parse::<u32>() {
                if pid > 0 {
                    info!("Tree-killing PID {} on port {}", pid, port);
                    let kill_output = Command::new("taskkill")
                        .args(["/F", "/T", "/PID", &pid.to_string()])
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

/// Kill all msedgewebview2.exe processes so WebView2 cache files can be deleted.
pub async fn kill_webview2_processes() -> anyhow::Result<bool> {
    taskkill_by_name("msedgewebview2.exe", true).await
}

/// Clear WebView2 browser cache directories for the runner app.
/// Must be called after WebView2 processes are killed, otherwise files are locked.
/// This forces the WebView2 runtime to re-fetch all JavaScript modules on next
/// startup, which is necessary because WebView2 on Windows aggressively caches
/// ES modules even with Cache-Control: no-store headers.
pub async fn clear_webview2_cache() -> anyhow::Result<bool> {
    let local_app_data = match std::env::var("LOCALAPPDATA") {
        Ok(v) => std::path::PathBuf::from(v),
        Err(_) => {
            debug!("LOCALAPPDATA not set, skipping WebView2 cache clear");
            return Ok(false);
        }
    };

    let webview_default = local_app_data
        .join("com.qontinui.runner")
        .join("EBWebView")
        .join("Default");

    if !webview_default.exists() {
        debug!("WebView2 profile not found at {:?}", webview_default);
        return Ok(false);
    }

    let cache_dirs = ["Cache", "Code Cache"];
    let mut cleared_any = false;

    for dir_name in &cache_dirs {
        let cache_path = webview_default.join(dir_name);
        if cache_path.exists() {
            match tokio::fs::remove_dir_all(&cache_path).await {
                Ok(()) => {
                    info!("Cleared WebView2 {} at {:?}", dir_name, cache_path);
                    cleared_any = true;
                }
                Err(e) => {
                    warn!(
                        "Failed to clear WebView2 {} at {:?}: {}",
                        dir_name, cache_path, e
                    );
                }
            }
        }
    }

    Ok(cleared_any)
}
