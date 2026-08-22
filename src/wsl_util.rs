//! Shared constructor for `wsl` subprocesses, and the non-waking distro
//! liveness gate that sits at that spawn boundary.
//!
//! Every `wsl` invocation in the supervisor goes through [`wsl_command`] so
//! that on Windows the transient console window is suppressed
//! (`CREATE_NO_WINDOW`). Without this flag each `wsl` spawn flashes a console
//! window on screen — and the CI-runner probe loop (`ci_runner_probe.rs`)
//! fires `wsl` calls every 30s, so the flashing is continuous.
//!
//! # The gate (plan `2026-08-21-supervisor-watchdog-observer-effect` §1)
//!
//! **`wsl -e <cmd>` STARTS the distro if it is down, and the exit of that
//! command re-arms WSL's poweroff timer.** A health probe built out of such
//! calls is therefore not an observer: it manufactures the liveness it
//! reports and destroys it ~60 s later. On MSI that produced 34 distro
//! poweroff/boot cycles in 2h20m, each one killing whatever CI job the
//! freshly-woken runner had just claimed.
//!
//! `wsl.exe --list --running --quiet` reads distro state **without starting
//! anything** (verified twice on MSI, most recently 2026-08-22: with
//! `docker-desktop` `Stopped`, it listed only `Ubuntu-24.04` and left
//! `docker-desktop` `Stopped`). [`wsl_command`] consults a cheaply-cached
//! reading of it and **refuses to construct the command at all** when the
//! distro `wsl -e` would target is not running, returning
//! [`WslUnavailable::DistroDown`].
//!
//! The gate lives here, at the single constructor, rather than at the call
//! sites **on purpose**. A per-call-site gate is reopened silently by the
//! next new caller, and it misses the two callers outside the probe module
//! that fire in exactly the distro-down path:
//! `ci_runner_lifecycle::is_runner_installed` and
//! `routes::ci_runner::resolve_runner_name`. Putting it at the constructor
//! makes the invariant structural instead of remembered.
//!
//! The gate's own `--list` calls go through [`wsl_command_ungated`], which is
//! private and used nowhere else.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a running-distro reading is reused before it is taken again.
///
/// The probe ticks every 30 s and now issues exactly one `wsl -e` per tick, so
/// in practice this means one non-waking `--list` pair per tick. It is short
/// enough that a distro that goes down mid-tick is noticed on the next one.
const SNAPSHOT_TTL: Duration = Duration::from_secs(5);

/// Env override naming the distro `wsl -e` targets. When set, the gate skips
/// resolving the default distro and asks only whether *this* one is running.
const DISTRO_ENV: &str = "QONTINUI_SUPERVISOR_WSL_DISTRO";

/// Why a `wsl` command could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WslUnavailable {
    /// The distro `wsl -e` would target is not running. Spawning would have
    /// started it — which is the observer effect this gate exists to prevent.
    DistroDown {
        /// The distro the gate asked about (`None` when the default distro
        /// could not be resolved and the gate fell back to "is anything
        /// running at all").
        distro: Option<String>,
        /// Distros that WERE running when the gate last looked.
        running: Vec<String>,
    },
    /// The gate itself could not be evaluated (no `wsl.exe`, unparseable
    /// output, …). This is UNKNOWN, not "down" and not "up" — callers map it
    /// to a probe failure rather than to a verdict about the runner.
    GateFailed(String),
}

impl std::fmt::Display for WslUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DistroDown { distro, running } => {
                let name = distro.as_deref().unwrap_or("<default>");
                if running.is_empty() {
                    write!(
                        f,
                        "WSL distro '{name}' is not running (no distro is running); \
                         refusing to spawn `wsl -e`, which would start it"
                    )
                } else {
                    write!(
                        f,
                        "WSL distro '{name}' is not running (running: {}); \
                         refusing to spawn `wsl -e`, which would start it",
                        running.join(", ")
                    )
                }
            }
            Self::GateFailed(msg) => {
                write!(f, "WSL running-distro gate could not be evaluated: {msg}")
            }
        }
    }
}

impl std::error::Error for WslUnavailable {}

/// Number of gated (`wsl -e`-capable) commands handed out by [`wsl_command`].
///
/// This is the spawn counter the plan's Verification §3 asks for: it counts
/// every distro-waking command the crate constructs, from any module.
static GATED_SPAWNS: AtomicU64 = AtomicU64::new(0);

/// Number of non-waking `wsl --list` invocations the gate itself has made.
static GATE_SPAWNS: AtomicU64 = AtomicU64::new(0);

