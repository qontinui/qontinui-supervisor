//! Job-guarded subprocess runner for build commands.
//!
//! Every build subprocess the supervisor spawns (cargo, pnpm, git) flows
//! through [`GuardedCommand`]. It exists to kill two specific bug classes:
//!
//! 1. **Whole-tree teardown on timeout/cancel.** A `rebuild` that hangs must
//!    take down not just the direct child but every grandchild it forked
//!    (rustc, the linker, vite/tsc/esbuild, a lingering node worker). The
//!    classic "spawn + `tokio::time::timeout(child.wait())` + `child.kill()`"
//!    pattern only kills the *direct* child; the grandchildren survive.
//!
//! 2. **The runtime-starvation wedge.** Earlier revisions read the child's
//!    stdout/stderr through tokio **pipes**. For `cmd /C pnpm.cmd run build`,
//!    build grandchildren (vite/esbuild/node) inherit the pipe write-ends and
//!    never close them, so the pipe-reader tasks can never reach EOF. Those
//!    reader tasks then **starved the async runtime** for the whole
//!    `GuardedCommand::run()` future — neither the exit-wait nor the
//!    `sleep(timeout)` arm of the `select!` was ever driven again, and the
//!    build wedged forever with no log, no timeout, no panic.
//!
//! ## The fix: files, not pipes, plus an established process crate
//!
//! There are **no pipes**. The child's stdout and stderr are redirected to two
//! separate on-disk log files via [`Stdio::from(File)`]. Reading a *file* back
//! into memory after the process exits can never block on a pipe that a
//! detached grandchild is holding open, so the runtime can no longer be
//! starved. Live SSE progress (cargo build output) is produced by a lightweight
//! async **file-tail** task that periodically reads newly-appended bytes from
//! the stderr log and forwards complete lines to a broadcast channel — tailing
//! a file likewise never blocks on pipe EOF.
//!
//! Tree spawn / tree-kill / async wait are delegated to the **`process-wrap`**
//! crate (the maintained successor to `command-group`, by the watchexec
//! author). On Windows the child is wrapped in a Win32 JobObject
//! ([`process_wrap::tokio::JobObject`]); on Unix in a process group
//! ([`process_wrap::tokio::ProcessGroup`]). The crate's
//! [`ChildWrapper::wait`] reaps the whole job/group, and
//! [`ChildWrapper::start_kill`] terminates the whole tree. We additionally apply
//! [`process_wrap::tokio::KillOnDrop`] so that even a dropped child (panic /
//! early return) tears the tree down via `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
//!
//! [`Stdio::from(File)`]: std::process::Stdio
//! [`ChildWrapper::wait`]: process_wrap::tokio::ChildWrapper::wait
//! [`ChildWrapper::start_kill`]: process_wrap::tokio::ChildWrapper::start_kill

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use process_wrap::tokio::{ChildWrapper, CommandWrap};
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject, KillOnDrop};
#[cfg(unix)]
use process_wrap::tokio::{KillOnDrop, ProcessGroup};

/// Creation flag: don't pop a console window for the build subprocess.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How often the SSE file-tail task re-reads the stderr log for newly-appended
/// bytes. Small enough that cargo progress feels live, large enough to keep the
/// poll cost negligible against a multi-minute build.
const TAIL_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Which budget ended a [`GuardedOutcome::TimedOut`] run.
///
/// The distinction is load-bearing for build call sites: a `NoProgress` kill
/// says the build produced nothing for the whole idle window (genuinely
/// wedged), while `Absolute` says it was still churning when the backstop
/// fired. Only the former is even a candidate for "the slot state is bad";
/// neither is evidence of a poisoned incremental cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// The process produced no new output and no new artifact for `idle`.
    NoProgress { idle: Duration },
    /// The absolute wall-clock backstop elapsed. The process may well have
    /// been making progress right up to the kill.
    Absolute,
}

