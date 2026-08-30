use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{Duration as StdDuration, Instant};
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

/// When a capture file is rolled over, and how many rolled-over segments
/// survive.
///
/// This exists because the runner-stdout capture had **no** bound at all:
/// `.dev-logs/primary.log` was measured at **1.85 GB** on 2026-08-30 after a
/// wedged runner spent ~14h flooding it. An unbounded append is not a logging
/// strategy, it is a disk-exhaustion clock — and the disk it exhausts is the
/// one every build slot shares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// Roll over before the live file would exceed this many bytes.
    pub max_bytes: u64,
    /// Roll over once the live segment has been open this long, however small
    /// it is — so a low-traffic log still produces readable time slices.
    ///
    /// Measured from when THIS process opened the file (a monotonic
    /// `Instant`, so a wall-clock correction cannot make a segment immortal
    /// or roll every line). A supervisor restart therefore starts a fresh age
    /// window; the size bound, not this one, is what caps disk.
    pub max_age: StdDuration,
    /// How many **rolled-over** segments to keep beside the live file. The
    /// live file is not counted, so the on-disk ceiling for one log is
    /// `max_bytes * (max_retained + 1)`, plus the tail of the single line that
    /// triggered the last roll.
    pub max_retained: usize,
}

impl RotationPolicy {
    /// 64 MiB per segment: large enough to hold a full cold-build log (the
    /// longest single burst this writer sees), small enough that a wedged
    /// runner's flood is capped in minutes rather than hours.
    pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
    /// 24h — one operator-day per segment when traffic is light.
    pub const DEFAULT_MAX_AGE_SECS: i64 = 86_400;
    /// 5 rolled-over segments => a ~384 MiB ceiling per log at the defaults.
    pub const DEFAULT_MAX_RETAINED: usize = 5;

    /// Read the policy from the environment, clamped to sane bounds.
    ///
    /// Knobs: `QONTINUI_SUPERVISOR_LOG_MAX_BYTES`,
    /// `QONTINUI_SUPERVISOR_LOG_MAX_AGE_SECS`,
    /// `QONTINUI_SUPERVISOR_LOG_MAX_RETAINED`. There is deliberately **no**
    /// "disable rotation" value: unbounded growth is the defect this closes,
    /// not a supported configuration.
    pub fn from_env() -> Self {
        let max_bytes = crate::config::parse_clamped_i64(
            "QONTINUI_SUPERVISOR_LOG_MAX_BYTES",
            Self::DEFAULT_MAX_BYTES as i64,
            1024 * 1024,            // 1 MiB floor
            8 * 1024 * 1024 * 1024, // 8 GiB ceiling
        ) as u64;
        let max_age_secs = crate::config::parse_clamped_i64(
            "QONTINUI_SUPERVISOR_LOG_MAX_AGE_SECS",
            Self::DEFAULT_MAX_AGE_SECS,
            60,
            30 * 86_400,
        );
        let max_retained = crate::config::parse_clamped_usize(
            "QONTINUI_SUPERVISOR_LOG_MAX_RETAINED",
            Self::DEFAULT_MAX_RETAINED,
            1,
            100,
        );
        Self {
            max_bytes,
            max_age: StdDuration::from_secs(max_age_secs as u64),
            max_retained,
        }
    }
}

/// An append-only log file that rolls over on size or age and prunes its own
/// history.
///
/// Every failure path is best-effort by construction: a failed rotate, a
/// failed prune and a failed reopen are each reported once and then tolerated
/// — persistent logging must never take the supervisor down, which is the
/// posture `open_append_log` has always had toward a bad path.
pub struct RotatingLogFile {
    path: std::path::PathBuf,
    /// `None` only between closing a handle and reopening it, or after a
    /// reopen failed — in which case writes are dropped until one succeeds.
    file: Option<File>,
    /// Bytes in the live segment. Seeded from the file's existing length on
    /// open, so an already-oversized file (the 1.85 GB one) rolls on the
    /// first line written rather than growing for another day.
    written: u64,
    opened_at: Instant,
    policy: RotationPolicy,
    /// Rotation/prune failures are reported once per segment, never per line
    /// — a broken log directory must not itself become a log flood.
    reported_failure: bool,
    /// The millisecond stamp and nonce of the segment this writer rotated to
    /// most recently, so a repeated stamp keeps counting up instead of
    /// restarting at 0 (see [`Self::rotated_path`]).
    last_stamp: Option<String>,
    last_nonce: u16,
}

