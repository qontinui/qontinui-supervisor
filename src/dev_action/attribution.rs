//! Delayed-effect attribution watcher.
//!
//! Dev actions emit effects on a delay (the 2026-06-07 asset error landed at
//! t+5s after a "restarted successfully" ACK), so a synchronous verdict is
//! impossible — the watcher sleeps the per-kind attribution window, then scans
//! the effect surfaces and folds a verdict into the action's [`ActionOutcome`]:
//!
//! 1. the early log of each spawn of the acted-on runner made inside the
//!    window, for `DEV-TAURI-ASSET-MISSING` / `DEV-WEBVIEW-CONN-REFUSED` /
//!    `DEV-PORT-BIND-FAIL` signatures,
//! 2. each such spawn's startup panic for `DEV-PANIC-STARTUP` (see
//!    [`scan_panic`] for where it is read from and which panics count),
//! 3. the cached `derived_status` / `ui_error` transition for
//!    `DEV-UI-ERROR-BOUNDARY`.
//!
//! The first two describe one boot, so they are read per spawn, and only for
//! the acted-on runner's spawns made inside the window
//! ([`scan_spawn_effects`]): an action that starts nothing inside its window
//! has no boot of its own to judge. The third is not gated by spawns.
//!
//! The verdict closes at window end. Late observations append to
//! `late_signatures` flagged `late: true` and update statistics only — they
//! NEVER re-open the closed verdict (§3 theory item 2), and they do not bleed
//! into the next action on the same surface.
//!
//! The watcher is a detached `tokio::spawn` owning a `SharedState` clone — the
//! same pattern `routes/runner.rs` uses for the detached rebuild — so it
//! survives the handler future that minted the action. At window close it
//! reads each spawn's early log at the path that spawn recorded on its entry
//! in the runner's spawn ring; that file lives under `temp_dir()` and
//! survives runner cleanup (per `early_log.rs`), so a runner that
//! white-screens and is purged still leaves its log for the watcher. A runner
//! stopped through `POST /runners/{id}/stop` does not: that route deletes the
//! stopped runner's early-log file as soon as the stop succeeds, so a window
//! still open over the stopped spawn reads no early-log signature for it and,
//! with no other signature, closes it `Confirmed` under `Ran` or with no run
//! result (a residual, unchanged from 5fd1ca62, whose reach
//! [`scan_spawn_effects`] states). The panic log does not survive cleanup: a
//! `test-*` runner writes it inside its instance root, which a stop reaps,
//! and the runner's next panic overwrites it. So the watcher first takes the
//! panic the exit monitor attached to the spawn's entry, then the one the
//! supervisor stored on the runner, and only then reads the file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use qontinui_types::dev_signatures::DevSignature;

use crate::dev_action::record::{
    ActionKind, ActionOutcome, ActionRecord, ActionRunResult, D3Category,
};
use crate::log_capture::{LogLevel, LogSource};
use crate::process::env_forwarders::{self, PanicLogDir};
use crate::process::panic_log;
use crate::state::{ManagedRunner, SharedState, SpawnMarker};

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

/// The evidence excerpt for a startup panic: its payload's first line, cut at
/// 200 characters like the early-log excerpt ([`scan_log_content`]). A payload
/// line can be a whole `Debug` dump, read from a panic log of up to 16 KiB,
/// and coord stores at most 2 KiB of evidence.
fn panic_excerpt(payload: &str) -> String {
    let line: String = payload
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect();
    format!("panic: {line}")
}

/// An attribution window as the watcher closed it: from the action's mint to
/// the moment the watcher began reading the effect surfaces.
#[derive(Debug, Clone, Copy)]
struct ClosedWindow {
    open_at: DateTime<Utc>,
    close_at: DateTime<Utc>,
}

impl ClosedWindow {
    fn contains(&self, at: DateTime<Utc>) -> bool {
        self.open_at <= at && at <= self.close_at
    }
}

