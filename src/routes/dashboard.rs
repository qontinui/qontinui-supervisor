use axum::extract::{Request, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

use crate::state::SharedState;

// Embedded SPA assets from frontend build output
#[derive(Embed)]
#[folder = "dist/"]
pub struct Assets;

// Fallback: the original self-contained HTML dashboard
const DASHBOARD_HTML: &str = include_str!("../../static/dashboard.html");

/// Inject a `<meta name="build-id" content="...">` tag into the served HTML
/// just before the closing `</head>` tag. Connected dashboard tabs read this
/// on mount and compare it against the live `buildId` field on the SSE health
/// stream so a supervisor rebuild can prompt a refresh.
///
/// Returns the original body unchanged when no `</head>` is present (e.g. a
/// malformed SPA bundle). HTML-escape the build_id defensively even though the
/// constructor only ever produces RFC3339 / "unknown" / "embed-error" strings.
fn inject_build_id_meta(body: &str, build_id: &str) -> String {
    let escaped = build_id
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let meta_tag = format!("<meta name=\"build-id\" content=\"{}\">", escaped);
    if let Some(idx) = body.find("</head>") {
        let mut out = String::with_capacity(body.len() + meta_tag.len() + 1);
        out.push_str(&body[..idx]);
        out.push_str(&meta_tag);
        out.push_str(&body[idx..]);
        out
    } else {
        body.to_string()
    }
}

/// Serve the root page.
/// If the SPA dist/ exists (index.html embedded), serve it.
/// Otherwise fall back to the legacy static HTML dashboard.
pub async fn index(State(state): State<SharedState>) -> Response {
    if let Some(file) = Assets::get("index.html") {
        let body = std::str::from_utf8(file.data.as_ref()).unwrap_or("");
        let body = inject_build_id_meta(body, &state.build_id);
        let mut resp = Html(body).into_response();
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
pub fn spa_routes(state: SharedState) -> Router {
    Router::new().fallback(get(spa_fallback)).with_state(state)
}

async fn spa_fallback(State(state): State<SharedState>, req: Request) -> Response {
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
        let body = inject_build_id_meta(body, &state.build_id);
        let mut resp = Html(body).into_response();
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            "no-cache, no-store, must-revalidate".parse().unwrap(),
        );
        return resp;
    }

    // Nothing found
    (axum::http::StatusCode::NOT_FOUND, "Not Found").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_meta_tag_before_head_close() {
        let body = "<html><head><title>x</title></head><body>hi</body></html>";
        let out = inject_build_id_meta(body, "2026-04-25T12:00:00Z");
        assert!(out.contains("<meta name=\"build-id\" content=\"2026-04-25T12:00:00Z\">"));
        assert!(out.contains("</head>"));
        // Inserted before </head>, not after
        let meta_idx = out.find("<meta name=\"build-id\"").unwrap();
        let head_close_idx = out.find("</head>").unwrap();
        assert!(meta_idx < head_close_idx);
    }

    #[test]
    fn returns_body_unchanged_when_no_head_close() {
        let body = "<div>no head tag</div>";
        let out = inject_build_id_meta(body, "id");
        assert_eq!(out, body);
    }

    #[test]
    fn escapes_dangerous_chars_in_build_id() {
        let body = "<head></head>";
        let out = inject_build_id_meta(body, "id\"><script>alert(1)</script>");
        assert!(out.contains("&quot;"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(!out.contains("<script>alert"));
    }
}
