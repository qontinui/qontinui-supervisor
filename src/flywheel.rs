//! Flywheel cron — drives the spec coverage-growth loop on a nightly cadence.
//!
//! spec-multi-app Stream E.4: the cron now iterates the runner's app registry
//! (`GET /apps`) and runs the full sequence per-app:
//!   1. POST /apps/<app_id>/spec/proposals/scan
//!   2. POST /apps/<app_id>/spec/proposals/<id>/execute  for every queued row
//!   3. POST /apps/<app_id>/spec/proposals/sweep-pending
//!
//! Per-app failures `warn` and the loop continues — a flaky app must not
//! starve its siblings.
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
//! Env gate: `QONTINUI_FLYWHEEL_ENABLED=1` is honored by the caller spawning
//! [`flywheel_loop`]; this module assumes the gate has already been checked.
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

use qontinui_types::apps::{App, AppListResponse};
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

/// Per-tick HTTP request timeout for the `GET /apps` registry list. Short —
/// the registry is a single PG SELECT.
const APPS_LIST_TIMEOUT_SECS: u64 = 30;

/// Sentinel error string returned by [`http_post_json`] when the runner
/// responds with 404 — interpreted as "spec-authoring feature is off or the
/// app id is unknown; warn-once and continue".
const FEATURE_OFF_SENTINEL: &str = "feature-off";

