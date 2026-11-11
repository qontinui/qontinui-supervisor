use crate::build_submissions::BuildSubmissionStore;
use crate::ci_runner_probe::CiRunnerState;
use crate::config::{RunnerConfig, SupervisorConfig};
use crate::dev_action::MintedAction;
use crate::diagnostics::DiagnosticsState;
use crate::health_cache::{CachedPortHealth, CachedRunnerHealth};
use crate::log_capture::{LogLevel, LogSource, LogState};
use crate::process::job::RunnerJob;
use crate::process::panic_log::RecentPanic;
use crate::process::stopped_cache::StoppedRunnerSnapshot;
use crate::routes::supervisor_bridge::CommandRelay;
use crate::velocity_improvement::VelocityImprovementState;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::{broadcast, watch, Notify, RwLock, Semaphore};
use uuid::Uuid;

pub type SharedState = Arc<SupervisorState>;

/// Per-runner state container. Each managed runner has its own state.
pub struct ManagedRunner {
    pub config: RunnerConfig,
    pub runner: RwLock<RunnerState>,
    pub watchdog: RwLock<WatchdogState>,
    pub cached_health: RwLock<CachedPortHealth>,
    pub health_cache_notify: Notify,
    pub logs: LogState,
    /// Runtime-mutable protection flag. When true, this runner cannot be stopped
    /// or restarted by smart rebuild, watchdog, AI sessions, or workflow loop.
    pub protected: RwLock<bool>,
    /// When this entry was inserted into the registry. Used by the reaper to
    /// avoid removing runners that were just created but haven't started yet.
    pub created_at: std::time::Instant,
    /// Most recent startup panic detected for this runner. Populated by
    /// `monitor_runner_process_exit` when the process exits non-zero AND a
    /// fresh `runner-panic.log` is on disk. Read by `GET /runners`,
    /// `GET /runners/{id}/logs`, and the spawn-test 500/502 response.
    pub recent_panic: RwLock<Option<RecentPanic>>,
    /// Filesystem path where the runner was told to write its panic log.
    /// Set at spawn time via `QONTINUI_RUNNER_LOG_DIR`. `None` when the
    /// runner is using its default path (which the supervisor falls back to).
    pub panic_log_dir: RwLock<Option<PathBuf>>,
    /// Path to the per-spawn early-death log file (if one was opened).
    /// Surfaced via the spawn-test 500/502 error response (`early_log_path`)
    /// and `GET /runners/{id}/logs` while the runner is alive. `None` if the
    /// supervisor failed to open the file or if this runner was constructed
    /// outside the spawn flow (primary, user-imported registry entry, etc.).
    /// See `crate::process::early_log` for the lifecycle.
    pub early_log_path: RwLock<Option<PathBuf>>,
    /// This supervisor's recent spawns of the runner, oldest first, in a
    /// bounded ring ([`SpawnRing`]). `start_managed_runner` pushes one entry
    /// per spawn (through `record_spawn_in_registry`), after it has re-pointed
    /// `panic_log_dir`, `recent_panic` and `early_log_path` for that spawn. The
    /// exit monitor stamps the entry of the spawn it watches with the exit
    /// time and, when a stop requested that exit before it happened, the time
    /// of the request (the lag [`SpawnMarker::stop_requested_at`] states
    /// excepted); once it has parsed the panic log it attaches the panic it
    /// parsed at that exit. Both land before that monitor hands the exit to
    /// the crash watchdog. No stop and no exit removes an entry.
    ///
    /// The dev-action attribution watcher reads it at window close
    /// (`dev_action::attribution::scan_spawn_effects`): which spawns the
    /// action's window holds, which of them that action initiated, which
    /// process each started, and, as those
    /// stamps record them, whether that process had exited and whether a stop
    /// had asked it to. `RunnerState::pid` cannot say whether the process had
    /// exited: a stop and an exit clear it, a later spawn overwrites it, and
    /// the health cache clears a primary's on every tick before its port
    /// binds. Empty until this supervisor first starts the runner, so an
    /// adopted runner has no entry until then.
    pub recent_spawns: RwLock<SpawnRing>,
    /// Per-spawn override for the source exe to copy when starting this
    /// runner. When `Some(path)`, `start_exe_mode_for_runner` skips the
    /// usual slot-resolution chain and copies this exact path instead.
    /// Set by `spawn_test` when the caller passes `use_lkg: true` so the
    /// runner is pinned to the last-known-good binary regardless of slot
    /// state. Persists across restarts of *this* runner so a crash + manual
    /// restart still gets the LKG; cleared only by replacing the runner
    /// (which spawn-test does anyway since each call creates a fresh id).
    pub source_exe_override: RwLock<Option<PathBuf>>,
    /// Most recent observed auto-login attempt for this runner, derived from
    /// the runner's stdout/stderr stream by `log_capture` matching the
    /// `AUTH_PATTERNS` regex. `None` means we have no evidence either way —
    /// either auto-login wasn't attempted, or the runner hasn't logged yet.
    /// Surfaced via the `auth_state` field of the `spawn-test` response.
    pub last_auth_result: RwLock<Option<LastAuthResult>>,
    /// Optional work-unit / attempt correlation for previews spawned by the
    /// autonomous-dev loop (Track 2 UI-Bridge preview-verification). When a
    /// `spawn-test` request carries `unit_id`/`attempt_id`, they are stored
    /// here so the runner round-trips them in the spawn-test response and is
    /// resolvable via `GET /runners/by-unit/{unit_id}`. `None` for ordinary
    /// (non-preview) runners.
    pub preview_binding: RwLock<Option<PreviewBinding>>,
    /// Identity of the caller that spawned this runner (the `requester_id`
    /// passed to `spawn-test` / `spawn-named`). `None` for runners with no
    /// declared owner (primary, user-imported registry entries, or callers
    /// that omitted the hint).
    ///
    /// Used to (a) surface ownership in `GET /runners` so a session can pin a
    /// runner by id rather than by its (reusable) port, and (b) scope the
    /// `purge-stale` reaper to a single requester so one session's purge can
    /// never evict another session's runner.
    pub requester_id: RwLock<Option<String>>,
    /// Provenance of the exe this runner is ACTUALLY running — recorded at
    /// start time from whichever artifact was resolved (the pinned LKG record,
    /// or the picked build slot's provenance sidecar).
    ///
    /// This is the per-runner answer to "does this runner contain commit X?":
    /// `git merge-base --is-ancestor <fix-sha> <build_provenance.sha>`. Before
    /// this existed the only per-runner staleness signal was `stale_binary`,
    /// which is an **mtime** comparison and therefore blind to a build from a
    /// branch parked behind `origin/main` — a fresh mtime over stale commits is
    /// exactly the case that made a landed fix read as a regression.
    ///
    /// `None` when the runner has never been started by this supervisor, or the
    /// resolved artifact carried no provenance record (legacy `target/debug/`
    /// exe, pre-upgrade slot). Absence is honest "unknown", never "current".
    pub build_provenance: RwLock<Option<crate::process::manager::BuildProvenance>>,
    /// Which artifact this runner was actually started from, and how it was
    /// resolved: the path, the origin (build-pool slot / non-pool cargo target
    /// dir + WHICH target-dir precedence level won / an explicit pin), the
    /// mtime, and any opt-in staleness warning.
    ///
    /// Surfaced on the spawn responses so a caller can see where the binary
    /// came from. Nothing reported the path before, which is how a
    /// `spawn-test {rebuild:false}` came to launch a 54-day-old exe from
    /// `<runner>/target/debug/` (cargo had been writing to the
    /// `CARGO_TARGET_DIR` override at `src-tauri/target/` for weeks) while
    /// looking perfectly healthy.
    ///
    /// `None` until this supervisor starts the runner.
    pub resolved_exe: RwLock<Option<crate::process::manager::ResolvedRunnerExe>>,
    /// Per-spawn opt-in: allow starting from a resolved exe whose build
    /// identity cannot be established (see
    /// [`crate::process::manager::unverified_exe_gate`]). Set from the
    /// `allow_stale_fallback` field of `spawn-test` / `spawn-named` — the
    /// caller explicitly asking for "whatever exists". The staleness is still
    /// logged and still reported on the response; only the refusal is waived.
    pub allow_unverified_exe: RwLock<bool>,
    /// Spawn-in-flight marker. `Some(budget)` while a spawn handler
    /// (`routes::runners::execute_spawn_build`) owns this runner — building,
    /// copying the exe, starting it and probing its health. `budget` is the
    /// startup allowance measured from `RunnerState::started_at`: both
    /// stale-runner sweeps treat a runner inside that window whose port is not
    /// listening yet as STARTING, not crashed (see
    /// [`crate::process::manager::startup_window_protects`]).
    ///
    /// A `std` mutex (not tokio) so the handler's RAII guard can clear it from
    /// `Drop` on every exit path, including a cancelled future. Never held
    /// across an `.await`.
    pub spawn_in_flight: std::sync::Mutex<Option<std::time::Duration>>,
    /// Orders this runner's starts, stops and removals against each other.
    ///
    /// `start_managed_runner` holds it from before what its `StartAs` requires,
    /// a restart's re-insert included, until the start ends, so through recording
    /// the spawn and spawning that spawn's exit monitor. An `Arc` because that
    /// start runs on its own task, which owns the guard, so a caller that goes away
    /// cannot end the start between the two.
    /// `stop_runner_by_id` holds it from its latch through its reset and a
    /// `test-*` runner's teardown. `reap_stale_test_runners`, the purge-stale
    /// sweep and `DELETE /runners/{id}` `try_lock` it, skip or refuse on contention
    /// and hold it through the removal.
    ///
    /// Not reentrant, so nothing holding it may call a start or a stop:
    /// `restart_after_gate` holds it around neither of its two calls. The exit
    /// monitor never takes it: a stop's graceful wait, under this lock, is
    /// waiting for the monitor to clear `running`.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// The spawn-isolation verdict of this supervisor's most recent start of
    /// the runner (plan `2026-11-04-linux-dev-supervisor-dies-with-its-launcher-
    /// and-orphans-its-temp-runners`, Phase 1b). On Linux a user-owned runner is
    /// `Scoped(<unit>)` — its own transient systemd scope, outside the
    /// supervisor's cgroup, confirmed by the landing check — or `Unscoped(why)`.
    /// `None` until this supervisor starts it (an adopted or external runner
    /// was never in this supervisor's cgroup). Surfaced by `GET /runners` as
    /// `isolation` / `isolation_unit`, and summarised in `/health`
    /// `runner_isolation`.
    pub isolation: RwLock<Option<crate::process::systemd_scope::Isolation>>,
}

/// One spawn of a managed runner, as [`ManagedRunner::recent_spawns`] keeps
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnMarker {
    /// When the spawn was recorded: the instant `start_managed_runner` wrote
    /// into `RunnerState::started_at`. This is the spawn's identity: the
    /// lifecycle lock serialises one runner's spawns, and the exit monitor
    /// keys both its reset and its ring writes on it.
    pub spawned_at: DateTime<Utc>,
    /// The child's pid, as the OS reported it at the spawn.
    pub pid: Option<u32>,
    /// This spawn's own early-log file, `None` when it could not open one.
    /// Early logs are per-spawn files, so a later spawn does not overwrite it.
    pub early_log_path: Option<PathBuf>,
    /// When the exit monitor observed this spawn's process exit, whatever
    /// ended it (a crash, a clean exit, a stop's kill). `None` until the
    /// monitor stamps it, which it does right after its first critical
    /// section once its wait on the child returns, before it logs the exit or
    /// parses the panic log. So an exit reaches this field after a short lag,
    /// the wait's wake-up and that critical section, and a reader inside the
    /// lag sees the process as still up.
    pub exited_at: Option<DateTime<Utc>>,
    /// When the stop request the exit monitor read for that exit was made: the
    /// earliest request still latched on the runner
    /// ([`RunnerState::stop_requested_at`]) when the monitor read the flag
    /// after the exit, or the exit's own time when the flag was latched with
    /// no time. `Some` when the stop flag was latched as the exit monitor read
    /// it and the request is dated at or before the exit, a failed stop's flag
    /// included, as it is for an exit an operator stop, a later restart or stop
    /// of the same runner, or the over-age kill asked for. `None` when it was
    /// not: an exit nobody asked for, a crash, an unrequested clean exit, the
    /// first-healthy watchdog's kill of a wedged boot, which latches no stop,
    /// or an exit that had already happened when the stop was latched
    /// ([`SpawnRing::stamp_exit`]). While no exit is stamped it is `None`, unless a
    /// later start has cleared a stop request meant for this spawn and carried it
    /// here ([`SpawnRing::carry_stop_request`]), since the flag that monitor would
    /// have read is gone by then.
    ///
    /// One lag the lifecycle lock cannot close, since the monitor waits outside
    /// it: `exited_at` is read after the wait's wake-up, so a stop latched between
    /// the real exit and that wake-up still dates before it and reads as the
    /// exit's request (see the residual of
    /// `dev_action::attribution::is_startup_panic_of`).
    pub stop_requested_at: Option<DateTime<Utc>>,
    /// The panic the exit monitor parsed from the runner's panic log at that
    /// exit, the one it also stored as `recent_panic`, when it parsed a fresh
    /// one. The monitor attaches it after the exit stamp, once it has parsed
    /// the log ([`SpawnRing::attach_exit_panic`]). Kept here because the next
    /// spawn clears `recent_panic` and the runner's next panic, a caught one
    /// included, overwrites the file. It need not be this spawn's own panic:
    /// a reader holds it to the same checks as either of those.
    pub exit_panic: Option<RecentPanic>,
    /// The boot panic spawn-test's failed health probe parsed and stored as
    /// `recent_panic` for this spawn (`routes::runners::probe_runner_health`),
    /// which it also attaches here ([`SpawnRing::attach_boot_panic`]). Kept
    /// because the three other places a panic is read from each hold only the
    /// runner's latest panic: a teardown panic of the stop that follows the
    /// probe overwrites `runner-panic.log`, and on an exit that is not clean
    /// also `recent_panic` and [`Self::exit_panic`]. The first one attached is
    /// kept, so this is the spawn's earliest stored panic. A reader holds it to
    /// the same checks as the others.
    pub boot_panic: Option<RecentPanic>,
    /// The dev action that started this spawn (`ActionRecord::action_id`).
    /// Every production start names one, the watchdogs' and the boot
    /// auto-start's included; `None` is for a test driving a start with no
    /// trial. At window close the attribution watcher judges only the entries
    /// its own action initiated, so a watchdog's respawn inside a long window is
    /// not read as that action's boot: it is the watchdog's own trial.
    pub initiated_by: Option<Uuid>,
}

impl SpawnMarker {
    /// A spawn as `start_managed_runner` records it: not exited yet.
    pub fn new(
        spawned_at: DateTime<Utc>,
        pid: Option<u32>,
        early_log_path: Option<PathBuf>,
        initiated_by: Option<Uuid>,
    ) -> Self {
        Self {
            spawned_at,
            pid,
            early_log_path,
            exited_at: None,
            stop_requested_at: None,
            exit_panic: None,
            boot_panic: None,
            initiated_by,
        }
    }
}

/// How many spawns [`ManagedRunner::recent_spawns`] keeps.
///
/// The attribution watcher judges every spawn its action made inside the
/// action's window, so the ring must hold every spawn one window can see. The
/// longest per-kind window is the 2-hour spawn and build backstop
/// (`dev_action::attribution::LONGEST_BACKSTOP`). The crash watchdog makes at
/// most three restarts in any rolling 30 minutes before it disarms itself, so
/// about twelve in 2 hours, fifteen counting a burst at each end. A window
/// therefore sees the action's own spawn, at most fifteen crash-watchdog
/// restarts, and whatever other actions start the runner meanwhile;
/// thirty-two leaves room for sixteen of those, about the margin sixteen left
/// against the old 45-minute backstop (nine). A backstop override
/// (`rebuild_when_idle`'s deadline) can hold a window longer, but its own spawn
/// comes after its wait and its window closes a tail after its result. If more
/// spawns than that ever land inside one window, the ring drops the oldest
/// before the window closes: the watcher never judges those boots, and the exit
/// monitor's stamp for a dropped spawn finds no entry and is discarded.
pub const SPAWN_RING_CAPACITY: usize = 32;

/// A runner's recent spawns, oldest first, at most [`SPAWN_RING_CAPACITY`].
///
/// Only [`Self::push`] removes an entry, and only the oldest, to make room for
/// a new spawn. [`Self::stamp_exit`], [`Self::attach_exit_panic`] and
/// [`Self::attach_boot_panic`] fill an entry in and remove none.
#[derive(Debug, Default)]
pub struct SpawnRing {
    entries: VecDeque<SpawnMarker>,
}

impl SpawnRing {
    /// Record a spawn, dropping the oldest entry when the ring is full.
    pub fn push(&mut self, spawn: SpawnMarker) {
        if self.entries.len() >= SPAWN_RING_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(spawn);
    }

    /// Stamp the exit of the spawn recorded at `spawned_at`: when it exited,
    /// and when a stop requested that exit. The monitor names the spawn it
    /// watches rather than a pid, so an exit cannot land on another spawn that
    /// got the same pid from the OS. A request a later start carried onto the entry
    /// wins over the one the monitor read: it is known to be this spawn's, while the
    /// flag may by then be a stop of the later spawn. A request dated after
    /// `exited_at` is dropped: that exit had already happened, so that stop did
    /// not ask for it. Returns `false`, and changes nothing, when no entry matches:
    /// the ring dropped that spawn.
    pub fn stamp_exit(
        &mut self,
        spawned_at: DateTime<Utc>,
        exited_at: DateTime<Utc>,
        stop_requested_at: Option<DateTime<Utc>>,
    ) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|spawn| spawn.spawned_at == spawned_at)
        {
            Some(spawn) => {
                spawn.exited_at = Some(exited_at);
                spawn.stop_requested_at = spawn
                    .stop_requested_at
                    .or(stop_requested_at)
                    .filter(|requested| *requested <= exited_at);
                true
            }
            None => false,
        }
    }

    /// Keep a stop request that the next start is about to clear, on the spawn it
    /// was meant for: the newest entry, while its exit is not stamped. That
    /// spawn's monitor has not read the flag yet and will read it cleared.
    /// An entry whose exit is stamped keeps what its monitor read.
    pub fn carry_stop_request(&mut self, requested_at: DateTime<Utc>) {
        if let Some(spawn) = self
            .entries
            .back_mut()
            .filter(|spawn| spawn.exited_at.is_none())
        {
            spawn.stop_requested_at = Some(requested_at);
        }
    }

    /// Attach the panic the exit monitor parsed at an exit it has already
    /// stamped, to the spawn recorded at `spawned_at`. An entry still waiting
    /// for its exit never matches. Returns `false`, and changes nothing, when
    /// no entry matches: the ring dropped that spawn.
    pub fn attach_exit_panic(&mut self, spawned_at: DateTime<Utc>, panic: RecentPanic) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|spawn| spawn.spawned_at == spawned_at && spawn.exited_at.is_some())
        {
            Some(spawn) => {
                spawn.exit_panic = Some(panic);
                true
            }
            None => false,
        }
    }

    /// Attach the boot panic spawn-test's failed probe stored for the spawn
    /// recorded at `spawned_at`, exited or not: a boot that died before the
    /// probe gave up is already stamped. A panic already attached is kept, so
    /// the entry holds its earliest one. Returns `false`, and changes nothing,
    /// when no entry matches (the ring dropped that spawn) or one is already
    /// attached.
    pub fn attach_boot_panic(&mut self, spawned_at: DateTime<Utc>, panic: RecentPanic) -> bool {
        match self
            .entries
            .iter_mut()
            .find(|spawn| spawn.spawned_at == spawned_at)
        {
            Some(spawn) if spawn.boot_panic.is_none() => {
                spawn.boot_panic = Some(panic);
                true
            }
            _ => false,
        }
    }

    /// Whether the spawn `action_id` started has ended: it exited, or a later
    /// spawn replaced it, which may be before its exit monitor has stamped it.
    /// `false` when no entry names the action, which says nothing about it.
    pub fn action_spawn_ended(&self, action_id: Uuid) -> bool {
        self.action_spawn_ended_by(action_id, DateTime::<Utc>::MAX_UTC)
    }

    /// Whether that spawn had already ended by `at`, for a reader asking about a
    /// moment in the past: a spawn that answered and ended later had not. An exit
    /// counts from its stamp, which lags the real exit a little, and a replacement
    /// from the later spawn's record, which needs no stamp.
    pub fn action_spawn_ended_by(&self, action_id: Uuid, at: DateTime<Utc>) -> bool {
        let Some(own) = self
            .entries
            .iter()
            .rposition(|spawn| spawn.initiated_by == Some(action_id))
        else {
            return false;
        };
        self.entries[own]
            .exited_at
            .is_some_and(|exited| exited <= at)
            || self
                .entries
                .get(own + 1)
                .is_some_and(|next| next.spawned_at <= at)
    }

    /// The newest spawn recorded, `None` until this supervisor starts the
    /// runner.
    pub fn newest(&self) -> Option<&SpawnMarker> {
        self.entries.back()
    }

    /// The spawns recorded from `from` to `to`, both included, oldest first.
    pub fn spawned_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<SpawnMarker> {
        self.entries
            .iter()
            .filter(|spawn| from <= spawn.spawned_at && spawn.spawned_at <= to)
            .cloned()
            .collect()
    }

    /// Every entry, oldest first: what the ring's tests compare against.
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = &SpawnMarker> {
        self.entries.iter()
    }
}

