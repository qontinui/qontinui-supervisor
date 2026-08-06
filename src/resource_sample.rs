//! Resource-sample publisher — the supervisor's half of §A2 of plan
//! `2026-08-02-fleet-resource-telemetry-and-ci-allocation.md`.
//!
//! The supervisor already computes, on a timer, everything a capacity sample
//! needs: free commit (the number its own pre-permit memory guard enforces),
//! disk free/total for the volume the build pool fills, and live build-pool
//! occupancy. This module turns that snapshot into the §A1 wire shape and POSTs
//! it to coord's `POST /coord/devices/:device_id/resource-sample`, so the
//! numbers a dashboard renders and an allocator ranks on are the same numbers
//! this machine's guards act on.
//!
//! **Lane.** The supervisor publishes `lane = "host"` with a NULL
//! `lane_instance`: it is the sole publisher for the Windows host lane on this
//! box. The WSL lane is a different pool (`.wslconfig` caps it) and is not this
//! module's to report — a summed or mislabelled lane is the confidently-wrong
//! dashboard the plan exists to avoid.
//!
//! **`ci_jobs_running` is deliberately NULL here.** The supervisor probes GitHub
//! Actions runner *services* (`ci_runner_probe`) and knows idle/busy/offline for
//! the aggregate, but each host runs two runner services inside one WSL VM, so
//! it cannot say how many jobs are running. Deriving a count from "busy" would
//! be a fabricated number in a column an allocator reads; NULL is UNKNOWN and
//! reads as such.
//!
//! **Best-effort throughout.** Every failure path returns without touching the
//! build lane: a missing `machine.json`, an absent coord URL, a transport error
//! and a non-2xx response all log and return. The first failure logs at WARN
//! naming the cause; subsequent ones drop to DEBUG so a coord outage cannot
//! spam the supervisor log. A coord outage must never affect a build.
//!
//! **The ingest route lands via a sibling coord PR.** Until it does, this POSTs
//! into a 404 and degrades silently by design — which is exactly the posture it
//! must have during any later coord outage, so it is not a temporary shim.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, warn};

use crate::footprint::{BuildPoolOccupancy, FootprintSnapshot, MemorySnapshot};

/// Per-request HTTP timeout. Short on purpose: this runs on the footprint
/// timer, and a slow coord must not keep a task parked.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Resource pool this sample describes. The supervisor measures the Windows
/// host, never the WSL VM.
const LANE: &str = "host";

/// Publisher class, per §A1's `source` column (`runner` | `supervisor` |
/// `ci-step`).
const SOURCE: &str = "supervisor";

/// Env var carrying a coord device JWT, checked before the on-disk file.
const DEVICE_JWT_ENV: &str = "COORD_DEVICE_JWT";

/// One-shot latch so a persistent failure warns once and then goes quiet.
static WARNED_ONCE: AtomicBool = AtomicBool::new(false);

/// §A1 wire shape for one row of `coord.device_resource_samples`.
///
/// The `memory` and `build_pool` groups are flattened, so the JSON keys are the
/// column names verbatim and are produced by the SAME structs `GET /builds`
/// serializes — a rename cannot make the two surfaces disagree.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceSamplePayload {
    /// `'host'` from the supervisor. Mandatory and load-bearing: host and WSL
    /// measure different (coupled) pools and must never be summed.
    pub lane: &'static str,
    /// NULL for this publisher — "the only publisher for this lane". Populated
    /// only where several services share one lane (the two GitHub Actions
    /// runner services inside one WSL VM).
    pub lane_instance: Option<String>,
    pub cpu_cores: Option<u32>,
    /// NULL on the Windows host lane — Windows has no load average, and a
    /// fabricated 0.0 would read as an idle box.
    pub load_1m: Option<f64>,
    #[serde(flatten)]
    pub memory: MemorySnapshot,
    pub disk_total_bytes: Option<u64>,
    pub disk_free_bytes: Option<u64>,
    /// Mount of the volume the build pool fills, so a free-byte figure is
    /// attributable on a multi-volume host.
    pub disk_mount: Option<String>,
    #[serde(flatten)]
    pub build_pool: BuildPoolOccupancy,
    /// Always NULL from the supervisor — see the module doc.
    pub ci_jobs_running: Option<u32>,
    pub source: &'static str,
}

