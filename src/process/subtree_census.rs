//! Supervisor-side census of live `claude` processes under a runner PID.
//!
//! # Why this exists
//!
//! The supervisor's restart-readiness gate (`crate::restart_readiness`) asks
//! the runner `GET http://127.0.0.1:<port>/restart-readiness` and refuses on
//! any answer it cannot parse — correctly fail-closed. But the wedge class the
//! serving watchdog exists for (plan
//! `2026-09-03-runner-zombie-serving-watchdog`) is precisely *the runner's
//! HTTP door has stopped answering while the process stays alive*. In that
//! state the only readiness source the gate has is served by the door that is
//! wedged, so every verdict is `Unknown` and every restart is refused —
//! measured three times in nine days, 12 h to 5 days each, zero recoveries.
//!
//! This module is the readiness source that **survives** the wedged door. It
//! reads the OS process table (via `sysinfo`, already a dependency and already
//! used for enumeration in [`crate::process::proc_kill`]) and walks the
//! runner PID's **inclusive** process subtree, counting every process whose
//! image is `claude`. The runner's own verdict counts two session planes —
//! terminal-hosted sessions and AI/task-run sessions — but both materialise as
//! a `claude` image somewhere under the runner PID (`runner → shell → claude`,
//! `runner → claude`, or `runner → cmd.exe → claude`), so one subtree walk
//! covers both planes with a single count.
//!
//! # What it mirrors
//!
//! The name predicate and the walk are a deliberate mirror of the runner's own
//! `qontinui-runner/src-tauri/src/process_capture/process_tree.rs`:
//! `is_claude_image` (lowercase basename, strip only a trailing `.exe`, must
//! equal `claude`) and `claude_pids_in_inclusive_subtree` (BFS from the root,
//! root included). The PID-reuse guard mirrors that file's
//! `PID_REUSE_SKEW_MS = 5_000` as [`PID_REUSE_SKEW_SECS`].
//!
//! # The over-count boundary
//!
//! The runner additionally excludes its own `claude.exe` **shim** by exe
//! directory (`is_countable_claude`). This census does NOT replicate that
//! exclusion: an over-count only ever makes the watchdog **refuse** a restart,
//! which is the safe direction, and a refusal carries the counted PIDs and exe
//! paths so an operator can see that a shim was counted. The one failure mode
//! this module must never have is *under*-counting — reporting `Idle` while a
//! session is live — which is why an absent root, a reused root PID and an
//! unreadable table are all errors rather than empty censuses. A census with
//! no root is never `Idle`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;

use serde_json::{json, Value};

/// Skew tolerance (seconds) for the PID-reuse guard in [`take_census`].
///
/// Mirrors the runner's `PID_REUSE_SKEW_MS = 5_000`
/// (`process_tree.rs`): process start times are second-granular on every
/// platform sysinfo supports, and the reference instant (the runner's
/// `started_at`, stamped at spawn or at adoption) is taken by a different
/// clock read, so a beat of slack is always allowed before a root PID is
/// declared reused.
pub const PID_REUSE_SKEW_SECS: u64 = 5;

/// One row of a process-table snapshot, in the shape the pure layer walks.
///
/// Built from `sysinfo::Process` by [`take_census`]; hand-built by tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    /// Parent PID, `None` when the platform could not resolve one (the init
    /// process, or a permission-denied `/proc` entry).
    pub ppid: Option<u32>,
    /// The process's image name as the platform reports it (`comm` on Linux,
    /// the image file name on Windows). Used only when `exe` is unresolved.
    pub name: String,
    /// Full executable path when resolvable. Preferred over `name` for the
    /// predicate because `comm` is truncated to 15 bytes on Linux.
    pub exe: Option<PathBuf>,
    /// Process start time as Unix epoch seconds; `0` when unknown.
    pub start_time_secs: u64,
}

/// A live `claude` process the census counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusEntry {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
}

/// Where the census got its root PID from — provenance the payload carries so
/// a log line says whether the supervisor trusted its registry or had to ask
/// the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// The caller passed the PID from `RunnerState.pid`.
    Registry,
    /// The registry had no PID (possible on Linux, where the health cache's
    /// PID recovery is Windows-only) and the root was resolved through
    /// [`crate::process::proc_kill::find_pid_on_port`].
    PortListener,
}

