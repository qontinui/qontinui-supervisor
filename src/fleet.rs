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
//! 4. POSTs the budget to qontinui-coord's `/coord/devices/:id/budget`
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
//!
//! # Column ownership: omission, not a placeholder value
//!
//! `coord.devices` is ONE row per machine and TWO processes publish to
//! it — this supervisor and the primary `qontinui-runner`. They own
//! disjoint columns, and each expresses "I do not own this" by
//! **omitting the field**, never by sending a default.
//!
//! | Column | Owner | Why |
//! |--------|-------|-----|
//! | `max_concurrent_builds` | **supervisor** | it holds the build-pool slot semaphore |
//! | `cpu_cores`, `memory_gb`, `disk_total_gb` | supervisor (measured) | they are the INPUTS to `derive_max_builds`, so publishing them beside the derived cap keeps the row self-explaining |
//! | `role` | supervisor | it is the only writer of `'build'`, which coord's `build_dispatcher::select_build_machine` requires (`WHERE role = 'build'`) — omitting it would silently disable remote build dispatch |
//! | `disk_reserved_gb` | either (hardware, not process, state) | both publishers send the same figure; omitting it on both sides would only NULL a real column |
//! | `hostname` | either (coord already `COALESCE`s it) | read from `machine.json`, identical on both sides |
//! | `max_concurrent_agents` | **runner** | omitted here |
//!
//! **The CI-runner columns are NOT on this route.** This module used to send
//! `ci_runner_labels` / `ci_runner_status` in the budget payload. Coord's
//! `BudgetPublishRequest` declares no such fields and `upsert_budget`'s SQL
//! never writes them, and there is no `deny_unknown_fields` — so both keys
//! were **silently discarded**. `coord.devices`'s CI-runner columns are
//! written by the device-register path (`device_state.rs`) and
//! `ci_runner_registrar.rs`. The fields, and the never-called
//! `publish_budget_with_ci` that fed them, are deleted rather than
//! documented: a wire contract the peer does not implement is worse than
//! no contract, because tests can pin it and read as coverage.
//!
//! The rule is narrow on purpose: **omit only where the two writers
//! genuinely disagree about a column's meaning.** Two writers observing the
//! same box and reporting the same number are not in conflict, and dropping
//! such a field from both sides loses real data for no benefit.
//!
//! This omission contract requires coord to declare `max_concurrent_agents`
//! `Option` and write it `SET col = COALESCE($n, col)` — see the ordering
//! note on `BudgetPayload`.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// §3.2 declared role for the supervisor. Always `Build` from the
/// supervisor's POV — even on dev workstations where the runner has
/// already published `Agent`.
///
/// **This is the one contested column the supervisor still asserts, and
/// deliberately so.** `coord.devices.role` is single-valued while the box
/// genuinely serves both roles, so the runner's `'agent'` and this
/// `'build'` overwrite each other last-writer-wins. Omitting it here would
/// not resolve the contention — it would settle it permanently on
/// `'agent'`, and coord's `build_dispatcher::select_build_machine` selects
/// `WHERE role = 'build'`, so the machine would silently stop being
/// dispatchable. Modelling role as a set is a coord-side data-model fix,
/// not something a publisher can express by staying quiet.
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
pub struct MachineFile {
    /// Canonical post-unified-devices field. The live `machine.json` carries
    /// `device_id`; older hosts used `machine_id`. Accept either (prefer
    /// `device_id`) — making both optional is REQUIRED so deserialization
    /// doesn't fail outright on a `device_id`-only file (which previously made
    /// `load_machine_file` return `None` and silently skipped all budget
    /// publishing — fixed 2026-06-08).
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    machine_id: Option<String>,
    hostname: String,
}

impl MachineFile {
    /// The device id, preferring the canonical `device_id` over legacy
    /// `machine_id`.
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref().or(self.machine_id.as_deref())
    }
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

