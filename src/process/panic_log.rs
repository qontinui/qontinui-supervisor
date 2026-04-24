//! Panic-log parser for the runner's `runner-panic.log`.
//!
//! The runner writes this file from its startup panic hook (see
//! `qontinui-runner/src-tauri/src/startup_panic.rs`). When a managed runner
//! exits non-zero, the supervisor checks whether a panic log exists within
//! 60 seconds of the exit; if so, it parses it into [`RecentPanic`] and
//! surfaces the data to:
//!
//! * the supervisor log buffer as a `[runner-panic]` ERROR entry,
//! * `GET /runners` and `GET /runners/{id}/logs` JSON payloads,
//! * the 500/502 error body from `POST /runners/spawn-test` when the
//!   spawned runner dies during startup,
//! * the dashboard, which renders a red "Panic" pill badge on the row.
//!
//! # File format
//!
//! ```text
//! [2026-04-22T12:34:56.789+00:00] PANIC runner-id=test-abc pid=1234 version=1.0.0
//! location: src/foo.rs:10:5
//! thread: main
//! payload:
//! <payload lines>
//!
//! backtrace:
//! <backtrace lines>
//! ```
//!
//! Fields are tolerant: any missing line yields `None` on the corresponding
//! field rather than a parse failure. The only required token is the
//! `PANIC` header on the first line.
//!
//! # Size cap
//!
//! The file is read with a 16 KiB cap so a log-poisoning panic payload
//! can't balloon the supervisor's JSON responses.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Maximum bytes read from the panic log. Long Rust backtraces can easily
/// exceed 100 KB; we truncate to keep JSON responses bounded.
pub const PANIC_LOG_READ_CAP: usize = 16 * 1024;

/// First N lines of the backtrace the supervisor surfaces in JSON/log output.
/// Per spec (Phase 2b): 15 frames.
pub const BACKTRACE_PREVIEW_FRAMES: usize = 15;

/// Max seconds between the log file's mtime and the runner's exit before
/// we consider the file "fresh" for this exit. Protects against a stale
/// `runner-panic.log` from a previous boot being attributed to a clean exit.
pub const PANIC_LOG_FRESHNESS_SECS: i64 = 60;

/// Parsed form of the runner's `runner-panic.log`.
///
/// Serialised camelCase-friendly; we keep snake_case field names for Rust
/// convention but the wire shape matches `recent_panic` on the frontend.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecentPanic {
    /// RFC3339 timestamp parsed from the header line, or the file's mtime
    /// if the header didn't have one.
    pub timestamp: DateTime<Utc>,
    /// Panic payload (the `payload:` section of the file), capped at
    /// [`PANIC_LOG_READ_CAP`]-ish — we don't trim mid-parse, the read cap
    /// already bounded it.
    pub payload: String,
    /// `file.rs:line:col` from the `location:` line. `None` if the panic
    /// was at an unknown location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// `thread:` line value. `None` for unnamed threads where the hook
    /// wrote `<unnamed>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// First [`BACKTRACE_PREVIEW_FRAMES`] frames of the backtrace, joined
    /// with `\n`. Omits the header line so the preview is just frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backtrace_preview: Option<String>,
    /// Runner id from the header line (`runner-id=...`). Useful when the
    /// log file is passed around outside its owning runner's log dir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<String>,
    /// Runner PID from the header (`pid=...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Runner version from the header (`version=...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Absolute path to the source file on disk.
    pub file_path: String,
}

/// Resolve the expected panic-log path for a runner. The supervisor sets
/// `QONTINUI_RUNNER_LOG_DIR` env per runner; if that was set at spawn time,
/// pass it as `log_dir_hint` here. Otherwise we fall back to the same
/// default the runner uses (`<LOCALAPPDATA>/qontinui-runner/dev-logs`).
///
/// Note: this does NOT check the file's existence — callers use
/// `parse_panic_file` for that.
pub fn resolve_panic_log_path(log_dir_hint: Option<&Path>) -> PathBuf {
    if let Some(dir) = log_dir_hint {
        return dir.join("runner-panic.log");
    }
    default_panic_log_path()
}

/// Default path when the supervisor has no hint (e.g. it wasn't given
/// `--log-dir` and didn't propagate `QONTINUI_RUNNER_LOG_DIR` to the
/// spawned runner).
fn default_panic_log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qontinui-runner")
        .join("dev-logs")
        .join("runner-panic.log")
}

/// Parse `path` into a [`RecentPanic`]. Returns `None` if the file is
/// missing, unreadable, empty, or doesn't contain the `PANIC` header.
///
/// Reads at most [`PANIC_LOG_READ_CAP`] bytes; longer files are
/// truncated mid-line (acceptable — the preview fields and payload are
/// already capped and we don't need byte-exact output).
pub fn parse_panic_file(path: &Path) -> Option<RecentPanic> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; PANIC_LOG_READ_CAP];
    let n = f.read(&mut buf).ok()?;
    if n == 0 {
        return None;
    }
    buf.truncate(n);
    let content = String::from_utf8_lossy(&buf).into_owned();
    let mtime = f
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|d| DateTime::<Utc>::from_timestamp(d.as_secs() as i64, d.subsec_nanos()));
    parse_panic_content(&content, mtime, path)
}