/// Work-unit → preview correlation for a runner spawned as an attempt's
/// preview. Keyed externally by `(unit_id, attempt_id)`; stored on the
/// `ManagedRunner` so the supervisor's existing `runner_id -> ManagedRunner`
/// map is the single source of truth (no parallel side map to keep in sync).
#[derive(Clone, Debug)]
pub struct PreviewBinding {
    /// The autonomous-loop work unit this preview is built for.
    pub unit_id: String,
    /// The specific attempt within that unit (its git ref was the build source).
    pub attempt_id: Option<String>,
    /// Git SHA the preview actually ran, as reported by the runner's `/health`
    /// probe at spawn time. `None` until the health probe completes (or if the
    /// probe was skipped / the runner reported no SHA). This is the provenance
    /// fact the verifier resolves via `GET /runners/by-unit/{unit_id}`.
    pub git_sha: Option<String>,
}

/// Snapshot of the most recent auto-login attempt observed for a runner.
///
/// Populated by the log-capture path when a line in the runner's stdout/stderr
/// matches `crate::log_capture::AUTH_PATTERNS`. Only failure cases are tracked
/// here — a successful login emits no diagnostic line, and the supervisor has
/// no other observability into the runner's auth state today.
#[derive(Clone, Debug)]
pub struct LastAuthResult {
    /// True when an auto-login attempt was observed (i.e. a matching
    /// log line surfaced).
    pub attempted: bool,
    /// `Some(true)` for a confirmed success (currently never set — no
    /// success-line pattern is matched), `Some(false)` for an observed
    /// failure, `None` if the outcome could not be determined.
    pub succeeded: Option<bool>,
    /// Wall-clock time the matching log line was captured.
    pub attempt_at: DateTime<Utc>,
    /// Up to ~200 chars of the offending log line. Kept short so it can
    /// be safely embedded in API responses.
    pub rate_limit_hint: Option<String>,
}

impl ManagedRunner {
    #[allow(dead_code)]
    pub fn new(config: RunnerConfig, watchdog_enabled: bool) -> Self {
        Self::new_with_log_dir(config, watchdog_enabled, None)
    }

    /// Construct a ManagedRunner and, when `log_dir` is set, attach a
    /// per-runner persistent log file at `<log_dir>/<runner_id>.log`.
    /// Every stdout/stderr line captured by `spawn_stdout_reader` /
    /// `spawn_stderr_reader` is tee'd to this file in append mode. If the
    /// file can't be opened, the runner still starts — persistent logging
    /// is strictly best-effort.
    pub fn new_with_log_dir(
        config: RunnerConfig,
        watchdog_enabled: bool,
        log_dir: Option<&std::path::Path>,
    ) -> Self {
        let protected = config.protected;
        let logs = LogState::new();
        if let Some(dir) = log_dir {
            let path = dir.join(format!("{}.log", config.id));
            if let Some(writer) = crate::log_capture::open_append_log(&path) {
                logs.set_file_writer(Some(writer));
            }
        }
        Self {
            config,
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            cached_health: RwLock::new(CachedPortHealth::default()),
            health_cache_notify: Notify::new(),
            logs,
            protected: RwLock::new(protected),
            created_at: std::time::Instant::now(),
            recent_panic: RwLock::new(None),
            panic_log_dir: RwLock::new(None),
            early_log_path: RwLock::new(None),
            recent_spawns: RwLock::new(SpawnRing::default()),
            source_exe_override: RwLock::new(None),
            last_auth_result: RwLock::new(None),
            preview_binding: RwLock::new(None),
            requester_id: RwLock::new(None),
            build_provenance: RwLock::new(None),
            resolved_exe: RwLock::new(None),
            allow_unverified_exe: RwLock::new(false),
            spawn_in_flight: std::sync::Mutex::new(None),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            isolation: RwLock::new(None),
        }
    }

    /// Read the spawn-in-flight marker. A poisoned lock is read through (the
    /// value is a plain `Option<Duration>`, so it cannot be half-written).
    pub fn spawn_in_flight(&self) -> Option<std::time::Duration> {
        *self
            .spawn_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Set (or clear, with `None`) the spawn-in-flight marker.
    pub fn set_spawn_in_flight(&self, budget: Option<std::time::Duration>) {
        *self
            .spawn_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = budget;
    }

    /// Check if this runner is protected.
    pub async fn is_protected(&self) -> bool {
        *self.protected.read().await
    }
}

/// A timestamped origin/main drift reading, cached on
/// [`SupervisorState::origin_drift`].
///
/// `computed_at` is carried so `GET /builds` can report the reading's AGE
/// rather than presenting a cached value as current. Per fleet policy
/// `verification-and-evidence` / `unknown-must-not-render-as-a-default`, a
/// never-computed cache must be distinguishable from "no drift" -- both used to
/// serialize as `null`, which is exactly the confident-looking default that
/// clause forbids.
#[derive(Debug, Clone)]
pub struct OriginDriftSnapshot {
    /// The LKG sha this reading was computed for. Compared against the CURRENT
    /// LKG sha at read time: if the LKG moved since, the reading describes a
    /// build that is no longer current and must be reported as stale.
    pub built_sha: String,
    pub drift: crate::git_provenance::OriginMainDrift,
    pub computed_at: chrono::DateTime<chrono::Utc>,
}

pub struct SupervisorState {
    pub config: SupervisorConfig,
    /// Multi-runner map: runner_id -> ManagedRunner
    pub runners: RwLock<HashMap<String, Arc<ManagedRunner>>>,
    // Legacy single-runner fields kept for backward compat during transition.
    // These point to the primary runner's state.
    pub runner: RwLock<RunnerState>,
    pub watchdog: RwLock<WatchdogState>,
    pub build: RwLock<BuildState>,
    /// Parallel cargo build slot pool. Semaphore permits + per-slot target dirs.
    pub build_pool: BuildPool,
    /// Generalized build-submissions store — Row 2 Phase 3.
    /// Backs `POST /build/submit` + `GET /build/:id/status`. Submissions
    /// share the build_pool's semaphore but bring their own external
    /// worktree path (not the slot dirs). Bounded at 1000 entries with
    /// terminal-oldest LRU eviction.
    pub build_submissions: Arc<BuildSubmissionStore>,
    /// Row 10 Items 6-7 — bazel-remote CAS+AC client for the
    /// content-addressed build cache. Fail-open: a down backend never
    /// stalls or fails a build.
    pub bazel_remote: crate::bazel_remote::BazelRemoteClient,
    /// Row 10 Items 6-7 — content-hash cache telemetry (ac_hit_rate per
    /// worker/repo/profile + dual-write shadow counters). Exposed at
    /// `GET /builds/cache-stats`.
    pub cache_telemetry: crate::cache_telemetry::CacheTelemetry,
    pub ai: RwLock<AiState>,
    pub expo: RwLock<ExpoState>,
    pub diagnostics: RwLock<DiagnosticsState>,
    /// In-memory capped store of dev-action snapshots (Phase 1 of the
    /// dev-event cause-effect ledger,
    /// `plans/2026-06-07-twin-dev-event-cause-effect-ledger.md`). Mirrors
    /// `diagnostics`'s ring discipline: keyed by `action_id`, oldest-evicted.
    /// Written by the action routes (restart/spawn/build) at mint time and by
    /// the detached attribution watcher at window close; read by
    /// `GET /actions/{id}/outcome`. Coord persistence is Phase 3.
    pub dev_actions: RwLock<crate::dev_action::ActionStore>,
    pub evaluation: RwLock<EvaluationState>,
    pub velocity_tests: RwLock<VelocityTestState>,
    pub velocity_improvement: RwLock<VelocityImprovementState>,
    pub command_relay: Arc<CommandRelay>,
    pub logs: LogState,
    pub health_tx: broadcast::Sender<()>,
    pub shutdown_tx: broadcast::Sender<()>,
    /// Latched shutdown flag. Flips to `true` the first time a shutdown is
    /// signaled (HTTP endpoint or Ctrl+C). Used by [`SupervisorState::shutdown_signal`]
    /// so handlers that subscribe to `shutdown_tx` *after* the broadcast
    /// already fired still observe the shutdown — broadcast channels do
    /// not replay missed messages, but this latched bool does.
    pub shutdown_latched: AtomicBool,
    /// The off-Windows shutdown temp-runner sweep, run ONCE per process.
    /// `shutdown_signal` and the self-restart endpoint both await it; whichever
    /// arrives second waits for the first's sweep to finish instead of
    /// starting a second, concurrent one
    /// (`process::shutdown_sweep::sweep_temp_runners_at_shutdown`). Never read
    /// on Windows, where the kill-on-close job reaps temp runners instead.
    #[cfg_attr(windows, allow(dead_code))]
    pub shutdown_sweep: tokio::sync::OnceCell<()>,
    pub cached_health: RwLock<CachedPortHealth>,
    /// Cached per-runner health snapshots, updated by the background health refresher.
    /// Readable via `try_read()` in sync contexts (SSE streams).
    pub cached_runner_health: RwLock<Vec<CachedRunnerHealth>>,
    pub health_cache_notify: Notify,
    pub http_client: reqwest::Client,
    /// Port the legacy `/ui-bridge/*`, `/runner-api/*` and `/graphql` proxies
    /// forward to.
    /// Always [`crate::config::RUNNER_API_PORT`] in production; a field only so
    /// a test can point those proxies at a stub runner on an ephemeral port.
    pub runner_api_port: u16,
    /// Runtime-configurable auto-login credentials for temp test runners.
    /// Set via `POST /test-login` and read by `forward_test_auto_login_env`.
    pub test_auto_login: RwLock<Option<(String, String)>>,
    /// Post-mortem log cache for runners removed from the active registry.
    /// Keyed by runner id. Bounded at 100 entries / 10 min TTL (see
    /// `process::stopped_cache`). Queryable via
    /// `GET /runners/{id}/logs?include_stopped=true`.
    pub stopped_runners: Arc<RwLock<HashMap<String, StoppedRunnerSnapshot>>>,
    /// Stable identifier for this supervisor *installation* (not this process).
    /// Persisted to a file under the user's local data directory
    /// (`%LOCALAPPDATA%\qontinui-supervisor\boot.id` on Windows,
    /// `~/.local/share/qontinui-supervisor/boot.id` on Linux,
    /// `~/Library/Application Support/qontinui-supervisor/boot.id` on macOS)
    /// and re-read on every supervisor startup, so it survives both bare
    /// restarts and rebuild-then-restart cycles.
    ///
    /// Returned in heartbeat responses and surfaced via
    /// `GET /supervisor-bridge/boot-id`. The dashboard's `BootIdWatcher` polls
    /// this value and reloads the page when it changes — but because the value
    /// is now stable across normal restarts, that reload acts as a *fallback*
    /// for catastrophic situations (the persistence file got deleted or
    /// corrupted, or the install was wiped) rather than the primary
    /// "new bundle available" signal. The primary signal for "a fresh frontend
    /// bundle is now being served" is `build_id` (see below), which the
    /// `BuildRefreshBanner` watches via `/health/stream`.
    pub boot_id: String,
    /// Identifier for the embedded frontend bundle this supervisor is serving.
    /// Computed at startup from the mtime of `dist/index.html` (RFC3339
    /// timestamp), or "unknown" if the file is missing. Surfaced in
    /// `GET /health`, `GET /health/stream`, and injected as a
    /// `<meta name="build-id">` tag into the served `index.html`. Connected
    /// dashboard tabs compare the meta tag value against the SSE stream so a
    /// supervisor rebuild + restart can prompt the user to refresh.
    pub build_id: String,
    /// Windows JobObject used to enforce kill-on-supervisor-exit semantics
    /// for **supervisor-owned ephemeral (temp) runners only**. Created once
    /// at startup; `start_managed_runner` assigns a spawned runner to it via
    /// `RunnerJob::assign` **iff**
    /// [`crate::process::job::should_assign_to_ephemeral_job`] says the kind
    /// is `Temp`. When this `Arc` drops (at supervisor exit) the kernel
    /// closes the last handle to the job and terminates every assigned
    /// process — so a force-killed or panicked supervisor cannot leave
    /// orphan temp runners holding slot binaries.
    ///
    /// The operator's primary, named, and external runners are deliberately
    /// assigned to **no job at all**: they are user-owned, they hold user
    /// sessions, and they must survive every supervisor exit path
    /// (`/supervisor/shutdown`, `/supervisor/restart`, `Stop-Process`,
    /// panic, BSOD). Assigning them was the 2026-07-27 incident.
    ///
    /// `None` when the OS refused to create the job (extremely rare on
    /// Windows; non-Windows builds always get a no-op stub via
    /// `process::job`'s cross-platform shim). Spawning continues either
    /// way — without the safety net, but functional.
    pub ephemeral_job: Option<Arc<RunnerJob>>,
    /// The most recent systemd preflight (`systemd-run` on PATH and the user
    /// manager answering), run once at startup on Linux and again before each
    /// user-owned spawn. `None` until the first one finishes, which `/health`
    /// `runner_isolation` reports as UNKNOWN rather than as "no isolation".
    pub runner_isolation_probe: RwLock<Option<crate::process::systemd_scope::SystemdProbe>>,
    /// Log messages captured during synchronous `SupervisorState::new`
    /// construction that need to be routed through `state.logs.emit` once
    /// async context is available. The `logs` field is initialized inside the
    /// struct literal, so callers like the JobObject creation step (which
    /// runs *before* the `Self { ... }` block completes) can't `.await` on
    /// `state.logs.emit(...)`. They push to this buffer instead, and
    /// [`SupervisorState::flush_pending_startup_logs`] drains it after the
    /// state is constructed and the runtime is ready.
    pub pending_startup_logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
    /// Live count of in-flight SSE connections across every long-lived
    /// streaming endpoint (`/health/stream`, `/logs/stream`,
    /// `/expo/logs/stream`, `/runners/{id}/logs/stream`,
    /// `/supervisor-bridge/commands/stream`). Each handler acquires an
    /// [`SseConnectionGuard`] on entry whose `Drop` decrements this counter
    /// when the response future is torn down.
    ///
    /// Surfaced via `GET /health` as `sse_active_connections` so ops can
    /// verify the graceful-shutdown drain is actually releasing connections
    /// without having to open a stream + trigger shutdown by hand.
    pub active_sse_connections: Arc<AtomicUsize>,
    /// True when debug-only HTTP endpoints (under `/control/dev/*`) are
    /// admitted. Cached at startup from
    /// `QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS=1` so handlers don't re-read the
    /// env on every request. Off by default — debug endpoints are local-dev
    /// only and must never be exposed in shared / multi-tenant deployments.
    pub debug_endpoints_enabled: bool,
    /// Wall-clock time the current supervisor process started. Used by
    /// startup-time slot pre-flight to distinguish stale `.cargo-lock`
    /// advisory files left behind by a previous supervisor (older mtime)
    /// from locks placed by a build that's just now starting on this slot.
    pub supervisor_started_at: std::time::SystemTime,
    /// Broadcast channel for synthetic build-id injection events from the
    /// debug endpoint `POST /control/dev/emit-build-id`. Each `String` sent
    /// is a build-id value that the `/health/stream` SSE handler should
    /// emit as a one-shot synthetic `event: health` to all currently
    /// connected dashboard tabs, overriding the real
    /// [`SupervisorState::build_id`] in the JSON payload (without changing
    /// the on-disk value). Capacity 8; if multiple synthetic events arrive
    /// faster than every SSE consumer drains, the oldest is dropped — the
    /// channel is best-effort, since the goal is exercising the watcher's
    /// divergence path during manual tests, not durable delivery.
    pub synthetic_build_id_tx: broadcast::Sender<String>,
    /// CI runner state probed via WSL. Updated every 30s by
    /// `ci_runner_probe::ci_runner_probe_loop`.
    ///
    /// **The fleet heartbeat does NOT read this** — that claim was stale. The
    /// `ci_runner_labels` / `ci_runner_status` keys were deleted from the budget
    /// payload because coord's `BudgetPublishRequest` declares no such fields
    /// and `upsert_budget` never wrote them (see `fleet.rs:45-53`, pinned by a
    /// test at `fleet.rs:637`). The single consumer is `GET /ci-runner/status`,
    /// which the runner's CI Runner settings panel polls every 10 s.
    /// `coord.devices.ci_runner_status` is written independently by coord's
    /// `ci_runner_registrar` from GitHub's runners API, so the honest
    /// `distro_down` / `probe_failed` readings are a LOCAL view and never reach
    /// fleet metrics.
    pub ci_runner_state: RwLock<CiRunnerState>,
    /// The most recent `POST /runner/fix-and-rebuild` detached rebuild, if any.
    /// `fix-and-rebuild` rebuilds the *single* live runner tree into the
    /// build-pool slots; two concurrent ones would race the same slots + LKG
    /// bookkeeping. The handler reads this under the lock: if it points at a
    /// still-non-terminal submission, the second request is an idempotent accept
    /// (returns the existing submission id and its action) instead of kicking off
    /// a duplicate build. Cleared/overwritten when a new rebuild is started (the
    /// previous id having reached terminal). `None` until the first rebuild.
    pub fix_and_rebuild_inflight: RwLock<Option<FixAndRebuildInflight>>,
    /// Single-flight index for `POST /runners/spawn-test`, keyed by
    /// **(requester, build target)** — see [`SpawnDedupKey`].
    ///
    /// The build pool has three slots. A caller whose `curl` appears to have
    /// returned nothing (a dropped connection, a client-side timeout, a
    /// swallowed 202) retries — and before this index the retry claimed a
    /// SECOND slot to compile the exact same tree for the exact same caller,
    /// spending a third of the pool on a build whose output already existed.
    ///
    /// [`SupervisorState::claim_spawn_build`] is the only door: a request whose
    /// key is already present JOINS the running build (same `build_id`, same
    /// runner, same action, `"deduplicated": true`) instead of starting a second
    /// one. The entry is removed when the build reaches a terminal state.
    ///
    /// Generalises the global `fix_and_rebuild_inflight` above: that one
    /// single-flights the *one* live-tree rebuild with no key at all, which is
    /// only correct because there is exactly one such tree.
    pub spawn_test_inflight: RwLock<HashMap<SpawnDedupKey, SpawnInflight>>,
    /// Spawn-worktree containers (`<workspace_root>/.spawn-<ref>/`) that are
    /// currently being built. A `git_ref` spawn-test materializes such a
    /// container, then holds a build-pool slot while compiling the tree inside
    /// it. The container is the supervisor's to delete once idle, but it is
    /// NEVER safe to remove while that build is in flight.
    ///
    /// [`crate::routes::runners::spawn_test`] inserts the container path before
    /// the build and removes it after (success or failure). The spawn-worktree
    /// pruner ([`crate::spawn_worktree::prune_spawn_worktrees`]) consults this
    /// set as its active-build exclusion: any candidate present here is skipped,
    /// regardless of age. Keyed by the canonical container path string so the
    /// pruner's directory enumeration and the build wiring agree on identity.
    pub active_spawn_worktrees: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// Per-spawn-container **mutual exclusion** for materialize-then-build.
    ///
    /// `prepare_worktree` REUSES a container by `git checkout --force` +
    /// `git reset --hard`. Doing that while another spawn is mid-`cargo build`
    /// in the same container yanks source files out from under the compiler —
    /// producing failures that look exactly like code errors. Before
    /// 2026-07-22 this could only happen when two callers passed the SAME
    /// explicit `git_ref`; now that `origin/main` is the spawn-test default,
    /// every default spawn targets one shared `.spawn-origin_main` container,
    /// so it would be the common case rather than a rare collision.
    ///
    /// The guard is held across BOTH the materialization and the build, so a
    /// container is never reset while a build reads it. Scope is per container
    /// path: spawns of DIFFERENT refs never contend. Concurrent spawns of the
    /// SAME ref serialize — an accepted trade, since they would otherwise
    /// compile byte-identical source twice in parallel for no benefit.
    ///
    /// `std::sync::Mutex` guards only the map (never held across an await);
    /// the per-container `tokio::sync::Mutex` is the async lock actually held
    /// across the build. Entries are never evicted — one empty mutex per
    /// distinct ref ever spawned is negligible, and evicting a lock another
    /// task is queued on is the classic way to reintroduce the race.
    pub spawn_container_locks:
        std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// Cached build-artifact footprint snapshot (plan
    /// `2026-06-05-supervisor-build-artifact-footprint`). Walking the GB-scale
    /// `target-pool/slot-*` + `.spawn-*` trees is minutes-slow, so the snapshot
    /// is computed off the hot path: a background timer refreshes it (default
    /// 15 min) and `GET /builds?refresh_footprint=1` forces a synchronous
    /// recompute. The prune endpoints invalidate it after freeing bytes.
    /// `None` until the first refresh completes. Each snapshot carries its own
    /// `computed_at` so readers can judge staleness.
    pub footprint: RwLock<Option<crate::footprint::FootprintSnapshot>>,

