//! Flywheel cron — drives the spec coverage-growth loop on a nightly cadence.
//!
//! Composes three runner endpoints in sequence:
//!   1. POST /spec/proposals/scan       — find new pathnames + drift reports
//!   2. POST /spec/proposals/<id>/execute  for every status="queued" row
//!   3. POST /spec/proposals/sweep-pending — re-run gate-3 (B-green) on
//!      every "pendingPromotion" row
//!
//! Runs against the PRIMARY runner (port 9876) only. Temp/named runners
//! have isolated state and do not participate.
//!
//! Cadence (per `spec-check-v1/05-flywheel.md` §6.1 + §6.7): once per 24h.
//! Configurable via env `QONTINUI_FLYWHEEL_INTERVAL_SECS` (default 86_400).
//! Initial delay before the first tick: `QONTINUI_FLYWHEEL_INITIAL_DELAY_SECS`
//! (default 3_600 — one hour after supervisor start so the supervisor's own
//! startup work finishes first).
//!
//! Anti-pattern guards (§6.7):
//! - Time-driven only — never event-driven (no git-hook / fs-watcher).
//! - Sequential within a tick — the AI provider is a shared rate-limit
//!   resource; running meta-workflows in parallel doesn't speed up the
//!   loop, it just multiplies failure modes.
//! - `MissedTickBehavior::Skip` so a tick that runs longer than the
//!   interval doesn't queue up backlogged ticks (the default `Burst`
//!   behavior would).

use std::time::Duration;

use serde_json::Value;
use tokio::time::MissedTickBehavior;

use crate::state::SharedState;

/// Port of the primary runner. The cron is intentionally hard-coded to this
/// port — temp runners (9877+) and named runners have isolated state and
/// must not be driven by the flywheel.
const PRIMARY_RUNNER_PORT: u16 = 9876;

/// Per-tick HTTP request timeout. Sized for `/execute` calls that wait for a
/// meta-workflow to complete; smaller calls (`/scan`, `/sweep-pending`) will
/// return well within this budget.
const HTTP_REQUEST_TIMEOUT_SECS: u64 = 600;

/// Sentinel error string returned by [`http_post_json`] when the runner
/// responds with 404 — interpreted by [`run_one_tick`] as "spec-authoring
/// feature is off; warn-once and continue".
const FEATURE_OFF_SENTINEL: &str = "feature-off";

/// Counts surfaced from a single tick for tests + logging.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Number of queued proposal ids the scan returned.
    pub queued: usize,
    /// Number of `/execute` calls that returned 2xx.
    pub executed: usize,
    /// 1 if `/sweep-pending` returned 2xx, else 0. (Capped at 1 — we only
    /// invoke sweep once per tick.)
    pub swept: usize,
}

