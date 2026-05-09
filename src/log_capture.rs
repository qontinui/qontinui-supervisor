use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::ChildStdout;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, warn};

use crate::config::{build_log_buffer_size, log_buffer_size};
use crate::process::early_log::EarlyLogWriter;
use crate::state::{LastAuthResult, ManagedRunner};

/// Patterns matched against runner stdout/stderr lines to detect failed
/// auto-login attempts and rate-limit signals. A match populates
/// `ManagedRunner::last_auth_result` so the spawn-test response can surface
/// the most recent diagnostic to callers (e.g. autonomous agents that need
/// to know whether their freshly-spawned runner can talk to the backend).
///
/// Only failure-side patterns are matched here — the runner emits no
/// distinguishing line on success today. Patterns are case-insensitive and
/// expected to land somewhere in a single log line. See item B of the
/// supervisor cleanup plan for the rationale.
pub static AUTH_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)auto-?login\s+failed|rate[\s-]?limit(?:ed)?|HTTP\s+429|backend\s+returned\s+429",
    )
    .expect("AUTH_PATTERNS regex must compile")
});

/// Inspect a captured log line and, if it matches `AUTH_PATTERNS`, update
/// the runner's `last_auth_result` to record an observed failed attempt.
///
/// Spawned as a fire-and-forget task so the log-reader path stays sync. The
/// snippet is truncated to 200 chars (by char-count, not bytes) to keep the
/// API response small even if a runner emits a 4 KiB JSON-formatted line.
pub fn record_auth_signal_if_matching(managed: Arc<ManagedRunner>, line: String) {
    if !AUTH_PATTERNS.is_match(&line) {
        return;
    }
    tokio::spawn(async move {
        let snippet: String = line.chars().take(200).collect();
        let now = Utc::now();
        let mut guard = managed.last_auth_result.write().await;
        let entry = guard.get_or_insert_with(|| LastAuthResult {
            attempted: true,
            succeeded: Some(false),
            attempt_at: now,
            rate_limit_hint: None,
        });
        entry.attempted = true;
        entry.succeeded = Some(false);
        entry.attempt_at = now;
        entry.rate_limit_hint = Some(snippet);
    });
}

/// Append-only file writer shared across log readers. Uses a std::sync::Mutex
/// because `write_all` is a blocking syscall — we only hold it long enough to
/// write a single already-formatted line. No rotation: file grows unbounded.
pub type FileWriter = Arc<StdMutex<File>>;

/// Open (or create) a log file in append mode. Parent directory is created
/// if missing. Returns `None` on any IO error after logging a warning, so a
/// bad log path never prevents supervisor startup.
pub fn open_append_log(path: &Path) -> Option<FileWriter> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!("Failed to create log directory {}: {}", parent.display(), e);
                return None;
            }
        }
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(Arc::new(StdMutex::new(f))),
        Err(e) => {
            warn!("Failed to open log file {}: {}", path.display(), e);
            None
        }
    }
}

/// Format a log entry as a single line for the persistent log file.
/// Keeps the in-memory format identical to the existing SSE/buffer view
/// so operators can grep both sources the same way.
fn format_entry_line(entry: &LogEntry) -> String {
    let source = match entry.source {
        LogSource::Runner => "runner",
        LogSource::Supervisor => "supervisor",
        LogSource::Build => "build",
        LogSource::Expo => "expo",
    };
    let level = match entry.level {
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
        LogLevel::Debug => "DEBUG",
    };
    format!(
        "{} [{}] [{}] {}\n",
        entry
            .timestamp
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        source,
        level,
        entry.message
    )
}