/// Host load average over 1 minute, or `None` where the OS has no such concept.
///
/// Windows genuinely has no load average; reporting `0.0` there would render as
/// a permanently idle machine, which is the false-safe class this plan exists
/// to remove.
fn load_1m() -> Option<f64> {
    #[cfg(windows)]
    {
        None
    }
    #[cfg(not(windows))]
    {
        Some(sysinfo::System::load_average().one)
    }
}

/// Build the §A1 payload from a footprint snapshot.
///
/// Pure: every field is projected from the snapshot the timer already
/// refreshed, plus the CPU count from the existing
/// [`crate::fleet::detect_resources`] probe. Nothing here re-samples memory,
/// disk or pool state — a second sampler is exactly what the plan forbids.
pub fn payload_from_snapshot(
    snapshot: &FootprintSnapshot,
    cpu_cores: u32,
) -> ResourceSamplePayload {
    ResourceSamplePayload {
        lane: LANE,
        lane_instance: None,
        cpu_cores: Some(cpu_cores),
        load_1m: load_1m(),
        memory: snapshot.memory,
        disk_total_bytes: snapshot.disk_total_bytes,
        disk_free_bytes: snapshot.disk_free_bytes,
        disk_mount: snapshot.disk_mount.clone(),
        build_pool: snapshot.build_pool,
        ci_jobs_running: None,
        source: SOURCE,
    }
}

