//! OpenTelemetry instrumentation — Row 9 Phase 5.
//!
//! Mirrors the qontinui-coord implementation; see
//! `../qontinui-coord/src/otel.rs` for the design rationale (sampling
//! policy, propagation seam, scrubbing posture). This module is a
//! per-service copy because the trace pipeline has to live inside
//! each service's own subscriber-init path — there isn't yet a shared
//! `qontinui-otel` crate. Promoting these three files to a shared
//! crate is a no-op when the time comes (the API is identical).
//!
//! ## Sampling — Row 9 §3.6
//!
//! Baseline: 1% of traces (configurable via `OTEL_TRACES_SAMPLER_ARG`).
//! Tagged events at 100% regardless of baseline:
//!
//! - `auth-failure` / `partition-event` / `cache-miss-cold`
//! - `merge-conflict` (relayed through here when supervisor forwards
//!   merge-related spans from coord)
//!
//! The build-pool itself emits `cache-miss-cold` spans on every
//! supervisor build that bypasses bazel-remote AC, which is the
//! load-bearing tagged-event for the supervisor side.

use opentelemetry::trace::{SamplingDecision, SamplingResult, SpanKind, TraceContextExt, TraceId};
use opentelemetry::{Context as OtelContext, KeyValue};
use opentelemetry_sdk::trace::{self as sdktrace, Sampler, ShouldSample};
use opentelemetry_sdk::Resource;
use std::env;
use tracing::info;

#[allow(dead_code)] // referenced by docs + by call sites that build spans with these names
pub const TAGGED_EVENT_PREFIXES: &[&str] = &[
    "auth-failure",
    "partition-event",
    "cache-miss-cold",
    "merge-conflict",
];

pub struct OtelGuard {
    provider: Option<sdktrace::TracerProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            if let Err(e) = p.shutdown() {
                eprintln!("OtelGuard shutdown failed: {e:?}");
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaggedSampler<S> {
    inner: S,
}

impl<S: ShouldSample + Clone + 'static> ShouldSample for TaggedSampler<S> {
    fn should_sample(
        &self,
        parent_context: Option<&OtelContext>,
        trace_id: TraceId,
        name: &str,
        span_kind: &SpanKind,
        attributes: &[KeyValue],
        links: &[opentelemetry::trace::Link],
    ) -> SamplingResult {
        for prefix in TAGGED_EVENT_PREFIXES {
            if name.starts_with(prefix) {
                let trace_state = parent_context
                    .and_then(|cx| {
                        let span = cx.span();
                        let span_ctx = span.span_context();
                        if span_ctx.is_valid() {
                            Some(span_ctx.trace_state().clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                return SamplingResult {
                    decision: SamplingDecision::RecordAndSample,
                    attributes: vec![KeyValue::new("sampling.reason", "tagged-event")],
                    trace_state,
                };
            }
        }
        self.inner
            .should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

/// Build the OTel SDK tracer + W3C propagator from env. Returns
/// `(guard, Some(tracer))` when wired, or `(guard, None)` when
/// disabled — keeps the caller's subscriber-build code branchless.
pub fn init_otel(service_name: &str) -> (OtelGuard, Option<sdktrace::Tracer>) {
    let endpoint = match env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            info!("OpenTelemetry disabled (OTEL_EXPORTER_OTLP_ENDPOINT unset)");
            return (OtelGuard { provider: None }, None);
        }
    };

    let sample_rate = env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|r| (0.0..=1.0).contains(r))
        .unwrap_or(0.01);

    let resolved_service_name = env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| service_name.to_string());

    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    match try_build_provider(&endpoint, &resolved_service_name, sample_rate) {
        Ok((provider, tracer)) => {
            info!(
                "OpenTelemetry initialized: endpoint={endpoint}, service={resolved_service_name}, baseline_sample={sample_rate}"
            );
            (
                OtelGuard {
                    provider: Some(provider),
                },
                Some(tracer),
            )
        }
        Err(e) => {
            tracing::warn!("OpenTelemetry init failed (continuing without it): {e:#}");
            (OtelGuard { provider: None }, None)
        }
    }
}

fn try_build_provider(
    endpoint: &str,
    service_name: &str,
    sample_rate: f64,
) -> anyhow::Result<(sdktrace::TracerProvider, sdktrace::Tracer)> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let base = if sample_rate >= 1.0 {
        Sampler::AlwaysOn
    } else if sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(sample_rate)
    };
    let sampler = TaggedSampler { inner: base };

    let provider = sdktrace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_sampler(Sampler::ParentBased(Box::new(sampler)))
        .with_resource(Resource::new(vec![
            KeyValue::new("service.name", service_name.to_string()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION").to_string()),
        ]))
        .build();

    let tracer = provider.tracer(service_name.to_string());
    Ok((provider, tracer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceState;

    fn ratio_zero() -> Sampler {
        Sampler::TraceIdRatioBased(0.0)
    }

    #[test]
    fn tagged_event_always_samples_even_at_zero_ratio() {
        let sampler = TaggedSampler {
            inner: ratio_zero(),
        };
        for prefix in TAGGED_EVENT_PREFIXES {
            let r = sampler.should_sample(
                None,
                TraceId::from_bytes([1; 16]),
                prefix,
                &SpanKind::Internal,
                &[],
                &[],
            );
            assert_eq!(
                r.decision,
                SamplingDecision::RecordAndSample,
                "tagged prefix {prefix} should promote"
            );
        }
    }

    #[test]
    fn untagged_event_at_zero_ratio_does_not_sample() {
        let sampler = TaggedSampler {
            inner: ratio_zero(),
        };
        let r = sampler.should_sample(
            None,
            TraceId::from_bytes([1; 16]),
            "ordinary-span",
            &SpanKind::Internal,
            &[],
            &[],
        );
        assert_eq!(r.decision, SamplingDecision::Drop);
    }

    #[test]
    fn tagged_event_inherits_trace_state_when_parent_invalid() {
        let sampler = TaggedSampler {
            inner: ratio_zero(),
        };
        let r = sampler.should_sample(
            None,
            TraceId::from_bytes([1; 16]),
            "merge-conflict",
            &SpanKind::Internal,
            &[],
            &[],
        );
        assert_eq!(r.decision, SamplingDecision::RecordAndSample);
        assert_eq!(r.trace_state, TraceState::default());
    }
}
