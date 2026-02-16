use axum::response::{Html, IntoResponse, Response};

const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");

pub async fn index() -> Response {
    let mut resp = Html(DASHBOARD_HTML).into_response();
    resp.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "no-cache, no-store, must-revalidate".parse().unwrap(),
    );
    resp
}
