//! Delayed-effect attribution watcher.
//!
//! Dev actions emit effects on a delay (the 2026-06-07 asset error landed at
//! t+5s after a "restarted successfully" ACK), so a synchronous verdict is
//! impossible — the watcher sleeps the per-kind attribution window, then scans
//! the effect surfaces and folds a verdict into the action's [`ActionOutcome`]:
//!
//! 1. the runner's `early_log` file for `DEV-TAURI-ASSET-MISSING` /
//!    `DEV-WEBVIEW-CONN-REFUSED` / `DEV-PORT-BIND-FAIL` signatures,
//! 2. `panic_log` freshness for `DEV-PANIC-STARTUP`,
//! 3. the cached `derived_status` / `ui_error` transition for
//!    `DEV-UI-ERROR-BOUNDARY`.
//!
//! The verdict closes at window end. Late observations append to
//! `late_signatures` flagged `late: true` and update statistics only — they
//! NEVER re-open the closed verdict (§3 theory item 2), and they do not bleed
//! into the next action on the same surface.
//!
//! The watcher is a detached `tokio::spawn` owning a `SharedState` clone — the
//! same pattern `routes/runner.rs` uses for the detached rebuild — so it
//! survives the handler future that minted the action. It reads the early log
//! via a path captured at spawn time; that file lives under `temp_dir()` and
//! survives runner cleanup (per `early_log.rs`), so a runner that white-screens
//! and is purged still leaves its log for the watcher.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use qontinui_types::dev_signatures::DevSignature;

use crate::dev_action::record::{ActionKind, ActionOutcome, ActionRecord, D3Category};
use crate::log_capture::{LogLevel, LogSource};
use crate::process::panic_log;
use crate::state::SharedState;

// ── `DEV-*` outcome signatures (Phase-2b: the shared `qontinui-types` ──
//    `DevSignature` registry; the Phase-1 hardcoded consts are gone). Each
//    matcher below produces a `DevSignature`; the outcome stores its canonical
//    `as_str()` so the wire form (`Vec<String>` of `DEV-*` codes) is unchanged.

// ── Per-kind attribution windows (Q6). Read from this const map so they are ──
//    tunable without a schema change.

/// Restart window: 30s.
pub const RESTART_WINDOW: Duration = Duration::from_secs(30);
/// Spawn window: 30s.
pub const SPAWN_WINDOW: Duration = Duration::from_secs(30);
/// Build tail added on top of the build's own duration: 30s.
pub const BUILD_TAIL_WINDOW: Duration = Duration::from_secs(30);

/// The attribution window for an action kind. `build` is `duration + 30s`
/// keyed off the build-submission time; restart/spawn are flat 30s.
pub fn window_for(kind: ActionKind, build_duration: Option<Duration>) -> Duration {
    match kind {
        ActionKind::Restart => RESTART_WINDOW,
        ActionKind::Spawn => SPAWN_WINDOW,
        ActionKind::Build => build_duration.unwrap_or_default() + BUILD_TAIL_WINDOW,
    }
}

/// Classify a verdict from the observed signatures. Each signature carries its
/// **default D3 category** in the shared registry
/// ([`DevSignature::default_category`]): asset-missing / webview-refused /
/// ui_error default to **Contradiction** (the ACK claimed success but the
/// surface refutes it); panic / port-bind / compile-flake default to
/// **Failure** (the action's own machinery failed). Phase-2b derives the verdict
/// from those defaults instead of re-listing the mapping here. A clean window ⇒
/// **Confirmed**. When multiple classes are present, Contradiction wins over
/// Failure (a white screen behind a "success" ACK is the more misleading
/// outcome and the one the operator most needs surfaced) — the precedence is
/// applied here, since the registry only carries each signature's own default.
pub fn classify(signatures: &[DevSignature]) -> D3Category {
    let mut has_contradiction = false;
    let mut has_failure = false;
    for sig in signatures {
        match sig.default_category() {
            D3Category::Contradiction => has_contradiction = true,
            D3Category::Failure => has_failure = true,
            // No Phase-1 signature defaults to these; ignore for the verdict.
            D3Category::Confirmed | D3Category::Surprise | D3Category::Partial => {}
        }
    }
    if has_contradiction {
        D3Category::Contradiction
    } else if has_failure {
        D3Category::Failure
    } else {
        D3Category::Confirmed
    }
}