fn parse_panic_content(
    content: &str,
    mtime_utc: Option<DateTime<Utc>>,
    path: &Path,
) -> Option<RecentPanic> {
    let mut lines = content.lines();

    // Header line: `[<ts>] PANIC runner-id=<id> pid=<pid> version=<ver>`.
    let header = lines.next()?;
    if !header.contains("PANIC") {
        return None;
    }

    let timestamp = parse_header_timestamp(header)
        .or(mtime_utc)
        .unwrap_or_else(Utc::now);
    let runner_id = parse_header_kv(header, "runner-id=");
    let pid = parse_header_kv(header, "pid=").and_then(|s| s.parse::<u32>().ok());
    let version = parse_header_kv(header, "version=");

    let mut location: Option<String> = None;
    let mut thread: Option<String> = None;
    let mut payload_lines: Vec<String> = Vec::new();
    let mut backtrace_lines: Vec<String> = Vec::new();

    #[derive(PartialEq)]
    enum Section {
        Top,
        Payload,
        Backtrace,
    }

    let mut section = Section::Top;

    for line in lines {
        match section {
            Section::Top => {
                if let Some(rest) = line.strip_prefix("location: ") {
                    location = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("thread: ") {
                    let raw = rest.trim();
                    // `<unnamed>` means the runner's thread had no name.
                    if raw != "<unnamed>" {
                        thread = Some(raw.to_string());
                    }
                } else if line.trim() == "payload:" {
                    section = Section::Payload;
                }
            }
            Section::Payload => {
                if line.trim() == "backtrace:" {
                    section = Section::Backtrace;
                } else {
                    payload_lines.push(line.to_string());
                }
            }
            Section::Backtrace => {
                backtrace_lines.push(line.to_string());
            }
        }
    }

    // Trim trailing blank lines from payload (the runner writes an empty
    // line between payload and backtrace).
    while matches!(payload_lines.last(), Some(s) if s.trim().is_empty()) {
        payload_lines.pop();
    }
    let payload = payload_lines.join("\n");

    let backtrace_preview = if backtrace_lines.is_empty() {
        None
    } else {
        Some(
            backtrace_lines
                .iter()
                .take(BACKTRACE_PREVIEW_FRAMES)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };

    Some(RecentPanic {
        timestamp,
        payload,
        location,
        thread,
        backtrace_preview,
        runner_id,
        pid,
        version,
        file_path: path.to_string_lossy().into_owned(),
    })
}

/// Extract the value of `key=...` (until next whitespace) from the header.
fn parse_header_kv(header: &str, key: &str) -> Option<String> {
    let idx = header.find(key)?;
    let rest = &header[idx + key.len()..];
    let value: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Extract the timestamp from `[<ts>]` at the start of the header.
fn parse_header_timestamp(header: &str) -> Option<DateTime<Utc>> {
    let rest = header.strip_prefix('[')?;
    let end = rest.find(']')?;
    let ts_str = &rest[..end];
    DateTime::parse_from_rfc3339(ts_str)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Return `true` if the panic log's mtime is within
/// [`PANIC_LOG_FRESHNESS_SECS`] of `exit_time`. Protects against a stale
/// panic log from an earlier boot being wrongly attributed to a clean
/// exit on a later boot.
pub fn is_fresh(panic: &RecentPanic, exit_time: DateTime<Utc>) -> bool {
    (exit_time - panic.timestamp).num_seconds().abs() <= PANIC_LOG_FRESHNESS_SECS
}

/// Format the first N frames of the backtrace preview for inclusion in a
/// log line. Identical to `recent_panic.backtrace_preview` but returns
/// the empty string when the preview is `None`, which is friendlier for
/// `format!()`.
#[allow(dead_code)]
pub fn format_backtrace_preview_for_log(panic: &RecentPanic) -> &str {
    panic.backtrace_preview.as_deref().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture_file(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    fn sample_body() -> &'static str {
        "[2026-04-22T12:00:00.000+00:00] PANIC runner-id=test-42 pid=4321 version=1.0.0\n\
         location: src/main.rs:50:10\n\
         thread: main\n\
         payload:\n\
         explicit panic in test\n\
         \n\
         backtrace:\n\
         0: frame_one\n\
         1: frame_two\n\
         2: frame_three\n"
    }

    #[test]
    fn parse_sample_body_populates_every_field() {
        let f = fixture_file(sample_body());
        let parsed = parse_panic_file(f.path()).expect("parse");
        assert_eq!(parsed.payload, "explicit panic in test");
        assert_eq!(parsed.location.as_deref(), Some("src/main.rs:50:10"));
        assert_eq!(parsed.thread.as_deref(), Some("main"));
        assert_eq!(parsed.runner_id.as_deref(), Some("test-42"));
        assert_eq!(parsed.pid, Some(4321));
        assert_eq!(parsed.version.as_deref(), Some("1.0.0"));
        let bt = parsed.backtrace_preview.as_deref().unwrap();
        assert!(bt.contains("0: frame_one"));
        assert!(bt.contains("2: frame_three"));
    }

    #[test]
    fn parse_missing_header_returns_none() {
        let f = fixture_file("this is not a panic log\nlocation: foo\n");
        assert!(parse_panic_file(f.path()).is_none());
    }

    #[test]
    fn parse_empty_file_returns_none() {
        let f = fixture_file("");
        assert!(parse_panic_file(f.path()).is_none());
    }

    #[test]
    fn parse_nonexistent_path_returns_none() {
        let p = PathBuf::from("/definitely/does/not/exist/runner-panic.log");
        assert!(parse_panic_file(&p).is_none());
    }

    #[test]
    fn parse_unnamed_thread_becomes_none() {
        let body = "[2026-04-22T12:00:00.000+00:00] PANIC runner-id=x pid=1 version=0.1\n\
                    location: src/foo.rs:1:1\n\
                    thread: <unnamed>\n\
                    payload:\n\
                    boom\n\
                    \n\
                    backtrace:\n\
                    0: frame\n";
        let f = fixture_file(body);
        let parsed = parse_panic_file(f.path()).unwrap();
        assert!(parsed.thread.is_none(), "<unnamed> should become None");
    }

    #[test]
    fn parse_multiline_payload_preserves_line_breaks() {
        let body = "[2026-04-22T12:00:00.000+00:00] PANIC runner-id=x pid=1 version=0.1\n\
                    location: src/foo.rs:1:1\n\
                    thread: main\n\
                    payload:\n\
                    first line\n\
                    second line\n\
                    third line\n\
                    \n\
                    backtrace:\n\
                    0: frame\n";
        let f = fixture_file(body);
        let parsed = parse_panic_file(f.path()).unwrap();
        assert_eq!(parsed.payload, "first line\nsecond line\nthird line");
    }

    #[test]
    fn parse_missing_optional_fields_ok() {
        // No thread line, no backtrace, minimal header.
        let body = "[2026-04-22T12:00:00.000+00:00] PANIC\n\
                    location: src/foo.rs:1:1\n\
                    payload:\n\
                    bang\n";
        let f = fixture_file(body);
        let parsed = parse_panic_file(f.path()).unwrap();
        assert_eq!(parsed.payload, "bang");
        assert!(parsed.thread.is_none());
        assert!(parsed.runner_id.is_none());
        assert!(parsed.pid.is_none());
        assert!(parsed.version.is_none());
        assert!(parsed.backtrace_preview.is_none());
    }

    #[test]
    fn backtrace_preview_caps_at_fifteen_frames() {
        let mut body = String::from(
            "[2026-04-22T12:00:00.000+00:00] PANIC runner-id=x pid=1 version=0.1\n\
             location: src/foo.rs:1:1\n\
             thread: main\n\
             payload:\n\
             b\n\
             \n\
             backtrace:\n",
        );
        for i in 0..30 {
            body.push_str(&format!("{i}: frame_{i}\n"));
        }
        let f = fixture_file(&body);
        let parsed = parse_panic_file(f.path()).unwrap();
        let preview = parsed.backtrace_preview.as_deref().unwrap();
        let frame_count = preview.lines().count();
        assert_eq!(frame_count, BACKTRACE_PREVIEW_FRAMES);
        assert!(preview.contains("0: frame_0"));
        assert!(preview.contains("14: frame_14"));
        assert!(!preview.contains("15: frame_15"));
    }

    #[test]
    fn is_fresh_within_window() {
        let parsed = RecentPanic {
            timestamp: DateTime::parse_from_rfc3339("2026-04-22T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            payload: "x".to_string(),
            location: None,
            thread: None,
            backtrace_preview: None,
            runner_id: None,
            pid: None,
            version: None,
            file_path: "x".to_string(),
        };
        let exit_30s_later = DateTime::parse_from_rfc3339("2026-04-22T12:00:30Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(is_fresh(&parsed, exit_30s_later));
        let exit_2m_later = DateTime::parse_from_rfc3339("2026-04-22T12:02:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!is_fresh(&parsed, exit_2m_later));
    }

    #[test]
    fn read_cap_does_not_break_short_files() {
        // Sanity: the cap is a ceiling, not a minimum.
        let f = fixture_file(sample_body());
        assert!(parse_panic_file(f.path()).is_some());
    }

    #[test]
    fn resolve_panic_log_path_uses_hint_when_given() {
        let hint = PathBuf::from("/tmp/my-runner-logs");
        let p = resolve_panic_log_path(Some(&hint));
        assert_eq!(p, PathBuf::from("/tmp/my-runner-logs/runner-panic.log"));
    }

    #[test]
    fn resolve_panic_log_path_falls_back_to_default() {
        let p = resolve_panic_log_path(None);
        assert!(p.ends_with("runner-panic.log"));
    }
}
