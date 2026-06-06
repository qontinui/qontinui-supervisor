//! Windows JobObject wrapper for kill-on-supervisor-exit semantics.
//!
//! On Windows, parent process death does NOT cascade to children (unlike
//! Unix `prctl(PR_SET_PDEATHSIG)`). When the supervisor force-restarts,
//! runners it spawned stay alive as orphans, hold open slot binaries, and
//! break subsequent cargo builds.
//!
//! Solution: at supervisor startup, create a Win32 JobObject with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Every spawned runner is assigned
//! to that job. When the supervisor exits — graceful, panic, force-kill,
//! or BSOD — the kernel closes the last handle to the job and terminates
//! every assigned process. WebView2 children of the runner are also
//! transitively in the job (Windows assigns child processes of a
//! job-tracked process to the same job by default; no extra flag needed).
//!
//! On non-Windows platforms this is a no-op stub. The supervisor only
//! actually runs on Windows, but compiling cleanly cross-platform keeps
//! `cargo check` ergonomic and avoids `#[cfg(windows)]` sprinkling at
//! every call site.

#[cfg(windows)]
mod imp {
    use anyhow::{anyhow, Context, Result};
    use std::mem::{size_of, zeroed};
    use tracing::warn;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenThread, ResumeThread, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        THREAD_SUSPEND_RESUME,
    };

    /// Assign a running process (by pid) to an already-created job handle.
    ///
    /// Factored out of [`RunnerJob::assign`] so both the supervisor's
    /// process-lifetime runner job and the per-build [`CommandJob`] share one
    /// audited implementation. Uses minimum-privilege
    /// `PROCESS_SET_QUOTA | PROCESS_TERMINATE` rather than `PROCESS_ALL_ACCESS`.
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

    /// Resume the primary (and every) thread of a `CREATE_SUSPENDED` process.
    ///
    /// We spawn build subprocesses suspended so we can assign them to a
    /// kill-on-close [`CommandJob`] *before* their first instruction runs —
    /// guaranteeing that any grandchild (rustc, linker, vite, esbuild) is
    /// captured by the job and cannot break away. Once assignment is recorded
    /// the process must be resumed or it would hang suspended forever.
    ///
    /// A freshly `CREATE_SUSPENDED` process has exactly one thread (the main
    /// thread, suspend count 1), but we resume every thread owned by the pid
    /// for robustness. Each `ResumeThread` decrements the suspend count; a
    /// thread that was never suspended is left untouched (its count returns 0
    /// → it was already running). Best-effort per-thread: a single failed
    /// `OpenThread`/`ResumeThread` is logged and skipped rather than aborting
    /// the whole resume (which would wedge the build).
    ///
    /// Returns the number of threads successfully resumed, or an error only if
    /// the toolhelp snapshot itself could not be created.
    pub(crate) fn resume_process_main_thread(pid: u32) -> Result<u32> {
        // SAFETY: CreateToolhelp32Snapshot with TH32CS_SNAPTHREAD + pid 0
        // snapshots all threads system-wide; we filter by owner pid below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            // SAFETY: GetLastError is always safe to call.
            let err = unsafe { GetLastError() };
            return Err(anyhow!(
                "CreateToolhelp32Snapshot failed (GetLastError = {})",
                err
            ));
        }

        // SAFETY: zeroed THREADENTRY32 is a valid initial state. dwSize MUST
        // be set to the struct size before Thread32First or the call fails.
        let mut entry: THREADENTRY32 = unsafe { zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;

        let mut resumed: u32 = 0;
        // SAFETY: snapshot is a valid handle; entry is a properly-sized,
        // zero-initialized THREADENTRY32 with dwSize set.
        let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
        while ok != 0 {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: documented call; bInheritHandle = 0.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    // SAFETY: thread is a valid open handle with
                    // THREAD_SUSPEND_RESUME. ResumeThread returns the prior
                    // suspend count, or u32::MAX (-1) on failure.
                    let prev = unsafe { ResumeThread(thread) };
                    // SAFETY: thread is a valid open handle.
                    unsafe {
                        CloseHandle(thread);
                    }
                    if prev != u32::MAX {
                        resumed += 1;
                    } else {
                        // SAFETY: GetLastError is always safe to call.
                        let err = unsafe { GetLastError() };
                        warn!(
                            "ResumeThread(tid={}, pid={}) failed (GetLastError = {})",
                            entry.th32ThreadID, pid, err
                        );
                    }
                }
            }
            // SAFETY: snapshot + entry remain valid for the duration of the loop.
            ok = unsafe { Thread32Next(snapshot, &mut entry) };
        }

        // SAFETY: snapshot is a valid handle we created and have not closed.
        unsafe {
            CloseHandle(snapshot);
        }
        Ok(resumed)
    }

    /// LimitFlags applied to the supervisor's runner job.
    ///
    /// - `KILL_ON_JOB_CLOSE`: the kernel terminates every assigned process when
    ///   the last handle to the job is closed. Since the supervisor holds the
    ///   only handle (no `DuplicateHandle`), "supervisor exits" == "last
    ///   handle closes" == "every runner dies".
    /// - `BREAKAWAY_OK`: lets a deliberately-launched child opt out of the job
    ///   via `CREATE_BREAKAWAY_FROM_JOB`. Harmless to set even if we never
    ///   use the opt-out — it just gives us the option later.
    const LIMIT_FLAGS: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;

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

    /// A short-lived JobObject scoped to a single build subprocess (and its
    /// whole descendant tree).
    ///
    /// Unlike [`RunnerJob`] (which spans the supervisor's whole lifetime and
    /// keeps runners alive), a `CommandJob` owns exactly one cargo/pnpm/git
    /// invocation. The build subprocess is spawned `CREATE_SUSPENDED`, assigned
    /// to this job, then resumed — so every grandchild it spawns (rustc, the
    /// linker, vite/tsc/esbuild) is captured by the job from birth.
    ///
    /// When the [`GuardedCommand`] runner hits a timeout or cancellation it
    /// simply drops the `CommandJob`. Because we hold the only handle and the
    /// job carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, the kernel terminates
    /// the ENTIRE tree — including the pipe-holding grandchildren that would
    /// otherwise keep the stdout/stderr pipe open forever and hang the build.
    ///
    /// Crucially this job does **not** set `JOB_OBJECT_LIMIT_BREAKAWAY_OK`: a
    /// child must not be able to opt its descendants out of the kill-on-close
    /// tree. (`RunnerJob` sets it so a deliberately-detached runner could break
    /// away; for a build tree the opposite is what we want.)
    ///
    /// [`GuardedCommand`]: crate::process::guarded_command::GuardedCommand
    pub struct CommandJob {
        handle: SendSyncHandle,
    }

    impl CommandJob {
        /// Create a kill-on-close job with NO breakaway. See [`CommandJob`].
        pub fn create() -> Result<Self> {
            // SAFETY: passing nulls for both attributes (default security) and
            // name (unnamed job). Returns NULL on failure.
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                // SAFETY: GetLastError is always safe to call.
                let err = unsafe { GetLastError() };
                return Err(anyhow!(
                    "CreateJobObjectW (CommandJob) failed (GetLastError = {})",
                    err
                ));
            }

            // SAFETY: zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a valid
            // "no limits" initial state.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            // KILL_ON_JOB_CLOSE only — deliberately omit BREAKAWAY_OK so build
            // grandchildren are auto-captured and cannot escape the tree.
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: handle is freshly created and owned; info is a properly
            // sized stack struct of the declared info class.
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
                    "SetInformationJobObject (CommandJob) failed (GetLastError = {})",
                    err
                ));
            }

            Ok(Self {
                handle: SendSyncHandle(raw),
            })
        }

        /// Assign a process (by pid) to this job. The process should be the
        /// freshly-spawned, still-suspended build child; assigning before it
        /// runs guarantees descendant capture.
        pub fn assign(&self, pid: u32) -> Result<()> {
            assign_pid_to(self.handle.0, pid)
        }
    }

    impl Drop for CommandJob {
        /// Closes the job handle, which (since we hold the only handle and the
        /// job is `KILL_ON_JOB_CLOSE`) terminates the whole build subprocess
        /// tree. This is the mechanism by which a timed-out/cancelled build
        /// kills its pipe-holding grandchildren.
        fn drop(&mut self) {
            // SAFETY: handle was created in `create()` and not closed elsewhere.
            let ok = unsafe { CloseHandle(self.handle.0) };
            if ok == 0 {
                // SAFETY: GetLastError is always safe to call.
                let err = unsafe { GetLastError() };
                warn!(
                    "CommandJob::drop: CloseHandle failed (GetLastError = {}) — \
                     build subprocess tree may not be killed",
                    err
                );
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    //! No-op stub for non-Windows targets. The supervisor only runs on
    //! Windows, but keeping this module cross-compilable lets `cargo check`
    //! work from any host without `#[cfg(windows)]` decorating every call
    //! site.
    use anyhow::Result;

    pub struct RunnerJob;

    impl RunnerJob {
        pub fn create() -> Result<Self> {
            Ok(Self)
        }

        pub fn assign(&self, _pid: u32) -> Result<()> {
            Ok(())
        }
    }

    /// No-op stub mirror of the Windows [`CommandJob`](super). On non-Windows
    /// platforms the [`GuardedCommand`](crate::process::guarded_command)
    /// runner relies on POSIX process groups instead of a JobObject, so this
    /// type does nothing but satisfy the cross-platform compile.
    pub struct CommandJob;

    impl CommandJob {
        pub fn create() -> Result<Self> {
            Ok(Self)
        }

        pub fn assign(&self, _pid: u32) -> Result<()> {
            Ok(())
        }
    }

    /// No-op stub: there is no `CREATE_SUSPENDED` on non-Windows, so nothing
    /// needs resuming. Returns 0 (threads resumed).
    pub(crate) fn resume_process_main_thread(_pid: u32) -> Result<u32> {
        Ok(0)
    }
}

pub(crate) use imp::resume_process_main_thread;
pub use imp::{CommandJob, RunnerJob};
