//! Post-mortem snapshot cache for runners that have been removed from the
//! active registry.
//!
//! When a runner is removed (graceful stop, reaper sweep, crash cleanup), its
//! in-memory log buffer would normally be dropped and `GET /runners/{id}/logs`
//! returns 404. That makes post-mortem debugging hard — especially for temp
//! runners that crash during workflow generation. We keep a capped,
//! time-bounded cache of the last N log lines + metadata so callers can add
//! `?include_stopped=true` to the logs endpoint and still retrieve them.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;

use crate::log_capture::LogEntry;

/// Maximum entries retained in the stopped-runner cache.
pub const STOPPED_CACHE_MAX_ENTRIES: usize = 100;

/// How long a snapshot is retained before eviction.
pub const STOPPED_CACHE_TTL_SECS: i64 = 600; // 10 minutes

/// How many recent log lines to retain per stopped runner.
pub const STOPPED_CACHE_LOG_CAP: usize = 500;

/// Why a runner left the active registry.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// User called `/stop` or the supervisor stopped it (graceful path).
    GracefulStop,
    /// `reap_stale_test_runners` removed it during a periodic sweep.
    Reaped,
    /// Process exited non-zero or failed a health probe unexpectedly.
    Crashed,
    /// Something removed it but we don't have a specific reason.
    #[allow(dead_code)]
    Unknown,
}

/// Snapshot of a runner captured at the moment it's removed from the registry.
#[derive(Debug, Clone, Serialize)]
pub struct StoppedRunnerSnapshot {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub exit_code: Option<i32>,
    pub exit_reason: StopReason,
    pub stopped_at: DateTime<Utc>,
    pub last_log_lines: Vec<LogEntry>,
    /// Joined panic / backtrace lines detected in stderr before the runner
    /// exited. `None` if no panic was detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panic_stack: Option<String>,
}

/// Insert a snapshot, then evict expired entries and trim to the size cap.
///
/// Eviction is inline on write: the 100-entry / 10-minute caps are small
/// enough that a linear scan per insert is fine.
pub fn insert_and_evict(
    cache: &mut HashMap<String, StoppedRunnerSnapshot>,
    snapshot: StoppedRunnerSnapshot,
) {
    cache.insert(snapshot.id.clone(), snapshot);

    // Drop entries older than the TTL.
    let now = Utc::now();
    cache.retain(|_, v| (now - v.stopped_at).num_seconds() < STOPPED_CACHE_TTL_SECS);

    // Trim to the size cap by evicting the oldest `stopped_at`.
    while cache.len() > STOPPED_CACHE_MAX_ENTRIES {
        if let Some(oldest_id) = cache
            .iter()
            .min_by_key(|(_, v)| v.stopped_at)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_id);
        } else {
            break;
        }
    }
}

/// Build a snapshot from a managed runner's current state. Logs are tailed to
/// [`STOPPED_CACHE_LOG_CAP`].
pub async fn snapshot_from_managed(
    managed: &crate::state::ManagedRunner,
    exit_code: Option<i32>,
    exit_reason: StopReason,
) -> StoppedRunnerSnapshot {
    let all = managed.logs.history().await;
    let start = all.len().saturating_sub(STOPPED_CACHE_LOG_CAP);
    let last_log_lines = all[start..].to_vec();
    let panic_stack = managed.logs.panic_buffer().snapshot_joined();
    StoppedRunnerSnapshot {
        id: managed.config.id.clone(),
        name: managed.config.name.clone(),
        port: managed.config.port,
        exit_code,
        exit_reason,
        stopped_at: Utc::now(),
        last_log_lines,
        panic_stack,
    }
}
