use axum::extract::Request;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

// Embedded SPA assets from frontend build output
#[derive(Embed)]
#[folder = "dist/"]
struct Assets;

// Fallback: the original self-contained HTML dashboard
const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");

/// Serve the root page.
/// If the SPA dist/ exists (index.html embedded), serve it.
/// Otherwise fall back to the legacy static HTML dashboard.
pub async fn index() -> Response {
    if let Some(file) = Assets::get("index.html") {
        let body = std::str::from_utf8(file.data.as_ref()).unwrap_or("");
        let mut resp = Html(body.to_string()).into_response();
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            "no-cache, no-store, must-revalidate".parse().unwrap(),
        );
        resp
    } else {
        // Fallback to legacy dashboard
        let mut resp = Html(DASHBOARD_HTML).into_response();
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            "no-cache, no-store, must-revalidate".parse().unwrap(),
        );
        resp
    }
}

/// Serve embedded SPA static assets (JS, CSS, etc.) and handle SPA client-side routing.
pub fn spa_routes() -> Router {
    Router::new().fallback(get(spa_fallback))
}

async fn spa_fallback(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');

    // Try to serve the exact file from embedded assets
    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        let mut resp = (
            [(axum::http::header::CONTENT_TYPE, mime)],
            file.data.to_vec(),
        )
            .into_response();
        // Cache static assets (JS/CSS have content hashes)
        if path.contains("assets/") {
            resp.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable".parse().unwrap(),
            );
        }
        return resp;
    }

    // For SPA client-side routes (e.g., /velocity, /dashboard), serve index.html
    if let Some(file) = Assets::get("index.html") {
        let body = std::str::from_utf8(file.data.as_ref()).unwrap_or("");
        let mut resp = Html(body.to_string()).into_response();
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            "no-cache, no-store, must-revalidate".parse().unwrap(),
        );
        return resp;
    }

    // Nothing found
    (axum::http::StatusCode::NOT_FOUND, "Not Found").into_response()
}
