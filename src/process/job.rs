//! Windows JobObject wrapper for kill-on-supervisor-exit semantics.
//!
//! On Windows, parent process death does NOT cascade to children (unlike
//! Unix `prctl(PR_SET_PDEATHSIG)`). When the supervisor force-restarts,
//! runners it spawned stay alive as orphans, hold open slot binaries, and
//! break subsequent cargo builds.
//!
//! Solution: at supervisor startup, create a Win32 JobObject with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When the supervisor exits —
//! graceful, panic, force-kill, or BSOD — the kernel closes the last handle
//! to the job and terminates every assigned process. WebView2 children of
//! the runner are also transitively in the job (Windows assigns child
//! processes of a job-tracked process to the same job by default; no extra
//! flag needed).
//!
//! **Only supervisor-owned ephemerals are assigned** — see
//! [`should_assign_to_ephemeral_job`]. Assigning the operator's primary was
//! the 2026-07-27 incident: a routine `restart-supervisor.ps1 -Build` closed
//! the job handle and the kernel silently reaped the primary runner, ~80
//! `claude.exe` processes, and 287 PTYs. A process cannot be removed from a
//! Windows job once assigned, so the ownership split has to happen at
//! assignment time; there is exactly one assignment site
//! ([`crate::process::manager::start_managed_runner`]) and at least three
//! supervisor exit paths, which is why this is fixed here and not at exit.
//!
//! **Off Windows there is no such kernel object, and nothing here reaps.**
//! The supervisor runs on Linux too (merytshost), where `RunnerJob` is a no-op
//! stub kept only so call sites need no `#[cfg(windows)]`. Its platform fact,
//! [`RunnerJob::REAPS_ON_SUPERVISOR_EXIT`], is `false` there, and every log
//! line about job semantics is derived from it (see the `*_message` builders
//! below) so none of them claims a reaper that does not run. What does reap a
//! temp runner off Windows:
//! - the graceful-shutdown sweep `shutdown_signal` runs at SIGNAL time
//!   (SIGTERM, Ctrl-C, `POST /supervisor/shutdown`) — see
//!   [`crate::process::shutdown_sweep`];
//! - the supervisor's own systemd unit (`KillMode=mixed`), when
//!   `supervisor-verbs.sh start` launched it in one — that also covers a crash
//!   or SIGKILL;
//! - the next supervisor's ledger-gated reconcile sweep
//!   (`manager::reconcile_orphaned_temp_runners`), which runs once ~30 s after
//!   startup and every 5 min after.
//!
//! User-owned runners leave the supervisor's cgroup on Linux instead of
//! "never being assigned": see [`crate::process::systemd_scope`].

use qontinui_types::wire::runner_kind::RunnerKind;

#[cfg(windows)]
mod imp {
    use anyhow::{anyhow, Context, Result};
    use std::mem::{size_of, zeroed};
    use tracing::warn;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// Assign a running process (by pid) to an already-created job handle.
    ///
    /// Used by [`RunnerJob::assign`] (the supervisor's process-lifetime runner
    /// job). Uses minimum-privilege `PROCESS_SET_QUOTA | PROCESS_TERMINATE`
    /// rather than `PROCESS_ALL_ACCESS`. (The per-build tree-kill job is now
    /// provided by the `process-wrap` crate's `JobObject` wrapper inside
    /// [`crate::process::guarded_command`], not by a hand-rolled job here.)
    ///
    /// `job` must be a valid job handle owned by the caller; `pid` must name a
    /// process owned by the current user (or the supervisor must be admin).
    pub(super) fn assign_pid_to(job: HANDLE, pid: u32) -> Result<()> {
        // SAFETY: OpenProcess with these access rights is documented as safe
        // for any PID owned by the current user (or with admin).
        let proc_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if proc_handle.is_null() {
            // SAFETY: GetLastError is always safe to call.
            let err = unsafe { GetLastError() };
            return Err(anyhow!(
                "OpenProcess(pid={}) failed (GetLastError = {})",
                pid,
                err
            ))
            .with_context(|| format!("assign_pid_to(pid = {pid})"));
        }

        // SAFETY: both handles are valid kernel handles owned by us.
        let ok = unsafe { AssignProcessToJobObject(job, proc_handle) };

        // Always close the per-process handle — the assignment is recorded by
        // the kernel and does not depend on us holding the duplicate handle.
        //
        // SAFETY: proc_handle is a valid open handle.
        unsafe {
            CloseHandle(proc_handle);
        }

        if ok == 0 {
            // SAFETY: GetLastError is always safe to call.
            let err = unsafe { GetLastError() };
            return Err(anyhow!(
                "AssignProcessToJobObject(pid={}) failed (GetLastError = {})",
                pid,
                err
            ));
        }
        Ok(())
    }