/// Counts surfaced from a single tick for tests + logging.
///
/// spec-multi-app Stream E.4: counts are summed across every app the tick
/// processed. Per-app breakdown is captured via `tracing::info!` spans.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Number of apps the registry returned.
    pub apps_seen: usize,
    /// Number of apps for which `/scan` succeeded.
    pub apps_scanned: usize,
    /// Total queued proposal ids across all apps.
    pub queued: usize,
    /// Total `/execute` calls that returned 2xx across all apps.
    pub executed: usize,
    /// Number of apps for which `/sweep-pending` returned 2xx.
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
                    "flywheel: tick complete — apps_seen={}, apps_scanned={}, queued={}, \
                     executed={}, swept={}",
                    report.apps_seen,
                    report.apps_scanned,
                    report.queued,
                    report.executed,
                    report.swept,
                );
            }
            Err(e) if e == FEATURE_OFF_SENTINEL => {
                if !feature_off_warned {
                    tracing::warn!(
                        "flywheel: runner returned 404 on /apps — spec-authoring \
                         feature is off (or runner not reachable). Sleeping until \
                         the next tick. (This warning is logged once per off-state run.)"
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

/// Run one complete tick: list apps → for each, scan → execute every queued
/// proposal → sweep.
///
/// Returns the count of apps/queued/executed/swept rows on success. Returns
/// [`FEATURE_OFF_SENTINEL`] if the initial `/apps` returned 404 (interpreted
/// as feature-off or runner not yet reachable).
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

    // 0. List apps. A 404 here is the feature-off signal — propagate the
    //    sentinel so the caller can warn-once and skip the rest of this tick.
    let apps_url = format!("{}/apps", base);
    let apps = match http_get_apps(client, &apps_url).await {
        Ok(list) => list,
        Err(e) if e == FEATURE_OFF_SENTINEL => return Err(FEATURE_OFF_SENTINEL.to_string()),
        Err(e) => return Err(format!("list apps failed: {}", e)),
    };
    report.apps_seen = apps.len();
    tracing::info!("flywheel: registry returned {} app(s) to scan", apps.len());

    // 1..3. Per-app scan → execute → sweep. Per-app failures are logged at
    // warn! and the loop continues — one flaky app must not starve the
    // siblings.
    for app in apps {
        match run_one_tick_for_app(state, base, &app).await {
            Ok(per_app) => {
                report.apps_scanned += 1;
                report.queued += per_app.queued;
                report.executed += per_app.executed;
                report.swept += per_app.swept;
                tracing::info!(
                    app_id = %app.app_id,
                    queued = per_app.queued,
                    executed = per_app.executed,
                    swept = per_app.swept,
                    "flywheel: app tick ok"
                );
            }
            Err(e) => {
                tracing::warn!(
                    app_id = %app.app_id,
                    error = %e,
                    "flywheel: app tick failed (continuing with next app)"
                );
            }
        }
    }

    Ok(report)
}

/// Per-app counts surfaced from one app's sweep. Aggregated into the
/// cross-app `TickReport` by `run_one_tick_against`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PerAppCounts {
    queued: usize,
    executed: usize,
    swept: usize,
}

/// Run the scan → execute → sweep sequence for a single registered app.
///
/// On any 4xx/5xx for `/scan`, returns an error and the caller skips
/// `/execute` + `/sweep` for this app. Per-proposal `/execute` failures are
/// logged inline and do NOT abort the app's sweep (so a transient AI failure
/// on proposal #2 doesn't starve proposals #3..N). `/sweep` failures are
/// logged but never bubble up (the next tick retries).
async fn run_one_tick_for_app(
    state: &SharedState,
    base: &str,
    app: &App,
) -> Result<PerAppCounts, String> {
    let client = &state.http_client;
    let mut counts = PerAppCounts::default();
    let app_id = &app.app_id;

    // 1. Scan.
    let scan_url = format!("{}/apps/{}/spec/proposals/scan", base, app_id);
    let scan_res = http_post_json(client, &scan_url, &serde_json::json!({})).await?;
    let queued = parse_queued_ids(&scan_res);
    counts.queued = queued.len();
    tracing::info!(
        app_id = %app_id,
        "flywheel: scan returned {} queued proposal(s)",
        queued.len()
    );

    // 2. Execute every queued proposal — SEQUENTIAL (§6.7 anti-pattern guard).
    for prop_id in &queued {
        let url = format!(
            "{}/apps/{}/spec/proposals/{}/execute",
            base, app_id, prop_id
        );
        match http_post_json(client, &url, &serde_json::json!({})).await {
            Ok(_) => {
                counts.executed += 1;
                tracing::info!(
                    app_id = %app_id,
                    proposal_id = %prop_id,
                    "flywheel: executed proposal"
                );
            }
            Err(e) => {
                tracing::warn!(
                    app_id = %app_id,
                    proposal_id = %prop_id,
                    error = %e,
                    "flywheel: execute proposal failed (continuing)"
                );
            }
        }
    }

    // 3. Sweep — re-run gate-3 against every pendingPromotion row for this app.
    let sweep_url = format!("{}/apps/{}/spec/proposals/sweep-pending", base, app_id);
    match http_post_json(client, &sweep_url, &serde_json::json!({})).await {
        Ok(_) => {
            counts.swept = 1;
            tracing::info!(app_id = %app_id, "flywheel: sweep-pending completed");
        }
        Err(e) if e == FEATURE_OFF_SENTINEL => {
            tracing::warn!(
                app_id = %app_id,
                "flywheel: sweep-pending returned 404 (feature off mid-tick?)"
            );
        }
        Err(e) => {
            tracing::warn!(
                app_id = %app_id,
                error = %e,
                "flywheel: sweep-pending failed (continuing)"
            );
        }
    }

    Ok(counts)
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

/// GET `/apps` and parse the canonical [`AppListResponse`] shape. A 404 maps
/// to [`FEATURE_OFF_SENTINEL`] (same convention as `http_post_json`) so the
/// caller can warn-once and skip the tick.
async fn http_get_apps(client: &reqwest::Client, url: &str) -> Result<Vec<App>, String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(APPS_LIST_TIMEOUT_SECS))
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
    let body: AppListResponse = resp
        .json()
        .await
        .map_err(|e| format!("parse AppListResponse from {} failed: {}", url, e))?;
    Ok(body.apps)
}

