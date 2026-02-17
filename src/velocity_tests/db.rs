use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{VelocityTestResult, VelocityTestRun, VelocityTestTrendPoint};

pub struct VelocityTestDb {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl VelocityTestDb {
    pub fn new(dev_logs_dir: &Path) -> anyhow::Result<Self> {
        let db_path = dev_logs_dir.join("velocity.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let db = Self {
            conn: Mutex::new(conn),
            db_path,
        };
        db.init_schema()?;
        db.cleanup_stale_runs()?;
        Ok(db)
    }

    /// Mark any runs left in "running" status as "interrupted" on startup.
    fn cleanup_stale_runs(&self) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE velocity_test_runs SET status = 'interrupted' WHERE status = 'running'",
            [],
        )?;
        Ok(())
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS velocity_test_runs (
                id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                overall_score REAL,
                status TEXT NOT NULL DEFAULT 'running',
                tests_total INTEGER NOT NULL DEFAULT 0,
                tests_completed INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS velocity_test_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL REFERENCES velocity_test_runs(id) ON DELETE CASCADE,
                test_name TEXT NOT NULL,
                page_url TEXT NOT NULL,
                load_time_ms REAL,
                console_errors INTEGER DEFAULT 0,
                element_found INTEGER DEFAULT 0,
                score REAL,
                error TEXT,
                tested_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_vtr_run_id ON velocity_test_results(run_id);
            CREATE INDEX IF NOT EXISTS idx_vtruns_started ON velocity_test_runs(started_at);
            CREATE INDEX IF NOT EXISTS idx_vtruns_status ON velocity_test_runs(status);
        ",
        )?;
        // Run diagnostic columns migration
        self.migrate_diagnostics(&conn)?;
        Ok(())
    }

    /// Add diagnostic columns if they don't exist yet.
    fn migrate_diagnostics(&self, conn: &Connection) -> anyhow::Result<()> {
        // Check if migration is needed by looking for one of the new columns
        let has_column: bool = conn
            .prepare(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='velocity_test_results'",
            )?
            .query_row([], |row| row.get::<_, String>(0))
            .map(|sql| sql.contains("api_response_time_ms"))
            .unwrap_or(false);

        if !has_column {
            let columns = [
                "api_response_time_ms REAL",
                "api_status_code INTEGER",
                "ttfb_ms REAL",
                "dom_interactive_ms REAL",
                "dom_complete_ms REAL",
                "fcp_ms REAL",
                "long_task_count INTEGER DEFAULT 0",
                "long_task_total_ms REAL DEFAULT 0",
                "resource_count INTEGER DEFAULT 0",
                "total_transfer_size_bytes INTEGER DEFAULT 0",
                "slowest_resource_ms REAL DEFAULT 0",
                "bottleneck TEXT",
                "diagnostics_json TEXT",
            ];
            for col in &columns {
                let sql = format!("ALTER TABLE velocity_test_results ADD COLUMN {}", col);
                if let Err(e) = conn.execute(&sql, []) {
                    // Ignore "duplicate column" errors (column already exists)
                    let msg = e.to_string();
                    if !msg.contains("duplicate column") {
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(())
    }

    // ========================================================================
    // Runs
    // ========================================================================

    pub fn insert_run(&self, run: &VelocityTestRun) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO velocity_test_runs (id, started_at, status, tests_total, tests_completed)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run.id,
                run.started_at,
                run.status,
                run.tests_total,
                run.tests_completed,
            ],
        )?;
        Ok(())
    }

    pub fn update_run_progress(&self, run_id: &str, tests_completed: i64) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE velocity_test_runs SET tests_completed=?2 WHERE id=?1",
            params![run_id, tests_completed],
        )?;
        Ok(())
    }

    pub fn complete_run(&self, run_id: &str, status: &str) -> anyhow::Result<()> {
        let conn = self.conn();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE velocity_test_runs SET
                status=?2,
                completed_at=?3,
                overall_score = (SELECT AVG(score) FROM velocity_test_results WHERE run_id=?1 AND score IS NOT NULL)
             WHERE id=?1",
            params![run_id, status, now],
        )?;
        Ok(())
    }

    pub fn list_runs(&self) -> anyhow::Result<Vec<VelocityTestRun>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, started_at, completed_at, overall_score, status, tests_total, tests_completed
             FROM velocity_test_runs ORDER BY started_at DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(VelocityTestRun {
                id: row.get(0)?,
                started_at: row.get(1)?,
                completed_at: row.get(2)?,
                overall_score: row.get(3)?,
                status: row.get(4)?,
                tests_total: row.get(5)?,
                tests_completed: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_run(&self, run_id: &str) -> anyhow::Result<Option<VelocityTestRun>> {
        let conn = self.conn();
        let result = conn
            .query_row(
                "SELECT id, started_at, completed_at, overall_score, status, tests_total, tests_completed
                 FROM velocity_test_runs WHERE id=?1",
                params![run_id],
                |row| {
                    Ok(VelocityTestRun {
                        id: row.get(0)?,
                        started_at: row.get(1)?,
                        completed_at: row.get(2)?,
                        overall_score: row.get(3)?,
                        status: row.get(4)?,
                        tests_total: row.get(5)?,
                        tests_completed: row.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(result)
    }

    // ========================================================================
    // Results
    // ========================================================================

    pub fn insert_result(&self, result: &VelocityTestResult) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO velocity_test_results (
                run_id, test_name, page_url, load_time_ms, console_errors, element_found, score, error, tested_at,
                api_response_time_ms, api_status_code, ttfb_ms, dom_interactive_ms, dom_complete_ms, fcp_ms,
                long_task_count, long_task_total_ms, resource_count, total_transfer_size_bytes, slowest_resource_ms,
                bottleneck, diagnostics_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            params![
                result.run_id,
                result.test_name,
                result.page_url,
                result.load_time_ms,
                result.console_errors,
                result.element_found as i64,
                result.score,
                result.error,
                result.tested_at,
                result.api_response_time_ms,
                result.api_status_code,
                result.ttfb_ms,
                result.dom_interactive_ms,
                result.dom_complete_ms,
                result.fcp_ms,
                result.long_task_count,
                result.long_task_total_ms,
                result.resource_count,
                result.total_transfer_size_bytes,
                result.slowest_resource_ms,
                result.bottleneck,
                result.diagnostics_json,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_results_for_run(&self, run_id: &str) -> anyhow::Result<Vec<VelocityTestResult>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, test_name, page_url, load_time_ms, console_errors, element_found, score, error, tested_at,
                    api_response_time_ms, api_status_code, ttfb_ms, dom_interactive_ms, dom_complete_ms, fcp_ms,
                    long_task_count, long_task_total_ms, resource_count, total_transfer_size_bytes, slowest_resource_ms,
                    bottleneck, diagnostics_json
             FROM velocity_test_results WHERE run_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            Ok(VelocityTestResult {
                id: row.get(0)?,
                run_id: row.get(1)?,
                test_name: row.get(2)?,
                page_url: row.get(3)?,
                load_time_ms: row.get(4)?,
                console_errors: row.get(5)?,
                element_found: row.get::<_, i64>(6)? != 0,
                score: row.get(7)?,
                error: row.get(8)?,
                tested_at: row.get(9)?,
                api_response_time_ms: row.get(10)?,
                api_status_code: row.get(11)?,
                ttfb_ms: row.get(12)?,
                dom_interactive_ms: row.get(13)?,
                dom_complete_ms: row.get(14)?,
                fcp_ms: row.get(15)?,
                long_task_count: row.get::<_, Option<i64>>(16)?.unwrap_or(0),
                long_task_total_ms: row.get::<_, Option<f64>>(17)?.unwrap_or(0.0),
                resource_count: row.get::<_, Option<i64>>(18)?.unwrap_or(0),
                total_transfer_size_bytes: row.get::<_, Option<i64>>(19)?.unwrap_or(0),
                slowest_resource_ms: row.get::<_, Option<f64>>(20)?.unwrap_or(0.0),
                bottleneck: row.get(21)?,
                diagnostics_json: row.get(22)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // ========================================================================
    // Trend
    // ========================================================================

    pub fn get_trend(&self, limit: i64) -> anyhow::Result<Vec<VelocityTestTrendPoint>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, started_at, overall_score
             FROM velocity_test_runs
             WHERE status = 'completed' AND overall_score IS NOT NULL
             ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(VelocityTestTrendPoint {
                run_id: row.get(0)?,
                started_at: row.get(1)?,
                overall_score: row.get(2)?,
            })
        })?;
        let mut points: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
        // Reverse so oldest is first (for chart chronological order)
        points.reverse();
        Ok(points)
    }
}
