//! Row 10 Items 6-7 — content-addressed cache telemetry.
//!
//! Records the outcome of every cache-keyed build submission so the
//! dual-write **measure phase** (tracker step B, 2026-05-15 →
//! 2026-05-29) has paired data:
//!
//! * `ac_hit` / `ac_miss` — content-hash AC lookup result (Item 6).
//! * `ac_write_ok` / `ac_write_fail` — AC populate result (Item 7).
//! * `shadow_timestamp_would_reuse` — would the *legacy* timestamp
//!   heuristic have reused here? Logged but **not acted on** — the
//!   content-hash path is authoritative for `/build/submit` (which had
//!   no reuse before this phase); the timestamp path stays untouched
//!   for the spawn-test/LKG flow. This is the "shadow" of the
//!   dual-write migration (memory
//!   `proj_supervisor_build_pool_cache_precedent`): both keys are
//!   recorded, only one decides.
//!
//! Broken down per worker, per repo, and per build profile (the three
//! axes the Row 10 done-criteria asks for), plus a global roll-up.
//! Exposed at `GET /builds/cache-stats`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

#[derive(Debug, Default, Serialize)]
pub struct Counts {
    pub ac_hit: u64,
    pub ac_miss: u64,
    pub ac_write_ok: u64,
    pub ac_write_fail: u64,
    pub shadow_timestamp_would_reuse: u64,
    /// content-hash hit where the legacy timestamp heuristic would have
    /// *missed* — the cross-worktree uplift this whole row is justified by.
    pub hash_hit_timestamp_miss: u64,
    /// timestamp would-reuse but content-hash missed — investigate; an
    /// excess here in the measure phase aborts cutover (tracker step B).
    pub timestamp_hit_hash_miss: u64,
}

impl Counts {
    pub fn hit_rate(&self) -> f64 {
        let total = self.ac_hit + self.ac_miss;
        if total == 0 {
            0.0
        } else {
            self.ac_hit as f64 / total as f64
        }
    }
}

/// One recorded build outcome. `worker`/`repo`/`profile` are the
/// breakdown axes; the booleans are what happened.
#[derive(Debug, Clone)]
pub struct CacheEvent {
    pub worker: String,
    pub repo: String,
    pub profile: String,
    pub ac_hit: bool,
    pub ac_write_ok: Option<bool>,
    pub timestamp_would_reuse: bool,
}

#[derive(Default)]
struct Bucket {
    ac_hit: AtomicU64,
    ac_miss: AtomicU64,
    ac_write_ok: AtomicU64,
    ac_write_fail: AtomicU64,
    shadow_ts_reuse: AtomicU64,
    hash_hit_ts_miss: AtomicU64,
    ts_hit_hash_miss: AtomicU64,
}

