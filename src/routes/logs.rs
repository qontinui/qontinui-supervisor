use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

/// GET /logs/history — return recent log entries from circular buffer.
pub async fn log_history(
    State(state): State<SharedState>,
    Query(query): Query<HistoryQuery>,
) -> Json<serde_json::Value> {
    let entries = state.logs.history().await;
    let total = entries.len();
    let entries: Vec<_> = entries.into_iter().rev().take(query.limit).collect();

    Json(serde_json::json!({
        "entries": entries,
        "total": total,
        "limit": query.limit,
    }))
}

/// GET /logs/stream — SSE stream of real-time log events.
pub async fn log_stream(
    State(state): State<SharedState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.logs.subscribe();
    let stream = BroadcastStream::new(rx);

    let event_stream = stream.filter_map(|result| {
        match result {
            Ok(entry) => {
                let data = serde_json::to_string(&entry).unwrap_or_default();
                Some(Ok(Event::default().event("log").data(data)))
            }
            Err(_) => None, // Skip lagged messages
        }
    });

    Sse::new(event_stream).keep_alive(KeepAlive::default())
}

/// GET /logs/file/{type} — read a log file from .dev-logs/.
pub async fn log_file(
    State(state): State<SharedState>,
    Path(log_type): Path<String>,
    Query(params): Query<LogFileQuery>,
) -> Result<impl IntoResponse, axum::http::StatusCode> {
    let file_path = match log_type.as_str() {
        "runner" | "runner-tauri" => state.config.dev_logs_dir.join("runner-tauri.log"),
        "backend" => state.config.dev_logs_dir.join("backend.log"),
        "backend-err" => state.config.dev_logs_dir.join("backend.err.log"),
        "frontend" => state.config.dev_logs_dir.join("frontend.log"),
        "frontend-err" => state.config.dev_logs_dir.join("frontend.err.log"),
        "runner-general" => state.config.dev_logs_dir.join("runner-general.jsonl"),
        "runner-actions" => state.config.dev_logs_dir.join("runner-actions.jsonl"),
        "runner-image" => state.config.dev_logs_dir.join("runner-image-recognition.jsonl"),
        "runner-playwright" => state.config.dev_logs_dir.join("runner-playwright.jsonl"),
        "ai-output" => state.config.dev_logs_dir.join("ai-output.jsonl"),
        _ => return Err(axum::http::StatusCode::NOT_FOUND),
    };

    if !file_path.exists() {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }

    // Read the file, handling potential UTF-16 encoding for runner-tauri.log
    let content = if log_type == "runner" || log_type == "runner-tauri" {
        read_runner_log(&file_path).await
    } else {
        tokio::fs::read_to_string(&file_path)
            .await
            .unwrap_or_else(|_| String::new())
    };

    // Apply tail_lines if requested
    let content = if let Some(tail) = params.tail_lines {
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail);
        lines[start..].join("\n")
    } else {
        content
    };

    Ok(Json(serde_json::json!({
        "file": file_path.display().to_string(),
        "type": log_type,
        "content": content,
        "lines": content.lines().count(),
    })))
}

#[derive(Deserialize)]
pub struct LogFileQuery {
    pub tail_lines: Option<usize>,
}

/// GET /logs/files — list available log files with metadata.
pub async fn log_files(
    State(state): State<SharedState>,
) -> Json<serde_json::Value> {
    let log_types = vec![
        ("runner-tauri", "runner-tauri.log"),
        ("backend", "backend.log"),
        ("backend-err", "backend.err.log"),
        ("frontend", "frontend.log"),
        ("frontend-err", "frontend.err.log"),
        ("runner-general", "runner-general.jsonl"),
        ("runner-actions", "runner-actions.jsonl"),
        ("runner-image", "runner-image-recognition.jsonl"),
        ("runner-playwright", "runner-playwright.jsonl"),
        ("ai-output", "ai-output.jsonl"),
    ];

    let mut files = Vec::new();
    for (type_name, filename) in log_types {
        let path = state.config.dev_logs_dir.join(filename);
        let exists = path.exists();
        let size = if exists {
            tokio::fs::metadata(&path).await.ok().map(|m| m.len())
        } else {
            None
        };
        let modified = if exists {
            tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.to_rfc3339()
                })
        } else {
            None
        };

        files.push(serde_json::json!({
            "type": type_name,
            "filename": filename,
            "path": path.display().to_string(),
            "exists": exists,
            "size": size,
            "modified": modified,
        }));
    }

    Json(serde_json::json!({ "files": files }))
}

/// Read runner-tauri.log which may be UTF-16LE encoded.
async fn read_runner_log(path: &std::path::Path) -> String {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            // Check for UTF-16 BOM or null bytes pattern
            if bytes.len() >= 2 && (bytes[0] == 0xFF && bytes[1] == 0xFE) {
                // UTF-16LE with BOM — decode
                decode_utf16le(&bytes[2..])
            } else if bytes.len() > 1 && bytes.iter().step_by(2).any(|&b| b != 0)
                && bytes.iter().skip(1).step_by(2).any(|&b| b == 0)
            {
                // Looks like UTF-16LE without BOM (null bytes in alternating positions)
                decode_utf16le(&bytes)
            } else {
                // Regular UTF-8
                String::from_utf8_lossy(&bytes).to_string()
            }
        }
        Err(_) => String::new(),
    }
}

fn decode_utf16le(bytes: &[u8]) -> String {
    // Strip null bytes — quick and dirty approach for ASCII content in UTF-16LE
    let filtered: Vec<u8> = bytes.iter().copied().filter(|&b| b != 0).collect();
    String::from_utf8_lossy(&filtered).to_string()
}