pub fn load_machine_file() -> Option<MachineFile> {
    let bytes = std::fs::read(machine_file_path()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Resolve the coord HTTP base from the active profile's `coord_url`.
/// Profile stores `ws://host:9870/ws` (the WebSocket upgrade URL); we
/// convert that to `http://host:9870` so reqwest can POST to
/// `/coord/devices/:id/budget`. Returns `None` if profiles.json is
/// missing or the active profile has no coord_url.
pub fn coord_http_base() -> Option<String> {
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

/// Wire shape of `POST /coord/devices/{device_id}/budget`, carrying ONLY
/// the columns this supervisor owns (see the module docs).
///
/// **`max_concurrent_agents` is deliberately absent from this struct**, not
/// an `Option` field pinned to `None`: a field the supervisor can never
/// populate is most honestly encoded by not existing, and its absence is
/// then unrepresentable-if-wrong rather than one edit away from sending a
/// zero again.
///
/// **Ordering dependency.** Coord must declare `max_concurrent_agents`
/// `Option` and write it `SET col = COALESCE($n, col)` before this payload
/// is deployed — until then `BudgetPublishRequest` declares it mandatory
/// and a publish that omits it is rejected outright.
#[derive(Debug, Serialize)]
struct BudgetPayload {
    role: &'static str,
    cpu_cores: u32,
    memory_gb: u32,
    disk_total_gb: u64,
    disk_reserved_gb: u64,
    max_concurrent_builds: u32,
    hostname: String,
}

/// Build the budget payload. Pure — no IO — so the omission contract is
/// asserted against the real construction path rather than a hand-built
/// struct in the tests.
fn build_budget_payload(
    role: &'static str,
    resources: Resources,
    disk_reserved_gb: u64,
    hostname: String,
) -> BudgetPayload {
    BudgetPayload {
        role,
        cpu_cores: resources.cpu_cores,
        memory_gb: resources.memory_gb,
        disk_total_gb: resources.disk_total_gb,
        disk_reserved_gb,
        max_concurrent_builds: derive_max_builds(resources.memory_gb, resources.cpu_cores),
        hostname,
    }
}

/// Env var carrying a coord device JWT, checked first. This is the
/// fleet-wide convention (`/coord-revive`, `render-memory-cache.ps1`,
/// `qontinui-runner`'s own tooling all read the same name).
const DEVICE_JWT_ENV: &str = "COORD_DEVICE_JWT";

/// How long the UI-Bridge mint may take before we give up and publish
/// unauthenticated. Deliberately shorter than the publish's own budget: a
/// WEDGED runner answers its doors slowly or not at all, and a credential
/// we cannot get in a few seconds is a credential we do not have.
const MINT_TIMEOUT: Duration = Duration::from_secs(3);

/// Gate so the no-credential warning is logged at most once per process.
static MISSING_BEARER_WARNED: std::sync::Once = std::sync::Once::new();

fn device_jwt_file_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("coord-device-jwt"))
}

/// JWS-compact shape check: exactly three non-empty base64url segments.
///
/// Values are shape-checked before they are trusted because a **signed-out**
/// runner answers 200 with an empty or opaque value, and presenting that as
/// a bearer converts "no credential" into a 401 the caller has to decode.
///
/// The **charset** half of the check is load-bearing, not decoration. A
/// segment-count-only check accepts two values that then do real damage:
///
/// - a JSON-wrapped credential file — `{"token":"a.b.c"}` splits into
///   exactly three non-empty segments — is attached as a garbage bearer;
/// - a token carrying an interior **CR/LF**, which a CRLF-written or
///   hard-wrapped file has and `trim()` cannot reach, passes the count
///   check and then makes `HeaderValue` reject it, so `.send()` errors and
///   the budget publish STOPS. (An interior *space* or *tab* is a legal
///   header-value byte, so that one lands in the first bucket, not this
///   one — it produces a garbage bearer rather than a build failure.)
///
/// Base64url is `[A-Za-z0-9_-]`, which excludes every character that
/// produces either failure.
fn looks_like_jwt(token: &str) -> bool {
    let mut segments = token.split('.');
    let three = [segments.next(), segments.next(), segments.next()];
    if segments.next().is_some() {
        return false;
    }
    three.iter().all(|segment| {
        segment.is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    })
}

/// Mint a device JWT from the **primary** runner's UI Bridge by invoking
/// the runner's `get_coord_device_token` command. The runner holds the
/// device credential in its own encrypted `SecureStorage`, which this
/// process cannot read — asking the runner for one is the only door.
///
/// `get_coord_device_token`, not `get_access_token_for_websocket`: the
/// latter is gated by `require_tier_2()` and by a `has_tokens()` check that
/// wants BOTH the access and refresh slots populated (pairing writes the
/// refresh slot empty), and it applies no shape check, so on a
/// pre-migration install it hands back a legacy opaque
/// `qontinui_runner_<random>` bearer. `get_coord_device_token` is the
/// purpose-built probe: no tier gate, shape-checked runner-side, and it
/// distinguishes "unpaired" (`null`) from "store unreadable" (a rejection).
///
/// Fixed to `RUNNER_API_PORT` on purpose: the credential being asked for is
/// the MACHINE's device identity, and the primary is the instance that owns
/// machine-scoped state. Temp runners on 9877-9899 are not asked.
///
/// Returns `None` for every failure mode (no runner, wedged runner,
/// signed-out runner, non-JWT answer). The token is kept in-process: it is
/// never written to disk, never placed on a command line, and never logged.
async fn mint_device_jwt_from_runner() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(MINT_TIMEOUT)
        // The mint targets the loopback. reqwest honours `HTTP_PROXY` /
        // `ALL_PROXY` but only exempts hosts named in `NO_PROXY`, so a box
        // with a proxy in the environment would route this at the proxy and
        // fail it. A loopback call must never be proxied.
        .no_proxy()
        .build()
        .ok()?;
    // 127.0.0.1, never `localhost`: Windows resolves `localhost` to ::1
    // first and the runner binds the IPv4 loopback only, so the name form
    // pays a doomed IPv6 connect before the socket that answers.
    let url = format!(
        "http://127.0.0.1:{}/ui-bridge/control/page/evaluate",
        crate::config::RUNNER_API_PORT
    );
    // `PageEvaluateRequest` is `rename_all = "camelCase"` — `await_promise`
    // would be silently dropped (no `deny_unknown_fields`) and default to
    // false. `timeoutMs` bounds the runner-side evaluation so giving up at
    // MINT_TIMEOUT does not strand an orphaned 10s evaluation on the runner.
    let body = serde_json::json!({
        "expression": "window.__TAURI__.core.invoke(\"get_coord_device_token\")",
        "awaitPromise": true,
        "timeoutMs": MINT_TIMEOUT.as_millis() as u64,
    });
    let resp = client.post(&url).json(&body).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let value: serde_json::Value = resp.json().await.ok()?;
    parse_minted_token(&value)
}