/// A coord device bearer, if this host has one.
///
/// The supervisor holds no credentials of its own (see `routes::web_fleet` and
/// `routes::ci_runner`, which forward the caller's `Authorization` verbatim and
/// add nothing), so this reads the machine-wide device JWT: `$COORD_DEVICE_JWT`
/// first, then `~/.qontinui/coord-device-jwt`. `None` means the POST goes out
/// unauthenticated exactly like the sibling budget publish — coord decides
/// whether to accept it, and a refusal degrades silently like any other
/// non-2xx.
fn device_bearer() -> Option<String> {
    if let Ok(v) = std::env::var(DEVICE_JWT_ENV) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let path = dirs::home_dir()?.join(".qontinui").join("coord-device-jwt");
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Log a publish failure: WARN the first time (naming the cause), DEBUG after.
fn note_failure(reason: &str) {
    if WARNED_ONCE.swap(true, Ordering::Relaxed) {
        debug!("resource_sample: publish skipped/failed: {reason}");
    } else {
        warn!(
            "resource_sample: publish skipped/failed: {reason}. \
             Fleet resource telemetry for this host will be missing until this clears; \
             the build lane is unaffected. Further occurrences log at DEBUG."
        );
    }
}

/// Best-effort publish of one `lane='host'`, `source='supervisor'` sample.
///
/// Never returns an error and never panics: the caller is the footprint timer,
/// and telemetry must not be able to disturb builds.
pub async fn publish(snapshot: &FootprintSnapshot) {
    let machine = match crate::fleet::load_machine_file() {
        Some(m) => m,
        None => {
            note_failure(
                "~/.qontinui/machine.json missing — run `qontinui_profile machine init` on this host",
            );
            return;
        }
    };
    let device_id = match machine.device_id() {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            note_failure("machine.json has neither device_id nor machine_id");
            return;
        }
    };
    let base = match crate::fleet::coord_http_base() {
        Some(b) => b,
        None => {
            note_failure(
                "~/.qontinui/profiles.json missing or its active profile has no coord_url",
            );
            return;
        }
    };

    let payload = payload_from_snapshot(snapshot, crate::fleet::detect_resources().cpu_cores);
    let url = format!("{base}/coord/devices/{device_id}/resource-sample");

    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            note_failure(&format!("reqwest builder: {e}"));
            return;
        }
    };
    let mut req = client.post(&url).json(&payload);
    if let Some(token) = device_bearer() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            debug!(
                "resource_sample: published lane={LANE} device_id={device_id} \
                 (commit_free={:?} swap_used={:?} slots_busy={:?})",
                payload.memory.commit_available_bytes,
                payload.memory.swap_used_bytes,
                payload.build_pool.build_slots_busy,
            );
        }
        Ok(resp) => {
            let status = resp.status();
            note_failure(&format!(
                "coord returned {status} for POST /coord/devices/{device_id}/resource-sample \
                 (a 404 is expected until the coord ingest route lands)"
            ));
        }
        Err(e) => note_failure(&format!("POST {url}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::{ExeCopiesFootprint, SpawnContainersFootprint};

    fn snapshot_fixture() -> FootprintSnapshot {
        FootprintSnapshot {
            computed_at: "2026-08-06T00:00:00Z".to_string(),
            disk_free_bytes: Some(76_182_294_528),
            disk_total_bytes: Some(500_000_000_000),
            disk_mount: Some("D:\\".to_string()),
            memory: MemorySnapshot {
                mem_total_bytes: Some(34_000_000_000),
                mem_available_bytes: Some(9_000_000_000),
                commit_total_bytes: Some(40_000_000_000),
                commit_available_bytes: Some(985_000_000),
                swap_total_bytes: Some(8_589_934_592),
                swap_used_bytes: Some(7_000_000_000),
            },
            build_pool: BuildPoolOccupancy {
                build_slots_total: Some(3),
                build_slots_busy: Some(2),
                build_queue_depth: Some(1),
            },
            slots: vec![],
            lkg_bytes: 0,
            spawn_containers: SpawnContainersFootprint::default(),
            exe_copies: ExeCopiesFootprint::default(),
        }
    }

    #[test]
    fn payload_projects_the_snapshot_without_resampling() {
        let snap = snapshot_fixture();
        let p = payload_from_snapshot(&snap, 24);
        assert_eq!(p.lane, "host");
        assert_eq!(p.source, "supervisor");
        // The supervisor is the sole publisher for its lane, so NULL is the
        // correct `lane_instance` — not an invented label.
        assert!(p.lane_instance.is_none());
        assert_eq!(p.cpu_cores, Some(24));
        assert_eq!(p.memory.commit_available_bytes, Some(985_000_000));
        assert_eq!(p.memory.swap_used_bytes, Some(7_000_000_000));
        assert_eq!(p.disk_free_bytes, snap.disk_free_bytes);
        assert_eq!(p.disk_total_bytes, snap.disk_total_bytes);
        assert_eq!(p.disk_mount.as_deref(), Some("D:\\"));
        assert_eq!(p.build_pool.build_slots_busy, Some(2));
        assert_eq!(p.build_pool.build_queue_depth, Some(1));
        // Never fabricated: the supervisor cannot count Actions jobs.
        assert!(p.ci_jobs_running.is_none());
    }

    #[test]
    fn payload_serializes_every_a1_column_name() {
        let v = serde_json::to_value(payload_from_snapshot(&snapshot_fixture(), 8)).unwrap();
        for key in [
            "lane",
            "lane_instance",
            "cpu_cores",
            "load_1m",
            "mem_total_bytes",
            "mem_available_bytes",
            "commit_total_bytes",
            "commit_available_bytes",
            "swap_total_bytes",
            "swap_used_bytes",
            "disk_total_bytes",
            "disk_free_bytes",
            "disk_mount",
            "build_slots_total",
            "build_slots_busy",
            "build_queue_depth",
            "ci_jobs_running",
            "source",
        ] {
            assert!(
                v.get(key).is_some(),
                "resource sample must carry the §A1 column `{key}`"
            );
        }
        assert_eq!(v["lane"], "host");
        assert_eq!(v["source"], "supervisor");
        assert!(v["lane_instance"].is_null());
        assert!(v["ci_jobs_running"].is_null());
    }

    #[test]
    fn unknown_fields_serialize_as_null_never_zero() {
        // An unreadable probe must reach coord as NULL. A zero would read as
        // "no headroom" / "idle pool" — a confidently wrong number is worse
        // than an absent one, and §C3 requires absence to render as unknown.
        let mut snap = snapshot_fixture();
        snap.memory = MemorySnapshot::default();
        snap.build_pool = BuildPoolOccupancy::default();
        snap.disk_free_bytes = None;
        snap.disk_total_bytes = None;
        snap.disk_mount = None;
        let v = serde_json::to_value(payload_from_snapshot(&snap, 1)).unwrap();
        for key in [
            "mem_total_bytes",
            "commit_available_bytes",
            "swap_used_bytes",
            "disk_free_bytes",
            "disk_mount",
            "build_slots_busy",
            "build_queue_depth",
        ] {
            assert!(v[key].is_null(), "`{key}` must be null when unknown");
        }
    }

    #[test]
    fn load_1m_is_null_on_windows_and_real_elsewhere() {
        let got = load_1m();
        if cfg!(windows) {
            assert!(
                got.is_none(),
                "Windows has no load average — a 0.0 would render as an idle box"
            );
        } else {
            assert!(got.is_some());
        }
    }
}