/// Whether `panic` is the startup panic of `spawn`, a spawn of the acted-on
/// runner made inside `window`. All four must hold:
///
/// 1. **Dated inside the window**, `open_at <= timestamp <= close_at`. This is
///    not a freshness slack measured back from the close: a build-kind window
///    stays open for the whole build and closes long after the boot it judges.
/// 2. **That spawn's.** The header's pid is the spawn's pid, or, with no pid in
///    the header, the panic is dated at or after the spawn. And when the
///    spawn's exit is stamped ([`SpawnMarker::exited_at`]), the panic is dated
///    at or before that exit: a process writes its own panic before it exits,
///    so a later panic under the same pid is a later process's (Windows reuses
///    pids). That also bounds the pid-less fallback. Other processes log
///    panics inside the window too, the process the action stopped (a
///    teardown panic, say, or an adopted runner's write into the stderr pipe a
///    supervisor restart closed) and a later spawn, each under its own pid.
/// 3. **That boot failed.** The spawn's process exited ON ITS OWN by the
///    close: its exit is stamped at or before the close with no stop requested
///    for it ([`SpawnMarker::stop_requested_at`]). Or the action's own result
///    is not [`ActionRunResult::Ran`]. The runner rewrites `runner-panic.log`
///    for every panic, including one that tokio or a catch-panic layer caught
///    while it went on serving, and a caught panic in a runner that came up is
///    not a boot failure. Nor is an exit a stop requested (an operator stop, a
///    later restart or stop of the same runner, the over-age kill): that spawn
///    reads like one still up once its exit is stamped with the request. A
///    crash, an exit nobody asked for (a clean one included) and the
///    first-healthy watchdog's kill of a wedged boot, which latches no stop,
///    read as failed boots. Both readings go by the exit monitor's stamp, so
///    they hold except as the residuals below say. That stamp is on the
///    spawn's ring entry, and the exit is never read from `RunnerState::pid`:
///    the health cache clears that on every tick a primary's port is closed
///    and its API silent, so a primary still booting, before it binds its
///    port, would read as gone.
/// 4. **Not a teardown panic.** When a stop requested the spawn's exit, the
///    panic is dated before the request.
///
/// So, except as the residuals below say: a later restart or stop
/// inside the window does not turn a boot that came up into a
/// failed one; a teardown panic never counts under `Ran`; and
/// with no run result, as a spawn has, or one other than `Ran`, a
/// teardown panic is turned away once the exit its stop asked for is
/// stamped by the close. Spawns, and a `rebuild_when_idle` that succeeds,
/// record no run result, so a panic of a runner they started that is dated
/// before any stop request still counts while one of the three places the
/// watcher reads ([`scan_panic`]) still holds it at the close, a caught panic
/// of a runner that stays up included.
///
/// Residuals, documented and not changed:
/// - `Ran` is what the restart routes record once the runner has answered its
///   health probe. Under `?wait=false` they record it right after the spawn,
///   before any probe, so there a boot whose process stays up with its API
///   never bound, and whose early log matches nothing, closes `Confirmed`: its
///   panic reads as a caught one. No caller passes `wait=false`.
/// - A stop requested but not finished at the close leaves the entry with no
///   request time yet, so with no run result or one other than `Ran` a
///   teardown panic logged between that request and the close counts.
/// - A stop that fails leaves the flag latched, so a later crash of that
///   process reads as requested, as the crash watchdog already reads it.
/// - The exit stamp becomes visible only right after the exit monitor's first
///   critical section after its wait on the child, so a process whose exit
///   falls in that short lag before the close reads as up, exactly like an
///   exit just after the close, and under `Ran` its panic does not count.
/// - Each of the three places the watcher reads holds only the runner's
///   latest panic, and a stop can empty them before the close: a teardown
///   panic overwrites `runner-panic.log`, and on an exit that is not clean
///   (a non-zero exit, or a wait on the child that returned no exit status,
///   the only exits on which the exit monitor parses the log) the stored and
///   attached copies too, and check 4 then turns it away; a plain stop of a
///   `test-*` runner reaps its instance root, `runner-panic.log` included,
///   and a clean exit attaches and stores nothing. A panic the spawn logged
///   before the request then counts only if a copy of it is still held at
///   the close, such as the one spawn-test's failed probe stored, which a
///   clean exit leaves in place.
/// - Starts and stops of one runner are not ordered against each other. No
///   per-runner lock serialises `start_managed_runner`, `stop_runner_by_id`
///   and the old process's exit monitor: a start clears every stop request
///   latched before it records its spawn (`record_spawn_in_registry`); a stop
///   picks its target only after its latch and resets the registry only after
///   its reap; and the exit monitor reads the stop flag and resets `running`,
///   `process` and `pid` as soon as its wait on the child returns. So when a
///   stop, or a restart's stop and start, overlaps a start of the same runner,
///   the stop request, the stop's kill, the stop's reset and the monitor's
///   reset can each land on the other spawn, and this rule's reading of that
///   window is not reliable. For example: a requested exit can read as
///   unrequested, so the stopped spawn reads as a failed boot and its
///   in-window panics count, under `Ran` too; a stop can be lost, answering
///   "stopped" while the runner comes up; a stop's kill or reset can land on
///   the new spawn (for a `test-*` runner, its teardown of the registry entry,
///   the instance root and the exe copy too); the crash watchdog may then act
///   on what it reads, even trying to start a runner that is running; and a
///   spawn whose child a reset dropped is never stamped with its own exit, so
///   it reads as up until another spawn's exit lands on its entry: while that
///   entry is still in the ring, a later spawn of the runner that gets the
///   same pid from the OS has its exit, and the panic parsed at it, stamped on
///   that entry instead (stamp_exit takes the oldest entry of the pid with no
///   exit yet), and that later spawn then reads as up, in its own window too,
///   even where no stop overlaps a start. A per-runner lifecycle lock, a
///   recorded follow-up, would order these; until then they are disclosed
///   here, not modelled. These examples are read from the code; none was
///   reproduced on a running supervisor.
fn is_startup_panic_of(
    panic: &panic_log::RecentPanic,
    spawn: &SpawnMarker,
    window: ClosedWindow,
    run: Option<&ActionRunResult>,
) -> bool {
    let its_pid = match panic.pid {
        Some(pid) => spawn.pid == Some(pid),
        None => panic.timestamp >= spawn.spawned_at,
    };
    let before_its_exit = spawn
        .exited_at
        .is_none_or(|exited| panic.timestamp <= exited);
    let exited_on_its_own = spawn.stop_requested_at.is_none()
        && spawn
            .exited_at
            .is_some_and(|exited| exited <= window.close_at);
    let came_up = matches!(run, Some(ActionRunResult::Ran));
    let before_any_stop = spawn
        .stop_requested_at
        .is_none_or(|requested| panic.timestamp < requested);
    window.contains(panic.timestamp)
        && its_pid
        && before_its_exit
        && (exited_on_its_own || !came_up)
        && before_any_stop
}

/// Where the acted-on runner's spawn sent its panic log: the dir that spawn
/// recorded on the runner (`PanicLogEnv`), else the dir
/// [`env_forwarders::panic_log_dir_for`] resolves for the runner. The runner
/// default path is the answer only for a primary under a supervisor with no
/// `--log-dir`; no other runner writes there, so any other runner whose dir
/// cannot be resolved has no panic log to read.
fn spawn_panic_log_path(
    recorded_dir: Option<&Path>,
    is_primary: bool,
    runner_id: &str,
    supervisor_log_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = recorded_dir {
        return Some(panic_log::resolve_panic_log_path(Some(dir)));
    }
    match env_forwarders::panic_log_dir_for(is_primary, runner_id, supervisor_log_dir) {
        PanicLogDir::Export(dir) | PanicLogDir::InstanceRoot(dir) => {
            Some(panic_log::resolve_panic_log_path(Some(&dir)))
        }
        PanicLogDir::RunnerDefault if is_primary => Some(panic_log::resolve_panic_log_path(None)),
        PanicLogDir::RunnerDefault => None,
    }
}

/// Check the acted-on runner for the startup panic of `spawn`, one of its
/// spawns made inside the window. Returns the `DEV-PANIC-STARTUP` signature +
/// a short excerpt when one is found.
///
/// The panic is that spawn's own. The supervisor sends a non-primary runner's
/// panic log to `<instance root>/logs` and the primary's, under `--log-dir`,
/// to `<log-dir>/runner-<id>`, not to the runner default path. Read, in order,
/// each held to [`is_startup_panic_of`] for this spawn:
///
/// 1. the panic the exit monitor attached to this spawn's ring entry when its
///    process exited ([`SpawnMarker::exit_panic`]). The next spawn clears the
///    stored panic and the runner's next panic, a caught one included,
///    overwrites the file, so for a boot the crash watchdog restarted this is
///    the copy that survives to the close;
/// 2. the panic the supervisor parsed and stored on the runner
///    (`recent_panic`). spawn-test's probe-failure path stores it and then
///    stops the runner, and that stop reaps a `test-*` runner's instance root,
///    `runner-panic.log` included, possibly before the window closes;
/// 3. the panic log the spawn set up ([`spawn_panic_log_path`]), read at
///    window close because the spawn, which comes after the mint, is what
///    records its dir. Every spawn of one runner sets up the same dir.
///
/// The exit monitor stamps a spawn's exit before it parses the panic log and
/// attaches the panic, so a window that closes in between sees the exit with
/// no attached panic, and finds that panic stored or in the file: no spawn
/// clears the file, which only a later panic overwrites, and the crash
/// watchdog's respawn for that exit comes only after both writes. An operator
/// start or restart can begin once `running` reads false (a stop's own reset,
/// or for a primary the health cache, can publish it before the monitor
/// does), but it clears the stored panic only after its exe copy and spawn,
/// and the monitor's own store rewrites it after any earlier clear.
/// Each place holds only the runner's latest panic, so a teardown panic of a
/// spawn a stop ended replaces a panic that spawn logged before the request in
/// the file, and on an exit that is not clean (a non-zero exit, or a wait on
/// the child that returned no exit status, the only exits on which the exit
/// monitor parses the log) in the stored and attached copies too, and is
/// itself turned away (see the residuals of [`is_startup_panic_of`]).
async fn scan_panic(
    state: &SharedState,
    managed: &ManagedRunner,
    spawn: &SpawnMarker,
    window: ClosedWindow,
    run: Option<&ActionRunResult>,
) -> (Vec<DevSignature>, Option<String>) {
    let counts = |panic: &panic_log::RecentPanic| is_startup_panic_of(panic, spawn, window, run);

    let mut panic = spawn.exit_panic.clone().filter(|panic| counts(panic));
    if panic.is_none() {
        let stored = managed.recent_panic.read().await.clone();
        panic = stored.filter(|panic| counts(panic));
    }
    if panic.is_none() {
        let recorded_dir = managed.panic_log_dir.read().await.clone();
        panic = spawn_panic_log_path(
            recorded_dir.as_deref(),
            managed.config.kind().is_primary(),
            &managed.config.id,
            state.config.log_dir.as_deref(),
        )
        .and_then(|path| panic_log::parse_panic_file(&path))
        .filter(|panic| counts(panic));
    }
    match panic {
        Some(panic) => (
            vec![DevSignature::DevPanicStartup],
            Some(panic_excerpt(&panic.payload)),
        ),
        None => (Vec::new(), None),
    }
}

