use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::{interval, Duration};
use tracing::debug;

use crate::log_capture::{LogLevel, LogSource};
use crate::process::manager::is_temp_runner;
use crate::process::port;
use crate::state::{RunnerLiveness, SupervisorState};
use qontinui_types::wire::runner_kind::RunnerKind;

/// Returns true if this runner is a named runner managed by the supervisor.
fn is_named_runner(runner_id: &str) -> bool {
    runner_id.starts_with("named-")
}

/// Derived health classification for a runner surfaced to dashboard consumers.
///
/// This is the supervisor-side view: it reflects both process liveness (is the
/// OS process + port alive?) and application-level signals from the runner's
/// own `/health` body (e.g. `ui_error`, `derived_status`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunnerStatus {
    Healthy,
    Degraded {
        reason: String,
    },
    Errored {
        reason: String,
    },
    #[default]
    Offline,
    Starting,
}

/// Snapshot of a UI-level runtime error reported by the runner's `/health`
/// endpoint. Mirrors the `ui_error` object the runner emits when its React
/// error boundary catches a crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiErrorSummary {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_stack: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub reported_at: DateTime<Utc>,
    pub count: u32,
}

/// Snapshot of the most recent Rust crash dump the runner found on startup.
/// Mirrors the runner's `RecentCrash` (camelCase on the wire) so
/// `fetch_runner_health_body` can deserialize the field directly. Non-unwinding
/// panics abort the process across the WebView2 FFI boundary and bypass the
/// React error boundary entirely, so this is the only way fleet consumers see
/// that a runner was just force-restarted after a Rust crash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentCrashSummary {
    pub file_path: String,
    pub reported_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panic_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

/// Raw /health response body shape we care about. Uses `serde(default)` so
/// older runners (without `ui_error` / `derived_status` / `recent_crash`) still
/// parse cleanly.
#[derive(Debug, Default, Deserialize)]
struct RunnerHealthBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    derived_status: Option<String>,
    #[serde(default)]
    ui_error: Option<UiErrorSummary>,
    #[serde(default)]
    recent_crash: Option<RecentCrashSummary>,
    /// `/health.embeddingService` — one of the inputs that can produce
    /// `derived_status: "degraded"`. Top-level form; the runner actually
    /// publishes it under `data` (see [`RunnerHealthData`]) — read through
    /// [`RunnerHealthBody::embedding_service`].
    #[serde(default, rename = "embeddingService")]
    embedding_service: Option<ReachableFlag>,
    /// `/health.database` — bounded PG liveness, another degraded input.
    #[serde(default)]
    database: Option<ReachableFlag>,
    /// `/health.webIntegration` — the backend WS relay. `connected: null`
    /// means no relay is EXPECTED (tier below qontinui_account, or
    /// web-integration disabled), which is not a fault.
    #[serde(default, rename = "webIntegration")]
    web_integration: Option<WebIntegrationSummary>,
    /// `/health.frontendReady` — whether the runner's WebView frontend mounted
    /// and is answering. A runner whose binary embedded the dev PLACEHOLDER
    /// frontend reports `false` here while its API answers 2xx (plan
    /// `2026-10-05-supervisor-first-start-embeds-placeholder-frontend`).
    /// Read here AND under `data` (see [`RunnerHealthData`]): the runner
    /// publishes it only inside its `data` envelope today.
    #[serde(default, rename = "frontendReady")]
    frontend_ready: Option<bool>,
    /// `/health.frontendState` — WHY `frontendReady` is what it is
    /// (e.g. `window_not_visible`). Same two locations as `frontend_ready`.
    #[serde(default, rename = "frontendState")]
    frontend_state: Option<String>,
    /// The runner's `data` envelope. Only the fields this supervisor reads
    /// from it are modelled; everything else is ignored.
    #[serde(default)]
    data: Option<RunnerHealthData>,
}

/// The subset of the runner's `/health.data` envelope read here.
///
/// The runner publishes every one of these ONLY inside `data` (qontinui-runner
/// `src-tauri/src/mcp_api.rs`, the `data` object built at :1428 and wrapped at
/// :1901, as of origin/main b58a81dce); its top level mirrors only
/// `derived_status` / `ui_error` / `recent_crash`. Reading them at the top level
/// alone meant `degraded_reason` never named a cause on a real runner.
#[derive(Debug, Default, Deserialize)]
struct RunnerHealthData {
    #[serde(default, rename = "frontendReady")]
    frontend_ready: Option<bool>,
    #[serde(default, rename = "frontendState")]
    frontend_state: Option<String>,
    #[serde(default, rename = "embeddingService")]
    embedding_service: Option<ReachableFlag>,
    #[serde(default)]
    database: Option<ReachableFlag>,
    #[serde(default, rename = "webIntegration")]
    web_integration: Option<WebIntegrationSummary>,
}

impl RunnerHealthBody {
    /// `embeddingService`, preferring a top-level block over the `data` one.
    fn embedding_service(&self) -> Option<&ReachableFlag> {
        self.embedding_service.as_ref().or_else(|| {
            self.data
                .as_ref()
                .and_then(|d| d.embedding_service.as_ref())
        })
    }

    /// `database`, preferring a top-level block over the `data` one.
    fn database(&self) -> Option<&ReachableFlag> {
        self.database
            .as_ref()
            .or_else(|| self.data.as_ref().and_then(|d| d.database.as_ref()))
    }

    /// `webIntegration`, preferring a top-level block over the `data` one.
    fn web_integration(&self) -> Option<&WebIntegrationSummary> {
        self.web_integration
            .as_ref()
            .or_else(|| self.data.as_ref().and_then(|d| d.web_integration.as_ref()))
    }

    /// `frontendReady`, preferring a top-level value over the `data` one.
    fn frontend_ready(&self) -> Option<bool> {
        self.frontend_ready
            .or_else(|| self.data.as_ref().and_then(|d| d.frontend_ready))
    }

    /// `frontendState`, preferring a top-level value over the `data` one.
    fn frontend_state(&self) -> Option<&str> {
        self.frontend_state
            .as_deref()
            .or_else(|| self.data.as_ref().and_then(|d| d.frontend_state.as_deref()))
    }
}

/// The reason text for a runner that self-reported `derived_status: "errored"`.
///
/// A bare "derived_status=errored" hid the one fact that mattered on
/// 2026-10-05: the runner was serving a PLACEHOLDER frontend
/// (`frontendReady: false`, `frontendState: "window_not_visible"`). When the
/// body says the frontend is not ready, name it. This changes only the reason,
/// never which status is derived.
fn errored_reason(body: &RunnerHealthBody) -> String {
    let base = "runner reported derived_status=errored";
    if body.frontend_ready() == Some(false) {
        return match body.frontend_state() {
            Some(st) if !st.trim().is_empty() => format!(
                "{base}; frontend not ready (frontendState={})",
                truncate_reason(st.trim(), 80)
            ),
            _ => format!("{base}; frontend not ready"),
        };
    }
    base.to_string()
}

/// A `{ "reachable": bool|null }` sub-object on `/health`.
#[derive(Debug, Deserialize)]
struct ReachableFlag {
    #[serde(default)]
    reachable: Option<bool>,
}

/// The `/health.webIntegration` block.
#[derive(Debug, Deserialize)]
struct WebIntegrationSummary {
    #[serde(default)]
    connected: Option<bool>,
    #[serde(default, rename = "lastError")]
    last_error: Option<String>,
}