/// Append one entry to the file, swallowing IO errors (persistent logging
/// must never crash the supervisor). Errors are warned once per call.
fn write_entry_to_file(writer: &FileWriter, entry: &LogEntry) {
    let line = format_entry_line(entry);
    if let Ok(mut f) = writer.lock() {
        if let Err(e) = f.write_all(line.as_bytes()) {
            // Don't use warn!/tracing here — it could recurse back into the
            // supervisor's own log buffer. Use eprintln which is unobserved.
            eprintln!("log_capture: failed to append to log file: {}", e);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub source: LogSource,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    Runner,
    Supervisor,
    Build,
    Expo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

/// Maximum number of panic-related stderr lines retained per runner.
const PANIC_WINDOW_CAP: usize = 50;

/// Sliding window that accumulates panic-related stderr lines. Thread-safe
/// via `std::sync::Mutex` (held only long enough to push/drain a line).
#[derive(Clone)]
pub struct PanicBuffer {
    inner: Arc<StdMutex<VecDeque<String>>>,
}

impl Default for PanicBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl PanicBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(VecDeque::with_capacity(PANIC_WINDOW_CAP))),
        }
    }

    /// Push a line into the sliding window. Evicts the oldest line when full.
    pub fn push(&self, line: String) {
        if let Ok(mut buf) = self.inner.lock() {
            if buf.len() >= PANIC_WINDOW_CAP {
                buf.pop_front();
            }
            buf.push_back(line);
        }
    }

    /// Drain the buffer and join all lines into a single string. Returns
    /// `None` if the buffer is empty.
    #[allow(dead_code)]
    pub fn drain_joined(&self) -> Option<String> {
        if let Ok(mut buf) = self.inner.lock() {
            if buf.is_empty() {
                return None;
            }
            let joined = buf.iter().cloned().collect::<Vec<_>>().join("\n");
            buf.clear();
            Some(joined)
        } else {
            None
        }
    }

    /// Snapshot the buffer without draining it.
    pub fn snapshot_joined(&self) -> Option<String> {
        if let Ok(buf) = self.inner.lock() {
            if buf.is_empty() {
                return None;
            }
            Some(buf.iter().cloned().collect::<Vec<_>>().join("\n"))
        } else {
            None
        }
    }
}

/// Returns true if the line looks like part of a Rust panic / backtrace.
fn is_panic_line(line: &str) -> bool {
    thread_local! {
        static PANIC_RE: Regex = Regex::new(
            r"thread '.*' panicked at|RUST_BACKTRACE|stack backtrace:|^\s+\d+:.*<.*as|note: run with"
        ).unwrap();
    }
    PANIC_RE.with(|re| re.is_match(line))
}

pub struct LogState {
    /// Buffer for everything except `LogSource::Build`. Capacity from
    /// `log_buffer_size()` (default 500).
    buffer: Arc<RwLock<VecDeque<LogEntry>>>,
    /// Dedicated buffer for `LogSource::Build` entries. Capacity from
    /// `build_log_buffer_size()` (default 5000). Cargo build output is dense
    /// — segregating it keeps a single rebuild from evicting supervisor
    /// events (placement preview, spawn lifecycle, expo status, etc.) from
    /// the main buffer. The broadcast channel still receives every entry so
    /// SSE consumers see one merged stream.
    build_buffer: Arc<RwLock<VecDeque<LogEntry>>>,
    sender: broadcast::Sender<LogEntry>,
    /// Optional append-only file writer. When set, every entry pushed to the
    /// in-memory buffer is also written here. Arc<Mutex<File>> so clones of
    /// this writer can be handed to spawn_*_reader helpers.
    file_writer: std::sync::RwLock<Option<FileWriter>>,
    /// Optional per-spawn early-death log writer. Independent from
    /// `file_writer`: this one only captures the runner's own stdout/stderr
    /// (via `spawn_stdout_reader` / `spawn_stderr_reader`) into a per-spawn
    /// file under `<TEMP_DIR>/qontinui-supervisor-spawn-logs/`. Its purpose is
    /// to survive cleanup of a runner that died mid-startup so post-mortem
    /// debugging is always possible. Supervisor-emitted entries (from
    /// `LogState::emit`) are NOT mirrored here — only the child's I/O.
    early_log_writer: std::sync::RwLock<Option<EarlyLogWriter>>,
    /// Sliding window of panic-related stderr lines. Populated by
    /// `spawn_stderr_reader`; flushed to `StoppedRunnerSnapshot::panic_stack`
    /// when the runner exits.
    panic_buffer: PanicBuffer,
}

