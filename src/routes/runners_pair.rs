//! `POST /runners/pair-with-token` — supervisor-driven headless device pairing.
//!
//! Phase 2c of `plans/2026-05-22-mtc-iter3-remediation-web-dashboard.md`.
//! Shells out to the existing `qontinui_profile device pair --auth-token ...`
//! CLI (lives in the runner workspace, built alongside `qontinui-runner.exe`)
//! to drive the headless pair flow without opening a system browser.
//!
//! This unblocks `/manual-test-coord`'s autonomous test-runner pairing —
//! the CI machine mints a token via the web dashboard, posts it here, and
//! the resulting device-JWT + paired-user state lands in
//! `{data_local_dir}/com.qontinui.runner/` for the next spawn-test runner
//! to pick up.
//!
//! ## CLI invocation
//!
//! ```text
//! qontinui_profile device pair --auth-token <token> --tenant-id <uuid>
//! ```
//!
//! `web_base_url` (when provided) is forwarded to the child as the
//! `QONTINUI_WEB_BASE` env var. The CLI itself has no `--web-base` flag —
//! it reads from the active profile's `coord_url` (with the `QONTINUI_WEB_BASE`
//! override). See `qontinui-runner/src-tauri/src/pair.rs::pair_via_browser`
//! for the env-var contract.
//!
//! ## Response shape
//!
//! On success the CLI writes the device-JWT to `auth_tokens.enc` and the
//! `{user_id, tenant_id}` pair to `paired_user.json`, then prints a single
//! line: `device paired: user_id=<uuid> (device-token JWT saved to ...)`.
//! It does NOT print the full JSON pair-cli response, so we recover the
//! shape by reading back the on-disk artifacts:
//!
//! - `device_id` from `~/.qontinui/machine.json`
//! - `user_id` from `paired_user.json` (cross-checked against the stdout line)
//! - `tenant_id` from `paired_user.json`
//!
//! `expires_at` is NOT surfaced — the JWT exp claim lives in the
//! AES-encrypted `auth_tokens.enc` which we cannot decrypt from this layer.
//! Callers that need exp should fetch a fresh token from coord or decode
//! the JWT themselves after reading it from secure storage.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;

/// JSON body for `POST /runners/pair-with-token`.
#[derive(Debug, Clone, Deserialize)]
pub struct PairWithTokenRequest {
    /// Bearer token minted by the qontinui-web dashboard's `Auth Tokens` tab
    /// (long-lived runner token) or the future short-lived pair-code surface.
    /// Forwarded verbatim to `qontinui_profile device pair --auth-token`.
    pub token: String,

    /// Optional web base URL override. When set, forwarded to the CLI as the
    /// `QONTINUI_WEB_BASE` env var. When unset, the CLI derives the web base
    /// from the active profile's `coord_url`.
    ///
    /// Example: `https://demo.staging.qontinui.io`.
    #[serde(default)]
    pub web_base_url: Option<String>,

    /// Tenant scope for the new device. The runner CLI requires this; the
    /// supervisor refuses to invoke without it (no silent fallback to "no
    /// tenant" — that would silently produce a device row with NULL tenant).
    pub tenant_id: String,

    /// Optional override for the `qontinui_profile` binary path. Used by
    /// tests to point at a stub script. When unset, the supervisor resolves
    /// the binary via [`resolve_qontinui_profile_path`].
    #[serde(default)]
    pub qontinui_profile_path_override: Option<PathBuf>,
}

/// Default timeout for the spawned CLI. Pair-CLI is a single HTTPS round-trip
/// to coord — 30s is generous. After this, the child is killed and the caller
/// gets HTTP 504.
const PAIR_CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// Locate the `qontinui_profile` binary built alongside `qontinui-runner.exe`.
///
/// Preference order:
/// 1. `<runner_npm_dir>/target/debug/qontinui_profile[.exe]` — the canonical
///    location (matches `runner_exe_path` for the runner binary itself).
/// 2. Any `<runner_npm_dir>/target-pool/slot-{k}/debug/qontinui_profile[.exe]`
///    that exists. We pick the first one we find; pair-cli is a thin client
///    and slot drift here is not load-bearing the way it is for the runner.
///
/// Returns an error when no binary exists — the caller should retry after
/// `POST /runners/spawn-test {rebuild: true}` builds the runner workspace
/// (cargo build always builds the workspace's bins, so a runner rebuild
/// produces a fresh `qontinui_profile.exe`).
pub fn resolve_qontinui_profile_path(state: &SharedState) -> Result<PathBuf, String> {
    let exe_name = qontinui_profile_exe_name();
    let npm_dir = state.config.runner_npm_dir();

    let canonical = npm_dir.join("target").join("debug").join(&exe_name);
    if canonical.exists() {
        return Ok(canonical);
    }

    let pool_root = npm_dir.join("target-pool");
    if let Ok(entries) = std::fs::read_dir(&pool_root) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("debug").join(&exe_name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Err(format!(
        "qontinui_profile binary not found at {:?} or under {:?}. \
         Run a runner build first (e.g. POST /runners/spawn-test {{\"rebuild\": true}}).",
        canonical, pool_root
    ))
}

/// Platform-specific binary name. Mirrors `runner_exe_path`'s logic — the
/// runner workspace builds Windows .exe files; everything else gets the
/// extension-less name.
#[cfg(windows)]
fn qontinui_profile_exe_name() -> String {
    "qontinui_profile.exe".to_string()
}

#[cfg(not(windows))]
fn qontinui_profile_exe_name() -> String {
    "qontinui_profile".to_string()
}

/// Path to `~/.qontinui/machine.json` — the persistent device-identity file
/// the runner CLI writes (and `qontinui_profile device init` mints).
pub(crate) fn machine_json_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("machine.json"))
}

