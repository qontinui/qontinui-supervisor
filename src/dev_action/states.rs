//! Seed dev-state predicate evaluators (supervisor-scoped).
//!
//! Dev-states are *predicates over live observations* — evaluated, never
//! inferred (the paper's `s ∩ E_Ξ ≠ ∅`, §5.1 of the plan). Phase 1 hardcodes
//! the supervisor-scoped seed set here; the shared `qontinui-types` registry is
//! Phase 2. Each evaluator returns [`Eval::True`] / [`Eval::False`] /
//! [`Eval::Unknown`] — a state that *could not* be evaluated (a missing path, a
//! probe that didn't run) is `Unknown`, never silently absent (D4 blind-spot
//! honesty, success criterion 7).
//!
//! # The genuinely-new signal
//!
//! `LEGACY_EXE_FALLBACK` is the signal §2 of the plan calls out as missing: the
//! `Option<usize>` slot id returned by
//! [`crate::process::manager::resolve_source_exe_with_slot`] is `None` exactly
//! when resolution fell back to the legacy `target/debug` exe (preference 3) —
//! the months-old binary with no embedded assets that white-screened the
//! primary on 2026-06-07. This is distinct from `compute_stale_binary`
//! (slot-drift), which is structurally incapable of catching the no-slot case.
//!
//! # Pure cores + an async gatherer
//!
//! Each predicate that touches the filesystem has a pure inner function taking
//! explicit paths/mtimes so it is unit-testable against a `tempfile` tree with
//! no env mutation and no `SharedState`. [`evaluate_all`] gathers the live facts
//! off `SharedState` and calls those cores. The state ids are `&'static str`.

use std::path::Path;
use std::time::SystemTime;

use crate::dev_action::record::{DevStateEval, Eval};
use crate::state::SharedState;

/// No slot exe present in the build pool (`target-pool/slot-*/debug/`) — a
/// fresh checkout or a wiped pool where resolution must fall back to LKG or the
/// legacy exe.
pub const SLOTS_EMPTY: &str = "SLOTS_EMPTY";
/// Resolution fell back to the legacy `target/debug` exe (no build-pool slot).
/// Evaluated from `resolve_source_exe_with_slot` returning a `None` slot id.
pub const LEGACY_EXE_FALLBACK: &str = "LEGACY_EXE_FALLBACK";
/// The LKG exe was built before the newest source mtime — pinning to it would
/// run stale code.
pub const LKG_STALE: &str = "LKG_STALE";
/// `dist/index.html` exists but is older than the newest frontend source — the
/// embedded UI does not reflect current source.
pub const DIST_STALE: &str = "DIST_STALE";
/// `dist/index.html` is absent or empty.
pub const DIST_MISSING: &str = "DIST_MISSING";
/// The primary runner is not responding to its `/health` probe.
pub const PRIMARY_DOWN: &str = "PRIMARY_DOWN";

/// Convert a bool to an [`Eval`] (`True`/`False`). For predicates where a
/// missing input must surface as `Unknown`, the evaluator returns the variant
/// directly instead of using this helper.
fn eval_bool(b: bool) -> Eval {
    if b {
        Eval::True
    } else {
        Eval::False
    }
}

/// `LEGACY_EXE_FALLBACK` from the resolved slot id. `None` ⇒ legacy fallback
/// (the genuinely-new signal). Pure — the caller supplies the slot id from
/// `resolve_source_exe_with_slot`. A resolution *error* (no exe anywhere) is
/// surfaced by the caller as `Unknown`, not by this function.
pub fn legacy_exe_fallback_from_slot(slot_id: Option<usize>) -> Eval {
    eval_bool(slot_id.is_none())
}

/// `SLOTS_EMPTY`: true when none of the given slot exe paths exist on disk.
/// Pure — the caller supplies the per-slot exe paths.
pub fn slots_empty_from_paths(slot_exe_paths: &[std::path::PathBuf]) -> Eval {
    eval_bool(!slot_exe_paths.iter().any(|p| p.exists()))
}

/// `DIST_MISSING`: true when `dist/index.html` is absent or empty. Pure.
pub fn dist_missing_from_path(dist_index: &Path) -> Eval {
    match std::fs::metadata(dist_index) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => Eval::False,
        // Present-but-empty or present-but-a-dir ⇒ missing.
        Ok(_) => Eval::True,
        // Absent ⇒ missing.
        Err(_) => Eval::True,
    }
}

/// `DIST_STALE`: true when `dist/index.html` exists and is OLDER than the
/// newest source mtime. `Unknown` when `dist/index.html` is missing (that is
/// `DIST_MISSING`'s job — staleness is undefined without a dist) or when no
/// source mtime could be determined. Pure — the caller supplies the dist mtime
/// and the newest source mtime.
pub fn dist_stale_from_mtimes(
    dist_index_mtime: Option<SystemTime>,
    newest_src_mtime: Option<SystemTime>,
) -> Eval {
    match (dist_index_mtime, newest_src_mtime) {
        (Some(dist), Some(src)) => eval_bool(dist < src),
        // No dist ⇒ staleness undefined (DIST_MISSING covers absence).
        (None, _) => Eval::Unknown,
        // No source signal ⇒ can't judge.
        (Some(_), None) => Eval::Unknown,
    }
}

