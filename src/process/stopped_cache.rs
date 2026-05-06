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
use std::path::PathBuf;

use crate::log_capture::LogEntry;
use crate::process::panic_log::RecentPanic;

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
    /// Structured startup-panic record parsed from the runner's
    /// `runner-panic.log`. Populated when `monitor_runner_process_exit`
    /// detected a panic file within the freshness window; `None` if the
    /// runner exited cleanly or the file was missing/stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_panic: Option<RecentPanic>,
    /// Path to the per-spawn early-death log file captured during this
    /// runner's lifetime, when one was opened. Survives the runner being
    /// removed from the active registry so post-mortem callers (e.g.
    /// `GET /runners/{id}/early-log`) can still read the on-disk file.
    /// `None` when the runner had no early-log capture (primary runners,
    /// runners imported into the registry without a managed start, or
    /// when the supervisor failed to open the file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub early_log_path: Option<PathBuf>,
    /// Time the runner was last observed to start, captured from
    /// `managed.runner.started_at` at snapshot time. Combined with
    /// `stopped_at` to compute `duration_alive_ms` in
    /// `GET /runners/{id}/crash-summary`. `None` when the runner never
    /// started (e.g. spawn failed before the process came up).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Filesystem directory where the runner was told to write its
    /// `runner-last-panic.txt` (via `QONTINUI_RUNNER_LOG_DIR`). Surfaced via
    /// `GET /runners/{id}/crash-summary` so the panic excerpt can be read
    /// post-mortem. `None` when no panic dir was configured for this
    /// runner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panic_log_dir: Option<PathBuf>,
}

/// Insert a snapshot, then evict expired entries and trim to the size cap.
///
/// Eviction is inline on write: a linear scan per insert is fine for the
/// configured cap. The cap and TTL come from the env-overridable accessors
/// [`crate::config::stopped_cache_max_entries`] and
/// [`crate::config::stopped_cache_ttl_secs`] (defaults: 1000 entries /
/// 60-min TTL; see those functions for env vars and clamps).
pub fn insert_and_evict(
    cache: &mut HashMap<String, StoppedRunnerSnapshot>,
    snapshot: StoppedRunnerSnapshot,
) {
    let max_entries = crate::config::stopped_cache_max_entries();
    let ttl_secs = crate::config::stopped_cache_ttl_secs();
    insert_and_evict_with_bounds(cache, snapshot, max_entries, ttl_secs);
}