/// Path to the paired-user JSON the CLI writes on successful pair. Lives
/// under the Tauri app's local data dir alongside the encrypted JWT cache.
pub(crate) fn paired_user_json_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("com.qontinui.runner").join("paired_user.json"))
}

/// `POST /runners/pair-with-token` handler.
///
/// Returns 200 with `{device_id, user_id, tenant_id, expires_at}` on
/// success. `expires_at` is always `null` — see the module docstring.
///
/// Error responses:
/// - 400 `validation_error` for missing / malformed body fields.
/// - 500 `qontinui_profile_not_found` when the runner binary isn't built.
/// - 500 `pair_cli_failed` with the captured stderr + exit code.
/// - 504 `pair_cli_timeout` when the CLI hangs beyond [`PAIR_CLI_TIMEOUT`].
pub async fn pair_with_token(
    State(state): State<SharedState>,
    Json(body): Json<PairWithTokenRequest>,
) -> impl IntoResponse {
    // Body validation. We surface 400 with a structured error body so
    // callers can branch on the `error` discriminator instead of parsing
    // freeform messages.
    if body.token.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "validation_error",
                "message": "token must be a non-empty string",
            })),
        )
            .into_response();
    }
    if let Err(e) = uuid::Uuid::parse_str(body.tenant_id.trim()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "validation_error",
                "message": format!("tenant_id is not a valid UUID: {e}"),
            })),
        )
            .into_response();
    }

    // Resolve the CLI path. The override branch is for tests; production
    // callers always go through `resolve_qontinui_profile_path`.
    let cli_path = match body.qontinui_profile_path_override.clone() {
        Some(p) => {
            if !p.exists() {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "qontinui_profile_not_found",
                        "message": format!("override path does not exist: {:?}", p),
                    })),
                )
                    .into_response();
            }
            p
        }
        None => match resolve_qontinui_profile_path(&state) {
            Ok(p) => p,
            Err(message) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "qontinui_profile_not_found",
                        "message": message,
                    })),
                )
                    .into_response();
            }
        },
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "pair-with-token: invoking {:?} with tenant_id={} web_base_url={:?}",
                cli_path, body.tenant_id, body.web_base_url
            ),
        )
        .await;

    // Spawn the CLI. Captured stdout/stderr feed the response body on
    // failure; on success we read the on-disk artifacts.
    let mut cmd = tokio::process::Command::new(&cli_path);
    cmd.arg("device")
        .arg("pair")
        .arg("--auth-token")
        .arg(&body.token)
        .arg("--tenant-id")
        .arg(&body.tenant_id)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        // Don't let an existing CLAUDECODE env var leak into the child;
        // mirrors what manager.rs does on runner spawn.
        .env_remove("CLAUDECODE");
    if let Some(web_base) = &body.web_base_url {
        cmd.env("QONTINUI_WEB_BASE", web_base);
    }

    let output_fut = async {
        let child = cmd.spawn().map_err(|e| {
            format!(
                "failed to spawn {:?}: {e} (path exists? {})",
                cli_path,
                cli_path.exists()
            )
        })?;
        child
            .wait_with_output()
            .await
            .map_err(|e| format!("wait_with_output: {e}"))
    };

    let output = match tokio::time::timeout(PAIR_CLI_TIMEOUT, output_fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "pair_cli_spawn_failed",
                    "message": e,
                })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "error": "pair_cli_timeout",
                    "message": format!(
                        "qontinui_profile pair-cli exceeded {}s timeout",
                        PAIR_CLI_TIMEOUT.as_secs()
                    ),
                })),
            )
                .into_response();
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let exit_code = output.status.code();
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Warn,
                format!(
                    "pair-with-token: qontinui_profile exit={:?} stderr={}",
                    exit_code,
                    stderr.lines().last().unwrap_or("")
                ),
            )
            .await;
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "pair_cli_failed",
                "exit_code": exit_code,
                "stderr": stderr,
                "stdout": stdout,
            })),
        )
            .into_response();
    }

    // CLI succeeded — recover the canonical response shape from the on-disk
    // artifacts. Anything missing here is a CLI contract violation; we
    // surface it as 500 so the caller knows the pair "succeeded" but the
    // supervisor couldn't read back the expected state.
    let machine_json = match machine_json_path() {
        Some(p) => p,
        None => {
            return server_error(
                "home_dir_unresolved",
                "could not resolve home directory to read machine.json",
            );
        }
    };
    let paired_json = match paired_user_json_path() {
        Some(p) => p,
        None => {
            return server_error(
                "data_local_dir_unresolved",
                "could not resolve data_local_dir to read paired_user.json",
            );
        }
    };

    let device_id = match read_machine_device_id(&machine_json) {
        Ok(id) => id,
        Err(e) => {
            return server_error("machine_json_read_failed", &e);
        }
    };
    let (user_id, persisted_tenant_id) = match read_paired_user(&paired_json) {
        Ok(p) => p,
        Err(e) => {
            return server_error("paired_user_read_failed", &e);
        }
    };

    let tenant_id_out = persisted_tenant_id.unwrap_or_else(|| body.tenant_id.clone());

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "pair-with-token: success device_id={} user_id={} tenant_id={}",
                device_id, user_id, tenant_id_out
            ),
        )
        .await;

    (
        StatusCode::OK,
        Json(json!({
            "device_id": device_id,
            "user_id": user_id,
            "tenant_id": tenant_id_out,
            // CLI does not surface the JWT's exp claim — see module docs.
            "expires_at": serde_json::Value::Null,
        })),
    )
        .into_response()
}

