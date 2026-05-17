//! Fleet health-URL advertiser (Row 9 Phase 3 fill-in).
//!
//! See `plans/2026-05-17-fleet-health-url-advertisement-plan.md` §Phase 3.
//! `coord.health_watcher` only polls machines whose `coord.machines.health_url`
//! column is non-NULL. Until this module landed, no component anywhere wrote
//! to that column, so the watcher idled across the fleet and Phase 3/4
//! produced no signal.
//!
//! On startup the supervisor:
//!
//! 1. Reads `~/.qontinui/machine.json` for `{machine_id, hostname}` (same
//!    file used by `fleet::publish_on_startup`).
//! 2. Resolves the coord HTTP base from the active profile's `coord_url`
//!    in `~/.qontinui/profiles.json` (also same as `fleet::publish_on_startup`).
//! 3. Computes the advertised health URL: env
//!    `QONTINUI_SUPERVISOR_PUBLIC_HEALTH_URL` overrides; otherwise the
//!    default `http://host.docker.internal:9875/health` (works for the
//!    local-dev coord-in-Docker case).
//! 4. POSTs `{machine_id, hostname, health_url}` to
//!    `{coord}/coord/machine/register` (coord PR #37 endpoint).
//!
//! Then it loops every 5 min (± 30s deterministic jitter, derived from
//! `machine_id` so cadence is stable per host but de-synced across the
//! fleet) and POSTs the same URL to `/coord/machines/:id/health-url` to
//! refresh `last_seen_at`. The 5min base aligns with the watcher's
//! `partitioned_min_secs: 300` dwell threshold so one missed tick never
//! trips a state transition.
//!
//! Both calls warn-and-continue on failure — never block startup, never
//! panic.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Default health URL advertised to coord when
/// `QONTINUI_SUPERVISOR_PUBLIC_HEALTH_URL` is not set.
/// `host.docker.internal` works for the local-dev coord-in-Docker case
/// (Docker Desktop on Win/Mac + a Linux `host-gateway` mapping).
/// Production multi-machine setups must set the env var.
const DEFAULT_HEALTH_URL: &str = "http://host.docker.internal:9875/health";

/// Env override for the advertised health URL.
const ENV_PUBLIC_HEALTH_URL: &str = "QONTINUI_SUPERVISOR_PUBLIC_HEALTH_URL";

/// Heartbeat base cadence: matches `partitioned_min_secs: 300` so a single
/// missed tick can never trip a `degraded → partitioned` transition.
const HEARTBEAT_BASE: Duration = Duration::from_secs(300);

/// Per-tick jitter range (±). Deterministic-per-machine so each host
/// settles on a stable phase but the fleet is de-synced — avoids
/// thundering-herd on the 5-min boundary.
const HEARTBEAT_JITTER_SECS: i64 = 30;

/// Per-request HTTP timeout for register + heartbeat POSTs.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize)]
struct MachineFile {
    machine_id: String,
    hostname: String,
}

/// `~/.qontinui/profiles.json` — minimum subset we need (the active
/// profile's `coord_url`). Duplicates `fleet::ProfilesFile` to keep
/// this module self-contained; the file is tiny and the extraction
/// would only save ~15 LoC.
#[derive(Debug, Clone, Deserialize)]
struct ProfilesFile {
    active: Option<String>,
    profiles: std::collections::HashMap<String, ProfileSubset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileSubset {
    coord_url: Option<String>,
}

fn machine_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("machine.json"))
}

fn profiles_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("profiles.json"))
}

