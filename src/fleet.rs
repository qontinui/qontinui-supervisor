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
use tracing::{debug, info, warn};

use crate::diagnostics::ServingEvent;

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
    // 127.0.0.1, never `localhost`: Windows resolves `localhost` to ::1
    // first and the runner binds the IPv4 loopback only, so the name form
    // pays a doomed IPv6 connect before the socket that answers.
    let base = format!("http://127.0.0.1:{}", crate::config::RUNNER_API_PORT);
    mint_device_jwt_at(&base).await
}

/// Path of the runner's in-process command door for the device-token probe.
const MINT_INVOKE_PATH: &str = "/ui-bridge/invoke/get_coord_device_token";

/// Path of the WebView evaluation door — the pre-invoke fallback.
const MINT_EVALUATE_PATH: &str = "/ui-bridge/control/page/evaluate";

/// [`mint_device_jwt_from_runner`] against an explicit runner base — the
/// seam the tests drive against an in-process fake runner.
///
/// Two doors, in order:
///
/// 1. `POST {base}/ui-bridge/invoke/get_coord_device_token` with `{}` — the
///    runner's in-process command door, answering
///    `{"success":true,"data":"<jwt>"}`. It needs no WebView, so it also
///    answers on a HEADLESS runner, where the evaluate door cannot. It is
///    the door the fleet's cache renderers already prefer.
/// 2. `POST {base}/ui-bridge/control/page/evaluate` — today's WebView
///    evaluation of the same command, used **only** when door 1 answers
///    HTTP 400 or 404, i.e. a runner build that has no invoke entry for it.
///
/// Every other door-1 outcome is final. In particular `data: null` means
/// the runner is UNPAIRED: the evaluate door would ask the same store the
/// same question, so falling through would only spend a second round trip
/// on a known answer.
async fn mint_device_jwt_at(base: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(MINT_TIMEOUT)
        // The mint targets the loopback. reqwest honours `HTTP_PROXY` /
        // `ALL_PROXY` but only exempts hosts named in `NO_PROXY`, so a box
        // with a proxy in the environment would route this at the proxy and
        // fail it. A loopback call must never be proxied.
        .no_proxy()
        .build()
        .ok()?;
    let base = base.trim_end_matches('/');

    let resp = client
        .post(format!("{base}{MINT_INVOKE_PATH}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .ok()?;
    let status = resp.status();
    if status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::NOT_FOUND {
        debug!(
            "fleet: runner answered the invoke mint door {status} (build without the invoke \
             entry) — falling back to page/evaluate"
        );
        return mint_via_page_evaluate(&client, base).await;
    }
    if !status.is_success() {
        return None;
    }
    let value: serde_json::Value = resp.json().await.ok()?;
    parse_invoked_token(&value, chrono::Utc::now().timestamp())
}

/// The WebView fallback: evaluate `get_coord_device_token` in the runner's
/// page. Only reached when the invoke door says it does not exist.
async fn mint_via_page_evaluate(client: &reqwest::Client, base: &str) -> Option<String> {
    // `PageEvaluateRequest` is `rename_all = "camelCase"` — `await_promise`
    // would be silently dropped (no `deny_unknown_fields`) and default to
    // false. `timeoutMs` bounds the runner-side evaluation so giving up at
    // MINT_TIMEOUT does not strand an orphaned 10s evaluation on the runner.
    let body = serde_json::json!({
        "expression": "window.__TAURI__.core.invoke(\"get_coord_device_token\")",
        "awaitPromise": true,
        "timeoutMs": MINT_TIMEOUT.as_millis() as u64,
    });
    let resp = client
        .post(format!("{base}{MINT_EVALUATE_PATH}"))
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let value: serde_json::Value = resp.json().await.ok()?;
    parse_minted_token(&value, chrono::Utc::now().timestamp())
}

/// Pull the minted token out of an invoke-door response body
/// (`{"success":true,"data":"<token>"}`), returning it only when it is
/// shaped like a JWT and not expired as of `now`. `data: null` is the
/// runner's "unpaired" answer and
/// yields `None` quietly; `success: false` (the command rejected — e.g. the
/// store is unreadable) yields `None` too.
fn parse_invoked_token(body: &serde_json::Value, now: i64) -> Option<String> {
    if body.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
        warn!(
            "fleet: the runner rejected the get_coord_device_token invoke (success=false) — \
             no minted device JWT."
        );
        return None;
    }
    let data = body.get("data")?;
    if data.is_null() {
        debug!("fleet: the runner answered get_coord_device_token with null (unpaired)");
        return None;
    }
    let minted = data.as_str()?.trim();
    if looks_like_jwt(minted) {
        // The runner's no-tenant arm does NOT check `exp` ("validate expiry
        // before use and treat an expired token as no credential" —
        // qontinui-runner `ui_bridge_invoke.rs`), so a minted token gets the
        // same expiry gate as a seeded one.
        return usable_token_at(Some(minted), SOURCE_RUNNER_MINT, now);
    }
    if !minted.is_empty() {
        warn!(
            "fleet: the runner answered the token mint with a non-JWT value (signed out?) — \
             ignoring it."
        );
    }
    None
}

/// Pull the minted token out of a UI-Bridge `page/evaluate` response body
/// (`{data: {result: {value: "<token>"}}}`), returning it only when it is
/// shaped like a JWT and not expired as of `now`.
fn parse_minted_token(body: &serde_json::Value, now: i64) -> Option<String> {
    let minted = body.pointer("/data/result/value")?.as_str()?.trim();
    if looks_like_jwt(minted) {
        // The runner's no-tenant arm does NOT check `exp` ("validate expiry
        // before use and treat an expired token as no credential" —
        // qontinui-runner `ui_bridge_invoke.rs`), so a minted token gets the
        // same expiry gate as a seeded one.
        return usable_token_at(Some(minted), SOURCE_RUNNER_MINT, now);
    }
    if !minted.is_empty() {
        warn!(
            "fleet: the runner answered the token mint with a non-JWT value (signed out?) — \
             ignoring it."
        );
    }
    None
}

/// Source name reported when the bearer came from `~/.qontinui/coord-device-jwt`.
pub(crate) const SOURCE_FILE: &str = "~/.qontinui/coord-device-jwt";

/// Source name reported when the bearer was minted by the local runner.
pub(crate) const SOURCE_RUNNER_MINT: &str = "runner UI Bridge mint";

/// Resolve a coord device JWT, returning it with the name of the source
/// that produced it (for logging — the token itself is never logged).
///
/// Cascade, most-explicit first, falling through on **validity, not
/// presence** (an expired seeded token is skipped — see [`usable_token`]):
/// 1. `$COORD_DEVICE_JWT`;
/// 2. `~/.qontinui/coord-device-jwt`;
/// 3. a mint from the local runner's UI Bridge.
///
/// Rungs 1 and 2 are the operator-seedable doors — they are what lets a
/// **build-only box with no runner** authenticate at all, since rung 3
/// structurally cannot answer there. On a workstation running a signed-in
/// primary runner, rung 3 is the one that resolves.
///
/// Private on purpose: every caller goes through [`resolve_device_bearer_for`],
/// which applies the transport guard first, so no publish can put a device
/// JWT on a cleartext non-loopback wire.
async fn resolve_device_bearer() -> Option<(String, &'static str)> {
    let env_token = std::env::var(DEVICE_JWT_ENV).ok();
    let file_token = match device_jwt_file_path() {
        Some(p) => tokio::fs::read_to_string(p).await.ok(),
        None => None,
    };
    if let Some(seeded) = select_seeded_bearer(
        env_token.as_deref(),
        file_token.as_deref(),
        chrono::Utc::now().timestamp(),
    ) {
        return Some(seeded);
    }
    mint_device_jwt_from_runner()
        .await
        .map(|token| (token, SOURCE_RUNNER_MINT))
}

/// A resolved device bearer: the token (if any) and the name of its source,
/// or — when there is no token — why none is sent. The label travels with
/// the absence so every log and refusal message can name the real cause.
pub(crate) type ResolvedBearer = (Option<String>, &'static str);

/// Source label reported when no credential resolved at all.
pub(crate) const SOURCE_NONE: &str = "none (no usable device JWT resolved)";

/// Source label reported when the transport guard withheld the credential.
pub(crate) const SOURCE_WITHHELD: &str =
    "none (withheld: coord base is neither https nor loopback)";

/// The device bearer to send to `base`, or `None`, with the name of the
/// source it came from — or why none was sent — for the caller's log and
/// refusal message.
///
/// [`coord_http_base`] maps `ws://` to plain `http://`, so a profile pointing
/// at a non-loopback `ws://` host would put a device JWT on the wire in
/// cleartext. The guard runs BEFORE the cascade: a withheld base consults no
/// env var, file or runner mint. Every outbound device-authed POST in this
/// crate (budget publish, resource sample, serving-watchdog notify) resolves
/// its bearer here, so the guard cannot be forgotten by one of them.
pub(crate) async fn resolve_device_bearer_for(base: &str) -> ResolvedBearer {
    if !transport_may_carry_bearer(base) {
        return (None, SOURCE_WITHHELD);
    }
    match resolve_device_bearer().await {
        Some((token, source)) => (Some(token), source),
        None => (None, SOURCE_NONE),
    }
}

/// The transport half of [`resolve_device_bearer_for`]: `true` over
/// `https://` or to a loopback host.
pub(crate) fn transport_may_carry_bearer(base: &str) -> bool {
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

/// The suffix a device-authed POST appends to its non-2xx error so the one
/// WARN says what to fix: 401/403 → the credential was refused, naming which
/// source was sent; 404 → this coord does not serve the route; anything else
/// → nothing (the status and coord's body already say it). Takes the source
/// NAME, never the token.
pub(crate) fn refusal_hint(status: reqwest::StatusCode, source: &str) -> String {
    match status.as_u16() {
        401 | 403 => format!(" (credential refused — sent: {source})"),
        404 => " (route not served by this coord)".to_string(),
        _ => String::new(),
    }
}

/// The seeded half of [`resolve_device_bearer`], with the clock and both
/// raw values passed in, so the env-over-file order and the expiry
/// fall-through are assertable without mutating process-global env or the
/// operator's credential file.
fn select_seeded_bearer(
    env_token: Option<&str>,
    file_token: Option<&str>,
    now: i64,
) -> Option<(String, &'static str)> {
    if let Some(token) = usable_token_at(env_token, DEVICE_JWT_ENV, now) {
        return Some((token, DEVICE_JWT_ENV));
    }
    usable_token_at(file_token, SOURCE_FILE, now).map(|token| (token, SOURCE_FILE))
}

/// A seeded token whose `exp` falls within this many seconds of now is
/// treated as already expired: it would lapse in flight, or within the
/// publish's own retry horizon.
const EXPIRY_SKEW_SECS: i64 = 60;

/// Trim a seeded token and accept it only if it is shaped like a JWT and
/// not expired, as of the current clock. See [`usable_token_at`].
#[cfg(test)]
fn usable_token(raw: Option<&str>, source: &str) -> Option<String> {
    usable_token_at(raw, source, chrono::Utc::now().timestamp())
}

/// Trim a seeded token and accept it only if it is shaped like a JWT and
/// its `exp` is more than [`EXPIRY_SKEW_SECS`] after `now`.
///
/// The seeded rungs are shape-checked for the same reason the mint is, plus
/// one of their own: an operator-authored file or env var can carry a
/// trailing newline or an outright wrong value, and a header value built
/// from that makes `RequestBuilder::build()` fail — turning "bad
/// credential" into "the publish stopped happening". Degrading to the next
/// rung keeps the publish alive and says why.
///
/// The **expiry** check exists because both seeded rungs are static while
/// device JWTs live ~4 h, and nothing on a Linux box refreshes the file on a
/// schedule. Selecting on presence sent an hours-dead token, coord refused
/// it 403, and the next rung — a live runner mint — was never tried. The
/// payload is decoded WITHOUT a signature check (coord verifies); only
/// `exp` is read. A token with no readable `exp` stays accepted: it is
/// shape-valid and coord decides.
///
/// Logs carry the source NAME only, never the token.
fn usable_token_at(raw: Option<&str>, source: &str, now: i64) -> Option<String> {
    let token = raw?.trim();
    if token.is_empty() {
        return None;
    }
    if !looks_like_jwt(token) {
        warn!("fleet: device bearer: {source} holds a non-JWT value — ignoring it.");
        return None;
    }
    if let Some(exp) = jwt_exp(token) {
        if exp <= now.saturating_add(EXPIRY_SKEW_SECS) {
            note_expired_source(source, &expired_note(source, exp, now));
            return None;
        }
    }
    Some(token.to_string())
}

/// The log text for a skipped (expired or about-to-expire) token. Takes the
/// source NAME and the `exp` claim, never the token. Saturating arithmetic:
/// `exp` comes from an unverified payload and may be `i64::MIN` (a
/// `-1e300` saturates to it), and a debug-build overflow panic here would
/// kill the footprint task that calls the publish.
fn expired_note(source: &str, exp: i64, now: i64) -> String {
    let when = if exp <= now {
        format!("expired {}s ago", now.saturating_sub(exp))
    } else {
        format!(
            "expires in {}s, inside the {EXPIRY_SKEW_SECS}s skew",
            exp.saturating_sub(now)
        )
    };
    format!(
        "fleet: device bearer: {source} holds an expired device JWT (exp {exp}, {when}) — \
         skipping it for the next rung."
    )
}

/// Sources that have already had their "expired" WARN this process.
static EXPIRED_WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Log a skipped expired token: WARN the first time per SOURCE per process,
/// DEBUG after. On a Linux box an expired `~/.qontinui/coord-device-jwt` is
/// the normal state (nothing refreshes it), and the resolver runs on every
/// footprint publish — an unlatched WARN would repeat every cycle even when
/// the runner mint then succeeds.
fn note_expired_source(source: &str, note: &str) {
    let first = match EXPIRED_WARNED.lock() {
        Ok(mut seen) if !seen.iter().any(|s| s == source) => {
            seen.push(source.to_string());
            true
        }
        Ok(_) => false,
        // A poisoned latch must not silence the signal.
        Err(_) => true,
    };
    if first {
        warn!("{note} Further occurrences for this source log at DEBUG.");
    } else {
        debug!("{note}");
    }
}

/// Read the `exp` claim of a JWS-compact token WITHOUT verifying it.
///
/// `None` when the payload segment is not base64url, not JSON, or carries
/// no numeric `exp` — all of which the caller treats as "no readable exp".
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let exp = claims.get("exp")?;
    exp.as_i64().or_else(|| exp.as_f64().map(|f| f as i64))
}

/// Minimal unpadded base64url decoder (RFC 4648 §5), padding tolerated.
///
/// Hand-rolled rather than a `base64` dependency: it is only ever used to
/// read one claim out of a JWT payload, and this crate declares no base64
/// crate of its own. `None` on any byte outside the alphabet or an
/// impossible length (a lone trailing sextet).
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let input = input.trim_end_matches('=').as_bytes();
    if input.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for chunk in input.chunks(4) {
        let mut acc: u32 = 0;
        for &b in chunk {
            acc = (acc << 6) | sextet(b)?;
        }
        // Left-align the chunk's bits into 24, then emit its whole bytes.
        acc <<= 6 * (4 - chunk.len() as u32);
        let bytes = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

/// Attach `Authorization: Bearer <device JWT>` when one was resolved by
/// [`resolve_device_bearer_for`].
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
/// The publishes' own error strings format the URL, the `reqwest::Error`,
/// coord's response body and the source NAME — none of which carries
/// headers.
///
/// Resolution is the caller's (so the request path stays assertable without
/// a network, a process-global env mutation, or a running runner).
fn attach_resolved_bearer(
    rb: reqwest::RequestBuilder,
    resolved: ResolvedBearer,
) -> reqwest::RequestBuilder {
    match resolved {
        (Some(token), source) => {
            info!("fleet: attaching coord device JWT (source: {source})");
            rb.header("Authorization", format!("Bearer {token}"))
        }
        (None, reason) => {
            MISSING_BEARER_WARNED.call_once(|| {
                if reason == SOURCE_WITHHELD {
                    warn!(
                        "fleet: coord device JWT {reason} — publishing ANONYMOUSLY. Point the \
                         active profile's coord_url at wss:// / https:// (or a loopback host) \
                         to authenticate."
                    );
                } else {
                    warn!(
                        "fleet: no usable coord device JWT (no unexpired {DEVICE_JWT_ENV}, no \
                         unexpired ~/.qontinui/coord-device-jwt, and the local runner did not \
                         mint one) — publishing ANONYMOUSLY. Seed one of the two files/vars to \
                         authenticate on a box with no runner."
                    );
                }
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
    let bearer = resolve_device_bearer_for(&base).await;
    let bearer_source = bearer.1;
    let resp = attach_resolved_bearer(client.post(&url).json(&payload), bearer)
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
        let hint = refusal_hint(status, bearer_source);
        Err(format!(
            "coord returned {status} for POST /coord/devices/{device_id}/budget: {body}{hint}"
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

// ---------------------------------------------------------------------------
// Serving-watchdog alert — plan `2026-09-03-runner-zombie-serving-watchdog`,
// Phase 5. The supervisor tells coord, durably and without any agent session
// in the loop, that a runner stopped SERVING while its process stayed alive.
//
// Why the alert originates HERE and not in coord: coord cannot infer a wedge
// from the runner's silence, because the runner is not silent towards coord.
// A wedged runner keeps its OUTBOUND work going — heartbeats, publishes, the
// device-budget republisher — while its local `:9876` door stops answering.
// Every local coord transport roots on that door (coord-mcp, the loopback
// proxy, the REST write forwarder, the UI-Bridge device-JWT mint), so the
// sessions that would otherwise report it are the first thing the wedge takes
// out. Three occurrences, 12 h to 5 days each, were surfaced only by a
// session's closeout. The supervisor is the one process on the box that
// watches the door from outside AND holds a device-authed coord client
// (`resolve_device_bearer_for`), so it is the only honest origin.
//
// Why `POST /coord/agent-notifications` with `action: "other"`: it is the
// only door on coord `origin/main` that lands in the operator-pull surface
// (the dashboard nav badge's `unread_count`) and it admits a DEVICE JWT — the
// mount is behind `require_jwt`, which checks signature and revocation only,
// and the handler resolves the tenant from the `device_id` claim. Its body
// (`AgentNotificationBody`) is flat and `deny_unknown_fields`: `action` is a
// closed set {publish, force_push, delete, rotate, other}, `artifact` is
// required, `reversible` ∈ {no, roll-forward, restore} is optional, `checks`
// is free text, and `repo` / `pr_number` are for PR-shaped events only. The
// kind is hard-coded server-side to `agent_took_irreversible_action`, which
// is exact for `RestartTaken` and stretched for the rest — so `checks` opens
// with `serving_watchdog:<event>` and carries the true class until coord
// grows a dedicated kind (a Follow-up in the plan).
//
// Why the fallback is `POST /coord/agent-findings`: if the notifications door
// ever answers 401/403 at runtime (a tenant-scoping change, a revoked device)
// the event must still land SOMEWHERE durable. A `status` finding on topic
// `operator-notify` is a peer-session feed rather than an operator badge, but
// it is device-authed by the same bearer and it is queryable. It is a
// degraded arm, taken only on those two statuses and logged as such.
//
// Why callers must spawn this fire-and-forget: a coord outage must never
// delay a restart by a single tick. Phase 4's `maybe_serving_restart` spawns
// `notify_serving_watchdog` as a detached task and continues; the
// `DiagnosticEventKind::ServingWatchdog` ring event it emits in the same
// breath is the durable record on this box whether or not this call ever
// completes. Nothing here retries: on 429 the local record already exists,
// and a retry loop inside a detached task is exactly the unbounded work the
// escalation cadence (one alert per re-escalation interval) is meant to
// bound.
// ---------------------------------------------------------------------------

/// Coord's `MAX_POSTED_FIELD_CHARS` (`notifications.rs`, `findings.rs`): every
/// posted string field is capped at this many CHARACTERS server-side, and an
/// over-long one is a 400, not a silent trim. Enforced here on the client so a
/// large census payload (dozens of PIDs with exe paths) can never turn the
/// alert into a rejected request.
const MAX_POSTED_FIELD_CHARS: usize = 2000;

/// Appended when a field is cut, and counted inside the cap.
const TRUNCATION_MARKER: char = '…';

/// Whole-request budget for one alert POST — the `publish_budget` idiom. The
/// caller has already detached us, so this bounds a leaked task, not a
/// restart.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

const AGENT_NOTIFICATIONS_PATH: &str = "/coord/agent-notifications";
const AGENT_FINDINGS_PATH: &str = "/coord/agent-findings";

/// One serving-watchdog decision, in the shape the alert channel needs.
///
/// Mirrors the field set of `DiagnosticEventKind::ServingWatchdog` on purpose
/// — Phase 4 builds both from the same locals — but is its own struct so the
/// alert body can be built from a borrow without cloning the ring event.
/// Carries no credential of any kind, so its `Debug` is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingWatchdogNotice {
    pub runner_id: String,
    pub event: ServingEvent,
    pub pid: Option<u32>,
    pub port: u16,
    /// `now - UnresponsiveSince(at)` at decision time.
    pub silent_for_secs: u64,
    /// The census / readiness verdict that backed the decision, verbatim.
    pub census_json: Option<serde_json::Value>,
    /// The error string for `RestartFailed`, the disarm reason for
    /// `Disarmed`, free text otherwise.
    pub detail: Option<String>,
}

/// How the alert landed. Every variant is a DELIVERY — the `Err` arm of
/// [`notify_serving_watchdog`] is the only "it did not land" answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// `POST /coord/agent-notifications` answered 2xx. `notification_id` is
    /// whatever the response body carried under `notification_id` (or `id`),
    /// `None` when coord answered with no parsable id.
    Notified { notification_id: Option<String> },
    /// The notifications door refused the bearer (401/403) and the event
    /// was recorded as a `status` finding on topic `operator-notify` instead.
    FallbackFinding { finding_id: Option<String> },
    /// Coord's per-tenant notification window is exhausted (429). Nothing was
    /// retried and nothing else was posted: the diagnostics ring on this box
    /// already holds the event, and the next re-escalation interval will try
    /// again on its own cadence.
    RateLimited { retry_after_secs: Option<u64> },
}

/// Cut a field to coord's per-field character cap, marking the cut.
///
/// Character-based, not byte-based: the census carries exe paths and the
/// `detail` carries arbitrary error text, either of which can be non-ASCII,
/// and a byte slice would land inside a code point.
fn truncate_field(raw: &str) -> String {
    if raw.chars().count() <= MAX_POSTED_FIELD_CHARS {
        return raw.to_string();
    }
    let mut cut: String = raw.chars().take(MAX_POSTED_FIELD_CHARS - 1).collect();
    cut.push(TRUNCATION_MARKER);
    cut
}

/// The one-line `artifact` an operator reads on the dashboard badge.
fn artifact_line(notice: &ServingWatchdogNotice) -> String {
    let pid = notice
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    truncate_field(&format!(
        "runner {} pid {pid} port {} — {} after {}s silent",
        notice.runner_id, notice.port, notice.event, notice.silent_for_secs
    ))
}

/// The `checks` field: the TRUE event class first (coord's kind is hard-coded
/// to `agent_took_irreversible_action` for every class), then the detail,
/// then the census/verdict JSON — so a truncation, if one happens, eats the
/// tail of the census and never the class.
fn checks_string(notice: &ServingWatchdogNotice) -> String {
    let mut checks = format!("serving_watchdog:{} ", notice.event);
    match notice.detail.as_deref() {
        Some(detail) if !detail.is_empty() => {
            checks.push_str("detail=");
            checks.push_str(detail);
            checks.push(' ');
        }
        _ => {}
    }
    match &notice.census_json {
        Some(census) => {
            checks.push_str("census=");
            checks.push_str(&census.to_string());
        }
        None => checks.push_str("census=none"),
    }
    truncate_field(&checks)
}

/// Build EXACTLY the flat body coord's `AgentNotificationBody` accepts. Pure
/// — no IO — so the golden test pins the wire shape against the real
/// construction path.
///
/// The key set is `{action, artifact, checks}` plus `reversible` for
/// `RestartTaken` ONLY. The door is `deny_unknown_fields`, so one extra key is
/// a 400 that surfaces nowhere an operator looks; and `reversible` is omitted
/// rather than sent for the non-restart classes because coord renders an
/// absent value as "unrecorded" — the honest answer for a threshold or a
/// refusal, which reversed nothing — whereas any present value would be a
/// claim. `repo` / `pr_number` are never emitted (a runner is not a PR), and
/// `kind` / `actor` are server-assigned and would be rejected.
pub fn build_agent_notification_body(notice: &ServingWatchdogNotice) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert("action".to_string(), serde_json::Value::from("other"));
    body.insert(
        "artifact".to_string(),
        serde_json::Value::from(artifact_line(notice)),
    );
    if notice.event == ServingEvent::RestartTaken {
        body.insert("reversible".to_string(), serde_json::Value::from("no"));
    }
    body.insert(
        "checks".to_string(),
        serde_json::Value::from(checks_string(notice)),
    );
    serde_json::Value::Object(body)
}

/// The degraded-arm body for `POST /coord/agent-findings`. `title` and `body`
/// are the same two strings the notification would have carried, so the two
/// records read alike; `resource_keys` names the runner and — when
/// `~/.qontinui/machine.json` resolves one — the device, so a peer session
/// can find the finding by either.
fn build_fallback_finding_body(
    notice: &ServingWatchdogNotice,
    device_id: Option<&str>,
) -> serde_json::Value {
    let mut resource_keys = vec![format!("runner:{}", notice.runner_id)];
    if let Some(device_id) = device_id.filter(|d| !d.is_empty()) {
        resource_keys.push(format!("device:{device_id}"));
    }
    serde_json::json!({
        "kind": "status",
        "topic": "operator-notify",
        "title": artifact_line(notice),
        "body": checks_string(notice),
        "resource_keys": resource_keys,
    })
}

/// Alert coord that the serving watchdog decided something about a runner.
///
/// Resolves the coord base from the active profile (`coord_http_base`) and
/// the device bearer through the transport-guarded cascade
/// ([`resolve_device_bearer_for`] — withheld on a cleartext non-loopback
/// base), then posts. See the block
/// comment above for why this exists, why it is `action: other`, and why the
/// caller MUST spawn it rather than await it on the restart path:
///
/// ```ignore
/// tokio::spawn(async move {
///     match fleet::notify_serving_watchdog(&notice).await {
///         Ok(outcome) => info!(...),
///         Err(e) => warn!("serving watchdog: coord alert not delivered: {e}"),
///     }
/// });
/// ```
///
/// `Err` means the event did NOT land on coord by either door; the
/// diagnostics ring event the caller emitted alongside is the record then.
/// Never logs, formats or returns the `Authorization` value on any path.
// Wired by Phase 4's `maybe_serving_restart` (`process::manager`), which
// lands in a separate commit on the same branch; until it does, the lib
// target reaches this through `lib.rs` and the bin target reports it dead.
#[allow(dead_code)]
pub async fn notify_serving_watchdog(
    notice: &ServingWatchdogNotice,
) -> Result<NotifyOutcome, String> {
    let base = coord_http_base().ok_or_else(|| {
        "no coord base: ~/.qontinui/profiles.json missing or the active profile has no coord_url"
            .to_string()
    })?;
    notify_serving_watchdog_at(&base, notice).await
}

/// [`notify_serving_watchdog`] against an explicit coord base — the seam the
/// tests drive against an in-process server. Resolves the device bearer and
/// the device id ONCE and hands both to the request path, so the fallback
/// arm cannot mint a second credential.
#[allow(dead_code)] // reached through `notify_serving_watchdog`; see its note
pub async fn notify_serving_watchdog_at(
    base: &str,
    notice: &ServingWatchdogNotice,
) -> Result<NotifyOutcome, String> {
    let bearer = resolve_device_bearer_for(base).await;
    let device_id = load_machine_file().and_then(|m| m.device_id().map(str::to_string));
    notify_serving_watchdog_with(base, notice, bearer, device_id.as_deref()).await
}

/// The request path with credential resolution factored out, so every arm
/// (2xx, 401/403 → fallback, 429, other, network) is assertable against a
/// mock without touching the environment, the credential file, or the live
/// runner's mint door.
async fn notify_serving_watchdog_with(
    base: &str,
    notice: &ServingWatchdogNotice,
    bearer: ResolvedBearer,
    device_id: Option<&str>,
) -> Result<NotifyOutcome, String> {
    let base = base.trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(NOTIFY_TIMEOUT)
        .build()
        .map_err(|e| format!("reqwest builder: {e}"))?;

    let url = format!("{base}{AGENT_NOTIFICATIONS_PATH}");
    let body = build_agent_notification_body(notice);
    // `reqwest::Error`'s Display carries the URL and the error kind only —
    // never headers — so this string is safe to log and to return.
    let resp = attach_resolved_bearer(client.post(&url).json(&body), bearer.clone())
        .send()
        .await
        .map_err(|e| format!("POST {url}: {e}"))?;

    let status = resp.status();
    if status.is_success() {
        let answer: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        let notification_id = id_from_body(&answer, "notification_id");
        info!(
            "fleet::notify_serving_watchdog: coord notified ({status}) runner={} event={} \
             notification_id={}",
            notice.runner_id,
            notice.event,
            notification_id.as_deref().unwrap_or("-")
        );
        return Ok(NotifyOutcome::Notified { notification_id });
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after_secs = retry_after_secs(&resp);
        warn!(
            "fleet::notify_serving_watchdog: coord rate-limited the alert (429, Retry-After={}) \
             runner={} event={} — not retrying; the diagnostics ring holds the event",
            retry_after_secs
                .map(|s| s.to_string())
                .unwrap_or_else(|| "absent".to_string()),
            notice.runner_id,
            notice.event
        );
        return Ok(NotifyOutcome::RateLimited { retry_after_secs });
    }

    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        warn!(
            "fleet::notify_serving_watchdog: coord refused the device bearer at \
             {AGENT_NOTIFICATIONS_PATH} ({status}, sent: {}) runner={} event={} — falling back \
             to a `status` finding on topic operator-notify",
            bearer.1, notice.runner_id, notice.event
        );
        return post_fallback_finding(&client, base, notice, bearer, device_id).await;
    }

    let text = resp.text().await.unwrap_or_default();
    Err(format!(
        "coord returned {status} for POST {AGENT_NOTIFICATIONS_PATH}: {}",
        truncate_field(&text)
    ))
}

/// The 401/403 degraded arm: the same event as a device-authed finding.
async fn post_fallback_finding(
    client: &reqwest::Client,
    base: &str,
    notice: &ServingWatchdogNotice,
    bearer: ResolvedBearer,
    device_id: Option<&str>,
) -> Result<NotifyOutcome, String> {
    let url = format!("{base}{AGENT_FINDINGS_PATH}");
    let body = build_fallback_finding_body(notice, device_id);
    let resp = attach_resolved_bearer(client.post(&url).json(&body), bearer)
        .send()
        .await
        .map_err(|e| format!("fallback POST {url}: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        let answer: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        let finding_id = id_from_body(&answer, "finding_id");
        info!(
            "fleet::notify_serving_watchdog: recorded as a finding ({status}) runner={} event={} \
             finding_id={}",
            notice.runner_id,
            notice.event,
            finding_id.as_deref().unwrap_or("-")
        );
        return Ok(NotifyOutcome::FallbackFinding { finding_id });
    }
    let text = resp.text().await.unwrap_or_default();
    Err(format!(
        "coord refused the notification AND the fallback finding: {status} for POST \
         {AGENT_FINDINGS_PATH}: {}",
        truncate_field(&text)
    ))
}

/// `Retry-After` as delay-seconds. An HTTP-date form is legal but coord's
/// typed 429 sends seconds; anything else reads as "absent".
fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Pull an id out of a coord answer: the named key first, then a bare `id`.
/// Accepts a string or an integer, since the two doors are not uniform.
fn id_from_body(body: &serde_json::Value, key: &str) -> Option<String> {
    let value = body.get(key).or_else(|| body.get("id"))?;
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
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
        assert_eq!(parse_minted_token(&ok, NOW).as_deref(), Some("aaa.bbb.ccc"));

        let signed_out = serde_json::json!({"data": {"result": {"value": ""}}});
        assert_eq!(parse_minted_token(&signed_out, NOW), None);

        let opaque = serde_json::json!({"data": {"result": {"value": "qontinui_runner_x"}}});
        assert_eq!(parse_minted_token(&opaque, NOW), None);

        let no_runner_shape = serde_json::json!({"error": "not connected"});
        assert_eq!(parse_minted_token(&no_runner_shape, NOW), None);
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

    fn built_request(resolved: ResolvedBearer) -> reqwest::Request {
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
        let req = built_request((Some("aaa.bbb.ccc".to_string()), "test source"));
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
    #[test]
    fn device_auth_still_produces_a_well_formed_request_when_no_credential_resolves() {
        let req = built_request((None, SOURCE_NONE));
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
        let req = built_request((Some(token.to_string()), "test source"));

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
    // -----------------------------------------------------------------------
    // Serving-watchdog alert (Phase 5).
    // -----------------------------------------------------------------------

    use axum::extract::State;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    const ALL_EVENTS: [ServingEvent; 6] = [
        ServingEvent::Threshold,
        ServingEvent::NeverAnsweredSinceAdoption,
        ServingEvent::RestartTaken,
        ServingEvent::RestartRefusedSessionsLive,
        ServingEvent::RestartFailed,
        ServingEvent::Disarmed,
    ];

    fn sample_notice(event: ServingEvent) -> ServingWatchdogNotice {
        ServingWatchdogNotice {
            runner_id: "primary".to_string(),
            event,
            pid: Some(48544),
            port: 9876,
            silent_for_secs: 301,
            census_json: Some(serde_json::json!({
                "source": "supervisor_subtree_census",
                "root_pid": 48544,
                "walked": 3,
                "live_claude": [],
                "verdict": "idle"
            })),
            detail: None,
        }
    }

    /// One recorded request at the mock: which door, whether a bearer came
    /// with it, and the parsed JSON body.
    #[derive(Debug, Clone)]
    struct Seen {
        path: &'static str,
        authorization: Option<String>,
        body: serde_json::Value,
    }

    #[derive(Clone)]
    struct MockState {
        notifications_status: u16,
        retry_after: Option<&'static str>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    fn record(st: &MockState, path: &'static str, headers: &HeaderMap, body: serde_json::Value) {
        let authorization = headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        st.seen.lock().unwrap().push(Seen {
            path,
            authorization,
            body,
        });
    }

    async fn mock_notifications(
        State(st): State<MockState>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        record(&st, AGENT_NOTIFICATIONS_PATH, &headers, body);
        let status = StatusCode::from_u16(st.notifications_status).unwrap();
        let mut resp = (
            status,
            Json(serde_json::json!({"notification_id": "n-1", "kind": "agent_took_irreversible_action"})),
        )
            .into_response();
        if let Some(ra) = st.retry_after {
            resp.headers_mut()
                .insert("Retry-After", HeaderValue::from_static(ra));
        }
        resp
    }

    async fn mock_findings(
        State(st): State<MockState>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        record(&st, AGENT_FINDINGS_PATH, &headers, body);
        (
            StatusCode::CREATED,
            Json(serde_json::json!({"finding_id": "f-1"})),
        )
            .into_response()
    }

    /// A coord stand-in on an ephemeral loopback port: answers the
    /// notifications door with a fixed status (plus an optional
    /// `Retry-After`) and the findings door with 201. Same shape as
    /// `restart_readiness`'s `spawn_mock` — a real HTTP round trip, and the
    /// real coord is never touched because the base URL is explicit.
    async fn spawn_coord_mock(
        notifications_status: u16,
        retry_after: Option<&'static str>,
    ) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let st = MockState {
            notifications_status,
            retry_after,
            seen: Arc::clone(&seen),
        };
        let app = Router::new()
            .route(AGENT_NOTIFICATIONS_PATH, post(mock_notifications))
            .route(AGENT_FINDINGS_PATH, post(mock_findings))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{}", addr.port()), seen)
    }

    /// A URL nothing is listening on.
    async fn dead_base() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    fn keys(v: &serde_json::Value) -> Vec<&str> {
        let mut k: Vec<&str> = v
            .as_object()
            .expect("body is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        k.sort_unstable();
        k
    }

    /// GOLDEN: coord's `AgentNotificationBody` is `deny_unknown_fields`, so
    /// one extra key is a 400 that no operator ever sees. Pin the exact key
    /// set for every event class: `{action, artifact, checks}`, plus
    /// `reversible` for `RestartTaken` ONLY — absent renders as "unrecorded"
    /// on coord, and a threshold or refusal reversed nothing. Never `repo`,
    /// `pr_number`, `kind` or `actor`.
    #[test]
    fn notification_body_carries_exactly_the_keys_coord_accepts() {
        for event in ALL_EVENTS {
            let body = build_agent_notification_body(&sample_notice(event));
            let expected: Vec<&str> = if event == ServingEvent::RestartTaken {
                vec!["action", "artifact", "checks", "reversible"]
            } else {
                vec!["action", "artifact", "checks"]
            };
            assert_eq!(keys(&body), expected, "event {event}: {body}");
            assert_eq!(body["action"], "other", "event {event}");
            if event == ServingEvent::RestartTaken {
                assert_eq!(body["reversible"], "no");
            }
            let checks = body["checks"].as_str().unwrap();
            assert!(
                checks.starts_with(&format!("serving_watchdog:{event} ")),
                "checks must open with the true class: {checks}"
            );
            assert!(checks.contains(r#""verdict":"idle""#), "{checks}");
            for forbidden in ["repo", "pr_number", "kind", "actor"] {
                assert!(body.get(forbidden).is_none(), "{forbidden} in {body}");
            }
        }

        // The artifact line, exactly.
        let body = build_agent_notification_body(&sample_notice(ServingEvent::RestartTaken));
        assert_eq!(
            body["artifact"],
            "runner primary pid 48544 port 9876 — restart_taken after 301s silent"
        );

        // An unknown PID is spelled, not omitted; a detail lands in checks
        // ahead of the census; no census is said, not left blank.
        let mut notice = sample_notice(ServingEvent::RestartFailed);
        notice.pid = None;
        notice.census_json = None;
        notice.detail = Some("port 9876 held by live runner pid 48544".to_string());
        let body = build_agent_notification_body(&notice);
        assert_eq!(
            body["artifact"],
            "runner primary pid unknown port 9876 — restart_failed after 301s silent"
        );
        assert_eq!(
            body["checks"],
            "serving_watchdog:restart_failed detail=port 9876 held by live runner pid 48544 \
             census=none"
        );
    }

    /// `checks` is cut to coord's `MAX_POSTED_FIELD_CHARS` (2000) with a
    /// marker inside the cap, counted in characters — and the cut eats the
    /// census tail, never the class prefix. A body under the cap is passed
    /// through untouched.
    #[test]
    fn notification_checks_is_truncated_at_coords_field_cap_with_a_marker() {
        let mut notice = sample_notice(ServingEvent::RestartRefusedSessionsLive);
        // Non-ASCII on purpose: a byte-based cut would land inside a code point.
        let long_exe = "é".repeat(5000);
        notice.census_json = Some(serde_json::json!({
            "verdict": "busy",
            "live_claude": [{"pid": 7, "exe": long_exe}]
        }));
        let body = build_agent_notification_body(&notice);
        let checks = body["checks"].as_str().unwrap();
        assert_eq!(checks.chars().count(), MAX_POSTED_FIELD_CHARS, "{checks}");
        assert!(checks.ends_with(TRUNCATION_MARKER), "{checks}");
        assert!(checks.starts_with("serving_watchdog:restart_refused_sessions_live "));
        assert!(
            body["artifact"].as_str().unwrap().chars().count() < MAX_POSTED_FIELD_CHARS,
            "the artifact line is short and must not be touched"
        );

        let short = build_agent_notification_body(&sample_notice(ServingEvent::Threshold));
        let checks = short["checks"].as_str().unwrap();
        assert!(checks.chars().count() < MAX_POSTED_FIELD_CHARS);
        assert!(!checks.contains(TRUNCATION_MARKER), "{checks}");

        // The helper itself, at the boundary.
        let exact: String = "x".repeat(MAX_POSTED_FIELD_CHARS);
        assert_eq!(truncate_field(&exact), exact);
        let over: String = "x".repeat(MAX_POSTED_FIELD_CHARS + 1);
        let cut = truncate_field(&over);
        assert_eq!(cut.chars().count(), MAX_POSTED_FIELD_CHARS);
        assert!(cut.ends_with(TRUNCATION_MARKER));
    }

    /// 2xx → `Notified` carrying the id coord answered with; exactly ONE
    /// request, to the notifications door, carrying the resolved bearer and
    /// the golden body.
    #[tokio::test]
    async fn notify_200_returns_notified_and_sends_the_bearer_once() {
        let (base, seen) = spawn_coord_mock(200, None).await;
        let notice = sample_notice(ServingEvent::RestartTaken);
        let outcome = notify_serving_watchdog_with(
            &base,
            &notice,
            (Some("aaa.bbb.ccc".to_string()), "test source"),
            Some("dev-1"),
        )
        .await
        .expect("delivered");
        assert_eq!(
            outcome,
            NotifyOutcome::Notified {
                notification_id: Some("n-1".to_string())
            }
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].path, AGENT_NOTIFICATIONS_PATH);
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer aaa.bbb.ccc"));
        assert_eq!(seen[0].body, build_agent_notification_body(&notice));
    }

    /// No resolvable credential → the request still goes out, anonymously
    /// (the `attach_resolved_bearer` posture): coord decides, not us.
    #[tokio::test]
    async fn notify_without_a_credential_still_posts_and_is_notified_on_200() {
        let (base, seen) = spawn_coord_mock(200, None).await;
        let notice = sample_notice(ServingEvent::Threshold);
        let outcome = notify_serving_watchdog_with(&base, &notice, (None, SOURCE_NONE), None)
            .await
            .expect("delivered");
        assert!(
            matches!(outcome, NotifyOutcome::Notified { .. }),
            "{outcome:?}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].authorization, None);
    }

    /// 429 → `RateLimited` with the parsed `Retry-After`, and NOTHING else is
    /// sent: no retry, no fallback. The ring event on this box is the record.
    #[tokio::test]
    async fn notify_429_is_rate_limited_and_never_retried() {
        let (base, seen) = spawn_coord_mock(429, Some("30")).await;
        let notice = sample_notice(ServingEvent::Threshold);
        let outcome =
            notify_serving_watchdog_with(&base, &notice, (None, SOURCE_NONE), Some("dev-1"))
                .await
                .expect("a 429 is a delivery outcome, not an error");
        assert_eq!(
            outcome,
            NotifyOutcome::RateLimited {
                retry_after_secs: Some(30)
            }
        );
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1, "no second request of any kind: {seen:?}");
            assert_eq!(seen[0].path, AGENT_NOTIFICATIONS_PATH);
        }

        // No header → `None`, still no retry.
        let (base, seen) = spawn_coord_mock(429, None).await;
        let outcome = notify_serving_watchdog_with(&base, &notice, (None, SOURCE_NONE), None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            NotifyOutcome::RateLimited {
                retry_after_secs: None
            }
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// 403 → exactly one fallback POST to the findings door, device-authed
    /// with the SAME bearer, carrying the `status` / `operator-notify` shape
    /// with the runner and device resource keys → `FallbackFinding`.
    #[tokio::test]
    async fn notify_403_falls_back_to_a_status_finding_exactly_once() {
        let (base, seen) = spawn_coord_mock(403, None).await;
        let notice = sample_notice(ServingEvent::RestartRefusedSessionsLive);
        let outcome = notify_serving_watchdog_with(
            &base,
            &notice,
            (Some("aaa.bbb.ccc".to_string()), "test source"),
            Some("dev-1"),
        )
        .await
        .expect("the fallback landed");
        assert_eq!(
            outcome,
            NotifyOutcome::FallbackFinding {
                finding_id: Some("f-1".to_string())
            }
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0].path, AGENT_NOTIFICATIONS_PATH);
        assert_eq!(seen[1].path, AGENT_FINDINGS_PATH);
        assert_eq!(seen[1].authorization.as_deref(), Some("Bearer aaa.bbb.ccc"));
        let finding = &seen[1].body;
        assert_eq!(
            keys(finding),
            vec!["body", "kind", "resource_keys", "title", "topic"]
        );
        assert_eq!(finding["kind"], "status");
        assert_eq!(finding["topic"], "operator-notify");
        assert_eq!(finding["title"], artifact_line(&notice));
        assert_eq!(finding["body"], checks_string(&notice));
        assert_eq!(
            finding["resource_keys"],
            serde_json::json!(["runner:primary", "device:dev-1"])
        );
    }

    /// 401 takes the same arm; with no device id the `device:` key is omitted
    /// rather than sent empty.
    #[tokio::test]
    async fn notify_401_falls_back_and_omits_an_unresolvable_device_key() {
        let (base, seen) = spawn_coord_mock(401, None).await;
        let notice = sample_notice(ServingEvent::Disarmed);
        let outcome = notify_serving_watchdog_with(&base, &notice, (None, SOURCE_NONE), None)
            .await
            .unwrap();
        assert!(
            matches!(outcome, NotifyOutcome::FallbackFinding { .. }),
            "{outcome:?}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[1].body["resource_keys"],
            serde_json::json!(["runner:primary"])
        );
    }

    /// Any other status is an `Err` naming it, with no fallback — a 500 is
    /// coord's fault, not the bearer's, and a finding POST would hit the same
    /// outage. And the error string must never carry the bearer: it formats
    /// the status and the response body only. This is the guard from
    /// `the_bearer_never_reaches_the_body_but_debug_would_expose_it` extended
    /// to the alert path, whose strings DO get logged.
    #[tokio::test]
    async fn notify_500_is_an_error_without_fallback_and_never_leaks_the_bearer() {
        let token = "notatoken.notatoken.notatoken";
        let (base, seen) = spawn_coord_mock(500, None).await;
        let notice = sample_notice(ServingEvent::RestartFailed);
        let err = notify_serving_watchdog_with(
            &base,
            &notice,
            (Some(token.to_string()), "test source"),
            Some("dev-1"),
        )
        .await
        .expect_err("a 500 is not a delivery");
        assert!(err.contains("500"), "{err}");
        assert!(err.contains(AGENT_NOTIFICATIONS_PATH), "{err}");
        assert!(
            !err.contains(token),
            "the error string must never carry the bearer: {err}"
        );
        assert!(!format!("{notice:?}").contains(token));
        assert_eq!(seen.lock().unwrap().len(), 1, "no fallback on a 5xx");
    }

    /// A dead coord (connection refused) is an `Err`, not a hang: the client
    /// has a 5 s budget and the caller has already detached us.
    #[tokio::test]
    async fn notify_network_error_is_an_error_naming_the_url() {
        let base = dead_base().await;
        let notice = sample_notice(ServingEvent::Threshold);
        let err = notify_serving_watchdog_with(&base, &notice, (None, SOURCE_NONE), None)
            .await
            .expect_err("nothing is listening");
        assert!(err.starts_with("POST "), "{err}");
        assert!(err.contains(AGENT_NOTIFICATIONS_PATH), "{err}");
    }

    #[test]
    fn id_from_body_reads_the_named_key_then_id_and_accepts_numbers() {
        assert_eq!(
            id_from_body(
                &serde_json::json!({"notification_id": "n-9"}),
                "notification_id"
            ),
            Some("n-9".to_string())
        );
        assert_eq!(
            id_from_body(&serde_json::json!({"id": 42}), "finding_id"),
            Some("42".to_string())
        );
        assert_eq!(
            id_from_body(&serde_json::json!({"id": ""}), "finding_id"),
            None
        );
        assert_eq!(id_from_body(&serde_json::Value::Null, "finding_id"), None);
    }

    // -----------------------------------------------------------------------
    // Device-bearer validity + the invoke-first mint.
    // -----------------------------------------------------------------------

    /// Test-only base64url encoder (unpadded), the inverse of
    /// [`base64url_decode`].
    fn b64url(bytes: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut acc: u32 = 0;
            for (i, &b) in chunk.iter().enumerate() {
                acc |= u32::from(b) << (16 - 8 * i);
            }
            for i in 0..=chunk.len() {
                out.push(A[((acc >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    /// A JWS-compact token whose payload is `claims`. Unsigned — the
    /// supervisor never verifies a signature, coord does.
    fn jwt_with(claims: serde_json::Value) -> String {
        format!(
            "{}.{}.c2ln",
            b64url(br#"{"alg":"HS256","typ":"JWT"}"#),
            b64url(claims.to_string().as_bytes())
        )
    }

    const NOW: i64 = 1_790_000_000;

    #[test]
    fn base64url_decode_round_trips_every_tail_length_and_refuses_junk() {
        let inputs: [&[u8]; 6] = [b"", b"a", b"ab", b"abc", b"abcd", b"\xff\xfe\x00?>"];
        for input in inputs {
            assert_eq!(base64url_decode(&b64url(input)).as_deref(), Some(input));
        }
        // Padding is tolerated.
        assert_eq!(base64url_decode("YQ==").as_deref(), Some(&b"a"[..]));
        // A lone trailing sextet cannot encode a byte.
        assert_eq!(base64url_decode("YWJjZ"), None);
        // Standard-alphabet bytes are not base64url.
        assert_eq!(base64url_decode("a+b/"), None);
        // Known answer exercising both url-safe symbols: '-'=62 '_'=63 '8'=60
        // → 111110 111111 111100 → 0xfb 0xff (4 pad bits dropped).
        assert_eq!(base64url_decode("-_8").as_deref(), Some(&[0xfb, 0xff][..]));
    }

    #[test]
    fn jwt_exp_reads_the_claim_and_is_none_when_unreadable() {
        assert_eq!(
            jwt_exp(&jwt_with(serde_json::json!({"exp": 123}))),
            Some(123)
        );
        assert_eq!(
            jwt_exp(&jwt_with(serde_json::json!({"exp": 123.9}))),
            Some(123)
        );
        assert_eq!(jwt_exp(&jwt_with(serde_json::json!({"sub": "x"}))), None);
        assert_eq!(jwt_exp(&jwt_with(serde_json::json!({"exp": "soon"}))), None);
        // `bbb` is not a JSON payload.
        assert_eq!(jwt_exp("aaa.bbb.ccc"), None);
    }

    #[test]
    fn an_expired_seeded_token_is_skipped_and_a_near_expiry_one_too() {
        let expired = jwt_with(serde_json::json!({"exp": NOW - 1400}));
        assert_eq!(usable_token_at(Some(&expired), "test", NOW), None);
        // Within the 60 s skew: would lapse in flight.
        let lapsing = jwt_with(serde_json::json!({"exp": NOW + EXPIRY_SKEW_SECS}));
        assert_eq!(usable_token_at(Some(&lapsing), "test", NOW), None);
        let live = jwt_with(serde_json::json!({"exp": NOW + EXPIRY_SKEW_SECS + 1}));
        assert_eq!(usable_token_at(Some(&live), "test", NOW), Some(live));
    }

    /// `exp` is read from an UNVERIFIED payload, so an adversarial or
    /// corrupt value must not overflow (a debug-build panic would kill the
    /// footprint task that runs the publish).
    #[test]
    fn an_extreme_negative_exp_is_expired_without_overflowing() {
        let min = jwt_with(serde_json::json!({"exp": i64::MIN}));
        assert_eq!(usable_token_at(Some(&min), "test", NOW), None);
        let huge_neg = jwt_with(serde_json::json!({"exp": -1e300}));
        assert_eq!(jwt_exp(&huge_neg), Some(i64::MIN));
        assert_eq!(usable_token_at(Some(&huge_neg), "test", NOW), None);
        assert!(expired_note("test", i64::MIN, i64::MAX).contains("expired"));
    }

    #[test]
    fn the_expired_note_says_ago_or_expires_in_and_names_only_the_source() {
        let past = expired_note("SRC", NOW - 1400, NOW);
        assert!(
            past.contains("SRC") && past.contains("expired 1400s ago"),
            "{past}"
        );
        let lapsing = expired_note("SRC", NOW + 30, NOW);
        assert!(lapsing.contains("expires in 30s"), "{lapsing}");
        assert!(!lapsing.contains("-30"), "{lapsing}");
    }

    /// Shape-valid with no readable `exp` stays accepted — coord decides.
    #[test]
    fn a_token_with_no_readable_exp_is_accepted() {
        let no_exp = jwt_with(serde_json::json!({"sub": "device"}));
        assert_eq!(usable_token_at(Some(&no_exp), "test", NOW), Some(no_exp));
        assert_eq!(
            usable_token_at(Some("aaa.bbb.ccc"), "test", NOW).as_deref(),
            Some("aaa.bbb.ccc")
        );
    }

    /// The incident shape: an hours-dead `$COORD_DEVICE_JWT` must not shadow
    /// a live file token.
    #[test]
    fn an_expired_env_token_falls_through_to_a_valid_file_token() {
        let expired = jwt_with(serde_json::json!({"exp": NOW - 1400}));
        let live = jwt_with(serde_json::json!({"exp": NOW + 3600}));
        assert_eq!(
            select_seeded_bearer(Some(&expired), Some(&live), NOW),
            Some((live.clone(), SOURCE_FILE))
        );
        // Env still wins when it is live.
        let env_live = jwt_with(serde_json::json!({"exp": NOW + 7200}));
        assert_eq!(
            select_seeded_bearer(Some(&env_live), Some(&live), NOW),
            Some((env_live, DEVICE_JWT_ENV))
        );
        // Both expired → nothing seeded, so the caller goes on to the mint.
        assert_eq!(
            select_seeded_bearer(Some(&expired), Some(&expired), NOW),
            None
        );
    }

    /// Captures everything logged on this thread while the guard is held.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl CapturedLog {
        fn install(&self) -> tracing::subscriber::DefaultGuard {
            let sub = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(self.clone())
                .with_ansi(false)
                .finish();
            tracing::subscriber::set_default(sub)
        }
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Every log line the seeded rungs and the mint parsers can emit names
    /// the source and never the token.
    #[test]
    fn no_bearer_log_line_carries_the_token() {
        let log = CapturedLog::default();
        let expired = jwt_with(serde_json::json!({"exp": NOW - 10}));
        let non_jwt = "opaque-secret-value-with-no-dots";
        {
            let _g = log.install();
            assert_eq!(usable_token_at(Some(&expired), "SRC_NAME", NOW), None);
            assert_eq!(usable_token_at(Some(non_jwt), "SRC_NAME", NOW), None);
            let v = serde_json::json!({"success": true, "data": non_jwt});
            assert_eq!(parse_invoked_token(&v, NOW), None);
        }
        let text = log.text();
        assert!(
            text.contains("SRC_NAME") && text.contains("expired"),
            "{text}"
        );
        assert!(
            !text.contains(&expired),
            "token leaked into a log line: {text}"
        );
        // Neither half of the token, either.
        for segment in expired.split('.') {
            assert!(!text.contains(segment), "token segment leaked: {text}");
        }
        assert!(
            !text.contains(non_jwt),
            "value leaked into a log line: {text}"
        );
    }

    /// The runner's no-tenant arm does not check `exp`, so a minted token
    /// gets the seeded rungs' expiry gate on both doors.
    #[test]
    fn a_minted_expired_token_is_no_credential_on_either_door() {
        let expired = jwt_with(serde_json::json!({"exp": NOW - 5}));
        let live = jwt_with(serde_json::json!({"exp": NOW + 3600}));
        let inv = |t: &str| serde_json::json!({"success": true, "data": t});
        let eval = |t: &str| serde_json::json!({"data": {"result": {"value": t}}});
        assert_eq!(parse_invoked_token(&inv(&expired), NOW), None);
        assert_eq!(parse_minted_token(&eval(&expired), NOW), None);
        assert_eq!(parse_invoked_token(&inv(&live), NOW), Some(live.clone()));
        assert_eq!(parse_minted_token(&eval(&live), NOW), Some(live));
    }

    #[test]
    fn parse_invoked_token_takes_a_jwt_and_refuses_every_other_answer() {
        let ok = serde_json::json!({"success": true, "data": " aaa.bbb.ccc "});
        assert_eq!(
            parse_invoked_token(&ok, NOW).as_deref(),
            Some("aaa.bbb.ccc")
        );
        let unpaired = serde_json::json!({"success": true, "data": null});
        assert_eq!(parse_invoked_token(&unpaired, NOW), None);
        let rejected = serde_json::json!({"success": false, "error": "store unreadable"});
        assert_eq!(parse_invoked_token(&rejected, NOW), None);
        let opaque = serde_json::json!({"success": true, "data": "qontinui_runner_x"});
        assert_eq!(parse_invoked_token(&opaque, NOW), None);
    }

    #[derive(Clone)]
    struct RunnerMock {
        invoke_status: u16,
        invoke_body: serde_json::Value,
        invoke_hits: Arc<Mutex<u32>>,
        evaluate_hits: Arc<Mutex<u32>>,
    }

    const EVALUATE_TOKEN: &str = "eval.minted.token";

    async fn mock_invoke(State(st): State<RunnerMock>) -> axum::response::Response {
        *st.invoke_hits.lock().unwrap() += 1;
        (
            StatusCode::from_u16(st.invoke_status).unwrap(),
            Json(st.invoke_body.clone()),
        )
            .into_response()
    }

    async fn mock_evaluate(State(st): State<RunnerMock>) -> axum::response::Response {
        *st.evaluate_hits.lock().unwrap() += 1;
        Json(serde_json::json!({"data": {"result": {"value": EVALUATE_TOKEN}}})).into_response()
    }

    /// A runner stand-in on an ephemeral loopback port serving both mint
    /// doors. Returns the base and the (invoke, evaluate) hit counters.
    async fn spawn_runner_mock(
        invoke_status: u16,
        invoke_body: serde_json::Value,
    ) -> (String, Arc<Mutex<u32>>, Arc<Mutex<u32>>) {
        let st = RunnerMock {
            invoke_status,
            invoke_body,
            invoke_hits: Arc::new(Mutex::new(0)),
            evaluate_hits: Arc::new(Mutex::new(0)),
        };
        let (inv, eval) = (Arc::clone(&st.invoke_hits), Arc::clone(&st.evaluate_hits));
        let app = Router::new()
            .route(MINT_INVOKE_PATH, post(mock_invoke))
            .route(MINT_EVALUATE_PATH, post(mock_evaluate))
            .with_state(st);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{}", addr.port()), inv, eval)
    }

    #[tokio::test]
    async fn mint_takes_the_invoke_answer_and_never_calls_evaluate() {
        let (base, inv, eval) = spawn_runner_mock(
            200,
            serde_json::json!({"success": true, "data": "inv.minted.token"}),
        )
        .await;
        assert_eq!(
            mint_device_jwt_at(&base).await.as_deref(),
            Some("inv.minted.token")
        );
        assert_eq!(*inv.lock().unwrap(), 1);
        assert_eq!(*eval.lock().unwrap(), 0, "evaluate must not be called");
    }

    #[tokio::test]
    async fn mint_falls_back_to_evaluate_on_invoke_400_and_404() {
        for status in [400, 404] {
            let (base, inv, eval) =
                spawn_runner_mock(status, serde_json::json!({"error": "unknown command"})).await;
            assert_eq!(
                mint_device_jwt_at(&base).await.as_deref(),
                Some(EVALUATE_TOKEN),
                "status {status}"
            );
            assert_eq!(*inv.lock().unwrap(), 1);
            assert_eq!(*eval.lock().unwrap(), 1, "status {status}");
        }
    }

    /// An expired token from the invoke door is no credential — and not a
    /// reason to ask the evaluate door, which reads the same store.
    #[tokio::test]
    async fn mint_invoke_expired_token_is_no_bearer() {
        let expired = jwt_with(serde_json::json!({"exp": 1_000}));
        let (base, _inv, eval) =
            spawn_runner_mock(200, serde_json::json!({"success": true, "data": expired})).await;
        assert_eq!(mint_device_jwt_at(&base).await, None);
        assert_eq!(*eval.lock().unwrap(), 0);
    }

    /// Unpaired is an ANSWER: evaluate would ask the same store the same
    /// question.
    #[tokio::test]
    async fn mint_invoke_null_is_no_bearer_and_no_evaluate() {
        let (base, _inv, eval) =
            spawn_runner_mock(200, serde_json::json!({"success": true, "data": null})).await;
        assert_eq!(mint_device_jwt_at(&base).await, None);
        assert_eq!(*eval.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn mint_invoke_other_failure_is_no_bearer_and_no_evaluate() {
        // 409 `tenant_required` is the realistic multi-tenant answer.
        for (status, body) in [
            (500, serde_json::json!({"error": "x"})),
            (409, serde_json::json!({"error": "tenant_required"})),
        ] {
            let (base, _inv, eval) = spawn_runner_mock(status, body).await;
            assert_eq!(mint_device_jwt_at(&base).await, None, "status {status}");
            assert_eq!(*eval.lock().unwrap(), 0, "status {status}");
        }
        assert_eq!(mint_device_jwt_at(&dead_base().await).await, None);
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
        // Inputs the old hand split wrongly read as loopback.
        assert!(!transport_may_carry_bearer("ws://[::1]:x@evil.com"));
        assert!(!transport_may_carry_bearer("http://[::1]@evil.com"));
        // Hosts are compared the way the HTTP client resolves them.
        assert!(transport_may_carry_bearer("http://LOCALHOST:9870"));
        assert!(!transport_may_carry_bearer("http://localhost.:9870"));
        assert!(!transport_may_carry_bearer("not a url"));
    }

    /// The withheld arm never reaches the resolver (no env, file or mint is
    /// consulted), and its label says why nothing was sent.
    #[tokio::test]
    async fn a_cleartext_non_loopback_base_withholds_the_bearer() {
        let (bearer, source) = resolve_device_bearer_for("http://coord.example.com").await;
        assert!(bearer.is_none());
        assert_eq!(source, SOURCE_WITHHELD);
    }

    #[test]
    fn refusal_hint_names_the_source_on_401_403_and_the_route_on_404() {
        let h = refusal_hint(reqwest::StatusCode::FORBIDDEN, SOURCE_FILE);
        assert!(
            h.contains("credential refused") && h.contains(SOURCE_FILE),
            "{h}"
        );
        let h = refusal_hint(reqwest::StatusCode::UNAUTHORIZED, SOURCE_WITHHELD);
        assert!(h.contains(SOURCE_WITHHELD), "{h}");
        assert_eq!(
            refusal_hint(reqwest::StatusCode::NOT_FOUND, SOURCE_FILE),
            " (route not served by this coord)"
        );
        assert_eq!(
            refusal_hint(reqwest::StatusCode::INTERNAL_SERVER_ERROR, SOURCE_FILE),
            ""
        );
    }
}
