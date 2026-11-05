//! The one coord-base resolver every supervisor path that reaches coord goes
//! through.
//!
//! [`coord_base`] turns [`AmbientCoord`] into a base URL: `None` under
//! [`AmbientCoord::Ignore`], the given URL under `At`, and under `Read` the
//! ambient resolution — `COORD_HTTP_URL` first, then the active profile's
//! `coord_url` in `~/.qontinui/profiles.json`. The dev-action client (the
//! expectations GET at every mint, `dev_action::prepare_action`, and the
//! attribution watcher's snapshot POST), the fleet budget publish, the
//! serving-watchdog alert, the resource sample, the CI-runner registration
//! token fetch and the lineage proxy all resolve here, so one config field
//! decides whether a process reaches coord at all.
//!
//! There used to be four copies of the profiles parser (`fleet`, `ci_runner`,
//! `lineage`, `dev_action::ingest`); only two honoured `COORD_HTTP_URL` and
//! only one obeyed the switch. A test config set to `Ignore` therefore still
//! published budgets, alerts and samples to whatever coord the box's profile
//! named — production, on the operator box. `config::tests::
//! no_ambient_coord_resolver_outside_coord_base` keeps a fifth copy from
//! appearing.

use std::path::PathBuf;

use serde::Deserialize;

use crate::config::AmbientCoord;

/// The subset of `~/.qontinui/profiles.json` the resolver needs.
#[derive(Debug, Clone, Deserialize)]
struct ProfilesFile {
    active: Option<String>,
    profiles: std::collections::HashMap<String, ProfileSubset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProfileSubset {
    coord_url: Option<String>,
}

fn profiles_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("profiles.json"))
}

/// The coord HTTP base under `access`: `None` for [`AmbientCoord::Ignore`], the
/// given URL for `At`, and for `Read` the ambient resolution, where
/// `COORD_HTTP_URL` wins, then the active profile's `coord_url`
/// (`ws://…/ws` → `http://…`, `wss://` → `https://`). Never ends in `/`.
///
/// A caller that also reads `machine.json` or mints a device JWT must call this
/// FIRST and return on `None`, so `Ignore` reads nothing ambient at all.
pub fn coord_base(access: &AmbientCoord) -> Option<String> {
    coord_base_with(
        access,
        || std::env::var("COORD_HTTP_URL").ok(),
        || std::fs::read(profiles_path()?).ok(),
    )
}

/// [`coord_base`] with its two ambient reads passed in, so a test can stand in
/// for them without setting process-global state. Each is called only under
/// [`AmbientCoord::Read`].
pub(crate) fn coord_base_with(
    access: &AmbientCoord,
    read_env: impl FnOnce() -> Option<String>,
    read_profiles: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<String> {
    match access {
        AmbientCoord::Ignore => None,
        AmbientCoord::At(base) => Some(base.trim_end_matches('/').to_string()),
        AmbientCoord::Read => ambient_coord_base(read_env, read_profiles),
    }
}

fn ambient_coord_base(
    read_env: impl FnOnce() -> Option<String>,
    read_profiles: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<String> {
    if let Some(env_url) = read_env() {
        let trimmed = env_url.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.trim_end_matches('/').to_string());
        }
    }

    let bytes = read_profiles()?;
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
    Some(with_http.trim_end_matches('/').to_string())
}

/// Test-only: a loopback listener that counts the TCP connections it accepts
/// and answers every request `200 {"token":"stub-token","success":true,
/// "data":null}` — a body every coord path here reads as success, and that the
/// runner's mint door reads as "unpaired", so a mint makes exactly one
/// connection and falls through to no bearer.
///
/// It counts CONNECTIONS, not requests: "this path never reached coord" is a
/// claim about the socket, and a request counter would miss a connect that
/// failed before a request was written.
#[cfg(test)]
pub(crate) mod counting_listener {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::serve::ListenerExt;
    use axum::{Json, Router};

    pub(crate) struct CountingListener {
        /// `http://127.0.0.1:<port>`.
        pub base: String,
        connections: Arc<AtomicUsize>,
    }

    impl CountingListener {
        /// Connections accepted so far.
        pub(crate) fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    /// Listen on `127.0.0.1` (not `localhost`, which costs a doomed IPv6 connect
    /// on Windows) and serve for the life of the test's runtime.
    pub(crate) async fn serve() -> CountingListener {
        let (listener, port) = crate::test_ports::held_ephemeral_tokio_listener().await;
        let base = format!("http://127.0.0.1:{port}");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let listener = listener.tap_io(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let app = Router::new().fallback(|| async {
            Json(serde_json::json!({
                "token": "stub-token",
                "success": true,
                "data": null,
            }))
        });
        tokio::spawn(async move { axum::serve(listener, app).await });
        CountingListener { base, connections }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rung of the ambient resolution comes through the two readers, and
    /// the two switches that are not `Read` touch neither.
    #[test]
    fn ignoring_the_ambient_coord_drops_both_rungs() {
        let profiles = br#"{"active":"production","profiles":{"production":{"coord_url":
            "wss://coord.example/ws"}}}"#;
        assert_eq!(
            coord_base_with(
                &AmbientCoord::Read,
                || Some("https://coord.env.example/".to_string()),
                || panic!("the env rung won, so profiles.json must not be read"),
            )
            .as_deref(),
            Some("https://coord.env.example")
        );
        assert_eq!(
            coord_base_with(&AmbientCoord::Read, || None, || Some(profiles.to_vec())).as_deref(),
            Some("https://coord.example")
        );
        assert_eq!(
            coord_base_with(
                &AmbientCoord::Read,
                || Some("   ".to_string()),
                || Some(br#"{"profiles":{"dev":{"coord_url":"ws://127.0.0.1:9870/ws"}}}"#.to_vec()),
            )
            .as_deref(),
            Some("http://127.0.0.1:9870"),
            "a blank override falls through to the `dev` profile"
        );
        assert_eq!(
            coord_base_with(
                &AmbientCoord::Ignore,
                || panic!("Ignore read COORD_HTTP_URL"),
                || panic!("Ignore read profiles.json"),
            ),
            None
        );
        assert_eq!(
            coord_base_with(
                &AmbientCoord::At("http://127.0.0.1:1/".to_string()),
                || panic!("At read COORD_HTTP_URL"),
                || panic!("At read profiles.json"),
            )
            .as_deref(),
            Some("http://127.0.0.1:1")
        );
    }
}
