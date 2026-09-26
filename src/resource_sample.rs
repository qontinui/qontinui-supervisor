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
//! **The ingest route is served** (`qontinui-coord`
//! `crates/coord/src/device_resource_samples.rs`). A refusal is therefore
//! reported by status — a 401/403 names the credential source that was sent,
//! a 404 says this coord does not serve the route — so the one WARN an
//! episode gets says what to fix.

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

/// One-shot latch so a persistent failure warns once and then goes quiet.
static WARNED_ONCE: AtomicBool = AtomicBool::new(false);

/// §A1 wire shape for one row of `coord.device_resource_samples`.
///
/// The `memory` and `build_pool` groups are flattened, so the JSON keys are the
/// column names verbatim and are produced by the SAME structs `GET /builds`
/// serializes — a rename cannot make the two surfaces disagree.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceSamplePayload {
    /// When the underlying snapshot was taken (its `computed_at`), NOT when
    /// coord received it. The snapshot is refreshed on a slow timer, so a
    /// consumer that aged the row by ingest time would treat a 15-minute-old
    /// reading as current — and §C3 requires a stale sample to render as stale
    /// rather than as its last value.
    pub sampled_at: String,
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
        sampled_at: snapshot.computed_at.clone(),
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

/// Everything the POST needs that comes from the filesystem, resolved in one
/// blocking hop. The bearer is NOT here: it is resolved on the async side of
/// [`publish`] through the shared [`crate::fleet::resolve_device_bearer`].
struct PublishTarget {
    device_id: String,
    /// Coord HTTP base, kept so [`bearer_for`] can apply its transport guard.
    base: String,
    url: String,
    cpu_cores: u32,
}

/// Source label reported when no credential resolved at all.
const SOURCE_NONE: &str = "none (no device JWT resolved)";

/// Source label reported when a credential exists but the transport guard
/// withheld it.
const SOURCE_WITHHELD: &str = "none (withheld: coord base is neither https nor loopback)";

/// A bearer to send to `base`, or `None`, with the name of the source it
/// came from (or why none was sent) for the refusal message.
///
/// This is one of the supervisor's outbound credentials, and
/// [`crate::fleet::coord_http_base`] maps `ws://` to plain `http://` — so a
/// profile pointing at a non-loopback `ws://` host would put a device JWT on
/// the wire in cleartext. Attach it only over `https://` or to a loopback
/// host; everything else publishes unauthenticated and lets coord decide,
/// which degrades exactly like any other non-2xx.
///
/// The credential itself comes from the SHARED cascade
/// ([`crate::fleet::resolve_device_bearer`]: env → file → runner mint,
/// falling through on validity, not presence) — this module keeps no second
/// copy of it.
async fn bearer_for(base: &str) -> (Option<String>, &'static str) {
    if !transport_may_carry_bearer(base) {
        return (None, SOURCE_WITHHELD);
    }
    match crate::fleet::resolve_device_bearer().await {
        Some((token, source)) => (Some(token), source),
        None => (None, SOURCE_NONE),
    }
}

