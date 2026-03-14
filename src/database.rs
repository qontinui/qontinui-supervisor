use rusqlite::{params, Connection, Result as SqliteResult};
use serde::Serialize;
use tracing::{info, warn};

/// Initialize the supervisor SQLite database.
/// Creates the config directory and database file if they don't exist.
pub fn init_db() -> Result<Connection, String> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| "Could not determine config directory".to_string())?
        .join("com.qontinui.supervisor");

    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir {:?}: {}", config_dir, e))?;

    let db_path = config_dir.join("supervisor.db");
    info!("Opening supervisor database at {:?}", db_path);

    let conn = Connection::open(&db_path)
        .map_err(|e| format!("Failed to open database {:?}: {}", db_path, e))?;

    // Enable WAL mode for better concurrent access
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("Failed to set WAL mode: {}", e))?;

    create_tables(&conn)?;

    Ok(conn)
}

fn create_tables(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_loops (
            id TEXT PRIMARY KEY,
            config_json TEXT NOT NULL,
            mode TEXT NOT NULL,
            workflow_id TEXT,
            started_at TEXT NOT NULL,
            completed_at TEXT,
            status TEXT NOT NULL,
            total_iterations INTEGER,
            error TEXT
        );

        CREATE TABLE IF NOT EXISTS workflow_loop_iterations (
            id TEXT PRIMARY KEY,
            loop_id TEXT NOT NULL,
            iteration INTEGER NOT NULL,
            started_at TEXT NOT NULL,
            completed_at TEXT NOT NULL,
            task_run_id TEXT NOT NULL,
            exit_check_json TEXT,
            generated_workflow_id TEXT,
            reflection_task_run_id TEXT,
            fix_count INTEGER,
            fixes_implemented INTEGER,
            rebuild_triggered INTEGER,
            FOREIGN KEY (loop_id) REFERENCES workflow_loops(id)
        );

        CREATE INDEX IF NOT EXISTS idx_iterations_loop_id ON workflow_loop_iterations(loop_id);",
    )
    .map_err(|e| format!("Failed to create tables: {}", e))?;

    Ok(())
}

// --- CRUD functions ---

/// Insert a new workflow loop record.
pub fn insert_loop(
    conn: &Connection,
    id: &str,
    config_json: &str,
    mode: &str,
    workflow_id: Option<&str>,
) -> SqliteResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO workflow_loops (id, config_json, mode, workflow_id, started_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, config_json, mode, workflow_id, now, "running"],
    )?;
    Ok(())
}

/// Mark a workflow loop as completed with final status.
pub fn complete_loop(
    conn: &Connection,
    id: &str,
    status: &str,
    total_iterations: u32,
    error: Option<&str>,
) -> SqliteResult<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE workflow_loops SET completed_at = ?1, status = ?2, total_iterations = ?3, error = ?4
         WHERE id = ?5",
        params![now, status, total_iterations, error, id],
    )?;
    Ok(())
}

/// Parameters for inserting a workflow loop iteration.
pub struct InsertIterationParams<'a> {
    pub id: &'a str,
    pub loop_id: &'a str,
    pub iteration: u32,
    pub started_at: &'a str,
    pub completed_at: &'a str,
    pub task_run_id: &'a str,
    pub exit_check_json: Option<&'a str>,
    pub generated_workflow_id: Option<&'a str>,
    pub reflection_task_run_id: Option<&'a str>,
    pub fix_count: Option<u32>,
    pub fixes_implemented: Option<bool>,
    pub rebuild_triggered: Option<bool>,
}

/// Insert a new iteration record for a workflow loop.
pub fn insert_iteration(conn: &Connection, p: &InsertIterationParams) -> SqliteResult<()> {
    conn.execute(
        "INSERT INTO workflow_loop_iterations
         (id, loop_id, iteration, started_at, completed_at, task_run_id, exit_check_json,
          generated_workflow_id, reflection_task_run_id, fix_count, fixes_implemented, rebuild_triggered)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            p.id,
            p.loop_id,
            p.iteration,
            p.started_at,
            p.completed_at,
            p.task_run_id,
            p.exit_check_json,
            p.generated_workflow_id,
            p.reflection_task_run_id,
            p.fix_count,
            p.fixes_implemented.map(|b| b as i32),
            p.rebuild_triggered.map(|b| b as i32),
        ],
    )?;
    Ok(())
}