/// Name WHICH subsystem is down, rather than restating the verdict.
///
/// `derived_status: "degraded"` is a fold over several independent inputs,
/// so "runner reported derived_status=degraded" told an operator only what
/// they already knew from the badge colour. A dead relay, an unreachable
/// embedding service and a dead data layer all rendered identically, and
/// the relay case is the one that is otherwise hardest to see: it costs the
/// runner every cloud client while local work keeps succeeding.
///
/// Every field is `serde(default)`, so a runner too old to publish these
/// blocks yields no causes and falls back to the original wording — no
/// regression for a mixed-version fleet.
fn degraded_reason(body: &RunnerHealthBody) -> String {
    let mut causes: Vec<String> = Vec::new();

    if body.embedding_service().and_then(|f| f.reachable) == Some(false) {
        causes.push("embedding service unreachable".to_string());
    }
    if body.database().and_then(|f| f.reachable) == Some(false) {
        causes.push("database unreachable".to_string());
    }
    if let Some(wi) = body.web_integration() {
        // Only `Some(false)` is a fault. `None` means no relay is expected.
        if wi.connected == Some(false) {
            causes.push(match wi.last_error.as_deref() {
                // Reuses the file's existing truncator so a relay
                // `last_error` cannot dominate the status line; the full text
                // stays on the runner's own `/web-integration/status`.
                Some(e) if !e.trim().is_empty() => {
                    format!("backend relay down ({})", truncate_reason(e.trim(), 120))
                }
                _ => "backend relay down".to_string(),
            });
        }
    }

    if causes.is_empty() {
        // Either an older runner, or a degraded input this supervisor build
        // does not know about. Say the verdict, and do NOT invent a cause.
        return "runner reported derived_status=degraded".to_string();
    }
    format!("runner degraded: {}", causes.join("; "))
}

#[derive(Clone, Debug, Default)]
pub struct CachedPortHealth {
    pub runner_port_open: bool,
    pub runner_responding: bool,
}

/// Cached per-runner health snapshot, built by the background refresher.
/// Readable via `try_read()` in sync contexts (e.g., SSE streams).
#[derive(Clone, Debug, serde::Serialize)]
pub struct CachedRunnerHealth {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub kind: RunnerKind,
    pub running: bool,
    pub pid: Option<u32>,
    pub api_responding: bool,
    /// Most recent UI-level error reported by the runner's `/health` body.
    /// `None` when the runner reports no error or when the field is missing
    /// (older runners predating Phase 3J.1).
    pub ui_error: Option<UiErrorSummary>,
    /// Most recent Rust crash dump surfaced by the runner's `/health` body.
    /// `None` when no fresh dump is on disk or the runner predates the
    /// crash-dump scanner (post-3J follow-up).
    pub recent_crash: Option<RecentCrashSummary>,
    /// Supervisor-derived status. Combines runner process state with the
    /// runner's own `derived_status` + `ui_error` + `recent_crash` signals.
    pub derived_status: RunnerStatus,
    /// Snapshot of the runner's crash-only watchdog state, so sync SSE
    /// consumers can surface live values without touching the per-runner
    /// `WatchdogState` lock.
    pub watchdog: crate::routes::health::WatchdogHealth,
    /// Three-state liveness, so consumers stop reading `running: false` as
    /// "gone" (Phase 3b).
    ///
    /// `running` + `pid` alone cannot distinguish a stopped runner from a
    /// wedged one, and on 2026-08-08 the dashboard asserted `running: false,
    /// pid: null` about a live process it could see holding the port in the
    /// same document. **Render this field, not `running`.**
    pub liveness: crate::state::RunnerLiveness,
    /// When the API was last seen responding — `None` if never seen.
    /// Gives "unresponsive since T" an actual T to show.
    pub last_seen_responding_at: Option<DateTime<Utc>>,
    /// Whether a listener held the runner's port on THIS tick's probe — the
    /// other half of the pair `liveness` is derived from. Carried so the
    /// sync SSE path can publish the derivation alongside the verdict without
    /// re-probing (it has no access to the per-runner `cached_health` lock).
    pub port_open: bool,
    /// The runner's own `/health` `frontendReady` on this tick. `None` =
    /// UNKNOWN (API not answering, body unparseable, or a runner too old to
    /// publish it) — never read as ready.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend_ready: Option<bool>,
    /// The runner's own `/health` `frontendState` on this tick (why
    /// `frontendReady` is what it is, e.g. `window_not_visible`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend_state: Option<String>,
    /// When this supervisor FIRST observed `frontendReady == false` in the
    /// current run for the current pid (UNKNOWN ticks pause the run rather than
    /// end it). `None` when there is no run. See [`next_frontend_run`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend_not_ready_since: Option<DateTime<Utc>>,
}

/// How long a run of `frontendReady == false` observations survives a
/// continuous stretch of UNKNOWN ticks (no parseable `/health` body, or a body
/// with no `frontendReady`) before it is dropped.
///
/// UNKNOWN must not END a run on its own: the body fetch has a 3 s timeout and
/// this fleet has measured the runner's `/health` at up to ~10 s under load, so
/// a runner that is really stuck on a placeholder frontend would otherwise be
/// reset by every slow read and never reach the grace. 60 s is 30 refresher
/// ticks (2 s each) and six times the slowest measured `/health` — long enough
/// to ride out load, short enough that a run is not carried across a
/// genuinely unobserved minute. The duration reported still includes such
/// gaps; nothing in them is evidence of readiness.
pub(crate) const FRONTEND_RUN_UNKNOWN_GAP_CAP_SECS: i64 = 60;

/// One runner's current run of `frontendReady == false` observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrontendRun {
    /// First `false` observation of the run.
    pub since: DateTime<Utc>,
    /// The pid the run was observed on; a different pid starts over.
    pub pid: Option<u32>,
    /// Start of the current stretch of UNKNOWN ticks, if in one.
    pub unknown_since: Option<DateTime<Utc>>,
    /// The last `frontendState` a `false` observation carried, so an UNKNOWN
    /// tick can still name it.
    pub last_state: Option<String>,
}

/// Advance a runner's frontend run by one tick.
///
/// - `Some(false)`: continue the run (same pid) or start one at `now`
///   (no run, or a new pid = the runner was restarted); clears any UNKNOWN gap.
/// - `Some(true)`: the frontend is ready; the run ends.
/// - `None` (UNKNOWN): PAUSES the run. It survives unless the pid changed or
///   the UNKNOWN stretch has lasted [`FRONTEND_RUN_UNKNOWN_GAP_CAP_SECS`]. No
///   run is ever STARTED by an UNKNOWN tick.
pub(crate) fn next_frontend_run(
    prev: Option<&FrontendRun>,
    frontend_ready: Option<bool>,
    frontend_state: Option<&str>,
    pid: Option<u32>,
    now: DateTime<Utc>,
) -> Option<FrontendRun> {
    match frontend_ready {
        Some(true) => None,
        Some(false) => {
            let since = match prev {
                Some(r) if r.pid == pid => r.since,
                _ => now,
            };
            Some(FrontendRun {
                since,
                pid,
                unknown_since: None,
                last_state: frontend_state.map(str::to_string).or_else(|| {
                    prev.filter(|r| r.pid == pid)
                        .and_then(|r| r.last_state.clone())
                }),
            })
        }
        None => {
            let r = prev?;
            if r.pid != pid {
                return None;
            }
            let unknown_since = r.unknown_since.unwrap_or(now);
            if (now - unknown_since).num_seconds() >= FRONTEND_RUN_UNKNOWN_GAP_CAP_SECS {
                return None;
            }
            Some(FrontendRun {
                unknown_since: Some(unknown_since),
                ..r.clone()
            })
        }
    }
}