    /// LimitFlags applied to the supervisor's runner job.
    ///
    /// - `KILL_ON_JOB_CLOSE`: the kernel terminates every assigned process when
    ///   the last handle to the job is closed. Since the supervisor holds the
    ///   only handle (no `DuplicateHandle`), "supervisor exits" == "last
    ///   handle closes" == "every *assigned* runner dies". Assignment is
    ///   restricted to supervisor-owned ephemerals by
    ///   [`super::should_assign_to_ephemeral_job`], so this flag reaps temp
    ///   runners only — never the operator's primary or a named runner.
    /// - `BREAKAWAY_OK`: lets a deliberately-launched child opt out of the job
    ///   via `CREATE_BREAKAWAY_FROM_JOB`. Harmless to set even if we never
    ///   use the opt-out — it just gives us the option later.
    pub(super) const LIMIT_FLAGS: u32 =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;

    /// Newtype that lets us put a raw `HANDLE` (`*mut c_void`) inside an
    /// `Arc` and share it across tokio tasks.
    ///
    /// SAFETY: Win32 `HANDLE`s for kernel objects (job, process, etc.) are
    /// process-wide, reference-counted by the kernel, and safe to use from
    /// any thread for the specific calls we make here
    /// (`AssignProcessToJobObject`, `CloseHandle`). Microsoft's
    /// documentation explicitly permits cross-thread use of these handles.
    /// We never duplicate or close the handle except via `Drop`.
    #[derive(Clone, Copy)]
    struct SendSyncHandle(HANDLE);

    // SAFETY: see SendSyncHandle docs above.
    unsafe impl Send for SendSyncHandle {}
    // SAFETY: see SendSyncHandle docs above.
    unsafe impl Sync for SendSyncHandle {}

    /// Owns the supervisor's process-lifetime job. Drop closes the handle,
    /// which (because we hold the only handle) triggers the kernel-side
    /// `KILL_ON_JOB_CLOSE` semantic and terminates every assigned process.
    pub struct RunnerJob {
        handle: SendSyncHandle,
    }

    impl RunnerJob {
        /// The kernel terminates every assigned process when the supervisor
        /// exits, by any path (`KILL_ON_JOB_CLOSE`).
        pub const REAPS_ON_SUPERVISOR_EXIT: bool = true;

        /// Create the job and configure `KILL_ON_JOB_CLOSE | BREAKAWAY_OK`.
        ///
        /// Errors propagate the underlying `GetLastError` value via
        /// `windows_sys`'s raw FFI. On error, no resources are leaked.
        pub fn create() -> Result<Self> {
            // SAFETY: passing nulls for both attributes (default security)
            // and name (unnamed job). Return value is the new job handle on
            // success, or NULL on failure.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                // SAFETY: GetLastError is always safe to call.
                let err = unsafe { GetLastError() };
                return Err(anyhow!("CreateJobObjectW failed (GetLastError = {})", err));
            }

            // SAFETY: zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a valid
            // initial state — every field is "no limit applied". We then
            // set only the LimitFlags we care about.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = LIMIT_FLAGS;

