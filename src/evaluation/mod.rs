pub mod db;
pub mod engine;
pub mod judge;
pub mod queries;

use serde::{Deserialize, Serialize};

// ============================================================================
// Shared data model structs
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestPrompt {
    pub id: String,
    pub prompt: String,
    pub category: String,
    pub complexity: String,
    pub expected_phases: Option<Vec<String>>,
    pub expected_step_types: Option<Vec<String>>,
    pub tags: Option<Vec<String>>,
    pub ground_truth_json: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunSummary {
    pub id: String,
    pub mode: String,
    pub status: String,
    pub prompts_total: i64,
    pub prompts_completed: i64,
    // Combined averages (all prompts)
    pub avg_overall_score: Option<f64>,
    pub avg_structural: Option<f64>,
    pub avg_command_accuracy: Option<f64>,
    pub avg_phase_flow: Option<f64>,
    pub avg_step_completeness: Option<f64>,
    pub avg_prompt_quality: Option<f64>,
    pub avg_determinism: Option<f64>,
    // Ground-truth prompt averages (gt-* prompts only)
    pub gt_avg_overall: Option<f64>,
    pub gt_avg_structural: Option<f64>,
    pub gt_avg_command_accuracy: Option<f64>,
    pub gt_avg_phase_flow: Option<f64>,
    pub gt_avg_step_completeness: Option<f64>,
    pub gt_avg_prompt_quality: Option<f64>,
    pub gt_avg_determinism: Option<f64>,
    pub gt_count: Option<i64>,
    // Generic prompt averages (non-gt prompts)
    pub gen_avg_overall: Option<f64>,
    pub gen_avg_structural: Option<f64>,
    pub gen_avg_command_accuracy: Option<f64>,
    pub gen_avg_phase_flow: Option<f64>,
    pub gen_avg_step_completeness: Option<f64>,
    pub gen_avg_prompt_quality: Option<f64>,
    pub gen_avg_determinism: Option<f64>,
    pub gen_count: Option<i64>,

    pub error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    pub id: i64,
    pub run_id: String,
    pub test_prompt_id: String,
    pub generated_workflow_json: Option<String>,
    pub task_run_id: Option<String>,
    pub workflow_id: Option<String>,

    // Scores (1-5)
    pub structural_correctness: Option<i64>,
    pub command_accuracy: Option<i64>,
    pub phase_flow_logic: Option<i64>,
    pub step_completeness: Option<i64>,
    pub prompt_quality: Option<i64>,
    pub determinism: Option<i64>,
    pub overall_score: Option<f64>,

    pub score_rationales: Option<String>,

    pub generation_error: Option<String>,
    pub scoring_error: Option<String>,

    pub generation_duration_ms: Option<i64>,
    pub scoring_duration_ms: Option<i64>,

    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunWithResults {
    #[serde(flatten)]
    pub run: EvalRunSummary,
    pub results: Vec<EvalResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalStatus {
    pub running: bool,
    pub current_run_id: Option<String>,
    pub continuous_mode: bool,
    pub continuous_interval_secs: u64,
    pub current_prompt_index: usize,
    pub total_prompts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareReport {
    pub current_run_id: String,
    pub baseline_run_id: String,
    pub per_prompt: Vec<PromptComparison>,
    pub aggregate: AggregateDelta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptComparison {
    pub test_prompt_id: String,
    pub baseline_overall: Option<f64>,
    pub current_overall: Option<f64>,
    pub delta: Option<f64>,
    pub regression: bool,
    pub improvement: bool,
    pub dimension_deltas: DimensionDeltas,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionDeltas {
    pub structural_correctness: Option<f64>,
    pub command_accuracy: Option<f64>,
    pub phase_flow_logic: Option<f64>,
    pub step_completeness: Option<f64>,
    pub prompt_quality: Option<f64>,
    pub determinism: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateDelta {
    pub avg_overall_delta: Option<f64>,
    pub regressions: usize,
    pub improvements: usize,
    pub unchanged: usize,
}

/// Scores parsed from the LLM judge response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreResponse {
    pub structural_correctness: DimensionScore,
    pub command_accuracy: DimensionScore,
    pub phase_flow_logic: DimensionScore,
    pub step_completeness: DimensionScore,
    pub prompt_quality: DimensionScore,
    pub determinism: DimensionScore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionScore {
    pub score: i64,
    pub rationale: String,
}

impl ScoreResponse {
    pub fn overall(&self) -> f64 {
        let sum = self.structural_correctness.score
            + self.command_accuracy.score
            + self.phase_flow_logic.score
            + self.step_completeness.score
            + self.prompt_quality.score
            + self.determinism.score;
        sum as f64 / 6.0
    }
}