    /// Cached origin/main drift for the LKG sha, refreshed on a timer.
    ///
    /// `GET /builds` used to compute this INLINE, and computing it runs
    /// `git fetch origin` -- a NETWORK call, with no deadline. Measured on this
    /// fleet 2026-08-10: `/builds` took 4.07s to 27.12s across 12 samples while
    /// `/health` answered in 0.0-0.1s and the disk queue sat at 0. The cost was
    /// neither disk nor memory; it was a per-request fetch. Any consumer with a
    /// timeout under ~30s saw intermittent failures, and the scoped-cleanup
    /// harness silently degraded to a guessed pool size because of it.
    ///
    /// A read endpoint must not perform network I/O. This is the same
    /// stale-while-revalidate shape as `footprint` above: the timer refreshes
    /// it, readers serve whatever is cached and NEVER block on git.
    pub origin_drift: RwLock<Option<OriginDriftSnapshot>>,
}

/// RAII guard that increments [`SupervisorState::active_sse_connections`]
/// on construction and decrements it on drop.
///
/// The guard is owned by the SSE stream itself (typically captured into the
/// stream's combinator state), so it lives exactly as long as the response
/// future. When axum drops the response — whether the client disconnected,
/// `take_until(shutdown_signal)` fired, or the server is being torn down —
/// the stream is dropped, the guard is dropped, and the counter ticks down.
pub struct SseConnectionGuard {
    counter: Arc<AtomicUsize>,
}

impl SseConnectionGuard {
    pub fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct RunnerState {
    /// Whether the runner's HTTP API answered on the most recent probe.
    ///
    /// **This is a two-state answer to a three-state question**, which is why
    /// [`Self::last_seen_responding_at`] exists beside it. `false` conflates
    /// "stopped" with "alive but not answering" — and the second is the
    /// wedge case, where the process is running fine and holding its port.
    /// Never render this alone as "gone"; use [`Self::liveness`].
    pub running: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub restart_requested: bool,
    /// A stop was requested for the current process. Latched by
    /// [`Self::latch_stop_request`] before any kill, cleared only by the next
    /// start ([`Self::clear_stop_request`]), so the exit monitor reads the
    /// flag as it stands when the monitor reads it, a failed stop's flag
    /// included. The lifecycle lock ([`ManagedRunner::lifecycle`]) orders the
    /// latch against a start: a stop arriving mid-start latches after that
    /// start recorded its spawn, and so targets it. It does not order the
    /// monitor's read, which happens outside that lock (see
    /// [`SpawnMarker::stop_requested_at`]).
    pub stop_requested: bool,
    /// When the earliest stop request still latched in `stop_requested` was
    /// made: set with the flag by [`Self::latch_stop_request`], which a later
    /// request leaves alone, and cleared with it by the next start. The exit
    /// monitor copies it onto the exited spawn's ring entry when it is dated
    /// at or before the exit ([`SpawnMarker::stop_requested_at`]).
    pub stop_requested_at: Option<DateTime<Utc>>,
    pub pid: Option<u32>,
    /// When the API was last observed responding.
    ///
    /// `None` means "never seen responding since the supervisor started
    /// tracking it", which is NOT the same as "not responding now". Set on
    /// every successful probe; never cleared. Together with `running` it
    /// separates a runner that stopped from one that went quiet and how long
    /// ago (plan `2026-08-07-runner-wedge-and-supervisor-hung-blindness`,
    /// Phase 3b).
    pub last_seen_responding_at: Option<DateTime<Utc>>,
}

/// What the supervisor actually knows about a runner's liveness.
///
/// Replaces reading the `running` boolean as if it were the whole truth. The
/// dashboard lost 7 hours to that conflation on 2026-08-08: it reported
/// `running: false, pid: null` for a 411 MB process that was holding its port
/// in the same JSON document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerLiveness {
    /// API answered on the most recent probe.
    Responding,
    /// API is silent but the port is still held — the process is alive and
    /// not answering. This is the wedge state, and the one that used to be
    /// mislabelled "stopped".
    UnresponsiveSince(DateTime<Utc>),
    /// API is silent and the port is not held. Positive evidence of absence.
    Stopped,
    /// API is silent, the port is not held, and we have never seen it
    /// respond — so we cannot distinguish "never started" from "stopped".
    Unknown,
}

impl RunnerLiveness {
    /// The wire name for this state.
    ///
    /// `UnresponsiveSince` renders as **`wedged`** — the word operators
    /// already use for it, and the one the escalation log
    /// (`RUNNER WEDGED: …`) emits. The timestamp rides alongside in
    /// `unresponsive_since` rather than being fused into the discriminant, so
    /// a consumer can branch on a plain string.
    pub fn state(&self) -> &'static str {
        match self {
            RunnerLiveness::Responding => "responding",
            RunnerLiveness::UnresponsiveSince(_) => "wedged",
            RunnerLiveness::Stopped => "stopped",
            RunnerLiveness::Unknown => "unknown",
        }
    }

    /// When the wedge window opened (the last time the API was seen
    /// responding), or `None` for every non-wedged state.
    pub fn unresponsive_since(&self) -> Option<DateTime<Utc>> {
        match self {
            RunnerLiveness::UnresponsiveSince(at) => Some(*at),
            _ => None,
        }
    }
}

/// Serialized as a **uniform object**, never as a bare string or an
/// externally-tagged variant:
///
/// ```json
/// {"state": "wedged", "unresponsive_since": "2026-08-30T04:11:07.918+00:00"}
/// ```
///
/// The derived (externally-tagged) form emitted `"responding"` for three
/// variants and `{"unresponsive_since": …}` for the fourth — a field whose
/// JSON *type* changes with its value, which every consumer would have to
/// branch on before it could read the state at all. One shape, one `state`
/// key, `unresponsive_since` always present (`null` when it does not apply).
///
/// The timestamp is rendered with `to_rfc3339()` rather than left to chrono's
/// serde impl, which spells UTC as `Z`. Every other timestamp on the
/// `GET /runners` row (`started_at`, `last_seen_responding_at`,
/// `watchdog.last_restart_at`) goes through `to_rfc3339()`, and one field in a
/// second spelling inside the same object is a parsing trap for no benefit.
impl serde::Serialize for RunnerLiveness {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("RunnerLiveness", 2)?;
        s.serialize_field("state", self.state())?;
        s.serialize_field(
            "unresponsive_since",
            &self.unresponsive_since().map(|t| t.to_rfc3339()),
        )?;
        s.end()
    }
}

/// Whether `last_seen_responding_at` belongs to the boot that began at
/// `started_at`: the ONE comparison every reader of "has this boot answered"
/// uses ([`RunnerState::answered_this_boot`],
/// `process::manager::answered_during_spawn`).
///
/// `started_at` absent is `None`, UNKNOWN: with no boot recorded there is
/// nothing to compare the stamp against, and an absent answer is not a "no".
/// Otherwise `Some(stamp >= started_at)`, and no stamp at all is `Some(false)`.
/// An answer dated at the instant the boot began counts for it.
pub fn answered_since(
    last_seen_responding_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
) -> Option<bool> {
    let started = started_at?;
    Some(last_seen_responding_at.is_some_and(|seen| seen >= started))
}

impl RunnerState {
    /// Whether the runner's HTTP API has answered during its current boot.
    ///
    /// `last_seen_responding_at` is never cleared, so it answers for the
    /// runner's whole history under this supervisor; this answers for the
    /// running process. `started_at` is set at every spawn and at adoption,
    /// so an adopted runner counts from its adoption. `None` is UNKNOWN (no
    /// boot recorded), never `false`. The stamp is dated from before the probe
    /// that saw the answer (`health_cache`), so an answer from a previous
    /// process to a probe in flight across a restart does not count here.
    pub fn answered_this_boot(&self) -> Option<bool> {
        answered_since(self.last_seen_responding_at, self.started_at)
    }

    /// Classify liveness from the API probe, the port observation, and the
    /// last-responding stamp.
    ///
    /// `port_open` and `api_responding` MUST both come from the SAME tick's
    /// actual probes. Passing a stale or assumed value re-creates the
    /// conflation this exists to remove.
    ///
    /// **`api_responding` is a parameter and not `self.running` on purpose.**
    /// It used to short-circuit on `self.running`, which is only synced to the
    /// probe for *user-managed* runners (`health_cache`'s refresher guards
    /// that sync on `!is_supervisor_managed`). For a temp/named runner
    /// `running` is latched `true` at spawn and never cleared while the
    /// process lives — so a wedged temp runner, the exact case this enum
    /// exists to name, classified as `Responding`. The bookkeeping flag says
    /// what the supervisor *believes it started*; only the probe says what
    /// answered.
    ///
    /// **`spawn_up` is the third observation**: whether the runner's newest
    /// spawn is still its current one and its pid is alive
    /// (`process::manager::newest_spawn_is_up`). It matters only with the port
    /// closed, where it keeps a live spawn that has not bound its listener yet
    /// from reading `Stopped`. `running` does not cover that case for the
    /// primary, whose `running` the health tick lowers from the failed probe.
    /// Callers read it from the same cache tick as the other two
    /// (`CachedPortHealth::spawn_up`), or pass `false` where there is no ring.
    pub fn liveness(
        &self,
        port_open: bool,
        api_responding: bool,
        spawn_up: bool,
    ) -> RunnerLiveness {
        if api_responding {
            return RunnerLiveness::Responding;
        }
        match (port_open, self.last_seen_responding_at) {
            (true, Some(at)) => RunnerLiveness::UnresponsiveSince(at),
            // Port held but never seen responding: still alive-and-quiet, and
            // the honest stamp is when we started watching, not "stopped".
            (true, None) => RunnerLiveness::Unknown,
            // Port closed. `Stopped` is a claim of POSITIVE evidence of
            // absence, so it may not be made while the supervisor still
            // believes it owns a live process, or while its newest spawn is
            // alive: a spawned runner that has not bound its port yet is
            // UNKNOWN, not gone.
            (false, _) if self.running || spawn_up => RunnerLiveness::Unknown,
            (false, Some(_)) => RunnerLiveness::Stopped,
            (false, None) => RunnerLiveness::Unknown,
        }
    }
}

pub struct WatchdogState {
    pub enabled: bool,
    pub restart_attempts: u32,
    pub last_restart_at: Option<DateTime<Utc>>,
    pub crash_history: Vec<DateTime<Utc>>,
    pub disabled_reason: Option<String>,

    // ── The serving arm (plan 2026-09-03-runner-zombie-serving-watchdog) ──
    //
    // Kept in the SAME struct as the crash arm — one watchdog, two triggers —
    // but with its OWN counters and its own disarm reason, so a crash loop and
    // a serving loop can never mask each other.
    /// Restarts taken by the serving arm, for the life of the process.
    pub serving_restart_attempts: u32,
    pub last_serving_restart_at: Option<DateTime<Utc>>,
    /// Rolling window of serving restarts, pruned like `crash_history`.
    pub serving_history: Vec<DateTime<Utc>>,
    /// Set when the serving-loop guard trips. Distinct from `disabled_reason`.
    pub serving_disabled_reason: Option<String>,
    /// True from the moment a serving `Restart` decision is taken until the
    /// detached task that runs it returns — either way.
    ///
    /// `restart_requested` cannot carry this: it is set INSIDE
    /// `restart_runner_by_id`, i.e. only after the task has been spawned, so
    /// two escalating ticks 2 s apart could both decide to restart before the
    /// first one latched anything.
    pub serving_restart_in_flight: bool,
    /// The last pre-bind hang alert, as `(the spawn's started_at, when it was
    /// sent)`, so the alert repeats on its own cadence and starts over for a
    /// new spawn. Alert bookkeeping only: nothing restarts on it.
    pub last_prebind_alert: Option<(DateTime<Utc>, DateTime<Utc>)>,
    /// The `started_at` of the spawn a never-answered alert has already gone out
    /// for on passing the first-healthy budget, so that alert fires once there and
    /// then keeps the escalation loop's slower cadence. Alert bookkeeping only.
    pub never_answered_alerted_spawn: Option<DateTime<Utc>>,
    /// The `started_at` of the spawn a first-healthy watchdog is tracking right now,
    /// set when `watch_first_healthy` starts and cleared on every way out of it, a
    /// panic included, by `FirstHealthyTracking`'s `Drop`. A sync mutex behind an
    /// `Arc` so that `Drop` can clear it without awaiting this struct's lock. Read
    /// with [`Self::first_healthy_tracks`]. Alert and restart bookkeeping only.
    pub first_healthy_tracked_spawn: std::sync::Arc<std::sync::Mutex<Option<DateTime<Utc>>>>,
}

pub struct BuildState {
    /// True when at least one build slot is busy.
    ///
    /// Maintained by `run_cargo_build`: set to true whenever a permit is
    /// acquired (first slot goes busy), cleared when the last active slot
    /// releases its permit. Existing readers (health endpoint, smart rebuild,
    /// overnight watchdog, process manager) observe this as a coarse
    /// "is the supervisor currently compiling anything" signal.
    pub build_in_progress: bool,
    pub build_error_detected: bool,
    pub last_build_error: Option<String>,
    pub last_build_at: Option<DateTime<Utc>>,
    pub last_build_stderr: Option<String>,
    /// Pre-permit memory-gate outcome of the most recent real (non-prewarm)
    /// build — set when that build claims its slot, alongside
    /// `last_build_at`. Surfaced as `GET /health` `build.last_mem_gate` so a
    /// build that ran because the gate's wait EXPIRED
    /// (`proceeded_after_wait`) is distinguishable from one that passed.
    /// `None` until the first build.
    pub last_mem_gate: Option<crate::build_monitor::MemGateOutcome>,
}

/// Metadata for an active build on a specific slot.
#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub started_at: DateTime<Utc>,
    pub requester_id: Option<String>,
    /// What kind of rebuild: "dev" or "exe" (custom-protocol/embedded frontend).
    pub rebuild_kind: String,
    /// What the pre-permit memory gate decided before this build claimed the
    /// slot. `None` only where no gate ran (test fixtures).
    pub mem_gate: Option<crate::build_monitor::MemGateOutcome>,
}

// State of the frontend (`npm run build`) for a specific slot.
//
// `BuildSlot::frontend_stale` = true means the most recent attempt to rebuild
// the frontend for this slot failed (e.g. tsc errors). The cargo build still
// proceeded, but it re-used whatever `dist/` happened to be on disk from a
// previous successful frontend build. Callers of `spawn-test {rebuild: true}`
// surface this so they don't debug a binary that embeds a stale UI. Cleared
// on the next successful `npm run build` for this slot.

/// Cap on the per-slot rolling duration window.
pub const RECENT_BUILD_SAMPLE_COUNT: usize = 10;

/// Cap on `SlotHistory::last_error_detail` size. When a captured cargo stderr
/// blob exceeds this, the front is truncated so the tail (where the actual
/// failure message lives in cargo output) is preserved.
pub const LAST_ERROR_DETAIL_MAX_BYTES: usize = 4 * 1024;

/// Cap on `SlotHistory::last_error_log` size. Sized for inline surfacing in
/// `GET /builds` so a single curl reveals the gist without dumping a wall of
/// text. The full last build's stderr lives on the slot (`BuildSlot::last_build_log`)
/// and is fetched on demand via `GET /builds/{slot_id}/log`.
pub const LAST_ERROR_LOG_MAX_BYTES: usize = 1024;

/// Cap on `BuildSlot::last_build_log` size. Hard upper bound to avoid
/// pathological retention if a build dumps gigabytes of output. Tail is
/// preserved (where the actual error message lives in cargo output).
pub const LAST_BUILD_LOG_MAX_BYTES: usize = 1024 * 1024;

/// Per-slot build duration history. In-memory only; resets on supervisor
/// restart. Used by `GET /builds` and the 503 `build_pool_full` response to
/// estimate wait times for callers.
#[derive(Debug, Clone)]
pub struct SlotHistory {
    pub recent_durations_secs: VecDeque<f64>,
    pub total_builds: u64,
    pub successful_builds: u64,
    pub last_completed_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Tail of the captured cargo stderr from the most recent failed build on
    /// this slot. Capped at [`LAST_ERROR_DETAIL_MAX_BYTES`]; oldest bytes are
    /// truncated to keep the tail (where the actual failure lives in cargo
    /// output). `None` until a failure is recorded with detail.
    pub last_error_detail: Option<String>,
    /// Short (≤[`LAST_ERROR_LOG_MAX_BYTES`]) tail of the most recent FAILED
    /// build's stderr. Surfaced inline in `GET /builds` so a single curl
    /// reveals the gist of the failure without paging through the full log.
    /// Cleared on the next successful build (success supersedes prior
    /// failures for that slot — the failure is no longer the current state).
    /// Use `GET /builds/{slot_id}/log` for the full untruncated log.
    pub last_error_log: Option<String>,
    /// Pre-permit memory-gate outcome of the most recent build (real or
    /// prewarm) to claim this slot. Surfaced as `GET /builds`
    /// `slots[].history.last_mem_gate`; persists after the build finishes so
    /// a fail-open (`proceeded_after_wait`) stays visible next to the build's
    /// result. `None` until a gated build claims the slot.
    pub last_mem_gate: Option<crate::build_monitor::MemGateOutcome>,
}

