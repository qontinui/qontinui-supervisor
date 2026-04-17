use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::ChildStdout;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, warn};

use crate::config::log_buffer_size;

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
    buffer: Arc<RwLock<VecDeque<LogEntry>>>,
    sender: broadcast::Sender<LogEntry>,
    /// Optional append-only file writer. When set, every entry pushed to the
    /// in-memory buffer is also written here. Arc<Mutex<File>> so clones of
    /// this writer can be handed to spawn_*_reader helpers.
    file_writer: std::sync::RwLock<Option<FileWriter>>,
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
            sender,
            file_writer: std::sync::RwLock::new(None),
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

    pub async fn push(&self, entry: LogEntry) {
        let mut buf = self.buffer.write().await;
        if buf.len() >= log_buffer_size() {
            buf.pop_front();
        }
        buf.push_back(entry.clone());
        drop(buf);

        // Persist before broadcasting so a crashing receiver can't drop the line.
        if let Some(w) = self.current_writer() {
            write_entry_to_file(&w, &entry);
        }

        let _ = self.sender.send(entry);
    }

    pub async fn history(&self) -> Vec<LogEntry> {
        self.buffer.read().await.iter().cloned().collect()
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
pub fn spawn_stdout_reader(stdout: ChildStdout, logs: &LogState) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    let buffer = logs.buffer.clone();
    let file_writer = logs.current_writer();

    tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
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
pub fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    logs: &LogState,
) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    let buffer = logs.buffer.clone();
    let file_writer = logs.current_writer();
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
    let buffer = logs.buffer.clone();
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
                        if buf.len() >= log_buffer_size() {
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
