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
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

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
            // SAFETY: OpenProcess with these access rights is documented as
            // safe for any PID owned by the current user (or with admin).
            let proc_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if proc_handle.is_null() {
                // SAFETY: GetLastError is always safe to call.
                let err = unsafe { GetLastError() };
                return Err(anyhow!(
                    "OpenProcess(pid={}) failed (GetLastError = {})",
                    pid,
                    err
                ))
                .with_context(|| format!("RunnerJob::assign(pid = {pid})"));
            }

            // SAFETY: both handles are valid kernel handles owned by us.
            let ok = unsafe { AssignProcessToJobObject(self.handle.0, proc_handle) };

            // Always close the per-process handle — the assignment is
            // recorded by the kernel and does not depend on us holding
            // the duplicate handle. Failing to close it would leak.
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
}

pub use imp::RunnerJob;