/// `LKG_STALE`: true when the LKG `built_at` precedes the newest source mtime.
/// `Unknown` when there is no LKG (nothing to be stale) or no source mtime.
/// Pure — the caller supplies both timestamps.
pub fn lkg_stale_from_times(
    lkg_built_at: Option<chrono::DateTime<chrono::Utc>>,
    newest_src_mtime: Option<SystemTime>,
) -> Eval {
    match (lkg_built_at, newest_src_mtime) {
        (Some(built), Some(src)) => {
            // Convert the source SystemTime to a chrono UTC for comparison.
            match src.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(d) => {
                    let src_utc =
                        chrono::DateTime::<chrono::Utc>::from_timestamp(d.as_secs() as i64, 0);
                    match src_utc {
                        Some(s) => eval_bool(built < s),
                        None => Eval::Unknown,
                    }
                }
                Err(_) => Eval::Unknown,
            }
        }
        // No LKG ⇒ nothing to be stale; not a `True`, and not a definite
        // `False` either (there's just no LKG) — report Unknown so the absence
        // is visible rather than read as "LKG is fresh".
        (None, _) => Eval::Unknown,
        (Some(_), None) => Eval::Unknown,
    }
}

/// Best-effort newest mtime across `index.html` plus the immediate entries of
/// the frontend `src/` dir. A shallow scan keeps this off the action hot path
/// (the deep walk is the Phase-3 ledger's concern); good enough to catch the
/// common "edited a component, never rebuilt dist" drift. Returns `None` when
/// nothing could be stat'd.
fn newest_frontend_src_mtime(npm_dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut consider = |p: &Path| {
        if let Ok(meta) = std::fs::metadata(p) {
            if let Ok(m) = meta.modified() {
                newest = Some(match newest {
                    Some(cur) if cur >= m => cur,
                    _ => m,
                });
            }
        }
    };
    consider(&npm_dir.join("index.html"));
    let src = npm_dir.join("src");
    if let Ok(entries) = std::fs::read_dir(&src) {
        for entry in entries.flatten() {
            consider(&entry.path());
        }
    }
    newest
}

/// Evaluate the full supervisor-scoped seed set at action time.
///
/// Cheap reads of in-memory + on-disk facts — the green path stays
/// non-blocking (success criterion 6). The `slot_id` argument is the
/// `Option<usize>` from `resolve_source_exe_with_slot`; pass `None` if
/// resolution was not run for this action (then `LEGACY_EXE_FALLBACK` is
/// reported `Unknown` rather than falsely `True`).
pub async fn evaluate_all(state: &SharedState, slot_id: SlotResolution) -> Vec<DevStateEval> {
    let npm_dir = state.config.runner_npm_dir();

    // SLOTS_EMPTY — do any build-pool slot exes exist?
    let slot_exe_paths: Vec<std::path::PathBuf> = state
        .build_pool
        .slots
        .iter()
        .map(|s| s.target_dir.join("debug").join("qontinui-runner.exe"))
        .collect();
    let slots_empty = slots_empty_from_paths(&slot_exe_paths);

    // LEGACY_EXE_FALLBACK — None slot id from resolution ⇒ legacy fallback.
    let legacy_fallback = match slot_id {
        SlotResolution::Resolved(id) => legacy_exe_fallback_from_slot(id),
        SlotResolution::NotEvaluated => Eval::Unknown,
    };

    // dist/index.html facts.
    let dist_index = npm_dir.join("dist").join("index.html");
    let dist_missing = dist_missing_from_path(&dist_index);
    let dist_index_mtime = std::fs::metadata(&dist_index)
        .ok()
        .and_then(|m| m.modified().ok());
    let newest_src = newest_frontend_src_mtime(&npm_dir);
    let dist_stale = dist_stale_from_mtimes(dist_index_mtime, newest_src);

    // LKG_STALE — LKG built_at vs newest source.
    let lkg_built_at = state
        .build_pool
        .last_known_good
        .read()
        .await
        .as_ref()
        .map(|l| l.built_at);
    let lkg_stale = lkg_stale_from_times(lkg_built_at, newest_src);

    // PRIMARY_DOWN — read the cached primary health (the background refresher
    // keeps this current; a cheap in-memory read, no probe on the hot path).
    let primary_down = match state.get_primary().await {
        Some(primary) => {
            let cached = primary.cached_health.read().await;
            eval_bool(!cached.runner_responding)
        }
        // No primary registered ⇒ can't judge whether it's down.
        None => Eval::Unknown,
    };

    vec![
        DevStateEval {
            id: SLOTS_EMPTY,
            value: slots_empty,
        },
        DevStateEval {
            id: LEGACY_EXE_FALLBACK,
            value: legacy_fallback,
        },
        DevStateEval {
            id: LKG_STALE,
            value: lkg_stale,
        },
        DevStateEval {
            id: DIST_STALE,
            value: dist_stale,
        },
        DevStateEval {
            id: DIST_MISSING,
            value: dist_missing,
        },
        DevStateEval {
            id: PRIMARY_DOWN,
            value: primary_down,
        },
    ]
}