/// Get all workflow loops with their iterations, ordered by most recent first.
pub fn get_loop_history(conn: &Connection) -> SqliteResult<Vec<LoopRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, config_json, mode, workflow_id, started_at, completed_at, status,
                total_iterations, error
         FROM workflow_loops
         ORDER BY started_at DESC",
    )?;

    let loops = stmt.query_map([], |row| {
        let loop_id: String = row.get(0)?;
        Ok(LoopRecord {
            id: loop_id,
            config_json: row.get(1)?,
            mode: row.get(2)?,
            workflow_id: row.get(3)?,
            started_at: row.get(4)?,
            completed_at: row.get(5)?,
            status: row.get(6)?,
            total_iterations: row.get(7)?,
            error: row.get(8)?,
            iterations: Vec::new(), // populated separately if needed
        })
    })?;

    let mut results = Vec::new();
    for loop_record in loops {
        match loop_record {
            Ok(mut rec) => {
                // Fetch iterations for this loop
                match get_iterations_for_loop(conn, &rec.id) {
                    Ok(iters) => rec.iterations = iters,
                    Err(e) => {
                        warn!("Failed to fetch iterations for loop {}: {}", rec.id, e);
                    }
                }
                results.push(rec);
            }
            Err(e) => {
                warn!("Failed to read loop record: {}", e);
            }
        }
    }

    Ok(results)
}

/// Get all iteration records for a specific loop.
pub fn get_iterations_for_loop(
    conn: &Connection,
    loop_id: &str,
) -> SqliteResult<Vec<IterationRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, loop_id, iteration, started_at, completed_at, task_run_id, exit_check_json,
                generated_workflow_id, reflection_task_run_id, fix_count, fixes_implemented,
                rebuild_triggered
         FROM workflow_loop_iterations
         WHERE loop_id = ?1
         ORDER BY iteration ASC",
    )?;

    let iterations = stmt.query_map(params![loop_id], |row| {
        let fixes_impl: Option<i32> = row.get(10)?;
        let rebuild: Option<i32> = row.get(11)?;
        Ok(IterationRecord {
            id: row.get(0)?,
            loop_id: row.get(1)?,
            iteration: row.get(2)?,
            started_at: row.get(3)?,
            completed_at: row.get(4)?,
            task_run_id: row.get(5)?,
            exit_check_json: row.get(6)?,
            generated_workflow_id: row.get(7)?,
            reflection_task_run_id: row.get(8)?,
            fix_count: row.get(9)?,
            fixes_implemented: fixes_impl.map(|v| v != 0),
            rebuild_triggered: rebuild.map(|v| v != 0),
        })
    })?;

    let mut results = Vec::new();
    for iter in iterations {
        match iter {
            Ok(rec) => results.push(rec),
            Err(e) => {
                warn!("Failed to read iteration record: {}", e);
            }
        }
    }

    Ok(results)
}

// --- Record types ---

/// A persisted workflow loop with its iteration history.
#[derive(Debug, Clone, Serialize)]
pub struct LoopRecord {
    pub id: String,
    pub config_json: String,
    pub mode: String,
    pub workflow_id: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: String,
    pub total_iterations: Option<u32>,
    pub error: Option<String>,
    pub iterations: Vec<IterationRecord>,
}

/// A persisted workflow loop iteration result.
#[derive(Debug, Clone, Serialize)]
pub struct IterationRecord {
    pub id: String,
    pub loop_id: String,
    pub iteration: u32,
    pub started_at: String,
    pub completed_at: String,
    pub task_run_id: String,
    pub exit_check_json: Option<String>,
    pub generated_workflow_id: Option<String>,
    pub reflection_task_run_id: Option<String>,
    pub fix_count: Option<u32>,
    pub fixes_implemented: Option<bool>,
    pub rebuild_triggered: Option<bool>,
}
