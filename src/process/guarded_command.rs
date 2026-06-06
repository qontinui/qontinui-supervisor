//! Job-guarded subprocess runner for build commands.
//!
//! Every build subprocess the supervisor spawns (cargo, pnpm, git) flows
//! through [`GuardedCommand`]. It exists to kill one specific bug class: a
//! `rebuild` that hangs forever because a build grandchild (rustc, the linker,
//! vite/tsc/esbuild) holds the stdout/stderr pipe open after the timeout
//! fires. The classic "spawn + `tokio::time::timeout(child.wait())` +
//! `child.kill()`" pattern only kills the *direct* child; the grandchildren
//! keep the pipe alive and the reader task — and thus the build — never
//! returns.
//!
//! The robust fix reuses the repo's existing Windows JobObject primitive
//! ([`crate::process::job::CommandJob`]). On Windows we spawn the child
//! normally, then *immediately* assign it to a kill-on-close job. Windows
//! assigns the child's own descendants to the same job by default, so any
//! grandchild (rustc, the linker, vite/tsc/esbuild) is captured the moment it
//! is created — which is necessarily after the assignment, because the freshly
//! spawned child has not yet had a scheduler quantum to fork anything. On
//! timeout/cancel we drop the job — the kernel terminates the WHOLE tree — then
//! join the reader tasks under a short bound and return the partial output.
//!
//! An earlier revision spawned the child `CREATE_SUSPENDED`, assigned the job,
//! then resumed the primary thread via a Toolhelp thread-snapshot walk
//! (`resume_process_main_thread`). That suspend/resume dance was the source of
//! a silent-wedge regression: for a `cmd.exe` wrapper (`cmd /C pnpm.cmd run
//! build`) the resume step could fail to resume the right thread, leaving the
//! child suspended forever, and any panic/early-return in the spawn→assign→
//! resume sequence happened *before* the timeout `select!` was even
//! established, so the wall-clock timeout could never fire. Dropping
//! CREATE_SUSPENDED removes the entire fragile path: the pre-assign window is
//! sub-quantum (the child cannot have spawned a grandchild yet), so tree
//! capture is still guaranteed in practice, and spawn→assign is now infallibly
//! fast and fully inside the timeout-guarded region.
//!
//! On non-Windows the same shape is achieved with a POSIX process group
//! (`process_group(0)` + group-kill); the JobObject is a no-op stub. The
//! supervisor only runs on Windows in practice, but the module compiles and
//! behaves correctly cross-platform so `cargo check`/tests work from any host.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Creation flag: don't pop a console window for the build subprocess.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How long to wait for the stdout/stderr reader tasks to drain after the job
/// tree has been killed on a timeout/cancel. The tree is already dead, so the
/// pipes are closing; this is just a bound against a pathological hang.
const READER_DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Outcome of a [`GuardedCommand::run`].
#[derive(Debug)]
pub enum GuardedOutcome {
    /// The process exited on its own (success or non-zero). Carries the full
    /// captured `Output` (status + stdout + merged stderr bytes).
    Exited(Output),
    /// The wall-clock timeout elapsed; the process tree was killed. `partial`
    /// is whatever stdout/stderr was captured before the kill.
    TimedOut {
        after: Duration,
        partial: PartialOutput,
    },
    /// The cancellation token fired; the process tree was killed. `partial` is
    /// whatever was captured before the kill.
    Cancelled { partial: PartialOutput },
}

