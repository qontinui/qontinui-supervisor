//! Restart-readiness gate — the supervisor half of plan
//! `2026-08-29-no-single-answer-to-is-it-safe-to-restart-the-runner` (Phase 3).
//!
//! # What was wrong
//!
//! The supervisor **advertised** a protection it did not implement.
//! `POST /runners/{id}/restart` and `POST /runners/{id}/stop` have carried a
//! `force` field since they were written, documented as *"Force restart/stop
//! even if the runner is protected"*, and the restart handler's doc comment
//! asserted *"Protected runners require `force: true` in the request body."*
//! Meanwhile `manager::restart_runner_by_id` declared the parameter
//! **`_force: bool`** and never read it, `manager::stop_runner_by_id` took no
//! force parameter at all, and `ManagedRunner::is_protected()` was read only to
//! *display* a flag in the runner listing. The only gate that ever ran was
//! manual-vs-automated. An operator who passed `force: false` — or who passed
//! nothing — got exactly the same behaviour as one who passed `force: true`:
//! the runner died, and with it every live agent session on it.
//!
//! That is the plan's "every advertised restart protection in this fleet is
//! inert" finding, instance 3 of 3.
//!
//! # What this module does
//!
//! Before the supervisor stops or restarts a runner it asks the runner itself:
//! `GET http://127.0.0.1:<port>/restart-readiness` (the endpoint built by the
//! plan's Phase 1). The runner answers with a verdict — `safe_to_restart`, a
//! human `reason`, and the two session planes reported **separately** (D6),
//! because `POST /drain` covers one of them and not the other.
//!
//! The verdict is the refusal condition. The *existing* `force` field is the
//! documented override. No second flag was introduced (delete-over-deprecate).
//!
//! # Three rules this module exists to enforce
//!
//! 1. **Fail closed on every unknown.** Unreachable, non-2xx, or unparseable is
//!    `Unknown`, and `Unknown` refuses. This gate is consulted precisely when
//!    someone is about to do something destructive, so an error path that
//!    resolves to "safe" is worse than no gate at all. A **404** is UNKNOWN too
//!    — but it is reported distinctly, because "this runner's build predates
//!    `/restart-readiness`" and "this runner is busy" call for completely
//!    different operator responses.
//!
//! 2. **Never recommend a drain that would no-op.** `POST /drain` enumerates
//!    `active_claude_sessions()` — the AI/task-run plane. The census that counts
//!    terminal-hosted agent sessions *explicitly exempts* that plane, so the two
//!    populations are disjoint by construction and a drain is a documented fast
//!    no-op for terminal-hosted sessions. A refusal that told an operator to
//!    "run `POST /drain` first" would manufacture exactly the false confidence
//!    the plan exists to destroy — the operator drains, sees nothing happen (or
//!    does not look), restarts, and loses the work anyway, now with the
//!    runner's own authority behind the mistake. See [`refusal_message`].
//!
//! 3. **IPv4 loopback, never `localhost`.** Windows resolves `localhost` to
//!    `::1` first while the runner binds IPv4 only, so `localhost` pays a doomed
//!    IPv6 connect before reaching the socket that answers (measured on this
//!    fleet: `127.0.0.1` 2133 ms, `[::1]` 2057 ms fail, `localhost` 4047 ms =
//!    the sum). Same rule as `health_cache::fetch_runner_health_body` and
//!    `fleet.rs`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::log_capture::{LogLevel, LogSource};
use crate::process::subtree_census::{CensusError, CensusVerdict, ClaudeCensus};
use crate::state::RunnerLiveness;

/// Path of the runner's readiness verdict (plan Phase 1).
pub const READINESS_PATH: &str = "/restart-readiness";

/// Timeout for the readiness probe, sized against the **tail** rather than the
/// median. `/health` on a loaded fleet box has been sampled between 296 ms and
/// 10120 ms, and `/restart-readiness` is strictly more expensive than `/health`
/// — it computes a fresh process-table cross-reference per request (plan D5)
/// instead of serving the 600 s cache. A timeout sized to the median would turn
/// a busy runner into an UNKNOWN, and UNKNOWN refuses, so an under-sized timeout
/// converts load into false refusals.
pub const READINESS_TIMEOUT: Duration = Duration::from_secs(15);

/// One session plane as the runner reports it (plan D6 — the two planes are
/// never fused into a single count, because a drain covers one and not the
/// other).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionPlane {
    #[serde(default)]
    pub count: u64,
    /// Whether `POST /drain` acts on this plane at all. Always `false` for
    /// `terminal_sessions`.
    #[serde(default)]
    pub drain_covers_these: bool,
    /// Subset of an AI-plane population whose work a drain would actually
    /// capture to `refs/wip/<agent_session_id>` (drain skips sessions holding
    /// no worktree). `None` on the terminal plane, where it is meaningless.
    #[serde(default)]
    pub wip_capture_eligible: Option<u64>,
}

/// The `drain` block of the readiness verdict.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DrainInfo {
    #[serde(default)]
    pub already_drained: bool,
    #[serde(default)]
    pub is_draining: bool,
    /// True when a drain would take its `"no live AI sessions — fast no-op"`
    /// branch. When this is true the refusal must never name a drain as a
    /// remedy.
    #[serde(default)]
    pub would_be_noop: bool,
    #[serde(default)]
    pub covers: Option<String>,
    #[serde(default)]
    pub call: Option<String>,
}

/// The runner's verdict, as parsed.
///
/// **`safe_to_restart` is deliberately NOT `#[serde(default)]`.** A body that
/// omits it fails to deserialize and lands in [`ReadinessUnknown::Unparseable`]
/// — which refuses. Defaulting it either way would be a lie: `false` would
/// refuse on a shape we never understood while claiming to know sessions were
/// live, and `true` would be the "unknown renders as safe" defect this whole
/// module exists to prevent. Every *other* field defaults, so a runner that
/// grows or drops an auxiliary field still gates correctly.
#[derive(Debug, Clone, Deserialize)]
pub struct ReadinessReport {
    pub safe_to_restart: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub terminal_sessions: Option<SessionPlane>,
    #[serde(default)]
    pub ai_sessions: Option<SessionPlane>,
    #[serde(default)]
    pub drain: Option<DrainInfo>,
    #[serde(default)]
    pub boundary: Option<String>,
}

impl ReadinessReport {
    pub fn terminal_count(&self) -> u64 {
        self.terminal_sessions
            .as_ref()
            .map(|p| p.count)
            .unwrap_or(0)
    }

    pub fn ai_count(&self) -> u64 {
        self.ai_sessions.as_ref().map(|p| p.count).unwrap_or(0)
    }

    /// Would `POST /drain` actually do anything?
    ///
    /// Conservative on purpose: we only believe a drain is useful when the
    /// runner said so *and* reported a non-empty AI plane. A missing `drain`
    /// block reads as "no, do not recommend it" — never as "yes".
    pub fn drain_would_help(&self) -> bool {
        let Some(drain) = self.drain.as_ref() else {
            return false;
        };
        !drain.would_be_noop
            && !drain.already_drained
            && self.ai_count() > 0
            && self
                .ai_sessions
                .as_ref()
                .map(|p| p.drain_covers_these)
                .unwrap_or(false)
    }
}

/// Why a readiness verdict could not be established. **Every variant refuses.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessUnknown {
    /// The runner answered `404`. Its build predates `GET /restart-readiness`
    /// (plan Phase 1). Still UNKNOWN — an old runner is not an idle runner —
    /// but reported distinctly so an operator can tell "old build" from
    /// "runner is busy" without reading the runner's source.
    EndpointAbsent,
    /// The runner did not answer at all: connection refused, DNS, TLS, or the
    /// [`READINESS_TIMEOUT`] elapsed.
    Unreachable { detail: String },
    /// The runner answered with a non-2xx that was not `404`.
    ErrorStatus { status: u16, body_excerpt: String },
    /// The runner answered 2xx with a body that is not a readiness verdict —
    /// not JSON, or JSON missing `safe_to_restart`.
    Unparseable {
        detail: String,
        body_excerpt: String,
    },
    /// The runner's door was wedged, so the supervisor fell back to its own
    /// process-subtree census — and the census could not be taken either (the
    /// root PID was absent, recycled, or the process table was unreadable).
    ///
    /// Both sources have now failed, so nothing is known about what is live on
    /// the runner. Refuses, like every other `Unknown`. The census error's own
    /// `code()` is carried inside `detail` so a log search can tell the three
    /// census failure modes apart without a second enum.
    CensusUnreadable { detail: String },
}

impl ReadinessUnknown {
    /// Stable machine-readable discriminant, surfaced as `cause` in the refusal
    /// body so a caller can branch without scraping prose.
    pub fn code(&self) -> &'static str {
        match self {
            ReadinessUnknown::EndpointAbsent => "readiness_endpoint_absent",
            ReadinessUnknown::Unreachable { .. } => "readiness_unreachable",
            ReadinessUnknown::ErrorStatus { .. } => "readiness_error_status",
            ReadinessUnknown::Unparseable { .. } => "readiness_unparseable",
            ReadinessUnknown::CensusUnreadable { .. } => "readiness_census_unreadable",
        }
    }
}

/// Which arm produced a readiness verdict.
///
/// The whole point of the census fallback is that a verdict can now come from
/// two places, so **every log line and every payload names the arm that
/// answered**. A `safe` that came from the supervisor's own process table and
/// a `safe` the runner reported are not the same claim, and an operator
/// reading the log afterwards must be able to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessSource {
    /// The runner answered `GET /restart-readiness` itself.
    RunnerVerdict,
    /// The runner's door was wedged and the supervisor walked the process
    /// table instead ([`crate::process::subtree_census`]).
    SupervisorCensus,
}