            // SAFETY: handle is a freshly-created job we own; we pass a
            // pointer to a stack-allocated info struct of the size declared
            // via `size_of` in the same call. `JobObjectExtendedLimitInformation`
            // is the documented info class for this struct shape.
            let ok = unsafe {
                SetInformationJobObject(
                    raw,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                // SAFETY: GetLastError is always safe to call.
                let err = unsafe { GetLastError() };
                // SAFETY: raw is the handle we just created and have not closed.
                unsafe {
                    CloseHandle(raw);
                }
                return Err(anyhow!(
                    "SetInformationJobObject failed (GetLastError = {})",
                    err
                ));
            }

            Ok(Self {
                handle: SendSyncHandle(raw),
            })
        }

        /// Assign a running process to this job.
        ///
        /// We pass `CREATE_SUSPENDED` to the spawn step? No — we are NOT
        /// using CREATE_SUSPENDED, so the child IS executing when we
        /// assign. That's fine for our use case: the only correctness
        /// trap is `JOB_OBJECT_LIMIT_BREAKAWAY_OK` interactions where a
        /// child could spawn a grand-child OUTSIDE the job before we
        /// assign — we don't need that guarantee here. Modern Windows
        /// also supports nested jobs, so even if the child were already
        /// in some other job (it isn't), assignment would still succeed.
        ///
        /// Uses minimum-privilege `PROCESS_SET_QUOTA | PROCESS_TERMINATE`
        /// rather than `PROCESS_ALL_ACCESS`.
        pub fn assign(&self, pid: u32) -> Result<()> {
            assign_pid_to(self.handle.0, pid)
        }
    }