impl RotatingLogFile {
    /// Open (creating if needed) `path` in append mode under `policy`.
    pub fn open(path: &Path, policy: RotationPolicy) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(file),
            written,
            opened_at: Instant::now(),
            policy,
            reported_failure: false,
            last_stamp: None,
            last_nonce: 0,
        })
    }

    /// Append one already-formatted line, rolling over first if this line
    /// would breach the policy.
    ///
    /// The check runs BEFORE the write, so `max_bytes` is a ceiling the live
    /// file does not cross rather than a threshold it overshoots by a line.
    /// A single line longer than `max_bytes` is still written whole (into an
    /// empty segment) — truncating a log line would corrupt the record to
    /// honour a bound whose purpose is disk, not tidiness.
    pub fn write_line(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.should_rotate(bytes.len() as u64) {
            self.rotate();
        }
        if self.file.is_none() {
            // A previous reopen failed. Try once more; a transient
            // permission / AV hold on Windows is the common case.
            self.reopen();
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn should_rotate(&self, incoming: u64) -> bool {
        // Never roll an empty segment: an age-triggered roll on an idle log
        // would otherwise mint an empty file per line and evict the real
        // history through the retained cap.
        if self.written == 0 {
            return false;
        }
        self.written.saturating_add(incoming) > self.policy.max_bytes
            || self.opened_at.elapsed() >= self.policy.max_age
    }

    /// Close, rename the live file to a timestamped sibling, prune the oldest
    /// siblings past the cap, then reopen a fresh live file.
    fn rotate(&mut self) {
        // Close FIRST. Windows refuses to rename a file that is open without
        // `FILE_SHARE_DELETE`, which `std::fs::File` does not request — so a
        // rename-while-open would fail on the platform this actually runs on.
        drop(self.file.take());

        let target = self.rotated_path();
        match std::fs::rename(&self.path, &target) {
            Ok(()) => {
                self.reported_failure = false;
                self.prune();
            }
            Err(e) => {
                self.report_once(format!(
                    "log_capture: failed to rotate {} -> {}: {} (continuing to append)",
                    self.path.display(),
                    target.display(),
                    e
                ));
                // Reopening the un-renamed file keeps logging alive, and
                // `written` is re-seeded from its real length below — so the
                // next line retries the rotate instead of going unbounded.
            }
        }
        self.reopen();
    }

    fn reopen(&mut self) {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => {
                self.written = f.metadata().map(|m| m.len()).unwrap_or(0);
                self.file = Some(f);
                self.opened_at = Instant::now();
            }
            Err(e) => {
                self.report_once(format!(
                    "log_capture: failed to reopen {} after rotation: {} (log lines dropped \
                     until it can be opened)",
                    self.path.display(),
                    e
                ));
                self.file = None;
                self.written = 0;
                self.opened_at = Instant::now();
            }
        }
    }

    /// `primary.log` -> `primary.20260830T041107123Z-000.log`.
    ///
    /// **Every part is fixed-width on purpose.** The stamp orders segments
    /// across milliseconds and the zero-padded nonce orders them within one,
    /// so a plain lexicographic name sort IS a chronological sort — which is
    /// what [`Self::prune`] relies on to delete the OLDEST segment rather
    /// than an arbitrary one. The nonce is always present (never elided for
    /// the first segment of a millisecond) because a mix of suffixed and
    /// un-suffixed names does not sort chronologically: `-` (0x2D) sorts
    /// before `.` (0x2E), which would rank the first segment of a burst as
    /// its newest.
    ///
    /// The nonce also stops a burst that crosses the cap repeatedly inside
    /// one millisecond from clobbering the segment it just wrote.
    fn rotated_path(&mut self) -> std::path::PathBuf {
        let dir = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let (stem, ext) = self.stem_and_ext();
        let stamp = Utc::now().format("%Y%m%dT%H%M%S%3fZ").to_string();

        // Resume the nonce within a repeated stamp instead of restarting at
        // 0. `exists()` alone is not enough: prune may already have DELETED
        // the low nonces, and reusing one would mint a name that sorts as the
        // oldest segment while holding the newest bytes — the next prune
        // would then evict the freshest history.
        let mut nonce = if self.last_stamp.as_deref() == Some(stamp.as_str()) {
            self.last_nonce.saturating_add(1)
        } else {
            0
        };
        let mut candidate = dir.join(format!("{stem}.{stamp}-{nonce:03}.{ext}"));
        while candidate.exists() && nonce < 999 {
            nonce += 1;
            candidate = dir.join(format!("{stem}.{stamp}-{nonce:03}.{ext}"));
        }
        self.last_nonce = nonce;
        self.last_stamp = Some(stamp);
        candidate
    }

    fn stem_and_ext(&self) -> (String, String) {
        let stem = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "log".to_string());
        let ext = self
            .path
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "log".to_string());
        (stem, ext)
    }

    /// Delete rolled-over segments beyond `max_retained`, oldest first.
    ///
    /// Only files this writer could have produced are considered: same
    /// directory, same `<stem>.` prefix, same `.<ext>` suffix, and never the
    /// live file itself.
    fn prune(&mut self) {
        let dir = self
            .path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let (stem, ext) = self.stem_and_ext();
        let live = self.path.file_name().map(|s| s.to_owned());
        let prefix = format!("{stem}.");
        let suffix = format!(".{ext}");

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                self.report_once(format!(
                    "log_capture: cannot list {} to prune rotated logs: {}",
                    dir.display(),
                    e
                ));
                return;
            }
        };

        let mut rotated: Vec<std::ffi::OsString> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.file_name())
            .filter(|name| {
                if live.as_deref() == Some(name.as_os_str()) {
                    return false;
                }
                let n = name.to_string_lossy();
                n.starts_with(&prefix) && n.ends_with(&suffix)
            })
            .collect();

        // Newest first — see `rotated_path`: the fixed-width stamp + nonce
        // make a name sort a chronological sort. Then drop the tail, so what
        // survives is the most RECENT history rather than an arbitrary slice
        // of it.
        rotated.sort_by(|a, b| b.cmp(a));
        for stale in rotated.into_iter().skip(self.policy.max_retained) {
            let victim = dir.join(&stale);
            if let Err(e) = std::fs::remove_file(&victim) {
                self.report_once(format!(
                    "log_capture: failed to delete rotated log {}: {}",
                    victim.display(),
                    e
                ));
            }
        }
    }

    /// Report at most one failure per segment, on stderr rather than through
    /// `tracing` — a warning here can be captured back into the supervisor's
    /// own log buffer and recurse into this very writer.
    fn report_once(&mut self, message: String) {
        if self.reported_failure {
            return;
        }
        self.reported_failure = true;
        eprintln!("{message}");
    }
}