impl Default for SlotHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotHistory {
    pub fn new() -> Self {
        Self {
            recent_durations_secs: VecDeque::with_capacity(RECENT_BUILD_SAMPLE_COUNT),
            total_builds: 0,
            successful_builds: 0,
            last_completed_at: None,
            last_error: None,
            last_error_detail: None,
            last_error_log: None,
            last_mem_gate: None,
        }
    }

    pub fn record(
        &mut self,
        duration_secs: f64,
        success: bool,
        error: Option<String>,
        error_detail: Option<String>,
    ) {
        if self.recent_durations_secs.len() >= RECENT_BUILD_SAMPLE_COUNT {
            self.recent_durations_secs.pop_front();
        }
        self.recent_durations_secs.push_back(duration_secs);
        self.total_builds += 1;
        if success {
            self.successful_builds += 1;
            // Clear the inline error log on a green build. Rationale: the
            // surfaced field reflects the slot's CURRENT failure state, not
            // its history — a successful build supersedes any prior failure.
            // Full last-build log stays at the slot level for forensics.
            self.last_error_log = None;
        } else {
            // `last_error_log` is the 1 KiB summary surfaced inline in
            // `GET /builds`; `last_error_detail` is the longer 4 KiB view.
            // Both derive from the same captured blob so they tell a
            // consistent story.
            //
            // It is a HEAD, not a tail. `error_detail` is no longer raw cargo
            // stderr — it is a rendered document whose whole point is that the
            // cause is hoisted to the FRONT (see `build_diagnostics`). Taking
            // its last 1 KiB would hand `GET /builds` the tail of an excerpt
            // that is mostly linker flags, faithfully reproducing the
            // 2026-07-31 defect on the most-consulted surface of all.
            self.last_error_log = error_detail
                .as_deref()
                .map(|s| head_bytes_keep_utf8(s, LAST_ERROR_LOG_MAX_BYTES));
            self.last_error = error;
            self.last_error_detail = error_detail.map(truncate_error_detail_keep_tail);
        }
        self.last_completed_at = Some(Utc::now());
    }

    pub fn avg_duration_secs(&self) -> Option<f64> {
        if self.recent_durations_secs.is_empty() {
            return None;
        }
        let sum: f64 = self.recent_durations_secs.iter().sum();
        Some(sum / self.recent_durations_secs.len() as f64)
    }

    pub fn p50_duration_secs(&self) -> Option<f64> {
        if self.recent_durations_secs.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.recent_durations_secs.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(sorted[sorted.len() / 2])
    }
}

/// Return the FIRST `max_bytes` bytes of `s`, snapped back to a UTF-8
/// character boundary, with a marker when a cut was made.
///
/// The counterpart to [`tail_bytes_keep_utf8`], for the surfaces whose input
/// is a *rendered* diagnostic rather than raw cargo output. Those put the
/// cause at the front by construction, so keeping the tail throws away
/// exactly the part that was hoisted there to be seen.
pub fn head_bytes_keep_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[...truncated]", &s[..cut])
}

/// Return the last `max_bytes` bytes of `s`, snapped forward to a UTF-8
/// character boundary so the result is always valid UTF-8. Returns `s`
/// unchanged when it's already shorter than `max_bytes`.
pub fn tail_bytes_keep_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = s.len() - max_bytes;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    s[cut..].to_string()
}

/// Truncate `s` to at most [`LAST_ERROR_DETAIL_MAX_BYTES`] bytes by removing
/// from the front, preserving the tail. Truncates on a UTF-8 boundary so the
/// result is always valid UTF-8. When a cut is performed, a leading marker
/// is prepended so consumers know the prefix was elided.
pub fn truncate_error_detail_keep_tail(s: String) -> String {
    if s.len() <= LAST_ERROR_DETAIL_MAX_BYTES {
        return s;
    }
    let cut_target = s.len().saturating_sub(LAST_ERROR_DETAIL_MAX_BYTES);
    let mut cut = cut_target;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    let mut out = String::with_capacity(s.len() - cut + 32);
    out.push_str("[...truncated]\n");
    out.push_str(&s[cut..]);
    out
}

/// One slot in the parallel build pool.
///
/// Each slot has its own `CARGO_TARGET_DIR` so concurrent `cargo build`s do
/// not clobber each other's `target/`. The `busy` field is guarded by its
/// own lock so the slot state can be inspected without holding the larger
/// `SupervisorState::build` lock.
pub struct BuildSlot {
    pub id: usize,
    pub target_dir: PathBuf,
    pub busy: RwLock<Option<BuildInfo>>,
    /// Rolling per-slot build duration history. Separate lock from `busy` so
    /// `list_builds` can `try_read` it without blocking in-progress builds.
    pub history: RwLock<SlotHistory>,
    /// True when the most recent `npm run build` for this slot failed but the
    /// cargo build proceeded anyway using a stale `dist/` snapshot. Cleared on
    /// the next successful npm build.
    ///
    /// This is a failure-propagation surface — set in `run_build_inner` after
    /// a non-zero npm exit or spawn failure, cleared after a zero-exit npm
    /// build. Independent from the `busy`/`history` locks so readers can check
    /// it cheaply without blocking in-progress builds.
    pub frontend_stale: RwLock<bool>,
    /// Tail of cargo stderr captured by the most recent build attempt on this
    /// slot. Populated by `run_build_inner` on a non-zero cargo exit so the
    /// outer `run_cargo_build_with_requester` can fold it into
    /// [`SlotHistory::last_error_detail`] alongside the duration record.
    /// Cleared when populated; readers consume by `take`.
    pub last_build_stderr_capture: RwLock<Option<String>>,
    /// Full combined cargo stderr (and stdout, if cargo wrote any) of the
    /// most recent build attempt on this slot — success or failure. Capped
    /// at [`LAST_BUILD_LOG_MAX_BYTES`] to avoid pathological retention.
    /// Populated at the end of every cargo build, replacing whatever was
    /// there. Surfaced via `GET /builds/{slot_id}/log`.
    ///
    /// Tuple: `(captured_at, log_bytes)`. The timestamp is when the build
    /// finished, not when the log was read.
    pub last_build_log: RwLock<Option<(DateTime<Utc>, String)>>,
    /// Live broadcast of cargo stderr lines for THIS slot's currently-running
    /// build. Each subscriber sees every line `run_build_inner` reads from
    /// `child.stderr` for this slot's cargo invocation. Lines are pushed
    /// untagged (plain `String`) — the SSE handler at
    /// `GET /builds/{slot_id}/log/stream` wraps them as `event: cargo` data
    /// frames.
    ///
    /// Channel capacity is intentionally small (256): cargo's output isn't
    /// dense relative to typical SSE consumers, and the broadcast channel
    /// drops slow subscribers via `RecvError::Lagged` rather than blocking
    /// the cargo reader. Lagged events are surfaced to clients as a
    /// `event: lagged` frame so they know to fetch the full log from
    /// `GET /builds/{slot_id}/log` on completion.
    ///
    /// The channel is created once at slot construction and reused across
    /// builds — receivers from a previous build naturally start seeing the
    /// next build's lines, which is the desired "tail -f" semantics.
    pub log_stream: tokio::sync::broadcast::Sender<String>,
}

impl BuildSlot {
    /// Mark this slot busy with `info`. `busy` MUST be this slot's own
    /// `busy` write guard, which the caller holds across the call.
    ///
    /// The slot's `history.last_mem_gate` is written FIRST, while the busy
    /// lock is still held and before `busy` is set, so a reader that sees the
    /// new build in `busy` (`GET /builds` reads `busy`, then `history`) can
    /// never pair it with the previous build's `last_mem_gate`. Lock order is
    /// `busy` -> `history`; nothing takes them the other way round. A claim
    /// carrying no gate outcome leaves the history value untouched (`None`
    /// until a gated build claims the slot).
    pub async fn occupy(&self, busy: &mut Option<BuildInfo>, info: BuildInfo) {
        if let Some(gate) = info.mem_gate {
            self.history.write().await.last_mem_gate = Some(gate);
        }
        *busy = Some(info);
    }
}

/// Metadata for the last-known-good (LKG) runner binary preserved at
/// `target-pool/lkg/qontinui-runner.exe`.
///
/// The LKG copy is captured after every successful `cargo build` and is
/// independent of slot state — a subsequent failed build that clobbers a
/// slot's exe still leaves LKG intact. Callers consult `built_at` to decide
/// whether their pending changes are reflected in the LKG binary:
///
///   if max(mtime of changed files) <= LKG.built_at  ⇒ LKG includes the
///       changes; safe to spawn with `{rebuild: false, use_lkg: true}`.
///   if max(mtime of changed files) >  LKG.built_at  ⇒ LKG predates the
///       changes; the runner would be stale. Rebuild instead.
///
/// `built_at` is the wall-clock time the cargo build completed, recorded
/// just before the exe was copied into the LKG dir. `source_slot` is the
/// pool slot the build ran on. `exe_size` is the byte size of the LKG exe
/// — useful for spotting truncated or partial copies after a crash.
///
/// `sha` and `source` record the [`BuildProvenance`](crate::process::manager::BuildProvenance)
/// of the build that produced this LKG. Since the LKG promotion gate skips
/// override builds, `source` is always [`BuildSource::LiveTree`] for any LKG
/// written from this code forward — it is recorded from provenance (not
/// hard-coded) so the record is honest by construction. Both fields are
/// `#[serde(default)]` so legacy `lkg.json` files predating this change still
/// hydrate: their `sha` reads back as `None` and `source` defaults to
/// `LiveTree` (the only kind of build that has ever been deployed from LKG).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LkgInfo {
    pub built_at: DateTime<Utc>,
    pub source_slot: usize,
    pub exe_size: u64,
    /// Git SHA of the tree that was built, or `None` when the git probe failed
    /// (or when hydrated from a legacy sidecar without this field).
    #[serde(default)]
    pub sha: Option<String>,
    /// Which tree the LKG exe was built from. Always `LiveTree` going forward
    /// (override builds never reach LKG promotion); defaults to `LiveTree` when
    /// hydrated from a legacy sidecar without this field.
    #[serde(default = "default_lkg_source")]
    pub source: crate::process::manager::BuildSource,
}

/// Default `source` for an `LkgInfo` hydrated from a legacy `lkg.json` that
/// predates the provenance fields. LKG has only ever held live-tree deploys,
/// so `LiveTree` is the honest assumption for those records.
fn default_lkg_source() -> crate::process::manager::BuildSource {
    crate::process::manager::BuildSource::LiveTree
}

/// Pool of parallel build slots.
///
/// Acquisition protocol:
/// 1. Wait on `permits.acquire_owned().await` (blocks until a slot is free).
/// 2. Scan `slots` for the first one whose `busy.is_none()`, flip it to `Some(..)`.
/// 3. Run cargo build with `CARGO_TARGET_DIR = slot.target_dir`.
/// 4. On completion, flip `slot.busy = None`; the permit is dropped automatically.
///
/// `npm_lock` serializes frontend (`npm run build`) invocations: the Tauri
/// binary embeds a single `dist/` directory via `rust-embed`, and two
/// concurrent npm builds would corrupt it. The lock is held only for the npm
/// step (~12s), not the whole cargo build (~3min), so it's a much smaller
/// serialization point than the legacy global build flag.
pub struct BuildPool {
    pub slots: Vec<Arc<BuildSlot>>,
    pub permits: Arc<Semaphore>,
    pub npm_lock: Arc<tokio::sync::Mutex<()>>,
    /// Number of callers currently waiting on `permits.acquire_owned()`.
    /// Incremented by `spawn-test` handler before awaiting, decremented after
    /// acquiring or timing out.
    pub queue_depth: Arc<AtomicUsize>,
    /// Number of callers currently waiting on `npm_lock.lock_owned()` (the
    /// serialized frontend `pnpm run build` step). Bracketed around the lock
    /// acquire in `build_monitor::run_build_inner` exactly as `queue_depth`
    /// brackets the permit acquire — `npm_lock` is a bare `Mutex<()>` with no
    /// native waiter accounting, so this counter is what lets `GET /builds`
    /// surface frontend-lock contention (`npm_lock_waiters`) and distinguish
    /// frontend starvation from cargo-slot exhaustion.
    pub npm_lock_waiters: Arc<AtomicUsize>,
    /// The slot id whose target dir holds the most recently successfully built
    /// binary. Used by `spawn-test {rebuild: false}` to locate the exe to copy.
    /// `None` at startup until the first successful build.
    pub last_successful_slot: RwLock<Option<usize>>,
    /// Metadata for the preserved last-known-good runner exe at
    /// `target-pool/lkg/qontinui-runner.exe`. Updated after every successful
    /// build; read by `spawn_test` when `use_lkg: true`. Hydrated from the
    /// `lkg.json` sidecar at supervisor startup so the value survives
    /// restarts. `None` only when no successful build has ever produced an
    /// LKG (fresh checkout) or the sidecar failed to load.
    pub last_known_good: RwLock<Option<LkgInfo>>,
}

impl BuildPool {
    pub fn new(config: &SupervisorConfig) -> Self {
        let pool_size = config.build_pool.pool_size.max(1);
        let mut slots = Vec::with_capacity(pool_size);
        for id in 0..pool_size {
            let target_dir = config.runner_slot_target_dir(id);
            // Create the dir eagerly so cargo doesn't race on it.
            if let Err(e) = std::fs::create_dir_all(&target_dir) {
                tracing::warn!(
                    "Failed to create build slot target dir {:?}: {}",
                    target_dir,
                    e
                );
            }
            let (log_tx, _) = tokio::sync::broadcast::channel::<String>(256);
            slots.push(Arc::new(BuildSlot {
                id,
                target_dir,
                busy: RwLock::new(None),
                history: RwLock::new(SlotHistory::new()),
                frontend_stale: RwLock::new(false),
                last_build_stderr_capture: RwLock::new(None),
                last_build_log: RwLock::new(None),
                log_stream: log_tx,
            }));
        }
        // Try to hydrate LKG metadata from the on-disk sidecar. We do this
        // synchronously in `BuildPool::new` (which runs once at startup) so
        // the field is correctly populated before any HTTP handler can read
        // it. A missing or unparsable sidecar is non-fatal — the LKG will
        // simply be considered absent until the next successful build.
        let lkg = load_lkg_from_disk(config);

        Self {
            slots,
            permits: Arc::new(Semaphore::new(pool_size)),
            npm_lock: Arc::new(tokio::sync::Mutex::new(())),
            queue_depth: Arc::new(AtomicUsize::new(0)),
            npm_lock_waiters: Arc::new(AtomicUsize::new(0)),
            last_successful_slot: RwLock::new(None),
            last_known_good: RwLock::new(lkg),
        }
    }

    /// Scan slots and return a snapshot of (slot_id, Option<BuildInfo>) pairs
    /// for the `GET /builds` endpoint.
    pub async fn snapshot(&self) -> Vec<(usize, PathBuf, Option<BuildInfo>)> {
        let mut out = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            let info = slot.busy.read().await.clone();
            out.push((slot.id, slot.target_dir.clone(), info));
        }
        out
    }

    /// Build-pool occupancy for the resource sample published to coord.
    ///
    /// Derived from [`BuildPool::snapshot`] — the same per-slot `busy` scan
    /// `GET /builds` renders its `slots[]` / `active_builds` from — and the
    /// same `queue_depth` counter it reports as `queued`. There is deliberately
    /// no second accounting of the pool: "busy" means exactly what the
    /// dashboard already means by it, so a sample and the endpoint can never
    /// disagree about whether this machine has room.
    ///
    /// Note the count comes from `slot.busy`, NOT from
    /// `permits.available_permits()`. The semaphore permit is released after
    /// `busy` is cleared, so the two views diverge transiently mid-release;
    /// `GET /builds` derives its `available_permits` from the slot scan for
    /// exactly this reason and exposes the raw semaphore separately.
    pub async fn occupancy(&self) -> crate::footprint::BuildPoolOccupancy {
        let slots = self.snapshot().await;
        let busy = slots.iter().filter(|(_, _, info)| info.is_some()).count();
        crate::footprint::BuildPoolOccupancy {
            build_slots_total: Some(slots.len() as u32),
            build_slots_busy: Some(busy as u32),
            build_queue_depth: Some(self.queue_depth.load(Ordering::Relaxed) as u32),
        }
    }

    /// Returns true when at least one slot has its `frontend_stale` flag set —
    /// i.e. its most recent `npm run build` failed but a cargo build proceeded
    /// anyway using a pre-existing `dist/`. Surfaced in `GET /builds` and
    /// `GET /health` so callers can notice a potentially-stale UI.
    pub async fn any_slot_has_stale_frontend(&self) -> bool {
        for slot in &self.slots {
            if *slot.frontend_stale.read().await {
                return true;
            }
        }
        false
    }

    /// Claim the first idle slot, marking it busy with the given metadata.
    /// Assumes the caller has already acquired a permit, so at least one slot
    /// is idle.
    pub async fn claim_idle_slot(&self, info: BuildInfo) -> Arc<BuildSlot> {
        for slot in &self.slots {
            let mut busy = slot.busy.write().await;
            if busy.is_none() {
                slot.occupy(&mut busy, info.clone()).await;
                return slot.clone();
            }
        }
        // Unreachable: semaphore guarantees an idle slot exists.
        panic!("claim_idle_slot called with no idle slots; semaphore invariant violated");
    }
}

/// Hydrate `LkgInfo` from `target-pool/lkg/lkg.json`. Returns `None` if the
/// sidecar is missing, unreadable, malformed, or its companion exe doesn't
/// exist. Called once during `BuildPool::new`. A missing LKG is benign — it
/// just means the supervisor was started before any successful build, or the
/// dir was wiped. Build success will rewrite both files.
fn load_lkg_from_disk(config: &SupervisorConfig) -> Option<LkgInfo> {
    let exe = config.lkg_exe_path();
    let meta = config.lkg_metadata_path();
    if !exe.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(&meta).ok()?;
    let parsed: LkgInfo = serde_json::from_str(&raw).ok()?;
    Some(parsed)
}

pub struct AiState {
    pub provider: String,
    pub model: String,
    pub auto_debug_enabled: bool,
}

pub struct ExpoState {
    pub process: Option<Child>,
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<DateTime<Utc>>,
    pub port: u16,
}

/// Compute the supervisor build identifier from the embedded `dist/index.html`.
///
/// rust-embed captures the file's mtime at compile time, so this string changes
/// every time the supervisor's frontend (`npm run build`) + cargo build pair
/// re-runs. Format: RFC3339 UTC timestamp (`2026-04-25T12:34:56+00:00`). When
/// the file is missing or rust-embed couldn't capture an mtime on this
/// platform, fall back to fixed sentinel strings so callers can still
/// distinguish "no signal" from a real change.
pub fn compute_build_id() -> String {
    use crate::routes::dashboard::Assets;
    match Assets::get("index.html") {
        Some(file) => match file.metadata.last_modified() {
            Some(secs) => {
                // rust-embed-utils returns seconds since UNIX epoch.
                match chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0) {
                    Some(dt) => dt.to_rfc3339(),
                    None => "embed-error".to_string(),
                }
            }
            None => "embed-no-mtime".to_string(),
        },
        None => "unknown".to_string(),
    }
}