fn load_machine_file() -> Option<MachineFile> {
    let bytes = std::fs::read(machine_file_path()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Resolve the coord HTTP base from the active profile's `coord_url`.
/// Mirrors `fleet::coord_http_base` exactly (kept duplicated rather than
/// `pub(crate)`-exporting to keep `fleet`'s public surface unchanged).
fn coord_http_base() -> Option<String> {
    let bytes = std::fs::read(profiles_path()?).ok()?;
    let pf: ProfilesFile = serde_json::from_slice(&bytes).ok()?;
    let active = pf.active.as_deref().unwrap_or("dev");
    let coord_url = pf.profiles.get(active)?.coord_url.as_deref()?;

    let trimmed = coord_url.trim_end_matches("/ws");
    let with_http = trimmed
        .strip_prefix("wss://")
        .map(|rest| format!("https://{rest}"))
        .or_else(|| {
            trimmed
                .strip_prefix("ws://")
                .map(|rest| format!("http://{rest}"))
        })
        .unwrap_or_else(|| trimmed.to_string());
    Some(with_http)
}

/// Resolve the URL we advertise to coord. Env override wins; default is
/// `http://host.docker.internal:9875/health`. Pure function: the only
/// input is the process environment, so it's trivially testable by
/// flipping the env var in-test.
pub(crate) fn resolve_health_url() -> String {
    std::env::var(ENV_PUBLIC_HEALTH_URL)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HEALTH_URL.to_string())
}

/// Deterministic per-machine jitter in `[-HEARTBEAT_JITTER_SECS,
/// HEARTBEAT_JITTER_SECS]`. We hash the machine_id (a UUID) and map the
/// low bits into the symmetric range. Same machine → same phase across
/// restarts; different machines → de-synced cadence.
fn deterministic_jitter_secs(machine_id: uuid::Uuid) -> i64 {
    // FNV-1a 64-bit on the UUID bytes. We don't need cryptographic
    // strength here — uniform-enough distribution is all that matters.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in machine_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let span = (HEARTBEAT_JITTER_SECS * 2 + 1) as u64;
    let in_range = (h % span) as i64;
    in_range - HEARTBEAT_JITTER_SECS
}

#[derive(Debug, Serialize)]
struct RegisterPayload {
    machine_id: uuid::Uuid,
    hostname: String,
    health_url: String,
}

#[derive(Debug, Serialize)]
struct HealthUrlPayload {
    health_url: String,
}

/// Best-effort one-shot register. Reads identity + coord URL, POSTs to
/// `/coord/machine/register`. Failure logs at warn and returns without
/// raising — never blocks startup, never panics.
pub async fn register_on_startup() {
    let machine = match load_machine_file() {
        Some(m) => m,
        None => {
            warn!(
                "health_advertiser::register_on_startup: ~/.qontinui/machine.json missing — \
                 run `qontinui_profile machine init` on this host to enable fleet health \
                 advertisement. Skipping."
            );
            return;
        }
    };
    let machine_id = match uuid::Uuid::parse_str(&machine.machine_id) {
        Ok(id) => id,
        Err(e) => {
            warn!(
                "health_advertiser::register_on_startup: machine.json machine_id not a \
                 UUID ({e}). Skipping."
            );
            return;
        }
    };
    let base = match coord_http_base() {
        Some(b) => b,
        None => {
            warn!(
                "health_advertiser::register_on_startup: ~/.qontinui/profiles.json missing \
                 or has no coord_url in the active profile. Skipping."
            );
            return;
        }
    };
    let health_url = resolve_health_url();

    let payload = RegisterPayload {
        machine_id,
        hostname: machine.hostname.clone(),
        health_url: health_url.clone(),
    };
    let url = format!("{base}/coord/machine/register");

    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            warn!("health_advertiser::register_on_startup: reqwest builder failed: {e}");
            return;
        }
    };

    match client.post(&url).json(&payload).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                info!(
                    "health_advertiser::register_on_startup: registered machine_id={} \
                     hostname={} health_url={}",
                    machine_id, machine.hostname, health_url
                );
            } else {
                let body = resp.text().await.unwrap_or_default();
                warn!(
                    "health_advertiser::register_on_startup: coord returned {} for POST {} \
                     (body: {}). Heartbeat will retry every {}s.",
                    status,
                    url,
                    body,
                    HEARTBEAT_BASE.as_secs()
                );
            }
        }
        Err(e) => {
            warn!(
                "health_advertiser::register_on_startup: POST {} failed: {}. Heartbeat \
                 will retry every {}s.",
                url,
                e,
                HEARTBEAT_BASE.as_secs()
            );
        }
    }
}

/// Periodic heartbeat: refresh `health_url` every `HEARTBEAT_BASE ± jitter`.
/// Each tick re-reads `machine.json` + `profiles.json` so config-file
/// edits take effect without a supervisor restart. Each POST also
/// touches `last_seen_at` on the coord side via the same UPSERT,
/// doubling as a fleet-health liveness signal.
///
/// Runs forever. Caller spawns it via `tokio::spawn` after
/// `register_on_startup` resolves so the first tick doesn't race a
/// still-in-flight registration. Failures log at warn and the loop
/// continues — never panics.
pub async fn heartbeat_loop() {
    loop {
        let sleep_secs = match next_sleep_secs() {
            Some(s) => s,
            None => {
                // Identity not resolvable right now; back off one base
                // cycle and retry on the next loop. (machine.json may
                // appear later — `qontinui_profile machine init` after
                // supervisor boot is a supported flow.)
                HEARTBEAT_BASE.as_secs()
            }
        };
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        send_one_heartbeat().await;
    }
}

