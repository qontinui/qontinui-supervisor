use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::config::{
    TargetDirSource, RUNNER_GRACEFUL_STOP_REQUEST_TIMEOUT_MS, RUNNER_GRACEFUL_STOP_TIMEOUT_MS,
};
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::claude_env::StripInheritedClaudeMarkers;
use crate::process::env_forwarders;
use crate::process::instance_config_dir;
use crate::process::port::wait_for_port_free;
use crate::process::stop_ledger::{
    resolve_stop_target, verify_target_image, PidSource, StopLedger, StopStrategy, StopTarget,
    TargetVerification as StopVerification,
};
#[cfg(target_os = "windows")]
use crate::process::windows::{
    remove_instance_config_dir, remove_runner_app_data_dirs, remove_webview2_user_data_folder,
    webview2_user_data_folder,
};
use crate::state::{ManagedRunner, SharedState};

// =============================================================================
// Runner Category Helpers
// =============================================================================

/// Classify a runner from its supervisor-assigned id.
///
/// Single source of truth for the prefix scheme — see
/// [`qontinui_types::wire::runner_kind::RunnerKind`] for the full mapping
/// and `routes::runners` for where the ids are constructed.
///
/// Note: this drops back to [`RunnerKind::from_id`] verbatim and exists
/// primarily to give callers a stable supervisor-side import path. For
/// classification that needs the user-friendly display name, prefer
/// [`RunnerConfig::kind`] which can mirror it from `RunnerConfig.name`.
pub fn runner_kind(runner_id: &str) -> qontinui_types::wire::runner_kind::RunnerKind {
    qontinui_types::wire::runner_kind::RunnerKind::from_id(runner_id)
}

/// Returns true if this runner is a temp/test runner managed by the supervisor.
/// Only temp runners can be started, stopped, or restarted by the supervisor.
/// All other runners (primary, user-opened) are observe-only.
///
/// Thin wrapper over [`runner_kind`] — kept as a standalone helper because
/// the boolean form is the most common predicate in the supervisor and
/// avoids a `match` ceremony at every call site. Migrating call sites to
/// `match runner_kind(id) { RunnerKind::Temp { .. } => ... }` is a
/// follow-up; out of scope for Item 2.
pub fn is_temp_runner(runner_id: &str) -> bool {
    runner_kind(runner_id).is_temp()
}

/// Has this runner outlived the max-age bound, and is it a kind the bound is
/// allowed to apply to?
///
/// The one sweep rule in the supervisor that kills a **healthy** process, so
/// its scope is written as an explicit `kind.is_temp()` **allowlist**, never as
/// `!kind.is_primary()` — exactly the shape (and for exactly the reason) of
/// [`crate::process::job::should_assign_to_ephemeral_job`]: `RunnerKind` is
/// `#[non_exhaustive]`, so a variant added upstream later must default to *not
/// reaped*, and getting that inverted was the 2026-07-27 incident. Primary,
/// named, and external runners are user-owned and are **never** age-bounded,
/// whatever `max_age` says.
///
/// `max_age: None` disables the bound entirely (see
/// [`crate::config::temp_runner_max_age`]).
///
/// How long has this temp runner been *running*, and by which clock?
///
/// **`ManagedRunner::created_at` is the wrong clock on its own.** It starts when
/// `spawn_test` reserves the placeholder — BEFORE the build. A cold
/// `spawn-test {rebuild:true}` is 40-50 min of `npm run build` + `cargo build`
/// (measured; `DEFAULT_BUILD_TIMEOUT_SECS` is 5400s for exactly that reason),
/// so charging build time to the runner's lifetime would reap a runner moments
/// after it finally bound its port, and the log would claim "alive 2700s" for a
/// process alive 90 seconds.
///
/// So prefer `RunnerState::started_at`, which `start_exe_mode_for_runner` sets
/// when the child actually starts (and `orphan_scan` sets on adoption). Fall
/// back to time-since-first-seen only when it is absent — a runner with
/// `running=true` and no `started_at` is an unexpected state, and the fallback
/// is conservative in the safe direction only insofar as it can never *under*
/// report.
///
/// A `started_at` in the FUTURE (clock skew, NTP step) yields a negative delta;
/// `to_std()` fails on it and we fall back rather than panic or wrap.
///
/// Returns the age plus a short human label naming which clock produced it, so
/// the kill log says which one it used instead of leaving the operator to guess.
/// Pure — every input injected — so it is unit-testable without a registry or a
/// real clock.
pub fn resolve_temp_runner_age(
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    since_first_seen: Duration,
) -> (Duration, &'static str) {
    match started_at.map(|s| now.signed_duration_since(s)) {
        Some(delta) => match delta.to_std() {
            Ok(d) => (d, "since process start"),
            // Negative delta — `started_at` is in the future. Clock skew, not a
            // real lifetime; do not trust it.
            Err(_) => (
                since_first_seen,
                "since first seen (started_at is in the future)",
            ),
        },
        None => (since_first_seen, "since first seen (no started_at)"),
    }
}

/// Pure — every input injected — so the policy is unit-testable without a
/// registry, a clock, or the env.
pub fn exceeds_temp_runner_max_age(
    kind: &qontinui_types::wire::runner_kind::RunnerKind,
    age: Duration,
    max_age: Option<Duration>,
) -> bool {
    if !kind.is_temp() {
        return false;
    }
    match max_age {
        Some(max) => age >= max,
        None => false,
    }
}

/// Decide the `QONTINUI_API_URL` value to set on a spawned runner child.
///
/// Policy (plan 2026-07-08-runner-relay-honor-persisted-backend-url):
///   - An explicit supervisor `QONTINUI_API_URL` (`explicit_env`) is forwarded
///     to EVERY runner — highest precedence in the runner's `get_api_base_url`
///     (operator / CI / local-E2E override; unchanged behavior).
///   - With no explicit env: **secondaries** (temp/named) are pinned to the
///     local backend so a shared prod `settings.json` can never route a local
///     build-test runner off-box; the **primary** is left UNSET (`None`) so the
///     runner's `get_api_base_url` falls through to the persisted paired backend
///     (`web_integration.backend_url`) — the backend the user signed into and
///     where its device JWT is verifiable.
///
/// Returns `Some(url)` to set the var, `None` to leave it unset.
fn resolve_child_api_url(explicit_env: Option<String>, is_primary: bool) -> Option<String> {
    match explicit_env {
        Some(url) => Some(url),
        None if is_primary => None,
        None => Some("http://127.0.0.1:8000".to_string()),
    }
}

/// The primary (user-started) runner's default secure-storage directory —
/// `dirs::data_local_dir()/com.qontinui.runner`.
///
/// This mirrors `SecureStorage::new()`'s fallback in qontinui-runner
/// (`src-tauri/src/secure_storage.rs`): the primary runs with no
/// `QONTINUI_SECURE_STORAGE_DIR` override, so its encrypted `auth_tokens.enc`
/// (holding the device machine key, `dmk_`) lives here. Non-primary spawns are
/// pointed at this dir via `QONTINUI_PRIMARY_SECURE_STORAGE_DIR` (a *path
/// pointer*, never the raw credential) so they can seed the primary's `dmk_`
/// into their own isolated store and reach Tier 2 headlessly (plan
/// `2026-07-13-runner-web-nav-and-workflows-auth-remediation`, item R4). A
/// spawned runner running as the same OS user on the same machine derives the
/// identical `SecureStorage` AES key (hostname + service name + salt + username,
/// no path/instance component), so it can decrypt the primary's store directly.
///
/// Returns `None` only when the platform data-local dir can't be resolved.
fn primary_secure_storage_dir() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|d| d.join("com.qontinui.runner"))
}

/// Binary metadata for diagnostics — lets callers detect stale binaries.
#[derive(Clone, serde::Serialize)]
pub struct BinaryMeta {
    pub binary_mtime: String,
    pub binary_size_bytes: u64,
    /// Wall-clock seconds since the file was last modified, computed at the
    /// time `binary_meta` ran. Saturates at 0 if mtime is in the future
    /// (clock skew).
    pub binary_age_secs: u64,
}

/// Read mtime + size of a binary file.
pub fn binary_meta(path: &std::path::Path) -> Option<BinaryMeta> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = mtime.into();
    let mtime_str = dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let now = chrono::Utc::now();
    let age_secs = (now - dt).num_seconds().max(0) as u64;
    Some(BinaryMeta {
        binary_mtime: mtime_str,
        binary_size_bytes: meta.len(),
        binary_age_secs: age_secs,
    })
}

// =============================================================================
// Stale-binary detection (Phase 2c — Item 9)
// =============================================================================

/// Minimum `slot_mtime - running_mtime` gap (in seconds) before we surface a
/// `stale_binary` entry. Tuned to absorb filesystem mtime resolution jitter
/// and near-simultaneous builds that racily complete around a running-runner
/// start. Anything finer than ~30s is not actionable ("rebuild now to pick it
/// up") because a user-issued restart at t=0 routinely reads a slot binary
/// stamped t+2s from the same cargo invocation. 30s keeps the badge meaningful.
pub const STALE_BINARY_THRESHOLD_SECS: i64 = 30;

/// Per-runner "newer build available" summary surfaced on `/runners` and
/// `/runners/{id}/logs`. `None` is the normal case (running binary is newer
/// than or equal to the newest slot, within the 30s jitter threshold).
#[derive(Clone, serde::Serialize)]
pub struct StaleBinary {
    /// Unix millis of the copy the supervisor made at start time
    /// (`target/debug/qontinui-runner-<id>.exe`).
    pub running_mtime_ms: i64,
    /// Unix millis of the newest `target-pool/slot-*/debug/qontinui-runner.exe`.
    pub slot_mtime_ms: i64,
    /// Which slot holds the newer build.
    pub slot_id: u8,
    /// `slot_mtime - running_mtime` in whole seconds. Always positive when
    /// surfaced — the field is `None` when the running binary is newer.
    pub age_delta_secs: i64,
}

/// Stat the supervisor's per-runner exe copy and return its mtime.
///
/// Returns `None` when the copy does not exist yet (runner never started under
/// this supervisor, or the path resolver failed to copy). The live path is
/// determined by [`crate::config::SupervisorConfig::runner_exe_copy_path`] —
/// pool-named for `Temp`/`Named`, id-named for `Primary`/`External`.
pub fn running_binary_mtime(
    state: &SharedState,
    config: &crate::config::RunnerConfig,
) -> Option<std::time::SystemTime> {
    let path = state.config.runner_exe_copy_path(config);
    std::fs::metadata(&path).ok()?.modified().ok()
}

/// Scan every `target-pool/slot-*/debug/qontinui-runner.exe` and return the
/// `(slot_id, mtime)` of the newest. Returns `None` when the pool has never
/// produced a binary yet.
pub async fn newest_slot_binary_mtime(state: &SharedState) -> Option<(u8, std::time::SystemTime)> {
    let mut best: Option<(u8, std::time::SystemTime)> = None;
    for slot in &state.build_pool.slots {
        let path = slot
            .target_dir
            .join("debug")
            .join(crate::config::RUNNER_BIN_NAME);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let slot_id_u8: u8 = slot.id.min(u8::MAX as usize) as u8;
        best = match best {
            Some((_, current)) if current >= mtime => best,
            _ => Some((slot_id_u8, mtime)),
        };
    }
    best
}

/// Convert a `SystemTime` to unix millis, saturating at i64 bounds. Values
/// predating the epoch return a negative ms count (shouldn't happen for
/// filesystem mtimes on sane clocks, but defined for test-fixture ergonomics).
fn system_time_to_unix_millis(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// Compute a `StaleBinary` record from the raw mtimes. Pure function — the
/// actual `SystemTime` lookups live in `running_binary_mtime` /
/// `newest_slot_binary_mtime` so this is trivially testable.
///
/// Returns `Some` only when the newest slot binary is strictly newer than the
/// running copy by more than `STALE_BINARY_THRESHOLD_SECS`. Equal or
/// within-threshold deltas yield `None` (normal state — restart is a no-op
/// from a binary-freshness perspective).
pub fn compute_stale_binary(
    running: Option<std::time::SystemTime>,
    newest_slot: Option<(u8, std::time::SystemTime)>,
) -> Option<StaleBinary> {
    let running = running?;
    let (slot_id, slot_mtime) = newest_slot?;
    // Compute the delta in whole seconds. `duration_since` errors when the
    // left side predates the right (i.e. running > slot) — that's the "not
    // stale" case. Ignore it and return `None`.
    let delta_secs = slot_mtime.duration_since(running).ok()?.as_secs() as i64;
    if delta_secs <= STALE_BINARY_THRESHOLD_SECS {
        return None;
    }
    Some(StaleBinary {
        running_mtime_ms: system_time_to_unix_millis(running),
        slot_mtime_ms: system_time_to_unix_millis(slot_mtime),
        slot_id,
        age_delta_secs: delta_secs,
    })
}

/// Convenience wrapper: look up the runner's running copy + newest slot and
/// call `compute_stale_binary`. Returns `None` on any I/O miss — callers
/// treat the field as strictly informational.
pub async fn stale_binary_for_runner(
    state: &SharedState,
    config: &crate::config::RunnerConfig,
) -> Option<StaleBinary> {
    let running = running_binary_mtime(state, config);
    let newest_slot = newest_slot_binary_mtime(state).await;
    compute_stale_binary(running, newest_slot)
}

/// Resolve the last-known-good runner exe path.
///
/// Returns the path only when both the on-disk LKG exe AND in-memory
/// `LkgInfo` are present — callers that pin a runner to LKG need the
/// metadata (notably `built_at`) to make their staleness decision, so a
/// dangling exe with no sidecar is treated as absent.
///
/// If the on-disk exe has gone missing while the in-memory `LkgInfo` is
/// still populated (e.g. the user wiped `target-pool/lkg/` between builds,
/// or a subsequent rename never landed), the stale `LkgInfo` is cleared
/// before returning the error so `/health.build.lkg` no longer reports
/// metadata for an exe that doesn't exist.
pub async fn resolve_lkg_exe(state: &SharedState) -> Result<std::path::PathBuf, SupervisorError> {
    let info_present = state.build_pool.last_known_good.read().await.is_some();
    if !info_present {
        return Err(SupervisorError::Process(
            "No last-known-good runner binary recorded yet. Run a build that succeeds first."
                .to_string(),
        ));
    }
    let p = state.config.lkg_exe_path();
    if !p.exists() {
        // Drop the stale in-memory entry so /health and /builds stop
        // reporting metadata for an exe that's no longer on disk.
        let mut guard = state.build_pool.last_known_good.write().await;
        *guard = None;
        return Err(SupervisorError::Process(format!(
            "LKG metadata is set but exe is missing at {:?}. The LKG dir may have been wiped; rebuild to repopulate.",
            p
        )));
    }
    Ok(p)
}

/// Filename of the sidecar that records the provenance of a slot's exe — the
/// git SHA of the tree it was actually built from, whether that tree was the
/// live runner working tree or a `build_dir_override` (spawn-test) tree, the
/// absolute dir built, and the build timestamp. Written by
/// `build_monitor::run_cargo_build_with_dir` after a successful build; read by
/// [`resolve_source_exe`] and `GET /builds` to detect cross-slot drift.
///
/// This replaces the legacy plain-SHA sidecar (`qontinui-runner.exe.git_sha`),
/// which only ever recorded the live tree's HEAD and therefore lied when an
/// override tree was built. Legacy files are ignored (read as absent) and
/// self-heal on the next build.
#[cfg(windows)]
pub const SLOT_PROVENANCE_SIDECAR_FILENAME: &str = "qontinui-runner.exe.provenance.json";
#[cfg(not(windows))]
pub const SLOT_PROVENANCE_SIDECAR_FILENAME: &str = "qontinui-runner.provenance.json";

/// Which source tree a slot's exe was built from.
///
/// Three-way by design:
/// - `live_tree` — cargo's `current_dir` was the live runner working tree
///   (`build_dir_override == None`); the contested working checkout.
/// - `origin_main` — cargo's `current_dir` was a supervisor-materialized
///   `origin/main` worktree (the default primary rebuild path, Phase B). This
///   is canonical merged truth: it is LKG-eligible and may start a non-temp
///   runner, exactly like a live-tree build.
/// - `override` — a foreign `build_dir_override` tree the supervisor does NOT
///   vouch for (a spawn-test `git_ref` / `worktree_path` tree). It is excluded
///   from LKG promotion and refused for non-temp starts.
///
/// The forensic detail of *which* tree is carried by
/// [`BuildProvenance::built_from`]. This is deliberately NOT the response
/// layer's three-way `source` split (`live_tree` / `worktree` / `worktree_path`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildSource {
    /// Built from the live runner working tree (`state.config.project_dir`).
    LiveTree,
    /// Built from a supervisor-materialized `origin/main` worktree (the default
    /// primary rebuild path). Canonical merged truth — LKG-eligible and
    /// startable as a non-temp runner.
    OriginMain,
    /// Built from a `build_dir_override` tree (spawn-test override path).
    #[serde(rename = "override")]
    Override,
}

impl BuildSource {
    /// Is a build from this source eligible for LKG promotion and to start a
    /// non-temp (primary/named) runner?
    ///
    /// `LiveTree` and `OriginMain` are both vouched-for trees the supervisor
    /// produced from a known checkout; `Override` is a foreign tree the
    /// supervisor does not vouch for (spawn-test `git_ref` / `worktree_path`)
    /// and is excluded. This is the single predicate behind both the LKG
    /// promotion gate ([`crate::build_monitor::update_lkg_after_success`]) and
    /// the non-temp start gate ([`start_provenance_gate`]).
    pub fn is_vouched(self) -> bool {
        match self {
            BuildSource::LiveTree | BuildSource::OriginMain => true,
            BuildSource::Override => false,
        }
    }
}

/// Provenance of a slot's freshly-built runner exe — computed once in the
/// success block of `run_cargo_build_with_dir` and written to the slot's
/// provenance sidecar. Records the tree that was *actually* built, so a
/// later reader (drift check, LKG gate, `GET /builds`) can tell whether a
/// slot's exe came from the live tree or a foreign override tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildProvenance {
    /// 40-hex git SHA of the tree that was built, or `None` when the git probe
    /// failed (git missing, not a repo, detached HEAD, etc.). Best-effort —
    /// probe failure does not fail the build.
    pub sha: Option<String>,
    /// Whether the built tree was the live runner tree or an override tree.
    pub source: BuildSource,
    /// Absolute path of the tree root that was probed/built (the live tree
    /// root, or the override worktree root). Forensic detail for the binary
    /// `source`.
    pub built_from: String,
    /// RFC3339 timestamp of when the build completed.
    pub built_at: String,
}

/// Read the provenance sidecar recording how the slot's exe was built.
/// Returns `None` if the sidecar is missing, unreadable, or unparseable
/// (including a legacy plain-SHA file, which is not valid provenance JSON).
///
/// Absence is "unknown provenance" — never an error. Slots self-heal on the
/// next successful build, which rewrites the sidecar.
pub fn read_slot_provenance(slot_target_dir: &std::path::Path) -> Option<BuildProvenance> {
    let p = slot_target_dir
        .join("debug")
        .join(SLOT_PROVENANCE_SIDECAR_FILENAME);
    let content = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str::<BuildProvenance>(&content).ok()
}

/// Convenience: the slot's recorded build SHA (`None` when no provenance
/// sidecar or its `sha` field is null). Test-only — production code reads the
/// full [`read_slot_provenance`] so it can compare `(sha, source)`.
#[cfg(test)]
pub fn read_slot_sha(slot_target_dir: &std::path::Path) -> Option<String> {
    read_slot_provenance(slot_target_dir).and_then(|p| p.sha)
}

/// A slot's provenance identity for drift comparison: its recorded build SHA
/// (`None` when unknown) and the source tree it was built from (`None` when no
/// provenance sidecar at all). Two slots "drift" when these pairs differ.
pub type SlotProvenanceKey = (Option<String>, Option<BuildSource>);

/// Structured warning produced when [`resolve_source_exe`] picks a slot whose
/// `(sha, source)` provenance differs from at least one other slot's. Pure
/// data — emitted to logs and `/builds`; never alters resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotShaDrift {
    pub picked_slot_id: usize,
    pub picked_sha: String,
    pub picked_source: BuildSource,
    /// Other slots whose `(sha, source)` differs from the picked slot's. Sorted
    /// by slot id for deterministic output. The string is the conflicting SHA
    /// ("(none)" when that slot has no SHA) for the human-readable warning.
    pub conflicting: Vec<(usize, String, Option<BuildSource>)>,
}

/// Compute provenance drift across the build pool. Returns `Some` only when
/// both:
/// - the picked slot has a provenance sidecar (a `source`), AND
/// - at least one other slot has a provenance sidecar whose `(sha, source)`
///   pair differs from the picked one.
///
/// Slots without a provenance sidecar are treated as unknown — they don't
/// trigger drift. A slot built from an *override* tree at the same SHA as a
/// live-tree slot still drifts, because the bytes came from a different tree.
pub fn detect_slot_sha_drift(
    picked_slot_id: usize,
    picked: &SlotProvenanceKey,
    all_slots: &[(usize, SlotProvenanceKey)],
) -> Option<SlotShaDrift> {
    let picked_source = picked.1?;
    let picked_sha = picked.0.clone();
    let mut conflicting: Vec<(usize, String, Option<BuildSource>)> = all_slots
        .iter()
        .filter_map(|(id, key)| {
            if *id == picked_slot_id {
                return None;
            }
            // Only slots that have provenance participate.
            key.1?;
            if *key == (picked_sha.clone(), Some(picked_source)) {
                return None;
            }
            let sha_str = key.0.clone().unwrap_or_else(|| "(none)".to_string());
            Some((*id, sha_str, key.1))
        })
        .collect();
    if conflicting.is_empty() {
        return None;
    }
    conflicting.sort_by_key(|(id, _, _)| *id);
    Some(SlotShaDrift {
        picked_slot_id,
        picked_sha: picked_sha.unwrap_or_else(|| "(none)".to_string()),
        picked_source,
        conflicting,
    })
}

fn sha_short(s: &str) -> &str {
    let cut = s.char_indices().nth(12).map(|(i, _)| i).unwrap_or(s.len());
    &s[..cut]
}

/// Stable machine-readable label for a [`BuildSource`], mirrored in logs, in
/// refusal messages, and in the `GET /builds` / `GET /runners` JSON. `pub` so
/// the route layer renders the same strings instead of keeping its own copies —
/// two spellings of one vocabulary is how a consumer's match arm silently stops
/// matching.
pub fn source_label(src: BuildSource) -> &'static str {
    match src {
        BuildSource::LiveTree => "live_tree",
        BuildSource::OriginMain => "origin_main",
        BuildSource::Override => "override",
    }
}

/// Format a [`SlotShaDrift`] as a human-readable warning line.
pub fn format_drift_warning(d: &SlotShaDrift) -> String {
    let others: Vec<String> = d
        .conflicting
        .iter()
        .map(|(id, sha, src)| {
            let src_label = src.map(source_label).unwrap_or("unknown");
            format!("slot {} (sha {}, source {})", id, sha_short(sha), src_label)
        })
        .collect();
    let plural = if d.conflicting.len() > 1 { "s" } else { "" };
    format!(
        "resolve_source_exe: picked slot {} (sha {}, source {}) but {} carries distinct \
         provenance{}. If newer, spawn-test {{rebuild:false}} will return a stale or \
         foreign binary. Stage fresh exe into slot {} or set last_successful_slot. See \
         proj_supervisor_slot_resolution_order.",
        d.picked_slot_id,
        sha_short(&d.picked_sha),
        source_label(d.picked_source),
        others.join(", "),
        plural,
        d.picked_slot_id,
    )
}

/// Outcome of [`start_provenance_gate`] when the start is allowed but the
/// supervisor has no positive evidence the slot exe is honest (pre-upgrade
/// sidecar, write failure, legacy file). Carries a human-readable warning to
/// log; the start proceeds. Refusal keys on POSITIVE evidence of wrongness
/// only — an unknown provenance must never brick a start (e.g. the first
/// watchdog auto-start after a deploy, when every pre-upgrade slot is unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartProvenanceWarning(pub String);

// NOTE: `legacy_exe_provenance_warning` used to live here — the warn-and-proceed
// text for a non-temp start off the non-pool `target/debug/` exe. It is deleted,
// not deprecated: that path no longer warns and proceeds, it refuses
// ([`unverified_exe_gate`]). Warning about a binary of unknown provenance and
// launching it anyway is exactly how a 54-day-old artifact served a healthy UI
// Bridge through a whole verification iteration.

/// Pure decision gate for a runner start, over `(temp-ness, slot provenance)`.
///
/// This is the last line of defense against the 2026-06-05 incident: a slot
/// whose exe was built from a foreign override tree (`source == override`) must
/// never be deployed to a NON-temp runner (the operator's primary, a named
/// runner, or the watchdog boot auto-start). Phase 1 gave slots an honest
/// provenance sidecar; this gate refuses to start a non-temp runner from a slot
/// that positively says `override`.
///
/// Decision matrix (`is_temp`, `slot_provenance`):
/// - **temp** → always `Ok(None)`. Temp runners (`test-*`) exist to run foreign
///   refs; their spawn responses already carry full provenance, so the operator
///   sees exactly what they asked for. Never gated.
/// - **non-temp + `Some(source == Override)`** → `Err` naming the slot, the
///   provenance (`built_from` + `sha`), and the recovery
///   (`POST /runner/fix-and-rebuild`, then start). Positive evidence of a
///   foreign exe — refuse. `Override` is the ONLY refused source.
/// - **non-temp + `None`** (no sidecar / unreadable — pre-upgrade slot, write
///   failure, legacy file) → `Ok(Some(warning))`. Warn-and-proceed: absence is
///   "unknown", not "wrong". Degrades to pre-Phase-3 behavior.
/// - **non-temp + `Some(source == LiveTree | OriginMain)`** → `Ok(None)`,
///   regardless of whether `sha == HEAD`. Both are vouched-for trees the
///   supervisor produced (`BuildSource::is_vouched`). `OriginMain` is the
///   default primary rebuild path (Phase B) — canonical merged truth, so it
///   MUST be allowed to start as a non-temp runner; folding it into the
///   `Override` refusal would brick every primary start. Main advancing between
///   build and start is staleness, NOT a provenance lie; existing slot-drift /
///   `target/debug` staleness warnings already cover it. We deliberately do NOT
///   gate on sha.
///
/// Pure (no I/O / no state) so it is unit-testable without a live `SharedState`,
/// mirroring [`provenance_rebuild_guard`].
pub fn start_provenance_gate(
    is_temp: bool,
    slot_id: usize,
    slot_provenance: Option<&BuildProvenance>,
) -> Result<Option<StartProvenanceWarning>, SupervisorError> {
    // Temp runners are always permissive — they are the sanctioned vehicle for
    // running foreign refs, and their spawn responses surface full provenance.
    if is_temp {
        return Ok(None);
    }

    match slot_provenance {
        Some(prov) if prov.source == BuildSource::Override => {
            let sha = prov.sha.as_deref().unwrap_or("(unknown)");
            Err(SupervisorError::Process(format!(
                "Refusing to start non-temp runner from slot {slot_id}: its exe was built \
                 from a foreign override tree (source=override, built_from={}, sha={sha}), \
                 not the live runner tree. Deploying it would put unverified branch code on \
                 a managed runner. Recovery: POST /runner/fix-and-rebuild to rebuild the \
                 live tree into the slots, then start. (Temp runners via spawn-test may run \
                 foreign refs; non-temp runners may not.)",
                prov.built_from,
            )))
        }
        // Positive vouched-tree evidence (LiveTree or OriginMain) — allow.
        // sha-vs-HEAD staleness is covered by the existing drift warnings, not
        // this gate. Only `Override` (handled above) is refused.
        Some(_) => Ok(None),
        // No positive evidence either way — warn and proceed (pre-upgrade
        // sidecar, write failure, legacy file). Never brick a start on unknown.
        None => Ok(Some(StartProvenanceWarning(format!(
            "Starting non-temp runner from slot {slot_id} with UNKNOWN provenance \
             (no readable provenance sidecar — likely a pre-upgrade build, a sidecar \
             write failure, or a legacy file). Proceeding: refusal keys on positive \
             evidence of a foreign exe only. The slot self-heals on the next successful \
             build, which rewrites the sidecar."
        )))),
    }
}

/// Pure decision: which slot would [`resolve_source_exe`] pick, given the
/// recorded `last_successful_slot` and a list of `(slot_id, exe_path)` pairs?
///
/// Preference order (unchanged from before sidecar instrumentation):
/// 1. `last_successful_slot` if its exe exists.
/// 2. First slot in iteration order whose exe exists.
/// 3. `None` (caller's legacy fallback applies).
///
/// `exists` is injected so tests can drive the decision without touching the
/// filesystem.
pub fn pick_slot_decision<F: Fn(&std::path::Path) -> bool>(
    last_successful_slot: Option<usize>,
    slots: &[(usize, std::path::PathBuf)],
    exists: F,
) -> Option<usize> {
    if let Some(id) = last_successful_slot {
        if let Some((_, p)) = slots.iter().find(|(sid, _)| *sid == id) {
            if exists(p) {
                return Some(id);
            }
        }
    }
    for (id, p) in slots {
        if exists(p) {
            return Some(*id);
        }
    }
    None
}

/// Determine which slot id [`resolve_source_exe`] would pick right now,
/// applying the same preference order without the legacy fallback.
///
/// Returns `None` when no slot has an exe on disk (legacy fallback applies).
pub async fn pick_slot_for_resolution(state: &SharedState) -> Option<usize> {
    let last = *state.build_pool.last_successful_slot.read().await;
    let slots: Vec<(usize, std::path::PathBuf)> = state
        .build_pool
        .slots
        .iter()
        .map(|s| {
            (
                s.id,
                s.target_dir
                    .join("debug")
                    .join(crate::config::RUNNER_BIN_NAME),
            )
        })
        .collect();
    pick_slot_decision(last, &slots, |p| p.exists())
}

/// Snapshot of cross-slot SHA state — what resolve_source_exe would pick now,
/// each slot's sidecar SHA (`None` when absent), and the drift warning (if any).
pub struct SlotFreshness {
    pub picked_slot_id: Option<usize>,
    /// Per-slot provenance: `(slot_id, (sha, source))`. `source` is `None` when
    /// the slot has no provenance sidecar; `sha` is `None` when the build's
    /// git probe failed. Used by `GET /builds` to surface `git_sha` + `source`.
    pub slot_provenance: Vec<(usize, SlotProvenanceKey)>,
    pub drift: Option<SlotShaDrift>,
    /// Sibling warning: a stale exe at the legacy `target/debug/` location
    /// that operators sometimes produce by running `cargo build` from the
    /// workspace root instead of into a slot. See
    /// [`detect_target_debug_staleness`] for the comparison rule;
    /// operator-facing recovery is documented in the `feedback_runner_manual_build`
    /// memory.
    pub target_debug_staleness: Option<TargetDebugStaleness>,
    /// The OPPOSITE direction, and the consequential one: the picked slot exe
    /// is older than a local `target/debug` build, i.e. the operator compiled
    /// code the supervisor may be about to ignore. Carries `adopted` — whether
    /// resolution will run the local build or the slot — so a reader can answer
    /// "is my build what runs?" without starting a runner and reading the log.
    ///
    /// The two fields are mutually exclusive by construction: `target_debug_staleness`
    /// needs the legacy exe OLDER than every slot, this one needs it NEWER than
    /// the picked slot.
    pub local_build_adoption: Option<LocalBuildAdoption>,
}

/// Compute the cross-slot SHA snapshot. Used by both `resolve_source_exe`
/// (which emits the warning) and `GET /builds` (which surfaces it as JSON).
pub async fn compute_slot_freshness(state: &SharedState) -> SlotFreshness {
    let slot_provenance: Vec<(usize, SlotProvenanceKey)> = state
        .build_pool
        .slots
        .iter()
        .map(|s| {
            let prov = read_slot_provenance(&s.target_dir);
            let key: SlotProvenanceKey = match prov {
                Some(p) => (p.sha, Some(p.source)),
                None => (None, None),
            };
            (s.id, key)
        })
        .collect();
    let picked_slot_id = pick_slot_for_resolution(state).await;
    let drift = picked_slot_id.and_then(|pid| {
        let picked = slot_provenance
            .iter()
            .find(|(id, _)| *id == pid)
            .map(|(_, k)| k.clone())
            .unwrap_or((None, None));
        detect_slot_sha_drift(pid, &picked, &slot_provenance)
    });
    let target_debug_staleness = compute_target_debug_staleness_for_state(state);
    // Same evaluator the start path runs, so `GET /builds` reports the decision
    // resolution will actually make rather than a second derivation of it.
    let local_build_adoption = picked_slot_id.and_then(|pid| {
        evaluate_local_build_adoption(
            &state.config,
            pid,
            &state.config.runner_exe_path_for_slot(pid),
        )
    });
    SlotFreshness {
        picked_slot_id,
        slot_provenance,
        drift,
        target_debug_staleness,
        local_build_adoption,
    }
}

// =============================================================================
// Legacy target/debug/ staleness detection (feedback_runner_manual_build)
// =============================================================================
//
// The supervisor's build pool writes exes into `target-pool/slot-N/debug/`.
// An operator running `cargo build` from the runner workspace root produces
// an exe at `<workspace>/target/debug/qontinui-runner.exe` — NOT in any slot.
// `resolve_source_exe` never picks from that path, so the workspace-root exe
// can sit stale indefinitely while slot exes move forward. Anyone scripting
// against `target/debug/qontinui-runner.exe` (or the operator expecting
// `spawn-test {rebuild:false}` to use it) hits silent staleness.
//
// This module surfaces the staleness as observability — same shape as the
// cross-slot SHA drift check, on the adjacent surface. It does NOT promote
// the legacy path to a resolution source.
//
// Scope note (since local-build adoption landed): the legacy path CAN now beat
// a slot, but only through `evaluate_local_build_adoption`, which requires the
// legacy exe to be NEWER than the picked slot. That is the exact negation of
// this module's precondition, so the two can never both fire and nothing here
// promotes anything. The promoting direction lives in `PoolBehindLocalBuild` /
// `local_build_is_adoptable`.

/// Structured warning produced when the legacy `target/debug/qontinui-runner.exe`
/// is older than every build-pool slot exe. Pure data — emitted to logs and
/// `/builds`; never alters resolution.
///
/// "Never alters resolution" is a claim about THIS finding, not about the
/// legacy path in general: a legacy exe NEWER than the picked slot can be
/// adopted over it ([`local_build_is_adoptable`]), and that condition is the
/// negation of this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetDebugStaleness {
    /// Absolute path to the legacy exe (`<workspace>/target/debug/qontinui-runner.exe`).
    pub legacy_path: std::path::PathBuf,
    /// mtime of the legacy exe.
    pub legacy_mtime: std::time::SystemTime,
    /// The oldest mtime across slots that have an exe on disk. Surface this
    /// (not the newest) so the operator knows the staleness gap reaches even
    /// the laggard slot — "legacy is older than every slot".
    pub oldest_slot_mtime: std::time::SystemTime,
}

/// Pure mtime-comparison core of [`detect_target_debug_staleness`]. Separated
/// so the staleness rule can be exercised with synthetic timestamps in tests
/// without depending on filesystem mtime resolution.
///
/// Returns `Some` only when:
/// - `legacy_mtime` is `Some`, AND
/// - at least one entry in `slot_mtimes` is `Some`, AND
/// - `legacy_mtime` is strictly less than every `Some` slot mtime.
///
/// `None` entries in `slot_mtimes` (failed reads, missing exes) are silently
/// skipped — matches the per-slot scan pattern in [`newest_slot_binary_mtime`].
pub fn compute_target_debug_staleness(
    legacy_path: &std::path::Path,
    legacy_mtime: Option<std::time::SystemTime>,
    slot_mtimes: &[Option<std::time::SystemTime>],
) -> Option<TargetDebugStaleness> {
    let legacy = legacy_mtime?;
    let oldest_slot = slot_mtimes.iter().filter_map(|m| *m).min()?;
    // Strict `<` — equal mtimes are NOT stale (same build wave; jitter possible).
    if legacy < oldest_slot {
        Some(TargetDebugStaleness {
            legacy_path: legacy_path.to_path_buf(),
            legacy_mtime: legacy,
            oldest_slot_mtime: oldest_slot,
        })
    } else {
        None
    }
}

/// Detect whether a legacy `target/debug/qontinui-runner.exe` is older than
/// every slot exe in the build pool. Returns `Some` only when:
/// - the legacy file exists AND its mtime is readable, AND
/// - at least one slot exe exists with a readable mtime, AND
/// - the legacy mtime is strictly older than every readable slot mtime.
///
/// Slot paths that can't be stat'd are silently skipped (matches the pattern
/// in [`newest_slot_binary_mtime`]). If all slot reads fail, treat that as
/// "no baseline" and return `None`.
///
/// **Observability only** — this finding never alters which artifact wins.
/// The non-pool exe reaches resolution two ways and NEITHER is available while
/// this warning can fire: as preference 3, only when no slot has an exe at all
/// (and then [`unverified_exe_gate`] refuses it rather than launching it
/// silently); or by ADOPTION over a staler slot, which requires the legacy exe
/// to be NEWER than the picked slot — the negation of the condition here. So
/// whenever this fires, a slot still wins.
pub fn detect_target_debug_staleness(
    legacy_exe_path: &std::path::Path,
    slot_exe_paths: &[(usize, &std::path::Path)],
) -> Option<TargetDebugStaleness> {
    let legacy_mtime = match std::fs::metadata(legacy_exe_path).and_then(|m| m.modified()) {
        Ok(t) => Some(t),
        Err(e) => {
            debug!(
                "detect_target_debug_staleness: legacy mtime unreadable at {:?}: {} \
                 — skipping staleness check",
                legacy_exe_path, e
            );
            None
        }
    };
    let slot_mtimes: Vec<Option<std::time::SystemTime>> = slot_exe_paths
        .iter()
        .map(|(_, p)| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect();
    compute_target_debug_staleness(legacy_exe_path, legacy_mtime, &slot_mtimes)
}

/// The picked build-pool slot exe is OLDER than a local `target/debug` build —
/// i.e. the operator compiled fresh code that the supervisor is about to ignore.
///
/// This is the INVERSE of [`TargetDebugStaleness`], and it is the direction that
/// actually costs the operator a rebuild. `dev-start.ps1` runs
/// `cargo build --bin qontinui-runner --features custom-protocol` with no
/// `CARGO_TARGET_DIR`, so its artifact lands at `target/debug/` — which
/// [`resolve_source_exe_detailed`] uses only as preference 3, "when NO slot has
/// an exe at all". While any slot exe exists the local build is discarded *by
/// construction*, and the console still prints "Runner binary rebuilt".
///
/// Measured live 2026-08-29: the operator's build landed at 15:59Z; the
/// supervisor then resolved slot 0 (`61be4b001cee`, built the previous day at
/// 22:31Z) and ran that. Two operator "rebuilds" in a row silently ran
/// 17.5-hour-old code, and nothing in any log said so — only the opposite
/// direction was ever checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolBehindLocalBuild {
    pub legacy_path: std::path::PathBuf,
    pub legacy_mtime: std::time::SystemTime,
    pub picked_slot_id: usize,
    pub picked_slot_mtime: std::time::SystemTime,
}

