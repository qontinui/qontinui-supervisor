use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{EvalResult, EvalRunSummary, TestPrompt};

pub struct EvalDb {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl EvalDb {
    pub fn new(dev_logs_dir: &Path) -> anyhow::Result<Self> {
        let db_path = dev_logs_dir.join("eval-benchmark.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn: Mutex::new(conn),
            db_path,
        };
        db.init_schema()?;
        db.seed_defaults()?;
        Ok(db)
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS test_prompts (
                id TEXT PRIMARY KEY,
                prompt TEXT NOT NULL,
                category TEXT NOT NULL,
                complexity TEXT NOT NULL DEFAULT 'medium',
                expected_phases TEXT,
                expected_step_types TEXT,
                tags TEXT,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS eval_runs (
                id TEXT PRIMARY KEY,
                mode TEXT NOT NULL,
                status TEXT NOT NULL,
                prompts_total INTEGER NOT NULL DEFAULT 0,
                prompts_completed INTEGER NOT NULL DEFAULT 0,
                avg_overall_score REAL,
                avg_structural REAL,
                avg_command_accuracy REAL,
                avg_phase_flow REAL,
                avg_step_completeness REAL,
                avg_prompt_quality REAL,
                avg_determinism REAL,
                error TEXT,
                started_at TEXT NOT NULL,
                completed_at TEXT
            );

            CREATE TABLE IF NOT EXISTS eval_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES eval_runs(id) ON DELETE CASCADE,
                test_prompt_id TEXT NOT NULL,
                generated_workflow_json TEXT,
                task_run_id TEXT,
                workflow_id TEXT,

                structural_correctness INTEGER,
                command_accuracy INTEGER,
                phase_flow_logic INTEGER,
                step_completeness INTEGER,
                prompt_quality INTEGER,
                determinism INTEGER,
                overall_score REAL,

                score_rationales TEXT,

                generation_error TEXT,
                scoring_error TEXT,

                generation_duration_ms INTEGER,
                scoring_duration_ms INTEGER,

                started_at TEXT NOT NULL,
                completed_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_er_run_id ON eval_results(run_id);
            CREATE INDEX IF NOT EXISTS idx_er_prompt_id ON eval_results(test_prompt_id);
            CREATE INDEX IF NOT EXISTS idx_er_overall ON eval_results(overall_score);
            CREATE INDEX IF NOT EXISTS idx_runs_started ON eval_runs(started_at);
            CREATE INDEX IF NOT EXISTS idx_runs_status ON eval_runs(status);
        ",
        )?;
        Ok(())
    }