/// Env var name that admits the supervisor's debug-only HTTP endpoints
/// (currently just `POST /control/dev/emit-build-id`). Set to `1` to enable;
/// any other value (or unset) keeps the endpoints returning 403. The env is
/// read once at startup and cached on
/// [`SupervisorState::debug_endpoints_enabled`] so per-request env reads
/// stay off the hot path.
pub const DEBUG_ENDPOINTS_ENV: &str = "QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS";

/// Read the debug-endpoints gate from the environment. Returns true only when
/// `DEBUG_ENDPOINTS_ENV` is exactly `"1"`. Empty / unset / `"0"` / anything
/// else is treated as disabled. We intentionally do not accept `"true"` /
/// `"yes"` to keep the activation surface as narrow and unambiguous as
/// possible — this gate guards endpoints that bypass production safety
/// checks, so the on-state must be deliberate.
pub fn read_debug_endpoints_env() -> bool {
    std::env::var(DEBUG_ENDPOINTS_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Default platform-appropriate path for the persisted `boot.id` file.
///
/// - Windows: `%LOCALAPPDATA%\qontinui-supervisor\boot.id`
/// - Linux:   `~/.local/share/qontinui-supervisor/boot.id`
/// - macOS:   `~/Library/Application Support/qontinui-supervisor/boot.id`
///
/// Falls back to `./qontinui-supervisor/boot.id` if `dirs::data_local_dir()`
/// can't determine a home/data dir (extremely rare — e.g. a stripped-down
/// container with no `HOME` set). The fallback keeps the supervisor running
/// rather than panicking; persistence may simply not survive a CWD change.
fn default_boot_id_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("qontinui-supervisor")
        .join("boot.id")
}

/// Load a previously-persisted `boot_id` from `path`, or generate a fresh
/// UUID v4 and persist it there. Failure to write the new UUID is non-fatal —
/// the in-memory UUID is returned and a warning is logged; the next startup
/// will simply generate a new one. The returned string is always a
/// well-formed UUID.
///
/// This is the testable variant of [`load_or_create_boot_id`]. The public
/// function calls this with [`default_boot_id_path()`].
pub fn load_or_create_boot_id_at(path: &std::path::Path) -> String {
    if let Ok(contents) = std::fs::read_to_string(path) {
        let trimmed = contents.trim();
        if uuid::Uuid::parse_str(trimmed).is_ok() {
            return trimmed.to_string();
        }
        tracing::warn!(
            path = %path.display(),
            "boot.id file exists but does not contain a valid UUID; regenerating"
        );
    }
    let fresh = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                path = %parent.display(),
                error = %e,
                "failed to create boot.id parent directory; boot_id will not persist"
            );
            return fresh;
        }
    }
    if let Err(e) = std::fs::write(path, &fresh) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to write boot.id; boot_id will not persist across restarts"
        );
    }
    fresh
}

/// Resolve the persistent `boot_id` for this supervisor installation.
/// Reads the persisted UUID from the platform-default path if present,
/// otherwise generates and persists a fresh one. See
/// [`SupervisorState::boot_id`] for semantics.
pub fn load_or_create_boot_id() -> String {
    load_or_create_boot_id_at(&default_boot_id_path())
}

/// Identity of a spawn-test build for single-flight purposes: **who** asked,
/// and **what tree** they asked to have compiled.
///
/// Both halves are load-bearing:
///
/// * `requester_id` — two different agents that happen to want the same tree
///   each want their OWN runner, so they must not collapse. A request with no
///   `requester_id` produces no key at all (see
///   `routes::runners::spawn_dedup_key`): anonymous requests are
///   indistinguishable from one another, and joining them would hand one
///   caller a runner another caller owns.
/// * `build_target` — derived from the resolved `SpawnBuildSource` (plus the
///   `frontend_only` flag, which changes what is compiled from the same tree),
///   so `origin/main`, an explicit ref, a caller's worktree path and the live
///   tree are four distinct targets that never join each other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpawnDedupKey {
    pub requester_id: String,
    pub build_target: String,
}

/// The in-flight spawn-test build that a duplicate request joins: the same
/// `build_id` (= submission id), the same reserved runner and the same action.
#[derive(Debug, Clone)]
pub struct SpawnInflight {
    /// `build_id` — the build-submission id, identical on the sync, async and
    /// poll paths.
    pub submission_id: uuid::Uuid,
    /// Registry id of the runner reserved for this build.
    pub runner_id: String,
    /// Port reserved for that runner.
    pub port: u16,
    /// The dev-action trial this build runs under. A joiner minted none of its
    /// own, and the answer it gets names this one, because a request that joins
    /// is usually the retry of a caller who never read the first answer.
    pub action: MintedAction,
}

/// The in-flight `POST /runner/fix-and-rebuild` a duplicate request joins.
#[derive(Debug, Clone)]
pub struct FixAndRebuildInflight {
    /// The detached rebuild's build-submission id.
    pub submission_id: uuid::Uuid,
    /// The `build` trial it runs under, named in a joiner's answer.
    pub action: MintedAction,
}

/// Result of asking for the spawn-test single flight for one key.
pub enum SpawnTicket<'a> {
    /// An equivalent build is already running: join it. Nothing was claimed.
    Join(SpawnInflight),
    /// This request owns the key. It MUST call [`SpawnClaimGuard::commit`] once
    /// its build is registered, or drop the guard to abandon the claim.
    Claim(SpawnClaimGuard<'a>),
}

/// Exclusive hold on one [`SpawnDedupKey`] while its build is being registered.
///
/// The index's write lock is held for the guard's whole life, so a concurrent
/// same-key request blocks here and then observes the committed entry rather
/// than starting a second build. Dropping the guard without committing (the
/// handler bailed out on a validation error, no port was free, …) leaves the
/// key free for the next request — an abandoned claim never wedges a key.
///
/// An unkeyed request (anonymous, or one that claims no build slot) gets a
/// guard with `key: None`: it still passes through the same code path, and
/// `commit` records nothing.
pub struct SpawnClaimGuard<'a> {
    key: Option<SpawnDedupKey>,
    index: tokio::sync::RwLockWriteGuard<'a, HashMap<SpawnDedupKey, SpawnInflight>>,
}

impl SpawnClaimGuard<'_> {
    /// Record the build this claim started, so duplicates can join it.
    pub fn commit(mut self, inflight: SpawnInflight) {
        if let Some(key) = self.key.take() {
            self.index.insert(key, inflight);
        }
    }
}

impl SupervisorState {
    /// Enter the `(requester, build target)` single flight for a spawn-test
    /// request: either join the build already running for that key, or take an
    /// exclusive claim on it.
    ///
    /// `key: None` (anonymous request, or one that claims no build-pool slot)
    /// always yields a claim that records nothing — such requests never join
    /// and are never joined.
    ///
    /// This serializes spawn-test *admission*, not the builds themselves (which
    /// run detached on the build pool). The handler already serializes on the
    /// runner-registry write lock immediately afterwards, so no new class of
    /// contention is introduced.
    pub async fn claim_spawn_build(&self, key: Option<SpawnDedupKey>) -> SpawnTicket<'_> {
        let mut index = self.spawn_test_inflight.write().await;
        if let Some(key) = &key {
            if let Some(existing) = index.get(key).cloned() {
                if self.spawn_build_in_flight(existing.submission_id).await {
                    return SpawnTicket::Join(existing);
                }
                // Terminal but never released — only reachable if a release was
                // lost. Drop it rather than refusing this key forever.
                index.remove(key);
            }
        }
        SpawnTicket::Claim(SpawnClaimGuard { key, index })
    }

    /// Is this build submission still running?
    ///
    /// An id the store does not know is **finished**, and this is evidence rather
    /// than a default. `build_submissions::submit_spawn` registers the submission
    /// before the claim is committed, so every index entry had a store row first,
    /// and the store evicts only terminal rows. Reading the absence as "running"
    /// would make every later request for this key join a build that has ended,
    /// until the supervisor restarted. Entries are normally removed on completion,
    /// on every exit, a panic included (`finish_spawn_build`), so this answers
    /// for one whose release was lost.
    async fn spawn_build_in_flight(&self, submission_id: uuid::Uuid) -> bool {
        match self.build_submissions.get(&submission_id).await {
            Some(arc) => !arc.read().await.status.is_terminal(),
            None => false,
        }
    }

    /// Snapshot the spawn-test single-flight index, optionally narrowed to one
    /// requester.
    ///
    /// **Why this is public.** The index already holds exactly the triple a
    /// caller needs to recover from a lost `POST /runners/spawn-test` answer —
    /// `(submission_id, runner_id, port)` — keyed by the requester. A caller
    /// whose connection died before the response was written (client timeout,
    /// supervisor restart) otherwise has no way to learn the runner id it just
    /// created, and without the id it cannot reach `/runners/{id}/logs` or
    /// `/runners/{id}/stop`: it created a runner it cannot follow or clean up.
    /// Polling `GET /runners` and pattern-matching on `requester_id` is the
    /// workaround that recovery previously required, and it is a guess — it
    /// cannot distinguish this request's runner from that requester's previous
    /// one. This is the exact answer.
    ///
    /// Returns entries sorted by key so the response is stable across calls.
    pub async fn spawn_inflight_snapshot(
        &self,
        requester_id: Option<&str>,
    ) -> Vec<(SpawnDedupKey, SpawnInflight)> {
        let index = self.spawn_test_inflight.read().await;
        let mut out: Vec<(SpawnDedupKey, SpawnInflight)> = index
            .iter()
            .filter(|(k, _)| requester_id.is_none_or(|r| k.requester_id == r))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| {
            (&a.0.requester_id, &a.0.build_target).cmp(&(&b.0.requester_id, &b.0.build_target))
        });
        out
    }

    /// Drop a spawn-test single-flight entry once its build is terminal, so the
    /// next request for the same key starts a fresh build.
    ///
    /// Removes only an entry that still points at `submission_id`, so a build
    /// finishing after a newer one claimed the key cannot evict its successor.
    ///
    /// Takes the id's cell, not the id, and reads it under the index lock:
    /// `spawn_test` fills the cell while its claim holds that lock, so only there
    /// is it certain to be filled, however fast the build finished. An id read
    /// before the lock could miss it, and the claim would then publish an entry
    /// for a build that had already released.
    pub async fn release_spawn_inflight(
        &self,
        key: &SpawnDedupKey,
        submission_id: &std::sync::OnceLock<uuid::Uuid>,
    ) {
        let mut index = self.spawn_test_inflight.write().await;
        let Some(submission_id) = submission_id.get() else {
            return;
        };
        if index
            .get(key)
            .is_some_and(|e| e.submission_id == *submission_id)
        {
            index.remove(key);
        }
    }

    pub fn new(config: SupervisorConfig) -> Self {
        let watchdog_enabled = config.watchdog_enabled_at_start;
        let auto_debug = config.auto_debug;
        let expo_port = config.expo_port;
        let (health_tx, _) = broadcast::channel(16);
        let (shutdown_tx, _) = broadcast::channel(1);
        // Capacity 8 with `broadcast` semantics: lagging receivers see Lagged,
        // not Closed, and we ignore Lagged in the SSE consumer (it just means
        // we missed an injection — fine for a debug-only "kick the watcher"
        // signal).
        let (synthetic_build_id_tx, _) = broadcast::channel::<String>(8);
        let debug_endpoints_enabled = read_debug_endpoints_env();
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create HTTP client");

        // Build multi-runner map from config. Thread the optional log dir
        // through so each ManagedRunner's LogState gets a per-runner append
        // file at <log_dir>/<runner_id>.log.
        //
        // Crash-only watchdog default scope: under `--watchdog` only the
        // PRIMARY is armed for crash auto-restart. Named/temp/external
        // runners default off (cheap to respawn, often killed deliberately
        // by agents) — arm one explicitly via `POST /runners/{id}/watchdog`.
        let log_dir = config.log_dir.as_deref();
        let mut runners_map = HashMap::new();
        for rc in &config.runners {
            let managed = Arc::new(ManagedRunner::new_with_log_dir(
                rc.clone(),
                watchdog_enabled && rc.kind().is_primary(),
                log_dir,
            ));
            runners_map.insert(rc.id.clone(), managed);
        }

        let build_pool = BuildPool::new(&config);

        // Create the kill-on-exit JobObject. On Windows this enforces that
        // every supervisor-spawned *temp* runner dies when the supervisor
        // exits (graceful, panic, force-kill); user-owned runners (primary,
        // named, external) are never assigned to it and survive. On
        // non-Windows it's a no-op stub that reaps NOTHING, and the startup
        // line says so: it is derived from `RunnerJob::REAPS_ON_SUPERVISOR_EXIT`
        // and names what does reap there (`job_created_message`). Failure to
        // create is logged loudly but never aborts startup — the supervisor
        // still functions, just without the safety net.
        //
        // The `state.logs` collector isn't constructed yet at this point
        // (it's initialized inside the `Self { ... }` block below), so we
        // can't directly emit to the dashboard log stream. Instead, we
        // capture the success/failure message into `pending_startup_logs`,
        // which `flush_pending_startup_logs` drains right after the state
        // is wrapped in an `Arc` (see `main.rs`).
        let mut startup_logs: Vec<(LogLevel, String)> = Vec::new();
        let ephemeral_job = match RunnerJob::create() {
            Ok(j) => {
                let msg =
                    crate::process::job::job_created_message(RunnerJob::REAPS_ON_SUPERVISOR_EXIT);
                tracing::info!("{}", msg);
                startup_logs.push((LogLevel::Info, msg));
                Some(Arc::new(j))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to create ephemeral-runner JobObject — supervisor exit will NOT \
                     terminate spawned temp runners; orphans may linger and lock build slots"
                );
                startup_logs.push((
                    LogLevel::Warn,
                    format!(
                        "Failed to create ephemeral-runner JobObject: {} — supervisor exit will \
                         NOT terminate spawned temp runners",
                        e
                    ),
                ));
                None
            }
        };

        Self {
            config,
            runners: RwLock::new(runners_map),
            runner: RwLock::new(RunnerState::new()),
            watchdog: RwLock::new(WatchdogState::new(watchdog_enabled)),
            build: RwLock::new(BuildState::new()),
            build_pool,
            build_submissions: Arc::new(BuildSubmissionStore::new(1000)),
            bazel_remote: crate::bazel_remote::BazelRemoteClient::from_env(),
            cache_telemetry: crate::cache_telemetry::CacheTelemetry::new(),
            ai: RwLock::new(AiState::new(auto_debug)),
            expo: RwLock::new(ExpoState::new(expo_port)),
            diagnostics: RwLock::new(DiagnosticsState::new()),
            dev_actions: RwLock::new(crate::dev_action::ActionStore::new()),
            evaluation: RwLock::new(EvaluationState::new()),
            velocity_tests: RwLock::new(VelocityTestState::new()),
            velocity_improvement: RwLock::new(VelocityImprovementState::new()),
            command_relay: CommandRelay::new(),
            logs: LogState::new(),
            health_tx,
            shutdown_tx,
            shutdown_latched: AtomicBool::new(false),
            shutdown_sweep: tokio::sync::OnceCell::new(),
            cached_health: RwLock::new(CachedPortHealth::default()),
            cached_runner_health: RwLock::new(Vec::new()),
            health_cache_notify: Notify::new(),
            http_client,
            runner_api_port: crate::config::RUNNER_API_PORT,
            test_auto_login: RwLock::new(None),
            stopped_runners: Arc::new(RwLock::new(HashMap::new())),
            boot_id: load_or_create_boot_id(),
            build_id: compute_build_id(),
            ephemeral_job,
            runner_isolation_probe: RwLock::new(None),
            pending_startup_logs: std::sync::Mutex::new(startup_logs),
            active_sse_connections: Arc::new(AtomicUsize::new(0)),
            debug_endpoints_enabled,
            supervisor_started_at: std::time::SystemTime::now(),
            synthetic_build_id_tx,
            ci_runner_state: RwLock::new(CiRunnerState::default()),
            fix_and_rebuild_inflight: RwLock::new(None),
            spawn_test_inflight: RwLock::new(HashMap::new()),
            active_spawn_worktrees: std::sync::Mutex::new(std::collections::HashSet::new()),
            spawn_container_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
            footprint: RwLock::new(None),
            origin_drift: RwLock::new(None),
        }
    }

    /// Recompute the build-artifact footprint snapshot and store it on
    /// `self.footprint`. The walk is CPU/IO-bound and slow on real trees, so it
    /// runs inside `spawn_blocking`; the only async work is taking the write
    /// lock to publish the result. Best-effort: returns the fresh snapshot to
    /// the caller as well (so the on-demand `?refresh_footprint=1` path can
    /// serialize it immediately without a re-read).
    pub async fn refresh_footprint(
        self: &std::sync::Arc<Self>,
    ) -> crate::footprint::FootprintSnapshot {
        let this = self.clone();
        let mut snapshot = tokio::task::spawn_blocking(move || {
            let snapshot = crate::footprint::compute_snapshot(&this.config);
            persist_one_target_estimate(&this.config, &snapshot);
            snapshot
        })
        .await
        .unwrap_or_else(|_| crate::footprint::compute_snapshot(&self.config));
        // Occupancy is read AFTER the walk, deliberately. The walk is
        // minutes-slow on real trees, and `computed_at` is stamped at its end —
        // a slot count taken before it would ship under a timestamp minutes
        // newer than itself, advertising slots that filled while it ran. It is
        // a cheap async read, so taking it here costs nothing.
        snapshot.build_pool = self.build_pool.occupancy().await;
        *self.footprint.write().await = Some(snapshot.clone());
        snapshot
    }

    /// Recompute the origin/main drift snapshot and store it on
    /// `self.origin_drift`. Runs `git fetch` (bounded by
    /// `QONTINUI_SUPERVISOR_GIT_FETCH_TIMEOUT_SECS`), so it belongs on the
    /// timer, never on a request path -- see the field's doc comment.
    ///
    /// Returns `None` when there is nothing to compute (no LKG sha recorded, or
    /// `project_dir` has no parent), leaving any previous snapshot in place: a
    /// transiently missing LKG should not erase a good reading.
    pub async fn refresh_origin_drift(&self) -> Option<OriginDriftSnapshot> {
        let lkg_sha: Option<String> = self
            .build_pool
            .last_known_good
            .read()
            .await
            .as_ref()
            .and_then(|info| info.sha.clone());
        let repo_root = self.config.project_dir.parent()?.to_path_buf();
        let sha = lkg_sha?;

        let drift = crate::git_provenance::origin_main_drift(&repo_root, &sha).await;
        let snapshot = OriginDriftSnapshot {
            built_sha: sha,
            drift,
            computed_at: chrono::Utc::now(),
        };
        *self.origin_drift.write().await = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Drain `pending_startup_logs` into `state.logs`.
    ///
    /// Called once from `main.rs` right after `SupervisorState::new` wraps
    /// the state in an `Arc`. Messages captured during synchronous
    /// construction (currently just JobObject create success/failure) are
    /// routed through the same `state.logs.emit` path that the rest of the
    /// supervisor uses, so they appear in `/logs/history`, the SSE stream,
    /// and any persistent log file.
    pub async fn flush_pending_startup_logs(&self) {
        let drained: Vec<(LogLevel, String)> = {
            let mut guard = match self.pending_startup_logs.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *guard)
        };
        for (level, msg) in drained {
            self.logs.emit(LogSource::Supervisor, level, msg).await;
        }
    }

    pub fn notify_health_change(&self) {
        let _ = self.health_tx.send(());
    }

    /// Get (creating on first use) the mutual-exclusion lock for a spawn
    /// container. See [`SupervisorState::spawn_container_locks`] for why this
    /// exists — in short, `prepare_worktree` hard-resets a reused container and
    /// must never do so while a build is reading it.
    ///
    /// Returns an `Arc` so the caller can `lock_owned()` and hold the guard
    /// across the whole materialize+build sequence. The map mutex is released
    /// before the caller ever awaits, so this never blocks the runtime.
    pub fn spawn_container_lock(&self, container: &std::path::Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self
            .spawn_container_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.entry(container.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Future that completes when a shutdown is signaled on `shutdown_tx`.
    ///
    /// Long-lived streams (SSE handlers, polling loops) call this to learn
    /// that the supervisor is exiting so they can terminate promptly. This
    /// is what unblocks `axum::serve(..).with_graceful_shutdown(..)`'s
    /// drain phase: until every in-flight response future resolves, axum
    /// keeps the listener alive and `serve_future.await` does not return.
    /// Without this hook, SSE handlers (`/health/stream`, `/logs/stream`,
    /// `/supervisor-bridge/commands/stream`, etc.) hold their connections
    /// open forever and the supervisor process lingers for 30+ seconds
    /// after `POST /supervisor/shutdown` fires.
    ///
    /// Resolution order:
    /// 1. **Latched check** — if `shutdown_latched` is already `true`,
    ///    return immediately. This handles handlers that subscribed AFTER
    ///    the broadcast already fired (broadcast channels don't replay).
    /// 2. **Subscribe + recv** — otherwise, subscribe a fresh receiver and
    ///    await. `recv()` returning either `Ok(())` (broadcast) or
    ///    `Err(_)` (sender dropped) both indicate "shut down now"; we
    ///    treat them identically.
    pub async fn shutdown_signal(&self) {
        // Subscribe BEFORE the latch check so we don't lose a broadcast
        // that fires between the check and the recv.
        let mut rx = self.shutdown_tx.subscribe();
        if self
            .shutdown_latched
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let _ = rx.recv().await;
    }

    /// Mark the supervisor as shutting down and broadcast to all subscribers.
    ///
    /// Sets the latched flag *before* the broadcast so handlers that race in
    /// (subscribing between the broadcast and observing it) still see the
    /// latch on their next poll. Idempotent — repeated calls are cheap and
    /// safe; the broadcast just goes to whoever subscribed since last time.
    pub fn signal_shutdown(&self) {
        self.shutdown_latched
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.shutdown_tx.send(());
    }

    /// Get a managed runner by ID.
    pub async fn get_runner(&self, id: &str) -> Option<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners.get(id).cloned()
    }

    /// Get the primary runner.
    pub async fn get_primary(&self) -> Option<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners
            .values()
            .find(|r| r.config.kind().is_primary())
            .cloned()
    }

    /// Get all runners as a Vec.
    pub async fn get_all_runners(&self) -> Vec<Arc<ManagedRunner>> {
        let runners = self.runners.read().await;
        runners.values().cloned().collect()
    }
}

