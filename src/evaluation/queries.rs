use rusqlite::params;

use super::db::EvalDb;
use super::{AggregateDelta, CompareReport, DimensionDeltas, EvalRunSummary, PromptComparison};

/// List all eval runs, most recent first.
pub fn list_runs(db: &EvalDb) -> anyhow::Result<Vec<EvalRunSummary>> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, mode, status, prompts_total, prompts_completed,
                avg_overall_score, avg_structural, avg_command_accuracy, avg_phase_flow,
                avg_step_completeness, avg_prompt_quality, avg_determinism,
                gt_avg_overall, gt_avg_structural, gt_avg_command_accuracy, gt_avg_phase_flow,
                gt_avg_step_completeness, gt_avg_prompt_quality, gt_avg_determinism, gt_count,
                gen_avg_overall, gen_avg_structural, gen_avg_command_accuracy, gen_avg_phase_flow,
                gen_avg_step_completeness, gen_avg_prompt_quality, gen_avg_determinism, gen_count,
                error, started_at, completed_at
         FROM eval_runs ORDER BY started_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EvalRunSummary {
            id: row.get(0)?,
            mode: row.get(1)?,
            status: row.get(2)?,
            prompts_total: row.get(3)?,
            prompts_completed: row.get(4)?,
            avg_overall_score: row.get(5)?,
            avg_structural: row.get(6)?,
            avg_command_accuracy: row.get(7)?,
            avg_phase_flow: row.get(8)?,
            avg_step_completeness: row.get(9)?,
            avg_prompt_quality: row.get(10)?,
            avg_determinism: row.get(11)?,
            gt_avg_overall: row.get(12)?,
            gt_avg_structural: row.get(13)?,
            gt_avg_command_accuracy: row.get(14)?,
            gt_avg_phase_flow: row.get(15)?,
            gt_avg_step_completeness: row.get(16)?,
            gt_avg_prompt_quality: row.get(17)?,
            gt_avg_determinism: row.get(18)?,
            gt_count: row.get(19)?,
            gen_avg_overall: row.get(20)?,
            gen_avg_structural: row.get(21)?,
            gen_avg_command_accuracy: row.get(22)?,
            gen_avg_phase_flow: row.get(23)?,
            gen_avg_step_completeness: row.get(24)?,
            gen_avg_prompt_quality: row.get(25)?,
            gen_avg_determinism: row.get(26)?,
            gen_count: row.get(27)?,
            error: row.get(28)?,
            started_at: row.get(29)?,
            completed_at: row.get(30)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

/// Compare two runs by matching results on test_prompt_id.
pub fn compare_runs(
    db: &EvalDb,
    current_id: &str,
    baseline_id: &str,
) -> anyhow::Result<CompareReport> {
    let conn = db.conn();

    // Fetch results for both runs keyed by prompt id
    let mut stmt = conn.prepare(
        "SELECT test_prompt_id, overall_score, structural_correctness, command_accuracy,
                phase_flow_logic, step_completeness, prompt_quality, determinism
         FROM eval_results WHERE run_id=?1",
    )?;

    let baseline_results: std::collections::HashMap<String, (Option<f64>, Vec<Option<i64>>)> = {
        let rows = stmt.query_map(params![baseline_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<f64>>(1)?,
                vec![
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ],
            ))
        })?;
        rows.filter_map(|r| r.ok())
            .map(|(id, overall, dims)| (id, (overall, dims)))
            .collect()
    };

    let current_results: Vec<(String, Option<f64>, Vec<Option<i64>>)> = {
        let rows = stmt.query_map(params![current_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<f64>>(1)?,
                vec![
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ],
            ))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let mut per_prompt = Vec::new();
    let mut regressions = 0usize;
    let mut improvements = 0usize;
    let mut unchanged = 0usize;

    for (prompt_id, current_overall, current_dims) in &current_results {
        let (baseline_overall, baseline_dims) = baseline_results
            .get(prompt_id)
            .cloned()
            .unwrap_or((None, vec![None; 6]));

        let delta = match (*current_overall, baseline_overall) {
            (Some(c), Some(b)) => Some(c - b),
            _ => None,
        };

        let regression = delta.map(|d| d <= -1.0).unwrap_or(false);
        let improvement = delta.map(|d| d >= 1.0).unwrap_or(false);

        if regression {
            regressions += 1;
        } else if improvement {
            improvements += 1;
        } else {
            unchanged += 1;
        }

        let dim_delta = |idx: usize| -> Option<f64> {
            match (current_dims[idx], baseline_dims[idx]) {
                (Some(c), Some(b)) => Some(c as f64 - b as f64),
                _ => None,
            }
        };

        per_prompt.push(PromptComparison {
            test_prompt_id: prompt_id.clone(),
            baseline_overall,
            current_overall: *current_overall,
            delta,
            regression,
            improvement,
            dimension_deltas: DimensionDeltas {
                structural_correctness: dim_delta(0),
                command_accuracy: dim_delta(1),
                phase_flow_logic: dim_delta(2),
                step_completeness: dim_delta(3),
                prompt_quality: dim_delta(4),
                determinism: dim_delta(5),
            },
        });
    }

    let avg_overall_delta = {
        let deltas: Vec<f64> = per_prompt.iter().filter_map(|p| p.delta).collect();
        if deltas.is_empty() {
            None
        } else {
            Some(deltas.iter().sum::<f64>() / deltas.len() as f64)
        }
    };

    Ok(CompareReport {
        current_run_id: current_id.to_string(),
        baseline_run_id: baseline_id.to_string(),
        per_prompt,
        aggregate: AggregateDelta {
            avg_overall_delta,
            regressions,
            improvements,
            unchanged,
        },
    })
}
