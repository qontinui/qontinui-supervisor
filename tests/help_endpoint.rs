//! Tests for `GET /help` (Item C of the supervisor cleanup plan).
//!
//! Drives the static handler through axum + tower's `oneshot` extension
//! without binding a real socket so the test stays self-contained.

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;

use qontinui_supervisor::server::{help_handler, ENDPOINT_MANIFEST};

#[tokio::test]
async fn help_endpoint_returns_full_manifest() {
    let app = Router::new().route("/help", get(help_handler));

    let req = Request::builder()
        .uri("/help")
        .body(axum::body::Body::empty())
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot succeeds");
    assert_eq!(resp.status(), StatusCode::OK);

    let body_bytes = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body collects");
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is valid JSON");

    let endpoints = v
        .get("endpoints")
        .and_then(|e| e.as_array())
        .expect("endpoints field is an array");

    // The current manifest is well over 100 entries — guard against
    // accidental wholesale deletion.
    assert!(
        endpoints.len() > 100,
        "expected >100 endpoint entries, got {}",
        endpoints.len()
    );

    // Every entry must carry all three fields, non-empty.
    for entry in endpoints {
        let method = entry
            .get("method")
            .and_then(|m| m.as_str())
            .expect("method field is string");
        let path = entry
            .get("path")
            .and_then(|p| p.as_str())
            .expect("path field is string");
        let summary = entry
            .get("summary")
            .and_then(|s| s.as_str())
            .expect("summary field is string");
        assert!(!method.is_empty(), "method must be non-empty");
        assert!(!path.is_empty(), "path must be non-empty");
        assert!(!summary.is_empty(), "summary must be non-empty");
    }
}

#[test]
fn manifest_contains_critical_routes() {
    // Ad-hoc sanity check: a few high-traffic routes must always be in
    // the manifest. If a route is renamed, fix the rename here too.
    let paths: Vec<&'static str> = ENDPOINT_MANIFEST.iter().map(|e| e.path).collect();
    for required in &[
        "/health",
        "/runners",
        "/runners/spawn-test",
        "/runners/spawn-named",
        "/builds",
        "/help",
        "/runners/{id}/rebuild-and-restart",
    ] {
        assert!(
            paths.contains(required),
            "endpoint manifest missing {}",
            required
        );
    }
}
