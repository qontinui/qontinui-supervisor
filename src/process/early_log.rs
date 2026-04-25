//! Early-death diagnostic log capture for spawned runners.
//!
//! When a temp/named runner is spawned via `POST /runners/spawn-test` (or
//! `spawn-named`), a per-spawn log file is opened in `std::env::temp_dir()`
//! and the runner's stdout/stderr are tee'd to it alongside the existing
//! in-memory ring buffer. The file path is surfaced to spawn-test callers
//! via the `early_log_path` field on early-failure error responses, and
//! via `GET /runners/{id}/logs` while the runner is alive.
//!
//! Why this exists: the in-memory log ring buffer (capped at ~500 lines and
//! often flooded with ANSI noise) gets scrubbed when the supervisor
//! finishes cleaning up a dead runner. If a runner dies mid-startup
//! (panic during DB connect, axum router build, Tauri builder), the
//! `recent_logs` array in the spawn-test 500/502 response can be
//! truncated to nothing useful — and a follow-up `GET /runners/{id}/logs`
//! returns 404 because the runner has been removed from the active
//! registry. The per-spawn file is a separate, additive capture that
//! survives across cleanup so post-mortem debugging is always possible.
//!
//! # Layout
//!
//! Files live in `<TEMP_DIR>/qontinui-supervisor-spawn-logs/<runner_id>-<rfc3339-millis>.log`.
//! The directory is created lazily on first use. There is no rotation — the
//! file is hard-capped at [`EARLY_LOG_BYTE_CAP`] bytes via inline truncation
//! so a runaway runner can't fill the disk. Files are deleted when the
//! runner is explicitly stopped via `POST /runners/{id}/stop`; files for
//! runners that died unexpectedly are preserved (the whole point of this
//! capture).
//!
//! # Best-effort
//!
//! Every IO operation in this module swallows errors at warn level. A
//! filesystem failure (disk full, permission denied, antivirus interfering)
//! must never crash the supervisor or block runner startup.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tracing::{debug, warn};

use crate::log_capture::LogLevel;

/// Hard cap on the early-log file size. When a write would push past this
/// limit, the file is truncated in place to the last ~75% of the cap so the
/// most recent activity (which is what crash diagnosis cares about) is
/// preserved.
///
/// 256 KiB ≈ ~2,500 medium-length log lines — far more than enough to cover
/// the first 30 seconds of a runner's startup.
pub const EARLY_LOG_BYTE_CAP: u64 = 256 * 1024;

/// After truncation, retain this fraction of the cap so we don't immediately
/// re-truncate on the next write.
const RETAIN_AFTER_TRUNCATE: u64 = (EARLY_LOG_BYTE_CAP * 3) / 4;

/// Subdirectory under `std::env::temp_dir()` where early-log files live.
pub const EARLY_LOG_SUBDIR: &str = "qontinui-supervisor-spawn-logs";

/// Resolve the early-log directory inside the platform temp dir. Creates the
/// directory if it does not exist. Returns `None` on a creation error so
/// callers can degrade gracefully (no early-log capture, but the runner
/// still starts).
pub fn early_log_dir() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(EARLY_LOG_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Failed to create early-log dir {:?}: {}", dir, e);
        return None;
    }
    Some(dir)
}

/// Build the per-spawn file path: `<early_log_dir>/<runner_id>-<rfc3339-millis>.log`.
///
/// The timestamp suffix lets the same `runner_id` be re-spawned later (e.g.
/// after cleanup) without overwriting the prior crash log. Colons in the
/// RFC3339 timestamp are replaced with `-` because Windows file paths reject
/// `:` outside of drive prefixes.
pub fn early_log_path_for(dir: &Path, runner_id: &str) -> PathBuf {
    let ts = chrono::Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .replace(':', "-");
    dir.join(format!("{}-{}.log", runner_id, ts))
}

/// Append-only writer for a runner's early-log file with byte-cap rotation.
///
/// Cloneable: the inner `Arc<StdMutex<...>>` is shared across the stdout and
/// stderr reader tasks so they both write to the same file. The mutex is
/// held only long enough to write a single already-formatted line.
#[derive(Clone)]
pub struct EarlyLogWriter {
    inner: Arc<StdMutex<EarlyLogInner>>,
}

struct EarlyLogInner {
    file: File,
    path: PathBuf,
    /// Current file size in bytes, tracked locally so we don't have to stat
    /// before every write. Updated after each successful append.
    size: u64,
}

