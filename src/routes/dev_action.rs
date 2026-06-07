//! Read-back routes for dev-action snapshots (Phase 1).
//!
//! `GET /actions/{id}/outcome` returns a single action record + its folded
//! outcome (404 if the id is unknown) — the one call that replaces the
//! ten-tool-call restart archeology the 2026-06-07 incident required. `GET
//! /actions` is a cheap most-recent-first list. Both read the in-memory
//! [`crate::dev_action::record::ActionStore`] held on `SupervisorState`; the
//! shapes mirror `routes/diagnostics.rs`.
//!
//! axum 0.8 path-param syntax is `{id}` (not `:id`); the route is registered in
//! `server.rs::build_router`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::state::SharedState;

/// Query string for `GET /actions`.
#[derive(Deserialize)]
pub struct ActionsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

/// `GET /actions/{id}/outcome` — the action record + its outcome.
///
/// Returns 404 with `{error, action_id}` when the id is unknown (or aged out of
/// the capped ring). The `outcome` field is `null` while the attribution window
/// is still open, and the full [`crate::dev_action::record::ActionOutcome`]
/// once the watcher has folded the verdict in.
pub async fn get_action_outcome(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let parsed = match Uuid::parse_str(&id) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_action_id",
                    "action_id": id,
                    "message": "action id must be a UUID",
                })),
            )
                .into_response();
        }
    };

    let record = {
        let store = state.dev_actions.read().await;
        store.get(&parsed)
    };

    match record {
        Some(rec) => Json(serde_json::json!({ "action": &*rec })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "action_not_found",
                "action_id": id,
                "message": "no action snapshot with this id (unknown or aged out of the in-memory ring)",
            })),
        )
            .into_response(),
    }
}

/// `GET /actions` — most-recent-first list of recent action records (cheap).
pub async fn list_actions(
    State(state): State<SharedState>,
    Query(query): Query<ActionsQuery>,
) -> impl IntoResponse {
    let limit = query.limit.min(crate::dev_action::record::ACTION_STORE_CAP);
    let records = {
        let store = state.dev_actions.read().await;
        store.recent(limit)
    };
    let total = records.len();
    let actions: Vec<_> = records.iter().map(|r| &**r).collect();
    Json(serde_json::json!({
        "actions": actions,
        "total": total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
    use crate::dev_action::record::{ActionKind, ActionOutcome, ActionRecord, D3Category};
    use crate::state::{SharedState, SupervisorState};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Same `test_state` shape as `routes/runner.rs:630` — a minimal
    /// `SupervisorState` over a tempdir, single-slot pool, no prewarm/webview.
    fn test_state(root: &std::path::Path) -> SharedState {
        let config = SupervisorConfig {
            project_dir: root.join("src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: 9875,
            dev_logs_dir: root.join(".dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 8081,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig { pool_size: 1 },
            no_prewarm: true,
            no_webview: true,
        };
        Arc::new(SupervisorState::new(config))
    }

    async fn body_json(resp: axum::response::Response) -> (u16, serde_json::Value) {
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body");
        let v: serde_json::Value =
            serde_json::from_slice(&bytes).expect("response body must be JSON");
        (status, v)
    }

    #[tokio::test]
    async fn unknown_id_returns_404() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(tmp.path());
        let unknown = Uuid::new_v4().to_string();
        let resp = get_action_outcome(State(state), Path(unknown))
            .await
            .into_response();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "action_not_found");
    }

    #[tokio::test]
    async fn non_uuid_id_returns_400() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(tmp.path());
        let resp = get_action_outcome(State(state), Path("not-a-uuid".to_string()))
            .await
            .into_response();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "invalid_action_id");
    }

    #[tokio::test]
    async fn seeded_record_returns_action_and_outcome() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(tmp.path());

        // Seed a record with a folded Contradiction outcome (the incident shape).
        let rec = ActionRecord::new(
            ActionKind::Restart,
            Some("agent-1".into()),
            "rebuild=false".into(),
            &[],
        );
        let id = rec.action_id;
        let arc = state.dev_actions.write().await.insert(rec);
        *arc.outcome.write().unwrap() = Some(ActionOutcome {
            category: D3Category::Contradiction,
            signatures: vec!["DEV-TAURI-ASSET-MISSING".into()],
            ended_at: chrono::Utc::now(),
            duration_ms: 30_000,
            evidence_ref: Some("asset not found: index.html".into()),
            late_signatures: vec![],
        });

        let resp = get_action_outcome(State(state), Path(id.to_string()))
            .await
            .into_response();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, 200);
        assert_eq!(body["action"]["kind"], "restart");
        assert_eq!(body["action"]["requester_id"], "agent-1");
        assert_eq!(body["action"]["outcome"]["category"], "contradiction");
        assert_eq!(
            body["action"]["outcome"]["signatures"][0],
            "DEV-TAURI-ASSET-MISSING"
        );
    }

    #[tokio::test]
    async fn list_returns_recent_records_most_recent_first() {
        let tmp = TempDir::new().unwrap();
        let state = test_state(tmp.path());
        let mut last_id = None;
        for i in 0..3 {
            let rec = ActionRecord::new(ActionKind::Spawn, None, format!("n={i}"), &[]);
            last_id = Some(rec.action_id);
            state.dev_actions.write().await.insert(rec);
        }
        let resp = list_actions(State(state), Query(ActionsQuery { limit: 50 }))
            .await
            .into_response();
        let (status, body) = body_json(resp).await;
        assert_eq!(status, 200);
        assert_eq!(body["total"], 3);
        // Most-recent-first: the last inserted id leads.
        assert_eq!(
            body["actions"][0]["action_id"],
            last_id.unwrap().to_string()
        );
    }
}