/// Pull the minted token out of a UI-Bridge `page/evaluate` response body
/// (`{data: {result: {value: "<token>"}}}`), returning it only when it is
/// shaped like a JWT.
fn parse_minted_token(body: &serde_json::Value) -> Option<String> {
    let minted = body.pointer("/data/result/value")?.as_str()?.trim();
    if looks_like_jwt(minted) {
        return Some(minted.to_string());
    }
    if !minted.is_empty() {
        warn!(
            "fleet::publish_budget: the runner answered the token mint with a non-JWT value \
             (signed out?) — ignoring it and publishing unauthenticated."
        );
    }
    None
}

/// Resolve a coord device JWT, returning it with the name of the source
/// that produced it (for logging — the token itself is never logged).
///
/// Cascade, most-explicit first:
/// 1. `$COORD_DEVICE_JWT`;
/// 2. `~/.qontinui/coord-device-jwt`;
/// 3. a mint from the local runner's UI Bridge.
///
/// Rungs 1 and 2 are the operator-seedable doors — they are what lets a
/// **build-only box with no runner** authenticate at all, since rung 3
/// structurally cannot answer there. On a workstation running a signed-in
/// primary runner, rung 3 is the one that resolves.
async fn resolve_device_bearer() -> Option<(String, &'static str)> {
    let env_token = std::env::var(DEVICE_JWT_ENV).ok();
    if let Some(token) = usable_token(env_token.as_deref(), DEVICE_JWT_ENV) {
        return Some((token, DEVICE_JWT_ENV));
    }
    let file_token = match device_jwt_file_path() {
        Some(p) => tokio::fs::read_to_string(p).await.ok(),
        None => None,
    };
    if let Some(token) = usable_token(file_token.as_deref(), "~/.qontinui/coord-device-jwt") {
        return Some((token, "~/.qontinui/coord-device-jwt"));
    }
    mint_device_jwt_from_runner()
        .await
        .map(|token| (token, "runner UI Bridge mint"))
}