/// Outcome of a [`GuardedCommand::run`].
#[derive(Debug)]
pub enum GuardedOutcome {
    /// The process exited on its own (success or non-zero). Carries the full
    /// captured `Output` (status + stdout + stderr bytes, read back from the
    /// redirect files).
    Exited(Output),
    /// A timeout budget elapsed; the process tree was killed. `after` is the
    /// total elapsed wall-clock, `kind` says WHICH budget fired, and `partial`
    /// is whatever stdout/stderr was captured (flushed to the files) before
    /// the kill.
    TimedOut {
        after: Duration,
        kind: TimeoutKind,
        partial: PartialOutput,
    },
    /// The cancellation token fired; the process tree was killed. `partial` is
    /// whatever was captured before the kill, and the build call sites now
    /// persist + render it the same way they do a timeout's — a cancelled
    /// build that leaves no trace is indistinguishable from one that never
    /// started.
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
    /// Captured stderr before the kill. This is where a killed build's cause
    /// lives (an OOM'd rustc, a wedged linker), so `run_build_inner`'s
    /// timeout/cancel arms read it as the fallback when the live line consumer
    /// yielded nothing — see `build_monitor::recover_partial_stderr`. Dropping
    /// it on the floor is what left a timed-out build with no persisted stderr
    /// at all (2026-07-31).
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
    /// Whether to wrap the spawned child in a tree-kill job (Windows JobObject)
    /// / process group (Unix) so a timeout/cancel tears down the WHOLE
    /// descendant tree. Defaults to `true` — the correct posture for long-lived
    /// build subprocesses (cargo/pnpm spawn rustc/linker/vite grandchildren).
    /// Set to `false` for short leaf subprocesses (git rev-parse) that never
    /// fork a pipe-holding grandchild: the wall-clock timeout + direct-child
    /// kill is sufficient and we avoid creating a JobObject per invocation.
    job_guarded: bool,
    /// When set, every stderr line is forwarded to this broadcast sender as it
    /// is appended to the stderr log (replaces the manual per-slot SSE stderr
    /// task in `build_monitor`). Lines are sent untagged; SSE framing is the
    /// consumer's job. Send errors (no subscribers) are ignored.
    stream_lines: Option<broadcast::Sender<String>>,
    /// When set, the run is governed by a NO-PROGRESS watchdog: the kill fires
    /// only after this long with no observed progress, and `timeout` degrades
    /// to an absolute backstop. When `None`, `timeout` is a plain wall-clock
    /// cap (the historical behaviour, kept for short leaf subprocesses).
    no_progress_timeout: Option<Duration>,
    /// Extra progress signal consulted by the no-progress watchdog, on top of
    /// "the process wrote more stderr". Returns an opaque token; any CHANGE in
    /// the token counts as progress. For a cargo build this is the newest
    /// artifact mtime under the slot's `CARGO_TARGET_DIR`, which is what keeps
    /// a long single-crate `rustc` (minutes of total silence, but writing
    /// artifacts) from being read as wedged.
    progress_probe: Option<ProgressProbe>,
}

/// Opaque progress token source — see [`GuardedCommand::progress_probe`].
pub type ProgressProbe = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