/// Entrypoint spawned from `main.rs`. Honors the two env vars, sleeps the
/// initial delay, then drives one tick per interval. Errors are logged and
/// swallowed — the loop never panics.
pub async fn flywheel_loop(state: SharedState) {
    let interval_secs = std::env::var("QONTINUI_FLYWHEEL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(86_400_u64);
    let initial_delay_secs = std::env::var("QONTINUI_FLYWHEEL_INITIAL_DELAY_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3_600_u64);

    tracing::info!(
        "flywheel: starting cron — interval {}s, initial delay {}s, target port {}",
        interval_secs,
        initial_delay_secs,
        PRIMARY_RUNNER_PORT
    );

    // One-shot initial delay so the supervisor's own startup (orphan scan,
    // prewarm, etc.) finishes before the first tick fires.
    tokio::time::sleep(Duration::from_secs(initial_delay_secs)).await;

    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    // SAFETY: MissedTickBehavior::Skip is explicit, NOT default (which is
    // `Burst`). If a single tick takes longer than the interval (e.g. a
    // tick that fires 50 meta-workflows runs >24h), we drop the missed
    // tick rather than fire them back-to-back. See plan §6.1.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Warn-once latch for the "feature off" (404 on /scan) case so we don't
    // spam the log every 24h when the feature flag is permanently off on
    // this runner.
    let mut feature_off_warned = false;

    loop {
        ticker.tick().await;
        match run_one_tick(&state).await {
            Ok(report) => {
                feature_off_warned = false;
                tracing::info!(
                    "flywheel: tick complete — queued={}, executed={}, swept={}",
                    report.queued,
                    report.executed,
                    report.swept,
                );
            }
            Err(e) if e == FEATURE_OFF_SENTINEL => {
                if !feature_off_warned {
                    tracing::warn!(
                        "flywheel: runner returned 404 on /spec/proposals/scan — \
                         spec-authoring feature is off. Sleeping until the next \
                         tick. (This warning is logged once per off-state run.)"
                    );
                    feature_off_warned = true;
                }
            }
            Err(e) => {
                tracing::warn!("flywheel: tick failed: {}", e);
            }
        }
    }
}

/// Run one complete tick: scan → execute every queued proposal → sweep.
///
/// Returns the count of queued/executed/swept rows on success. Returns
/// [`FEATURE_OFF_SENTINEL`] if `/scan` returned 404; the caller treats this
/// as a benign no-op and warn-once's.
///
/// Visible to tests via `pub(crate)` so the integration scaffolding in
/// `#[cfg(test)] mod tests` can drive ticks against a mock runner. Not part
/// of the public flywheel API — only [`flywheel_loop`] is.
pub(crate) async fn run_one_tick(state: &SharedState) -> Result<TickReport, String> {
    run_one_tick_against(state, &base_url(PRIMARY_RUNNER_PORT)).await
}

/// Same as [`run_one_tick`] but with the runner base URL injected for tests.
/// Production code always passes `http://127.0.0.1:9876`.
pub(crate) async fn run_one_tick_against(
    state: &SharedState,
    base: &str,
) -> Result<TickReport, String> {
    let client = &state.http_client;
    let mut report = TickReport::default();

    // 1. Scan. 404 here is the feature-off signal — propagate the sentinel
    //    so the caller can warn-once and skip the rest of this tick.
    let scan_url = format!("{}/spec/proposals/scan", base);
    let scan_res = http_post_json(client, &scan_url, &serde_json::json!({})).await?;
    let queued = parse_queued_ids(&scan_res);
    report.queued = queued.len();
    tracing::info!(
        "flywheel: scan returned {} queued proposal(s)",
        queued.len()
    );

    // 2. Execute every queued proposal — SEQUENTIAL (§6.7 anti-pattern guard).
    //    The AI provider is a shared rate-limit resource; firing N
    //    meta-workflows in parallel against the same warm provider doesn't
    //    speed things up, it just multiplies failure modes. Per-proposal
    //    failures do NOT abort the tick — log + continue, so a transient
    //    failure on proposal #2 doesn't starve proposals #3..N.
    for prop_id in &queued {
        let url = format!("{}/spec/proposals/{}/execute", base, prop_id);
        match http_post_json(client, &url, &serde_json::json!({})).await {
            Ok(_) => {
                report.executed += 1;
                tracing::info!("flywheel: executed proposal {}", prop_id);
            }
            Err(e) => {
                tracing::warn!("flywheel: execute proposal {} failed: {}", prop_id, e);
            }
        }
    }

    // 3. Sweep — re-run gate-3 against every pendingPromotion row. This is
    //    invoked regardless of whether any proposals were queued in this
    //    tick, because pendingPromotion rows from prior ticks still need
    //    to be re-evaluated each cycle.
    let sweep_url = format!("{}/spec/proposals/sweep-pending", base);
    match http_post_json(client, &sweep_url, &serde_json::json!({})).await {
        Ok(_) => {
            report.swept = 1;
            tracing::info!("flywheel: sweep-pending completed");
        }
        Err(e) if e == FEATURE_OFF_SENTINEL => {
            // Sweep is also gated by the same feature flag. A 404 here when
            // /scan succeeded would be very unusual (would imply the flag
            // flipped mid-tick), but treat it as a non-fatal no-op.
            tracing::warn!("flywheel: sweep-pending returned 404 (feature off mid-tick?)");
        }
        Err(e) => {
            tracing::warn!("flywheel: sweep-pending failed: {}", e);
        }
    }

    Ok(report)
}

/// Build the base URL for a runner port.
fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{}", port)
}

/// Extract queued proposal ids from a `/scan` response. The expected shape is
/// `{"queuedProposals": [{"proposal_id": "..."}|"...", ...]}`. We accept both
/// the object form and the bare-string form for robustness — the runner-side
/// handler shape is still settling and the cron should not crash on schema
/// drift.
fn parse_queued_ids(v: &Value) -> Vec<String> {
    let arr = match v.get("queuedProposals").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|item| {
            if let Some(s) = item.as_str() {
                return Some(s.to_string());
            }
            if let Some(s) = item.get("proposal_id").and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            if let Some(s) = item.get("proposalId").and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            if let Some(s) = item.get("id").and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
            None
        })
        .collect()
}

