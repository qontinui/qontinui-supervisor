//! S3-backend degrade guard for the supervisor's OWN cargo spawns.
//!
//! Phase 1.3 of
//! `plans/2026-08-04-landed-infra-fixes-not-in-effect-on-this-machine.md`.
//!
//! ## The gap this closes
//!
//! The 2026-07-23 S3 → local-disk sccache cutover regressed fleet-wide once
//! (`plans/2026-08-04-sccache-daemon-wedge-and-s3-regression.md`): the sccache
//! server latches its storage backend at server START from the env of whichever
//! client spawned it, so a session env still carrying `SCCACHE_BUCKET` re-pins
//! the whole machine to a ~3-4 s/op remote backend and every cargo build on the
//! box joins a client pileup.
//!
//! Two guards shipped for that, and neither covers this process:
//!
//! * `qontinui-claude-config/scripts/cargo-guard.sh` →
//!   `maybe_degrade_on_s3_backend()` only runs when a **shell** invokes it.
//! * `qontinui-claude-config/.claude/hooks/sccache-backend-guard.sh` is a
//!   PreToolUse hook and only intercepts an **agent's** Bash calls.
//!
//! The supervisor spawns cargo IN-PROCESS, so the single most-built path on
//! this machine had no S3 guard at all.
//!
//! ## What is covered, and what is deliberately not
//!
//! **Four compiling cargo spawns, all guarded:**
//!
//! | Call site | Seam |
//! |---|---|
//! | `build_monitor::run_build_inner` (the pool build) | [`guarded_cargo`] |
//! | `build_monitor::build_shim_sidecar` | [`guarded_cargo`] |
//! | `build_monitor::prewarm_single_slot` | [`guarded_cargo`] |
//! | `build_submissions::run_submission` (`/build/submit`, `submit_detached`, `submit_spawn`) | [`degrade_cargo_env_if_s3`] |
//!
//! The fourth is a bare `tokio::process::Command`, not a `GuardedCommand` —
//! which is exactly why the first cut of this module missed it (that pass
//! searched for `GuardedCommand::new("cargo", …)`). It is on the `/build/submit`
//! and **spawn-test** paths and inherits the supervisor's sccache env by
//! design, so leaving it out meant an S3-latched daemon stalled it silently
//! while the pool build beside it degraded and logged.
//!
//! **Two cargo spawns are excluded on purpose, not by oversight:**
//!
//! * `process::manager`'s two spawns are inside `#[cfg(test)]` — they never
//!   run in a shipped supervisor and compile nothing.
//! * `routes::runners`' `cargo clean --target-dir` compiles nothing, so
//!   `RUSTC_WRAPPER` is irrelevant to it; it neither consults sccache nor
//!   benefits from a degrade.
//!
//! ## The predicate: mirror the shipped one, in spirit and in letter
//!
//! * **Probe the server's ACTUAL latched backend** — the `s3,` marker in
//!   `sccache --show-stats` **stdout** — not whether `SCCACHE_BUCKET` happens to
//!   be set. The bucket variable is only a proxy for the real thing: it MISSES a
//!   server latched to S3 whose spawning env has since been cleaned, and it
//!   FALSE-FIRES on a set-but-unused variable. `--show-stats` answers instantly
//!   even while every compile slot is blocked, so it cannot hang the gate.
//!   Stdout only, byte-for-byte the stream `cargo-guard.sh` greps: a bucket name
//!   in an sccache *diagnostic* on stderr must not read as a latched backend.
//! * **Probe only when a listener already holds the pinned sccache port; never
//!   spawn the daemon to probe it.** This is load-bearing, not defensive style:
//!   a bare `sccache --show-stats` with no server running STARTS one, and
//!   starting it from a polluted env is the exact mechanism that re-pins the
//!   machine to S3. The probe would then have caused the regression it exists
//!   to detect. The check-then-probe ordering makes that *unlikely, not
//!   impossible*: sccache self-terminates on its idle timeout (600s by
//!   default), so a daemon that idles out in the window between the two would
//!   still be started by the client. The residual risk is bounded by removing
//!   the S3 selectors from the probe child's env — any server it did manage to
//!   start could not be S3-bound — which also covers sccache's own
//!   version-mismatch respawn.
//! * **Warn and degrade — never fail the build.** A slow build beats a build
//!   that will not run. This matches the shipped `RUSTC_WRAPPER=""` /
//!   `CARGO_GUARD_SCCACHE_CONN_CAP` precedent; no new mechanism.
//! * **Fail open.** An inconclusive or errored probe keeps the cache. The gate
//!   must never withhold caching on a question it could not answer.
//!
//! ## Known limit, stated rather than hidden
//!
//! The degrade works by setting `RUSTC_WRAPPER=""` on the spawned child, which
//! is exactly what `cargo-guard.sh` does — and, like it, only bites when the
//! wrapper came from the ENVIRONMENT. A wrapper configured as
//! `build.rustc-wrapper` in a `.cargo/config.toml` is out of reach of an env
//! override; on this fleet the wrapper is unpinned in every repo config on
//! purpose (`RUSTC_WRAPPER` stays opt-in) so that case does not arise here.