impl EarlyLogWriter {
    /// Open (or create) the early-log file at `path` in append mode.
    /// Returns `None` on any IO error — early-log capture is best-effort
    /// and must never block runner startup.
    pub fn open(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    warn!("Failed to create early-log parent dir {:?}: {}", parent, e);
                    return None;
                }
            }
        }
        // Open with `write(true)` (NOT `append(true)`) so `set_len(0)` is
        // permitted by the same handle on Windows — append mode there locks
        // the file against in-place truncation. We compensate by explicitly
        // seeking to EOF before every write below.
        //
        // `.truncate(false)` is explicit: we want to preserve any existing
        // content (this matters if the same `runner_id-<ts>` ever re-opens,
        // unlikely with millisecond timestamps but cheap insurance) and we
        // do our own truncation in `truncate_file_in_place` for cap
        // enforcement.
        match OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(false)
            .open(path)
        {
            Ok(mut file) => {
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                // Position at EOF so the first write appends rather than
                // overwriting any prior content (relevant if a prior spawn of
                // the same `runner_id-<ts>` somehow re-opens the same file —
                // unlikely with the timestamp suffix but cheap insurance).
                let _ = file.seek(SeekFrom::End(0));
                debug!(
                    "Opened early-log file {:?} (existing size {} bytes)",
                    path, size
                );
                Some(Self {
                    inner: Arc::new(StdMutex::new(EarlyLogInner {
                        file,
                        path: path.to_path_buf(),
                        size,
                    })),
                })
            }
            Err(e) => {
                warn!("Failed to open early-log file {:?}: {}", path, e);
                None
            }
        }
    }

    /// Append a single log line, formatted as `<level> <message>\n`.
    /// Best-effort: IO errors are swallowed at warn level (logged via
    /// `eprintln!` to avoid recursing into the supervisor's own log buffer).
    /// If the resulting file size would exceed [`EARLY_LOG_BYTE_CAP`], the
    /// file is truncated in place first, retaining the most recent
    /// [`RETAIN_AFTER_TRUNCATE`] bytes.
    pub fn write_line(&self, level: LogLevel, message: &str) {
        let level_str = match level {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Debug => "DEBUG",
        };
        let line = format!("{} {}\n", level_str, message);
        let bytes = line.as_bytes();

        let Ok(mut guard) = self.inner.lock() else {
            return;
        };

        // Cap enforcement: if writing this line would push us past the cap,
        // truncate the file to its last RETAIN_AFTER_TRUNCATE bytes first.
        // Failures here are non-fatal — we'll just keep appending and let the
        // file grow a bit past the cap.
        if guard.size + bytes.len() as u64 > EARLY_LOG_BYTE_CAP {
            if let Err(e) = truncate_file_in_place(&mut guard.file, RETAIN_AFTER_TRUNCATE) {
                eprintln!(
                    "early_log: failed to truncate {:?}: {} (continuing without truncation)",
                    guard.path, e
                );
            } else {
                guard.size = guard
                    .file
                    .metadata()
                    .map(|m| m.len())
                    .unwrap_or(RETAIN_AFTER_TRUNCATE);
            }
        }

        // Explicit seek-to-EOF: we opened with `write(true)`, not `append`, so
        // the file cursor doesn't auto-advance to EOF on each write. Failing
        // the seek is non-fatal — fall through and let write_all do its
        // best (worst case: we overwrite a few bytes mid-file, which is
        // ugly but not crash-worthy).
        let _ = guard.file.seek(SeekFrom::End(0));

        if let Err(e) = guard.file.write_all(bytes) {
            eprintln!("early_log: failed to append to {:?}: {}", guard.path, e);
            return;
        }
        guard.size += bytes.len() as u64;
    }

    /// Return the path this writer is appending to. Cheap clone of the
    /// stored `PathBuf`. Currently only used in tests; production code
    /// retrieves the path via `ManagedRunner::early_log_path` instead.
    #[allow(dead_code)]
    pub fn path(&self) -> Option<PathBuf> {
        self.inner.lock().ok().map(|g| g.path.clone())
    }
}

/// Truncate `file` to its last `keep_bytes` bytes by reading the tail into
/// memory, rewinding to byte 0, writing the tail, and setting the new
/// length. Used for inline cap enforcement so the file never grows past
/// [`EARLY_LOG_BYTE_CAP`] for long.
///
/// On Windows, `set_len` shrinks the file in place; on Unix it does the
/// same. The seek-rewind-rewrite-truncate sequence keeps the file handle
/// usable for further appends.
fn truncate_file_in_place(file: &mut File, keep_bytes: u64) -> std::io::Result<()> {
    use std::io::Read;
    let len = file.metadata()?.len();
    if len <= keep_bytes {
        return Ok(());
    }
    let skip = len - keep_bytes;
    file.seek(SeekFrom::Start(skip))?;
    let mut tail = Vec::with_capacity(keep_bytes as usize);
    file.read_to_end(&mut tail)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&tail)?;
    file.flush()?;
    // Reposition for subsequent appends. With OpenOptions::append() on most
    // platforms writes always go to EOF regardless of position, but be
    // explicit anyway.
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