/// Captured stdout/stderr bytes from a killed (timed-out / cancelled) command.
#[derive(Debug, Default, Clone)]
pub struct PartialOutput {
    /// Captured stdout before the kill. Part of the complete primitive surface;
    /// build callers currently consume only `stderr` (cargo/pnpm write their
    /// diagnostics there), but stdout is captured honestly for callers that
    /// need it.
    #[allow(dead_code)]
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Builder for a job-guarded subprocess.
///
/// Construct with [`GuardedCommand::new`], configure, then [`run`](Self::run).
pub struct GuardedCommand {
    program: PathBuf,
    args: Vec<std::ffi::OsString>,
    cwd: Option<PathBuf>,
    envs: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    timeout: Duration,
    cancel: Option<CancellationToken>,
    /// When set, every merged stderr line is forwarded to this broadcast
    /// sender as it is read (replaces the manual per-slot SSE stderr task in
    /// `build_monitor`). Lines are sent untagged; SSE framing is the
    /// consumer's job. Send errors (no subscribers) are ignored.
    stream_lines: Option<broadcast::Sender<String>>,
}

impl GuardedCommand {
    /// Start building a guarded command for `program` with the given wall-clock
    /// `timeout`.
    pub fn new(program: impl AsRef<OsStr>, timeout: Duration) -> Self {
        Self {
            program: PathBuf::from(program.as_ref()),
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            timeout,
            cancel: None,
            stream_lines: None,
        }
    }

