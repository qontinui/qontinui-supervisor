//! Delayed-effect attribution watcher.
//!
//! Dev actions emit effects on a delay (the 2026-06-07 asset error landed at
//! t+5s after a "restarted successfully" ACK), so a synchronous verdict is
//! impossible — the watcher holds the action's attribution window open until the
//! action records its own terminal result, plus a tail (capped by a per-kind
//! backstop: [`WindowTiming::for_kind`]), then scans the effect surfaces and
//! folds a verdict into the action's [`ActionOutcome`]:
//!
//! 1. the early log of each spawn this action initiated on the acted-on runner
//!    inside the window, for `DEV-TAURI-ASSET-MISSING` / `DEV-WEBVIEW-CONN-REFUSED` /
//!    `DEV-PORT-BIND-FAIL` signatures,
//! 2. each such spawn's startup panic for `DEV-PANIC-STARTUP` (see
//!    [`scan_panic`] for where it is read from and which panics count),
//! 3. the acted-on runner's cached `ui_error` / `recent_crash` /
//!    `derived_status` for `DEV-UI-ERROR-BOUNDARY` ([`scan_health`]).
//!
//! The first two describe one boot, so they are read per spawn, and only for
//! the spawns this action initiated on the acted-on runner inside the window
//! ([`scan_spawn_effects`]): an action that starts nothing inside its window
//! has no boot of its own to judge. The third is gated the same way: what of
//! it can be dated is held to the span from the action's earliest such spawn
//! to the close, a recent crash to the crash of one of those spawns, and the
//! undated self-report to an API seen answering after that spawn.
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
//! stopped through `POST /runners/{id}/stop` does too: no stop deletes an early
//! log, and the max-age sweep prunes one only once it is older than
//! [`LONGEST_BACKSTOP`] plus a margin
//! (`process::early_log::EARLY_LOG_RETENTION`). The panic log does not survive
//! cleanup: a `test-*` runner writes it inside its instance root, which a stop
//! reaps, and the runner's next panic overwrites it. So the watcher first takes
//! the boot panic spawn-test's failed probe attached to the spawn's entry, then
//! the panic the exit monitor attached to it, then the one the supervisor
//! stored on the runner, and only then reads the file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use qontinui_types::dev_signatures::DevSignature;
use uuid::Uuid;

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

// ── Per-kind attribution windows (Q6). Every kind's window stays open until ──
//    the action records its own terminal result, then adds a tail; the
//    backstop bounds a window whose result never arrives. Read from these
//    consts so they are tunable without a schema change.

/// The tail added after an action's terminal result, during which delayed
/// effects (the 2026-06-07 asset error landed at t+5s) still count: 30s.
pub const RESULT_TAIL_WINDOW: Duration = Duration::from_secs(30);

/// The backstop of a `spawn` and a `build` window: 2 h.
///
/// A spawn mints before its build (`mint_spawn_action` precedes the build in
/// both spawn routes) and a build mints at submission, so either window covers
/// queue wait (`queue_timeout_secs` defaults to unbounded), a cold build (20-50
/// min), spawn-test's 60 s probe and its 120 s wait. A rebuild restart uses the
/// same pool and the same cold-build time, and a backstop fires only when no
/// result arrives, so one constant serves both and loses nothing. It replaced a
/// 45-minute build backstop, which could be shorter than a cold build.
pub const LONG_BACKSTOP_WINDOW: Duration = Duration::from_secs(2 * 60 * 60);

/// The backstop of a `restart` window: 10 min.
///
/// A synchronous restart records `runner_unhealthy_after_start` only after the
/// readiness gate (15 s), the stop (a drain of up to 30 s, a 0.5 s close
/// request, a 30 s handoff wait, about 24 s of reap), the start and the 30 s
/// health wait: about 160 s before the start's exe copy with the tail, and about
/// 330 s for a restart queued behind two stops under the lifecycle lock. The
/// window closes on the result, so a longer backstop costs only a dead
/// handler's row.
pub const RESTART_BACKSTOP_WINDOW: Duration = Duration::from_secs(10 * 60);

/// The longest per-kind backstop: how long after a spawn a window can still be
/// reading that spawn's evidence when nothing records a result. A caller's
/// backstop override (`rebuild_when_idle`'s deadline) can be longer, but that
/// path records a result on every exit, and its spawn comes after its wait, so
/// its window closes a tail after the result. Sizes anything that must keep a
/// spawn's evidence until its window has closed.
pub const LONGEST_BACKSTOP: Duration = LONG_BACKSTOP_WINDOW;
const _: () = assert!(LONGEST_BACKSTOP.as_secs() >= RESTART_BACKSTOP_WINDOW.as_secs());

/// The two timing knobs of an attribution window, injectable so the
/// regression tests can exercise the real waiting logic in milliseconds rather
/// than minutes. Production takes [`WindowTiming::for_kind`], with the mint's
/// optional backstop override ([`WindowTiming::with_backstop_override`]).
#[derive(Debug, Clone, Copy)]
pub struct WindowTiming {
    /// Grace period added AFTER the action's terminal result, during which
    /// delayed effects (the 2026-06-07 asset error landed at t+5s) still count.
    pub tail: Duration,
    /// Upper bound on waiting for a terminal result that never arrives.
    pub backstop: Duration,
}

impl WindowTiming {
    /// The production timing of a `kind` window: [`RESULT_TAIL_WINDOW`], and
    /// [`LONG_BACKSTOP_WINDOW`] for a spawn or a build or
    /// [`RESTART_BACKSTOP_WINDOW`] for a restart.
    pub fn for_kind(kind: ActionKind) -> Self {
        let backstop = match kind {
            ActionKind::Spawn | ActionKind::Build => LONG_BACKSTOP_WINDOW,
            ActionKind::Restart => RESTART_BACKSTOP_WINDOW,
        };
        Self {
            tail: RESULT_TAIL_WINDOW,
            backstop,
        }
    }

    /// This timing with its backstop replaced by `backstop`, when the mint
    /// supplied one: a path whose action can legitimately outlast the kind's
    /// backstop (`rebuild_when_idle` waits up to its `deadline_secs` before it
    /// even builds).
    pub fn with_backstop_override(self, backstop: Option<Duration>) -> Self {
        Self {
            backstop: backstop.unwrap_or(self.backstop),
            ..self
        }
    }
}