/// Delete an early-log file. Best-effort: a missing file is fine, and any
/// other IO error is logged at debug level (the file lives under temp_dir
/// and will be cleaned up by OS-level temp sweeps eventually).
pub fn delete_early_log(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => debug!("Removed early-log file {:?}", path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => debug!("Failed to remove early-log file {:?}: {}", path, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_to_string(path: &Path) -> String {
        let mut s = String::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    #[test]
    fn write_appends_lines_with_level_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        let writer = EarlyLogWriter::open(&path).expect("open");
        writer.write_line(LogLevel::Info, "first line");
        writer.write_line(LogLevel::Error, "boom");
        let content = read_to_string(&path);
        assert_eq!(content, "INFO first line\nERROR boom\n");
    }

    #[test]
    fn write_is_idempotent_across_clones() {
        // Both clones share the same underlying file via Arc<Mutex>.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.log");
        let writer = EarlyLogWriter::open(&path).expect("open");
        let clone = writer.clone();
        writer.write_line(LogLevel::Info, "from-original");
        clone.write_line(LogLevel::Warn, "from-clone");
        let content = read_to_string(&path);
        assert!(content.contains("INFO from-original"));
        assert!(content.contains("WARN from-clone"));
    }

    #[test]
    fn truncation_keeps_most_recent_bytes_when_cap_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncate.log");
        let writer = EarlyLogWriter::open(&path).expect("open");
        // Write enough data to exceed EARLY_LOG_BYTE_CAP. Each line is ~120
        // bytes, so 5,000 lines ~= 600 KB, well past the 256 KB cap.
        let big_line = "X".repeat(110);
        for i in 0..5_000 {
            writer.write_line(LogLevel::Info, &format!("{} {}", i, big_line));
        }
        let final_size = std::fs::metadata(&path).unwrap().len();
        assert!(
            final_size <= EARLY_LOG_BYTE_CAP,
            "file should be at or below cap after truncation; got {} bytes (cap {})",
            final_size,
            EARLY_LOG_BYTE_CAP
        );
        let content = read_to_string(&path);
        // The final lines must still be present — that's the whole point.
        assert!(
            content.contains("4999"),
            "most recent line should survive truncation"
        );
    }

    #[test]
    fn truncation_no_op_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.log");
        let writer = EarlyLogWriter::open(&path).expect("open");
        for i in 0..10 {
            writer.write_line(LogLevel::Info, &format!("line {}", i));
        }
        let content = read_to_string(&path);
        // No truncation should have occurred — every line still present.
        for i in 0..10 {
            assert!(content.contains(&format!("line {}", i)));
        }
    }

    #[test]
    fn delete_is_idempotent_and_swallows_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doomed.log");
        let writer = EarlyLogWriter::open(&path).expect("open");
        writer.write_line(LogLevel::Info, "soon to die");
        assert!(path.exists());
        delete_early_log(&path);
        assert!(!path.exists());
        // Calling again on a missing file does not panic or warn.
        delete_early_log(&path);
    }

    #[test]
    fn early_log_path_for_includes_runner_id_and_extension() {
        let dir = tempfile::tempdir().unwrap();
        let p = early_log_path_for(dir.path(), "test-abc123");
        let s = p.to_string_lossy();
        assert!(s.contains("test-abc123"));
        assert!(s.ends_with(".log"));
        // Windows path safety: no raw `:` characters in the filename portion.
        let filename = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !filename.contains(':'),
            "filename contains ':' which is invalid on Windows: {}",
            filename
        );
    }

    #[test]
    fn early_log_dir_creates_subdir_under_temp() {
        let dir = early_log_dir().expect("temp dir should be writable");
        assert!(dir.exists());
        assert!(dir.ends_with(EARLY_LOG_SUBDIR));
    }

    #[test]
    fn open_returns_none_on_unwritable_path() {
        // Use a path with a non-existent directory inside an existing one to
        // force an open failure that isn't recoverable via create_dir_all.
        // On most platforms, opening with a NUL byte in the path fails fast.
        let bad = Path::new("\0/cannot/open/this.log");
        assert!(EarlyLogWriter::open(bad).is_none());
    }
}