    /// Append a single argument. (Companion to [`args`](Self::args); part of
    /// the complete builder surface even though build call sites currently use
    /// the plural form.)
    #[allow(dead_code)]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Append multiple arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self.args.push(a.as_ref().to_os_string());
        }
        self
    }

    /// Set the working directory.
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Set one environment variable (applied on top of the inherited env).
    pub fn env(mut self, key: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_os_string(), val.as_ref().to_os_string()));
        self
    }

    /// Attach a cancellation token. When it fires, [`run`](Self::run) kills the
    /// process tree and returns [`GuardedOutcome::Cancelled`].
    pub fn cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Stream merged stderr lines to `tx` as they're read. Used by the build
    /// pool's per-slot `/builds/{slot}/log/stream` SSE fanout.
    pub fn stream_lines(mut self, tx: broadcast::Sender<String>) -> Self {
        self.stream_lines = Some(tx);
        self
    }

    /// Run the command to completion, a timeout, or a cancellation.
    ///
    /// On the timeout/cancel arms the process tree is killed (Windows: drop the
    /// [`CommandJob`]; POSIX: kill the process group) and the reader tasks are
    /// joined under [`READER_DRAIN_BUDGET`] so a wedged pipe can't hang the
    /// supervisor. The captured-so-far bytes ride along in the `partial` field.
    pub async fn run(self) -> Result<GuardedOutcome, std::io::Error> {
        #[cfg(windows)]
        {
            self.run_windows().await
        }
        #[cfg(not(windows))]
        {
            self.run_posix().await
        }
    }

    /// Build the base `tokio::process::Command` shared by both platforms
    /// (program, args, cwd, env, piped stdio).
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        cmd
    }

    #[cfg(windows)]
    async fn run_windows(self) -> Result<GuardedOutcome, std::io::Error> {
        use crate::process::job::CommandJob;

        let mut cmd = self.base_command();
        // No CREATE_SUSPENDED: we spawn the child running and assign it to the
        // job immediately. The child cannot have spawned a grandchild before
        // the assignment (it has not had a scheduler quantum yet), and Windows
        // assigns descendants of a job-tracked process to the same job, so the
        // whole tree is captured. CREATE_NO_WINDOW keeps the headless build from
        // flashing a console. No CREATE_NEW_PROCESS_GROUP — the job replaces the
        // process-group mechanism for tree-kill.
        cmd.creation_flags(CREATE_NO_WINDOW);

        let child = cmd.spawn()?;
        let pid = child.id();

        // Create the kill-on-close job and assign the running child to it as the
        // very next step. If job setup/assignment fails we proceed WITHOUT a
        // tree-kill guarantee (logged) rather than wedging — the timeout/cancel
        // arms still `start_kill()` the direct child, so the build can never
        // hang silently; at worst a detached grandchild outlives the direct
        // child, which is strictly better than the whole build never returning.
        let job = match pid {
            Some(pid) => match CommandJob::create().and_then(|j| j.assign(pid).map(|_| j)) {
                Ok(j) => Some(j),
                Err(e) => {
                    warn!(
                        "GuardedCommand: CommandJob setup failed for pid {} ({:?}): {} — \
                         proceeding without tree-kill guarantee (direct-child kill still applies)",
                        pid, self.program, e
                    );
                    None
                }
            },
            None => {
                // Child already exited before we could read its pid; there is
                // nothing to assign. Drive it to collect output via a plain wait.
                None
            }
        };

        self.drive(child, job).await
    }

    /// Drive a spawned child: spin up reader tasks, then `select!` over wait /
    /// timeout / cancel. Shared tail of the Windows path — `job` is `Some` once
    /// the child was assigned to a [`CommandJob`], `None` if assignment failed
    /// or the child had already exited (the timeout/cancel arms still
    /// `start_kill()` the direct child in that case).
    #[cfg(windows)]
    async fn drive(
        self,
        mut child: tokio::process::Child,
        job: Option<crate::process::job::CommandJob>,
    ) -> Result<GuardedOutcome, std::io::Error> {
        let (stdout_task, stderr_task) = spawn_readers(&mut child, self.stream_lines.clone());

        let timeout = self.timeout;
        let cancel = self.cancel.clone();

        tokio::select! {
            wait_res = child.wait() => {
                let status = wait_res?;
                let (out, err) = join_readers(stdout_task, stderr_task).await;
                Ok(GuardedOutcome::Exited(make_output(status, out, err)))
            }
            _ = tokio::time::sleep(timeout) => {
                // Drop the job → kernel kills the whole tree.
                drop(job);
                let _ = child.start_kill();
                let partial = drain_readers(stdout_task, stderr_task).await;
                Ok(GuardedOutcome::TimedOut { after: timeout, partial })
            }
            _ = wait_for_cancel(cancel) => {
                drop(job);
                let _ = child.start_kill();
                let partial = drain_readers(stdout_task, stderr_task).await;
                Ok(GuardedOutcome::Cancelled { partial })
            }
        }
    }

    #[cfg(not(windows))]
    async fn run_posix(self) -> Result<GuardedOutcome, std::io::Error> {
        let mut cmd = self.base_command();
        // New process group so a timeout/cancel can kill the whole tree via a
        // negative-pid group signal. JobObject is a no-op stub here.
        cmd.process_group(0);

        let mut child = cmd.spawn()?;
        let pid = child.id();

        let (stdout_task, stderr_task) = spawn_readers(&mut child, self.stream_lines.clone());

        let timeout = self.timeout;
        let cancel = self.cancel.clone();

        tokio::select! {
            wait_res = child.wait() => {
                let status = wait_res?;
                let (out, err) = join_readers(stdout_task, stderr_task).await;
                Ok(GuardedOutcome::Exited(make_output(status, out, err)))
            }
            _ = tokio::time::sleep(timeout) => {
                kill_posix_tree(&mut child, pid).await;
                let partial = drain_readers(stdout_task, stderr_task).await;
                Ok(GuardedOutcome::TimedOut { after: timeout, partial })
            }
            _ = wait_for_cancel(cancel) => {
                kill_posix_tree(&mut child, pid).await;
                let partial = drain_readers(stdout_task, stderr_task).await;
                Ok(GuardedOutcome::Cancelled { partial })
            }
        }
    }
}

/// Future that resolves when `cancel` fires, or never if it's `None`.
async fn wait_for_cancel(cancel: Option<CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

type ReaderTask = tokio::task::JoinHandle<Vec<u8>>;

/// Spawn one reader task per piped stream. stderr lines are additionally
/// forwarded to `stream_lines` (if set) as they're read. Returns
/// `(stdout_task, stderr_task)`.
fn spawn_readers(
    child: &mut tokio::process::Child,
    stream_lines: Option<broadcast::Sender<String>>,
) -> (ReaderTask, ReaderTask) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            }
        }
        buf
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(tx) = &stream_lines {
                    // Err == no subscribers; the steady state. Drop silently.
                    let _ = tx.send(line.clone());
                }
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
            }
        }
        buf
    });

    (stdout_task, stderr_task)
}