/// Trim a seeded token and accept it only if it is shaped like a JWT.
///
/// The seeded rungs are shape-checked for the same reason the mint is, plus
/// one of their own: an operator-authored file or env var can carry a
/// trailing newline or an outright wrong value, and a header value built
/// from that makes `RequestBuilder::build()` fail — turning "bad
/// credential" into "the budget publish stopped happening". Degrading to
/// the next rung keeps the publish alive and says why.
fn usable_token(raw: Option<&str>, source: &str) -> Option<String> {
    let token = raw?.trim();
    if token.is_empty() {
        return None;
    }
    if !looks_like_jwt(token) {
        warn!("fleet::publish_budget: {source} holds a non-JWT value — ignoring it.");
        return None;
    }
    Some(token.to_string())
}

/// Attach `Authorization: Bearer <device JWT>` when one is resolvable.
///
/// Mirrors the runner's `auth::attach_device_auth` posture: attach when
/// available, **never fail**. Coord still accepts anonymous budget publishes
/// (plan `2026-08-03-per-instance-device-identity` Phase 3(b) is what changes
/// that), so a supervisor with no reachable credential must keep publishing
/// rather than drop off the fleet.
///
/// The `info!` naming the source — and the warn-once naming its absence —
/// are this process's half of Phase 3(a)'s accept-and-log evidence. They
/// carry the source's NAME, never the token.
///
/// ⚠ **Never `{:?}` the request or the builder after this point.**
/// `http::HeaderMap`'s `Debug` renders header values verbatim and does not
/// redact `Authorization`, so a single `{req:?}` in a log or an error would
/// print the device credential. `the_bearer_never_reaches_the_body_but_debug_would_expose_it`
/// pins that as measured fact rather than leaving it to be rediscovered.
/// The publish's own error string formats the URL and the `reqwest::Error`
/// only, neither of which carries headers.
async fn attach_device_auth(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    attach_resolved_bearer(rb, resolve_device_bearer().await)
}