/// The transport half of [`bearer_for`]: `true` over `https://` or to a
/// loopback host.
fn transport_may_carry_bearer(base: &str) -> bool {
    // Parse with a real URL parser rather than splitting on ':' and '/': a
    // hand split reads `http://localhost:x@evil.com` as host `localhost` and
    // would send the device JWT in cleartext to `evil.com`. `url` resolves
    // the userinfo, IPv6 brackets and ports the way the HTTP client will.
    let Ok(url) = url::Url::parse(base) else {
        return false;
    };
    if url.scheme() == "https" {
        return true;
    }
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The failure text for a non-2xx publish response.
///
/// Status-specific, because the one WARN an episode gets is the operator's
/// only signal that this host fell off fleet telemetry:
/// - 401/403 → the credential was refused, naming which source was sent;
/// - 404 → this coord does not serve the route;
/// - anything else → the status and coord's own body.
///
/// Takes the source NAME, never the token.
fn failure_message(
    status: reqwest::StatusCode,
    device_id: &str,
    body: &str,
    source: &str,
) -> String {
    let head = format!(
        "coord returned {status} for POST /coord/devices/{device_id}/resource-sample: {body}"
    );
    match status.as_u16() {
        401 | 403 => format!("{head} (credential refused — sent: {source})"),
        404 => format!("{head} (route not served by this coord)"),
        _ => head,
    }
}

/// Resolve machine identity, coord URL and CPU count.
///
/// Every step here touches the filesystem (`machine.json`, `profiles.json`),
/// so it runs inside `spawn_blocking` — a stalled volume must
/// not park a tokio worker, and the surrounding code already knows this (the
/// footprint walk is `spawn_blocking`-hosted for the same reason).
///
/// `Err` carries a ready-to-log reason.
fn resolve_target() -> Result<PublishTarget, String> {
    let machine = crate::fleet::load_machine_file().ok_or_else(|| {
        "~/.qontinui/machine.json missing — run `qontinui_profile machine init` on this host"
            .to_string()
    })?;
    let raw = machine
        .device_id()
        .ok_or_else(|| "machine.json has neither device_id nor machine_id".to_string())?;
    // Validate as a UUID exactly as `fleet::publish_budget` does. Pasting an
    // unvalidated string into the URL path would silently produce a different
    // (or invalid) request and take this host off fleet telemetry with no
    // diagnosable cause.
    let device_id = uuid::Uuid::parse_str(raw.trim())
        .map_err(|e| format!("machine.json device_id is not a UUID ({e})"))?
        .to_string();
    let base = crate::fleet::coord_http_base().ok_or_else(|| {
        "~/.qontinui/profiles.json missing or its active profile has no coord_url".to_string()
    })?;
    // Same probe `fleet::detect_resources` uses for `cpu_cores`, without its
    // disk enumeration + memory refresh — this module must not re-sample what
    // the footprint snapshot already carries.
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1);
    Ok(PublishTarget {
        url: format!("{base}/coord/devices/{device_id}/resource-sample"),
        device_id,
        base,
        cpu_cores,
    })
}