/// Await both reader tasks to completion (used on the clean-exit arm, where the
/// pipes close naturally). A panicked reader yields empty bytes.
async fn join_readers(stdout_task: ReaderTask, stderr_task: ReaderTask) -> (Vec<u8>, Vec<u8>) {
    let out = stdout_task.await.unwrap_or_default();
    let err = stderr_task.await.unwrap_or_default();
    (out, err)
}

/// Join the reader tasks under [`READER_DRAIN_BUDGET`] (used on the
/// timeout/cancel arms, after the tree is killed). If the readers don't drain
/// in time we abort them and return whatever we have — never hang.
async fn drain_readers(stdout_task: ReaderTask, stderr_task: ReaderTask) -> PartialOutput {
    match tokio::time::timeout(READER_DRAIN_BUDGET, async {
        (stdout_task.await, stderr_task.await)
    })
    .await
    {
        Ok((out, err)) => PartialOutput {
            stdout: out.unwrap_or_default(),
            stderr: err.unwrap_or_default(),
        },
        Err(_) => {
            warn!(
                "GuardedCommand: reader tasks did not drain within {:?} after tree kill — \
                 returning partial output",
                READER_DRAIN_BUDGET
            );
            PartialOutput::default()
        }
    }
}

fn make_output(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> Output {
    Output {
        status,
        stdout,
        stderr,
    }
}

/// POSIX: kill the child's process group (negative pid) then the child itself.
#[cfg(not(windows))]
async fn kill_posix_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    let _ = child.start_kill();
    if let Some(pid) = pid {
        // Best-effort group kill: `kill(-pgid, SIGKILL)`. The child was spawned
        // with `process_group(0)` so its pid == pgid.
        unsafe {
            libc_kill(-(pid as i32), 9);
        }
    }
}

