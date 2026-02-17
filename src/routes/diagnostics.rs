use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::state::SharedState;

#[derive(Deserialize)]
pub struct DiagnosticsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    filter: Option<String>,
}

fn default_limit() -> usize {
    50
}

pub async fn get_diagnostics(
    State(state): State<SharedState>,
    Query(query): Query<DiagnosticsQuery>,
) -> impl IntoResponse {
    let limit = query.limit.min(200);
    let filter: Option<Vec<String>> = query
        .filter
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

    let diag = state.diagnostics.read().await;
    let events = diag.events(limit, filter.as_deref());
    let total = events.len();

    Json(serde_json::json!({
        "events": events,
        "total": total
    }))
}

pub async fn clear_diagnostics(State(state): State<SharedState>) -> impl IntoResponse {
    state.diagnostics.write().await.clear();
    Json(serde_json::json!({
        "status": "cleared"
    }))
}