/// The evidence a window carries when it closed on its backstop with no
/// terminal result and no signature (D8).
pub const BACKSTOP_CLOSED_NO_RESULT: &str = "backstop_closed_no_result";

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
/// with no run result (a window closed on its backstop) or one other than
/// `Ran`, a teardown panic is turned away once the exit its stop asked for is
/// stamped by the close, and a panic of a runner the action started that is
/// dated before any stop request counts while one of the four places the
/// watcher reads ([`scan_panic`]) still holds it at the close.
///
/// `Ran` is what a start records once its runner has answered a health check:
/// the restart routes' wait, spawn-test's probe or wait, spawn-named's wait,
/// or, when the route answered without one, the detached health wait
/// (`dev_action::run_result::record_start_verdict`). So `Ran` never stands for
/// a boot whose API never bound.
///
/// Residuals, documented and not changed:
/// - A stop requested but not finished at the close leaves the entry with no
///   request time yet, so with no run result or one other than `Ran` a
///   teardown panic logged between that request and the close counts.
/// - A stop that fails leaves the flag latched, so a later crash of that
///   process reads as requested, as the crash watchdog already reads it.
/// - The exit stamp becomes visible only right after the exit monitor's first
///   critical section after its wait on the child, so a process whose exit
///   falls in that short lag before the close reads as up, exactly like an
///   exit just after the close, and under `Ran` its panic does not count.
/// - Three of the four places the watcher reads hold only the runner's
///   latest panic, and a stop can empty them before the close: a teardown
///   panic overwrites `runner-panic.log`, and on an exit that is not clean
///   (a non-zero exit, or a wait on the child that returned no exit status,
///   the only exits on which the exit monitor parses the log) the stored and
///   exit-attached copies too, and check 4 then turns it away; a plain stop of
///   a `test-*` runner reaps its instance root, `runner-panic.log` included,
///   and a clean exit attaches and stores nothing. The fourth, the boot panic
///   spawn-test's failed probe attached to the spawn's entry
///   ([`SpawnMarker::boot_panic`]), is the earliest one the probe stored and
///   survives all of that. A panic the spawn logged before the request on a
///   path with no failed probe has no such copy, so it counts only if one of
///   the other three still holds it at the close: a caught boot panic followed
///   by a teardown panic stays unrecoverable without a runner-side change to
///   keep more than one panic.
/// - The lifecycle lock (`ManagedRunner::lifecycle`) orders one runner's starts,
///   stops and removals, so a stop arriving mid-start latches after the
///   spawn is recorded and targets it, and a second start finds it recorded. The
///   exit monitor waits outside that lock, which leaves two lags in the stamp
///   this rule reads (`SpawnMarker::stop_requested_at`). A stop latched between
///   the process's real exit and the monitor's wake-up dates before `exited_at`,
///   so a crash in that lag reads as a requested stop and, under `Ran`, its panic
///   does not count. A monitor that wakes after a later start recorded its spawn
///   reads that start's cleared flag, so the requested exit it stamps reads as
///   unrequested: the stopped spawn reads as a failed boot and its in-window
///   panics count, under `Ran` too. These are read from the code; none was
///   reproduced on a running supervisor.
fn is_startup_panic_of(
    panic: &panic_log::RecentPanic,
    spawn: &SpawnMarker,
    window: ClosedWindow,
    run: Option<&ActionRunResult>,
) -> bool {
    let exited_on_its_own = spawn.stop_requested_at.is_none()
        && spawn
            .exited_at
            .is_some_and(|exited| exited <= window.close_at);
    let came_up = matches!(run, Some(ActionRunResult::Ran));
    let before_any_stop = spawn
        .stop_requested_at
        .is_none_or(|requested| panic.timestamp < requested);
    window.contains(panic.timestamp)
        && is_panic_of_spawn(panic, spawn)
        && (exited_on_its_own || !came_up)
        && before_any_stop
}

