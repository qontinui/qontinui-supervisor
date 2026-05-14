//! Fleet topology publisher (Row 2 Phase 1, supervisor side).
//!
//! See `plans/2026-05-14-fleet-topology-and-build-pool-design.md` §3.2.
//! The supervisor is the natural host of the **Build** role in the
//! fleet model — it owns the build pool slot semaphore (today's three
//! per-machine concurrent cargo builds) and is the only process per
//! machine that can authoritatively answer "how many concurrent builds
//! can I sustain?".
//!
//! On startup the supervisor:
//!
//! 1. Reads the local machine identity from `~/.qontinui/machine.json`
//!    (already minted by `qontinui_profile machine init`).
//! 2. Detects local CPU + RAM + disk via `sysinfo`.
//! 3. Computes `max_concurrent_builds = min(memory_gb / 4, cpu_cores / 4)`
//!    per §3.2.
//! 4. POSTs the budget to qontinui-coord's `/coord/machines/budget`
//!    endpoint. The coord URL is sourced from
//!    `~/.qontinui/profiles.json`'s active profile.
//!
//! Why HTTP not direct PG: the supervisor has no PG dependency today
//! and adding tokio-postgres + a connection pool is far heavier than
//! one reqwest POST. The runner-side publisher does direct PG because
//! it already has a PG pool open from main.rs PG bootstrap.
//!
//! Phase 1 is visibility-only. Failures log a warning and the
//! supervisor still boots.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// §3.2 declared role for the supervisor. Always `Build` from the
/// supervisor's POV — even on dev workstations where the runner has
/// already published `Agent`, the supervisor authoritatively owns
/// `max_concurrent_builds`. Last-writer-wins on the `role` column for
/// Phase 1; visibility-only readers tolerate either.
const ROLE: &str = "build";

/// Detected local resources, all in CPU-core / GiB units.
#[derive(Debug, Clone, Copy)]
pub struct Resources {
    pub cpu_cores: u32,
    pub memory_gb: u32,
    pub disk_total_gb: u64,
}

/// §3.2 policy: `min(floor(memory_gb / 4), floor(cpu_cores / 4))`.
/// 4 GiB + 4 cores per build slot — empirical from cold qontinui-runner
/// builds with LLD linking.
pub fn derive_max_builds(memory_gb: u32, cpu_cores: u32) -> u32 {
    (memory_gb / 4).min(cpu_cores / 4)
}