/// Best-effort publish of one `lane='host'`, `source='supervisor'` sample.
///
/// Never returns an error and never panics: the caller is the footprint timer,
/// and telemetry must not be able to disturb builds.
pub async fn publish(snapshot: &FootprintSnapshot) {
    let target = match tokio::task::spawn_blocking(resolve_target).await {
        Ok(Ok(t)) => t,
        Ok(Err(reason)) => {
            note_failure(&reason);
            return;
        }
        Err(e) => {
            note_failure(&format!("identity resolution task failed: {e}"));
            return;
        }
    };
    let PublishTarget {
        device_id,
        base,
        url,
        cpu_cores,
    } = target;
    // The shared resolver is async (file read + runner mint), so the bearer
    // is resolved here rather than inside the blocking identity hop.
    let (bearer, bearer_source) = bearer_for(&base).await;

    let payload = payload_from_snapshot(snapshot, cpu_cores);

    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            note_failure(&format!("reqwest builder: {e}"));
            return;
        }
    };
    let mut req = client.post(&url).json(&payload);
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            // Re-arm the warn-once latch: "once" must mean once per failure
            // EPISODE, not once per process. Otherwise one early refusal
            // consumes the only WARN this module will ever emit, and a later
            // real failure — an expired JWT, a coord outage — takes this
            // machine off the fleet dashboard in total silence.
            WARNED_ONCE.store(false, Ordering::Relaxed);
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
            // Coord's own error text is the thing you most want on a real
            // failure; bound it so a stray HTML page can't fill the log.
            let mut body = resp.text().await.unwrap_or_default();
            body.truncate(500);
            note_failure(&failure_message(status, &device_id, &body, bearer_source));
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
            "sampled_at",
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
    fn windows_payload_carries_no_swap_reading() {
        // End-to-end version of the footprint rule, asserted on the WIRE shape
        // because that is what a consumer sees. The sibling runner publisher
        // applies the same platform rule, so `swap_*` presence must describe
        // the machine — never which publisher wrote the row. A Windows row
        // carrying swap would let a ranker read ~0.77 saturation off an idle
        // box, and §C3 forbids absence of signal being read as anything else.
        let mut snap = snapshot_fixture();
        snap.memory = crate::footprint::memory_snapshot();
        let v = serde_json::to_value(payload_from_snapshot(&snap, 1)).unwrap();
        if crate::footprint::SWAP_IS_DERIVED_FROM_COMMIT {
            assert!(
                v["swap_total_bytes"].is_null(),
                "no swap_total may reach coord from this platform"
            );
            assert!(
                v["swap_used_bytes"].is_null(),
                "no swap_used may reach coord from this platform"
            );
            // Withholding swap is only honest if the replacement lead metric
            // is present in the same row.
            assert!(
                !v["commit_available_bytes"].is_null(),
                "the commit pair must carry the saturation signal instead"
            );
        } else {
            assert!(
                !v["swap_total_bytes"].is_null() && !v["swap_used_bytes"].is_null(),
                "a real swap device must be published (a Linux supervisor must not \
                 inherit the Windows omission)"
            );
        }
        // The §A1 columns still exist either way — withheld means null, not a
        // renamed or dropped key.
        assert!(v.get("swap_total_bytes").is_some());
        assert!(v.get("swap_used_bytes").is_some());
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

    #[test]
    fn a_403_names_the_credential_source_and_blames_no_route() {
        let msg = failure_message(
            reqwest::StatusCode::FORBIDDEN,
            "00000000-0000-0000-0000-000000000001",
            r#"{"error":"auth_required"}"#,
            "~/.qontinui/coord-device-jwt",
        );
        assert!(msg.contains("credential refused"), "{msg}");
        assert!(msg.contains("~/.qontinui/coord-device-jwt"), "{msg}");
        assert!(msg.contains("auth_required"), "{msg}");
        assert!(!msg.contains("404 is expected"), "{msg}");
        let unauth = failure_message(reqwest::StatusCode::UNAUTHORIZED, "d", "", SOURCE_NONE);
        assert!(unauth.contains("credential refused"), "{unauth}");
        assert!(unauth.contains(SOURCE_NONE), "{unauth}");
    }

    #[test]
    fn a_404_says_the_route_is_not_served_and_other_statuses_stay_plain() {
        let msg = failure_message(reqwest::StatusCode::NOT_FOUND, "d", "", "COORD_DEVICE_JWT");
        assert!(msg.contains("route not served by this coord"), "{msg}");
        assert!(!msg.contains("credential"), "{msg}");
        let msg = failure_message(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "d",
            "boom",
            "COORD_DEVICE_JWT",
        );
        assert!(msg.ends_with(": boom"), "{msg}");
    }

    #[test]
    fn the_bearer_rides_only_https_or_loopback() {
        assert!(transport_may_carry_bearer("https://coord.qontinui.io"));
        assert!(transport_may_carry_bearer("http://127.0.0.1:9870"));
        assert!(transport_may_carry_bearer("http://localhost:9870/x"));
        assert!(transport_may_carry_bearer("http://[::1]:9870"));
        assert!(transport_may_carry_bearer("ws://[::1]"));
        assert!(transport_may_carry_bearer("http://[::1]/coord"));
        assert!(!transport_may_carry_bearer("http://[2001:db8::1]:9870"));
        assert!(!transport_may_carry_bearer("http://coord.example.com"));
        assert!(!transport_may_carry_bearer("http://10.0.0.5:9870"));
        // Userinfo must not smuggle a remote host past the loopback test.
        assert!(!transport_may_carry_bearer("http://localhost:x@evil.com"));
        assert!(!transport_may_carry_bearer("ws://127.0.0.1@evil.com:9870"));
        assert!(!transport_may_carry_bearer("not a url"));
    }

    /// The withheld arm never reaches the resolver (no env, file or mint is
    /// consulted), and its label says why nothing was sent.
    #[tokio::test]
    async fn a_cleartext_non_loopback_base_withholds_the_bearer() {
        let (bearer, source) = bearer_for("http://coord.example.com").await;
        assert!(bearer.is_none());
        assert_eq!(source, SOURCE_WITHHELD);
    }
}