impl Default for RunnerState {
    fn default() -> Self {
        Self::new()
    }
}

impl RunnerState {
    pub fn new() -> Self {
        Self {
            running: false,
            started_at: None,
            restart_requested: false,
            stop_requested: false,
            stop_requested_at: None,
            pid: None,
            last_seen_responding_at: None,
        }
    }

    /// Latch a stop request: `stop_requested` set, and `stop_requested_at`
    /// set to `now` unless an earlier request already set it, so it keeps the
    /// EARLIEST request still latched. Every site that asks a runner to stop
    /// (`stop_runner_by_id`, through which every restart passes, and the
    /// max-age sweep's kill) latches through here.
    pub fn latch_stop_request(&mut self, now: DateTime<Utc>) {
        self.stop_requested = true;
        if self.stop_requested_at.is_none() {
            self.stop_requested_at = Some(now);
        }
    }

    /// Clear the stop request, flag and time together. Only a start (or an
    /// adoption, which is the supervisor taking a live runner on) does this:
    /// clearing it when a stop completes would race the exit monitor's read.
    /// A start's clear can race that read too, when the monitor of an earlier
    /// spawn wakes after the start, so a start first carries the request onto
    /// that spawn's ring entry ([`SpawnRing::carry_stop_request`]).
    pub fn clear_stop_request(&mut self) {
        self.stop_requested = false;
        self.stop_requested_at = None;
    }
}

impl WatchdogState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            restart_attempts: 0,
            last_restart_at: None,
            crash_history: Vec::new(),
            disabled_reason: None,
            serving_restart_attempts: 0,
            last_serving_restart_at: None,
            serving_history: Vec::new(),
            serving_disabled_reason: None,
            serving_restart_in_flight: false,
            last_prebind_alert: None,
            never_answered_alerted_spawn: None,
            first_healthy_tracked_spawn: Default::default(),
        }
    }

    /// The `started_at` of the spawn a first-healthy watchdog is tracking right
    /// now, or `None` when none is. What `GET /health` and `GET /runners` serve.
    pub fn first_healthy_tracked_spawn_at(&self) -> Option<DateTime<Utc>> {
        *self
            .first_healthy_tracked_spawn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether a first-healthy watchdog is tracking the spawn that started at
    /// `started_at`. False for no recorded start, and for a marker naming another
    /// spawn.
    pub fn first_healthy_tracks(&self, started_at: Option<DateTime<Utc>>) -> bool {
        started_at.is_some() && self.first_healthy_tracked_spawn_at() == started_at
    }

    /// Clear BOTH arms' counters and disarm reasons — what
    /// `POST /runners/{id}/watchdog {"reset_attempts": true}` means.
    ///
    /// The operator reset has always been "put this watchdog back to zero"; a
    /// reset that cleared only the crash arm would leave a serving-loop disarm
    /// latched with no route to clear it.
    pub fn reset_attempts(&mut self) {
        self.restart_attempts = 0;
        self.disabled_reason = None;
        self.crash_history.clear();
        self.serving_restart_attempts = 0;
        self.last_serving_restart_at = None;
        self.serving_history.clear();
        self.serving_disabled_reason = None;
        self.serving_restart_in_flight = false;
    }
}

