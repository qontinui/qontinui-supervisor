//! HTTP route handlers for CI runner lifecycle management (Phase 4b).
//!
//! - `POST /ci-runner/enable`  — install + configure a GitHub Actions runner
//! - `POST /ci-runner/disable` — stop + deregister the runner
//! - `POST /ci-runner/start`   — start the runner service
//! - `POST /ci-runner/stop`    — stop the runner service
//! - `POST /ci-runner/update`  — update the runner binary to latest
//! - `GET  /ci-runner/status`  — current CI runner state from the probe loop

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::ci_runner_lifecycle;
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Coord integration: resolve coord base URL from ~/.qontinui/profiles.json
// ---------------------------------------------------------------------------

/// Minimal subset of `~/.qontinui/profiles.json` — duplicated from
/// `fleet.rs` / `health_advertiser.rs` to keep this module self-contained.
#[derive(Debug, Clone, Deserialize)]
struct ProfilesFile {
    active: Option<String>,
    profiles: std::collections::HashMap<String, ProfileSubset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileSubset {
    coord_url: Option<String>,
}

fn profiles_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("profiles.json"))
}

/// Resolve the coord HTTP base from the active profile's `coord_url`.
fn coord_http_base() -> Option<String> {
    let bytes = std::fs::read(profiles_path()?).ok()?;
    let pf: ProfilesFile = serde_json::from_slice(&bytes).ok()?;
    let active = pf.active.as_deref().unwrap_or("dev");
    let coord_url = pf.profiles.get(active)?.coord_url.as_deref()?;

    let trimmed = coord_url.trim_end_matches("/ws");
    let with_http = trimmed
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            trimmed
                .strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .unwrap_or_else(|| trimmed.to_string());
    Some(with_http)
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct EnableRequest {
    /// GitHub organization (e.g. "qontinui"). If omitted, defaults to
    /// `"qontinui"`.
    pub org: Option<String>,
    /// Runner name. If omitted, defaults to the machine hostname.
    pub name: Option<String>,
    /// Extra labels to apply to the runner.
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Deserialize)]
pub struct DisableRequest {
    /// Registration token for deregistration. If omitted, the handler
    /// fetches one from coord.
    pub token: Option<String>,
}

#[derive(Serialize)]
pub struct CiRunnerActionResponse {
    pub status: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct CiRunnerStatusResponse {
    pub runner_status: String,
    pub labels: Vec<String>,
    pub service_names: Vec<String>,
    pub installed: bool,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn action_ok(msg: impl Into<String>) -> Json<CiRunnerActionResponse> {
    Json(CiRunnerActionResponse {
        status: "ok".to_string(),
        message: msg.into(),
    })
}

fn action_err(status: StatusCode, msg: impl Into<String>) -> Response {
    let body = CiRunnerActionResponse {
        status: "error".to_string(),
        message: msg.into(),
    };
    (status, Json(body)).into_response()
}

/// Fetch a CI runner registration token from coord.
async fn fetch_registration_token() -> Result<String, String> {
    let base = coord_http_base()
        .ok_or_else(|| "could not resolve coord URL from ~/.qontinui/profiles.json".to_string())?;

    let url = format!("{base}/coord/ci-runner/registration-token");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("coord request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<no body>".to_string());
        return Err(format!("coord returned {status} from {url}: {body}"));
    }

    // Expect JSON `{ "token": "..." }` or plain-text token.
    let body = resp
        .text()
        .await
        .map_err(|e| format!("failed to read coord response body: {e}"))?;

    // Try JSON first.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body) {
        if let Some(token) = parsed.get("token").and_then(|v| v.as_str()) {
            return Ok(token.to_string());
        }
    }

    // Fall back to treating the whole body as the token.
    let trimmed = body.trim().to_string();
    if trimmed.is_empty() {
        return Err("coord returned empty registration token".to_string());
    }
    Ok(trimmed)
}