use std::path::Path;
use std::time::Duration;

use tracing::{debug, warn};

use crate::process::guarded_command::GuardedCommand;
use crate::process::port::is_port_listening;

/// sccache's own default server port, used when nothing pins one.
///
/// The fleet pins 4230 instead (Docker/WSL relays hold 4226 on the build
/// hosts), which is why [`resolve_server_port`] reads the env and the cargo
/// config hierarchy before falling back here.
const SCCACHE_DEFAULT_SERVER_PORT: u16 = 4226;

/// Wall-clock cap on the `sccache --show-stats` probe. Mirrors
/// `cargo-guard.sh`'s `timeout 8`: the stats RPC answers in milliseconds, so
/// anything near this is a wedged daemon and we fail open rather than wait.
const STATS_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// The substring that marks an S3-backed cache in `sccache --show-stats`
/// STDOUT (`Cache location  S3, bucket: …`). Byte-for-byte the same assertion
/// `cargo-guard.sh` makes with `grep -qi 's3,'`, over byte-for-byte the same
/// stream (the shell greps command-substitution output, i.e. stdout) — the
/// trailing comma is what keeps it off unrelated prose.
const S3_BACKEND_MARKER: &str = "s3,";

/// What to do with `RUSTC_WRAPPER` for one cargo spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperDecision {
    /// Leave the inherited wrapper alone — no sccache wrapper configured, no
    /// live server to ask, or the server is not S3-bound.
    Keep,
    /// The live server reported an S3 backend: clear the wrapper for THIS
    /// spawn so cargo compiles direct (uncached, slower, but not stuck behind
    /// multi-second S3 round-trips).
    DegradeS3,
}

/// Does `rustc_wrapper` name sccache?
///
/// Mirrors `cargo-guard.sh`'s `case "${RUSTC_WRAPPER:-}" in *sccache*)` — a
/// substring test, because the value is routinely an absolute path
/// (`C:\Users\…\.cargo\bin\sccache.exe`) rather than the bare word.
fn wrapper_is_sccache(rustc_wrapper: Option<&str>) -> bool {
    rustc_wrapper
        .map(|w| w.to_ascii_lowercase().contains("sccache"))
        .unwrap_or(false)
}

/// Does this `sccache --show-stats` output describe an S3-backed server?
fn stats_report_s3_backend(stats: &str) -> bool {
    stats.to_ascii_lowercase().contains(S3_BACKEND_MARKER)
}

/// The whole decision, as a pure function over the three inputs — this is the
/// unit under test.
///
/// `stats` is `None` for "the probe did not run or did not answer", which
/// covers both the deliberate no-listener skip and any error; both fail open.
fn decide_wrapper(
    rustc_wrapper: Option<&str>,
    listener_present: bool,
    stats: Option<&str>,
) -> WrapperDecision {
    if !wrapper_is_sccache(rustc_wrapper) {
        // Nothing to degrade. Also the reason the guard costs nothing on a
        // machine that does not use sccache at all.
        return WrapperDecision::Keep;
    }
    if !listener_present {
        // No server ⇒ nothing has latched a backend yet, and asking would
        // START one. Keep the wrapper; the daemon the BUILD spawns inherits
        // the same env either way.
        return WrapperDecision::Keep;
    }
    match stats {
        Some(s) if stats_report_s3_backend(s) => WrapperDecision::DegradeS3,
        _ => WrapperDecision::Keep,
    }
}