impl Default for LogState {
    fn default() -> Self {
        Self::new()
    }
}

impl LogState {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            buffer: Arc::new(RwLock::new(VecDeque::with_capacity(log_buffer_size()))),
            build_buffer: Arc::new(RwLock::new(
                VecDeque::with_capacity(build_log_buffer_size()),
            )),
            sender,
            file_writer: std::sync::RwLock::new(None),
            early_log_writer: std::sync::RwLock::new(None),
            panic_buffer: PanicBuffer::new(),
        }
    }

    /// Access the panic buffer for this log state.
    pub fn panic_buffer(&self) -> &PanicBuffer {
        &self.panic_buffer
    }

    /// Attach (or replace) the persistent log file writer. Every subsequent
    /// `push`/`emit` call appends the entry to this file. Passing `None`
    /// detaches the writer — buffered entries are NOT retroactively flushed.
    pub fn set_file_writer(&self, writer: Option<FileWriter>) {
        if let Ok(mut guard) = self.file_writer.write() {
            *guard = writer;
        }
    }

    /// Snapshot the current file writer (cheap Arc clone). Used by
    /// `spawn_*_reader` helpers so each reader can write directly without
    /// holding an RwLock on every line.
    fn current_writer(&self) -> Option<FileWriter> {
        self.file_writer.read().ok().and_then(|g| g.clone())
    }

    /// Attach (or replace) the per-spawn early-death log writer. Subsequent
    /// `spawn_stdout_reader` / `spawn_stderr_reader` invocations will tee
    /// their lines to this writer in addition to the in-memory buffer and
    /// the persistent file writer (if any).
    ///
    /// Pass `None` to detach. Detaching does not close the file — readers
    /// that already captured a snapshot of the writer keep writing to it
    /// until they shut down. This is intentional: detach is meant for the
    /// "runner exited cleanly, drop the diagnostic file" path, not for
    /// suppressing late writes.
    pub fn set_early_log_writer(&self, writer: Option<EarlyLogWriter>) {
        if let Ok(mut guard) = self.early_log_writer.write() {
            *guard = writer;
        }
    }

    /// Snapshot the current early-log writer. Like [`current_writer`], this
    /// is called once per `spawn_*_reader` start so each reader can write
    /// directly without re-locking on every line.
    fn current_early_writer(&self) -> Option<EarlyLogWriter> {
        self.early_log_writer.read().ok().and_then(|g| g.clone())
    }

    pub async fn push(&self, entry: LogEntry) {
        // Route by source: `LogSource::Build` lands in the dedicated
        // build_buffer (5000-cap default) so cargo's flood does not evict
        // supervisor events from the main 500-cap buffer. Everything else
        // goes to the main buffer. The broadcast channel below still
        // receives every entry — SSE/stream consumers see one merged stream.
        if entry.source == LogSource::Build {
            let mut buf = self.build_buffer.write().await;
            if buf.len() >= build_log_buffer_size() {
                buf.pop_front();
            }
            buf.push_back(entry.clone());
        } else {
            let mut buf = self.buffer.write().await;
            if buf.len() >= log_buffer_size() {
                buf.pop_front();
            }
            buf.push_back(entry.clone());
        }

        // Persist before broadcasting so a crashing receiver can't drop the line.
        if let Some(w) = self.current_writer() {
            write_entry_to_file(&w, &entry);
        }

        let _ = self.sender.send(entry);
    }

    /// Returns the non-build history. Build output is segregated into
    /// `build_history()` so that callers (and the dashboard's `/logs/history`
    /// endpoint) are not flooded by cargo output during a rebuild.
    pub async fn history(&self) -> Vec<LogEntry> {
        self.buffer.read().await.iter().cloned().collect()
    }

    /// Returns only `LogSource::Build` entries from the dedicated build
    /// buffer. Surfaces via `GET /logs/build/history`.
    pub async fn build_history(&self) -> Vec<LogEntry> {
        self.build_buffer.read().await.iter().cloned().collect()
    }

    /// Returns build + non-build entries merged in chronological order.
    /// Convenience accessor for callers that want one stream over the live
    /// state of both buffers (e.g. ad-hoc debugging dumps). Allocates O(n+m).
    #[allow(dead_code)]
    pub async fn merged_history(&self) -> Vec<LogEntry> {
        let main = self.buffer.read().await;
        let build = self.build_buffer.read().await;
        let mut merged: Vec<LogEntry> = Vec::with_capacity(main.len() + build.len());
        merged.extend(main.iter().cloned());
        merged.extend(build.iter().cloned());
        merged.sort_by_key(|e| e.timestamp);
        merged
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.sender.subscribe()
    }

    pub async fn emit(&self, source: LogSource, level: LogLevel, message: impl Into<String>) {
        let entry = LogEntry {
            timestamp: Utc::now(),
            source,
            level,
            message: message.into(),
        };
        self.push(entry).await;
    }
}