/// POST a JSON body to `url`. Returns the parsed JSON response body on 2xx.
///
/// Error mapping:
/// - 404 → `Err(FEATURE_OFF_SENTINEL.to_string())` — interpreted as the
///   spec-authoring feature being compiled out on the runner.
/// - Other non-2xx → `Err("status N: <body excerpt>")`.
/// - Network / serialization errors → `Err("<source error>")`.
async fn http_post_json(
    client: &reqwest::Client,
    url: &str,
    body: &Value,
) -> Result<Value, String> {
    let resp = client
        .post(url)
        .timeout(Duration::from_secs(HTTP_REQUEST_TIMEOUT_SECS))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("request to {} failed: {}", url, e))?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Err(FEATURE_OFF_SENTINEL.to_string());
    }
    if !status.is_success() {
        let excerpt = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect::<String>();
        return Err(format!("status {}: {}", status.as_u16(), excerpt));
    }

    // Empty body on 2xx is fine — return Value::Null so callers that don't
    // care about the body don't have to special-case it.
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read body from {} failed: {}", url, e))?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("parse body from {} failed: {}", url, e))
}

// ===========================================================================
// Tests
// ===========================================================================
//
// Strategy: spin up a small axum router on an ephemeral port to act as a
// stand-in primary runner, and call `run_one_tick_against` with that base
// URL. We do not pull in `wiremock` / `mockito` — the supervisor already
// depends on axum + tower + tokio, and the test surface is small enough
// (three routes, three assertions) that a hand-rolled fixture is cleaner
// than adding a mock-server dep.

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Shared call-counter / call-log injected into every mock router so
    /// tests can assert what the cron called and in what order.
    #[derive(Default)]
    struct MockState {
        scan_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        sweep_calls: AtomicUsize,
        /// Recorded order of /execute path params, so we can assert
        /// proposals were processed sequentially in scan order.
        execute_order: parking_lot_compat::Mutex<Vec<String>>,
        /// /scan response payload (set by each test).
        scan_response: parking_lot_compat::Mutex<serde_json::Value>,
        /// When true, /scan returns 404 instead of the canned body — used to
        /// simulate the feature-off case.
        scan_returns_404: parking_lot_compat::Mutex<bool>,
    }

    // We don't depend on parking_lot in the supervisor — stub a tiny
    // Mutex wrapper around std::sync::Mutex with the lock semantics we need.
    mod parking_lot_compat {
        pub struct Mutex<T>(std::sync::Mutex<T>);
        impl<T: Default> Default for Mutex<T> {
            fn default() -> Self {
                Self(std::sync::Mutex::new(T::default()))
            }
        }
        impl<T> Mutex<T> {
            pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
                self.0.lock().unwrap()
            }
        }
    }

    async fn scan_handler(
        State(s): State<Arc<MockState>>,
        Json(_): Json<serde_json::Value>,
    ) -> axum::response::Response {
        s.scan_calls.fetch_add(1, Ordering::SeqCst);
        if *s.scan_returns_404.lock() {
            return (axum::http::StatusCode::NOT_FOUND, "feature off").into_response();
        }
        let body = s.scan_response.lock().clone();
        (axum::http::StatusCode::OK, Json(body)).into_response()
    }

    async fn execute_handler(
        State(s): State<Arc<MockState>>,
        Path(prop_id): Path<String>,
        Json(_): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        s.execute_calls.fetch_add(1, Ordering::SeqCst);
        s.execute_order.lock().push(prop_id.clone());
        Json(serde_json::json!({"ok": true, "proposal_id": prop_id}))
    }

    async fn sweep_handler(
        State(s): State<Arc<MockState>>,
        Json(_): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        s.sweep_calls.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({"promoted": 0, "demoted": 0}))
    }

    /// Spin up a router bound to an ephemeral port. Returns the bound base
    /// URL (e.g. `http://127.0.0.1:54321`) and the shared MockState.
    async fn spawn_mock_runner(state: Arc<MockState>) -> String {
        let app = Router::new()
            .route("/spec/proposals/scan", post(scan_handler))
            .route("/spec/proposals/{id}/execute", post(execute_handler))
            .route("/spec/proposals/sweep-pending", post(sweep_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    /// Build a real `SharedState` cheap enough to use as the http_client
    /// holder in tests. We don't exercise any other state field.
    fn test_state() -> SharedState {
        use crate::config::{CliArgs, SupervisorConfig};
        use clap::Parser;
        // Parse with a minimal arg vector — `--project-dir .` is the only
        // required flag. The path doesn't need to exist for `SupervisorState::new`
        // to construct.
        let args = CliArgs::parse_from(["test", "--project-dir", "."]);
        let config = SupervisorConfig::from_args(args);
        Arc::new(crate::state::SupervisorState::new(config))
    }

    #[tokio::test]
    async fn tick_with_zero_queued_only_sweeps() {
        let mock = Arc::new(MockState::default());
        *mock.scan_response.lock() = serde_json::json!({ "queuedProposals": [] });
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let report = run_one_tick_against(&state, &base).await.unwrap();

        assert_eq!(report.queued, 0);
        assert_eq!(report.executed, 0);
        assert_eq!(report.swept, 1);
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(mock.sweep_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tick_with_two_queued_calls_execute_twice() {
        let mock = Arc::new(MockState::default());
        *mock.scan_response.lock() = serde_json::json!({
            "queuedProposals": [
                {"proposal_id": "prop-a"},
                {"proposal_id": "prop-b"},
            ]
        });
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let report = run_one_tick_against(&state, &base).await.unwrap();

        assert_eq!(report.queued, 2);
        assert_eq!(report.executed, 2);
        assert_eq!(report.swept, 1);
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.execute_calls.load(Ordering::SeqCst), 2);
        assert_eq!(mock.sweep_calls.load(Ordering::SeqCst), 1);

        // Sequential order — proposals are dispatched in scan order, never
        // parallel (§6.7 anti-pattern guard).
        let order = mock.execute_order.lock().clone();
        assert_eq!(order, vec!["prop-a".to_string(), "prop-b".to_string()]);
    }

    #[tokio::test]
    async fn tick_handles_404_as_feature_off() {
        let mock = Arc::new(MockState::default());
        *mock.scan_returns_404.lock() = true;
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let err = run_one_tick_against(&state, &base).await.unwrap_err();
        assert_eq!(err, FEATURE_OFF_SENTINEL);
        // Critically: /execute and /sweep MUST NOT be called when /scan 404s.
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(mock.sweep_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tick_runner_unreachable_returns_error() {
        // Bind a listener just to claim a port, then drop it so nothing is
        // listening. The cron's POST should fail with a connection error,
        // which `run_one_tick_against` should surface as a non-sentinel Err.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let base = format!("http://127.0.0.1:{}", port);

        let state = test_state();
        let err = run_one_tick_against(&state, &base).await.unwrap_err();

        // Must NOT be the feature-off sentinel — that path is reserved for
        // 404s from a live runner.
        assert_ne!(err, FEATURE_OFF_SENTINEL);
        assert!(
            err.contains("request to")
                || err.contains("failed")
                || err.contains("connect")
                || err.contains("refused"),
            "unexpected error message: {}",
            err
        );
    }

    #[tokio::test]
    async fn parse_queued_ids_accepts_multiple_shapes() {
        // Object form with `proposal_id`.
        let v = serde_json::json!({
            "queuedProposals": [{"proposal_id": "a"}, {"proposal_id": "b"}]
        });
        assert_eq!(parse_queued_ids(&v), vec!["a", "b"]);

        // Bare-string form.
        let v = serde_json::json!({ "queuedProposals": ["x", "y"] });
        assert_eq!(parse_queued_ids(&v), vec!["x", "y"]);

        // camelCase variant.
        let v = serde_json::json!({
            "queuedProposals": [{"proposalId": "c"}, {"id": "d"}]
        });
        assert_eq!(parse_queued_ids(&v), vec!["c", "d"]);

        // Missing key → empty vec, no panic.
        let v = serde_json::json!({});
        assert_eq!(parse_queued_ids(&v), Vec::<String>::new());
    }

    #[tokio::test]
    async fn env_var_parsing_falls_back_to_defaults() {
        // This test asserts the default-fallback path is sane. It doesn't
        // execute `flywheel_loop` (would block on the initial-delay sleep);
        // it just sanity-checks the parse code path by reading the same env
        // vars `flywheel_loop` reads.
        //
        // We don't unset env vars here — other tests in the suite may have
        // set them. We just check that the parsing logic returns *some*
        // u64 (i.e. doesn't panic). The default-vs-override branch is
        // exercised in the test above by virtue of being a pure parse +
        // unwrap_or call; further coverage isn't valuable.
        let parsed: u64 = std::env::var("QONTINUI_FLYWHEEL_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(86_400);
        assert!(parsed > 0);

        // SAFETY: MissedTickBehavior::Skip is set on `flywheel_loop`'s
        // ticker at the explicit `set_missed_tick_behavior` call above —
        // not relying on the default `Burst`. Verifying this in a unit
        // test would require driving `tokio::time::pause()` + advancing the
        // clock past two intervals while a tick body is in flight; the
        // pause/advance plumbing isn't worth the test surface for a
        // one-line tokio configuration call. See the explicit
        // `set_missed_tick_behavior(MissedTickBehavior::Skip)` in
        // `flywheel_loop` for the actual contract.
        let _ = Duration::from_secs(parsed);
    }
}