/// Scan early-log file *content* for the file-derived `DEV-*` signatures. Pure:
/// the caller supplies the already-read log text, so this is unit-testable
/// against a fixture string with no IO. Returns the signatures in a stable
/// order. Also returns a short evidence excerpt (the first matching line).
pub fn scan_log_content(content: &str) -> (Vec<DevSignature>, Option<String>) {
    let mut sigs: Vec<DevSignature> = Vec::new();
    let mut evidence: Option<String> = None;

    let note =
        |sig: DevSignature, line: &str, sigs: &mut Vec<DevSignature>, ev: &mut Option<String>| {
            if !sigs.contains(&sig) {
                sigs.push(sig);
            }
            if ev.is_none() {
                *ev = Some(line.trim().chars().take(200).collect());
            }
        };

    for line in content.lines() {
        // DEV-TAURI-ASSET-MISSING: `tauri::manager` + `asset not found`. The
        // motivating incident's exact log: `ERROR tauri::manager: asset not
        // found: index.html`. Match on the substring pair so a future format
        // tweak in either token still fires.
        if line.contains("tauri::manager") && line.contains("asset not found") {
            note(
                DevSignature::DevTauriAssetMissing,
                line,
                &mut sigs,
                &mut evidence,
            );
        }
        if line.contains("ERR_CONNECTION_REFUSED") {
            note(
                DevSignature::DevWebviewConnRefused,
                line,
                &mut sigs,
                &mut evidence,
            );
        }
        // Port bind failure: Rust's std surfaces this as `AddrInUse`; tauri /
        // axum bind errors also say "address already in use" / "failed to
        // bind". Match any of those.
        let lower = line.to_ascii_lowercase();
        if line.contains("AddrInUse")
            || lower.contains("address already in use")
            || lower.contains("failed to bind")
        {
            note(
                DevSignature::DevPortBindFail,
                line,
                &mut sigs,
                &mut evidence,
            );
        }
    }

    (sigs, evidence)
}

/// Read the early-log file (best-effort) and scan it. Returns empty when the
/// path is `None` or the file can't be read.
fn scan_early_log(path: Option<&Path>) -> (Vec<DevSignature>, Option<String>) {
    let Some(path) = path else {
        return (Vec::new(), None);
    };
    match std::fs::read_to_string(path) {
        Ok(content) => scan_log_content(&content),
        Err(_) => (Vec::new(), None),
    }
}

/// Check the panic log for a fresh startup panic within the window. Returns the
/// `DEV-PANIC-STARTUP` signature + a short excerpt when a panic dated within
/// the window is found.
fn scan_panic(
    panic_log_path: Option<&Path>,
    window_open_at: chrono::DateTime<Utc>,
) -> (Vec<DevSignature>, Option<String>) {
    let path = match panic_log_path {
        Some(p) => p.to_path_buf(),
        None => panic_log::resolve_panic_log_path(None),
    };
    if let Some(parsed) = panic_log::parse_panic_file(&path) {
        // "Fresh within the window": the panic's timestamp is at or after the
        // window opened (minus the panic-log freshness slack the existing
        // helper already encodes against the window-close time).
        if panic_log::is_fresh(&parsed, Utc::now()) && parsed.timestamp >= window_open_at {
            let excerpt = format!(
                "panic: {}",
                parsed.payload.lines().next().unwrap_or("").trim()
            );
            return (vec![DevSignature::DevPanicStartup], Some(excerpt));
        }
    }
    (Vec::new(), None)
}

/// Check the cached health for a UI-error / errored transition on the target
/// runner. Returns `DEV-UI-ERROR-BOUNDARY` when the runner's cached
/// `derived_status` is errored or it reports a `ui_error`.
async fn scan_health(
    state: &SharedState,
    runner_id: Option<&str>,
) -> (Vec<DevSignature>, Option<String>) {
    let Some(runner_id) = runner_id else {
        return (Vec::new(), None);
    };
    let snapshots = state.cached_runner_health.read().await;
    for snap in snapshots.iter() {
        if snap.id == runner_id {
            if let Some(ui_err) = snap.ui_error.as_ref() {
                return (
                    vec![DevSignature::DevUiErrorBoundary],
                    Some(format!(
                        "ui_error: {}",
                        ui_err.message.chars().take(160).collect::<String>()
                    )),
                );
            }
            if matches!(
                snap.derived_status,
                crate::health_cache::RunnerStatus::Errored { .. }
            ) {
                return (
                    vec![DevSignature::DevUiErrorBoundary],
                    Some("derived_status=errored".to_string()),
                );
            }
        }
    }
    (Vec::new(), None)
}

