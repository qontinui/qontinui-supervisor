use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info};

/// Kill a process by its image name using taskkill.
pub async fn taskkill_by_name(name: &str, force: bool) -> anyhow::Result<bool> {
    let mut cmd = Command::new("taskkill");
    if force {
        cmd.arg("/F");
    }
    cmd.arg("/IM").arg(name);
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
        .args(["/C", &format!("netstat -ano | findstr :{} | findstr LISTENING", port)])
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
            .args(["/C", &format!(
                "wmic process where \"name='{}'\" get ProcessId /format:list",
                proc_name
            )])
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

/// Kill the runner by all known methods: process name, ports, etc.
pub async fn kill_runner_comprehensive() {
    // 1. Kill by process name
    let _ = taskkill_by_name("qontinui-runner.exe", true).await;

    // 2. Kill by known ports
    let _ = kill_by_port(9876).await;
    let _ = kill_by_port(1420).await;

    // Small delay for OS to release resources
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}