/// Spawn a background task that reads lines from runner stdout and emits log entries.
///
/// Convenience wrapper that does not track per-runner auth signals. Callers
/// that want to populate `ManagedRunner::last_auth_result` should use
/// [`spawn_stdout_reader_for_runner`] instead.
#[allow(dead_code)]
pub fn spawn_stdout_reader(stdout: ChildStdout, logs: &LogState) -> tokio::task::JoinHandle<()> {
    spawn_stdout_reader_for_runner(stdout, logs, None)
}

/// Spawn a background task that reads lines from runner stdout and emits log entries.
///
/// When `managed` is `Some`, every captured line is also passed through
/// [`record_auth_signal_if_matching`] so failed auto-login attempts and
/// rate-limit hints surface on the runner's `last_auth_result` field.
pub fn spawn_stdout_reader_for_runner(
    stdout: ChildStdout,
    logs: &LogState,
    managed: Option<Arc<ManagedRunner>>,
) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    let buffer = logs.buffer.clone();
    let file_writer = logs.current_writer();
    let early_writer = logs.current_early_writer();

    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(m) = managed.as_ref() {
                        record_auth_signal_if_matching(m.clone(), line.clone());
                    }
                    let level = classify_log_level(&line);
                    let entry = LogEntry {
                        timestamp: Utc::now(),
                        source: LogSource::Runner,
                        level,
                        message: line,
                    };

                    {
                        let mut buf = buffer.write().await;
                        if buf.len() >= log_buffer_size() {
                            buf.pop_front();
                        }
                        buf.push_back(entry.clone());
                    }

                    if let Some(ref w) = file_writer {
                        write_entry_to_file(w, &entry);
                    }

                    if let Some(ref w) = early_writer {
                        w.write_line(entry.level.clone(), &entry.message);
                    }

                    let _ = sender.send(entry);
                }
                Ok(None) => {
                    debug!("Runner stdout closed");
                    break;
                }
                Err(e) => {
                    warn!("Error reading runner stdout: {}", e);
                    break;
                }
            }
        }
    })
}

/// Spawn a background task for stderr, tagging lines as errors.
///
/// Also detects Rust panic / backtrace lines and accumulates them in the
/// per-runner [`PanicBuffer`] so they can be preserved in the stopped-runner
/// cache for the `/runners/{id}/crash-dump` endpoint.
///
/// Convenience wrapper around [`spawn_stderr_reader_for_runner`] without
/// auth-signal tracking.
#[allow(dead_code)]
pub fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    logs: &LogState,
) -> tokio::task::JoinHandle<()> {
    spawn_stderr_reader_for_runner(stderr, logs, None)
}

