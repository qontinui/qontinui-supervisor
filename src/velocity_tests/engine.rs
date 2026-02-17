use chrono::Utc;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{error, info, warn};

use super::db::VelocityTestDb;
use super::tests::TEST_CASES;
use super::{VelocityTestResult, VelocityTestRun};
use crate::log_capture::{LogLevel, LogSource};
use crate::state::SharedState;

const WEB_FRONTEND_BASE: &str = "http://localhost:3001";
const ELEMENT_POLL_INTERVAL_MS: u64 = 500;
const ELEMENT_POLL_TIMEOUT_MS: u64 = 15_000;
const BETWEEN_TESTS_DELAY_MS: u64 = 1_000;
const BACKEND_API_BASE: &str = "http://localhost:8000";

/// Run all velocity tests sequentially.
pub async fn run_velocity_tests(
    db: Arc<VelocityTestDb>,
    state: SharedState,
    stop_rx: watch::Receiver<bool>,
) {
    let run_id = uuid::Uuid::new_v4().to_string();
    let total = TEST_CASES.len() as i64;

    let run = VelocityTestRun {
        id: run_id.clone(),
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
        overall_score: None,
        status: "running".to_string(),
        tests_total: total,
        tests_completed: 0,
    };

    if let Err(e) = db.insert_run(&run) {
        error!("Failed to create velocity test run: {}", e);
        let mut vt = state.velocity_tests.write().await;
        vt.running = false;
        vt.stop_tx = None;
        return;
    }

    // Update in-memory state (running flag already set by start_handler)
    {
        let mut vt = state.velocity_tests.write().await;
        vt.current_run_id = Some(run_id.clone());
        vt.current_test_index = 0;
        vt.total_tests = TEST_CASES.len();
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "Velocity tests started: run_id={}, tests={}",
                run_id,
                TEST_CASES.len()
            ),
        )
        .await;

    let http_client = state.http_client.clone();

    for (i, test_case) in TEST_CASES.iter().enumerate() {
        // Check for cancellation
        if *stop_rx.borrow() {
            info!(
                "Velocity tests cancelled at test {}/{}",
                i,
                TEST_CASES.len()
            );
            let _ = db.complete_run(&run_id, "stopped");
            break;
        }

        // Update progress
        {
            let mut vt = state.velocity_tests.write().await;
            vt.current_test_index = i;
        }

        info!(
            "Testing page {}/{}: {} ({})",
            i + 1,
            TEST_CASES.len(),
            test_case.name,
            test_case.page_url
        );

        let result = run_single_test(&http_client, &run_id, test_case).await;

        match &result {
            Ok(r) => {
                info!(
                    "  {} — score: {:.1}, load: {:.0}ms, errors: {}, element: {}",
                    test_case.name,
                    r.score.unwrap_or(0.0),
                    r.load_time_ms.unwrap_or(0.0),
                    r.console_errors,
                    r.element_found
                );
            }
            Err(e) => {
                warn!("  {} — failed: {}", test_case.name, e);
            }
        }

        // Build result (either from success or error)
        let db_result = match result {
            Ok(r) => r,
            Err(e) => VelocityTestResult {
                id: 0,
                run_id: run_id.clone(),
                test_name: test_case.name.to_string(),
                page_url: test_case.page_url.to_string(),
                load_time_ms: None,
                console_errors: 0,
                element_found: false,
                score: Some(0.0),
                error: Some(e.to_string()),
                tested_at: Utc::now().to_rfc3339(),
                api_response_time_ms: None,
                api_status_code: None,
                ttfb_ms: None,
                dom_interactive_ms: None,
                dom_complete_ms: None,
                fcp_ms: None,
                long_task_count: 0,
                long_task_total_ms: 0.0,
                resource_count: 0,
                total_transfer_size_bytes: 0,
                slowest_resource_ms: 0.0,
                bottleneck: None,
                diagnostics_json: None,
            },
        };

        let _ = db.insert_result(&db_result);
        let _ = db.update_run_progress(&run_id, (i + 1) as i64);

        // Delay between tests
        if i + 1 < TEST_CASES.len() {
            tokio::time::sleep(std::time::Duration::from_millis(BETWEEN_TESTS_DELAY_MS)).await;
        }
    }

    // Complete the run (unless cancelled above)
    if !*stop_rx.borrow() {
        let _ = db.complete_run(&run_id, "completed");
    }

    // Clear in-memory state
    {
        let mut vt = state.velocity_tests.write().await;
        vt.running = false;
        vt.current_run_id = None;
        vt.current_test_index = 0;
        vt.total_tests = 0;
    }

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Velocity tests completed: run_id={}", run_id),
        )
        .await;
}