/// Resolve the sccache server port this machine's cargo builds will talk to.
///
/// Precedence mirrors what cargo itself does for the child: an already-exported
/// `SCCACHE_SERVER_PORT` wins, otherwise the nearest `.cargo/config.toml`
/// `[env]` entry at or above `cargo_cwd`, otherwise the user-level
/// `$CARGO_HOME/config.toml`, otherwise sccache's default. Getting this wrong
/// does not misfire the guard — it makes it probe a port nothing is listening
/// on, which fails open.
fn resolve_server_port(cargo_cwd: &Path) -> u16 {
    if let Some(p) = std::env::var("SCCACHE_SERVER_PORT")
        .ok()
        .and_then(|v| parse_port(&v))
    {
        return p;
    }
    for dir in cargo_cwd.ancestors() {
        for name in ["config.toml", "config"] {
            let path = dir.join(".cargo").join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Some(p) = parse_config_server_port(&text) {
                    return p;
                }
            }
        }
    }
    if let Some(home) = cargo_home() {
        for name in ["config.toml", "config"] {
            if let Ok(text) = std::fs::read_to_string(home.join(name)) {
                if let Some(p) = parse_config_server_port(&text) {
                    return p;
                }
            }
        }
    }
    SCCACHE_DEFAULT_SERVER_PORT
}

/// `$CARGO_HOME`, else `~/.cargo`.
fn cargo_home() -> Option<std::path::PathBuf> {
    if let Ok(h) = std::env::var("CARGO_HOME") {
        if !h.trim().is_empty() {
            return Some(std::path::PathBuf::from(h));
        }
    }
    dirs::home_dir().map(|h| h.join(".cargo"))
}

/// Parse a port out of a raw string, rejecting 0 and anything non-numeric.
fn parse_port(raw: &str) -> Option<u16> {
    let p: u16 = raw.trim().trim_matches('"').parse().ok()?;
    if p == 0 {
        None
    } else {
        Some(p)
    }
}

/// Read `[env] SCCACHE_SERVER_PORT` out of a `.cargo/config.toml`.
///
/// Both spellings cargo accepts are honoured: the bare string
/// (`SCCACHE_SERVER_PORT = "4230"`) and the table form
/// (`SCCACHE_SERVER_PORT = { value = "4230", force = false }`) the fleet's
/// configs actually use. A real toml parse rather than a grep — the same
/// reasoning as `config::parse_build_target_dir`.
fn parse_config_server_port(toml_text: &str) -> Option<u16> {
    let value: toml::Value = toml::from_str(toml_text).ok()?;
    let entry = value.get("env")?.get("SCCACHE_SERVER_PORT")?;
    if let Some(s) = entry.as_str() {
        return parse_port(s);
    }
    if let Some(i) = entry.as_integer() {
        return u16::try_from(i).ok().filter(|p| *p != 0);
    }
    let inner = entry.get("value")?;
    if let Some(s) = inner.as_str() {
        return parse_port(s);
    }
    u16::try_from(inner.as_integer()?).ok().filter(|p| *p != 0)
}