impl RootSource {
    /// Stable wire spelling for [`ClaudeCensus::to_json`].
    pub fn as_str(self) -> &'static str {
        match self {
            RootSource::Registry => "registry",
            RootSource::PortListener => "port_listener",
        }
    }
}

/// Whether the PID-reuse guard actually ran for this census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidReuseGuard {
    /// A reference instant was supplied and the root's start time was
    /// compared against it (and passed — a failure is [`CensusError::RootReused`]).
    Checked,
    /// The caller had no reference instant: a registry row with neither a
    /// spawn nor an adoption record. (An ADOPTED runner does carry one — the
    /// orphan scan stamps `started_at` at adoption — so this is not the
    /// adopted case.)
    SkippedNoReference,
    /// A reference was supplied but the platform reported `0` for the root's
    /// start time, so there was nothing to compare. Fail-open like the
    /// runner's guard (`created_secs > 0 &&`), reported honestly.
    SkippedUnknownStartTime,
}

impl PidReuseGuard {
    /// Stable wire spelling for [`ClaudeCensus::to_json`].
    pub fn as_str(self) -> &'static str {
        match self {
            PidReuseGuard::Checked => "checked",
            PidReuseGuard::SkippedNoReference => "skipped_no_reference",
            PidReuseGuard::SkippedUnknownStartTime => "skipped_unknown_start_time",
        }
    }
}

/// The result of one subtree walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCensus {
    pub root_pid: u32,
    pub root_source: RootSource,
    /// Processes visited by the inclusive walk (root + every descendant).
    pub walked: usize,
    /// Every `claude`-image process in the subtree, sorted by PID.
    pub live: Vec<CensusEntry>,
    pub pid_reuse_guard: PidReuseGuard,
}

/// The census's answer to "is it safe to restart this runner?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CensusVerdict {
    /// Zero `claude` processes under the root: nothing to destroy.
    Idle,
    /// At least one live `claude`; `pids` is sorted ascending.
    Busy { count: usize, pids: Vec<u32> },
}

impl CensusVerdict {
    /// Stable wire spelling for [`ClaudeCensus::to_json`].
    pub fn as_str(&self) -> &'static str {
        match self {
            CensusVerdict::Idle => "idle",
            CensusVerdict::Busy { .. } => "busy",
        }
    }
}

impl ClaudeCensus {
    /// `Idle` iff no `claude` image was counted anywhere in the subtree.
    pub fn verdict(&self) -> CensusVerdict {
        if self.live.is_empty() {
            CensusVerdict::Idle
        } else {
            CensusVerdict::Busy {
                count: self.live.len(),
                pids: self.live.iter().map(|e| e.pid).collect(),
            }
        }
    }

    /// The stable payload shape, in the same field spirit as the runner's
    /// `ReadinessReport` so a refusal from either arm reads alike in the log:
    ///
    /// ```json
    /// {"source":"supervisor_subtree_census","root_pid":…,
    ///  "root_source":"registry|port_listener","walked":…,
    ///  "live_claude":[{"pid":…,"name":…,"exe":…}],
    ///  "verdict":"idle|busy",
    ///  "pid_reuse_guard":"checked|skipped_no_reference|skipped_unknown_start_time"}
    /// ```
    pub fn to_json(&self) -> Value {
        json!({
            "source": "supervisor_subtree_census",
            "root_pid": self.root_pid,
            "root_source": self.root_source.as_str(),
            "walked": self.walked,
            "live_claude": self
                .live
                .iter()
                .map(|e| {
                    json!({
                        "pid": e.pid,
                        "name": e.name,
                        "exe": e.exe.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    })
                })
                .collect::<Vec<_>>(),
            "verdict": self.verdict().as_str(),
            "pid_reuse_guard": self.pid_reuse_guard.as_str(),
        })
    }
}