/// Run a single test case: navigate, poll for element, collect diagnostics, compute score.
async fn run_single_test(
    http_client: &reqwest::Client,
    run_id: &str,
    test_case: &super::tests::TestCase,
) -> anyhow::Result<VelocityTestResult> {
    let now = Utc::now().to_rfc3339();

    // 1. Clear console errors before navigating
    let _ = http_client
        .post(format!(
            "{}/api/ui-bridge/control/console-errors/clear",
            WEB_FRONTEND_BASE
        ))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    // 2. Clear performance entries
    let _ = http_client
        .post(format!(
            "{}/api/ui-bridge/control/performance-entries/clear",
            WEB_FRONTEND_BASE
        ))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    // 3. Navigate to the page and start timer
    let nav_start = std::time::Instant::now();

    let nav_url = if test_case.page_url == "/" {
        WEB_FRONTEND_BASE.to_string()
    } else {
        format!("{}{}", WEB_FRONTEND_BASE, test_case.page_url)
    };

    let nav_resp = http_client
        .post(format!(
            "{}/api/ui-bridge/control/page/navigate",
            WEB_FRONTEND_BASE
        ))
        .json(&serde_json::json!({ "url": nav_url }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    if let Err(e) = &nav_resp {
        let err_str = e.to_string();
        if !err_str.contains("connection") && !err_str.contains("reset") && !err_str.contains("eof")
        {
            warn!("Navigate request error (may be expected): {}", e);
        }
    }

    // 4. Poll for key element to appear (this measures load time)
    let mut element_found = false;
    let deadline = nav_start + std::time::Duration::from_millis(ELEMENT_POLL_TIMEOUT_MS);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    loop {
        if std::time::Instant::now() > deadline {
            break;
        }

        if let Ok(elements) = get_elements(http_client).await {
            if has_key_element(&elements, test_case.key_element) {
                element_found = true;
                break;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(ELEMENT_POLL_INTERVAL_MS)).await;
    }

    let load_time_ms = nav_start.elapsed().as_secs_f64() * 1000.0;

    // 5. Let late resources finish loading
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 6. Get console errors
    let console_errors = get_console_error_count(http_client).await.unwrap_or(0);

    // 7. Measure backend API response time
    let (api_response_time_ms, api_status_code) =
        measure_api_response(http_client, test_case.api_endpoint).await;

    // 8. Get browser performance entries (navigation timing + resource waterfall)
    let perf_entries = get_performance_entries(http_client).await;

    // 9. Get long tasks from browser event capture
    let long_tasks = get_long_tasks(http_client).await;

    // Extract metrics from performance data
    let (ttfb_ms, dom_interactive_ms, dom_complete_ms, fcp_ms) =
        extract_navigation_timing(&perf_entries);
    let (resource_count, total_transfer_size_bytes, slowest_resource_ms) =
        extract_resource_stats(&perf_entries);
    let (long_task_count, long_task_total_ms) = extract_long_task_stats(&long_tasks);

    // 10. Classify bottleneck
    let bottleneck = classify_bottleneck(
        load_time_ms,
        api_response_time_ms,
        ttfb_ms,
        dom_interactive_ms,
        dom_complete_ms,
        long_task_count,
        long_task_total_ms,
        resource_count,
        total_transfer_size_bytes,
        slowest_resource_ms,
    );

    // Build diagnostics JSON blob (full resource list + long task list)
    let diagnostics_json = build_diagnostics_json(&perf_entries, &long_tasks);

    // 11. Compute score with new weights
    let score = compute_score(
        load_time_ms,
        api_response_time_ms,
        console_errors,
        element_found,
        long_task_count,
        resource_count,
        total_transfer_size_bytes,
    );

    Ok(VelocityTestResult {
        id: 0,
        run_id: run_id.to_string(),
        test_name: test_case.name.to_string(),
        page_url: test_case.page_url.to_string(),
        load_time_ms: Some(load_time_ms),
        console_errors,
        element_found,
        score: Some(score),
        error: None,
        tested_at: now,
        api_response_time_ms,
        api_status_code,
        ttfb_ms,
        dom_interactive_ms,
        dom_complete_ms,
        fcp_ms,
        long_task_count,
        long_task_total_ms,
        resource_count,
        total_transfer_size_bytes,
        slowest_resource_ms,
        bottleneck: Some(bottleneck),
        diagnostics_json,
    })
}

/// Fetch elements from the UI Bridge.
async fn get_elements(http_client: &reqwest::Client) -> anyhow::Result<serde_json::Value> {
    let resp = http_client
        .get(format!(
            "{}/api/ui-bridge/control/elements",
            WEB_FRONTEND_BASE
        ))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("Elements endpoint returned {}", resp.status());
    }

    let body: serde_json::Value = resp.json().await?;
    Ok(body)
}

/// Check if the key element is present in the elements response.
fn has_key_element(elements: &serde_json::Value, key: &str) -> bool {
    let key_lower = key.to_lowercase();

    // The response is { "success": true, "data": [...elements...] }
    if let Some(data) = elements.get("data").and_then(|d| d.as_array()) {
        for element in data {
            // Check id, label, type fields for the key substring
            if let Some(id) = element.get("id").and_then(|v| v.as_str()) {
                if id.to_lowercase().contains(&key_lower) {
                    return true;
                }
            }
            if let Some(label) = element.get("label").and_then(|v| v.as_str()) {
                if label.to_lowercase().contains(&key_lower) {
                    return true;
                }
            }
            if let Some(etype) = element.get("type").and_then(|v| v.as_str()) {
                if etype.to_lowercase().contains(&key_lower) {
                    return true;
                }
            }
            // Also check text_content in state
            if let Some(state) = element.get("state") {
                if let Some(tc) = state.get("text_content").and_then(|v| v.as_str()) {
                    if tc.to_lowercase().contains(&key_lower) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Get the count of console errors since we cleared them.
async fn get_console_error_count(http_client: &reqwest::Client) -> anyhow::Result<i64> {
    let resp = http_client
        .get(format!(
            "{}/api/ui-bridge/control/console-errors",
            WEB_FRONTEND_BASE
        ))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(0);
    }

    let body: serde_json::Value = resp.json().await?;

    // Response: { "success": true, "data": { "errors": [...], "count": N } }
    let count = body
        .get("data")
        .and_then(|d| d.get("count"))
        .and_then(|c| c.as_i64())
        .unwrap_or(0);

    Ok(count)
}

/// Compute page score (0-100) from metrics with diagnostic weights.
///
/// | Metric              | Weight | Scoring                                     |
/// |---------------------|--------|---------------------------------------------|
/// | Page load time      | 40%    | 40pts if <1s, linear decay to 0 at 10s      |
/// | API response time   | 15%    | 15pts if <200ms, linear decay to 0 at 3s    |
/// | Console errors      | 10%    | 10pts if 0 errors, -3pts per error (min 0)  |
/// | Element presence    | 15%    | 15pts if found, 0 if missing                |
/// | Long task penalty   | 10%    | 10pts if 0, -2pts per task (min 0)          |
/// | Resource efficiency | 10%    | 10pts if <20 resources and <1MB             |
fn compute_score(
    load_time_ms: f64,
    api_response_time_ms: Option<f64>,
    console_errors: i64,
    element_found: bool,
    long_task_count: i64,
    resource_count: i64,
    total_transfer_size_bytes: i64,
) -> f64 {
    // Load time score (0-40)
    let load_secs = load_time_ms / 1000.0;
    let load_score = if load_secs <= 1.0 {
        40.0
    } else if load_secs >= 10.0 {
        0.0
    } else {
        40.0 * (1.0 - (load_secs - 1.0) / 9.0)
    };

    // API response time score (0-15)
    let api_score = match api_response_time_ms {
        Some(ms) => {
            let secs = ms / 1000.0;
            if secs <= 0.2 {
                15.0
            } else if secs >= 3.0 {
                0.0
            } else {
                15.0 * (1.0 - (secs - 0.2) / 2.8)
            }
        }
        None => 7.5, // Neutral if we couldn't measure
    };

    // Console errors score (0-10)
    let error_score = (10.0 - (console_errors as f64 * 3.0)).max(0.0);

    // Element presence score (0-15)
    let element_score = if element_found { 15.0 } else { 0.0 };

    // Long task penalty (0-10)
    let long_task_score = (10.0 - (long_task_count as f64 * 2.0)).max(0.0);

    // Resource efficiency (0-10)
    let transfer_mb = total_transfer_size_bytes as f64 / (1024.0 * 1024.0);
    let resource_score = if resource_count <= 20 && transfer_mb <= 1.0 {
        10.0
    } else {
        let count_penalty = if resource_count > 20 {
            ((resource_count - 20) as f64 * 0.2).min(5.0)
        } else {
            0.0
        };
        let size_penalty = if transfer_mb > 1.0 {
            ((transfer_mb - 1.0) * 2.0).min(5.0)
        } else {
            0.0
        };
        (10.0 - count_penalty - size_penalty).max(0.0)
    };

    load_score + api_score + error_score + element_score + long_task_score + resource_score
}

// =============================================================================
// Diagnostic helpers
// =============================================================================

/// Measure direct backend API response time.
async fn measure_api_response(
    http_client: &reqwest::Client,
    api_endpoint: &str,
) -> (Option<f64>, Option<i64>) {
    let url = format!("{}{}", BACKEND_API_BASE, api_endpoint);
    let start = std::time::Instant::now();

    match http_client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(resp) => {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let status = resp.status().as_u16() as i64;
            (Some(elapsed_ms), Some(status))
        }
        Err(_) => (None, None),
    }
}

/// Fetch browser performance entries via UI Bridge.
async fn get_performance_entries(http_client: &reqwest::Client) -> Option<serde_json::Value> {
    let resp = http_client
        .get(format!(
            "{}/api/ui-bridge/control/performance-entries",
            WEB_FRONTEND_BASE
        ))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    // Response: { "success": true, "data": { "navigation": {...}, "resources": [...], "paint": [...] } }
    body.get("data").cloned()
}

/// Fetch long tasks from browser event capture via UI Bridge.
async fn get_long_tasks(http_client: &reqwest::Client) -> Option<serde_json::Value> {
    let resp = http_client
        .get(format!(
            "{}/api/ui-bridge/control/browser-events?type=long-task",
            WEB_FRONTEND_BASE
        ))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("data").cloned()
}

/// Extract navigation timing metrics from performance entries.
fn extract_navigation_timing(
    perf: &Option<serde_json::Value>,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    let nav = match perf {
        Some(p) => p.get("navigation"),
        None => return (None, None, None, None),
    };

    let nav = match nav {
        Some(n) => n,
        None => return (None, None, None, None),
    };

    let ttfb = nav.get("ttfbMs").and_then(|v| v.as_f64());
    let dom_interactive = nav.get("domInteractiveMs").and_then(|v| v.as_f64());
    let dom_complete = nav.get("domCompleteMs").and_then(|v| v.as_f64());

    // FCP from paint entries
    let fcp = perf
        .as_ref()
        .and_then(|p| p.get("paint"))
        .and_then(|p| p.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("first-contentful-paint"))
                .and_then(|e| e.get("startTime").and_then(|v| v.as_f64()))
        });

    (ttfb, dom_interactive, dom_complete, fcp)
}

/// Extract resource statistics from performance entries.
fn extract_resource_stats(perf: &Option<serde_json::Value>) -> (i64, i64, f64) {
    let resources = match perf {
        Some(p) => p.get("resources").and_then(|r| r.as_array()),
        None => return (0, 0, 0.0),
    };

    let resources = match resources {
        Some(r) => r,
        None => return (0, 0, 0.0),
    };

    let count = resources.len() as i64;
    let total_bytes: i64 = resources
        .iter()
        .filter_map(|r| r.get("transferSize").and_then(|v| v.as_i64()))
        .sum();
    let slowest: f64 = resources
        .iter()
        .filter_map(|r| r.get("duration").and_then(|v| v.as_f64()))
        .fold(0.0_f64, f64::max);

    (count, total_bytes, slowest)
}

/// Extract long task statistics from browser events.
fn extract_long_task_stats(events: &Option<serde_json::Value>) -> (i64, f64) {
    let events_arr = match events {
        Some(e) => e.get("events").and_then(|ev| ev.as_array()),
        None => return (0, 0.0),
    };

    let events_arr = match events_arr {
        Some(a) => a,
        None => return (0, 0.0),
    };

    let count = events_arr.len() as i64;
    let total_ms: f64 = events_arr
        .iter()
        .filter_map(|e| e.get("duration").and_then(|v| v.as_f64()))
        .sum();

    (count, total_ms)
}

/// Classify the primary bottleneck for a page load.
#[allow(clippy::too_many_arguments)]
fn classify_bottleneck(
    load_time_ms: f64,
    api_ms: Option<f64>,
    ttfb_ms: Option<f64>,
    dom_interactive_ms: Option<f64>,
    dom_complete_ms: Option<f64>,
    long_task_count: i64,
    long_task_total_ms: f64,
    resource_count: i64,
    total_transfer_bytes: i64,
    slowest_resource_ms: f64,
) -> String {
    // 1. Backend slow
    if let Some(api) = api_ms {
        if api > 500.0 || (load_time_ms > 0.0 && api / load_time_ms > 0.6) {
            return "Backend Slow".to_string();
        }
    }

    // 2. JS blocking
    if long_task_total_ms > 200.0 || long_task_count > 3 {
        return "JS Blocking".to_string();
    }

    // 3. Bundle heavy
    let transfer_mb = total_transfer_bytes as f64 / (1024.0 * 1024.0);
    if transfer_mb > 2.0 || resource_count > 50 {
        return "Bundle Heavy".to_string();
    }

    // 4. TTFB slow
    if let Some(ttfb) = ttfb_ms {
        if ttfb > 600.0 {
            return "TTFB Slow".to_string();
        }
    }

    // 5. Render slow
    if let (Some(di), Some(dc)) = (dom_interactive_ms, dom_complete_ms) {
        if dc - di > 500.0 {
            return "Render Slow".to_string();
        }
    }

    // 6. Network slow
    if slowest_resource_ms > 2000.0 {
        return "Network Slow".to_string();
    }

    "Healthy".to_string()
}

/// Build a JSON blob with full resource waterfall and long task list for detail view.
fn build_diagnostics_json(
    perf: &Option<serde_json::Value>,
    long_tasks: &Option<serde_json::Value>,
) -> Option<String> {
    let mut diag = serde_json::Map::new();

    if let Some(p) = perf {
        if let Some(resources) = p.get("resources") {
            diag.insert("resources".to_string(), resources.clone());
        }
        if let Some(nav) = p.get("navigation") {
            diag.insert("navigation".to_string(), nav.clone());
        }
        if let Some(paint) = p.get("paint") {
            diag.insert("paint".to_string(), paint.clone());
        }
    }

    if let Some(lt) = long_tasks {
        if let Some(events) = lt.get("events") {
            diag.insert("longTasks".to_string(), events.clone());
        }
    }

    if diag.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(diag).to_string())
    }
}