/// Count of gated `wsl` commands constructed so far (process-wide).
pub fn gated_spawn_count() -> u64 {
    GATED_SPAWNS.load(Ordering::Relaxed)
}

/// Count of the gate's own non-waking `wsl --list` invocations.
pub fn gate_spawn_count() -> u64 {
    GATE_SPAWNS.load(Ordering::Relaxed)
}

/// A cached reading of which distros are running.
#[derive(Debug, Clone)]
struct DistroSnapshot {
    taken_at: Instant,
    /// The distro `wsl -e` targets, when it could be resolved.
    target: Option<String>,
    /// Distros reported running by `wsl --list --running --quiet`.
    running: Vec<String>,
}

static SNAPSHOT: Mutex<Option<DistroSnapshot>> = Mutex::new(None);

/// Test-only override of the gate verdict, so unit tests never touch a real
/// `wsl.exe`. `None` = consult the real gate.
#[cfg(test)]
static TEST_GATE: Mutex<Option<Result<(), WslUnavailable>>> = Mutex::new(None);

/// Serializes tests that install a [`TEST_GATE`] override.
#[cfg(test)]
static TEST_GATE_LOCK: Mutex<()> = Mutex::new(());

/// Serialize a test against every other test that touches the gate override or
/// the spawn counters. `GATED_SPAWNS` is process-wide, so a test asserting a
/// *delta* on it must hold this or a peer test's increment lands inside the
/// window and the assertion flakes.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Install a gate verdict for the duration of a test. Returns a guard that
/// serializes against other tests doing the same; drop it to clear.
#[cfg(test)]
pub fn test_gate_override(verdict: Result<(), WslUnavailable>) -> TestGateGuard {
    let guard = test_lock();
    *TEST_GATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(verdict);
    TestGateGuard { _guard: guard }
}

#[cfg(test)]
pub struct TestGateGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestGateGuard {
    fn drop(&mut self) {
        *TEST_GATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Build a `Command` for `wsl`, **without** the running-distro gate.
///
/// Private on purpose: the only legitimate ungated invocations are the gate's
/// own `--list` reads, which do not start a distro. Everything else must go
/// through [`wsl_command`].
fn wsl_command_ungated() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = Command::new("wsl");
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        Command::new("wsl")
    }
}

/// Build a `Command` for `wsl`, suppressing the transient console window on
/// Windows, **after** confirming the target distro is already running.
///
/// Returns [`WslUnavailable::DistroDown`] instead of a command when the distro
/// is down: spawning would start it, and the caller almost certainly wants to
/// *observe* the distro rather than resurrect it. Add args/output handling at
/// the call site as usual.
pub fn wsl_command() -> Result<Command, WslUnavailable> {
    ensure_distro_running()?;
    GATED_SPAWNS.fetch_add(1, Ordering::Relaxed);
    Ok(wsl_command_ungated())
}

/// The non-waking liveness gate: `Ok(())` iff the distro `wsl -e` targets is
/// already running.
pub fn ensure_distro_running() -> Result<(), WslUnavailable> {
    #[cfg(test)]
    {
        if let Some(verdict) = TEST_GATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
        {
            return verdict;
        }
    }

    let snapshot = current_snapshot()?;
    distro_gate_verdict(snapshot.target.as_deref(), &snapshot.running)
}

/// Return a snapshot no older than [`SNAPSHOT_TTL`], refreshing it if needed.
fn current_snapshot() -> Result<DistroSnapshot, WslUnavailable> {
    let mut slot = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = slot.as_ref() {
        if existing.taken_at.elapsed() < SNAPSHOT_TTL {
            return Ok(existing.clone());
        }
    }
    let fresh = take_snapshot()?;
    *slot = Some(fresh.clone());
    Ok(fresh)
}

/// Take a fresh reading with `wsl --list`. Neither invocation starts a distro.
fn take_snapshot() -> Result<DistroSnapshot, WslUnavailable> {
    let running = read_running_distros()?;
    let target = match std::env::var(DISTRO_ENV) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        // A failure to resolve the default distro is not fatal: the gate
        // degrades to "is anything running at all", which is still strictly
        // better than waking one.
        _ => read_default_distro(),
    };
    Ok(DistroSnapshot {
        taken_at: Instant::now(),
        target,
        running,
    })
}

