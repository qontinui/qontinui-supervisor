use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub struct VelocityDb {
    conn: Mutex<Connection>,
}

impl VelocityDb {
    pub fn new(dev_logs_dir: &Path) -> anyhow::Result<Self> {
        let db_path = dev_logs_dir.join("velocity.db");
        let conn = Connection::open(&db_path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS velocity_spans (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                service TEXT NOT NULL,
                trace_id TEXT,
                span_id TEXT,
                parent_span_id TEXT,
                name TEXT NOT NULL,
                start_ts TEXT NOT NULL,
                end_ts TEXT,
                duration_ms REAL,
                http_method TEXT,
                http_route TEXT,
                http_status_code INTEGER,
                request_id TEXT,
                attributes TEXT,
                success INTEGER DEFAULT 1,
                error TEXT,
                ingested_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS ingestion_state (
                file_path TEXT PRIMARY KEY,
                last_byte_offset INTEGER NOT NULL DEFAULT 0,
                last_ingested_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_vs_service ON velocity_spans(service);
            CREATE INDEX IF NOT EXISTS idx_vs_start ON velocity_spans(start_ts);
            CREATE INDEX IF NOT EXISTS idx_vs_duration ON velocity_spans(duration_ms);
            CREATE INDEX IF NOT EXISTS idx_vs_route ON velocity_spans(http_route);
            CREATE INDEX IF NOT EXISTS idx_vs_request_id ON velocity_spans(request_id);
            CREATE INDEX IF NOT EXISTS idx_vs_service_route ON velocity_spans(service, http_route);
        ",
        )?;
        Ok(())
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }
}
