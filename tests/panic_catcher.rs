//! Tests for the supervisor's panic-catcher middleware (Item D of the
//! supervisor cleanup plan).
//!
//! Wraps a one-route router with `tower_http::catch_panic::CatchPanicLayer`
//! using the supervisor's custom handler and asserts the panicking route
//! returns a 500 + JSON body in the documented shape instead of dropping
//! the connection.

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;

use qontinui_supervisor::server::supervisor_panic_handler;

async fn boom() -> &'static str {
    panic!("test panic");
}

#[tokio::test]
async fn panic_handler_returns_500_with_panicked_field() {
    let app = Router::new()
        .route("/boom", get(boom))
        .layer(CatchPanicLayer::custom(supervisor_panic_handler));

    let req = Request::builder()
        .uri("/boom")
        .body(axum::body::Body::empty())
        .expect("request builds");
    let resp = app.oneshot(req).await.expect("oneshot succeeds");
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let body_bytes = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body collects");
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).expect("body is valid JSON");

    assert_eq!(v["panicked"], true, "body must carry panicked: true");
    let err = v
        .get("error")
        .and_then(|e| e.as_str())
        .expect("error field is string");
    assert!(
        err.contains("panicked"),
        "error message should mention panicked, got: {}",
        err
    );
    assert!(
        err.contains("test panic"),
        "error should embed the panic message, got: {}",
        err
    );
}
