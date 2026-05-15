//! Row 10 Items 6-7 — bazel-remote HTTP client (CAS + AC).
//!
//! Thin REST client for the `qontinui-canonical-bazel-remote` container
//! (memory `proj_bazel_remote_deployment`). Used by the supervisor
//! build pool to:
//!
//! * **Item 6** — `GET /ac/<key>` before dispatching a build; a hit
//!   short-circuits the cargo invocation.
//! * **Item 7** — `PUT /cas/<sha>` (artifact) then `PUT /ac/<key>`
//!   (REAPI `ActionResult` protobuf) after a green build.
//!
//! ## Fail-open contract
//!
//! Per the Row 10 constraint *"don't break the existing timestamp-keyed
//! pool reuse path"*, every method here is best-effort: a timeout,
//! connection refused, 5xx, or malformed body is reported as
//! `Ok(None)` / `Err` that the caller treats as **cache miss** and
//! proceeds with a normal build. bazel-remote being down must never
//! fail or stall a build — it is a pure accelerator.
//!
//! Endpoint resolution: `QONTINUI_BAZEL_REMOTE_URL` env →
//! `BAZEL_REMOTE_HTTP_URL` env (the `qontinui-stack/.env` name) →
//! `http://localhost:9094` (the documented host-side default). Set
//! `QONTINUI_BAZEL_REMOTE_DISABLED=1` to hard-off the integration.

use std::time::Duration;

use crate::reapi::ActionResult;

/// Host-side default per `qontinui-stack/.env` (`BAZEL_REMOTE_HTTP_URL`).
const DEFAULT_URL: &str = "http://localhost:9094";
/// Cache-hit path must stay well under the 2s done-criteria budget;
/// the AC HEAD/GET + CAS GET each get their own short timeout.
const AC_TIMEOUT: Duration = Duration::from_millis(1500);
const CAS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct BazelRemoteClient {
    base_url: String,
    http: reqwest::Client,
    enabled: bool,
}

impl BazelRemoteClient {
    pub fn from_env() -> Self {
        let disabled = std::env::var("QONTINUI_BAZEL_REMOTE_DISABLED")
            .ok()
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let base_url = std::env::var("QONTINUI_BAZEL_REMOTE_URL")
            .or_else(|_| std::env::var("BAZEL_REMOTE_HTTP_URL"))
            .unwrap_or_else(|_| DEFAULT_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(800))
            .build()
            .unwrap_or_default();
        Self {
            base_url,
            http,
            enabled: !disabled,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /ac/<key>` → decoded `ActionResult` on a hit. `Ok(None)`
    /// for a clean miss (404) or any fail-open condition.
    pub async fn ac_get(&self, key: &str) -> Option<ActionResult> {
        if !self.enabled {
            return None;
        }
        let url = format!("{}/ac/{}", self.base_url, key);
        let resp = self.http.get(&url).timeout(AC_TIMEOUT).send().await.ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        let body = resp.bytes().await.ok()?;
        match ActionResult::decode(&body) {
            Ok(ar) => Some(ar),
            Err(e) => {
                tracing::warn!(
                    "bazel-remote AC body at {} failed protobuf decode (treating as miss): {}",
                    key,
                    e
                );
                None
            }
        }
    }

    /// `GET /cas/<sha>` → blob bytes, or `None` (fail-open).
    pub async fn cas_get(&self, sha: &str) -> Option<bytes::Bytes> {
        if !self.enabled {
            return None;
        }
        let url = format!("{}/cas/{}", self.base_url, sha);
        let resp = self.http.get(&url).timeout(CAS_TIMEOUT).send().await.ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        resp.bytes().await.ok()
    }

    /// `PUT /cas/<sha>` with the artifact bytes. Returns `true` on 2xx.
    /// Best-effort: a failed CAS write just means the next consumer
    /// rebuilds.
    pub async fn cas_put(&self, sha: &str, body: Vec<u8>) -> bool {
        if !self.enabled {
            return false;
        }
        let url = format!("{}/cas/{}", self.base_url, sha);
        match self
            .http
            .put(&url)
            .timeout(CAS_TIMEOUT)
            .body(body)
            .send()
            .await
        {
            Ok(r) => r.status().is_success(),
            Err(e) => {
                tracing::warn!("bazel-remote CAS PUT {} failed: {}", sha, e);
                false
            }
        }
    }

    /// `PUT /ac/<key>` with the serialized REAPI `ActionResult`.
    ///
    /// Item 7 contract: validation is ON server-side, so the body MUST
    /// be a valid `ActionResult` whose digests are already present in
    /// CAS — callers PUT CAS blobs *first*. Returns `true` on 2xx.
    pub async fn ac_put(&self, key: &str, ar: &ActionResult) -> bool {
        if !self.enabled {
            return false;
        }
        let url = format!("{}/ac/{}", self.base_url, key);
        match self
            .http
            .put(&url)
            .timeout(AC_TIMEOUT)
            .body(ar.encode())
            .send()
            .await
        {
            Ok(r) => {
                if r.status().is_success() {
                    true
                } else {
                    tracing::warn!(
                        "bazel-remote AC PUT {} rejected: HTTP {} (validation may have failed)",
                        key,
                        r.status()
                    );
                    false
                }
            }
            Err(e) => {
                tracing::warn!("bazel-remote AC PUT {} failed: {}", key, e);
                false
            }
        }
    }
}

impl Default for BazelRemoteClient {
    fn default() -> Self {
        Self::from_env()
    }
}