/// Append-only, **self-rotating** file writer shared across log readers. Uses
/// a std::sync::Mutex because `write_all` is a blocking syscall — we only hold
/// it long enough to write a single already-formatted line (plus, once per
/// `max_bytes`, a rename and a directory listing).
pub type FileWriter = Arc<StdMutex<RotatingLogFile>>;

/// Open (or create) a log file in append mode under the environment's
/// [`RotationPolicy`]. Parent directory is created if missing. Returns `None`
/// on any IO error after logging a warning, so a bad log path never prevents
/// supervisor startup.
pub fn open_append_log(path: &Path) -> Option<FileWriter> {
    open_append_log_with_policy(path, RotationPolicy::from_env())
}

/// [`open_append_log`] with an explicit policy — the seam the rotation tests
/// drive with a small cap instead of waiting out a 24h soak.
pub fn open_append_log_with_policy(path: &Path, policy: RotationPolicy) -> Option<FileWriter> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!("Failed to create log directory {}: {}", parent.display(), e);
                return None;
            }
        }
    }
    match RotatingLogFile::open(path, policy) {
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
        if let Err(e) = f.write_line(line.as_bytes()) {
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

#[cfg(test)]
mod rotation_tests {
    use super::*;

    /// The 24h soak the remediation item asks for is not runnable in CI, so
    /// this drives the SAME rotation logic with a small cap: 200-byte
    /// segments, 2 retained. Both triggers and the retained cap are asserted
    /// from the filesystem, not from the writer's own bookkeeping.
    fn tiny_policy(max_bytes: u64, max_retained: usize) -> RotationPolicy {
        RotationPolicy {
            max_bytes,
            // Far larger than the test's runtime, so nothing rolls on age
            // here — the size trigger is what is under test.
            max_age: StdDuration::from_secs(3600),
            max_retained,
        }
    }

    fn rotated_segments(dir: &Path, live: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("list log dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != live)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn size_trigger_keeps_every_file_under_the_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        // 32-byte lines against a 100-byte cap: rolls every 4th line.
        let mut f = RotatingLogFile::open(&path, tiny_policy(100, 50)).expect("open");
        let line = [b'x'; 31];
        let mut payload = line.to_vec();
        payload.push(b'\n');
        for _ in 0..40 {
            f.write_line(&payload).expect("write");
        }
        drop(f);

        let mut checked = 0;
        for entry in std::fs::read_dir(dir.path()).expect("list") {
            let entry = entry.expect("entry");
            let len = entry.metadata().expect("metadata").len();
            assert!(
                len <= 100,
                "{} is {len} bytes — rotation must cap every file at 100",
                entry.path().display()
            );
            checked += 1;
        }
        assert!(
            checked > 1,
            "40 x 32 bytes against a 100-byte cap must have rotated at least once, \
             saw {checked} file(s) — a passing assertion over ONE never-rotated file \
             would be vacuous"
        );
    }

    #[test]
    fn retained_cap_bounds_how_many_rotated_segments_survive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        let mut f = RotatingLogFile::open(&path, tiny_policy(60, 2)).expect("open");
        let payload = b"0123456789012345678901234567890123456789\n"; // 41 bytes

        // 20 lines at 41 bytes against a 60-byte cap => ~19 rotations, which
        // is far more than the cap of 2.
        for _ in 0..20 {
            f.write_line(payload).expect("write");
        }
        drop(f);

        let rotated = rotated_segments(dir.path(), "primary.log");
        assert_eq!(
            rotated.len(),
            2,
            "max_retained=2 must leave exactly 2 rotated segments, saw {rotated:?}"
        );
        assert!(path.exists(), "the live file is not counted by the cap");

        // The survivors are the NEWEST segments — pruning oldest-first is
        // what makes a bounded log still useful for diagnosis.
        for name in &rotated {
            assert!(
                name.starts_with("primary.") && name.ends_with(".log"),
                "unexpected survivor {name}"
            );
        }
    }

    /// The cap must evict the OLDEST segment, not an arbitrary one — a
    /// bounded log that keeps the wrong slice is no more diagnosable than an
    /// unbounded one. Each segment here carries a distinguishable payload, so
    /// the assertion is about identity rather than about a count.
    #[test]
    fn pruning_evicts_the_oldest_segment_not_an_arbitrary_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        // 8-byte lines against an 8-byte cap: every line after the first
        // rolls, so segment N holds exactly the Nth marker.
        let mut f = RotatingLogFile::open(&path, tiny_policy(8, 2)).expect("open");
        for marker in ["aaaaaaa", "bbbbbbb", "ccccccc", "ddddddd"] {
            f.write_line(format!("{marker}\n").as_bytes())
                .expect("write");
        }
        drop(f);

        let rotated = rotated_segments(dir.path(), "primary.log");
        assert_eq!(rotated.len(), 2, "cap of 2 rotated segments: {rotated:?}");

        let bodies: Vec<String> = rotated
            .iter()
            .map(|n| std::fs::read_to_string(dir.path().join(n)).expect("read segment"))
            .collect();
        let live = std::fs::read_to_string(&path).expect("read live");

        assert!(
            bodies.iter().any(|b| b.contains("bbbbbbb")),
            "the second-oldest surviving segment is kept: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|b| b.contains("ccccccc")),
            "the newest rotated segment is kept: {bodies:?}"
        );
        assert!(
            !bodies.iter().any(|b| b.contains("aaaaaaa")),
            "the OLDEST segment is the one evicted: {bodies:?}"
        );
        assert!(live.contains("ddddddd"), "live file holds the newest line");
    }

    #[test]
    fn age_trigger_rolls_a_segment_that_never_reaches_the_size_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        let policy = RotationPolicy {
            max_bytes: 1024 * 1024, // never reached by this test
            max_age: StdDuration::ZERO,
            max_retained: 3,
        };
        let mut f = RotatingLogFile::open(&path, policy).expect("open");
        for _ in 0..3 {
            f.write_line(b"tiny\n").expect("write");
        }
        drop(f);

        let rotated = rotated_segments(dir.path(), "primary.log");
        assert!(
            !rotated.is_empty(),
            "an expired segment must roll on age even far below max_bytes"
        );
        assert!(
            rotated.len() <= 3,
            "the retained cap applies to age-triggered rolls too: {rotated:?}"
        );
    }

    #[test]
    fn an_empty_segment_never_rotates() {
        // Otherwise an expired-but-idle log mints one empty file per line and
        // evicts the real history through the retained cap.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        let policy = RotationPolicy {
            max_bytes: 10,
            max_age: StdDuration::ZERO,
            max_retained: 3,
        };
        let mut f = RotatingLogFile::open(&path, policy).expect("open");
        f.write_line(b"a\n").expect("write");
        drop(f);

        assert!(
            rotated_segments(dir.path(), "primary.log").is_empty(),
            "the first line into a fresh empty file must not roll it"
        );
    }

    #[test]
    fn an_already_oversized_file_rolls_on_the_first_line() {
        // The 1.85 GB case: the file exists and is already past the cap when
        // the supervisor starts. `written` is seeded from its real length, so
        // it rolls immediately instead of growing for another day.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        std::fs::write(&path, vec![b'x'; 5000]).expect("seed oversized log");

        let mut f = RotatingLogFile::open(&path, tiny_policy(100, 2)).expect("open");
        f.write_line(b"first line after restart\n").expect("write");
        drop(f);

        let rotated = rotated_segments(dir.path(), "primary.log");
        assert_eq!(
            rotated.len(),
            1,
            "the oversized file was rolled: {rotated:?}"
        );
        assert_eq!(
            std::fs::metadata(&path).expect("live metadata").len(),
            25,
            "the live file now holds only the new line"
        );
        assert_eq!(
            std::fs::metadata(dir.path().join(&rotated[0]))
                .expect("rotated metadata")
                .len(),
            5000,
            "rotation renames, it never truncates — the old bytes survive until \
             the retained cap evicts them"
        );
    }

    #[test]
    fn a_line_longer_than_the_cap_is_written_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        let mut f = RotatingLogFile::open(&path, tiny_policy(50, 3)).expect("open");
        f.write_line(b"seed\n").expect("write");
        let long = vec![b'y'; 500];
        f.write_line(&long).expect("write long line");
        drop(f);

        assert_eq!(
            std::fs::metadata(&path).expect("metadata").len(),
            500,
            "a single line over the cap is never truncated — a corrupt record \
             is worse than an oversized segment"
        );
    }

    #[test]
    fn pruning_ignores_files_that_are_not_this_log_s_segments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        std::fs::write(dir.path().join("secondary.log"), b"not ours").expect("write sibling");
        std::fs::write(dir.path().join("primary.txt"), b"not ours").expect("write sibling");

        let mut f = RotatingLogFile::open(&path, tiny_policy(20, 1)).expect("open");
        for _ in 0..10 {
            f.write_line(b"0123456789012345\n").expect("write");
        }
        drop(f);

        assert!(
            dir.path().join("secondary.log").exists(),
            "another runner's log must never be pruned by this one"
        );
        assert!(
            dir.path().join("primary.txt").exists(),
            "a same-stem file with a different extension is not our segment"
        );
    }

    #[test]
    fn the_default_policy_is_bounded_in_all_three_dimensions() {
        // A regression here would restore the unbounded growth this closes.
        let p = RotationPolicy {
            max_bytes: RotationPolicy::DEFAULT_MAX_BYTES,
            max_age: StdDuration::from_secs(RotationPolicy::DEFAULT_MAX_AGE_SECS as u64),
            max_retained: RotationPolicy::DEFAULT_MAX_RETAINED,
        };
        assert!(p.max_bytes > 0 && p.max_bytes <= 128 * 1024 * 1024);
        assert!(p.max_age > StdDuration::ZERO);
        assert!(p.max_retained > 0);
        // The advertised on-disk ceiling for one log.
        assert_eq!(
            p.max_bytes * (p.max_retained as u64 + 1),
            384 * 1024 * 1024,
            "the documented ~384 MiB per-log ceiling"
        );
    }

    #[test]
    fn entries_still_land_in_the_live_file_through_the_writer_shim() {
        // `write_entry_to_file` is the only path the log readers use; prove
        // rotation did not break plain appending.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("primary.log");
        let writer =
            open_append_log_with_policy(&path, tiny_policy(1024 * 1024, 3)).expect("open writer");
        write_entry_to_file(
            &writer,
            &LogEntry {
                timestamp: Utc::now(),
                source: LogSource::Runner,
                level: LogLevel::Error,
                message: "runner said something".to_string(),
            },
        );
        drop(writer);

        let body = std::fs::read_to_string(&path).expect("read log");
        assert!(
            body.contains("[runner] [ERROR] runner said something"),
            "{body}"
        );
    }
}
