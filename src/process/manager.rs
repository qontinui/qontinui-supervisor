use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::config::{RUNNER_GRACEFUL_STOP_REQUEST_TIMEOUT_MS, RUNNER_GRACEFUL_STOP_TIMEOUT_MS};
use crate::diagnostics::{DiagnosticEventKind, RestartSource};
use crate::error::SupervisorError;
use crate::log_capture::{LogLevel, LogSource};
use crate::process::env_forwarders;
use crate::process::port::wait_for_port_free;
#[cfg(target_os = "windows")]
use crate::process::windows::{
    kill_by_pid, kill_by_port, remove_instance_config_dir, remove_runner_app_data_dirs,
    remove_webview2_user_data_folder, webview2_user_data_folder,
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
        let path = slot.target_dir.join("debug").join("qontinui-runner.exe");
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
pub const SLOT_PROVENANCE_SIDECAR_FILENAME: &str = "qontinui-runner.exe.provenance.json";

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

fn source_label(src: BuildSource) -> &'static str {
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

/// Warning text for a NON-temp runner start that resolved to the legacy
/// `target/debug/` exe (no build-pool slot, therefore no provenance sidecar at
/// all). The legacy artifact is the most-unknown binary in the system — it is
/// the very path `detect_target_debug_staleness` exists to distrust — so the
/// unknown-provenance posture (warn-and-proceed, never refuse on absence)
/// applies to it the same as to a sidecar-less slot. Pure so it is
/// unit-testable.
pub fn legacy_exe_provenance_warning(exe: &std::path::Path) -> StartProvenanceWarning {
    StartProvenanceWarning(format!(
        "Starting non-temp runner from the legacy exe {exe:?} with UNKNOWN provenance          (no build-pool slot, no provenance sidecar). Proceeding: refusal keys on          positive evidence of a foreign exe only. Prefer a pool build          (POST /runner/fix-and-rebuild) so the deploy carries a provenance record."
    ))
}

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
        .map(|s| (s.id, s.target_dir.join("debug").join("qontinui-runner.exe")))
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
    SlotFreshness {
        picked_slot_id,
        slot_provenance,
        drift,
        target_debug_staleness,
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

/// Structured warning produced when the legacy `target/debug/qontinui-runner.exe`
/// is older than every build-pool slot exe. Pure data — emitted to logs and
/// `/builds`; never alters resolution. The legacy path is observability-only.
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
/// **Observability only.** Resolution order is unchanged; the legacy path is
/// never used as a fallback resolution source by [`resolve_source_exe`] (it
/// only falls back to the legacy path when NO slot has an exe, which is the
/// pre-pool case the legacy fallback originally covered).
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

/// Format a [`TargetDebugStaleness`] as a human-readable warning line.
pub fn format_target_debug_warning(s: &TargetDebugStaleness) -> String {
    let legacy_iso: chrono::DateTime<chrono::Utc> = s.legacy_mtime.into();
    let oldest_iso: chrono::DateTime<chrono::Utc> = s.oldest_slot_mtime.into();
    format!(
        "target_debug_staleness: legacy {} (mtime {}) is older than every \
         slot exe (oldest slot mtime {}). It will not be used by spawn-test \
         {{rebuild:false}}. Either rebuild via supervisor (build into a slot) \
         or delete the stale exe. See feedback_runner_manual_build.",
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
        .map(|s| (s.id, s.target_dir.join("debug").join("qontinui-runner.exe")))
        .collect();
    let slot_refs: Vec<(usize, &std::path::Path)> = slot_paths
        .iter()
        .map(|(id, p)| (*id, p.as_path()))
        .collect();
    detect_target_debug_staleness(&legacy, &slot_refs)
}

/// Locate the most recent successfully-built runner exe across the build pool.
///
/// Preference order:
/// 1. The exe in the slot recorded as `last_successful_slot` (fresh build).
/// 2. Any slot whose exe exists on disk (e.g. after a supervisor restart).
/// 3. The legacy `runner_exe_path()` (default `target/debug/`) for builds
///    that predate the build pool.
///
/// After picking a slot (preference 1 or 2), this function emits a `WARN`
/// log line when the picked slot's `.git_sha` sidecar differs from any other
/// slot's sidecar. The warning is observability only — resolution proceeds
/// with the picked slot regardless. See `proj_supervisor_slot_resolution_order`.
pub async fn resolve_source_exe(
    state: &SharedState,
) -> Result<std::path::PathBuf, SupervisorError> {
    resolve_source_exe_with_slot(state).await.map(|(_, p)| p)
}

/// [`resolve_source_exe`] variant that ALSO returns which build-pool slot the
/// exe came from (`None` for the legacy `target/debug/` fallback).
///
/// Exists so the start-path provenance gate can be evaluated on the SAME pick
/// the resolution deploys: the previous shape (gate runs its own
/// `pick_slot_for_resolution`, then `resolve_source_exe` re-picks) had a
/// race — a build succeeding between the two picks moves
/// `last_successful_slot`, so resolution could deploy a slot the gate never
/// evaluated. A freshly-succeeded OVERRIDE build is exactly what moves
/// `last_successful_slot`, i.e. the race window reintroduced the incident
/// class the gate exists to kill. One pick, gate and path from the same id.
pub async fn resolve_source_exe_with_slot(
    state: &SharedState,
) -> Result<(Option<usize>, std::path::PathBuf), SupervisorError> {
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

        return Ok((Some(picked_id), picked_path));
    }

    // Preference 3: legacy path for pre-pool builds. No slot id — callers that
    // gate on provenance treat this as the most-unknown artifact there is.
    let legacy = state.config.runner_exe_path();
    if legacy.exists() {
        return Ok((None, legacy));
    }

    Err(SupervisorError::Process(format!(
        "Runner exe not found in any build slot or at legacy path {:?}. Run a build first.",
        legacy
    )))
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
            } else if crate::process::port::is_port_in_use(r.config.port) {
                warn!(
                    "Runner '{}' port {} is occupied but /health is not responding — \
                     treating as offline (likely a stale socket from a just-killed process)",
                    r.config.name, r.config.port
                );
            }
        }
    }

    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut killed_any = false;
    #[cfg(target_os = "windows")]
    for &port in &ports {
        if let Ok(true) = kill_by_port(port).await {
            info!("Killed orphaned temp runner on port {}", port);
            killed_any = true;
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = ports;

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
///   - Its `running` flag is true but nothing is listening on its port (crash).
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
            if is_running {
                if crate::process::port::is_port_in_use(managed.config.port) {
                    continue; // genuinely alive
                }
                // Port free but state says running — crashed
                {
                    let mut runner = managed.runner.write().await;
                    runner.running = false;
                    runner.pid = None;
                }
            }

            let id = managed.config.id.clone();
            let name = managed.config.name.clone();
            let port = managed.config.port;

            #[cfg(target_os = "windows")]
            let _ = kill_by_port(port).await;

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

    // Assign the spawned process to the supervisor's kill-on-exit JobObject.
    // When the supervisor process dies (graceful exit, panic, force-kill, or
    // BSOD), the kernel closes the last handle to the job and terminates
    // every assigned process per `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    // WebView2 children of the runner are transitively in the job too —
    // Windows assigns child processes of a job-tracked process to the same
    // job by default.
    //
    // We assign AFTER `cmd.spawn()` because the runner is not started with
    // `CREATE_SUSPENDED`, so it's already executing. That's fine for
    // KILL_ON_JOB_CLOSE — the only correctness trap with post-spawn
    // assignment is BREAKAWAY_OK interactions where a child could escape
    // before assignment, which we don't rely on here.
    //
    // Assignment failure is loud but non-fatal — the runner is functional;
    // it just won't be auto-killed if the supervisor dies abruptly.
    if let (Some(job), Some(pid_val)) = (state.runner_job.as_ref(), pid) {
        match job.assign(pid_val) {
            Ok(()) => {
                let msg = format!(
                    "Assigned runner '{}' (PID {}) to kill-on-exit JobObject",
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
                    "Failed to assign runner '{}' (PID {}) to kill-on-exit JobObject: {}. \
                     Supervisor exit will not terminate this runner.",
                    runner_name, pid_val, e
                );
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Warn,
                        format!(
                            "Runner '{}' (PID {}) NOT assigned to kill-on-exit JobObject: {}. \
                             If the supervisor crashes, this runner may linger as an orphan.",
                            runner_name, pid_val, e
                        ),
                    )
                    .await;
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

    // Update per-runner state
    {
        let mut runner = managed.runner.write().await;
        runner.process = Some(child);
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
    }

    // If this is the primary runner, also update legacy state for backward compat
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.running = true;
        runner.started_at = Some(chrono::Utc::now());
        runner.pid = pid;
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

/// Result of spawning a runner in exe mode.
struct SpawnResult {
    child: tokio::process::Child,
    /// Directory the supervisor told the runner to write its panic log to
    /// via `QONTINUI_RUNNER_LOG_DIR`. `None` when the supervisor deferred
    /// to the runner's default path (no `--log-dir` configured).
    panic_log_dir: Option<std::path::PathBuf>,
}

/// Start exe mode for a specific runner with port/name env vars.
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
    let source_exe = {
        let override_path = managed.source_exe_override.read().await.clone();
        match override_path {
            Some(p) if p.exists() => {
                info!(
                    "Runner '{}' pinned to source exe override {:?}",
                    managed.config.name, p
                );
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
                let (picked_slot, resolved_path) = resolve_source_exe_with_slot(state).await?;
                match picked_slot {
                    Some(picked_id) => {
                        let prov = state
                            .build_pool
                            .slots
                            .iter()
                            .find(|s| s.id == picked_id)
                            .and_then(|s| read_slot_provenance(&s.target_dir));
                        if let Some(StartProvenanceWarning(msg)) =
                            start_provenance_gate(is_temp, picked_id, prov.as_ref())?
                        {
                            warn!("{}", msg);
                            state
                                .logs
                                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                                .await;
                        }
                    }
                    // Legacy `target/debug/` fallback — no slot, no sidecar:
                    // the most-unknown artifact in the system. Same
                    // unknown-provenance posture as a sidecar-less slot:
                    // warn-and-proceed for non-temp runners, never refuse on
                    // absence.
                    None => {
                        if !is_temp {
                            let StartProvenanceWarning(msg) =
                                legacy_exe_provenance_warning(&resolved_path);
                            warn!("{}", msg);
                            state
                                .logs
                                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                                .await;
                        }
                    }
                }
                resolved_path
            }
        }
    };

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

    info!(
        "Starting runner '{}' in exe mode from {:?} on port {}",
        managed.config.name, exe_path, managed.config.port
    );

    let mut cmd = Command::new(&exe_path);
    cmd.current_dir(&state.config.project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE")
        .env("QONTINUI_PORT", managed.config.port.to_string())
        .env(
            "QONTINUI_API_URL",
            std::env::var("QONTINUI_API_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8000".to_string()),
        );

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
        cmd.env("QONTINUI_INSTANCE_NAME", &managed.config.name);
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
        if let Some(webview_dir) = webview2_user_data_folder(&managed.config.id, false) {
            // Ensure the folder exists so WebView2 doesn't race to create
            // it against the parent dir's permissions.
            if let Err(e) = std::fs::create_dir_all(&webview_dir) {
                warn!(
                    "Failed to pre-create WebView2 data dir {:?} for runner '{}': {}",
                    webview_dir, managed.config.name, e
                );
            }
            info!(
                "Runner '{}' using isolated WebView2 data dir: {:?}",
                managed.config.name, webview_dir
            );
            cmd.env("WEBVIEW2_USER_DATA_FOLDER", webview_dir);
        }

        let instance_dir = dirs::config_dir().map(|d| {
            d.join("com.qontinui.runner")
                .join("instances")
                .join(&managed.config.id)
        });
        if let Some(ref dir) = instance_dir {
            if let Err(e) = std::fs::create_dir_all(dir) {
                warn!(
                    "Failed to create instance dir {:?} for runner '{}': {}",
                    dir, managed.config.name, e
                );
            }
            cmd.env("QONTINUI_CONFIG_DIR", dir);
            cmd.env("QONTINUI_SECURE_STORAGE_DIR", dir);
        }
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
    /// a blind kill-by-port (kills anything listening regardless of PID),
    /// then re-confirm.
    EscalatePort,
    /// Every escalation was tried and the port is still in use — the stop
    /// must NOT report success.
    StillHeld,
}

/// Decide the next reap action given the current attempt index and whether
/// the port is still in use.
///
/// `attempt` is 0-based and counts escalations already performed:
///   * 0 → after the initial graceful + PID kill, port still held: tree-kill.
///   * 1 → after the tree-kill, port still held: blind kill-by-port.
///   * ≥2 → both escalations exhausted: give up (StillHeld).
///
/// A free port at any attempt short-circuits to `Confirmed`.
fn decide_stop_reap(attempt: u32, port_in_use: bool) -> StopReapOutcome {
    if !port_in_use {
        return StopReapOutcome::Confirmed;
    }
    match attempt {
        0 => StopReapOutcome::EscalateTree,
        1 => StopReapOutcome::EscalatePort,
        _ => StopReapOutcome::StillHeld,
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

                #[cfg(target_os = "windows")]
                {
                    match crate::process::windows::kill_by_pid(pid).await {
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
                }
                #[cfg(not(target_os = "windows"))]
                {
                    warn!(
                        "First-healthy watchdog: kill_by_pid not implemented on non-Windows; \
                         leaving wedged runner '{}' PID {} in place",
                        runner_name, pid
                    );
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
async fn monitor_runner_process_exit(
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

    // Update per-runner state
    {
        let mut runner = managed.runner.write().await;
        runner.running = false;
        runner.process = None;
        runner.pid = None;
    }

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

/// Stop a specific runner by ID. Kills by PID (not by process name).
pub async fn stop_runner_by_id(
    state: &SharedState,
    runner_id: &str,
) -> Result<(), SupervisorError> {
    let managed = state
        .get_runner(runner_id)
        .await
        .ok_or_else(|| SupervisorError::RunnerNotFound(runner_id.to_string()))?;

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
    // we have to work via (a) the graceful HTTP close endpoint, (b) the stored
    // PID, and (c) the `running` flag that the monitor task flips to false
    // when the process exits.
    let pid_to_kill = {
        let runner = managed.runner.read().await;
        runner.pid
    };

    // Orphan-PID recovery: if the registry lost track of the PID (None) but
    // the runner's configured port is in use by some process, that process is
    // the de-facto runner — likely a zombie from a prior supervisor instance
    // that the current supervisor adopted partially. Kill it up-front so the
    // graceful path below has a free port to verify, instead of returning
    // success while the OS process keeps running.
    #[cfg(target_os = "windows")]
    if pid_to_kill.is_none() {
        if let Some(orphan_pid) = crate::process::windows::find_pid_on_port(port).await {
            let msg = format!(
                "Recovered orphan PID {} on port {} for runner '{}'; killing",
                orphan_pid, port, runner_name
            );
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
            let _ = kill_by_pid(orphan_pid).await;
            // Be explicit about the resulting registry state. The PID was
            // already None, but flip running=false so callers that race a
            // health probe see the post-kill view immediately rather than the
            // stale "running=true, pid=None" tuple.
            {
                let mut runner = managed.runner.write().await;
                runner.pid = None;
                runner.running = false;
            }
        }
    }

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

    // 3. Kill by PID. This is a no-op if the process already exited gracefully
    //    (taskkill reports "PID not found" at debug level) and the primary
    //    mechanism otherwise.
    if let Some(pid) = pid_to_kill {
        #[cfg(target_os = "windows")]
        let _ = kill_by_pid(pid).await;
        #[cfg(not(target_os = "windows"))]
        let _ = pid;
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
                    #[cfg(target_os = "windows")]
                    {
                        if let Some(pid) = pid_to_kill {
                            let _ = crate::process::windows::kill_by_pid_tree(pid).await;
                        }
                        // Also catch a survivor whose PID we never tracked
                        // (orphan adopted on a port we knew but no PID for).
                        if let Some(live_pid) =
                            crate::process::windows::find_pid_on_port(port).await
                        {
                            let _ = crate::process::windows::kill_by_pid_tree(live_pid).await;
                        }
                    }
                }
                StopReapOutcome::EscalatePort => {
                    warn!(
                        "Port {} STILL in use for runner '{}' after tree-kill, \
                         escalating to kill-by-port (attempt {})",
                        port, runner_name, attempt
                    );
                    #[cfg(target_os = "windows")]
                    let _ = kill_by_port(port).await;
                }
                StopReapOutcome::StillHeld => {
                    let msg = format!(
                        "Runner '{}' stop could not be confirmed: port {} is still in \
                         use after PID kill, tree-kill, and kill-by-port — refusing to \
                         report success",
                        runner_name, port
                    );
                    warn!("{}", msg);
                    state
                        .logs
                        .emit(LogSource::Supervisor, LogLevel::Warn, msg.clone())
                        .await;
                    // Leave the runner in the registry (not removed, not marked
                    // stopped) so the caller and the dashboard see it as still
                    // present rather than a phantom "stopped" entry holding a port.
                    {
                        let mut runner = managed.runner.write().await;
                        runner.stop_requested = false;
                    }
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

    // 4. Update per-runner state
    {
        let mut runner = managed.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
        runner.stop_requested = false;
    }

    // Update legacy state for primary
    if is_primary {
        let mut runner = state.runner.write().await;
        runner.process = None;
        runner.running = false;
        runner.started_at = None;
        runner.pid = None;
        runner.stop_requested = false;
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

/// Restart a specific runner by ID.
/// Automated sources (watchdog, workflow loop, smart rebuild) are rejected for
/// non-temp runners — only manual API calls can restart user runners.
pub async fn restart_runner_by_id(
    state: &SharedState,
    runner_id: &str,
    rebuild: bool,
    source: RestartSource,
    _force: bool,
    from_working_tree: bool,
) -> Result<(), SupervisorError> {
    if !is_temp_runner(runner_id) && !source.is_manual() {
        let msg = format!(
            "Automated restart of non-temp runner '{}' blocked (source: {}). \
             Only manual restarts are allowed for user runners.",
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

    {
        let mut runner = managed.runner.write().await;
        runner.restart_requested = true;
    }

    // Stop if running
    {
        let runner = managed.runner.read().await;
        if runner.running {
            drop(runner);
            if let Err(e) = stop_runner_by_id(state, runner_id).await {
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
        }
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
        let build_outcome = if build_origin_main {
            primary_rebuild_from_origin_main(state).await
        } else {
            crate::build_monitor::run_cargo_build(state).await
        };
        if let Err(e) = build_outcome {
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
    if let Err(e) = start_managed_runner(state, &managed).await {
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
        runner.restart_requested = false;
    }

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
            if let Err(e) = stop_runner_by_id(state, &managed.config.id).await {
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
pub async fn stop_runner(state: &SharedState, _force: bool) -> Result<(), SupervisorError> {
    let primary = state
        .get_primary()
        .await
        .ok_or_else(|| SupervisorError::Other("No primary runner configured".to_string()))?;

    stop_runner_by_id(state, &primary.config.id).await
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
    match stop_runner_by_id(state, runner_id).await {
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

    /// Every escalation exhausted and still held — stop must NOT be confirmed.
    #[test]
    fn stop_reap_exhausted_is_still_held() {
        assert_eq!(decide_stop_reap(2, true), StopReapOutcome::StillHeld);
        // Any attempt beyond the ladder also reports StillHeld (never loops
        // back to a kill it already tried).
        assert_eq!(decide_stop_reap(3, true), StopReapOutcome::StillHeld);
        assert_eq!(decide_stop_reap(99, true), StopReapOutcome::StillHeld);
    }

    /// The full escalation ladder visits each rung exactly once before
    /// giving up — guards against an infinite reap loop on a wedged survivor.
    #[test]
    fn stop_reap_ladder_terminates() {
        // Simulate a survivor that never releases the port: the loop must
        // walk Tree → Port → StillHeld and stop.
        assert_eq!(decide_stop_reap(0, true), StopReapOutcome::EscalateTree);
        assert_eq!(decide_stop_reap(1, true), StopReapOutcome::EscalatePort);
        assert_eq!(decide_stop_reap(2, true), StopReapOutcome::StillHeld);
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

    /// The legacy-exe warning is the unknown-provenance posture applied to the
    /// pool-less fallback path: names the exe, says UNKNOWN, names the
    /// recovery. Pinning the load-bearing fragments keeps the start-path log
    /// greppable.
    #[test]
    fn legacy_exe_warning_names_exe_and_recovery() {
        let StartProvenanceWarning(msg) =
            legacy_exe_provenance_warning(std::path::Path::new("/ws/target/debug/q.exe"));
        assert!(msg.contains("q.exe"), "must name the exe: {msg}");
        assert!(
            msg.contains("UNKNOWN provenance"),
            "must say unknown: {msg}"
        );
        assert!(
            msg.contains("/runner/fix-and-rebuild"),
            "must name the recovery: {msg}"
        );
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
}
