use super::db::VelocityDb;
use chrono::Utc;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

pub struct IngestResult {
    pub total_new_spans: usize,
    pub files_processed: Vec<FileIngestResult>,
}

pub struct FileIngestResult {
    pub file: String,
    pub new_spans: usize,
    pub errors: usize,
}

pub fn ingest_all(db: &VelocityDb, dev_logs_dir: &Path) -> anyhow::Result<IngestResult> {
    let files = vec![
        ("runner-spans.jsonl", "runner"),
        ("backend-velocity.jsonl", "backend"),
        ("supervisor-velocity.jsonl", "supervisor"),
    ];

    let mut result = IngestResult {
        total_new_spans: 0,
        files_processed: Vec::new(),
    };

    for (filename, default_service) in &files {
        let file_path = dev_logs_dir.join(filename);
        if !file_path.exists() {
            continue;
        }

        let file_result = ingest_file(db, &file_path, default_service)?;
        result.total_new_spans += file_result.new_spans;
        result.files_processed.push(file_result);
    }

    Ok(result)
}

fn ingest_file(
    db: &VelocityDb,
    file_path: &Path,
    default_service: &str,
) -> anyhow::Result<FileIngestResult> {
    let file_path_str = file_path.to_string_lossy().to_string();

    // Get last byte offset for this file (separate scope so MutexGuard is dropped)
    let last_offset = {
        let conn = db.conn();
        conn.query_row(
            "SELECT last_byte_offset FROM ingestion_state WHERE file_path = ?1",
            [&file_path_str],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    };

    let mut file = File::open(file_path)?;
    let file_len = file.metadata()?.len() as i64;

    // If file is smaller than our offset, it was truncated/rotated - start from beginning
    let seek_offset = if file_len < last_offset {
        0
    } else {
        last_offset
    };
    file.seek(SeekFrom::Start(seek_offset as u64))?;

    let reader = BufReader::new(file);
    let mut new_spans = 0;
    let mut errors = 0;
    let now = Utc::now().to_rfc3339();
    let mut current_offset = seek_offset;

    // Collect lines first so we don't hold the file open during DB operations
    let lines: Vec<String> = reader
        .lines()
        .map_while(|line_result| line_result.ok())
        .collect();

    // Single conn + transaction for the entire insert batch + ingestion state update
    {
        let conn = db.conn();
        conn.execute_batch("BEGIN")?;
        let mut stmt = conn.prepare(
            "INSERT INTO velocity_spans (
                service, trace_id, span_id, parent_span_id, name, start_ts, end_ts,
                duration_ms, http_method, http_route, http_status_code, request_id,
                attributes, success, error, ingested_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;

        for line in &lines {
            current_offset += line.len() as i64 + 1; // +1 for newline

            if line.trim().is_empty() {
                continue;
            }

            let entry: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => {
                    errors += 1;
                    continue;
                }
            };

            // Determine if this is an HTTP span worth ingesting
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let attributes = entry.get("attributes");
            let target = entry.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let is_http_span = name.contains("HTTP")
                || name.contains("request")
                || target.contains("tower_http")
                || attributes.is_some_and(|a| a.get("http.method").is_some());

            if !is_http_span {
                continue;
            }

            let service = entry
                .get("service")
                .and_then(|v| v.as_str())
                .unwrap_or(default_service);
            let trace_id = entry.get("trace_id").and_then(|v| v.as_str());
            let span_id = entry.get("span_id").and_then(|v| v.as_str());
            let parent_span_id = entry.get("parent_span_id").and_then(|v| v.as_str());
            let start_ts = entry.get("start_ts").and_then(|v| v.as_str()).unwrap_or("");
            let end_ts = entry.get("end_ts").and_then(|v| v.as_str());
            let duration_ms = entry.get("duration_ms").and_then(|v| v.as_f64());
            let success = entry
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let error = entry.get("error").and_then(|v| v.as_str());

            // Denormalize HTTP attributes from the attributes object
            let http_method = attributes
                .and_then(|a| {
                    a.get("http.method")
                        .or_else(|| a.get("http.request.method"))
                })
                .and_then(|v| v.as_str());
            let http_route = attributes
                .and_then(|a| a.get("http.route").or_else(|| a.get("http.target")))
                .and_then(|v| v.as_str());
            let http_status_code = attributes
                .and_then(|a| {
                    a.get("http.status_code")
                        .or_else(|| a.get("http.response.status_code"))
                })
                .and_then(|v| v.as_i64());
            let request_id = attributes
                .and_then(|a| a.get("request_id").or_else(|| a.get("http.request_id")))
                .and_then(|v| v.as_str());

            let attrs_str = attributes.map(|a| a.to_string());

            match stmt.execute(rusqlite::params![
                service,
                trace_id,
                span_id,
                parent_span_id,
                name,
                start_ts,
                end_ts,
                duration_ms,
                http_method,
                http_route,
                http_status_code,
                request_id,
                attrs_str,
                success as i32,
                error,
                &now,
            ]) {
                Ok(_) => new_spans += 1,
                Err(_) => errors += 1,
            }
        }

        // Update ingestion state within the same conn scope
        conn.execute(
            "INSERT OR REPLACE INTO ingestion_state (file_path, last_byte_offset, last_ingested_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![&file_path_str, current_offset, &now],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    Ok(FileIngestResult {
        file: file_path_str,
        new_spans,
        errors,
    })
}
