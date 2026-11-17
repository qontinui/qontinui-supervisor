//! The browser-origin and Host guard for the supervisor's loopback HTTP API.
//!
//! Plan `2026-09-17-retire-the-runner-origin-guard-dev-grace`, Phase 1.
//!
//! # Why
//!
//! The supervisor binds `127.0.0.1`, which keeps other machines out but not a
//! web page open in the operator's own browser. Before this module every such
//! page could call `http://127.0.0.1:9875/…` and read the reply, because CORS
//! answered `Access-Control-Allow-Origin: *`. Four of the supervisor's routes
//! are proxies to a runner (`/runner-api/*`, `/ui-bridge/*`, `POST /graphql`,
//! `/runners/{id}/ui-bridge/*`), and each copies only `content-type` onto the
//! outgoing request. The runner therefore saw every proxied call as
//! non-browser and granted it full local trust, so the proxies laundered any
//! origin past the runner's own origin guard. A page could read a coord device
//! JWT through `POST /ui-bridge/invoke/get_coord_device_token`. The
//! supervisor's own routes also spawn processes (`/runners/spawn-test`, the
//! `ci-runner` routes) and make server-side requests to caller-supplied URLs
//! (`/web-fleet?backend_url=`).
//!
//! # What it does
//!
//! **Invariant (autonomy):** a request with no `Origin`, no cross-site Fetch
//! Metadata and a loopback `Host` behaves exactly as before. Every agent
//! `curl`, script and service caller is such a request. The guard adds no
//! credential an agent must hold.
//!
//! One middleware, layered outside CORS, runs two checks on every request,
//! before any handler and before any WebSocket upgrade:
//!
//! 1. **Host gate.** `Host` must be `127.0.0.1`, `localhost` or `[::1]` on the
//!    supervisor's bound port, or a value listed in [`ENV_ALLOWED_HOSTS`]. When
//!    `Host` is absent the request URI's authority (HTTP/2 `:authority`, or an
//!    absolute-form request target) is judged instead; only a request carrying
//!    neither is admitted unconditionally. This is the DNS-rebinding control: a
//!    rebound page is same-origin with its target and sends no `Origin`, so
//!    only the `Host` it names tells it apart from `curl`. Refusal: 403
//!    [`CODE_HOST_NOT_LOOPBACK`].
//! 2. **Origin gate.** Admitted:
//!    - no `Origin`, with `Sec-Fetch-Site` absent, `none` or `same-origin`
//!      (non-browser callers, and same-origin GETs, which carry no `Origin`);
//!    - the supervisor's own origins, `http://localhost|127.0.0.1|[::1]:<port>`
//!      (the dashboard SPA it serves);
//!    - the runner webview (the runner's CI-runner and dev-loop settings
//!      panels call the supervisor directly): `tauri://localhost` everywhere,
//!      and on Windows only also `http://tauri.localhost` and
//!      `https://tauri.localhost`, which is what WebView2 reports;
//!    - any origin listed in [`ENV_ALLOWED_ORIGINS`];
//!    - a top-level navigation to the dashboard SPA shell, see below.
//!
//!    **Windows residual.** `tauri.localhost` is a `*.localhost` name, which
//!    browsers resolve to loopback. A page served by any local process on port
//!    80 (`http://tauri.localhost`) or 443 (`https://tauri.localhost`) carries
//!    exactly the webview's origin and is admitted. Off Windows those two
//!    origins are refused, because the Tauri webview there reports
//!    `tauri://localhost`, which no web page can produce.
//!
//!    **SPA-shell navigations.** Following a link to the dashboard from another
//!    site sends no `Origin` and `Sec-Fetch-Site: cross-site` (or `same-site`
//!    from another `localhost` port). Such a request is admitted only when it
//!    is a GET or HEAD with `Sec-Fetch-Mode: navigate` and
//!    `Sec-Fetch-Dest: document`, and it targets `/` or falls through to the
//!    SPA fallback (no matched route) under a first path segment outside
//!    [`API_SEGMENTS`]. No API route is ever reachable this way, an iframe or
//!    subresource never is, and the navigating site cannot read the reply.
//!
//!    Everything else is refused with 403 [`CODE_CROSS_ORIGIN_REFUSED`],
//!    including `Origin: null`, extension pages, and a cross-site request that
//!    carries no `Origin` at all. There is no route allowlist: an origin the
//!    supervisor does not trust reaches no route. A preflight gets the same
//!    verdict its real request would, so a refused origin never receives an
//!    `Access-Control-Allow-Origin`.
//!
//! CORS ([`cors_layer`]) echoes only an origin this guard admitted, never `*`,
//! and tower-http adds `Vary: Origin`. The verdict also reads Fetch Metadata —
//! `Sec-Fetch-Site` whenever `Origin` is absent, plus `Sec-Fetch-Mode` and
//! `Sec-Fetch-Dest` for the SPA-shell navigation carve-out below — so
//! [`middleware`] appends all three to `Vary` on an admitted response. Two
//! requests differing only in those headers can get opposite verdicts, and a
//! cache keyed on `Origin` alone would not tell them apart. Nothing is
//! appended when the guard is off: there is no verdict to vary on.
//!
//! # What an admitted origin gets, and why the admit list is not a small dial
//!
//! The four runner proxies copy only `content-type` onto the outgoing request
//! (`routes/ui_bridge.rs`, `routes/graphql_proxy.rs`), so the runner classifies
//! every proxied call as non-browser and grants it FULL LOCAL TRUST — the
//! runner's own origin guard, its credential-door list and its route policy all
//! see a request with no browser provenance at all. That is the design: this
//! guard decides origin ONCE, here, instead of the runner deciding it again.
//!
//! The consequence is worth stating plainly, because nothing downstream can
//! re-impose it: an origin added to [`ENV_ALLOWED_ORIGINS`] is admitted to
//! EVERY route, the proxies included, so that page reaches the runner's
//! credential doors (`POST /ui-bridge/invoke/get_coord_device_token` among
//! them) whatever the runner's own allow-list says. This admit list is
//! therefore strictly more powerful than the runner's, and is for a trusted
//! first-party dev origin only — never a convenience switch for a page that
//! merely wants to read `/health`. The knowledge-base rule that a server-side
//! hop either enforces origin itself or forwards the browser's provenance
//! headers is satisfied here by the first arm
//! (`knowledge-base/qontinui-specific/runner-origin-guard.md`).
//!
//! # Configuration (read once, at startup)
//!
//! - [`ENV_GUARD`]`=0` turns both checks off. It is read when the router is
//!   built, so it takes effect at the supervisor's NEXT start. Never restart a
//!   supervisor to apply it: its JobObject reaps the temp runners it spawned.
//! - [`ENV_ALLOWED_ORIGINS`] and [`ENV_ALLOWED_HOSTS`] are comma lists. An
//!   entry that is not a well-formed origin, or not a well-formed
//!   `name[:port]`, is DROPPED with a WARN naming it — an entry kept but
//!   unmatchable would leave the operator reading a 403 whose body points at
//!   the very env var they had already set.
//!
//! `/health` reports `originGuard { enabled, requesterClass,
//! refusals{host,origin}, logSaturated{host,origin}, recent[<=20],
//! killSwitchEnv, admitOriginEnv, admitHostEnv }`. `recent` names other sites
//! the operator's browser pointed at this supervisor, so it is shown only to
//! non-browser and same-origin callers. `logSaturated` is how a reader tells a
//! quiet WARN log from a saturated one — read it before treating an absence of
//! refusal lines as an absence of refusals.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{MatchedPath, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

/// `0` disables the whole guard (Host gate and Origin gate).
pub const ENV_GUARD: &str = "QONTINUI_SUPERVISOR_ORIGIN_GUARD";
/// Comma list of extra browser origins the Origin gate admits.
pub const ENV_ALLOWED_ORIGINS: &str = "QONTINUI_SUPERVISOR_ALLOWED_ORIGINS";
/// Comma list of extra `Host` header values (`name[:port]`) the Host gate admits.
pub const ENV_ALLOWED_HOSTS: &str = "QONTINUI_SUPERVISOR_ALLOWED_HOSTS";

pub const CODE_HOST_NOT_LOOPBACK: &str = "HOST_NOT_LOOPBACK";
pub const CODE_CROSS_ORIGIN_REFUSED: &str = "CROSS_ORIGIN_REFUSED";

/// The runner webview's origins. `http(s)://tauri.localhost` is WebView2's
/// (Windows only): any local server on port 80/443 can carry it, see the
/// module doc's Windows residual.
#[cfg(windows)]
const WEBVIEW_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];
#[cfg(not(windows))]
const WEBVIEW_ORIGINS: &[&str] = &["tauri://localhost"];

/// First path segments of the supervisor's API routes. A cross-site top-level
/// navigation that falls through to the SPA fallback is admitted only outside
/// these. Pinned against every registered `.route(...)` by
/// `unit_tests::api_segments_cover_every_registered_route`.
pub const API_SEGMENTS: &[&str] = &[
    "actions",
    "ai",
    "build",
    "builds",
    "ci-runner",
    "control",
    "diagnostics",
    "eval",
    "expo",
    "graphql",
    "health",
    "help",
    "lkg",
    "logs",
    "runner",
    "runner-api",
    "runners",
    "spawn-worktrees",
    "supervisor",
    "supervisor-bridge",
    "test-login",
    "ui-bridge",
    "web-fleet",
    "ws",
];

/// First segments shared by API routes and the dashboard's client routes
/// (`frontend/src/App.tsx`). A navigation to an UNREGISTERED path under one of
/// these reaches only the SPA fallback; a registered API path under one of
/// them has a matched route and is refused. Read only by the tripwire test.
#[cfg(test)]
const SHARED_SPA_SEGMENTS: &[&str] = &[
    "lineage",
    "velocity",
    "velocity-improvement",
    "velocity-tests",
];

/// How many recent refusals `/health` carries.
const RECENT_CAP: usize = 20;
/// Distinct refusal subjects logged at WARN, per gate. Host keys and origin
/// keys have SEPARATE sets and separate caps, so saturating one cannot silence
/// the other — wildcard DNS (`r0..rN.evil.example`, all resolving to loopback)
/// makes Host keys attacker-multipliable, and N cross-origin iframes on
/// `i0..iN.evil.example` do the same for origin keys.
///
/// So the cap does NOT bound what an attacker can push out of the log. What it
/// buys is a bounded set: the guard never grows memory per refused subject, and
/// an ordinary polling page logs once rather than once per request. The refusal
/// COUNTERS keep counting after saturation, `recent` keeps its last
/// [`RECENT_CAP`] entries, and `/health` reports `logSaturated` per gate, so a
/// saturated log is visible rather than silent.
const LOG_KEYS_CAP: usize = 256;
/// Longest header value echoed into a log, `/health` or a refusal body.
const ECHO_MAX: usize = 256;

/// The guard's configuration: raw values, parsed by [`OriginGuard::new`].
#[derive(Debug, Clone)]
pub struct OriginGuardConfig {
    pub enabled: bool,
    /// The port the supervisor binds; its own origins and loopback `Host`
    /// values are judged against it.
    pub bound_port: u16,
    pub allowed_origins: Vec<String>,
    pub allowed_hosts: Vec<String>,
}