/// Check 2 of [`is_startup_panic_of`]: `panic` was logged by `spawn`'s own
/// process. The header's pid is the spawn's pid, or, with no pid in the header,
/// the panic is dated at or after the spawn; and when the spawn's exit is
/// stamped, the panic is dated at or before it (Windows reuses pids). The pid,
/// not a date, does the work: the spawn's `spawned_at` is read after the child
/// was spawned, so a very early panic can be dated before it.
///
/// spawn-test's failed probe holds the panic it stores to this rule against the
/// runner's newest spawn (`routes::runners::probe_runner_health`), so a boot
/// panic is kept however long the probe ran, and an older process's panic
/// still in the file is not: the runner rewrites `runner-panic.log` on every
/// panic and never truncates it at start.
pub(crate) fn is_panic_of_spawn(panic: &panic_log::RecentPanic, spawn: &SpawnMarker) -> bool {
    let its_pid = match panic.pid {
        Some(pid) => spawn.pid == Some(pid),
        None => panic.timestamp >= spawn.spawned_at,
    };
    let before_its_exit = spawn
        .exited_at
        .is_none_or(|exited| panic.timestamp <= exited);
    its_pid && before_its_exit
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
/// 1. the boot panic spawn-test's failed health probe stored for this spawn
///    and attached to its ring entry ([`SpawnMarker::boot_panic`]). The stop
///    that follows the probe can end in a teardown panic that overwrites the
///    file and, on an exit that is not clean, the next two places too, so this
///    is the copy of a probe-caught boot panic that survives to the close;
/// 2. the panic the exit monitor attached to this spawn's ring entry when its
///    process exited ([`SpawnMarker::exit_panic`]). The next spawn clears the
///    stored panic and the runner's next panic, a caught one included,
///    overwrites the file, so for a boot the crash watchdog restarted this is
///    the copy that survives to the close;
/// 3. the panic the supervisor parsed and stored on the runner
///    (`recent_panic`). spawn-test's probe-failure path stores it and then
///    stops the runner, and that stop reaps a `test-*` runner's instance root,
///    `runner-panic.log` included, possibly before the window closes;
/// 4. the panic log the spawn set up ([`spawn_panic_log_path`]), read at
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
/// Places 2 to 4 each hold only the runner's latest panic, so a teardown panic
/// of a spawn a stop ended replaces a panic that spawn logged before the
/// request in the file, and on an exit that is not clean (a non-zero exit, or
/// a wait on the child that returned no exit status, the only exits on which
/// the exit monitor parses the log) in the stored and exit-attached copies
/// too, and is itself turned away (see the residuals of
/// [`is_startup_panic_of`]). Place 1 keeps the earliest panic the probe
/// stored, so a boot panic the probe caught survives that.
async fn scan_panic(
    state: &SharedState,
    managed: &ManagedRunner,
    spawn: &SpawnMarker,
    window: ClosedWindow,
    run: Option<&ActionRunResult>,
) -> (Vec<DevSignature>, Option<String>) {
    let counts = |panic: &panic_log::RecentPanic| is_startup_panic_of(panic, spawn, window, run);

    let mut panic = spawn.boot_panic.clone().filter(|panic| counts(panic));
    if panic.is_none() {
        panic = spawn.exit_panic.clone().filter(|panic| counts(panic));
    }
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

/// The early-log and startup-panic signatures of every spawn `action_id`
/// initiated on the acted-on runner inside the window, read at window close,
/// oldest spawn first.
///
/// Both surfaces describe one boot, so they are read per spawn: each spawn's
/// own early log and its own startup panic ([`scan_panic`]). Only the entries of
/// the runner's ring ([`ManagedRunner::recent_spawns`]) this action initiated
/// ([`SpawnMarker::initiated_by`]) are judged. A crash-watchdog respawn or
/// another action's spawn inside the window is not this action's boot, and
/// neither is read. The action's own entry still carries its crashed boot, with
/// the exit and panic stamped on it, so a respawn cannot hide that boot. A later
/// restart or stop of the runner inside the window ends a spawn too, and that
/// requested exit does not make a boot that came up a failed one, the residuals
/// [`is_startup_panic_of`] lists excepted.
///
/// An action that initiated no spawn inside its window reads neither surface:
/// `fix_and_rebuild`, which only builds; a refused restart or rebuild, or one
/// whose build failed; a spawn that lands after its window has closed. What
/// those files hold then describes a boot the action did not cause. With no
/// managed runner there is no spawn to read.
///
/// An explicit `POST /runners/{id}/stop` no longer deletes the stopped
/// runner's early-log file, so a window still open over that spawn reads its
/// early-log signatures at the close. The max-age sweep prunes an early log
/// only once it is older than [`LONGEST_BACKSTOP`] plus a margin
/// (`process::early_log::EARLY_LOG_RETENTION`).
async fn scan_spawn_effects(
    state: &SharedState,
    managed: Option<&ManagedRunner>,
    action_id: Uuid,
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
        .spawned_between(window.open_at, window.close_at)
        .into_iter()
        .filter(|spawn| spawn.initiated_by == Some(action_id));
    let (mut signatures, mut evidence) = (Vec::new(), None);
    for spawn in spawns {
        // Early logs are per-spawn files: this is the one the spawn opened.
        // `None` when it could not open one: that boot then has no early log,
        // and no other boot's file is read in its place.
        let (log_sigs, log_ev) = scan_early_log(spawn.early_log_path.as_deref());
        merge(&mut signatures, &mut evidence, log_sigs, log_ev);
        let (panic_sigs, panic_ev) = scan_panic(state, managed, &spawn, window, run).await;
        merge(&mut signatures, &mut evidence, panic_sigs, panic_ev);
    }
    (signatures, evidence)
}

/// The `DEV-UI-ERROR-BOUNDARY` signature of the acted-on runner's cached health,
/// read only for an action that initiated a spawn of it inside the window, as
/// [`scan_spawn_effects`] reads its early logs and panics.
///
/// A build-only `fix_and_rebuild`, a refused restart, a failed build and a
/// `rebuild_when_idle` still waiting start nothing, so the runner's current
/// `ui_error` describes a boot they did not perform, and none is read for them.
///
/// - A `ui_error` counts only if its `first_seen` lies in the span from the
///   action's earliest in-window spawn to the close.
/// - A `recent_crash` counts only if it is the crash of one of the action's
///   own in-window spawns ([`is_crash_of_spawn`]) dated at or before the
///   close. A runner reports the dump it found at startup, so the reporter is
///   always a later process than the one that crashed, and a date in the span
///   alone can describe a crash of a spawn the action did not make (a
///   watchdog's respawn, another action's start).
/// - `derived_status` is the supervisor's own fold of those two
///   (`health_cache::derive_runner_status` returns `Errored` for either), so
///   its `Errored` arm counts only when both are absent: otherwise a signal
///   dated out of the span would fall through to it and still emit the
///   signature. With both absent the `Errored` is the runner's own undated
///   self-report, and it counts only when the snapshot shows the API
///   answering at a time after the action's earliest spawn
///   (`api_responding` with `last_seen_responding_at` past that spawn):
///   otherwise nothing places the report after the action's boot and the arm
///   is inconclusive, so it emits nothing.
///
/// **Residual:** an undated `Errored` that does pass that check is still read
/// as this action's even if the runner, answering after the spawn, carried
/// over a status about an earlier boot. Only the API answering after the
/// spawn is proven, not the report's own date.
async fn scan_health(
    state: &SharedState,
    managed: Option<&ManagedRunner>,
    runner_id: Option<&str>,
    action_id: Uuid,
    window: ClosedWindow,
) -> (Vec<DevSignature>, Option<String>) {
    let (Some(managed), Some(runner_id)) = (managed, runner_id) else {
        return (Vec::new(), None);
    };
    let (initiated, ring_to_close) = {
        let ring = managed.recent_spawns.read().await;
        let initiated: Vec<SpawnMarker> = ring
            .spawned_between(window.open_at, window.close_at)
            .into_iter()
            .filter(|spawn| spawn.initiated_by == Some(action_id))
            .collect();
        (
            initiated,
            ring.spawned_between(DateTime::<Utc>::MIN_UTC, window.close_at),
        )
    };
    let Some(earliest_spawn) = initiated.iter().map(|spawn| spawn.spawned_at).min() else {
        return (Vec::new(), None);
    };
    let in_span = |at: DateTime<Utc>| earliest_spawn <= at && at <= window.close_at;

    let snapshots = state.cached_runner_health.read().await;
    let Some(snap) = snapshots.iter().find(|snap| snap.id == runner_id) else {
        return (Vec::new(), None);
    };
    if let Some(ui_err) = snap.ui_error.as_ref() {
        if in_span(ui_err.first_seen) {
            return (
                vec![DevSignature::DevUiErrorBoundary],
                Some(format!(
                    "ui_error: {}",
                    ui_err.message.chars().take(160).collect::<String>()
                )),
            );
        }
    }
    if let Some(crash) = snap.recent_crash.as_ref() {
        let its_spawns_crash = crash.reported_at <= window.close_at
            && initiated
                .iter()
                .any(|spawn| is_crash_of_spawn(crash, spawn, &ring_to_close));
        if its_spawns_crash {
            let what = crash
                .panic_message
                .as_deref()
                .unwrap_or(crash.file_path.as_str());
            return (
                vec![DevSignature::DevUiErrorBoundary],
                Some(format!(
                    "recent_crash: {}",
                    what.chars().take(160).collect::<String>()
                )),
            );
        }
    }
    let undated_errored = snap.ui_error.is_none()
        && snap.recent_crash.is_none()
        && matches!(
            snap.derived_status,
            crate::health_cache::RunnerStatus::Errored { .. }
        );
    let api_answered_after_spawn = snap.api_responding
        && snap
            .last_seen_responding_at
            .is_some_and(|seen| seen > earliest_spawn);
    if undated_errored && api_answered_after_spawn {
        return (
            vec![DevSignature::DevUiErrorBoundary],
            Some("derived_status=errored".to_string()),
        );
    }
    (Vec::new(), None)
}

/// Whether `crash`, the dump a runner reported on its `/health`, is the crash
/// of `spawn`, one of the acted-on runner's spawns. `ring` is the runner's
/// spawn ring up to the window's close. All three must hold:
///
/// 1. **Dated after the spawn**: `reported_at > spawned_at`. A dump dated
///    before the spawn is an older process's.
/// 2. **Dated at or before the spawn's exit**, when that exit is stamped
///    (Windows reuses pids).
/// 3. **The spawn's pid.** With a `pid` on the dump (the runner emits it from
///    plan Phase 6), it equals the spawn's ring-entry pid. With none, the
///    interim rule: the ring entry that was the runner's newest spawn when the
///    dump was written, the process that was running then, is this spawn's,
///    entry and pid both, so a crash of a later spawn (a crash watchdog's
///    respawn, another action's start) dated inside the window is not read as
///    this one's.
///
/// The same discipline as [`is_panic_of_spawn`]: the pid, not the date, ties a
/// crash to a process.
pub(crate) fn is_crash_of_spawn(
    crash: &crate::health_cache::RecentCrashSummary,
    spawn: &SpawnMarker,
    ring: &[SpawnMarker],
) -> bool {
    let after_its_spawn = crash.reported_at > spawn.spawned_at;
    let before_its_exit = spawn
        .exited_at
        .is_none_or(|exited| crash.reported_at <= exited);
    let its_pid = match crash.pid {
        Some(pid) => spawn.pid == Some(pid),
        None => ring
            .iter()
            .filter(|entry| entry.spawned_at < crash.reported_at)
            .max_by_key(|entry| entry.spawned_at)
            .is_some_and(|running| {
                running.spawned_at == spawn.spawned_at && running.pid == spawn.pid
            }),
    };
    after_its_spawn && before_its_exit && its_pid
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
/// **This is the framing fix** (plan §6 Phase 2; plan
/// `2026-10-29-supervisor-spawn-attribution-window-and-hermetic-dev-action-coord`
/// D1, D6). A flat 30s window from the mint used to be the window for every
/// rebuild-restart, every spawn and every restart, so a build that fails at
/// t+12min, a spawn whose build and boot take longer than 30s, and a restart
/// whose stop and health wait outlast 30s all closed `Confirmed` before their
/// result existed, and the ledger's never-re-open rule made that permanent.
/// Now every kind holds the window open until the action records its own
/// terminal result, then adds [`WindowTiming::tail`] so delayed effects still
/// count, still clamped by [`WindowTiming::backstop`]. A path that never records
/// a result closes on the backstop instead of parking forever.
///
/// **[`ActionRunResult::Refused`] closes the window immediately.** A refused
/// action never ran, so no effect surface was touched and there is nothing left
/// for the window to observe; waiting out the tail would only delay a verdict
/// that is already known.
///
/// The watcher only ever READS the result, so the never-re-open invariant
/// (module docs; the ledger plan §3.2) is untouched: the verdict is still
/// composed exactly once, at window close.
async fn await_attribution_window(
    record: &ActionRecord,
    timing: WindowTiming,
) -> Option<ActionRunResult> {
    match wait_for_run_result(record, timing.backstop).await {
        // Nothing ran — close now.
        Some(r @ ActionRunResult::Refused { .. }) => Some(r),
        Some(other) => {
            // Tail measured from the action's REAL end, and still clamped by
            // the backstop so the window is bounded either way.
            // Both ends on the tokio clock the backstop deadline runs on.
            let room = timing
                .backstop
                .saturating_sub(record.started_instant.elapsed());
            tokio::time::sleep(timing.tail.min(room)).await;
            Some(other)
        }
        None => record.run_result(),
    }
}

/// The verdict of a closed window: [`classify`], except that a window that
/// closed on its backstop with no terminal result and no signature is
/// [`D3Category::Partial`], not `Confirmed` (D8). Every minting path records a
/// result on every exit, a dropped future included (`RunResultGuard`), so a
/// window that reaches its backstop has no evidence of what happened, and an
/// unknown must not render as a success. A signature the window did observe
/// is evidence, and keeps its own category.
pub fn classify_close(run: Option<&ActionRunResult>, signatures: &[DevSignature]) -> D3Category {
    match classify(run, signatures) {
        D3Category::Confirmed if run.is_none() => D3Category::Partial,
        category => category,
    }
}

/// Spawn the detached attribution watcher for a minted action.
///
/// Holds the window open per [`await_attribution_window`] on the kind's
/// production timing ([`WindowTiming::for_kind`]), its backstop replaced by
/// `backstop_override` when the mint supplies one, scans the effect surfaces,
/// folds the verdict into `record.outcome`, and logs the verdict to the
/// supervisor log buffer. Triggers `notify_health_change()` so connected
/// dashboards refresh (Phase 1 has no dedicated `events.*` WS channel — that
/// arrives with coord in Phase 3; see the plan's §6.1 watcher note).
/// Best-effort throughout: any scan failure degrades to "no signal", never a
/// panic.
pub fn spawn_attribution_watcher(
    state: SharedState,
    record: Arc<ActionRecord>,
    targets: AttributionTargets,
    backstop_override: Option<Duration>,
) {
    let timing = WindowTiming::for_kind(record.kind).with_backstop_override(backstop_override);
    spawn_attribution_watcher_with_timing(state, record, targets, timing);
}

/// [`spawn_attribution_watcher`] with the window timing supplied explicitly.
/// Production uses [`WindowTiming::for_kind`]; the regression tests use
/// millisecond values so they exercise the real waiting logic without a
/// 30-second sleep.
pub fn spawn_attribution_watcher_with_timing(
    state: SharedState,
    record: Arc<ActionRecord>,
    targets: AttributionTargets,
    timing: WindowTiming,
) {
    tokio::spawn(async move {
        let kind = record.kind;

        let run_result = await_attribution_window(&record, timing).await;
        let window = ClosedWindow {
            open_at: record.started_at,
            close_at: Utc::now(),
        };

        // Gather signatures from every effect surface.
        let (mut signatures, mut evidence) = scan_spawn_effects(
            &state,
            targets.managed.as_deref(),
            record.action_id,
            window,
            run_result.as_ref(),
        )
        .await;
        let (health_sigs, health_ev) = scan_health(
            &state,
            targets.managed.as_deref(),
            targets.runner_id.as_deref(),
            record.action_id,
            window,
        )
        .await;
        merge(&mut signatures, &mut evidence, health_sigs, health_ev);

        let category = classify_close(run_result.as_ref(), &signatures);
        let ended_at = Utc::now();
        let duration_ms = (ended_at - record.started_at).num_milliseconds();

        // Plan §5: the refusal/error distinction rides `evidence_ref`, not a
        // sixth `D3Category`. A `DEV-*` signature's own excerpt wins when there
        // is one (it names an observed effect); otherwise the action's terminal
        // reason is the only evidence there is — and for a refusal it is the
        // ONLY evidence there ever could be, since nothing ran.
        if evidence.is_none() {
            evidence = match run_result.as_ref() {
                Some(r) => r.reason().map(|r| r.chars().take(400).collect::<String>()),
                // D8: a backstop close with no result says so.
                None => Some(BACKSTOP_CLOSED_NO_RESULT.to_string()),
            };
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
        // reads for the device id), and only under `AmbientCoord::Read`: an
        // `At(<stub>)` coord gets the snapshot with no identity, so a test never
        // sends this machine's device id to a planted coord.
        let access = &state.config.ambient_coord;
        if let Some(base) = crate::coord_client::coord_base(access) {
            let (device_id, tenant_id) = snapshot_identity(
                access,
                crate::dev_action::ingest::resolve_device_id,
                crate::dev_action::ingest::resolve_tenant_id,
            );
            let ingest_record = Arc::clone(&record);
            tokio::spawn(async move {
                crate::dev_action::post_snapshot_to_coord(
                    &base,
                    &ingest_record,
                    &outcome,
                    device_id,
                    tenant_id,
                )
                .await;
            });
        } else {
            tracing::debug!(
                "dev_action::attribution: no coord base — skipping snapshot {} persistence.",
                record.action_id
            );
        }

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

/// The device and tenant ids a dev-action snapshot carries to coord: the
/// machine's own, read through `device` / `tenant`, under `AmbientCoord::Read`
/// only. Any other access sends none, and never calls either reader, so a test's
/// planted coord never receives this machine's identity.
fn snapshot_identity(
    access: &crate::config::AmbientCoord,
    device: impl FnOnce() -> Option<uuid::Uuid>,
    tenant: impl FnOnce() -> Option<uuid::Uuid>,
) -> (Option<uuid::Uuid>, Option<uuid::Uuid>) {
    if matches!(access, crate::config::AmbientCoord::Read) {
        (device(), tenant())
    } else {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Regression tests for
    //    `2026-09-04-supervisor-refused-restart-reports-confirmed` ──────────

    /// Same minimal `SupervisorState` shape the route tests use.
    fn watcher_test_state(root: &std::path::Path) -> SharedState {
        Arc::new(crate::state::SupervisorState::new(watcher_test_config(
            root,
        )))
    }

    fn watcher_test_config(root: &std::path::Path) -> crate::config::SupervisorConfig {
        use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
        SupervisorConfig {
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
            ambient_coord: crate::config::AmbientCoord::Ignore,
        }
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

    /// Record a spawn of `runner` that `record` initiated, at `at`, child `pid`,
    /// panic log under `panic_log_dir`, with no early log of its own
    /// ([`record_spawn_with_early_log`]).
    async fn record_spawn(
        runner: &ManagedRunner,
        record: &ActionRecord,
        at: DateTime<Utc>,
        pid: u32,
        panic_log_dir: &Path,
    ) {
        record_spawn_with_early_log(runner, Some(record.action_id), at, pid, panic_log_dir, None)
            .await;
    }

    /// Record a spawn of `runner` at `at`, child `pid`, that `initiated_by`
    /// asked for, as `start_managed_runner` leaves the runner: the panic-log
    /// dir re-pointed to `panic_log_dir`, the stored panic cleared, the early
    /// log re-pointed to `early_log`, the live process, and the spawn's entry
    /// in the ring.
    /// The process stays live until a test records its exit
    /// ([`record_exit`]).
    async fn record_spawn_with_early_log(
        runner: &ManagedRunner,
        initiated_by: Option<Uuid>,
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
        runner.recent_spawns.write().await.push(SpawnMarker::new(
            at,
            Some(pid),
            early_log,
            initiated_by,
        ));
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
        let spawned_at = runner
            .recent_spawns
            .read()
            .await
            .iter()
            .find(|spawn| spawn.pid == Some(pid) && spawn.exited_at.is_none())
            .unwrap_or_else(|| panic!("no spawn of pid {pid} is waiting for its exit stamp"))
            .spawned_at;
        assert!(runner
            .recent_spawns
            .write()
            .await
            .stamp_exit(spawned_at, at, requested_at));
        if let Some(panic) = exit_panic {
            *runner.recent_panic.write().await = Some(panic.clone());
            assert!(
                runner
                    .recent_spawns
                    .write()
                    .await
                    .attach_exit_panic(spawned_at, panic),
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
    /// what keeps this test sub-second against the restart's 10-minute
    /// backstop.
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

        // Well inside the restart tail: the refusal closes the window early.
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

    /// The positive control for the switch: a unit-test build posts when its
    /// config names a coord, so the config governs the write and no compile flag
    /// does. A refusal closes the window at once.
    #[tokio::test]
    async fn a_completed_snapshot_posts_to_the_configured_coord() {
        use crate::dev_action::test_support::serve_planted_expectations;
        let coord = serve_planted_expectations(serde_json::json!([])).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut config = watcher_test_config(tmp.path());
        config.ambient_coord = crate::config::AmbientCoord::At(coord.base.clone());
        let state = Arc::new(crate::state::SupervisorState::new(config));
        let record = bare_record(ActionKind::Restart);

        spawn_attribution_watcher(state, record.clone(), AttributionTargets::default(), None);
        record.record_run_result(ActionRunResult::Refused {
            reason: "restart_refused_unsafe: a terminal-hosted agent session is live".to_string(),
        });

        let budget = crate::test_clock::poll_budget(Duration::from_secs(30));
        let deadline = std::time::Instant::now() + budget;
        let seen = loop {
            let seen = coord.seen();
            if !seen.is_empty() {
                break seen;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the watcher posted nothing to the configured coord within {budget:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].method, axum::http::Method::POST);
        assert_eq!(seen[0].path, "/coord/dev-actions/snapshot");
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("a JSON body");
        assert_eq!(body["action_id"], record.action_id.to_string());
    }

    /// Plan §6 Phase 5 test 4 — pins Phase 2, the framing fix. A build whose
    /// terminal result lands AFTER the old flat window would have elapsed must
    /// still be judged on that result.
    ///
    /// `tail` here plays the role the flat 30s played in production: under the
    /// old code a build's window with no known duration was exactly the tail
    /// and closed `Confirmed` before the failure existed. The failure is recorded
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

        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets::default(),
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
    /// would park forever on any path that forgets to report. Every build mint
    /// site records a result on every exit now (a `RunResultGuard` covers a
    /// dropped one), so such a close has no evidence of what happened: it is
    /// `Partial`, not `Confirmed` (D8).
    #[tokio::test]
    async fn build_with_no_terminal_result_closes_on_the_backstop() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Build);

        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets::default(),
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
        // No terminal result and no signatures: nothing says what happened.
        assert_eq!(outcome.category, D3Category::Partial);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some(BACKSTOP_CLOSED_NO_RESULT)
        );
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
            &record,
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
        record_spawn(
            &runner,
            &record,
            spawned_after_the_mint(&record),
            SPAWNED_PID,
            &logs,
        )
        .await;
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
            &record,
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
            &record,
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
        record_spawn(&primary, &record, new_spawn_at, SPAWNED_PID, &log_dir).await;
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
            Some(record.action_id),
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
                &record,
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
            &record,
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
    /// and the crash watchdog's respawn (5 s later in production, inside the
    /// action's window) clears the stored panic, comes up and is still
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
            &record,
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
        // The watchdog's respawn: no action initiated it.
        record_spawn_with_early_log(&primary, None, respawned_at, RESPAWNED_PID, &log_dir, None)
            .await;
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

    /// The crash watchdog's respawn, live and up at the close, does not hide
    /// the action's own boot, which panicked and exited inside the window.
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
            &record,
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
            &record,
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
            &record,
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
            &record,
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
    /// as a dead executing task records none: it closes on the backstop. The
    /// panic clauses read only the run result, not the kind.
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
            &record,
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
            &record,
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
        // No signature and no result: a backstop close says nothing (D8).
        assert_eq!(outcome.category, D3Category::Partial);
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
            &record,
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

    /// D13: only the entries this action initiated are its boots. A spawn nobody
    /// initiated (a watchdog's respawn) and another action's spawn, each with a
    /// white screen in its early log and inside this action's window, are not
    /// read. The action's own crashed entry still is.
    #[tokio::test]
    async fn a_spawn_this_action_did_not_initiate_is_not_read_as_its_boot() {
        for own_boot_crashes in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let state = watcher_test_state(tmp.path());
            let primary = test_primary();
            let record = bare_record(ActionKind::Build);
            watch_on_a_fast_build_window(state, &record, &primary);

            let log_dir = tmp.path().join("dev-logs").join("runner-primary");
            let white = tmp.path().join("white-early.log");
            std::fs::write(&white, format!("{WHITE_SCREEN_LINE}\n")).unwrap();

            record_spawn(
                &primary,
                &record,
                spawned_after_the_mint(&record),
                SPAWNED_PID,
                &log_dir,
            )
            .await;
            let exited_at = inside_the_window(&record);
            if own_boot_crashes {
                let crash = write_panic_log(
                    &log_dir,
                    "primary",
                    SPAWNED_PID,
                    exited_at,
                    "the action's own boot panicked",
                );
                let crash = panic_log::parse_panic_file(&crash).expect("the fixture parses");
                record_exit(&primary, SPAWNED_PID, exited_at, Some(crash)).await;
            }
            for (at_ms, pid, initiated_by) in [(1, 4343, None), (2, 4444, Some(Uuid::new_v4()))] {
                record_spawn_with_early_log(
                    &primary,
                    initiated_by,
                    exited_at + chrono::Duration::milliseconds(at_ms),
                    pid,
                    &log_dir,
                    Some(white.clone()),
                )
                .await;
            }
            record.record_run_result(ActionRunResult::Ran);

            let outcome = await_outcome(
                &record,
                crate::test_clock::poll_budget(Duration::from_secs(30)),
            )
            .await;
            assert!(
                !outcome
                    .signatures
                    .iter()
                    .any(|signature| signature == "DEV-TAURI-ASSET-MISSING"),
                "own_boot_crashes={own_boot_crashes}: another spawn's early log was read: \
                 {outcome:?}"
            );
            if own_boot_crashes {
                assert_eq!(outcome.signatures, ["DEV-PANIC-STARTUP"]);
                assert_eq!(
                    outcome.evidence_ref.as_deref(),
                    Some("panic: the action's own boot panicked")
                );
            } else {
                assert_eq!(outcome.signatures, Vec::<String>::new());
                assert_eq!(outcome.category, D3Category::Confirmed);
            }
        }
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
            Some(record.action_id),
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
            Some(record.action_id),
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
        let live = SpawnMarker::new(
            open + chrono::Duration::seconds(5),
            Some(SPAWNED_PID),
            None,
            None,
        );
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

    /// Every kind holds its window open for its result; only the backstop
    /// differs, and an override replaces it.
    #[test]
    fn window_timing_is_per_kind_and_the_override_replaces_the_backstop() {
        for kind in [ActionKind::Spawn, ActionKind::Build, ActionKind::Restart] {
            assert_eq!(WindowTiming::for_kind(kind).tail, Duration::from_secs(30));
        }
        assert_eq!(
            WindowTiming::for_kind(ActionKind::Spawn).backstop,
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            WindowTiming::for_kind(ActionKind::Build).backstop,
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            WindowTiming::for_kind(ActionKind::Restart).backstop,
            Duration::from_secs(10 * 60)
        );
        let day = Duration::from_secs(24 * 60 * 60);
        assert_eq!(
            WindowTiming::for_kind(ActionKind::Build)
                .with_backstop_override(Some(day))
                .backstop,
            day
        );
        assert_eq!(
            WindowTiming::for_kind(ActionKind::Build)
                .with_backstop_override(None)
                .backstop,
            LONG_BACKSTOP_WINDOW
        );
    }

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

    // ── Plan `2026-10-29-supervisor-spawn-attribution-window-and-hermetic-
    //    dev-action-coord` Phase 2 (PR-B) ──

    /// Whether the watcher has folded an outcome into `record` yet.
    fn outcome_of(record: &ActionRecord) -> Option<ActionOutcome> {
        record.outcome.read().ok().and_then(|g| g.clone())
    }

    /// T1-1 (D1). A spawn's window, on production timing, stays open past the
    /// flat 30 s it used to close at, until the spawn records its result.
    #[tokio::test(start_paused = true)]
    async fn a_spawn_window_stays_open_past_thirty_seconds_until_the_run_result() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Spawn);
        spawn_attribution_watcher(state, record.clone(), AttributionTargets::default(), None);

        tokio::time::sleep(Duration::from_secs(31)).await;
        assert!(
            outcome_of(&record).is_none(),
            "the spawn window closed before the spawn recorded its result"
        );

        record.record_run_result(ActionRunResult::Errored {
            reason: "runner_died_during_startup: the spawned runner exited".to_string(),
        });
        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert_eq!(outcome.category, D3Category::Failure);
        assert!(
            outcome
                .evidence_ref
                .as_deref()
                .is_some_and(|e| e.contains("runner_died_during_startup")),
            "{:?}",
            outcome.evidence_ref
        );
    }

    /// T1-2 (D1, D8). A spawn that records no result is still open past the
    /// old 45-minute build backstop, closes on the 2-hour spawn backstop, and
    /// says it closed with no result rather than `Confirmed`. Literals, not the
    /// constant, so changing the constant turns this red.
    #[tokio::test(start_paused = true)]
    async fn a_spawn_with_no_terminal_result_closes_on_the_spawn_backstop() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Spawn);
        spawn_attribution_watcher(state, record.clone(), AttributionTargets::default(), None);

        let past_the_old_backstop = Duration::from_secs(45 * 60 + 1);
        tokio::time::sleep(past_the_old_backstop).await;
        assert!(
            outcome_of(&record).is_none(),
            "the spawn window closed on the old 45-minute build backstop"
        );

        let past_the_spawn_backstop = Duration::from_secs(2 * 60 * 60) + Duration::from_secs(30);
        tokio::time::sleep(past_the_spawn_backstop - past_the_old_backstop).await;
        let outcome = outcome_of(&record).expect("the spawn window outlived its 2-hour backstop");
        assert!(record.run_result().is_none());
        assert_eq!(outcome.category, D3Category::Partial);
        assert_eq!(
            outcome.evidence_ref.as_deref(),
            Some("backstop_closed_no_result")
        );
    }

    /// T1-11 (D6). A restart's window waits for the
    /// `runner_unhealthy_after_start` its route records after its stop, start
    /// and 30 s health wait, all past the flat 30 s.
    #[tokio::test(start_paused = true)]
    async fn a_restart_window_waits_for_an_unhealthy_after_start_result() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Restart);
        spawn_attribution_watcher(state, record.clone(), AttributionTargets::default(), None);

        tokio::time::sleep(Duration::from_secs(31)).await;
        assert!(
            outcome_of(&record).is_none(),
            "the restart window closed before its result"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        record.record_run_result(ActionRunResult::Errored {
            reason: "runner_unhealthy_after_start: runner 'primary' did not become healthy \
                     within 30000ms of the restart"
                .to_string(),
        });
        let outcome = await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await;
        assert_eq!(outcome.category, D3Category::Failure);
        assert!(outcome
            .evidence_ref
            .as_deref()
            .is_some_and(|e| e.contains("runner_unhealthy_after_start")));
    }

    /// Phase 4.7 (plan `2026-11-03-supervisor-dev-action-residuals-…`). The
    /// tail after a result is capped by the backstop, and the cap is measured
    /// on the tokio clock from the mint ([`ActionRecord::started_instant`]),
    /// the clock the backstop deadline runs on. A result at t+90 s under a
    /// 60 s tail and a 100 s backstop closes at t+100 s. Measured from the
    /// wall clock, as it was, a paused tokio clock reads ~0 s elapsed and the
    /// window would close at t+150 s.
    #[tokio::test(start_paused = true)]
    async fn the_tail_cap_fires_on_the_tokio_clock_from_the_mint() {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let record = bare_record(ActionKind::Build);
        spawn_attribution_watcher_with_timing(
            state,
            record.clone(),
            AttributionTargets::default(),
            WindowTiming {
                tail: Duration::from_secs(60),
                backstop: Duration::from_secs(100),
            },
        );

        tokio::time::sleep(Duration::from_secs(90)).await;
        assert!(outcome_of(&record).is_none());
        record.record_run_result(ActionRunResult::Ran);

        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(
            outcome_of(&record).is_none(),
            "the window closed before the backstop capped its tail"
        );

        tokio::time::sleep(Duration::from_secs(2)).await;
        for _ in 0..100 {
            if outcome_of(&record).is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let outcome = outcome_of(&record).expect(
            "the tail ran past the 100 s backstop: the cap was not measured on the tokio              clock from the mint",
        );
        assert_eq!(outcome.category, D3Category::Confirmed);
    }

    /// The primary's cached health snapshot, with `ui_error` and
    /// `derived_status` as given and every other field neutral.
    fn cached_health(
        ui_error: Option<crate::health_cache::UiErrorSummary>,
        derived_status: crate::health_cache::RunnerStatus,
    ) -> crate::health_cache::CachedRunnerHealth {
        crate::health_cache::CachedRunnerHealth {
            id: crate::config::RunnerConfig::default_primary().id,
            name: "Primary".to_string(),
            port: 9876,
            kind: qontinui_types::wire::runner_kind::RunnerKind::Primary,
            running: true,
            pid: Some(SPAWNED_PID),
            api_responding: true,
            ui_error,
            recent_crash: None,
            derived_status,
            watchdog: crate::routes::health::WatchdogHealth::unavailable(),
            liveness: crate::state::RunnerLiveness::Responding,
            last_seen_responding_at: None,
            port_open: true,
            frontend_ready: None,
            frontend_state: None,
            frontend_not_ready_since: None,
        }
    }

    fn ui_error_first_seen(at: DateTime<Utc>) -> crate::health_cache::UiErrorSummary {
        crate::health_cache::UiErrorSummary {
            message: "TypeError: cannot read properties of undefined".to_string(),
            digest: None,
            stack: None,
            component_stack: None,
            first_seen: at,
            reported_at: at,
            count: 1,
        }
    }

    /// Run a `Build` action that records `Ran` against the primary, with the
    /// cached health `health` builds from its record, and, when `spawn_at`
    /// gives a time, a spawn it initiated then. Returns the outcome.
    async fn build_outcome_over_cached_health(
        spawn_at: impl Fn(&ActionRecord) -> Option<DateTime<Utc>>,
        health: impl Fn(&ActionRecord) -> crate::health_cache::CachedRunnerHealth,
    ) -> ActionOutcome {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let primary = test_primary();
        let record = bare_record(ActionKind::Build);
        spawn_attribution_watcher_with_timing(
            state.clone(),
            record.clone(),
            AttributionTargets {
                runner_id: Some(primary.config.id.clone()),
                managed: Some(primary.clone()),
            },
            FAST_BUILD_WINDOW,
        );
        if let Some(at) = spawn_at(&record) {
            let log_dir = tmp.path().join("dev-logs").join("runner-primary");
            record_spawn(&primary, &record, at, SPAWNED_PID, &log_dir).await;
        }
        *state.cached_runner_health.write().await = vec![health(&record)];
        record.record_run_result(ActionRunResult::Ran);
        await_outcome(
            &record,
            crate::test_clock::poll_budget(Duration::from_secs(30)),
        )
        .await
    }

    /// T1-13 (D3). The cached-health surface is read only for an action that
    /// initiated a spawn inside its window, and its `ui_error` only when first
    /// seen after that spawn; the `derived_status` fold does not re-admit a
    /// `ui_error` its date dropped.
    #[tokio::test]
    async fn a_build_only_action_reads_no_cached_health() {
        use crate::health_cache::RunnerStatus;

        // A build that started nothing: the runner's ui_error is not its.
        let build_only = build_outcome_over_cached_health(
            |_| None,
            |record| {
                cached_health(
                    Some(ui_error_first_seen(inside_the_window(record))),
                    RunnerStatus::Healthy,
                )
            },
        )
        .await;
        assert!(
            build_only.signatures.is_empty(),
            "{:?}",
            build_only.signatures
        );
        assert_eq!(build_only.category, D3Category::Confirmed);

        // A boot it initiated, then a ui_error first seen after that boot.
        let its_boot = build_outcome_over_cached_health(
            |record| Some(spawned_after_the_mint(record)),
            |record| {
                cached_health(
                    Some(ui_error_first_seen(inside_the_window(record))),
                    RunnerStatus::Healthy,
                )
            },
        )
        .await;
        assert_eq!(its_boot.signatures, ["DEV-UI-ERROR-BOUNDARY"]);

        // A ui_error first seen before that boot, folded into `Errored`.
        let earlier = build_outcome_over_cached_health(
            |record| Some(spawned_after_the_mint(record)),
            |record| {
                cached_health(
                    Some(ui_error_first_seen(
                        record.started_at - chrono::Duration::seconds(5),
                    )),
                    RunnerStatus::Errored {
                        reason: "ui_error".to_string(),
                    },
                )
            },
        )
        .await;
        assert!(earlier.signatures.is_empty(), "{:?}", earlier.signatures);
        assert_eq!(earlier.category, D3Category::Confirmed);
    }
    #[test]
    fn snapshot_identity_is_read_under_read_only() {
        use crate::config::AmbientCoord;
        let id = uuid::Uuid::from_u128(7);
        assert_eq!(
            snapshot_identity(&AmbientCoord::Read, || Some(id), || Some(id)),
            (Some(id), Some(id)),
            "under Read the snapshot carries this machine's identity"
        );
        for access in [
            AmbientCoord::Ignore,
            AmbientCoord::At("http://127.0.0.1:1".into()),
        ] {
            assert_eq!(
                snapshot_identity(
                    &access,
                    || panic!("{access:?} read the device id"),
                    || panic!("{access:?} read the tenant id"),
                ),
                (None, None),
                "{access:?} must send no identity and read none"
            );
        }
    }

    // ── Plan `2026-11-03-supervisor-dev-action-residuals-coord-resolvers-
    //    restart-door-and-pre-fix-ledger-rows` Phase 4.2-4.4: `scan_health` ──

    /// A closed window of one minute ending now, and the primary as the
    /// acted-on runner with no spawn recorded yet.
    fn health_scan_fixture() -> (
        SharedState,
        Arc<ManagedRunner>,
        ClosedWindow,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let state = watcher_test_state(tmp.path());
        let close_at = Utc::now();
        let window = ClosedWindow {
            open_at: close_at - chrono::Duration::seconds(60),
            close_at,
        };
        (state, test_primary(), window, tmp)
    }

    /// `seconds` after the window opened.
    fn at(window: ClosedWindow, seconds: i64) -> DateTime<Utc> {
        window.open_at + chrono::Duration::seconds(seconds)
    }

    /// Record a spawn of `runner` at `spawned_at`, child `pid`, that
    /// `initiated_by` asked for, with no early log.
    async fn push_spawn(
        runner: &ManagedRunner,
        spawned_at: DateTime<Utc>,
        pid: u32,
        initiated_by: Option<Uuid>,
    ) {
        runner.recent_spawns.write().await.push(SpawnMarker::new(
            spawned_at,
            Some(pid),
            None,
            initiated_by,
        ));
    }

    fn recent_crash(
        reported_at: DateTime<Utc>,
        pid: Option<u32>,
    ) -> crate::health_cache::RecentCrashSummary {
        crate::health_cache::RecentCrashSummary {
            file_path: "D:/.dev-logs/crash_1.txt".to_string(),
            reported_at,
            panic_location: None,
            panic_message: Some("there is no reactor running".to_string()),
            thread: Some("main".to_string()),
            pid,
        }
    }

    /// The signatures `scan_health` reads for `action_id` with the primary's
    /// cached health set to `snap`.
    async fn scan_health_with(
        state: &SharedState,
        runner: &ManagedRunner,
        action_id: Uuid,
        window: ClosedWindow,
        snap: crate::health_cache::CachedRunnerHealth,
    ) -> Vec<DevSignature> {
        *state.cached_runner_health.write().await = vec![snap];
        scan_health(
            state,
            Some(runner),
            Some(runner.config.id.as_str()),
            action_id,
            window,
        )
        .await
        .0
    }

    /// The primary's cached health reporting `crash`, folded into `Errored`.
    fn crashed_health(
        crash: crate::health_cache::RecentCrashSummary,
    ) -> crate::health_cache::CachedRunnerHealth {
        let mut snap = cached_health(
            None,
            crate::health_cache::RunnerStatus::Errored {
                reason: "there is no reactor running".to_string(),
            },
        );
        snap.recent_crash = Some(crash);
        snap
    }

    /// 4.2: a crash of the action's own spawn, dated after that spawn and
    /// inside the window, is attributed: with no pid on the dump by the
    /// interim rule (the spawn was the runner's newest when the dump was
    /// written), and with one by the pid.
    #[tokio::test]
    async fn scan_health_recent_crash_inside_span_attributes() {
        let (state, primary, window, _tmp) = health_scan_fixture();
        let action_id = Uuid::new_v4();
        push_spawn(&primary, at(window, 1), SPAWNED_PID, Some(action_id)).await;

        for pid in [None, Some(SPAWNED_PID)] {
            let sigs = scan_health_with(
                &state,
                &primary,
                action_id,
                window,
                crashed_health(recent_crash(at(window, 5), pid)),
            )
            .await;
            assert_eq!(sigs, [DevSignature::DevUiErrorBoundary], "dump pid {pid:?}");
        }
    }

    /// 4.2: a crash dated before (or at) the action's spawn is an older
    /// process's, inside the window or not, with or without a matching pid.
    #[tokio::test]
    async fn scan_health_recent_crash_before_spawn_is_ignored() {
        let (state, primary, window, _tmp) = health_scan_fixture();
        let action_id = Uuid::new_v4();
        let spawned_at = at(window, 10);
        push_spawn(&primary, spawned_at, SPAWNED_PID, Some(action_id)).await;

        for reported_at in [at(window, -5), at(window, 5), spawned_at] {
            for pid in [None, Some(SPAWNED_PID)] {
                let sigs = scan_health_with(
                    &state,
                    &primary,
                    action_id,
                    window,
                    crashed_health(recent_crash(reported_at, pid)),
                )
                .await;
                assert!(
                    sigs.is_empty(),
                    "a crash dated {reported_at} (spawn at {spawned_at}) with pid {pid:?} \
                     was attributed: {sigs:?}"
                );
            }
        }
    }

    /// 4.3, the dump-pid arm: a pid on the dump decides. A crash dated inside
    /// the span is not the action's when the dump names another process, even
    /// where the interim rule would have read it as the action's spawn.
    #[tokio::test]
    async fn scan_health_recent_crash_keys_on_the_dump_pid() {
        let (state, primary, window, _tmp) = health_scan_fixture();
        let action_id = Uuid::new_v4();
        push_spawn(&primary, at(window, 1), SPAWNED_PID, Some(action_id)).await;

        let other = scan_health_with(
            &state,
            &primary,
            action_id,
            window,
            crashed_health(recent_crash(at(window, 5), Some(SPAWNED_PID + 1))),
        )
        .await;
        assert!(other.is_empty(), "another process's dump: {other:?}");

        let its_own = scan_health_with(
            &state,
            &primary,
            action_id,
            window,
            crashed_health(recent_crash(at(window, 5), Some(SPAWNED_PID))),
        )
        .await;
        assert_eq!(its_own, [DevSignature::DevUiErrorBoundary]);
    }

    /// 4.3, the interim (ring-pid) arm and the exit bound: the action's spawn
    /// came up and exited, and a crash-watchdog respawn it did not initiate
    /// followed. A pid-less crash dated after that respawn is the respawn's,
    /// not the action's. A dump naming the action's pid but dated after that
    /// spawn's stamped exit is not its either (Windows reuses pids). One
    /// dated while the action's spawn was the one running is its.
    #[tokio::test]
    async fn scan_health_recent_crash_of_a_later_spawn_is_ignored() {
        let (state, primary, window, _tmp) = health_scan_fixture();
        let action_id = Uuid::new_v4();
        push_spawn(&primary, at(window, 1), SPAWNED_PID, Some(action_id)).await;
        assert!(primary
            .recent_spawns
            .write()
            .await
            .stamp_exit(at(window, 1), at(window, 3), None));
        push_spawn(&primary, at(window, 4), SPAWNED_PID + 1, None).await;

        for pid in [None, Some(SPAWNED_PID)] {
            let sigs = scan_health_with(
                &state,
                &primary,
                action_id,
                window,
                crashed_health(recent_crash(at(window, 5), pid)),
            )
            .await;
            assert!(
                sigs.is_empty(),
                "a crash after the respawn with pid {pid:?} was read as the action's: {sigs:?}"
            );
        }

        for pid in [None, Some(SPAWNED_PID)] {
            let sigs = scan_health_with(
                &state,
                &primary,
                action_id,
                window,
                crashed_health(recent_crash(at(window, 2), pid)),
            )
            .await;
            assert_eq!(sigs, [DevSignature::DevUiErrorBoundary], "dump pid {pid:?}");
        }
    }

    /// 4.4: the runner's undated `derived_status: errored` counts only when
    /// the snapshot shows the API answering after the action's spawn.
    /// Otherwise nothing places the report after that boot, and the arm is
    /// inconclusive: no signature.
    #[tokio::test]
    async fn scan_health_undated_errored_without_post_spawn_api_is_inconclusive() {
        let (state, primary, window, _tmp) = health_scan_fixture();
        let action_id = Uuid::new_v4();
        let spawned_at = at(window, 10);
        push_spawn(&primary, spawned_at, SPAWNED_PID, Some(action_id)).await;

        let errored = |api_responding: bool, last_seen: Option<DateTime<Utc>>| {
            let mut snap = cached_health(
                None,
                crate::health_cache::RunnerStatus::Errored {
                    reason: "runner reported derived_status=errored".to_string(),
                },
            );
            snap.api_responding = api_responding;
            snap.last_seen_responding_at = last_seen;
            snap
        };

        for (api_responding, last_seen, why) in [
            (true, None, "the API was never seen answering"),
            (
                true,
                Some(at(window, 5)),
                "the API was last seen before the spawn",
            ),
            (
                true,
                Some(spawned_at),
                "the API was last seen at the spawn, not after",
            ),
            (
                false,
                Some(at(window, 20)),
                "the API is not answering on this snapshot",
            ),
        ] {
            let sigs = scan_health_with(
                &state,
                &primary,
                action_id,
                window,
                errored(api_responding, last_seen),
            )
            .await;
            assert!(sigs.is_empty(), "{why}, yet it was attributed: {sigs:?}");
        }

        let answered_after = scan_health_with(
            &state,
            &primary,
            action_id,
            window,
            errored(true, Some(at(window, 20))),
        )
        .await;
        assert_eq!(answered_after, [DevSignature::DevUiErrorBoundary]);
    }
}