    impl Drop for RunnerJob {
        /// Closes the job handle. When this is the last open handle to the
        /// job (which it always is, in our design — we never duplicate),
        /// the kernel terminates every assigned process per the
        /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` flag.
        fn drop(&mut self) {
            // SAFETY: handle was created in `create()` and not closed
            // anywhere else.
            let ok = unsafe { CloseHandle(self.handle.0) };
            if ok == 0 {
                // SAFETY: GetLastError is always safe to call.
                let err = unsafe { GetLastError() };
                warn!(
                    "RunnerJob::drop: CloseHandle failed (GetLastError = {}) — \
                     spawned runners may not be killed",
                    err
                );
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    //! No-op stub for non-Windows targets. The supervisor DOES run here
    //! (Linux dev boxes), but there is no kill-on-close kernel object to
    //! back it: `create` and `assign` succeed and do nothing, and
    //! [`RunnerJob::REAPS_ON_SUPERVISOR_EXIT`] is `false`, so a temp runner
    //! "assigned" here is NOT terminated when the supervisor dies. The
    //! module docs name what reaps it instead.
    use anyhow::Result;

    pub struct RunnerJob;

    impl RunnerJob {
        /// Nothing in the OS reaps an "assigned" process when the supervisor
        /// exits on this platform.
        pub const REAPS_ON_SUPERVISOR_EXIT: bool = false;

        pub fn create() -> Result<Self> {
            Ok(Self)
        }

        pub fn assign(&self, _pid: u32) -> Result<()> {
            Ok(())
        }
    }
}

// `RunnerJob` spans the supervisor lifetime and keeps the ephemeral runners
// assigned to it alive until the supervisor exits (kill-on-job-close). The
// per-build tree-kill job is provided by the `process-wrap` crate's
// `JobObject`/`ProcessGroup` wrappers inside `guarded_command::GuardedCommand`,
// not by a hand-rolled job here.
pub use imp::RunnerJob;

/// Ownership routing for the kill-on-exit JobObject: may this runner kind be
/// assigned to it?
///
/// **Temp runners are supervisor-owned and SHOULD die with the supervisor; the
/// primary, named, and external runners are user-owned and SHOULD NOT.** Only
/// `Temp` is assigned; every other kind gets no job assignment at all (the
/// supervisor already tracks their pids in `state.runners`, so a second
/// kernel object holding processes we have decided not to kill would buy
/// nothing).
///
/// Written as an explicit `is_temp()` **allowlist**, never as
/// `!kind.is_primary()`: [`RunnerKind`] is `#[non_exhaustive]`, so a variant
/// added upstream later must default to *not reaped*. Getting this inverted is
/// exactly the 2026-07-27 incident.
///
/// The orphan-locks-a-slot-binary rationale the job was built for does not
/// apply to user-owned runners: every runner start copies its resolved source
/// exe to `target/debug/qontinui-runner-{id}.exe` (the primary's flat copy),
/// so a surviving primary holds
/// a lock on its own copy, never on a `target-pool/slot-{k}` binary.
pub fn should_assign_to_ephemeral_job(kind: &RunnerKind) -> bool {
    kind.is_temp()
}

/// What reaps a temp runner when the OS does not — named in every job-semantics
/// line off Windows so the operator reading it knows where to look.
const OFF_WINDOWS_TEMP_REAPERS: &str = "they are reaped by the graceful-shutdown sweep \
     (SIGTERM, Ctrl-C or POST /supervisor/shutdown), by the supervisor's systemd unit when \
     supervisor-verbs.sh launched it in one, and by the next supervisor's ledger-gated \
     reconcile sweep";

/// The startup line about the runner job, derived from the platform fact
/// ([`RunnerJob::REAPS_ON_SUPERVISOR_EXIT`]) so it never names a reaper that
/// does not run.
pub fn job_created_message(reaps_on_supervisor_exit: bool) -> String {
    if reaps_on_supervisor_exit {
        "Created kill-on-exit JobObject for spawned temp runners \
         (KILL_ON_JOB_CLOSE; user-owned runners are never assigned)"
            .to_string()
    } else {
        format!(
            "Runner job is a no-op on this platform: the OS does NOT reap temp runners when the \
             supervisor dies (the kill-on-close job is Windows-only); {OFF_WINDOWS_TEMP_REAPERS}."
        )
    }
}

/// The per-spawn line for a temp runner that was routed to the runner job.
pub fn temp_assigned_message(reaps_on_supervisor_exit: bool, runner: &str, pid: u32) -> String {
    if reaps_on_supervisor_exit {
        format!("Assigned temp runner '{runner}' (PID {pid}) to kill-on-exit JobObject")
    } else {
        format!(
            "Temp runner '{runner}' (PID {pid}): the OS does NOT reap it if the supervisor dies \
             (no kill-on-close job off Windows); {OFF_WINDOWS_TEMP_REAPERS}."
        )
    }
}

/// The per-spawn line for a user-owned runner (never routed to the job), with
/// the Linux spawn-isolation verdict beside it.
pub fn user_owned_spawn_message(
    reaps_on_supervisor_exit: bool,
    runner: &str,
    pid: u32,
    isolation: &crate::process::systemd_scope::Isolation,
) -> String {
    if reaps_on_supervisor_exit {
        format!(
            "Runner '{runner}' (PID {pid}) NOT assigned to the kill-on-exit JobObject — \
             user-owned runners survive supervisor exit."
        )
    } else {
        format!(
            "Runner '{runner}' (PID {pid}) is user-owned: the shutdown sweep does NOT reap it, and \
             its survival of the supervisor's exit rests on its isolation — {}.",
            isolation.describe()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds() -> Vec<RunnerKind> {
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

    /// The routing predicate itself: temps are reaped with the supervisor,
    /// user-owned runners are not.
    #[test]
    fn ephemeral_job_routes_temp_only() {
        assert!(
            should_assign_to_ephemeral_job(&RunnerKind::Temp {
                id: "test-abc123".to_string()
            }),
            "temp runners are supervisor-owned and must be reaped on exit"
        );
        assert!(
            !should_assign_to_ephemeral_job(&RunnerKind::Primary),
            "the operator's primary must survive supervisor exit (2026-07-27 incident)"
        );
        assert!(
            !should_assign_to_ephemeral_job(&RunnerKind::Named {
                name: "named-9880".to_string()
            }),
            "named runners hold user sessions and must survive supervisor exit"
        );
        assert!(
            !should_assign_to_ephemeral_job(&RunnerKind::External),
            "external runners were never ours to kill"
        );
    }

    /// Non-exhaustive guard: the predicate must be an `is_temp()` ALLOWLIST.
    /// Anything that is not `Temp` — including a `RunnerKind` variant added
    /// upstream after this was written — must never be assigned.
    #[test]
    fn ephemeral_job_predicate_is_a_temp_allowlist() {
        for kind in all_kinds() {
            assert_eq!(
                should_assign_to_ephemeral_job(&kind),
                kind.is_temp(),
                "assignment must track is_temp() exactly, not a !is_primary() denylist: {kind:?}"
            );
        }
    }

    /// The job MUST keep killing on close — that is what still reaps temp
    /// runners on every supervisor exit path. What must never regress is *who
    /// gets assigned to it* (the two tests above), not the flag itself.
    #[cfg(windows)]
    #[test]
    fn limit_flags_still_kill_on_job_close() {
        use windows_sys::Win32::System::JobObjects::{
            JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        assert_ne!(
            imp::LIMIT_FLAGS & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            0,
            "KILL_ON_JOB_CLOSE is what reaps temp runners on supervisor exit"
        );
        assert_ne!(
            imp::LIMIT_FLAGS & JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            0,
            "BREAKAWAY_OK is retained so a child can opt out at CreateProcess time"
        );
    }

    /// Off Windows the stub reaps nothing, so no job-semantics line may name a
    /// kill-on-exit JobObject, and each must say the OS does NOT reap. Before
    /// plan `2026-11-04-linux-dev-supervisor-…` Phase 1 the Linux log claimed
    /// "Created kill-on-exit JobObject … (KILL_ON_JOB_CLOSE …)" over a no-op.
    #[cfg(not(windows))]
    #[test]
    // The platform fact IS a constant; asserting it is the point.
    #[allow(clippy::assertions_on_constants)]
    fn off_windows_job_messages_claim_no_reaper() {
        use crate::process::systemd_scope::Isolation;
        assert!(
            !RunnerJob::REAPS_ON_SUPERVISOR_EXIT,
            "the non-Windows stub must not claim to reap on supervisor exit"
        );
        let reaps = RunnerJob::REAPS_ON_SUPERVISOR_EXIT;
        let messages = [
            job_created_message(reaps),
            temp_assigned_message(reaps, "test-abc123", 4242),
            user_owned_spawn_message(
                reaps,
                "named-9880",
                4243,
                &Isolation::Scoped("qontinui-runner-named-9880-20261104T230000000Z.scope".into()),
            ),
            user_owned_spawn_message(
                reaps,
                "named-9881",
                4244,
                &Isolation::Unscoped("systemd-run is not on PATH".into()),
            ),
        ];
        for msg in &messages {
            assert!(
                !msg.contains("kill-on-exit JobObject"),
                "a non-Windows job-semantics line names a reaper that does not run: {msg}"
            );
            assert!(
                msg.contains("NOT reap"),
                "a non-Windows job-semantics line must say what is NOT reaped: {msg}"
            );
        }
        // And the temp lines name what DOES reap.
        for msg in &messages[..2] {
            assert!(
                msg.contains("graceful-shutdown sweep") && msg.contains("reconcile sweep"),
                "the temp-runner lines must name the real reapers: {msg}"
            );
        }
    }

    /// The Windows wording is unchanged: there the job really reaps.
    #[test]
    fn windows_job_messages_name_the_job() {
        use crate::process::systemd_scope::Isolation;
        assert!(job_created_message(true).contains("KILL_ON_JOB_CLOSE"));
        assert!(temp_assigned_message(true, "t", 1).contains("kill-on-exit JobObject"));
        assert!(
            user_owned_spawn_message(true, "n", 2, &Isolation::NotApplicable)
                .contains("NOT assigned to the kill-on-exit JobObject")
        );
    }

    // The kernel-level "assigned child dies, unassigned child survives" test
    // lives in `tests/job_object_kill_on_close.rs` — spawning processes and
    // manipulating Win32 jobs alongside `guarded_command`'s wall-clock
    // tree-kill tests in the SAME binary made `AssignProcessToJobObject`
    // return ERROR_ACCESS_DENIED and blew their timing budgets. Cargo runs
    // test targets one at a time, so its own binary isolates it.
}