/// `wsl --list --running --quiet` — the verified non-waking reading.
fn read_running_distros() -> Result<Vec<String>, WslUnavailable> {
    GATE_SPAWNS.fetch_add(1, Ordering::Relaxed);
    let output = wsl_command_ungated()
        .args(["--list", "--running", "--quiet"])
        .output()
        .map_err(|e| WslUnavailable::GateFailed(format!("failed to spawn `wsl --list`: {e}")))?;

    // `wsl --list --running --quiet` exits non-zero when NO distro is running
    // (it prints a localized "no running distributions" notice). That is a
    // legitimate answer, not a gate failure, so an empty parse of a non-zero
    // exit is treated as "nothing running" rather than UNKNOWN.
    let parsed = parse_running_distros(&String::from_utf8_lossy(&output.stdout));
    if !output.status.success() && !parsed.is_empty() {
        return Err(WslUnavailable::GateFailed(format!(
            "`wsl --list --running --quiet` exited {} with output",
            output.status
        )));
    }
    Ok(parsed)
}

/// `wsl --list --verbose` — used only to learn WHICH distro is the default,
/// i.e. the one a bare `wsl -e` targets. Also non-waking.
///
/// Deliberately reads only the `*` marker and the name: the STATE column is
/// localized (this fleet has a German-locale box), so nothing here may depend
/// on the word "Running".
fn read_default_distro() -> Option<String> {
    GATE_SPAWNS.fetch_add(1, Ordering::Relaxed);
    let output = wsl_command_ungated()
        .args(["--list", "--verbose"])
        .output()
        .ok()?;
    parse_default_distro(&String::from_utf8_lossy(&output.stdout))
}

// ---------------------------------------------------------------------------
// Pure parsers (unit-tested; no process spawning)
// ---------------------------------------------------------------------------

/// Normalize `wsl.exe --list` output.
///
/// The real output is UTF-16LE, so a lossy UTF-8 read of it interleaves every
/// character with a NUL byte (this is why the plan's own manual probe piped
/// through `tr -d '\0'`). It may also carry a BOM and CRLF line endings.
fn normalize_wsl_list(raw: &str) -> String {
    raw.replace(['\0', '\u{feff}'], "")
}

/// Parse `wsl --list --running --quiet` into distro names.
///
/// Tolerates NUL-interleaved UTF-16 bytes, a BOM, CRLF, blank lines, and the
/// localized "no running distributions" notice (which contains spaces and is
/// therefore not a distro name — WSL distro names never contain whitespace).
pub fn parse_running_distros(raw: &str) -> Vec<String> {
    normalize_wsl_list(raw)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // A distro name is a single whitespace-free token. Anything else is a
        // notice/error line, never a distro.
        .filter(|line| !line.chars().any(char::is_whitespace))
        .map(str::to_string)
        .collect()
}

/// Parse `wsl --list --verbose` for the default distro (the line marked `*`).
///
/// Only the marker and the name are read; the STATE column is localized and
/// must never be depended on.
pub fn parse_default_distro(raw: &str) -> Option<String> {
    normalize_wsl_list(raw)
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix('*'))
        .map(str::trim)
        .find_map(|rest| rest.split_whitespace().next().map(str::to_string))
        .filter(|name| !name.is_empty())
}

