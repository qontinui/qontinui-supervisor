//! [`OptionalJson`] — the body extractor for routes whose request body is
//! *entirely* optional.
//!
//! # Why this exists rather than `Option<Json<T>>`
//!
//! Several routes here take a body every field of which has a `#[serde(default)]`
//! (`POST /runner/stop`, `POST /runners/{id}/stop`, `POST /runners/purge-stale`).
//! They are documented as taking no body, and they were written as
//! `Option<Json<T>>` precisely to express "a body-less POST is fine". That
//! extractor does not mean what it reads like. axum 0.8's
//! `OptionalFromRequest for Json<T>` branches on the **`Content-Type` header**,
//! never on whether there are any bytes to parse:
//!
//! | `Content-Type` | body | `Option<Json<T>>` | what the caller wanted |
//! |---|---|---|---|
//! | absent | empty | `None` | `None` ✔ |
//! | `application/json` | `{}` | `Some({})` | `Some({})` ✔ |
//! | `application/json` | *empty* | **400** — EOF while parsing | `None` ✘ |
//! | `application/x-www-form-urlencoded` | *empty* | **415** | `None` ✘ |
//! | absent | `{"force":true}` | `None` — **body silently dropped** | `Some(..)` ✘ |
//!
//! The two `*empty*` rows are the ones that bite, because an empty body with a
//! non-JSON content type is what an ordinary HTTP client sends for a bodyless
//! POST. `.NET`'s `HttpWebRequest` — and therefore PowerShell 5.1's
//! `Invoke-RestMethod -Method Post` / `Invoke-WebRequest -Method Post` with no
//! `-Body` — defaults to `application/x-www-form-urlencoded`; `curl -d ''` does
//! the same. Every one of those callers got a 415 from a route that requires no
//! body at all, *before the handler ran*.
//!
//! That is not hypothetical. `scripts/verify-scoped-cleanup.ps1` stopped the
//! temp runner it spawns with exactly that call shape; the route answered 415,
//! teardown degraded to a warning, and the harness **leaked a runner** while
//! reporting all 9 assertions passed (2026-08-09, fixed client-side in #138).
//! #138 fixed that one caller. This fixes the contract for all of them.
//!
//! # What `OptionalJson` does
//!
//! Emptiness is decided first, and it decides alone:
//!
//! - **Empty body → `None`**, whatever the `Content-Type` says (or doesn't).
//!   Nothing was sent, so there is nothing to disagree with about the encoding.
//! - **Non-empty body → delegate to [`Json`] verbatim**, inheriting axum's own
//!   content-type check, parse errors, and rejection bodies. A caller that sends
//!   bytes gets exactly the diagnostics it gets on any other JSON route.
//!
//! The second bullet also closes the last row of the table above: a body sent
//! without a `Content-Type` is now a **415 that names the problem** instead of a
//! field that vanishes. This repo prefers a loud rejection to a silent drop —
//! the same reason `spawn-test` answers 400 on a misspelled provenance selector
//! rather than ignoring it. It matters concretely here: `force` is the only
//! field these three bodies carry, and silently dropping `force: true` turns an
//! operator's deliberate force-stop of a protected runner into a confusing
//! "runner is protected" refusal.

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;

/// A JSON body that the route treats as optional. See the module docs.
///
/// `OptionalJson(None)` means *no bytes were sent*, not "the bytes did not
/// parse" — a malformed body is still a rejection.
#[derive(Debug, Clone, Copy, Default)]
pub struct OptionalJson<T>(pub Option<T>);

impl<T> OptionalJson<T> {
    /// The deserialized body, or `T::default()` when the caller sent none.
    ///
    /// This is the shape every current caller wants: the bodies exist only to
    /// carry opt-in flags whose absence is the default.
    pub fn into_inner_or_default(self) -> T
    where
        T: Default,
    {
        self.0.unwrap_or_default()
    }
}