impl ReadinessSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadinessSource::RunnerVerdict => "runner_verdict",
            ReadinessSource::SupervisorCensus => "supervisor_census",
        }
    }
}

/// The outcome of asking a runner whether it is safe to stop.
///
/// The two `Census*` variants are the wedged-door arm (plan
/// `2026-09-03-runner-zombie-serving-watchdog` Phase 2). They are separate
/// variants rather than an extra field on `Safe`/`Unsafe` because the census
/// has no [`ReadinessReport`] to carry — it answers the same question from a
/// different source, and conflating the two would let a census verdict be
/// read as something the runner said.
#[derive(Debug, Clone)]
pub enum Readiness {
    Safe {
        report: ReadinessReport,
        raw: serde_json::Value,
    },
    Unsafe {
        report: ReadinessReport,
        raw: serde_json::Value,
    },
    /// Zero live `claude` processes under the runner's PID: there is nothing
    /// for a restart to destroy.
    CensusSafe {
        census: ClaudeCensus,
    },
    /// The census found live `claude` processes under the runner. Refuses —
    /// exactly as an `Unsafe` verdict from the runner would.
    CensusUnsafe {
        census: ClaudeCensus,
    },
    Unknown(ReadinessUnknown),
}

impl Readiness {
    /// Which arm answered. `Unknown` is attributed to the census only when the
    /// census itself is what failed.
    pub fn source(&self) -> ReadinessSource {
        match self {
            Readiness::Safe { .. } | Readiness::Unsafe { .. } => ReadinessSource::RunnerVerdict,
            Readiness::CensusSafe { .. } | Readiness::CensusUnsafe { .. } => {
                ReadinessSource::SupervisorCensus
            }
            Readiness::Unknown(ReadinessUnknown::CensusUnreadable { .. }) => {
                ReadinessSource::SupervisorCensus
            }
            Readiness::Unknown(_) => ReadinessSource::RunnerVerdict,
        }
    }
}

/// Keep body excerpts bounded — a runner that answers with an HTML error page
/// must not push a megabyte into a log line or an error body.
const BODY_EXCERPT_CHARS: usize = 400;

fn excerpt(s: &str) -> String {
    if s.chars().count() <= BODY_EXCERPT_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(BODY_EXCERPT_CHARS - 1).collect();
    out.push('…');
    out
}

/// Ask the runner on `port` whether it is safe to stop.
///
/// IPv4 loopback only — see the module docs for the measurement.
pub async fn probe(port: u16) -> Readiness {
    probe_at(&format!("http://127.0.0.1:{}", port)).await
}

/// [`probe`] against an explicit base URL. Split out so the tests can drive a
/// real HTTP server on an ephemeral port instead of requiring a live runner.
pub async fn probe_at(base_url: &str) -> Readiness {
    probe_at_with_timeout(base_url, READINESS_TIMEOUT).await
}

/// [`probe_at`] with an explicit timeout.
///
/// The timeout is a parameter only so the phase-2 signature of a wedge —
/// a listener that ACCEPTS and never answers — can be exercised in a unit test
/// without spending [`READINESS_TIMEOUT`] per case. Production callers use
/// [`probe`]/[`probe_at`], which pass the constant.
pub async fn probe_at_with_timeout(base_url: &str, timeout: Duration) -> Readiness {
    let url = format!("{}{}", base_url.trim_end_matches('/'), READINESS_PATH);

    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            // Could not even construct a client. Nothing was learned about the
            // runner, so this is UNKNOWN like any other probe failure.
            return Readiness::Unknown(ReadinessUnknown::Unreachable {
                detail: format!("could not build HTTP client: {e}"),
            });
        }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return Readiness::Unknown(ReadinessUnknown::Unreachable {
                detail: format!("GET {url} failed: {e}"),
            });
        }
    };

    let status = resp.status();
    // Read the body as text first: we need an excerpt for the non-2xx and
    // unparseable arms, and `resp.json()` would consume the response and throw
    // the bytes away.
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Readiness::Unknown(ReadinessUnknown::Unreachable {
                detail: format!("GET {url} returned {status} but the body could not be read: {e}"),
            });
        }
    };

    if status.as_u16() == 404 {
        return Readiness::Unknown(ReadinessUnknown::EndpointAbsent);
    }
    if !status.is_success() {
        return Readiness::Unknown(ReadinessUnknown::ErrorStatus {
            status: status.as_u16(),
            body_excerpt: excerpt(&body),
        });
    }

    let raw: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return Readiness::Unknown(ReadinessUnknown::Unparseable {
                detail: format!("response body is not JSON: {e}"),
                body_excerpt: excerpt(&body),
            });
        }
    };

    let report: ReadinessReport = match serde_json::from_value(raw.clone()) {
        Ok(r) => r,
        Err(e) => {
            return Readiness::Unknown(ReadinessUnknown::Unparseable {
                detail: format!("response is not a restart-readiness verdict: {e}"),
                body_excerpt: excerpt(&body),
            });
        }
    };

    if report.safe_to_restart {
        Readiness::Safe { report, raw }
    } else {
        Readiness::Unsafe { report, raw }
    }
}

// ── The census fallback: a readiness source that survives a wedged door ─────

/// Is the supervisor's own census the right thing to consult for this probe
/// result?
///
/// **Only when the door is wedged, never when it is merely disagreeing.** The
/// two conditions are both required and both narrow:
///
/// - the probe came back [`ReadinessUnknown::Unreachable`] — the runner did
///   not answer at all. A runner that answered `404`, a non-2xx, or garbage
///   *answered*: its door is up and its verdict (or its refusal to give one)
///   stands, and second-guessing it with a process walk would let the census
///   overrule a live runner.
/// - the supervisor has independently classified the runner as
///   [`RunnerLiveness::UnresponsiveSince`] — port held, API silent, and
///   previously seen responding. [`RunnerLiveness::Unknown`] is deliberately
///   excluded (plan Design decision 4): a runner that has never answered has
///   nothing to prove it is the wedge class.
pub fn census_applies(probe: &Readiness, liveness: RunnerLiveness) -> bool {
    matches!(
        probe,
        Readiness::Unknown(ReadinessUnknown::Unreachable { .. })
    ) && matches!(liveness, RunnerLiveness::UnresponsiveSince(_))
}

/// Fold a census outcome into a readiness verdict. Pure — no I/O — so every
/// arm is unit-testable with hand-built censuses.
///
/// `census` is `None` when [`census_applies`] said not to consult it, in which
/// case the probe result passes through untouched.
pub fn fold_census(
    probe: Readiness,
    census: Option<Result<ClaudeCensus, CensusError>>,
) -> Readiness {
    match census {
        None => probe,
        Some(Ok(census)) => match census.verdict() {
            CensusVerdict::Idle => Readiness::CensusSafe { census },
            CensusVerdict::Busy { .. } => Readiness::CensusUnsafe { census },
        },
        // Both sources have now failed. Fail closed, and carry the census
        // error's own code so the three failure modes stay distinguishable.
        Some(Err(e)) => Readiness::Unknown(ReadinessUnknown::CensusUnreadable {
            detail: format!("{} ({})", e, e.code()),
        }),
    }
}

/// Ask the runner, and — if and only if its door is wedged — ask the operating
/// system instead.
///
/// This is the readiness source the plan exists to add: before it, the verdict
/// for a wedged runner was served by the very door that was wedged, so it was
/// always `Unreachable` → `Unknown` → refuse, and the one recovery path the
/// fleet has could never be taken.
pub async fn probe_with_census_fallback(
    port: u16,
    liveness: RunnerLiveness,
    root_pid: Option<u32>,
    started_at: Option<DateTime<Utc>>,
) -> Readiness {
    let probe_result = probe(port).await;
    if !census_applies(&probe_result, liveness) {
        return probe_result;
    }
    let census = crate::process::subtree_census::take_census_for_port(
        root_pid,
        port,
        started_at.map(|t| t.timestamp()),
    )
    .await;
    fold_census(probe_result, Some(census))
}

/// Verdict of [`drained_for_deferred_rebuild`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainedVerdict {
    /// True ONLY when the primary is positively known to hold no sessions.
    pub drained: bool,
    /// Human-readable why, for the log line and the build-status body.
    pub reason: String,
}

/// Is the runner positively known to hold no Claude sessions right now?
///
/// This is the predicate behind `POST /runner/rebuild-when-idle`, split out
/// from the polling loop so it can be tested directly — the loop around it is
/// just a timer.
///
/// The asymmetry is the point. `true` requires POSITIVE evidence of emptiness:
/// either the runner's own verdict with BOTH session planes at zero, or a
/// census that walked the process subtree and found no live `claude`. Every
/// other state — a refusal, live processes, and **every `Unknown`** — is
/// `false`. An unreachable runner, a 404 from a build that predates the
/// endpoint, an unparseable body, and a census that could not be taken are all
/// "not proven empty", never "empty"
/// [policy: verification-and-evidence silent-empty-is-unknown].
///
/// Note it does NOT consult `safe_to_restart`. That flag is the runner's own
/// opinion about restart safety, which can be true for reasons unrelated to
/// session count; this endpoint promised to wait for the SESSIONS to drain, so
/// it reads the counts.
pub fn drained_for_deferred_rebuild(readiness: &Readiness) -> DrainedVerdict {
    match readiness {
        Readiness::Safe { report, .. } => {
            let t = report.terminal_count();
            let a = report.ai_count();
            DrainedVerdict {
                drained: t == 0 && a == 0,
                reason: format!("runner reports {t} terminal / {a} ai session(s)"),
            }
        }
        Readiness::Unsafe { report, .. } => DrainedVerdict {
            drained: false,
            reason: format!(
                "runner refuses: {} terminal / {} ai session(s)",
                report.terminal_count(),
                report.ai_count()
            ),
        },
        Readiness::CensusSafe { census } => DrainedVerdict {
            drained: true,
            reason: format!(
                "census: zero live claude under pid {} ({} process(es) walked)",
                census.root_pid, census.walked
            ),
        },
        Readiness::CensusUnsafe { census } => DrainedVerdict {
            drained: false,
            reason: format!(
                "census: {} live claude under pid {}",
                census.live.len(),
                census.root_pid
            ),
        },
        Readiness::Unknown(u) => DrainedVerdict {
            drained: false,
            reason: format!("readiness unknown ({})", u.code()),
        },
    }
}