/// Why a census could not be taken. Every variant is UNKNOWN to the readiness
/// gate — none of them is evidence that the runner is idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CensusError {
    /// The root PID is not in the process table (or, for the port path, no
    /// process is listening on the port). Positive evidence that the process
    /// is GONE — the caller treats it as *stopped*, never as *idle*.
    RootAbsent { root_pid: Option<u32> },
    /// The root PID exists but its start time postdates the caller's
    /// reference instant by more than [`PID_REUSE_SKEW_SECS`]: the PID has been
    /// recycled onto a process that is not the runner the registry recorded.
    RootReused {
        root_pid: u32,
        start_time_secs: u64,
        reference_unix_secs: i64,
    },
    /// The process table could not be enumerated (or the port probe could not
    /// run). The subtree's state is UNKNOWN.
    Unreadable(String),
}

impl CensusError {
    /// Stable machine-readable code for the readiness gate to surface.
    pub fn code(&self) -> &'static str {
        match self {
            CensusError::RootAbsent { .. } => "census_root_absent",
            CensusError::RootReused { .. } => "census_root_reused",
            CensusError::Unreadable(_) => "census_unreadable",
        }
    }
}

impl fmt::Display for CensusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CensusError::RootAbsent {
                root_pid: Some(pid),
            } => write!(f, "root pid {pid} is not in the process table"),
            CensusError::RootAbsent { root_pid: None } => {
                write!(f, "no process is listening on the runner's port")
            }
            CensusError::RootReused {
                root_pid,
                start_time_secs,
                reference_unix_secs,
            } => write!(
                f,
                "root pid {root_pid} started at unix {start_time_secs}s, more than \
                 {PID_REUSE_SKEW_SECS}s after the reference instant {reference_unix_secs}s — \
                 the pid has been reused by a different process"
            ),
            CensusError::Unreadable(detail) => {
                write!(f, "process table unreadable: {detail}")
            }
        }
    }
}

impl std::error::Error for CensusError {}

/// The runner's `is_claude_image` predicate, mirrored: lowercase basename of
/// the exe file name (falling back to `name` when no exe resolved), strip only
/// a trailing `.exe`, must equal `claude`.
///
/// `claude.cmd`, `claude-code-helper` and `xclaude` are deliberately NOT
/// matches — the runner's predicate rejects them too, and widening it here
/// would count launcher scripts as sessions.
fn is_claude_image(row: &ProcRow) -> bool {
    let candidate: String = match row.exe.as_ref().and_then(|p| p.file_name()) {
        Some(file) => file.to_string_lossy().into_owned(),
        None => row.name.clone(),
    };
    // `name` may itself be path-qualified on some platforms; take the basename.
    let base = candidate
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(candidate.as_str())
        .to_ascii_lowercase();
    let stem = base.strip_suffix(".exe").unwrap_or(&base);
    stem == "claude"
}