/// Detect that the picked slot exe is strictly older than the local
/// `target/debug` exe.
///
/// Strict `>` on the legacy side — equal mtimes are the same build wave and are
/// NOT a finding (mirrors [`compute_target_debug_staleness`]). Unreadable
/// mtimes yield `None`: an unknown timestamp is never reported as a finding.
pub fn compute_pool_behind_local_build(
    legacy_path: &std::path::Path,
    legacy_mtime: Option<std::time::SystemTime>,
    picked_slot_id: usize,
    picked_slot_mtime: Option<std::time::SystemTime>,
) -> Option<PoolBehindLocalBuild> {
    let legacy = legacy_mtime?;
    let picked = picked_slot_mtime?;
    if legacy > picked {
        Some(PoolBehindLocalBuild {
            legacy_path: legacy_path.to_path_buf(),
            legacy_mtime: legacy,
            picked_slot_id,
            picked_slot_mtime: picked,
        })
    } else {
        None
    }
}

/// Is a local build ADOPTABLE — may the supervisor run it in place of the
/// staler slot exe?
///
/// Only when it carries a provenance sidecar that demonstrably describes THIS
/// file. Two independent conditions, both required:
///
/// 1. **A vouched source.** [`BuildSource::is_vouched`] — a `live_tree` or
///    `origin_main` build. An `override` tree is foreign by definition.
/// 2. **The sidecar is not stale relative to the exe.** `npm run tauri dev`
///    writes the SAME path with `--no-default-features` (no `custom-protocol`),
///    producing a binary that loads its frontend from the Vite dev server and
///    shows "refused to connect" when the supervisor launches it standalone
///    (observed 2026-08-05). Such a build writes no sidecar — but it can
///    OVERWRITE an exe whose sidecar is still lying around. Requiring the
///    sidecar to be at least as new as the exe rejects that case, so a dev-mode
///    binary can never be adopted on the strength of an earlier stamp.
///
/// Everything else is refused and merely warned about. An unidentified artifact
/// is never promoted over a slot on the strength of its mtime alone — mtime says
/// when a file was written, not what is in it.
pub fn local_build_is_adoptable(
    provenance: Option<&BuildProvenance>,
    sidecar_mtime: Option<std::time::SystemTime>,
    legacy_mtime: std::time::SystemTime,
) -> bool {
    let Some(p) = provenance else { return false };
    if !p.source.is_vouched() {
        return false;
    }
    match sidecar_mtime {
        Some(t) => t >= legacy_mtime,
        None => false,
    }
}

/// Format a [`PoolBehindLocalBuild`] for the operator, saying what will happen.
pub fn format_pool_behind_local_build_warning(s: &PoolBehindLocalBuild, adopted: bool) -> String {
    let legacy_iso: chrono::DateTime<chrono::Utc> = s.legacy_mtime.into();
    let slot_iso: chrono::DateTime<chrono::Utc> = s.picked_slot_mtime.into();
    if adopted {
        format!(
            "pool_behind_local_build: local build {} (mtime {}) is NEWER than \
             picked slot {} (mtime {}) — running the local build, because a \
             rebuild must take the latest code. It carries a vouched provenance \
             sidecar describing this exact file. Pass use_lkg:true to pin the \
             last-known-good binary instead.",
            s.legacy_path.display(),
            legacy_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            s.picked_slot_id,
            slot_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
    } else {
        format!(
            "pool_behind_local_build: local build {} (mtime {}) is NEWER than \
             picked slot {} (mtime {}), but carries no vouched provenance sidecar \
             describing this file — running the SLOT exe, so your local build is \
             NOT what is running. Rebuild through the supervisor (POST \
             /runner/restart {{\"rebuild\": true}}) to put the latest code \
             in a slot.",
            s.legacy_path.display(),
            legacy_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            s.picked_slot_id,
            slot_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )
    }
}

/// The `pool_behind_local_build` finding together with the decision resolution
/// will act on.
///
/// Computed in exactly ONE place ([`evaluate_local_build_adoption`]) so the
/// observability surface (`GET /builds`) can never disagree with what the start
/// path is about to do. Two independent copies of this decision would be a worse
/// failure than the one they report: an operator told "your build IS running" by
/// a route that re-derived it differently has no way to notice.
#[derive(Debug, Clone)]
pub struct LocalBuildAdoption {
    /// The mtime comparison: the local build is newer than the picked slot.
    pub finding: PoolBehindLocalBuild,
    /// Will resolution actually RUN the local build instead of the picked slot?
    /// `false` = the slot still wins and the operator's build is NOT running.
    pub adopted: bool,
    /// Which cargo target-dir precedence level produced the local build.
    pub target_dir_source: TargetDirSource,
    /// The provenance sidecar beside the local build. `None` is an honest
    /// UNKNOWN — and is also exactly why `adopted` is then `false`.
    pub provenance: Option<BuildProvenance>,
}

impl LocalBuildAdoption {
    /// The operator-facing line, which states WHICH binary actually runs.
    pub fn message(&self) -> String {
        format_pool_behind_local_build_warning(&self.finding, self.adopted)
    }
}

/// Is the local `target/debug` build newer than the slot resolution picked, and
/// if so may it be adopted over that slot?
///
/// `None` when there is nothing to say: no readable local build, no readable
/// slot mtime, or the slot is at least as new (the healthy steady state).
pub fn evaluate_local_build_adoption(
    config: &crate::config::SupervisorConfig,
    picked_slot_id: usize,
    picked_slot_path: &std::path::Path,
) -> Option<LocalBuildAdoption> {
    let (target_dir_source, legacy_exe) = config.runner_exe_path_resolved();
    evaluate_local_build_adoption_at(
        &legacy_exe,
        target_dir_source,
        picked_slot_id,
        picked_slot_path,
    )
}

/// Filesystem core of [`evaluate_local_build_adoption`], taking explicit paths
/// instead of reading them off `SupervisorConfig`.
///
/// Separated so the composition — finding, sidecar read, freshness rule,
/// adoption verdict — is testable against a `tempfile` tree without mutating
/// `CARGO_TARGET_DIR`, which `runner_exe_path_resolved` reads at level-1
/// precedence and which no test may set without breaking its neighbours.
pub fn evaluate_local_build_adoption_at(
    legacy_exe: &std::path::Path,
    target_dir_source: TargetDirSource,
    picked_slot_id: usize,
    picked_slot_path: &std::path::Path,
) -> Option<LocalBuildAdoption> {
    let finding = compute_pool_behind_local_build(
        legacy_exe,
        file_mtime(legacy_exe),
        picked_slot_id,
        file_mtime(picked_slot_path),
    )?;
    let provenance = read_provenance_beside_exe(legacy_exe);
    // Derive the sidecar path from the same parent the provenance read uses.
    // No `unwrap_or_else(exe)` fallback: statting the exe AS its own sidecar is
    // meaningless, and a parentless exe path has no sidecar to find — `None`
    // says exactly that.
    let sidecar_mtime = legacy_exe
        .parent()
        .map(|d| d.join(SLOT_PROVENANCE_SIDECAR_FILENAME))
        .and_then(|p| file_mtime(&p));
    let adopted =
        local_build_is_adoptable(provenance.as_ref(), sidecar_mtime, finding.legacy_mtime);
    Some(LocalBuildAdoption {
        finding,
        adopted,
        target_dir_source,
        provenance,
    })
}

/// Format a [`TargetDebugStaleness`] as a human-readable warning line.
pub fn format_target_debug_warning(s: &TargetDebugStaleness) -> String {
    let legacy_iso: chrono::DateTime<chrono::Utc> = s.legacy_mtime.into();
    let oldest_iso: chrono::DateTime<chrono::Utc> = s.oldest_slot_mtime.into();
    format!(
        "target_debug_staleness: non-pool exe {} (mtime {}) is older than every \
         slot exe (oldest slot mtime {}). Being OLDER is what makes it unused: \
         a non-pool exe NEWER than the picked slot can be adopted over that \
         slot, but this one loses to every slot. If every slot is emptied, \
         spawn-test {{rebuild:false}} falls through to it (preference 3) and it \
         is then refused unless allow_stale_fallback is set. Either rebuild via \
         supervisor (build into a slot) or delete the stale exe. See \
         feedback_runner_manual_build.",
        s.legacy_path.display(),
        legacy_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        oldest_iso.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// Read the legacy + slot exe paths from `SharedState` and run the staleness
/// check. Returns `None` when the legacy exe is absent, no slot exes exist,
/// or the legacy exe is not strictly older than every slot exe.
fn compute_target_debug_staleness_for_state(state: &SharedState) -> Option<TargetDebugStaleness> {
    let legacy = state.config.runner_exe_path();
    let slot_paths: Vec<(usize, std::path::PathBuf)> = state
        .build_pool
        .slots
        .iter()
        .map(|s| {
            (
                s.id,
                s.target_dir
                    .join("debug")
                    .join(crate::config::RUNNER_BIN_NAME),
            )
        })
        .collect();
    let slot_refs: Vec<(usize, &std::path::Path)> = slot_paths
        .iter()
        .map(|(id, p)| (*id, p.as_path()))
        .collect();
    detect_target_debug_staleness(&legacy, &slot_refs)
}

// =============================================================================
// Source-exe resolution + identity
// =============================================================================

/// Where the resolved source exe came from. Reported in logs and on the spawn
/// response so a caller can always see which path won — the reason the
/// 54-day-old-binary spawn went unnoticed is that nothing reported the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExeOrigin {
    /// A build-pool slot (`target-pool/slot-N/debug/`) — preference 1/2.
    Slot(usize),
    /// The non-pool cargo target dir — preference 3 — resolved through cargo's
    /// own target-dir precedence. The variant carries WHICH precedence level
    /// won (`CARGO_TARGET_DIR`, `build.target-dir`, workspace default).
    ///
    /// **Last-resort fallthrough only.** Reaching it means every build-pool
    /// slot was empty, so resolution landed on the artifact nobody maintains.
    /// This is the origin the `LEGACY_EXE_FALLBACK` dev-state names.
    CargoTargetDir(TargetDirSource),
    /// The SAME path as [`ExeOrigin::CargoTargetDir`], reached deliberately: a
    /// local build NEWER than the picked slot that carried a vouched provenance
    /// sidecar describing itself, so resolution ADOPTED it over the slot (see
    /// [`local_build_is_adoptable`]).
    ///
    /// **Why a distinct variant and not a flag on `CargoTargetDir`.** The two
    /// are the same *path* under opposite *conditions*: the fallthrough means
    /// "nothing better existed and this artifact has no identity", adoption
    /// means "something better DID exist and it proved what it is". Adoption
    /// landed reporting itself as `CargoTargetDir`, and `slot_id() == None` is
    /// precisely what `LEGACY_EXE_FALLBACK` keys on — so every healthy
    /// `dev-start.ps1` adoption raised the 2026-06-07 white-screen incident
    /// state, feeding a risk model that can route a start to the LKG binary.
    /// That re-creates the exact defect adoption exists to fix, through the
    /// dev-action door instead of the resolution one.
    AdoptedLocalBuild(TargetDirSource),
    /// A per-runner `source_exe_override` (today: the LKG pin).
    PinnedOverride,
}

impl ExeOrigin {
    /// Stable machine-readable label for logs and API responses.
    pub fn label(self) -> String {
        match self {
            ExeOrigin::Slot(id) => format!("slot-{id}"),
            ExeOrigin::CargoTargetDir(src) => format!("cargo_target_dir:{}", src.label()),
            ExeOrigin::AdoptedLocalBuild(src) => {
                format!("adopted_local_build:{}", src.label())
            }
            ExeOrigin::PinnedOverride => "pinned_override".to_string(),
        }
    }

    /// The build-pool slot id, when the exe came from one.
    pub fn slot_id(self) -> Option<usize> {
        match self {
            ExeOrigin::Slot(id) => Some(id),
            _ => None,
        }
    }

    /// The cargo target-dir precedence level, for the two origins that resolve
    /// to the non-pool path. `None` for a slot or a pinned override.
    pub fn target_dir_source(self) -> Option<TargetDirSource> {
        match self {
            ExeOrigin::CargoTargetDir(src) | ExeOrigin::AdoptedLocalBuild(src) => Some(src),
            ExeOrigin::Slot(_) | ExeOrigin::PinnedOverride => None,
        }
    }

    /// Did resolution fall THROUGH to the unmaintained non-pool artifact
    /// because no build-pool slot had an exe at all?
    ///
    /// The `LEGACY_EXE_FALLBACK` dev-state predicate — and deliberately NOT
    /// `slot_id().is_none()`. Since local-build adoption landed there are two
    /// slot-less origins with opposite meanings, and conflating them reports
    /// the 2026-06-07 white-screen incident state on the healthy path. See
    /// [`ExeOrigin::AdoptedLocalBuild`].
    pub fn is_legacy_fallback(self) -> bool {
        matches!(self, ExeOrigin::CargoTargetDir(_))
    }
}

/// A resolved runner exe plus everything needed to judge and report it: where
/// it came from, when it was written, and what build identity it carries.
///
/// `provenance == None` is an honest UNKNOWN — never "fine". The identity gate
/// ([`unverified_exe_gate`]) treats it as a refusal condition on the non-pool
/// path, because an artifact with no identity cannot be shown to match what the
/// caller asked for.
#[derive(Debug, Clone)]
pub struct ResolvedRunnerExe {
    pub path: std::path::PathBuf,
    pub origin: ExeOrigin,
    /// mtime of the resolved exe, `None` when unreadable.
    pub mtime: Option<std::time::SystemTime>,
    /// Build identity recorded next to the artifact, when there is one.
    pub provenance: Option<BuildProvenance>,
    /// Set when the identity gate ALLOWED an unverified artifact through an
    /// explicit opt-in. Callers MUST surface it — an opted-in stale spawn
    /// states its staleness, it does not go quiet.
    pub unverified_warning: Option<String>,
}

impl ResolvedRunnerExe {
    pub fn slot_id(&self) -> Option<usize> {
        self.origin.slot_id()
    }

    /// mtime as RFC3339, for messages and API responses.
    pub fn mtime_rfc3339(&self) -> Option<String> {
        self.mtime.map(|m| {
            let dt: chrono::DateTime<chrono::Utc> = m.into();
            dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
    }

    /// Human-readable build identity: `sha (source)`, or an explicit "absent"
    /// so a reader can never mistake unknown for current.
    pub fn identity_label(&self) -> String {
        match &self.provenance {
            Some(p) => format!(
                "sha {} (source {}, built_from {}, built_at {})",
                p.sha.as_deref().unwrap_or("(unknown)"),
                source_label(p.source),
                p.built_from,
                p.built_at,
            ),
            None => "ABSENT (no provenance sidecar next to the artifact)".to_string(),
        }
    }
}

/// Read the provenance sidecar sitting NEXT TO an exe (as opposed to
/// [`read_slot_provenance`], which takes a slot's target dir and joins
/// `debug/`). Used for the non-pool artifact, which has no slot.
pub fn read_provenance_beside_exe(exe_path: &std::path::Path) -> Option<BuildProvenance> {
    let sidecar = exe_path.parent()?.join(SLOT_PROVENANCE_SIDECAR_FILENAME);
    let content = std::fs::read_to_string(&sidecar).ok()?;
    serde_json::from_str::<BuildProvenance>(&content).ok()
}

/// Env kill-switch: treat every start as if the caller had opted into an
/// unverified exe. Exists so a box whose build pool is empty and whose only
/// artifact is a sidecar-less manual build can still start a runner without a
/// supervisor rebuild — the same shape as
/// `QONTINUI_SUPERVISOR_NO_CRASH_RESTART`. The staleness is still logged and
/// still reported on the response; only the refusal is waived.
pub const ALLOW_UNVERIFIED_EXE_ENV: &str = "QONTINUI_SUPERVISOR_ALLOW_UNVERIFIED_EXE";

/// Is the fleet-level unverified-exe opt-in set?
pub fn allow_unverified_exe_env() -> bool {
    std::env::var(ALLOW_UNVERIFIED_EXE_ENV)
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Verdict of [`unverified_exe_gate`] on the allow side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExeIdentityVerdict {
    /// Positive evidence the artifact is a vouched supervisor build (or it came
    /// from a build-pool slot, which has its own [`start_provenance_gate`]).
    Verified,
    /// Identity is unknown or foreign, and an explicit opt-in allowed it. The
    /// string states the staleness and MUST be surfaced to the caller.
    AllowedByOptIn(String),
}

/// Refuse to start a runner from an artifact that cannot be shown to be what
/// was asked for.
///
/// **Scope: the two non-pool origins only** (`ExeOrigin::CargoTargetDir` and
/// `ExeOrigin::AdoptedLocalBuild` — the same path, reached by fallthrough and
/// by adoption respectively; both run this gate). Slot
/// artifacts keep their existing posture ([`start_provenance_gate`]: refuse only
/// on POSITIVE evidence of a foreign tree, warn-and-proceed on absence), because
/// a slot exe at least came from a supervisor build and a refusal there would
/// brick the normal path.
///
/// **Why absence refuses HERE and warns THERE.** Absence can only arise on the
/// FALLTHROUGH arm — an adopted local build carries a vouched sidecar by
/// construction, which is this gate's own allow condition — and fallthrough
/// happens only when NO slot has an exe at all, an already-degenerate state.
/// It is then the artifact nobody maintains: on the fleet box it was a 54-day-old
/// binary that started healthy, served the UI Bridge, and made a
/// `spawn-test {rebuild:false}` measure code from June while the caller believed
/// it was testing a branch. Warning about that and launching it anyway is
/// exactly the false green this gate exists to remove, so unknown reads as
/// *refuse*, not as *fine*. The escape hatches are explicit:
/// `allow_stale_fallback: true` per request, or the
/// `QONTINUI_SUPERVISOR_ALLOW_UNVERIFIED_EXE=1` env kill-switch — both of which
/// still report the staleness rather than hiding it.
///
/// Pure (no I/O, no state) so the decision is unit-testable.
pub fn unverified_exe_gate(
    allow_unverified: bool,
    resolved: &ResolvedRunnerExe,
) -> Result<ExeIdentityVerdict, SupervisorError> {
    let target_dir_source = match resolved.origin {
        // Slots and explicit pins are governed elsewhere.
        ExeOrigin::Slot(_) | ExeOrigin::PinnedOverride => return Ok(ExeIdentityVerdict::Verified),
        // Both non-pool origins run the SAME gate. An adopted local build is
        // vouched by construction, so it takes the allow arm below — but it is
        // routed through the identical predicate rather than short-circuited,
        // so a future tightening of the gate cannot be bypassed by having been
        // adopted. `an_adopted_local_build_passes_the_unverified_exe_gate`
        // pins the agreement between the two.
        ExeOrigin::CargoTargetDir(src) | ExeOrigin::AdoptedLocalBuild(src) => src,
    };

    // Positive evidence of a vouched supervisor build — allow.
    if let Some(p) = &resolved.provenance {
        if p.source.is_vouched() {
            return Ok(ExeIdentityVerdict::Verified);
        }
    }

    let mtime = resolved.mtime_rfc3339();
    let detail = format!(
        "Refusing to start a runner from {path}: the supervisor cannot show this binary is the \
         code you asked for. Resolved outside the build pool via cargo's target-dir precedence \
         ({target_dir_source}); mtime {mtime}; build identity {identity}. No build-pool slot has \
         an exe, so resolution fell through to this artifact — the one nobody maintains (on \
         2026-08-06 it was a 54-day-old binary that started healthy and served the UI Bridge \
         while a spawn-test believed it was measuring a branch). Recovery: \
         POST /runners/spawn-test {{\"rebuild\": true}} (or POST /runner/fix-and-rebuild) so the \
         binary carries a provenance record. To deliberately run whatever exists, re-send with \
         {{\"allow_stale_fallback\": true}} — the response will state the staleness — or set \
         {env}=1 on the supervisor.",
        path = resolved.path.display(),
        target_dir_source = target_dir_source.label(),
        mtime = mtime.as_deref().unwrap_or("(unreadable)"),
        identity = resolved.identity_label(),
        env = ALLOW_UNVERIFIED_EXE_ENV,
    );

    if allow_unverified {
        return Ok(ExeIdentityVerdict::AllowedByOptIn(format!(
            "Starting from UNVERIFIED runner exe {path} by explicit opt-in. Resolved via \
             {target_dir_source}; mtime {mtime}; build identity {identity}. This binary's \
             contents are not attributable to any commit — do NOT read a test result from it \
             as evidence about a branch.",
            path = resolved.path.display(),
            target_dir_source = target_dir_source.label(),
            mtime = mtime.as_deref().unwrap_or("(unreadable)"),
            identity = resolved.identity_label(),
        )));
    }

    Err(SupervisorError::UnverifiedExe(Box::new(
        crate::error::UnverifiedExeInfo {
            path: resolved.path.display().to_string(),
            mtime,
            build_sha: resolved.provenance.as_ref().and_then(|p| p.sha.clone()),
            build_source: resolved
                .provenance
                .as_ref()
                .map(|p| source_label(p.source).to_string()),
            target_dir_source: target_dir_source.label().to_string(),
            detail,
        },
    )))
}

/// Locate the most recent successfully-built runner exe across the build pool.
///
/// Preference order:
/// 1. The exe in the slot recorded as `last_successful_slot` (fresh build).
/// 2. Any slot whose exe exists on disk (e.g. after a supervisor restart).
/// 3. The non-pool cargo target dir — resolved through cargo's OWN target-dir
///    precedence ([`crate::config::TargetDirSource`]), not a hardcoded
///    `<workspace>/target/debug` — for builds that predate the build pool or
///    were run by hand.
///
/// After picking a slot (preference 1 or 2), this function emits a `WARN`
/// log line when the picked slot's `.git_sha` sidecar differs from any other
/// slot's sidecar. The warning is observability only — resolution proceeds
/// with the picked slot regardless. See `proj_supervisor_slot_resolution_order`.
pub async fn resolve_source_exe(
    state: &SharedState,
) -> Result<std::path::PathBuf, SupervisorError> {
    resolve_source_exe_detailed(state).await.map(|r| r.path)
}

/// [`resolve_source_exe`] variant that ALSO returns the [`ExeOrigin`] the exe
/// came from.
///
/// It returns the ORIGIN rather than a bare `Option<usize>` slot id because
/// there are now two slot-less origins with opposite meanings — the
/// preference-3 fallthrough and an adopted local build — and a caller handed
/// only `None` cannot tell the incident from the healthy outcome. Callers that
/// want the slot id call [`ExeOrigin::slot_id`]; callers asking "did we fall
/// through to the unmaintained artifact?" call [`ExeOrigin::is_legacy_fallback`].
///
/// Exists so the start-path provenance gate can be evaluated on the SAME pick
/// the resolution deploys: the previous shape (gate runs its own
/// `pick_slot_for_resolution`, then `resolve_source_exe` re-picks) had a
/// race — a build succeeding between the two picks moves
/// `last_successful_slot`, so resolution could deploy a slot the gate never
/// evaluated. A freshly-succeeded OVERRIDE build is exactly what moves
/// `last_successful_slot`, i.e. the race window reintroduced the incident
/// class the gate exists to kill. One pick, gate and path from the same id.
pub async fn resolve_source_exe_with_origin(
    state: &SharedState,
) -> Result<(ExeOrigin, std::path::PathBuf), SupervisorError> {
    resolve_source_exe_detailed(state)
        .await
        .map(|r| (r.origin, r.path))
}

/// Full resolution: the winning path, WHICH source produced it, its mtime and
/// its build identity. The two thinner wrappers above delegate here so there is
/// exactly one resolution implementation.
pub async fn resolve_source_exe_detailed(
    state: &SharedState,
) -> Result<ResolvedRunnerExe, SupervisorError> {
    if let Some(picked_id) = pick_slot_for_resolution(state).await {
        let picked_path = state.config.runner_exe_path_for_slot(picked_id);

        // Drift check after pick — observability only, does NOT change which slot wins.
        let slot_provenance: Vec<(usize, SlotProvenanceKey)> = state
            .build_pool
            .slots
            .iter()
            .map(|s| {
                let key: SlotProvenanceKey = match read_slot_provenance(&s.target_dir) {
                    Some(p) => (p.sha, Some(p.source)),
                    None => (None, None),
                };
                (s.id, key)
            })
            .collect();
        let picked = slot_provenance
            .iter()
            .find(|(id, _)| *id == picked_id)
            .map(|(_, k)| k.clone())
            .unwrap_or((None, None));
        if let Some(drift) = detect_slot_sha_drift(picked_id, &picked, &slot_provenance) {
            let msg = format_drift_warning(&drift);
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }

        // Adjacent observability: a stale exe at `target/debug/` (operator ran
        // `cargo build` from the workspace root instead of into a slot). Same
        // shape as the cross-slot drift check — log + SSE; resolution unchanged.
        if let Some(staleness) = compute_target_debug_staleness_for_state(state) {
            let msg = format_target_debug_warning(&staleness);
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }

        // ── "A rebuild must take the latest code." ──────────────────────
        //
        // The slot pool is the supervisor's own artifact store, but it is NOT
        // the only place a runner exe gets built: `dev-start.ps1` compiles the
        // runner straight into `target/debug/` on every start. Preference 3
        // means that artifact is used only when NO slot exists, so a fresh
        // local build was silently discarded in favour of an arbitrarily old
        // slot — the operator rebuilt, was told "Runner binary rebuilt", and
        // ran day-old code anyway.
        //
        // When the local build is NEWER than the slot we picked, prefer it —
        // but only when it can prove what it is (see
        // [`local_build_is_adoptable`]). An unidentifiable artifact is warned
        // about and NOT run: serving a stale-but-known slot beats launching an
        // unknown binary, and the operator is told plainly that their build is
        // not the one running.
        //
        // An explicit LKG request never reaches here — `source_exe_override`
        // short-circuits resolution upstream — so "unless the operator asks for
        // the LKG binary" holds by construction.
        if let Some(adoption) =
            evaluate_local_build_adoption(&state.config, picked_id, &picked_path)
        {
            let adopt = adoption.adopted;
            let msg = adoption.message();
            // Adoption is the HEALTHY outcome this fix exists to produce — the
            // operator built, and the operator's build is running. Logging that
            // at WARN on every resolution would train the reader to skim past
            // the line that matters: the REFUSED case, where their build is
            // silently not running.
            if adopt {
                info!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Info, msg)
                    .await;
            } else {
                warn!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                    .await;
            }
            if adopt {
                // `AdoptedLocalBuild`, NOT `CargoTargetDir`: same path, opposite
                // condition. `CargoTargetDir` means resolution fell THROUGH to
                // the artifact nobody maintains, and that is what the
                // `LEGACY_EXE_FALLBACK` dev-state names — reporting it here
                // flags every healthy adoption as the 2026-06-07 white-screen
                // incident state.
                return Ok(ResolvedRunnerExe {
                    mtime: Some(adoption.finding.legacy_mtime),
                    path: adoption.finding.legacy_path,
                    origin: ExeOrigin::AdoptedLocalBuild(adoption.target_dir_source),
                    provenance: adoption.provenance,
                    unverified_warning: None,
                });
            }
        }

        let provenance = state
            .build_pool
            .slots
            .iter()
            .find(|s| s.id == picked_id)
            .and_then(|s| read_slot_provenance(&s.target_dir));
        return Ok(ResolvedRunnerExe {
            mtime: file_mtime(&picked_path),
            path: picked_path,
            origin: ExeOrigin::Slot(picked_id),
            provenance,
            unverified_warning: None,
        });
    }

    // Preference 3: the non-pool cargo target dir, resolved through cargo's own
    // precedence ladder. No slot id — callers that gate on provenance treat
    // this as the most-unknown artifact there is.
    let candidates = state.config.runner_exe_candidates();
    if let Some((src, path)) = candidates.iter().find(|(_, p)| p.exists()) {
        return Ok(ResolvedRunnerExe {
            mtime: file_mtime(path),
            provenance: read_provenance_beside_exe(path),
            path: path.clone(),
            origin: ExeOrigin::CargoTargetDir(*src),
            unverified_warning: None,
        });
    }

    // Name every candidate that was tried, with the precedence level that
    // produced it: "not found at <one path>" was actively misleading once the
    // ladder had more than one rung.
    let tried = candidates
        .iter()
        .map(|(src, p)| format!("{} ({})", p.display(), src.label()))
        .collect::<Vec<_>>()
        .join("; ");
    Err(SupervisorError::Process(format!(
        "Runner exe not found in any build slot, nor at any cargo target-dir candidate: {tried}. \
         Run a build first."
    )))
}

/// mtime of a path, `None` when it can't be stat'd. Best-effort by design —
/// an unreadable mtime is reported as unknown, never as fresh.
fn file_mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

// =============================================================================
// Startup Cleanup
// =============================================================================

/// Kill any orphaned temp runner processes AND remove stale registry entries
/// from previous supervisor sessions.
/// Only cleans up temp runner ports — user runners are never touched.
pub async fn cleanup_orphaned_runners(state: &SharedState) {
    let runners = state.get_all_runners().await;

    // Collect temp runner ports (to kill processes) and stale IDs (to remove from registry).
    let mut ports: Vec<u16> = Vec::new();
    let mut stale_ids: Vec<String> = Vec::new();
    for r in &runners {
        if is_temp_runner(&r.config.id) {
            ports.push(r.config.port);
            // On startup, ALL pre-existing test runners are stale — they're
            // leftovers from a previous supervisor session.  Remove them from
            // the registry after killing their processes.
            stale_ids.push(r.config.id.clone());
        } else {
            // Mark non-temp runners as running if the HTTP /health endpoint
            // responds, so the supervisor tracks their health without managing
            // them. We probe HTTP rather than just TCP here because a stale
            // socket left behind by a just-killed runner can make the TCP
            // check return true for several seconds — that false positive
            // used to leave the primary stuck as `running=true, pid=null`
            // and prevented manual restart from being triggered on boot.
            if crate::process::port::is_runner_responding(r.config.port).await {
                info!(
                    "Runner '{}' (port {}) already running — tracking health only",
                    r.config.name, r.config.port
                );
                let mut runner = r.runner.write().await;
                runner.running = true;
            } else if crate::process::port::is_port_listening(r.config.port) {
                warn!(
                    "Runner '{}' port {} has a live socket but /health is not responding — \
                     treating as offline (process may be wedged mid-startup or mid-teardown)",
                    r.config.name, r.config.port
                );
            }
        }
    }

    let mut killed_any = false;
    for &port in &ports {
        if let Ok(true) = crate::process::proc_kill::kill_by_port(port).await {
            info!("Killed orphaned temp runner on port {}", port);
            killed_any = true;
        }
    }

    // Remove stale test runner entries from the in-memory registry.
    if !stale_ids.is_empty() {
        let mut runners_map = state.runners.write().await;
        for id in &stale_ids {
            runners_map.remove(id);
        }
        info!(
            "Purged {} stale test runner entries from registry on startup",
            stale_ids.len()
        );
    }

    if killed_any {
        tokio::time::sleep(Duration::from_secs(1)).await;
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                "Cleaned up orphaned temp runner processes",
            )
            .await;
    }
}

// =============================================================================
// Periodic Stale Test Runner Reaper
// =============================================================================

/// Background task that periodically detects and removes stopped/crashed test
/// runners from the in-memory registry. Runs every 5 minutes.
///
/// A test runner is considered stale if:
///   - Its `running` flag is false, OR
///   - Its `running` flag is true but nothing is listening on its port (crash),
///     OR
///   - It is a `RunnerKind::Temp` that has outlived
///     [`crate::config::temp_runner_max_age`] — the only rule here that reaps a
///     **healthy** runner.
///
/// **This is the sweeper that owns the max-age bound, and it owns it alone.**
/// There are two independent stale-temp sweeps, both spawned from `main.rs`:
/// this one, and `main::reap_stale_test_runners` →
/// `routes::runners::purge_stale_test_runners_core`. The bound lives here for
/// two reasons.
/// (1) **It is the only one that can act on it** — this loop is the sole holder
/// of `ManagedRunner::created_at` (the core has no time input at all), and it
/// is the one with the full kill ladder (tree-kill by pid → kill by port →
/// confirm the port freed) that terminating a live process needs; the core's
/// Windows-only best-effort `kill_by_port` runs only after it has already
/// decided the process is dead. (2) **The core must stay liveness-only**,
/// because it also backs the operator-facing `POST /runners/purge-stale`, whose
/// contract is "remove runners whose processes are no longer alive" — reaping a
/// healthy runner from under an operator who asked to tidy up dead records
/// would be a surprise. So the two loops do not disagree: the core's
/// `port_alive => continue` remains the whole truth for the core, and the
/// max-age exception exists in exactly one place.
///
/// **Active-build grace:** a placeholder with `running=false` is the normal
/// pre-spawn state while `spawn-test --rebuild` runs `npm run build` +
/// `cargo build`. Cold cargo builds can exceed 10-15 min on a fresh checkout,
/// far longer than the prior 2-minute grace period. Reaping a placeholder
/// mid-build leaves the build orphaned (cancelled when its associated
/// placeholder vanishes) and the user with no runner. We now additionally
/// skip reaping when any build slot is currently busy — the assumption being
/// that an active build is overwhelmingly likely to be feeding a recent
/// placeholder.
pub async fn reap_stale_test_runners(state: SharedState) {
    const INTERVAL: Duration = Duration::from_secs(5 * 60);
    // Wait a bit on startup to let normal init complete
    tokio::time::sleep(Duration::from_secs(30)).await;

    // Resolved once — the accessor memoizes anyway, and reading it here means
    // the setting is reported at startup rather than only when it first bites.
    let max_age_bound = crate::config::temp_runner_max_age();
    match max_age_bound {
        Some(max) => info!(
            "Stale-temp-runner sweep armed with a {}s max-age bound (RunnerKind::Temp only; \
             primary/named/external are exempt). Override with \
             QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS, 0 to disable.",
            max.as_secs()
        ),
        None => info!(
            "Stale-temp-runner sweep running with the max-age bound DISABLED \
             (QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS=0) — a healthy temp runner will \
             never be reaped for age."
        ),
    }

    loop {
        tokio::time::sleep(INTERVAL).await;

        // Sample build-pool state once per sweep. Cheap (per-slot RwLock try_read).
        let any_build_active = state
            .build_pool
            .slots
            .iter()
            .any(|s| s.busy.try_read().map(|g| g.is_some()).unwrap_or(true));

        let runners = state.get_all_runners().await;
        let mut reaped = 0u32;

        for managed in &runners {
            if !is_temp_runner(&managed.config.id) {
                continue;
            }
            // Skip runners created less than 2 minutes ago — they may still
            // be in the build+start pipeline (spawn_test inserts a placeholder
            // with running=false before the build completes).
            if managed.created_at.elapsed() < Duration::from_secs(120) {
                continue;
            }
            // Max-age bound. Measured from when the runner actually STARTED,
            // not from when `spawn-test` reserved its placeholder — see
            // `resolve_temp_runner_age`. A cold `spawn-test {rebuild:true}`
            // spends 40-50 min building before the child ever runs, and
            // charging that to the runner's lifetime would reap it moments
            // after it finally came up.
            let (age, age_basis) = {
                let started_at = managed.runner.read().await.started_at;
                resolve_temp_runner_age(
                    started_at,
                    chrono::Utc::now(),
                    managed.created_at.elapsed(),
                )
            };
            // Second gate is DEFENSIVE, not a different predicate: the loop
            // already filtered on `is_temp_runner(&config.id)` (the id prefix)
            // above, and this re-asks the same question of the typed
            // `RunnerKind`. Two independent spellings must both say "temp"
            // before anything healthy is killed.
            let over_max_age =
                exceeds_temp_runner_max_age(&managed.config.kind(), age, max_age_bound);

            let is_running = {
                let runner = managed.runner.read().await;
                runner.running
            };
            // Active-build grace: a pre-running placeholder (`running=false`)
            // while ANY build slot is busy is almost certainly the spawn-test
            // request that triggered that build. Don't reap it — wait for the
            // build to finish so the handler can promote it to running=true.
            // Runners that say `running=true` get the post-crash sweep below
            // regardless of pool state.
            if !is_running && any_build_active {
                continue;
            }
            // True only on the max-age arm — i.e. the one path that reaches the
            // kill ladder with a runner we just observed LISTENING. It makes the
            // ladder's terminal "port still busy → drop the record anyway" rule
            // conditional; see there.
            let mut age_kill = false;

            if is_running {
                let port_alive = crate::process::port::is_port_listening(managed.config.port);
                if port_alive && !over_max_age {
                    continue; // genuinely alive, and inside its lifetime bound
                }
                if port_alive {
                    // A HEALTHY temp runner is about to be killed for age. This
                    // is the only place the sweep does that, so say so loudly
                    // and completely BEFORE acting — with the runner's identity,
                    // its measured age (and which clock measured it), the bound
                    // it broke, and the knob that changes or disables the bound.
                    // An operator who finds a temp runner gone must be able to
                    // learn why from the log alone.
                    age_kill = true;
                    let owner = managed.requester_id.read().await.clone();
                    let msg = format!(
                        "Reaping temp runner '{}' (id {}, port {}, requester {:?}) — it has \
                         been alive {}s ({}), past the {}s max-age bound. Raise or disable the \
                         bound with QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS \
                         (0 = never reap for age). Only RunnerKind::Temp is ever age-bounded; \
                         primary/named/external runners are exempt, and `protected: true` does \
                         NOT exempt a temp runner (every temp is created protected, so honouring \
                         it here would make the bound inert).",
                        managed.config.name,
                        managed.config.id,
                        managed.config.port,
                        owner,
                        age.as_secs(),
                        age_basis,
                        max_age_bound.map(|d| d.as_secs()).unwrap_or(0),
                    );
                    warn!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                        .await;

                    // Latch the stop INTENT before the kill ladder, exactly as
                    // `stop_runner_by_id` does. This is a supervisor-spawned
                    // child with a live `Child` handle, so `start_managed_runner`
                    // has a `monitor_runner_process_exit` watching it; without
                    // the flag the age-kill exit reaches `decide_crash_restart`
                    // as `had_child_handle = true, stop_requested = false,
                    // clean_exit = false` — a textbook crash — and a runner
                    // armed via `POST /runners/{id}/watchdog {"enabled": true}`
                    // gets auto-restarted for a death the supervisor itself
                    // caused.
                    //
                    // The dangerous case is not the noisy one: on the
                    // `age_kill => continue` path below we deliberately KEEP the
                    // record, so a process that dies just after the 5s
                    // `wait_for_port_free` window would be restarted into that
                    // surviving record — and `start_managed_runner` sets
                    // `started_at = Some(now)`, resetting the measured age to
                    // zero. The over-age temp would become unreapable in
                    // perpetuity: kill → resurrect → age resets → the next sweep
                    // measures a young runner.
                    //
                    // No unwind is needed on the `continue` path: the flag's
                    // lifecycle is "latched on stop request, cleared on the next
                    // start", so a retry simply re-latches and a legitimate
                    // operator restart clears it.
                    managed.runner.write().await.stop_requested = true;
                } else {
                    // Port free but state says running — crashed
                    let mut runner = managed.runner.write().await;
                    runner.running = false;
                    runner.pid = None;
                }
            }

            let id = managed.config.id.clone();
            let name = managed.config.name.clone();
            let port = managed.config.port;
            let pid = {
                let runner = managed.runner.read().await;
                runner.pid
            };

            // ── Terminate the owned process ATOMICALLY with record removal. ──
            // This used to be a Windows-only `kill_by_port`, so on macOS/Linux
            // the reaper dropped the registry record while the OS process
            // stayed alive — the orphan leak (D7): `POST /stop` then 404'd
            // while the runner kept serving its port. We now (1) tree-kill the
            // tracked PID (reaps the runner's child panes/helpers too),
            // (2) backstop with kill-by-port for a re-parented orphan whose PID
            // we lost, and (3) confirm the port is free before we forget the
            // record. All three go through the cross-platform `proc_kill`
            // facade, so every platform gets a real kill.
            if let Some(pid) = pid {
                let _ = crate::process::proc_kill::kill_by_pid_tree(pid).await;
            }
            // `Err` is UNKNOWN, not "port idle" — say so rather than
            // swallowing it (contract on `proc_kill::find_pid_on_port`).
            if let Err(e) = crate::process::proc_kill::kill_by_port(port).await {
                warn!(
                    "Stale-test-runner reap: listener probe on port {} could not answer \
                     ({}) — nothing was killed by port",
                    port, e
                );
            }
            // Confirm the process actually released the port.
            if !crate::process::port::wait_for_port_free(port, 5).await {
                if age_kill {
                    // The max-age arm is the ONLY path that reaches here with a
                    // runner we just watched LISTENING. Forgetting it now would
                    // delete its WebView2 profile, `instance-<name>` tree and
                    // instance config dir out from under a live process, and
                    // leave that process serving the port untracked. The port
                    // allocator in `routes::runners::spawn_test` is
                    // registry-only (it derives `used_ports` from
                    // `runners.values()` and never probes the socket), so the
                    // next spawn would reuse this port and `wait_for_healthy`
                    // would poll the ZOMBIE's `/health` — handing the caller a
                    // "healthy" runner id served by an untracked,
                    // config-dir-less process.
                    //
                    // We know it was alive, so retrying on the next sweep is
                    // both safe and correct: the runner is still registered,
                    // still owns its dirs, and is still over the bound.
                    warn!(
                        "reaper: temp runner '{}' (id {}, pid {:?}) survived the max-age kill — \
                         port {} is still in use after 5s. KEEPING the registry record and its \
                         on-disk dirs; retrying on the next sweep. Dropping it here would orphan \
                         a LIVE process onto a port the allocator would then hand to a new spawn.",
                        name, id, pid, port
                    );
                    continue;
                }
                // Dead-runner arms (running=false, or running=true with a dead
                // port): the process was already concluded dead, so log loudly
                // but still drop the record — leaving it would re-trip the same
                // reap next cycle; the reconcile sweep will re-kill any survivor
                // by port. Bounded so a wedged process never stalls the loop.
                warn!(
                    "reaper: port {} still in use after killing stale test runner '{}' \
                     (pid {:?}); dropping record anyway — reconcile sweep will re-kill by port",
                    port, name, pid
                );
            }

            // Preserve the runner's logs in the stopped-runners cache before
            // dropping its ManagedRunner so post-mortem debugging still works
            // via `GET /runners/{id}/logs?include_stopped=true`.
            let snapshot = crate::process::stopped_cache::snapshot_from_managed(
                managed,
                None,
                crate::process::stopped_cache::StopReason::Reaped,
            )
            .await;
            {
                let mut cache = state.stopped_runners.write().await;
                crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
            }

            {
                let mut runners_map = state.runners.write().await;
                runners_map.remove(&id);
            }

            #[cfg(windows)]
            {
                let _ = remove_webview2_user_data_folder(&id, false).await;
                let _ = remove_runner_app_data_dirs(&name, false).await;
                let _ = remove_instance_config_dir(&id, false).await;
            }

            info!(
                "reaper: removed stale test runner '{}' (port {})",
                name, port
            );
            reaped += 1;
        }

        if reaped > 0 {
            state
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Info,
                    format!("Reaper: purged {} stale test runner(s)", reaped),
                )
                .await;
        }

        // Reconcile sweep: catch ALREADY-orphaned temp-runner processes whose
        // registry record is gone (e.g. reaped by an older binary that didn't
        // kill the process, or a stop that 404'd while the process survived).
        // This is the safety net for the D7 orphan leak — even if a record was
        // dropped without a kill, the next sweep frees the port.
        reconcile_orphaned_temp_runners(&state).await;
    }
}

/// Temp-runner port range (mirrors the `9877..=9899` allocation in
/// `routes::runners`). Kept here so the reconcile sweep scans exactly the
/// ports the spawner hands out — never the primary (9876) or supervisor
/// (9875) ports.
const TEMP_RUNNER_PORT_RANGE: std::ops::RangeInclusive<u16> = 9877..=9899;

/// Reconcile sweep: kill orphaned `qontinui-runner` processes that are
/// LISTENING on a temp-runner port (9877-9899) but have NO matching record in
/// the registry. Frees ports leaked by a record-removed-but-process-alive
/// event (the D7 bug, or a record dropped by a prior supervisor binary).
///
/// Safety: only kills a PID that (a) is listening on a temp-range port, (b) is
/// NOT claimed by any registered runner, AND (c) is confirmed to be a
/// `qontinui-runner` image via [`proc_kill::is_qontinui_runner_pid`]. An
/// unrelated process that grabbed a temp-range port is left untouched. The
/// primary (9876) is outside the swept range and can never be hit.
async fn reconcile_orphaned_temp_runners(state: &SharedState) {
    // Ports currently claimed by a registered runner — never sweep these.
    let claimed_ports: std::collections::HashSet<u16> = state
        .get_all_runners()
        .await
        .iter()
        .map(|r| r.config.port)
        .collect();

    let mut freed = 0u32;
    for port in TEMP_RUNNER_PORT_RANGE {
        if claimed_ports.contains(&port) {
            continue;
        }
        if !crate::process::port::is_port_listening(port) {
            continue;
        }
        // Something is on an unclaimed temp port. Confirm it's a runner of
        // ours before killing. A probe that could not RUN is UNKNOWN — skip,
        // never kill blind (the sweep re-runs next cycle).
        let pid = match crate::process::proc_kill::find_pid_on_port(port).await {
            Ok(Some(pid)) => pid,
            Ok(None) => continue,
            Err(e) => {
                debug!(
                    "reconcile sweep: listener probe on port {} could not answer ({}); \
                     leaving it alone this pass",
                    port, e
                );
                continue;
            }
        };
        if !crate::process::proc_kill::is_qontinui_runner_pid(pid) {
            debug!(
                "reconcile sweep: port {} held by non-runner PID {}; leaving alone",
                port, pid
            );
            continue;
        }

        warn!(
            "reconcile sweep: killing orphaned qontinui-runner PID {} on unclaimed \
             temp port {} (no registry record)",
            pid, port
        );
        let _ = crate::process::proc_kill::kill_by_pid_tree(pid).await;
        let _ = crate::process::proc_kill::kill_by_port(port).await;
        if crate::process::port::wait_for_port_free(port, 5).await {
            freed += 1;
        } else {
            warn!(
                "reconcile sweep: port {} still in use after killing orphan PID {}",
                port, pid
            );
        }
    }

    if freed > 0 {
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Info,
                format!(
                    "Reconcile sweep: killed {} orphaned temp-runner process(es) and freed their port(s)",
                    freed
                ),
            )
            .await;
    }
}

// =============================================================================
// Per-Runner Process Management (multi-runner)
// =============================================================================

// Per-runner env forwarders moved to `process::env_forwarders`. See
// [`crate::process::env_forwarders::EnvForwarder`] and
// [`crate::process::env_forwarders::default_env_forwarders`]. Every spawned
// runner runs the same registered list once in `start_exe_mode_for_runner`,
// replacing the previous five hand-written `forward_*_env` functions and
// the duplicated cfg(windows) / cfg(not(windows)) call-site chains.

/// Start a specific runner by ID.
///
/// Thin wrapper around [`start_managed_runner`] that first resolves the id in
/// the registry. Prefer `start_managed_runner` when the caller already holds
/// an `Arc<ManagedRunner>` — that path is race-free, whereas id-based lookup
/// can fail if a concurrent remove (reaper, stop, failed probe) fires between
/// insertion and start.
pub async fn start_runner_by_id(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;
    start_managed_runner(state, &managed).await
}

/// Start a runner given a direct `Arc<ManagedRunner>` reference.
///
/// Used by `spawn_test` / `spawn_named` to avoid a re-lookup race: the
/// registry insertion and the start must use the same ManagedRunner, even if
/// another task concurrently removes the id from the map. If the id is
/// missing from the registry when we start (which shouldn't normally happen,
/// but has been observed as a transient 404 under load), we re-insert the Arc
/// so downstream health / monitoring can find it by id.
pub async fn start_managed_runner(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
) -> Result<(), SupervisorError> {
    // With the parallel build pool, a concurrent build on one slot does not
    // prevent us from starting a runner from a previously-built exe in
    // another slot (or the legacy target path). `resolve_source_exe` inside
    // `start_exe_mode_for_runner` returns an explicit error if no binary is
    // available anywhere. No coarse `build_in_progress` check here.

    let runner_id = managed.config.id.clone();

    // Defensive re-insertion: if something removed our id between placeholder
    // insertion and start, put it back. This fixes the ~1-in-10 spawn-test
    // 404 "Runner not found" race observed in smoke tests. Using `entry` +
    // `or_insert` instead of unconditional insert preserves any other Arc
    // that may have replaced ours (so we don't clobber a different managed
    // runner sharing the same id, which would itself indicate a bug).
    {
        let mut runners = state.runners.write().await;
        runners
            .entry(runner_id.clone())
            .or_insert_with(|| managed.clone());
    }

    {
        let runner = managed.runner.read().await;
        if runner.running {
            return Err(SupervisorError::RunnerAlreadyRunning);
        }
    }

    let is_primary = managed.config.kind().is_primary();
    let port = managed.config.port;
    let runner_name = managed.config.name.clone();

    // S2 guard: `running == false` is NOT "the port is free". A wedged,
    // adopted primary reads `running: false` while its process holds the
    // port, and this function used to spawn a second runner against it.
    // Refuse on positive evidence of a live runner image on the port; every
    // start path funnels through here, so one check covers them all.
    refuse_if_port_held_by_live_runner(port).await?;

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Starting runner '{}' (port {})", runner_name, port),
        )
        .await;

    let SpawnResult {
        mut child,
        panic_log_dir,
    } = start_exe_mode_for_runner(state, managed).await?;

    let pid = child.id();
    info!(
        "Runner '{}' started with PID {:?} on port {}",
        runner_name, pid, port
    );

    // Route the spawned process to the supervisor's kill-on-exit JobObject —
    // but ONLY if it is a supervisor-owned ephemeral. When the supervisor
    // process dies (graceful exit, panic, force-kill, or BSOD), the kernel
    // closes the last handle to the job and terminates every assigned process
    // per `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. WebView2 children of the
    // runner are transitively in the job too — Windows assigns child
    // processes of a job-tracked process to the same job by default.
    //
    // A process cannot be removed from a Windows job once assigned, so the
    // temp-vs-user-owned split has to happen HERE. This is the single
    // assignment site; there are at least three supervisor exit paths, which
    // is why the fix lives here rather than at any of them.
    //
    // We assign AFTER `cmd.spawn()` because the runner is not started with
    // `CREATE_SUSPENDED`, so it's already executing. That's fine for
    // KILL_ON_JOB_CLOSE — the only correctness trap with post-spawn
    // assignment is BREAKAWAY_OK interactions where a child could escape
    // before assignment, which we don't rely on here.
    //
    // Assignment failure is loud but non-fatal — the runner is functional;
    // it just won't be auto-killed if the supervisor dies abruptly.
    if let Some(pid_val) = pid {
        if !crate::process::job::should_assign_to_ephemeral_job(&managed.config.kind()) {
            // Log the skip too. A durable runner that is silently never
            // assigned would make the next incident's forensics as hard as
            // the 2026-07-27 one, where the reap produced NO log line at all.
            let msg = format!(
                "Runner '{}' (PID {}) NOT assigned to the kill-on-exit JobObject — \
                 user-owned runners survive supervisor exit.",
                runner_name, pid_val
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
        } else if let Some(job) = state.ephemeral_job.as_ref() {
            match job.assign(pid_val) {
                Ok(()) => {
                    let msg = format!(
                        "Assigned temp runner '{}' (PID {}) to kill-on-exit JobObject",
                        runner_name, pid_val
                    );
                    info!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Info, msg)
                        .await;
                }
                Err(e) => {
                    warn!(
                        "Failed to assign temp runner '{}' (PID {}) to kill-on-exit JobObject: {}. \
                         Supervisor exit will not terminate this runner.",
                        runner_name, pid_val, e
                    );
                    state
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Warn,
                            format!(
                                "Temp runner '{}' (PID {}) NOT assigned to kill-on-exit \
                                 JobObject: {}. If the supervisor crashes, this runner may \
                                 linger as an orphan.",
                                runner_name, pid_val, e
                            ),
                        )
                        .await;
                }
            }
        }
    }

    // Remember the panic log dir so `monitor_runner_process_exit` can find
    // the file after a non-zero exit. Also clear any stale `recent_panic`
    // left over from a previous boot of this runner id — a clean start
    // should not continue surfacing an old panic in the runner list.
    {
        let mut slot = managed.panic_log_dir.write().await;
        *slot = panic_log_dir.clone();
    }
    {
        let mut slot = managed.recent_panic.write().await;
        *slot = None;
    }

    // Open the per-spawn early-death log file BEFORE attaching readers.
    // `spawn_stdout_reader` / `spawn_stderr_reader` snapshot the writer at
    // spawn time, so this must precede them. If the file can't be opened
    // (out of disk space, perms, etc.) the runner still starts — early-log
    // capture is strictly best-effort. Drops any path stored from a prior
    // start of this runner id.
    let early_log_path = crate::process::early_log::early_log_dir()
        .map(|dir| crate::process::early_log::early_log_path_for(&dir, &runner_id));
    if let Some(ref path) = early_log_path {
        match crate::process::early_log::EarlyLogWriter::open(path) {
            Some(writer) => {
                managed.logs.set_early_log_writer(Some(writer));
                let mut slot = managed.early_log_path.write().await;
                *slot = Some(path.clone());
                debug!(
                    "Early-log capture enabled for runner '{}' at {:?}",
                    runner_name, path
                );
            }
            None => {
                // Couldn't open the file — clear any prior path so we don't
                // surface a stale value via the API.
                managed.logs.set_early_log_writer(None);
                let mut slot = managed.early_log_path.write().await;
                *slot = None;
            }
        }
    } else {
        managed.logs.set_early_log_writer(None);
        let mut slot = managed.early_log_path.write().await;
        *slot = None;
    }

    // Capture stdout/stderr to the managed runner's logs. Pass the managed
    // Arc so the readers can populate `last_auth_result` on auth-failure
    // patterns (Item B of the supervisor cleanup plan).
    if let Some(stdout) = child.stdout.take() {
        crate::log_capture::spawn_stdout_reader_for_runner(
            stdout,
            &managed.logs,
            Some(managed.clone()),
        );
    }
    if let Some(stderr) = child.stderr.take() {
        crate::log_capture::spawn_stderr_reader_for_runner(
            stderr,
            &managed.logs,
            Some(managed.clone()),
        );
    }

    // Update per-runner state. Clearing `stop_requested` HERE (rather than
    // at stop completion) makes the operator-stop marker race-free for the
    // exit monitor: the flag can only transition true→false through a
    // subsequent start, so any process exit observed after a stop request
    // always reads `stop_requested == true`.
    {
        let mut runner = managed.runner.write().await;
        runner.process = Some(child);
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
        runner.stop_requested = false;
    }

    // If this is the primary runner, also update legacy state for backward compat
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
        runner.stop_requested = false;
        // process stays None in legacy — managed runner owns it
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Runner '{}' process started (PID: {:?}, port: {})",
                runner_name, pid, port
            ),
        )
        .await;

    state.notify_health_change();
    managed.health_cache_notify.notify_one();

    // Spawn a task to monitor the process exit
    let state_clone = state.clone();
    let managed_clone = managed.clone();
    tokio::spawn(async move {
        monitor_runner_process_exit(state_clone, managed_clone, runner_id).await;
    });

    // Spawn the first-healthy watchdog so a child that spawns but never
    // binds its HTTP API is killed instead of lingering as a zombie.
    if let Some(pid_val) = pid {
        let state_clone = state.clone();
        let managed_clone = managed.clone();
        tokio::spawn(async move {
            watch_first_healthy(state_clone, managed_clone, pid_val).await;
        });
    }

    Ok(())
}

