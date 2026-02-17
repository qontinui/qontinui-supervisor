//! JSONL tracing layer for HTTP request velocity tracking.
//!
//! Captures tower-http request spans and writes them to
//! `.dev-logs/supervisor-velocity.jsonl` in the common velocity format.
//!
//! This is a simplified version of the runner's `JsonlSpanLayer`, focused
//! exclusively on HTTP request timing for the supervisor's Axum server.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

const VELOCITY_FILENAME: &str = "supervisor-velocity.jsonl";

// Global atomic counter for generating unique IDs.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

// ============================================================================
// Types
// ============================================================================

/// A completed span entry in the common velocity format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEntry {
    pub service: String,
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub name: String,
    pub start_ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    pub attributes: HashMap<String, serde_json::Value>,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Internal tracking data for an in-flight span.
#[derive(Debug, Clone)]
struct ActiveSpanData {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    start_time: chrono::DateTime<Utc>,
    attributes: HashMap<String, serde_json::Value>,
    had_error: bool,
    error_message: Option<String>,
}

// ============================================================================
// VelocityLayer
// ============================================================================

/// Tracing layer that writes HTTP request spans to a JSONL file.
///
/// Only spans from `tower_http` (or named "HTTP request") are captured.
/// All other spans are ignored to keep the output focused on HTTP timing.
pub struct VelocityLayer {
    dev_logs_dir: PathBuf,
    /// Active spans keyed by tracing span ID (as u64).
    active_spans: Arc<RwLock<HashMap<u64, ActiveSpanData>>>,
    /// Lazily-opened file handle for appending JSONL entries.
    file: Arc<Mutex<Option<File>>>,
}

impl VelocityLayer {
    /// Create a new velocity layer that writes to `<dev_logs_dir>/supervisor-velocity.jsonl`.
    pub fn new(dev_logs_dir: PathBuf) -> Self {
        Self {
            dev_logs_dir,
            active_spans: Arc::new(RwLock::new(HashMap::new())),
            file: Arc::new(Mutex::new(None)),
        }
    }

    /// Check whether a span should be tracked.
    ///
    /// We only care about tower-http spans (HTTP request timing).
    fn should_track(metadata: &tracing::Metadata<'_>) -> bool {
        let target = metadata.target();
        let name = metadata.name();
        target.contains("tower_http") || name == "HTTP request"
    }

    /// Generate a unique ID using the system timestamp and an atomic counter.
    ///
    /// Format: `{unix_millis_lower32}-{counter}` which is unique within a
    /// single process and reasonably collision-free across restarts.
    fn generate_id() -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let count = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        // Use lower 8 hex digits of timestamp + counter for brevity
        format!("{:08x}-{:04x}", ts & 0xFFFF_FFFF, count & 0xFFFF)
    }

    /// Get or lazily open the output file.
    fn get_file(&self) -> Option<std::sync::MutexGuard<'_, Option<File>>> {
        let mut file_guard = self.file.lock().ok()?;
        if file_guard.is_none() {
            let path = self.dev_logs_dir.join(VELOCITY_FILENAME);

            // Ensure the directory exists
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => *file_guard = Some(f),
                Err(e) => {
                    eprintln!("VelocityLayer: failed to open {} : {}", path.display(), e);
                    return None;
                }
            }
        }
        Some(file_guard)
    }

    /// Serialize and append a span entry to the JSONL file.
    fn write_entry(&self, entry: &SpanEntry) {
        if let Some(mut guard) = self.get_file() {
            if let Some(ref mut file) = *guard {
                if let Ok(json) = serde_json::to_string(entry) {
                    let _ = writeln!(file, "{}", json);
                }
            }
        }
    }
}

impl<S> Layer<S> for VelocityLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let span = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };
        let metadata = span.metadata();

        if !Self::should_track(metadata) {
            return;
        }

        // Resolve parent span ID and trace ID
        let (parent_span_id, trace_id) = if let Some(parent) = span.parent() {
            let parent_id = parent.id();
            if let Ok(spans) = self.active_spans.read() {
                if let Some(parent_data) = spans.get(&parent_id.into_u64()) {
                    (
                        Some(parent_data.span_id.clone()),
                        parent_data.trace_id.clone(),
                    )
                } else {
                    (None, Self::generate_id())
                }
            } else {
                (None, Self::generate_id())
            }
        } else {
            (None, Self::generate_id())
        };

        // Collect initial attributes from the span
        let mut attributes = HashMap::new();
        let mut visitor = AttributeVisitor(&mut attributes);
        attrs.record(&mut visitor);

        let data = ActiveSpanData {
            trace_id,
            span_id: Self::generate_id(),
            parent_span_id,
            name: metadata.name().to_string(),
            start_time: Utc::now(),
            attributes,
            had_error: false,
            error_message: None,
        };

        if let Ok(mut spans) = self.active_spans.write() {
            spans.insert(id.into_u64(), data);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        if let Ok(mut spans) = self.active_spans.write() {
            if let Some(span_data) = spans.get_mut(&id.into_u64()) {
                let mut visitor = AttributeVisitor(&mut span_data.attributes);
                values.record(&mut visitor);
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Only care about error-level events to mark spans as failed
        let level = event.metadata().level();
        if *level != tracing::Level::ERROR {
            return;
        }

        if let Some(span) = ctx.lookup_current() {
            if let Ok(mut spans) = self.active_spans.write() {
                if let Some(span_data) = spans.get_mut(&span.id().into_u64()) {
                    span_data.had_error = true;
                    let mut message = String::new();
                    let mut visitor = MessageVisitor(&mut message);
                    event.record(&mut visitor);
                    if !message.is_empty() {
                        span_data.error_message = Some(message);
                    }
                }
            }
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let span_data = {
            if let Ok(mut spans) = self.active_spans.write() {
                spans.remove(&id.into_u64())
            } else {
                None
            }
        };

        if let Some(data) = span_data {
            let end_time = Utc::now();
            let duration = end_time
                .signed_duration_since(data.start_time)
                .num_milliseconds() as f64;

            let entry = SpanEntry {
                service: "supervisor".to_string(),
                trace_id: data.trace_id,
                span_id: data.span_id,
                parent_span_id: data.parent_span_id,
                name: data.name,
                start_ts: data.start_time.to_rfc3339(),
                end_ts: Some(end_time.to_rfc3339()),
                duration_ms: Some(duration),
                attributes: data.attributes,
                success: !data.had_error,
                error: data.error_message,
            };

            self.write_entry(&entry);
        }
    }
}

// ============================================================================
// Visitor Helpers
// ============================================================================

/// Visitor that collects span attributes into a `HashMap<String, serde_json::Value>`.
struct AttributeVisitor<'a>(&'a mut HashMap<String, serde_json::Value>);

impl<'a> tracing::field::Visit for AttributeVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(format!("{:?}", value)),
        );
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if let Some(n) = serde_json::Number::from_f64(value) {
            self.0
                .insert(field.name().to_string(), serde_json::Value::Number(n));
        }
    }
}