/// Which destructive operation the gate is guarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    Stop,
    Restart,
}

impl GateAction {
    pub fn as_str(self) -> &'static str {
        match self {
            GateAction::Stop => "stop",
            GateAction::Restart => "restart",
        }
    }

    /// Present-tense verb for the refusal prose.
    fn verb(self) -> &'static str {
        match self {
            GateAction::Stop => "stopping",
            GateAction::Restart => "restarting",
        }
    }
}

/// Why the gate did not probe at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Ephemeral `test-*` runner. Temp runners are the sanctioned way agents
    /// verify a change without touching the primary, and the supervisor's own
    /// lifecycle stops them constantly — the spawn-test failed-probe cleanup,
    /// the frontend-stale teardown, `purge-stale`, the max-age reaper, and the
    /// build-slot exe-lock breaker all call `stop_runner_by_id`. Gating those
    /// would make `POST /runners/spawn-test` unusable and would leak zombie
    /// runners on every failed spawn.
    ///
    /// Note every temp runner is minted `protected: true`, so a protection-based
    /// exemption would not work here — the same reasoning the temp-runner
    /// max-age reaper already records at its own kill site.
    TempRunner,
    /// The runner is explicitly unprotected (`POST /runners/{id}/protect`
    /// `{"protected": false}`). That setter existed and, until this module, did
    /// nothing but flip a displayed flag; honouring it here is what makes
    /// `is_protected()` participate in a stop/restart decision at all.
    NotProtected,
    /// The supervisor has no evidence the runner is alive — it is not tracked
    /// as running AND its API is not responding. There is no work in flight to
    /// destroy, so there is nothing for the gate to protect. This is decided
    /// from the supervisor's own state, never from a failed probe: a probe that
    /// fails is [`ReadinessUnknown`] and refuses.
    NotRunning,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::TempRunner => "temp_runner",
            SkipReason::NotProtected => "runner_not_protected",
            SkipReason::NotRunning => "runner_not_running",
        }
    }
}

/// The three exemptions, as a pure four-way decision. `None` means "probe".
///
/// # The defect this replaces
///
/// Exemption 3 used to read `!tracked_running && !api_responding` — a pair of
/// booleans that, for the primary, is EXACTLY the signature of a wedge.
/// `health_cache` overwrites `running` from the `/health` probe for a
/// user-managed runner, so a wedged primary sits at `running: false,
/// api_responding: false` while owning its port and hosting live sessions. The
/// gate read that as *"not running, nothing to protect"* and let the restart
/// through **with no gate at all** — the exact inverse of the failure the gate
/// was built to prevent, and the more dangerous direction.
///
/// [`RunnerLiveness`] is the classifier that already knows the difference.
/// `NotRunning` is now granted only where the supervisor can say the port is
/// **not held**:
///
/// - [`RunnerLiveness::Stopped`] — positive evidence of absence (`state.rs`).
/// - [`RunnerLiveness::Unknown`] with the port closed and nothing tracked
///   running — a runner that has never answered and holds no socket. This is
///   the rest of the old exemption, kept: it was never wrong, it was too wide.
///
/// What is REMOVED from the exemption is the port-held case:
/// [`RunnerLiveness::UnresponsiveSince`] (the wedge) and `Unknown` **with the
/// port still held** now fall through to the probe — and, for a wedge, on to
/// the census. A process holding a socket is not a process with nothing to
/// protect.
pub fn decide_skip(
    liveness: RunnerLiveness,
    port_open: bool,
    tracked_running: bool,
    is_temp: bool,
    protected: bool,
) -> Option<SkipReason> {
    if is_temp {
        return Some(SkipReason::TempRunner);
    }
    if !protected {
        return Some(SkipReason::NotProtected);
    }
    match liveness {
        RunnerLiveness::Stopped => Some(SkipReason::NotRunning),
        RunnerLiveness::Unknown if !port_open && !tracked_running => Some(SkipReason::NotRunning),
        _ => None,
    }
}

/// Everything an operator or a caller needs to see about a refusal — or about
/// an override, which is logged with the identical payload so the two are
/// comparable in a log search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalDetail {
    /// Machine-readable cause: `sessions_live`, or a [`ReadinessUnknown::code`].
    pub cause: &'static str,
    /// Full operator-facing prose, including what would be lost and the
    /// override.
    pub message: String,
    /// Structured body, embedded in the error response and in the log line.
    pub payload: serde_json::Value,
}

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// The runner reported it is safe to stop. Proceed.
    Allowed,
    /// The runner reported unsafe (or could not be reached) but the caller
    /// passed `force: true`. Proceed — and log the refusal that was overridden.
    Overridden(RefusalDetail),
    /// Refuse.
    Refused(RefusalDetail),
}

/// Build the operator-facing refusal prose.
///
/// **The drain rule lives here** (plan D3). The message names `POST /drain` as
/// a remedy only when [`ReadinessReport::drain_would_help`] — i.e. the runner
/// itself reported a non-empty AI/task-run plane that a drain covers. Whenever
/// terminal-hosted sessions are live, the message says the opposite in as many
/// words: a drain does not cover them and there is no graceful path. Recommending
/// a drain that no-ops would ship a system *worse* than the status quo, because
/// the false reassurance would carry the runner's own authority.
fn refusal_message(
    runner_name: &str,
    port: u16,
    action: GateAction,
    report: &ReadinessReport,
) -> String {
    let terminal = report.terminal_count();
    let ai = report.ai_count();

    let mut msg = format!(
        "restart_refused_unsafe: runner '{runner_name}' (port {port}) reports it is NOT safe to \
         restart. Runner's reason: {}.",
        report
            .reason
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or("(none given)")
    );

    if terminal > 0 {
        msg.push_str(&format!(
            " {verb} it now DESTROYS {terminal} live terminal-hosted agent session(s) — their \
             unflushed turns and uncommitted worktrees are lost. There is NO graceful path for \
             this plane: `POST /drain` acts on the AI/task-run plane only and is a documented \
             fast no-op for terminal-hosted sessions, so draining first would change nothing \
             while making the loss look handled. Do not drain and retry — either wait for these \
             sessions to finish, or accept the loss explicitly.",
            verb = match action {
                GateAction::Stop => "Stopping",
                GateAction::Restart => "Restarting",
            },
        ));
    }

    if ai > 0 {
        let eligible = report
            .ai_sessions
            .as_ref()
            .and_then(|p| p.wip_capture_eligible);
        if report.drain_would_help() {
            let call = report
                .drain
                .as_ref()
                .and_then(|d| d.call.clone())
                .unwrap_or_else(|| format!("POST http://127.0.0.1:{}/drain", port));
            msg.push_str(&format!(
                " {ai} AI/task-run session(s) are also live; those a drain DOES cover — `{call}` \
                 would capture{} their work to refs/wip/<agent_session_id>. Note a drain is \
                 TERMINAL: a drained runner refuses new AI turns for the rest of its life and \
                 must be restarted afterwards.",
                match eligible {
                    Some(n) => format!(" {n} of"),
                    None => String::new(),
                }
            ));
        } else {
            msg.push_str(&format!(
                " {ai} AI/task-run session(s) are also live, and a drain would not help them \
                 either (the runner reports the drain as already-run or a no-op)."
            ));
        }
    }

    if terminal == 0 && ai == 0 {
        // The runner said unsafe without reporting a live session on either
        // plane — e.g. its own process-table snapshot was unavailable, which
        // Phase 1 fails closed on. Do not paper over it with a session count we
        // do not have.
        msg.push_str(
            " The runner reported no live sessions on either plane, so the refusal comes from \
             the runner's own fail-closed path rather than from a session count — read the \
             reason above and the `readiness` payload before overriding.",
        );
    }

    msg.push_str(&format!(
        " To {} anyway and accept the loss, re-issue this request with {{\"force\": true}}; the \
         override is logged with this verdict.",
        action.as_str()
    ));

    msg
}