/// Ask a LIVE sccache server for its stats. Returns `None` on every failure —
/// binary absent, timeout, non-zero exit — so the caller fails open.
///
/// The child is spawned with the S3 selectors stripped from its env. That does
/// not change what a running server reports (we are reading ITS latched
/// backend, not configuring one); it exists so that if the sccache CLI decides
/// to respawn the daemon behind our back — it does that on a client/server
/// version mismatch — the server it starts cannot be S3-bound.
async fn probe_show_stats(port: u16) -> Option<String> {
    let mut cmd = tokio::process::Command::new("sccache");
    cmd.arg("--show-stats")
        .env("SCCACHE_SERVER_PORT", port.to_string())
        .env_remove("SCCACHE_BUCKET")
        .env_remove("SCCACHE_REGION")
        .env_remove("SCCACHE_ENDPOINT")
        .env_remove("SCCACHE_S3_KEY_PREFIX")
        .env_remove("SCCACHE_S3_USE_SSL")
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // Don't flash a console window out of the supervisor for a probe.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = match tokio::time::timeout(STATS_PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            debug!("sccache guard: --show-stats could not be spawned ({e}); failing open");
            return None;
        }
        Err(_) => {
            debug!(
                "sccache guard: --show-stats did not answer within {}s; failing open",
                STATS_PROBE_TIMEOUT.as_secs()
            );
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            "sccache guard: --show-stats exited {}; failing open",
            output.status
        );
        return None;
    }
    // STDOUT ONLY — the stats table lives there, and that is also the exact
    // stream `cargo-guard.sh` greps (it pipes command-substitution output).
    // Folding stderr in would widen the marker's reach to sccache DIAGNOSTICS:
    // a config-parse warning or a credential error naming an S3 bucket would
    // then read as "the server is S3-bound" and silently uncache every
    // supervisor build until the daemon restarted.
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run the full decision against the live machine.
///
/// `pub` because not every cargo spawn is a [`GuardedCommand`]: the build
/// submission runner (`build_submissions::run_submission`) drives a bare
/// `tokio::process::Command` so it can stream stdout+stderr through its own
/// ring buffers. It applies the same degrade via
/// [`degrade_cargo_env_if_s3`].
pub async fn decide_for_live_server(cargo_cwd: &Path) -> WrapperDecision {
    let wrapper = std::env::var("RUSTC_WRAPPER").ok();
    if !wrapper_is_sccache(wrapper.as_deref()) {
        return WrapperDecision::Keep;
    }
    let port = resolve_server_port(cargo_cwd);
    // ORDER IS THE GUARD: no listener ⇒ no probe, because the probe would
    // start the daemon. Not an absolute guarantee — sccache can idle out
    // (600s default) in the window between this check and the probe — which is
    // why `probe_show_stats` also strips the S3 selectors from the child env.
    //
    // `is_port_listening` binds the port to test it, so this runs a brief
    // bind/close on the sccache port before every guarded cargo spawn — far
    // more often than any previous caller. The socket is bound without
    // SO_REUSEADDR and never listened on, so it leaves no TIME_WAIT state, and
    // a real listener simply makes the bind fail (which IS the answer).
    let listener_present = is_port_listening(port);
    let stats = if listener_present {
        probe_show_stats(port).await
    } else {
        None
    };
    decide_wrapper(wrapper.as_deref(), listener_present, stats.as_deref())
}

/// The one WARN every degrade path emits. Shared so the two seams cannot
/// describe the same condition two different ways in the log.
fn warn_degraded() {
    warn!(
        "sccache server is bound to the S3 backend — the 2026-07-23 local-disk cutover \
         has regressed. Building direct (uncached) for this cargo spawn to avoid the \
         S3-latency pileup. Fix: unset SCCACHE_BUCKET/SCCACHE_REGION in the \
         session/launcher env and restart the daemon from a clean env \
         (plans/2026-08-04-sccache-daemon-wedge-and-s3-regression.md)."
    );
}

/// The env override that expresses the degrade: empty value, matching
/// `cargo-guard.sh`'s `export RUSTC_WRAPPER=""` — cargo treats an empty wrapper
/// as no wrapper.
const DEGRADE_ENV: (&str, &str) = ("RUSTC_WRAPPER", "");

/// Build the [`GuardedCommand`] for a supervisor-spawned `cargo` invocation,
/// degrading `RUSTC_WRAPPER` when the live sccache server is S3-bound.
///
/// **Every `GuardedCommand`-based cargo spawn goes through this** rather than
/// `GuardedCommand::new("cargo", …)` — one shared helper is what keeps the pool
/// build, the shim sidecar build and the prewarm from drifting into three
/// different postures (they had no posture at all before Phase 1.3). The fourth
/// compiling spawn, `build_submissions::run_submission`, is a bare
/// `tokio::process::Command` and uses [`degrade_cargo_env_if_s3`] instead; see
/// the module docs for the full inventory.
///
/// `cargo_cwd` is the directory cargo will run in; it is used only to resolve
/// which `.cargo/config.toml` pins the sccache port. The caller still sets its
/// own `.current_dir(…)`.
pub async fn guarded_cargo(timeout: Duration, cargo_cwd: &Path) -> GuardedCommand {
    let cmd = GuardedCommand::new("cargo", timeout);
    match decide_for_live_server(cargo_cwd).await {
        WrapperDecision::Keep => cmd,
        WrapperDecision::DegradeS3 => {
            warn_degraded();
            cmd.env(DEGRADE_ENV.0, DEGRADE_ENV.1)
        }
    }
}