impl Bucket {
    fn apply(&self, e: &CacheEvent) {
        if e.ac_hit {
            self.ac_hit.fetch_add(1, Ordering::Relaxed);
        } else {
            self.ac_miss.fetch_add(1, Ordering::Relaxed);
        }
        match e.ac_write_ok {
            Some(true) => {
                self.ac_write_ok.fetch_add(1, Ordering::Relaxed);
            }
            Some(false) => {
                self.ac_write_fail.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        if e.timestamp_would_reuse {
            self.shadow_ts_reuse.fetch_add(1, Ordering::Relaxed);
        }
        if e.ac_hit && !e.timestamp_would_reuse {
            self.hash_hit_ts_miss.fetch_add(1, Ordering::Relaxed);
        }
        if !e.ac_hit && e.timestamp_would_reuse {
            self.ts_hit_hash_miss.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn snapshot(&self) -> Counts {
        Counts {
            ac_hit: self.ac_hit.load(Ordering::Relaxed),
            ac_miss: self.ac_miss.load(Ordering::Relaxed),
            ac_write_ok: self.ac_write_ok.load(Ordering::Relaxed),
            ac_write_fail: self.ac_write_fail.load(Ordering::Relaxed),
            shadow_timestamp_would_reuse: self.shadow_ts_reuse.load(Ordering::Relaxed),
            hash_hit_timestamp_miss: self.hash_hit_ts_miss.load(Ordering::Relaxed),
            timestamp_hit_hash_miss: self.ts_hit_hash_miss.load(Ordering::Relaxed),
        }
    }
}

/// Process-lifetime cache telemetry. Cheap to clone (Arc-backed maps).
#[derive(Clone, Default)]
pub struct CacheTelemetry {
    global: Arc<Bucket>,
    by_worker: Arc<RwLock<HashMap<String, Arc<Bucket>>>>,
    by_repo: Arc<RwLock<HashMap<String, Arc<Bucket>>>>,
    by_profile: Arc<RwLock<HashMap<String, Arc<Bucket>>>>,
}

impl CacheTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    async fn bucket(map: &Arc<RwLock<HashMap<String, Arc<Bucket>>>>, k: &str) -> Arc<Bucket> {
        if let Some(b) = map.read().await.get(k) {
            return b.clone();
        }
        let mut w = map.write().await;
        w.entry(k.to_string())
            .or_insert_with(|| Arc::new(Bucket::default()))
            .clone()
    }

    pub async fn record(&self, e: CacheEvent) {
        self.global.apply(&e);
        Self::bucket(&self.by_worker, &e.worker).await.apply(&e);
        Self::bucket(&self.by_repo, &e.repo).await.apply(&e);
        Self::bucket(&self.by_profile, &e.profile).await.apply(&e);
    }

    async fn dump(map: &Arc<RwLock<HashMap<String, Arc<Bucket>>>>) -> serde_json::Value {
        let m = map.read().await;
        let mut out = serde_json::Map::new();
        for (k, b) in m.iter() {
            let c = b.snapshot();
            let mut v = serde_json::to_value(&c).unwrap_or(serde_json::Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "hit_rate".into(),
                    serde_json::json!((c.hit_rate() * 10000.0).round() / 10000.0),
                );
            }
            out.insert(k.clone(), v);
        }
        serde_json::Value::Object(out)
    }

    /// JSON for `GET /builds/cache-stats`.
    pub async fn snapshot_json(&self) -> serde_json::Value {
        let g = self.global.snapshot();
        serde_json::json!({
            "global": {
                "ac_hit": g.ac_hit,
                "ac_miss": g.ac_miss,
                "ac_write_ok": g.ac_write_ok,
                "ac_write_fail": g.ac_write_fail,
                "hit_rate": (g.hit_rate() * 10000.0).round() / 10000.0,
                "shadow_timestamp_would_reuse": g.shadow_timestamp_would_reuse,
                "hash_hit_timestamp_miss": g.hash_hit_timestamp_miss,
                "timestamp_hit_hash_miss": g.timestamp_hit_hash_miss,
            },
            "by_worker": Self::dump(&self.by_worker).await,
            "by_repo": Self::dump(&self.by_repo).await,
            "by_profile": Self::dump(&self.by_profile).await,
            "dual_write_window": {
                "phase": "shadow",
                "measure_window": "2026-05-15..2026-05-29",
                "note": "content-hash is authoritative for /build/submit; \
                         timestamp/LKG path untouched. See Row 10 tracker step B.",
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(worker: &str, repo: &str, profile: &str, hit: bool, ts: bool) -> CacheEvent {
        CacheEvent {
            worker: worker.into(),
            repo: repo.into(),
            profile: profile.into(),
            ac_hit: hit,
            ac_write_ok: if hit { None } else { Some(true) },
            timestamp_would_reuse: ts,
        }
    }

    #[tokio::test]
    async fn records_and_breaks_down_by_axis() {
        let t = CacheTelemetry::new();
        t.record(ev("a1", "runner", "build:dev", true, false)).await;
        t.record(ev("a1", "runner", "build:dev", false, false))
            .await;
        t.record(ev("a2", "coord", "release:release", true, true))
            .await;

        let j = t.snapshot_json().await;
        assert_eq!(j["global"]["ac_hit"], 2);
        assert_eq!(j["global"]["ac_miss"], 1);
        // hash hit where timestamp would have missed: the a1 hit.
        assert_eq!(j["global"]["hash_hit_timestamp_miss"], 1);
        assert_eq!(j["by_worker"]["a1"]["ac_hit"], 1);
        assert_eq!(j["by_worker"]["a1"]["ac_miss"], 1);
        assert_eq!(j["by_repo"]["coord"]["ac_hit"], 1);
        assert_eq!(j["by_profile"]["release:release"]["ac_hit"], 1);
    }

    #[test]
    fn hit_rate_zero_when_empty() {
        assert_eq!(Counts::default().hit_rate(), 0.0);
    }
}