/// Compute the next sleep duration in seconds: `HEARTBEAT_BASE` plus the
/// per-machine deterministic jitter. Returns `None` if the machine
/// identity can't be loaded yet (caller defaults to the base cycle).
fn next_sleep_secs() -> Option<u64> {
    let machine = load_machine_file()?;
    let machine_id = uuid::Uuid::parse_str(&machine.machine_id).ok()?;
    let jitter = deterministic_jitter_secs(machine_id);
    let base = HEARTBEAT_BASE.as_secs() as i64;
    Some((base + jitter).max(1) as u64)
}

/// One heartbeat POST. All failure modes log at warn; never panics.
async fn send_one_heartbeat() {
    let machine = match load_machine_file() {
        Some(m) => m,
        None => {
            warn!(
                "health_advertiser::heartbeat: ~/.qontinui/machine.json missing. Skipping \
                 this tick."
            );
            return;
        }
    };
    let machine_id = match uuid::Uuid::parse_str(&machine.machine_id) {
        Ok(id) => id,
        Err(e) => {
            warn!(
                "health_advertiser::heartbeat: machine.json machine_id not a UUID ({e}). \
                 Skipping this tick."
            );
            return;
        }
    };
    let base = match coord_http_base() {
        Some(b) => b,
        None => {
            warn!(
                "health_advertiser::heartbeat: ~/.qontinui/profiles.json missing or has \
                 no coord_url. Skipping this tick."
            );
            return;
        }
    };
    let health_url = resolve_health_url();

    let payload = HealthUrlPayload {
        health_url: health_url.clone(),
    };
    let url = format!("{base}/coord/machines/{machine_id}/health-url");

    let client = match reqwest::Client::builder().timeout(HTTP_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            warn!("health_advertiser::heartbeat: reqwest builder failed: {e}");
            return;
        }
    };

    match client.post(&url).json(&payload).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                info!(
                    "health_advertiser::heartbeat: refreshed machine_id={} health_url={}",
                    machine_id, health_url
                );
            } else {
                let body = resp.text().await.unwrap_or_default();
                warn!(
                    "health_advertiser::heartbeat: coord returned {} for POST {} \
                     (body: {}).",
                    status, url, body
                );
            }
        }
        Err(e) => {
            warn!("health_advertiser::heartbeat: POST {} failed: {}.", url, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env-var tests via a process-wide mutex — `std::env::set_var`
    /// is process-global and clobbers other tests that read the same key.
    /// `serial_test` isn't a dep here; a local mutex is enough since this
    /// module owns the only callers of this env var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, val);
            Self { key, prior }
        }
        fn unset(key: &'static str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prior }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn resolve_health_url_default_when_env_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::unset(ENV_PUBLIC_HEALTH_URL);
        assert_eq!(resolve_health_url(), DEFAULT_HEALTH_URL);
    }

    #[test]
    fn resolve_health_url_env_override_wins() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set(ENV_PUBLIC_HEALTH_URL, "https://example.com:443/health");
        assert_eq!(resolve_health_url(), "https://example.com:443/health");
    }

    #[test]
    fn resolve_health_url_empty_env_treated_as_unset() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set(ENV_PUBLIC_HEALTH_URL, "   ");
        assert_eq!(resolve_health_url(), DEFAULT_HEALTH_URL);
    }

    #[test]
    fn deterministic_jitter_is_in_range() {
        // Sample a variety of UUIDs and confirm every output sits in the
        // symmetric ±HEARTBEAT_JITTER_SECS range.
        for _ in 0..1000 {
            let id = uuid::Uuid::new_v4();
            let j = deterministic_jitter_secs(id);
            assert!(
                j >= -HEARTBEAT_JITTER_SECS && j <= HEARTBEAT_JITTER_SECS,
                "jitter {} outside [-{}, {}] for {}",
                j,
                HEARTBEAT_JITTER_SECS,
                HEARTBEAT_JITTER_SECS,
                id
            );
        }
    }

    #[test]
    fn deterministic_jitter_is_stable_per_machine() {
        let id = uuid::Uuid::parse_str("c79a07d5-7e40-49b4-87fa-554c749f9644").unwrap();
        let a = deterministic_jitter_secs(id);
        let b = deterministic_jitter_secs(id);
        assert_eq!(a, b, "same uuid must yield same jitter");
    }
}
