//! HTTP client for the coord `/claims/acquire`, `/claims/heartbeat`, and
//! `/claims/release` endpoints — symbol-claim subset.
//!
//! Backed by the `CoordTransport` trait so the daemon can run in three modes:
//!
//! - **Real coord** (`HttpTransport`): POSTs to `{coord_url}/claims/...`.
//! - **No-coord** (`NoOpTransport`): logs the would-be request and returns
//!   success. Used when `~/.qontinui/machine.json` is missing — the daemon
//!   stays useful (extracts symbols, logs them) instead of erroring out.
//! - **Test** (`MockTransport`): records each call into a `Vec` so unit
//!   tests can assert on the sequence without spinning up a server.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, warn};

/// TTL for `ClaimKind::Symbol` claims. Matches coord's
/// `default_ttl_for(ClaimKind::Symbol) = 300s` so the daemon and coord
/// agree on dwell time. The daemon also releases on idle (no save
/// within `IDLE_TTL`) so the TTL is primarily a safety-net against
/// daemon crashes.
pub const SYMBOL_CLAIM_TTL_SECONDS: i64 = 300;

/// Per-request HTTP timeout. Symbol claims fire on every save — keep
/// this short so a slow coord doesn't block the watcher loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Wire payload sent to `POST /claims/acquire`, `/claims/heartbeat`,
/// `/claims/release`. Fields match `qontinui-coord::claims::ClaimRequest`
/// but only the subset symbol-claims actually populate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimRequestWire {
    pub kind: String,
    pub resource_key: String,
    pub machine_id: String,
    pub ttl_seconds: Option<i64>,
    pub metadata: JsonValue,
}

impl ClaimRequestWire {
    pub fn for_symbol(machine_id: &str, resource_key: &str, metadata: JsonValue) -> Self {
        Self {
            kind: "symbol".to_string(),
            resource_key: resource_key.to_string(),
            machine_id: machine_id.to_string(),
            ttl_seconds: Some(SYMBOL_CLAIM_TTL_SECONDS),
            metadata,
        }
    }
}

/// Subset of coord's `AcquireResult` we care about. We never inspect the
/// `Held` case in this daemon (best-effort claim — if coord reports a
/// conflict, we log and move on; the daemon doesn't try to steal or wait).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    Claimed,
    Renewed,
    HeldBySomeoneElse {
        current_holder: String,
    },
    /// Network / server failure. The daemon ignores these — the file
    /// watcher continues and the next save retries.
    Error(String),
}

/// Subset of coord's `ReleaseResult`. We don't distinguish success modes
/// — any non-error response is "released".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    Released,
    Error(String),
}

#[async_trait]
pub trait CoordTransport: Send + Sync {
    async fn acquire(&self, req: ClaimRequestWire) -> AcquireOutcome;
    async fn release(&self, req: ClaimRequestWire) -> ReleaseOutcome;
}

/// Real HTTP implementation. Constructed once per daemon and shared
/// across all per-file claim emitters.
pub struct HttpTransport {
    base_url: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base_url: String) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;
        Ok(Self { base_url, client })
    }
}