/// Pure layer: walk the inclusive subtree of `root_pid` over `procs` and
/// count every `claude` image in it.
///
/// BFS over a `ppid → [pid]` map, root INCLUDED (the runner itself is never
/// `claude`, but the runner's agent-launched PTY child can be `claude`
/// directly, and the walk is also used with the test process as root). A
/// visited set guards against a malformed table (a PID listed as its own
/// ancestor) looping forever. Children are visited in ascending PID order and
/// `live` is sorted by PID so the output is deterministic regardless of the
/// snapshot's map order.
///
/// Returns `Err(RootAbsent)` when `root_pid` is not among `procs` — a census
/// with no root must never be representable as `Idle`, which is why this
/// returns a `Result` rather than an empty census. When `reference_unix_secs`
/// is given, the PID-reuse guard runs on the root first (`Err(RootReused)`).
pub fn census_from_snapshot(
    root_pid: u32,
    procs: &[ProcRow],
    reference_unix_secs: Option<i64>,
    root_source: RootSource,
) -> Result<ClaudeCensus, CensusError> {
    let by_pid: HashMap<u32, &ProcRow> = procs.iter().map(|r| (r.pid, r)).collect();
    let Some(root) = by_pid.get(&root_pid) else {
        return Err(CensusError::RootAbsent {
            root_pid: Some(root_pid),
        });
    };

    let pid_reuse_guard = match reference_unix_secs {
        None => PidReuseGuard::SkippedNoReference,
        Some(_) if root.start_time_secs == 0 => PidReuseGuard::SkippedUnknownStartTime,
        Some(reference) => {
            let started = i64::try_from(root.start_time_secs).unwrap_or(i64::MAX);
            let skew = i64::try_from(PID_REUSE_SKEW_SECS).unwrap_or(i64::MAX);
            if started > reference.saturating_add(skew) {
                return Err(CensusError::RootReused {
                    root_pid,
                    start_time_secs: root.start_time_secs,
                    reference_unix_secs: reference,
                });
            }
            PidReuseGuard::Checked
        }
    };

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for row in procs {
        if let Some(ppid) = row.ppid {
            children.entry(ppid).or_default().push(row.pid);
        }
    }
    for kids in children.values_mut() {
        kids.sort_unstable();
        kids.dedup();
    }

    let mut queue: VecDeque<u32> = VecDeque::from([root_pid]);
    let mut visited: HashSet<u32> = HashSet::from([root_pid]);
    let mut walked = 0usize;
    let mut live: Vec<CensusEntry> = Vec::new();

    while let Some(pid) = queue.pop_front() {
        walked += 1;
        if let Some(row) = by_pid.get(&pid) {
            if is_claude_image(row) {
                live.push(CensusEntry {
                    pid: row.pid,
                    name: row.name.clone(),
                    exe: row.exe.clone(),
                });
            }
        }
        if let Some(kids) = children.get(&pid) {
            for &kid in kids {
                if visited.insert(kid) {
                    queue.push_back(kid);
                }
            }
        }
    }
    live.sort_by_key(|e| e.pid);

    Ok(ClaudeCensus {
        root_pid,
        root_source,
        walked,
        live,
        pid_reuse_guard,
    })
}

/// One `sysinfo` snapshot of the whole process table, in [`ProcRow`] form.
///
/// Same refresh idiom as [`crate::process::proc_kill::find_pids_holding_exe`]:
/// processes only, with exe paths always resolved. `parent()` and
/// `start_time()` are populated by the base process refresh on every
/// platform.
///
/// `Err(Unreadable)` when the table does not even contain the supervisor's
/// own PID — a readable table always does, so its absence is the one
/// enumeration failure `sysinfo` reports by omission rather than by error.
fn snapshot_rows() -> Result<Vec<ProcRow>, CensusError> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System, UpdateKind};
    let system = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_exe(UpdateKind::Always)),
    );
    let rows: Vec<ProcRow> = system
        .processes()
        .values()
        .map(|p| ProcRow {
            pid: p.pid().as_u32(),
            ppid: p.parent().map(|pp| pp.as_u32()),
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(|e| e.to_path_buf()),
            start_time_secs: p.start_time(),
        })
        .collect();
    let own = std::process::id();
    if !rows.iter().any(|r| r.pid == own) {
        return Err(CensusError::Unreadable(format!(
            "snapshot of {} processes does not contain the supervisor's own pid {own}",
            rows.len()
        )));
    }
    Ok(rows)
}

/// I/O layer: take one process-table snapshot and census the inclusive
/// subtree of `root_pid` (a PID the caller got from its registry).
///
/// - `Err(RootAbsent)` when `root_pid` is not in the table — the process is
///   gone (treat as *stopped*, not *idle*).
/// - `Err(RootReused)` when `reference_unix_secs` is given (the runner's
///   `started_at` — spawn or adoption instant) and the root's start time
///   postdates it by more than [`PID_REUSE_SKEW_SECS`].
/// - `Err(Unreadable)` when the table could not be enumerated.
///
/// Blocking (one `/proc` walk or one Windows process enumeration); async
/// callers go through [`take_census_for_port`], which runs it on the blocking
/// pool.
pub fn take_census(
    root_pid: u32,
    reference_unix_secs: Option<i64>,
) -> Result<ClaudeCensus, CensusError> {
    take_census_with_source(root_pid, reference_unix_secs, RootSource::Registry)
}