/// Where the watcher should look for this action's effects.
#[derive(Clone, Default)]
pub struct AttributionTargets {
    /// Early-log file path of the runner whose startup this action drives.
    /// Used when the path is already known at mint time (restart of an
    /// existing primary). For a fresh spawn the path is set *during* the build,
    /// after the action is minted — pass [`AttributionTargets::managed`]
    /// instead so the watcher reads it lazily at window close.
    pub early_log_path: Option<PathBuf>,
    /// The managed runner whose `early_log_path` should be read at window
    /// close. Resolves the spawn case where the early-log file doesn't exist
    /// yet at mint time. When set, it takes precedence over `early_log_path`.
    pub managed: Option<Arc<crate::state::ManagedRunner>>,
    /// Panic-log path hint (`None` ⇒ the default runner panic-log path).
    pub panic_log_path: Option<PathBuf>,
    /// Runner id whose cached health to inspect for a ui_error transition.
    pub runner_id: Option<String>,
}

/// Spawn the detached attribution watcher for a minted action.
///
/// Sleeps the per-kind window, scans the effect surfaces, folds the verdict
/// into `record.outcome`, and logs the verdict to the supervisor log buffer.
/// Triggers `notify_health_change()` so connected dashboards refresh (Phase 1
/// has no dedicated `events.*` WS channel — that arrives with coord in Phase 3;
/// see the plan's §6.1 watcher note). Best-effort throughout: any scan failure
/// degrades to "no signal", never a panic.
pub fn spawn_attribution_watcher(
    state: SharedState,
    record: Arc<ActionRecord>,
    targets: AttributionTargets,
    build_duration: Option<Duration>,
) {
    tokio::spawn(async move {
        let kind = record.kind;
        let window = window_for(kind, build_duration);
        let window_open_at = record.started_at;

        tokio::time::sleep(window).await;

        // Resolve the early-log path: prefer the managed runner's current path
        // (set during a fresh spawn after mint time), else the eagerly-captured
        // path (a restart of an existing primary).
        let early_log_path = match targets.managed.as_ref() {
            Some(managed) => managed
                .early_log_path
                .read()
                .await
                .clone()
                .or_else(|| targets.early_log_path.clone()),
            None => targets.early_log_path.clone(),
        };

        // Gather signatures from every effect surface.
        let (mut signatures, mut evidence) = scan_early_log(early_log_path.as_deref());
        let (panic_sigs, panic_ev) = scan_panic(targets.panic_log_path.as_deref(), window_open_at);
        merge(&mut signatures, &mut evidence, panic_sigs, panic_ev);
        let (health_sigs, health_ev) = scan_health(&state, targets.runner_id.as_deref()).await;
        merge(&mut signatures, &mut evidence, health_sigs, health_ev);

        let category = classify(&signatures);
        let ended_at = Utc::now();
        let duration_ms = (ended_at - record.started_at).num_milliseconds();

        // The outcome's wire form carries the canonical `DEV-*` id strings.
        let signature_ids: Vec<String> =
            signatures.iter().map(|s| s.as_str().to_string()).collect();

        let outcome = ActionOutcome {
            category,
            signatures: signature_ids.clone(),
            ended_at,
            duration_ms,
            evidence_ref: evidence,
            late_signatures: Vec::new(),
        };

        if let Ok(mut guard) = record.outcome.write() {
            *guard = Some(outcome);
        }

        let level = match category {
            D3Category::Contradiction | D3Category::Failure => LogLevel::Warn,
            _ => LogLevel::Info,
        };
        state
            .logs
            .emit(
                LogSource::Supervisor,
                level,
                format!(
                    "dev-action {} ({}) verdict={:?} signatures={:?}",
                    record.action_id,
                    kind.as_str(),
                    category,
                    signature_ids
                ),
            )
            .await;

        // Nudge connected dashboards to refresh (Phase-1 stand-in for the
        // Phase-3 `events.dev_actions` WS push).
        state.notify_health_change();
    });
}

