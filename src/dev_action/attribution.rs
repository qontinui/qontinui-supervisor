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

use crate::dev_action::record::{
    ActionKind, ActionOutcome, ActionRecord, ActionRunResult, D3Category,
};
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
/// Hard upper bound on a `build`-kind attribution window when the build's real
/// duration is not known up front (which is EVERY caller today — both
/// `mint_primary_action` sites pass `build_duration: None`).
///
/// Before this existed, `window_for(Build, None)` resolved to a flat
/// [`BUILD_TAIL_WINDOW`] and every rebuild-restart's verdict was stamped 30
/// seconds after submission — typically 10-20 minutes before the build it was
/// judging had finished
/// (`plans/2026-09-04-supervisor-refused-restart-reports-confirmed.md` §1-§2).
/// The window now stays open until the action records its own terminal result
/// ([`ActionRunResult`]); this is the backstop for the case where no path ever
/// records one, so a watcher can never park forever. Sized well above the
/// observed 10-20 minute runner rebuild.
pub const BUILD_BACKSTOP_WINDOW: Duration = Duration::from_secs(45 * 60);

/// The two timing knobs of a build attribution window, injectable so the
/// regression tests can exercise the real waiting logic in milliseconds rather
/// than minutes. Production always uses [`WindowTiming::default`].
#[derive(Debug, Clone, Copy)]
pub struct WindowTiming {
    /// Grace period added AFTER the action's terminal result, during which
    /// delayed effects (the 2026-06-07 asset error landed at t+5s) still count.
    pub tail: Duration,
    /// Upper bound on waiting for a terminal result that never arrives.
    pub backstop: Duration,
}

impl Default for WindowTiming {
    fn default() -> Self {
        Self {
            tail: BUILD_TAIL_WINDOW,
            backstop: BUILD_BACKSTOP_WINDOW,
        }
    }
}

/// The attribution window for an action kind. `build` is `duration + 30s`
/// keyed off the build-submission time; restart/spawn are flat 30s.
///
/// **`build_duration` is `None` at every live call site**, which is what made
/// this a flat 30s window for builds. [`await_attribution_window`] is what the
/// watcher actually waits on now: it holds a build's window open until the
/// action's own terminal result lands. This function stays as the declared
/// per-kind window (and the flat window restart/spawn still use verbatim).
pub fn window_for(kind: ActionKind, build_duration: Option<Duration>) -> Duration {
    match kind {
        ActionKind::Restart => RESTART_WINDOW,
        ActionKind::Spawn => SPAWN_WINDOW,
        ActionKind::Build => build_duration.unwrap_or_default() + BUILD_TAIL_WINDOW,
    }
}