    fn seed_defaults(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM test_prompts", [], |row| row.get(0))?;
        if count > 0 {
            return Ok(());
        }

        let now = Utc::now().to_rfc3339();
        let seeds: &[(&str, &str, &str, &str)] = &[
            (
                "lint-python-basic",
                "Run Python linting on the backend directory and fix any issues",
                "code_quality",
                "simple",
            ),
            (
                "typecheck-ts",
                "Run TypeScript type checking on the frontend and fix type errors",
                "code_quality",
                "simple",
            ),
            (
                "api-health-check",
                "Verify the backend API is running and the /health endpoint returns 200",
                "api_validation",
                "simple",
            ),
            (
                "web-page-load",
                "Navigate to localhost:3001 and verify the main page loads with the correct title",
                "navigation",
                "medium",
            ),
            (
                "full-stack-lint",
                "Run linting and formatting checks on both the Python backend and TypeScript frontend, fix all issues",
                "code_quality",
                "complex",
            ),
            (
                "api-crud-test",
                "Test the CRUD operations for the workflow API endpoints",
                "api_validation",
                "medium",
            ),
            (
                "ui-form-validation",
                "Test the login form validation - verify error messages appear for invalid inputs",
                "form_interaction",
                "medium",
            ),
            (
                "e2e-workflow-create",
                "Create a new workflow through the web UI, verify it appears in the workflow list",
                "end_to_end",
                "complex",
            ),
        ];

        let mut stmt = conn.prepare(
            "INSERT INTO test_prompts (id, prompt, category, complexity, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)",
        )?;

        for (id, prompt, category, complexity) in seeds {
            stmt.execute(params![id, prompt, category, complexity, now])?;
        }

        tracing::info!("Seeded {} default test prompts", seeds.len());
        Ok(())
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    // ========================================================================
    // Test prompt CRUD
    // ========================================================================

    pub fn list_test_prompts(&self) -> anyhow::Result<Vec<TestPrompt>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, prompt, category, complexity, expected_phases, expected_step_types, tags, enabled, created_at, updated_at
             FROM test_prompts ORDER BY category, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TestPrompt {
                id: row.get(0)?,
                prompt: row.get(1)?,
                category: row.get(2)?,
                complexity: row.get(3)?,
                expected_phases: row
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                expected_step_types: row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                tags: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                enabled: row.get::<_, i64>(7)? != 0,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_enabled_test_prompts(&self) -> anyhow::Result<Vec<TestPrompt>> {
        let all = self.list_test_prompts()?;
        Ok(all.into_iter().filter(|p| p.enabled).collect())
    }

    pub fn insert_test_prompt(&self, prompt: &TestPrompt) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO test_prompts (id, prompt, category, complexity, expected_phases, expected_step_types, tags, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                prompt.id,
                prompt.prompt,
                prompt.category,
                prompt.complexity,
                prompt.expected_phases.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.expected_step_types.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.tags.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.enabled as i64,
                prompt.created_at,
                prompt.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_test_prompt(&self, id: &str, prompt: &TestPrompt) -> anyhow::Result<bool> {
        let conn = self.conn();
        let updated = conn.execute(
            "UPDATE test_prompts SET prompt=?2, category=?3, complexity=?4, expected_phases=?5, expected_step_types=?6, tags=?7, enabled=?8, updated_at=?9
             WHERE id=?1",
            params![
                id,
                prompt.prompt,
                prompt.category,
                prompt.complexity,
                prompt.expected_phases.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.expected_step_types.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.tags.as_ref().map(|v| serde_json::to_string(v).unwrap()),
                prompt.enabled as i64,
                prompt.updated_at,
            ],
        )?;
        Ok(updated > 0)
    }

    pub fn delete_test_prompt(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn();
        let deleted = conn.execute("DELETE FROM test_prompts WHERE id=?1", params![id])?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Eval run CRUD
    // ========================================================================

    pub fn insert_eval_run(&self, run: &EvalRunSummary) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO eval_runs (id, mode, status, prompts_total, prompts_completed, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.id,
                run.mode,
                run.status,
                run.prompts_total,
                run.prompts_completed,
                run.started_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_eval_run_progress(
        &self,
        run_id: &str,
        prompts_completed: i64,
    ) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE eval_runs SET prompts_completed=?2 WHERE id=?1",
            params![run_id, prompts_completed],
        )?;
        Ok(())
    }

    pub fn complete_eval_run(
        &self,
        run_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();

        // Compute aggregates from results
        conn.execute(
            "UPDATE eval_runs SET
                status=?2,
                error=?3,
                completed_at=?4,
                avg_overall_score = (SELECT AVG(overall_score) FROM eval_results WHERE run_id=?1 AND overall_score IS NOT NULL),
                avg_structural = (SELECT AVG(structural_correctness) FROM eval_results WHERE run_id=?1 AND structural_correctness IS NOT NULL),
                avg_command_accuracy = (SELECT AVG(command_accuracy) FROM eval_results WHERE run_id=?1 AND command_accuracy IS NOT NULL),
                avg_phase_flow = (SELECT AVG(phase_flow_logic) FROM eval_results WHERE run_id=?1 AND phase_flow_logic IS NOT NULL),
                avg_step_completeness = (SELECT AVG(step_completeness) FROM eval_results WHERE run_id=?1 AND step_completeness IS NOT NULL),
                avg_prompt_quality = (SELECT AVG(prompt_quality) FROM eval_results WHERE run_id=?1 AND prompt_quality IS NOT NULL),
                avg_determinism = (SELECT AVG(determinism) FROM eval_results WHERE run_id=?1 AND determinism IS NOT NULL)
             WHERE id=?1",
            params![run_id, status, error, now],
        )?;
        Ok(())
    }

    // ========================================================================
    // Eval result CRUD
    // ========================================================================

    pub fn insert_eval_result(&self, result: &EvalResult) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO eval_results (run_id, test_prompt_id, generated_workflow_json, task_run_id, workflow_id,
                structural_correctness, command_accuracy, phase_flow_logic, step_completeness, prompt_quality, determinism, overall_score,
                score_rationales, generation_error, scoring_error, generation_duration_ms, scoring_duration_ms, started_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                result.run_id,
                result.test_prompt_id,
                result.generated_workflow_json,
                result.task_run_id,
                result.workflow_id,
                result.structural_correctness,
                result.command_accuracy,
                result.phase_flow_logic,
                result.step_completeness,
                result.prompt_quality,
                result.determinism,
                result.overall_score,
                result.score_rationales,
                result.generation_error,
                result.scoring_error,
                result.generation_duration_ms,
                result.scoring_duration_ms,
                result.started_at,
                result.completed_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    // ========================================================================
    // Query helpers
    // ========================================================================

    pub fn get_eval_run(&self, run_id: &str) -> anyhow::Result<Option<EvalRunSummary>> {
        let conn = self.conn();
        let result = conn
            .query_row(
                "SELECT id, mode, status, prompts_total, prompts_completed,
                    avg_overall_score, avg_structural, avg_command_accuracy, avg_phase_flow,
                    avg_step_completeness, avg_prompt_quality, avg_determinism,
                    error, started_at, completed_at
                 FROM eval_runs WHERE id=?1",
                params![run_id],
                |row| {
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
                        error: row.get(12)?,
                        started_at: row.get(13)?,
                        completed_at: row.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(result)
    }

    pub fn get_results_for_run(&self, run_id: &str) -> anyhow::Result<Vec<EvalResult>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, test_prompt_id, generated_workflow_json, task_run_id, workflow_id,
                    structural_correctness, command_accuracy, phase_flow_logic, step_completeness,
                    prompt_quality, determinism, overall_score, score_rationales,
                    generation_error, scoring_error, generation_duration_ms, scoring_duration_ms,
                    started_at, completed_at
             FROM eval_results WHERE run_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            Ok(EvalResult {
                id: row.get(0)?,
                run_id: row.get(1)?,
                test_prompt_id: row.get(2)?,
                generated_workflow_json: row.get(3)?,
                task_run_id: row.get(4)?,
                workflow_id: row.get(5)?,
                structural_correctness: row.get(6)?,
                command_accuracy: row.get(7)?,
                phase_flow_logic: row.get(8)?,
                step_completeness: row.get(9)?,
                prompt_quality: row.get(10)?,
                determinism: row.get(11)?,
                overall_score: row.get(12)?,
                score_rationales: row.get(13)?,
                generation_error: row.get(14)?,
                scoring_error: row.get(15)?,
                generation_duration_ms: row.get(16)?,
                scoring_duration_ms: row.get(17)?,
                started_at: row.get(18)?,
                completed_at: row.get(19)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