impl OriginGuardConfig {
    /// An enabled guard for `bound_port` with nothing extra admitted.
    #[cfg(test)]
    pub fn enabled(bound_port: u16) -> Self {
        Self {
            enabled: true,
            bound_port,
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
        }
    }

    /// Build from the raw values of [`ENV_GUARD`], [`ENV_ALLOWED_ORIGINS`] and
    /// [`ENV_ALLOWED_HOSTS`]. Pure, so tests never touch process env.
    pub fn from_values(
        guard: Option<&str>,
        origins: Option<&str>,
        hosts: Option<&str>,
        bound_port: u16,
    ) -> Self {
        Self {
            enabled: guard.map(|v| v.trim() != "0").unwrap_or(true),
            bound_port,
            allowed_origins: split_list(origins),
            allowed_hosts: split_list(hosts),
        }
    }

    /// Read the three env vars now. Called once, when the router is built.
    pub fn from_env(bound_port: u16) -> Self {
        let env = |k: &str| std::env::var(k).ok();
        Self::from_values(
            env(ENV_GUARD).as_deref(),
            env(ENV_ALLOWED_ORIGINS).as_deref(),
            env(ENV_ALLOWED_HOSTS).as_deref(),
            bound_port,
        )
    }
}

/// Who a request is, as far as its headers can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginClass {
    /// No `Origin`, and no cross-site Fetch Metadata: agents, scripts, curl,
    /// and same-origin GETs from the dashboard.
    NonBrowser,
    /// The supervisor's own loopback origin (the dashboard it serves).
    SameOrigin,
    /// The runner webview.
    Webview,
    /// An origin listed in [`ENV_ALLOWED_ORIGINS`].
    Allowed,
    /// Anything else, including `null` and a cross-site request with no
    /// `Origin`.
    Foreign,
}

impl OriginClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonBrowser => "non_browser",
            Self::SameOrigin => "same_origin",
            Self::Webview => "webview",
            Self::Allowed => "allowed",
            Self::Foreign => "foreign",
        }
    }
}

/// Inserted into every admitted request's extensions. `/health` reads it to
/// render `originGuard` for the class of its caller; the CORS layer reads
/// `allow_origin` instead of re-classifying.
#[derive(Clone)]
pub struct OriginGuardContext {
    guard: Arc<OriginGuard>,
    class: OriginClass,
    allow_origin: bool,
}

impl OriginGuardContext {
    /// The `/health` `originGuard` block, as this request's caller may see it.
    pub fn health_json(&self) -> Value {
        self.guard.health_json(self.class)
    }
}

/// The guard's parsed configuration and counters. One per router.
pub struct OriginGuard {
    enabled: bool,
    bound_port: u16,
    allowed_origins: Vec<NormOrigin>,
    allowed_hosts: Vec<String>,
    refused_host: AtomicU64,
    refused_origin: AtomicU64,
    recent: Mutex<VecDeque<Value>>,
    logged_host: Mutex<HashSet<String>>,
    logged_origin: Mutex<HashSet<String>>,
    log_saturated_host: AtomicBool,
    log_saturated_origin: AtomicBool,
}

impl OriginGuard {
    pub fn new(config: OriginGuardConfig) -> Self {
        let allowed_origins = config
            .allowed_origins
            .iter()
            .filter_map(|raw| {
                let parsed = NormOrigin::parse(raw);
                if parsed.is_none() {
                    tracing::warn!(
                        value = %truncate(raw),
                        "{ENV_ALLOWED_ORIGINS}: ignoring an entry that is not an origin"
                    );
                }
                parsed
            })
            .collect();
        Self {
            enabled: config.enabled,
            bound_port: config.bound_port,
            allowed_origins,
            allowed_hosts: config
                .allowed_hosts
                .iter()
                .filter_map(|raw| {
                    let parsed = parse_allowed_host(raw);
                    if parsed.is_none() {
                        tracing::warn!(
                            value = %truncate(raw),
                            "{ENV_ALLOWED_HOSTS}: ignoring an entry that is not a `name[:port]` Host value (an origin, a URL or a path never matches a Host header)"
                        );
                    }
                    parsed
                })
                .collect(),
            refused_host: AtomicU64::new(0),
            refused_origin: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
            logged_host: Mutex::new(HashSet::new()),
            logged_origin: Mutex::new(HashSet::new()),
            log_saturated_host: AtomicBool::new(false),
            log_saturated_origin: AtomicBool::new(false),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn host_admitted(&self, headers: &HeaderMap, uri: &Uri) -> bool {
        let host = match request_host(headers, uri) {
            RequestHost::Absent => return true,
            RequestHost::Unreadable => return false,
            RequestHost::Value(h) => h,
        };
        let host = host.trim().to_ascii_lowercase();
        if self.allowed_hosts.contains(&host) {
            return true;
        }
        let Some((name, port)) = split_host_port(&host) else {
            return false;
        };
        matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]")
            && port.and_then(|p| p.parse::<u16>().ok()) == Some(self.bound_port)
    }

    fn classify(&self, headers: &HeaderMap) -> OriginClass {
        let Some(origin) = headers.get(header::ORIGIN) else {
            let sfs = headers
                .get("sec-fetch-site")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_ascii_lowercase());
            return match sfs.as_deref() {
                None | Some("none") | Some("same-origin") => OriginClass::NonBrowser,
                _ => OriginClass::Foreign,
            };
        };
        let Some(norm) = origin.to_str().ok().and_then(NormOrigin::parse) else {
            return OriginClass::Foreign;
        };
        let loopback = matches!(norm.host.as_str(), "127.0.0.1" | "localhost" | "[::1]");
        if norm.scheme == "http" && loopback && norm.port == Some(self.bound_port) {
            return OriginClass::SameOrigin;
        }
        if WEBVIEW_ORIGINS
            .iter()
            .filter_map(|w| NormOrigin::parse(w))
            .any(|w| w == norm)
        {
            return OriginClass::Webview;
        }
        if self.allowed_origins.contains(&norm) {
            return OriginClass::Allowed;
        }
        OriginClass::Foreign
    }

    /// Count a refusal, keep it in `recent`, and say whether it is the first
    /// refusal of its subject (and so should be logged). The subject is the
    /// refused `Host` for a Host refusal and class + `Origin` for an origin
    /// refusal; the path in `entry` is deliberately not part of it.
    fn record_refusal(
        &self,
        kind: &'static str,
        class: OriginClass,
        subject: Option<&str>,
        entry: Value,
    ) -> bool {
        let (counter, keys, saturated, gate, log_key) = if kind == CODE_HOST_NOT_LOOPBACK {
            (
                &self.refused_host,
                &self.logged_host,
                &self.log_saturated_host,
                "host",
                format!("host|{subject:?}"),
            )
        } else {
            (
                &self.refused_origin,
                &self.logged_origin,
                &self.log_saturated_origin,
                "origin",
                format!("origin|{}|{subject:?}", class.as_str()),
            )
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut recent) = self.recent.lock() {
            if recent.len() == RECENT_CAP {
                recent.pop_front();
            }
            recent.push_back(entry);
        }
        let Ok(mut set) = keys.lock() else {
            return false;
        };
        if set.len() >= LOG_KEYS_CAP {
            if !saturated.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    gate,
                    distinct_subjects = LOG_KEYS_CAP,
                    "origin guard: the {gate} refusal log is SATURATED — further {gate} refusals are counted on /health (refusals.{gate}, logSaturated.{gate}) but no longer logged"
                );
            }
            return false;
        }
        set.insert(log_key)
    }

    /// The `/health` `originGuard` block. `recent` only for non-browser and
    /// same-origin callers.
    pub fn health_json(&self, requester: OriginClass) -> Value {
        let mut out = json!({
            "enabled": self.enabled,
            "requesterClass": requester.as_str(),
            "refusals": {
                "host": self.refused_host.load(Ordering::Relaxed),
                "origin": self.refused_origin.load(Ordering::Relaxed),
            },
            // Per-gate WARN logs stop at LOG_KEYS_CAP distinct subjects; the
            // counters above do not.
            "logSaturated": {
                "host": self.log_saturated_host.load(Ordering::Relaxed),
                "origin": self.log_saturated_origin.load(Ordering::Relaxed),
            },
            "killSwitchEnv": ENV_GUARD,
            "admitOriginEnv": ENV_ALLOWED_ORIGINS,
            "admitHostEnv": ENV_ALLOWED_HOSTS,
        });
        if matches!(requester, OriginClass::NonBrowser | OriginClass::SameOrigin) {
            let recent: Vec<Value> = self
                .recent
                .lock()
                .map(|r| r.iter().cloned().collect())
                .unwrap_or_default();
            out["recent"] = Value::Array(recent);
        }
        out
    }
}

/// CORS that echoes only an origin the guard admitted: never `*`. Must sit
/// INSIDE [`middleware`], which sets the [`OriginGuardContext`] it reads.
pub fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|_origin, parts| {
            parts
                .extensions
                .get::<OriginGuardContext>()
                .map(|c| c.allow_origin)
                .unwrap_or(false)
        }))
        .allow_methods(Any)
        .allow_headers(Any)
}