/// Same as [`spawn_stderr_reader`] but with optional per-runner auth-signal
/// tracking. When `managed` is `Some`, every captured line is matched
/// against [`AUTH_PATTERNS`] and `last_auth_result` is updated on a hit.
pub fn spawn_stderr_reader_for_runner(
    stderr: tokio::process::ChildStderr,
    logs: &LogState,
    managed: Option<Arc<ManagedRunner>>,
) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    let buffer = logs.buffer.clone();
    let file_writer = logs.current_writer();
    let early_writer = logs.current_early_writer();
    let panic_buf = logs.panic_buffer.clone();

    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        // Once a panic header is seen, capture all subsequent lines until
        // the stream closes (backtraces can be many lines long). This avoids
        // missing late backtrace frames.
        let mut in_panic = false;

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(m) = managed.as_ref() {
                        record_auth_signal_if_matching(m.clone(), line.clone());
                    }
                    // Panic detection: accumulate into the sliding window.
                    if !in_panic && is_panic_line(&line) {
                        in_panic = true;
                    }
                    if in_panic {
                        panic_buf.push(line.clone());
                    }

                    let level = if line.contains("WARN") || line.contains("warn") {
                        LogLevel::Warn
                    } else {
                        LogLevel::Error
                    };
                    let entry = LogEntry {
                        timestamp: Utc::now(),
                        source: LogSource::Runner,
                        level,
                        message: line,
                    };

                    {
                        let mut buf = buffer.write().await;
                        if buf.len() >= log_buffer_size() {
                            buf.pop_front();
                        }
                        buf.push_back(entry.clone());
                    }

                    if let Some(ref w) = file_writer {
                        write_entry_to_file(w, &entry);
                    }

                    if let Some(ref w) = early_writer {
                        w.write_line(entry.level.clone(), &entry.message);
                    }

                    let _ = sender.send(entry);
                }
                Ok(None) => {
                    debug!("Runner stderr closed");
                    break;
                }
                Err(e) => {
                    warn!("Error reading runner stderr: {}", e);
                    break;
                }
            }
        }
    })
}

/// Spawn a reader for any async source, with configurable LogSource and classification behavior.
/// If `classify` is true, the log level is inferred from the line content.
/// If `classify` is false, all lines are tagged as Error (useful for stderr).
pub fn spawn_reader_with_source(
    reader: impl AsyncRead + Unpin + Send + 'static,
    logs: &LogState,
    source: LogSource,
    classify: bool,
) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    // Route by source: `LogSource::Build` lands in the segregated build_buffer
    // (5000-cap default); everything else uses the main buffer. See
    // `LogState::push` for the same routing logic on the `emit` path.
    let (buffer, buffer_cap) = if source == LogSource::Build {
        (logs.build_buffer.clone(), build_log_buffer_size())
    } else {
        (logs.buffer.clone(), log_buffer_size())
    };
    let file_writer = logs.current_writer();
    let source_name = format!("{:?}", source).to_lowercase();

    tokio::spawn(async move {
        let buf_reader = BufReader::new(reader);
        let mut lines = buf_reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let level = if classify {
                        classify_log_level(&line)
                    } else if line.contains("WARN") || line.contains("warn") {
                        LogLevel::Warn
                    } else {
                        LogLevel::Error
                    };
                    let entry = LogEntry {
                        timestamp: Utc::now(),
                        source: source.clone(),
                        level,
                        message: line,
                    };

                    {
                        let mut buf = buffer.write().await;
                        if buf.len() >= buffer_cap {
                            buf.pop_front();
                        }
                        buf.push_back(entry.clone());
                    }

                    if let Some(ref w) = file_writer {
                        write_entry_to_file(w, &entry);
                    }

                    let _ = sender.send(entry);
                }
                Ok(None) => {
                    debug!("{} stream closed", source_name);
                    break;
                }
                Err(e) => {
                    warn!("Error reading {} stream: {}", source_name, e);
                    break;
                }
            }
        }
    })
}

fn classify_log_level(line: &str) -> LogLevel {
    if line.contains("ERROR") || line.contains("error[E") || line.contains("panic") {
        LogLevel::Error
    } else if line.contains("WARN") || line.contains("warning") {
        LogLevel::Warn
    } else if line.contains("DEBUG") || line.contains("TRACE") {
        LogLevel::Debug
    } else {
        LogLevel::Info
    }
}