/// Refusal prose for an UNKNOWN verdict.
///
/// The 404 arm reads differently from the others on purpose: "this runner's
/// build predates the endpoint" and "this runner is busy" demand completely
/// different operator responses, and both would otherwise collapse into a
/// generic "could not determine".
fn unknown_message(
    runner_name: &str,
    port: u16,
    action: GateAction,
    url: &str,
    unknown: &ReadinessUnknown,
) -> String {
    let head = match unknown {
        ReadinessUnknown::EndpointAbsent => format!(
            "restart_refused_unknown: runner '{runner_name}' (port {port}) answered 404 at \
             {url} — THIS RUNNER'S BUILD PREDATES the restart-readiness endpoint, so it cannot \
             report whether agent sessions are live on it. This is an OLD RUNNER, not an idle \
             one."
        ),
        ReadinessUnknown::Unreachable { detail } => format!(
            "restart_refused_unknown: runner '{runner_name}' (port {port}) did not answer \
             {url} ({detail}). A runner that does not answer is not a runner that is idle."
        ),
        ReadinessUnknown::ErrorStatus {
            status,
            body_excerpt,
        } => format!(
            "restart_refused_unknown: runner '{runner_name}' (port {port}) answered HTTP {status} \
             at {url} instead of a readiness verdict (body: {body_excerpt})."
        ),
        ReadinessUnknown::Unparseable {
            detail,
            body_excerpt,
        } => format!(
            "restart_refused_unknown: runner '{runner_name}' (port {port}) answered {url} with \
             something that is not a readiness verdict ({detail}; body: {body_excerpt})."
        ),
        ReadinessUnknown::CensusUnreadable { detail } => format!(
            "restart_refused_unknown: runner '{runner_name}' (port {port}) did not answer {url} \
             AND the supervisor's own process-subtree census could not be taken either \
             ({detail}). BOTH readiness sources have failed."
        ),
    };

    let tail = match unknown {
        ReadinessUnknown::EndpointAbsent => {
            " Rebuild or update this runner to a build that serves GET /restart-readiness so the \
             question can be answered, or re-issue with {\"force\": true} to proceed without \
             knowing what is live on it."
        }
        ReadinessUnknown::CensusUnreadable { .. } => {
            " Refusing: the wedged-door fallback exists precisely so a silent runner still gets \
             an answer, and it did not get one here, so nothing at all is known about what is \
             live on this runner. Re-issue with {\"force\": true} to proceed without knowing."
        }
        _ => {
            " Refusing: an unknown verdict is never treated as safe, because this gate is \
             consulted precisely when the next step is destructive. Re-issue with \
             {\"force\": true} to proceed without knowing what is live on it."
        }
    };

    format!(
        "{head} Cannot determine whether {} this runner would destroy live work.{tail}",
        action.verb()
    )
}

/// Refusal prose for a census `Busy` verdict.
///
/// Deliberately says WHERE the count came from. The runner never spoke here —
/// its door was wedged — and a refusal that read like a runner verdict would
/// misattribute a claim the runner did not make.
fn census_refusal_message(
    runner_name: &str,
    port: u16,
    action: GateAction,
    census: &ClaudeCensus,
) -> String {
    let (count, pids) = match census.verdict() {
        CensusVerdict::Busy { count, pids } => (count, pids),
        CensusVerdict::Idle => (0, Vec::new()),
    };
    let pid_list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "restart_refused_unsafe: runner '{runner_name}' (port {port}) did not answer \
         {READINESS_PATH}, so the supervisor counted live sessions itself: {count} live \
         `claude` process(es) (PID(s) {pid_list}) counted by the supervisor's process-subtree \
         census because the runner's `{READINESS_PATH}` was unreachable. {} it now DESTROYS \
         them — their unflushed turns and uncommitted worktrees are lost, and `POST /drain` \
         cannot be reached to capture anything (the same door is wedged). To {} anyway and \
         accept the loss, re-issue this request with {{\"force\": true}}; the override is \
         logged with this verdict.",
        match action {
            GateAction::Stop => "Stopping",
            GateAction::Restart => "Restarting",
        },
        action.as_str(),
    )
}

/// Decide, from a readiness outcome, whether to proceed. Pure — no I/O, no
/// state — so every arm is unit-testable without a runner.
pub fn decide(
    runner_id: &str,
    runner_name: &str,
    port: u16,
    action: GateAction,
    force: bool,
    readiness: &Readiness,
) -> GateDecision {
    let url = format!("http://127.0.0.1:{}{}", port, READINESS_PATH);

    let refusal = match readiness {
        Readiness::Safe { .. } | Readiness::CensusSafe { .. } => return GateDecision::Allowed,
        Readiness::CensusUnsafe { census } => {
            let message = census_refusal_message(runner_name, port, action, census);
            RefusalDetail {
                cause: "sessions_live",
                payload: serde_json::json!({
                    "error": "restart_refused_unsafe",
                    "cause": "sessions_live",
                    "source": ReadinessSource::SupervisorCensus.as_str(),
                    "runner_id": runner_id,
                    "runner_name": runner_name,
                    "action": action.as_str(),
                    "message": message,
                    // The census cannot separate the two planes the runner
                    // reports — one `claude` image covers both — so it reports
                    // the total under its own key rather than guessing a split.
                    "would_be_lost": {
                        "live_claude_processes": census.live.len(),
                    },
                    "boundary": "supervisor process-subtree census: counts every `claude` image \
                                 in the runner PID's inclusive subtree, which over-approximates \
                                 (a lingering shim counts) and therefore only ever refuses more \
                                 often, never less",
                    "readiness_url": url,
                    "readiness": serde_json::Value::Null,
                    "census": census.to_json(),
                    "override": "re-issue with {\"force\": true}",
                }),
                message,
            }
        }
        Readiness::Unsafe { report, raw } => {
            let message = refusal_message(runner_name, port, action, report);
            RefusalDetail {
                cause: "sessions_live",
                payload: serde_json::json!({
                    "error": "restart_refused_unsafe",
                    "cause": "sessions_live",
                    "source": ReadinessSource::RunnerVerdict.as_str(),
                    "runner_id": runner_id,
                    "runner_name": runner_name,
                    "action": action.as_str(),
                    "message": message,
                    "would_be_lost": {
                        "terminal_sessions": report.terminal_count(),
                        "ai_sessions": report.ai_count(),
                    },
                    "drain_would_help": report.drain_would_help(),
                    "drain": {
                        "already_drained": report.drain.as_ref().map(|d| d.already_drained),
                        "is_draining": report.drain.as_ref().map(|d| d.is_draining),
                        "would_be_noop": report.drain.as_ref().map(|d| d.would_be_noop),
                        "covers": report.drain.as_ref().and_then(|d| d.covers.clone()),
                    },
                    // The runner's own statement of what its census can and
                    // cannot see. Reproduced rather than summarised: a refusal
                    // that implied omniscience would be its own defect.
                    "boundary": report.boundary,
                    "readiness_url": url,
                    "readiness": raw,
                    "override": "re-issue with {\"force\": true}",
                }),
                message,
            }
        }
        Readiness::Unknown(unknown) => {
            let message = unknown_message(runner_name, port, action, &url, unknown);
            RefusalDetail {
                cause: unknown.code(),
                payload: serde_json::json!({
                    "error": "restart_refused_unknown",
                    "cause": unknown.code(),
                    "source": readiness.source().as_str(),
                    "runner_id": runner_id,
                    "runner_name": runner_name,
                    "action": action.as_str(),
                    "message": message,
                    // Explicit, so nobody reads the absence of a count as zero.
                    "would_be_lost": serde_json::Value::Null,
                    "readiness_url": url,
                    "readiness": serde_json::Value::Null,
                    "override": "re-issue with {\"force\": true}",
                }),
                message,
            }
        }
    };

    if force {
        GateDecision::Overridden(refusal)
    } else {
        GateDecision::Refused(refusal)
    }
}

// ── The gate, as the stop/restart paths call it ─────────────────────────────

