//! HTTP routes for the generalized build pool (Row 2 Phase 3).
//!
//! - `POST /build/submit` — accepts a build submission and returns
//!   `{ proposal_id, status }`.
//! - `GET  /build/:id/status` — returns the current submission state.
//!
//! These complement the existing `POST /runners/spawn-test` flow.
//! spawn-test is specialized for "build the runner binary, then start
//! it"; `/build/submit` is general: arbitrary build action against an
//! arbitrary worktree, no spawn step. Cache-hit short-circuit + AC
//! integration are out of scope here (Wave 5, Row 10 Items 5-7).

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::build_submissions::{self, BuildKind};
use crate::state::SharedState;

#[derive(Debug, Deserialize)]
pub struct SubmitRequest {
    /// Absolute path on disk to the worktree's cargo workspace root.
    /// Must already exist and contain Cargo.toml.
    pub worktree_path: PathBuf,
    /// What cargo command to run.
    pub build_kind: BuildKind,
    /// Optional agent identifier for telemetry / debugging.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional `-p <package>` argument for workspace builds.
    #[serde(default)]
    pub package: Option<String>,
    /// Optional `--features` list. Each entry passed as a separate
    /// `--features` arg.
    #[serde(default)]
    pub features: Vec<String>,
    /// Optional base ref the content-hash diff is computed against
    /// (Row 10 Item 5). Production callers should pass `origin/main`
    /// (or the agent's branch point) so the cache key reflects the
    /// agent's change set, not the whole history. Omitted ⇒
    /// `canonical_diff_hash.py` defaults to the first commit on HEAD.
    #[serde(default)]
    pub base_ref: Option<String>,
}

/// `POST /build/submit` — submit a build action against a worktree.
///
/// Returns immediately with `{ proposal_id, status }`. The actual build
/// runs in a background task that acquires a build-pool permit. Poll
/// `GET /build/:id/status` for completion.
pub async fn post_submit(
    State(state): State<SharedState>,
    Json(req): Json<SubmitRequest>,
) -> impl IntoResponse {
    match build_submissions::submit(
        state,
        req.worktree_path,
        req.build_kind,
        req.agent_id,
        req.package,
        req.features,
        req.base_ref,
    )
    .await
    {
        Ok(id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "proposal_id": id.to_string(),
                "status": "queued",
            })),
        )
            .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// `GET /build/:id/status` — return the current submission state.
///
/// Returns 404 if the submission id is unknown (either never submitted,
/// or evicted from the store after completion).
pub async fn get_status(
    State(state): State<SharedState>,
    Path(id_str): Path<String>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id_str) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid uuid: {id_str}")})),
            )
                .into_response();
        }
    };
    let arc = match state.build_submissions.get(&id).await {
        Some(a) => a,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("submission {id} not found")})),
            )
                .into_response();
        }
    };
    let sub = arc.read().await;
    (StatusCode::OK, Json(serde_json::to_value(&*sub).unwrap())).into_response()
}

/// `GET /builds/cache-stats` — Row 10 Items 6-7 content-addressed
/// cache telemetry: `ac_hit_rate` per worker / repo / profile, plus
/// the dual-write shadow counters the measure phase (tracker step B)
/// consumes. Also reports whether the bazel-remote backend is wired.
pub async fn get_cache_stats(State(state): State<SharedState>) -> impl IntoResponse {
    let mut stats = state.cache_telemetry.snapshot_json().await;
    if let Some(obj) = stats.as_object_mut() {
        obj.insert(
            "bazel_remote".into(),
            json!({
                "enabled": state.bazel_remote.enabled(),
                "url": state.bazel_remote.base_url(),
            }),
        );
    }
    (StatusCode::OK, Json(stats)).into_response()
}