/// The refresher's per-runner map of [`FrontendRun`]s.
#[derive(Debug, Default)]
pub(crate) struct FrontendRunTracker {
    runs: std::collections::HashMap<String, FrontendRun>,
}

impl FrontendRunTracker {
    /// Feed one tick's observation for runner `id`; returns the run after it.
    pub(crate) fn observe(
        &mut self,
        id: &str,
        frontend_ready: Option<bool>,
        frontend_state: Option<&str>,
        pid: Option<u32>,
        now: DateTime<Utc>,
    ) -> Option<FrontendRun> {
        let next = next_frontend_run(self.runs.get(id), frontend_ready, frontend_state, pid, now);
        match &next {
            Some(run) => {
                self.runs.insert(id.to_string(), run.clone());
            }
            None => {
                self.runs.remove(id);
            }
        }
        next
    }

    /// Drop runs for runners no longer in the registry.
    pub(crate) fn retain_ids<'a>(&mut self, mut live: impl FnMut(&str) -> bool + 'a) {
        self.runs.retain(|id, _| live(id));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.runs.len()
    }
}

/// Truncate a string to at most `max_chars` chars, adding an ellipsis marker
/// if truncation occurred. Uses char boundaries, not byte boundaries, so it is
/// safe on multi-byte UTF-8 input.
fn truncate_reason(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Fetch `{runner_url}/health` with a 3-second timeout and parse the
/// runner's self-reported UI error + derived status. Returns `None` if the
/// endpoint is unreachable, the body fails to parse, or the request times
/// out — callers treat that as "no signal" and fall back to port-level state.
async fn fetch_runner_health_body(port: u16) -> Option<RunnerHealthBody> {
    let url = format!("http://127.0.0.1:{}/health", port);
    let client = reqwest::Client::builder()
        .timeout(StdDuration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<RunnerHealthBody>().await.ok()
}

/// Derive the supervisor's view of a runner's status from the signals we have.
///
/// Rules (in order):
/// 1. /health unreachable → `Offline` (if supervisor thinks process is dead)
///    or `Starting` (process exists but /health isn't responding yet).
/// 2. /health reachable with a `ui_error` → `Errored { reason }` where reason
///    is the ui_error message truncated to 200 chars.
/// 3. /health reachable with a `recent_crash` → `Errored { reason }` using
///    the crash's `panicMessage` (or a generic fallback if the message was
///    not captured). Non-unwinding Rust panics abort the process before the
///    React boundary sees them, so this is the only signal for that class.
/// 4. /health reachable, runner body reports `derived_status: "errored"` → `Errored`.
/// 5. /health reachable, no error signals, running=true → `Healthy`.
/// 6. Everything else → `Offline`.
fn derive_runner_status(
    running: bool,
    api_responding: bool,
    body: Option<&RunnerHealthBody>,
) -> RunnerStatus {
    if !api_responding {
        // Process may be alive (e.g. Tauri window launched, HTTP server still
        // warming) but /health doesn't respond yet. Treat that as Starting so
        // operators see a blue "spinning up" indicator instead of red.
        if running {
            return RunnerStatus::Starting;
        }
        return RunnerStatus::Offline;
    }

    if let Some(body) = body {
        if let Some(ui_err) = body.ui_error.as_ref() {
            return RunnerStatus::Errored {
                reason: truncate_reason(&ui_err.message, 200),
            };
        }
        if let Some(crash) = body.recent_crash.as_ref() {
            let reason = crash
                .panic_message
                .as_deref()
                .map(|m| truncate_reason(m, 200))
                .unwrap_or_else(|| {
                    "runner restarted after Rust panic (no message captured)".to_string()
                });
            return RunnerStatus::Errored { reason };
        }
        // Honour runner's own self-classification if present. The runner's
        // derived_status is an application-layer signal; we already handled
        // ui_error + recent_crash above, so here we surface `errored` (as a
        // belt-and-braces signal) and `degraded` (subsystem outage — e.g.
        // embedding service unreachable — where the runner is still
        // functional but operating in reduced capacity).
        if let Some(ds) = body.derived_status.as_deref() {
            if ds.eq_ignore_ascii_case("errored") {
                return RunnerStatus::Errored {
                    reason: errored_reason(body),
                };
            }
            if ds.eq_ignore_ascii_case("degraded") {
                return RunnerStatus::Degraded {
                    reason: degraded_reason(body),
                };
            }
        }
        // Runner reports `status: "starting"` during boot (legacy field kept
        // for backward compat with older runners). Surface that so the
        // dashboard can show a blue badge instead of green-too-soon.
        if body.status.as_deref() == Some("starting") {
            return RunnerStatus::Starting;
        }
    }

    if running {
        RunnerStatus::Healthy
    } else {
        // /health is responding but supervisor state says not running. This is
        // the "external runner" case (user-started). Treat as healthy since
        // the runner is clearly up and serving requests.
        RunnerStatus::Healthy
    }
}

/// Consecutive 2s ticks of "port held, API silent" before the supervisor
/// escalates.
///
/// 15 ticks ≈ 30 s. Long enough that a GC pause, a slow `/health`, or a
/// restart window does not page; short enough that a real wedge surfaces in
/// well under a minute instead of the 7 hours the 2026-08-08 incident took.
const UNRESPONSIVE_ESCALATION_TICKS: u32 = 15;

/// Re-escalate every this many further ticks while the condition persists
/// (≈5 min), so a long wedge stays visible without spamming every 2 s.
const UNRESPONSIVE_REESCALATION_TICKS: u32 = 150;

pub fn spawn_health_cache_refresher(state: Arc<SupervisorState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(2));
        let mut tick_count: u64 = 0;
        // 3e — consecutive "alive but not answering" ticks, per runner id.
        // Local to this loop on purpose: it is escalation bookkeeping, not
        // state any consumer should read.
        let mut unresponsive_ticks: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        // Per-runner run of `frontendReady == false` — see `next_frontend_run`.
        let mut frontend_runs = FrontendRunTracker::default();
        loop {
            // Wait for either the periodic tick or an immediate refresh notification
            tokio::select! {
                _ = ticker.tick() => {},
                _ = state.health_cache_notify.notified() => {
                    // Small delay to let state settle after a start/stop
                    tokio::time::sleep(Duration::from_millis(100)).await;
                },
            }

            // Refresh health for all managed runners
            let runners = state.get_all_runners().await;

            // Primary runner's health also goes into the legacy cached_health
            let mut primary_health = CachedPortHealth::default();
            let mut runner_snapshots = Vec::with_capacity(runners.len());

            for managed in &runners {
                let runner_port = managed.config.port;
                let kind = managed.config.kind();
                let is_primary = kind.is_primary();

                let runner_port_open = port::is_port_listening(runner_port);
                let runner_responding = port::is_runner_responding(runner_port).await;

                let new_health = CachedPortHealth {
                    runner_port_open,
                    runner_responding,
                };

                // 3b — record WHEN it was last seen responding, so the three
                // states the `running` boolean conflates can be told apart
                // downstream: responding / alive but unresponsive since T /
                // stopped.
                //
                // **Every runner kind, not just the user-managed ones.** This
                // stamp is the ONLY input that lets `RunnerState::liveness`
                // return `wedged` rather than `unknown` — with no stamp, a
                // held port and a silent API classify as `(true, None) =>
                // Unknown`. It used to live inside the `!is_supervisor_managed`
                // block below, so a temp/named runner never got one and could
                // never be reported as wedged, whatever the probes said. The
                // guard exists for the `running` sync (which must NOT touch a
                // latched supervisor-managed flag); the stamp has nothing to do
                // with that flag and is a pure observation of what answered.
                if runner_responding {
                    let mut runner_state = managed.runner.write().await;
                    runner_state.last_seen_responding_at = Some(Utc::now());
                }

                // For user-managed runners (not temp, not named), the supervisor only
                // observes. The `running` flag is initialized once at startup from
                // port-in-use and would otherwise go stale. Sync it to the observed
                // API responsiveness so the header status reflects reality.
                let runner_id = &managed.config.id;
                let is_supervisor_managed = is_temp_runner(runner_id) || is_named_runner(runner_id);

                if !is_supervisor_managed {
                    let needs_pid_recovery = {
                        let mut runner_state = managed.runner.write().await;
                        if runner_state.running != runner_responding {
                            runner_state.running = runner_responding;
                        }

                        // 3a — SILENCE IS `UNKNOWN`, NOT `GONE`.
                        //
                        // This used to clear `pid` whenever the API stopped
                        // answering. That is how the dashboard came to assert
                        // `running: false, pid: null` about a process it could
                        // see holding the port in the same JSON document
                        // (reproduced 2026-08-08 against live PID 148320,
                        // which was alive throughout and read
                        // `running: true, pid: 148320` again after recovery).
                        //
                        // An unresponsive API says nothing about whether the
                        // PROCESS exists — a wedged runner is unresponsive and
                        // very much alive. So keep the last known pid and let
                        // the liveness rules below decide what to say about
                        // it. This extends the discipline already applied a
                        // few lines down to a probe that could not RUN: that
                        // one is UNKNOWN, and so is this one.
                        //
                        // `pid` is now cleared only on positive evidence of
                        // absence: the port is closed AND the API is silent.
                        if !runner_responding && !runner_port_open {
                            runner_state.pid = None;
                        }

                        // 3c — allow PID recovery while the port is listening
                        // even if the API is silent. The old guard was
                        // `runner_responding && pid.is_none()`, which blocked
                        // recovery for exactly the wedged case that needs it
                        // most: the port is demonstrably held, so `netstat`
                        // CAN tell us by whom. `pid.is_none()` is retained so
                        // the ~100ms netstat stays out of the steady state.
                        (runner_responding || runner_port_open) && runner_state.pid.is_none()
                    };
                    // Recover the PID for a re-discovered runner after a
                    // supervisor restart: the process is still the one
                    // listening on the port, so netstat tells us which PID
                    // to track. Netstat is ~100ms on Windows, so guard on
                    // `pid.is_none()` to keep this out of the steady state.
                    //
                    // A probe that could not RUN (`Err`) is UNKNOWN, not
                    // "port idle" — leave `pid` as None and try again next
                    // tick rather than recording a wrong answer.
                    #[cfg(target_os = "windows")]
                    if needs_pid_recovery {
                        match crate::process::windows::find_pid_on_port(runner_port).await {
                            Ok(Some(pid)) => {
                                let mut runner_state = managed.runner.write().await;
                                if runner_state.pid.is_none() {
                                    runner_state.pid = Some(pid);
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(
                                    "PID recovery for runner '{}' on port {}: listener probe \
                                     failed ({}) — leaving pid unknown for this tick",
                                    runner_id,
                                    runner_port,
                                    e
                                );
                            }
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    let _ = needs_pid_recovery;
                }

                // 3e — ESCALATE a persistently unresponsive runner.
                //
                // "Alive but not answering" is the state that cost this
                // box 7 hours on 2026-08-08 while the dashboard quietly
                // said the runner was gone. It is not a normal state and
                // it must not be silent.
                //
                // **Every runner kind, not just the user-managed ones.**
                // This sat inside the `!is_supervisor_managed` guard above,
                // so a wedged temp or named runner produced no escalation at
                // all — and because the same guard also withheld its
                // `last_seen_responding_at` stamp, it did not read as
                // `wedged` on any surface either. A supervisor-managed runner
                // is exactly the kind an agent spawns and then waits on, so
                // silence there is the more expensive of the two.
                //
                // ESCALATE FIRST, then restart — but ONLY on a zero census,
                // and NEVER on a session count the wedged door reported.
                //
                // This comment used to read "NEVER auto-restart from here,
                // whoever owns the runner … Escalate only; the operator
                // decides." That doctrine's cost is now measured: three
                // occurrences in nine days, 12 hours to 5 days each, zero
                // recoveries, and each one taking out every coord transport on
                // the box (they all root on the runner's :9876). Its REASON —
                // a restart destroys in-flight sessions — is preserved intact
                // by `manager::maybe_serving_restart`, which acts only when
                // the supervisor's own process-subtree census finds zero live
                // `claude` sessions under the runner. A zero census is the
                // statement "there is nothing to destroy", made from evidence
                // the wedged door cannot corrupt. What was removed is the
                // UNCONDITIONAL refusal, which had converted "protect
                // sessions" into "protect a process that has already lost
                // them". Plan `2026-09-03-runner-zombie-serving-watchdog`.
                // Classified from the SAME probes the snapshot below
                // publishes, so the log and the API can never disagree about
                // whether this is a wedge. The condition used to be the raw
                // pair `!responding && port_open`, which is BROADER than the
                // `wedged` verdict: with no `last_seen_responding_at` stamp
                // that pair classifies `Unknown`, not `UnresponsiveSince`.
                // Escalating it as a wedge would claim "the process is ALIVE
                // and not responding — capture a thread dump" about a runner
                // that has simply not finished booting — which every
                // `spawn-test` produces, since a runner holds its port through
                // a 30s-per-stage PG bootstrap. Both states are still counted
                // and still escalated; they just say what they are.
                let liveness_now = {
                    let runner_state = managed.runner.read().await;
                    runner_state.liveness(runner_port_open, runner_responding)
                };
                let escalating = runner_port_open
                    && matches!(
                        liveness_now,
                        RunnerLiveness::UnresponsiveSince(_) | RunnerLiveness::Unknown
                    );

                if escalating {
                    let ticks = unresponsive_ticks
                        .entry(runner_id.clone())
                        .and_modify(|t| *t = t.saturating_add(1))
                        .or_insert(1);

                    let should_escalate = *ticks == UNRESPONSIVE_ESCALATION_TICKS
                        || (*ticks > UNRESPONSIVE_ESCALATION_TICKS
                            && (*ticks - UNRESPONSIVE_ESCALATION_TICKS)
                                .is_multiple_of(UNRESPONSIVE_REESCALATION_TICKS));

                    // TWO CADENCES, deliberately. The serving DECISION runs on
                    // every escalating tick — it is a pure function over state
                    // already held, and gating it on `should_escalate` would
                    // make the effective threshold floor 330 s whatever the
                    // operator configured, so the 60 s clamp floor would be
                    // unreachable. The ALERT keeps `should_escalate`'s slower
                    // cadence, which is passed in rather than recomputed.
                    if *ticks >= UNRESPONSIVE_ESCALATION_TICKS {
                        crate::process::manager::maybe_serving_restart(
                            &state,
                            managed,
                            liveness_now,
                            should_escalate,
                        )
                        .await;
                    }

                    if should_escalate {
                        let (pid, since) = {
                            let runner_state = managed.runner.read().await;
                            (runner_state.pid, runner_state.last_seen_responding_at)
                        };
                        if matches!(liveness_now, RunnerLiveness::UnresponsiveSince(_)) {
                            tracing::error!(
                                runner_id = %runner_id,
                                port = runner_port,
                                pid = ?pid,
                                last_seen_responding_at = ?since,
                                unresponsive_for_secs = (*ticks * 2),
                                "RUNNER WEDGED: port is held but the HTTP API has not answered \
                                 for {}s. The process is ALIVE and not responding — this is not \
                                 a stopped runner. Not restarting ({}); capture a thread dump \
                                 before any manual restart.",
                                *ticks * 2,
                                if is_supervisor_managed {
                                    "a wedge is not an exit, so the crash-only watchdog does not \
                                     cover it, and a restart destroys the in-flight work and the \
                                     evidence"
                                } else {
                                    "user-managed runners are observed only, and a restart \
                                     destroys in-flight sessions"
                                }
                            );
                            state
                                .logs
                                .emit(
                                    LogSource::Supervisor,
                                    LogLevel::Error,
                                    format!(
                                        "Runner '{}' WEDGED — port {} held, API silent for {}s \
                                         (pid {:?}, last responded {:?}). Alive, not stopped.",
                                        runner_id,
                                        runner_port,
                                        *ticks * 2,
                                        pid,
                                        since
                                    ),
                                )
                                .await;
                        } else {
                            // Port held, and it has NEVER been seen answering.
                            // Not a wedge (there is no "since" — nothing has
                            // been lost yet) and not stopped either. A start
                            // that never binds its API is what the
                            // first-healthy watchdog exists for, and a runner
                            // stuck in PG bootstrap looks exactly like this.
                            tracing::warn!(
                                runner_id = %runner_id,
                                port = runner_port,
                                pid = ?pid,
                                silent_for_secs = (*ticks * 2),
                                "Runner has held port {} for {}s and has NEVER answered its \
                                 HTTP API. UNKNOWN, not a wedge and not a stopped runner — \
                                 most likely a start that has not finished (PG bootstrap \
                                 holds the port through 30s-per-stage timeouts).",
                                runner_port,
                                *ticks * 2
                            );
                        }
                    }
                } else if runner_responding {
                    // Recovered (or never wedged) — reset, and say so if
                    // we had previously escalated, so the log carries the
                    // end of the window as well as the start.
                    if let Some(prev) = unresponsive_ticks.remove(runner_id) {
                        if prev >= UNRESPONSIVE_ESCALATION_TICKS {
                            tracing::warn!(
                                runner_id = %runner_id,
                                port = runner_port,
                                silent_for_secs = (prev * 2),
                                "Runner started answering after {}s of silence, without any \
                                 supervisor intervention. (The silence was a wedge only if a \
                                 RUNNER WEDGED line names this runner; otherwise it had never \
                                 answered.)",
                                prev * 2
                            );
                        }
                    }
                } else {
                    // Port not held either — that is a stopped runner,
                    // not a wedge. Different condition, different signal.
                    unresponsive_ticks.remove(runner_id);
                }

                // If the runner's TCP port is responsive, GET its /health to
                // extract the application-layer signals (ui_error,
                // derived_status, recent_crash). Older runners that don't
                // emit these fields still parse cleanly thanks to
                // `serde(default)` on RunnerHealthBody — the missing fields
                // stay `None` and `derived_status` is inferred from process
                // state.
                let health_body = if runner_responding {
                    fetch_runner_health_body(runner_port).await
                } else {
                    None
                };
                let ui_error = health_body.as_ref().and_then(|b| b.ui_error.clone());
                let recent_crash = health_body.as_ref().and_then(|b| b.recent_crash.clone());

                // Snapshot the crash-only watchdog state for SSE consumers.
                let watchdog = {
                    let wd = managed.watchdog.read().await;
                    crate::routes::health::WatchdogHealth::from_state_with_arms(
                        &wd,
                        crate::process::manager::crash_restart_globally_armed(&state.config),
                        crate::process::manager::serving_restart_globally_armed(&state.config),
                    )
                };

                // Build runner snapshot for SSE consumers
                let runner_state = managed.runner.read().await;
                let derived_status = derive_runner_status(
                    runner_state.running,
                    runner_responding,
                    health_body.as_ref(),
                );
                let frontend_ready = health_body.as_ref().and_then(|b| b.frontend_ready());
                let frontend_state = health_body
                    .as_ref()
                    .and_then(|b| b.frontend_state().map(str::to_string));
                let frontend_run = frontend_runs.observe(
                    &managed.config.id,
                    frontend_ready,
                    frontend_state.as_deref(),
                    runner_state.pid,
                    Utc::now(),
                );
                // An UNKNOWN tick inside a run still names the last state seen.
                let frontend_state = frontend_state
                    .or_else(|| frontend_run.as_ref().and_then(|r| r.last_state.clone()));
                runner_snapshots.push(CachedRunnerHealth {
                    id: managed.config.id.clone(),
                    name: managed.config.name.clone(),
                    port: runner_port,
                    kind: kind.clone(),
                    running: runner_state.running,
                    pid: runner_state.pid,
                    api_responding: runner_responding,
                    ui_error,
                    recent_crash,
                    derived_status,
                    watchdog,
                    // Phase 3b — classified from the SAME tick's live listener
                    // probe, never from a remembered or assumed port state.
                    liveness: runner_state.liveness(runner_port_open, runner_responding),
                    last_seen_responding_at: runner_state.last_seen_responding_at,
                    port_open: runner_port_open,
                    frontend_ready,
                    frontend_state,
                    frontend_not_ready_since: frontend_run.map(|r| r.since),
                });
                drop(runner_state);

                // Update per-runner cache
                let mut cache = managed.cached_health.write().await;
                *cache = new_health.clone();
                drop(cache);

                if is_primary {
                    primary_health = new_health;
                }
            }

            // The wedge counter now sees EPHEMERAL runners too (3e covers
            // every kind), and a temp runner removed while wedged would
            // otherwise leave its entry behind forever — an unbounded map on a
            // box that spawns temp runners all day. Drop whatever is no longer
            // in the registry: this is per-tick bookkeeping, not state.
            unresponsive_ticks.retain(|id, _| runners.iter().any(|m| &m.config.id == id));
            frontend_runs.retain_ids(|id| runners.iter().any(|m| m.config.id == id));

            // If no runners exist, check legacy ports
            if runners.is_empty() {
                let runner_port = crate::config::RUNNER_API_PORT;

                primary_health = CachedPortHealth {
                    runner_port_open: port::is_port_listening(runner_port),
                    runner_responding: port::is_runner_responding(runner_port).await,
                };
            }

            // Update legacy cached_health (from primary)
            let mut cache = state.cached_health.write().await;
            *cache = primary_health.clone();
            drop(cache);

            // Update cached runner health snapshot (for SSE consumers)
            let mut runner_cache = state.cached_runner_health.write().await;
            *runner_cache = runner_snapshots;
            drop(runner_cache);

            tick_count += 1;
            // Log to dashboard once per minute (every 30 ticks at 2s interval)
            if tick_count % 30 == 1 {
                state
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Debug,
                        format!(
                            "Health cache: runner_port={}, api_responding={} (runners: {})",
                            primary_health.runner_port_open,
                            primary_health.runner_responding,
                            runners.len()
                        ),
                    )
                    .await;
            }
            debug!("Health cache refreshed");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a `/health` body the way the real path does, so these tests
    /// exercise the serde renames (`embeddingService`, `webIntegration`,
    /// `lastError`) rather than a hand-built struct that could drift.
    fn body(json: &str) -> RunnerHealthBody {
        serde_json::from_str(json).expect("health body must parse")
    }

    /// The REAL runner shape (qontinui-runner origin/main b58a81dce,
    /// `src-tauri/src/mcp_api.rs`): `embeddingService` / `webIntegration` /
    /// `database` exist only inside `data`; the top level mirrors only
    /// `derived_status`. The reason must still name every cause.
    #[test]
    fn degraded_reason_names_causes_published_only_under_data() {
        let b = body(
            r#"{"success":true,
                "data":{"status":"degraded",
                        "embeddingService":{"reachable":false,"url":"http://127.0.0.1:8001"},
                        "webIntegration":{"relayExpected":true,"connected":false,
                                          "lastError":"401 Unauthorized"},
                        "database":{"reachable":false,"arm":"external"},
                        "derived_status":"degraded"},
                "derived_status":"degraded"}"#,
        );
        match derive_runner_status(true, true, Some(&b)) {
            RunnerStatus::Degraded { reason } => {
                assert_eq!(
                    reason,
                    "runner degraded: embedding service unreachable; database unreachable; \
                     backend relay down (401 Unauthorized)"
                );
            }
            other => panic!("expected Degraded, got {:?}", other),
        }
    }

    #[test]
    fn degraded_reason_names_the_relay_rather_than_restating_the_verdict() {
        let b = body(
            r#"{"derived_status":"degraded",
                "embeddingService":{"reachable":true},
                "database":{"reachable":true},
                "webIntegration":{"connected":false,"wsConnected":false,
                  "lastError":"Backend closed the WS before the `connected` ack"}}"#,
        );
        let r = degraded_reason(&b);
        assert!(r.contains("backend relay down"), "{r}");
        assert!(r.contains("connected` ack"), "the cause must survive: {r}");
        assert!(
            !r.contains("derived_status=degraded"),
            "must not restate: {r}"
        );
    }

    #[test]
    fn degraded_reason_distinguishes_the_three_causes() {
        let relay = degraded_reason(&body(r#"{"webIntegration":{"connected":false}}"#));
        let embed = degraded_reason(&body(r#"{"embeddingService":{"reachable":false}}"#));
        let db = degraded_reason(&body(r#"{"database":{"reachable":false}}"#));

        assert!(relay.contains("relay"), "{relay}");
        assert!(embed.contains("embedding"), "{embed}");
        assert!(db.contains("database"), "{db}");
        // The whole point: a dead relay must not read like a PG outage.
        assert_ne!(relay, embed);
        assert_ne!(relay, db);
        assert_ne!(embed, db);
    }

    #[test]
    fn a_relay_that_is_not_expected_is_not_a_cause() {
        // `connected: null` = no relay expected (tier below qontinui_account,
        // or web-integration disabled). Reporting that as a fault would make
        // every deliberately local-only runner look broken.
        let r = degraded_reason(&body(
            r#"{"embeddingService":{"reachable":false},
                "webIntegration":{"connected":null,"wsConnected":false}}"#,
        ));
        assert!(r.contains("embedding"), "{r}");
        assert!(
            !r.contains("relay"),
            "an unexpected relay is not a cause: {r}"
        );
    }

    #[test]
    fn degraded_reason_lists_every_simultaneous_cause() {
        let r = degraded_reason(&body(
            r#"{"embeddingService":{"reachable":false},
                "database":{"reachable":false},
                "webIntegration":{"connected":false}}"#,
        ));
        assert!(r.contains("embedding"), "{r}");
        assert!(r.contains("database"), "{r}");
        assert!(r.contains("relay"), "{r}");
    }

    #[test]
    fn an_older_runner_falls_back_to_the_original_wording() {
        // A runner too old to publish these blocks yields no causes. Say the
        // verdict and invent nothing — a mixed-version fleet must not get a
        // fabricated cause.
        let r = degraded_reason(&body(r#"{"derived_status":"degraded"}"#));
        assert_eq!(r, "runner reported derived_status=degraded");
    }

    #[test]
    fn a_blank_last_error_does_not_produce_empty_parentheses() {
        let r = degraded_reason(&body(
            r#"{"webIntegration":{"connected":false,"lastError":"   "}}"#,
        ));
        assert_eq!(r, "runner degraded: backend relay down");
    }

    #[test]
    fn test_cached_port_health_default_all_false() {
        let health = CachedPortHealth::default();
        assert!(!health.runner_port_open);
        assert!(!health.runner_responding);
    }

    #[test]
    fn test_cached_port_health_clone() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
        };
        let cloned = health.clone();
        assert!(cloned.runner_port_open);
        assert!(cloned.runner_responding);
    }

    #[test]
    fn test_cached_port_health_debug_format() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: false,
        };
        let debug_str = format!("{:?}", health);
        assert!(debug_str.contains("runner_port_open: true"));
        assert!(debug_str.contains("runner_responding: false"));
    }

    #[test]
    fn test_cached_port_health_all_true() {
        let health = CachedPortHealth {
            runner_port_open: true,
            runner_responding: true,
        };
        assert!(health.runner_port_open);
        assert!(health.runner_responding);
    }

    #[test]
    fn test_derive_runner_status_offline_when_api_down_and_not_running() {
        let status = derive_runner_status(false, false, None);
        assert!(matches!(status, RunnerStatus::Offline));
    }

    #[test]
    fn test_derive_runner_status_starting_when_process_alive_but_api_down() {
        // Process exists but /health isn't responding yet — the spec says to
        // treat this as Starting, not Offline.
        let status = derive_runner_status(true, false, None);
        assert!(matches!(status, RunnerStatus::Starting));
    }

    #[test]
    fn test_derive_runner_status_healthy_when_api_responding_no_body() {
        // /health reachable but body wasn't parseable (older runner or
        // network hiccup). Fall through to Healthy when supervisor tracks
        // the process as running.
        let status = derive_runner_status(true, true, None);
        assert!(matches!(status, RunnerStatus::Healthy));
    }

    #[test]
    fn test_derive_runner_status_errored_when_ui_error_present() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: Some(UiErrorSummary {
                message: "ReferenceError: foo is not defined".to_string(),
                digest: None,
                stack: None,
                component_stack: None,
                first_seen: Utc::now(),
                reported_at: Utc::now(),
                count: 1,
            }),
            recent_crash: None,
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("ReferenceError"));
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_starting_when_body_reports_starting() {
        // Older runners use top-level status="starting" during boot. Respect
        // that so the dashboard shows a blue badge, not green.
        let body = RunnerHealthBody {
            status: Some("starting".to_string()),
            derived_status: None,
            ui_error: None,
            recent_crash: None,
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        assert!(matches!(status, RunnerStatus::Starting));
    }

    #[test]
    fn test_derive_runner_status_errored_from_derived_status_field() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("ERRORED".to_string()), // case-insensitive
            ui_error: None,
            recent_crash: None,
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        assert!(matches!(status, RunnerStatus::Errored { .. }));
    }

    /// The 2026-10-05 shape: a runner serving the PLACEHOLDER frontend
    /// answers 2xx with `frontendReady: false` inside its `data` envelope
    /// (where the runner actually publishes it) and `derived_status: "errored"`
    /// at the top level. The reason must name the frontend state.
    #[test]
    fn test_derive_runner_status_errored_names_frontend_state_from_data_envelope() {
        let b = body(
            r#"{"success":true,
                "data":{"status":"ok","frontendReady":false,
                        "frontendState":"window_not_visible",
                        "derived_status":"errored"},
                "derived_status":"errored"}"#,
        );
        match derive_runner_status(true, true, Some(&b)) {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("derived_status=errored"), "{reason}");
                assert!(reason.contains("frontend not ready"), "{reason}");
                assert!(reason.contains("window_not_visible"), "{reason}");
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    /// Same, with the fields at the top level (the camelCase names the brief
    /// observed) — accepted too, so either placement names the frontend.
    #[test]
    fn test_derive_runner_status_errored_names_frontend_state_top_level() {
        let b = body(
            r#"{"frontendReady":false,"frontendState":"window_not_visible",
                "derived_status":"errored"}"#,
        );
        assert_eq!(b.frontend_ready(), Some(false));
        match derive_runner_status(true, true, Some(&b)) {
            RunnerStatus::Errored { reason } => {
                assert_eq!(
                    reason,
                    "runner reported derived_status=errored; frontend not ready \
                     (frontendState=window_not_visible)"
                );
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    /// A ready frontend adds nothing, and the status derived is unchanged.
    #[test]
    fn test_derive_runner_status_errored_reason_unchanged_when_frontend_ready() {
        let b = body(
            r#"{"data":{"frontendReady":true,"frontendState":"ready"},
                "derived_status":"errored"}"#,
        );
        match derive_runner_status(true, true, Some(&b)) {
            RunnerStatus::Errored { reason } => {
                assert_eq!(reason, "runner reported derived_status=errored");
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    /// `frontendReady: false` on a runner that does NOT say errored must not
    /// change the derived status — only the errored reason gets richer.
    #[test]
    fn test_frontend_not_ready_alone_does_not_derive_errored() {
        let b = body(r#"{"data":{"frontendReady":false,"frontendState":"booting"}}"#);
        assert!(matches!(
            derive_runner_status(true, true, Some(&b)),
            RunnerStatus::Healthy
        ));
    }

    fn secs(t: DateTime<Utc>, n: i64) -> DateTime<Utc> {
        t + chrono::Duration::seconds(n)
    }

    #[test]
    fn frontend_run_keeps_the_first_instant_for_the_same_pid() {
        let t0 = Utc::now();
        let first = next_frontend_run(None, Some(false), Some("booting"), Some(7), t0).unwrap();
        assert_eq!(first.since, t0);
        let second = next_frontend_run(
            Some(&first),
            Some(false),
            Some("booting"),
            Some(7),
            secs(t0, 2),
        )
        .unwrap();
        assert_eq!(second.since, t0, "an unbroken run keeps t0");
    }

    /// false, UNKNOWN, false keeps the original start — a slow or failed
    /// `/health` read must not reset the run.
    #[test]
    fn frontend_run_unknown_tick_pauses_rather_than_ends() {
        let t0 = Utc::now();
        let a = next_frontend_run(None, Some(false), Some("window_not_visible"), Some(7), t0);
        let b = next_frontend_run(a.as_ref(), None, None, Some(7), secs(t0, 2));
        let b = b.expect("an UNKNOWN tick must not end the run");
        assert_eq!(b.since, t0);
        assert_eq!(b.last_state.as_deref(), Some("window_not_visible"));
        let c = next_frontend_run(Some(&b), Some(false), None, Some(7), secs(t0, 4)).unwrap();
        assert_eq!(c.since, t0);
        assert_eq!(
            c.unknown_since, None,
            "a false observation closes the UNKNOWN gap"
        );
    }

    /// A continuous UNKNOWN stretch of 60 s or more drops the run; just under
    /// it does not.
    #[test]
    fn frontend_run_unknown_gap_past_cap_resets() {
        let t0 = Utc::now();
        let a = next_frontend_run(None, Some(false), None, Some(7), t0);
        let gap_start = next_frontend_run(a.as_ref(), None, None, Some(7), secs(t0, 2)).unwrap();
        let at_59 = next_frontend_run(Some(&gap_start), None, None, Some(7), secs(t0, 61));
        assert!(at_59.is_some(), "59 s of UNKNOWN is inside the cap");
        let at_60 = next_frontend_run(at_59.as_ref(), None, None, Some(7), secs(t0, 62));
        assert_eq!(at_60, None, "60 s of continuous UNKNOWN drops the run");
    }

    #[test]
    fn frontend_run_ready_resets_and_new_pid_restarts() {
        let t0 = Utc::now();
        let run = next_frontend_run(None, Some(false), None, Some(7), t0).unwrap();
        assert_eq!(
            next_frontend_run(Some(&run), Some(true), None, Some(7), secs(t0, 2)),
            None,
            "a ready frontend ends the run"
        );
        let restarted =
            next_frontend_run(Some(&run), Some(false), None, Some(8), secs(t0, 2)).unwrap();
        assert_eq!(restarted.since, secs(t0, 2), "a new pid starts a new run");
        assert_eq!(
            next_frontend_run(Some(&run), None, None, Some(8), secs(t0, 2)),
            None,
            "a new pid during UNKNOWN ends the run"
        );
        assert_eq!(
            next_frontend_run(None, None, None, Some(7), t0),
            None,
            "UNKNOWN never starts a run"
        );
    }

    /// The refresher's map wiring: observe inserts, a ready tick removes, and
    /// `retain_ids` drops runners that left the registry.
    #[test]
    fn frontend_run_tracker_inserts_removes_and_retains() {
        let t0 = Utc::now();
        let mut tr = FrontendRunTracker::default();
        assert!(tr
            .observe("primary", Some(false), None, Some(1), t0)
            .is_some());
        assert!(tr
            .observe("temp-1", Some(false), None, Some(2), t0)
            .is_some());
        assert_eq!(tr.len(), 2);
        let again = tr
            .observe("primary", Some(false), None, Some(1), secs(t0, 2))
            .unwrap();
        assert_eq!(again.since, t0, "the tracker carries the run across ticks");
        assert!(tr
            .observe("primary", Some(true), None, Some(1), secs(t0, 4))
            .is_none());
        assert_eq!(tr.len(), 1);
        tr.retain_ids(|id| id != "temp-1");
        assert_eq!(tr.len(), 0);
    }

    #[test]
    fn test_derive_runner_status_degraded_from_derived_status_field() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("Degraded".to_string()), // case-insensitive
            ui_error: None,
            recent_crash: None,
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Degraded { reason } => {
                assert!(reason.contains("degraded"));
            }
            other => panic!("expected Degraded, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_errored_when_recent_crash_present() {
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: None,
            recent_crash: Some(RecentCrashSummary {
                file_path: r"D:\.dev-logs\crash_1.txt".to_string(),
                reported_at: Utc::now(),
                panic_location: Some("src-tauri/src/foo.rs:42:9".to_string()),
                panic_message: Some("no reactor running".to_string()),
                thread: Some("main".to_string()),
            }),
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("no reactor running"));
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_errored_when_crash_has_no_message() {
        // Truncated dumps can miss the `=== PANIC MESSAGE ===` section. The
        // badge should still flip to errored with a placeholder reason so
        // operators see *something*.
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: None,
            recent_crash: Some(RecentCrashSummary {
                file_path: r"D:\.dev-logs\crash_partial.txt".to_string(),
                reported_at: Utc::now(),
                panic_location: None,
                panic_message: None,
                thread: None,
            }),
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => {
                assert!(reason.contains("Rust panic"), "got: {reason}");
            }
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_derive_runner_status_ui_error_wins_over_recent_crash() {
        // A live UI error is more actionable than a historical crash dump:
        // if both are present, surface the UI error reason in the badge.
        let body = RunnerHealthBody {
            status: Some("ok".to_string()),
            derived_status: Some("errored".to_string()),
            ui_error: Some(UiErrorSummary {
                message: "live ui error".to_string(),
                digest: None,
                stack: None,
                component_stack: None,
                first_seen: Utc::now(),
                reported_at: Utc::now(),
                count: 1,
            }),
            recent_crash: Some(RecentCrashSummary {
                file_path: r"D:\.dev-logs\crash_1.txt".to_string(),
                reported_at: Utc::now(),
                panic_location: None,
                panic_message: Some("stale crash".to_string()),
                thread: None,
            }),
            ..Default::default()
        };
        let status = derive_runner_status(true, true, Some(&body));
        match status {
            RunnerStatus::Errored { reason } => assert!(reason.contains("live ui error")),
            other => panic!("expected Errored, got {:?}", other),
        }
    }

    #[test]
    fn test_runner_health_body_parses_recent_crash_camel_case() {
        // The runner serializes RecentCrash with serde(rename_all="camelCase").
        // Deserializer must match that shape.
        let json = r#"{
            "status": "ok",
            "derived_status": "errored",
            "recent_crash": {
                "filePath": "D:/.dev-logs/crash_1.txt",
                "reportedAt": "2026-04-22T10:15:30Z",
                "panicLocation": "src-tauri/src/foo.rs:42:9",
                "panicMessage": "boom",
                "thread": "main"
            }
        }"#;
        let body: RunnerHealthBody = serde_json::from_str(json).expect("should parse");
        let crash = body.recent_crash.expect("recent_crash present");
        assert_eq!(crash.panic_message.as_deref(), Some("boom"));
        assert_eq!(crash.thread.as_deref(), Some("main"));
    }

    #[test]
    fn test_truncate_reason_preserves_short_input() {
        assert_eq!(truncate_reason("hello", 200), "hello");
    }

    #[test]
    fn test_truncate_reason_clips_long_input() {
        let long = "x".repeat(500);
        let out = truncate_reason(&long, 200);
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn test_runner_health_body_parses_older_runner_without_new_fields() {
        // Older runners only emit `status`. The new fields must parse as
        // `None`, not error the whole response out.
        let json = r#"{"status":"ok"}"#;
        let body: RunnerHealthBody = serde_json::from_str(json).expect("should parse");
        assert_eq!(body.status.as_deref(), Some("ok"));
        assert!(body.derived_status.is_none());
        assert!(body.ui_error.is_none());
        assert!(body.recent_crash.is_none());
    }

    #[test]
    fn test_runner_health_body_parses_full_payload() {
        let json = r#"{
            "status": "ok",
            "derived_status": "errored",
            "ui_error": {
                "message": "boom",
                "stack": null,
                "component_stack": null,
                "digest": null,
                "first_seen": "2026-04-21T00:00:00Z",
                "reported_at": "2026-04-21T00:00:05Z",
                "count": 3
            }
        }"#;
        let body: RunnerHealthBody = serde_json::from_str(json).expect("should parse");
        assert_eq!(body.derived_status.as_deref(), Some("errored"));
        let ui_err = body.ui_error.expect("ui_error should be present");
        assert_eq!(ui_err.message, "boom");
        assert_eq!(ui_err.count, 3);
    }

    #[test]
    fn test_runner_status_default_is_offline() {
        let s: RunnerStatus = Default::default();
        assert!(matches!(s, RunnerStatus::Offline));
    }

    /// Return the byte range of the `if !is_supervisor_managed { .. }` block,
    /// matching braces while ignoring anything inside a string literal, a line
    /// comment or a block comment — all three occur inside that block.
    fn user_managed_guard_span(src: &str) -> (usize, usize) {
        // Every literal this test searches for is ASSEMBLED AT RUNTIME, never
        // written out whole. `include_str!` embeds this file into itself, so a
        // whole-literal needle would also match the test’s own copy of it —
        // which is both an extra hit for the exactly-once check below and a
        // way for the test to pass by finding itself.
        let needle = ["if !is_supervisor_", "managed {"].concat();
        let needle = needle.as_str();
        let start = src.find(needle).expect(
            "the refresher's user-managed guard must still exist — if it was renamed, \
             re-point this test rather than deleting it",
        );
        let mut i = start + needle.len();
        let mut depth = 1usize;
        let b = src.as_bytes();
        while i < b.len() && depth > 0 {
            match b[i] {
                b'"' => {
                    i += 1;
                    while i < b.len() {
                        match b[i] {
                            b'\\' => i += 2,
                            b'"' => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                }
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    i += 2;
                    while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                        i += 1;
                    }
                    i += 2;
                }
                b'{' => {
                    depth += 1;
                    i += 1;
                }
                b'}' => {
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        assert_eq!(depth, 0, "unbalanced braces while scanning the guard block");
        (start, i)
    }

    /// The `last_seen_responding_at` stamp and the `RUNNER WEDGED` escalation
    /// must NOT sit inside the `!is_supervisor_managed` guard.
    ///
    /// Both did. The consequence was not a cosmetic one: that stamp is the only
    /// input that lets `RunnerState::liveness` answer `wedged` rather than
    /// `unknown`, so a temp or named runner — the kind an agent spawns and then
    /// waits on — could never be reported as wedged on ANY surface, and no
    /// escalation was ever logged for one either. A prose comment saying "every
    /// runner kind" cannot notice being moved back inside a guard; this can.
    #[test]
    fn the_wedge_signal_is_not_gated_on_runner_ownership() {
        let src = include_str!("health_cache.rs");
        let (start, end) = user_managed_guard_span(src);

        // Self-check FIRST: prove the span is real by finding something that
        // genuinely IS inside the guard (the `running` sync, which must stay
        // there — a latched supervisor-managed flag must never be synced to
        // the probe). Without this, a scanner that returned an empty or
        // degenerate span would make every assertion below vacuously pass.
        let inside = ["runner_state.running = ", "runner_responding;"].concat();
        let inside_at = src
            .find(inside.as_str())
            .expect("the `running` sync must still exist");
        assert!(
            inside_at > start && inside_at < end,
            "the guard span {start}..{end} does not contain the `running` sync at \
             {inside_at} — the brace scanner is wrong, not the code",
        );
        // Assembled at runtime for the reason given in `user_managed_guard_span`.
        for marker in [
            [
                "runner_state.last_seen_responding_at = ",
                "Some(Utc::now());",
            ]
            .concat(),
            ["RUNNER ", "WEDGED: port is held"].concat(),
        ] {
            let marker = marker.as_str();
            let at = src
                .find(marker)
                .unwrap_or_else(|| panic!("marker vanished from health_cache.rs: {marker}"));
            assert!(
                at < start || at > end,
                "`{marker}` is inside the `!is_supervisor_managed` guard (bytes {start}..{end}), \
                 so temp and named runners lose the wedge signal entirely",
            );
            assert_eq!(
                src.matches(marker).count(),
                1,
                "`{marker}` must appear exactly once, or this test can pass on a copy \
                 while the gated original still runs",
            );
        }
    }
}