/// POST a JSON body to `url`. Returns the parsed JSON response body on 2xx.
///
/// Error mapping:
/// - 404 → `Err(FEATURE_OFF_SENTINEL.to_string())` — interpreted as the
///   spec-authoring feature being compiled out on the runner OR the app id
///   being unknown to the registry.
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
// (four routes, a handful of assertions) that a hand-rolled fixture is
// cleaner than adding a mock-server dep.

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Shared call-counter / call-log injected into every mock router so
    /// tests can assert what the cron called and in what order.
    #[derive(Default)]
    struct MockState {
        apps_calls: AtomicUsize,
        scan_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        sweep_calls: AtomicUsize,
        /// Recorded order of /execute path params, so we can assert
        /// proposals were processed sequentially in scan order.
        execute_order: parking_lot_compat::Mutex<Vec<String>>,
        /// Recorded order of /scan path apps, so we can assert apps were
        /// processed in registry order.
        scan_order: parking_lot_compat::Mutex<Vec<String>>,
        /// /apps response payload (set by each test).
        apps_response: parking_lot_compat::Mutex<Vec<App>>,
        /// /scan response payload per-app (set by each test).
        scan_response: parking_lot_compat::Mutex<serde_json::Value>,
        /// Apps that should return 500 on /scan to exercise the
        /// per-app-failure-doesn't-abort path.
        scan_fails_for: parking_lot_compat::Mutex<HashSet<String>>,
        /// When true, /apps returns 404 instead of the canned body — used to
        /// simulate the feature-off case.
        apps_returns_404: parking_lot_compat::Mutex<bool>,
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

    async fn apps_handler(State(s): State<Arc<MockState>>) -> axum::response::Response {
        s.apps_calls.fetch_add(1, Ordering::SeqCst);
        if *s.apps_returns_404.lock() {
            return (axum::http::StatusCode::NOT_FOUND, "feature off").into_response();
        }
        let apps = s.apps_response.lock().clone();
        let resp = AppListResponse { ok: true, apps };
        (axum::http::StatusCode::OK, Json(resp)).into_response()
    }

    async fn scan_handler(
        State(s): State<Arc<MockState>>,
        Path(app_id): Path<String>,
        Json(_): Json<serde_json::Value>,
    ) -> axum::response::Response {
        s.scan_calls.fetch_add(1, Ordering::SeqCst);
        s.scan_order.lock().push(app_id.clone());
        if s.scan_fails_for.lock().contains(&app_id) {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "synthetic scan failure",
            )
                .into_response();
        }
        let body = s.scan_response.lock().clone();
        (axum::http::StatusCode::OK, Json(body)).into_response()
    }

    async fn execute_handler(
        State(s): State<Arc<MockState>>,
        Path((_app_id, prop_id)): Path<(String, String)>,
        Json(_): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        s.execute_calls.fetch_add(1, Ordering::SeqCst);
        s.execute_order.lock().push(prop_id.clone());
        Json(serde_json::json!({"ok": true, "proposal_id": prop_id}))
    }

    async fn sweep_handler(
        State(s): State<Arc<MockState>>,
        Path(_app_id): Path<String>,
        Json(_): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        s.sweep_calls.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({"promoted": 0, "demoted": 0}))
    }

    /// Spin up a router bound to an ephemeral port. Returns the bound base
    /// URL (e.g. `http://127.0.0.1:54321`) and the shared MockState.
    async fn spawn_mock_runner(state: Arc<MockState>) -> String {
        let app = Router::new()
            .route("/apps", get(apps_handler))
            .route("/apps/{app_id}/spec/proposals/scan", post(scan_handler))
            .route(
                "/apps/{app_id}/spec/proposals/{id}/execute",
                post(execute_handler),
            )
            .route(
                "/apps/{app_id}/spec/proposals/sweep-pending",
                post(sweep_handler),
            )
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

    fn mk_app(app_id: &str) -> App {
        App {
            app_id: app_id.to_string(),
            repo_root: format!("/tmp/{}", app_id),
            ui_bridge_url: format!("http://localhost:0/{}", app_id),
            display_name: format!("Mock {}", app_id),
            created_at_ms: 0,
            last_seen_at_ms: 0,
            auth_required: false,
            red_threshold: 0.5,
            yellow_threshold: 0.8,
        }
    }

    #[tokio::test]
    async fn tick_with_empty_app_list_does_nothing() {
        let mock = Arc::new(MockState::default());
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let report = run_one_tick_against(&state, &base).await.unwrap();

        assert_eq!(report.apps_seen, 0);
        assert_eq!(report.apps_scanned, 0);
        assert_eq!(report.queued, 0);
        assert_eq!(report.executed, 0);
        assert_eq!(report.swept, 0);
        assert_eq!(mock.apps_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tick_with_one_app_and_zero_queued_only_sweeps() {
        let mock = Arc::new(MockState::default());
        *mock.apps_response.lock() = vec![mk_app("qontinui-runner")];
        *mock.scan_response.lock() = serde_json::json!({ "queuedProposals": [] });
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let report = run_one_tick_against(&state, &base).await.unwrap();

        assert_eq!(report.apps_seen, 1);
        assert_eq!(report.apps_scanned, 1);
        assert_eq!(report.queued, 0);
        assert_eq!(report.executed, 0);
        assert_eq!(report.swept, 1);
        assert_eq!(mock.apps_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(mock.sweep_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tick_with_two_queued_calls_execute_twice() {
        let mock = Arc::new(MockState::default());
        *mock.apps_response.lock() = vec![mk_app("qontinui-runner")];
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
    async fn tick_handles_404_apps_as_feature_off() {
        let mock = Arc::new(MockState::default());
        *mock.apps_returns_404.lock() = true;
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let err = run_one_tick_against(&state, &base).await.unwrap_err();
        assert_eq!(err, FEATURE_OFF_SENTINEL);
        // Critically: /scan, /execute and /sweep MUST NOT be called when /apps 404s.
        assert_eq!(mock.apps_calls.load(Ordering::SeqCst), 1);
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 0);
        assert_eq!(mock.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(mock.sweep_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tick_runner_unreachable_returns_error() {
        // Bind a listener just to claim a port, then drop it so nothing is
        // listening. The cron's GET /apps should fail with a connection
        // error, which `run_one_tick_against` should surface as a
        // non-sentinel Err.
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

    /// Stream E.9 test #5: cron iterates the registry and a 5xx on one
    /// app's `/scan` does NOT abort the tick — siblings still run.
    #[tokio::test]
    async fn cron_iterates_app_list_and_continues_on_per_app_failure() {
        let mock = Arc::new(MockState::default());
        *mock.apps_response.lock() = vec![
            mk_app("qontinui-runner"),
            mk_app("flaky-app"),
            mk_app("qontinui-web"),
        ];
        *mock.scan_response.lock() = serde_json::json!({ "queuedProposals": [] });
        mock.scan_fails_for.lock().insert("flaky-app".to_string());
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let report = run_one_tick_against(&state, &base).await.unwrap();

        // All three apps were visited (scan attempted), but only the two
        // healthy ones completed the full sequence.
        assert_eq!(report.apps_seen, 3);
        assert_eq!(report.apps_scanned, 2, "flaky-app should fail at scan");
        assert_eq!(report.swept, 2, "only healthy apps sweep");
        assert_eq!(mock.scan_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            mock.sweep_calls.load(Ordering::SeqCst),
            2,
            "flaky-app never sweeps"
        );

        // Apps were iterated in registry order.
        let order = mock.scan_order.lock().clone();
        assert_eq!(
            order,
            vec![
                "qontinui-runner".to_string(),
                "flaky-app".to_string(),
                "qontinui-web".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn tick_multiple_apps_aggregates_counts() {
        let mock = Arc::new(MockState::default());
        *mock.apps_response.lock() = vec![mk_app("qontinui-runner"), mk_app("qontinui-web")];
        *mock.scan_response.lock() = serde_json::json!({
            "queuedProposals": [{"proposal_id": "p1"}]
        });
        let base = spawn_mock_runner(mock.clone()).await;
        let state = test_state();

        let report = run_one_tick_against(&state, &base).await.unwrap();

        // Each app gets one queued proposal → 2 queued + 2 executed + 2 swept.
        assert_eq!(report.apps_seen, 2);
        assert_eq!(report.apps_scanned, 2);
        assert_eq!(report.queued, 2);
        assert_eq!(report.executed, 2);
        assert_eq!(report.swept, 2);
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
        let parsed: u64 = std::env::var("QONTINUI_FLYWHEEL_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(86_400);
        assert!(parsed > 0);

        let _ = Duration::from_secs(parsed);
    }
}