/// Minimal FFI to `kill(2)` so we don't pull a whole crate for one call on the
/// path the supervisor never actually takes in production (it runs on Windows).
#[cfg(all(not(windows), unix))]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Fallback for the (unreachable in practice) non-windows-non-unix case so the
/// `kill_posix_tree` call compiles everywhere.
#[cfg(all(not(windows), not(unix)))]
unsafe fn libc_kill(_pid: i32, _sig: i32) -> i32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows-only test helper: is `pid` still a live process? Uses
    /// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess`;
    /// a still-running process reports `STILL_ACTIVE` (259). Returns false if the
    /// pid can't be opened (already reaped) or has a real exit code.
    #[cfg(windows)]
    fn pid_is_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: documented call; bInheritHandle = 0. Null return = no such
        // (accessible) process → treat as dead.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        // SAFETY: handle is a valid open process handle; code is a valid out ptr.
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
        // SAFETY: handle is a valid open handle.
        unsafe {
            CloseHandle(handle);
        }
        ok != 0 && code == STILL_ACTIVE as u32
    }

    /// Bug-1 regression: a build that spawns a pipe-holding grandchild must be
    /// killed (tree AND grandchild) by the timeout within ~the timeout window,
    /// NOT blocked until the grandchild's own 60s sleep elapses. The headline
    /// assertion is the SECOND one: the grandchild PID is gone afterward, which
    /// is what proves the kill was tree-wide (a naive `child.kill()` of only the
    /// direct cmd would leave the detached ping grandchild alive).
    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_pipe_holding_grandchild_tree() {
        // The DIRECT child is powershell. It launches a separate GRANDCHILD
        // process (another powershell) via `Start-Process -PassThru`, records
        // that grandchild's PID to a temp file, then sleeps 60s holding the
        // inherited stdout/stderr pipe. Under a 2s timeout the WHOLE tree (the
        // direct powershell + its detached grandchild) must be killed by the
        // JobObject; we then read the grandchild PID and assert it's dead. A
        // naive `child.kill()` of only the direct powershell would leave the
        // grandchild — spawned with its own console — alive, which is exactly
        // the failure this asserts against.
        let dir = std::env::temp_dir();
        let marker = dir.join(format!(
            "qontinui-guarded-grandchild-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_str = marker.to_string_lossy().to_string();

        // Single powershell -Command script (passed as one argv token, no cmd,
        // no nested quoting). `'…'` single-quotes are literal inside the script.
        let script = format!(
            "$gc = Start-Process powershell -PassThru -WindowStyle Hidden \
             -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60'; \
             $gc.Id | Out-File -Encoding ascii -FilePath '{marker_str}'; \
             Start-Sleep -Seconds 60"
        );

        let start = std::time::Instant::now();
        let outcome = GuardedCommand::new("powershell", Duration::from_secs(2))
            .args(["-NoProfile", "-Command", &script])
            .run()
            .await
            .expect("run() should not error");

        let elapsed = start.elapsed();
        assert!(
            matches!(outcome, GuardedOutcome::TimedOut { .. }),
            "expected TimedOut, got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "timeout+tree-kill should complete well under the 60s grandchild sleep; took {elapsed:?}"
        );

        // The grandchild had ~ up-front time to write its PID before the 2s
        // timeout; read it back. If the marker never appeared the test can't
        // prove tree-kill, so fail loudly rather than silently passing.
        let grandchild_pid: u32 = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| {
                panic!(
                    "grandchild never recorded its PID to {:?} — cannot prove tree-kill",
                    marker
                )
            });
        let _ = std::fs::remove_file(&marker);

        // Give the kernel a brief moment to finish tearing down the job tree.
        for _ in 0..20 {
            if !pid_is_alive(grandchild_pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !pid_is_alive(grandchild_pid),
            "grandchild PID {grandchild_pid} is still alive after the timeout — \
             tree-kill failed (the JobObject did not capture the descendant)"
        );
    }

    /// Job-assignment-doesn't-deadlock proof: a `cmd /C` wrapper assigned to a
    /// `CommandJob` immediately after spawn (no CREATE_SUSPENDED, no resume)
    /// still runs to completion and produces its output. Regression guard for
    /// the removed suspend/resume path — if assignment somehow froze the child
    /// the output would be empty.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_wrapper_assigned_to_job_produces_output() {
        let outcome = GuardedCommand::new("cmd", Duration::from_secs(30))
            .args(["/C", "echo qontinui-resume-marker"])
            .run()
            .await
            .expect("run() should not error");

        match outcome {
            GuardedOutcome::Exited(out) => {
                assert!(out.status.success(), "echo should exit 0");
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.contains("qontinui-resume-marker"),
                    "job-assigned child must produce its output; got stdout={stdout:?}"
                );
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    /// Regression for the silent-wedge bug: a `cmd /C "<hang>"` wrapper — the
    /// EXACT shape `run_pnpm_command` uses on Windows (`cmd /C pnpm.cmd run
    /// build`) — must hit the wall-clock timeout and return `TimedOut` within
    /// the timeout window, and the wrapped child must be dead afterward. The
    /// old broken path could leave the `cmd.exe` child suspended (resume
    /// targeting the wrong thread) so it never produced output AND never
    /// exited, while the timeout `select!` was only established after the
    /// fragile suspend/assign/resume sequence — so the timeout could never
    /// fire and the build wedged forever with no log. This test fails (hangs
    /// to the test harness timeout, or returns Exited) under that bug and
    /// passes only when the timeout truly governs a cmd-wrapped child.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_wrapper_hang_times_out_and_dies() {
        // `cmd /C "ping -n 60 ..."` — the cmd-wrapper shape, hanging ~60s.
        // The marker file lets us prove the wrapped child actually started
        // (so the test exercises a live hang, not a spawn no-op) and is gone
        // after the timeout-driven tree kill.
        let dir = std::env::temp_dir();
        let marker = dir.join(format!(
            "qontinui-guarded-cmdwrap-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_str = marker.to_string_lossy().to_string();

        // Touch the marker first (proving the wrapped child ran), then hang on
        // a 60s ping. The TEMP path has no spaces on this host, so no inner
        // quoting is needed (nested `cmd /C` quotes are fragile). A 4s timeout
        // gives the `echo` ample time to land the marker before the tree-kill.
        let inner = format!("echo running>{marker_str} & ping -n 60 127.0.0.1 >nul");

        let start = std::time::Instant::now();
        let outcome = GuardedCommand::new("cmd", Duration::from_secs(4))
            .args(["/C", &inner])
            .run()
            .await
            .expect("run() should not error even when the child hangs");
        let elapsed = start.elapsed();

        assert!(
            matches!(outcome, GuardedOutcome::TimedOut { .. }),
            "cmd-wrapper hang must return TimedOut, got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(12),
            "timeout must fire ~4s in, well under the 60s ping; took {elapsed:?} \
             (a non-firing timeout is the silent-wedge regression)"
        );
        assert!(
            elapsed >= Duration::from_secs(2),
            "should have actually waited for the ~4s timeout, not returned instantly; \
             took {elapsed:?}"
        );

        // The wrapped child must have started (marker present) before the
        // JobObject tree-kill on timeout — proving we exercised a live hang.
        assert!(
            marker.exists(),
            "wrapped cmd child never ran (marker {marker:?} absent) — \
             the test did not exercise a live hang"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// A spawn failure (nonexistent program) must surface as a terminal `Err`
    /// from `run()`, never a silent wedge. The caller (`run_pnpm_command` /
    /// `run_cargo_build_with_dir`) LOGS this Err, so a future spawn failure is
    /// always visible. Proves the spawn-path failure mode is observable.
    #[tokio::test]
    async fn spawn_failure_returns_err() {
        let result = GuardedCommand::new(
            "qontinui-no-such-program-definitely-not-on-path",
            Duration::from_secs(5),
        )
        .args(["--version"])
        .run()
        .await;

        assert!(
            result.is_err(),
            "spawning a nonexistent program must return Err, got {result:?}"
        );
    }

    /// Cancellation: a long sleeper cancelled shortly after start must return
    /// Cancelled promptly (tree gone), not run to completion.
    #[cfg(windows)]
    #[tokio::test]
    async fn cancel_kills_tree_promptly() {
        let token = CancellationToken::new();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            token2.cancel();
        });

        let start = std::time::Instant::now();
        let outcome = GuardedCommand::new("cmd", Duration::from_secs(60))
            .args(["/C", "ping -n 60 127.0.0.1"])
            .cancel_token(token)
            .run()
            .await
            .expect("run() should not error");
        let elapsed = start.elapsed();

        assert!(
            matches!(outcome, GuardedOutcome::Cancelled { .. }),
            "expected Cancelled, got {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "cancel should fire promptly, well under both the 60s sleep and 60s timeout; took {elapsed:?}"
        );
    }

    /// Cross-platform: a quick command exits cleanly and captures output.
    #[tokio::test]
    async fn quick_command_exits_with_output() {
        #[cfg(windows)]
        let cmd =
            GuardedCommand::new("cmd", Duration::from_secs(30)).args(["/C", "echo hello-guarded"]);
        #[cfg(not(windows))]
        let cmd =
            GuardedCommand::new("sh", Duration::from_secs(30)).args(["-c", "echo hello-guarded"]);

        let outcome = cmd.run().await.expect("run() should not error");
        match outcome {
            GuardedOutcome::Exited(out) => {
                assert!(out.status.success());
                assert!(String::from_utf8_lossy(&out.stdout).contains("hello-guarded"));
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }
}