fn resolve_runner_name() -> String {
    // Try the WSL hostname first; fall back to the Windows COMPUTERNAME env var.
    if let Ok(output) = std::process::Command::new("wsl")
        .args(["-e", "hostname"])
        .output()
    {
        if output.status.success() {
            let h = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !h.is_empty() {
                return h;
            }
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "qontinui-runner".to_string())
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// `POST /ci-runner/enable` — Install, configure, and start a CI runner.
///
/// 1. Fetches a registration token from coord.
/// 2. Downloads the runner binary (if not already present).
/// 3. Configures the runner with the token + labels.
/// 4. Installs and starts the systemd service.
pub async fn enable(
    State(_state): State<SharedState>,
    Json(body): Json<EnableRequest>,
) -> Response {
    info!("POST /ci-runner/enable: starting CI runner setup");

    // 1. Get registration token from coord.
    let token = match fetch_registration_token().await {
        Ok(t) => t,
        Err(e) => {
            warn!("POST /ci-runner/enable: failed to get registration token: {e}");
            return action_err(
                StatusCode::BAD_GATEWAY,
                format!("failed to get registration token from coord: {e}"),
            );
        }
    };

    let org = body.org.unwrap_or_else(|| "qontinui".to_string());
    let name = body.name.unwrap_or_else(resolve_runner_name);
    let mut labels = body.labels;
    if labels.is_empty() {
        labels.push("self-hosted".to_string());
    }

    // 2. Install + configure + start.
    match ci_runner_lifecycle::install_runner(&token, &org, &labels, &name).await {
        Ok(()) => {
            info!("POST /ci-runner/enable: runner installed successfully");
            action_ok("CI runner installed and started").into_response()
        }
        Err(e) => {
            warn!("POST /ci-runner/enable: install failed: {e}");
            action_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("CI runner install failed: {e}"),
            )
        }
    }
}

/// `POST /ci-runner/disable` — Stop and deregister the CI runner.
pub async fn disable(
    State(_state): State<SharedState>,
    Json(body): Json<DisableRequest>,
) -> Response {
    info!("POST /ci-runner/disable: removing CI runner");

    let token = match body.token {
        Some(t) => t,
        None => match fetch_registration_token().await {
            Ok(t) => t,
            Err(e) => {
                warn!("POST /ci-runner/disable: failed to get removal token: {e}");
                return action_err(
                    StatusCode::BAD_GATEWAY,
                    format!("failed to get removal token from coord: {e}"),
                );
            }
        },
    };

    match ci_runner_lifecycle::remove_runner(&token).await {
        Ok(()) => {
            info!("POST /ci-runner/disable: runner removed");
            action_ok("CI runner stopped and deregistered").into_response()
        }
        Err(e) => {
            warn!("POST /ci-runner/disable: remove failed: {e}");
            action_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("CI runner removal failed: {e}"),
            )
        }
    }
}

/// `POST /ci-runner/start` — Start the runner systemd service.
pub async fn start(State(_state): State<SharedState>) -> Response {
    match ci_runner_lifecycle::start_runner().await {
        Ok(()) => action_ok("CI runner service started").into_response(),
        Err(e) => action_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to start CI runner service: {e}"),
        ),
    }
}

/// `POST /ci-runner/stop` — Stop the runner systemd service.
pub async fn stop(State(_state): State<SharedState>) -> Response {
    match ci_runner_lifecycle::stop_runner().await {
        Ok(()) => action_ok("CI runner service stopped").into_response(),
        Err(e) => action_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to stop CI runner service: {e}"),
        ),
    }
}

/// `POST /ci-runner/update` — Update the runner binary to the latest version.
pub async fn update(State(_state): State<SharedState>) -> Response {
    match ci_runner_lifecycle::update_runner().await {
        Ok(()) => action_ok("CI runner updated to latest version").into_response(),
        Err(e) => action_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to update CI runner: {e}"),
        ),
    }
}

/// `GET /ci-runner/status` — Return the current CI runner state.
pub async fn status(State(state): State<SharedState>) -> Json<CiRunnerStatusResponse> {
    let ci_state = state.ci_runner_state.read().await;
    let installed = tokio::task::spawn_blocking(ci_runner_lifecycle::is_runner_installed)
        .await
        .unwrap_or(false);
    Json(CiRunnerStatusResponse {
        runner_status: ci_state.status.as_str().to_string(),
        labels: ci_state.labels.clone(),
        service_names: ci_state.service_names.clone(),
        installed,
    })
}