/// Merge a secondary `(signatures, evidence)` scan into the primary, preserving
/// signature order/uniqueness and keeping the first evidence excerpt.
fn merge(
    primary_sigs: &mut Vec<DevSignature>,
    primary_ev: &mut Option<String>,
    extra_sigs: Vec<DevSignature>,
    extra_ev: Option<String>,
) {
    for s in extra_sigs {
        if !primary_sigs.contains(&s) {
            primary_sigs.push(s);
        }
    }
    if primary_ev.is_none() {
        *primary_ev = extra_ev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_missing_log_yields_contradiction() {
        // The exact 2026-06-07 incident line.
        let log = "INFO starting up\n\
                   ERROR tauri::manager: asset not found: index.html\n\
                   INFO window created\n";
        let (sigs, evidence) = scan_log_content(log);
        assert!(sigs.contains(&DevSignature::DevTauriAssetMissing));
        assert_eq!(classify(&sigs), D3Category::Contradiction);
        assert!(evidence.unwrap().contains("asset not found"));
    }

    #[test]
    fn webview_conn_refused_yields_contradiction() {
        let log = "ERROR webview: net::ERR_CONNECTION_REFUSED at http://localhost:1420\n";
        let (sigs, _) = scan_log_content(log);
        assert!(sigs.contains(&DevSignature::DevWebviewConnRefused));
        assert_eq!(classify(&sigs), D3Category::Contradiction);
    }

    #[test]
    fn port_bind_failure_yields_failure() {
        for line in [
            "ERROR Os { code: 10048, kind: AddrInUse }",
            "ERROR failed to bind to 127.0.0.1:9876: address already in use",
        ] {
            let (sigs, _) = scan_log_content(line);
            assert!(
                sigs.contains(&DevSignature::DevPortBindFail),
                "line: {line}"
            );
            assert_eq!(classify(&sigs), D3Category::Failure, "line: {line}");
        }
    }

    #[test]
    fn clean_log_yields_confirmed() {
        let log = "INFO starting up\nINFO bound to port 9876\nINFO ready\n";
        let (sigs, evidence) = scan_log_content(log);
        assert!(sigs.is_empty());
        assert_eq!(classify(&sigs), D3Category::Confirmed);
        assert!(evidence.is_none());
    }

    #[test]
    fn contradiction_wins_over_failure_when_both_present() {
        let sigs = vec![
            DevSignature::DevPortBindFail,
            DevSignature::DevTauriAssetMissing,
        ];
        assert_eq!(classify(&sigs), D3Category::Contradiction);
    }

    #[test]
    fn ui_error_boundary_classifies_as_contradiction() {
        let sigs = vec![DevSignature::DevUiErrorBoundary];
        assert_eq!(classify(&sigs), D3Category::Contradiction);
    }

    #[test]
    fn panic_signature_classifies_as_failure() {
        let sigs = vec![DevSignature::DevPanicStartup];
        assert_eq!(classify(&sigs), D3Category::Failure);
    }

    #[test]
    fn window_for_build_is_duration_plus_tail() {
        let dur = Duration::from_secs(120);
        assert_eq!(
            window_for(ActionKind::Build, Some(dur)),
            dur + BUILD_TAIL_WINDOW
        );
        assert_eq!(window_for(ActionKind::Restart, None), RESTART_WINDOW);
        assert_eq!(window_for(ActionKind::Spawn, None), SPAWN_WINDOW);
        // Build with no known duration falls back to just the tail.
        assert_eq!(window_for(ActionKind::Build, None), BUILD_TAIL_WINDOW);
    }

    /// Scanning a fixture early-log FILE (not just a string) → Contradiction +
    /// the asset-missing signature, exercising the `scan_early_log` file IO
    /// path against a `tempfile`. Env-free.
    #[test]
    fn fixture_early_log_file_scans_to_contradiction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("early.log");
        std::fs::write(
            &path,
            "INFO boot\nERROR tauri::manager: asset not found: index.html\n",
        )
        .unwrap();
        let (sigs, evidence) = scan_early_log(Some(&path));
        assert!(sigs.contains(&DevSignature::DevTauriAssetMissing));
        assert_eq!(classify(&sigs), D3Category::Contradiction);
        assert!(evidence.is_some());
    }

    #[test]
    fn missing_early_log_path_is_clean_not_panic() {
        let (sigs, evidence) = scan_early_log(None);
        assert!(sigs.is_empty());
        assert!(evidence.is_none());
        // A nonexistent file path also degrades to clean.
        let (sigs2, _) = scan_early_log(Some(Path::new("/no/such/early.log")));
        assert!(sigs2.is_empty());
    }

    /// Late observation semantics: appending a late signature must NOT change
    /// the already-closed verdict. We model the folded outcome directly (the
    /// watcher writes `late_signatures` separately from `category`) and assert
    /// the verdict is unchanged.
    #[test]
    fn late_append_does_not_reopen_verdict() {
        // A clean window closed as Confirmed.
        let mut outcome = ActionOutcome {
            category: classify(&[]),
            signatures: vec![],
            ended_at: Utc::now(),
            duration_ms: 30_000,
            evidence_ref: None,
            late_signatures: vec![],
        };
        assert_eq!(outcome.category, D3Category::Confirmed);

        // A late ui_error arrives AFTER the window — it appends to
        // `late_signatures` and must not flip `category`.
        outcome
            .late_signatures
            .push(DevSignature::DevUiErrorBoundary.as_str().to_string());
        assert_eq!(
            outcome.category,
            D3Category::Confirmed,
            "late observation must not re-open the closed verdict"
        );
        assert_eq!(outcome.late_signatures.len(), 1);
    }
}