/// Outcome of the fail-open `qontinui-shim.exe` sidecar deploy step in
/// [`start_exe_mode_for_runner`]. Pure enough to unit-test with tempdirs —
/// the caller only maps variants to log lines.
///
/// The runner materializes each terminal's identity shim from the stub
/// sitting NEXT TO ITS OWN EXE (`current_exe().parent()` — `locate_stub_exe`
/// in the runner's `shim_materializer.rs`), so the stub must ride along with
/// every runner-exe deploy. Skipping it re-materializes whatever stale stub
/// already sits next to the copy (the 2026-07-03 incident). See
/// [`crate::build_monitor::SHIM_EXE_FILENAME`] for the full placement
/// contract.
#[derive(Debug)]
pub(crate) enum ShimSidecarDeploy {
    /// Sidecar copied next to the runner exe copy.
    Copied { to: std::path::PathBuf },
    /// Source and destination exes share a directory (the legacy
    /// `target/debug/` fallback resolution) — nothing to copy; whatever shim
    /// sits there is already "next to" the deployed exe.
    SameDir,
    /// No shim next to the source exe (a slot/LKG predating the sidecar
    /// build, or the fail-open shim build failed). Identity shims will be
    /// stale for runners started from this source.
    SourceMissing { expected: std::path::PathBuf },
    /// The copy/replace itself failed.
    CopyFailed {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        error: String,
    },
}

/// Copy the `qontinui-shim.exe` sidecar from next to `source_exe` to next to
/// `dest_exe` (the per-runner exe copy). Fail-open by contract: this returns
/// an outcome for the caller to log — it must never fail the runner start.
///
/// The copy goes through a tmp file + atomic rename (keyed by the dest exe's
/// file stem so concurrent spawns of different runners can't clobber each
/// other's tmp) because the destination `target/debug/qontinui-shim.exe` is
/// SHARED by every runner copy in that dir and a concurrently-starting
/// runner's materializer could otherwise read a torn stub mid-copy.
pub(crate) fn deploy_shim_sidecar(
    source_exe: &std::path::Path,
    dest_exe: &std::path::Path,
) -> ShimSidecarDeploy {
    let shim_name = crate::build_monitor::SHIM_EXE_FILENAME;
    let (src_dir, dst_dir) = match (source_exe.parent(), dest_exe.parent()) {
        (Some(s), Some(d)) => (s, d),
        // Pathological (no parent dir) — treat as nothing-to-do rather than
        // inventing a failure for a case the exe copy itself already handled.
        _ => return ShimSidecarDeploy::SameDir,
    };
    if src_dir == dst_dir {
        return ShimSidecarDeploy::SameDir;
    }

    let shim_src = src_dir.join(shim_name);
    if !shim_src.exists() {
        return ShimSidecarDeploy::SourceMissing { expected: shim_src };
    }
    let shim_dst = dst_dir.join(shim_name);

    let stem = dest_exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "runner".to_string());
    let shim_tmp = dst_dir.join(format!("{}.tmp-{}", shim_name, stem));
    let _ = std::fs::remove_file(&shim_tmp);

    let result = std::fs::copy(&shim_src, &shim_tmp)
        .map_err(|e| format!("copy to tmp: {}", e))
        .and_then(|_| {
            std::fs::rename(&shim_tmp, &shim_dst).or_else(|first_err| {
                // Windows can refuse the replace while another process holds
                // the dest open; drop the dest and retry once (mirrors the
                // exe-copy retry above).
                let _ = std::fs::remove_file(&shim_dst);
                std::fs::rename(&shim_tmp, &shim_dst).map_err(|retry_err| {
                    format!(
                        "rename into place: {}; retry after remove: {}",
                        first_err, retry_err
                    )
                })
            })
        });

    match result {
        Ok(()) => ShimSidecarDeploy::Copied { to: shim_dst },
        Err(error) => {
            let _ = std::fs::remove_file(&shim_tmp);
            ShimSidecarDeploy::CopyFailed {
                from: shim_src,
                to: shim_dst,
                error,
            }
        }
    }
}

/// Result of spawning a runner in exe mode.
struct SpawnResult {
    child: tokio::process::Child,
    /// Directory the supervisor told the runner to write its panic log to
    /// via `QONTINUI_RUNNER_LOG_DIR`. `None` when the supervisor deferred
    /// to the runner's default path (no `--log-dir` configured).
    panic_log_dir: Option<std::path::PathBuf>,
}

/// Start exe mode for a specific runner with port/name env vars.
/// Applies the per-instance config + secure-storage env vars to a spawn command.
///
/// Resolves [`instance_config_dir`] for `runner_id`, creates it, and sets BOTH
/// `QONTINUI_CONFIG_DIR` and `QONTINUI_SECURE_STORAGE_DIR` to it. Returns the
/// dir applied.
///
/// **This is the load-bearing site**: whatever this function writes onto the
/// command is the directory the child loads its pairing and token cache from.
/// It is a standalone function precisely so a test can drive it against a real
/// [`Command`] and read the vars back — re-inlining a divergent path at the
/// call site is then a test failure rather than a silently unpaired runner.
///
/// Both failure modes are hard errors, never a skip. Leaving the vars unset
/// makes the child fall back to the shared
/// `dirs::data_local_dir()/com.qontinui.runner`, which is the exact
/// forbidden fallback documented on [`instance_config_dir`]: the spawn-test
/// paired-profile writer would have copied the snapshot into the instance dir
/// while the child read somewhere else.
fn apply_instance_dir_env(
    cmd: &mut Command,
    runner_id: &str,
) -> Result<std::path::PathBuf, SupervisorError> {
    let dir = instance_config_dir(runner_id).ok_or_else(|| {
        SupervisorError::Process(format!(
            "Could not resolve the per-instance config dir \
             <config_dir>/com.qontinui.runner/instances/{runner_id} for runner '{runner_id}': \
             dirs::config_dir() returned None. Refusing to spawn — without \
             QONTINUI_CONFIG_DIR/QONTINUI_SECURE_STORAGE_DIR the child falls back to the \
             shared data_local_dir(), which is where paired-profile snapshots are NOT written."
        ))
    })?;
    std::fs::create_dir_all(&dir).map_err(|e| {
        SupervisorError::Process(format!(
            "Failed to create the per-instance config dir {dir:?} for runner '{runner_id}': {e}. \
             Refusing to spawn — the child would be told to use a directory that does not exist, \
             or fall back to the shared data_local_dir()."
        ))
    })?;
    cmd.env("QONTINUI_CONFIG_DIR", &dir);
    cmd.env("QONTINUI_SECURE_STORAGE_DIR", &dir);
    Ok(dir)
}

/// Apply the whole **non-primary** per-instance env block to a spawn command.
///
/// Everything a secondary (temp / named) runner needs in order to keep its
/// state off the primary's, in one place:
///
/// | Var | Keyed on | Why |
/// |---|---|---|
/// | `QONTINUI_INSTANCE_NAME` | `config.name` | Roots the runner's `instance-<sanitized>` app-data tree (`instance::scope_path`) — the terminal-session registry, dev logs, macros, prompts, contexts. |
/// | `QONTINUI_PRIMARY_PORT` | caller-resolved | The runner requires BOTH this and the instance name to classify itself as a secondary; unset makes it silently behave as a primary. |
/// | `WEBVIEW2_USER_DATA_FOLDER` | `config.id` | Isolated localStorage / IndexedDB / cookies (Windows only). |
/// | `QONTINUI_CONFIG_DIR` + `QONTINUI_SECURE_STORAGE_DIR` | `config.id` | Per-instance config + pairing store, via [`apply_instance_dir_env`]. |
/// | `QONTINUI_PRIMARY_SECURE_STORAGE_DIR` | fixed | Path *pointer* (not a credential) so a secondary can seed the primary's device machine key. |
///
/// **`QONTINUI_INSTANCE_NAME` must be unique per SPAWN, not per port.** Temp
/// ports are recycled inside 9877-9899, so a port-derived name made two
/// sequential temps resolve to the same instance dir and the second inherited
/// the first's live `terminal-sessions.json`. `routes::runners::spawn_test`
/// mints it via `process::temp_runner_instance_name(&id)`; the assertion lives in
/// `tests::non_primary_env_block_keys_instance_name_per_spawn_not_per_port`.
///
/// Extracted as a standalone function for the same reason
/// [`apply_instance_dir_env`] is: so a test can drive it against a REAL
/// [`Command`] and read the vars back, rather than re-deriving the body.
/// The primary-port lookup stays in the caller because it is async.
fn apply_non_primary_instance_env(
    cmd: &mut Command,
    config: &crate::config::RunnerConfig,
    primary_port: u16,
) -> Result<std::path::PathBuf, SupervisorError> {
    cmd.env("QONTINUI_INSTANCE_NAME", &config.name);
    cmd.env("QONTINUI_PRIMARY_PORT", primary_port.to_string());

    // Per-runner WebView2 data dir — non-primary runners get isolated
    // localStorage, IndexedDB, cookies, and caches. Primary keeps the
    // default path so its existing state (auth, terminal layouts, etc.)
    // is preserved. This prevents state bleed-over when spawning temp
    // test runners and eliminates the "216 restored terminals" problem
    // where one runner's persisted UI state floods every other runner.
    // On non-Windows the variable is ignored by other webview backends,
    // so this is harmless but keeps behavior consistent.
    #[cfg(target_os = "windows")]
    if let Some(webview_dir) = webview2_user_data_folder(&config.id, false) {
        // Ensure the folder exists so WebView2 doesn't race to create
        // it against the parent dir's permissions.
        if let Err(e) = std::fs::create_dir_all(&webview_dir) {
            warn!(
                "Failed to pre-create WebView2 data dir {:?} for runner '{}': {}",
                webview_dir, config.name, e
            );
        }
        info!(
            "Runner '{}' using isolated WebView2 data dir: {:?}",
            config.name, webview_dir
        );
        cmd.env("WEBVIEW2_USER_DATA_FOLDER", webview_dir);
    }

    // Per-instance config + secure-storage dir. `instance_config_dir` is
    // the single source of truth for this path — the spawn-test paired
    // profile writer and the instance-dir reaper resolve it through the
    // same helper so a snapshot can never be copied somewhere the child
    // does not read (see `process::instance_config_dir`). Failures here
    // abort the spawn rather than letting the child fall back to the
    // shared data dir.
    let instance_dir = apply_instance_dir_env(cmd, &config.id)?;
    debug!(
        "Runner '{}' using per-instance config dir: {:?}",
        config.name, instance_dir
    );

    // Point the non-primary runner at the PRIMARY's secure-storage dir so it
    // can seed the primary's device machine key (`dmk_`) into its own
    // isolated store and reach Tier 2 headlessly (plan
    // `2026-07-13-…-auth-remediation`, R4.1). This is a *computed path
    // pointer*, NOT the raw credential — the spawned runner decrypts the
    // primary's `auth_tokens.enc` itself with its own machine-derived
    // `SecureStorage` key (same OS user + host → same key), keeping the
    // high-privilege `dmk_` out of process listings, argv, and logs. Inert
    // until the runner reads it; the runner degrades to Tier 0/1 if absent.
    if let Some(primary_dir) = primary_secure_storage_dir() {
        cmd.env("QONTINUI_PRIMARY_SECURE_STORAGE_DIR", &primary_dir);
    }

    Ok(instance_dir)
}