/// The guard middleware. Layer it OUTSIDE [`cors_layer`], so a refused request
/// (preflights included) never reaches CORS, a handler or a WebSocket upgrade.
pub async fn middleware(
    State(guard): State<Arc<OriginGuard>>,
    mut req: Request,
    next: Next,
) -> Response {
    let class = guard.classify(req.headers());
    if !guard.enabled {
        req.extensions_mut().insert(OriginGuardContext {
            guard,
            class,
            allow_origin: true,
        });
        return next.run(req).await;
    }

    let method = effective_method(req.method(), req.headers());
    let path = truncate(req.uri().path());
    let route_pattern = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string());

    if !guard.host_admitted(req.headers(), req.uri()) {
        let host = match request_host(req.headers(), req.uri()) {
            RequestHost::Value(h) => Some(truncate(&h)),
            RequestHost::Unreadable => req
                .headers()
                .get(header::HOST)
                .map(|v| truncate(&String::from_utf8_lossy(v.as_bytes()))),
            RequestHost::Absent => None,
        };
        let first = guard.record_refusal(
            CODE_HOST_NOT_LOOPBACK,
            class,
            host.as_deref(),
            json!({
                "code": CODE_HOST_NOT_LOOPBACK,
                "host": host,
                "method": method,
                "path": path,
                "at": chrono::Utc::now().to_rfc3339(),
            }),
        );
        if first {
            tracing::warn!(host = ?host, method = %method, path = %path, "origin guard: refused a non-loopback Host (logged once per host; counted on /health)");
        }
        return refusal(json!({
            "error": "Host header does not name this supervisor's loopback address and bound port",
            "code": CODE_HOST_NOT_LOOPBACK,
            "host": host,
            "method": method,
            "path": path,
            "route_pattern": route_pattern,
            "admitHostEnv": ENV_ALLOWED_HOSTS,
        }));
    }

    let spa_navigation = class == OriginClass::Foreign
        && is_spa_shell_navigation(
            req.method(),
            req.headers(),
            route_pattern.as_deref(),
            req.uri().path(),
        );
    if class == OriginClass::Foreign && !spa_navigation {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .map(|v| truncate(&String::from_utf8_lossy(v.as_bytes())));
        let first = guard.record_refusal(
            CODE_CROSS_ORIGIN_REFUSED,
            class,
            origin.as_deref(),
            json!({
                "code": CODE_CROSS_ORIGIN_REFUSED,
                "origin": origin,
                "method": method,
                "path": path,
                "at": chrono::Utc::now().to_rfc3339(),
            }),
        );
        if first {
            tracing::warn!(origin = ?origin, method = %method, path = %path, "origin guard: refused a browser origin (logged once per origin; counted on /health)");
        }
        return refusal(json!({
            "error": "This supervisor answers only non-browser callers, its own dashboard origin, the runner webview, and origins listed in the admit env var",
            "code": CODE_CROSS_ORIGIN_REFUSED,
            "origin": origin,
            "method": method,
            "path": path,
            "route_pattern": route_pattern,
            "admitOriginEnv": ENV_ALLOWED_ORIGINS,
        }));
    }

    req.extensions_mut().insert(OriginGuardContext {
        guard,
        class,
        allow_origin: true,
    });
    let mut resp = next.run(req).await;
    // Name every Fetch Metadata header the verdict above reads, not just the
    // first. With `Origin` absent the admission turns on `Sec-Fetch-Site`; and
    // for a Foreign request `is_spa_shell_navigation` additionally reads
    // `Sec-Fetch-Mode` and `Sec-Fetch-Dest`, so `Mode: navigate` +
    // `Dest: document` reaches the SPA shell where `Mode: cors` on the same
    // URL is a 403. Two requests differing only in headers `Vary` does not
    // name are not interchangeable, and CORS contributes only `Vary: Origin`.
    //
    // APPEND rather than insert: a second `Vary` field line is equivalent to
    // one comma list, and overwriting would drop CORS's.
    //
    // Only on this path. With the guard disabled the middleware returned above
    // without appending — there is no verdict, so there is nothing to vary on.
    resp.headers_mut().append(
        header::VARY,
        HeaderValue::from_static("Sec-Fetch-Site, Sec-Fetch-Mode, Sec-Fetch-Dest"),
    );
    resp
}

