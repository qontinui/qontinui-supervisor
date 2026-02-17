use axum::extract::{Query, State};
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::velocity::db::VelocityDb;
use crate::velocity::ingest;
use crate::velocity::queries::{self, QueryFilter};

// ============================================================================
// State
// ============================================================================

pub struct VelocityState {
    pub db: VelocityDb,
    pub dev_logs_dir: PathBuf,
}

// ============================================================================
// Query params
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct FilterParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub service: Option<String>,
}

impl From<&FilterParams> for QueryFilter {
    fn from(p: &FilterParams) -> Self {
        QueryFilter {
            since: p.since.clone(),
            until: p.until.clone(),
            service: p.service.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CompareParams {
    pub before_start: String,
    pub before_end: String,
    pub after_start: String,
    pub after_end: String,
    pub service: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SlowParams {
    pub since: Option<String>,
    pub until: Option<String>,
    pub service: Option<String>,
    pub threshold_ms: Option<f64>,
    pub limit: Option<usize>,
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Serialize)]
pub struct IngestResponse {
    pub total_new_spans: usize,
    pub files_processed: Vec<FileIngestResponse>,
}

#[derive(Serialize)]
pub struct FileIngestResponse {
    pub file: String,
    pub new_spans: usize,
    pub errors: usize,
}

// ============================================================================
// Routes
// ============================================================================

pub fn velocity_routes(dev_logs_dir: PathBuf) -> Router {
    let db = match VelocityDb::new(&dev_logs_dir) {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("Failed to initialize velocity database: {}", e);
            // Return empty router if DB fails
            return Router::new();
        }
    };

    let state = Arc::new(VelocityState { db, dev_logs_dir });

    Router::new()
        .route("/velocity/ingest", post(ingest_handler))
        .route("/velocity/summary", get(summary_handler))
        .route("/velocity/endpoints", get(endpoints_handler))
        .route("/velocity/slow", get(slow_handler))
        .route("/velocity/timeline", get(timeline_handler))
        .route("/velocity/compare", get(compare_handler))
        .route("/velocity/trace/{request_id}", get(trace_handler))
        .with_state(state)
}

// ============================================================================
// Handlers
// ============================================================================

async fn ingest_handler(State(state): State<Arc<VelocityState>>) -> Json<IngestResponse> {
    match ingest::ingest_all(&state.db, &state.dev_logs_dir) {
        Ok(result) => Json(IngestResponse {
            total_new_spans: result.total_new_spans,
            files_processed: result
                .files_processed
                .into_iter()
                .map(|f| FileIngestResponse {
                    file: f.file,
                    new_spans: f.new_spans,
                    errors: f.errors,
                })
                .collect(),
        }),
        Err(e) => {
            tracing::error!("Ingestion failed: {}", e);
            Json(IngestResponse {
                total_new_spans: 0,
                files_processed: Vec::new(),
            })
        }
    }
}

async fn summary_handler(
    State(state): State<Arc<VelocityState>>,
    Query(params): Query<FilterParams>,
) -> Json<Vec<queries::ServiceSummary>> {
    let filter = QueryFilter::from(&params);
    match queries::get_summary(&state.db, &filter) {
        Ok(results) => Json(results),
        Err(e) => {
            tracing::error!("Summary query failed: {}", e);
            Json(Vec::new())
        }
    }
}

async fn endpoints_handler(
    State(state): State<Arc<VelocityState>>,
    Query(params): Query<FilterParams>,
) -> Json<Vec<queries::EndpointSummary>> {
    let filter = QueryFilter::from(&params);
    match queries::get_endpoints(&state.db, &filter) {
        Ok(results) => Json(results),
        Err(e) => {
            tracing::error!("Endpoints query failed: {}", e);
            Json(Vec::new())
        }
    }
}

async fn slow_handler(
    State(state): State<Arc<VelocityState>>,
    Query(params): Query<SlowParams>,
) -> Json<Vec<queries::SlowRequest>> {
    let filter = QueryFilter {
        since: params.since,
        until: params.until,
        service: params.service,
    };
    let threshold = params.threshold_ms.unwrap_or(1000.0);
    let limit = params.limit.unwrap_or(50);
    match queries::get_slow_requests(&state.db, &filter, threshold, limit) {
        Ok(results) => Json(results),
        Err(e) => {
            tracing::error!("Slow requests query failed: {}", e);
            Json(Vec::new())
        }
    }
}

async fn timeline_handler(
    State(state): State<Arc<VelocityState>>,
    Query(params): Query<FilterParams>,
) -> Json<Vec<queries::TimelineBucket>> {
    let filter = QueryFilter::from(&params);
    match queries::get_timeline(&state.db, &filter) {
        Ok(results) => Json(results),
        Err(e) => {
            tracing::error!("Timeline query failed: {}", e);
            Json(Vec::new())
        }
    }
}

async fn compare_handler(
    State(state): State<Arc<VelocityState>>,
    Query(params): Query<CompareParams>,
) -> Json<Vec<queries::CompareResult>> {
    match queries::get_compare(
        &state.db,
        &params.before_start,
        &params.before_end,
        &params.after_start,
        &params.after_end,
        params.service.as_deref(),
    ) {
        Ok(results) => Json(results),
        Err(e) => {
            tracing::error!("Compare query failed: {}", e);
            Json(Vec::new())
        }
    }
}

async fn trace_handler(
    State(state): State<Arc<VelocityState>>,
    axum::extract::Path(request_id): axum::extract::Path<String>,
) -> Json<Vec<queries::TraceSpan>> {
    match queries::get_trace(&state.db, &request_id) {
        Ok(results) => Json(results),
        Err(e) => {
            tracing::error!("Trace query failed: {}", e);
            Json(Vec::new())
        }
    }
}