async fn start_exe_mode_for_runner(
    state: &SharedState,
    managed: &ManagedRunner,
) -> Result<SpawnResult, SupervisorError> {
    // Locate the source exe. The per-runner `source_exe_override` takes
    // precedence — set by `spawn_test` when the caller passes `use_lkg: true`
    // so the runner is pinned to the last-known-good binary regardless of
    // current slot state. With no override, fall back to the parallel build
    // pool: each slot builds into its own `target-pool/slot-{k}/debug/`.
    // Prefer the slot that produced the most recent successful build; then
    // any slot with an exe on disk; then the legacy single-target path for
    // cases where no parallel build has run yet (e.g. pre-pool-era builds
    // or manual `cargo build` invocations).
    // Provenance of the artifact we end up resolving, recorded on the runner so
    // `GET /runners` can answer "which COMMIT is this runner running?" exactly,
    // rather than the mtime approximation `stale_binary` gives. Set in each
    // resolution branch below; left `None` (honest unknown) when the artifact
    // carries no provenance record.
    let mut resolved_provenance: Option<BuildProvenance> = None;
    // Full record of what resolution picked and why — published on the runner
    // so every caller (spawn response, `GET /runners`) can see WHICH path won.
    // Nothing reported the path before, which is precisely why a months-stale
    // binary served a healthy UI Bridge for a whole test iteration unnoticed.
    // Assigned exactly once, in whichever resolution arm wins (the remaining
    // arm returns early), so it is deliberately left uninitialized here rather
    // than seeded with a `None` nothing ever reads.
    let resolved_record: Option<ResolvedRunnerExe>;
    let source_exe = {
        let override_path = managed.source_exe_override.read().await.clone();
        match override_path {
            Some(p) if p.exists() => {
                info!(
                    "Runner '{}' pinned to source exe override {:?}",
                    managed.config.name, p
                );
                // The only producer of `source_exe_override` today is a
                // `use_lkg: true` spawn, so the LKG record IS this exe's
                // provenance — but only claim that when the override actually
                // points at the LKG exe. A future override producer aiming
                // elsewhere must not silently inherit the LKG's sha.
                if p == state.config.lkg_exe_path() {
                    if let Some(info) = state.build_pool.last_known_good.read().await.as_ref() {
                        resolved_provenance = Some(BuildProvenance {
                            sha: info.sha.clone(),
                            source: info.source,
                            built_from: p.to_string_lossy().to_string(),
                            built_at: info.built_at.to_rfc3339(),
                        });
                    }
                }
                resolved_record = Some(ResolvedRunnerExe {
                    mtime: file_mtime(&p),
                    path: p.clone(),
                    origin: ExeOrigin::PinnedOverride,
                    provenance: resolved_provenance.clone(),
                    unverified_warning: None,
                });
                p
            }
            Some(p) => {
                // Hard-fail: the caller explicitly pinned this runner to a
                // specific binary (typically the LKG via spawn-test
                // {use_lkg: true}). Silently falling back to slot resolution
                // would launch a different binary while the response keeps
                // claiming `used_lkg: true`, which is exactly the kind of
                // staleness the LKG path is meant to *prevent*.
                return Err(SupervisorError::Process(format!(
                    "Runner '{}' was pinned to source exe override {:?} but the file is missing. The LKG dir may have been wiped between the spawn-time check and process start; rebuild to repopulate.",
                    managed.config.name, p
                )));
            }
            None => {
                // Provenance gate (Phase 3): refuse to deploy a slot exe whose
                // provenance positively says it was built from a foreign
                // override tree, for any NON-temp runner. This is the single
                // funnel for manual start, restart_all, and the `--watchdog`
                // boot auto-start — one wire-in covers every path. Temp
                // runners are permissive (they exist to run foreign refs).
                //
                // SINGLE PICK: resolution and the gate share one
                // `resolve_source_exe_with_slot` call, so the slot the gate
                // evaluates IS the slot whose exe gets deployed. (The previous
                // shape ran `pick_slot_for_resolution` for the gate and then
                // re-picked inside `resolve_source_exe`; a build succeeding
                // between the two picks — and a fresh OVERRIDE build is
                // exactly what moves `last_successful_slot` — could deploy a
                // slot the gate never saw.)
                let is_temp = is_temp_runner(&managed.config.id);
                let mut resolved = resolve_source_exe_detailed(state).await?;
                match resolved.origin {
                    ExeOrigin::Slot(picked_id) => {
                        // Same single pick the gate evaluates, so the recorded
                        // provenance always describes the exe actually deployed.
                        resolved_provenance = resolved.provenance.clone();
                        if let Some(StartProvenanceWarning(msg)) =
                            start_provenance_gate(is_temp, picked_id, resolved.provenance.as_ref())?
                        {
                            warn!("{}", msg);
                            state
                                .logs
                                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                                .await;
                        }
                    }
                    // Non-pool artifact (preference 3) — no slot, and in
                    // practice no provenance sidecar either: the most-unknown
                    // binary in the system. Unknown here reads as REFUSE, not
                    // as "fine": this is the artifact that spawned a 54-day-old
                    // runner which came up healthy and made a whole
                    // `spawn-test {rebuild:false}` iteration measure the wrong
                    // code. Two explicit opt-ins keep the "whatever exists"
                    // case available, and both state the staleness.
                    ExeOrigin::CargoTargetDir(_) | ExeOrigin::AdoptedLocalBuild(_) => {
                        let allow = *managed.allow_unverified_exe.read().await
                            || allow_unverified_exe_env();
                        match unverified_exe_gate(allow, &resolved)? {
                            ExeIdentityVerdict::Verified => {
                                resolved_provenance = resolved.provenance.clone();
                            }
                            ExeIdentityVerdict::AllowedByOptIn(msg) => {
                                warn!("{}", msg);
                                state
                                    .logs
                                    .emit(LogSource::Supervisor, LogLevel::Warn, msg.clone())
                                    .await;
                                resolved.unverified_warning = Some(msg);
                                resolved_provenance = resolved.provenance.clone();
                            }
                        }
                    }
                    // `resolve_source_exe_detailed` never yields a pinned
                    // override — that arm is handled above, from the per-runner
                    // `source_exe_override`.
                    ExeOrigin::PinnedOverride => {}
                }
                let path = resolved.path.clone();
                resolved_record = Some(resolved);
                path
            }
        }
    };
    // Say which path won, every time. The stale-spawn incident survived a full
    // manual-test iteration because no log line and no response field named the
    // binary's directory.
    if let Some(record) = &resolved_record {
        info!(
            "Runner '{}' source exe resolved: {} (origin {}, mtime {}, identity {})",
            managed.config.name,
            record.path.display(),
            record.origin.label(),
            record.mtime_rfc3339().as_deref().unwrap_or("(unreadable)"),
            record.identity_label(),
        );
    }
    // Publish before the (fallible) copy+spawn below: the provenance describes
    // the artifact we RESOLVED, which is a fact regardless of whether the
    // subsequent copy or process start succeeds — and a failed start is exactly
    // when an operator most wants to know which binary was about to run.
    *managed.build_provenance.write().await = resolved_provenance;
    *managed.resolved_exe.write().await = resolved_record;

    // All runners use a copy of the exe to avoid locking the build artifact.
    // This allows cargo build to succeed while any runner is running.
    //
    // The first copy can fail when a previous instance of this runner died
    // with the supervisor losing its PID — Windows will hold the prior copy
    // open until the OS releases the handle. Try to remove the stale copy
    // and retry once. If that still fails, fail the spawn rather than fall
    // back to running directly from `source_exe` (the slot binary).
    //
    // Why we never fall back to source_exe: it leaves a process running
    // out of `target-pool/slot-{k}/debug/qontinui-runner.exe`, locking the
    // slot for every future cargo build. If the supervisor then loses the
    // PID, the slot becomes permanently unbuildable until the OS process
    // is killed externally — exactly the deadlock this code is meant to
    // prevent. A clean failure here surfaces the underlying problem
    // (locked previous copy, disk full, AV) instead of silently producing
    // a worse failure mode later.
    let exe_path = {
        let copy_path = state.config.runner_exe_copy_path(&managed.config);
        // Ensure the copy target's parent dir exists. Supervisor-managed trees
        // only ever materialize `target-pool/`; a tree that has never had a
        // default `cargo build` won't have `target/debug/`, so the copy below
        // would fail with `os error 3` (path not found) and 500 the spawn.
        // Create it up-front, propagating any failure as the same
        // `SupervisorError::Process` kind the copy failure would produce.
        if let Some(parent) = copy_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Err(SupervisorError::Process(format!(
                    "Failed to create runner exe copy directory {:?} for '{}': {}",
                    parent, managed.config.name, e
                )));
            }
        }
        match std::fs::copy(&source_exe, &copy_path) {
            Ok(_) => {
                info!(
                    "Copied runner exe for '{}' to {:?}",
                    managed.config.name, copy_path
                );
                copy_path
            }
            Err(first_err) => {
                warn!(
                    "Initial copy of runner exe for '{}' failed: {} — \
                     attempting to remove stale copy and retry",
                    managed.config.name, first_err
                );
                let _ = std::fs::remove_file(&copy_path);
                match std::fs::copy(&source_exe, &copy_path) {
                    Ok(_) => {
                        info!(
                            "Copied runner exe for '{}' to {:?} on retry",
                            managed.config.name, copy_path
                        );
                        copy_path
                    }
                    Err(retry_err) => {
                        return Err(SupervisorError::Process(format!(
                            "Failed to copy runner exe for '{}' from {:?} to {:?}: \
                             initial error: {}; retry error: {}. \
                             Refusing to run directly from the build slot — that \
                             would lock the slot for future builds. Resolve the \
                             copy-target lock (likely a prior runner instance the \
                             supervisor lost track of) and retry.",
                            managed.config.name, source_exe, copy_path, first_err, retry_err
                        )));
                    }
                }
            }
        }
    };

    // Deploy the `qontinui-shim.exe` sidecar next to the exe copy, in
    // lockstep with the exe itself. The runner materializes terminal identity
    // shims from the stub next to its own exe, so a runner started without a
    // fresh sidecar propagates a stale stub into every terminal's shim dir
    // (2026-07-03 incident). FAIL-OPEN: a missing/uncopyable stub logs one
    // WARN and never fails the runner start.
    match deploy_shim_sidecar(&source_exe, &exe_path) {
        ShimSidecarDeploy::Copied { to } => {
            info!(
                "Copied qontinui-shim sidecar for '{}' to {:?}",
                managed.config.name, to
            );
        }
        ShimSidecarDeploy::SameDir => {}
        ShimSidecarDeploy::SourceMissing { expected } => {
            let msg = format!(
                "No qontinui-shim.exe next to the source exe for '{}' (expected {:?}) — \
                 identity shims will be stale; rebuild (POST /runner/restart {{rebuild:true}} \
                 or spawn-test {{rebuild:true}}) to produce the sidecar",
                managed.config.name, expected
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }
        ShimSidecarDeploy::CopyFailed { from, to, error } => {
            let msg = format!(
                "Failed to copy qontinui-shim sidecar for '{}' from {:?} to {:?} ({}) — \
                 identity shims will be stale until a later runner start succeeds in copying it",
                managed.config.name, from, to, error
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }
    }

    info!(
        "Starting runner '{}' in exe mode from {:?} on port {}",
        managed.config.name, exe_path, managed.config.port
    );

    let mut cmd = Command::new(&exe_path);
    cmd.current_dir(&state.config.project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The supervisor inherits Claude Code's topology markers from whatever
        // session launched it, and would pass them to every runner and every
        // `claude` those runners spawn — so top-level sessions would claim to
        // be somebody's child forever. `ExtraEnv` runs last (see
        // `env_forwarders::default_env_forwarders`), so a caller can still
        // re-inject a marker via `POST /runners/spawn-test {extra_env:{...}}`
        // to deliberately test the marked-child case.
        .strip_inherited_claude_markers()
        .env("QONTINUI_PORT", managed.config.port.to_string());

    // QONTINUI_API_URL policy (plan 2026-07-08). Decision factored into the pure
    // `resolve_child_api_url` helper so the primary/secondary/explicit branches
    // are unit-testable without spawning a process.
    if let Some(api_url) = resolve_child_api_url(
        std::env::var("QONTINUI_API_URL").ok(),
        managed.config.kind().is_primary(),
    ) {
        cmd.env("QONTINUI_API_URL", api_url);
    }

    if let Ok(tier) = std::env::var("QONTINUI_RUNNER_TIER") {
        cmd.env("QONTINUI_RUNNER_TIER", tier);
    }

    // Windows-only creation flags: detach from console (no flash window) +
    // own process group (so the supervisor can send Ctrl-Break for graceful
    // shutdown without killing siblings).
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    // Inline non-forwarder env vars. Test-auto-login credentials are pulled
    // by `TestAutoLoginEnv` below and apply to every supervisor-spawned
    // runner — primary included — for the rationale documented on the
    // forwarder type.
    //
    // Non-primary runners additionally get `QONTINUI_INSTANCE_NAME` to skip
    // the scheduler and `QONTINUI_PRIMARY_PORT` so they can proxy process
    // commands to the primary.
    if !managed.config.kind().is_primary() {
        // Find the primary runner's port for process log proxying.
        //
        // The user-started primary isn't in the supervisor's runners
        // registry (the supervisor only tracks runners IT spawned),
        // so `state.get_primary()` returns None on most setups. Fall
        // back to the conventional default port — the runner's
        // `process_capture::primary_proxy::is_secondary()` requires
        // BOTH env vars to be set, so leaving the port unset would
        // cause every secondary to silently behave as a primary
        // (re-introducing the wrappers-dir contention this var was
        // added to fix).
        let primary_port = state
            .get_primary()
            .await
            .map(|p| p.config.port)
            .unwrap_or(crate::config::DEFAULT_RUNNER_API_PORT);

        // The rest of the block is `apply_non_primary_instance_env` so it can
        // be asserted as a WHOLE against a real `Command` in tests — see that
        // function for the per-var rationale.
        apply_non_primary_instance_env(&mut cmd, &managed.config, primary_port)?;
    }

    // Apply the registered env forwarders. Order is load-bearing — see
    // `process::env_forwarders` for the per-forwarder rationale. Adding a
    // new forwarder is one struct + one registration line in
    // `default_env_forwarders`, replacing the previous five-place edit
    // (forwarder fn + two cfg-gated call sites + state.rs storage).
    for forwarder in env_forwarders::default_env_forwarders() {
        debug!(
            "applying env forwarder '{}' for runner '{}'",
            forwarder.name(),
            managed.config.name
        );
        forwarder.apply(&mut cmd, state, managed).await;
    }

    // `PanicLogEnv` stashed the resolved per-runner panic-log path on
    // `managed.panic_log_dir` while applying — read it back so
    // `monitor_runner_process_exit` can find `runner-panic.log` after a
    // non-zero exit. Cloning out keeps the lock held for the minimum span.
    let panic_log_dir = managed.panic_log_dir.read().await.clone();

    let child = cmd
        .spawn()
        .map_err(|e| SupervisorError::Process(format!("Failed to spawn exe: {}", e)))?;

    Ok(SpawnResult {
        child,
        panic_log_dir,
    })
}

/// Default deadline for a newly-spawned runner to bind its HTTP API
/// before the supervisor declares the spawn a failure and kills the PID.
/// Override via `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS`.
const DEFAULT_FIRST_HEALTHY_TIMEOUT_SECS: u64 = 90;
const FIRST_HEALTHY_POLL_INTERVAL_SECS: u64 = 3;

fn first_healthy_timeout_secs() -> u64 {
    std::env::var("QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_FIRST_HEALTHY_TIMEOUT_SECS)
}

/// Outcome of one poll tick of the first-healthy watchdog. Extracted as a
/// pure decision so the priority rules can be asserted by unit tests
/// without spinning up a process, HTTP server, or SharedState.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstHealthyDecision {
    /// Exit quietly — the exit monitor already reaped the process.
    Abandon,
    /// HTTP /health responded; record success and exit.
    Healthy,
    /// Deadline passed and still no /health response; kill the PID.
    Kill,
    /// None of the above; sleep one poll interval and retry.
    Wait,
}

/// Decide what the watchdog should do this tick. Priority is intentional:
///   1. Abandon — process is gone, nothing we should do to it.
///   2. Healthy — responding wins even if the deadline just passed
///      (avoids a pointless kill on a runner that made it just in time).
///   3. Kill — deadline passed and still unresponsive.
///   4. Wait — not yet past deadline, keep polling.
fn decide_first_healthy(
    still_tracked: bool,
    api_responding: bool,
    deadline_passed: bool,
) -> FirstHealthyDecision {
    if !still_tracked {
        return FirstHealthyDecision::Abandon;
    }
    if api_responding {
        return FirstHealthyDecision::Healthy;
    }
    if deadline_passed {
        return FirstHealthyDecision::Kill;
    }
    FirstHealthyDecision::Wait
}

/// Outcome of the post-kill port-confirmation reap inside
/// [`stop_runner_by_id`]. Extracted as a pure decision so the escalation
/// ladder (plain kill → tree kill → kill-by-port) can be asserted by unit
/// tests without spawning a process or touching a real port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReapOutcome {
    /// The port is free (or never had a known holder) — the stop is
    /// confirmed and may return success.
    Confirmed,
    /// The port is still held — escalate by killing the holder's whole
    /// process tree, then re-confirm.
    EscalateTree,
    /// Tree-kill already attempted and the port is still held — escalate to
    /// a blind kill-by-port (kills whatever holds the port regardless of
    /// PID), then re-confirm.
    EscalatePort,
    /// Both kill escalations were tried and the port is still held — take
    /// no further action, wait one bounded backoff, and re-confirm once
    /// more. Covers a kill that landed but whose socket teardown is slow to
    /// be reflected by the OS.
    RetryAfterBackoff,
    /// Every escalation (and the backoff retry) was tried and the port is
    /// still in use — the stop must NOT report success.
    StillHeld,
}

/// Extra bounded wait before the final re-confirmation of the stop reap
/// (see [`StopReapOutcome::RetryAfterBackoff`]).
const STOP_REAP_BACKOFF_SECS: u64 = 4;

/// Decide the next reap action given the current attempt index and whether
/// the port is still in use.
///
/// `attempt` is 0-based and counts escalations already performed:
///   * 0 → after the initial graceful + PID kill, port still held: tree-kill.
///   * 1 → after the tree-kill, port still held: blind kill-by-port.
///   * 2 → both escalations exhausted: one bounded backoff, then re-confirm.
///   * ≥3 → backoff retry exhausted too: give up (StillHeld).
///
/// A free port at any attempt short-circuits to `Confirmed`.
fn decide_stop_reap(attempt: u32, port_in_use: bool) -> StopReapOutcome {
    if !port_in_use {
        return StopReapOutcome::Confirmed;
    }
    match attempt {
        0 => StopReapOutcome::EscalateTree,
        1 => StopReapOutcome::EscalatePort,
        2 => StopReapOutcome::RetryAfterBackoff,
        _ => StopReapOutcome::StillHeld,
    }
}

// =============================================================================
// Crash-Only Ambient Watchdog
// =============================================================================

/// Exponential backoff before each auto-restart attempt within one rolling
/// crash window: 5s → 30s → 120s.
const CRASH_RESTART_BACKOFF_SECS: [u64; 3] = [5, 30, 120];

/// Maximum auto-restarts per rolling [`CRASH_RESTART_WINDOW_SECS`] window
/// before the watchdog disarms itself and demands an operator.
const CRASH_RESTART_MAX_PER_WINDOW: usize = 3;

/// Rolling window (seconds) over which [`CRASH_RESTART_MAX_PER_WINDOW`] is
/// evaluated, using `WatchdogState::crash_history` timestamps.
const CRASH_RESTART_WINDOW_SECS: i64 = 30 * 60;

/// `disabled_reason` set when the crash-loop guard trips. `enabled` stays
/// true so the operator's intent remains visible — the reason field is what
/// blocks further restarts until an operator clears it (via
/// `POST /runners/{id}/watchdog {reset_attempts: true}`).
pub const CRASH_LOOP_DISABLED_REASON: &str = "crash loop — operator required";

/// Env kill-switch: `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1` disables all
/// crash-only auto-restarts without a rebuild or code change.
pub fn crash_restart_env_disabled() -> bool {
    std::env::var("QONTINUI_SUPERVISOR_NO_CRASH_RESTART")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// The single source of truth for whether crash-only auto-restart is globally
/// armed: the `--watchdog` CLI flag was set at launch AND the
/// `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1` kill-switch is not set. This is the
/// bit that actually gates [`maybe_crash_restart`] and is **independent** of
/// any per-runner [`crate::state::WatchdogState::enabled`] (which defaults
/// `true` for the primary). Surfaced in `/health` + `/runners` so
/// "watchdog enabled" can never imply protection that isn't there.
pub fn crash_restart_globally_armed(config: &crate::config::SupervisorConfig) -> bool {
    config.watchdog_enabled_at_start && !crash_restart_env_disabled()
}

/// Outcome of the crash-only watchdog decision for one observed exit.
/// Extracted as a pure function (like [`decide_first_healthy`] /
/// [`decide_stop_reap`]) so the priority, backoff, and rolling-window rules
/// can be asserted by unit tests without processes or SharedState.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashRestartDecision {
    /// Schedule an auto-restart after `delay_secs` of backoff. `attempt` is
    /// 1-based within the current rolling window (for the "attempt N/3" log).
    Restart { attempt: u32, delay_secs: u64 },
    /// Crash budget exhausted for the rolling window — set
    /// [`CRASH_LOOP_DISABLED_REASON`] and stop restarting.
    Disarm,
    /// No Child handle was ever held — the supervisor did not spawn this
    /// process, so it has no provenance to restart it.
    SkipNoChildHandle,
    /// The exit followed an operator stop request (`stop_requested` latched
    /// true) — never restart.
    SkipOperatorStop,
    /// The process exited cleanly (code 0) — a deliberate shutdown (window
    /// close, internal exit), not a crash.
    SkipCleanExit,
    /// Crash-restart is not globally armed (`--watchdog` absent, or the
    /// `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1` kill-switch is set).
    SkipNotArmed,
    /// Per-runner `WatchdogState.enabled` is false.
    SkipDisabled,
    /// The watchdog already disarmed itself (`disabled_reason` set) — an
    /// operator must reset it before restarts resume.
    SkipDisarmed,
}

/// Decide whether an observed runner exit warrants a crash auto-restart.
/// Priority is intentional: provenance and operator intent always win over
/// arming state, and the crash-loop guard is evaluated last so `Disarm`
/// only fires for an exit that would otherwise have restarted.
fn decide_crash_restart(
    had_child_handle: bool,
    stop_requested: bool,
    clean_exit: bool,
    globally_armed: bool,
    per_runner_enabled: bool,
    already_disarmed: bool,
    restarts_in_window: usize,
) -> CrashRestartDecision {
    if !had_child_handle {
        return CrashRestartDecision::SkipNoChildHandle;
    }
    if stop_requested {
        return CrashRestartDecision::SkipOperatorStop;
    }
    if clean_exit {
        return CrashRestartDecision::SkipCleanExit;
    }
    if !globally_armed {
        return CrashRestartDecision::SkipNotArmed;
    }
    if !per_runner_enabled {
        return CrashRestartDecision::SkipDisabled;
    }
    if already_disarmed {
        return CrashRestartDecision::SkipDisarmed;
    }
    if restarts_in_window >= CRASH_RESTART_MAX_PER_WINDOW {
        return CrashRestartDecision::Disarm;
    }
    CrashRestartDecision::Restart {
        attempt: restarts_in_window as u32 + 1,
        delay_secs: CRASH_RESTART_BACKOFF_SECS[restarts_in_window],
    }
}

// =============================================================================
// Serving Watchdog — liveness is per-route SERVING, not process-exists
// =============================================================================
//
// Plan `2026-09-03-runner-zombie-serving-watchdog`.
//
// The crash arm above is armed on process EXIT. Three times in nine days the
// primary runner's HTTP API stopped answering while the process stayed alive
// and kept its outbound work going, for 12 hours to 5 days, with
// `crash_restart_armed: true, restart_attempts: 0` throughout — an armed
// watchdog is not a recovery path for a process that never crashes. The
// supervisor detected every occurrence correctly and had no route from that
// signal to an actor permitted to use it.
//
// This is that route. It is deliberately narrower than the crash arm: it fires
// only when the supervisor's OWN process-subtree census finds zero live
// `claude` sessions under the runner — a zero census is the statement "there is
// nothing to destroy", made from evidence a wedged door cannot corrupt. Every
// prior plan's reason ("never destroy in-flight sessions") is preserved; what
// is removed is the UNCONDITIONAL refusal, which had turned "protect sessions"
// into "protect a process that has already lost them".

/// Maximum serving restarts per rolling [`SERVING_RESTART_WINDOW_SECS`] window
/// before the serving arm disarms itself.
///
/// No backoff ladder: the threshold IS the delay, and a recurrence inside the
/// window is the runner's accept-path leak, which a fourth restart does not fix.
const SERVING_RESTART_MAX_PER_WINDOW: usize = 3;

/// Rolling window for [`SERVING_RESTART_MAX_PER_WINDOW`].
const SERVING_RESTART_WINDOW_SECS: i64 = 24 * 60 * 60;

/// `serving_disabled_reason` set when the serving-loop guard trips. Distinct
/// from [`CRASH_LOOP_DISABLED_REASON`] so the two arms never mask each other.
pub const SERVING_LOOP_DISABLED_REASON: &str = "serving restart loop — operator required";

/// Env kill-switch: `QONTINUI_SUPERVISOR_NO_SERVING_RESTART=1`.
///
/// Separate from the crash arm's switch on purpose. The `--watchdog` CLI flag
/// is shared (one operator intent: "supervise the primary"); the kill-switches
/// are not, so an operator can disarm one arm without losing the other.
pub fn serving_restart_env_disabled() -> bool {
    std::env::var("QONTINUI_SUPERVISOR_NO_SERVING_RESTART")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// The global arm for serving restarts — the twin of
/// [`crash_restart_globally_armed`], sharing `--watchdog` and nothing else.
pub fn serving_restart_globally_armed(config: &crate::config::SupervisorConfig) -> bool {
    config.watchdog_enabled_at_start && !serving_restart_env_disabled()
}

/// Outcome of the serving watchdog decision for one escalating tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServingRestartDecision {
    /// Restart now, subject to the readiness gate (which consults the census).
    /// `attempt` is 1-based within the rolling window.
    Restart { attempt: u32 },
    /// Serving budget exhausted for the window — set
    /// [`SERVING_LOOP_DISABLED_REASON`] and stop.
    Disarm,
    /// Not the wedge class. `Responding`, `Stopped`, and — per Design decision
    /// 4 — `Unknown`, which is a runner that has NEVER answered: it has nothing
    /// to prove it is this failure mode, so it is alerted, never restarted.
    SkipNotWedged,
    /// Wedged, but not for long enough yet.
    SkipBelowThreshold,
    /// A temp runner. The max-age reaper owns those.
    SkipTempRunner,
    /// An operator is already stopping or restarting this runner.
    SkipOperatorIntent,
    /// A serving restart taken on an earlier tick has not returned yet.
    SkipInFlight,
    /// Not globally armed (`--watchdog` absent, or the kill-switch is set).
    SkipNotArmed,
    /// Per-runner `WatchdogState.enabled` is false.
    SkipDisabled,
    /// The serving arm already disarmed itself.
    SkipDisarmed,
}

/// Decide whether a wedged runner warrants a serving restart. Pure.
///
/// Priority mirrors [`decide_crash_restart`]: classification and operator
/// intent first, arming next, the loop guard last, so `Disarm` only fires for a
/// wedge that would otherwise have restarted.
///
/// `silent_for_secs` is `(now - at)` from
/// [`RunnerLiveness::UnresponsiveSince(at)`](crate::state::RunnerLiveness) —
/// **never a tick count.** The health-cache loop is not 2 s per tick when
/// `health_cache_notify` fires an immediate refresh, and the stamp is the same
/// value `/runners` publishes, so the log, the API and this decision cannot
/// disagree about how long the runner has been silent.
#[allow(clippy::too_many_arguments)]
pub fn decide_serving_restart(
    liveness: crate::state::RunnerLiveness,
    silent_for_secs: u64,
    threshold_secs: u64,
    kind: &qontinui_types::wire::runner_kind::RunnerKind,
    stop_requested: bool,
    restart_requested: bool,
    in_flight: bool,
    globally_armed: bool,
    per_runner_enabled: bool,
    already_disarmed: bool,
    restarts_in_window: usize,
) -> ServingRestartDecision {
    if !matches!(liveness, crate::state::RunnerLiveness::UnresponsiveSince(_)) {
        return ServingRestartDecision::SkipNotWedged;
    }
    if kind.is_temp() {
        return ServingRestartDecision::SkipTempRunner;
    }
    if stop_requested || restart_requested {
        return ServingRestartDecision::SkipOperatorIntent;
    }
    if in_flight {
        return ServingRestartDecision::SkipInFlight;
    }
    if silent_for_secs < threshold_secs {
        return ServingRestartDecision::SkipBelowThreshold;
    }
    if !globally_armed {
        return ServingRestartDecision::SkipNotArmed;
    }
    if !per_runner_enabled {
        return ServingRestartDecision::SkipDisabled;
    }
    if already_disarmed {
        return ServingRestartDecision::SkipDisarmed;
    }
    if restarts_in_window >= SERVING_RESTART_MAX_PER_WINDOW {
        return ServingRestartDecision::Disarm;
    }
    ServingRestartDecision::Restart {
        attempt: restarts_in_window as u32 + 1,
    }
}

/// Emit one serving-watchdog diagnostic event and — fire and forget — one coord
/// alert.
///
/// **The alert is never awaited on a decision path.** A coord outage must not
/// delay a restart by a single tick, so this spawns and returns; the
/// diagnostics ring and the operator log buffer are the durable record on this
/// box regardless of what coord does.
#[allow(clippy::too_many_arguments)]
async fn emit_serving_event(
    state: &SharedState,
    runner_id: &str,
    port: u16,
    pid: Option<u32>,
    silent_for_secs: u64,
    event: crate::diagnostics::ServingEvent,
    census: Option<serde_json::Value>,
    detail: Option<String>,
) {
    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::ServingWatchdog {
            runner_id: runner_id.to_string(),
            event,
            pid,
            port,
            silent_for_secs,
            census: census.clone(),
            detail: detail.clone(),
        });

    let notice = crate::fleet::ServingWatchdogNotice {
        runner_id: runner_id.to_string(),
        event,
        pid,
        port,
        silent_for_secs,
        census_json: census,
        detail,
    };
    tokio::spawn(async move {
        match crate::fleet::notify_serving_watchdog(&notice).await {
            Ok(outcome) => {
                debug!("serving watchdog alert delivered to coord: {outcome:?}");
            }
            Err(e) => {
                // A coord failure of any kind is a Warn and nothing else.
                warn!("serving watchdog alert could not reach coord: {e}");
            }
        }
    });
}

/// The serving watchdog hook, called from the health-cache escalation loop on
/// every escalating tick for a runner.
///
/// `should_alert` carries the loop's existing re-escalation cadence
/// (`UNRESPONSIVE_ESCALATION_TICKS`, then every `UNRESPONSIVE_REESCALATION_TICKS`).
/// **Two cadences, deliberately:** the DECISION is evaluated every escalating
/// tick — it is a pure function over state already held, with no I/O — because
/// evaluating it only on the alert cadence would make the effective threshold
/// floor 330 s no matter what the operator configured. The ALERT keeps the
/// slower cadence so a long wedge does not spam coord.
pub async fn maybe_serving_restart(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    liveness: crate::state::RunnerLiveness,
    should_alert: bool,
) {
    use crate::diagnostics::ServingEvent;

    let runner_id = managed.config.id.clone();
    let runner_name = managed.config.name.clone();
    let port = managed.config.port;
    let kind = managed.config.kind();
    let now = chrono::Utc::now();

    let silent_for_secs = match liveness {
        crate::state::RunnerLiveness::UnresponsiveSince(at) => {
            (now - at).num_seconds().max(0) as u64
        }
        _ => 0,
    };

    let (pid, started_at, stop_requested, restart_requested, adopted) = {
        let r = managed.runner.read().await;
        (
            r.pid,
            r.started_at,
            r.stop_requested,
            r.restart_requested,
            r.process.is_none(),
        )
    };

    // Design decision 4: an ADOPTED runner that holds its port and has NEVER
    // answered reads `Unknown` forever (adoption stamps `running`/`pid` from a
    // port probe and never stamps `last_seen_responding_at`). That shape is
    // occurrence 1's, and it is never restarted — the supervisor cannot back
    // the classification — but it is never SILENT again either.
    if matches!(liveness, crate::state::RunnerLiveness::Unknown) {
        if adopted && should_alert {
            let msg = format!(
                "serving watchdog: adopted runner '{runner_name}' (port {port}, pid {pid:?}) \
                 holds its port and has NEVER answered its HTTP API. Alerting, not restarting — \
                 the supervisor cannot classify this as a wedge, so it will not act on it."
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Error, msg)
                .await;
            emit_serving_event(
                state,
                &runner_id,
                port,
                pid,
                silent_for_secs,
                ServingEvent::NeverAnsweredSinceAdoption,
                None,
                Some("never-answered-since-adoption".to_string()),
            )
            .await;
        }
        return;
    }

    let threshold_secs = crate::config::serving_restart_after_secs();
    let globally_armed = serving_restart_globally_armed(&state.config);

    // Decide + bookkeep under ONE watchdog write lock, so two escalating ticks
    // can never both observe the same window count or the same in-flight bit.
    let decision = {
        let mut wd = managed.watchdog.write().await;
        wd.serving_history
            .retain(|t| (now - *t).num_seconds() < SERVING_RESTART_WINDOW_SECS);
        let decision = decide_serving_restart(
            liveness,
            silent_for_secs,
            threshold_secs,
            &kind,
            stop_requested,
            restart_requested,
            wd.serving_restart_in_flight,
            globally_armed,
            wd.enabled,
            wd.serving_disabled_reason.is_some(),
            wd.serving_history.len(),
        );
        match decision {
            ServingRestartDecision::Restart { .. } => {
                wd.serving_restart_attempts += 1;
                wd.last_serving_restart_at = Some(now);
                wd.serving_history.push(now);
                wd.serving_restart_in_flight = true;
            }
            ServingRestartDecision::Disarm => {
                wd.serving_disabled_reason = Some(SERVING_LOOP_DISABLED_REASON.to_string());
            }
            _ => {}
        }
        decision
    };

    match decision {
        ServingRestartDecision::Restart { attempt } => {
            let msg = format!(
                "serving watchdog: runner '{runner_name}' (port {port}, pid {pid:?}) has held its \
                 port with a silent HTTP API for {silent_for_secs}s (threshold {threshold_secs}s) \
                 — attempting restart {attempt}/{SERVING_RESTART_MAX_PER_WINDOW}. The restart is \
                 still gated: it proceeds only if the supervisor's process-subtree census finds \
                 ZERO live `claude` sessions under this runner."
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
            state.notify_health_change();

            emit_serving_event(
                state,
                &runner_id,
                port,
                pid,
                silent_for_secs,
                ServingEvent::Threshold,
                None,
                Some(format!(
                    "attempt {attempt}/{SERVING_RESTART_MAX_PER_WINDOW}, threshold \
                     {threshold_secs}s"
                )),
            )
            .await;

            let state = state.clone();
            let managed = managed.clone();
            tokio::spawn(async move {
                // The census safeguard must hold UNCONDITIONALLY for this arm.
                //
                // The readiness gate normally supplies it, but `enforce`
                // exempts an explicitly unprotected runner
                // (`POST /runners/{id}/protect {"protected": false}`) — an
                // operator opt-out that means "I can stop this without being
                // nagged", NOT "a background watchdog may kill it while
                // sessions are live". Without this pre-check an unprotected
                // wedged runner would be auto-restarted with NO evidence at
                // all, and this plan's central guarantee — restart only on a
                // zero census — would quietly not hold. A protected runner
                // (the primary is `protected: true` by default) skips it and
                // pays nothing: the gate answers for it.
                let unprotected_refusal = if managed.is_protected().await {
                    None
                } else {
                    match crate::process::subtree_census::take_census_for_port(
                        pid,
                        port,
                        started_at.map(|t| t.timestamp()),
                    )
                    .await
                    {
                        Ok(census)
                            if matches!(
                                census.verdict(),
                                crate::process::subtree_census::CensusVerdict::Idle
                            ) =>
                        {
                            None
                        }
                        Ok(census) => Some((
                            format!(
                                "serving watchdog refuses to restart UNPROTECTED runner \
                                 '{runner_name}': its own process-subtree census counted {} live \
                                 `claude` process(es). The readiness gate exempts an unprotected \
                                 runner, so this arm counts for itself.",
                                census.live.len()
                            ),
                            Some(census.to_json()),
                        )),
                        Err(e) => Some((
                            format!(
                                "serving watchdog refuses to restart UNPROTECTED runner \
                                 '{runner_name}': its process-subtree census could not be taken \
                                 ({e}), so nothing is known about what is live on it."
                            ),
                            None,
                        )),
                    }
                };

                let result = match unprotected_refusal {
                    None => {
                        restart_runner_by_id(
                            &state,
                            &runner_id,
                            false,
                            RestartSource::ServingWatchdog,
                            false,
                            true,
                        )
                        .await
                    }
                    Some((message, census)) => Err(SupervisorError::RestartUnsafe(Box::new(
                        crate::restart_readiness::RefusalDetail {
                            cause: "sessions_live",
                            payload: serde_json::json!({
                                "error": "restart_refused_unsafe",
                                "cause": "sessions_live",
                                "source": "supervisor_census",
                                "runner_id": runner_id,
                                "message": message,
                                "census": census,
                            }),
                            message,
                        },
                    ))),
                };

                // Bookkeeping and the latch release happen under ONE lock.
                //
                // The latch is released on EVERY path — a restart that failed
                // must not silence the arm for the runner's lifetime. And a
                // REFUSAL gives its attempt back: the census counting live
                // sessions is the safeguard working, not a restart that was
                // taken, so three refusals must not disarm the arm without a
                // single restart having happened. Releasing the latch first
                // and refunding second would let the next 2 s tick push its
                // own history entry in between, and the refund would pop the
                // wrong one.
                let refunded = matches!(result, Err(SupervisorError::RestartUnsafe(_)));
                {
                    let mut wd = managed.watchdog.write().await;
                    if refunded {
                        wd.serving_restart_attempts = wd.serving_restart_attempts.saturating_sub(1);
                        wd.serving_history.pop();
                    }
                    wd.serving_restart_in_flight = false;
                }

                match result {
                    Ok(()) => {
                        let msg = format!(
                            "serving watchdog RESTARTED runner '{runner_name}' \
                             (attempt {attempt}/{SERVING_RESTART_MAX_PER_WINDOW}) after \
                             {silent_for_secs}s of a held port with a silent API."
                        );
                        warn!("{}", msg);
                        state
                            .logs
                            .emit(LogSource::Supervisor, LogLevel::Error, msg)
                            .await;
                        emit_serving_event(
                            &state,
                            &runner_id,
                            port,
                            pid,
                            silent_for_secs,
                            ServingEvent::RestartTaken,
                            None,
                            None,
                        )
                        .await;
                    }
                    // The readiness gate refused: the census counted live
                    // sessions (or could not be taken). NOT an attempt that
                    // failed — a refusal, which is the safeguard working.
                    Err(SupervisorError::RestartUnsafe(detail)) => {
                        let msg = format!(
                            "serving watchdog REFUSED to restart runner '{runner_name}': \
                             {}",
                            detail.message
                        );
                        warn!("{}", msg);
                        state
                            .logs
                            .emit(LogSource::Supervisor, LogLevel::Error, msg)
                            .await;
                        emit_serving_event(
                            &state,
                            &runner_id,
                            port,
                            pid,
                            silent_for_secs,
                            ServingEvent::RestartRefusedSessionsLive,
                            detail.payload.get("census").cloned(),
                            Some(detail.cause.to_string()),
                        )
                        .await;
                    }
                    Err(e) => {
                        let msg = format!(
                            "serving watchdog FAILED to restart runner '{runner_name}' \
                             (attempt {attempt}/{SERVING_RESTART_MAX_PER_WINDOW}): {e}"
                        );
                        error!("{}", msg);
                        state
                            .logs
                            .emit(LogSource::Supervisor, LogLevel::Error, msg)
                            .await;
                        emit_serving_event(
                            &state,
                            &runner_id,
                            port,
                            pid,
                            silent_for_secs,
                            ServingEvent::RestartFailed,
                            None,
                            Some(e.to_string()),
                        )
                        .await;
                    }
                }
                state.notify_health_change();
            });
        }
        ServingRestartDecision::Disarm => {
            let msg = format!(
                "serving watchdog DISARMED for runner '{runner_name}': \
                 {SERVING_RESTART_MAX_PER_WINDOW} serving restarts inside \
                 {SERVING_RESTART_WINDOW_SECS}s. A fourth restart does not fix a recurring \
                 wedge — an operator is required (POST /runners/{runner_id}/watchdog \
                 {{\"enabled\": true, \"reset_attempts\": true}})."
            );
            error!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Error, msg)
                .await;
            state.notify_health_change();
            emit_serving_event(
                state,
                &runner_id,
                port,
                pid,
                silent_for_secs,
                ServingEvent::Disarmed,
                None,
                Some(SERVING_LOOP_DISABLED_REASON.to_string()),
            )
            .await;
        }
        other => {
            debug!(
                "serving watchdog: no action for runner '{}' ({:?}, silent {}s/{}s)",
                runner_id, other, silent_for_secs, threshold_secs
            );
        }
    }
}

/// Crash-only ambient watchdog hook, called by `monitor_runner_process_exit`
/// after every observed exit of a supervisor-spawned runner. Evaluates
/// [`decide_crash_restart`], does the `WatchdogState` bookkeeping, and — for
/// a `Restart` — spawns a detached task that waits out the backoff and
/// funnels through [`start_runner_by_id`] (so the provenance start gate
/// applies). Never blocks the exit monitor.
async fn maybe_crash_restart(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    had_child_handle: bool,
    stop_requested_at_exit: bool,
    clean_exit: bool,
) {
    let runner_id = managed.config.id.clone();
    let runner_name = managed.config.name.clone();
    let globally_armed = crash_restart_globally_armed(&state.config);
    let now = chrono::Utc::now();

    // Decide + bookkeep under one watchdog write lock so two exits can't
    // both observe the same window count.
    let decision = {
        let mut wd = managed.watchdog.write().await;
        // Prune history that fell out of the rolling window; keeps the vec
        // bounded and makes `len()` the window count.
        wd.crash_history
            .retain(|t| (now - *t).num_seconds() < CRASH_RESTART_WINDOW_SECS);
        let decision = decide_crash_restart(
            had_child_handle,
            stop_requested_at_exit,
            clean_exit,
            globally_armed,
            wd.enabled,
            wd.disabled_reason.is_some(),
            wd.crash_history.len(),
        );
        match decision {
            CrashRestartDecision::Restart { .. } => {
                wd.restart_attempts += 1;
                wd.last_restart_at = Some(now);
                wd.crash_history.push(now);
            }
            CrashRestartDecision::Disarm => {
                wd.disabled_reason = Some(CRASH_LOOP_DISABLED_REASON.to_string());
            }
            _ => {}
        }
        decision
    };

    match decision {
        CrashRestartDecision::Restart {
            attempt,
            delay_secs,
        } => {
            let msg = format!(
                "crash-only watchdog restarting runner '{}' (attempt {}/{}) after {}s backoff",
                runner_name, attempt, CRASH_RESTART_MAX_PER_WINDOW, delay_secs
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
            state.notify_health_change();

            let state = state.clone();
            let managed = managed.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;

                // Re-check intent right before starting: the operator may
                // have stopped, started, or disabled the runner during the
                // backoff window.
                {
                    let runner = managed.runner.read().await;
                    if runner.running || runner.stop_requested {
                        info!(
                            "crash-only watchdog: skipping restart of runner '{}' — \
                             state changed during backoff (running={}, stop_requested={})",
                            runner_name, runner.running, runner.stop_requested
                        );
                        return;
                    }
                }
                {
                    let wd = managed.watchdog.read().await;
                    if !wd.enabled || wd.disabled_reason.is_some() {
                        info!(
                            "crash-only watchdog: skipping restart of runner '{}' — \
                             watchdog disabled during backoff",
                            runner_name
                        );
                        return;
                    }
                }

                state
                    .diagnostics
                    .write()
                    .await
                    .emit(DiagnosticEventKind::RestartStarted {
                        source: RestartSource::Watchdog,
                        rebuild: false,
                    });
                let started = std::time::Instant::now();
                match start_runner_by_id(&state, &runner_id).await {
                    Ok(()) => {
                        let msg = format!(
                            "crash-only watchdog restarted runner '{}' (attempt {}/{})",
                            runner_name, attempt, CRASH_RESTART_MAX_PER_WINDOW
                        );
                        info!("{}", msg);
                        state
                            .logs
                            .emit(LogSource::Supervisor, LogLevel::Info, msg)
                            .await;
                        state.diagnostics.write().await.emit(
                            DiagnosticEventKind::RestartCompleted {
                                source: RestartSource::Watchdog,
                                rebuild: false,
                                duration_secs: started.elapsed().as_secs_f64(),
                                build_duration_secs: None,
                            },
                        );
                    }
                    Err(e) => {
                        let msg = format!(
                            "crash-only watchdog FAILED to restart runner '{}' \
                             (attempt {}/{}): {}",
                            runner_name, attempt, CRASH_RESTART_MAX_PER_WINDOW, e
                        );
                        error!("{}", msg);
                        state
                            .logs
                            .emit(LogSource::Supervisor, LogLevel::Error, msg)
                            .await;
                        state
                            .diagnostics
                            .write()
                            .await
                            .emit(DiagnosticEventKind::RestartFailed {
                                source: RestartSource::Watchdog,
                                error: e.to_string(),
                            });
                    }
                }
            });
        }
        CrashRestartDecision::Disarm => {
            let msg = format!(
                "crash-only watchdog DISARMED for runner '{}': {} auto-restarts within \
                 {} minutes — {}. Clear with POST /runners/{}/watchdog \
                 {{\"enabled\": true, \"reset_attempts\": true}}",
                runner_name,
                CRASH_RESTART_MAX_PER_WINDOW,
                CRASH_RESTART_WINDOW_SECS / 60,
                CRASH_LOOP_DISABLED_REASON,
                runner_id
            );
            error!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Error, msg.clone())
                .await;
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::RestartFailed {
                    source: RestartSource::Watchdog,
                    error: msg,
                });
            state.notify_health_change();
        }
        // A GENUINE crash (a supervisor-spawned Child that exited uncleanly
        // without an operator stop) that goes unrestarted because crash-restart
        // is not armed / has disarmed itself must leave a PERSISTED breadcrumb,
        // not a debug line the buffer and log file both drop. This is the
        // observability half of the 2026-07-20 incident: the primary crashed,
        // the supervisor was up but launched without `--watchdog`, and the skip
        // was swallowed at `debug!`.
        skip @ (CrashRestartDecision::SkipNotArmed | CrashRestartDecision::SkipDisarmed)
            if had_child_handle && !clean_exit && !stop_requested_at_exit =>
        {
            let reason = if matches!(skip, CrashRestartDecision::SkipNotArmed) {
                "not armed (started without --watchdog)"
            } else {
                "disarmed (crash-loop guard tripped — operator required)"
            };
            let msg = format!(
                "runner '{}' crashed (unclean exit) but crash-restart is {}; not restarting.",
                runner_name, reason
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }
        // An UNREQUESTED clean exit: the supervisor spawned this runner, nobody
        // asked it to stop, and it exited 0 anyway. Crash-only means we
        // correctly do not restart it — code 0 is also what an operator closing
        // the runner window produces, and resurrecting a deliberately closed
        // app would be worse than the silence. But the two are indistinguishable
        // from out here, so the one thing this MUST NOT do is stay quiet.
        //
        // The 2026-08-06 incident is the case in point: the runner's webview
        // recovery destroyed and rebuilt its window, a late `ExitRequested`
        // slipped past the veto, and the process exited 0 at 03:00. It read as
        // an ordinary shutdown, so nothing surfaced and it stayed down ~6 hours.
        // Same lesson as the 2026-07-20 breadcrumb above: an exit nobody asked
        // for is a reportable event even when not restarting it is correct.
        CrashRestartDecision::SkipCleanExit if had_child_handle && !stop_requested_at_exit => {
            let msg = format!(
                "runner '{}' exited cleanly (code 0) but NO stop was requested. Not restarting \
                 (crash-only watchdog: code 0 is indistinguishable from an operator closing the \
                 window). If nobody closed it, this is an unrequested self-exit — check the \
                 runner log around the exit for the cause.",
                runner_name
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
        }
        skip => {
            debug!(
                "crash-only watchdog: no restart for runner '{}' exit ({:?})",
                runner_name, skip
            );
        }
    }
}