/// The same degrade for a cargo spawn that is NOT a [`GuardedCommand`].
///
/// `build_submissions::run_submission` (`POST /build/submit`, `submit_detached`
/// from the runner/runners routes, and `submit_spawn` on the **spawn-test**
/// path) drives a bare `tokio::process::Command` so it can pipe stdout and
/// stderr into its own bounded tail buffers. It inherits the supervisor's
/// sccache env by design — which is exactly the polluted-env inheritance this
/// guard exists to break, so it must not be left out. It was, in the first cut
/// of Phase 1.3, because that pass searched for `GuardedCommand::new("cargo",
/// …)` and this call site does not match; an S3-latched daemon would then have
/// stalled `/build/submit` and every `spawn-test {rebuild:true}` silently while
/// the pool build beside it degraded and logged — a split posture that is worse
/// to diagnose than a uniformly slow one.
///
/// Call it after the command is configured and before `spawn()`.
pub async fn degrade_cargo_env_if_s3(cmd: &mut tokio::process::Command, cargo_cwd: &Path) {
    if decide_for_live_server(cargo_cwd).await == WrapperDecision::DegradeS3 {
        warn_degraded();
        cmd.env(DEGRADE_ENV.0, DEGRADE_ENV.1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decide_wrapper, parse_config_server_port, parse_port, stats_report_s3_backend,
        wrapper_is_sccache, WrapperDecision,
    };

    /// Real `sccache --show-stats` output, S3 flavour.
    const S3_STATS: &str = "\
Compile requests                 1234
Cache hits                          12
Cache location                   S3, bucket: Bucket(name=qontinui-sccache, base_url=https://qontinui-sccache.s3.us-east-1.amazonaws.com/)
Version (client)                 0.8.2
";

    /// Real `sccache --show-stats` output, local-disk flavour (the post-cutover
    /// state this fleet is supposed to be in).
    const LOCAL_STATS: &str = "\
Compile requests                 1234
Cache hits                        1100
Cache location                   Local disk: \"D:\\\\sccache-cache\"
Max cache size                   40 GiB
";

    #[test]
    fn s3_marker_is_recognised_case_insensitively() {
        assert!(stats_report_s3_backend(S3_STATS));
        assert!(stats_report_s3_backend("cache location   s3, bucket: x"));
    }

    /// The marker is deliberately broad, which is WHY the probe feeds it stdout
    /// only. An sccache diagnostic naming a bucket — the sort of line that goes
    /// to stderr — matches it, so folding stderr into the search would degrade
    /// a healthy local-disk server and silently uncache every supervisor build
    /// until the daemon restarted.
    #[test]
    fn a_diagnostic_naming_a_bucket_would_match_which_is_why_stderr_is_excluded() {
        assert!(stats_report_s3_backend(
            "error: failed to create S3, bucket qontinui-sccache: credentials expired"
        ));
    }

    /// The post-cutover state must NEVER degrade — a guard that fires on a
    /// healthy local-disk server would silently uncache every build on the box.
    #[test]
    fn local_disk_backend_is_not_reported_as_s3() {
        assert!(!stats_report_s3_backend(LOCAL_STATS));
        assert!(!stats_report_s3_backend(""));
        // The trailing comma is what keeps the marker off unrelated prose —
        // the same assertion cargo-guard.sh makes with `grep -qi 's3,'`.
        assert!(!stats_report_s3_backend(
            "Cache location   Local disk: \"/mnt/s3-backup/sccache\""
        ));
    }

    #[test]
    fn wrapper_detection_handles_bare_names_and_absolute_paths() {
        assert!(wrapper_is_sccache(Some("sccache")));
        assert!(wrapper_is_sccache(Some(
            "C:\\Users\\jspin\\.cargo\\bin\\sccache.exe"
        )));
        assert!(wrapper_is_sccache(Some("/usr/local/bin/SCCACHE")));
        assert!(!wrapper_is_sccache(Some("")));
        assert!(!wrapper_is_sccache(Some("cachepot")));
        assert!(!wrapper_is_sccache(None));
    }

    /// The one case that must degrade: sccache is the wrapper, a server is
    /// live, and it reports S3.
    #[test]
    fn degrades_only_when_a_live_server_reports_s3() {
        assert_eq!(
            decide_wrapper(Some("sccache"), true, Some(S3_STATS)),
            WrapperDecision::DegradeS3
        );
    }

    /// NEVER spawn the daemon to answer the question: with no listener there is
    /// no probe and no degrade, whatever the env says.
    #[test]
    fn no_listener_means_no_probe_and_no_degrade() {
        assert_eq!(
            decide_wrapper(Some("sccache"), false, None),
            WrapperDecision::Keep
        );
        // Even if a caller handed us stale stats, absence of a listener wins:
        // the decision must not be reachable without a live server.
        assert_eq!(
            decide_wrapper(Some("sccache"), false, Some(S3_STATS)),
            WrapperDecision::Keep
        );
    }

    /// Fail open on an inconclusive probe — a guard that withholds caching on a
    /// question it could not answer is worse than the regression it guards.
    #[test]
    fn an_inconclusive_probe_keeps_the_cache() {
        assert_eq!(
            decide_wrapper(Some("sccache"), true, None),
            WrapperDecision::Keep
        );
        assert_eq!(
            decide_wrapper(Some("sccache"), true, Some("")),
            WrapperDecision::Keep
        );
        assert_eq!(
            decide_wrapper(Some("sccache"), true, Some(LOCAL_STATS)),
            WrapperDecision::Keep
        );
    }

    /// No sccache wrapper ⇒ nothing to degrade, regardless of what a server on
    /// the port reports (some other build's daemon, say).
    #[test]
    fn a_non_sccache_wrapper_is_left_alone() {
        assert_eq!(
            decide_wrapper(None, true, Some(S3_STATS)),
            WrapperDecision::Keep
        );
        assert_eq!(
            decide_wrapper(Some("cachepot"), true, Some(S3_STATS)),
            WrapperDecision::Keep
        );
    }

    /// The fleet's real config spelling is the TABLE form; the bare-string form
    /// is what a hand-edited config usually carries. Both must resolve, or the
    /// guard probes 4226 while the machine's server sits on 4230 and the
    /// predicate is silently vacuous.
    #[test]
    fn server_port_is_read_from_both_config_spellings() {
        assert_eq!(
            parse_config_server_port(
                "[env]\nSCCACHE_SERVER_PORT = { value = \"4230\", force = false }\n"
            ),
            Some(4230)
        );
        assert_eq!(
            parse_config_server_port("[env]\nSCCACHE_SERVER_PORT = \"4230\"\n"),
            Some(4230)
        );
        // Integer spelling — cargo rejects it, but reading it here is strictly
        // better than falling back to the wrong port.
        assert_eq!(
            parse_config_server_port("[env]\nSCCACHE_SERVER_PORT = 4230\n"),
            Some(4230)
        );
    }

    #[test]
    fn server_port_absent_or_malformed_reads_as_no_opinion() {
        // The supervisor's real repo config also carries rustflags/target
        // tables; a config with no port entry must read as "no opinion".
        assert_eq!(
            parse_config_server_port("[env]\nOTHER = \"1\"\n[build]\nrustflags = []\n"),
            None
        );
        assert_eq!(parse_config_server_port("not = valid = toml"), None);
        assert_eq!(
            parse_config_server_port("[env]\nSCCACHE_SERVER_PORT = \"nope\"\n"),
            None
        );
        assert_eq!(
            parse_config_server_port("[env]\nSCCACHE_SERVER_PORT = \"0\"\n"),
            None
        );
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("  4230  "), Some(4230));
    }
}