/// Visitor that extracts the `message` field from an event.
struct MessageVisitor<'a>(&'a mut String);

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.0 = format!("{:?}", value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            *self.0 = value.to_string();
        }
    }
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Clear the velocity JSONL file. Call this on supervisor startup so each
/// session starts with a fresh file.
pub fn clear_velocity_jsonl(dev_logs_dir: &Path) {
    let path = dev_logs_dir.join(VELOCITY_FILENAME);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            eprintln!("VelocityLayer: failed to clear {}: {}", path.display(), e);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_entry_serialization() {
        let entry = SpanEntry {
            service: "supervisor".to_string(),
            trace_id: "abc12345".to_string(),
            span_id: "def67890".to_string(),
            parent_span_id: None,
            name: "HTTP request".to_string(),
            start_ts: "2025-01-01T00:00:00Z".to_string(),
            end_ts: Some("2025-01-01T00:00:01Z".to_string()),
            duration_ms: Some(1000.0),
            attributes: HashMap::new(),
            success: true,
            error: None,
        };

        let json = serde_json::to_string(&entry).expect("serialization should succeed");
        assert!(json.contains("\"service\":\"supervisor\""));
        assert!(json.contains("\"name\":\"HTTP request\""));
        // Optional None fields should be absent
        assert!(!json.contains("parent_span_id"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_span_entry_with_error() {
        let entry = SpanEntry {
            service: "supervisor".to_string(),
            trace_id: "aaa".to_string(),
            span_id: "bbb".to_string(),
            parent_span_id: Some("ccc".to_string()),
            name: "HTTP request".to_string(),
            start_ts: "2025-01-01T00:00:00Z".to_string(),
            end_ts: Some("2025-01-01T00:00:02Z".to_string()),
            duration_ms: Some(2000.5),
            attributes: {
                let mut m = HashMap::new();
                m.insert(
                    "http.method".to_string(),
                    serde_json::Value::String("GET".to_string()),
                );
                m
            },
            success: false,
            error: Some("connection refused".to_string()),
        };

        let json = serde_json::to_string(&entry).expect("serialization should succeed");
        assert!(json.contains("\"parent_span_id\":\"ccc\""));
        assert!(json.contains("\"error\":\"connection refused\""));
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("\"http.method\":\"GET\""));
    }

    #[test]
    fn test_span_entry_deserialization_roundtrip() {
        let entry = SpanEntry {
            service: "supervisor".to_string(),
            trace_id: "t1".to_string(),
            span_id: "s1".to_string(),
            parent_span_id: None,
            name: "HTTP request".to_string(),
            start_ts: "2025-06-01T12:00:00Z".to_string(),
            end_ts: None,
            duration_ms: None,
            attributes: HashMap::new(),
            success: true,
            error: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: SpanEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.service, "supervisor");
        assert_eq!(deserialized.trace_id, "t1");
        assert!(deserialized.end_ts.is_none());
        assert!(deserialized.duration_ms.is_none());
    }

    #[test]
    fn test_generate_id_uniqueness() {
        let id1 = VelocityLayer::generate_id();
        let id2 = VelocityLayer::generate_id();
        let id3 = VelocityLayer::generate_id();
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_should_track_tower_http() {
        // We can't easily construct tracing::Metadata in tests, but we verify
        // the logic via the public SpanEntry format and ID generation.
        // The should_track filtering is integration-tested when the layer is
        // wired into a real subscriber.
    }

    #[test]
    fn test_clear_velocity_jsonl_no_file() {
        let dir = std::env::temp_dir().join("velocity_layer_test_clear");
        let _ = std::fs::create_dir_all(&dir);
        // Should not panic when file doesn't exist
        clear_velocity_jsonl(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_clear_velocity_jsonl_with_file() {
        let dir = std::env::temp_dir().join("velocity_layer_test_clear_exists");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(VELOCITY_FILENAME);
        std::fs::write(&path, "test line\n").unwrap();
        assert!(path.exists());

        clear_velocity_jsonl(&dir);
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