fn server_error(error_kind: &str, message: &str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": error_kind,
            "message": message,
        })),
    )
        .into_response()
}

/// Read `device_id` from `~/.qontinui/machine.json`. Tolerates the legacy
/// `machine_id` key via the same serde alias the runner uses.
pub(crate) fn read_machine_device_id(path: &Path) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct MachineFile {
        #[serde(alias = "machine_id")]
        device_id: String,
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: MachineFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(parsed.device_id)
}

/// Read `(user_id, tenant_id)` from `paired_user.json`. `tenant_id` may be
/// absent on legacy files written before the 2026-05-22 schema bump — we
/// fall back to the request body's tenant_id in that case (see caller).
pub(crate) fn read_paired_user(path: &Path) -> Result<(String, Option<String>), String> {
    #[derive(serde::Deserialize)]
    struct PairedUserFile {
        user_id: String,
        #[serde(default)]
        tenant_id: Option<String>,
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: PairedUserFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok((parsed.user_id, parsed.tenant_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn reads_machine_device_id_canonical() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("machine.json");
        fs::write(
            &path,
            r#"{"device_id":"11111111-1111-4111-8111-111111111111","hostname":"x"}"#,
        )
        .unwrap();
        let id = read_machine_device_id(&path).expect("read device_id");
        assert_eq!(id, "11111111-1111-4111-8111-111111111111");
    }

    #[test]
    fn reads_machine_device_id_legacy_alias() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("machine.json");
        // Pre-rename file uses `machine_id` instead of `device_id`.
        fs::write(
            &path,
            r#"{"machine_id":"22222222-2222-4222-8222-222222222222","hostname":"x"}"#,
        )
        .unwrap();
        let id = read_machine_device_id(&path).expect("read device_id");
        assert_eq!(id, "22222222-2222-4222-8222-222222222222");
    }

    #[test]
    fn reads_paired_user_with_tenant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("paired_user.json");
        fs::write(
            &path,
            r#"{"user_id":"33333333-3333-4333-8333-333333333333","tenant_id":"44444444-4444-4444-8444-444444444444"}"#,
        )
        .unwrap();
        let (user_id, tenant_id) = read_paired_user(&path).expect("read");
        assert_eq!(user_id, "33333333-3333-4333-8333-333333333333");
        assert_eq!(
            tenant_id.as_deref(),
            Some("44444444-4444-4444-8444-444444444444")
        );
    }

    #[test]
    fn reads_paired_user_legacy_no_tenant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("paired_user.json");
        fs::write(
            &path,
            r#"{"user_id":"33333333-3333-4333-8333-333333333333"}"#,
        )
        .unwrap();
        let (user_id, tenant_id) = read_paired_user(&path).expect("read");
        assert_eq!(user_id, "33333333-3333-4333-8333-333333333333");
        assert!(tenant_id.is_none());
    }
}