#[async_trait]
impl CoordTransport for HttpTransport {
    async fn acquire(&self, req: ClaimRequestWire) -> AcquireOutcome {
        let url = format!("{}/claims/acquire", self.base_url);
        let resp = match self.client.post(&url).json(&req).send().await {
            Ok(r) => r,
            Err(e) => return AcquireOutcome::Error(format!("acquire POST {url}: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return AcquireOutcome::Error(format!("acquire {url}: HTTP {status} body={body}"));
        }
        // Body is `AcquireResult` serialized with `{ "result": "claimed"|"held"|"renewed", ... }`.
        let json: JsonValue = match resp.json().await {
            Ok(j) => j,
            Err(e) => return AcquireOutcome::Error(format!("acquire {url} parse JSON: {e}")),
        };
        match json.get("result").and_then(|v| v.as_str()) {
            Some("claimed") => AcquireOutcome::Claimed,
            Some("renewed") => AcquireOutcome::Renewed,
            Some("held") => {
                let holder = json
                    .get("current_holder")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                AcquireOutcome::HeldBySomeoneElse {
                    current_holder: holder,
                }
            }
            _ => AcquireOutcome::Error(format!("acquire {url}: unknown result: {json}")),
        }
    }

    async fn release(&self, req: ClaimRequestWire) -> ReleaseOutcome {
        let url = format!("{}/claims/release", self.base_url);
        let resp = match self.client.post(&url).json(&req).send().await {
            Ok(r) => r,
            Err(e) => return ReleaseOutcome::Error(format!("release POST {url}: {e}")),
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return ReleaseOutcome::Error(format!("release {url}: HTTP {status} body={body}"));
        }
        ReleaseOutcome::Released
    }
}

/// No-op transport used when `machine.json` is missing — keeps the
/// daemon useful (extracts + logs symbols) without contacting coord.
pub struct NoOpTransport;

#[async_trait]
impl CoordTransport for NoOpTransport {
    async fn acquire(&self, req: ClaimRequestWire) -> AcquireOutcome {
        debug!(
            "symbol_watcher: NO-COORD mode — would acquire {} (machine_id={})",
            req.resource_key, req.machine_id
        );
        AcquireOutcome::Claimed
    }

    async fn release(&self, req: ClaimRequestWire) -> ReleaseOutcome {
        debug!(
            "symbol_watcher: NO-COORD mode — would release {} (machine_id={})",
            req.resource_key, req.machine_id
        );
        ReleaseOutcome::Released
    }
}

/// In-memory test transport — records every acquire/release into a
/// shared `Vec` for assertions.
#[derive(Default, Clone)]
pub struct MockTransport {
    pub acquires: Arc<Mutex<Vec<ClaimRequestWire>>>,
    pub releases: Arc<Mutex<Vec<ClaimRequestWire>>>,
    /// When set, every acquire returns this outcome instead of `Claimed`.
    pub force_acquire_outcome: Option<AcquireOutcome>,
}

#[async_trait]
impl CoordTransport for MockTransport {
    async fn acquire(&self, req: ClaimRequestWire) -> AcquireOutcome {
        self.acquires.lock().unwrap().push(req);
        self.force_acquire_outcome
            .clone()
            .unwrap_or(AcquireOutcome::Claimed)
    }

    async fn release(&self, req: ClaimRequestWire) -> ReleaseOutcome {
        self.releases.lock().unwrap().push(req);
        ReleaseOutcome::Released
    }
}

/// Best-effort acquire wrapper used by the watcher loop — logs failures
/// at warn but never propagates them. The watcher should not stop on a
/// transient coord outage.
pub async fn acquire_best_effort(transport: &dyn CoordTransport, req: ClaimRequestWire) {
    let key = req.resource_key.clone();
    let outcome = transport.acquire(req).await;
    match outcome {
        AcquireOutcome::Claimed | AcquireOutcome::Renewed => {
            debug!("symbol_watcher: acquired {}", key);
        }
        AcquireOutcome::HeldBySomeoneElse { current_holder } => {
            warn!(
                "symbol_watcher: {} held by {} — not stealing (soft-warn mode)",
                key, current_holder
            );
        }
        AcquireOutcome::Error(e) => {
            warn!("symbol_watcher: acquire {} failed: {}", key, e);
        }
    }
}

/// Best-effort release wrapper — symmetric to [`acquire_best_effort`].
pub async fn release_best_effort(transport: &dyn CoordTransport, req: ClaimRequestWire) {
    let key = req.resource_key.clone();
    match transport.release(req).await {
        ReleaseOutcome::Released => {
            debug!("symbol_watcher: released {}", key);
        }
        ReleaseOutcome::Error(e) => {
            warn!("symbol_watcher: release {} failed: {}", key, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_request_for_symbol_shape() {
        let req = ClaimRequestWire::for_symbol(
            "machine-a",
            "qontinui-supervisor:src/main.rs:foo",
            serde_json::json!({"file": "src/main.rs"}),
        );
        assert_eq!(req.kind, "symbol");
        assert_eq!(req.machine_id, "machine-a");
        assert_eq!(req.ttl_seconds, Some(SYMBOL_CLAIM_TTL_SECONDS));
        // Sanity: serialized JSON has snake_case kind for coord wire compat.
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"kind\":\"symbol\""), "got: {s}");
        assert!(s.contains("\"resource_key\":\"qontinui-supervisor:src/main.rs:foo\""));
    }

    #[tokio::test]
    async fn noop_transport_returns_claimed_and_released() {
        let t = NoOpTransport;
        let req = ClaimRequestWire::for_symbol("m", "r", JsonValue::Null);
        assert_eq!(t.acquire(req.clone()).await, AcquireOutcome::Claimed);
        assert_eq!(t.release(req).await, ReleaseOutcome::Released);
    }

    #[tokio::test]
    async fn mock_transport_records_calls() {
        let t = MockTransport::default();
        let req = ClaimRequestWire::for_symbol("m", "r1", JsonValue::Null);
        let _ = t.acquire(req.clone()).await;
        let _ = t
            .acquire(ClaimRequestWire::for_symbol("m", "r2", JsonValue::Null))
            .await;
        let _ = t.release(req).await;
        assert_eq!(t.acquires.lock().unwrap().len(), 2);
        assert_eq!(t.releases.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mock_transport_force_outcome_propagates() {
        let t = MockTransport {
            force_acquire_outcome: Some(AcquireOutcome::HeldBySomeoneElse {
                current_holder: "other".to_string(),
            }),
            ..Default::default()
        };
        let req = ClaimRequestWire::for_symbol("m", "r", JsonValue::Null);
        match t.acquire(req).await {
            AcquireOutcome::HeldBySomeoneElse { current_holder } => {
                assert_eq!(current_holder, "other");
            }
            other => panic!("expected HeldBySomeoneElse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_transport_parses_claimed_response() {
        // Spin up a tiny axum server that returns a Claimed response and
        // confirm the transport parses it. Uses the real HttpTransport so
        // we exercise the JSON-shape contract end-to-end.
        let app = axum::Router::new().route(
            "/claims/acquire",
            axum::routing::post(|axum::Json(_req): axum::Json<JsonValue>| async {
                axum::Json(serde_json::json!({
                    "result": "claimed",
                    "ttl_seconds": SYMBOL_CLAIM_TTL_SECONDS,
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let transport = HttpTransport::new(format!("http://{addr}")).unwrap();
        let req = ClaimRequestWire::for_symbol("m", "r", JsonValue::Null);
        let outcome = transport.acquire(req).await;
        assert_eq!(outcome, AcquireOutcome::Claimed);
    }

    #[tokio::test]
    async fn http_transport_parses_held_response() {
        let app = axum::Router::new().route(
            "/claims/acquire",
            axum::routing::post(|axum::Json(_req): axum::Json<JsonValue>| async {
                axum::Json(serde_json::json!({
                    "result": "held",
                    "current_holder": "other-machine",
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let transport = HttpTransport::new(format!("http://{addr}")).unwrap();
        let req = ClaimRequestWire::for_symbol("m", "r", JsonValue::Null);
        let outcome = transport.acquire(req).await;
        match outcome {
            AcquireOutcome::HeldBySomeoneElse { current_holder } => {
                assert_eq!(current_holder, "other-machine");
            }
            other => panic!("expected Held, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_transport_release_success() {
        let app = axum::Router::new().route(
            "/claims/release",
            axum::routing::post(|axum::Json(_req): axum::Json<JsonValue>| async {
                axum::Json(serde_json::json!({"result": "released"}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let transport = HttpTransport::new(format!("http://{addr}")).unwrap();
        let req = ClaimRequestWire::for_symbol("m", "r", JsonValue::Null);
        assert_eq!(transport.release(req).await, ReleaseOutcome::Released);
    }
}