/// Whether slot resolution was run for this action, and if so its result.
/// Restart/build evaluate states *before* invoking resolution (the facts are in
/// memory at resolve time), so they pass [`SlotResolution::NotEvaluated`] when
/// they evaluate states pre-resolution — but Phase 1 resolves the slot id
/// cheaply itself (see the route hooks) so `LEGACY_EXE_FALLBACK` is a real
/// signal, not `Unknown`, for the motivating restart case.
#[derive(Debug, Clone, Copy)]
pub enum SlotResolution {
    /// Resolution ran; carries the `Option<usize>` slot id (`None` = legacy).
    Resolved(Option<usize>),
    /// Resolution was not run for this action.
    NotEvaluated,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn legacy_fallback_true_when_slot_none_false_otherwise() {
        // `None` slot id ⇒ legacy fallback — the genuinely-new signal.
        assert_eq!(legacy_exe_fallback_from_slot(None), Eval::True);
        // A real slot id ⇒ not a fallback.
        assert_eq!(legacy_exe_fallback_from_slot(Some(0)), Eval::False);
        assert_eq!(legacy_exe_fallback_from_slot(Some(2)), Eval::False);
    }

    #[test]
    fn slots_empty_true_when_no_exe_exists() {
        let dir = tempfile::tempdir().unwrap();
        let s0 = dir.path().join("slot-0").join("debug").join("runner.exe");
        let s1 = dir.path().join("slot-1").join("debug").join("runner.exe");
        // Neither exists yet.
        assert_eq!(
            slots_empty_from_paths(&[s0.clone(), s1.clone()]),
            Eval::True
        );
        // Create one ⇒ not empty.
        fs::create_dir_all(s0.parent().unwrap()).unwrap();
        fs::write(&s0, b"exe").unwrap();
        assert_eq!(slots_empty_from_paths(&[s0, s1]), Eval::False);
    }

    #[test]
    fn dist_missing_against_tempfile_tree() {
        let dir = tempfile::tempdir().unwrap();
        let dist_index = dir.path().join("dist").join("index.html");
        // Absent ⇒ missing.
        assert_eq!(dist_missing_from_path(&dist_index), Eval::True);
        // Present-but-empty ⇒ missing.
        fs::create_dir_all(dist_index.parent().unwrap()).unwrap();
        fs::write(&dist_index, b"").unwrap();
        assert_eq!(dist_missing_from_path(&dist_index), Eval::True);
        // Present-and-nonempty ⇒ not missing.
        fs::write(&dist_index, b"<html/>").unwrap();
        assert_eq!(dist_missing_from_path(&dist_index), Eval::False);
    }

    #[test]
    fn dist_stale_compares_mtimes_and_reports_unknown_without_dist() {
        let older = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        // dist older than src ⇒ stale.
        assert_eq!(dist_stale_from_mtimes(Some(older), Some(newer)), Eval::True);
        // dist newer than src ⇒ fresh.
        assert_eq!(
            dist_stale_from_mtimes(Some(newer), Some(older)),
            Eval::False
        );
        // No dist ⇒ Unknown (DIST_MISSING owns absence).
        assert_eq!(dist_stale_from_mtimes(None, Some(newer)), Eval::Unknown);
        // No source signal ⇒ Unknown.
        assert_eq!(dist_stale_from_mtimes(Some(older), None), Eval::Unknown);
    }

    #[test]
    fn lkg_stale_compares_times_and_reports_unknown_without_lkg() {
        let src = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        let built_before = chrono::DateTime::<chrono::Utc>::from_timestamp(1_000_000, 0).unwrap();
        let built_after = chrono::DateTime::<chrono::Utc>::from_timestamp(3_000_000, 0).unwrap();
        // LKG built before source ⇒ stale.
        assert_eq!(
            lkg_stale_from_times(Some(built_before), Some(src)),
            Eval::True
        );
        // LKG built after source ⇒ fresh.
        assert_eq!(
            lkg_stale_from_times(Some(built_after), Some(src)),
            Eval::False
        );
        // No LKG ⇒ Unknown (absence is visible, not "fresh").
        assert_eq!(lkg_stale_from_times(None, Some(src)), Eval::Unknown);
    }
}