/// Inner form of [`insert_and_evict`] with explicit bounds. Exists so unit
/// tests can drive eviction logic without going through the
/// `OnceLock`-cached accessors in `crate::config`.
fn insert_and_evict_with_bounds(
    cache: &mut HashMap<String, StoppedRunnerSnapshot>,
    snapshot: StoppedRunnerSnapshot,
    max_entries: usize,
    ttl_secs: i64,
) {
    cache.insert(snapshot.id.clone(), snapshot);

    // Drop entries older than the TTL.
    let now = Utc::now();
    cache.retain(|_, v| (now - v.stopped_at).num_seconds() < ttl_secs);

    // Trim to the size cap by evicting the oldest `stopped_at`.
    while cache.len() > max_entries {
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
    let recent_panic = managed.recent_panic.read().await.clone();
    let early_log_path = managed.early_log_path.read().await.clone();
    let panic_log_dir = managed.panic_log_dir.read().await.clone();
    let started_at = managed.runner.read().await.started_at;
    StoppedRunnerSnapshot {
        id: managed.config.id.clone(),
        name: managed.config.name.clone(),
        port: managed.config.port,
        exit_code,
        exit_reason,
        stopped_at: Utc::now(),
        last_log_lines,
        panic_stack,
        recent_panic,
        early_log_path,
        started_at,
        panic_log_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// Fixture builder so tests don't have to spell out every Option field.
    fn fixture_snapshot(id: &str, stopped_at: DateTime<Utc>) -> StoppedRunnerSnapshot {
        StoppedRunnerSnapshot {
            id: id.to_string(),
            name: id.to_string(),
            port: 9876,
            exit_code: Some(0),
            exit_reason: StopReason::GracefulStop,
            stopped_at,
            last_log_lines: Vec::new(),
            panic_stack: None,
            recent_panic: None,
            early_log_path: None,
            started_at: None,
            panic_log_dir: None,
        }
    }

    #[test]
    fn insert_and_evict_with_bounds_trims_to_cap() {
        let mut cache: HashMap<String, StoppedRunnerSnapshot> = HashMap::new();
        let now = Utc::now();
        let cap = 3usize;
        let ttl = 86_400i64; // huge TTL so cap is the only thing evicting

        // Insert 5 entries with monotonically increasing stopped_at so the
        // "oldest" eviction order is deterministic.
        for i in 0..5 {
            let stopped_at = now + Duration::seconds(i as i64);
            let snap = fixture_snapshot(&format!("runner-{}", i), stopped_at);
            insert_and_evict_with_bounds(&mut cache, snap, cap, ttl);
        }

        assert_eq!(cache.len(), cap, "cache should be trimmed to cap");
        // The two oldest (runner-0, runner-1) should have been evicted.
        assert!(!cache.contains_key("runner-0"));
        assert!(!cache.contains_key("runner-1"));
        // The three newest survive.
        assert!(cache.contains_key("runner-2"));
        assert!(cache.contains_key("runner-3"));
        assert!(cache.contains_key("runner-4"));
    }

    #[test]
    fn insert_and_evict_with_bounds_drops_expired_entries() {
        let mut cache: HashMap<String, StoppedRunnerSnapshot> = HashMap::new();
        let now = Utc::now();
        let cap = 100usize;
        let ttl = 60i64; // 1 minute

        // Stale entry: stopped 2 minutes ago, well past the 1-minute TTL.
        let stale = fixture_snapshot("stale", now - Duration::seconds(120));
        insert_and_evict_with_bounds(&mut cache, stale, cap, ttl);
        // After the first insert, the stale entry was just inserted then
        // immediately retained; (now - stopped_at) = 120 >= 60 so it should
        // be evicted on the same call.
        assert!(
            !cache.contains_key("stale"),
            "stale entry older than TTL should be evicted on insert"
        );

        // Now insert a fresh one and another stale one to confirm the fresh
        // one survives while a newly-inserted stale entry also gets pruned.
        let fresh = fixture_snapshot("fresh", now);
        insert_and_evict_with_bounds(&mut cache, fresh, cap, ttl);
        assert!(cache.contains_key("fresh"));

        let stale2 = fixture_snapshot("stale2", now - Duration::seconds(120));
        insert_and_evict_with_bounds(&mut cache, stale2, cap, ttl);
        assert!(!cache.contains_key("stale2"));
        assert!(
            cache.contains_key("fresh"),
            "fresh entry should survive subsequent insertions"
        );
    }

    #[test]
    fn insert_and_evict_with_bounds_replaces_same_id() {
        // Reinserting the same id should overwrite (HashMap semantics) and
        // keep cache size at 1.
        let mut cache: HashMap<String, StoppedRunnerSnapshot> = HashMap::new();
        let now = Utc::now();
        let snap1 = fixture_snapshot("dup", now);
        insert_and_evict_with_bounds(&mut cache, snap1, 100, 86_400);
        let mut snap2 = fixture_snapshot("dup", now + Duration::seconds(1));
        snap2.port = 1234;
        insert_and_evict_with_bounds(&mut cache, snap2, 100, 86_400);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("dup").unwrap().port, 1234);
    }

    // --- parse_clamped_* helper tests ---
    //
    // Each test uses a unique env-var name so concurrent test runners don't
    // clobber each other (no `serial_test` dep available in this crate).

    #[test]
    fn parse_clamped_usize_returns_default_when_unset() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_USIZE_UNSET";
        std::env::remove_var(var);
        let got = crate::config::parse_clamped_usize(var, 1000, 100, 100_000);
        assert_eq!(got, 1000);
    }

    #[test]
    fn parse_clamped_usize_parses_valid_value() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_USIZE_VALID";
        std::env::set_var(var, "5000");
        let got = crate::config::parse_clamped_usize(var, 1000, 100, 100_000);
        std::env::remove_var(var);
        assert_eq!(got, 5000);
    }

    #[test]
    fn parse_clamped_usize_clamps_below_min() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_USIZE_LOW";
        std::env::set_var(var, "5");
        let got = crate::config::parse_clamped_usize(var, 1000, 100, 100_000);
        std::env::remove_var(var);
        assert_eq!(got, 100);
    }

    #[test]
    fn parse_clamped_usize_clamps_above_max() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_USIZE_HIGH";
        std::env::set_var(var, "999999999");
        let got = crate::config::parse_clamped_usize(var, 1000, 100, 100_000);
        std::env::remove_var(var);
        assert_eq!(got, 100_000);
    }

    #[test]
    fn parse_clamped_usize_falls_back_on_garbage() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_USIZE_BAD";
        std::env::set_var(var, "not-a-number");
        let got = crate::config::parse_clamped_usize(var, 1000, 100, 100_000);
        std::env::remove_var(var);
        assert_eq!(got, 1000);
    }

    #[test]
    fn parse_clamped_i64_returns_default_when_unset() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_I64_UNSET";
        std::env::remove_var(var);
        let got = crate::config::parse_clamped_i64(var, 3600, 60, 86_400);
        assert_eq!(got, 3600);
    }

    #[test]
    fn parse_clamped_i64_parses_valid_value() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_I64_VALID";
        std::env::set_var(var, "1800");
        let got = crate::config::parse_clamped_i64(var, 3600, 60, 86_400);
        std::env::remove_var(var);
        assert_eq!(got, 1800);
    }

    #[test]
    fn parse_clamped_i64_clamps_below_min() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_I64_LOW";
        std::env::set_var(var, "1");
        let got = crate::config::parse_clamped_i64(var, 3600, 60, 86_400);
        std::env::remove_var(var);
        assert_eq!(got, 60);
    }

    #[test]
    fn parse_clamped_i64_clamps_above_max() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_I64_HIGH";
        std::env::set_var(var, "999999");
        let got = crate::config::parse_clamped_i64(var, 3600, 60, 86_400);
        std::env::remove_var(var);
        assert_eq!(got, 86_400);
    }

    #[test]
    fn parse_clamped_i64_falls_back_on_garbage() {
        let var = "QONTINUI_TEST_PARSE_CLAMP_I64_BAD";
        std::env::set_var(var, "abc");
        let got = crate::config::parse_clamped_i64(var, 3600, 60, 86_400);
        std::env::remove_var(var);
        assert_eq!(got, 3600);
    }
}