/// Watchdog for a newly-spawned runner. If its HTTP API doesn't respond
/// within `first_healthy_timeout_secs()`, the process is considered
/// wedged (alive but hung during startup — e.g. stuck on a DDL, on
/// WebView2 init, or inside a subprocess spawn) and the PID is killed.
/// `monitor_runner_process_exit` observes the resulting exit and cleans
/// up runner state naturally.
///
/// Scope: runs once per supervisor-initiated start. Does not auto-restart
/// and does not touch runners started outside the supervisor.
async fn watch_first_healthy(state: SharedState, managed: Arc<ManagedRunner>, pid: u32) {
    let timeout_secs = first_healthy_timeout_secs();
    let runner_name = managed.config.name.clone();
    let port = managed.config.port;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    let poll = Duration::from_secs(FIRST_HEALTHY_POLL_INTERVAL_SECS);

    loop {
        let still_tracked = {
            let runner = managed.runner.read().await;
            runner.pid == Some(pid) && runner.running
        };
        let api_responding = if still_tracked {
            crate::process::port::is_runner_responding(port).await
        } else {
            false
        };
        let deadline_passed = tokio::time::Instant::now() >= deadline;

        match decide_first_healthy(still_tracked, api_responding, deadline_passed) {
            FirstHealthyDecision::Abandon => {
                debug!(
                    "First-healthy watchdog for runner '{}' (PID {}) exiting — process no longer tracked",
                    runner_name, pid
                );
                return;
            }
            FirstHealthyDecision::Healthy => {
                info!(
                    "Runner '{}' (PID {}) HTTP API responsive — first-healthy watchdog clear",
                    runner_name, pid
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        format!(
                            "Runner '{}' healthy within first-healthy budget",
                            runner_name
                        ),
                    )
                    .await;
                return;
            }
            FirstHealthyDecision::Kill => {
                let msg = format!(
                    "Runner '{}' (PID {}) did not bind HTTP API within {}s — killing wedged process",
                    runner_name, pid, timeout_secs
                );
                error!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Error, msg)
                    .await;

                // Tree-kill so the wedged runner's child processes release the
                // port too — cross-platform via the proc_kill facade.
                match crate::process::proc_kill::kill_by_pid_tree(pid).await {
                    Ok(true) => info!(
                        "First-healthy watchdog killed wedged runner '{}' PID {}",
                        runner_name, pid
                    ),
                    Ok(false) => warn!(
                        "First-healthy watchdog: PID {} for runner '{}' no longer present",
                        pid, runner_name
                    ),
                    Err(e) => error!(
                        "First-healthy watchdog: failed to kill PID {} for runner '{}': {}",
                        pid, runner_name, e
                    ),
                }
                return;
            }
            FirstHealthyDecision::Wait => {
                tokio::time::sleep(poll).await;
            }
        }
    }
}

/// Monitor a specific runner's process for exit.
///
/// Returns a concrete `BoxFuture` (not an `async fn` opaque type) on
/// purpose: the crash-only watchdog makes this function transitively
/// self-referential (`start_managed_runner` spawns this monitor → the
/// monitor calls [`maybe_crash_restart`] → which spawns a task calling
/// [`start_runner_by_id`] → which calls `start_managed_runner`). With an
/// opaque `async fn` future the compiler cannot resolve the cyclic hidden
/// type; boxing at this edge makes the spawn argument inside
/// `start_managed_runner` a concrete `Send` type and breaks the cycle.
fn monitor_runner_process_exit(
    state: SharedState,
    managed: Arc<ManagedRunner>,
    runner_id: String,
) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(monitor_runner_process_exit_inner(state, managed, runner_id))
}

async fn monitor_runner_process_exit_inner(
    state: SharedState,
    managed: Arc<ManagedRunner>,
    _runner_id: String,
) {
    let is_primary = managed.config.kind().is_primary();
    let runner_name = managed.config.name.clone();

    // Take the child out of state so we can await without holding the lock.
    let child = {
        let mut runner = managed.runner.write().await;
        runner.process.take()
    };

    // Provenance: a Some(child) here means WE spawned this process via
    // `start_managed_runner` — the only place that stores a Child. The
    // crash-only watchdog must never restart a runner the supervisor didn't
    // spawn, so this is threaded into the restart decision.
    let had_child_handle = child.is_some();

    let exit_status = if let Some(mut child) = child {
        match child.wait().await {
            Ok(status) => Some(status),
            Err(e) => {
                error!("Error waiting for runner '{}' process: {}", runner_name, e);
                None
            }
        }
    } else {
        None
    };

    // Update per-runner state. Latch `stop_requested` in the same critical
    // section, BEFORE publishing `running = false`: the operator stop path
    // (`stop_runner_by_id`) only proceeds past its graceful-wait once it
    // observes `running == false`, and the flag is only ever cleared by a
    // subsequent start — so the value read here is exactly the operator's
    // intent for THIS exit, race-free.
    let stop_requested_at_exit = {
        let mut runner = managed.runner.write().await;
        let latched = runner.stop_requested;
        runner.running = false;
        runner.process = None;
        runner.pid = None;
        latched
    };

    // Update legacy state for primary
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.running = false;
        runner.process = None;
        runner.pid = None;
    }

    state.notify_health_change();

    if let Some(status) = exit_status {
        let msg = if status.success() {
            format!("Runner '{}' process exited normally", runner_name)
        } else {
            format!(
                "Runner '{}' process exited with status: {}",
                runner_name, status
            )
        };

        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, &msg)
            .await;
        info!("{}", msg);

        // If the process died non-zero, look for a startup-panic log. A
        // panic that fires during early init (DB connect, Tauri builder,
        // axum router construction) doesn't flow through stderr in a
        // shape our buffered reader can latch onto, so this file is the
        // only place the panic payload actually lives.
        if !status.success() {
            check_and_record_panic_log(&state, &managed, &runner_name).await;
        }
    } else {
        let msg = format!("Runner '{}' process terminated unexpectedly", runner_name);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, &msg)
            .await;
        warn!("{}", msg);
        // Also check on unexpected termination — the child is gone either
        // way, and if a panic log exists within the freshness window it's
        // almost certainly the cause.
        check_and_record_panic_log(&state, &managed, &runner_name).await;
    }

    // Crash-only ambient watchdog: if this exit was a crash (not an
    // operator stop, not a clean exit) and the watchdog is armed for this
    // runner, schedule an auto-restart with exponential backoff + a
    // crash-loop guard. A wait() error (exit_status == None) on a child we
    // actually spawned is treated as a crash — the process is gone and we
    // can't prove it exited cleanly.
    let clean_exit = exit_status.map(|s| s.success()).unwrap_or(false);
    maybe_crash_restart(
        &state,
        &managed,
        had_child_handle,
        stop_requested_at_exit,
        clean_exit,
    )
    .await;
}

/// Look for `<panic_log_dir>/runner-panic.log`; if it exists and its
/// timestamp is within [`PANIC_LOG_FRESHNESS_SECS`] of now, parse it and
/// stash it on the managed runner so `GET /runners` can surface it. Also
/// emit a tagged `[runner-panic]` ERROR into the supervisor log buffer.
///
/// All errors are swallowed at debug level — panic telemetry is strictly
/// best-effort and must never interfere with normal process-exit handling.
async fn check_and_record_panic_log(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    runner_name: &str,
) {
    let dir_opt = managed.panic_log_dir.read().await.clone();
    let path = crate::process::panic_log::resolve_panic_log_path(dir_opt.as_deref());

    let Some(parsed) = crate::process::panic_log::parse_panic_file(&path) else {
        debug!(
            "No panic log found for runner '{}' at {:?}",
            runner_name, path
        );
        return;
    };

    // Freshness gate — a stale file from a previous boot shouldn't be
    // attributed to the exit we just observed.
    let now = chrono::Utc::now();
    if !crate::process::panic_log::is_fresh(&parsed, now) {
        debug!(
            "Panic log at {:?} is stale (timestamp {} vs now {}) — ignoring",
            path, parsed.timestamp, now
        );
        return;
    }

    let location_str = parsed.location.as_deref().unwrap_or("<unknown>");
    let payload_preview: String = parsed.payload.chars().take(500).collect();
    let backtrace_preview = parsed.backtrace_preview.as_deref().unwrap_or("");
    let msg = format!(
        "[runner-panic] Runner '{}' panicked during startup at {}:\n{}\n{}",
        runner_name, location_str, payload_preview, backtrace_preview,
    );
    state
        .logs
        .emit(LogSource::Supervisor, LogLevel::Error, msg.clone())
        .await;
    error!("{}", msg);

    // Stash on the managed runner for JSON surfacing. The reaper may
    // later drop the runner from the registry — that's fine, callers
    // passing `?include_stopped=true` to the logs endpoint see the
    // panic via the stopped-cache snapshot once we extend that path.
    let mut slot = managed.recent_panic.write().await;
    *slot = Some(parsed);
}

/// POST to the runner's UI Bridge close-request endpoint so that
/// Tauri's WindowEvent::CloseRequested fires on the runner side and its
/// graceful teardown hooks run (e.g. UsbTransport::release_all, which
/// removes adb forwards). Best-effort: any error — including a hung
/// endpoint — is swallowed at debug level, and the caller falls through
/// to child.kill() after the wait window elapses.
/// Best-effort, bounded pre-stop drain (Phase 2 of
/// `2026-06-06-runner-dev-loop-and-restart-resilience`).
///
/// POST `http://127.0.0.1:<port>/drain` BEFORE the graceful close-request +
/// taskkill so the runner flushes in-flight AI turns to `output_log`, stashes
/// each session's dirty worktree to `refs/wip/*`, and heartbeats coord claims.
/// The runner-side `/drain` is itself hard-bounded (default 25s) and idempotent
/// with its own exit-seam drain. This call adds a short client-side timeout on
/// top so a wedged runner can NEVER block the restart — on any error/timeout we
/// log and fall through to the existing close-request + kill.
async fn request_drain(state: &SharedState, port: u16, runner_name: &str) {
    // Client-side cap. Generous enough to let the runner's bounded drain finish
    // a normal turn-flush + stash, but it never blocks the restart: on timeout
    // we proceed straight to the close-request + kill below.
    const DRAIN_REQUEST_TIMEOUT_SECS: u64 = 30;
    let url = format!("http://127.0.0.1:{}/drain", port);
    let result = state
        .http_client
        .post(&url)
        .timeout(Duration::from_secs(DRAIN_REQUEST_TIMEOUT_SECS))
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.unwrap_or_default();
            let msg = format!(
                "Drained runner '{}' (port {}) before stop: {}",
                runner_name, port, body
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
        }
        Ok(resp) => {
            // Non-2xx (e.g. an older runner with no /drain route → 404). Not
            // fatal — the runner either drains on its own exit seam or there's
            // nothing to drain. Proceed to the close-request + kill.
            let msg = format!(
                "Pre-stop drain for runner '{}' (port {}) returned {} — proceeding to stop",
                runner_name,
                port,
                resp.status()
            );
            debug!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Debug, msg)
                .await;
        }
        Err(e) => {
            let msg = format!(
                "Pre-stop drain for runner '{}' (port {}) failed: {} — proceeding to stop",
                runner_name, port, e
            );
            debug!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Debug, msg)
                .await;
        }
    }
}

async fn request_graceful_stop(state: &SharedState, port: u16, runner_name: &str) {
    let url = format!(
        "http://127.0.0.1:{}/ui-bridge/control/page/close-request",
        port
    );
    let result = state
        .http_client
        .post(&url)
        .timeout(Duration::from_millis(
            RUNNER_GRACEFUL_STOP_REQUEST_TIMEOUT_MS,
        ))
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {
            let msg = format!(
                "Requested graceful stop for runner '{}' via close-request (port {})",
                runner_name, port
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
        }
        Ok(resp) => {
            let msg = format!(
                "Graceful close-request for runner '{}' (port {}) returned {} — falling through to kill",
                runner_name,
                port,
                resp.status()
            );
            debug!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Debug, msg)
                .await;
        }
        Err(e) => {
            let msg = format!(
                "Graceful close-request for runner '{}' (port {}) failed: {} — falling through to kill",
                runner_name, port, e
            );
            debug!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Debug, msg)
                .await;
        }
    }
}

/// Identify the process a stop should target, WITHOUT killing anything.
///
/// Drives [`resolve_stop_target`] from the three live sources. The third —
/// image-path identity via sysinfo — is the one that makes a stop survivable
/// when the netstat listener probe cannot answer: every runner runs from its
/// own deterministic `runner_exe_copy_path`, so the process can be named with
/// no subprocess, no locale dependence, and no listening socket required. It
/// is the same signal `orphan_scan::classify_exe_owner` already trusts to
/// decide a surviving primary is the operator's and must not be killed.
async fn resolve_stop_target_for(
    state: &SharedState,
    managed: &ManagedRunner,
    port: u16,
    registry_pid: Option<u32>,
) -> StopTarget {
    // Short-circuit: the registry already knows, so neither probe is worth
    // paying for.
    if let Some(pid) = registry_pid {
        return resolve_stop_target(Some(pid), Ok(None), &[]);
    }

    // Both probes go through the cross-platform `proc_kill` facade. They used
    // to be Windows-only, with the non-Windows arm hard-coding "this build has
    // no Windows process probes compiled in" — honest, but it meant macOS/Linux
    // could never identify a stop target and so never killed anything (D7).
    // `Err` still means UNKNOWN, never "the port was idle".
    let listener = crate::process::proc_kill::find_pid_on_port(port)
        .await
        .map_err(|e| e.to_string());
    let exe_copy = state.config.runner_exe_copy_path(&managed.config);
    let by_exe = crate::process::proc_kill::find_pids_holding_exe(&exe_copy).await;
    resolve_stop_target(None, listener, &by_exe)
}

/// Adapter for [`verify_target_image`]: read the target's live image path and
/// compare it with the runner's own exe copy. sysinfo reads the image path on
/// every platform, so the pre-kill identity re-check guards the Unix kill path
/// too rather than only the Windows one.
async fn verify_kill_target(
    state: &SharedState,
    managed: &ManagedRunner,
    stop_target: &StopTarget,
    pid: u32,
) -> StopVerification {
    let source = match stop_target {
        StopTarget::Found { source, .. } => source.clone(),
        // Unreachable in practice (there is no pid without a Found), but a
        // conservative default beats an unwrap.
        StopTarget::NotFound { .. } => PidSource::ListenerProbe,
    };
    let expected = state.config.runner_exe_copy_path(&managed.config);
    let observed = crate::process::proc_kill::pid_exe_path(pid).await;
    verify_target_image(&source, observed.as_deref(), &expected)
}

/// Stop a specific runner by ID. Kills by PID (not by process name).
///
/// # `force` is real now
///
/// `force: false` (the default a caller gets by sending no body) consults the
/// runner's own `GET /restart-readiness` verdict first and **refuses** with
/// [`SupervisorError::RestartUnsafe`] when the runner reports live agent
/// sessions — or when the verdict cannot be established at all, which is
/// UNKNOWN and therefore also a refusal. `force: true` proceeds and logs the
/// verdict it overrode.
///
/// This function previously took **no** force parameter, while the routes above
/// it parsed one and threw it away. See [`crate::restart_readiness`] for the
/// exemptions (temp runners, explicitly-unprotected runners, and runners the
/// supervisor has no evidence are alive).
pub async fn stop_runner_by_id(
    state: &SharedState,
    runner_id: &str,
    force: bool,
) -> Result<(), SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;

    // The gate runs BEFORE `stop_requested` is latched and before any kill
    // rung: a refused stop must leave the runner — and the crash-watchdog's
    // stop-intent marker — exactly as it found them.
    crate::restart_readiness::enforce(
        state,
        &managed,
        crate::restart_readiness::GateAction::Stop,
        force,
    )
    .await?;

    let runner_name = managed.config.name.clone();
    let port = managed.config.port;
    let is_primary = managed.config.kind().is_primary();

    {
        let mut runner = managed.runner.write().await;
        runner.stop_requested = true;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Stopping runner '{}'...", runner_name),
        )
        .await;

    // The Child handle is owned by the `monitor_runner_process_exit` task that
    // was spawned when the runner started — it calls `runner.process.take()`
    // immediately so it can await `child.wait()` without holding the lock. So
    // by the time we get here, `managed.runner.process` is always None, and
    // we have to work via (a) the graceful HTTP close endpoint, (b) an
    // identified PID, and (c) the `running` flag that the monitor task flips
    // to false when the process exits.
    //
    // For an ADOPTED runner there is no monitor task and no Child at all, and
    // the stored PID can be None indefinitely — so (b) is resolved below from
    // three independent sources rather than read straight out of the registry.
    let registry_pid = {
        let runner = managed.runner.read().await;
        runner.pid
    };

    // Ledger of what this stop ACTUALLY does, so a failure can say which rungs
    // ran and which had nothing to aim at. See `stop_ledger` for why a fixed
    // "after PID kill, tree-kill, kill-by-port" sentence was worse than no
    // message at all.
    let mut ledger = StopLedger::new();

    // --- Target identification (no kills here) ---------------------------
    //
    // An ADOPTED runner (the startup orphan scan inherits a primary this
    // supervisor did not spawn) can sit at `pid: None` indefinitely: the health
    // cache's PID-recovery tick is the only writer, and it is gated on the
    // netstat listener probe. When that probe cannot answer, EVERY PID-based
    // rung below silently becomes a no-op and the stop fails while the runner
    // is plainly alive (observed 2026-07-31, PID 8872 on port 9876).
    //
    // So resolve the target from three independent sources before touching
    // anything, and fall back to DETERMINISTIC IDENTITY — the runner runs from
    // its own `runner_exe_copy_path`, so sysinfo can name it with no
    // subprocess, no locale, and no listening socket required. This is the
    // same identity signal `orphan_scan::classify_exe_owner` already trusts.
    //
    // Deliberately identification-only: the previous revision killed the
    // recovered PID here, on the spot, which skipped `request_drain` and
    // `request_graceful_stop` entirely — a hard kill of the operator's primary
    // with no chance to flush in-flight AI turns or stash dirty worktrees.
    // Identify now, kill through the normal ladder below.
    let stop_target = resolve_stop_target_for(state, &managed, port, registry_pid).await;

    // `mut` because the pre-kill identity re-check below can retract the
    // target.
    let mut pid_to_kill = match &stop_target {
        StopTarget::Found { pid, source } => {
            if registry_pid.is_none() {
                let msg = format!(
                    "Runner '{}': registry tracked no PID; identified PID {} via {} — \
                     stopping that process",
                    runner_name, pid, source
                );
                info!("{}", msg);
                state
                    .logs
                    .emit(LogSource::Supervisor, LogLevel::Info, msg)
                    .await;
                // Publish it so the dashboard, the exit monitor and any racing
                // health probe all see the same process we are about to stop.
                {
                    let mut runner = managed.runner.write().await;
                    if runner.pid.is_none() {
                        runner.pid = Some(*pid);
                    }
                }
            }
            Some(*pid)
        }
        StopTarget::NotFound { why } => {
            warn!(
                "Runner '{}' stop: could not identify a target process — {}. \
                 The port-level rung is the only one left.",
                runner_name, why
            );
            None
        }
    };
    // The reason every later PID-based rung records when it has nothing to
    // aim at. Starts as the identification failure and is REPLACED by the
    // retraction reason if the pre-kill identity check refuses the target —
    // otherwise the tree-kill rung would render "DID NOT RUN ()" with an
    // empty parenthesis, which is the content-free message this whole change
    // exists to remove.
    let mut no_target_why = match &stop_target {
        StopTarget::Found { .. } => String::new(),
        StopTarget::NotFound { why } => why.clone(),
    };

    // 0. Pre-stop drain (Phase 2): give the runner a bounded chance to flush
    //    in-flight AI turns, stash dirty worktrees to refs/wip/*, and persist
    //    coord claims BEFORE we close/kill it. Best-effort + hard-bounded — a
    //    wedged runner never blocks the stop; we fall straight through to the
    //    close-request + kill on any error/timeout.
    request_drain(state, port, &runner_name).await;

    // 1. Graceful-first: ask the runner to close itself via the same endpoint
    //    the UI uses, so WindowEvent::CloseRequested fires and its teardown
    //    hooks run (notably UsbTransport::release_all, which removes adb
    //    forwards — see qontinui-runner §1.6a). Best-effort: on any failure
    //    we fall through to the PID kill below.
    request_graceful_stop(state, port, &runner_name).await;

    // 2. Poll the monitor's `running` flag for up to
    //    RUNNER_GRACEFUL_STOP_TIMEOUT_MS. When the runner exits, the monitor
    //    task sets running=false — that's our signal that graceful worked.
    let graceful_deadline =
        std::time::Instant::now() + Duration::from_millis(RUNNER_GRACEFUL_STOP_TIMEOUT_MS);
    let mut exited_gracefully = false;
    while std::time::Instant::now() < graceful_deadline {
        if !managed.runner.read().await.running {
            let msg = format!(
                "Runner '{}' exited gracefully after close-request",
                runner_name
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
            exited_gracefully = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if !exited_gracefully {
        let msg = format!(
            "Graceful stop timed out for runner '{}' after {}ms, falling through to taskkill",
            runner_name, RUNNER_GRACEFUL_STOP_TIMEOUT_MS
        );
        info!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Info, msg)
            .await;
    }

    ledger.ran(
        StopStrategy::GracefulClose,
        format!("port {}", port),
        exited_gracefully,
    );

    // 3. Kill by PID. This is a no-op if the process already exited gracefully
    //    (taskkill reports "PID not found" at debug level) and the primary
    //    mechanism otherwise.
    //
    // Re-verified immediately before firing: identification happened up to
    // ~34s ago (drain + close-request + graceful poll), and a probe-derived
    // PID was only ever a candidate. See `stop_ledger::verify_target_image`.
    // The kill itself goes through the cross-platform `proc_kill` facade — it
    // used to be Windows-only, with the non-Windows arm recording "not a
    // Windows host" and killing nothing, which is the D7 orphan leak.
    match pid_to_kill {
        Some(pid) => match verify_kill_target(state, &managed, &stop_target, pid).await {
            StopVerification::Proceed { note } => {
                if let Some(note) = note {
                    warn!("Runner '{}' stop: PID {} — {}", runner_name, pid, note);
                }
                let killed = crate::process::proc_kill::kill_by_pid(pid)
                    .await
                    .unwrap_or(false);
                ledger.ran(StopStrategy::PidKill, format!("PID {}", pid), killed);
            }
            StopVerification::Refuse { why } => {
                warn!(
                    "Runner '{}' stop: REFUSING to kill PID {} — {}",
                    runner_name, pid, why
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Runner '{}' stop: refused to kill PID {} — {}",
                            runner_name, pid, why
                        ),
                    )
                    .await;
                ledger.no_target(StopStrategy::PidKill, why.clone());
                no_target_why = why;
                pid_to_kill = None;
            }
        },
        None => ledger.no_target(StopStrategy::PidKill, no_target_why.clone()),
    }

    // 4. Confirm the process is actually gone before reporting success.
    //
    // Returning "stopped" while the OS process survives and keeps the port
    // held was an observed failure. The reap below gates success on a
    // confirmed port-free check (poll up to ~5s) and escalates the kill if
    // a survivor lingers:
    //   attempt 0 → wait_for_port_free; if still held, tree-kill the PID
    //               (`/F /T`) so the runner's *child* processes — which a
    //               plain `kill_by_pid` (`/F`, no `/T`) leaves alive — die
    //               too and release the port;
    //   attempt 1 → blind kill-by-port (kills whatever is LISTENING,
    //               regardless of PID — covers a re-parented orphan whose
    //               PID we never tracked);
    //   attempt 2 → give up and surface a `Process` error so the caller does
    //               NOT believe the runner stopped.
    // Each escalation re-confirms via `wait_for_port_free` before deciding
    // the next step, so a kill that lands late still resolves to success.
    {
        let mut attempt: u32 = 0;
        loop {
            // Confirm-first: poll the port for up to 5s. A graceful exit or a
            // prior kill that lands late resolves here without escalation.
            let port_free = wait_for_port_free(port, 5).await;
            match decide_stop_reap(attempt, !port_free) {
                StopReapOutcome::Confirmed => break,
                StopReapOutcome::EscalateTree => {
                    warn!(
                        "Port {} still in use after stopping runner '{}', \
                         escalating to tree-kill (attempt {})",
                        port, runner_name, attempt
                    );
                    {
                        match pid_to_kill {
                            Some(pid) => {
                                let killed = crate::process::proc_kill::kill_by_pid_tree(pid)
                                    .await
                                    .unwrap_or(false);
                                ledger.ran(
                                    StopStrategy::TreeKill,
                                    format!("PID {} (+ child tree)", pid),
                                    killed,
                                );
                            }
                            None => ledger.no_target(StopStrategy::TreeKill, no_target_why.clone()),
                        }
                        // Also catch a survivor whose PID we never tracked
                        // (orphan adopted on a port we knew but no PID for).
                        // A probe that could not RUN is UNKNOWN — never a
                        // kill; the next escalation step re-confirms anyway.
                        match crate::process::proc_kill::find_pid_on_port(port).await {
                            Ok(Some(live_pid)) if Some(live_pid) != pid_to_kill => {
                                let killed = crate::process::proc_kill::kill_by_pid_tree(live_pid)
                                    .await
                                    .unwrap_or(false);
                                ledger.ran(
                                    StopStrategy::TreeKill,
                                    format!("untracked listener PID {}", live_pid),
                                    killed,
                                );
                            }
                            Ok(_) => {}
                            Err(e) => warn!(
                                "Stop-reap escalation for runner '{}': listener probe on port \
                                 {} failed ({}) — skipping the untracked-survivor kill",
                                runner_name, port, e
                            ),
                        }
                    }
                }
                StopReapOutcome::EscalatePort => {
                    warn!(
                        "Port {} STILL in use for runner '{}' after tree-kill, \
                         escalating to kill-by-port (attempt {})",
                        port, runner_name, attempt
                    );
                    match crate::process::proc_kill::kill_by_port(port).await {
                        Ok(killed) => {
                            ledger.ran(StopStrategy::KillByPort, format!("port {}", port), killed)
                        }
                        // The listener probe could not answer, so kill-by-port
                        // had nothing to aim at. Recording this as a NON-RUN is
                        // the whole point: it is exactly the case the old fixed
                        // failure sentence misreported as an attempted kill.
                        Err(e) => ledger.no_target(
                            StopStrategy::KillByPort,
                            format!("listener probe could not answer: {}", e),
                        ),
                    }
                }
                StopReapOutcome::RetryAfterBackoff => {
                    warn!(
                        "Port {} still in use for runner '{}' after every kill \
                         escalation — waiting {}s and re-confirming once before \
                         giving up (attempt {})",
                        port, runner_name, STOP_REAP_BACKOFF_SECS, attempt
                    );
                    tokio::time::sleep(Duration::from_secs(STOP_REAP_BACKOFF_SECS)).await;
                }
                StopReapOutcome::StillHeld => {
                    // Derived from the LEDGER, never asserted. The old fixed
                    // sentence claimed "after PID kill, tree-kill,
                    // kill-by-port" unconditionally — on 2026-07-31 none of
                    // the three had a target and the message sent diagnosis
                    // hunting for an unkillable process for hours. See
                    // `process::stop_ledger`.
                    let msg = ledger.failure_message(&runner_name, port, STOP_REAP_BACKOFF_SECS);
                    warn!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Warn, msg.clone())
                        .await;
                    // Leave the runner in the registry (not removed, not marked
                    // stopped) so the caller and the dashboard see it as still
                    // present rather than a phantom "stopped" entry holding a
                    // port. `stop_requested` deliberately stays LATCHED (true):
                    // the operator asked for a stop that we could not confirm —
                    // if the process dies moments later of its own accord, the
                    // exit monitor must still classify that death as
                    // operator-intended, NOT as a crash to auto-restart. The
                    // flag is cleared on the next start (see
                    // `start_managed_runner`).
                    return Err(SupervisorError::Process(msg));
                }
            }
            attempt += 1;
        }
    }

    // Snapshot the runner's state for post-mortem cache BEFORE clearing
    // `started_at` below. The crash-summary endpoint reports
    // `duration_alive_ms` computed from `started_at`/`stopped_at`, so the
    // snapshot must capture the value before the per-runner reset wipes it.
    // (For non-test runners we still capture so future post-mortem queries
    // can see the most recent stop event; the cache is bounded so this is
    // cheap.)
    let runner_id = managed.config.id.clone();
    let pre_clear_snapshot = if runner_id.starts_with("test-") {
        Some(
            crate::process::stopped_cache::snapshot_from_managed(
                managed.as_ref(),
                None,
                crate::process::stopped_cache::StopReason::GracefulStop,
            )
            .await,
        )
    } else {
        None
    };

    // 4. Update per-runner state.
    //
    // `stop_requested` deliberately stays TRUE here. Its lifecycle is
    // "latched on stop request, cleared on next start" (see
    // `start_managed_runner`): the exit monitor reads it to distinguish an
    // operator-initiated stop from a crash, and clearing it at stop
    // *completion* raced the monitor's read (set → kill → child.wait()
    // returns → port-confirm → reset) — a lost race would auto-restart a
    // runner the operator deliberately stopped.
    {
        let mut runner = managed.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
    }

    // Update legacy state for primary
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Runner '{}' stopped", runner_name),
        )
        .await;
    info!("Runner '{}' stopped", runner_name);

    // Auto-remove ephemeral test runners (spawned via /runners/spawn-test)
    // from the runners map so they don't accumulate over time. These have IDs
    // prefixed with "test-" and are not persisted to settings.
    if runner_id.starts_with("test-") {
        if let Some(snapshot) = pre_clear_snapshot {
            let mut cache = state.stopped_runners.write().await;
            crate::process::stopped_cache::insert_and_evict(&mut cache, snapshot);
        }

        let mut runners = state.runners.write().await;
        if runners.remove(&runner_id).is_some() {
            info!(
                "Removed ephemeral test runner '{}' (id: {}) from state",
                runner_name, runner_id
            );
        }
        drop(runners);
        // Also remove the test runner's isolated WebView2 data folder so its
        // localStorage, cookies, and caches don't accumulate on disk.
        #[cfg(windows)]
        {
            if let Err(e) = remove_webview2_user_data_folder(&runner_id, false).await {
                warn!(
                    "Failed to remove WebView2 data folder for test runner '{}': {}",
                    runner_id, e
                );
            }
            // And the per-instance app data dirs (dev-logs, restate journal,
            // macros, prompts, playwright, contexts) — keyed off the config
            // name because that's what the runner received as
            // QONTINUI_INSTANCE_NAME.
            if let Err(e) = remove_runner_app_data_dirs(&runner_name, false).await {
                warn!(
                    "Failed to remove per-instance app data for test runner '{}': {}",
                    runner_name, e
                );
            }
            if let Err(e) = remove_instance_config_dir(&runner_id, false).await {
                warn!(
                    "Failed to remove instance config dir for test runner '{}': {}",
                    runner_id, e
                );
            }
        }

        // Clean up the per-runner exe copy to prevent disk bloat.
        // Each copy is ~200MB + ~1.3GB PDB; without cleanup, orphaned copies
        // accumulated to ~200GB in a recent audit.
        let exe_copy = state.config.runner_exe_copy_path(&managed.config);
        if exe_copy.exists() {
            if let Err(e) = std::fs::remove_file(&exe_copy) {
                warn!("Failed to remove runner exe copy {:?}: {}", exe_copy, e);
            } else {
                info!("Removed runner exe copy {:?}", exe_copy);
            }
        }
        // Also try to remove the PDB file (same name but .pdb extension)
        let pdb_copy = exe_copy.with_extension("pdb");
        if pdb_copy.exists() {
            let _ = std::fs::remove_file(&pdb_copy);
        }
    }

    state.notify_health_change();
    managed.health_cache_notify.notify_one();

    Ok(())
}

/// Phase B: build the primary from a fresh `origin/main` worktree.
///
/// Materializes (or refreshes) a managed `origin/main` worktree via
/// [`crate::spawn_worktree::prepare_worktree`] — which fetches origin itself and
/// pins the `qontinui-schemas` sibling to `origin/main`, handling the
/// shared-path-dep-drift hazard for the primary too — then compiles its
/// `src-tauri` with an explicit [`BuildSourceKind::OriginMain`] carrying the
/// worktree's resolved SHA. The result is provenance-classified `origin_main`:
/// LKG-eligible and startable as the primary, unlike a spawn-test `Override`.
///
/// The chosen source + resolved sha are logged before the (long) build so the
/// next operator restart self-documents which commit the primary will run
/// (Phase B verification is deferred to that real restart — it can't be
/// exercised against the live primary from a session).
async fn primary_rebuild_from_origin_main(state: &SharedState) -> Result<(), SupervisorError> {
    let prepared =
        crate::spawn_worktree::prepare_worktree(&state.config.project_dir, "origin/main").await?;

    let msg = format!(
        "Primary rebuild: building from origin/main worktree (resolved_sha={}, src_tauri={:?}). \
         This is the default Phase B path — the primary runs latest-green-main, not the working \
         checkout. Pass from_working_tree:true to compile the live tree instead.",
        prepared.resolved_sha, prepared.src_tauri
    );
    info!("{}", msg);
    state.logs.emit(LogSource::Build, LogLevel::Info, msg).await;

    crate::build_monitor::run_cargo_build_with_dir(
        state,
        Some("primary-rebuild:origin/main".to_string()),
        Some(prepared.src_tauri.clone()),
        false,
        crate::build_monitor::BuildSourceKind::OriginMain {
            resolved_sha: prepared.resolved_sha.clone(),
        },
    )
    .await
}

/// Pure gate for [`restart_runner_by_id`]: is an automated restart of this
/// runner refused outright?
///
/// Temp runners (`test-*`) are never blocked — the supervisor's own lifecycle
/// restarts them. For every other runner:
///
/// - [`RestartSource::Manual`] is admitted: an operator asked.
/// - [`RestartSource::Watchdog`] stays BLOCKED. It is the wire value
///   (`{"source": "watchdog"}` on `POST /runners/{id}/restart`), so any HTTP
///   caller can claim it, and "any caller may automate a restart of the
///   operator's primary" is precisely what this block exists to refuse.
/// - [`RestartSource::ServingWatchdog`] is admitted. It is unconstructible from
///   the wire (`diagnostics::restart_source_from_wire` never yields it — pinned
///   by test), so the only producer is the in-process serving-watchdog decision
///   (plan `2026-09-03-runner-zombie-serving-watchdog`, Phase 4), which fires
///   only on `UnresponsiveSince` past the serving threshold AND a zero
///   process-subtree census. That decision is the supervisor's own, not a
///   caller's claim, which is the whole difference.
pub fn automated_restart_blocked(is_temp: bool, source: &RestartSource) -> bool {
    !(is_temp || source.is_manual() || matches!(source, RestartSource::ServingWatchdog))
}

/// Pure stop predicate for [`restart_runner_by_id`]: is there evidence of a
/// live process to stop before starting?
///
/// The old predicate was `running` alone, and for a user-managed runner
/// `running` is overwritten from the `/health` probe every health-cache tick —
/// so a WEDGED primary (process alive, port held, API silent) read
/// `running == false`, the stop was skipped, and `start_managed_runner` spawned
/// a second runner against the held port (finding S2). Any of the three
/// signals is evidence of life: the tracked flag, a known PID, or a listener
/// on the runner's port (`CachedPortHealth::runner_port_open`, the same
/// port-level fact the liveness classifier derives `UnresponsiveSince` from).
/// `stop_runner_by_id` already copes with a runner it has no `Child` for; this
/// predicate is what gets it *reached*.
pub fn restart_should_stop(running: bool, pid: Option<u32>, port_open: bool) -> bool {
    running || pid.is_some() || port_open
}

/// Pure half of [`refuse_if_port_held_by_live_runner`]: refuse only on
/// POSITIVE evidence — a listener on the port AND the listener's PID resolved
/// to a `qontinui-runner` image. A held port whose holder is not a runner (or
/// could not be named) is left to the spawn's own bind failure, exactly as
/// before; this guard exists to stop the S2 double-spawn, not to arbitrate
/// every port collision.
pub fn port_held_by_live_runner(listening: bool, pid_is_runner: bool) -> bool {
    listening && pid_is_runner
}

