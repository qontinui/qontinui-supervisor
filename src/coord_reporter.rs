//! Coord build-event reporter.
//!
//! Best-effort POST hooks called from `build_monitor.rs` to make this
//! supervisor's per-build outcomes visible to peer agents via the
//! `coord.build_events` Tinderbox surface (plan
//! `coord-tinderbox-build-status-2026-05-09.md`).
//!
//! **Never blocks the build.** Both functions short-circuit when the
//! supervisor has no `machine_id` (no `~/.qontinui/machine.json`) or when
//! `COORD_URL` is empty/unset. HTTP errors and non-2xx responses are
//! logged at warn and swallowed — the reporter must not propagate
//! failures back to `run_cargo_build_with_requester`.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::process::Command;
use tracing::warn;

use crate::state::SharedState;

/// Default coord base URL. Overridable via `COORD_URL`. Empty string disables
/// reporting entirely.
const DEFAULT_COORD_URL: &str = "http://localhost:9870";

/// Total HTTP timeout per coord request — must be tight enough that a slow
/// or unreachable coord cannot delay a build.
const COORD_REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// `git rev-parse HEAD` timeout. Failures fall back to `head_sha = None`.
const GIT_HEAD_SHA_TIMEOUT: Duration = Duration::from_secs(1);

/// Shared reqwest client tuned for this module's tight timeout. Reused
/// across calls — building one is cheap, but reusing pools the connection
/// to coord across the build's start/finish round-trip.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(COORD_REPORT_TIMEOUT)
        .build()
        .expect("coord_reporter: reqwest client build")
});

/// Cargo error-line file-path extractor, e.g. ` --> src/foo.rs:12:34`.
static ERROR_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"--> ([^:]+):\d+:\d+").unwrap());

/// Resolve the coord base URL. Returns `None` if `COORD_URL` is set to an
/// empty string (explicit disable). Treats unset as "use default."
fn resolve_coord_url() -> Option<String> {
    match std::env::var("COORD_URL") {
        Ok(s) if s.is_empty() => None,
        Ok(s) => Some(s),
        Err(_) => Some(DEFAULT_COORD_URL.to_string()),
    }
}

/// Calls coord's `POST /coord/builds/start`. Returns the generated `build_id`
/// on success, `None` when reporting is disabled (no machine_id, empty
/// `COORD_URL`) or any HTTP error (logs warn, swallows).
pub async fn report_build_start(
    state: &SharedState,
    repo: &str,
    slot_id: usize,
    requester_id: Option<String>,
    head_sha: Option<String>,
) -> Option<uuid::Uuid> {
    let machine_id = state.machine_id?;
    let coord_url = resolve_coord_url()?;

    let build_id = uuid::Uuid::new_v4();
    let body = serde_json::json!({
        "build_id":     build_id,
        "machine_id":   machine_id,
        "repo":         repo,
        "started_at":   chrono::Utc::now().to_rfc3339(),
        "slot":         slot_id,
        "requester_id": requester_id,
        "head_sha":     head_sha,
    });

    let url = format!("{}/coord/builds/start", coord_url.trim_end_matches('/'));
    match HTTP_CLIENT.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => Some(build_id),
        Ok(resp) => {
            warn!(
                "coord_reporter: POST /coord/builds/start returned {}",
                resp.status()
            );
            None
        }
        Err(e) => {
            warn!("coord_reporter: POST /coord/builds/start failed: {}", e);
            None
        }
    }
}

/// Calls coord's `POST /coord/builds/finish`. Best-effort; never propagates
/// errors back to the caller.
pub async fn report_build_finish(
    state: &SharedState,
    build_id: uuid::Uuid,
    success: bool,
    duration_ms: u64,
    error_summary: Option<String>,
    error_file: Option<String>,
) {
    if state.machine_id.is_none() {
        return;
    }
    let coord_url = match resolve_coord_url() {
        Some(u) => u,
        None => return,
    };

    let body = serde_json::json!({
        "build_id":      build_id,
        "ended_at":      chrono::Utc::now().to_rfc3339(),
        "result":        if success { "success" } else { "failure" },
        "duration_ms":   duration_ms,
        "error_summary": error_summary,
        "error_file":    error_file,
        "metadata":      {},
    });

    let url = format!("{}/coord/builds/finish", coord_url.trim_end_matches('/'));
    match HTTP_CLIENT.post(&url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            warn!(
                "coord_reporter: POST /coord/builds/finish returned {}",
                resp.status()
            );
        }
        Err(e) => {
            warn!("coord_reporter: POST /coord/builds/finish failed: {}", e);
        }
    }
}

/// Join the first `n` matched error lines into a single newline-separated
/// summary string. Returns `None` for empty input.
pub fn first_n_error_lines(error_lines: &[String], n: usize) -> Option<String> {
    if error_lines.is_empty() {
        return None;
    }
    let joined: Vec<&str> = error_lines.iter().take(n).map(|s| s.as_str()).collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join("\n"))
    }
}

/// Attempt to parse a file path out of the first matched error line, e.g.
/// `--> src/verification/wsm.rs:12:34`. Returns `None` if no match.
pub fn parse_error_file(error_lines: &[String]) -> Option<String> {
    let first = error_lines.first()?;
    let caps = ERROR_FILE_RE.captures(first)?;
    caps.get(1).map(|m| m.as_str().to_string())
}

/// Resolve the current HEAD sha for the project dir via `git rev-parse HEAD`.
/// Returns `None` for any failure (git missing, dir not a repo, command
/// timeout). The 1-second cap guarantees we never stall a build.
pub async fn resolve_head_sha(project_dir: &Path) -> Option<String> {
    let cmd_future = async {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project_dir)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };

    tokio::time::timeout(GIT_HEAD_SHA_TIMEOUT, cmd_future)
        .await
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_n_error_lines_empty() {
        let lines: Vec<String> = vec![];
        assert!(first_n_error_lines(&lines, 3).is_none());
    }

    #[test]
    fn first_n_error_lines_truncates() {
        let lines = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert_eq!(first_n_error_lines(&lines, 2).as_deref(), Some("a\nb"));
    }

    #[test]
    fn first_n_error_lines_below_cap() {
        let lines = vec!["only".to_string()];
        assert_eq!(first_n_error_lines(&lines, 5).as_deref(), Some("only"));
    }

    #[test]
    fn parse_error_file_extracts_path() {
        let lines = vec![
            "  --> src/verification/wsm.rs:12:34".to_string(),
            "ignored".to_string(),
        ];
        assert_eq!(
            parse_error_file(&lines).as_deref(),
            Some("src/verification/wsm.rs")
        );
    }

    #[test]
    fn parse_error_file_no_match() {
        let lines = vec!["error[E0432]: unresolved import".to_string()];
        assert!(parse_error_file(&lines).is_none());
    }

    #[test]
    fn parse_error_file_empty() {
        let lines: Vec<String> = vec![];
        assert!(parse_error_file(&lines).is_none());
    }
}