impl Default for BuildState {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildState {
    pub fn new() -> Self {
        Self {
            build_in_progress: false,
            build_error_detected: false,
            last_build_error: None,
            last_build_at: None,
            last_build_stderr: None,
            last_mem_gate: None,
        }
    }
}

impl AiState {
    pub fn new(auto_debug_enabled: bool) -> Self {
        Self {
            provider: "claude".to_string(),
            model: "opus".to_string(),
            auto_debug_enabled,
        }
    }
}

impl ExpoState {
    pub fn new(port: u16) -> Self {
        Self {
            process: None,
            running: false,
            pid: None,
            started_at: None,
            port,
        }
    }
}

pub struct EvaluationState {
    pub running: bool,
    pub current_run_id: Option<String>,
    pub continuous_mode: bool,
    pub continuous_interval_secs: u64,
    pub current_prompt_index: usize,
    pub total_prompts: usize,
    pub stop_tx: Option<watch::Sender<bool>>,
}

impl EvaluationState {
    pub fn new() -> Self {
        Self {
            running: false,
            current_run_id: None,
            continuous_mode: false,
            continuous_interval_secs: 3600,
            current_prompt_index: 0,
            total_prompts: 0,
            stop_tx: None,
        }
    }
}

impl Default for EvaluationState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct VelocityTestState {
    pub running: bool,
    pub current_run_id: Option<String>,
    pub current_test_index: usize,
    pub total_tests: usize,
    pub stop_tx: Option<watch::Sender<bool>>,
}

impl VelocityTestState {
    pub fn new() -> Self {
        Self {
            running: false,
            current_run_id: None,
            current_test_index: 0,
            total_tests: 0,
            stop_tx: None,
        }
    }
}

impl Default for VelocityTestState {
    fn default() -> Self {
        Self::new()
    }
}

/// Persist the largest built slot target dir from `snapshot` as the
/// one-target size estimate (plan `2026-10-02-cargo-jobs-…` Phases 2/4), so the
/// NEXT supervisor start can include the disk term in its pool-size derivation
/// without walking a slot tree. Nothing is written when no slot is built yet —
/// an absent estimate stays UNKNOWN rather than becoming a zero.
fn persist_one_target_estimate(
    config: &crate::config::SupervisorConfig,
    snapshot: &crate::footprint::FootprintSnapshot,
) {
    if let Some((slot, bytes)) = crate::build_sizing::largest_slot(&snapshot.slots) {
        crate::build_sizing::write_one_target_estimate(
            &config.runner_npm_dir().join("target-pool"),
            &crate::build_sizing::OneTargetEstimate {
                bytes,
                slot,
                measured_at: snapshot.computed_at.clone(),
                disk_total_bytes: snapshot.disk_total_bytes,
                built_slots: snapshot.slots.iter().filter(|s| s.bytes > 0).count() as u32,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RunnerConfig, SupervisorConfig, DEFAULT_SUPERVISOR_PORT, EXPO_PORT};
    use std::path::PathBuf;

    fn make_test_config() -> SupervisorConfig {
        SupervisorConfig {
            project_dir: PathBuf::from("/tmp/test/src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: DEFAULT_SUPERVISOR_PORT,
            dev_logs_dir: PathBuf::from("/tmp/.dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: EXPO_PORT,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: crate::config::BuildPoolConfig::fixed(1),
            no_prewarm: false,
            no_webview: true,
            temp_runner_display: None,
            ambient_target_dir: crate::config::AmbientTargetDir::Ignore,
            ambient_coord: crate::config::AmbientCoord::Ignore,
        }
    }

    // --- RunnerState tests ---

    #[test]
    fn test_runner_state_new_defaults() {
        let state = RunnerState::new();
        assert!(!state.running);
        assert!(state.started_at.is_none());
        assert!(!state.restart_requested);
        assert!(!state.stop_requested);
        assert!(state.stop_requested_at.is_none());
        assert!(state.pid.is_none());
    }

    /// A stop request latches the flag and the time of the EARLIEST request
    /// still latched: a later request keeps the first time, and only a clear
    /// (the next start) drops both.
    #[test]
    fn a_stop_request_keeps_the_earliest_request_time_until_cleared() {
        let first = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(10);
        let later = first + chrono::Duration::seconds(5);
        let mut state = RunnerState::new();

        state.latch_stop_request(first);
        assert!(state.stop_requested);
        assert_eq!(state.stop_requested_at, Some(first), "set when none was");

        state.latch_stop_request(later);
        assert!(state.stop_requested);
        assert_eq!(
            state.stop_requested_at,
            Some(first),
            "a later request must keep the earliest one's time"
        );

        state.clear_stop_request();
        assert!(!state.stop_requested);
        assert_eq!(state.stop_requested_at, None, "cleared with the flag");

        state.latch_stop_request(later);
        assert_eq!(
            state.stop_requested_at,
            Some(later),
            "a request after a clear is the earliest again"
        );
    }

    #[test]
    fn test_runner_state_default_matches_new() {
        let from_new = RunnerState::new();
        let from_default = RunnerState::default();
        assert_eq!(from_new.running, from_default.running);
        assert_eq!(from_new.pid, from_default.pid);
        assert_eq!(from_new.restart_requested, from_default.restart_requested);
        assert_eq!(from_new.stop_requested, from_default.stop_requested);
    }

    // --- WatchdogState tests ---

    #[test]
    fn test_watchdog_state_new_enabled() {
        let state = WatchdogState::new(true);
        assert!(state.enabled);
        assert_eq!(state.restart_attempts, 0);
        assert!(state.last_restart_at.is_none());
        assert!(state.crash_history.is_empty());
        assert!(state.disabled_reason.is_none());
    }

    #[test]
    fn test_watchdog_state_new_disabled() {
        let state = WatchdogState::new(false);
        assert!(!state.enabled);
    }

    // --- BuildState tests ---

    #[test]
    fn test_build_state_new_defaults() {
        let state = BuildState::new();
        assert!(!state.build_in_progress);
        assert!(!state.build_error_detected);
        assert!(state.last_build_error.is_none());
        assert!(state.last_build_at.is_none());
    }

    #[test]
    fn test_build_state_default_matches_new() {
        let from_new = BuildState::new();
        let from_default = BuildState::default();
        assert_eq!(from_new.build_in_progress, from_default.build_in_progress);
        assert_eq!(
            from_new.build_error_detected,
            from_default.build_error_detected
        );
    }

    // --- AiState tests ---

    #[test]
    fn test_ai_state_new_with_auto_debug_enabled() {
        let state = AiState::new(true);
        assert_eq!(state.provider, "claude");
        assert_eq!(state.model, "opus");
        assert!(state.auto_debug_enabled);
    }

    #[test]
    fn test_ai_state_new_with_auto_debug_disabled() {
        let state = AiState::new(false);
        assert!(!state.auto_debug_enabled);
    }

    // --- ExpoState tests ---

    #[test]
    fn test_expo_state_new() {
        let state = ExpoState::new(8081);
        assert!(!state.running);
        assert!(state.process.is_none());
        assert!(state.pid.is_none());
        assert!(state.started_at.is_none());
        assert_eq!(state.port, 8081);
    }

    #[test]
    fn test_expo_state_custom_port() {
        let state = ExpoState::new(3000);
        assert_eq!(state.port, 3000);
    }

    // --- EvaluationState tests ---

    #[test]
    fn test_evaluation_state_new_defaults() {
        let state = EvaluationState::new();
        assert!(!state.running);
        assert!(state.current_run_id.is_none());
        assert!(!state.continuous_mode);
        assert_eq!(state.continuous_interval_secs, 3600);
        assert_eq!(state.current_prompt_index, 0);
        assert_eq!(state.total_prompts, 0);
        assert!(state.stop_tx.is_none());
    }

    #[test]
    fn test_evaluation_state_default_matches_new() {
        let from_new = EvaluationState::new();
        let from_default = EvaluationState::default();
        assert_eq!(from_new.running, from_default.running);
        assert_eq!(
            from_new.continuous_interval_secs,
            from_default.continuous_interval_secs
        );
    }

    // --- VelocityTestState tests ---

    #[test]
    fn test_velocity_test_state_new_defaults() {
        let state = VelocityTestState::new();
        assert!(!state.running);
        assert!(state.current_run_id.is_none());
        assert_eq!(state.current_test_index, 0);
        assert_eq!(state.total_tests, 0);
        assert!(state.stop_tx.is_none());
    }

    // --- SupervisorState tests ---

    #[tokio::test]
    async fn test_supervisor_state_construction() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        assert_eq!(state.config.port, DEFAULT_SUPERVISOR_PORT);
    }

    #[tokio::test]
    async fn test_supervisor_state_runner_initial_state() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let runner = state.runner.try_read().unwrap();
        assert!(!runner.running);
        assert!(runner.pid.is_none());
    }

    #[tokio::test]
    async fn test_supervisor_state_watchdog_disabled_by_default() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let watchdog = state.watchdog.try_read().unwrap();
        assert!(!watchdog.enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_watchdog_enabled_from_config() {
        let mut config = make_test_config();
        config.watchdog_enabled_at_start = true;
        let state = SupervisorState::new(config);
        let watchdog = state.watchdog.try_read().unwrap();
        assert!(watchdog.enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_auto_debug_disabled_by_default() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let ai = state.ai.try_read().unwrap();
        assert!(!ai.auto_debug_enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_auto_debug_enabled_from_config() {
        let mut config = make_test_config();
        config.auto_debug = true;
        let state = SupervisorState::new(config);
        let ai = state.ai.try_read().unwrap();
        assert!(ai.auto_debug_enabled);
    }

    #[tokio::test]
    async fn test_supervisor_state_cached_health_defaults_to_all_false() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let cached = state.cached_health.try_read().unwrap();
        assert!(!cached.runner_port_open);
        assert!(!cached.runner_responding);
    }

    #[tokio::test]
    async fn test_supervisor_state_build_not_in_progress() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        let build = state.build.try_read().unwrap();
        assert!(!build.build_in_progress);
        assert!(!build.build_error_detected);
    }

    #[tokio::test]
    async fn test_supervisor_state_notify_health_change_does_not_panic() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        // Should not panic even with no subscribers
        state.notify_health_change();
    }

    #[tokio::test]
    async fn test_shutdown_signal_unblocks_live_subscriber() {
        // Regression test for the /supervisor/shutdown hang fix — live path.
        //
        // Long-lived SSE handlers race their work against
        // `state.shutdown_signal()`; if this future doesn't resolve when
        // shutdown is signaled, axum's graceful drain never completes and
        // the supervisor process lingers.
        let config = make_test_config();
        let state = Arc::new(SupervisorState::new(config));

        // Subscribe BEFORE the signal so the broadcast catches the
        // pre-existing receiver — this is the steady-state production path
        // (handlers connect, then shutdown fires later).
        let signal_state = state.clone();
        let signal = tokio::spawn(async move { signal_state.shutdown_signal().await });
        // Yield briefly so the spawned task reaches `rx.recv().await`
        // before we send. Without this, the test passes for the wrong
        // reason (it would hit the latched-fast-path instead).
        tokio::task::yield_now().await;

        // Fires the same path that `routes/runner.rs::supervisor_shutdown` uses.
        state.signal_shutdown();

        // A give-up budget, not a speed assertion: the property is that the
        // signal RESOLVES, and the await returns the instant it does. The
        // previous 1s was sized against "a few millis on an idle box", which
        // measures the box (plan
        // `2026-09-06-supervisor-test-wall-clock-deadlines-fail-under-fleet-load`);
        // widening a give-up budget weakens no assertion.
        let budget = crate::test_clock::poll_budget(std::time::Duration::from_secs(30));
        tokio::time::timeout(budget, signal)
            .await
            .expect("shutdown_signal must resolve once signal_shutdown fires")
            .expect("signal task must not panic");
    }

    #[tokio::test]
    async fn test_shutdown_signal_late_subscriber_sees_latch() {
        // The other half of the regression: handlers that race in *after*
        // shutdown was already signaled. Broadcast channels do not replay
        // missed messages, so without the latched bool, a late subscriber
        // would block forever — wedging axum's graceful drain just as
        // surely as a never-terminating SSE stream.
        let config = make_test_config();
        let state = Arc::new(SupervisorState::new(config));

        // Signal first, subscribe second.
        state.signal_shutdown();

        // Give-up budget, same reasoning as above.
        let budget = crate::test_clock::poll_budget(std::time::Duration::from_secs(30));
        let result = tokio::time::timeout(budget, state.shutdown_signal()).await;
        assert!(
            result.is_ok(),
            "late shutdown_signal subscriber must see the latched flag (waited {budget:?})"
        );
    }

    // --- SlotHistory tests ---

    #[test]
    fn test_slot_history_new_empty() {
        let h = SlotHistory::new();
        assert_eq!(h.total_builds, 0);
        assert!(h.avg_duration_secs().is_none());
        assert!(h.p50_duration_secs().is_none());
    }

    #[test]
    fn test_slot_history_record_and_avg() {
        let mut h = SlotHistory::new();
        h.record(10.0, true, None, None);
        h.record(20.0, true, None, None);
        h.record(
            30.0,
            false,
            Some("boom".into()),
            Some("stderr detail".into()),
        );
        assert_eq!(h.total_builds, 3);
        assert_eq!(h.successful_builds, 2);
        assert!((h.avg_duration_secs().unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(h.last_error.as_deref(), Some("boom"));
        assert_eq!(h.last_error_detail.as_deref(), Some("stderr detail"));
        // Short error log mirrors the detail when below the cap.
        assert_eq!(h.last_error_log.as_deref(), Some("stderr detail"));
    }

    #[test]
    fn test_slot_history_success_clears_last_error_log() {
        let mut h = SlotHistory::new();
        h.record(1.0, false, Some("err".into()), Some("boom".into()));
        assert_eq!(h.last_error_log.as_deref(), Some("boom"));
        // A subsequent green build supersedes the failure for inline display.
        h.record(2.0, true, None, None);
        assert!(h.last_error_log.is_none());
        // The longer detail / error string can stay (forensic history).
        // We only enforce the inline summary clears.
    }

    #[test]
    /// `last_error_log` keeps the HEAD of the detail, not the tail.
    ///
    /// This assertion was inverted on 2026-07-31 along with the behaviour.
    /// `error_detail` is no longer raw cargo stderr — it is a rendered
    /// diagnostic (`build_diagnostics::render_capped_detail`) that hoists the
    /// cause to the FRONT precisely so a size cap cannot drop it. Keeping the
    /// tail of that document hands `GET /builds` an excerpt of linker flags,
    /// which is the exact defect the rendering was introduced to fix.
    fn test_slot_history_last_error_log_keeps_the_head_within_1k() {
        let mut h = SlotHistory::new();
        let big = "Z".repeat(LAST_ERROR_LOG_MAX_BYTES * 4);
        let detail = format!("HEAD_LOG_START{}TAIL_LOG_END", big);
        h.record(1.0, false, Some("err".into()), Some(detail));
        let stored = h.last_error_log.as_ref().expect("log recorded");
        assert!(
            stored.starts_with("HEAD_LOG_START"),
            "the head — where the rendered cause lives — must survive: {stored:.60}"
        );
        assert!(
            !stored.contains("TAIL_LOG_END"),
            "a 4x-oversized detail must actually be cut"
        );
        // The head helper appends a short truncation marker; allow for it.
        assert!(
            stored.len() <= LAST_ERROR_LOG_MAX_BYTES + 32,
            "log too large: {}",
            stored.len()
        );
    }

    #[test]
    fn test_tail_bytes_keep_utf8_short_passthrough() {
        let s = "short";
        assert_eq!(tail_bytes_keep_utf8(s, 1024), "short");
    }

    #[test]
    fn test_tail_bytes_keep_utf8_long_keeps_tail_on_boundary() {
        // Build a multi-byte string and ensure the result is valid UTF-8.
        let s: String = "ééééééééééééé".repeat(200); // 'é' is 2 bytes in UTF-8
        let out = tail_bytes_keep_utf8(&s, 50);
        assert!(out.len() <= 50 + 1); // up to 1 byte of slack to land on boundary
        assert!(s.ends_with(&out));
    }

    #[test]
    fn test_head_bytes_keep_utf8_short_passthrough() {
        assert_eq!(head_bytes_keep_utf8("short", 1024), "short");
    }

    /// The head counterpart must be UTF-8 safe at the cut and must mark that
    /// it cut — an unmarked crop invites the reader to treat a truncated
    /// document as complete.
    #[test]
    fn test_head_bytes_keep_utf8_long_keeps_head_on_boundary() {
        let s: String = "ééééééééééééé".repeat(200); // 'é' is 2 bytes in UTF-8
        let out = head_bytes_keep_utf8(&s, 51); // odd cap lands mid-character
        let body = out.strip_suffix("\n[...truncated]").expect("cut is marked");
        assert!(body.len() <= 51);
        assert!(s.starts_with(body), "must be a prefix of the input");
        assert!(body.chars().all(|c| c == 'é'), "must stay valid UTF-8");
    }

    #[test]
    fn test_slot_history_window_evicts() {
        let mut h = SlotHistory::new();
        for i in 0..(RECENT_BUILD_SAMPLE_COUNT + 3) {
            h.record(i as f64, true, None, None);
        }
        assert_eq!(h.recent_durations_secs.len(), RECENT_BUILD_SAMPLE_COUNT);
        assert_eq!(h.recent_durations_secs.front().copied(), Some(3.0));
    }

    #[test]
    fn test_slot_history_p50() {
        let mut h = SlotHistory::new();
        h.record(5.0, true, None, None);
        h.record(1.0, true, None, None);
        h.record(9.0, true, None, None);
        assert_eq!(h.p50_duration_secs(), Some(5.0));
    }

    #[test]
    fn test_truncate_error_detail_keep_tail_short_passthrough() {
        let s = "short string".to_string();
        let out = truncate_error_detail_keep_tail(s.clone());
        assert_eq!(out, s);
    }

    #[test]
    fn test_truncate_error_detail_keep_tail_truncates_front() {
        // Build a string substantially larger than the cap.
        let big = "X".repeat(LAST_ERROR_DETAIL_MAX_BYTES * 2);
        // Make the tail uniquely identifiable so we can confirm it survived.
        let s = format!("{}TAIL_MARKER_END", big);
        let out = truncate_error_detail_keep_tail(s.clone());
        // The tail must be preserved verbatim.
        assert!(
            out.ends_with("TAIL_MARKER_END"),
            "tail not preserved; got: {:?}",
            &out[out.len().saturating_sub(40)..]
        );
        // The result must be near the cap (cap + small marker).
        assert!(
            out.len() <= LAST_ERROR_DETAIL_MAX_BYTES + 64,
            "result too large: {}",
            out.len()
        );
        // And a truncation marker must appear at the start so consumers know.
        assert!(out.starts_with("[...truncated]"));
    }

    #[test]
    fn test_slot_history_record_truncates_long_detail() {
        let mut h = SlotHistory::new();
        let big = "Y".repeat(LAST_ERROR_DETAIL_MAX_BYTES + 1024);
        let detail = format!("{}END_OF_STDERR", big);
        h.record(1.0, false, Some("err".into()), Some(detail));
        let stored = h.last_error_detail.as_ref().expect("detail recorded");
        assert!(stored.ends_with("END_OF_STDERR"));
        assert!(stored.len() <= LAST_ERROR_DETAIL_MAX_BYTES + 64);
    }

    #[test]
    fn test_slot_history_success_clears_no_detail() {
        let mut h = SlotHistory::new();
        h.record(1.0, true, None, Some("should be ignored".into()));
        assert!(h.last_error_detail.is_none());
    }

    #[test]
    fn test_sse_connection_guard_increments_and_decrements() {
        // Construction must increment, drop must decrement back to zero.
        // The /health endpoint reads this via Ordering::Relaxed.
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
        {
            let _g = SseConnectionGuard::new(counter.clone());
            assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 1);
            let _g2 = SseConnectionGuard::new(counter.clone());
            assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 2);
        }
        // Both guards dropped — counter must be back at zero.
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_supervisor_state_active_sse_connections_starts_at_zero() {
        let config = make_test_config();
        let state = SupervisorState::new(config);
        assert_eq!(
            state
                .active_sse_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "fresh supervisor must report zero active SSE connections"
        );
    }

    #[tokio::test]
    async fn test_supervisor_state_expo_port_from_config() {
        let mut config = make_test_config();
        config.expo_port = 9999;
        let state = SupervisorState::new(config);
        let expo = state.expo.try_read().unwrap();
        assert_eq!(expo.port, 9999);
    }

    // --- boot_id persistence tests ---

    #[test]
    fn test_load_or_create_boot_id_at_persists_across_calls() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("nested").join("boot.id");
        // First call: file does not exist, helper must generate + persist.
        let first = load_or_create_boot_id_at(&path);
        assert!(
            uuid::Uuid::parse_str(&first).is_ok(),
            "first call must return a valid UUID, got {first:?}"
        );
        assert!(path.exists(), "first call must create the boot.id file");
        // Second call: must read the persisted UUID, not generate a fresh one.
        let second = load_or_create_boot_id_at(&path);
        assert_eq!(
            first, second,
            "second call must return the same UUID as the first"
        );
    }

    #[test]
    fn test_load_or_create_boot_id_at_regenerates_on_invalid_contents() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("boot.id");
        std::fs::write(&path, "not-a-uuid").expect("seed invalid contents");
        let id = load_or_create_boot_id_at(&path);
        assert!(
            uuid::Uuid::parse_str(&id).is_ok(),
            "invalid contents must be replaced with a valid UUID"
        );
        // The file should now contain the freshly-generated UUID.
        let persisted = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(persisted.trim(), id);
    }

    #[test]
    fn test_load_or_create_boot_id_at_tolerates_trailing_whitespace() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let path = dir.path().join("boot.id");
        let seeded = uuid::Uuid::new_v4().to_string();
        std::fs::write(&path, format!("{seeded}\n")).expect("seed valid contents");
        let id = load_or_create_boot_id_at(&path);
        assert_eq!(id, seeded, "trailing whitespace must be trimmed");
    }

    // ---- answered_this_boot: per boot, not per supervisor lifetime ----

    /// The table: no boot recorded is UNKNOWN whatever the stamp says; with a
    /// boot, no stamp and an earlier stamp are `false`, and a stamp at or after
    /// the boot's start is `true`. Literal instants, so an off-by-one in the
    /// comparison (`>` for `>=`) turns the "at" row red.
    #[test]
    fn answered_this_boot_compares_the_stamp_with_this_boots_start() {
        let started: DateTime<Utc> = "2026-11-04T10:00:00Z".parse().unwrap();
        let before: DateTime<Utc> = "2026-11-02T09:59:59Z".parse().unwrap();
        let after: DateTime<Utc> = "2026-11-04T10:00:01Z".parse().unwrap();
        let row = |stamp: Option<DateTime<Utc>>, start: Option<DateTime<Utc>>| {
            let mut st = RunnerState::new();
            st.last_seen_responding_at = stamp;
            st.started_at = start;
            st.answered_this_boot()
        };
        assert_eq!(row(None, None), None, "no start, no stamp");
        assert_eq!(row(Some(after), None), None, "no start: UNKNOWN, not true");
        assert_eq!(row(None, Some(started)), Some(false), "no stamp");
        assert_eq!(row(Some(before), Some(started)), Some(false), "before");
        assert_eq!(row(Some(started), Some(started)), Some(true), "at");
        assert_eq!(row(Some(after), Some(started)), Some(true), "after");
    }

    // ---- Phase 3b: liveness must not conflate "quiet" with "gone" ----

    #[test]
    fn responding_runner_is_responding() {
        let mut st = RunnerState::new();
        st.running = true;
        assert_eq!(st.liveness(true, true, false), RunnerLiveness::Responding);
        // Even with the port probe somehow false, a positive API answer wins:
        // it is direct evidence the process is serving.
        assert_eq!(st.liveness(false, true, false), RunnerLiveness::Responding);
    }

    #[test]
    fn silent_api_with_a_held_port_is_unresponsive_not_stopped() {
        // This is the 2026-08-08 incident shape: PID alive, port held, API
        // silent. The old boolean reported this as "not running, pid null".
        let seen = Utc::now();
        let mut st = RunnerState::new();
        st.running = false;
        st.last_seen_responding_at = Some(seen);
        st.pid = Some(148320);

        assert_eq!(
            st.liveness(true, false, false),
            RunnerLiveness::UnresponsiveSince(seen)
        );
        assert_ne!(st.liveness(true, false, false), RunnerLiveness::Stopped);
    }

    /// A **supervisor-managed** runner keeps `running: true` for its whole
    /// life — the refresher only syncs that flag down to the probe for
    /// user-managed runners. Classifying off the flag therefore called the
    /// wedge `Responding`, which is the false-healthy this enum exists to
    /// prevent. Only the probe decides.
    #[test]
    fn a_wedged_runner_that_still_reads_running_true_is_not_responding() {
        let seen = Utc::now();
        let mut st = RunnerState::new();
        st.running = true; // latched at spawn, never cleared while alive
        st.pid = Some(4242);
        st.last_seen_responding_at = Some(seen);

        assert_eq!(
            st.liveness(true, false, false),
            RunnerLiveness::UnresponsiveSince(seen),
            "port held + API silent is a wedge no matter what `running` says"
        );
        assert_eq!(st.liveness(true, false, false).state(), "wedged");
    }

    #[test]
    fn silent_api_with_a_closed_port_is_stopped() {
        let mut st = RunnerState::new();
        st.running = false;
        st.last_seen_responding_at = Some(Utc::now());
        assert_eq!(st.liveness(false, false, false), RunnerLiveness::Stopped);
    }

    /// `Stopped` is a claim of positive evidence of absence. A runner the
    /// supervisor believes it spawned, which has not bound its port yet, is
    /// not evidence of absence — it is UNKNOWN.
    #[test]
    fn a_believed_running_process_with_no_port_yet_is_unknown_not_stopped() {
        let mut st = RunnerState::new();
        st.running = true;
        st.last_seen_responding_at = Some(Utc::now());
        assert_eq!(st.liveness(false, false, false), RunnerLiveness::Unknown);
    }

    /// A primary hung before its listener binds: the health tick has lowered
    /// `running` from the failed probe and the stamp is a previous boot's, so
    /// without `spawn_up` this read `Stopped` about a live process. With the
    /// newest spawn up it is UNKNOWN; with it down, the earlier answer and the
    /// closed port are positive evidence of absence again.
    #[test]
    fn a_live_spawn_with_no_port_is_never_read_as_stopped() {
        let earlier: DateTime<Utc> = "2026-11-02T10:00:00Z".parse().unwrap();
        let mut st = RunnerState::new();
        st.running = false;
        st.last_seen_responding_at = Some(earlier);
        st.started_at = Some("2026-11-04T10:00:00Z".parse().unwrap());
        assert_eq!(st.liveness(false, false, true), RunnerLiveness::Unknown);
        assert_eq!(st.liveness(false, false, false), RunnerLiveness::Stopped);
        // A held port or an answer still decides on its own.
        assert_eq!(
            st.liveness(true, false, true),
            RunnerLiveness::UnresponsiveSince(earlier)
        );
        assert_eq!(st.liveness(false, true, true), RunnerLiveness::Responding);
    }

    #[test]
    fn never_seen_responding_is_unknown_not_stopped() {
        // No stamp means we cannot distinguish "never started" from
        // "stopped before we looked" — say UNKNOWN rather than guessing.
        let st = RunnerState::new();
        assert_eq!(st.liveness(false, false, false), RunnerLiveness::Unknown);
        assert_eq!(st.liveness(true, false, false), RunnerLiveness::Unknown);
    }

    /// The wire shape is one object for all four states — a consumer reads
    /// `.liveness.state` without first branching on the JSON type.
    #[test]
    fn liveness_serializes_as_a_uniform_state_object() {
        let at = Utc::now();
        let wedged =
            serde_json::to_value(RunnerLiveness::UnresponsiveSince(at)).expect("serialize wedged");
        assert_eq!(wedged["state"], "wedged");
        assert_eq!(
            wedged["unresponsive_since"],
            at.to_rfc3339(),
            "the same RFC3339 spelling every other timestamp on the row uses"
        );

        for (variant, name) in [
            (RunnerLiveness::Responding, "responding"),
            (RunnerLiveness::Stopped, "stopped"),
            (RunnerLiveness::Unknown, "unknown"),
        ] {
            let v = serde_json::to_value(variant).expect("serialize");
            assert_eq!(v["state"], name);
            assert!(
                v["unresponsive_since"].is_null(),
                "{name} must carry an explicit null, never a missing key: {v}"
            );
        }
    }
    // --- spawn-test single flight ---

    fn dedup_key(requester: &str, target: &str) -> SpawnDedupKey {
        SpawnDedupKey {
            requester_id: requester.to_string(),
            build_target: target.to_string(),
        }
    }

    fn spawn_state() -> Arc<SupervisorState> {
        Arc::new(SupervisorState::new(make_test_config()))
    }

    /// A submission the store will report as still running.
    fn running_submission(id: uuid::Uuid) -> crate::build_submissions::BuildSubmission {
        crate::build_submissions::BuildSubmission {
            id,
            worktree_path: PathBuf::from("/tmp/test/src-tauri"),
            source: None,
            build_kind: crate::build_submissions::BuildKind::Build,
            agent_id: None,
            package: None,
            features: vec![],
            base_ref: None,
            submitted_at: Utc::now(),
            status: crate::build_submissions::BuildStatus::Running {
                started_at: Utc::now(),
            },
            cache_key: None,
            cache_outcome: None,
            cache_hit: false,
            stdout_tail: vec![],
            stderr_tail: vec![],
            spawn: None,
            detached: None,
        }
    }

    fn finished_submission(id: uuid::Uuid) -> crate::build_submissions::BuildSubmission {
        let mut sub = running_submission(id);
        sub.status = crate::build_submissions::BuildStatus::Succeeded {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            duration_secs: 1.0,
        };
        sub
    }

    fn inflight(id: uuid::Uuid, port: u16) -> SpawnInflight {
        SpawnInflight {
            submission_id: id,
            runner_id: format!("test-{port}"),
            port,
            action: crate::dev_action::test_support::minted_action(),
        }
    }

    /// The property the whole index exists for: two rapid same-key spawn-test
    /// requests must produce ONE build, and both must see the same `build_id`.
    /// Raced through a barrier so the second genuinely contends for the claim.
    #[tokio::test]
    async fn two_concurrent_same_key_spawns_start_one_build_with_one_build_id() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let starts = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let state = state.clone();
            let key = key.clone();
            let starts = starts.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                match state.claim_spawn_build(Some(key)).await {
                    SpawnTicket::Join(existing) => existing.submission_id,
                    SpawnTicket::Claim(claim) => {
                        starts.fetch_add(1, Ordering::SeqCst);
                        let id = uuid::Uuid::new_v4();
                        // Stand in for `submit_spawn`: register a live build,
                        // then publish the claim (both under the guard, exactly
                        // as `spawn_test` does).
                        state.build_submissions.insert(running_submission(id)).await;
                        claim.commit(inflight(id, 9877));
                        id
                    }
                }
            }));
        }

        let mut ids = Vec::new();
        for h in handles {
            ids.push(h.await.expect("task"));
        }

        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "exactly one of two concurrent same-key spawns may claim a build slot"
        );
        assert_eq!(
            ids[0], ids[1],
            "the joining request must be handed the SAME build_id, not a fresh one"
        );
    }

    /// Different build targets are different builds: same requester, four
    /// distinct targets, four independent claims.
    #[tokio::test]
    async fn different_build_targets_never_join_each_other() {
        let state = spawn_state();
        let occupied = dedup_key("agent-1", "origin_main");
        let id = uuid::Uuid::new_v4();
        state.build_submissions.insert(running_submission(id)).await;
        match state.claim_spawn_build(Some(occupied.clone())).await {
            SpawnTicket::Claim(claim) => claim.commit(inflight(id, 9877)),
            SpawnTicket::Join(_) => panic!("first claim on an empty index must not join"),
        }

        for target in ["live_tree", "git_ref:feature/x", "worktree_path:D:/wt"] {
            match state
                .claim_spawn_build(Some(dedup_key("agent-1", target)))
                .await
            {
                SpawnTicket::Claim(_) => {}
                SpawnTicket::Join(_) => {
                    panic!("target {target} must not join a build of a different target")
                }
            }
        }

        // A different requester on the SAME target is also its own build — two
        // agents each want their own runner.
        match state
            .claim_spawn_build(Some(dedup_key("agent-2", "origin_main")))
            .await
        {
            SpawnTicket::Claim(_) => {}
            SpawnTicket::Join(_) => panic!("a different requester must never join another's build"),
        };
    }

    /// Anonymous requests are indistinguishable from one another, so they never
    /// join — not each other, and not a keyed build. They also record nothing.
    #[tokio::test]
    async fn anonymous_requests_never_join_and_never_record() {
        let state = spawn_state();
        for _ in 0..3 {
            match state.claim_spawn_build(None).await {
                SpawnTicket::Claim(claim) => claim.commit(inflight(uuid::Uuid::new_v4(), 9877)),
                SpawnTicket::Join(_) => panic!("an unkeyed request must never join"),
            }
        }
        assert!(
            state.spawn_test_inflight.read().await.is_empty(),
            "an unkeyed claim must record nothing — otherwise the next anonymous \
             request would be handed another caller's runner"
        );
    }

    /// A finished build is not joinable: its entry is dropped and the new
    /// request starts a real build.
    #[tokio::test]
    async fn a_terminal_build_is_replaced_not_joined() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let old = uuid::Uuid::new_v4();
        state
            .build_submissions
            .insert(finished_submission(old))
            .await;
        state
            .spawn_test_inflight
            .write()
            .await
            .insert(key.clone(), inflight(old, 9877));

        match state.claim_spawn_build(Some(key.clone())).await {
            SpawnTicket::Claim(claim) => claim.commit(inflight(uuid::Uuid::new_v4(), 9878)),
            SpawnTicket::Join(_) => panic!("a terminal build must not be joined"),
        }
        assert_ne!(
            state.spawn_test_inflight.read().await[&key].submission_id,
            old,
            "the stale entry must be replaced by the new build"
        );
    }

    /// A build id the store no longer holds has been evicted, and only a terminal
    /// row is: `submit_spawn` registers before the claim commits, so no live build
    /// is absent. Joining it would answer every later request with a dead build.
    #[tokio::test]
    async fn an_evicted_submission_does_not_read_as_in_flight() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        state
            .spawn_test_inflight
            .write()
            .await
            .insert(key.clone(), inflight(uuid::Uuid::new_v4(), 9877));

        match state.claim_spawn_build(Some(key)).await {
            SpawnTicket::Claim(_) => {}
            SpawnTicket::Join(_) => panic!("a build the store evicted must not be joined"),
        };
    }

    fn id_cell(id: uuid::Uuid) -> std::sync::OnceLock<uuid::Uuid> {
        let cell = std::sync::OnceLock::new();
        cell.set(id).unwrap();
        cell
    }

    /// A build that finishes before `spawn_test` has filled its id cell and
    /// published its claim waits on the claim's lock, and then frees the key, so
    /// the claim does not publish an entry for a build already over.
    #[tokio::test]
    async fn a_release_racing_its_claim_still_frees_the_key() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let id = uuid::Uuid::new_v4();
        let cell = std::sync::Arc::new(std::sync::OnceLock::new());

        let SpawnTicket::Claim(claim) = state.claim_spawn_build(Some(key.clone())).await else {
            panic!("nothing held the key");
        };
        let releaser = tokio::spawn({
            let (state, key, cell) = (state.clone(), key.clone(), cell.clone());
            async move { state.release_spawn_inflight(&key, &cell).await }
        });
        // Let the releaser run up to the lock the claim holds.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !releaser.is_finished(),
            "the release must wait for the claim"
        );

        cell.set(id).unwrap();
        claim.commit(inflight(id, 9877));
        releaser.await.unwrap();
        assert!(
            state.spawn_test_inflight.read().await.is_empty(),
            "the claim published a build that had already released"
        );
    }

    /// Releasing is scoped to the submission that owns the key, so a build
    /// finishing late cannot evict the build that replaced it.
    #[tokio::test]
    async fn release_only_removes_its_own_submission() {
        let state = spawn_state();
        let key = dedup_key("agent-1", "origin_main");
        let current = uuid::Uuid::new_v4();
        state
            .spawn_test_inflight
            .write()
            .await
            .insert(key.clone(), inflight(current, 9878));

        state
            .release_spawn_inflight(&key, &id_cell(uuid::Uuid::new_v4()))
            .await;
        assert!(
            state.spawn_test_inflight.read().await.contains_key(&key),
            "a stale build's release must not evict its successor"
        );

        state.release_spawn_inflight(&key, &id_cell(current)).await;
        assert!(
            !state.spawn_test_inflight.read().await.contains_key(&key),
            "the owning build's release must free the key"
        );
    }

    /// The recovery surface for a lost `POST /runners/spawn-test` answer.
    ///
    /// A caller whose connection died before the response was written (its own
    /// timeout, or a supervisor restart during a 20-50 minute build) has
    /// created a runner whose id it does not know — and the id is the only
    /// handle on `/runners/{id}/logs` and `/runners/{id}/stop`. The index
    /// already holds the exact triple; the snapshot is what lets the caller
    /// read it back instead of polling `GET /runners` and guessing by
    /// `requester_id`.
    #[tokio::test]
    async fn inflight_snapshot_answers_which_runner_a_requester_just_created() {
        let state = spawn_state();
        let mine = uuid::Uuid::new_v4();
        {
            let mut index = state.spawn_test_inflight.write().await;
            index.insert(dedup_key("agent-1", "origin_main"), inflight(mine, 9878));
            index.insert(
                dedup_key("agent-2", "origin_main"),
                inflight(uuid::Uuid::new_v4(), 9879),
            );
        }

        let mine_only = state.spawn_inflight_snapshot(Some("agent-1")).await;
        assert_eq!(mine_only.len(), 1, "must not leak a peer's spawn");
        assert_eq!(mine_only[0].0.requester_id, "agent-1");
        assert_eq!(mine_only[0].1.submission_id, mine);
        assert_eq!(
            mine_only[0].1.runner_id, "test-9878",
            "the snapshot must name the RUNNER ID, which is the caller's only handle on \
             /runners/{{id}}/logs and /runners/{{id}}/stop"
        );
        assert_eq!(mine_only[0].1.port, 9878);

        // Unfiltered lists everything, stably ordered.
        let all = state.spawn_inflight_snapshot(None).await;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0.requester_id, "agent-1");
        assert_eq!(all[1].0.requester_id, "agent-2");

        // A requester with nothing in flight gets an empty answer, not a peer's.
        assert!(state
            .spawn_inflight_snapshot(Some("agent-3"))
            .await
            .is_empty());
    }

    // --- Build-pool occupancy (resource sample §A2) ---

    #[tokio::test]
    async fn occupancy_counts_the_same_busy_slots_get_builds_renders() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut config = make_test_config();
        config.project_dir = tmp.path().join("qontinui-runner").join("src-tauri");
        config.build_pool = crate::config::BuildPoolConfig::fixed(3);
        let pool = BuildPool::new(&config);

        let idle = pool.occupancy().await;
        assert_eq!(idle.build_slots_total, Some(3));
        assert_eq!(idle.build_slots_busy, Some(0));
        assert_eq!(idle.build_queue_depth, Some(0));

        // Claim a slot the way a build does, and queue two callers.
        let _slot = pool
            .claim_idle_slot(BuildInfo {
                started_at: Utc::now(),
                requester_id: Some("test".to_string()),
                rebuild_kind: "exe".to_string(),
                mem_gate: None,
            })
            .await;
        pool.queue_depth.fetch_add(2, Ordering::Relaxed);

        let busy = pool.occupancy().await;
        assert_eq!(busy.build_slots_total, Some(3));
        assert_eq!(busy.build_slots_busy, Some(1));
        assert_eq!(busy.build_queue_depth, Some(2));

        // The whole point: this is not a second accounting. The published
        // busy count must equal the count `GET /builds` derives from the same
        // per-slot `busy` scan (its `active_builds` / `available_permits`), so
        // a sample can never claim room the dashboard says is taken.
        let busy_in_snapshot = pool
            .snapshot()
            .await
            .iter()
            .filter(|(_, _, info)| info.is_some())
            .count() as u32;
        assert_eq!(busy.build_slots_busy, Some(busy_in_snapshot));
        // ...and it must NOT be read off the semaphore, which releases after
        // `busy` is cleared and so disagrees transiently mid-release.
        assert_eq!(pool.permits.available_permits(), 3);
    }

    #[tokio::test]
    async fn a_claim_records_its_mem_gate_on_history_before_it_is_visible() {
        use crate::build_monitor::MemGateOutcome;
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut config = make_test_config();
        config.project_dir = tmp.path().join("qontinui-runner").join("src-tauri");
        config.build_pool = crate::config::BuildPoolConfig::fixed(1);
        let pool = BuildPool::new(&config);
        // A previous build left its outcome on the slot's history.
        pool.slots[0].history.write().await.last_mem_gate = Some(MemGateOutcome::Passed);

        let slot = pool
            .claim_idle_slot(BuildInfo {
                started_at: Utc::now(),
                requester_id: Some("test".to_string()),
                rebuild_kind: "exe".to_string(),
                mem_gate: Some(MemGateOutcome::ProceededAfterWait),
            })
            .await;
        // The claim returns with both already agreeing — no separate history
        // write is left for the caller to race against `GET /builds`.
        let busy_gate = slot.busy.read().await.as_ref().and_then(|i| i.mem_gate);
        assert_eq!(busy_gate, Some(MemGateOutcome::ProceededAfterWait));
        assert_eq!(slot.history.read().await.last_mem_gate, busy_gate);

        // A claim carrying no gate outcome leaves the history value alone.
        *slot.busy.write().await = None;
        let slot = pool
            .claim_idle_slot(BuildInfo {
                started_at: Utc::now(),
                requester_id: None,
                rebuild_kind: "exe".to_string(),
                mem_gate: None,
            })
            .await;
        assert_eq!(
            slot.history.read().await.last_mem_gate,
            Some(MemGateOutcome::ProceededAfterWait)
        );
    }

    // --- SpawnRing ---

    fn ring_time(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(secs)
    }

    fn spawn_of(secs: i64, pid: u32) -> SpawnMarker {
        SpawnMarker::new(ring_time(secs), Some(pid), None, None)
    }

    /// An action's spawn ends when it exits, or when a later spawn replaces it
    /// before its exit is stamped. No entry naming the action says nothing.
    #[test]
    fn an_actions_spawn_ends_when_it_exits_or_a_later_spawn_replaces_it() {
        let (own, later) = (Uuid::new_v4(), Uuid::new_v4());
        let mut ring = SpawnRing::default();
        assert!(!ring.action_spawn_ended(own), "no spawn names it");

        ring.push(SpawnMarker::new(ring_time(1), Some(1), None, Some(own)));
        assert!(!ring.action_spawn_ended(own), "it is the newest and up");

        ring.push(SpawnMarker::new(ring_time(2), Some(2), None, Some(later)));
        assert!(ring.action_spawn_ended(own), "replaced, no exit stamped");
        assert!(!ring.action_spawn_ended(later));

        assert!(ring.stamp_exit(ring_time(2), ring_time(3), None));
        assert!(ring.action_spawn_ended(later), "the newest, but exited");
    }

    /// Asked about a moment, a spawn has ended only from its replacement or its
    /// exit on: before either, it had not, whatever has happened since.
    #[test]
    fn an_actions_spawn_had_ended_by_a_moment_only_from_its_end_on() {
        let (own, later) = (Uuid::new_v4(), Uuid::new_v4());
        let mut ring = SpawnRing::default();
        assert!(
            !ring.action_spawn_ended_by(own, ring_time(9)),
            "no spawn names it"
        );

        ring.push(SpawnMarker::new(ring_time(1), Some(1), None, Some(own)));
        assert!(
            !ring.action_spawn_ended_by(own, ring_time(9)),
            "it is the newest and up"
        );

        ring.push(SpawnMarker::new(ring_time(3), Some(2), None, Some(later)));
        assert!(
            !ring.action_spawn_ended_by(own, ring_time(2)),
            "before its replacement"
        );
        assert!(
            ring.action_spawn_ended_by(own, ring_time(3)),
            "as of its replacement"
        );

        assert!(ring.stamp_exit(ring_time(3), ring_time(5), None));
        assert!(
            !ring.action_spawn_ended_by(later, ring_time(4)),
            "before its exit"
        );
        assert!(
            ring.action_spawn_ended_by(later, ring_time(5)),
            "as of its exit"
        );
    }

    /// A full ring drops its OLDEST entry to make room for a spawn, and
    /// nothing else removes one: a stamp fills its entry in, and a stamp for a
    /// spawn the ring dropped changes nothing.
    #[test]
    fn the_spawn_ring_drops_only_its_oldest_entry_and_a_stamp_removes_none() {
        let mut ring = SpawnRing::default();
        let pushed = SPAWN_RING_CAPACITY as u32 + 2;
        for i in 0..pushed {
            ring.push(spawn_of(i64::from(i), 1000 + i));
        }
        let pids: Vec<u32> = ring.iter().filter_map(|spawn| spawn.pid).collect();
        assert_eq!(pids, (1002..1000 + pushed).collect::<Vec<u32>>());

        assert!(
            !ring.stamp_exit(ring_time(0), ring_time(100), None),
            "the ring dropped pid 1000's spawn"
        );
        assert!(ring.iter().all(|spawn| spawn.exited_at.is_none()));

        assert!(ring.stamp_exit(ring_time(5), ring_time(100), None));
        assert_eq!(ring.iter().count(), SPAWN_RING_CAPACITY);
        let stamped: Vec<Option<u32>> = ring
            .iter()
            .filter(|spawn| spawn.exited_at.is_some())
            .map(|spawn| spawn.pid)
            .collect();
        assert_eq!(stamped, [Some(1005)]);
    }

    fn panic_at(secs: i64, pid: u32, payload: &str) -> RecentPanic {
        RecentPanic {
            timestamp: ring_time(secs),
            payload: payload.to_string(),
            location: None,
            thread: None,
            backtrace_preview: None,
            runner_id: Some("primary".to_string()),
            pid: Some(pid),
            version: None,
            file_path: "runner-panic.log".to_string(),
        }
    }

    /// The exit lands on the spawn its monitor watches, named by `spawned_at`,
    /// not on the oldest unstamped entry of the pid: here an earlier spawn of
    /// pid 7 whose child was dropped, so it is never stamped, sits ahead of
    /// the later spawn that got pid 7 from the OS. The stamp carries the
    /// stop-request time it is given, `None` included.
    #[test]
    fn an_exit_is_stamped_on_the_spawn_its_monitor_watches() {
        let mut ring = SpawnRing::default();
        ring.push(spawn_of(0, 7));
        ring.push(spawn_of(10, 7));

        assert!(ring.stamp_exit(ring_time(10), ring_time(15), Some(ring_time(12))));
        let entries: Vec<SpawnMarker> = ring.iter().cloned().collect();
        assert_eq!(entries[0], spawn_of(0, 7), "the dropped spawn was stamped");
        assert_eq!(entries[1].exited_at, Some(ring_time(15)));
        assert_eq!(entries[1].stop_requested_at, Some(ring_time(12)));
        assert_eq!(entries[1].exit_panic, None, "a stamp attaches no panic");

        assert!(ring.stamp_exit(ring_time(0), ring_time(20), None));
        let entries: Vec<SpawnMarker> = ring.iter().cloned().collect();
        assert_eq!(entries[0].exited_at, Some(ring_time(20)));
        assert_eq!(entries[0].stop_requested_at, None);
        assert!(
            !ring.stamp_exit(ring_time(5), ring_time(20), None),
            "no spawn was recorded at 5"
        );
    }

    /// D12: a stop latched after the exit did not ask for that exit, so the
    /// entry does not carry it. One dated at or before the exit is kept.
    #[test]
    fn a_stop_latched_after_the_exit_is_not_stamped_as_its_request() {
        for (requested, kept) in [
            (ring_time(19), Some(ring_time(19))),
            (ring_time(20), Some(ring_time(20))),
            (ring_time(21), None),
        ] {
            let mut ring = SpawnRing::default();
            ring.push(spawn_of(0, 7));
            assert!(ring.stamp_exit(ring_time(0), ring_time(20), Some(requested)));
            let entry = ring.newest().expect("the spawn");
            assert_eq!(entry.exited_at, Some(ring_time(20)));
            assert_eq!(entry.stop_requested_at, kept, "requested at {requested}");
        }
    }

    /// The panic parsed at an exit lands on the spawn that exit was stamped
    /// on, named by `spawned_at`: not on another spawn of the pid, and not on
    /// a spawn still waiting for its exit.
    #[test]
    fn a_panic_is_attached_to_the_exit_it_was_parsed_at_and_to_no_reuse_of_the_pid() {
        let mut ring = SpawnRing::default();
        ring.push(spawn_of(-5, 8));
        ring.push(spawn_of(0, 7));
        ring.push(spawn_of(10, 7));
        // Pid 8 and pid 7's first spawn exit at the same instant.
        assert!(ring.stamp_exit(ring_time(-5), ring_time(5), None));
        assert!(ring.stamp_exit(ring_time(0), ring_time(5), None));
        let first = panic_at(4, 7, "the first boot's panic");

        assert!(
            !ring.attach_exit_panic(ring_time(3), first.clone()),
            "no spawn was recorded at 3"
        );
        assert!(ring.attach_exit_panic(ring_time(0), first.clone()));
        let entries: Vec<SpawnMarker> = ring.iter().cloned().collect();
        assert_eq!(entries[0].exit_panic, None, "another pid's entry");
        assert_eq!(entries[1].exit_panic, Some(first.clone()));
        assert_eq!(entries[2], spawn_of(10, 7), "the reuse of the pid");

        // The reuse has not exited, so a panic finds nothing to attach to.
        let second = panic_at(14, 7, "the second boot's panic");
        assert!(!ring.attach_exit_panic(ring_time(10), second.clone()));
        assert_eq!(ring.iter().nth(2), Some(&spawn_of(10, 7)));

        assert!(ring.stamp_exit(ring_time(10), ring_time(15), Some(ring_time(12))));
        assert!(ring.attach_exit_panic(ring_time(10), second.clone()));
        let entries: Vec<SpawnMarker> = ring.iter().cloned().collect();
        assert_eq!(entries[1].exit_panic, Some(first), "the first exit's panic");
        assert_eq!(entries[2].exit_panic, Some(second));
        assert_eq!(entries[2].stop_requested_at, Some(ring_time(12)));
    }

    /// A boot panic lands on the spawn named by `spawned_at`, whether that
    /// spawn has exited or not, and the first one attached is kept: a later
    /// (teardown) panic of the same spawn does not replace it.
    #[test]
    fn a_boot_panic_is_attached_to_its_spawn_exited_or_not_and_the_first_is_kept() {
        let mut ring = SpawnRing::default();
        ring.push(spawn_of(0, 7));
        ring.push(spawn_of(10, 8));
        let boot = panic_at(2, 7, "the boot panic");
        let teardown = panic_at(6, 7, "the teardown panic");

        let stray = panic_at(3, 7, "a panic for no recorded spawn");
        assert!(
            !ring.attach_boot_panic(ring_time(3), stray),
            "no spawn was recorded at 3"
        );
        // Not exited yet: a boot panic still attaches.
        assert!(ring.attach_boot_panic(ring_time(0), boot.clone()));
        assert!(
            !ring.attach_boot_panic(ring_time(0), teardown.clone()),
            "a panic already attached is kept"
        );

        // Exited before the probe gave up: still attaches.
        assert!(ring.stamp_exit(ring_time(10), ring_time(15), None));
        let other = panic_at(12, 8, "the other spawn's boot panic");
        assert!(ring.attach_boot_panic(ring_time(10), other.clone()));

        let entries: Vec<SpawnMarker> = ring.iter().cloned().collect();
        assert_eq!(entries[0].boot_panic, Some(boot), "the earliest panic");
        assert_eq!(
            entries[0].exit_panic, None,
            "a boot panic is not an exit panic"
        );
        assert_eq!(entries[1].boot_panic, Some(other));
        assert_eq!(entries[1].exit_panic, None, "nor on an exited spawn");
    }

    /// The spawns of a window run from its open to its close, both included.
    #[test]
    fn spawned_between_includes_both_ends_of_the_window() {
        let mut ring = SpawnRing::default();
        for secs in [9, 10, 15, 20, 21] {
            ring.push(spawn_of(secs, 100 + secs as u32));
        }
        let inside: Vec<i64> = ring
            .spawned_between(ring_time(10), ring_time(20))
            .iter()
            .map(|spawn| spawn.spawned_at.timestamp())
            .collect();
        assert_eq!(inside, [10, 15, 20]);
    }
}