fn refusal(body: Value) -> Response {
    let mut resp = (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::VARY,
        HeaderValue::from_static("Origin, Sec-Fetch-Site"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

enum RequestHost {
    Absent,
    Unreadable,
    Value(String),
}

/// The host a request names: the `Host` header, or, when that is absent, the
/// request URI's authority (HTTP/2 `:authority`, absolute-form targets).
fn request_host(headers: &HeaderMap, uri: &Uri) -> RequestHost {
    match headers.get(header::HOST) {
        Some(raw) => match raw.to_str() {
            Ok(h) => RequestHost::Value(h.to_string()),
            Err(_) => RequestHost::Unreadable,
        },
        None => match uri.authority() {
            Some(a) => RequestHost::Value(a.as_str().to_string()),
            None => RequestHost::Absent,
        },
    }
}

/// A browser's top-level navigation to the dashboard SPA shell: GET/HEAD,
/// `Sec-Fetch-Mode: navigate`, `Sec-Fetch-Dest: document`, no `Origin`, and
/// either the `/` route or the SPA fallback (no matched route) outside
/// [`API_SEGMENTS`].
fn is_spa_shell_navigation(
    method: &Method,
    headers: &HeaderMap,
    route_pattern: Option<&str>,
    path: &str,
) -> bool {
    let header_is = |name: &str, want: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case(want))
    };
    if !(method == Method::GET || method == Method::HEAD)
        || headers.contains_key(header::ORIGIN)
        || !header_is("sec-fetch-mode", "navigate")
        || !header_is("sec-fetch-dest", "document")
    {
        return false;
    }
    match route_pattern {
        Some(pattern) => pattern == "/",
        None => {
            let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
            !API_SEGMENTS.iter().any(|s| s.eq_ignore_ascii_case(first))
        }
    }
}

/// A preflight is recorded under the method it asks about.
fn effective_method(method: &Method, headers: &HeaderMap) -> String {
    if method == Method::OPTIONS {
        if let Some(m) = headers
            .get(header::ACCESS_CONTROL_REQUEST_METHOD)
            .and_then(|v| v.to_str().ok())
        {
            return truncate(&m.trim().to_ascii_uppercase());
        }
    }
    method.as_str().to_string()
}

fn truncate(s: &str) -> String {
    match s.char_indices().nth(ECHO_MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Split a lowercased `Host` value into `(name, port)`, keeping an IPv6
/// literal's brackets on the name. `None` means the value is not a Host at all
/// (an unterminated `[`). Shared by [`OriginGuard::host_admitted`] and
/// [`parse_allowed_host`], so what the gate matches and what the env var
/// accepts cannot drift apart.
fn split_host_port(host: &str) -> Option<(String, Option<&str>)> {
    if let Some(rest) = host.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']')?;
        Some((format!("[{inner}]"), tail.strip_prefix(':')))
    } else {
        Some(match host.rsplit_once(':') {
            Some((n, p)) => (n.to_string(), Some(p)),
            None => (host.to_string(), None),
        })
    }
}

/// Normalize one [`ENV_ALLOWED_HOSTS`] entry, rejecting an origin, a URL, a
/// path, a query string, userinfo, whitespace, an unbracketed IPv6 address,
/// bracket junk, and a port that is not a bare `u16`.
///
/// [`OriginGuard::host_admitted`] compares the entry to the request's `Host`
/// by exact string equality, so a malformed entry does not fail loudly — it
/// simply never matches, and the operator is left reading a 403 whose body
/// names [`ENV_ALLOWED_HOSTS`] as the fix they had already applied. The
/// natural mistake is pasting an ORIGIN (`http://host.docker.internal:9875`)
/// into a variable that wants a Host (`host.docker.internal:9875`).
///
/// The list above is what this checks; it is deliberately not stated as
/// "anything that could never be a Host", because the two are not the same
/// set and a universal claim here would be the very thing this function
/// exists to stop — prose asserting more than the code delivers.
fn parse_allowed_host(raw: &str) -> Option<String> {
    let host = raw.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    if host
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@' | '\\'))
    {
        return None;
    }
    let (name, port) = split_host_port(&host)?;
    if host.starts_with('[') {
        // The bracket branch stops at `]`, so anything between `]` and the
        // port is silently dropped by the split: `[::1]9875` (a forgotten
        // colon) would otherwise normalize to a value no `Host` can equal.
        let tail = &host[name.len()..];
        if !(tail.is_empty() || tail.starts_with(':')) {
            return None;
        }
    } else if name.contains(':') {
        // An unbracketed IPv6 address. `rsplit_once(':')` happily reads `::1`
        // as name ":" port "1"; a real `Host` always brackets the literal, so
        // `::1` for `[::1]` — the likeliest typo in this variable — could
        // never match.
        return None;
    }
    if let Some(port) = port {
        // Round-trip, not merely parse: `u16::from_str` accepts a leading `+`
        // and leading zeros, so `:+80` and `:0080` parse to 80 while no `Host`
        // is ever spelled that way.
        match port.parse::<u16>() {
            Ok(n) if n.to_string() == port => {}
            _ => return None,
        }
    }
    let bare = name.trim_start_matches('[').trim_end_matches(']');
    if bare.is_empty()
        || !bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_'))
    {
        return None;
    }
    Some(host)
}

fn split_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// An origin compared on scheme, host and port (default ports filled in).
#[derive(Debug, Clone, PartialEq, Eq)]
struct NormOrigin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl NormOrigin {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim().trim_end_matches('/');
        if raw.is_empty() || raw.eq_ignore_ascii_case("null") {
            return None;
        }
        let u = url::Url::parse(raw).ok()?;
        Some(Self {
            scheme: u.scheme().to_ascii_lowercase(),
            host: u.host_str()?.to_ascii_lowercase(),
            port: u.port_or_known_default(),
        })
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::collections::BTreeSet;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn guard() -> OriginGuard {
        OriginGuard::new(OriginGuardConfig::from_values(
            None,
            Some("https://allowed.example, not an origin"),
            Some("host.docker.internal:9875"),
            9875,
        ))
    }

    #[test]
    fn classifies_origins() {
        let g = guard();
        let c = |pairs: &[(&'static str, &str)]| g.classify(&headers(pairs));
        assert_eq!(c(&[]), OriginClass::NonBrowser);
        assert_eq!(c(&[("sec-fetch-site", "none")]), OriginClass::NonBrowser);
        assert_eq!(
            c(&[("sec-fetch-site", "same-origin")]),
            OriginClass::NonBrowser
        );
        assert_eq!(c(&[("sec-fetch-site", "cross-site")]), OriginClass::Foreign);
        assert_eq!(c(&[("sec-fetch-site", "same-site")]), OriginClass::Foreign);
        assert_eq!(
            c(&[("origin", "http://127.0.0.1:9875")]),
            OriginClass::SameOrigin
        );
        assert_eq!(
            c(&[("origin", "http://localhost:9875")]),
            OriginClass::SameOrigin
        );
        assert_eq!(
            c(&[("origin", "http://localhost:9876")]),
            OriginClass::Foreign
        );
        assert_eq!(
            c(&[("origin", "https://127.0.0.1:9875")]),
            OriginClass::Foreign
        );
        assert_eq!(c(&[("origin", "tauri://localhost")]), OriginClass::Webview);
        // WebView2's origins: admitted on Windows only (module doc residual).
        let webview2 = if cfg!(windows) {
            OriginClass::Webview
        } else {
            OriginClass::Foreign
        };
        assert_eq!(c(&[("origin", "http://tauri.localhost")]), webview2);
        assert_eq!(c(&[("origin", "https://tauri.localhost")]), webview2);
        assert_eq!(
            c(&[("origin", "https://allowed.example")]),
            OriginClass::Allowed
        );
        assert_eq!(c(&[("origin", "null")]), OriginClass::Foreign);
        assert_eq!(
            c(&[("origin", "chrome-extension://abcdef")]),
            OriginClass::Foreign
        );
    }

    #[test]
    fn host_gate() {
        let g = guard();
        let root: Uri = "/".parse().unwrap();
        let ok = |h: &str| g.host_admitted(&headers(&[("host", h)]), &root);
        assert!(g.host_admitted(&HeaderMap::new(), &root));
        // No Host: the URI authority is judged instead.
        let abs = |u: &str| g.host_admitted(&HeaderMap::new(), &u.parse::<Uri>().unwrap());
        assert!(abs("http://127.0.0.1:9875/health"));
        assert!(!abs("http://evil.example:9875/health"));
        assert!(!ok("localhost.:9875"));
        assert!(ok("127.0.0.1:9875"));
        assert!(ok("LOCALHOST:9875"));
        assert!(ok("[::1]:9875"));
        assert!(ok("host.docker.internal:9875"));
        assert!(!ok("localhost"));
        assert!(!ok("localhost:9876"));
        assert!(!ok("evil.example:9875"));
        assert!(!ok("127.0.0.1.evil.example:9875"));
    }

    /// The router sources, as (module name as `crate::routes::<name>`,
    /// source). One list, so the scan and the merge allowlist cannot drift.
    const SCANNED_ROUTERS: &[(&str, &str)] = &[
        ("server", include_str!("server.rs")),
        ("velocity", include_str!("routes/velocity.rs")),
        ("evaluation", include_str!("routes/evaluation.rs")),
        ("velocity_tests", include_str!("routes/velocity_tests.rs")),
        (
            "velocity_improvement",
            include_str!("routes/velocity_improvement.rs"),
        ),
        ("dev_endpoints", include_str!("routes/dev_endpoints.rs")),
        ("dashboard", include_str!("routes/dashboard.rs")),
    ];

    /// Every literal route path `build_router` registers — `server.rs` and the
    /// routers it merges, closed over `.merge(` — normalised by
    /// [`normalise_path`].
    ///
    /// It also checks, in every scanned source and not just `server.rs`, that
    /// no route reaches the router by a path this scan cannot read: a
    /// `.merge(` of anything but another scanned module, a non-literal
    /// `.route(` path, or one of the forbidden service/nest registrations. So
    /// a caller can trust the set is the whole registered surface. Panics
    /// (failing the calling test) when any of that does not hold, or when the
    /// scan finds too few routes to be reading the real router.
    fn registered_route_paths() -> BTreeSet<String> {
        let route = regex::Regex::new(r#"\.route\(\s*"([^"]+)""#).unwrap();
        let merge = regex::Regex::new(r"\.merge\(\s*crate::routes::(\w+)::").unwrap();
        let mut paths = BTreeSet::new();
        let mut seen = 0;
        for (name, src) in SCANNED_ROUTERS {
            // `.fallback(` is fine (dashboard.rs serves the SPA through one);
            // `.fallback_service(` is not, because a service fallback (a
            // `ServeDir`, say) serves paths the SPA fallback does not, which
            // the navigation rule's API_SEGMENTS check would not see.
            for forbidden in [
                ".nest(",
                ".nest_service(",
                ".route_service(",
                ".fallback_service(",
            ] {
                assert!(
                    !src.contains(forbidden),
                    "{name}.rs: {forbidden} registers routes this scan cannot see"
                );
            }
            // Every merged router must itself be a scanned module — in any
            // source, not just server.rs.
            let merges = src.matches(".merge(").count();
            let merged: Vec<String> = merge.captures_iter(src).map(|c| c[1].to_string()).collect();
            assert_eq!(
                merges,
                merged.len(),
                "{name}.rs: a `.merge(` is not `crate::routes::<module>::…`"
            );
            for module in &merged {
                assert!(
                    SCANNED_ROUTERS.iter().any(|(n, _)| n == module),
                    "{name}.rs merges routes::{module}, which this scan does not read"
                );
            }
            // Every `.route(` must name its path as a string literal, or the
            // capture below silently skips it.
            let calls = src.matches(".route(").count();
            let literal = route.captures_iter(src).count();
            assert_eq!(
                calls, literal,
                "{name}.rs: {calls} `.route(` calls but {literal} with a literal path"
            );
            for cap in route.captures_iter(src) {
                seen += 1;
                paths.insert(normalise_path(&cap[1]));
            }
        }
        assert!(seen > 100, "the scan found only {seen} routes");
        assert!(
            SCANNED_ROUTERS[0].1.matches(".merge(").count() >= 6,
            "server.rs no longer merges the stateless routers this scan expects"
        );
        paths
    }

    /// `path` with every parameter segment (`{x}`, `:x`, or a URL segment
    /// built from an interpolated `{x}`) written `{}`, and every wildcard
    /// segment (`{*x}`, `*x`) written `*`.
    fn normalise_path(path: &str) -> String {
        path.split('/')
            .map(|seg| {
                if seg.starts_with("{*") || seg.starts_with('*') {
                    "*"
                } else if seg.starts_with(':') || (seg.contains('{') && seg.contains('}')) {
                    "{}"
                } else {
                    seg
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Whether the normalised self-call `call` would reach the normalised
    /// registered route `route`: segment by segment, a route's `{}` takes any
    /// one segment and its `*` takes the rest. A call's own `{}` (an
    /// interpolated segment) is taken to match any one segment, since its
    /// runtime value is unknown here.
    fn route_matches(route: &str, call: &str) -> bool {
        let route: Vec<&str> = route.split('/').collect();
        let call: Vec<&str> = call.split('/').collect();
        for (i, seg) in route.iter().enumerate() {
            if *seg == "*" {
                return call.len() > i;
            }
            match call.get(i) {
                Some(c) if c == seg || *seg == "{}" || *c == "{}" => {}
                _ => return false,
            }
        }
        route.len() == call.len()
    }

    /// The path of every request this production source sends to the
    /// supervisor's OWN API, normalised by [`normalise_path`].
    ///
    /// A self-call is recognised in these shapes, all built on
    /// [`crate::loopback::loopback_base`] with this supervisor's own port
    /// (`config.port`, `state.config.port`, `self.config.port`,
    /// `supervisor_port`, `self.supervisor_port`) as its argument, a
    /// rustfmt-split `loopback_base(\n    port,\n)` included:
    ///
    /// - `format!("{}/…", loopback_base(<own port>))`;
    /// - a local bound to that base, then either a `"{local}/…"` string
    ///   literal or a positional `format!("{}/…", local)`. Only the rest of
    ///   the block the binding sits in is read, up to a `let <local>` rebind
    ///   at the binding's own depth, so a same-named local bound to another
    ///   base elsewhere in the file is not taken for this one.
    ///
    /// Reach limits of the rebind cut: a shadow made by a pattern
    /// (`if let Some(base) = …`, `match`, a destructuring `let (base, _)`)
    /// or by a closure parameter (`|base| …`) is not seen, and the uses after
    /// it are still read as this binding's. A `let` right after `if`,
    /// `while` or `&&` is a condition binding that shadows only its own
    /// block, so it is not a cut either. A `let <local>` inside a string or
    /// comment is not a cut, but only because the block walk skips those; a
    /// literal or comment shape the walk does not recognise (see
    /// [`scope_end`]) would be matched. A multi-byte character outside a
    /// recognised literal is stepped over byte by byte and never read as a
    /// cut.
    ///
    /// Reach, so a green run is not over-read: a self-call spelled any other
    /// way (a hard-coded `127.0.0.1:9875`, a base passed in from elsewhere) is
    /// outside it. See [`normalise_call_path`] for how a path is read.
    fn self_call_paths(production: &str) -> Vec<String> {
        const OWN_PORT: &str = r"&?(?:(?:state|self)\.)?(?:config\.port|supervisor_port)\s*,?\s*\)";
        const BASE: &str = r"(?:crate::)?(?:loopback::)?loopback_base\(\s*";
        let binding =
            regex::Regex::new(&format!(r"let\s+(?:mut\s+)?(\w+)\s*=\s*{BASE}{OWN_PORT}")).unwrap();
        let inline = regex::Regex::new(&format!(
            r#"format!\(\s*"\{{\}}(/[^"]*)"\s*,\s*{BASE}{OWN_PORT}"#
        ))
        .unwrap();
        let mut raw: Vec<String> = inline
            .captures_iter(production)
            .map(|c| c[1].to_string())
            .collect();
        for local in binding.captures_iter(production) {
            let from = local.get(0).unwrap().end();
            let name = regex::escape(&local[1]);
            // A later `let <name>` at the binding's own brace depth shadows
            // it, so its scope ends there. One inside an inner block shadows
            // only that block and does not end it.
            let rebind = regex::Regex::new(&format!(r"^let\s+(?:mut\s+)?{name}\b")).unwrap();
            let bytes = production.as_bytes();
            // The walk steps byte by byte, so `i` can sit inside a multi-byte
            // character: compare bytes, and slice only through `get`.
            let stop_at = |i: usize| {
                bytes[i..].starts_with(b"let")
                    // Defensive: `<ident ending in let> <name>` is not valid
                    // Rust, so this check decides no real source today.
                    && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
                    && !follows_a_condition(bytes, i)
                    && production.get(i..).is_some_and(|rest| rebind.is_match(rest))
            };
            let scope = &production[from..scope_end(production, from, stop_at)];
            let named = regex::Regex::new(&format!(r#""\{{{name}\}}(/[^"]*)""#)).unwrap();
            let positional = regex::Regex::new(&format!(
                r#"format!\(\s*"\{{\}}(/[^"]*)"\s*,\s*{name}\s*[,)]"#
            ))
            .unwrap();
            raw.extend(named.captures_iter(scope).map(|c| c[1].to_string()));
            raw.extend(positional.captures_iter(scope).map(|c| c[1].to_string()));
        }
        raw.iter().map(|p| normalise_call_path(p)).collect()
    }

    /// Whether the `let` at `i` is a condition binding (`if let`,
    /// `while let`, `&& let`): the previous token, past whitespace, is `if`,
    /// `while` or `&&`.
    fn follows_a_condition(bytes: &[u8], i: usize) -> bool {
        let before = &bytes[..i];
        let end = before.len()
            - before
                .iter()
                .rev()
                .take_while(|b| b.is_ascii_whitespace())
                .count();
        let before = &before[..end];
        if before.ends_with(b"&&") {
            return true;
        }
        let word_len = before
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
            .count();
        matches!(&before[end - word_len..], b"if" | b"while")
    }

    /// A self-call's path as [`route_matches`] reads it. A query string or
    /// fragment is dropped. A `{…}` interpolation that FOLLOWS other
    /// characters in a segment (`/x{query}`, `/runs{suffix}`) is dropped and
    /// the path is read up to it: read as a parameter it would become `{}`,
    /// which matches any segment and would let `/dev-start{q}` pass as
    /// `/health`. A segment that is wholly an interpolation (`/{id}`) stays a
    /// parameter.
    ///
    /// Reach limit: the value of a dropped interpolation is unknown here. It
    /// is usually a query string or suffix, but it may carry further segments
    /// (`{me}/runs{rest}` with `rest = "/a/b"`), and then the path checked is
    /// only a prefix of the one sent.
    fn normalise_call_path(path: &str) -> String {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let segments: Vec<&str> = path
            .split('/')
            .map(|seg| match seg.find('{') {
                Some(at) if at > 0 => &seg[..at],
                _ => seg,
            })
            .collect();
        normalise_path(&segments.join("/"))
    }

    /// The byte offset where a scope that starts at `from` ends: the `}` that
    /// closes the block containing `from`, or the first CODE position at the
    /// same brace depth as `from` for which `stop_at(i)` holds, whichever
    /// comes first (the end of `text` when neither does). Braces inside
    /// string, raw string and char literals and inside comments are not
    /// counted, and `stop_at` is never asked about a position inside one, so a
    /// `"{base}/x"` format string does not move the count and a `let` in a
    /// comment is not a stop.
    ///
    /// Reach limits: a nested block comment (`/* a /* b */ c */`) ends at its
    /// first `*/`, and a char literal holding a multi-byte character (`'é'`)
    /// is not recognised as one. Neither holds a brace in this crate today;
    /// either could shift the count if one did.
    fn scope_end(text: &str, from: usize, stop_at: impl Fn(usize) -> bool) -> usize {
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut i = from;
        while i < bytes.len() {
            if depth == 0 && stop_at(i) {
                return i;
            }
            match bytes[i] {
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    i = text[i..].find('\n').map_or(bytes.len(), |n| i + n);
                    continue;
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    i = text[i + 2..]
                        .find("*/")
                        .map_or(bytes.len(), |n| i + 2 + n + 2);
                    continue;
                }
                // `r"…"`, `r#"…"#`, and the byte / C-string forms `br…`,
                // `cr…`: the `r` starts a token, or follows a `b` / `c` that
                // does.
                b'r' if matches!(bytes.get(i + 1), Some(b'"') | Some(b'#'))
                    && (i == 0
                        || !is_ident(bytes[i - 1])
                        || (matches!(bytes[i - 1], b'b' | b'c')
                            && (i == 1 || !is_ident(bytes[i - 2])))) =>
                {
                    let hashes = bytes[i + 1..].iter().take_while(|b| **b == b'#').count();
                    let open = i + 1 + hashes;
                    if bytes.get(open) == Some(&b'"') {
                        let close = format!("\"{}", "#".repeat(hashes));
                        i = text[open + 1..]
                            .find(&close)
                            .map_or(bytes.len(), |n| open + 1 + n + close.len());
                        continue;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < bytes.len() && bytes[i] != b'"' {
                        i += if bytes[i] == b'\\' { 2 } else { 1 };
                    }
                }
                b'\'' if bytes.get(i + 1) == Some(&b'\\') => {
                    // `'\''`, `'\n'`, `'\u{7f}'`: the closing quote is at or
                    // after the char following the backslash.
                    i = text
                        .get(i + 3..)
                        .and_then(|t| t.find('\''))
                        .map_or(bytes.len(), |n| i + 3 + n);
                }
                b'\'' if bytes.get(i + 2) == Some(&b'\'') => i += 2,
                b'{' => depth += 1,
                b'}' if depth == 0 => return i,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        bytes.len()
    }

    /// Every `.rs` file under `src/`, as (path relative to the crate, text).
    fn crate_sources() -> Vec<(String, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = Vec::new();
        let mut stack = vec![root.join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src/ is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let text = std::fs::read_to_string(&path)
                        .expect("source is readable")
                        .replace("\r\n", "\n");
                    let rel = path.strip_prefix(root).unwrap().display().to_string();
                    out.push((rel, text));
                }
            }
        }
        out
    }

    /// The self-calls in `sources` (as (name, production text)) whose path no
    /// registered route answers, as `name: path` lines; and how many
    /// self-calls were read in all.
    fn unregistered_self_calls(
        sources: &[(String, &str)],
        routes: &BTreeSet<String>,
    ) -> (Vec<String>, usize) {
        let mut read = 0;
        let mut dead = Vec::new();
        for (name, production) in sources {
            for call in self_call_paths(production) {
                read += 1;
                if !routes.iter().any(|r| route_matches(r, &call)) {
                    dead.push(format!("{name}: {call}"));
                }
            }
        }
        (dead, read)
    }

    /// No production request the supervisor sends to its own API names a
    /// route it does not register.
    ///
    /// Why: `velocity_improvement::restart_frontend` POSTed to
    /// `/dev-start/frontend` for seven months after `c415c5f` deleted every
    /// `/dev-start/*` route, and the dashboard showed "Restarting Frontend"
    /// while nothing restarted (plan
    /// `2026-11-17-supervisor-loopback-follow-ups-dead-dev-start-test-env-header-fragment-marker`).
    /// Zero self-calls is the expected reading today, so the fixture tests
    /// below are what prove this can fail. The `self-calls read: N, routes
    /// registered: M` line is printed on every run, but cargo captures it on
    /// a passing test: it is visible under `--nocapture` and in a failing
    /// run's output, not on an ordinary green run. See [`self_call_paths`]
    /// for what the scan recognises.
    #[test]
    fn supervisor_self_calls_name_registered_routes() {
        let routes = registered_route_paths();
        let files = crate_sources();
        let sources: Vec<(String, &str)> = files
            .iter()
            .map(|(name, text)| (name.clone(), crate::source_scan::production_span(text)))
            .collect();
        let (dead, read) = unregistered_self_calls(&sources, &routes);
        println!(
            "self-calls read: {read}, routes registered: {}",
            routes.len()
        );
        assert!(
            files.len() > 50,
            "read only {} sources under src/",
            files.len()
        );
        assert!(
            dead.is_empty(),
            "supervisor self-calls to routes it does not register:\n{}",
            dead.join("\n")
        );
    }

    /// The pre-deletion `restart_frontend` body, as it stood at `208b3f6`.
    const OLD_RESTART_FRONTEND: &str = r#"
async fn restart_frontend(state: &SharedState, restart_backend: bool) -> Result<(), String> {
    let client = &state.http_client;
    let supervisor = crate::loopback::loopback_base(state.config.port);

    if restart_backend {
        let _ = client
            .post(format!("{supervisor}/dev-start/backend"))
            .timeout(Duration::from_secs(120))
            .send()
            .await;
    }

    let resp = client
        .post(format!("{supervisor}/dev-start/frontend"))
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .map_err(|e| format!("Frontend restart request failed: {}", e))?;
    Ok(())
}
"#;

    #[test]
    fn self_call_guard_flags_the_old_dev_start_restart() {
        let routes = registered_route_paths();
        let (dead, read) = unregistered_self_calls(
            &[("velocity_improvement.rs".to_string(), OLD_RESTART_FRONTEND)],
            &routes,
        );
        assert_eq!(read, 2, "both /dev-start POSTs are self-calls");
        for path in ["/dev-start/frontend", "/dev-start/backend"] {
            assert!(
                dead.contains(&format!("velocity_improvement.rs: {path}")),
                "{path} not flagged: {dead:?}"
            );
        }
    }

    #[test]
    fn self_call_guard_passes_a_registered_route() {
        let routes = registered_route_paths();
        let bound = r#"
fn probe(state: &SharedState) {
    let me = crate::loopback::loopback_base(state.config.port);
    let _ = client.get(format!("{me}/health?verbose=1"));
}
"#;
        let inline =
            r#"let url = format!("{}/health", loopback::loopback_base(self.supervisor_port));"#;
        let (dead, read) = unregistered_self_calls(
            &[
                ("bound.rs".to_string(), bound),
                ("inline.rs".to_string(), inline),
            ],
            &routes,
        );
        assert_eq!(read, 2);
        assert!(dead.is_empty(), "{dead:?}");
        // A port that is not this supervisor's own is not a self-call.
        let other_port =
            r#"let url = format!("{}/dev-start/x", crate::loopback::loopback_base(port));"#;
        let (_, other) = unregistered_self_calls(&[("port.rs".to_string(), other_port)], &routes);
        assert_eq!(other, 0);
    }

    /// A1: a binding's `"{local}/…"` uses are read only in the rest of its own
    /// block. Here the own-port `base` comes FIRST, and the later `runner`
    /// takes a runner URL as a PARAMETER named `base` (no `let`, so no rebind
    /// cut can hide it): a file-wide search, or a walk that over-counts an
    /// opening brace, also reads `runner`'s `"{base}/runner-only"`.
    ///
    /// Between the binding and its use sit brace-bearing literals, a comment
    /// and a lifetime, each of which the block walk must skip. A closing brace
    /// that is counted ends the scope before the use (read 0); an opening
    /// brace that is counted carries the scope into `runner` (read 2). The
    /// closing literals come FIRST, so a walk that skipped no string at all
    /// would still see an unmatched `}` rather than a `"}"` / `"{"` pair that
    /// cancels out. Each raw literal holds a `"` before its `{`, so only real
    /// raw-string handling (prefix included) skips it; a plain-string read
    /// would end at that quote and count the brace.
    #[test]
    fn self_call_guard_reads_a_binding_only_in_its_own_block() {
        let routes = registered_route_paths();
        let src = r##"
fn own(state: &SharedState) {
    let base = crate::loopback::loopback_base(state.config.port);
    let close = ('}', "}");
    let open = ('{', "{", r#""{"#, br#""{"#, cr#"a"{"#); // {
    fn f<'a>(s: &'a str) -> &'a str {
        s
    }
    let _ = client.get(format!("{base}/health"));
}

fn runner(base: &str) {
    let _ = client.get(format!("{base}/runner-only"));
}
"##;
        let (dead, read) = unregistered_self_calls(&[("two_fns.rs".to_string(), src)], &routes);
        assert_eq!(read, 1, "only the own-port fn's /health is a self-call");
        assert!(dead.is_empty(), "{dead:?}");
    }

    /// A later `let <name>` in the same block shadows the own-port binding, so
    /// a use after it is not read as a self-call.
    #[test]
    fn self_call_guard_stops_at_a_shadowing_rebind() {
        let routes = registered_route_paths();
        let src = r#"
fn f(state: &SharedState, port: u16) {
    let base = crate::loopback::loopback_base(state.config.port);
    let _ = client.get(format!("{base}/health"));
    let mut base = crate::loopback::runner_loopback_base(port);
    let _ = client.get(format!("{base}/runner-only"));
}
"#;
        let (dead, read) = unregistered_self_calls(&[("shadow.rs".to_string(), src)], &routes);
        assert_eq!(read, 1, "only the use before the rebind is a self-call");
        assert!(dead.is_empty(), "{dead:?}");
    }

    /// A `let` after `if`, `while` or `&&` binds only inside its own block,
    /// so the outer binding's later use is still read, and here flagged.
    #[test]
    fn self_call_guard_ignores_a_condition_binding() {
        let routes = registered_route_paths();
        let src = r#"
fn f(state: &SharedState, x: bool) {
    let base = crate::loopback::loopback_base(state.config.port);
    if let base = other() {
        drop(base);
    }
    while let base = other() {}
    while x && let base = other() {}
    let _ = client.post(format!("{base}/dead-route"));
}
"#;
        let (dead, read) = unregistered_self_calls(&[("cond.rs".to_string(), src)], &routes);
        assert_eq!(read, 1, "the outer use is still this binding's");
        assert_eq!(dead, ["cond.rs: /dead-route"]);
    }

    /// A multi-byte char literal before the use: the walk steps through its
    /// bytes, and the rebind check must not slice inside the character.
    #[test]
    fn self_call_guard_survives_a_multi_byte_char() {
        let routes = registered_route_paths();
        let src = r#"
fn f(state: &SharedState) {
    let base = crate::loopback::loopback_base(state.config.port);
    let c = '…';
    let _ = client.get(format!("{base}/health"));
}
"#;
        let (dead, read) = unregistered_self_calls(&[("utf8.rs".to_string(), src)], &routes);
        assert_eq!(read, 1);
        assert!(dead.is_empty(), "{dead:?}");
    }

    /// A rebind inside an INNER block shadows only that block, so the outer
    /// binding's later use is still read, and here flagged.
    #[test]
    fn self_call_guard_ignores_a_rebind_in_an_inner_block() {
        let routes = registered_route_paths();
        let src = r#"
fn f(state: &SharedState, x: bool) {
    let base = crate::loopback::loopback_base(state.config.port);
    if x {
        let base = other();
    }
    let _ = client.post(format!("{base}/dead-route"));
}
"#;
        let (dead, read) = unregistered_self_calls(&[("inner.rs".to_string(), src)], &routes);
        assert_eq!(read, 1, "the outer use is still this binding's");
        assert_eq!(dead, ["inner.rs: /dead-route"]);
    }

    /// `let mut` binds an own-port base as well as `let` does.
    #[test]
    fn self_call_guard_reads_a_let_mut_binding() {
        let routes = registered_route_paths();
        let src = r#"
fn f(state: &SharedState) {
    let mut me = crate::loopback::loopback_base(state.config.port);
    let _ = client.post(format!("{me}/dev-start/frontend"));
}
"#;
        let (dead, read) = unregistered_self_calls(&[("mut.rs".to_string(), src)], &routes);
        assert_eq!(read, 1);
        assert_eq!(dead, ["mut.rs: /dev-start/frontend"]);
    }

    /// A2: the positional, rustfmt-split and suffix-interpolated spellings are
    /// all read, and an interpolated suffix does not become a parameter that
    /// matches any route.
    #[test]
    fn self_call_guard_reads_every_recognised_spelling() {
        let routes = registered_route_paths();
        let read_one = |src: &str| {
            let (dead, read) = unregistered_self_calls(&[("f.rs".to_string(), src)], &routes);
            assert_eq!(read, 1, "expected one self-call in {src}");
            dead
        };
        // Positional `format!("{}/…", local)`.
        let positional = r#"
fn f(state: &SharedState) {
    let me = crate::loopback::loopback_base(state.config.port);
    let _ = client.post(format!("{}/dev-start/frontend", me));
}
"#;
        assert_eq!(read_one(positional), ["f.rs: /dev-start/frontend"]);
        assert!(read_one(&positional.replace("/dev-start/frontend", "/health")).is_empty());
        // A rustfmt-split argument list, trailing comma included.
        let split = r#"
fn f(state: &SharedState) {
    let me = crate::loopback::loopback_base(
        state.config.port,
    );
    let _ = client.post(format!("{me}/dev-start/frontend"));
}
"#;
        assert_eq!(read_one(split), ["f.rs: /dev-start/frontend"]);
        // The inline form, rustfmt-split.
        let split_inline = r#"
fn f(state: &SharedState) {
    let _ = client.post(format!(
        "{}/dev-start/frontend",
        crate::loopback::loopback_base(
            state.config.port,
        )
    ));
}
"#;
        assert_eq!(read_one(split_inline), ["f.rs: /dev-start/frontend"]);
        // An interpolated suffix is not a wildcard segment.
        let suffix = r#"
fn f(state: &SharedState) {
    let me = crate::loopback::loopback_base(state.config.port);
    let _ = client.get(format!("{me}/dev-start{query}"));
}
"#;
        assert_eq!(read_one(suffix), ["f.rs: /dev-start"]);
        assert!(read_one(&suffix.replace("/dev-start{query}", "/health{query}")).is_empty());
    }

    #[test]
    fn route_matching_reads_parameters_and_wildcards() {
        assert!(route_matches(
            "/eval/runs/{}",
            &normalise_path("/eval/runs/{id}")
        ));
        assert!(route_matches("/eval/runs/{}", "/eval/runs/abc"));
        assert!(!route_matches("/eval/runs/{}", "/eval/runs"));
        assert!(!route_matches("/eval/runs/{}", "/eval/runs/a/b"));
        assert!(route_matches(
            &normalise_path("/assets/{*rest}"),
            "/assets/a/b"
        ));
        assert!(!route_matches("/health", "/healthz"));
    }

    /// Every route `build_router` registers (in `server.rs` and the routers it
    /// merges) has a first segment in [`API_SEGMENTS`] or
    /// [`SHARED_SPA_SEGMENTS`], so a new API prefix cannot silently become
    /// reachable by a cross-site navigation. And no dashboard client route is
    /// an API segment, so deep links keep working.
    ///
    /// The routes come from [`registered_route_paths`], which reads only the
    /// sources in [`SCANNED_ROUTERS`] and so also checks that no route reaches
    /// the router by a path it cannot read.
    #[test]
    fn api_segments_cover_every_registered_route() {
        for path in registered_route_paths() {
            if path == "/" {
                continue;
            }
            let first = path.trim_start_matches('/').split('/').next().unwrap();
            assert!(
                API_SEGMENTS.contains(&first) || SHARED_SPA_SEGMENTS.contains(&first),
                "route {path}: add {first:?} to API_SEGMENTS (or SHARED_SPA_SEGMENTS if the dashboard has a client route under it)"
            );
        }

        let app = include_str!("../frontend/src/App.tsx");
        let client = regex::Regex::new(r#"<Route\s+path="/([^/"]*)"#).unwrap();
        let client_segments: Vec<String> = client
            .captures_iter(app)
            .map(|c| c[1].to_string())
            .collect();
        assert!(client_segments.iter().any(|s| s == "dashboard"));
        for seg in &client_segments {
            assert!(
                !API_SEGMENTS.contains(&seg.as_str()),
                "dashboard client route /{seg} would be refused to cross-site navigations"
            );
        }
        for seg in SHARED_SPA_SEGMENTS {
            assert!(
                client_segments.iter().any(|s| s == seg),
                "{seg} is in SHARED_SPA_SEGMENTS but is no longer a dashboard client route"
            );
        }
    }

    /// Every [`API_SEGMENTS`] entry has a Vite dev-proxy prefix, so a page on
    /// the dev server (`frontend`, port 5174) can still reach every API route.
    ///
    /// This became load-bearing WITH the guard, not before it. A dev page used
    /// to reach an unproxied route by asking `http://localhost:9875` directly,
    /// cross-origin, under `Access-Control-Allow-Origin: *`; the Origin gate
    /// refuses that now, on purpose. So a segment missing from the proxy is
    /// unreachable in dev and fails SILENTLY — the Vite server answers the SPA
    /// shell where a JSON route was expected.
    ///
    /// Vite matches a string proxy key as a plain prefix, and this test does
    /// the same, so `/runner` legitimately covers `/runner-api`.
    /// [`SHARED_SPA_SEGMENTS`] is deliberately NOT required: each of those is
    /// both an API route and a dashboard client route, so whether the dev
    /// server should proxy it or serve it is a judgement this test cannot make.
    #[test]
    fn every_api_segment_is_reachable_through_the_dev_proxy() {
        let config = include_str!("../frontend/vite.config.ts");
        let block = config
            .split_once("proxy: {")
            .expect("vite.config.ts no longer has a `proxy: {` block")
            .1;
        let block = block
            .split_once("\n    },")
            .expect("the `proxy: {` block is no longer closed by `\\n    },`")
            .0;
        // Strip whole-line `//` comments FIRST. The block deliberately carries
        // prose, and a comment containing `'/foo':` would mint a phantom key —
        // which fails in the vacuous direction, letting a genuinely missing
        // segment pass. Only leading-`//` lines are dropped, so a `//` inside a
        // string (a URL) is left alone.
        let code: String = block
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // Both quote styles: a MIXED switch would otherwise hide one key and
        // send the author to add an entry that is already there.
        let key = regex::Regex::new(r#"['"](/[^'"]+)['"]\s*:"#).unwrap();
        let keys: Vec<&str> = key
            .captures_iter(&code)
            .map(|c| c.get(1).unwrap().as_str())
            .collect();
        // Anti-vacuity: an extraction that silently matched nothing would make
        // every assertion below pass. The bar is deliberately far BELOW
        // `API_SEGMENTS.len()` — a short list means the config is incomplete,
        // which is the defect the per-segment assertion below reports by name,
        // and folding the two would hide it behind "the extraction is broken".
        assert!(
            keys.len() >= 5 && keys.contains(&"/health"),
            "read {} proxy keys from vite.config.ts ({keys:?}) — the extraction is broken, not the config",
            keys.len()
        );
        for seg in API_SEGMENTS {
            let path = format!("/{seg}");
            assert!(
                keys.iter().any(|k| path.starts_with(k)),
                "API segment /{seg} has no Vite dev-proxy entry: add '/{seg}': toSupervisor() to frontend/vite.config.ts, or the dev dashboard gets the SPA shell instead of the route"
            );
        }
        // The rewrite the guard depends on must still be wired to both hooks.
        for hook in ["proxyReq", "proxyReqWs"] {
            assert!(
                config.contains(&format!("proxy.on('{hook}'")),
                "vite.config.ts no longer rewrites the dev server's own Origin on {hook}"
            );
        }
        assert!(
            config.contains("strictPort: true"),
            "strictPort guards the Origin rewrite, which is keyed on DEV_PORT"
        );
    }

    #[test]
    fn allowed_hosts_entries_that_can_never_match_a_host_header_are_dropped() {
        // A Host header is `name[:port]`. The natural operator mistake is
        // pasting an ORIGIN into the variable that wants a Host; it would be
        // stored, never match, and leave a 403 pointing at the env var the
        // operator had already set.
        for ok in [
            "host.docker.internal:9875",
            "HOST.DOCKER.INTERNAL:9875",
            "  box.local:80  ",
            "[::1]:9875",
            "[::1]",
            "example.test",
        ] {
            assert!(
                parse_allowed_host(ok).is_some(),
                "{ok:?} is a valid Host value"
            );
        }
        for bad in [
            "http://host.docker.internal:9875",
            "https://box.local",
            "box.local/path",
            "box.local:9875/",
            "box.local?x=1",
            "user@box.local",
            "box.local:99999",
            "box.local:notaport",
            "box local",
            "[::1:9875",
            "",
            "   ",
            // Unbracketed IPv6: `rsplit_once(':')` reads `::1` as name ":"
            // port "1", and a real Host always brackets the literal.
            "::1",
            "2001:db8::1",
            // Bracket junk the `]` split would otherwise discard.
            "[::1]9875",
            "[::1]x:80",
            // Ports that `u16::from_str` accepts but no Host is spelled with.
            "box.local:+80",
            "box.local:0080",
        ] {
            assert!(
                parse_allowed_host(bad).is_none(),
                "{bad:?} can never equal a Host header and must be dropped"
            );
        }
        // Dropped, not merely unmatched: the guard holds only usable entries.
        let g = OriginGuard::new(OriginGuardConfig::from_values(
            None,
            None,
            Some("http://box.local:9875, box.local:9875"),
            9875,
        ));
        assert_eq!(g.allowed_hosts, vec!["box.local:9875".to_string()]);
        let root: Uri = "/".parse().unwrap();
        assert!(g.host_admitted(&headers(&[("host", "box.local:9875")]), &root));
        assert!(!g.host_admitted(&headers(&[("host", "http://box.local:9875")]), &root));
    }

    #[test]
    fn kill_switch_values() {
        let on = |v| OriginGuardConfig::from_values(v, None, None, 1).enabled;
        assert!(on(None));
        assert!(on(Some("1")));
        assert!(on(Some("")));
        assert!(!on(Some("0")));
        assert!(!on(Some(" 0 ")));
    }

    /// One origin refused on 300 distinct paths logs once, and cannot fill the
    /// log-once set: a new origin (and a new Host) afterwards is still logged.
    #[test]
    fn junk_paths_cannot_silence_later_refusal_logs() {
        let g = guard();
        let refuse = |origin: &str, path: &str| {
            g.record_refusal(
                CODE_CROSS_ORIGIN_REFUSED,
                OriginClass::Foreign,
                Some(origin),
                json!({ "origin": origin, "path": path }),
            )
        };
        assert!(refuse("https://evil.example", "/junk/0"));
        for i in 1..300 {
            assert!(!refuse("https://evil.example", &format!("/junk/{i}")));
        }
        assert!(refuse("https://second.example", "/ui-bridge/x"));
        assert!(g.record_refusal(
            CODE_HOST_NOT_LOOPBACK,
            OriginClass::NonBrowser,
            Some("rebound.example:9875"),
            json!({}),
        ));
        assert_eq!(
            g.health_json(OriginClass::NonBrowser)["refusals"]["origin"],
            json!(301)
        );
    }

    /// The two log-once sets are separate and each is capped: filling the Host
    /// set (wildcard DNS) silences further HOST logs, marks
    /// `logSaturated.host`, and leaves origin logging untouched.
    #[test]
    fn the_log_once_sets_are_capped_separately() {
        let g = guard();
        let host = |h: &str| {
            g.record_refusal(
                CODE_HOST_NOT_LOOPBACK,
                OriginClass::NonBrowser,
                Some(h),
                json!({}),
            )
        };
        for i in 0..LOG_KEYS_CAP {
            assert!(host(&format!("r{i}.evil.example:9875")), "host {i}");
        }
        assert!(!host("overflow.evil.example:9875"));
        assert!(g.record_refusal(
            CODE_CROSS_ORIGIN_REFUSED,
            OriginClass::Foreign,
            Some("https://after-saturation.example"),
            json!({}),
        ));
        let health = g.health_json(OriginClass::NonBrowser);
        assert_eq!(health["logSaturated"]["host"], json!(true));
        assert_eq!(health["logSaturated"]["origin"], json!(false));
        assert_eq!(
            health["refusals"]["host"],
            json!(LOG_KEYS_CAP as u64 + 1),
            "counting continues after saturation"
        );
    }

    #[test]
    fn recent_is_withheld_from_browser_classes_and_capped() {
        let g = guard();
        for i in 0..(RECENT_CAP + 5) {
            g.record_refusal(
                CODE_CROSS_ORIGIN_REFUSED,
                OriginClass::Foreign,
                Some("https://evil.example"),
                json!({ "i": i }),
            );
        }
        g.record_refusal(
            CODE_HOST_NOT_LOOPBACK,
            OriginClass::NonBrowser,
            Some("evil.example:9875"),
            json!({}),
        );
        let nb = g.health_json(OriginClass::NonBrowser);
        assert_eq!(nb["refusals"]["origin"], json!(RECENT_CAP as u64 + 5));
        assert_eq!(nb["refusals"]["host"], json!(1));
        assert_eq!(nb["recent"].as_array().unwrap().len(), RECENT_CAP);
        assert!(g.health_json(OriginClass::SameOrigin)["recent"].is_array());
        for class in [
            OriginClass::Webview,
            OriginClass::Allowed,
            OriginClass::Foreign,
        ] {
            assert!(g.health_json(class).get("recent").is_none(), "{class:?}");
        }
    }
}

#[cfg(test)]
mod router_tests {
    use super::{
        OriginGuardConfig, CODE_CROSS_ORIGIN_REFUSED, CODE_HOST_NOT_LOOPBACK, ENV_ALLOWED_ORIGINS,
    };
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use serde_json::Value;
    use tower::ServiceExt;

    const PORT: u16 = 9875;
    const EVIL: &str = "https://evil.example";

    fn config(root: &std::path::Path) -> crate::config::SupervisorConfig {
        use crate::config::{BuildPoolConfig, RunnerConfig, SupervisorConfig};
        SupervisorConfig {
            project_dir: root.join("qontinui-runner").join("src-tauri"),
            watchdog_enabled_at_start: false,
            auto_start: false,
            auto_debug: false,
            log_file: None,
            log_dir: None,
            port: PORT,
            dev_logs_dir: root.join(".dev-logs"),
            cli_args: vec![],
            expo_dir: None,
            expo_port: 19000,
            runners: vec![RunnerConfig::default_primary()],
            build_pool: BuildPoolConfig::fixed(1),
            no_prewarm: false,
            no_webview: true,
            temp_runner_display: None,
            ambient_target_dir: crate::config::AmbientTargetDir::Ignore,
            ambient_coord: crate::config::AmbientCoord::Ignore,
        }
    }

    fn state(root: &std::path::Path) -> crate::state::SharedState {
        std::sync::Arc::new(crate::state::SupervisorState::new(config(root)))
    }

    /// The router as production builds it, with the guard config passed in
    /// rather than read from process env (hermetic: no test touches env).
    async fn router_with(root: &std::path::Path, config: OriginGuardConfig) -> Router {
        let st = state(root);
        st.cached_health.write().await.runner_responding = false;
        crate::server::build_router_with_origin_guard(st, config)
    }

    async fn router(root: &std::path::Path) -> Router {
        router_with(root, OriginGuardConfig::enabled(PORT)).await
    }

    fn loopback_host() -> String {
        format!("127.0.0.1:{PORT}")
    }

    fn acao(resp: &axum::response::Response) -> Option<String> {
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn token_request(origin: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri("/ui-bridge/invoke/get_coord_device_token")
            .header(header::HOST, loopback_host())
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        b.body(Body::from("{}")).unwrap()
    }

    #[tokio::test]
    async fn t1_foreign_origin_is_refused_on_the_ui_bridge_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let resp = router(dir.path())
            .await
            .oneshot(token_request(Some(EVIL)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(acao(&resp), None);
        assert_eq!(
            resp.headers().get(header::VARY).unwrap(),
            "Origin, Sec-Fetch-Site"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = body_json(resp).await;
        assert_eq!(body["code"], CODE_CROSS_ORIGIN_REFUSED);
        assert_eq!(body["origin"], EVIL);
        assert_eq!(body["path"], "/ui-bridge/invoke/get_coord_device_token");
        assert_eq!(body["route_pattern"], "/ui-bridge/{*path}");
        assert_eq!(body["admitOriginEnv"], ENV_ALLOWED_ORIGINS);
    }

    #[tokio::test]
    async fn t2_foreign_preflight_gets_no_allow_origin() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/runner-api/instances/spawn")
            .header(header::HOST, loopback_host())
            .header(header::ORIGIN, EVIL)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .body(Body::empty())
            .unwrap();
        let resp = router(dir.path()).await.oneshot(req).await.unwrap();
        assert_eq!(acao(&resp), None, "status {}", resp.status());
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn t3_non_loopback_host_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/runner-api/health")
            .header(header::HOST, format!("evil.example:{PORT}"))
            .body(Body::empty())
            .unwrap();
        let resp = router(dir.path()).await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["code"], CODE_HOST_NOT_LOOPBACK);
    }

    #[tokio::test]
    async fn t4_foreign_websocket_upgrade_is_refused_before_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/ws")
            .header(header::HOST, loopback_host())
            .header(header::ORIGIN, EVIL)
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .unwrap();
        let resp = router(dir.path()).await.oneshot(req).await.unwrap();
        // Under `oneshot` there is no hyper connection to upgrade, so `!= 101`
        // is not upgrade coverage: without the guard the extractor answers 426.
        // The 403 is what proves the guard ran before the WebSocket handler.
        assert_ne!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn t5_foreign_origin_is_refused_on_the_per_runner_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/runners/primary/ui-bridge/invoke/get_coord_device_token")
            .header(header::HOST, loopback_host())
            .header(header::ORIGIN, EVIL)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = router(dir.path()).await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn t6_runner_webview_origin_is_admitted_and_echoed() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/ci-runner/status")
            .header(header::HOST, format!("localhost:{PORT}"))
            .header(header::ORIGIN, "tauri://localhost")
            .body(Body::empty())
            .unwrap();
        let resp = router(dir.path()).await.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(acao(&resp).as_deref(), Some("tauri://localhost"));
        let vary: Vec<String> = resp
            .headers()
            .get_all(header::VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase)
            .collect();
        assert!(
            vary.iter().any(|v| v.contains("origin")),
            "Vary must name Origin, got {vary:?}"
        );
    }

    #[tokio::test]
    async fn t7_same_origin_dashboard_reaches_the_handler() {
        let dir = tempfile::tempdir().unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/runner-api/health")
            .header(header::HOST, loopback_host())
            .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
            .body(Body::empty())
            .unwrap();
        let resp = router(dir.path()).await.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    /// An admitted response says it varies on BOTH headers the verdict reads.
    ///
    /// CORS contributes `Vary: Origin`, but with no `Origin` present the
    /// verdict turns on `Sec-Fetch-Site` alone — `t8`'s agent call is admitted
    /// and the same request with `Sec-Fetch-Site: cross-site` is refused. A
    /// cache keyed on `Origin` only would not tell those two apart.
    #[tokio::test]
    async fn an_admitted_response_varies_on_sec_fetch_site() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(dir.path()).await;

        let vary = |resp: &axum::response::Response| -> Vec<String> {
            resp.headers()
                .get_all(header::VARY)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(|v| v.trim().to_ascii_lowercase())
                .collect()
        };

        // No Origin, admitted (the agent path).
        let resp = app.clone().oneshot(token_request(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert!(
            vary(&resp).contains(&"sec-fetch-site".to_string()),
            "admitted response's Vary was {:?}",
            vary(&resp)
        );

        // Same-origin, admitted: Origin is echoed by CORS, and Sec-Fetch-Site
        // is still part of the verdict for the no-Origin sibling of this URL.
        let req = Request::builder()
            .method("GET")
            .uri("/runner-api/health")
            .header(header::HOST, loopback_host())
            .header(header::ORIGIN, format!("http://127.0.0.1:{PORT}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let names = vary(&resp);
        assert!(names.contains(&"origin".to_string()), "Vary was {names:?}");
        assert!(
            names.contains(&"sec-fetch-site".to_string()),
            "Vary was {names:?}"
        );

        // And the refusal path keeps its own explicit pair.
        let resp = app.oneshot(token_request(Some(EVIL))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let names = vary(&resp);
        assert!(names.contains(&"origin".to_string()), "Vary was {names:?}");
        assert!(
            names.contains(&"sec-fetch-site".to_string()),
            "Vary was {names:?}"
        );
    }

    #[tokio::test]
    async fn t8_no_origin_agent_call_reaches_the_handler() {
        let dir = tempfile::tempdir().unwrap();
        let resp = router(dir.path())
            .await
            .oneshot(token_request(None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn t9_kill_switch_disables_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let off = OriginGuardConfig::from_values(Some("0"), None, None, PORT);
        let resp = router_with(dir.path(), off)
            .await
            .oneshot(token_request(Some(EVIL)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn the_spa_fallback_and_unmatched_paths_are_guarded_too() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(dir.path()).await;
        for path in ["/", "/no/such/route"] {
            let req = Request::builder()
                .uri(path)
                .header(header::HOST, loopback_host())
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    async fn send(req: Request<Body>) -> axum::response::Response {
        let dir = tempfile::tempdir().unwrap();
        router(dir.path()).await.oneshot(req).await.unwrap()
    }

    fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            b = b.header(header::HOST, loopback_host());
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    const NAVIGATE: [(&str, &str); 3] = [
        ("sec-fetch-site", "cross-site"),
        ("sec-fetch-mode", "navigate"),
        ("sec-fetch-dest", "document"),
    ];

    #[tokio::test]
    async fn null_origin_is_refused_on_a_proxy_route() {
        let resp = send(token_request(Some("null"))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["code"], CODE_CROSS_ORIGIN_REFUSED);
    }

    #[tokio::test]
    async fn same_site_fetch_without_origin_is_refused_on_the_ui_bridge_proxy() {
        let resp = send(get(
            "/ui-bridge/control/snapshot",
            &[("sec-fetch-site", "same-site")],
        ))
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["code"], CODE_CROSS_ORIGIN_REFUSED);
    }

    #[tokio::test]
    async fn loopback_host_on_the_wrong_port_is_refused() {
        let resp = send(get("/runner-api/health", &[("host", "127.0.0.1:9876")])).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["code"], CODE_HOST_NOT_LOOPBACK);
    }

    #[tokio::test]
    async fn trailing_dot_localhost_host_is_refused() {
        let host = format!("localhost.:{PORT}");
        let resp = send(get("/runner-api/health", &[("host", host.as_str())])).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["code"], CODE_HOST_NOT_LOOPBACK);
    }

    #[tokio::test]
    async fn missing_host_falls_back_to_the_uri_authority() {
        let refused = Request::builder()
            .uri(format!("http://evil.example:{PORT}/runner-api/health"))
            .body(Body::empty())
            .unwrap();
        let resp = send(refused).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(body_json(resp).await["code"], CODE_HOST_NOT_LOOPBACK);

        let admitted = Request::builder()
            .uri(format!("http://127.0.0.1:{PORT}/runner-api/health"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(admitted).await.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn admitted_origin_preflight_gets_that_allow_origin() {
        let req = Request::builder()
            .method("OPTIONS")
            .uri("/ci-runner/start")
            .header(header::HOST, loopback_host())
            .header(header::ORIGIN, "tauri://localhost")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .body(Body::empty())
            .unwrap();
        let resp = send(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(acao(&resp).as_deref(), Some("tauri://localhost"));
    }

    #[tokio::test]
    async fn cross_site_link_navigation_to_the_dashboard_is_admitted() {
        let resp = send(get("/", &NAVIGATE)).await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
        // A client route falls through to the SPA fallback.
        let resp = send(get("/dashboard", &NAVIGATE)).await;
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn cross_site_navigation_to_an_api_route_is_refused() {
        for path in [
            "/runner-api/backup",
            "/ui-bridge/control/snapshot",
            "/runners",
            "/supervisor-bridge/health",
            "/lkg/coverage",
            "/lineage/recent",
            "/web-fleet",
            // unregistered, but under an API segment: never the SPA shell
            "/runner-api",
            "/lkg",
        ] {
            let resp = send(get(path, &NAVIGATE)).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }

    #[tokio::test]
    async fn cross_site_iframe_or_origin_bearing_request_to_the_dashboard_is_refused() {
        let iframe = [
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-dest", "iframe"),
        ];
        assert_eq!(
            send(get("/", &iframe)).await.status(),
            StatusCode::FORBIDDEN
        );
        let fetch = [
            ("sec-fetch-site", "cross-site"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-dest", "empty"),
        ];
        assert_eq!(send(get("/", &fetch)).await.status(), StatusCode::FORBIDDEN);
        let mut with_origin = NAVIGATE.to_vec();
        with_origin.push(("origin", EVIL));
        assert_eq!(
            send(get("/", &with_origin)).await.status(),
            StatusCode::FORBIDDEN
        );
        let post = Request::builder()
            .method("POST")
            .uri("/")
            .header(header::HOST, loopback_host())
            .header("sec-fetch-site", "cross-site")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-dest", "document")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(post).await.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn allowed_origin_env_value_admits_that_origin() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = OriginGuardConfig::from_values(None, Some(EVIL), None, PORT);
        let resp = router_with(dir.path(), cfg)
            .await
            .oneshot(token_request(Some(EVIL)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(acao(&resp).as_deref(), Some(EVIL));
    }

    #[tokio::test]
    async fn health_reports_the_guard_and_shows_recent_to_agents_only() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(dir.path()).await;
        let refused = app
            .clone()
            .oneshot(token_request(Some(EVIL)))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let health = |origin: Option<&'static str>| {
            let mut b = Request::builder()
                .uri("/health")
                .header(header::HOST, loopback_host());
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            b.body(Body::empty()).unwrap()
        };
        let agent = body_json(app.clone().oneshot(health(None)).await.unwrap()).await;
        let block = &agent["originGuard"];
        assert_eq!(block["enabled"], true);
        assert_eq!(block["requesterClass"], "non_browser");
        assert_eq!(block["refusals"]["origin"], 1);
        assert_eq!(block["refusals"]["host"], 0);
        assert_eq!(block["recent"][0]["origin"], EVIL);

        let webview = body_json(
            app.clone()
                .oneshot(health(Some("tauri://localhost")))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(webview["originGuard"]["refusals"]["origin"], 1);
        assert!(webview["originGuard"].get("recent").is_none());
    }

    /// One request as a stub runner saw it: method, path and query, and the
    /// `Origin` it arrived with.
    type Seen = (String, String, Option<String>);

    /// A stub runner on `127.0.0.1:0` that records every request it receives
    /// and answers each with `200 {"ok":true}`.
    async fn stub_runner() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Seen>::new()));
        let record = seen.clone();
        let app = Router::new().fallback(move |req: Request<Body>| {
            let record = record.clone();
            async move {
                let origin = req
                    .headers()
                    .get(header::ORIGIN)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let path = req
                    .uri()
                    .path_and_query()
                    .map(|pq| pq.as_str().to_string())
                    .unwrap_or_default();
                record
                    .lock()
                    .unwrap()
                    .push((req.method().to_string(), path, origin));
                axum::Json(serde_json::json!({ "ok": true }))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, seen)
    }

    /// The production router with every runner proxy pointed at the stub: the
    /// legacy `/ui-bridge/*`, `/runner-api/*` and `/graphql` proxies through
    /// `runner_api_port`, `/runners/primary/ui-bridge/*` through the primary
    /// runner's configured port. Both health caches say the runner responds,
    /// so an admitted request really is forwarded.
    async fn relay_router(root: &std::path::Path, stub_port: u16) -> Router {
        let mut config = config(root);
        for rc in &mut config.runners {
            rc.port = stub_port;
        }
        let mut st = crate::state::SupervisorState::new(config);
        st.runner_api_port = stub_port;
        let st = std::sync::Arc::new(st);
        st.cached_health.write().await.runner_responding = true;
        st.get_runner("primary")
            .await
            .unwrap()
            .cached_health
            .write()
            .await
            .runner_responding = true;
        crate::server::build_router_with_origin_guard(st, OriginGuardConfig::enabled(PORT))
    }

    /// Plan 2026-09-17-ui-bridge-relay-registration-is-unauthenticated,
    /// Phase 4 (supervisor residual). The three proxies strip every header but
    /// `content-type`, so whatever they forward reaches the runner as a
    /// non-browser caller with full local trust. A foreign page must therefore
    /// be stopped HERE, before anything is forwarded: an HTTP registration
    /// overwrite, a forged relay result, and a relay-tab stream takeover each
    /// answer 403 and the runner sees nothing. The same calls with no `Origin`
    /// (agents, scripts) are forwarded exactly once each.
    #[tokio::test]
    async fn relay_routes_refuse_a_foreign_origin_and_forward_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (stub_port, seen) = stub_runner().await;
        let app = relay_router(dir.path(), stub_port).await;

        let relay_request = |method: &str, uri: &str, body: &str, origin: Option<&str>| {
            let mut b = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, loopback_host());
            if !body.is_empty() {
                b = b.header(header::CONTENT_TYPE, "application/json");
            }
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            b.body(Body::from(body.to_string())).unwrap()
        };
        // (method, supervisor URI, body, path + query the runner should see)
        let routes = [
            (
                "POST",
                "/ui-bridge/apps/register",
                r#"{"appId":"victim","transport":"http","baseUrl":"https://evil.example"}"#,
                "/ui-bridge/apps/register",
            ),
            (
                "POST",
                "/runner-api/ui-bridge/commands",
                r#"{"commandId":"c1","success":true}"#,
                "/ui-bridge/commands",
            ),
            (
                "GET",
                "/runners/primary/ui-bridge/commands/stream?tabId=t1",
                "",
                "/ui-bridge/commands/stream?tabId=t1",
            ),
        ];

        for (method, uri, body, _) in routes {
            let resp = app
                .clone()
                .oneshot(relay_request(method, uri, body, Some(EVIL)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
            assert_eq!(acao(&resp), None, "{method} {uri}");
            assert_eq!(
                body_json(resp).await["code"],
                CODE_CROSS_ORIGIN_REFUSED,
                "{method} {uri}"
            );
        }
        assert!(
            seen.lock().unwrap().is_empty(),
            "a refused request reached the runner: {:?}",
            seen.lock().unwrap()
        );

        for (method, uri, body, _) in routes {
            let resp = app
                .clone()
                .oneshot(relay_request(method, uri, body, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
        }
        let expected: Vec<Seen> = routes
            .iter()
            .map(|(method, _, _, runner_path)| (method.to_string(), runner_path.to_string(), None))
            .collect();
        assert_eq!(*seen.lock().unwrap(), expected);
    }

    /// `POST /graphql` is the fourth fixed-port runner proxy, and it too
    /// forwards only `content-type`, so a foreign page's mutation would reach
    /// the runner with full local trust. Refused with nothing forwarded; the
    /// same call with no `Origin` is forwarded once, to the runner's
    /// `/graphql`.
    #[tokio::test]
    async fn graphql_proxy_refuses_a_foreign_origin_and_forwards_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (stub_port, seen) = stub_runner().await;
        let app = relay_router(dir.path(), stub_port).await;

        let graphql_request = |origin: Option<&str>| {
            let mut b = Request::builder()
                .method("POST")
                .uri("/graphql")
                .header(header::HOST, loopback_host())
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            b.body(Body::from(r#"{"query":"{ __typename }"}"#)).unwrap()
        };

        let resp = app
            .clone()
            .oneshot(graphql_request(Some(EVIL)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(acao(&resp), None);
        assert_eq!(body_json(resp).await["code"], CODE_CROSS_ORIGIN_REFUSED);
        assert!(
            seen.lock().unwrap().is_empty(),
            "a refused request reached the runner: {:?}",
            seen.lock().unwrap()
        );

        let resp = app.oneshot(graphql_request(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("POST".to_string(), "/graphql".to_string(), None)]
        );
    }
}