/// Poll cadence of the no-progress watchdog, derived from the budget so a
/// 2-second budget in a test is still responsive while a 20-minute budget in
/// production costs a handful of `stat`s per half-minute.
fn progress_poll_interval(budget: Duration) -> Duration {
    (budget / 8).clamp(Duration::from_millis(250), Duration::from_secs(30))
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
            job_guarded: true,
            stream_lines: None,
            no_progress_timeout: None,
            progress_probe: None,
        }
    }

    /// Govern this run with a NO-PROGRESS watchdog of `budget`: the tree is
    /// killed only after `budget` elapses with no new stderr bytes and no
    /// change in the [`progress_probe`](Self::progress_probe) token. The
    /// `timeout` passed to [`new`](Self::new) becomes an absolute backstop.
    pub fn no_progress_timeout(mut self, budget: Duration) -> Self {
        self.no_progress_timeout = Some(budget);
        self
    }

    /// Attach an extra progress signal for the no-progress watchdog (e.g. the
    /// newest artifact mtime under a cargo target dir). Any change in the
    /// returned token resets the idle window.
    pub fn progress_probe(mut self, probe: ProgressProbe) -> Self {
        self.progress_probe = Some(probe);
        self
    }

    /// Control whether the spawned child is wrapped in a tree-kill job /
    /// process group (default `true`). Pass `false` for short leaf subprocesses
    /// (e.g. `git rev-parse`) that never spawn a pipe-holding grandchild.
    pub fn job_guarded(mut self, guarded: bool) -> Self {
        self.job_guarded = guarded;
        self
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
    ///
    /// Part of the complete builder surface and exercised by the cancellation
    /// regression test; no build call site wires a cancel token yet (builds run
    /// to completion or the wall-clock timeout), hence the allow.
    #[allow(dead_code)]
    pub fn cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Stream stderr lines to `tx` as they're appended to the stderr log. Used
    /// by the build pool's per-slot `/builds/{slot}/log/stream` SSE fanout.
    pub fn stream_lines(mut self, tx: broadcast::Sender<String>) -> Self {
        self.stream_lines = Some(tx);
        self
    }

    /// Run the command to completion, a timeout, or a cancellation.
    ///
    /// The child's stdout/stderr are redirected to two on-disk log files (never
    /// pipes), so reading them back can't starve the runtime. On the
    /// timeout/cancel arms the process tree is killed via `process-wrap`
    /// (Windows JobObject / Unix process group) and the captured-so-far bytes
    /// ride along in the `partial` field.
    pub async fn run(self) -> Result<GuardedOutcome, std::io::Error> {
        let flavor = format!("{:?}", tokio::runtime::Handle::current().runtime_flavor());
        info!(
            "GCMD: run begin program={:?} args={:?} cwd={:?} timeout={}s job_guarded={} runtime_flavor={} thread={:?}",
            self.program,
            self.args,
            self.cwd,
            self.timeout.as_secs(),
            self.job_guarded,
            flavor,
            std::thread::current().id(),
        );
        let result = self.run_inner().await;
        match &result {
            Ok(outcome) => info!("GCMD: run return outcome={}", outcome_label(outcome)),
            Err(e) => info!("GCMD: run return err={}", e),
        }
        result
    }

    /// Build the base `tokio::process::Command`: program, args, cwd, env, and
    /// stdio redirected to the supplied log files (stdin null). The two `File`s
    /// are consumed into `Stdio` so the child writes directly to disk — no pipe
    /// is ever created, which is the load-bearing property of this module.
    fn configure_command(
        &self,
        cmd: &mut tokio::process::Command,
        stdout_file: std::fs::File,
        stderr_file: std::fs::File,
    ) {
        cmd.args(&self.args)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .stdin(Stdio::null());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
    }

    async fn run_inner(self) -> Result<GuardedOutcome, std::io::Error> {
        // Deterministic per-invocation log paths under the temp dir. A uuid
        // keeps two concurrent guarded commands from colliding; the pid prefix
        // makes the files greppable while a build is live.
        let stem = format!(
            "qontinui-guarded-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        );
        let dir = std::env::temp_dir();
        let stdout_path = dir.join(format!("{stem}.out.log"));
        let stderr_path = dir.join(format!("{stem}.err.log"));

        // Create both files up-front (truncating) so the child's `Stdio::from`
        // has a valid write target and the tail task has something to open.
        let stdout_file = std::fs::File::create(&stdout_path)?;
        let stderr_file = std::fs::File::create(&stderr_path)?;
        info!(
            "GCMD: redirect stdout={:?} stderr={:?}",
            stdout_path, stderr_path
        );

        // Best-effort cleanup of both files on every return path.
        let _cleanup = FileCleanup {
            paths: vec![stdout_path.clone(), stderr_path.clone()],
        };

        self.drive(stdout_file, stderr_file, &stdout_path, &stderr_path)
            .await
    }

    /// Spawn the (optionally tree-wrapped) child and `select!` over wait /
    /// timeout / cancel. On exit/kill, read both redirect files back into
    /// memory for the returned `Output` / `PartialOutput`.
    async fn drive(
        self,
        stdout_file: std::fs::File,
        stderr_file: std::fs::File,
        stdout_path: &Path,
        stderr_path: &Path,
    ) -> Result<GuardedOutcome, std::io::Error> {
        let mut child = self.spawn_child(stdout_file, stderr_file)?;
        let pid = child.id();
        info!("GCMD: spawned pid={:?} program={:?}", pid, self.program);

        // SSE live-tail: forward newly-appended stderr lines to subscribers
        // until the process exits. Tailing a FILE never blocks on pipe EOF, so
        // it cannot starve the runtime (the wedge this module exists to kill).
        let tail_stop = CancellationToken::new();
        let tail_handle = self.stream_lines.clone().map(|tx| {
            let stop = tail_stop.clone();
            let path = stderr_path.to_path_buf();
            tokio::spawn(async move { tail_stderr_to_broadcast(path, tx, stop).await })
        });

        let timeout = self.timeout;
        let cancel = self.cancel.clone();
        let select_start = std::time::Instant::now();
        let watchdog = TimeoutWatchdog {
            absolute: timeout,
            no_progress: self.no_progress_timeout,
            stderr_path: stderr_path.to_path_buf(),
            probe: self.progress_probe.clone(),
        };

        info!(
            "GCMD: select enter absolute_timeout={}s no_progress={:?}",
            timeout.as_secs(),
            self.no_progress_timeout.map(|d| d.as_secs())
        );
        let outcome = tokio::select! {
            wait_res = child.wait() => {
                let status = wait_res?;
                info!(
                    "GCMD: child exited status={} elapsed={}ms",
                    status,
                    select_start.elapsed().as_millis()
                );
                stop_tail(tail_stop, tail_handle).await;
                let (out, err) = read_back(stdout_path, stderr_path);
                info!(
                    "GCMD: drain→file-read out={}B err={}B",
                    out.len(),
                    err.len()
                );
                GuardedOutcome::Exited(make_output(status, out, err))
            }
            kind = watchdog.fire() => {
                let elapsed = select_start.elapsed();
                warn!(
                    "GCMD: TIMEOUT kind={:?} after {}s — killing tree",
                    kind,
                    elapsed.as_secs()
                );
                kill_tree(&mut child).await;
                stop_tail(tail_stop, tail_handle).await;
                let (stdout, stderr) = read_back(stdout_path, stderr_path);
                info!(
                    "GCMD: drain→file-read (timeout) out={}B err={}B",
                    stdout.len(),
                    stderr.len()
                );
                GuardedOutcome::TimedOut {
                    after: elapsed,
                    kind,
                    partial: PartialOutput { stdout, stderr },
                }
            }
            _ = wait_for_cancel(cancel) => {
                info!("GCMD: cancelled — killing tree");
                kill_tree(&mut child).await;
                stop_tail(tail_stop, tail_handle).await;
                let (stdout, stderr) = read_back(stdout_path, stderr_path);
                info!(
                    "GCMD: drain→file-read (cancel) out={}B err={}B",
                    stdout.len(),
                    stderr.len()
                );
                GuardedOutcome::Cancelled {
                    partial: PartialOutput { stdout, stderr },
                }
            }
        };

        Ok(outcome)
    }

    /// Spawn the child with the file-redirected stdio, wrapping it in a
    /// tree-kill job/group via `process-wrap` when `job_guarded`.
    ///
    /// On Windows the wrap order is `CreationFlags(CREATE_NO_WINDOW)` →
    /// `JobObject` → `KillOnDrop`: `CreationFlags` must precede `JobObject` so
    /// our `CREATE_NO_WINDOW` is OR'd into the flags `JobObject` sets
    /// (`CREATE_SUSPENDED`), per the crate's contract, instead of being
    /// clobbered. `KillOnDrop` opts the job into `KILL_ON_JOB_CLOSE` so a
    /// dropped child still tears down the tree.
    #[cfg(windows)]
    fn spawn_child(
        &self,
        stdout_file: std::fs::File,
        stderr_file: std::fs::File,
    ) -> Result<Box<dyn ChildWrapper>, std::io::Error> {
        use windows::Win32::System::Threading::PROCESS_CREATION_FLAGS;

        let mut wrap = CommandWrap::with_new(&self.program, |cmd| {
            self.configure_command(cmd, stdout_file, stderr_file);
        });

        if self.job_guarded {
            wrap.wrap(CreationFlags(PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW)))
                .wrap(JobObject)
                .wrap(KillOnDrop);
            info!("GCMD: wrapped child in JobObject (tree-kill) pid-pending");
        } else {
            // No job: a leaf subprocess (git) that never forks a pipe-holding
            // grandchild. Still suppress the console window.
            wrap.command_mut().creation_flags(CREATE_NO_WINDOW);
            info!("GCMD: spawning leaf child without JobObject (job_guarded=false)");
        }

        wrap.spawn()
    }

    /// Spawn the child with the file-redirected stdio, wrapping it in a POSIX
    /// process group via `process-wrap` when `job_guarded` so a timeout/cancel
    /// can kill the whole tree. The supervisor only runs on Windows in
    /// practice; this keeps the module compiling + behaving on Unix hosts.
    #[cfg(unix)]
    fn spawn_child(
        &self,
        stdout_file: std::fs::File,
        stderr_file: std::fs::File,
    ) -> Result<Box<dyn ChildWrapper>, std::io::Error> {
        let mut wrap = CommandWrap::with_new(&self.program, |cmd| {
            self.configure_command(cmd, stdout_file, stderr_file);
        });

        if self.job_guarded {
            wrap.wrap(ProcessGroup::leader()).wrap(KillOnDrop);
            info!("GCMD: wrapped child in process group (tree-kill)");
        } else {
            info!("GCMD: spawning leaf child without process group (job_guarded=false)");
        }

        wrap.spawn()
    }
}