fn take_census_with_source(
    root_pid: u32,
    reference_unix_secs: Option<i64>,
    root_source: RootSource,
) -> Result<ClaudeCensus, CensusError> {
    let rows = snapshot_rows()?;
    census_from_snapshot(root_pid, &rows, reference_unix_secs, root_source)
}

/// [`take_census`] for a runner known by port, resolving a missing root PID
/// from the port's listener.
///
/// `root_pid: Some(pid)` (from `RunnerState.pid`) proceeds exactly like
/// [`take_census`] with `root_source: registry`. `None` — possible on Linux,
/// where the health cache's PID recovery is Windows-only — resolves the root
/// through the cross-platform
/// [`crate::process::proc_kill::find_pid_on_port`]: `Ok(Some(pid))` proceeds
/// with `root_source: port_listener`; `Ok(None)` (the probe RAN and the port
/// is idle) is `Err(RootAbsent)`; `Err(_)` (the probe could not run) is
/// `Err(Unreadable)`. A census with no root is never `Idle`.
pub async fn take_census_for_port(
    root_pid: Option<u32>,
    port: u16,
    reference_unix_secs: Option<i64>,
) -> Result<ClaudeCensus, CensusError> {
    let (root, source) = match root_pid {
        Some(pid) => (pid, RootSource::Registry),
        None => match crate::process::proc_kill::find_pid_on_port(port).await {
            Ok(Some(pid)) => (pid, RootSource::PortListener),
            Ok(None) => return Err(CensusError::RootAbsent { root_pid: None }),
            Err(e) => {
                return Err(CensusError::Unreadable(format!(
                    "port {port} listener probe could not run: {e:#}"
                )))
            }
        },
    };
    tokio::task::spawn_blocking(move || take_census_with_source(root, reference_unix_secs, source))
        .await
        .map_err(|e| CensusError::Unreadable(format!("census task did not complete: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn row(pid: u32, ppid: Option<u32>, name: &str, exe: Option<&str>, start: u64) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            name: name.to_string(),
            exe: exe.map(PathBuf::from),
            start_time_secs: start,
        }
    }

    /// The registry-root census of a live process with one non-`claude`
    /// child walks at least the root plus that child and reads `Idle`.
    #[test]
    fn census_of_own_process_with_a_sleep_child_is_idle() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();

        let census = take_census(std::process::id(), None).expect("census of own pid");

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(census.root_pid, std::process::id());
        assert_eq!(census.root_source, RootSource::Registry);
        assert!(
            census.walked >= 2,
            "expected the root and at least the sleep child {child_pid}, walked {}",
            census.walked
        );
        assert_eq!(census.verdict(), CensusVerdict::Idle);
        assert_eq!(census.pid_reuse_guard, PidReuseGuard::SkippedNoReference);
        assert_eq!(census.to_json()["verdict"], json!("idle"));
    }

    /// A live root with a reference instant at or after its own start time
    /// passes the guard and reports `checked`.
    #[test]
    fn census_of_own_process_with_a_current_reference_passes_the_reuse_guard() {
        let now = chrono::Utc::now().timestamp();
        let census = take_census(std::process::id(), Some(now)).expect("census of own pid");
        assert_eq!(census.pid_reuse_guard, PidReuseGuard::Checked);
    }

    /// A PID that no process holds is `RootAbsent`, never an empty (`Idle`)
    /// census.
    #[test]
    fn census_of_a_dead_pid_is_root_absent() {
        // A reaped child's PID is the surest "was a process, is not now".
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait true");
        match take_census(pid, None) {
            Err(CensusError::RootAbsent { root_pid }) => assert_eq!(root_pid, Some(pid)),
            // PID reuse inside the test window is astronomically unlikely but
            // legal; a reused PID is still not an `Idle` misreport.
            Ok(c) => assert_ne!(c.root_pid, 0),
            Err(other) => panic!("expected RootAbsent, got {other}"),
        }
    }

    /// (c) The port door with a registry root behaves like `take_census`.
    #[tokio::test]
    async fn take_census_for_port_with_a_registry_root_matches_take_census() {
        let direct = take_census(std::process::id(), None).expect("direct census");
        let via_port = take_census_for_port(Some(std::process::id()), 1, None)
            .await
            .expect("port-door census");
        assert_eq!(via_port.root_pid, direct.root_pid);
        assert_eq!(via_port.root_source, RootSource::Registry);
        assert_eq!(via_port.verdict(), CensusVerdict::Idle);
        assert_eq!(via_port.pid_reuse_guard, direct.pid_reuse_guard);
    }

    #[test]
    fn claude_image_predicate_matches_the_runner_spellings() {
        for (name, exe) in [
            ("claude", None),
            ("claude.exe", None),
            ("CLAUDE.EXE", None),
            ("claude", Some("/opt/x/claude")),
            (
                "node",
                Some("C:\\Users\\me\\AppData\\Local\\Programs\\claude\\Claude.EXE"),
            ),
        ] {
            assert!(
                is_claude_image(&row(1, None, name, exe, 0)),
                "{name:?} / {exe:?} must match"
            );
        }
        for (name, exe) in [
            ("claude.cmd", None),
            ("claude-code-helper", None),
            ("xclaude", None),
            ("claude", Some("/usr/bin/claude.cmd")),
            ("claudex", Some("/opt/claude/claudex")),
            ("", None),
        ] {
            assert!(
                !is_claude_image(&row(1, None, name, exe, 0)),
                "{name:?} / {exe:?} must NOT match"
            );
        }
    }

    /// `runner → cmd.exe → claude` is counted two levels down; a `claude`
    /// outside the subtree is not.
    #[test]
    fn counts_claude_descendants_and_ignores_processes_outside_the_subtree() {
        let rows = vec![
            row(1, None, "init", None, 1),
            row(
                100,
                Some(1),
                "qontinui-runner",
                Some("/opt/runner/qontinui-runner"),
                10,
            ),
            row(
                200,
                Some(100),
                "cmd.exe",
                Some("C:\\Windows\\System32\\cmd.exe"),
                11,
            ),
            row(
                300,
                Some(200),
                "claude.exe",
                Some("C:\\claude\\claude.exe"),
                12,
            ),
            row(400, Some(100), "bash", Some("/bin/bash"), 13),
            row(
                500,
                Some(400),
                "claude",
                Some("/home/u/.local/bin/claude"),
                14,
            ),
            // A claude under a DIFFERENT parent: not this runner's session.
            row(
                900,
                Some(1),
                "claude",
                Some("/home/u/.local/bin/claude"),
                15,
            ),
        ];
        let census =
            census_from_snapshot(100, &rows, None, RootSource::Registry).expect("root present");
        assert_eq!(census.walked, 5, "root, cmd.exe, bash, and two claudes");
        assert_eq!(
            census.verdict(),
            CensusVerdict::Busy {
                count: 2,
                pids: vec![300, 500]
            }
        );
        assert_eq!(
            census.live[0].exe.as_deref(),
            Some(std::path::Path::new("C:\\claude\\claude.exe"))
        );
    }

    /// The root itself counts when its image is `claude` (the agent-launched
    /// PTY child shape).
    #[test]
    fn root_is_inclusive() {
        let rows = vec![row(42, Some(1), "claude", Some("/opt/x/claude"), 5)];
        let census =
            census_from_snapshot(42, &rows, None, RootSource::Registry).expect("root present");
        assert_eq!(census.walked, 1);
        assert_eq!(
            census.verdict(),
            CensusVerdict::Busy {
                count: 1,
                pids: vec![42]
            }
        );
    }

    #[test]
    fn root_absent_for_a_pid_not_in_the_rows() {
        let rows = vec![
            row(1, None, "init", None, 1),
            row(2, Some(1), "claude", None, 2),
        ];
        assert_eq!(
            census_from_snapshot(7, &rows, None, RootSource::Registry),
            Err(CensusError::RootAbsent { root_pid: Some(7) })
        );
    }

    /// A malformed table (a PID that is its own ancestor) terminates.
    #[test]
    fn cyclic_parent_map_terminates() {
        let rows = vec![
            row(10, Some(20), "a", None, 1),
            row(20, Some(10), "claude", None, 1),
        ];
        let census =
            census_from_snapshot(10, &rows, None, RootSource::Registry).expect("root present");
        assert_eq!(census.walked, 2);
        assert_eq!(
            census.verdict(),
            CensusVerdict::Busy {
                count: 1,
                pids: vec![20]
            }
        );
    }

    #[test]
    fn pid_reuse_guard_fires_only_beyond_the_skew() {
        let reference: i64 = 1_000;
        let within = |start: u64| {
            census_from_snapshot(
                1,
                &[row(1, None, "qontinui-runner", None, start)],
                Some(reference),
                RootSource::Registry,
            )
        };
        // Started before, at, and inside the skew after the reference: fine.
        for start in [900u64, 1_000, 1_005] {
            let census = within(start).unwrap_or_else(|e| panic!("start {start}: {e}"));
            assert_eq!(
                census.pid_reuse_guard,
                PidReuseGuard::Checked,
                "start {start}"
            );
        }
        // One second past the skew: reused.
        assert_eq!(
            within(1_006),
            Err(CensusError::RootReused {
                root_pid: 1,
                start_time_secs: 1_006,
                reference_unix_secs: reference,
            })
        );
        // Unknown start time: the guard cannot run, and says so.
        assert_eq!(
            within(0).expect("unknown start").pid_reuse_guard,
            PidReuseGuard::SkippedUnknownStartTime
        );
        // No reference at all: skipped, even for a start far in the future.
        let census = census_from_snapshot(
            1,
            &[row(1, None, "qontinui-runner", None, 5_000)],
            None,
            RootSource::Registry,
        )
        .expect("no reference");
        assert_eq!(census.pid_reuse_guard, PidReuseGuard::SkippedNoReference);
    }

    /// The payload shape is a contract with the readiness gate's log line
    /// and the operator alert; pin it exactly.
    #[test]
    fn json_shape_is_stable() {
        let rows = vec![
            row(
                100,
                Some(1),
                "qontinui-runner",
                Some("/opt/runner/qontinui-runner"),
                10,
            ),
            row(
                300,
                Some(100),
                "claude",
                Some("/home/u/.local/bin/claude"),
                12,
            ),
            row(301, Some(100), "claude", None, 12),
        ];
        let busy = census_from_snapshot(100, &rows, Some(10), RootSource::PortListener)
            .expect("root present");
        assert_eq!(
            busy.to_json(),
            json!({
                "source": "supervisor_subtree_census",
                "root_pid": 100,
                "root_source": "port_listener",
                "walked": 3,
                "live_claude": [
                    {"pid": 300, "name": "claude", "exe": "/home/u/.local/bin/claude"},
                    {"pid": 301, "name": "claude", "exe": null},
                ],
                "verdict": "busy",
                "pid_reuse_guard": "checked",
            })
        );

        let idle = census_from_snapshot(100, &rows[..1], None, RootSource::Registry)
            .expect("root present");
        assert_eq!(
            idle.to_json(),
            json!({
                "source": "supervisor_subtree_census",
                "root_pid": 100,
                "root_source": "registry",
                "walked": 1,
                "live_claude": [],
                "verdict": "idle",
                "pid_reuse_guard": "skipped_no_reference",
            })
        );
    }

    #[test]
    fn error_codes_and_display_are_stable() {
        let absent = CensusError::RootAbsent { root_pid: Some(7) };
        let absent_port = CensusError::RootAbsent { root_pid: None };
        let reused = CensusError::RootReused {
            root_pid: 7,
            start_time_secs: 1_006,
            reference_unix_secs: 1_000,
        };
        let unreadable = CensusError::Unreadable("boom".into());
        assert_eq!(absent.code(), "census_root_absent");
        assert_eq!(absent_port.code(), "census_root_absent");
        assert_eq!(reused.code(), "census_root_reused");
        assert_eq!(unreadable.code(), "census_unreadable");
        assert_eq!(absent.to_string(), "root pid 7 is not in the process table");
        assert_eq!(
            absent_port.to_string(),
            "no process is listening on the runner's port"
        );
        assert!(reused.to_string().contains("pid has been reused"));
        assert_eq!(unreadable.to_string(), "process table unreadable: boom");
    }
}