/// Refuse a start when `port` is already held by a LIVE `qontinui-runner`
/// process (finding S2: the double-spawn against a held port).
///
/// Called by [`start_managed_runner`] before it resolves an exe or spawns
/// anything, so it covers every caller — manual start, `restart_all`, the
/// `--watchdog` boot auto-start, the crash watchdog and the serving watchdog.
/// Three probes, each honest about its own failure:
///
/// 1. [`crate::process::port::is_port_listening`] (bind probe). Not held →
///    `Ok(())`.
/// 2. [`crate::process::proc_kill::find_pid_on_port`]. `Ok(None)` (the probe
///    RAN and named nobody) → `Ok(())`. `Err(_)` (the probe could not run —
///    no `lsof`/`netstat`) → the holder is UNKNOWN; log it and proceed, since
///    refusal keys on positive evidence only and the spawn's bind failure is
///    the pre-existing backstop.
/// 3. [`crate::process::proc_kill::is_qontinui_runner_pid`]. A runner image →
///    [`SupervisorError::PortHeldByLiveRunner`]; anything else → `Ok(())`.
///
/// Note for tests: on Unix `find_pid_on_port` deliberately excludes the
/// calling process's own PID, so a listener held by the test process itself
/// resolves as `Ok(None)` (step 2), never as a runner.
pub async fn refuse_if_port_held_by_live_runner(port: u16) -> Result<(), SupervisorError> {
    if !crate::process::port::is_port_listening(port) {
        return Ok(());
    }
    let pid = match crate::process::proc_kill::find_pid_on_port(port).await {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            debug!(
                "port {} is held but the listener probe named no PID; not refusing the start \
                 (refusal keys on a positively identified runner image)",
                port
            );
            return Ok(());
        }
        Err(e) => {
            warn!(
                "port {} is held but the listener probe could not run ({}); the holder is \
                 UNKNOWN — proceeding to the spawn, whose bind failure is the backstop",
                port, e
            );
            return Ok(());
        }
    };
    let pid_is_runner = crate::process::proc_kill::is_qontinui_runner_pid(pid);
    if port_held_by_live_runner(true, pid_is_runner) {
        return Err(SupervisorError::PortHeldByLiveRunner { port, pid });
    }
    debug!(
        "port {} is held by non-runner PID {}; not refusing the start on its account",
        port, pid
    );
    Ok(())
}

/// Restart a specific runner by ID.
///
/// Automated sources are rejected for non-temp runners — see
/// [`automated_restart_blocked`] for the one exception (the in-process serving
/// watchdog) and why the wire `watchdog` value is not it.
///
/// `restart_requested` is latched `true` only while a restart is genuinely in
/// flight: it is set after the readiness gate admits the request and cleared
/// on EVERY exit of the stop → build → start sequence, success or failure
/// (`restart_after_gate` is the single body; this wrapper clears the flag
/// once, before matching its result). Before this shape the flag was cleared
/// only on the success path, so one failed stop or start left it `true` for
/// the runner's lifetime — and the serving watchdog's `SkipOperatorIntent`
/// reads that flag, so its first failed attempt would have silenced it for
/// good (plan `2026-09-03-runner-zombie-serving-watchdog`, Phase 3 vet
/// finding).
pub async fn restart_runner_by_id(
    state: &SharedState,
    runner_id: &str,
    rebuild: bool,
    source: RestartSource,
    force: bool,
    from_working_tree: bool,
) -> Result<(), SupervisorError> {
    if automated_restart_blocked(is_temp_runner(runner_id), &source) {
        let msg = format!(
            "Automated restart of non-temp runner '{}' blocked (source: {}). \
             Only manual restarts — and the supervisor's own serving watchdog — \
             are allowed for user runners.",
            runner_id, source
        );
        warn!("{}", msg);
        return Err(SupervisorError::Validation(msg));
    }

    let restart_start = std::time::Instant::now();

    state
        .diagnostics
        .write()
        .await
        .emit(DiagnosticEventKind::RestartStarted {
            source: source.clone(),
            rebuild,
        });

    // Up-front existence check (option (b) for already-removed ids). A
    // `test-*` runner is auto-removed from `state.runners` when it stops, so
    // a restart on an already-stopped temp id finds nothing here. Reject it
    // cleanly WITHOUT killing or touching anything (we never had a process to
    // act on) and with a message that names the auto-remove so the caller
    // knows to spawn a fresh temp runner rather than restart this dead id.
    let managed = match state.get_runner(runner_id).await {
        Some(m) => m,
        None => {
            // `RunnerNotFound`'s Display prepends "Runner not found: ", so we
            // pass only the descriptive suffix for the temp case and the bare
            // id for the non-temp case to avoid a doubled prefix.
            let detail = if is_temp_runner(runner_id) {
                format!(
                    "{} — ephemeral test runners are auto-removed when stopped and \
                     cannot be restarted; spawn a new one via POST /runners/spawn-test",
                    runner_id
                )
            } else {
                runner_id.to_string()
            };
            return Err(SupervisorError::RunnerNotFound(detail));
        }
    };

    // Readiness gate. Evaluated here, at the restart boundary, rather than being
    // left to the nested `stop_runner_by_id` call below — a restart that is
    // going to be refused must be refused BEFORE `restart_requested` is
    // latched, and before a `rebuild` kicks off a 40-minute cargo build whose
    // whole point was to be started.
    if let Err(e) = crate::restart_readiness::enforce(
        state,
        &managed,
        crate::restart_readiness::GateAction::Restart,
        force,
    )
    .await
    {
        state
            .diagnostics
            .write()
            .await
            .emit(DiagnosticEventKind::RestartFailed {
                source,
                error: e.to_string(),
            });
        return Err(e);
    }

    {
        let mut runner = managed.runner.write().await;
        runner.restart_requested = true;
    }

    // The single in-flight body. Whatever it returns, the latch comes off
    // BEFORE the result is inspected — there is exactly one exit from here.
    let outcome = restart_after_gate(state, &managed, runner_id, rebuild, from_working_tree).await;

    {
        let mut runner = managed.runner.write().await;
        runner.restart_requested = false;
    }

    match outcome {
        Ok(build_duration) => {
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::RestartCompleted {
                    source,
                    rebuild,
                    duration_secs: restart_start.elapsed().as_secs_f64(),
                    build_duration_secs: build_duration,
                });
            Ok(())
        }
        Err(e) => {
            state
                .diagnostics
                .write()
                .await
                .emit(DiagnosticEventKind::RestartFailed {
                    source,
                    error: e.to_string(),
                });
            Err(e)
        }
    }
}

/// The stop → (build) → start body of [`restart_runner_by_id`], run only
/// after the readiness gate admitted the request and `restart_requested` was
/// latched. Returns the build duration (`None` when no rebuild was asked for).
///
/// Kept as a separate function so the caller can clear `restart_requested`
/// exactly once regardless of which step failed — every `?` here lands on the
/// same clearing line in the wrapper.
async fn restart_after_gate(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    runner_id: &str,
    rebuild: bool,
    from_working_tree: bool,
) -> Result<Option<f64>, SupervisorError> {
    // Stop on EVIDENCE OF LIFE, not on `running`. For a user-managed runner
    // `running` mirrors the last `/health` probe, so a wedged primary — the
    // process alive and holding its port, the API silent — reads
    // `running == false`. Gating the stop on that flag skipped the stop and
    // spawned a second runner against the held port (finding S2, PID 247696).
    // The port-level fact comes from the per-runner `CachedPortHealth`
    // (`managed.cached_health`), the same snapshot the liveness classifier
    // reads — never from the SSE `CachedRunnerHealth` vector.
    let (running, pid) = {
        let runner = managed.runner.read().await;
        (runner.running, runner.pid)
    };
    let port_open = managed.cached_health.read().await.runner_port_open;
    if restart_should_stop(running, pid, port_open) {
        if !running {
            info!(
                "Restart of runner '{}': tracked running=false but pid={:?}, port_open={} — \
                 stopping the live process before starting (S2 guard)",
                runner_id, pid, port_open
            );
        }
        // `force: true` — NOT a bypass. The readiness verdict was already
        // evaluated (and logged) at the restart boundary in the caller;
        // re-probing here would pay a second HTTP round trip and, worse,
        // could refuse a restart the caller was already granted because a
        // session appeared in between. One decision per operator request.
        stop_runner_by_id(state, runner_id, true).await?;
    }

    // Rebuild if requested (global — single binary).
    //
    // Phase B: the PRIMARY rebuild defaults to building a fresh `origin/main`
    // worktree (provenance `origin_main`) so the primary always runs
    // latest-green-main and never compiles the contested working checkout.
    // `from_working_tree: true` is the escape hatch back to the legacy
    // live-tree build. Non-primary runners (named/temp) keep the legacy
    // live-tree build unconditionally — origin/main pinning is a
    // primary-only policy.
    let build_duration = if rebuild {
        let build_start = std::time::Instant::now();
        let build_origin_main = managed.config.kind().is_primary() && !from_working_tree;
        if build_origin_main {
            primary_rebuild_from_origin_main(state).await?;
        } else {
            crate::build_monitor::run_cargo_build(state).await?;
        }
        Some(build_start.elapsed().as_secs_f64())
    } else {
        None
    };

    // Start.
    //
    // Use the `managed` Arc we already hold rather than re-looking-up by id:
    // stopping a `test-*` runner auto-removes its id from `state.runners`, so
    // `start_runner_by_id(runner_id)` would 404 with "Runner not found" even
    // though we have the full config in hand. `start_managed_runner`
    // re-inserts the id into the registry (its defensive `or_insert`), so a
    // restart of a *running* temp runner re-spawns on the SAME port instead
    // of stranding it. For non-temp runners the id was never removed, so this
    // is equivalent to the by-id path.
    start_managed_runner(state, managed).await?;

    Ok(build_duration)
}