/// The early-log and startup-panic signatures of every spawn of the acted-on
/// runner made inside the window, read at window close, oldest spawn first.
///
/// Both surfaces describe one boot, so they are read per spawn: each spawn's
/// own early log and its own startup panic ([`scan_panic`]). Every spawn the
/// runner's ring ([`ManagedRunner::recent_spawns`]) recorded from the mint to
/// the close is judged, not only the last. The crash watchdog can restart a
/// boot that died inside a window, after a first backoff of 5 seconds, and its
/// respawn must not hide the action's own crashed boot. A later restart or
/// stop of the runner inside the window ends a spawn too, and that requested
/// exit does not make a boot that came up a failed one, the residuals
/// [`is_startup_panic_of`] lists excepted. Nothing ties a spawn to the action
/// that asked for it, so any spawn inside the window, a crash-watchdog
/// respawn included, is read as the action's.
///
/// An action with no spawn inside its window reads neither surface:
/// `fix_and_rebuild`, which only builds; a refused restart or rebuild, or one
/// whose build failed; a spawn that lands after its window has closed. What
/// those files hold then describes a boot the action did not cause. With no
/// managed runner there is no spawn to read.
///
/// Residual, documented and not changed: an explicit
/// `POST /runners/{id}/stop` deletes the stopped runner's early-log file as
/// soon as the stop succeeds, so a window still open over that spawn reads no
/// early-log signature for it (`DEV-TAURI-ASSET-MISSING`,
/// `DEV-WEBVIEW-CONN-REFUSED`, `DEV-PORT-BIND-FAIL`) and, with no other
/// signature, closes it `Confirmed` under `Ran` or with no run result. The
/// dashboard's Stop (named and temp runners) and spawn-test's `stop_url`
/// reach spawn windows; a restart's, a build's or a `rebuild_when_idle`'s
/// window is reached only by a direct `POST /runners/primary/stop`. A
/// restart's own stop and the legacy `/runner/stop` delete nothing. Unchanged
/// from 5fd1ca62.
async fn scan_spawn_effects(
    state: &SharedState,
    managed: Option<&ManagedRunner>,
    window: ClosedWindow,
    run: Option<&ActionRunResult>,
) -> (Vec<DevSignature>, Option<String>) {
    let Some(managed) = managed else {
        return (Vec::new(), None);
    };
    let spawns = managed
        .recent_spawns
        .read()
        .await
        .spawned_between(window.open_at, window.close_at);
    let (mut signatures, mut evidence) = (Vec::new(), None);
    for spawn in &spawns {
        // Early logs are per-spawn files: this is the one the spawn opened.
        // `None` when it could not open one: that boot then has no early log,
        // and no other boot's file is read in its place.
        let (log_sigs, log_ev) = scan_early_log(spawn.early_log_path.as_deref());
        merge(&mut signatures, &mut evidence, log_sigs, log_ev);
        let (panic_sigs, panic_ev) = scan_panic(state, managed, spawn, window, run).await;
        merge(&mut signatures, &mut evidence, panic_sigs, panic_ev);
    }
    (signatures, evidence)
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
    /// The runner whose boot this action drives: the registry entry the
    /// action's spawn starts. Everything it is read for is read at window
    /// close, because the spawn comes after the mint: its spawn ring and, for
    /// each spawn inside the window, that spawn's early log and startup panic
    /// ([`scan_spawn_effects`]). `None` reads neither surface.
    pub managed: Option<Arc<ManagedRunner>>,
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

        let run_result = await_attribution_window(&record, kind, build_duration, timing).await;
        let window = ClosedWindow {
            open_at: record.started_at,
            close_at: Utc::now(),
        };

        // Gather signatures from every effect surface.
        let (mut signatures, mut evidence) = scan_spawn_effects(
            &state,
            targets.managed.as_deref(),
            window,
            run_result.as_ref(),
        )
        .await;
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
            build_pool: BuildPoolConfig::fixed(1),
            no_prewarm: true,
            no_webview: true,
            temp_runner_display: None,
            ambient_target_dir: crate::config::AmbientTargetDir::Ignore,
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
                "the attribution watcher never folded an outcome in within {budget:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The child pid of the spawn a watcher test's action makes.
    const SPAWNED_PID: u32 = 4242;

    /// Write `<dir>/runner-panic.log` in the runner's format (the
    /// `process::panic_log` module docs), its header naming `pid` and dated
    /// `at` to the millisecond, as the runner writes it.
    fn write_panic_log(
        dir: &Path,
        runner_id: &str,
        pid: u32,
        at: DateTime<Utc>,
        payload: &str,
    ) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("runner-panic.log");
        let stamp = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
        std::fs::write(
            &path,
            format!(
                "[{stamp}] PANIC runner-id={runner_id} pid={pid} version=1.0.0\n\
                 location: src/main.rs:10:5\n\
                 thread: main\n\
                 payload:\n\
                 {payload}\n\
                 \n\
                 backtrace:\n\
                 0: qontinui_runner::main\n"
            ),
        )
        .unwrap();
        path
    }

    /// The primary as a watcher test's acted-on runner. Nothing starts it:
    /// each test records the spawns it needs with [`record_spawn`].
    fn test_primary() -> Arc<ManagedRunner> {
        Arc::new(ManagedRunner::new(
            crate::config::RunnerConfig::default_primary(),
            false,
        ))
    }

    /// Record a spawn of `runner` at `at`, child `pid`, panic log under
    /// `panic_log_dir`, with no early log of its own
    /// ([`record_spawn_with_early_log`]).
    async fn record_spawn(
        runner: &ManagedRunner,
        at: DateTime<Utc>,
        pid: u32,
        panic_log_dir: &Path,
    ) {
        record_spawn_with_early_log(runner, at, pid, panic_log_dir, None).await;
    }

    /// Record a spawn of `runner` at `at`, child `pid`, as
    /// `start_managed_runner` leaves the runner: the panic-log dir re-pointed
    /// to `panic_log_dir`, the stored panic cleared, the early log re-pointed
    /// to `early_log`, the live process, and the spawn's entry in the ring.
    /// The process stays live until a test records its exit
    /// ([`record_exit`]).
    async fn record_spawn_with_early_log(
        runner: &ManagedRunner,
        at: DateTime<Utc>,
        pid: u32,
        panic_log_dir: &Path,
        early_log: Option<PathBuf>,
    ) {
        *runner.panic_log_dir.write().await = Some(panic_log_dir.to_path_buf());
        *runner.recent_panic.write().await = None;
        *runner.early_log_path.write().await = early_log.clone();
        {
            let mut process = runner.runner.write().await;
            process.started_at = Some(at);
            process.pid = Some(pid);
        }
        runner
            .recent_spawns
            .write()
            .await
            .push(SpawnMarker::new(at, Some(pid), early_log));
    }

    /// Record an exit nobody asked for (a crash, an unrequested clean exit)
    /// of `runner`'s spawn with child `pid` at `at`: [`record_requested_exit`]
    /// with no stop request.
    async fn record_exit(
        runner: &ManagedRunner,
        pid: u32,
        at: DateTime<Utc>,
        exit_panic: Option<panic_log::RecentPanic>,
    ) {
        record_requested_exit(runner, pid, None, at, exit_panic).await;
    }

    /// Record the exit of `runner`'s spawn with child `pid` at `at`, which a
    /// stop requested at `requested_at` (`None`: nobody asked for it), as the
    /// exit monitor does: the process cleared from the registry and the exit
    /// stamped on that spawn's ring entry with the request's time; then the
    /// panic it parsed at the exit (if any) stored on the runner and attached
    /// to that entry.
    async fn record_requested_exit(
        runner: &ManagedRunner,
        pid: u32,
        requested_at: Option<DateTime<Utc>>,
        at: DateTime<Utc>,
        exit_panic: Option<panic_log::RecentPanic>,
    ) {
        {
            let mut process = runner.runner.write().await;
            process.running = false;
            process.pid = None;
        }
        assert!(
            runner
                .recent_spawns
                .write()
                .await
                .stamp_exit(pid, at, requested_at),
            "no spawn of pid {pid} is waiting for its exit stamp"
        );
        if let Some(panic) = exit_panic {
            *runner.recent_panic.write().await = Some(panic.clone());
            assert!(
                runner
                    .recent_spawns
                    .write()
                    .await
                    .attach_exit_panic(pid, at, panic),
                "no exit of pid {pid} was stamped at {at}"
            );
        }
    }

    /// When the action's own spawn is recorded: just after the mint.
    fn spawned_after_the_mint(record: &ActionRecord) -> DateTime<Utc> {
        record.started_at + chrono::Duration::milliseconds(1)
    }

    /// A moment inside the action's window, after its spawn, that survives the
    /// header's millisecond precision: a panic dated in the window's first
    /// millisecond would read back as dated before the window opened.
    fn inside_the_window(record: &ActionRecord) -> DateTime<Utc> {
        record.started_at + chrono::Duration::milliseconds(2)
    }

    /// The millisecond timing the panic tests run a build-kind window on: it
    /// closes `tail` after the action's terminal result. Where the panic is
    /// read from does not depend on the action's kind.
    const FAST_BUILD_WINDOW: WindowTiming = WindowTiming {
        tail: Duration::from_millis(50),
        backstop: Duration::from_secs(20),
    };

    /// Spawn the watcher for `record`, a build-kind action, against `runner`
    /// on [`FAST_BUILD_WINDOW`] timing.
    fn watch_on_a_fast_build_window(
        state: SharedState,
        record: &Arc<ActionRecord>,
        runner: &Arc<ManagedRunner>,
    ) {
        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets {
                managed: Some(runner.clone()),
                runner_id: None,
            },
            None,
            FAST_BUILD_WINDOW,
        );
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

        spawn_attribution_watcher(state, record.clone(), AttributionTargets::default(), None);

        record.record_run_result(ActionRunResult::Refused {
            reason: "restart_refused_unsafe: runner 'primary' (port 9876) reports it is NOT \
                     safe to restart"
                .to_string(),
        });

        // Well inside RESTART_WINDOW: the refusal closes the window early.
        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
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
            AttributionTargets::default(),
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

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
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
            AttributionTargets::default(),
            None,
            WindowTiming {
                tail: Duration::from_millis(10),
                backstop: Duration::from_millis(150),
            },
        );

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        // No terminal result and no signatures: the pre-existing meaning of a
        // clean window is unchanged for an action that reported nothing.
        assert_eq!(outcome.category, D3Category::Confirmed);
    }

    /// The watcher reads the startup panic of the spawn the action made, in the
    /// runner's own log dir (`<log-dir>/runner-primary` for a primary under
    /// `--log-dir`), not at the runner default path. It reads the dir at window
    /// close, because the spawn records it after the mint. Here an init task
    /// panics and the process stays up with its API never bound, so the
    /// restart reports it unhealthy: a live process does not hide the panic of
    /// a boot that failed.
    #[tokio::test]
    async fn a_startup_panic_in_the_acted_on_runners_log_dir_is_attributed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let first_line = "y".repeat(300);
        write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            inside_the_window(&record),
            &format!("{first_line}\nsecond line"),
        );
        record.record_run_result(ActionRunResult::Errored {
            reason: "runner unhealthy after start".to_string(),
        });

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(outcome.category, D3Category::Failure);
        // The panic's excerpt, not the run's reason, is the evidence: the
        // payload's first line cut at 200 characters, 207 with its prefix.
        let evidence = outcome.evidence_ref.expect("the panic's excerpt");
        assert_eq!(evidence, format!("panic: {}", "y".repeat(200)));
        assert_eq!(evidence.chars().count(), 207);
    }

    /// spawn-test's probe failure stores the panic on the runner and then stops
    /// it, and stopping a `test-*` runner reaps its instance root,
    /// `logs/runner-panic.log` included, possibly before the window closes.
    /// The stored panic is what the watcher reads then, for the spawn's ring
    /// entry, which no stop removes.
    #[tokio::test]
    async fn a_stored_panic_inside_the_window_is_attributed_after_its_file_is_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let id = "test-panic-reaped";
        let runner = Arc::new(ManagedRunner::new(
            crate::config::RunnerConfig {
                id: id.to_string(),
                name: id.to_string(),
                port: 9877,
                kind: qontinui_types::wire::runner_kind::RunnerKind::Temp { id: id.to_string() },
                ..crate::config::RunnerConfig::default_primary()
            },
            false,
        ));
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &runner);

        let logs = tmp.path().join("instances").join(id).join("logs");
        record_spawn(&runner, spawned_after_the_mint(&record), SPAWNED_PID, &logs).await;
        let path = write_panic_log(
            &logs,
            id,
            SPAWNED_PID,
            inside_the_window(&record),
            "index out of bounds: the len is 0 but the index is 0",
        );
        // The probe-failure path parses the file and stores it on the runner,
        // then its stop clears the process and reaps the instance root.
        let stored = panic_log::parse_panic_file(&path).expect("the fixture parses");
        *runner.recent_panic.write().await = Some(stored);
        {
            let mut process = runner.runner.write().await;
            process.started_at = None;
            process.pid = None;
        }
        std::fs::remove_dir_all(tmp.path().join("instances").join(id)).unwrap();
        assert!(!path.exists());
        record.record_run_result(ActionRunResult::Errored {
            reason: "runner never answered /health".to_string(),
        });

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("panic: index out of bounds: the len is 0 but the index is 0")
        );
    }

    /// An action that never starts the runner (refused, here) leaves the
    /// running boot's earlier panic stored on the runner and in its log dir.
    /// It is not this action's: no spawn landed in the window, so neither copy
    /// is read, and the refusal stays the evidence.
    #[tokio::test]
    async fn a_stored_panic_from_before_the_window_is_not_attributed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Restart);
        // The running boot, spawned a minute before the mint, panicked five
        // seconds before it and kept serving.
        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            record.started_at - chrono::Duration::seconds(60),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let earlier = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            record.started_at - chrono::Duration::seconds(5),
            "an earlier boot's panic",
        );
        *primary.recent_panic.write().await =
            Some(panic_log::parse_panic_file(&earlier).expect("the fixture parses"));

        spawn_attribution_watcher(
            state,
            record.clone(),
            AttributionTargets {
                managed: Some(primary),
                runner_id: None,
            },
            None,
        );
        // A refusal closes the window at once.
        record.record_run_result(ActionRunResult::Refused {
            reason: "restart_refused_unsafe: a terminal-hosted agent session is live".to_string(),
        });

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert!(
            outcome.signatures.is_empty(),
            "an earlier boot's panic was attributed: {:?}",
            outcome.signatures
        );
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("restart_refused_unsafe: a terminal-hosted agent session is live")
        );
    }

    /// A restart stops the old process after the mint and only then spawns the
    /// new one, so a panic the old process logs on its way down (a teardown
    /// panic, say) is dated inside the window. It carries the old pid and is
    /// not the spawn's: not from the log file the new boot writes to, and not
    /// when the old process's exit monitor stores it after the new spawn
    /// cleared the slot. The old process's own spawn predates the window, so
    /// the panic that monitor attached to that spawn's entry is not read
    /// either.
    #[tokio::test]
    async fn an_old_process_panic_dated_after_the_mint_is_not_attributed() {
        const OLD_PID: u32 = 1111;
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        // The old process, spawned a minute before the mint.
        record_spawn(
            &primary,
            record.started_at - chrono::Duration::seconds(60),
            OLD_PID,
            &log_dir,
        )
        .await;
        let teardown = write_panic_log(
            &log_dir,
            "primary",
            OLD_PID,
            inside_the_window(&record),
            "cannot move state from Destroyed",
        );
        let new_spawn_at = inside_the_window(&record) + chrono::Duration::milliseconds(1);
        record_spawn(&primary, new_spawn_at, SPAWNED_PID, &log_dir).await;
        // The old process's exit monitor stamps its exit, then attaches and
        // stores the teardown panic, after the new spawn cleared the slot. The
        // exit is recorded as one nobody asked for, so the stop-request check
        // cannot be what turns the panic away: only the window and the pid do.
        let teardown = panic_log::parse_panic_file(&teardown).expect("the fixture parses");
        record_exit(&primary, OLD_PID, new_spawn_at, Some(teardown)).await;
        record.record_run_result(ActionRunResult::Errored {
            reason: "runner unhealthy after start".to_string(),
        });

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert!(
            outcome.signatures.is_empty(),
            "the old process's panic was attributed: {:?}",
            outcome.signatures
        );
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("runner unhealthy after start")
        );
    }

    /// `fix_and_rebuild` mints a build of the primary and starts nothing
    /// ("restart manually to apply"). The running primary's panic log and
    /// early log describe the boot that was running before the action, so a
    /// panic that boot logs during the build is not the action's, and neither
    /// is a white-screen line in its early log. The build failed here, so the
    /// run result is not `Ran` and only the missing spawn turns both away.
    #[tokio::test]
    async fn a_build_that_starts_nothing_attributes_no_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        // The running boot, spawned before the action was minted, with a
        // white-screen line in its early log.
        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        let early_log = tmp.path().join("primary-early.log");
        std::fs::write(
            &early_log,
            "ERROR tauri::manager: asset not found: index.html\n",
        )
        .unwrap();
        record_spawn_with_early_log(
            &primary,
            record.started_at - chrono::Duration::minutes(10),
            SPAWNED_PID,
            &log_dir,
            Some(early_log),
        )
        .await;
        watch_on_a_fast_build_window(state, &record, &primary);

        // During the build the running primary logs a panic of its own.
        let panic = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            inside_the_window(&record),
            "a background task panicked",
        );
        *primary.recent_panic.write().await =
            Some(panic_log::parse_panic_file(&panic).expect("the fixture parses"));
        record.record_run_result(ActionRunResult::Errored {
            reason: "cargo build failed".to_string(),
        });

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert!(
            outcome.signatures.is_empty(),
            "the running boot's surfaces were attributed: {:?}",
            outcome.signatures
        );
        assert_eq!(outcome.evidence_ref.as_deref(), Some("cargo build failed"));
    }

    /// The runner writes `runner-panic.log` for every panic, a caught one
    /// included. A restart whose spawned runner came up healthy (`Ran`) and
    /// has not exited has not failed to boot, whatever it logged; the same
    /// panic under a restart that reported the runner unhealthy is the boot's.
    #[tokio::test]
    async fn a_caught_panic_in_a_live_healthy_spawned_runner_is_not_attributed() {
        let unhealthy = ActionRunResult::Errored {
            reason: "runner unhealthy after start".to_string(),
        };
        for (result, attributed) in [(ActionRunResult::Ran, false), (unhealthy, true)] {
            let tmp = tempfile::tempdir().unwrap();
            let state = watcher_test_state(tmp.path());
            let primary = test_primary();
            let record = bare_record(ActionKind::Build);
            watch_on_a_fast_build_window(state, &record, &primary);

            let log_dir = tmp.path().join("dev-logs").join("runner-primary");
            record_spawn(
                &primary,
                spawned_after_the_mint(&record),
                SPAWNED_PID,
                &log_dir,
            )
            .await;
            write_panic_log(
                &log_dir,
                "primary",
                SPAWNED_PID,
                inside_the_window(&record),
                "a background task panicked",
            );
            record.record_run_result(result.clone());

            let outcome = await_outcome(
                &record,
                crate::test_clock::poll_budget(Duration::from_secs(30)),
            )
            .await;
            if attributed {
                assert_eq!(
                    outcome.signatures,
                    ["DEV-PANIC-STARTUP"],
                    "under {result:?}"
                );
            } else {
                assert!(
                    outcome.signatures.is_empty(),
                    "under {result:?}: {:?}",
                    outcome.signatures
                );
                assert_eq!(outcome.category, D3Category::Confirmed);
            }
        }
    }

    /// A build-kind window closes on the action's own result, which can land
    /// minutes after the boot it judges: a rebuild-restart's health wait runs
    /// 30 s past a runner that died at startup before the 30 s tail, and
    /// `rebuild_when_idle` records no result on success, so its window runs to
    /// the backstop. The panic is held to the window, not to a freshness slack
    /// measured back from the close.
    #[tokio::test]
    async fn a_startup_panic_long_before_a_build_window_closes_is_attributed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        // Minted two minutes ago, so the window closes about 120 s after the
        // spawn's panic.
        let mut minted = ActionRecord::new(ActionKind::Build, None, "test".to_string(), &[]);
        minted.started_at -= chrono::Duration::seconds(120);
        let record = Arc::new(minted);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let path = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            inside_the_window(&record),
            "called `Option::unwrap()` on a `None` value",
        );
        // The process died at startup, and its exit monitor stamped the exit
        // with the panic it parsed.
        let panic = panic_log::parse_panic_file(&path).expect("the fixture parses");
        record_exit(
            &primary,
            SPAWNED_PID,
            inside_the_window(&record) + chrono::Duration::milliseconds(1),
            Some(panic),
        )
        .await;
        record.record_run_result(ActionRunResult::Errored {
            reason: "runner unhealthy after start".to_string(),
        });

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert!(
            outcome.duration_ms >= 120_000,
            "the window closed {}ms after the mint",
            outcome.duration_ms
        );
    }

    /// The crash-watchdog timeline: the action's own boot panics at startup
    /// and exits, its exit monitor stamps that exit with the panic it parsed,
    /// and the crash watchdog's respawn (5 s later in production, inside a
    /// restart's 30 s window) clears the stored panic, comes up and is still
    /// live at the close, where the action records `Ran`. With
    /// `respawn_panics` the respawn also logs a caught panic of its own, which
    /// overwrites `runner-panic.log`. Returns the action's outcome.
    async fn crashed_boot_then_crash_watchdog_respawn(respawn_panics: bool) -> ActionOutcome {
        const RESPAWNED_PID: u32 = 4343;
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let crashed_at = inside_the_window(&record);
        let crash = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            crashed_at,
            "the action's own boot panicked",
        );
        let crash = panic_log::parse_panic_file(&crash).expect("the fixture parses");
        record_exit(&primary, SPAWNED_PID, crashed_at, Some(crash)).await;

        let respawned_at = crashed_at + chrono::Duration::milliseconds(1);
        record_spawn(&primary, respawned_at, RESPAWNED_PID, &log_dir).await;
        if respawn_panics {
            write_panic_log(
                &log_dir,
                "primary",
                RESPAWNED_PID,
                respawned_at + chrono::Duration::milliseconds(1),
                "a background task panicked",
            );
        }
        record.record_run_result(ActionRunResult::Ran);

        await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await
    }

    /// Every spawn inside the window is judged, not only the last: the crash
    /// watchdog's respawn, live and up at the close, does not hide the
    /// action's own boot, which panicked and exited inside the window.
    #[tokio::test]
    async fn a_crashed_boot_the_crash_watchdog_respawned_inside_the_window_is_attributed() {
        let outcome = crashed_boot_then_crash_watchdog_respawn(false).await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(outcome.category, D3Category::Failure);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("panic: the action's own boot panicked")
        );
    }

    /// The respawn's start cleared the stored panic and its caught panic
    /// overwrote the file, so the crashed boot's panic survives only where its
    /// exit monitor attached it, on that boot's own ring entry. The respawn's
    /// caught panic is its own and, with the respawn up and live, not a boot
    /// failure.
    #[tokio::test]
    async fn a_crashed_boots_panic_is_attributed_after_the_respawn_overwrites_the_panic_log() {
        let outcome = crashed_boot_then_crash_watchdog_respawn(true).await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(outcome.category, D3Category::Failure);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("panic: the action's own boot panicked")
        );
    }

    /// Liveness is the spawn's own exit stamp, not the registry pid. The
    /// health cache clears the primary's pid on every tick its port is closed
    /// and its API silent, which is every tick of a boot before it binds, and
    /// only Windows puts it back. A spawned primary that came up (`Ran`) and
    /// whose exit no stamp records is read as live, whatever the registry
    /// says, and its caught panic is not a boot failure.
    #[tokio::test]
    async fn a_booting_primary_with_no_registry_pid_is_live_until_its_exit_is_stamped() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            inside_the_window(&record),
            "a background task panicked",
        );
        // A health-cache tick before the port bound.
        primary.runner.write().await.pid = None;
        record.record_run_result(ActionRunResult::Ran);

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert!(
            outcome.signatures.is_empty(),
            "a live spawn's caught panic was attributed: {:?}",
            outcome.signatures
        );
        assert_eq!(outcome.category, D3Category::Confirmed);
    }

    /// A spawn whose exit its exit monitor stamped before the close, with no
    /// stop requested, is gone, so its own panic counts even when the action
    /// recorded `Ran`: the runner came up (or, under `wait=false`, merely
    /// spawned) and died inside the window.
    #[tokio::test]
    async fn a_spawn_stamped_exited_before_the_close_is_attributed_under_ran() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let path = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            inside_the_window(&record),
            "called `Option::unwrap()` on a `None` value",
        );
        let panic = panic_log::parse_panic_file(&path).expect("the fixture parses");
        record_exit(
            &primary,
            SPAWNED_PID,
            inside_the_window(&record) + chrono::Duration::milliseconds(1),
            Some(panic),
        )
        .await;
        record.record_run_result(ActionRunResult::Ran);

        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(outcome.category, D3Category::Failure);
    }

    /// The pid of a later spawn inside a watcher test's window.
    const NEXT_PID: u32 = 4343;

    /// A boot that came up, logged a panic tokio caught while it served, and
    /// then exited inside the window, with the runner's next spawn up and live
    /// at the close, where the action records `Ran`. With `stop_requested` a
    /// stop asked for the exit (a later restart of the runner, which made the
    /// next spawn); without it the process exited on its own. The exit monitor
    /// parsed the caught panic at the exit and attached it either way. Returns
    /// the action's outcome.
    async fn came_up_then_exited_inside_the_window(stop_requested: bool) -> ActionOutcome {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let caught_at = inside_the_window(&record);
        let caught = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            caught_at,
            "a background task panicked",
        );
        let caught = panic_log::parse_panic_file(&caught).expect("the fixture parses");
        let requested_at = caught_at + chrono::Duration::milliseconds(1);
        let exited_at = caught_at + chrono::Duration::milliseconds(2);
        record_requested_exit(
            &primary,
            SPAWNED_PID,
            stop_requested.then_some(requested_at),
            exited_at,
            Some(caught),
        )
        .await;
        record_spawn(
            &primary,
            exited_at + chrono::Duration::milliseconds(1),
            NEXT_PID,
            &log_dir,
        )
        .await;
        record.record_run_result(ActionRunResult::Ran);

        await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await
    }

    /// A later restart or stop inside the window does not turn a boot that
    /// came up into a failed one once its exit is stamped with the request:
    /// that requested exit reads like a spawn still up, so under `Ran` the
    /// panic it caught while it served is not a boot failure, from its
    /// attached panic, the stored one or the file.
    #[tokio::test]
    async fn a_boot_that_came_up_and_a_later_stop_ended_is_not_attributed_under_ran() {
        let outcome = came_up_then_exited_inside_the_window(true).await;
        assert!(
            outcome.signatures.is_empty(),
            "a stopped boot that came up was read as failed: {:?}",
            outcome.signatures
        );
        assert_eq!(outcome.category, D3Category::Confirmed);
    }

    /// The same boot exiting with nobody asking it to is a failed boot even
    /// under `Ran`, and its panic counts.
    #[tokio::test]
    async fn a_boot_that_came_up_and_exited_on_its_own_is_attributed_under_ran() {
        let outcome = came_up_then_exited_inside_the_window(false).await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(outcome.category, D3Category::Failure);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("panic: a background task panicked")
        );
    }

    /// The timing of a build-kind window whose action records no run result,
    /// as a spawn records none and a `rebuild_when_idle` that succeeds records
    /// none: it closes on the backstop. The panic clauses read only the run
    /// result, not the kind, and a spawn window is a flat 30 s.
    const NO_RESULT_WINDOW: WindowTiming = WindowTiming {
        tail: Duration::from_millis(10),
        backstop: Duration::from_millis(300),
    };

    /// Spawn the watcher for `record`, a build-kind action that records no run
    /// result, against `runner` on [`NO_RESULT_WINDOW`] timing, and return its
    /// outcome. Called once every spawn and exit is recorded, so the window
    /// cannot close before they are.
    async fn outcome_with_no_run_result(
        state: SharedState,
        record: &Arc<ActionRecord>,
        runner: &Arc<ManagedRunner>,
    ) -> ActionOutcome {
        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets {
                managed: Some(runner.clone()),
                runner_id: None,
            },
            None,
            NO_RESULT_WINDOW,
        );
        let outcome = await_outcome(
            record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert!(
            record.run_result().is_none(),
            "the action recorded a result"
        );
        outcome
    }

    /// With no run result, a spawn the window holds counts as a boot that did
    /// not come up, but once its exit is stamped with the stop request, the
    /// teardown panic of that stop does not: it is dated after the request.
    /// Here a later restart stops the spawn, the runner panics on its way
    /// down, and the next spawn is live at the close.
    #[tokio::test]
    async fn a_teardown_panic_after_a_requested_stop_is_not_attributed_with_no_run_result() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let requested_at = inside_the_window(&record);
        let teardown = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            requested_at + chrono::Duration::milliseconds(1),
            "cannot move state from Destroyed",
        );
        let teardown = panic_log::parse_panic_file(&teardown).expect("the fixture parses");
        let exited_at = requested_at + chrono::Duration::milliseconds(2);
        record_requested_exit(
            &primary,
            SPAWNED_PID,
            Some(requested_at),
            exited_at,
            Some(teardown),
        )
        .await;
        record_spawn(
            &primary,
            exited_at + chrono::Duration::milliseconds(1),
            NEXT_PID,
            &log_dir,
        )
        .await;

        let outcome = outcome_with_no_run_result(state, &record, &primary).await;
        assert!(
            outcome.signatures.is_empty(),
            "a teardown panic was attributed: {:?}",
            outcome.signatures
        );
        assert_eq!(outcome.category, D3Category::Confirmed);
    }

    /// spawn-test's probe failure: the spawned runner's boot panics and never
    /// answers, the probe stores the panic, then a stop ends the runner. With
    /// no run result the boot did not come up, and its panic, dated before the
    /// stop request, counts.
    #[tokio::test]
    async fn a_boot_panic_before_a_requested_stop_is_attributed_with_no_run_result() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
        )
        .await;
        let panicked_at = inside_the_window(&record);
        let boot = write_panic_log(
            &log_dir,
            "primary",
            SPAWNED_PID,
            panicked_at,
            "the boot panicked before its API bound",
        );
        let boot = panic_log::parse_panic_file(&boot).expect("the fixture parses");
        *primary.recent_panic.write().await = Some(boot.clone());
        record_requested_exit(
            &primary,
            SPAWNED_PID,
            Some(panicked_at + chrono::Duration::milliseconds(1)),
            panicked_at + chrono::Duration::milliseconds(2),
            Some(boot),
        )
        .await;

        let outcome = outcome_with_no_run_result(state, &record, &primary).await;
        assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
        assert_eq!(outcome.category, D3Category::Failure);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("panic: the boot panicked before its API bound")
        );
    }

    /// The early-log line of the 2026-06-07 white screen.
    const WHITE_SCREEN_LINE: &str = "ERROR tauri::manager: asset not found: index.html";

    /// Two spawns inside the window, one of whose early logs holds the white
    /// screen line and the other a clean boot: the first spawn's when
    /// `first_white_screens`, else the second's. A later restart stops the
    /// first spawn and makes the second, which is live at the close, where
    /// the action records `Ran`. Returns the action's outcome.
    async fn two_spawns_one_white_screens(first_white_screens: bool) -> ActionOutcome {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        watch_on_a_fast_build_window(state, &record, &primary);

        let clean = tmp.path().join("clean-early.log");
        std::fs::write(&clean, "INFO starting up\nINFO bound to port 9876\n").unwrap();
        let white = tmp.path().join("white-early.log");
        std::fs::write(&white, format!("INFO starting up\n{WHITE_SCREEN_LINE}\n")).unwrap();
        let (first, second) = if first_white_screens {
            (white, clean)
        } else {
            (clean, white)
        };

        let log_dir = tmp.path().join("dev-logs").join("runner-primary");
        record_spawn_with_early_log(
            &primary,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &log_dir,
            Some(first),
        )
        .await;
        let stopped_at = inside_the_window(&record);
        record_requested_exit(&primary, SPAWNED_PID, Some(stopped_at), stopped_at, None).await;
        record_spawn_with_early_log(
            &primary,
            stopped_at + chrono::Duration::milliseconds(1),
            NEXT_PID,
            &log_dir,
            Some(second),
        )
        .await;
        record.record_run_result(ActionRunResult::Ran);

        await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await
    }

    /// Each spawn inside the window is read through the early log its own
    /// ring entry names, not the runner's current one: the first spawn's white
    /// screen is attributed although the second, clean, spawn re-pointed the
    /// runner's early log after it.
    #[tokio::test]
    async fn the_first_spawns_white_screen_is_read_from_its_own_early_log() {
        let outcome = two_spawns_one_white_screens(true).await;
        assert_eq!(outcome.signatures, ["DEV-TAURI-ASSET-MISSING"]);
        assert_eq!(outcome.category, D3Category::Contradiction);
        assert_eq!(outcome.evidence_ref.as_deref(), Some(WHITE_SCREEN_LINE));
    }

    /// The reverse order: the second spawn's white screen is attributed, not
    /// hidden by the first spawn's clean early log.
    #[tokio::test]
    async fn the_second_spawns_white_screen_is_read_from_its_own_early_log() {
        let outcome = two_spawns_one_white_screens(false).await;
        assert_eq!(outcome.signatures, ["DEV-TAURI-ASSET-MISSING"]);
        assert_eq!(outcome.category, D3Category::Contradiction);
        assert_eq!(outcome.evidence_ref.as_deref(), Some(WHITE_SCREEN_LINE));
    }

    /// The four clauses of [`is_startup_panic_of`], one at a time.
    #[test]
    fn a_panic_counts_inside_the_window_from_the_spawned_process_when_the_boot_failed() {
        let open = Utc::now();
        // A long build window: the clauses hold the panic to the window, not
        // to its age at the close.
        let window = ClosedWindow {
            open_at: open,
            close_at: open + chrono::Duration::minutes(30),
        };
        // A spawn whose exit no stamp records; the same spawn stamped exited
        // on its own at 20 s, before the close; stamped after the close,
        // which the close does not see; and stamped exited at 20 s by a stop
        // requested at 15 s.
        let live = SpawnMarker::new(open + chrono::Duration::seconds(5), Some(SPAWNED_PID), None);
        let gone = SpawnMarker {
            exited_at: Some(open + chrono::Duration::seconds(20)),
            ..live.clone()
        };
        let gone_after_the_close = SpawnMarker {
            exited_at: Some(window.close_at + chrono::Duration::seconds(1)),
            ..live.clone()
        };
        let unmatchable = SpawnMarker {
            pid: None,
            ..gone.clone()
        };
        let stopped = SpawnMarker {
            stop_requested_at: Some(open + chrono::Duration::seconds(15)),
            ..gone.clone()
        };
        let at = |secs: i64, pid: Option<u32>| panic_log::RecentPanic {
            timestamp: open + chrono::Duration::seconds(secs),
            payload: "boom".to_string(),
            location: None,
            thread: None,
            backtrace_preview: None,
            runner_id: Some("primary".to_string()),
            pid,
            version: None,
            file_path: "runner-panic.log".to_string(),
        };
        let unhealthy = ActionRunResult::Errored {
            reason: "runner unhealthy after start".to_string(),
        };
        let ran = ActionRunResult::Ran;
        let spawned = Some(SPAWNED_PID);
        #[rustfmt::skip]
        let cases = [
            (at(10, spawned), &gone, Some(&unhealthy), true, "the spawn's panic, 29 min before the close"),
            (at(-1, spawned), &gone, Some(&unhealthy), false, "dated before the window opened"),
            (at(30 * 60 + 1, spawned), &gone, Some(&unhealthy), false, "dated after the window closed"),
            (at(10, Some(1111)), &gone, Some(&unhealthy), false, "another process's pid, after the spawn"),
            (at(10, None), &gone, Some(&unhealthy), true, "no pid, dated after the spawn"),
            (at(4, None), &gone, Some(&unhealthy), false, "no pid, dated before the spawn"),
            (at(10, spawned), &unmatchable, Some(&unhealthy), false, "a pid the spawn cannot match"),
            (at(21, spawned), &gone, Some(&unhealthy), false, "the spawn's pid, dated after its exit stamp"),
            (at(21, None), &gone, Some(&unhealthy), false, "no pid, dated after the spawn's exit stamp"),
            (at(10, spawned), &live, Some(&ran), false, "a caught panic in a live runner that came up"),
            (at(10, spawned), &live, Some(&unhealthy), true, "a live runner that did not come up"),
            (at(10, spawned), &live, None, true, "a live runner with no result recorded"),
            (at(10, spawned), &gone, Some(&ran), true, "a runner that came up and exited on its own before the close"),
            (at(10, spawned), &gone_after_the_close, Some(&ran), false, "a runner that came up and exited after the close"),
            (at(10, spawned), &stopped, Some(&ran), false, "a runner that came up and a requested stop ended"),
            (at(10, spawned), &stopped, Some(&unhealthy), true, "dated before the stop request, the runner not up"),
            (at(10, spawned), &stopped, None, true, "dated before the stop request, no result recorded"),
            (at(15, spawned), &stopped, None, false, "dated at the stop request"),
            (at(16, spawned), &stopped, Some(&unhealthy), false, "a teardown panic, dated after the stop request"),
        ];
        for (panic, spawn, run, expected, case) in cases {
            assert_eq!(
                is_startup_panic_of(&panic, spawn, window, run),
                expected,
                "{case}"
            );
        }
    }

    /// The runner default path is read only for a primary under a supervisor
    /// with no `--log-dir`. Every other runner's panic log is where its spawn
    /// sent it, or there is none to read.
    #[test]
    fn only_a_primary_with_no_log_dir_reads_the_runner_default_panic_log() {
        let recorded = Path::new("/var/log/sup/runner-primary");
        assert_eq!(
            spawn_panic_log_path(Some(recorded), true, "primary", None),
            Some(recorded.join("runner-panic.log"))
        );
        assert_eq!(
            spawn_panic_log_path(None, true, "primary", None),
            Some(panic_log::resolve_panic_log_path(None))
        );
        let log_dir = Path::new("/var/log/sup");
        assert_eq!(
            spawn_panic_log_path(None, true, "primary", Some(log_dir)),
            Some(log_dir.join("runner-primary").join("runner-panic.log"))
        );
        // A non-primary's panic log is in its instance root whatever
        // `--log-dir` says, and with no root there is none to read.
        let temp = spawn_panic_log_path(None, false, "test-panic-dir", Some(log_dir));
        match crate::process::instance_config_dir("test-panic-dir") {
            Some(root) => assert_eq!(temp, Some(root.join("logs").join("runner-panic.log"))),
            None => assert_eq!(temp, None),
        }
        // No root on any box: an id with a path separator is refused before
        // any config-dir lookup. Not the runner default path, which only the
        // primary writes, and not a `--log-dir` subdir either.
        assert_eq!(
            spawn_panic_log_path(None, false, "bad/id", Some(log_dir)),
            None
        );
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
    fn a_panic_excerpt_is_its_first_line_cut_at_200_characters() {
        assert_eq!(
            panic_excerpt("  index out of bounds  \nsecond line"),
            "panic: index out of bounds"
        );
        assert_eq!(panic_excerpt(""), "panic: ");
        // A whole Debug dump on one line, multi-byte characters included: cut
        // by characters, so the excerpt stays valid UTF-8 and far inside
        // coord's 2 KiB evidence bound.
        let dump = format!("called `Result::unwrap()` on {}", "é".repeat(16 * 1024));
        let excerpt = panic_excerpt(&dump);
        assert_eq!(excerpt.chars().count(), "panic: ".len() + 200);
        assert!(excerpt.len() <= 2048);
        // A NUL in the first line (a `Display`-formatted, NUL-padded Win32
        // buffer, say) stays in the excerpt and reaches coord as U+FFFD: the
        // wire body replaces it, since coord cannot store a NUL in text.
        let excerpt = panic_excerpt("open C:\\runner\0\0 failed\nsecond line");
        assert_eq!(excerpt, "panic: open C:\\runner\0\0 failed");
        let outcome = ActionOutcome {
            category: D3Category::Failure,
            signatures: vec![DevSignature::DevPanicStartup.as_str().to_string()],
            ended_at: Utc::now(),
            duration_ms: 30_000,
            evidence_ref: Some(excerpt),
            late_signatures: Vec::new(),
        };
        let body = crate::dev_action::ingest::SnapshotIngestBody::from_record(
            &bare_record(ActionKind::Restart),
            &outcome,
            None,
            None,
        );
        let wire = serde_json::to_value(body).expect("serialize");
        assert_eq!(
            wire["evidence_ref"],
            "panic: open C:\\runner\u{FFFD}\u{FFFD} failed"
        );
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
