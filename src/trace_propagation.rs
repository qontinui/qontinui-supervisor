//! HTTP trace-context propagation — Row 9 Phase 5.
//!
//! Mirrors `../qontinui-coord/src/trace_propagation.rs` — see that
//! file for the design rationale. The supervisor uses axum 0.8 (coord
//! uses 0.7) but the middleware shape is identical.

use axum::{extract::Request, http::HeaderMap, middleware::Next, response::Response};
use opentelemetry::propagation::{Extractor, Injector};
use std::collections::HashMap;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::pii_scrub;

#[allow(dead_code)] // wired in tests + by future outbound HTTP call sites
pub const TRACEPARENT_HEADER: &str = "traceparent";

struct HeaderMapExtractor<'a>(&'a HeaderMap);

impl<'a> Extractor for HeaderMapExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Axum middleware — extracts W3C trace context from request headers
/// and attaches it as the OTel parent of the request-handler span.
pub async fn extract_trace_context(req: Request, next: Next) -> Response {
    let extracted_cx = opentelemetry::global::get_text_map_propagator(|prop| {
        prop.extract(&HeaderMapExtractor(req.headers()))
    });
    Span::current().set_parent(extracted_cx);
    next.run(req).await
}

/// Inject the **current** trace context into an arbitrary
/// `HashMap<String, String>` — for any non-HTTP carrier (e.g.
/// background-worker dispatch, future NATS publish).
#[allow(dead_code)] // wired by future cross-process dispatch sites
pub fn inject_into_map(map: &mut HashMap<String, String>) {
    let cx = Span::current().context();
    let mut adapter = MapInjector(map);
    opentelemetry::global::get_text_map_propagator(|prop| {
        prop.inject_context(&cx, &mut adapter);
    });
}

struct MapInjector<'a>(&'a mut HashMap<String, String>);

impl<'a> Injector for MapInjector<'a> {
    fn set(&mut self, key: &str, value: String) {
        self.0
            .insert(key.to_string(), pii_scrub::sanitize_value(&value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use opentelemetry_sdk::propagation::TraceContextPropagator;

    fn install_propagator() {
        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    }

    #[test]
    fn extractor_roundtrips_headers() {
        install_propagator();
        let mut headers = HeaderMap::new();
        headers.insert(
            TRACEPARENT_HEADER,
            HeaderValue::from_static("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
        );
        let cx = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&HeaderMapExtractor(&headers))
        });
        let span_ctx = opentelemetry::trace::TraceContextExt::span(&cx)
            .span_context()
            .clone();
        assert!(span_ctx.is_valid(), "extracted span context must be valid");
        assert_eq!(
            span_ctx.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
    }
}