impl<T, S> FromRequest<S> for OptionalJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    /// An already-rendered response, so a non-empty body's rejection is byte-for
    /// byte the one `Json<T>` would have produced.
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Buffer the body BEFORE looking at any header: `Content-Type` is a
        // claim about bytes, and with zero bytes there is nothing for it to be
        // right or wrong about. `Bytes::from_request` accepts any content type.
        let (parts, body) = req.into_parts();
        let bytes = Bytes::from_request(Request::from_parts(parts.clone(), body), state)
            .await
            .map_err(IntoResponse::into_response)?;

        if bytes.is_empty() {
            return Ok(OptionalJson(None));
        }

        // Non-empty: hand the *same* request back to `Json` so the content-type
        // check and every rejection body stay axum's, not ours.
        let req = Request::from_parts(parts, Body::from(bytes));
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(OptionalJson(Some(value))),
            Err(rejection) => Err(rejection.into_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::post;
    use axum::Router;
    use serde::Deserialize;
    use tower::util::ServiceExt;

    #[derive(Debug, Default, Deserialize)]
    struct Flags {
        #[serde(default)]
        force: bool,
    }

    /// Echoes what the extractor decided, so a test can tell `None` from
    /// `Some(default)` — the distinction the whole module is about.
    async fn handler(OptionalJson(body): OptionalJson<Flags>) -> String {
        match body {
            None => "none".to_string(),
            Some(f) => format!("some:force={}", f.force),
        }
    }

    /// The extractor this module replaces, mounted alongside so the tests below
    /// pin the *difference* rather than asserting the new behavior in a vacuum.
    async fn legacy_handler(body: Option<Json<Flags>>) -> String {
        match body {
            None => "none".to_string(),
            Some(Json(f)) => format!("some:force={}", f.force),
        }
    }

    fn app() -> Router {
        Router::new()
            .route("/opt", post(handler))
            .route("/legacy", post(legacy_handler))
    }

    /// `content_type: None` sends no header at all.
    async fn post_body(
        uri: &str,
        content_type: Option<&str>,
        body: &'static str,
    ) -> (StatusCode, String) {
        let mut req = HttpRequest::builder().method("POST").uri(uri);
        if let Some(ct) = content_type {
            req = req.header("content-type", ct);
        }
        let req = req.body(Body::from(body)).expect("build request");

        let response = app().oneshot(req).await.expect("oneshot");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("read body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    // ---- the two rows that caused the incident ---------------------------

    #[tokio::test]
    async fn empty_body_with_a_form_content_type_is_none_not_415() {
        // PowerShell 5.1: `Invoke-RestMethod -Method Post` with no -Body. This
        // is the exact call shape that 415'd and leaked a temp runner.
        let (status, body) = post_body("/opt", Some("application/x-www-form-urlencoded"), "").await;
        assert_eq!(status, StatusCode::OK, "body was: {body}");
        assert_eq!(body, "none");
    }

    #[tokio::test]
    async fn empty_body_with_a_json_content_type_is_none_not_400() {
        // `Invoke-RestMethod -ContentType 'application/json'` with no -Body:
        // `Json` would try to parse zero bytes and fail with "EOF while parsing".
        let (status, body) = post_body("/opt", Some("application/json"), "").await;
        assert_eq!(status, StatusCode::OK, "body was: {body}");
        assert_eq!(body, "none");
    }

    /// Pins the defect itself, so this module cannot be "simplified" back to
    /// `Option<Json<T>>` without a red test naming the consequence.
    #[tokio::test]
    async fn the_legacy_extractor_rejects_both_of_those() {
        let (form, _) = post_body("/legacy", Some("application/x-www-form-urlencoded"), "").await;
        assert_eq!(
            form,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Option<Json<T>> 415s an empty body when the content type is not JSON"
        );

        let (json, _) = post_body("/legacy", Some("application/json"), "").await;
        assert_eq!(
            json,
            StatusCode::BAD_REQUEST,
            "Option<Json<T>> 400s an empty body when the content type IS JSON"
        );
    }

    // ---- the cases that must keep working exactly as before --------------

    #[tokio::test]
    async fn no_content_type_and_no_body_is_none() {
        // A browser `fetch(url, {method: 'POST'})` — what the dashboard sends.
        // This already worked; it must not regress.
        let (status, body) = post_body("/opt", None, "").await;
        assert_eq!(status, StatusCode::OK, "body was: {body}");
        assert_eq!(body, "none");
    }

    #[tokio::test]
    async fn a_json_body_still_deserializes() {
        let (status, body) = post_body("/opt", Some("application/json"), r#"{"force":true}"#).await;
        assert_eq!(status, StatusCode::OK, "body was: {body}");
        assert_eq!(body, "some:force=true");
    }

    #[tokio::test]
    async fn an_empty_json_object_is_some_with_defaults_not_none() {
        // `-Body '{}'` (what #138 made the harness send) must stay a real body,
        // distinct from "no body" — otherwise the extractor is lying about which
        // arm it took.
        let (status, body) = post_body("/opt", Some("application/json"), "{}").await;
        assert_eq!(status, StatusCode::OK, "body was: {body}");
        assert_eq!(body, "some:force=false");
    }

    #[tokio::test]
    async fn a_malformed_json_body_is_still_rejected() {
        // Emptiness is the only thing that yields `None`. Garbage is an error.
        let (status, _) = post_body("/opt", Some("application/json"), "{not json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_non_empty_body_with_a_form_content_type_is_still_415() {
        // The caller declared a form and sent JSON. Refuse loudly rather than
        // guess; only *emptiness* is forgiven.
        let (status, _) = post_body(
            "/opt",
            Some("application/x-www-form-urlencoded"),
            r#"{"force":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn a_body_sent_without_a_content_type_is_refused_not_silently_dropped() {
        // The deliberate behavior change. `Option<Json<T>>` returns None here,
        // so `force: true` vanishes and the operator is told the runner is
        // protected. A 415 names the problem instead.
        let (status, _) = post_body("/opt", None, r#"{"force":true}"#).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let (legacy_status, legacy_body) = post_body("/legacy", None, r#"{"force":true}"#).await;
        assert_eq!(legacy_status, StatusCode::OK);
        assert_eq!(
            legacy_body, "none",
            "Option<Json<T>> drops a body sent without a content type"
        );
    }

    #[tokio::test]
    async fn into_inner_or_default_collapses_none_to_the_default() {
        assert!(!OptionalJson::<Flags>(None).into_inner_or_default().force);
        assert!(
            OptionalJson(Some(Flags { force: true }))
                .into_inner_or_default()
                .force
        );
    }
}