/// The timeout arm of [`GuardedCommand::drive`]'s `select!`.
///
/// With `no_progress: None` this is exactly the historical behaviour — one
/// `sleep(absolute)`. With a no-progress budget it polls two progress signals
/// and only fires once BOTH have been static for the whole budget:
///
/// 1. the length of the stderr redirect file (cargo's own output), and
/// 2. the optional [`ProgressProbe`] token (for cargo: the newest artifact
///    mtime under the slot's target dir).
///
/// Signal 2 is the one that matters for the motivating incident: a single
/// long `rustc` on the runner's bin crate emits no cargo output for many
/// minutes while steadily writing artifacts, so a stderr-only watchdog would
/// still kill a perfectly healthy build.
struct TimeoutWatchdog {
    absolute: Duration,
    no_progress: Option<Duration>,
    stderr_path: PathBuf,
    probe: Option<ProgressProbe>,
}

impl TimeoutWatchdog {
    /// Current progress token: stderr bytes written so far, mixed with the
    /// optional external probe. Any change means the process did something.
    fn sample(&self) -> u64 {
        let stderr_len = std::fs::metadata(&self.stderr_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let probed = self.probe.as_ref().map(|p| p()).unwrap_or(0);
        stderr_len ^ probed.rotate_left(32)
    }

    /// Resolve when a budget is exhausted, reporting which one.
    async fn fire(&self) -> TimeoutKind {
        let Some(budget) = self.no_progress else {
            tokio::time::sleep(self.absolute).await;
            return TimeoutKind::Absolute;
        };

        let poll = progress_poll_interval(budget);
        let started = std::time::Instant::now();
        let mut last_token = self.sample();
        let mut last_progress = started;

        loop {
            tokio::time::sleep(poll).await;

            if started.elapsed() >= self.absolute {
                return TimeoutKind::Absolute;
            }

            let token = self.sample();
            if token != last_token {
                last_token = token;
                last_progress = std::time::Instant::now();
                continue;
            }

            let idle = last_progress.elapsed();
            if idle >= budget {
                return TimeoutKind::NoProgress { idle };
            }
        }
    }
}

/// Best-effort removal of the redirect log files when the guarded run returns.
struct FileCleanup {
    paths: Vec<PathBuf>,
}

impl Drop for FileCleanup {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Kill the whole process tree via the `process-wrap` child wrapper. On Windows
/// this terminates the JobObject (every descendant); on Unix the process group.
/// `start_kill` returns immediately; the subsequent `wait()` reaps the tree.
async fn kill_tree(child: &mut Box<dyn ChildWrapper>) {
    if let Err(e) = child.start_kill() {
        warn!("GCMD: start_kill (tree) failed: {e}");
    }
    // Reap so the OS releases the tree and the files are no longer being
    // written. Bounded so a pathological reaper can't hang the supervisor.
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!("GCMD: post-kill wait error: {e}"),
        Err(_) => warn!("GCMD: post-kill wait did not complete within 5s"),
    }
}

/// Future that resolves when `cancel` fires, or never if it's `None`.
async fn wait_for_cancel(cancel: Option<CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// Read both redirect files back into memory. Best-effort: a missing/unreadable
/// file yields empty bytes (the process may have died before writing anything).
fn read_back(stdout_path: &Path, stderr_path: &Path) -> (Vec<u8>, Vec<u8>) {
    let out = std::fs::read(stdout_path).unwrap_or_default();
    let err = std::fs::read(stderr_path).unwrap_or_default();
    (out, err)
}

/// Signal the SSE tail task to stop, then await it under a short bound so a
/// pathological final read can't hang the caller.
async fn stop_tail(stop: CancellationToken, handle: Option<tokio::task::JoinHandle<()>>) {
    stop.cancel();
    if let Some(handle) = handle {
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}

/// Tail `path` (the stderr redirect file) and forward each newly-appended
/// complete line to `tx`, until `stop` fires. After `stop`, do one final read
/// so the last lines flushed before process exit are not lost.
///
/// Reads only the bytes beyond the last offset each tick, buffers a partial
/// trailing line across ticks, and splits on `\n`. Because it reads a FILE it
/// never blocks on a pipe write-handle a detached grandchild is holding open —
/// the exact property that makes this immune to the runtime-starvation wedge.
async fn tail_stderr_to_broadcast(
    path: PathBuf,
    tx: broadcast::Sender<String>,
    stop: CancellationToken,
) {
    use std::io::{Read, Seek, SeekFrom};

    let mut offset: u64 = 0;
    let mut carry: Vec<u8> = Vec::new();

    // One pass: read new bytes from `offset` onward, emit complete lines,
    // retain a partial trailing line in `carry`. Returns the new offset.
    fn pump(path: &Path, offset: &mut u64, carry: &mut Vec<u8>, tx: &broadcast::Sender<String>) {
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        if file.seek(SeekFrom::Start(*offset)).is_err() {
            return;
        }
        let mut chunk = Vec::new();
        if file.read_to_end(&mut chunk).is_err() || chunk.is_empty() {
            return;
        }
        *offset += chunk.len() as u64;
        carry.extend_from_slice(&chunk);

        // Drain complete lines out of `carry`.
        while let Some(nl) = carry.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = carry.drain(..=nl).collect();
            // Trim the trailing \n (and a \r if present, for cmd.exe output).
            let mut line = String::from_utf8_lossy(&line_bytes).into_owned();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            // Err == no subscribers; the steady state. Drop silently.
            let _ = tx.send(line);
        }
    }

    loop {
        pump(&path, &mut offset, &mut carry, &tx);
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(TAIL_POLL_INTERVAL) => {}
        }
    }

    // Final read to catch anything flushed right before exit.
    pump(&path, &mut offset, &mut carry, &tx);
    // Emit any trailing partial line (no terminating newline) so the very last
    // cargo line isn't dropped.
    if !carry.is_empty() {
        let mut line = String::from_utf8_lossy(&carry).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        if !line.is_empty() {
            let _ = tx.send(line);
        }
    }
}

/// Short greppable label for a [`GuardedOutcome`] used in `GCMD:` tracing.
fn outcome_label(outcome: &GuardedOutcome) -> String {
    match outcome {
        GuardedOutcome::Exited(out) => format!("Exited(exit={})", out.status),
        GuardedOutcome::TimedOut { after, kind, .. } => {
            format!("TimedOut({}s, {:?})", after.as_secs(), kind)
        }
        GuardedOutcome::Cancelled { .. } => "Cancelled".to_string(),
    }
}

fn make_output(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> Output {
    Output {
        status,
        stdout,
        stderr,
    }
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

    /// Exit-code capture: a command that exits with a specific non-zero code
    /// must return `Exited` carrying that exact `status.code()`, with the output
    /// read back from the redirect file.
    #[tokio::test]
    async fn exit_code_is_captured_from_file() {
        #[cfg(windows)]
        let cmd = GuardedCommand::new("cmd", Duration::from_secs(30))
            .args(["/C", "echo marker-exit && exit /b 13"]);
        #[cfg(not(windows))]
        let cmd = GuardedCommand::new("sh", Duration::from_secs(30))
            .args(["-c", "echo marker-exit; exit 13"]);

        let outcome = cmd.run().await.expect("run() should not error");
        match outcome {
            GuardedOutcome::Exited(out) => {
                assert_eq!(
                    out.status.code(),
                    Some(13),
                    "exit code must be captured; got {:?}",
                    out.status.code()
                );
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.contains("marker-exit"),
                    "stdout must be read back from the redirect file; got {stdout:?}"
                );
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    /// Output capture: a command that writes known text to BOTH stdout and
    /// stderr must have each captured into the corresponding (separate) file and
    /// read back correctly — proving the streams are not merged.
    #[tokio::test]
    async fn stdout_and_stderr_captured_separately() {
        #[cfg(windows)]
        let cmd = GuardedCommand::new("cmd", Duration::from_secs(30))
            .args(["/C", "echo to-stdout-marker & echo to-stderr-marker 1>&2"]);
        #[cfg(not(windows))]
        let cmd = GuardedCommand::new("sh", Duration::from_secs(30))
            .args(["-c", "echo to-stdout-marker; echo to-stderr-marker 1>&2"]);

        let outcome = cmd.run().await.expect("run() should not error");
        match outcome {
            GuardedOutcome::Exited(out) => {
                assert!(out.status.success(), "should exit 0");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    stdout.contains("to-stdout-marker"),
                    "stdout file must hold the stdout marker; got {stdout:?}"
                );
                assert!(
                    !stdout.contains("to-stderr-marker"),
                    "stdout file must NOT contain stderr (streams must stay separate); got {stdout:?}"
                );
                assert!(
                    stderr.contains("to-stderr-marker"),
                    "stderr file must hold the stderr marker; got {stderr:?}"
                );
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    /// Large-output clean-exit: a `cmd /C` wrapper that emits a lot of stdout and
    /// exits 0 must return `Exited(success)` with the WHOLE output intact —
    /// catches the large-output / file-readback class. (`cmd /C` is the exact
    /// shape `run_pnpm_command` uses, and vite's real output is multi-MB.)
    #[cfg(windows)]
    #[tokio::test]
    async fn large_output_exits_with_full_capture() {
        let outcome = GuardedCommand::new("cmd", Duration::from_secs(60))
            .args(["/C", "for /L %i in (1,1,5000) do @echo qontinui-line-%i"])
            .run()
            .await
            .expect("run() should not error");

        match outcome {
            GuardedOutcome::Exited(out) => {
                assert!(out.status.success(), "should exit 0");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let count = stdout.matches("qontinui-line-").count();
                assert_eq!(count, 5000, "all 5000 lines must be captured; got {count}");
                assert!(stdout.contains("qontinui-line-1"));
                assert!(stdout.contains("qontinui-line-5000"));
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    /// HEADLINE regression: a `cmd /C` wrapper over a process that forks a
    /// detached grandchild which INHERITS the (now file-redirected) stdout and
    /// hangs 60s, while the direct child exits 0 immediately. Under the old
    /// pipe-based design the reader tasks never saw EOF and starved the runtime
    /// → permanent silent wedge. With FILE redirection, `child.wait()` resolves
    /// on the direct child's exit and we read the file back — no pipe to hold
    /// open, so it returns promptly with the parent's output, NOT after the
    /// grandchild's 60s sleep.
    #[cfg(windows)]
    #[tokio::test]
    async fn clean_exit_with_inheriting_grandchild_does_not_wedge() {
        // The DIRECT child (powershell) prints a marker, launches a grandchild
        // that inherits stdio and sleeps 60s, then exits 0 immediately.
        let script = "Write-Output 'qontinui-parent-marker'; \
             Start-Process powershell -NoNewWindow \
             -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 60'; \
             exit 0";

        let start = std::time::Instant::now();
        let outcome = GuardedCommand::new("powershell", Duration::from_secs(120))
            .args(["-NoProfile", "-Command", script])
            .run()
            .await
            .expect("run() must not error — and must NOT hang");
        let elapsed = start.elapsed();

        match &outcome {
            GuardedOutcome::Exited(out) => {
                assert!(
                    out.status.success(),
                    "direct child exited 0; status should be success, got {:?}",
                    out.status
                );
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.contains("qontinui-parent-marker"),
                    "parent's captured stdout must survive the file readback; got {stdout:?}"
                );
            }
            other => panic!("expected Exited (clean-exit arm), got {other:?}"),
        }

        // Headline: returns promptly on the direct child's exit, NOT after the
        // grandchild's 60s sleep and NOT via the 120s wall-clock timeout.
        assert!(
            elapsed < Duration::from_secs(20),
            "clean-exit-with-inheriting-grandchild must return on the direct child's exit, \
             not wait out the grandchild's 60s sleep; took {elapsed:?}"
        );
    }

    /// Tree-kill regression: a command that spawns a detached GRANDCHILD and
    /// then hangs must be killed (tree AND grandchild) by the timeout within ~the
    /// timeout window, NOT blocked until the grandchild's own 60s sleep elapses.
    /// The headline is the SECOND assertion: the grandchild PID is gone
    /// afterward, proving the kill was tree-wide (a naive direct-child kill would
    /// leave the detached grandchild alive).
    #[cfg(windows)]
    #[tokio::test]
    async fn timeout_kills_grandchild_tree() {
        let dir = std::env::temp_dir();
        let marker = dir.join(format!(
            "qontinui-guarded-grandchild-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_str = marker.to_string_lossy().to_string();

        // Direct child (powershell) launches a separate GRANDCHILD powershell
        // via Start-Process -PassThru, records its PID to a temp file, then
        // sleeps 60s. Under a 2s timeout the WHOLE tree must be killed by the
        // JobObject; we then read the grandchild PID and assert it's dead.
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
            elapsed < Duration::from_secs(10),
            "timeout+tree-kill should complete well under the 60s grandchild sleep; took {elapsed:?}"
        );

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
        for _ in 0..30 {
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

    /// A `cmd /C "<hang>"` wrapper — the EXACT shape `run_pnpm_command` uses on
    /// Windows (`cmd /C pnpm.cmd run build`) — must hit the wall-clock timeout
    /// and return `TimedOut` within the timeout window, and the wrapped child
    /// must be dead afterward.
    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_wrapper_hang_times_out() {
        let dir = std::env::temp_dir();
        let marker = dir.join(format!(
            "qontinui-guarded-cmdwrap-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let marker_str = marker.to_string_lossy().to_string();

        // Touch a marker (proving the wrapped child ran), then hang on a 60s
        // ping. A 4s timeout gives the echo ample time to land the marker.
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
            "timeout must fire ~4s in, well under the 60s ping; took {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(2),
            "should have actually waited for the ~4s timeout; took {elapsed:?}"
        );
        assert!(
            marker.exists(),
            "wrapped cmd child never ran (marker {marker:?} absent) — \
             the test did not exercise a live hang"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// A spawn failure (nonexistent program) must surface as a terminal `Err`
    /// from `run()`, never a silent wedge.
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
            "cancel should fire promptly, well under the 60s sleep/timeout; took {elapsed:?}"
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

    /// HEADLINE regression (2026-08-03): a process that keeps PRODUCING must
    /// not be killed just because it outlived the budget. It emits a stderr
    /// line every ~400ms for ~5s under a 2s no-progress budget — under the old
    /// wall-clock semantics a 2s timeout killed it at 2s; with the watchdog it
    /// runs to completion, because the idle window keeps resetting.
    ///
    /// This is the shape of the incident: a cargo build killed at 2650 of
    /// ~2800 compile units with rustc actively working, on a box loaded by 6-7
    /// peer builds.
    #[tokio::test]
    async fn no_progress_watchdog_spares_a_progressing_process() {
        // One long-lived interpreter emitting every 500ms, rather than a
        // `cmd` chain shelling out to `ping` for its sleeps: on a box under
        // real build load a single `ping.exe` spawn measured >2s, which the
        // watchdog correctly read as no progress. The budget below is sized
        // to absorb interpreter startup on such a box while staying far under
        // the ~10s the process actually runs.
        #[cfg(windows)]
        let cmd = GuardedCommand::new("powershell", Duration::from_secs(120)).args([
            "-NoProfile",
            "-Command",
            "1..20 | ForEach-Object { [Console]::Error.WriteLine('tick'); \
             Start-Sleep -Milliseconds 500 }",
        ]);
        #[cfg(not(windows))]
        let cmd = GuardedCommand::new("sh", Duration::from_secs(60)).args([
            "-c",
            "i=0; while [ $i -lt 12 ]; do echo tick 1>&2; sleep 0.4; i=$((i+1)); done",
        ]);

        let start = std::time::Instant::now();
        let outcome = cmd
            .no_progress_timeout(Duration::from_secs(6))
            .run()
            .await
            .expect("run() should not error");

        match &outcome {
            GuardedOutcome::Exited(out) => assert!(
                out.status.success(),
                "progressing process should exit cleanly, got {:?}",
                out.status
            ),
            other => panic!(
                "a progressing process must NOT be killed by the no-progress watchdog; got {other:?}"
            ),
        }
        assert!(
            start.elapsed() >= Duration::from_secs(7),
            "test did not actually outlive the 6s budget, so it proved nothing; elapsed {:?}",
            start.elapsed()
        );
    }

    /// The other half: a genuinely SILENT process (no output, no probe signal)
    /// is killed once the idle window is exhausted, and the outcome names the
    /// no-progress reason rather than a bare wall-clock timeout.
    #[cfg(windows)]
    #[tokio::test]
    async fn no_progress_watchdog_kills_a_silent_process() {
        let start = std::time::Instant::now();
        let outcome = GuardedCommand::new("cmd", Duration::from_secs(120))
            .args(["/C", "ping -n 60 127.0.0.1 >nul"])
            .no_progress_timeout(Duration::from_secs(2))
            .run()
            .await
            .expect("run() should not error");

        match outcome {
            GuardedOutcome::TimedOut {
                kind: TimeoutKind::NoProgress { idle },
                ..
            } => assert!(
                idle >= Duration::from_secs(2),
                "reported idle window must be at least the budget; got {idle:?}"
            ),
            other => panic!("expected TimedOut(NoProgress), got {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the no-progress kill must fire on the 2s budget, not the 120s backstop; took {:?}",
            start.elapsed()
        );
    }

    /// A silent process whose EXTERNAL probe keeps changing (the cargo case: a
    /// single long rustc emitting nothing while writing artifacts) is never
    /// killed by the no-progress arm — only the absolute backstop ends it, and
    /// the outcome says so.
    #[cfg(windows)]
    #[tokio::test]
    async fn progress_probe_alone_keeps_a_silent_process_alive() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let ticks = std::sync::Arc::new(AtomicU64::new(0));
        let probe_ticks = ticks.clone();

        let outcome = GuardedCommand::new("cmd", Duration::from_secs(5))
            .args(["/C", "ping -n 60 127.0.0.1 >nul"])
            .no_progress_timeout(Duration::from_secs(1))
            .progress_probe(std::sync::Arc::new(move || {
                probe_ticks.fetch_add(1, Ordering::Relaxed) + 1
            }))
            .run()
            .await
            .expect("run() should not error");

        assert!(
            matches!(
                outcome,
                GuardedOutcome::TimedOut {
                    kind: TimeoutKind::Absolute,
                    ..
                }
            ),
            "a probe that keeps moving must suppress the no-progress kill, leaving only the \
             absolute backstop; got {outcome:?}"
        );
        assert!(
            ticks.load(Ordering::Relaxed) > 0,
            "the probe was never called — the test proved nothing"
        );
    }

    /// The poll cadence must stay responsive for small budgets and cheap for
    /// production-sized ones.
    #[test]
    fn progress_poll_interval_is_bounded() {
        assert_eq!(
            progress_poll_interval(Duration::from_millis(100)),
            Duration::from_millis(250)
        );
        assert_eq!(
            progress_poll_interval(Duration::from_secs(8)),
            Duration::from_secs(1)
        );
        // 20-minute production budget → capped at the 30s ceiling.
        assert_eq!(
            progress_poll_interval(Duration::from_secs(1200)),
            Duration::from_secs(30)
        );
    }

    /// SSE live-tail: stderr lines must be forwarded to the broadcast channel as
    /// the child writes them, then the channel closes (sender dropped) when the
    /// command returns. Proves the file-tail replacement for the old reader-task
    /// fanout works.
    #[tokio::test]
    async fn stream_lines_forwards_stderr() {
        let (tx, mut rx) = broadcast::channel::<String>(256);

        #[cfg(windows)]
        let cmd = GuardedCommand::new("cmd", Duration::from_secs(30))
            .args(["/C", "echo stream-line-a 1>&2 & echo stream-line-b 1>&2"]);
        #[cfg(not(windows))]
        let cmd = GuardedCommand::new("sh", Duration::from_secs(30))
            .args(["-c", "echo stream-line-a 1>&2; echo stream-line-b 1>&2"]);

        let cmd = cmd.stream_lines(tx);
        let outcome = cmd.run().await.expect("run() should not error");
        assert!(
            matches!(outcome, GuardedOutcome::Exited(_)),
            "expected Exited, got {outcome:?}"
        );

        // Collect every line the tail task broadcast before the sender dropped.
        let mut got = Vec::new();
        while let Ok(line) = rx.try_recv() {
            got.push(line);
        }
        let joined = got.join("\n");
        assert!(
            joined.contains("stream-line-a") && joined.contains("stream-line-b"),
            "both stderr lines must be streamed; got {got:?}"
        );
    }
}