/// Classify a verdict from the action's own terminal result and the observed
/// signatures.
///
/// `run` comes FIRST. If the action reports that it was refused or errored, the
/// verdict is [`D3Category::Failure`] regardless of how clean the effect
/// surfaces look — a refused restart never touched them, so their cleanliness
/// is the absence of an observation, not evidence of success
/// (`verification-and-evidence` `silent-empty-is-unknown`). This is the fix for
/// the doc comment below having quietly assumed its own premise: "a clean
/// window ⇒ Confirmed" only holds for a window that describes an action which
/// actually ran.
///
/// **`Failure` is deliberately reused rather than a sixth `D3Category` variant
/// added.** The enum lives in `qontinui-schemas` and its five wire values are
/// pinned by a hard Postgres CHECK; a sixth value would fail coord's ingest
/// INSERT, and the supervisor's fail-open ingest would swallow that into one
/// `warn!` — making refusal snapshots the only rows silently missing from the
/// durable ledger. See the plan's §5. The refusal/error distinction rides
/// [`ActionRecord::run_result`] and [`ActionOutcome::evidence_ref`] instead.
///
/// With `run` absent or [`ActionRunResult::Ran`], the pre-existing precedence
/// applies unchanged. Each signature carries its **default D3 category** in the
/// shared registry ([`DevSignature::default_category`]): asset-missing /
/// webview-refused / ui_error default to **Contradiction** (the ACK claimed
/// success but the surface refutes it); panic / port-bind / compile-flake
/// default to **Failure** (the action's own machinery failed). A clean window ⇒
/// **Confirmed**. When multiple classes are present, Contradiction wins over
/// Failure (a white screen behind a "success" ACK is the more misleading
/// outcome and the one the operator most needs surfaced) — the precedence is
/// applied here, since the registry only carries each signature's own default.
pub fn classify(run: Option<&ActionRunResult>, signatures: &[DevSignature]) -> D3Category {
    if run.is_some_and(ActionRunResult::is_failure) {
        return D3Category::Failure;
    }
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

/// Wait for a terminal [`ActionRunResult`] on `record`, or until `cap` elapses.
///
/// Returns the result if one landed inside the budget, else `None`. The
/// `Notify` permit is stored by [`ActionRecord::record_run_result`], so a
/// result recorded *before* this is first awaited still completes immediately —
/// the read-before-and-after-arming pair below is belt to that brace.
async fn wait_for_run_result(record: &ActionRecord, cap: Duration) -> Option<ActionRunResult> {
    let deadline = tokio::time::Instant::now() + cap;
    loop {
        if let Some(r) = record.run_result() {
            return Some(r);
        }
        let notified = record.run_signal.notified();
        if let Some(r) = record.run_result() {
            return Some(r);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            // Budget exhausted. Re-read once: the result may have landed in the
            // same tick the timeout fired.
            return record.run_result();
        }
    }
}

/// Wait out the attribution window, and report the action's terminal result if
/// one landed inside it.
///
/// **This is the framing fix** (plan §6 Phase 2). `window_for(Build, None)` —
/// a flat 30s — used to be the window for every rebuild-restart, so a build
/// that fails at t+12min closed `Confirmed` at t+30s and the ledger's
/// never-re-open rule made that permanent. Now:
///
/// - **`build`**: hold the window open until the action records its own
///   terminal result, then add [`WindowTiming::tail`] so delayed effects still
///   count. Capped at `build_duration + tail` when the real duration IS known,
///   else at [`WindowTiming::backstop`] — so a path that never records a result
///   still closes, exactly as it did before, instead of parking forever.
/// - **`restart` / `spawn`**: the flat per-kind window, unchanged.
///
/// **[`ActionRunResult::Refused`] closes the window immediately, for every
/// kind.** A refused action never ran, so no effect surface was touched and
/// there is nothing left for the window to observe; waiting out the remainder
/// would only delay a verdict that is already known.
///
/// The watcher only ever READS the result, so the never-re-open invariant
/// (module docs; the ledger plan §3.2) is untouched: the verdict is still
/// composed exactly once, at window close.
async fn await_attribution_window(
    record: &ActionRecord,
    kind: ActionKind,
    build_duration: Option<Duration>,
    timing: WindowTiming,
) -> Option<ActionRunResult> {
    match kind {
        ActionKind::Restart | ActionKind::Spawn => {
            let flat = window_for(kind, build_duration);
            match wait_for_run_result(record, flat).await {
                // Nothing ran — close now.
                Some(r @ ActionRunResult::Refused { .. }) => Some(r),
                // Ran (well or badly): the effect surfaces still need the rest
                // of the flat window to produce their signatures.
                Some(other) => {
                    tokio::time::sleep(remaining(record, flat)).await;
                    Some(other)
                }
                None => record.run_result(),
            }
        }
        ActionKind::Build => {
            let cap = build_duration
                .map(|d| d + timing.tail)
                .unwrap_or(timing.backstop);
            match wait_for_run_result(record, cap).await {
                Some(r @ ActionRunResult::Refused { .. }) => Some(r),
                Some(other) => {
                    // Tail measured from the action's REAL end, and still
                    // clamped by the cap so the window is bounded either way.
                    let elapsed = elapsed_since_start(record);
                    let room = cap.saturating_sub(elapsed);
                    tokio::time::sleep(timing.tail.min(room)).await;
                    Some(other)
                }
                None => record.run_result(),
            }
        }
    }
}

/// How much of `window` is left, measured from the record's `started_at`.
fn remaining(record: &ActionRecord, window: Duration) -> Duration {
    window.saturating_sub(elapsed_since_start(record))
}

/// Wall-clock elapsed since the action was minted. A clock that has gone
/// backwards reads as zero elapsed rather than as a negative (which would
/// otherwise wrap into an enormous sleep).
fn elapsed_since_start(record: &ActionRecord) -> Duration {
    (Utc::now() - record.started_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
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
    spawn_attribution_watcher_with_timing(
        state,
        record,
        targets,
        build_duration,
        WindowTiming::default(),
    );
}

/// [`spawn_attribution_watcher`] with the window timing supplied explicitly.
/// Production uses the `Default`; the regression tests use millisecond values
/// so they exercise the real waiting logic without a 30-second sleep.
pub fn spawn_attribution_watcher_with_timing(
    state: SharedState,
    record: Arc<ActionRecord>,
    targets: AttributionTargets,
    build_duration: Option<Duration>,
    timing: WindowTiming,
) {
    tokio::spawn(async move {
        let kind = record.kind;
        let window_open_at = record.started_at;

        let run_result = await_attribution_window(&record, kind, build_duration, timing).await;

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

        let category = classify(run_result.as_ref(), &signatures);
        let ended_at = Utc::now();
        let duration_ms = (ended_at - record.started_at).num_milliseconds();

        // Plan §5: the refusal/error distinction rides `evidence_ref`, not a
        // sixth `D3Category`. A `DEV-*` signature's own excerpt wins when there
        // is one (it names an observed effect); otherwise the action's terminal
        // reason is the only evidence there is — and for a refusal it is the
        // ONLY evidence there ever could be, since nothing ran.
        if evidence.is_none() {
            evidence = run_result
                .as_ref()
                .and_then(ActionRunResult::reason)
                .map(|r| r.chars().take(400).collect::<String>());
        }

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
            *guard = Some(outcome.clone());
        }

        // Phase 3: persist the completed snapshot to coord for durable storage.
        // Best-effort + fail-open — `post_snapshot_to_coord` swallows every
        // error into a single `warn!`, so this can never block or fail the
        // watcher. Detached via `tokio::spawn` so even the 5s ingest timeout
        // can't delay the dashboard nudge below. device_id / tenant_id come from
        // `~/.qontinui/machine.json` (the same `machine_id` field `fleet.rs`
        // reads for the device id).
        let device_id = crate::dev_action::ingest::resolve_device_id();
        let tenant_id = crate::dev_action::ingest::resolve_tenant_id();
        let ingest_record = Arc::clone(&record);
        tokio::spawn(async move {
            crate::dev_action::post_snapshot_to_coord(
                &ingest_record,
                &outcome,
                device_id,
                tenant_id,
            )
            .await;
        });

        let level = match category {
            D3Category::Contradiction | D3Category::Failure => LogLevel::Warn,
            _ => LogLevel::Info,
        };
        // Name the action's terminal result on the operator log line. Without
        // it a refusal reads as `verdict=Failure signatures=[]` — the same
        // shape as an unexplained one — and the whole point of this plan is
        // that the gate's verdict REACHES the caller.
        let run_note = match run_result.as_ref() {
            Some(r) => match r.reason() {
                Some(reason) => format!(
                    " run={} reason={:?}",
                    run_kind(r),
                    reason.chars().take(200).collect::<String>()
                ),
                None => format!(" run={}", run_kind(r)),
            },
            None => String::new(),
        };
        state
            .logs
            .emit(
                LogSource::Supervisor,
                level,
                format!(
                    "dev-action {} ({}) verdict={:?} signatures={:?}{}",
                    record.action_id,
                    kind.as_str(),
                    category,
                    signature_ids,
                    run_note
                ),
            )
            .await;

        // Nudge connected dashboards to refresh (Phase-1 stand-in for the
        // Phase-3 `events.dev_actions` WS push).
        state.notify_health_change();
    });
}

/// Stable log token for an [`ActionRunResult`] variant.
fn run_kind(r: &ActionRunResult) -> &'static str {
    match r {
        ActionRunResult::Refused { .. } => "refused",
        ActionRunResult::Errored { .. } => "errored",
        ActionRunResult::Ran => "ran",
    }
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

    // ── Regression tests for
    //    `2026-09-04-supervisor-refused-restart-reports-confirmed` ──────────

    /// Same minimal `SupervisorState` shape the route tests use.
    fn watcher_test_state(root: &std::path::Path) -> SharedState {
        use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
        use crate::state::SupervisorState;
        Arc::new(SupervisorState::new(SupervisorConfig {
            project_dir: root.join("src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: 9875,
            dev_logs_dir: root.join(".dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 8081,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: true,
            no_webview: true,
        }))
    }

    fn bare_record(kind: ActionKind) -> Arc<ActionRecord> {
        Arc::new(ActionRecord::new(kind, None, "test".to_string(), &[]))
    }

    /// Poll a record's outcome until it is folded in, or give up.
    async fn await_outcome(record: &ActionRecord, budget: Duration) -> ActionOutcome {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(o) = record.outcome.read().ok().and_then(|g| g.clone()) {
                return o;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the attribution watcher never folded an outcome in"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Plan §6 Phase 5 test 1. THE bug: a readiness refusal leaves every effect
    /// surface clean, and a clean window used to classify `Confirmed`. The
    /// action's own terminal result now overrides that.
    #[test]
    fn refused_run_result_classifies_as_failure_not_confirmed() {
        let refused = ActionRunResult::Refused {
            reason: "restart_refused_unsafe: 1 terminal-hosted agent session is live".to_string(),
        };
        assert_eq!(
            classify(Some(&refused), &[]),
            D3Category::Failure,
            "a refused action with a clean window must NOT be Confirmed"
        );
        // An errored action is the same class — and it is the expensive one:
        // a build that fails at t+12min.
        let errored = ActionRunResult::Errored {
            reason: "cargo build failed".to_string(),
        };
        assert_eq!(classify(Some(&errored), &[]), D3Category::Failure);
        // §5: `Failure` is REUSED. A sixth wire value would violate coord's
        // hard Postgres CHECK and the fail-open ingest would swallow it.
        assert_eq!(
            serde_json::to_string(&classify(Some(&refused), &[])).unwrap(),
            "\"failure\""
        );
    }

    /// Plan §6 Phase 5 test 2. The existing meaning of a clean window is
    /// preserved for an action that actually ran (or reported nothing).
    #[test]
    fn absent_or_ran_run_result_leaves_clean_window_confirmed() {
        assert_eq!(classify(None, &[]), D3Category::Confirmed);
        assert_eq!(
            classify(Some(&ActionRunResult::Ran), &[]),
            D3Category::Confirmed
        );
        // And a `Ran` action still defers to the effect surfaces.
        assert_eq!(
            classify(
                Some(&ActionRunResult::Ran),
                &[DevSignature::DevTauriAssetMissing]
            ),
            D3Category::Contradiction
        );
    }

    /// Plan §6 Phase 5 test 3 + §8 acceptance: a readiness-refused restart must
    /// leave an outcome on `GET /actions/{id}/outcome` that is NOT `confirmed`
    /// and whose evidence names `restart_refused_unsafe`.
    ///
    /// Drives the real watcher. A `Refused` closes the window immediately
    /// (nothing ran, so there is no effect window to wait out), which is also
    /// what keeps this test sub-second against the flat 30s restart window.
    #[tokio::test]
    async fn refused_restart_outcome_is_not_confirmed_and_names_the_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Restart);

        spawn_attribution_watcher(
            state,
            record.clone(),
            AttributionTargets {
                early_log_path: None,
                managed: None,
                panic_log_path: None,
                runner_id: None,
            },
            None,
        );

        record.record_run_result(ActionRunResult::Refused {
            reason: "restart_refused_unsafe: runner 'primary' (port 9876) reports it is NOT \
                     safe to restart"
                .to_string(),
        });

        // Well inside RESTART_WINDOW: the refusal closes the window early.
        let outcome = await_outcome(&record, Duration::from_secs(10)).await;
        assert_ne!(
            outcome.category,
            D3Category::Confirmed,
            "the refused restart must not report Confirmed"
        );
        assert_eq!(outcome.category, D3Category::Failure);
        assert!(
            outcome
                .evidence_ref
                .as_deref()
                .is_some_and(|e| e.contains("restart_refused_unsafe")),
            "the outcome must name the refusal; got {:?}",
            outcome.evidence_ref
        );
        assert!(outcome.signatures.is_empty());
    }

    /// Plan §6 Phase 5 test 4 — pins Phase 2, the framing fix. A build whose
    /// terminal result lands AFTER the old flat window would have elapsed must
    /// still be judged on that result.
    ///
    /// `tail` here plays the role the flat 30s played in production: under the
    /// old code the window was exactly `window_for(Build, None) == tail` and
    /// closed `Confirmed` before the failure existed. The failure is recorded
    /// at ~4x `tail`, so this fails against the old sleep-the-window watcher
    /// and passes against the hold-open one.
    #[tokio::test]
    async fn build_failing_after_the_flat_window_does_not_report_confirmed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Build);

        let timing = WindowTiming {
            tail: Duration::from_millis(50),
            backstop: Duration::from_secs(20),
        };
        assert_eq!(
            window_for(ActionKind::Build, None),
            BUILD_TAIL_WINDOW,
            "the pre-fix window for a build with no known duration was the flat tail"
        );

        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets {
                early_log_path: None,
                managed: None,
                panic_log_path: None,
                runner_id: None,
            },
            None,
            timing,
        );

        // Long past the old flat window — and the outcome must still be open.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            record.outcome.read().unwrap().is_none(),
            "the window must stay OPEN until the build reports a terminal result"
        );

        record.record_run_result(ActionRunResult::Errored {
            reason: "cargo build failed: could not compile qontinui-runner".to_string(),
        });

        let outcome = await_outcome(&record, Duration::from_secs(10)).await;
        assert_ne!(outcome.category, D3Category::Confirmed);
        assert_eq!(outcome.category, D3Category::Failure);
        assert!(outcome
            .evidence_ref
            .as_deref()
            .is_some_and(|e| e.contains("cargo build failed")));
        // `duration_ms` must now be a MEASUREMENT of the action, not the flat
        // window plus overhead (the plan's §2 finding: `duration_ms: 30342`).
        assert!(
            outcome.duration_ms >= 200,
            "window closed before the action ended: {}ms",
            outcome.duration_ms
        );
    }

    /// A build that never records a terminal result still closes — on the
    /// backstop. Without this the watcher (and the coord ingest behind it)
    /// would park forever on any path that forgets to report.
    #[tokio::test]
    async fn build_with_no_terminal_result_closes_on_the_backstop() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Build);

        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets {
                early_log_path: None,
                managed: None,
                panic_log_path: None,
                runner_id: None,
            },
            None,
            WindowTiming {
                tail: Duration::from_millis(10),
                backstop: Duration::from_millis(150),
            },
        );

        let outcome = await_outcome(&record, Duration::from_secs(10)).await;
        // No terminal result and no signatures: the pre-existing meaning of a
        // clean window is unchanged for an action that reported nothing.
        assert_eq!(outcome.category, D3Category::Confirmed);
    }

    #[test]
    fn asset_missing_log_yields_contradiction() {
        // The exact 2026-06-07 incident line.
        let log = "INFO starting up\n\
                   ERROR tauri::manager: asset not found: index.html\n\
                   INFO window created\n";
        let (sigs, evidence) = scan_log_content(log);
        assert!(sigs.contains(&DevSignature::DevTauriAssetMissing));
        assert_eq!(classify(None, &sigs), D3Category::Contradiction);
        assert!(evidence.unwrap().contains("asset not found"));
    }

    #[test]
    fn webview_conn_refused_yields_contradiction() {
        let log = "ERROR webview: net::ERR_CONNECTION_REFUSED at http://localhost:1420\n";
        let (sigs, _) = scan_log_content(log);
        assert!(sigs.contains(&DevSignature::DevWebviewConnRefused));
        assert_eq!(classify(None, &sigs), D3Category::Contradiction);
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
            assert_eq!(classify(None, &sigs), D3Category::Failure, "line: {line}");
        }
    }

    #[test]
    fn clean_log_yields_confirmed() {
        let log = "INFO starting up\nINFO bound to port 9876\nINFO ready\n";
        let (sigs, evidence) = scan_log_content(log);
        assert!(sigs.is_empty());
        assert_eq!(classify(None, &sigs), D3Category::Confirmed);
        assert!(evidence.is_none());
    }

    #[test]
    fn contradiction_wins_over_failure_when_both_present() {
        let sigs = vec![
            DevSignature::DevPortBindFail,
            DevSignature::DevTauriAssetMissing,
        ];
        assert_eq!(classify(None, &sigs), D3Category::Contradiction);
    }

    #[test]
    fn ui_error_boundary_classifies_as_contradiction() {
        let sigs = vec![DevSignature::DevUiErrorBoundary];
        assert_eq!(classify(None, &sigs), D3Category::Contradiction);
    }

    #[test]
    fn panic_signature_classifies_as_failure() {
        let sigs = vec![DevSignature::DevPanicStartup];
        assert_eq!(classify(None, &sigs), D3Category::Failure);
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
        assert_eq!(classify(None, &sigs), D3Category::Contradiction);
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
            category: classify(None, &[]),
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