/// The decision half of [`attach_device_auth`], split out from the
/// resolution half so both arms are assertable without a network, a
/// process-global env mutation, or a running runner.
fn attach_resolved_bearer(
    rb: reqwest::RequestBuilder,
    resolved: Option<(String, &'static str)>,
) -> reqwest::RequestBuilder {
    match resolved {
        Some((token, source)) => {
            info!("fleet::publish_budget: attaching coord device JWT (source: {source})");
            rb.header("Authorization", format!("Bearer {token}"))
        }
        None => {
            MISSING_BEARER_WARNED.call_once(|| {
                warn!(
                    "fleet::publish_budget: no coord device JWT available (no {DEVICE_JWT_ENV}, \
                     no ~/.qontinui/coord-device-jwt, and the local runner did not mint one) — \
                     publishing ANONYMOUSLY. Seed one of the two files/vars to authenticate on a \
                     box with no runner."
                );
            });
            rb
        }
    }
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
    let device_id = match machine.device_id() {
        Some(raw) => match uuid::Uuid::parse_str(raw) {
            Ok(id) => id,
            Err(e) => {
                warn!("fleet::publish_budget: machine.json device_id not a UUID ({e}). Skipping.");
                return Ok(());
            }
        },
        None => {
            warn!(
                "fleet::publish_budget: machine.json has neither device_id nor machine_id. Skipping."
            );
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

    // The supervisor owns the build-side columns and OMITS the agent cap it
    // does not own — see the module docs. Omission is the only way to say
    // "leave this column alone": coord writes each budget column with a
    // straight `SET`, so a placeholder value is a write, not an abstention.
    // Sending `max_concurrent_agents: 0` used to FLAP the runner's real cap
    // to zero on every supervisor publish until the runner's 10-minute
    // republisher re-asserted it. That flap is exactly the 2026-07-28
    // outage's mechanism — a writer emitting a default over another writer's
    // real value — and it became an outage that day only because the victim
    // publish was a one-shot with nothing to re-assert it.
    let payload = build_budget_payload(role, resources, disk_reserved_gb, machine.hostname);
    let max_concurrent_builds = payload.max_concurrent_builds;

    let url = format!("{base}/coord/devices/{device_id}/budget");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("reqwest builder: {e}"))?;
    let resp = attach_device_auth(client.post(&url).json(&payload))
        .await
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        info!(
            "fleet::publish_budget: published role={role} device_id={device_id} \
             max_concurrent_builds={max_concurrent_builds} (cpu={} mem_gb={} disk_gb={})",
            resources.cpu_cores, resources.memory_gb, resources.disk_total_gb
        );
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!(
            "coord returned {status} for POST /coord/devices/{device_id}/budget: {body}"
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

    fn sample_resources() -> Resources {
        Resources {
            cpu_cores: 32,
            memory_gb: 125,
            disk_total_gb: 5587,
        }
    }

    /// Non-zero on purpose: a `0` here could not tell pass-through apart
    /// from a hardcoded zero, which is the exact bug class this module was
    /// changed to remove.
    const SAMPLE_DISK_RESERVED_GB: u64 = 77;

    fn serialized() -> serde_json::Value {
        // `ROLE`, not a literal "build": pinning the literal would leave the
        // test green if the const were flipped to "agent", which is the
        // change that would make this box undispatchable.
        serde_json::to_value(build_budget_payload(
            ROLE,
            sample_resources(),
            SAMPLE_DISK_RESERVED_GB,
            "spaceship".to_string(),
        ))
        .expect("payload serializes")
    }

    /// The live defect this module was changed to fix: coord writes every
    /// budget column with a straight `SET`, so a `max_concurrent_agents: 0`
    /// here flapped the runner's real cap to zero on every publish. The
    /// supervisor does not own that column, and the ONLY way to say so on
    /// the wire is to omit the key.
    #[test]
    fn budget_payload_omits_the_agent_cap_the_supervisor_does_not_own() {
        let v = serialized();
        let obj = v.as_object().expect("payload is a JSON object");
        assert!(
            !obj.contains_key("max_concurrent_agents"),
            "max_concurrent_agents is the RUNNER's column — sending any value overwrites it: {v}"
        );
    }

    /// Coord's `BudgetPublishRequest` declares no CI-runner fields and
    /// `upsert_budget` never writes them, so sending them was a wire
    /// contract the peer does not implement.
    #[test]
    fn budget_payload_carries_no_ci_runner_fields_this_route_cannot_write() {
        let v = serialized();
        let obj = v.as_object().expect("payload is a JSON object");
        assert!(!obj.contains_key("ci_runner_labels"));
        assert!(!obj.contains_key("ci_runner_status"));
    }

    #[test]
    fn budget_payload_still_asserts_the_columns_the_supervisor_owns() {
        let v = serialized();
        // `role` is contested but deliberately still sent: coord's
        // `build_dispatcher::select_build_machine` selects `WHERE role =
        // 'build'`, so staying quiet here would settle the column on the
        // runner's 'agent' and make this box undispatchable.
        assert_eq!(v["role"], "build");
        assert_eq!(v["cpu_cores"], 32);
        assert_eq!(v["memory_gb"], 125);
        assert_eq!(v["disk_total_gb"], 5587);
        // Hardware, not process, state — both publishers observe the same
        // box, so this is not a conflict and must not be dropped.
        assert_eq!(v["disk_reserved_gb"], SAMPLE_DISK_RESERVED_GB);
        assert_eq!(v["hostname"], "spaceship");
        // min(125/4, 32/4) = 8 — the derived cap the build-pool semaphore backs.
        assert_eq!(v["max_concurrent_builds"], 8);
    }

    #[test]
    fn looks_like_jwt_accepts_a_jwt_and_rejects_every_shape_that_would_misfire() {
        assert!(looks_like_jwt("aaa.bbb.ccc"));
        assert!(looks_like_jwt("eyJhb-G_ci9.eyJzdWIi.SflKxwRJ"));

        // Segment-count failures.
        assert!(!looks_like_jwt(""));
        assert!(!looks_like_jwt("qontinui_runner_deadbeef"));
        assert!(!looks_like_jwt("aaa.bbb"));
        assert!(!looks_like_jwt("aaa.bbb.ccc.ddd"));
        assert!(!looks_like_jwt("aaa..ccc"));

        // Charset failures a count-only check would have WAVED THROUGH.
        // A JSON-wrapped credential file splits into three non-empty
        // segments and would have been attached as a garbage bearer:
        assert!(!looks_like_jwt(r#"{"token":"aaa.bbb.ccc"}"#));
        // Internal whitespace `trim()` cannot reach; `HeaderValue` rejects
        // these, which would make the publish itself fail:
        assert!(!looks_like_jwt("aaa.bbb.cc c"));
        assert!(!looks_like_jwt("aaa.bbb\r\n.ccc"));
        assert!(!looks_like_jwt("aaa.bb\tb.ccc"));
    }

    /// Half the charset check's job is to stop `HeaderValue` rejecting a
    /// bearer and taking the whole publish down with it. Prove that the
    /// CR/LF shape really would have done that, so the guard is grounded in
    /// measured behaviour rather than plausibility — and prove the space
    /// and tab shapes really would NOT have, so the doc comment does not
    /// overclaim about them.
    #[test]
    fn only_the_control_char_shape_would_have_broken_the_header() {
        fn builds(bearer: &str) -> bool {
            reqwest::Client::new()
                .post("http://127.0.0.1:1/")
                .header("Authorization", format!("Bearer {bearer}"))
                .build()
                .is_ok()
        }
        assert!(!builds("aaa.bbb\r\n.ccc"), "CR/LF must be unbuildable");
        // Legal header-value bytes: these produce a GARBAGE BEARER, not a
        // build failure — a different harm, rejected by the same check.
        assert!(builds("aaa.bbb.cc c"));
        assert!(builds("aaa.bb\tb.ccc"));
        for bad in ["aaa.bbb\r\n.ccc", "aaa.bbb.cc c", "aaa.bb\tb.ccc"] {
            assert!(!looks_like_jwt(bad), "{bad:?} must be rejected");
        }
    }

    /// A signed-out runner answers the mint 200 with an empty / opaque
    /// value; presenting that as a bearer would turn "no credential" into
    /// a 401 the caller has to decode.
    #[test]
    fn parse_minted_token_takes_a_jwt_and_refuses_every_other_answer() {
        let ok = serde_json::json!({"data": {"result": {"value": "aaa.bbb.ccc"}}});
        assert_eq!(parse_minted_token(&ok).as_deref(), Some("aaa.bbb.ccc"));

        let signed_out = serde_json::json!({"data": {"result": {"value": ""}}});
        assert_eq!(parse_minted_token(&signed_out), None);

        let opaque = serde_json::json!({"data": {"result": {"value": "qontinui_runner_x"}}});
        assert_eq!(parse_minted_token(&opaque), None);

        let no_runner_shape = serde_json::json!({"error": "not connected"});
        assert_eq!(parse_minted_token(&no_runner_shape), None);
    }

    /// A seeded rung must not be able to break the publish: an operator
    /// file/env with a stray newline or a wrong value degrades to the next
    /// rung instead of producing a header value that fails to build.
    #[test]
    fn usable_token_trims_a_jwt_and_refuses_anything_that_would_break_the_header() {
        assert_eq!(
            usable_token(Some("  aaa.bbb.ccc\n"), "test").as_deref(),
            Some("aaa.bbb.ccc")
        );
        assert_eq!(usable_token(None, "test"), None);
        assert_eq!(usable_token(Some("   "), "test"), None);
        assert_eq!(usable_token(Some("not a jwt"), "test"), None);
        // The two shapes a count-only check let through.
        assert_eq!(usable_token(Some(r#"{"token":"a.b.c"}"#), "test"), None);
        assert_eq!(usable_token(Some("aaa.bbb.cc c"), "test"), None);
    }

    fn built_request(resolved: Option<(String, &'static str)>) -> reqwest::Request {
        let client = reqwest::Client::new();
        let rb = client
            .post("http://127.0.0.1:1/coord/devices/00000000-0000-0000-0000-000000000000/budget")
            .json(&serialized());
        attach_resolved_bearer(rb, resolved)
            .build()
            .expect("request builds")
    }

    #[test]
    fn device_auth_attaches_the_bearer_when_a_credential_resolves() {
        let req = built_request(Some(("aaa.bbb.ccc".to_string(), "test source")));
        assert_eq!(
            req.headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer aaa.bbb.ccc")
        );
    }

    /// Degrade, never fail: coord still accepts anonymous budget publishes,
    /// so a supervisor with no reachable credential must keep publishing
    /// rather than drop off the fleet.
    /// Degrade, never fail: coord still accepts anonymous budget publishes,
    /// so a supervisor with no reachable credential must keep publishing
    /// rather than drop off the fleet.
    #[test]
    fn device_auth_still_produces_a_well_formed_request_when_no_credential_resolves() {
        let req = built_request(None);
        assert!(req.headers().get("Authorization").is_none());
        assert_eq!(req.method(), reqwest::Method::POST);
        assert!(req.url().path().ends_with("/budget"));
        assert!(req.body().is_some(), "the payload survives the no-auth arm");
    }

    /// Pin the no-leak property rather than leaving it in prose — and pin
    /// the one place it does NOT hold, so the boundary is measured.
    ///
    /// The bearer never reaches the request BODY, so nothing that logs or
    /// echoes the payload can carry it. It **does** appear in
    /// `reqwest::Request`'s `Debug`, because `http::HeaderMap` renders
    /// header values verbatim and does not redact `Authorization`. That is
    /// asserted here deliberately: it is the reason this module must never
    /// `{:?}` a request or a builder, and a test that claimed the opposite
    /// would license exactly the log line that leaks the credential.
    #[test]
    fn the_bearer_never_reaches_the_body_but_debug_would_expose_it() {
        let token = "notatoken.notatoken.notatoken";
        let req = built_request(Some((token.to_string(), "test source")));

        let body = std::str::from_utf8(
            req.body()
                .and_then(|b| b.as_bytes())
                .expect("json body is in memory"),
        )
        .expect("payload is utf-8");
        assert!(
            !body.contains(token),
            "the token must never reach the request body"
        );

        assert!(
            format!("{req:?}").contains(token),
            "if HeaderMap ever starts redacting Authorization this test should be \
             re-examined — until then, NEVER {{:?}} a request in this module"
        );
    }
}