/// The gate's decision, split out as a pure function so it is testable without
/// a WSL installation.
pub fn distro_gate_verdict(target: Option<&str>, running: &[String]) -> Result<(), WslUnavailable> {
    let down = || {
        Err(WslUnavailable::DistroDown {
            distro: target.map(str::to_string),
            running: running.to_vec(),
        })
    };

    if running.is_empty() {
        return down();
    }
    match target {
        // The default distro could not be resolved. "Something is running" is
        // a weaker answer than we would like, but it never wakes a distro and
        // it is the honest limit of what we know.
        None => Ok(()),
        Some(name) => {
            if running.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                Ok(())
            } else {
                down()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape: UTF-16LE read lossily as UTF-8 leaves a NUL after every
    /// ASCII character, plus a BOM and CRLF endings.
    fn utf16ish(lines: &[&str]) -> String {
        let mut s = String::from("\u{feff}");
        for line in lines {
            for ch in line.chars() {
                s.push(ch);
                s.push('\0');
            }
            s.push('\r');
            s.push('\0');
            s.push('\n');
            s.push('\0');
        }
        s
    }

    #[test]
    fn parses_running_distros_from_nul_carrying_output() {
        let raw = utf16ish(&["Ubuntu-24.04", "docker-desktop"]);
        assert_eq!(
            parse_running_distros(&raw),
            vec!["Ubuntu-24.04".to_string(), "docker-desktop".to_string()]
        );
    }

    #[test]
    fn parses_running_distros_from_plain_output() {
        assert_eq!(
            parse_running_distros("Ubuntu-24.04\r\n"),
            vec!["Ubuntu-24.04".to_string()]
        );
    }

    #[test]
    fn empty_running_output_is_no_distros() {
        assert!(parse_running_distros("").is_empty());
        assert!(parse_running_distros("\r\n\r\n").is_empty());
        assert!(parse_running_distros(&utf16ish(&[""])).is_empty());
    }

    #[test]
    fn localized_no_running_notice_is_not_a_distro() {
        // wsl prints a prose notice (localized — German on one fleet box) when
        // nothing is running. It has spaces; distro names never do.
        let raw = utf16ish(&["Es werden keine Distributionen ausgeführt."]);
        assert!(parse_running_distros(&raw).is_empty());
        let raw_en = utf16ish(&["There are no running distributions."]);
        assert!(parse_running_distros(&raw_en).is_empty());
    }

    #[test]
    fn malformed_running_output_yields_no_false_distro() {
        // Garbage bytes must not become a distro name that satisfies the gate.
        assert!(parse_running_distros("   \t  \r\n \r\n").is_empty());
    }

    #[test]
    fn parses_default_distro_from_verbose_output() {
        let raw = utf16ish(&[
            "  NAME            STATE           VERSION",
            "* Ubuntu-24.04    Running         2",
            "  docker-desktop  Stopped         2",
        ]);
        assert_eq!(parse_default_distro(&raw).as_deref(), Some("Ubuntu-24.04"));
    }

    #[test]
    fn parses_default_distro_with_localized_state_column() {
        // The STATE column is localized; only the `*` and the name are read.
        let raw = utf16ish(&[
            "  NAME            STATUS          VERSION",
            "* Ubuntu-24.04    Wird ausgeführt 2",
        ]);
        assert_eq!(parse_default_distro(&raw).as_deref(), Some("Ubuntu-24.04"));
    }

    #[test]
    fn default_distro_absent_when_no_marker() {
        let raw = utf16ish(&["  NAME  STATE  VERSION", "  Ubuntu-24.04  Stopped  2"]);
        assert_eq!(parse_default_distro(&raw), None);
        assert_eq!(parse_default_distro(""), None);
    }

    #[test]
    fn gate_refuses_when_nothing_is_running() {
        let verdict = distro_gate_verdict(Some("Ubuntu-24.04"), &[]);
        assert_eq!(
            verdict,
            Err(WslUnavailable::DistroDown {
                distro: Some("Ubuntu-24.04".to_string()),
                running: vec![],
            })
        );
    }

    #[test]
    fn gate_refuses_when_only_a_different_distro_runs() {
        // The MSI shape: docker-desktop up, the runner's Ubuntu down. A gate
        // that only asked "is anything running" would wake Ubuntu here.
        let running = vec!["docker-desktop".to_string()];
        assert!(distro_gate_verdict(Some("Ubuntu-24.04"), &running).is_err());
    }

    #[test]
    fn gate_admits_when_the_target_distro_runs() {
        let running = vec!["docker-desktop".to_string(), "Ubuntu-24.04".to_string()];
        assert!(distro_gate_verdict(Some("Ubuntu-24.04"), &running).is_ok());
        // WSL distro names are case-insensitive on Windows.
        assert!(distro_gate_verdict(Some("ubuntu-24.04"), &running).is_ok());
    }

    #[test]
    fn gate_degrades_to_any_running_when_default_is_unknown() {
        let running = vec!["Ubuntu-24.04".to_string()];
        assert!(distro_gate_verdict(None, &running).is_ok());
        assert!(distro_gate_verdict(None, &[]).is_err());
    }

    #[test]
    fn distro_down_display_names_the_refusal() {
        let msg = WslUnavailable::DistroDown {
            distro: Some("Ubuntu-24.04".to_string()),
            running: vec!["docker-desktop".to_string()],
        }
        .to_string();
        assert!(msg.contains("Ubuntu-24.04"));
        assert!(msg.contains("docker-desktop"));
        assert!(msg.contains("refusing to spawn"));
    }

    #[test]
    fn gate_override_is_honored_and_cleared() {
        {
            let _guard = test_gate_override(Err(WslUnavailable::DistroDown {
                distro: None,
                running: vec![],
            }));
            assert!(ensure_distro_running().is_err());
            // The gate refusing means no gated command is ever constructed.
            let before = gated_spawn_count();
            assert!(wsl_command().is_err());
            assert_eq!(gated_spawn_count(), before);
        }
        // Guard dropped ⇒ override cleared (the next call would consult the
        // real gate, which we do not exercise here).
        assert!(TEST_GATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none());
    }

    #[test]
    fn gate_override_ok_lets_a_command_through_and_counts_it() {
        let _guard = test_gate_override(Ok(()));
        let before = gated_spawn_count();
        assert!(wsl_command().is_ok());
        assert_eq!(gated_spawn_count(), before + 1);
    }
}