/// Ask the runner whether it is safe to stop, and either proceed or refuse.
///
/// Called by [`crate::process::manager::stop_runner_by_id`] and
/// [`crate::process::manager::restart_runner_by_id`] — the two funnels every
/// destructive lifecycle path in the supervisor already goes through.
///
/// Returns `Ok(())` to proceed and
/// [`SupervisorError::RestartUnsafe`](crate::error::SupervisorError::RestartUnsafe)
/// to refuse. **Every refusal and every override is logged with the readiness
/// payload that produced it**, at `Warn` for a refusal and `Error` for an
/// override — an override is the louder of the two on purpose, because it is
/// the one that destroys work.
///
/// # Three exemptions, each for a stated reason
///
/// See [`SkipReason`]. None of them is an error path: an exemption is decided
/// from the supervisor's own state before any probe is issued, whereas a probe
/// that fails is [`ReadinessUnknown`] and refuses.
pub async fn enforce(
    state: &crate::state::SharedState,
    managed: &crate::state::ManagedRunner,
    action: GateAction,
    force: bool,
) -> Result<(), crate::error::SupervisorError> {
    let runner_id = managed.config.id.clone();
    let runner_name = managed.config.name.clone();
    let port = managed.config.port;

    // The exemptions are decided from the supervisor's own state, before any
    // probe is issued. `CachedPortHealth` (`managed.cached_health`) is the
    // per-runner probe result — NOT `CachedRunnerHealth`, which is the SSE
    // snapshot vector and carries a different pair of fields.
    let (port_open, api_responding) = {
        let h = managed.cached_health.read().await;
        (h.runner_port_open, h.runner_responding)
    };
    let (liveness, tracked_running, root_pid, started_at) = {
        let r = managed.runner.read().await;
        (
            r.liveness(port_open, api_responding),
            r.running,
            r.pid,
            r.started_at,
        )
    };

    if let Some(reason) = decide_skip(
        liveness,
        port_open,
        tracked_running,
        crate::process::manager::is_temp_runner(&runner_id),
        managed.is_protected().await,
    ) {
        skip(&runner_id, action, reason);
        return Ok(());
    }

    // Ask the runner; fall back to the supervisor's own process table only
    // when the runner's door is wedged (`census_applies`).
    let readiness = probe_with_census_fallback(port, liveness, root_pid, started_at).await;
    match decide(&runner_id, &runner_name, port, action, force, &readiness) {
        GateDecision::Allowed => {
            // The allow path carries the verdict too, not just the refusals.
            // "Why was this stop permitted?" has to be answerable from the log
            // afterwards — a gate that records only what it blocked cannot be
            // audited for what it let through.
            let msg = match &readiness {
                Readiness::Safe { report, raw } => format!(
                    "restart-readiness: runner '{runner_name}' reports safe_to_restart=true \
                     (terminal_sessions={terminal}, ai_sessions={ai}, reason: {reason}) — \
                     proceeding with {act} | source={src} | verdict={verdict}",
                    terminal = report.terminal_count(),
                    ai = report.ai_count(),
                    reason = report
                        .reason
                        .clone()
                        .unwrap_or_else(|| "(none given)".to_string()),
                    act = action.as_str(),
                    src = ReadinessSource::RunnerVerdict.as_str(),
                    verdict = raw,
                ),
                // The census allow prints the same `verdict=` suffix as a
                // runner allow, so the two are comparable in one log search —
                // and names its source, so they are never confused.
                Readiness::CensusSafe { census } => format!(
                    "restart-readiness: runner '{runner_name}' did not answer \
                     {READINESS_PATH} (wedged), and the supervisor's process-subtree census \
                     found ZERO live `claude` processes under PID {root} — nothing to destroy, \
                     proceeding with {act} | source={src} | verdict={verdict}",
                    root = census.root_pid,
                    act = action.as_str(),
                    src = ReadinessSource::SupervisorCensus.as_str(),
                    verdict = census.to_json(),
                ),
                // Unreachable: `decide` returns `Allowed` only for the two
                // safe arms.
                _ => format!(
                    "restart-readiness: proceeding with {} for runner '{runner_name}' | \
                     verdict=null",
                    action.as_str()
                ),
            };
            info!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Info, msg)
                .await;
            Ok(())
        }
        GateDecision::Overridden(detail) => {
            // The loudest line the supervisor writes. An override is a
            // deliberate destruction of live work, and it must be
            // reconstructible from the log alone — verdict included.
            let msg = format!(
                "restart-readiness OVERRIDE (force=true): {} runner '{runner_name}' anyway. \
                 {} | verdict={}",
                action.as_str(),
                detail.message,
                serde_json::to_string(&detail.payload)
                    .unwrap_or_else(|_| "<unserializable>".to_string()),
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Error, msg)
                .await;
            Ok(())
        }
        GateDecision::Refused(detail) => {
            let msg = format!(
                "restart-readiness REFUSED: {} | verdict={}",
                detail.message,
                serde_json::to_string(&detail.payload)
                    .unwrap_or_else(|_| "<unserializable>".to_string()),
            );
            warn!("{}", msg);
            state
                .logs
                .emit(LogSource::Supervisor, LogLevel::Warn, msg)
                .await;
            Err(crate::error::SupervisorError::RestartUnsafe(Box::new(
                detail,
            )))
        }
    }
}