/// Detect cpu_cores / memory_gb / disk_total_gb on the current host.
/// `cpu_cores` uses [`std::thread::available_parallelism`] (cgroup-aware
/// on Linux). Disks dedupe by mount-point.
pub fn detect_resources() -> Resources {
    use sysinfo::{Disks, System};

    let cpu_cores: u32 = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or_else(|_| System::new_all().cpus().len() as u32)
        .max(1);

    let mut sys = System::new();
    sys.refresh_memory();
    let memory_gb: u32 = (sys.total_memory() / (1024 * 1024 * 1024)).min(u32::MAX as u64) as u32;

    let mut seen = std::collections::HashSet::<PathBuf>::new();
    let disks = Disks::new_with_refreshed_list();
    let mut total_bytes: u64 = 0;
    for d in disks.list() {
        if seen.insert(d.mount_point().to_path_buf()) {
            total_bytes = total_bytes.saturating_add(d.total_space());
        }
    }
    let disk_total_gb: u64 = total_bytes / (1024 * 1024 * 1024);

    Resources {
        cpu_cores,
        memory_gb,
        disk_total_gb,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MachineFile {
    machine_id: String,
    hostname: String,
}

/// `~/.qontinui/profiles.json` — minimum subset we need (the active
/// profile's `coord_url`). Mirrors `qontinui_runner_lib::profiles` so
/// we don't pull the whole crate in.
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
/// Profile stores `ws://host:9870/ws` (the WebSocket upgrade URL); we
/// convert that to `http://host:9870` so reqwest can POST to
/// `/coord/machines/budget`. Returns `None` if profiles.json is
/// missing or the active profile has no coord_url.
fn coord_http_base() -> Option<String> {
    let bytes = std::fs::read(profiles_path()?).ok()?;
    let pf: ProfilesFile = serde_json::from_slice(&bytes).ok()?;
    let active = pf.active.as_deref().unwrap_or("dev");
    let coord_url = pf.profiles.get(active)?.coord_url.as_deref()?;

    // Strip the `/ws` suffix if present, then swap ws→http and wss→https.
    // The url crate's parse + scheme swap is overkill for this; explicit
    // string manipulation keeps it inspectable.
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

#[derive(Debug, Serialize)]
struct BudgetPayload {
    machine_id: uuid::Uuid,
    role: &'static str,
    cpu_cores: u32,
    memory_gb: u32,
    disk_total_gb: u64,
    disk_reserved_gb: u64,
    max_concurrent_agents: u32,
    max_concurrent_builds: u32,
    hostname: String,
}

/// Best-effort publish: POST the supervisor's MachineBudget to coord.
/// Failures log a warning and return `Ok(())` so they don't break
/// supervisor boot.
pub async fn publish_budget(
    role: &'static str,
    resources: Resources,
    disk_reserved_gb: u64,
) -> Result<(), String> {
    let machine = match load_machine_file() {
        Some(m) => m,
        None => {
            warn!(
                "fleet::publish_budget: ~/.qontinui/machine.json missing — \
                 run `qontinui_profile machine init` on this host to enable fleet visibility. Skipping."
            );
            return Ok(());
        }
    };
    let machine_id = match uuid::Uuid::parse_str(&machine.machine_id) {
        Ok(id) => id,
        Err(e) => {
            warn!("fleet::publish_budget: machine.json machine_id not a UUID ({e}). Skipping.");
            return Ok(());
        }
    };
    let base = match coord_http_base() {
        Some(b) => b,
        None => {
            warn!(
                "fleet::publish_budget: ~/.qontinui/profiles.json missing or has no \
                 coord_url in the active profile. Skipping."
            );
            return Ok(());
        }
    };

    let max_concurrent_builds = derive_max_builds(resources.memory_gb, resources.cpu_cores);
    let payload = BudgetPayload {
        machine_id,
        role,
        cpu_cores: resources.cpu_cores,
        memory_gb: resources.memory_gb,
        disk_total_gb: resources.disk_total_gb,
        disk_reserved_gb,
        // Supervisor owns the build-side budget. Agent caps stay owned
        // by the runner publisher; we leave them at 0 here so we
        // don't accidentally wipe what the runner already wrote on a
        // dev workstation.
        max_concurrent_agents: 0,
        max_concurrent_builds,
        hostname: machine.hostname.clone(),
    };

    let url = format!("{base}/coord/machines/budget");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("reqwest builder: {e}"))?;
    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        info!(
            "fleet::publish_budget: published role={role} machine_id={machine_id} \
             max_concurrent_builds={max_concurrent_builds} (cpu={} mem_gb={} disk_gb={})",
            resources.cpu_cores, resources.memory_gb, resources.disk_total_gb
        );
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "coord returned {status} for POST /coord/machines/budget: {body}"
        ))
    }
}

/// Convenience: detect + publish on startup. Spawned from `main.rs`
/// as a non-blocking task — supervisor boots immediately, fleet
/// publish settles in the background.
pub async fn publish_on_startup() {
    let resources = detect_resources();
    if let Err(e) = publish_budget(ROLE, resources, 0).await {
        warn!("fleet::publish_on_startup failed (non-fatal): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_max_builds_takes_min_of_mem_and_cpu() {
        // §3.2 examples — keep aligned with qontinui-coord/src/fleet.rs tests.
        assert_eq!(derive_max_builds(32, 16), 4); // min(8, 4)
        assert_eq!(derive_max_builds(64, 8), 2); // CPU-bound: min(16, 2)
        assert_eq!(derive_max_builds(16, 32), 4); // mem-bound: min(4, 8)
        assert_eq!(derive_max_builds(2, 2), 0); // tiny
    }

    #[test]
    fn detect_returns_non_zero_on_dev_host() {
        let r = detect_resources();
        assert!(r.cpu_cores >= 1);
        assert!(r.memory_gb >= 1);
        assert!(r.disk_total_gb >= 1);
    }
}