/// Stop all runners. Primary is stopped last.
/// Stop all temp runners. User runners (primary and secondary) are never touched.
pub async fn stop_all_temp_runners(state: &SharedState) -> Result<(), SupervisorError> {
    let runners = state.get_all_runners().await;
    let mut errors = Vec::new();

    for managed in &runners {
        if !is_temp_runner(&managed.config.id) {
            continue;
        }
        let running = managed.runner.read().await.running;
        if running {
            // Temp-only sweep, and the gate exempts temp runners anyway;
            // `false` keeps the honest value rather than asserting an
            // override that is never consulted.
            if let Err(e) = stop_runner_by_id(state, &managed.config.id, false).await {
                errors.push(format!("'{}': {}", managed.config.name, e));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(SupervisorError::Other(format!(
            "Errors stopping temp runners: {}",
            errors.join("; ")
        )))
    }
}

/// Restart all runners. Stop all, optionally rebuild, start all (primary first).
#[allow(dead_code)]
pub async fn restart_all(
    state: &SharedState,
    rebuild: bool,
    _source: RestartSource,
) -> Result<(), SupervisorError> {
    // Collect which runners were running before stop
    let runners = state.get_all_runners().await;
    let mut was_running = Vec::new();
    for managed in &runners {
        let running = managed.runner.read().await.running;
        if running {
            was_running.push(managed.config.id.clone());
        }
    }

    stop_all_temp_runners(state).await?;

    if rebuild {
        crate::build_monitor::run_cargo_build(state).await?;
    }

    // Start primary first
    for managed in &runners {
        if managed.config.kind().is_primary() && was_running.contains(&managed.config.id) {
            start_runner_by_id(state, &managed.config.id).await?;
        }
    }

    // Then start non-primary with 2s delay
    for managed in &runners {
        if !managed.config.kind().is_primary() && was_running.contains(&managed.config.id) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            start_runner_by_id(state, &managed.config.id).await?;
        }
    }

    Ok(())
}

/// Stop the runner process (primary). Attempts graceful shutdown, then force kill.
/// Legacy stop — targets the primary runner. Allowed for manual use.
/// `force` was `_force` here too — parsed by `POST /runner/stop`, threaded all
/// the way down, and dropped on the floor. It now reaches the readiness gate.
pub async fn stop_runner(state: &SharedState, force: bool) -> Result<(), SupervisorError> {
    let primary = state
        .get_primary()
        .await
        .ok_or_else(|| SupervisorError::Other("No primary runner configured".to_string()))?;

    stop_runner_by_id(state, &primary.config.id, force).await
}

/// Legacy restart wrapper — targets the primary runner.
/// Only manual restarts are allowed; automated sources are rejected.
///
/// `from_working_tree` (Phase B): when `false` (default) a `rebuild` materializes
/// a fresh `origin/main` worktree and compiles THAT (provenance `origin_main`)
/// so the primary always runs latest-green-main; when `true` it compiles the
/// live working tree (legacy `live_tree` behavior). Only consulted on the
/// primary rebuild path inside [`restart_runner_by_id`].
pub async fn restart_runner(
    state: &SharedState,
    rebuild: bool,
    source: RestartSource,
    force: bool,
    from_working_tree: bool,
) -> Result<(), SupervisorError> {
    let primary = state
        .get_primary()
        .await
        .ok_or_else(|| SupervisorError::Other("No primary runner configured".to_string()))?;

    restart_runner_by_id(
        state,
        &primary.config.id,
        rebuild,
        source,
        force,
        from_working_tree,
    )
    .await
}

/// Implementation backing `POST /runners/{id}/rebuild-and-restart` (Item E
/// of the supervisor cleanup plan).
///
/// Sequence: stop → cargo build → start. Returns a JSON envelope containing
/// the same `build_result` shape used by spawn-test plus stop/build/start
/// timestamps. Rejects the primary outright — the supervisor never
/// rebuilds-and-restarts a user-managed primary runner.
///
/// On build failure this returns the cargo error directly (no automatic
/// stale-binary fallback). The runner is left stopped — callers can hit
/// `/runners/{id}/start` if they want to revive it from the previous slot
/// exe.
pub async fn rebuild_and_restart_by_id(
    state: &SharedState,
    runner_id: &str,
    body: crate::routes::runners::RebuildAndRestartRequest,
) -> Result<serde_json::Value, SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;

    if managed.config.kind().is_primary() {
        return Err(SupervisorError::Validation(
            "cannot_rebuild_primary: refusing to rebuild a user-managed primary runner".to_string(),
        ));
    }

    let runner_name = managed.config.name.clone();
    let source_label = if body.source.is_empty() {
        "rebuild-and-restart".to_string()
    } else {
        format!("rebuild-and-restart:{}", body.source)
    };

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "rebuild-and-restart: stopping runner '{}' (source={})",
                runner_name, source_label
            ),
        )
        .await;

    // Step 1: stop. Best-effort — if the runner is already stopped this
    // returns NotRunning which we tolerate.
    let stopped_at = chrono::Utc::now();
    // `body.force` was documented as "Reserved for future 'force unprotect'
    // semantics. Currently a no-op" and carried `#[allow(dead_code)]`. It is
    // no longer a no-op: it is the readiness-gate override, same meaning as on
    // `/runners/{id}/stop` and `/runners/{id}/restart`.
    match stop_runner_by_id(state, runner_id, body.force).await {
        Ok(()) | Err(SupervisorError::RunnerNotRunning) => {}
        Err(e) => return Err(e),
    }

    // Step 2: rebuild.
    let rebuilt_at = chrono::Utc::now();
    let build_outcome = crate::build_monitor::run_cargo_build_with_requester(
        state,
        Some(format!("rebuild-and-restart:{}", runner_id)),
    )
    .await;

    let (build_attempted, build_succeeded, build_error): (bool, Option<bool>, Option<String>) =
        match build_outcome {
            Ok(()) => (true, Some(true), None),
            Err(e) => return Err(e),
        };

    // Step 3: start.
    let started_at = chrono::Utc::now();
    start_managed_runner(state, &managed).await?;
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "rebuild-and-restart: runner '{}' restarted (source={})",
                runner_name, source_label
            ),
        )
        .await;

    // Step 4: optional wait for /health.
    let mut wait_ms: u64 = 0;
    if body.wait {
        let timeout_secs = body.wait_timeout_secs.unwrap_or(120);
        let timeout = std::time::Duration::from_secs(timeout_secs);
        let poll_interval = std::time::Duration::from_secs(2);
        let start = std::time::Instant::now();
        let port = managed.config.port;
        let health_url = format!("http://localhost:{}/health", port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_default();

        while start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;
            if let Ok(resp) = client.get(&health_url).send().await {
                if resp.status().is_success() {
                    wait_ms = start.elapsed().as_millis() as u64;
                    break;
                }
            }
        }
        if wait_ms == 0 {
            wait_ms = start.elapsed().as_millis() as u64;
        }
    }

    // Build the response. Mirror spawn-test/spawn-named's build_result shape.
    let exe_meta = resolve_source_exe(state)
        .await
        .ok()
        .and_then(|p| binary_meta(&p));
    let post_build_slot_id = *state.build_pool.last_successful_slot.read().await;
    let build_result = crate::routes::runners::build_result_json(
        build_attempted,
        build_succeeded,
        false,
        build_error.as_deref(),
        post_build_slot_id,
        exe_meta.as_ref(),
    );

    Ok(serde_json::json!({
        "id": runner_id,
        "build_result": build_result,
        "stopped_at": stopped_at.to_rfc3339(),
        "rebuilt_at": rebuilt_at.to_rfc3339(),
        "started_at": started_at.to_rfc3339(),
        "wait_ms": wait_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    // Temp-runner max-age bound (plan
    // 2026-08-10-temp-runner-session-restore-isolation, Phase 5).

    fn all_kinds() -> Vec<qontinui_types::wire::runner_kind::RunnerKind> {
        use qontinui_types::wire::runner_kind::RunnerKind;
        vec![
            RunnerKind::Primary,
            RunnerKind::Named {
                name: "named-9880".to_string(),
            },
            RunnerKind::Temp {
                id: "test-abc123".to_string(),
            },
            RunnerKind::External,
        ]
    }

    /// The bound is an `is_temp()` ALLOWLIST. A user-owned runner is never
    /// age-reaped, no matter how old it is or how tight the bound — the
    /// `!is_primary()` denylist spelling of this rule is what took the
    /// operator's primary down on 2026-07-27.
    #[test]
    fn max_age_bound_applies_to_temp_runners_only() {
        let ancient = Duration::from_secs(365 * 24 * 60 * 60);
        let tight = Some(Duration::from_secs(1));
        for kind in all_kinds() {
            let reaped = exceeds_temp_runner_max_age(&kind, ancient, tight);
            assert_eq!(
                reaped,
                kind.is_temp(),
                "{kind:?}: only RunnerKind::Temp may be reaped for age — primary, named \
                 and external runners are user-owned"
            );
        }
    }

    /// A temp runner inside the bound survives; one past it does not.
    #[test]
    fn max_age_bound_fires_only_past_the_bound() {
        let kind = qontinui_types::wire::runner_kind::RunnerKind::Temp {
            id: "test-abc123".to_string(),
        };
        let max = Duration::from_secs(3600);
        assert!(!exceeds_temp_runner_max_age(
            &kind,
            Duration::from_secs(0),
            Some(max)
        ));
        assert!(!exceeds_temp_runner_max_age(
            &kind,
            Duration::from_secs(3599),
            Some(max)
        ));
        assert!(exceeds_temp_runner_max_age(
            &kind,
            Duration::from_secs(3600),
            Some(max)
        ));
        assert!(exceeds_temp_runner_max_age(
            &kind,
            Duration::from_secs(100_000),
            Some(max)
        ));
    }

    /// The age clock is `started_at`, NOT time-since-placeholder. This is the
    /// cold-build case: `spawn_test` reserved the placeholder 50 minutes ago,
    /// the child bound its port 90 seconds ago. The runner is 90s old, not
    /// 3000s — reading it the other way reaps a brand-new runner on the very
    /// next sweep, every retry.
    #[test]
    fn age_is_measured_from_process_start_not_from_the_spawn_request() {
        let now = chrono::Utc::now();
        let started = now - chrono::Duration::seconds(90);
        let (age, basis) = resolve_temp_runner_age(Some(started), now, Duration::from_secs(3000));
        assert_eq!(age.as_secs(), 90);
        assert!(basis.contains("process start"), "basis was {basis:?}");

        // And that age is INSIDE a 1h bound, where the placeholder clock would
        // have blown straight past it.
        let kind = qontinui_types::wire::runner_kind::RunnerKind::Temp {
            id: "test-abc123".to_string(),
        };
        let bound = Some(Duration::from_secs(3600));
        assert!(!exceeds_temp_runner_max_age(&kind, age, bound));
        assert!(exceeds_temp_runner_max_age(
            &kind,
            Duration::from_secs(3000 * 2),
            bound
        ));
    }

    /// No `started_at` → fall back to time-since-first-seen rather than
    /// reporting zero age (which would make the bound unreachable).
    #[test]
    fn age_falls_back_to_first_seen_without_started_at() {
        let now = chrono::Utc::now();
        let (age, basis) = resolve_temp_runner_age(None, now, Duration::from_secs(4242));
        assert_eq!(age.as_secs(), 4242);
        assert!(basis.contains("first seen"), "basis was {basis:?}");
    }

    /// A `started_at` in the FUTURE (clock skew / NTP step) must not panic or
    /// wrap into a huge age that reaps a live runner — fall back and say so.
    #[test]
    fn age_falls_back_when_started_at_is_in_the_future() {
        let now = chrono::Utc::now();
        let started = now + chrono::Duration::seconds(600);
        let (age, basis) = resolve_temp_runner_age(Some(started), now, Duration::from_secs(11));
        assert_eq!(age.as_secs(), 11);
        assert!(basis.contains("future"), "basis was {basis:?}");
    }

    /// `None` is the off-switch: nothing is ever reaped for age, including a
    /// temp runner that has been alive for a year.
    #[test]
    fn max_age_bound_disabled_never_reaps() {
        let ancient = Duration::from_secs(365 * 24 * 60 * 60);
        for kind in all_kinds() {
            assert!(
                !exceeds_temp_runner_max_age(&kind, ancient, None),
                "{kind:?}: a disabled bound must never reap anything"
            );
        }
    }

    // QONTINUI_API_URL child-env policy (plan 2026-07-08).

    #[test]
    fn child_api_url_primary_no_env_is_unset() {
        // Primary with no explicit supervisor QONTINUI_API_URL → leave it unset
        // so the runner resolves via its persisted paired backend.
        assert_eq!(resolve_child_api_url(None, true), None);
    }

    #[test]
    fn child_api_url_secondary_no_env_pins_local() {
        // Temp/named with no explicit env → pinned to the local backend.
        assert_eq!(
            resolve_child_api_url(None, false),
            Some("http://127.0.0.1:8000".to_string())
        );
    }

    #[test]
    fn child_api_url_explicit_env_forwarded_to_all() {
        // An explicit supervisor QONTINUI_API_URL wins for every runner kind.
        let explicit = || Some("https://api.qontinui.io".to_string());
        assert_eq!(resolve_child_api_url(explicit(), true), explicit());
        assert_eq!(resolve_child_api_url(explicit(), false), explicit());
    }

    // QONTINUI_PRIMARY_SECURE_STORAGE_DIR pointer (plan 2026-07-13, R4.1).
    // The env var is set only in the non-primary spawn block of
    // `start_exe_mode_for_runner` (guarded by `!is_primary()`), so a primary
    // spawn never carries it. Here we assert the *value* the non-primary block
    // forwards: the primary's default secure-storage dir, which must equal
    // qontinui-runner's `SecureStorage::new()` fallback
    // (`dirs::data_local_dir()/com.qontinui.runner`) so the spawned runner's
    // machine-derived key can decrypt the primary's `auth_tokens.enc`.
    #[test]
    fn primary_secure_storage_dir_is_data_local_com_qontinui_runner() {
        let expected = dirs::data_local_dir().map(|d| d.join("com.qontinui.runner"));
        assert_eq!(primary_secure_storage_dir(), expected);
        // On any platform with a resolvable data-local dir, the pointer ends in
        // the runner's service-name subdir — the exact dir SecureStorage::new()
        // writes to when unoverridden.
        if let Some(dir) = primary_secure_storage_dir() {
            assert!(
                dir.ends_with("com.qontinui.runner"),
                "primary secure-storage pointer must target the runner's default \
                 store dir, got {dir:?}"
            );
        }
    }

    // Per-instance config/secure-storage dir, asserted at the site that
    // actually decides where the child reads.
    //
    // `start_exe_mode_for_runner` applies `apply_instance_dir_env` to every
    // non-primary child, and `spawn-test` only ever mints non-primary
    // (`RunnerKind::Temp`) runners — so the dir this function sets IS the
    // directory a spawn-test runner loads its pairing and token cache from.
    // `routes::runners` writes the `paired_profile_id` snapshot into
    // `instance_config_dir`'s output for the same id.
    //
    // The assertion is made over a REAL `Command`'s env map rather than over a
    // re-derivation of the helper's body: re-inlining a divergent path at the
    // spawn's `cmd.env(...)` site — the failure this test exists to catch —
    // fails here. (It covers the spawn side only; the route side is pinned by
    // `routes::runners::tests::instance_config_dir_is_where_apply_paired_profile_for_spawn_writes`.)
    #[test]
    fn apply_instance_dir_env_sets_both_vars_to_instance_config_dir() {
        let id = format!("test-apply-instance-dir-env-{}", std::process::id());
        let Some(expected) = instance_config_dir(&id) else {
            // No resolvable config dir on this platform — nothing to assert.
            return;
        };
        let _cleanup = scopeguard::guard(expected.clone(), |dir| {
            let _ = std::fs::remove_dir_all(dir);
        });

        let mut cmd = Command::new("cargo");
        let applied = super::apply_instance_dir_env(&mut cmd, &id).expect("must resolve + create");
        assert_eq!(applied, expected);

        let envs: std::collections::HashMap<String, Option<String>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        let want = expected.to_string_lossy().into_owned();
        for key in ["QONTINUI_CONFIG_DIR", "QONTINUI_SECURE_STORAGE_DIR"] {
            assert_eq!(
                envs.get(key).cloned().flatten().as_deref(),
                Some(want.as_str()),
                "{key} must be exported to the child as instance_config_dir({id}) = {expected:?}; \
                 a spawn that inlines a different path leaves the runner unpaired"
            );
        }
        assert!(expected.is_dir(), "the instance dir must have been created");
    }

    /// Read a `Command`'s env overrides back as a plain map.
    fn command_envs(cmd: &Command) -> std::collections::HashMap<String, Option<String>> {
        cmd.as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    /// A `RunnerConfig` shaped exactly like the one `spawn_test` builds — the
    /// instance name comes from the same helper the spawn site uses, so this
    /// test tracks the real minting policy instead of a copy of it.
    fn temp_config(id: &str, port: u16) -> crate::config::RunnerConfig {
        crate::config::RunnerConfig {
            id: id.to_string(),
            name: crate::process::temp_runner_instance_name(id),
            port,
            kind: qontinui_types::wire::runner_kind::RunnerKind::Temp { id: id.to_string() },
            protected: true,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: Default::default(),
        }
    }

    /// The non-primary env block, asserted **as a whole** against a real
    /// `Command` — `QONTINUI_INSTANCE_NAME`, `QONTINUI_PRIMARY_PORT` and
    /// `WEBVIEW2_USER_DATA_FOLDER` together, because it is the combination
    /// that isolates a secondary's state from the primary's and from other
    /// secondaries'. Sibling of
    /// `apply_instance_dir_env_sets_both_vars_to_instance_config_dir`, which
    /// covers the two config-dir vars.
    ///
    /// **The load-bearing assertion is that `QONTINUI_INSTANCE_NAME` is unique
    /// per SPAWN, not per port.** Both configs here take the SAME port —
    /// temp ports are recycled inside the 23-slot range 9877-9899, so a
    /// second spawn routinely lands on a port a previous temp just released.
    /// While the name was `format!("test-{port}")`, those two spawns shared
    /// one `instance-test-<port>` app-data tree, and the second runner booted
    /// on the first's live `terminal-sessions.json` — 283 inherited PTYs
    /// observed 2026-08-08 (plan
    /// `2026-08-10-temp-runner-session-restore-isolation`). Nothing failed
    /// when that scheme changed, because no test asserted it; this is that
    /// test.
    #[test]
    fn non_primary_env_block_keys_instance_name_per_spawn_not_per_port() {
        // Two sequential spawns that RECYCLE the same port.
        const PORT: u16 = 9877;
        const PRIMARY_PORT: u16 = 9876;
        let pid = std::process::id();
        let first = temp_config(&format!("test-envblock-a-{pid}"), PORT);
        let second = temp_config(&format!("test-envblock-b-{pid}"), PORT);

        if instance_config_dir(&first.id).is_none() {
            // No resolvable config dir on this platform — nothing to assert.
            return;
        }
        // Both dirs land under the operator's REAL app-data roots (the block
        // creates them), so they must not survive a failing assertion below.
        // Same guard shape as `apply_instance_dir_env_sets_both_vars_to_…`.
        let _cleanup = scopeguard::guard(
            vec![first.id.clone(), second.id.clone()],
            |ids: Vec<String>| {
                for id in ids {
                    if let Some(dir) = instance_config_dir(&id) {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                    #[cfg(target_os = "windows")]
                    if let Some(dir) = webview2_user_data_folder(&id, false) {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
            },
        );

        let mut cmd_a = Command::new("cargo");
        let mut cmd_b = Command::new("cargo");
        super::apply_non_primary_instance_env(&mut cmd_a, &first, PRIMARY_PORT)
            .expect("must resolve + create");
        super::apply_non_primary_instance_env(&mut cmd_b, &second, PRIMARY_PORT)
            .expect("must resolve + create");
        let envs_a = command_envs(&cmd_a);
        let envs_b = command_envs(&cmd_b);

        let name_a = envs_a
            .get("QONTINUI_INSTANCE_NAME")
            .cloned()
            .flatten()
            .expect("a secondary must always be given QONTINUI_INSTANCE_NAME");
        let name_b = envs_b
            .get("QONTINUI_INSTANCE_NAME")
            .cloned()
            .flatten()
            .expect("a secondary must always be given QONTINUI_INSTANCE_NAME");

        assert_ne!(
            name_a, name_b,
            "two temp runners spawned on the SAME recycled port {PORT} must get \
             DIFFERENT QONTINUI_INSTANCE_NAME values — the runner roots its whole \
             instance-<name> app-data tree (terminal-sessions.json included) on this \
             string, so a port-derived name makes the second spawn inherit the \
             first's live terminal registry"
        );
        for (name, config) in [(&name_a, &first), (&name_b, &second)] {
            assert_eq!(
                name, &config.id,
                "the instance name must BE the per-spawn runner id (no second uuid, \
                 nothing port-derived) so it stays in lockstep with the id-keyed \
                 instance_config_dir / WebView2 / QONTINUI_RUNNER_ID resources"
            );
            assert!(
                !name.contains(&PORT.to_string()),
                "instance name {name:?} must carry no trace of the recycled port"
            );
        }

        for (envs, config) in [(&envs_a, &first), (&envs_b, &second)] {
            assert_eq!(
                envs.get("QONTINUI_PRIMARY_PORT").cloned().flatten(),
                Some(PRIMARY_PORT.to_string()),
                "a secondary needs BOTH the instance name and the primary port to \
                 classify itself as secondary; unset makes it behave as a primary"
            );

            #[cfg(target_os = "windows")]
            {
                let want = webview2_user_data_folder(&config.id, false)
                    .expect("LOCALAPPDATA resolves on Windows")
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(
                    envs.get("WEBVIEW2_USER_DATA_FOLDER").cloned().flatten(),
                    Some(want),
                    "the WebView2 profile must be keyed on the per-spawn runner id \
                     so two temps never share localStorage/IndexedDB/cookies"
                );
            }
            #[cfg(not(target_os = "windows"))]
            let _ = config;
        }

        #[cfg(target_os = "windows")]
        assert_ne!(
            envs_a.get("WEBVIEW2_USER_DATA_FOLDER"),
            envs_b.get("WEBVIEW2_USER_DATA_FOLDER"),
            "same-port spawns must not share a WebView2 profile"
        );
    }

    /// A slot freshly built 5 minutes after the running copy is "stale".
    #[test]
    fn stale_binary_detection_slot_much_newer() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(300); // +5 min
        let out = compute_stale_binary(Some(running), Some((0, slot)))
            .expect("5-minute gap should be surfaced");
        assert_eq!(out.slot_id, 0);
        assert_eq!(out.age_delta_secs, 300);
        assert_eq!(out.running_mtime_ms, 1_700_000_000 * 1000);
        assert_eq!(out.slot_mtime_ms, (1_700_000_000 + 300) * 1000);
    }

    /// A slot 10 seconds newer is within jitter — no badge.
    #[test]
    fn stale_binary_detection_within_threshold() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(10);
        assert!(compute_stale_binary(Some(running), Some((0, slot))).is_none());
    }

    /// A slot exactly at the threshold (30s) does not trigger — strict `>`.
    #[test]
    fn stale_binary_detection_at_exact_threshold() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(STALE_BINARY_THRESHOLD_SECS as u64);
        assert!(
            compute_stale_binary(Some(running), Some((0, slot))).is_none(),
            "delta == threshold must not surface a stale_binary entry"
        );
    }

    /// One second over the threshold DOES trigger.
    #[test]
    fn stale_binary_detection_just_over_threshold() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(STALE_BINARY_THRESHOLD_SECS as u64 + 1);
        let out = compute_stale_binary(Some(running), Some((0, slot)))
            .expect("threshold + 1s should surface a stale_binary entry");
        assert_eq!(out.age_delta_secs, STALE_BINARY_THRESHOLD_SECS + 1);
    }

    /// A slot older than the running copy means the running copy is the
    /// freshest binary on disk — normal state, no badge.
    #[test]
    fn stale_binary_detection_running_newer() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running - Duration::from_secs(120);
        assert!(compute_stale_binary(Some(running), Some((0, slot))).is_none());
    }

    /// Identical mtimes — no divergence, no badge.
    #[test]
    fn stale_binary_detection_equal() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(compute_stale_binary(Some(running), Some((1, running))).is_none());
    }

    /// Missing running-copy mtime (first start, fs stat failed, etc.) — the
    /// feature silently skips.
    #[test]
    fn stale_binary_detection_missing_running_mtime() {
        let slot = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(compute_stale_binary(None, Some((0, slot))).is_none());
    }

    /// No slot has ever produced a binary — nothing to compare against.
    #[test]
    fn stale_binary_detection_no_slot_binary() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(compute_stale_binary(Some(running), None).is_none());
    }

    /// Slot id is preserved through the struct (not always 0).
    #[test]
    fn stale_binary_detection_preserves_slot_id() {
        let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slot = running + Duration::from_secs(600);
        let out = compute_stale_binary(Some(running), Some((2, slot))).expect("stale");
        assert_eq!(out.slot_id, 2);
    }

    // =========================================================================
    // First-healthy watchdog decision tests
    // =========================================================================

    /// Process gone — exit quietly regardless of other flags.
    #[test]
    fn first_healthy_abandon_when_untracked() {
        assert_eq!(
            decide_first_healthy(false, false, false),
            FirstHealthyDecision::Abandon
        );
        // Even if the port is "responding" and the deadline passed, an
        // untracked PID is not ours to act on.
        assert_eq!(
            decide_first_healthy(false, true, true),
            FirstHealthyDecision::Abandon
        );
    }

    /// HTTP /health responded — healthy outcome, even if the deadline just
    /// elapsed on the same tick.
    #[test]
    fn first_healthy_healthy_wins_over_kill() {
        assert_eq!(
            decide_first_healthy(true, true, false),
            FirstHealthyDecision::Healthy
        );
        // Edge case the priority rule exists for: responsive AND past
        // deadline on the same poll. We do NOT kill — the runner made it.
        assert_eq!(
            decide_first_healthy(true, true, true),
            FirstHealthyDecision::Healthy
        );
    }

    /// Tracked, not responding, deadline passed — kill path.
    #[test]
    fn first_healthy_kill_when_deadline_passed_and_unresponsive() {
        assert_eq!(
            decide_first_healthy(true, false, true),
            FirstHealthyDecision::Kill
        );
    }

    /// Tracked, not responding, still within budget — keep waiting.
    #[test]
    fn first_healthy_wait_while_within_budget() {
        assert_eq!(
            decide_first_healthy(true, false, false),
            FirstHealthyDecision::Wait
        );
    }

    // =========================================================================
    // Stop-reap escalation decision tests (Item 2: confirmed port-free stop)
    // =========================================================================

    /// Port free on the very first check — no escalation, stop confirmed.
    #[test]
    fn stop_reap_confirmed_when_port_free() {
        assert_eq!(decide_stop_reap(0, false), StopReapOutcome::Confirmed);
        // A free port short-circuits at every attempt index, even after
        // escalations have run.
        assert_eq!(decide_stop_reap(1, false), StopReapOutcome::Confirmed);
        assert_eq!(decide_stop_reap(2, false), StopReapOutcome::Confirmed);
    }

    /// First attempt with the port still held escalates to a tree-kill
    /// (kills the runner's child processes a plain `/F` PID kill leaves alive).
    #[test]
    fn stop_reap_first_held_escalates_to_tree() {
        assert_eq!(decide_stop_reap(0, true), StopReapOutcome::EscalateTree);
    }

    /// Second attempt still held escalates to a blind kill-by-port.
    #[test]
    fn stop_reap_second_held_escalates_to_port() {
        assert_eq!(decide_stop_reap(1, true), StopReapOutcome::EscalatePort);
    }

    /// Both kill escalations exhausted — one bounded backoff retry before
    /// giving up (covers a kill that landed but whose socket teardown was
    /// slow to be reflected by the OS).
    #[test]
    fn stop_reap_third_held_retries_after_backoff() {
        assert_eq!(
            decide_stop_reap(2, true),
            StopReapOutcome::RetryAfterBackoff
        );
    }

    /// Every escalation (and the backoff retry) exhausted and still held —
    /// stop must NOT be confirmed.
    #[test]
    fn stop_reap_exhausted_is_still_held() {
        assert_eq!(decide_stop_reap(3, true), StopReapOutcome::StillHeld);
        // Any attempt beyond the ladder also reports StillHeld (never loops
        // back to a kill it already tried).
        assert_eq!(decide_stop_reap(4, true), StopReapOutcome::StillHeld);
        assert_eq!(decide_stop_reap(99, true), StopReapOutcome::StillHeld);
    }

    /// The full escalation ladder visits each rung exactly once before
    /// giving up — guards against an infinite reap loop on a wedged survivor.
    #[test]
    fn stop_reap_ladder_terminates() {
        // Simulate a survivor that never releases the port: the loop must
        // walk Tree → Port → RetryAfterBackoff → StillHeld and stop.
        assert_eq!(decide_stop_reap(0, true), StopReapOutcome::EscalateTree);
        assert_eq!(decide_stop_reap(1, true), StopReapOutcome::EscalatePort);
        assert_eq!(
            decide_stop_reap(2, true),
            StopReapOutcome::RetryAfterBackoff
        );
        assert_eq!(decide_stop_reap(3, true), StopReapOutcome::StillHeld);
    }

    // =========================================================================
    // Crash-only ambient watchdog decision tests (Phase 1,
    // plans/2026-07-03-primary-runner-crash-resilience.md)
    // =========================================================================

    /// Baseline crash: supervisor-spawned child, no stop intent, non-zero
    /// exit, everything armed, empty window → restart attempt 1 after 5s.
    #[test]
    fn crash_restart_first_crash_restarts_after_5s() {
        assert_eq!(
            decide_crash_restart(true, false, false, true, true, false, 0),
            CrashRestartDecision::Restart {
                attempt: 1,
                delay_secs: 5
            }
        );
    }

    /// Exponential backoff ladder: 5s → 30s → 120s across the rolling window.
    #[test]
    fn crash_restart_backoff_is_exponential() {
        assert_eq!(
            decide_crash_restart(true, false, false, true, true, false, 1),
            CrashRestartDecision::Restart {
                attempt: 2,
                delay_secs: 30
            }
        );
        assert_eq!(
            decide_crash_restart(true, false, false, true, true, false, 2),
            CrashRestartDecision::Restart {
                attempt: 3,
                delay_secs: 120
            }
        );
    }

    /// Rolling-window budget exhausted (3 restarts already) → disarm, loudly.
    #[test]
    fn crash_restart_window_exhausted_disarms() {
        assert_eq!(
            decide_crash_restart(true, false, false, true, true, false, 3),
            CrashRestartDecision::Disarm
        );
        // Even further past the budget it stays Disarm, never Restart.
        assert_eq!(
            decide_crash_restart(true, false, false, true, true, false, 7),
            CrashRestartDecision::Disarm
        );
    }

    /// Operator stop intent wins over everything else armed — never restart
    /// a runner the operator deliberately stopped.
    #[test]
    fn crash_restart_operator_stop_never_restarts() {
        assert_eq!(
            decide_crash_restart(true, true, false, true, true, false, 0),
            CrashRestartDecision::SkipOperatorStop
        );
        // Even a non-clean exit after a stop request (taskkill path) skips.
        assert_eq!(
            decide_crash_restart(true, true, true, true, true, false, 0),
            CrashRestartDecision::SkipOperatorStop
        );
    }

    /// A clean exit (code 0) is a deliberate shutdown (window close,
    /// internal exit) — crash-only means we never restart it.
    #[test]
    fn crash_restart_clean_exit_skips() {
        assert_eq!(
            decide_crash_restart(true, false, true, true, true, false, 0),
            CrashRestartDecision::SkipCleanExit
        );
    }

    /// No Child handle → the supervisor never spawned this process; no
    /// provenance to restart it.
    #[test]
    fn crash_restart_requires_spawn_provenance() {
        assert_eq!(
            decide_crash_restart(false, false, false, true, true, false, 0),
            CrashRestartDecision::SkipNoChildHandle
        );
    }

    /// Global arm off (no --watchdog, or the env kill-switch) → skip.
    #[test]
    fn crash_restart_global_arm_gates() {
        assert_eq!(
            decide_crash_restart(true, false, false, false, true, false, 0),
            CrashRestartDecision::SkipNotArmed
        );
    }

    /// Per-runner enabled=false (default for named/temp/external) → skip.
    #[test]
    fn crash_restart_per_runner_enabled_gates() {
        assert_eq!(
            decide_crash_restart(true, false, false, true, false, false, 0),
            CrashRestartDecision::SkipDisabled
        );
    }

    /// Once disarmed (`disabled_reason` set), restarts stay off until an
    /// operator resets — even with a fresh (pruned) window.
    #[test]
    fn crash_restart_disarmed_latch_holds() {
        assert_eq!(
            decide_crash_restart(true, false, false, true, true, true, 0),
            CrashRestartDecision::SkipDisarmed
        );
    }

    /// The two CRASH WARN-worthy skip variants (`SkipNotArmed` /
    /// `SkipDisarmed`, promoted to `warn!` + persisted emit in
    /// `maybe_crash_restart`) are ONLY reachable for a genuine crash — a held
    /// Child handle, no operator stop, and an unclean exit. The benign skips
    /// for the same crash inputs (clean exit, operator stop, no child handle)
    /// classify away from the WARN set, so the CRASH promotion never fires on
    /// a benign exit.
    ///
    /// Note `SkipCleanExit` carries its OWN, separate warn arm for the
    /// unrequested-clean-exit case (supervisor-spawned, code 0, no stop
    /// requested — the 2026-08-06 self-exit). That arm is deliberately not part
    /// of this crash set: it reports rather than restarts, and it is gated on
    /// `!stop_requested_at_exit` in `maybe_crash_restart`, not here.
    #[test]
    fn crash_restart_warn_worthy_skips_are_genuine_crash_only() {
        // Genuine crash, global arm off → WARN-worthy SkipNotArmed.
        let not_armed = decide_crash_restart(true, false, false, false, true, false, 0);
        assert_eq!(not_armed, CrashRestartDecision::SkipNotArmed);
        assert!(matches!(
            not_armed,
            CrashRestartDecision::SkipNotArmed | CrashRestartDecision::SkipDisarmed
        ));
        // Genuine crash, armed+enabled but disarmed latch → WARN-worthy SkipDisarmed.
        let disarmed = decide_crash_restart(true, false, false, true, true, true, 0);
        assert_eq!(disarmed, CrashRestartDecision::SkipDisarmed);
        // Benign skips for the SAME arm state must NOT be in the WARN-worthy set.
        for benign in [
            decide_crash_restart(true, false, true, false, true, false, 0), // clean exit
            decide_crash_restart(true, true, false, false, true, false, 0), // operator stop
            decide_crash_restart(false, false, false, false, true, false, 0), // no child handle
        ] {
            assert!(
                !matches!(
                    benign,
                    CrashRestartDecision::SkipNotArmed | CrashRestartDecision::SkipDisarmed
                ),
                "benign skip {benign:?} must not be WARN-worthy"
            );
        }
    }

    // =========================================================================
    // Serving watchdog decision table
    // =========================================================================

    use crate::state::RunnerLiveness;

    fn primary_kind() -> qontinui_types::wire::runner_kind::RunnerKind {
        qontinui_types::wire::runner_kind::RunnerKind::Primary
    }

    fn temp_kind() -> qontinui_types::wire::runner_kind::RunnerKind {
        qontinui_types::wire::runner_kind::RunnerKind::Temp {
            id: "test-1".to_string(),
        }
    }

    /// The happy path: a wedge past the threshold, armed, nothing else in the
    /// way. This is the decision that ends the outage class the plan exists for.
    #[test]
    fn serving_restart_fires_on_a_wedge_past_the_threshold() {
        let wedged = RunnerLiveness::UnresponsiveSince(chrono::Utc::now());
        assert_eq!(
            decide_serving_restart(
                wedged,
                301,
                300,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::Restart { attempt: 1 }
        );
        // The attempt number is 1-based within the rolling window.
        assert_eq!(
            decide_serving_restart(
                wedged,
                301,
                300,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                2
            ),
            ServingRestartDecision::Restart { attempt: 3 }
        );
    }

    /// Every skip variant, in priority order. Classification and operator
    /// intent win over arming; the loop guard is evaluated last so `Disarm`
    /// only fires for a wedge that would otherwise have restarted.
    #[test]
    fn serving_restart_decision_table() {
        let wedged = RunnerLiveness::UnresponsiveSince(chrono::Utc::now());
        let d = |liveness,
                 silent,
                 kind: &qontinui_types::wire::runner_kind::RunnerKind,
                 stop,
                 restart,
                 in_flight,
                 armed,
                 enabled,
                 disarmed,
                 window| {
            decide_serving_restart(
                liveness, silent, 300, kind, stop, restart, in_flight, armed, enabled, disarmed,
                window,
            )
        };

        // Not the wedge class. `Unknown` is excluded on purpose (Design
        // decision 4): a runner that has NEVER answered has nothing to prove it
        // is this failure mode, so it is alerted, never restarted.
        for liveness in [
            RunnerLiveness::Responding,
            RunnerLiveness::Stopped,
            RunnerLiveness::Unknown,
        ] {
            assert_eq!(
                d(
                    liveness,
                    99_999,
                    &primary_kind(),
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                    0
                ),
                ServingRestartDecision::SkipNotWedged,
                "{liveness:?} must never reach a restart"
            );
        }

        // Temp runners belong to the max-age reaper.
        assert_eq!(
            d(
                wedged,
                99_999,
                &temp_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipTempRunner
        );

        // Operator intent beats arming.
        assert_eq!(
            d(
                wedged,
                99_999,
                &primary_kind(),
                true,
                false,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipOperatorIntent
        );
        assert_eq!(
            d(
                wedged,
                99_999,
                &primary_kind(),
                false,
                true,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipOperatorIntent
        );

        // A restart already in flight: the tick that follows 2s later must not
        // schedule a second one. `restart_requested` cannot carry this — it is
        // set inside `restart_runner_by_id`, after the task is already spawned.
        assert_eq!(
            d(
                wedged,
                99_999,
                &primary_kind(),
                false,
                false,
                true,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipInFlight
        );

        // Threshold boundary: strictly-below skips, equal fires.
        assert_eq!(
            d(
                wedged,
                299,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipBelowThreshold
        );
        assert_eq!(
            d(
                wedged,
                300,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::Restart { attempt: 1 }
        );

        // Arming.
        assert_eq!(
            d(
                wedged,
                301,
                &primary_kind(),
                false,
                false,
                false,
                false,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipNotArmed
        );
        assert_eq!(
            d(
                wedged,
                301,
                &primary_kind(),
                false,
                false,
                false,
                true,
                false,
                false,
                0
            ),
            ServingRestartDecision::SkipDisabled
        );
        assert_eq!(
            d(
                wedged,
                301,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                true,
                0
            ),
            ServingRestartDecision::SkipDisarmed
        );

        // Window exhaustion — evaluated LAST.
        assert_eq!(
            d(
                wedged,
                301,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                SERVING_RESTART_MAX_PER_WINDOW
            ),
            ServingRestartDecision::Disarm
        );
        // …but a runner that would have skipped for a benign reason never
        // disarms the arm for every other runner's sake.
        assert_eq!(
            d(
                RunnerLiveness::Responding,
                301,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                SERVING_RESTART_MAX_PER_WINDOW
            ),
            ServingRestartDecision::SkipNotWedged
        );
    }

    /// The two kill-switches are independent: `--watchdog` is shared (one
    /// operator intent, "supervise the primary"), the env switches are not.
    #[test]
    fn the_two_arms_share_the_cli_flag_and_nothing_else() {
        let mut config = crate::config::SupervisorConfig::from_args(
            <crate::config::CliArgs as clap::Parser>::parse_from(["test", "--project-dir", "."]),
        );
        config.watchdog_enabled_at_start = false;
        assert!(!serving_restart_globally_armed(&config));
        assert!(!crash_restart_globally_armed(&config));
        config.watchdog_enabled_at_start = true;
        // Both arms follow the shared flag; the env switches (read from the
        // process environment, which tests must not mutate) are asserted by
        // their own parse functions.
        assert_eq!(
            serving_restart_globally_armed(&config),
            !serving_restart_env_disabled()
        );
        assert_ne!(SERVING_LOOP_DISABLED_REASON, CRASH_LOOP_DISABLED_REASON);
    }

    /// The operator reset clears BOTH arms. A reset that cleared only the crash
    /// arm would leave a serving-loop disarm latched with no route to clear it.
    #[test]
    fn the_operator_reset_clears_both_arms() {
        let mut wd = crate::state::WatchdogState::new(true);
        wd.restart_attempts = 2;
        wd.disabled_reason = Some(CRASH_LOOP_DISABLED_REASON.to_string());
        wd.crash_history.push(chrono::Utc::now());
        wd.serving_restart_attempts = 3;
        wd.last_serving_restart_at = Some(chrono::Utc::now());
        wd.serving_history.push(chrono::Utc::now());
        wd.serving_disabled_reason = Some(SERVING_LOOP_DISABLED_REASON.to_string());
        wd.serving_restart_in_flight = true;

        wd.reset_attempts();

        assert_eq!(wd.restart_attempts, 0);
        assert!(wd.disabled_reason.is_none());
        assert!(wd.crash_history.is_empty());
        assert_eq!(wd.serving_restart_attempts, 0);
        assert!(wd.last_serving_restart_at.is_none());
        assert!(wd.serving_history.is_empty());
        assert!(wd.serving_disabled_reason.is_none());
        assert!(!wd.serving_restart_in_flight);
        // The operator's intent bit is NOT what a reset clears.
        assert!(wd.enabled);
    }

    // =========================================================================
    // Restart-of-stopped-test-id rejection (Item 3)
    // =========================================================================

    /// A `test-*` id is classified as a temp runner, so the restart-not-found
    /// branch emits the "auto-removed, spawn a new one" guidance rather than a
    /// bare "Runner not found". (The restart path itself needs SharedState, so
    /// we assert the classification predicate that drives the message choice.)
    #[test]
    fn restart_temp_id_is_temp_runner() {
        assert!(
            is_temp_runner("test-abc123"),
            "test-* ids must classify as temp so restart returns the \
             auto-removed guidance"
        );
    }

    /// A non-temp id (primary / named) is NOT a temp runner, so the
    /// restart-not-found branch falls back to the bare id (no auto-remove
    /// guidance, and no doubled Display prefix).
    #[test]
    fn restart_non_temp_id_is_not_temp_runner() {
        assert!(!is_temp_runner("primary"));
        assert!(!is_temp_runner("named-staging"));
    }

    /// The temp-not-found error renders with a single "Runner not found:"
    /// prefix (from the variant's Display) plus the spawn-test guidance —
    /// guards against the doubled-prefix regression.
    #[test]
    fn restart_temp_not_found_message_shape() {
        let detail = format!(
            "{} — ephemeral test runners are auto-removed when stopped and \
             cannot be restarted; spawn a new one via POST /runners/spawn-test",
            "test-xyz"
        );
        let err = SupervisorError::RunnerNotFound(detail);
        let rendered = err.to_string();
        assert!(rendered.starts_with("Runner not found: test-xyz"));
        assert_eq!(
            rendered.matches("Runner not found").count(),
            1,
            "must not double the 'Runner not found' prefix"
        );
        assert!(rendered.contains("spawn-test"));
    }

    // =========================================================================
    // Slot SHA drift detection (proj_supervisor_slot_resolution_order)
    // =========================================================================

    fn sha_a() -> String {
        "a".repeat(40)
    }
    fn sha_b() -> String {
        "b".repeat(40)
    }
    fn sha_c() -> String {
        "c".repeat(40)
    }

    /// Build a `(sha, source)` provenance key for a live-tree build.
    fn live(sha: String) -> SlotProvenanceKey {
        (Some(sha), Some(BuildSource::LiveTree))
    }
    /// Build a `(sha, source)` provenance key for an override build.
    fn over(sha: String) -> SlotProvenanceKey {
        (Some(sha), Some(BuildSource::Override))
    }
    /// A slot with no provenance sidecar at all.
    fn absent() -> SlotProvenanceKey {
        (None, None)
    }

    /// Distinct SHAs across multiple slots — drift surfaces.
    #[test]
    fn drift_fires_when_two_slots_disagree() {
        let all = vec![(0usize, live(sha_a())), (1usize, live(sha_b()))];
        let d = detect_slot_sha_drift(0, &live(sha_a()), &all)
            .expect("distinct SHAs must surface drift");
        assert_eq!(d.picked_slot_id, 0);
        assert_eq!(d.picked_sha, sha_a());
        assert_eq!(d.picked_source, BuildSource::LiveTree);
        assert_eq!(d.conflicting.len(), 1);
        assert_eq!(d.conflicting[0].0, 1);
        assert_eq!(d.conflicting[0].1, sha_b());
        assert_eq!(d.conflicting[0].2, Some(BuildSource::LiveTree));
    }

    /// Same SHA but DIFFERENT source tree (live vs override) — still drift,
    /// because the bytes came from a different tree. This is the core 2026-06-05
    /// incident guard.
    #[test]
    fn drift_fires_on_same_sha_different_source() {
        let all = vec![(0usize, live(sha_a())), (1usize, over(sha_a()))];
        let d = detect_slot_sha_drift(0, &live(sha_a()), &all)
            .expect("same sha, different source must surface drift");
        assert_eq!(d.conflicting.len(), 1);
        assert_eq!(d.conflicting[0].0, 1);
        assert_eq!(d.conflicting[0].2, Some(BuildSource::Override));
    }

    /// All sidecar-present slots share the same `(sha, source)` — no drift.
    #[test]
    fn drift_silent_when_all_slots_agree() {
        let all = vec![
            (0usize, live(sha_a())),
            (1usize, live(sha_a())),
            (2usize, live(sha_a())),
        ];
        assert!(detect_slot_sha_drift(0, &live(sha_a()), &all).is_none());
    }

    /// Picked slot has no sidecar — drift is silent (unknown provenance can't compare).
    #[test]
    fn drift_silent_when_picked_provenance_missing() {
        let all = vec![(0usize, absent()), (1usize, live(sha_b()))];
        assert!(detect_slot_sha_drift(0, &absent(), &all).is_none());
    }

    /// Other slots have no sidecar — drift is silent (no conflict to surface).
    #[test]
    fn drift_silent_when_other_slots_have_no_sidecar() {
        let all = vec![
            (0usize, live(sha_a())),
            (1usize, absent()),
            (2usize, absent()),
        ];
        assert!(detect_slot_sha_drift(0, &live(sha_a()), &all).is_none());
    }

    /// Three slots, two carry distinct provenance — both surface in `conflicting`.
    #[test]
    fn drift_collects_all_distinct_others() {
        let all = vec![
            (0usize, live(sha_a())),
            (1usize, live(sha_b())),
            (2usize, live(sha_c())),
        ];
        let d = detect_slot_sha_drift(0, &live(sha_a()), &all)
            .expect("two distinct others must surface");
        assert_eq!(d.conflicting.len(), 2);
        // Sorted by slot id deterministically.
        assert_eq!(d.conflicting[0].0, 1);
        assert_eq!(d.conflicting[1].0, 2);
    }

    /// `format_drift_warning` includes the picked slot id, abbreviated SHA,
    /// the source label, and the conflict count.
    #[test]
    fn drift_warning_message_shape() {
        let d = SlotShaDrift {
            picked_slot_id: 0,
            picked_sha: sha_a(),
            picked_source: BuildSource::LiveTree,
            conflicting: vec![(1, sha_b(), Some(BuildSource::Override))],
        };
        let msg = format_drift_warning(&d);
        assert!(msg.contains("picked slot 0"));
        assert!(msg.contains("aaaaaaaaaaaa"), "{}", msg);
        assert!(msg.contains("slot 1"), "{}", msg);
        assert!(msg.contains("bbbbbbbbbbbb"), "{}", msg);
        assert!(msg.contains("source live_tree"), "{}", msg);
        assert!(msg.contains("source override"), "{}", msg);
        assert!(
            msg.contains("proj_supervisor_slot_resolution_order"),
            "warning must point operator at the relevant memory: {}",
            msg
        );
    }

    /// Pluralization: multiple conflicting slots produce "provenances", not "provenance".
    #[test]
    fn drift_warning_pluralizes_multiple_conflicts() {
        let d = SlotShaDrift {
            picked_slot_id: 0,
            picked_sha: sha_a(),
            picked_source: BuildSource::LiveTree,
            conflicting: vec![
                (1, sha_b(), Some(BuildSource::LiveTree)),
                (2, sha_c(), Some(BuildSource::LiveTree)),
            ],
        };
        let msg = format_drift_warning(&d);
        assert!(msg.contains("distinct provenances"), "{}", msg);
    }

    // =========================================================================
    // Provenance sidecar IO (read_slot_provenance / read_slot_sha)
    // =========================================================================

    fn write_provenance(dir: &std::path::Path, p: &BuildProvenance) {
        let debug = dir.join("debug");
        std::fs::create_dir_all(&debug).expect("mkdir debug");
        let sidecar = debug.join(SLOT_PROVENANCE_SIDECAR_FILENAME);
        std::fs::write(&sidecar, serde_json::to_string(p).expect("serialize")).expect("write");
    }

    /// Round-trip: write provenance JSON, read returns the identical struct,
    /// and the serialized `source` uses the wire labels `live_tree`/`override`.
    #[test]
    fn read_slot_provenance_round_trip() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let prov = BuildProvenance {
            sha: Some(sha_a()),
            source: BuildSource::Override,
            built_from: "/some/abs/worktree".to_string(),
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
        };
        write_provenance(dir.path(), &prov);

        // Raw JSON carries the wire shape we promised consumers.
        let raw = std::fs::read_to_string(
            dir.path()
                .join("debug")
                .join(SLOT_PROVENANCE_SIDECAR_FILENAME),
        )
        .expect("read raw");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parse raw");
        assert_eq!(v["sha"], serde_json::json!(sha_a()));
        assert_eq!(v["source"], serde_json::json!("override"));
        assert_eq!(v["built_from"], serde_json::json!("/some/abs/worktree"));

        let got = read_slot_provenance(dir.path()).expect("must read");
        assert_eq!(got, prov);
        // The convenience SHA accessor mirrors the provenance sha.
        assert_eq!(read_slot_sha(dir.path()), Some(sha_a()));
    }

    /// A `live_tree` source serializes to `"live_tree"`.
    #[test]
    fn provenance_live_tree_source_wire_label() {
        let prov = BuildProvenance {
            sha: Some(sha_a()),
            source: BuildSource::LiveTree,
            built_from: "/live/tree".to_string(),
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&prov).unwrap()).unwrap();
        assert_eq!(v["source"], serde_json::json!("live_tree"));
    }

    /// `sha: null` round-trips (the git probe failed at build time).
    #[test]
    fn read_slot_provenance_null_sha() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let prov = BuildProvenance {
            sha: None,
            source: BuildSource::LiveTree,
            built_from: "/live/tree".to_string(),
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
        };
        write_provenance(dir.path(), &prov);
        let got = read_slot_provenance(dir.path()).expect("must read");
        assert_eq!(got.sha, None);
        assert_eq!(read_slot_sha(dir.path()), None);
    }

    /// Missing sidecar — no error, returns None.
    #[test]
    fn read_slot_provenance_missing_returns_none() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        assert!(read_slot_provenance(dir.path()).is_none());
        assert!(read_slot_sha(dir.path()).is_none());
    }

    /// A legacy plain-SHA file (the old `qontinui-runner.exe.git_sha` content,
    /// or any non-JSON) under the new filename is unparseable → treated as
    /// absent. Slots self-heal on the next build.
    #[test]
    fn read_slot_provenance_legacy_plain_sha_returns_none() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let debug = dir.path().join("debug");
        std::fs::create_dir_all(&debug).expect("mkdir debug");
        // Old format: a bare 40-hex SHA, no JSON.
        std::fs::write(
            debug.join(SLOT_PROVENANCE_SIDECAR_FILENAME),
            sha_a().as_bytes(),
        )
        .expect("write");
        assert!(read_slot_provenance(dir.path()).is_none());
        assert!(read_slot_sha(dir.path()).is_none());
    }

    /// Empty / whitespace-only sidecar — returns None (unparseable).
    #[test]
    fn read_slot_provenance_blank_returns_none() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let debug = dir.path().join("debug");
        std::fs::create_dir_all(&debug).expect("mkdir debug");
        std::fs::write(debug.join(SLOT_PROVENANCE_SIDECAR_FILENAME), b"   \n\t  ").expect("write");
        assert!(read_slot_provenance(dir.path()).is_none());
    }

    // =========================================================================
    // exe-copy parent-dir creation (start_exe_mode_for_runner copy step)
    // =========================================================================

    /// The copy-never-run-from-slot step in `start_exe_mode_for_runner` must
    /// create `target/debug/` before copying the slot/LKG exe into it.
    /// Supervisor-managed trees only ever materialize `target-pool/`, so a
    /// tree that has never had a default `cargo build` won't have
    /// `target/debug/` and the copy would fail with `os error 3`
    /// (path not found). Mirrors the inline mkdir-then-copy step at the same
    /// abstraction level (the copy itself lives inside the async
    /// process-spawning `start_exe_mode_for_runner`, which isn't unit-testable
    /// without launching a real process).
    #[test]
    fn exe_copy_creates_missing_target_debug_parent() {
        let root = tempfile::TempDir::new().expect("tempdir");

        // Source exe lives where a build slot would have put it.
        let slot_debug = root.path().join("target-pool").join("slot-0").join("debug");
        std::fs::create_dir_all(&slot_debug).expect("mkdir slot debug");
        let source_exe = slot_debug.join("qontinui-runner.exe");
        std::fs::write(&source_exe, b"fake-exe-bytes").expect("write source exe");

        // Copy target's parent (`target/debug/`) deliberately does NOT exist.
        let copy_path = root
            .path()
            .join("target")
            .join("debug")
            .join("qontinui-runner-test-9877.exe");
        let parent = copy_path.parent().expect("copy_path has a parent");
        assert!(
            !parent.exists(),
            "precondition: target/debug must be absent"
        );

        // The fix: create_dir_all(parent) before the copy.
        std::fs::create_dir_all(parent).expect("create_dir_all must succeed");
        assert!(parent.is_dir(), "target/debug should now exist");

        // And the copy then succeeds (previously failed with os error 3).
        std::fs::copy(&source_exe, &copy_path).expect("copy into freshly-created dir");
        assert!(copy_path.exists(), "exe copy should land in the new dir");
        assert_eq!(
            std::fs::read(&copy_path).expect("read copy"),
            b"fake-exe-bytes"
        );
    }

    // =========================================================================
    // qontinui-shim sidecar deploy (start_exe_mode_for_runner copy step).
    // The runner materializes identity shims from the stub next to its OWN
    // exe, so the stub must ride along with every exe copy — 2026-07-03
    // incident: a stale stub was re-materialized into every terminal.
    // =========================================================================

    /// Happy path: shim next to the slot exe is copied next to the per-runner
    /// exe copy, replacing any stale stub already there.
    #[test]
    fn shim_sidecar_copies_next_to_dest_exe_and_replaces_stale() {
        let root = tempfile::TempDir::new().expect("tempdir");

        let slot_debug = root.path().join("target-pool").join("slot-0").join("debug");
        std::fs::create_dir_all(&slot_debug).expect("mkdir slot debug");
        let source_exe = slot_debug.join("qontinui-runner.exe");
        std::fs::write(&source_exe, b"exe").expect("write exe");
        std::fs::write(
            slot_debug.join(crate::build_monitor::SHIM_EXE_FILENAME),
            b"fresh-shim",
        )
        .expect("write shim");

        let target_debug = root.path().join("target").join("debug");
        std::fs::create_dir_all(&target_debug).expect("mkdir target debug");
        let dest_exe = target_debug.join("qontinui-runner-test-9877.exe");
        std::fs::write(&dest_exe, b"exe-copy").expect("write exe copy");
        // Pre-existing STALE stub — the exact incident artifact.
        let dest_shim = target_debug.join(crate::build_monitor::SHIM_EXE_FILENAME);
        std::fs::write(&dest_shim, b"stale-shim").expect("write stale shim");

        match deploy_shim_sidecar(&source_exe, &dest_exe) {
            ShimSidecarDeploy::Copied { to } => assert_eq!(to, dest_shim),
            other => panic!("expected Copied, got {:?}", other),
        }
        assert_eq!(
            std::fs::read(&dest_shim).expect("read deployed shim"),
            b"fresh-shim",
            "the stale stub must be replaced by the source's shim"
        );
        // No tmp litter left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&target_debug)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left: {:?}", leftovers);
    }

    /// Legacy fallback resolution (`target/debug/qontinui-runner.exe`): source
    /// and dest share a dir, so there is nothing to copy — and critically no
    /// self-copy that could truncate the stub in place.
    #[test]
    fn shim_sidecar_same_dir_is_noop() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let debug = root.path().join("target").join("debug");
        std::fs::create_dir_all(&debug).expect("mkdir");
        let source_exe = debug.join("qontinui-runner.exe");
        std::fs::write(&source_exe, b"exe").expect("write");
        let shim = debug.join(crate::build_monitor::SHIM_EXE_FILENAME);
        std::fs::write(&shim, b"shim-bytes").expect("write shim");
        let dest_exe = debug.join("qontinui-runner-primary.exe");

        assert!(matches!(
            deploy_shim_sidecar(&source_exe, &dest_exe),
            ShimSidecarDeploy::SameDir
        ));
        assert_eq!(
            std::fs::read(&shim).expect("read shim"),
            b"shim-bytes",
            "the in-place stub must be untouched"
        );
    }

    /// Source without a shim (pre-sidecar slot/LKG, or the fail-open shim
    /// build failed): reports SourceMissing naming the expected path, and
    /// leaves the destination dir untouched. The caller logs the
    /// "identity shims will be stale" WARN — the start itself proceeds.
    #[test]
    fn shim_sidecar_missing_source_reports_and_writes_nothing() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let slot_debug = root.path().join("target-pool").join("slot-1").join("debug");
        std::fs::create_dir_all(&slot_debug).expect("mkdir");
        let source_exe = slot_debug.join("qontinui-runner.exe");
        std::fs::write(&source_exe, b"exe").expect("write");

        let target_debug = root.path().join("target").join("debug");
        std::fs::create_dir_all(&target_debug).expect("mkdir");
        let dest_exe = target_debug.join("qontinui-runner-primary.exe");

        match deploy_shim_sidecar(&source_exe, &dest_exe) {
            ShimSidecarDeploy::SourceMissing { expected } => {
                assert_eq!(
                    expected,
                    slot_debug.join(crate::build_monitor::SHIM_EXE_FILENAME)
                );
            }
            other => panic!("expected SourceMissing, got {:?}", other),
        }
        assert!(
            !target_debug
                .join(crate::build_monitor::SHIM_EXE_FILENAME)
                .exists(),
            "no shim must be fabricated at the destination"
        );
    }

    // =========================================================================
    // Start provenance gate (Phase 3): non-temp start refuses a known-foreign
    // (override) slot exe; temp stays permissive; unknown warns; live allows.
    // =========================================================================

    fn override_prov(sha: Option<String>) -> BuildProvenance {
        BuildProvenance {
            sha,
            source: BuildSource::Override,
            built_from: "/some/abs/.spawn-feat-x/qontinui-runner".to_string(),
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
        }
    }
    fn live_prov(sha: Option<String>) -> BuildProvenance {
        BuildProvenance {
            sha,
            source: BuildSource::LiveTree,
            built_from: "/live/tree".to_string(),
            built_at: "2026-06-05T12:00:00+00:00".to_string(),
        }
    }
    fn origin_main_prov(sha: Option<String>) -> BuildProvenance {
        BuildProvenance {
            sha,
            source: BuildSource::OriginMain,
            built_from: "/ws/.spawn-origin-main/qontinui-runner".to_string(),
            built_at: "2026-06-07T12:00:00+00:00".to_string(),
        }
    }

    /// LKG-eligibility / start-eligibility predicate: `LiveTree` AND `OriginMain`
    /// are vouched (true); `Override` is not (false). This is the single
    /// predicate behind both the LKG promotion gate and the non-temp start gate
    /// — Phase B widens it to include `OriginMain`.
    #[test]
    fn build_source_is_vouched_predicate() {
        assert!(
            BuildSource::LiveTree.is_vouched(),
            "live tree must be vouched"
        );
        assert!(
            BuildSource::OriginMain.is_vouched(),
            "origin/main must be vouched (LKG-eligible + startable as primary)"
        );
        assert!(
            !BuildSource::Override.is_vouched(),
            "override must NOT be vouched"
        );
    }

    /// Temp runner: always allowed, regardless of provenance. Temp runners
    /// exist to run foreign refs.
    #[test]
    fn start_gate_temp_always_ok() {
        // override
        assert_eq!(
            start_provenance_gate(true, 0, Some(&override_prov(Some(sha_a())))).unwrap(),
            None
        );
        // live tree
        assert_eq!(
            start_provenance_gate(true, 1, Some(&live_prov(Some(sha_b())))).unwrap(),
            None
        );
        // unknown
        assert_eq!(start_provenance_gate(true, 2, None).unwrap(), None);
    }

    /// Non-temp + positive override evidence: refuse with an error naming the
    /// slot, the provenance (built_from + sha), and the recovery path.
    #[test]
    fn start_gate_non_temp_override_refuses_with_recovery() {
        let err = start_provenance_gate(false, 2, Some(&override_prov(Some(sha_a()))))
            .expect_err("override must refuse");
        let msg = err.to_string();
        // Names the slot.
        assert!(msg.contains("slot 2"), "missing slot id: {msg}");
        // Names the provenance detail.
        assert!(msg.contains("source=override"), "missing source: {msg}");
        assert!(
            msg.contains(".spawn-feat-x/qontinui-runner"),
            "missing built_from: {msg}"
        );
        assert!(msg.contains(&sha_a()), "missing sha: {msg}");
        // Names the recovery.
        assert!(
            msg.contains("POST /runner/fix-and-rebuild"),
            "missing recovery: {msg}"
        );
        // Maps to a 500 through existing start-failure plumbing.
        assert_eq!(
            err.to_status_body().0,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// Non-temp + override with no sha still refuses and renders `(unknown)`.
    #[test]
    fn start_gate_non_temp_override_null_sha_still_refuses() {
        let err = start_provenance_gate(false, 0, Some(&override_prov(None)))
            .expect_err("override must refuse even without a sha");
        assert!(err.to_string().contains("sha=(unknown)"), "{err}");
    }

    /// Non-temp + unknown provenance (no sidecar): warn-and-proceed, NOT a
    /// refusal. Avoids bricking the first watchdog auto-start after a deploy.
    #[test]
    fn start_gate_non_temp_unknown_warns_proceeds() {
        let out = start_provenance_gate(false, 1, None).expect("unknown must not error");
        let StartProvenanceWarning(msg) = out.expect("unknown must produce a warning");
        assert!(msg.contains("slot 1"), "{msg}");
        assert!(msg.to_lowercase().contains("unknown"), "{msg}");
    }

    /// Non-temp + live-tree provenance: allowed regardless of sha. main
    /// advancing between build and start is staleness, not a provenance lie.
    #[test]
    fn start_gate_non_temp_live_tree_ok_regardless_of_sha() {
        assert_eq!(
            start_provenance_gate(false, 0, Some(&live_prov(Some(sha_a())))).unwrap(),
            None
        );
        // A different (stale) sha is still fine — no sha gating.
        assert_eq!(
            start_provenance_gate(false, 0, Some(&live_prov(Some(sha_c())))).unwrap(),
            None
        );
        // Even a null sha live-tree build is allowed.
        assert_eq!(
            start_provenance_gate(false, 0, Some(&live_prov(None))).unwrap(),
            None
        );
    }

    /// Non-temp + origin/main provenance: ALLOWED (Phase B). An origin/main
    /// worktree build is canonical merged truth — folding it into the Override
    /// refusal would brick every primary start. Allowed regardless of sha,
    /// exactly like live-tree.
    #[test]
    fn start_gate_non_temp_origin_main_ok() {
        assert_eq!(
            start_provenance_gate(false, 0, Some(&origin_main_prov(Some(sha_a())))).unwrap(),
            None
        );
        // A different sha is still fine — no sha gating.
        assert_eq!(
            start_provenance_gate(false, 0, Some(&origin_main_prov(Some(sha_c())))).unwrap(),
            None
        );
        // Even a null sha origin/main build is allowed.
        assert_eq!(
            start_provenance_gate(false, 0, Some(&origin_main_prov(None))).unwrap(),
            None
        );
    }

    /// Integration-style: a slot whose on-disk provenance sidecar says
    /// `override` makes a NON-temp (primary) start fail with the documented
    /// recovery message, while a `test-*` spawn resolving the SAME slot still
    /// works. Exercises the real `read_slot_provenance` read path + the gate
    /// together, reusing the Phase 1 temp-dir slot fixture (`write_provenance`).
    #[test]
    fn start_gate_same_override_slot_refuses_primary_allows_temp() {
        let slot_dir = tempfile::TempDir::new().expect("tempdir");
        // Phase 1 fixture: write a real override provenance sidecar into the
        // slot's target dir.
        write_provenance(slot_dir.path(), &override_prov(Some(sha_a())));
        let prov = read_slot_provenance(slot_dir.path());
        assert!(prov.is_some(), "fixture must produce readable provenance");

        // Same slot id (7), same provenance. Primary (non-temp) is refused...
        let primary_is_temp = is_temp_runner("primary");
        assert!(!primary_is_temp, "primary must be non-temp");
        let primary = start_provenance_gate(primary_is_temp, 7, prov.as_ref());
        let err = primary.expect_err("primary start must be refused for an override slot");
        assert!(err.to_string().contains("slot 7"), "{err}");
        assert!(
            err.to_string().contains("POST /runner/fix-and-rebuild"),
            "{err}"
        );

        // ...while a test-* spawn resolving the SAME slot is allowed.
        let temp_is_temp = is_temp_runner("test-9877");
        assert!(temp_is_temp, "test-* must be temp");
        let temp = start_provenance_gate(temp_is_temp, 7, prov.as_ref());
        assert_eq!(temp.expect("temp start must be allowed"), None);
    }

    // =========================================================================
    // unverified_exe_gate — "refuse stale" on the non-pool artifact
    // =========================================================================

    fn resolved(
        origin: ExeOrigin,
        provenance: Option<BuildProvenance>,
        mtime: Option<std::time::SystemTime>,
    ) -> ResolvedRunnerExe {
        ResolvedRunnerExe {
            path: std::path::PathBuf::from("/ws/qontinui-runner/target/debug/q.exe"),
            origin,
            mtime,
            provenance,
            unverified_warning: None,
        }
    }

    fn prov(source: BuildSource) -> BuildProvenance {
        BuildProvenance {
            sha: Some("a".repeat(40)),
            source,
            built_from: "/ws/qontinui-runner".to_string(),
            built_at: "2026-08-06T02:54:00Z".to_string(),
        }
    }

    /// The defect itself: a non-pool artifact with NO build identity must be
    /// refused, not launched — and the refusal must name the path, the mtime
    /// and the (absent) identity so the operator can see what was rejected.
    #[test]
    fn unverified_exe_gate_refuses_an_identity_less_non_pool_exe() {
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
        let r = resolved(
            ExeOrigin::CargoTargetDir(TargetDirSource::WorkspaceDefault),
            None,
            Some(mtime),
        );
        let err = unverified_exe_gate(false, &r).expect_err("unknown identity must refuse");
        match &err {
            SupervisorError::UnverifiedExe(info) => {
                let crate::error::UnverifiedExeInfo {
                    path,
                    mtime,
                    build_sha,
                    build_source,
                    target_dir_source,
                    detail,
                } = info.as_ref();
                assert!(path.contains("q.exe"), "names the path: {path}");
                assert!(mtime.is_some(), "carries the mtime");
                assert_eq!(*build_sha, None);
                assert_eq!(*build_source, None);
                assert_eq!(target_dir_source, "workspace_default");
                assert!(detail.contains("ABSENT"), "identity reads absent: {detail}");
                assert!(
                    detail.contains("allow_stale_fallback"),
                    "names the opt-in: {detail}"
                );
            }
            other => panic!("expected UnverifiedExe, got {other:?}"),
        }
        // 409, not 500: the request is fine, the on-disk state is not.
        let (status, body) = err.to_status_body();
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert_eq!(body["error"], "unverified_runner_exe");
    }

    /// Absence is refused; so is POSITIVE evidence of a foreign tree. Only a
    /// vouched build passes silently.
    #[test]
    fn unverified_exe_gate_refuses_an_override_built_non_pool_exe() {
        let r = resolved(
            ExeOrigin::CargoTargetDir(TargetDirSource::CargoTargetDirEnv),
            Some(prov(BuildSource::Override)),
            None,
        );
        let err = unverified_exe_gate(false, &r).expect_err("override identity must refuse");
        let msg = err.to_string();
        assert!(msg.contains("override"), "names the foreign source: {msg}");
    }

    #[test]
    fn unverified_exe_gate_allows_a_vouched_non_pool_exe() {
        for source in [BuildSource::LiveTree, BuildSource::OriginMain] {
            let r = resolved(
                ExeOrigin::CargoTargetDir(TargetDirSource::CargoTargetDirEnv),
                Some(prov(source)),
                None,
            );
            assert_eq!(
                unverified_exe_gate(false, &r).expect("vouched identity must pass"),
                ExeIdentityVerdict::Verified,
                "{source:?} is a vouched supervisor build"
            );
        }
    }

    /// The opt-in does not go quiet — it STATES the staleness, so a caller that
    /// deliberately asked for "whatever exists" still cannot mistake the result
    /// for evidence about a branch.
    #[test]
    fn unverified_exe_gate_opt_in_allows_but_states_the_staleness() {
        let r = resolved(
            ExeOrigin::CargoTargetDir(TargetDirSource::WorkspaceDefault),
            None,
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000)),
        );
        match unverified_exe_gate(true, &r).expect("opt-in must allow") {
            ExeIdentityVerdict::AllowedByOptIn(msg) => {
                assert!(msg.contains("UNVERIFIED"), "states it is unverified: {msg}");
                assert!(msg.contains("q.exe"), "names the path: {msg}");
                assert!(
                    msg.contains("workspace_default"),
                    "names which path won: {msg}"
                );
            }
            other => panic!("expected AllowedByOptIn, got {other:?}"),
        }
    }

    /// Slot artifacts keep their existing posture — this gate must not
    /// second-guess `start_provenance_gate` and brick the normal path.
    #[test]
    fn unverified_exe_gate_never_touches_slot_or_pinned_artifacts() {
        for origin in [ExeOrigin::Slot(1), ExeOrigin::PinnedOverride] {
            let r = resolved(origin, None, None);
            assert_eq!(
                unverified_exe_gate(false, &r).expect("must not refuse"),
                ExeIdentityVerdict::Verified,
                "{origin:?} is governed elsewhere"
            );
        }
    }

    #[test]
    fn exe_origin_labels_name_which_path_won() {
        assert_eq!(ExeOrigin::Slot(2).label(), "slot-2");
        assert_eq!(
            ExeOrigin::CargoTargetDir(TargetDirSource::CargoTargetDirEnv).label(),
            "cargo_target_dir:cargo_target_dir_env"
        );
        assert_eq!(ExeOrigin::PinnedOverride.label(), "pinned_override");
    }

    // =========================================================================
    // pick_slot_decision — guards that the sidecar instrumentation didn't shift
    // resolution behavior. Slot selection must remain:
    //   1. last_successful_slot (if its exe exists)
    //   2. first slot by iteration order whose exe exists
    //   3. None
    // =========================================================================

    fn fake_slots(ids_with_paths: &[(usize, &str)]) -> Vec<(usize, std::path::PathBuf)> {
        ids_with_paths
            .iter()
            .map(|(id, p)| (*id, std::path::PathBuf::from(p)))
            .collect()
    }

    /// last_successful_slot wins when its exe exists, even if other slots also have exes.
    #[test]
    fn pick_decision_prefers_last_successful_slot() {
        let slots = fake_slots(&[(0, "/a"), (1, "/b"), (2, "/c")]);
        let picked = pick_slot_decision(Some(1), &slots, |p| {
            p == std::path::Path::new("/a")
                || p == std::path::Path::new("/b")
                || p == std::path::Path::new("/c")
        });
        assert_eq!(picked, Some(1));
    }

    /// last_successful_slot is recorded but its exe is missing — fall through to
    /// first-by-index scan. This is the multi-slot-staleness scenario the
    /// memory was written about.
    #[test]
    fn pick_decision_falls_through_when_recorded_slot_missing() {
        let slots = fake_slots(&[(0, "/a"), (1, "/b"), (2, "/c")]);
        // Recorded slot is 2, but only slots 0 and 1 have exes.
        let picked = pick_slot_decision(Some(2), &slots, |p| {
            p == std::path::Path::new("/a") || p == std::path::Path::new("/b")
        });
        // Scan returns first-by-index, NOT newest-by-anything.
        assert_eq!(picked, Some(0));
    }

    /// No last_successful_slot, scan picks the lowest-id slot with an exe
    /// (this is exactly the silent-staleness quirk the sidecar surfaces).
    #[test]
    fn pick_decision_scan_returns_first_by_index() {
        let slots = fake_slots(&[(0, "/a"), (1, "/b"), (2, "/c")]);
        let picked = pick_slot_decision(None, &slots, |p| p == std::path::Path::new("/b"));
        assert_eq!(picked, Some(1));
        // Even if multiple slots have exes, the lower id still wins.
        let picked2 = pick_slot_decision(None, &slots, |p| {
            p == std::path::Path::new("/b") || p == std::path::Path::new("/c")
        });
        assert_eq!(picked2, Some(1));
    }

    /// No exe anywhere — None, caller falls back to legacy.
    #[test]
    fn pick_decision_none_when_no_exe_exists() {
        let slots = fake_slots(&[(0, "/a"), (1, "/b")]);
        let picked = pick_slot_decision(Some(0), &slots, |_| false);
        assert_eq!(picked, None);
        let picked2 = pick_slot_decision(None, &slots, |_| false);
        assert_eq!(picked2, None);
    }

    /// last_successful_slot points at an id NOT in the slots list (e.g. stale
    /// state after pool size shrink) — must fall through cleanly, not panic.
    #[test]
    fn pick_decision_handles_unknown_recorded_slot() {
        let slots = fake_slots(&[(0, "/a")]);
        let picked = pick_slot_decision(Some(99), &slots, |p| p == std::path::Path::new("/a"));
        assert_eq!(picked, Some(0));
    }

    // =========================================================================
    // Legacy target/debug/ staleness detection
    // (feedback_runner_manual_build — sibling failure mode of slot drift)
    //
    // The pure comparison logic (`compute_target_debug_staleness`) is exercised
    // with synthetic SystemTime values so the staleness rule can be tested
    // without depending on filesystem mtime resolution. The I/O wrapper
    // (`detect_target_debug_staleness`) gets one round-trip sanity test
    // against a real tempdir to guard the read path.
    // =========================================================================

    fn legacy_p() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/qontinui-runner/target/debug/qontinui-runner.exe")
    }

    /// Legacy mtime strictly older than every slot mtime — staleness fires.
    #[test]
    fn target_debug_staleness_fires_when_older_than_all_slots() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let legacy = Some(t0);
        // Two slots, both newer than legacy.
        let slots = vec![
            Some(t0 + Duration::from_secs(3600)),
            Some(t0 + Duration::from_secs(7200)),
        ];
        let s = compute_target_debug_staleness(&legacy_p(), legacy, &slots)
            .expect("legacy older than every slot must surface staleness");
        assert_eq!(s.legacy_mtime, t0);
        // oldest_slot_mtime is the OLDER of the two slot mtimes.
        assert_eq!(s.oldest_slot_mtime, t0 + Duration::from_secs(3600));
    }

    /// Legacy exe doesn't exist (or its mtime read failed) — silent.
    #[test]
    fn target_debug_staleness_silent_when_no_legacy() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slots = vec![Some(t0)];
        assert!(
            compute_target_debug_staleness(&legacy_p(), None, &slots).is_none(),
            "missing legacy must yield None"
        );
    }

    // ── "A rebuild must take the latest code" ──────────────────────────
    //
    // The inverse of the staleness check above: the operator built fresh code
    // locally and the supervisor was about to run an older slot exe instead.
    // Only the stale-legacy direction was ever detected, which is why two
    // consecutive operator rebuilds silently ran 17.5-hour-old code with no
    // log line anywhere saying so.

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// THE DEFECT. A local build newer than the picked slot must be detected.
    #[test]
    fn pool_behind_local_build_detects_a_newer_local_build() {
        let f = compute_pool_behind_local_build(&legacy_p(), Some(t(2_000)), 0, Some(t(1_000)))
            .expect("newer legacy than picked slot must be a finding");
        assert_eq!(f.picked_slot_id, 0);
        assert_eq!(f.legacy_mtime, t(2_000));
        assert_eq!(f.picked_slot_mtime, t(1_000));
    }

    /// The healthy case: the slot IS the freshest artifact. Silent.
    #[test]
    fn pool_behind_local_build_silent_when_slot_is_newer() {
        assert!(
            compute_pool_behind_local_build(&legacy_p(), Some(t(1_000)), 0, Some(t(2_000)))
                .is_none()
        );
    }

    /// Equal mtimes are one build wave, not a finding (mirrors the staleness
    /// check's strict comparison).
    #[test]
    fn pool_behind_local_build_equal_mtimes_are_not_a_finding() {
        assert!(
            compute_pool_behind_local_build(&legacy_p(), Some(t(1_000)), 0, Some(t(1_000)))
                .is_none()
        );
    }

    /// An unknown timestamp is never reported as a finding.
    #[test]
    fn pool_behind_local_build_silent_on_unreadable_mtimes() {
        assert!(compute_pool_behind_local_build(&legacy_p(), None, 0, Some(t(1_000))).is_none());
        assert!(compute_pool_behind_local_build(&legacy_p(), Some(t(1_000)), 0, None).is_none());
    }

    /// A vouched sidecar at least as new as the exe is adoptable — this is what
    /// makes the operator's rebuild actually run.
    #[test]
    fn a_vouched_sidecar_describing_this_exe_is_adoptable() {
        for source in [BuildSource::LiveTree, BuildSource::OriginMain] {
            assert!(
                local_build_is_adoptable(Some(&prov(source)), Some(t(2_000)), t(2_000)),
                "{source:?} sidecar written with the exe must be adoptable"
            );
            assert!(local_build_is_adoptable(
                Some(&prov(source)),
                Some(t(2_001)),
                t(2_000)
            ));
        }
    }

    /// No sidecar ⇒ never adopted. mtime says WHEN a file was written, not what
    /// is in it, so an unidentified artifact is never promoted over a slot.
    #[test]
    fn an_unstamped_local_build_is_never_adopted() {
        assert!(!local_build_is_adoptable(None, Some(t(2_000)), t(2_000)));
        assert!(!local_build_is_adoptable(None, None, t(2_000)));
    }

    /// A foreign `override` tree is not vouched, so it cannot be adopted even
    /// with a perfectly fresh sidecar.
    #[test]
    fn an_override_tree_build_is_not_adoptable() {
        assert!(!local_build_is_adoptable(
            Some(&prov(BuildSource::Override)),
            Some(t(2_000)),
            t(2_000)
        ));
    }

    /// **The `tauri dev` trap.** `npm run tauri dev` rebuilds the SAME path
    /// without `custom-protocol` (blank window, frontendReady:false — observed
    /// 2026-08-05) and writes no sidecar. If it overwrites an exe whose older
    /// sidecar is still lying around, adopting on the strength of that stamp
    /// would launch a dev-mode binary as the primary. A sidecar older than the
    /// exe it claims to describe must be refused.
    #[test]
    fn a_sidecar_older_than_the_exe_is_refused() {
        assert!(!local_build_is_adoptable(
            Some(&prov(BuildSource::LiveTree)),
            Some(t(1_999)),
            t(2_000)
        ));
    }

    /// **The interaction that makes adoption actually work.** Resolution and
    /// the START GATE are two separate predicates, and adoption is only useful
    /// if they agree: `resolve_source_exe_detailed` hands back an adopted local
    /// build as `ExeOrigin::CargoTargetDir`, and the start path routes exactly
    /// that origin into [`unverified_exe_gate`], which REFUSES a non-pool exe
    /// it cannot identify. If the gate refused what resolution adopted, the
    /// primary would resolve a binary and then fail to launch it at all —
    /// strictly worse than the stale-code bug this change fixes.
    ///
    /// They agree because both key on the same fact: `local_build_is_adoptable`
    /// requires `source.is_vouched()`, and that is precisely the gate's allow
    /// condition. This test pins the agreement so a later tightening of the
    /// gate cannot silently brick the start path.
    #[test]
    fn an_adopted_local_build_passes_the_unverified_exe_gate() {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
        for source in [BuildSource::LiveTree, BuildSource::OriginMain] {
            let p = prov(source);
            assert!(
                local_build_is_adoptable(Some(&p), Some(mtime), mtime),
                "{source:?} must be adoptable"
            );
            let resolved = ResolvedRunnerExe {
                mtime: Some(mtime),
                path: legacy_p(),
                // The origin resolution ACTUALLY hands back for an adoption.
                // Pinning `CargoTargetDir` here would test the gate against a
                // shape the start path never sees.
                origin: ExeOrigin::AdoptedLocalBuild(TargetDirSource::WorkspaceDefault),
                provenance: Some(p),
                unverified_warning: None,
            };
            assert_eq!(
                unverified_exe_gate(false, &resolved).expect("must not refuse an adopted build"),
                ExeIdentityVerdict::Verified,
                "{source:?}: resolution adopted it, so the gate must not refuse it"
            );
        }
    }

    /// **Cross-repo contract.** This is the literal sidecar `dev-start.ps1`
    /// writes (captured from a real run, 2026-08-29). If either side drifts —
    /// a serde rename here, a key change there — adoption silently stops
    /// firing and the operator is back to running stale code with no error
    /// anywhere. Parsing the real bytes is the only thing that catches it.
    #[test]
    fn the_sidecar_dev_start_writes_deserializes_as_provenance() {
        let written = r#"{"sha":"65082b7f50fdeb26c2e9105695a017b40be4d764","source":"live_tree","built_from":"D:\\qontinui-root\\qontinui-runner","built_at":"2026-08-29T09:47:40.6091322Z"}"#;
        let p: BuildProvenance =
            serde_json::from_str(written).expect("dev-start.ps1's sidecar must parse");
        assert_eq!(p.source, BuildSource::LiveTree);
        assert!(p.source.is_vouched(), "live_tree must be adoptable");
        assert_eq!(
            p.sha.as_deref(),
            Some("65082b7f50fdeb26c2e9105695a017b40be4d764")
        );
        assert_eq!(p.built_from, r"D:\qontinui-root\qontinui-runner");
    }

    /// **The two slot-less origins must be distinguishable.** Adoption landed
    /// reporting itself as `CargoTargetDir`, so nothing separated "the
    /// operator's fresh vouched build is running" from "we fell through to the
    /// artifact nobody maintains" — and `slot_id().is_none()`, which the
    /// `LEGACY_EXE_FALLBACK` dev-state keys on, said the incident had happened
    /// in both cases.
    #[test]
    fn the_two_slotless_origins_report_opposite_legacy_fallback_verdicts() {
        let src = TargetDirSource::WorkspaceDefault;
        let fallthrough = ExeOrigin::CargoTargetDir(src);
        let adopted = ExeOrigin::AdoptedLocalBuild(src);

        // Both are slot-less — which is exactly why the old rule conflated them.
        assert_eq!(fallthrough.slot_id(), None);
        assert_eq!(adopted.slot_id(), None);

        // ...and they now answer the incident question oppositely.
        assert!(fallthrough.is_legacy_fallback());
        assert!(!adopted.is_legacy_fallback());

        // A slot and a pin are never the fallthrough either.
        assert!(!ExeOrigin::Slot(0).is_legacy_fallback());
        assert!(!ExeOrigin::PinnedOverride.is_legacy_fallback());

        // Machine-readable labels differ, so logs and API responses can tell
        // them apart too.
        assert_ne!(fallthrough.label(), adopted.label());
        assert_eq!(adopted.label(), "adopted_local_build:workspace_default");
    }

    /// Both non-pool origins report WHICH cargo precedence level produced them.
    /// `source_exe_json` renders this field, and it returned `null` for an
    /// adoption — withholding the env-override-vs-workspace-default split on
    /// precisely the path where an operator's own build is running.
    #[test]
    fn both_non_pool_origins_carry_their_target_dir_source() {
        for src in [
            TargetDirSource::CargoTargetDirEnv,
            TargetDirSource::CargoConfigBuildTargetDir,
            TargetDirSource::WorkspaceDefault,
        ] {
            assert_eq!(
                ExeOrigin::CargoTargetDir(src).target_dir_source(),
                Some(src)
            );
            assert_eq!(
                ExeOrigin::AdoptedLocalBuild(src).target_dir_source(),
                Some(src)
            );
        }
        assert_eq!(ExeOrigin::Slot(1).target_dir_source(), None);
        assert_eq!(ExeOrigin::PinnedOverride.target_dir_source(), None);
    }

    /// Write a fake local build plus an optional sidecar. The sidecar is
    /// written AFTER the exe, so its mtime is >= the exe's — the freshness rule
    /// adoption requires.
    fn plant_local_build(
        dir: &std::path::Path,
        sidecar: Option<BuildSource>,
    ) -> std::path::PathBuf {
        let exe = dir.join(crate::config::RUNNER_BIN_NAME);
        std::fs::write(&exe, b"exe").expect("write exe");
        if let Some(source) = sidecar {
            let body = serde_json::to_string(&prov(source)).expect("serialize provenance");
            std::fs::write(dir.join(SLOT_PROVENANCE_SIDECAR_FILENAME), body)
                .expect("write sidecar");
        }
        exe
    }

    /// Plant a slot exe and backdate it, so the comparison is deterministic
    /// regardless of filesystem timestamp granularity.
    fn plant_backdated_exe(path: &std::path::Path, body: &[u8]) {
        std::fs::write(path, body).expect("write exe");
        backdate(path);
    }

    /// Push a file's mtime an hour into the past. Explicit rather than relying
    /// on write ORDER: NTFS timestamp granularity is coarse enough that two
    /// writes microseconds apart can land on the same mtime, and the comparison
    /// under test uses a strict `>`.
    fn backdate(path: &std::path::Path) {
        let f = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for backdate");
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3_600))
            .expect("backdate exe");
    }

    /// **End-to-end composition, on a real tree.** `GET /builds` and the start
    /// path both go through this, so a defect here is a defect in what the
    /// operator is told AND in which binary runs. A vouched sidecar written
    /// with the exe ⇒ adopted.
    #[test]
    fn evaluate_local_build_adoption_adopts_a_vouched_newer_local_build() {
        let root = tempfile::tempdir().expect("tempdir");
        let local = root.path().join("target").join("debug");
        let slot = root.path().join("slot-0").join("debug");
        std::fs::create_dir_all(&local).expect("mkdir local");
        std::fs::create_dir_all(&slot).expect("mkdir slot");

        let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
        plant_backdated_exe(&slot_exe, b"slot");
        let local_exe = plant_local_build(&local, Some(BuildSource::LiveTree));

        let a = evaluate_local_build_adoption_at(
            &local_exe,
            TargetDirSource::WorkspaceDefault,
            0,
            &slot_exe,
        )
        .expect("a newer local build must produce a finding");
        assert!(a.adopted, "vouched sidecar written with the exe must adopt");
        assert_eq!(a.finding.picked_slot_id, 0);
        assert_eq!(a.target_dir_source, TargetDirSource::WorkspaceDefault);
        assert_eq!(
            a.provenance.as_ref().map(|p| p.source),
            Some(BuildSource::LiveTree)
        );
        let msg = a.message();
        assert!(msg.contains("running the local build"), "{msg}");
    }

    /// The unstamped case — a hand-run `cargo build`, or `npm run tauri dev`
    /// overwriting the path. Still REPORTED (the operator must learn their
    /// build is not running), but not adopted.
    #[test]
    fn evaluate_local_build_adoption_refuses_an_unstamped_newer_local_build() {
        let root = tempfile::tempdir().expect("tempdir");
        let local = root.path().join("target").join("debug");
        let slot = root.path().join("slot-1").join("debug");
        std::fs::create_dir_all(&local).expect("mkdir local");
        std::fs::create_dir_all(&slot).expect("mkdir slot");

        let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
        plant_backdated_exe(&slot_exe, b"slot");
        let local_exe = plant_local_build(&local, None);

        let a = evaluate_local_build_adoption_at(
            &local_exe,
            TargetDirSource::CargoTargetDirEnv,
            1,
            &slot_exe,
        )
        .expect("the finding is reported even when adoption is refused");
        assert!(!a.adopted, "no sidecar must never adopt");
        assert!(a.provenance.is_none());
        let msg = a.message();
        assert!(msg.contains("running the SLOT exe"), "{msg}");
        assert!(msg.contains("NOT what is running"), "{msg}");
    }

    /// The healthy steady state: the slot IS the freshest artifact. Nothing to
    /// report, so `GET /builds` renders `null` rather than a reassuring-looking
    /// object.
    #[test]
    fn evaluate_local_build_adoption_is_silent_when_the_slot_is_newer() {
        let root = tempfile::tempdir().expect("tempdir");
        let local = root.path().join("target").join("debug");
        let slot = root.path().join("slot-0").join("debug");
        std::fs::create_dir_all(&local).expect("mkdir local");
        std::fs::create_dir_all(&slot).expect("mkdir slot");

        let local_exe = plant_local_build(&local, Some(BuildSource::LiveTree));
        // Backdate the LOCAL build this time; the slot is written after it.
        backdate(&local_exe);
        let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
        std::fs::write(&slot_exe, b"slot").expect("write slot exe");

        assert!(evaluate_local_build_adoption_at(
            &local_exe,
            TargetDirSource::WorkspaceDefault,
            0,
            &slot_exe,
        )
        .is_none());
    }

    /// A missing local build is not a finding — an unreadable mtime is UNKNOWN,
    /// and unknown is never rendered as "your build is not running".
    #[test]
    fn evaluate_local_build_adoption_is_silent_without_a_local_build() {
        let root = tempfile::tempdir().expect("tempdir");
        let slot = root.path().join("slot-0").join("debug");
        std::fs::create_dir_all(&slot).expect("mkdir slot");
        let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
        plant_backdated_exe(&slot_exe, b"slot");
        let absent = root.path().join("target").join("debug").join("nope.exe");

        assert!(evaluate_local_build_adoption_at(
            &absent,
            TargetDirSource::WorkspaceDefault,
            0,
            &slot_exe,
        )
        .is_none());
    }

    /// The two warning texts must be distinguishable: one says the local build
    /// IS running, the other says it is NOT. Reporting the wrong one is worse
    /// than silence, because the operator would stop looking.
    #[test]
    fn the_warning_states_which_binary_actually_runs() {
        let f = compute_pool_behind_local_build(&legacy_p(), Some(t(2_000)), 0, Some(t(1_000)))
            .expect("finding");
        let adopted = format_pool_behind_local_build_warning(&f, true);
        let refused = format_pool_behind_local_build_warning(&f, false);
        assert!(adopted.contains("running the local build"), "{adopted}");
        assert!(refused.contains("running the SLOT exe"), "{refused}");
        assert!(refused.contains("NOT what is running"), "{refused}");
        assert_ne!(adopted, refused);
    }

    /// No slot exes exist — silent (no baseline to compare against).
    #[test]
    fn target_debug_staleness_silent_when_no_slots() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // All slot entries are None (no exe present in any slot).
        let slots_all_missing: Vec<Option<std::time::SystemTime>> = vec![None, None];
        assert!(
            compute_target_debug_staleness(&legacy_p(), Some(t0), &slots_all_missing).is_none(),
            "no slot exe means no baseline — must yield None"
        );
        // Truly empty slot list.
        let empty: Vec<Option<std::time::SystemTime>> = vec![];
        assert!(compute_target_debug_staleness(&legacy_p(), Some(t0), &empty).is_none());
    }

    /// Legacy is newer than at least one slot — silent. That other slot might
    /// be stale (PR #34's drift surface, if SHA-distinct), but THIS check
    /// only fires when legacy is older than EVERY slot.
    #[test]
    fn target_debug_staleness_silent_when_legacy_newer_than_any_slot() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let legacy = Some(t0 + Duration::from_secs(60));
        let slots = vec![
            Some(t0), // older than legacy — this is the one that prevents firing
            Some(t0 + Duration::from_secs(3600)),
        ];
        assert!(
            compute_target_debug_staleness(&legacy_p(), legacy, &slots).is_none(),
            "legacy newer than ANY slot must yield None"
        );
    }

    /// Equal mtimes (legacy == oldest slot) — silent. Strict `<` only.
    #[test]
    fn target_debug_staleness_silent_when_equal() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let slots = vec![Some(t0)];
        assert!(
            compute_target_debug_staleness(&legacy_p(), Some(t0), &slots).is_none(),
            "equal mtimes must yield None (strict ordering)"
        );
    }

    /// Mixed slot-readability: some slots have mtimes, some are None (failed
    /// reads / missing exes). Only the readable ones contribute to the
    /// staleness comparison.
    #[test]
    fn target_debug_staleness_skips_unreadable_slots() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let legacy = Some(t0);
        let slots = vec![
            None,                                 // slot-0 has no exe
            Some(t0 + Duration::from_secs(3600)), // slot-1 exists, newer
            None,                                 // slot-2 has no exe
        ];
        let s = compute_target_debug_staleness(&legacy_p(), legacy, &slots)
            .expect("legacy older than the one readable slot must fire");
        assert_eq!(s.oldest_slot_mtime, t0 + Duration::from_secs(3600));
    }

    /// Unreadable legacy mtime in the I/O wrapper — synthetic IO failure →
    /// returns None (debug log, no panic). Driven by pointing the function
    /// at a path inside a non-existent directory.
    #[test]
    fn target_debug_staleness_handles_unreadable_mtime() {
        let root = tempfile::TempDir::new().expect("tempdir");
        // legacy_path points inside a directory that doesn't exist —
        // `std::fs::metadata` returns Err with ErrorKind::NotFound.
        let bogus_legacy = root
            .path()
            .join("does-not-exist")
            .join("nested")
            .join("qontinui-runner.exe");
        // Also point slots at non-existent paths — verifies the wrapper
        // returns None without panicking when nothing is readable.
        let bogus_slot = root.path().join("slot-0").join("qontinui-runner.exe");
        let slots: Vec<(usize, &std::path::Path)> = vec![(0, &bogus_slot)];
        assert!(detect_target_debug_staleness(&bogus_legacy, &slots).is_none());
    }

    /// I/O wrapper sanity: real legacy file + a real slot file with legacy
    /// strictly older. Verifies the wrapper threads filesystem reads through
    /// to the pure helper correctly.
    #[test]
    fn target_debug_staleness_io_wrapper_roundtrip() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let legacy = root.path().join("legacy.exe");
        std::fs::write(&legacy, b"old").expect("write legacy");
        // Force >= 50ms gap so even coarse filesystem mtime resolution
        // produces a strict-less-than ordering. NTFS mtime res ~100ns,
        // FAT32 ~2s; we don't ship on FAT32 dev machines.
        std::thread::sleep(Duration::from_millis(50));
        let slot0 = root.path().join("slot-0.exe");
        std::fs::write(&slot0, b"new").expect("write slot");
        let slots: Vec<(usize, &std::path::Path)> = vec![(0, &slot0)];
        let s = detect_target_debug_staleness(&legacy, &slots)
            .expect("legacy older than slot (file-write order) must fire");
        assert_eq!(s.legacy_path, legacy);
        assert!(s.legacy_mtime < s.oldest_slot_mtime);
    }

    /// Warning message includes the legacy path, both ISO timestamps, and the
    /// pointer to feedback_runner_manual_build.
    #[test]
    fn target_debug_warning_message_shape() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let s = TargetDebugStaleness {
            legacy_path: legacy_p(),
            legacy_mtime: t0,
            oldest_slot_mtime: t0 + Duration::from_secs(3600),
        };
        let msg = format_target_debug_warning(&s);
        assert!(msg.contains("target_debug_staleness"), "{}", msg);
        assert!(msg.contains("qontinui-runner.exe"), "{}", msg);
        assert!(
            msg.contains("feedback_runner_manual_build"),
            "warning must point operator at the relevant memory: {}",
            msg
        );
        assert!(
            msg.contains("spawn-test {rebuild:false}"),
            "warning must name the failure mode: {}",
            msg
        );
    }

    // ── Phase 3 of `2026-09-03-runner-zombie-serving-watchdog`: the restart
    //    funnel can stop-then-start a wedged, adopted primary (closes S2). ──

    /// The non-temp automated-restart block, three ways: `Manual` admitted,
    /// `Watchdog` (the wire value any caller can claim) still blocked,
    /// `ServingWatchdog` (unconstructible from the wire) admitted. Temp
    /// runners are never blocked for any source.
    #[test]
    fn automated_restart_block_admits_manual_and_serving_watchdog_only() {
        use crate::diagnostics::RestartSource;
        assert!(!automated_restart_blocked(false, &RestartSource::Manual));
        assert!(automated_restart_blocked(false, &RestartSource::Watchdog));
        assert!(!automated_restart_blocked(
            false,
            &RestartSource::ServingWatchdog
        ));
        for source in [
            RestartSource::Manual,
            RestartSource::Watchdog,
            RestartSource::ServingWatchdog,
        ] {
            assert!(
                !automated_restart_blocked(true, &source),
                "temp runners are never blocked ({source})"
            );
        }
    }

    /// The stop predicate keys on ANY evidence of life. The S2 shape —
    /// `running=false` (overwritten from a silent `/health`), a PID, a held
    /// port — must stop; only the all-negative row skips the stop.
    #[test]
    fn restart_stop_predicate_keys_on_evidence_of_life_not_on_running() {
        // The S2 wedge: tracked flag false, process alive on the port.
        assert!(restart_should_stop(false, Some(247_696), true));
        // Linux adopted wedge: no PID recovered (the health cache's PID
        // recovery is Windows-only), port still held.
        assert!(restart_should_stop(false, None, true));
        // A PID with no port: the process exists, stop it.
        assert!(restart_should_stop(false, Some(1), false));
        // Tracked running alone (the old predicate) still stops.
        assert!(restart_should_stop(true, None, false));
        // Nothing to stop.
        assert!(!restart_should_stop(false, None, false));
    }

    /// Truth table of the pure half of the port-held guard: refusal needs BOTH
    /// a listener and a positively identified runner image.
    #[test]
    fn port_held_by_live_runner_truth_table() {
        assert!(port_held_by_live_runner(true, true));
        assert!(!port_held_by_live_runner(true, false));
        assert!(!port_held_by_live_runner(false, true));
        assert!(!port_held_by_live_runner(false, false));
    }

    /// Negative branch, end to end: a listener on an ephemeral port that is
    /// NOT a runner does not trigger the refusal. The test process holds the
    /// listener (which the Unix `lsof` probe excludes by PID, so the probe
    /// reports no holder) and a spawned `sleep` child stands in for "a live
    /// process that is not a qontinui-runner" — `is_qontinui_runner_pid` is
    /// false for it, which is the identity half of the same predicate. A
    /// dead port is the trivially-allowed case.
    #[tokio::test]
    async fn port_held_guard_does_not_refuse_a_non_runner_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(
            crate::process::port::is_port_listening(port),
            "the bind probe must see our own listener"
        );

        let mut sleeper = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let sleep_pid = sleeper.id().expect("sleep has a pid");
        assert!(
            !crate::process::proc_kill::is_qontinui_runner_pid(sleep_pid),
            "`sleep` must not read as a qontinui-runner image"
        );

        match refuse_if_port_held_by_live_runner(port).await {
            Ok(()) => {}
            Err(SupervisorError::PortHeldByLiveRunner { .. }) => {
                panic!("a non-runner listener must not produce PortHeldByLiveRunner")
            }
            Err(e) => panic!("unexpected error: {e}"),
        }

        drop(listener);
        let _ = sleeper.kill().await;

        // A port nothing holds is allowed without consulting any probe.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        assert!(refuse_if_port_held_by_live_runner(dead_port).await.is_ok());
    }

    fn restart_test_state() -> SharedState {
        use crate::config::{CliArgs, SupervisorConfig};
        use clap::Parser;
        let args = CliArgs::parse_from(["test", "--project-dir", "."]);
        Arc::new(crate::state::SupervisorState::new(
            SupervisorConfig::from_args(args),
        ))
    }

    /// A registered, UNPROTECTED named runner on a port nothing listens on.
    /// Unprotected so the readiness gate skips (`NotProtected`) and the
    /// funnel reaches the latch; a dead port so any stop confirms instantly
    /// and the start fails at exe resolution (this state has no build slot
    /// and no cargo target-dir artifact).
    async fn register_dead_named_runner(state: &SharedState, id: &str) -> Arc<ManagedRunner> {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let config = crate::config::RunnerConfig {
            id: id.to_string(),
            name: format!("Runner {id}"),
            port,
            kind: qontinui_types::wire::runner_kind::RunnerKind::Named {
                name: id.to_string(),
            },
            protected: false,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: Default::default(),
        };
        let managed = Arc::new(ManagedRunner::new(config, false));
        state
            .runners
            .write()
            .await
            .insert(id.to_string(), managed.clone());
        managed
    }

    /// `restart_requested` is cleared on the FAILURE exit too. A manual
    /// restart of a stopped runner whose exe cannot be resolved fails at
    /// `start_managed_runner`; before Phase 3 the latch stayed `true` forever
    /// (only the success path cleared it), which would have silenced the
    /// serving watchdog's `SkipOperatorIntent` after one failed attempt.
    #[tokio::test]
    async fn failed_restart_clears_restart_requested() {
        use crate::diagnostics::RestartSource;
        let state = restart_test_state();
        let managed = register_dead_named_runner(&state, "named-p3-start-fail").await;

        let err = restart_runner_by_id(
            &state,
            "named-p3-start-fail",
            false,
            RestartSource::Manual,
            false,
            true,
        )
        .await
        .expect_err("no exe can be resolved from an empty project dir");
        assert!(
            !matches!(err, SupervisorError::PortHeldByLiveRunner { .. }),
            "a dead port must not be reported as held: {err}"
        );

        let runner = managed.runner.read().await;
        assert!(
            !runner.restart_requested,
            "restart_requested must not stay latched after a failed start"
        );
        // No evidence of life → the stop was correctly skipped, so the
        // stop-intent marker was never latched.
        assert!(!runner.stop_requested);
    }

    /// The S2 fix end to end: with `running=false` but a held-port fact in
    /// the cached snapshot, the funnel now REACHES `stop_runner_by_id`
    /// (evidence: `stop_requested` latched, which only the stop path sets and
    /// only a successful spawn clears) before the start — and the latch is
    /// still cleared when that start fails.
    #[tokio::test]
    async fn restart_stops_on_held_port_evidence_even_when_running_is_false() {
        use crate::diagnostics::RestartSource;
        let state = restart_test_state();
        let managed = register_dead_named_runner(&state, "named-p3-s2").await;
        managed.cached_health.write().await.runner_port_open = true;
        assert!(!managed.runner.read().await.running);

        let err = restart_runner_by_id(
            &state,
            "named-p3-s2",
            false,
            RestartSource::Manual,
            false,
            true,
        )
        .await
        .expect_err("the start still fails at exe resolution");
        assert!(
            !matches!(err, SupervisorError::PortHeldByLiveRunner { .. }),
            "{err}"
        );

        let runner = managed.runner.read().await;
        assert!(
            runner.stop_requested,
            "the stop must have been reached on port-held evidence alone"
        );
        assert!(
            !runner.restart_requested,
            "restart_requested must be cleared on the failure exit after a stop"
        );
    }

    /// The wire `watchdog` source is still refused for a non-temp runner, and
    /// the refusal happens BEFORE anything is latched or emitted.
    #[tokio::test]
    async fn wire_watchdog_source_is_still_blocked_for_non_temp_runners() {
        use crate::diagnostics::RestartSource;
        let state = restart_test_state();
        let managed = register_dead_named_runner(&state, "named-p3-blocked").await;

        let err = restart_runner_by_id(
            &state,
            "named-p3-blocked",
            false,
            RestartSource::Watchdog,
            false,
            true,
        )
        .await
        .expect_err("wire watchdog must be blocked");
        assert!(matches!(err, SupervisorError::Validation(_)), "{err}");
        assert!(err.to_string().contains("blocked"), "{err}");
        assert!(!managed.runner.read().await.restart_requested);
    }
}
