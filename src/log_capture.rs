use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, warn};

use crate::config::LOG_BUFFER_SIZE;

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
    Watchdog,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

pub struct LogState {
    buffer: Arc<RwLock<VecDeque<LogEntry>>>,
    sender: broadcast::Sender<LogEntry>,
}

impl LogState {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            buffer: Arc::new(RwLock::new(VecDeque::with_capacity(LOG_BUFFER_SIZE))),
            sender,
        }
    }

    pub async fn push(&self, entry: LogEntry) {
        let mut buf = self.buffer.write().await;
        if buf.len() >= LOG_BUFFER_SIZE {
            buf.pop_front();
        }
        buf.push_back(entry.clone());
        drop(buf);

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
pub fn spawn_stdout_reader(
    stdout: ChildStdout,
    logs: &LogState,
) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    let buffer = logs.buffer.clone();

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
                        if buf.len() >= LOG_BUFFER_SIZE {
                            buf.pop_front();
                        }
                        buf.push_back(entry.clone());
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
pub fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    logs: &LogState,
) -> tokio::task::JoinHandle<()> {
    let sender = logs.sender.clone();
    let buffer = logs.buffer.clone();

    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
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
                        if buf.len() >= LOG_BUFFER_SIZE {
                            buf.pop_front();
                        }
                        buf.push_back(entry.clone());
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