/// Log an exemption at debug level and proceed. Exemptions are routine (the
/// temp-runner reaper alone hits one every sweep), so they do not go to the
/// operator log buffer — but they are traceable.
fn skip(runner_id: &str, action: GateAction, reason: SkipReason) {
    debug!(
        "restart-readiness gate skipped for '{}' ({}): {}",
        runner_id,
        action.as_str(),
        reason.as_str()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    /// Spin up a server on an ephemeral port that answers `/restart-readiness`
    /// with a fixed (status, body). Same shape the flywheel tests use — a real
    /// HTTP round trip, no live runner required.
    async fn spawn_mock(status: u16, body: &'static str) -> String {
        let app = Router::new().route(
            READINESS_PATH,
            get(move || async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    /// A URL nothing is listening on: bind an ephemeral port to reserve it,
    /// then drop the listener.
    async fn dead_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{}", port)
    }

    const UNSAFE_TERMINAL: &str = r#"{
        "safe_to_restart": false,
        "reason": "25 terminal-hosted agent sessions are live; no graceful stop path exists for them",
        "terminal_sessions": { "count": 25, "drain_covers_these": false },
        "ai_sessions": { "count": 0, "drain_covers_these": true, "wip_capture_eligible": 0 },
        "drain": { "already_drained": false, "is_draining": false, "would_be_noop": true,
                   "covers": "ai_sessions only", "call": "POST http://127.0.0.1:9876/drain" }
    }"#;

    const UNSAFE_AI_ONLY: &str = r#"{
        "safe_to_restart": false,
        "reason": "3 AI/task-run sessions are live",
        "terminal_sessions": { "count": 0, "drain_covers_these": false },
        "ai_sessions": { "count": 3, "drain_covers_these": true, "wip_capture_eligible": 2 },
        "drain": { "already_drained": false, "is_draining": false, "would_be_noop": false,
                   "covers": "ai_sessions only", "call": "POST http://127.0.0.1:9876/drain" }
    }"#;

    const SAFE: &str = r#"{
        "safe_to_restart": true,
        "reason": "no live sessions on either plane",
        "terminal_sessions": { "count": 0, "drain_covers_these": false },
        "ai_sessions": { "count": 0, "drain_covers_these": true, "wip_capture_eligible": 0 },
        "drain": { "already_drained": false, "is_draining": false, "would_be_noop": true }
    }"#;

    fn decide_stop(force: bool, r: &Readiness) -> GateDecision {
        decide("primary", "Primary", 9876, GateAction::Stop, force, r)
    }

    // ── probe: the five failure modes ───────────────────────────────────────

    #[tokio::test]
    async fn probe_parses_an_unsafe_verdict() {
        let base = spawn_mock(200, UNSAFE_TERMINAL).await;
        let r = probe_at(&base).await;
        let Readiness::Unsafe { report, raw } = r else {
            panic!("expected Unsafe, got {r:?}");
        };
        assert_eq!(report.terminal_count(), 25);
        assert_eq!(report.ai_count(), 0);
        assert!(!report.drain_would_help());
        assert!(raw["reason"].as_str().unwrap().contains("25"));
    }

    #[tokio::test]
    async fn probe_parses_a_safe_verdict() {
        let base = spawn_mock(200, SAFE).await;
        assert!(matches!(probe_at(&base).await, Readiness::Safe { .. }));
    }

    #[tokio::test]
    async fn probe_unreachable_is_unknown_not_safe() {
        let base = dead_url().await;
        let r = probe_at(&base).await;
        let Readiness::Unknown(ReadinessUnknown::Unreachable { detail }) = r else {
            panic!("expected Unreachable, got {r:?}");
        };
        assert!(detail.contains("failed"), "detail was: {detail}");
        // The load-bearing assertion of this whole module: an error never
        // resolves to Safe.
        assert!(!matches!(probe_at(&base).await, Readiness::Safe { .. }));
    }

    #[tokio::test]
    async fn probe_404_is_endpoint_absent() {
        let base = spawn_mock(404, r#"{"error":"not found"}"#).await;
        assert!(matches!(
            probe_at(&base).await,
            Readiness::Unknown(ReadinessUnknown::EndpointAbsent)
        ));
    }

    #[tokio::test]
    async fn probe_500_is_error_status_not_endpoint_absent() {
        let base = spawn_mock(500, r#"{"error":"boom"}"#).await;
        let Readiness::Unknown(ReadinessUnknown::ErrorStatus { status, .. }) =
            probe_at(&base).await
        else {
            panic!("expected ErrorStatus");
        };
        assert_eq!(status, 500);
    }

    #[tokio::test]
    async fn probe_non_json_body_is_unparseable() {
        let base = spawn_mock(200, "<html>gateway</html>").await;
        assert!(matches!(
            probe_at(&base).await,
            Readiness::Unknown(ReadinessUnknown::Unparseable { .. })
        ));
    }

    #[tokio::test]
    async fn probe_json_missing_safe_to_restart_is_unparseable_never_safe() {
        // A verdict without the verdict field. Defaulting it would be the
        // "unknown renders as a default" defect; it must refuse.
        let base = spawn_mock(
            200,
            r#"{"reason":"who knows","terminal_sessions":{"count":0}}"#,
        )
        .await;
        assert!(matches!(
            probe_at(&base).await,
            Readiness::Unknown(ReadinessUnknown::Unparseable { .. })
        ));
    }

    #[tokio::test]
    async fn probe_tolerates_unknown_extra_fields_and_missing_optional_blocks() {
        let base = spawn_mock(
            200,
            r#"{"safe_to_restart": false, "reason": "x", "some_future_field": 42}"#,
        )
        .await;
        let Readiness::Unsafe { report, .. } = probe_at(&base).await else {
            panic!("expected Unsafe");
        };
        assert_eq!(report.terminal_count(), 0);
        assert!(!report.drain_would_help());
    }

    #[tokio::test]
    async fn probe_body_excerpt_is_bounded() {
        // Leaked as a &'static str via Box::leak so the mock router can own it.
        let huge: &'static str = Box::leak(("x".repeat(50_000)).into_boxed_str());
        let base = spawn_mock(200, huge).await;
        let Readiness::Unknown(ReadinessUnknown::Unparseable { body_excerpt, .. }) =
            probe_at(&base).await
        else {
            panic!("expected Unparseable");
        };
        assert!(body_excerpt.chars().count() <= BODY_EXCERPT_CHARS);
    }

    // ── decide: refuse / override / allow ───────────────────────────────────

    #[tokio::test]
    async fn unsafe_without_force_is_refused() {
        let base = spawn_mock(200, UNSAFE_TERMINAL).await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(detail) = decide_stop(false, &r) else {
            panic!("expected Refused");
        };
        assert_eq!(detail.cause, "sessions_live");
        assert_eq!(detail.payload["would_be_lost"]["terminal_sessions"], 25);
        assert_eq!(detail.payload["error"], "restart_refused_unsafe");
        // The refusal carries the verdict that produced it.
        assert_eq!(detail.payload["readiness"]["safe_to_restart"], false);
    }

    #[tokio::test]
    async fn unsafe_with_force_is_overridden_and_carries_the_payload() {
        let base = spawn_mock(200, UNSAFE_TERMINAL).await;
        let r = probe_at(&base).await;
        let GateDecision::Overridden(detail) = decide_stop(true, &r) else {
            panic!("expected Overridden");
        };
        // The override is logged with the SAME payload the refusal would have
        // carried, so a log search finds both.
        assert_eq!(detail.cause, "sessions_live");
        assert_eq!(detail.payload["would_be_lost"]["terminal_sessions"], 25);
    }

    #[tokio::test]
    async fn safe_proceeds_with_or_without_force() {
        let base = spawn_mock(200, SAFE).await;
        let r = probe_at(&base).await;
        assert!(matches!(decide_stop(false, &r), GateDecision::Allowed));
        assert!(matches!(decide_stop(true, &r), GateDecision::Allowed));
    }

    #[tokio::test]
    async fn unreachable_is_refused_without_force() {
        let base = dead_url().await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(detail) = decide_stop(false, &r) else {
            panic!("expected Refused — UNKNOWN must never resolve to safe");
        };
        assert_eq!(detail.cause, "readiness_unreachable");
        assert_eq!(detail.payload["error"], "restart_refused_unknown");
        // Never a fabricated zero.
        assert!(detail.payload["would_be_lost"].is_null());
    }

    #[tokio::test]
    async fn unreachable_with_force_proceeds() {
        let base = dead_url().await;
        let r = probe_at(&base).await;
        assert!(matches!(decide_stop(true, &r), GateDecision::Overridden(_)));
    }

    #[tokio::test]
    async fn endpoint_absent_404_says_the_runner_predates_the_endpoint() {
        let base = spawn_mock(404, "not found").await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(detail) = decide_stop(false, &r) else {
            panic!("expected Refused");
        };
        assert_eq!(detail.cause, "readiness_endpoint_absent");
        // The distinct message an operator needs to tell "old runner" from
        // "runner is busy".
        assert!(
            detail.message.contains("PREDATES"),
            "message was: {}",
            detail.message
        );
        assert!(detail.message.contains("OLD RUNNER"));
        // And it must NOT read as a busy-runner refusal.
        assert!(!detail.message.contains("terminal-hosted agent session"));
    }

    #[tokio::test]
    async fn error_status_and_unparseable_are_refused_distinctly() {
        let five_hundred = probe_at(&spawn_mock(500, "boom").await).await;
        let garbage = probe_at(&spawn_mock(200, "<html/>").await).await;
        let GateDecision::Refused(a) = decide_stop(false, &five_hundred) else {
            panic!("expected Refused");
        };
        let GateDecision::Refused(b) = decide_stop(false, &garbage) else {
            panic!("expected Refused");
        };
        assert_eq!(a.cause, "readiness_error_status");
        assert_eq!(b.cause, "readiness_unparseable");
        assert_ne!(a.message, b.message);
    }

    // ── D3: the refusal must never recommend a drain that no-ops ────────────

    #[tokio::test]
    async fn terminal_sessions_refusal_never_recommends_a_drain() {
        let base = spawn_mock(200, UNSAFE_TERMINAL).await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(detail) = decide_stop(false, &r) else {
            panic!("expected Refused");
        };
        let m = &detail.message;
        assert!(m.contains("DESTROYS 25 live terminal-hosted agent session"));
        assert!(m.contains("NO graceful path"));
        assert!(m.contains("fast no-op"));
        assert!(m.contains("Do not drain and retry"), "message was: {m}");
        // The regression assertion from the plan's Verification section: the
        // refusal must not read as an instruction to drain first.
        for forbidden in ["run POST /drain first", "drain first", "then restart"] {
            assert!(
                !m.to_lowercase().contains(&forbidden.to_lowercase()),
                "refusal recommended a drain that would no-op: {m}"
            );
        }
        assert_eq!(detail.payload["drain_would_help"], false);
    }

    #[tokio::test]
    async fn ai_only_refusal_may_name_the_drain_because_it_covers_that_plane() {
        let base = spawn_mock(200, UNSAFE_AI_ONLY).await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(detail) = decide_stop(false, &r) else {
            panic!("expected Refused");
        };
        let m = &detail.message;
        assert!(m.contains("/drain"), "message was: {m}");
        assert!(m.contains("capture 2 of"), "message was: {m}");
        assert!(m.contains("TERMINAL"), "a drain is terminal — say so: {m}");
        // No terminal-plane sessions, so no destroyed-work sentence.
        assert!(!m.contains("DESTROYS"));
        assert_eq!(detail.payload["drain_would_help"], true);
    }

    #[tokio::test]
    async fn unsafe_with_no_reported_sessions_does_not_fabricate_a_count() {
        let base = spawn_mock(
            200,
            r#"{"safe_to_restart": false,
                "reason": "process table snapshot unavailable; cannot enumerate sessions"}"#,
        )
        .await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(detail) = decide_stop(false, &r) else {
            panic!("expected Refused");
        };
        assert!(detail.message.contains("fail-closed path"));
        assert!(detail
            .message
            .contains("process table snapshot unavailable"));
        assert!(!detail.message.contains("DESTROYS"));
    }

    #[tokio::test]
    async fn restart_and_stop_messages_name_their_own_action() {
        let base = spawn_mock(200, UNSAFE_TERMINAL).await;
        let r = probe_at(&base).await;
        let GateDecision::Refused(stop) =
            decide("primary", "Primary", 9876, GateAction::Stop, false, &r)
        else {
            panic!("expected Refused");
        };
        let GateDecision::Refused(restart) =
            decide("primary", "Primary", 9876, GateAction::Restart, false, &r)
        else {
            panic!("expected Refused");
        };
        assert!(stop.message.contains("Stopping it now"));
        assert!(restart.message.contains("Restarting it now"));
        assert_eq!(stop.payload["action"], "stop");
        assert_eq!(restart.payload["action"], "restart");
    }

    // ── Phase 2: the census fallback ────────────────────────────────────────

    use crate::process::subtree_census::{CensusEntry, ClaudeCensus, PidReuseGuard, RootSource};

    fn census(live: &[u32]) -> ClaudeCensus {
        ClaudeCensus {
            root_pid: 4242,
            root_source: RootSource::Registry,
            walked: 9,
            live: live
                .iter()
                .map(|pid| CensusEntry {
                    pid: *pid,
                    name: "claude".to_string(),
                    exe: Some(std::path::PathBuf::from("/usr/bin/claude")),
                })
                .collect(),
            pid_reuse_guard: PidReuseGuard::Checked,
        }
    }

    fn wedged() -> RunnerLiveness {
        RunnerLiveness::UnresponsiveSince(chrono::Utc::now())
    }

    /// (vii) The four-way exemption table, without any state.
    #[test]
    fn decide_skip_covers_the_four_way_table() {
        // Temp wins over everything, including protection.
        assert_eq!(
            decide_skip(wedged(), true, true, true, true),
            Some(SkipReason::TempRunner)
        );
        // Unprotected is next.
        assert_eq!(
            decide_skip(wedged(), true, true, false, false),
            Some(SkipReason::NotProtected)
        );
        // Positive evidence of absence.
        assert_eq!(
            decide_skip(RunnerLiveness::Stopped, false, false, false, true),
            Some(SkipReason::NotRunning)
        );
        // Never answered, port closed, nothing tracked: still exempt.
        assert_eq!(
            decide_skip(RunnerLiveness::Unknown, false, false, false, true),
            Some(SkipReason::NotRunning)
        );
        // THE REGRESSION THIS PHASE EXISTS TO CLOSE: a wedge is `running:
        // false, api_responding: false` for a user-managed runner, and the old
        // pair read that as "nothing to protect" and skipped the gate
        // entirely. It must now probe.
        assert_eq!(decide_skip(wedged(), true, false, false, true), None);
        // Never answered but HOLDING the port: also probes.
        assert_eq!(
            decide_skip(RunnerLiveness::Unknown, true, false, false, true),
            None
        );
        // Responding: probes, obviously.
        assert_eq!(
            decide_skip(RunnerLiveness::Responding, true, true, false, true),
            None
        );
    }

    /// (iv)(v)(vi) The census is consulted ONLY for an unreachable probe on a
    /// runner the supervisor has classified as wedged.
    #[tokio::test]
    async fn census_applies_only_to_an_unreachable_probe_on_a_wedged_runner() {
        let unreachable = probe_at(&dead_url().await).await;
        assert!(matches!(
            unreachable,
            Readiness::Unknown(ReadinessUnknown::Unreachable { .. })
        ));

        // (i)/(ii) shape: wedged + unreachable → consult.
        assert!(census_applies(&unreachable, wedged()));
        // (iv) The runner is answering elsewhere; a single failed probe is not
        // a wedge and must not be second-guessed by a process walk.
        assert!(!census_applies(&unreachable, RunnerLiveness::Responding));
        // (v) Design decision 4: `Unknown` never reaches the census.
        assert!(!census_applies(&unreachable, RunnerLiveness::Unknown));
        assert!(!census_applies(&unreachable, RunnerLiveness::Stopped));

        // (vi) A runner that ANSWERED — 404, non-2xx, or garbage — has a live
        // door, and its answer stands.
        let absent = probe_at(&spawn_mock(404, "not found").await).await;
        assert!(matches!(
            absent,
            Readiness::Unknown(ReadinessUnknown::EndpointAbsent)
        ));
        assert!(!census_applies(&absent, wedged()));
        let garbage = probe_at(&spawn_mock(200, "<html>nope</html>").await).await;
        assert!(!census_applies(&garbage, wedged()));
        let error = probe_at(&spawn_mock(503, "{}").await).await;
        assert!(!census_applies(&error, wedged()));
    }

    /// (i) The census answers `Idle` → the gate ALLOWS what it used to refuse
    /// forever, and says which arm answered.
    #[test]
    fn an_idle_census_allows_and_names_its_source() {
        let folded = fold_census(
            Readiness::Unknown(ReadinessUnknown::Unreachable {
                detail: "connection refused".to_string(),
            }),
            Some(Ok(census(&[]))),
        );
        assert!(matches!(folded, Readiness::CensusSafe { .. }));
        assert_eq!(folded.source(), ReadinessSource::SupervisorCensus);
        assert!(matches!(
            decide(
                "primary",
                "Primary",
                9876,
                GateAction::Restart,
                false,
                &folded
            ),
            GateDecision::Allowed
        ));
    }

    /// (iii) A busy census refuses, and the refusal carries the PIDs, names the
    /// census as the source, and still offers the documented override.
    #[test]
    fn a_busy_census_refuses_with_the_pids_it_counted() {
        let folded = fold_census(
            Readiness::Unknown(ReadinessUnknown::Unreachable {
                detail: "connection refused".to_string(),
            }),
            Some(Ok(census(&[1111, 2222]))),
        );
        let GateDecision::Refused(detail) = decide(
            "primary",
            "Primary",
            9876,
            GateAction::Restart,
            false,
            &folded,
        ) else {
            panic!("a busy census must refuse");
        };
        assert_eq!(detail.cause, "sessions_live");
        assert!(detail.message.contains("1111, 2222"), "{}", detail.message);
        assert!(
            detail
                .message
                .contains("process-subtree census because the runner's"),
            "the refusal must say where the count came from: {}",
            detail.message
        );
        assert!(detail.message.contains("{\"force\": true}"));
        assert_eq!(detail.payload["source"], "supervisor_census");
        assert_eq!(detail.payload["census"]["verdict"], "busy");
        assert_eq!(detail.payload["census"]["live_claude"][1]["pid"], 2222);
        // The runner said nothing here; the payload must not imply it did.
        assert!(detail.payload["readiness"].is_null());
    }

    /// Both sources failing is UNKNOWN, and UNKNOWN refuses.
    #[test]
    fn an_unreadable_census_refuses_and_carries_the_census_error_code() {
        let folded = fold_census(
            Readiness::Unknown(ReadinessUnknown::Unreachable {
                detail: "connection refused".to_string(),
            }),
            Some(Err(
                crate::process::subtree_census::CensusError::Unreadable(
                    "process table unavailable".to_string(),
                ),
            )),
        );
        assert_eq!(folded.source(), ReadinessSource::SupervisorCensus);
        let GateDecision::Refused(detail) =
            decide("primary", "Primary", 9876, GateAction::Stop, false, &folded)
        else {
            panic!("an unreadable census must refuse");
        };
        assert_eq!(detail.cause, "readiness_census_unreadable");
        assert!(detail
            .message
            .contains("BOTH readiness sources have failed"));
        assert!(
            detail.message.contains("census_unreadable"),
            "the census error's own code must survive into the refusal: {}",
            detail.message
        );
    }

    /// `None` (the census was not consulted) passes the probe result straight
    /// through — the no-change path for every non-wedge caller.
    #[test]
    fn no_census_passes_the_probe_result_through() {
        let folded = fold_census(Readiness::Unknown(ReadinessUnknown::EndpointAbsent), None);
        assert!(matches!(
            folded,
            Readiness::Unknown(ReadinessUnknown::EndpointAbsent)
        ));
        assert_eq!(folded.source(), ReadinessSource::RunnerVerdict);
    }

    /// (i) End to end, phase-1 wedge signature: the port REFUSES connections
    /// and the census walks this very test process, which hosts no `claude`.
    #[tokio::test]
    async fn phase_one_signature_refused_port_falls_back_to_a_real_census() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let r = probe_with_census_fallback(
            port,
            wedged(),
            Some(std::process::id()),
            // The reference instant is the runner's own `started_at`, so a
            // healthy root's start time never postdates it. `Utc::now()` is
            // the loosest honest reference for this test process.
            Some(chrono::Utc::now()),
        )
        .await;

        let Readiness::CensusSafe { census } = &r else {
            panic!("expected a real census to answer for a refused port, got {r:?}");
        };
        assert_eq!(census.root_pid, std::process::id());
        assert!(census.walked >= 1);
        assert!(matches!(
            decide("primary", "Primary", port, GateAction::Restart, false, &r),
            GateDecision::Allowed
        ));
    }

    /// (ii) End to end, phase-2 wedge signature: connections are ACCEPTED and
    /// never answered. The probe must time out into `Unreachable` (not hang,
    /// and not be mistaken for a live door) and the census must answer.
    #[tokio::test]
    async fn phase_two_signature_accept_and_hang_falls_back_to_a_real_census() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and hold every connection open forever, answering nothing.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        let base = format!("http://127.0.0.1:{}", port);
        let probed = probe_at_with_timeout(&base, Duration::from_millis(300)).await;
        assert!(
            matches!(
                probed,
                Readiness::Unknown(ReadinessUnknown::Unreachable { .. })
            ),
            "an accepted-but-unanswered connection must time out into Unreachable, got {probed:?}"
        );
        assert!(census_applies(&probed, wedged()));

        let folded = fold_census(
            probed,
            Some(
                crate::process::subtree_census::take_census_for_port(
                    Some(std::process::id()),
                    port,
                    None,
                )
                .await,
            ),
        );
        assert!(
            matches!(folded, Readiness::CensusSafe { .. }),
            "this test process hosts no `claude`, so the census is idle: {folded:?}"
        );
    }

    /// The allow path logs the census verdict with the same `verdict=` suffix
    /// a runner allow uses, and names the arm that answered.
    #[tokio::test]
    async fn enforce_allows_a_wedged_idle_runner_and_logs_the_census() {
        let state = enforce_state();
        let (managed, port) = managed_on_dead_port(
            "primary",
            qontinui_types::wire::runner_kind::RunnerKind::Primary,
            true,
            false,
        )
        .await;
        {
            // The wedge signature: port held, API silent, previously seen
            // responding, and a PID the census can root on.
            let mut h = managed.cached_health.write().await;
            h.runner_port_open = true;
            h.runner_responding = false;
        }
        {
            let mut r = managed.runner.write().await;
            r.last_seen_responding_at = Some(chrono::Utc::now() - chrono::Duration::minutes(30));
            r.pid = Some(std::process::id());
            r.started_at = Some(chrono::Utc::now());
        }
        assert!(port > 0);

        enforce(&state, &managed, GateAction::Restart, false)
            .await
            .expect("a wedged runner with a zero census must be allowed to restart");

        let logs = state.logs.history().await;
        let line = logs
            .iter()
            .find(|e| e.message.contains("source=supervisor_census"))
            .expect("the census allow must be on the operator log buffer");
        assert!(line.message.contains("verdict="), "{}", line.message);
        assert!(line.message.contains("ZERO live"), "{}", line.message);
    }

    #[test]
    fn skip_reasons_have_stable_codes() {
        // These land in logs and in the `skipped` breadcrumb; keep them stable.
        assert_eq!(SkipReason::TempRunner.as_str(), "temp_runner");
        assert_eq!(SkipReason::NotProtected.as_str(), "runner_not_protected");
        assert_eq!(SkipReason::NotRunning.as_str(), "runner_not_running");
    }
    // ── enforce(): the wiring, against a real SharedState ───────────────────
    //
    // These drive the function `manager::stop_runner_by_id` and
    // `manager::restart_runner_by_id` actually call, so they cover the three
    // exemptions and the refusal→SupervisorError mapping — not just `decide`.

    fn enforce_state() -> crate::state::SharedState {
        use crate::config::{CliArgs, SupervisorConfig};
        use clap::Parser;
        let args = CliArgs::parse_from(["test", "--project-dir", "."]);
        std::sync::Arc::new(crate::state::SupervisorState::new(
            SupervisorConfig::from_args(args),
        ))
    }

    /// A registered runner of the given id/kind on a port nothing listens on,
    /// so the readiness probe is guaranteed to come back UNKNOWN unless an
    /// exemption short-circuits it first.
    async fn managed_on_dead_port(
        id: &str,
        kind: qontinui_types::wire::runner_kind::RunnerKind,
        protected: bool,
        running: bool,
    ) -> (crate::state::ManagedRunner, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = crate::config::RunnerConfig {
            id: id.to_string(),
            name: format!("Runner {id}"),
            port,
            kind,
            protected,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: Default::default(),
        };
        let managed = crate::state::ManagedRunner::new(config, false);
        managed.runner.write().await.running = running;
        (managed, port)
    }

    #[tokio::test]
    async fn enforce_refuses_a_live_protected_runner_whose_readiness_is_unreachable() {
        let state = enforce_state();
        let (managed, _port) = managed_on_dead_port(
            "primary",
            qontinui_types::wire::runner_kind::RunnerKind::Primary,
            true,
            true,
        )
        .await;

        let err = enforce(&state, &managed, GateAction::Stop, false)
            .await
            .expect_err("an unreachable verdict must refuse, never resolve to safe");

        let crate::error::SupervisorError::RestartUnsafe(detail) = &err else {
            panic!("expected RestartUnsafe, got {err:?}");
        };
        assert_eq!(detail.cause, "readiness_unreachable");

        // 409 with the typed body the caller branches on.
        let (status, body) = err.to_status_body();
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert_eq!(body["error"], "restart_refused_unknown");
        assert_eq!(body["action"], "stop");
        assert_eq!(body["runner_id"], "primary");
    }

    #[tokio::test]
    async fn enforce_lets_force_through_and_reports_the_same_action_it_was_asked_about() {
        let state = enforce_state();
        let (managed, _) = managed_on_dead_port(
            "primary",
            qontinui_types::wire::runner_kind::RunnerKind::Primary,
            true,
            true,
        )
        .await;

        enforce(&state, &managed, GateAction::Restart, true)
            .await
            .expect("force:true must proceed");

        // The override is on the operator-visible log buffer, not just tracing.
        let logs = state.logs.history().await;
        let line = logs
            .iter()
            .find(|e| e.message.contains("restart-readiness OVERRIDE"))
            .expect("an override must be logged");
        assert!(
            line.message.contains("verdict="),
            "the override must carry the verdict it overrode: {}",
            line.message
        );
        assert!(matches!(line.level, crate::log_capture::LogLevel::Error));
    }

    #[tokio::test]
    async fn enforce_exempts_temp_runners_so_the_spawn_test_lifecycle_still_works() {
        // A temp runner on a dead port. Without the exemption this is exactly
        // the UNKNOWN refusal above — which would leak a zombie on every
        // failed spawn-test and wedge the max-age reaper.
        let state = enforce_state();
        let id = "test-18f2a3b4-1";
        let (managed, _) = managed_on_dead_port(
            id,
            qontinui_types::wire::runner_kind::RunnerKind::Temp { id: id.to_string() },
            true, // every temp runner is minted protected — the exemption must
            true, // not be keyed on protection.
        )
        .await;
        assert!(
            crate::process::manager::is_temp_runner(id),
            "test id must actually classify as temp"
        );

        enforce(&state, &managed, GateAction::Stop, false)
            .await
            .expect("temp runners are exempt from the readiness gate");
    }

    #[tokio::test]
    async fn enforce_exempts_an_explicitly_unprotected_runner() {
        let state = enforce_state();
        let (managed, _) = managed_on_dead_port(
            "named-9880-abc",
            qontinui_types::wire::runner_kind::RunnerKind::Named {
                name: "sandbox".to_string(),
            },
            false, // POST /runners/{id}/protect {"protected": false}
            true,
        )
        .await;

        enforce(&state, &managed, GateAction::Stop, false)
            .await
            .expect("an unprotected runner is not gated");
    }

    #[tokio::test]
    async fn enforce_exempts_a_runner_with_no_evidence_of_life() {
        let state = enforce_state();
        let (managed, _) = managed_on_dead_port(
            "named-9881-abc",
            qontinui_types::wire::runner_kind::RunnerKind::Named {
                name: "sandbox".to_string(),
            },
            true,
            false, // not tracked running, and cached_health defaults to
                   // runner_responding: false
        )
        .await;

        enforce(&state, &managed, GateAction::Stop, false)
            .await
            .expect("there is no in-flight work to protect on a dead runner");
    }

    #[tokio::test]
    async fn enforce_refuses_and_logs_when_the_runner_reports_live_terminal_sessions() {
        // End-to-end through a real HTTP server: the runner says unsafe, the
        // gate refuses, and the operator log carries the verdict.
        let state = enforce_state();
        let app = Router::new().route(
            READINESS_PATH,
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    UNSAFE_TERMINAL,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let config = crate::config::RunnerConfig {
            id: "primary".to_string(),
            name: "Primary".to_string(),
            port,
            kind: qontinui_types::wire::runner_kind::RunnerKind::Primary,
            protected: true,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: Default::default(),
        };
        let managed = crate::state::ManagedRunner::new(config, false);
        managed.runner.write().await.running = true;

        let err = enforce(&state, &managed, GateAction::Restart, false)
            .await
            .expect_err("25 live sessions must refuse");
        let (status, body) = err.to_status_body();
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
        assert_eq!(body["error"], "restart_refused_unsafe");
        assert_eq!(body["would_be_lost"]["terminal_sessions"], 25);
        assert_eq!(body["drain_would_help"], false);
        assert_eq!(body["readiness"]["safe_to_restart"], false);

        let logs = state.logs.history().await;
        let line = logs
            .iter()
            .find(|e| e.message.contains("restart-readiness REFUSED"))
            .expect("a refusal must be logged");
        assert!(line.message.contains("verdict="));
        assert!(matches!(line.level, crate::log_capture::LogLevel::Warn));
    }

    #[tokio::test]
    async fn enforce_allows_and_logs_the_verdict_that_permitted_it() {
        let state = enforce_state();
        let app = Router::new().route(
            READINESS_PATH,
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    SAFE,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let config = crate::config::RunnerConfig {
            id: "primary".to_string(),
            name: "Primary".to_string(),
            port,
            kind: qontinui_types::wire::runner_kind::RunnerKind::Primary,
            protected: true,
            server_mode: false,
            restate_ingress_port: None,
            restate_admin_port: None,
            restate_service_port: None,
            external_restate_admin_url: None,
            external_restate_ingress_url: None,
            extra_env: Default::default(),
        };
        let managed = crate::state::ManagedRunner::new(config, false);
        managed.runner.write().await.running = true;

        enforce(&state, &managed, GateAction::Stop, false)
            .await
            .expect("a safe verdict proceeds");

        let logs = state.logs.history().await;
        let line = logs
            .iter()
            .find(|e| e.message.contains("safe_to_restart=true"))
            .expect("the allow path must be auditable too");
        assert!(line.message.contains("verdict="));
    }
    // ---- drained_for_deferred_rebuild -------------------------------------
    // The predicate behind POST /runner/rebuild-when-idle. Its whole contract
    // is the asymmetry: `true` demands positive evidence of emptiness, and
    // every other state — including every Unknown — is `false`.

    fn report_json(safe: bool, terminal: u64, ai: u64) -> Readiness {
        // Built through the REAL deserializer, so these fixtures cannot drift
        // from the wire shape the runner actually sends.
        let raw = serde_json::json!({
            "safe_to_restart": safe,
            "terminal_sessions": { "count": terminal },
            "ai_sessions": { "count": ai },
        });
        let report: ReadinessReport = serde_json::from_value(raw.clone()).unwrap();
        if safe {
            Readiness::Safe { report, raw }
        } else {
            Readiness::Unsafe { report, raw }
        }
    }

    #[test]
    fn drained_only_when_both_planes_are_zero() {
        assert!(drained_for_deferred_rebuild(&report_json(true, 0, 0)).drained);
        assert!(!drained_for_deferred_rebuild(&report_json(true, 1, 0)).drained);
        assert!(!drained_for_deferred_rebuild(&report_json(true, 0, 1)).drained);
        assert!(!drained_for_deferred_rebuild(&report_json(true, 2, 3)).drained);
    }

    #[test]
    fn an_unsafe_verdict_is_never_drained_even_with_zero_counts() {
        // safe_to_restart=false with empty planes is contradictory; the gate
        // must not resolve that contradiction in favour of acting.
        assert!(!drained_for_deferred_rebuild(&report_json(false, 0, 0)).drained);
    }

    #[test]
    fn census_safe_is_drained_and_census_unsafe_is_not() {
        assert!(
            drained_for_deferred_rebuild(&Readiness::CensusSafe {
                census: census(&[])
            })
            .drained
        );
        assert!(
            !drained_for_deferred_rebuild(&Readiness::CensusUnsafe {
                census: census(&[4321])
            })
            .drained
        );
    }

    #[test]
    fn every_unknown_refuses_to_call_it_drained() {
        // The arm that decides whether the endpoint is safe at all. If a new
        // ReadinessUnknown variant is added, add it here.
        let unknowns = vec![
            ReadinessUnknown::EndpointAbsent,
            ReadinessUnknown::Unreachable {
                detail: "connection refused".into(),
            },
            ReadinessUnknown::ErrorStatus {
                status: 500,
                body_excerpt: "boom".into(),
            },
            ReadinessUnknown::Unparseable {
                detail: "missing field `safe_to_restart`".into(),
                body_excerpt: "{}".into(),
            },
            // Both sources failed. This is the one an operator is most likely
            // to hit on a wedged runner, and the one it would be most tempting
            // to read as "quiet, therefore idle".
            ReadinessUnknown::CensusUnreadable {
                detail: "census_root_pid_absent".into(),
            },
        ];
        for u in unknowns {
            let v = drained_for_deferred_rebuild(&Readiness::Unknown(u.clone()));
            assert!(!v.drained, "Unknown({:?}) must not be drained", u);
            assert!(
                v.reason.contains("unknown"),
                "reason should name the unknown: {}",
                v.reason
            );
        }
    }
}
