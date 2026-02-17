pub mod db;
pub mod engine;
pub mod tests;

use serde::{Deserialize, Serialize};

// ============================================================================
// Data model
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityTestRun {
    pub id: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub overall_score: Option<f64>,
    pub status: String, // running, completed, failed, stopped
    pub tests_total: i64,
    pub tests_completed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityTestResult {
    pub id: i64,
    pub run_id: String,
    pub test_name: String,
    pub page_url: String,
    pub load_time_ms: Option<f64>,
    pub console_errors: i64,
    pub element_found: bool,
    pub score: Option<f64>,
    pub error: Option<String>,
    pub tested_at: String,
    // Diagnostic fields
    pub api_response_time_ms: Option<f64>,
    pub api_status_code: Option<i64>,
    pub ttfb_ms: Option<f64>,
    pub dom_interactive_ms: Option<f64>,
    pub dom_complete_ms: Option<f64>,
    pub fcp_ms: Option<f64>,
    pub long_task_count: i64,
    pub long_task_total_ms: f64,
    pub resource_count: i64,
    pub total_transfer_size_bytes: i64,
    pub slowest_resource_ms: f64,
    pub bottleneck: Option<String>,
    pub diagnostics_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityTestStatus {
    pub running: bool,
    pub current_run_id: Option<String>,
    pub current_test_index: usize,
    pub total_tests: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityTestRunWithResults {
    #[serde(flatten)]
    pub run: VelocityTestRun,
    pub results: Vec<VelocityTestResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VelocityTestTrendPoint {
    pub run_id: String,
    pub started_at: String,
    pub overall_score: Option<f64>,
}
