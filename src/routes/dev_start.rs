use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};

use crate::config::SERVICE_PORTS;
use crate::process::port::{check_http_health, is_port_in_use};
use crate::state::SharedState;

#[derive(Serialize)]
pub struct DevStartResponse {
    pub status: String,
    pub flag: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Serialize)]
pub struct DevStartStatusResponse {
    pub services: Vec<ServiceStatus>,
}

#[derive(Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub port: u16,
    pub available: bool,
}

/// Locate the dev-start.ps1 script relative to the project directory.
fn find_dev_start_script(project_dir: &std::path::Path) -> Option<PathBuf> {
    // project_dir = qontinui-runner/src-tauri
    // Try: project_dir/../../dev-start.ps1 (qontinui_parent/dev-start.ps1)
    if let Some(parent) = project_dir.parent().and_then(|p| p.parent()) {
        let path = parent.join("dev-start.ps1");
        if path.exists() {
            return Some(path);
        }
    }

    // Try: project_dir/../dev-start.ps1
    if let Some(parent) = project_dir.parent() {
        let path = parent.join("dev-start.ps1");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Run dev-start.ps1 with the given flag and timeout.
async fn run_dev_start(
    project_dir: &std::path::Path,
    flag: &str,
    timeout_secs: u64,
) -> DevStartResponse {
    let script = match find_dev_start_script(project_dir) {
        Some(s) => s,
        None => {
            return DevStartResponse {
                status: "error".to_string(),
                flag: flag.to_string(),
                stdout: String::new(),
                stderr: "dev-start.ps1 not found".to_string(),
                exit_code: None,
            };
        }
    };

    info!(
        "Running dev-start.ps1 -{} (timeout: {}s)",
        flag, timeout_secs
    );

    let result = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        tokio::process::Command::new("powershell.exe")
            .args([
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &script.display().to_string(),
                &format!("-{}", flag),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code();
            let status = if output.status.success() {
                "success"
            } else {
                "error"
            };

            DevStartResponse {
                status: status.to_string(),
                flag: flag.to_string(),
                stdout,
                stderr,
                exit_code,
            }
        }
        Ok(Err(e)) => {
            warn!("dev-start.ps1 -{} spawn error: {}", flag, e);
            DevStartResponse {
                status: "error".to_string(),
                flag: flag.to_string(),
                stdout: String::new(),
                stderr: format!("Failed to spawn: {}", e),
                exit_code: None,
            }
        }
        Err(_) => {
            warn!("dev-start.ps1 -{} timed out after {}s", flag, timeout_secs);
            DevStartResponse {
                status: "timeout".to_string(),
                flag: flag.to_string(),
                stdout: String::new(),
                stderr: format!("Timed out after {}s", timeout_secs),
                exit_code: None,
            }
        }
    }
}

// --- Route handlers ---

pub async fn backend(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Backend", 60).await)
}

pub async fn backend_stop(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "StopApps", 30).await)
}

pub async fn frontend(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Frontend", 180).await)
}

pub async fn frontend_stop(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "StopApps", 30).await)
}

pub async fn docker(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Docker", 60).await)
}

pub async fn docker_stop(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Stop", 30).await)
}

pub async fn all(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "All", 300).await)
}

pub async fn stop(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Stop", 30).await)
}

pub async fn clean(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Clean", 30).await)
}

pub async fn fresh(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Fresh", 300).await)
}

pub async fn migrate(State(state): State<SharedState>) -> Json<DevStartResponse> {
    Json(run_dev_start(&state.config.project_dir, "Migrate", 120).await)
}

pub async fn status(_state: State<SharedState>) -> Json<DevStartStatusResponse> {
    let mut services = Vec::new();

    for (name, port) in SERVICE_PORTS {
        let available = if *name == "backend" {
            // Use HTTP health check for the backend to detect startup failures
            check_http_health(*port, "/health").await
        } else {
            is_port_in_use(*port)
        };

        services.push(ServiceStatus {
            name: name.to_string(),
            port: *port,
            available,
        });
    }

    Json(DevStartStatusResponse { services })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_find_dev_start_script_returns_none_for_nonexistent_path() {
        let result = find_dev_start_script(std::path::Path::new("/nonexistent/path/src-tauri"));
        assert!(result.is_none());
    }

    #[test]
    fn test_find_dev_start_script_finds_in_grandparent() {
        // Create temp dir structure: tmp/parent/child/src-tauri with dev-start.ps1 in tmp/
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let child_dir = tmp.path().join("parent").join("child").join("src-tauri");
        fs::create_dir_all(&child_dir).expect("failed to create child dirs");
        // Place dev-start.ps1 at grandparent (tmp/parent/)
        // The function looks at project_dir.parent().parent() first
        // project_dir = tmp/parent/child/src-tauri
        // parent = tmp/parent/child
        // grandparent = tmp/parent
        let script_path = tmp.path().join("parent").join("dev-start.ps1");
        fs::write(&script_path, "# test script").expect("failed to write script");

        let result = find_dev_start_script(&child_dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), script_path);
    }

    #[test]
    fn test_find_dev_start_script_finds_in_parent() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let child_dir = tmp.path().join("src-tauri");
        fs::create_dir_all(&child_dir).expect("failed to create child dir");
        // Place dev-start.ps1 in the parent (tmp/)
        let script_path = tmp.path().join("dev-start.ps1");
        fs::write(&script_path, "# test script").expect("failed to write script");

        let result = find_dev_start_script(&child_dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), script_path);
    }

    #[test]
    fn test_find_dev_start_script_prefers_grandparent_over_parent() {
        // Both grandparent and parent have dev-start.ps1; grandparent should win
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let child_dir = tmp.path().join("runner").join("src-tauri");
        fs::create_dir_all(&child_dir).expect("failed to create child dirs");

        let grandparent_script = tmp.path().join("dev-start.ps1");
        fs::write(&grandparent_script, "# grandparent").expect("failed to write");
        let parent_script = tmp.path().join("runner").join("dev-start.ps1");
        fs::write(&parent_script, "# parent").expect("failed to write");

        let result = find_dev_start_script(&child_dir);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), grandparent_script);
    }

    #[test]
    fn test_dev_start_response_serializes() {
        let response = DevStartResponse {
            status: "success".to_string(),
            flag: "Backend".to_string(),
            stdout: "Started".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"flag\":\"Backend\""));
        assert!(json.contains("\"exit_code\":0"));
    }

    #[test]
    fn test_dev_start_response_serializes_null_exit_code() {
        let response = DevStartResponse {
            status: "timeout".to_string(),
            flag: "All".to_string(),
            stdout: String::new(),
            stderr: "Timed out after 300s".to_string(),
            exit_code: None,
        };
        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("\"exit_code\":null"));
    }

    #[test]
    fn test_service_status_serializes() {
        let status = ServiceStatus {
            name: "postgresql".to_string(),
            port: 5432,
            available: true,
        };
        let json = serde_json::to_string(&status).expect("should serialize");
        assert!(json.contains("\"name\":\"postgresql\""));
        assert!(json.contains("\"port\":5432"));
        assert!(json.contains("\"available\":true"));
    }

    #[test]
    fn test_dev_start_status_response_serializes() {
        let response = DevStartStatusResponse {
            services: vec![
                ServiceStatus {
                    name: "redis".to_string(),
                    port: 6379,
                    available: false,
                },
                ServiceStatus {
                    name: "backend".to_string(),
                    port: 8000,
                    available: true,
                },
            ],
        };
        let json = serde_json::to_string(&response).expect("should serialize");
        assert!(json.contains("\"redis\""));
        assert!(json.contains("\"backend\""));
    }
}
