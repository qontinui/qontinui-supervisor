#![cfg(test)]

use super::*;
use crate::process::instance_config_dir;
use std::time::{Duration, SystemTime};

// Startup-window predicate (plan
// 2026-09-19-supervisor-reaper-purges-an-in-flight-spawn-test).
fn startup_test_runner(port: u16) -> ManagedRunner {
    let mut config = crate::config::RunnerConfig::default_primary();
    config.id = format!("test-{port}");
    config.name = config.id.clone();
    config.port = port;
    ManagedRunner::new_with_log_dir(config, false, None)
}

/// The per-runner skip decision the manager sweep makes (and purge-core
/// shares): exercised directly because the sweep itself is an endless loop.
#[tokio::test]
async fn skip_for_startup_window_decisions() {
    // A port nothing holds and no kernel allocation hands out. A dropped
    // ephemeral port can be handed to a concurrent test's listener, which then
    // reads as "the runner bound its port".
    let dead_port = crate::test_ports::unheld_port();
    let now = chrono::Utc::now();
    let budget = Some(Duration::from_secs(240));

    // Started ~now by an in-flight spawn, port not bound → skip.
    let m = startup_test_runner(dead_port);
    m.runner.write().await.started_at = Some(now);
    m.set_spawn_in_flight(budget);
    assert!(
        skip_for_startup_window(&m, true).await,
        "in-window starting runner"
    );

    // Post-build pre-start placeholder with marker → skip.
    let m = startup_test_runner(dead_port);
    m.set_spawn_in_flight(budget);
    assert!(
        skip_for_startup_window(&m, false).await,
        "marked placeholder"
    );

    // Placeholder with no marker, never started → NOT skipped.
    let m = startup_test_runner(dead_port);
    assert!(
        !skip_for_startup_window(&m, false).await,
        "unmarked placeholder"
    );

    // Started an hour ago, no marker, port dead → NOT skipped (crash arm).
    let m = startup_test_runner(dead_port);
    m.runner.write().await.started_at = Some(now - chrono::Duration::hours(1));
    assert!(!skip_for_startup_window(&m, true).await, "crashed runner");

    // Started 120 s ago: past the floor, inside a 240 s marker → skip only
    // with the marker (isolates the marker from the floor).
    let m = startup_test_runner(dead_port);
    m.runner.write().await.started_at = Some(now - chrono::Duration::seconds(120));
    assert!(
        !skip_for_startup_window(&m, true).await,
        "past floor, no marker"
    );
    m.set_spawn_in_flight(budget);
    assert!(
        skip_for_startup_window(&m, true).await,
        "past floor, inside budget"
    );

    // In window but LISTENING → not skipped; proceeds to the sweep's logic.
    let live = std::net::TcpListener::bind("127.0.0.1:0").expect("bind live");
    let live_port = live.local_addr().expect("addr").port();
    let m = startup_test_runner(live_port);
    m.runner.write().await.started_at = Some(now);
    m.set_spawn_in_flight(budget);
    assert!(!skip_for_startup_window(&m, true).await, "listening runner");
    drop(live);
}

#[test]
fn startup_window_protects_table() {
    let now = chrono::Utc::now();
    let secs = |n: i64| now - chrono::Duration::seconds(n);
    let budget = Some(Duration::from_secs(240));
    type Case = (
        Option<Duration>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
        &'static str,
    );
    // (in_flight, started_at, expected, label)
    let cases: Vec<Case> = vec![
        (budget, None, true, "marker + not started"),
        (
            budget,
            Some(secs(100)),
            true,
            "marker + started within budget",
        ),
        (
            budget,
            Some(secs(300)),
            false,
            "marker + started past budget and floor",
        ),
        (None, Some(secs(10)), true, "no marker within floor"),
        (None, Some(secs(61)), false, "no marker past floor"),
        (
            budget,
            Some(now + chrono::Duration::seconds(3600)),
            true,
            "future start, marker",
        ),
        (
            None,
            Some(now + chrono::Duration::seconds(3600)),
            true,
            "future start, no marker",
        ),
        (None, None, false, "no marker, never started"),
        (
            Some(Duration::from_secs(5)),
            Some(secs(30)),
            true,
            "marker budget below floor, started within floor",
        ),
        (
            Some(Duration::from_secs(5)),
            Some(secs(61)),
            false,
            "marker budget below floor, started past floor",
        ),
    ];
    for (in_flight, started_at, expected, label) in cases {
        assert_eq!(
            startup_window_protects(in_flight, started_at, now),
            expected,
            "{label}"
        );
    }
}

// Temp-runner max-age bound (plan
// 2026-08-10-temp-runner-session-restore-isolation, Phase 5).

fn all_kinds() -> Vec<qontinui_types::wire::runner_kind::RunnerKind> {
    use qontinui_types::wire::runner_kind::RunnerKind;
    vec![
        RunnerKind::Primary,
        RunnerKind::Named {
            name: "named-9880".to_string(),
        },
        RunnerKind::Temp {
            id: "test-abc123".to_string(),
        },
        RunnerKind::External,
    ]
}

/// The bound is an `is_temp()` ALLOWLIST. A user-owned runner is never
/// age-reaped, no matter how old it is or how tight the bound — the
/// `!is_primary()` denylist spelling of this rule is what took the
/// operator's primary down on 2026-07-27.
#[test]
fn max_age_bound_applies_to_temp_runners_only() {
    let ancient = Duration::from_secs(365 * 24 * 60 * 60);
    let tight = Some(Duration::from_secs(1));
    for kind in all_kinds() {
        let reaped = exceeds_temp_runner_max_age(&kind, ancient, tight);
        assert_eq!(
            reaped,
            kind.is_temp(),
            "{kind:?}: only RunnerKind::Temp may be reaped for age — primary, named \
                 and external runners are user-owned"
        );
    }
}

/// A temp runner inside the bound survives; one past it does not.
#[test]
fn max_age_bound_fires_only_past_the_bound() {
    let kind = qontinui_types::wire::runner_kind::RunnerKind::Temp {
        id: "test-abc123".to_string(),
    };
    let max = Duration::from_secs(3600);
    assert!(!exceeds_temp_runner_max_age(
        &kind,
        Duration::from_secs(0),
        Some(max)
    ));
    assert!(!exceeds_temp_runner_max_age(
        &kind,
        Duration::from_secs(3599),
        Some(max)
    ));
    assert!(exceeds_temp_runner_max_age(
        &kind,
        Duration::from_secs(3600),
        Some(max)
    ));
    assert!(exceeds_temp_runner_max_age(
        &kind,
        Duration::from_secs(100_000),
        Some(max)
    ));
}

/// The age clock is `started_at`, NOT time-since-placeholder. This is the
/// cold-build case: `spawn_test` reserved the placeholder 50 minutes ago,
/// the child bound its port 90 seconds ago. The runner is 90s old, not
/// 3000s — reading it the other way reaps a brand-new runner on the very
/// next sweep, every retry.
#[test]
fn age_is_measured_from_process_start_not_from_the_spawn_request() {
    let now = chrono::Utc::now();
    let started = now - chrono::Duration::seconds(90);
    let (age, basis) = resolve_temp_runner_age(Some(started), now, Duration::from_secs(3000));
    assert_eq!(age.as_secs(), 90);
    assert!(basis.contains("process start"), "basis was {basis:?}");

    // And that age is INSIDE a 1h bound, where the placeholder clock would
    // have blown straight past it.
    let kind = qontinui_types::wire::runner_kind::RunnerKind::Temp {
        id: "test-abc123".to_string(),
    };
    let bound = Some(Duration::from_secs(3600));
    assert!(!exceeds_temp_runner_max_age(&kind, age, bound));
    assert!(exceeds_temp_runner_max_age(
        &kind,
        Duration::from_secs(3000 * 2),
        bound
    ));
}

/// No `started_at` → fall back to time-since-first-seen rather than
/// reporting zero age (which would make the bound unreachable).
#[test]
fn age_falls_back_to_first_seen_without_started_at() {
    let now = chrono::Utc::now();
    let (age, basis) = resolve_temp_runner_age(None, now, Duration::from_secs(4242));
    assert_eq!(age.as_secs(), 4242);
    assert!(basis.contains("first seen"), "basis was {basis:?}");
}

/// A `started_at` in the FUTURE (clock skew / NTP step) must not panic or
/// wrap into a huge age that reaps a live runner — fall back and say so.
#[test]
fn age_falls_back_when_started_at_is_in_the_future() {
    let now = chrono::Utc::now();
    let started = now + chrono::Duration::seconds(600);
    let (age, basis) = resolve_temp_runner_age(Some(started), now, Duration::from_secs(11));
    assert_eq!(age.as_secs(), 11);
    assert!(basis.contains("future"), "basis was {basis:?}");
}

/// `None` is the off-switch: nothing is ever reaped for age, including a
/// temp runner that has been alive for a year.
#[test]
fn max_age_bound_disabled_never_reaps() {
    let ancient = Duration::from_secs(365 * 24 * 60 * 60);
    for kind in all_kinds() {
        assert!(
            !exceeds_temp_runner_max_age(&kind, ancient, None),
            "{kind:?}: a disabled bound must never reap anything"
        );
    }
}

// QONTINUI_API_URL child-env policy (plan 2026-07-08).

#[test]
fn child_api_url_primary_no_env_is_unset() {
    // Primary with no explicit supervisor QONTINUI_API_URL → leave it unset
    // so the runner resolves via its persisted paired backend.
    assert_eq!(resolve_child_api_url(None, true), None);
}

#[test]
fn child_api_url_secondary_no_env_pins_local() {
    // Temp/named with no explicit env → pinned to the local backend.
    assert_eq!(
        resolve_child_api_url(None, false),
        Some("http://127.0.0.1:8000".to_string())
    );
}

#[test]
fn child_api_url_explicit_env_forwarded_to_all() {
    // An explicit supervisor QONTINUI_API_URL wins for every runner kind.
    let explicit = || Some("https://api.qontinui.io".to_string());
    assert_eq!(resolve_child_api_url(explicit(), true), explicit());
    assert_eq!(resolve_child_api_url(explicit(), false), explicit());
}

// The instance root, asserted at the site that actually decides where the
// child reads and writes.
//
// `start_exe_mode_for_runner` applies `apply_non_primary_instance_env` (and
// through it `apply_instance_root_env`) to every non-primary child, and
// `spawn-test` only ever mints non-primary (`RunnerKind::Temp`) runners — so
// the vars this sets ARE where a spawn-test runner keeps its state and loads
// its pairing and token cache from. `routes::runners` writes the
// `paired_profile_id` snapshot into `instance_secure_storage_dir` for the same
// id; the cross-check is
// `routes::runners::tests::paired_state_writer_targets_the_childs_secure_storage_dir`.
//
// The assertion is made over a REAL `Command`'s env map rather than over a
// re-derivation of the helper's body: re-inlining a divergent path at the
// spawn's `cmd.env(...)` site — the failure this test exists to catch —
// fails here.
/// A tempdir's per-user dirs ([`crate::config::AmbientUserDirs::At`]): a test
/// that applies the non-primary env block creates its instance root there, never
/// under the operator's config dir. Hold the `TempDir` for the test's lifetime.
fn temp_user_dirs() -> (crate::config::AmbientUserDirs, tempfile::TempDir) {
    let root = tempfile::TempDir::new().expect("tempdir");
    (
        crate::config::AmbientUserDirs::At(root.path().join("user-dirs")),
        root,
    )
}

#[test]
fn apply_instance_root_env_exports_the_root_and_confines_every_location() {
    let (dirs, _user_dirs) = temp_user_dirs();
    let id = "test-apply-instance-root-env".to_string();
    let expected = instance_config_dir(&dirs, &id).expect("At resolves an instance root");

    let mut cmd = Command::new("cargo");
    let root = super::apply_instance_root_env(
        &mut cmd,
        &dirs,
        &id,
        None,
        crate::process::instance_root::DeviceIdentity::Harness,
    )
    .expect("must resolve + create");
    assert_eq!(root.path(), expected.as_path());

    let envs = command_envs(&cmd);
    let want_root = expected.to_string_lossy().into_owned();
    assert_eq!(
        envs.get("QONTINUI_INSTANCE_ROOT")
            .cloned()
            .flatten()
            .as_deref(),
        Some(want_root.as_str()),
        "the child must be told its instance root = instance_config_dir({id})"
    );
    // Secure storage and config ARE the root, so an older binary that ignores
    // the root still reads its pairing where the writer put it.
    for key in ["QONTINUI_CONFIG_DIR", "QONTINUI_SECURE_STORAGE_DIR"] {
        assert_eq!(
            envs.get(key).cloned().flatten().as_deref(),
            Some(want_root.as_str()),
            "{key} must be exported as the instance root {expected:?}; a spawn that inlines a \
             different path leaves the runner unpaired"
        );
    }
    assert_confined_to_root(&envs, &expected);
    assert_eq!(
        envs.get("QONTINUI_DISABLE_KEYCHAIN")
            .cloned()
            .flatten()
            .as_deref(),
        Some("1"),
        "the OS keychain is keyed on a service name every runner on the box shares"
    );
    for (key, dir) in root.confined_exports() {
        assert!(dir.is_dir(), "{key}={dir:?} must have been created");
    }
}

/// EVERY root-confined key is decided by the command — set to a path inside
/// `root`, or explicitly removed — the contract a root-honouring runner
/// enforces with exit 78 (qontinui-runner `instance_env::validate_instance_root`).
///
/// A key the command leaves untouched is a failure even though it is absent
/// from the overrides: the child would INHERIT the supervisor's value for it,
/// and nothing then guarantees that value lies inside the root.
fn assert_confined_to_root(
    envs: &std::collections::HashMap<String, Option<String>>,
    root: &std::path::Path,
) {
    for key in crate::process::instance_root::ROOT_CONFINED_ENV_KEYS {
        match envs.get(key) {
            Some(Some(value)) => assert!(
                crate::process::instance_root::is_inside_root(std::path::Path::new(value), root),
                "{key}={value:?} lies outside the instance root {root:?} — a runner under the \
                 root refuses to start (exit 78)"
            ),
            Some(None) => {} // explicitly removed: the child derives it under the root
            None => panic!(
                "{key} is neither set inside the root nor removed — the child inherits the \
                 supervisor's value, which may lie outside {root:?}"
            ),
        }
    }
}

/// The whole non-primary block — the root, every confined location, the
/// provisioned home — and the port refusal.
#[test]
fn non_primary_env_block_carries_the_root_and_provisions_the_home() {
    let (dirs, _user_dirs) = temp_user_dirs();
    let config = temp_config("test-rootblock", 9877);
    let expected_root =
        instance_config_dir(&dirs, &config.id).expect("At resolves an instance root");
    let harness_home = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(
        harness_home.path().join("machine.json"),
        br#"{"device_id":"d"}"#,
    )
    .unwrap();
    std::fs::write(harness_home.path().join("coord-device-jwt"), b"secret").unwrap();

    let mut cmd = Command::new("cargo");
    let root = super::apply_non_primary_instance_env_with(
        &mut cmd,
        &dirs,
        &config,
        9876,
        Some(harness_home.path()),
    )
    .expect("must resolve + create");
    let envs = command_envs(&cmd);
    assert_confined_to_root(&envs, &expected_root);
    for key in [
        "QONTINUI_HOME",
        "QONTINUI_RUNNER_LOG_DIR",
        "QONTINUI_PANIC_LOG_DIR",
        "QONTINUI_EMBEDDED_PG_DIR",
        "WEBVIEW2_USER_DATA_FOLDER",
    ] {
        assert!(
            envs.get(key).cloned().flatten().is_some(),
            "{key} must be exported explicitly so an older binary is isolated too"
        );
    }
    assert!(
        root.home_dir().join("machine.json").is_file(),
        "the harness's device identity is provisioned into <root>/home"
    );
    assert!(
        !root.home_dir().join("coord-device-jwt").exists(),
        "a credential the subject mints for itself is never copied"
    );
}

#[test]
fn non_primary_env_block_refuses_the_primary_port() {
    let (dirs, _user_dirs) = temp_user_dirs();
    for (port, primary) in [(9876u16, 9876u16), (9876, 9990), (9990, 9990), (0, 9876)] {
        let config = temp_config("test-refuse-port", port);
        let mut cmd = Command::new("cargo");
        let err =
            super::apply_non_primary_instance_env_with(&mut cmd, &dirs, &config, primary, None)
                .expect_err("a non-primary runner on a refused port must not spawn");
        assert!(
            err.to_string().contains("may not use"),
            "port {port} (primary {primary}): {err}"
        );
        assert!(
            command_envs(&cmd).is_empty(),
            "a refused spawn must not have half-applied the block"
        );
    }
}

/// Read a `Command`'s env overrides back as a plain map.
fn command_envs(cmd: &Command) -> std::collections::HashMap<String, Option<String>> {
    cmd.as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

/// A `RunnerConfig` shaped exactly like the one `spawn_test` builds — the
/// instance name comes from the same helper the spawn site uses, so this
/// test tracks the real minting policy instead of a copy of it.
fn temp_config(id: &str, port: u16) -> crate::config::RunnerConfig {
    crate::config::RunnerConfig {
        id: id.to_string(),
        name: crate::process::temp_runner_instance_name(id),
        port,
        kind: qontinui_types::wire::runner_kind::RunnerKind::Temp { id: id.to_string() },
        protected: true,
        server_mode: false,
        restate_ingress_port: None,
        restate_admin_port: None,
        restate_service_port: None,
        external_restate_admin_url: None,
        external_restate_ingress_url: None,
        extra_env: Default::default(),
        fresh_device_identity: false,
    }
}

/// The non-primary env block, asserted **as a whole** against a real
/// `Command` — `QONTINUI_INSTANCE_NAME`, `QONTINUI_PRIMARY_PORT` and
/// `WEBVIEW2_USER_DATA_FOLDER` together, because it is the combination
/// that isolates a secondary's state from the primary's and from other
/// secondaries'. Sibling of
/// `apply_instance_root_env_exports_the_root_and_confines_every_location`, which
/// covers the two config-dir vars.
///
/// **The load-bearing assertion is that `QONTINUI_INSTANCE_NAME` is unique
/// per SPAWN, not per port.** Both configs here take the SAME port —
/// temp ports are recycled inside the 23-slot range 9877-9899, so a
/// second spawn routinely lands on a port a previous temp just released.
/// While the name was `format!("test-{port}")`, those two spawns shared
/// one `instance-test-<port>` app-data tree, and the second runner booted
/// on the first's live `terminal-sessions.json` — 283 inherited PTYs
/// observed 2026-08-08 (plan
/// `2026-08-10-temp-runner-session-restore-isolation`). Nothing failed
/// when that scheme changed, because no test asserted it; this is that
/// test.
#[test]
fn non_primary_env_block_keys_instance_name_per_spawn_not_per_port() {
    // Two sequential spawns that RECYCLE the same port.
    const PORT: u16 = 9877;
    const PRIMARY_PORT: u16 = 9876;
    let (dirs, _user_dirs) = temp_user_dirs();
    let first = temp_config("test-envblock-a", PORT);
    let second = temp_config("test-envblock-b", PORT);

    let mut cmd_a = Command::new("cargo");
    let mut cmd_b = Command::new("cargo");
    super::apply_non_primary_instance_env_with(&mut cmd_a, &dirs, &first, PRIMARY_PORT, None)
        .expect("must resolve + create");
    super::apply_non_primary_instance_env_with(&mut cmd_b, &dirs, &second, PRIMARY_PORT, None)
        .expect("must resolve + create");
    let envs_a = command_envs(&cmd_a);
    let envs_b = command_envs(&cmd_b);

    let name_a = envs_a
        .get("QONTINUI_INSTANCE_NAME")
        .cloned()
        .flatten()
        .expect("a secondary must always be given QONTINUI_INSTANCE_NAME");
    let name_b = envs_b
        .get("QONTINUI_INSTANCE_NAME")
        .cloned()
        .flatten()
        .expect("a secondary must always be given QONTINUI_INSTANCE_NAME");

    assert_ne!(
        name_a, name_b,
        "two temp runners spawned on the SAME recycled port {PORT} must get \
             DIFFERENT QONTINUI_INSTANCE_NAME values — the runner roots its whole \
             instance-<name> app-data tree (terminal-sessions.json included) on this \
             string, so a port-derived name makes the second spawn inherit the \
             first's live terminal registry"
    );
    for (name, config) in [(&name_a, &first), (&name_b, &second)] {
        assert_eq!(
            name, &config.id,
            "the instance name must BE the per-spawn runner id (no second uuid, \
                 nothing port-derived) so it stays in lockstep with the id-keyed \
                 instance_config_dir / WebView2 / QONTINUI_RUNNER_ID resources"
        );
        assert!(
            !name.contains(&PORT.to_string()),
            "instance name {name:?} must carry no trace of the recycled port"
        );
    }

    for (envs, config) in [(&envs_a, &first), (&envs_b, &second)] {
        assert_eq!(
            envs.get("QONTINUI_PRIMARY_PORT").cloned().flatten(),
            Some(PRIMARY_PORT.to_string()),
            "a secondary needs BOTH the instance name and the primary port to \
                 classify itself as secondary; unset makes it behave as a primary"
        );

        let want = instance_config_dir(&dirs, &config.id)
            .expect("At resolves an instance root")
            .join("webview")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            envs.get("WEBVIEW2_USER_DATA_FOLDER").cloned().flatten(),
            Some(want),
            "the WebView data dir must live in the per-spawn instance root so two \
             temps never share localStorage/IndexedDB/cookies"
        );
    }

    assert_ne!(
        envs_a.get("WEBVIEW2_USER_DATA_FOLDER"),
        envs_b.get("WEBVIEW2_USER_DATA_FOLDER"),
        "same-port spawns must not share a WebView data dir"
    );
    assert_ne!(
        envs_a.get("QONTINUI_INSTANCE_ROOT"),
        envs_b.get("QONTINUI_INSTANCE_ROOT"),
        "same-port spawns must not share an instance root"
    );
}

/// A slot freshly built 5 minutes after the running copy is "stale".
#[test]
fn stale_binary_detection_slot_much_newer() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slot = running + Duration::from_secs(300); // +5 min
    let out = compute_stale_binary(Some(running), Some((0, slot)))
        .expect("5-minute gap should be surfaced");
    assert_eq!(out.slot_id, 0);
    assert_eq!(out.age_delta_secs, 300);
    assert_eq!(out.running_mtime_ms, 1_700_000_000 * 1000);
    assert_eq!(out.slot_mtime_ms, (1_700_000_000 + 300) * 1000);
}

/// A slot 10 seconds newer is within jitter — no badge.
#[test]
fn stale_binary_detection_within_threshold() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slot = running + Duration::from_secs(10);
    assert!(compute_stale_binary(Some(running), Some((0, slot))).is_none());
}

/// A slot exactly at the threshold (30s) does not trigger — strict `>`.
#[test]
fn stale_binary_detection_at_exact_threshold() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slot = running + Duration::from_secs(STALE_BINARY_THRESHOLD_SECS as u64);
    assert!(
        compute_stale_binary(Some(running), Some((0, slot))).is_none(),
        "delta == threshold must not surface a stale_binary entry"
    );
}

/// One second over the threshold DOES trigger.
#[test]
fn stale_binary_detection_just_over_threshold() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slot = running + Duration::from_secs(STALE_BINARY_THRESHOLD_SECS as u64 + 1);
    let out = compute_stale_binary(Some(running), Some((0, slot)))
        .expect("threshold + 1s should surface a stale_binary entry");
    assert_eq!(out.age_delta_secs, STALE_BINARY_THRESHOLD_SECS + 1);
}

/// A slot older than the running copy means the running copy is the
/// freshest binary on disk — normal state, no badge.
#[test]
fn stale_binary_detection_running_newer() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slot = running - Duration::from_secs(120);
    assert!(compute_stale_binary(Some(running), Some((0, slot))).is_none());
}

/// Identical mtimes — no divergence, no badge.
#[test]
fn stale_binary_detection_equal() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert!(compute_stale_binary(Some(running), Some((1, running))).is_none());
}

/// Missing running-copy mtime (first start, fs stat failed, etc.) — the
/// feature silently skips.
#[test]
fn stale_binary_detection_missing_running_mtime() {
    let slot = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert!(compute_stale_binary(None, Some((0, slot))).is_none());
}

/// No slot has ever produced a binary — nothing to compare against.
#[test]
fn stale_binary_detection_no_slot_binary() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert!(compute_stale_binary(Some(running), None).is_none());
}

/// Slot id is preserved through the struct (not always 0).
#[test]
fn stale_binary_detection_preserves_slot_id() {
    let running = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slot = running + Duration::from_secs(600);
    let out = compute_stale_binary(Some(running), Some((2, slot))).expect("stale");
    assert_eq!(out.slot_id, 2);
}

// =========================================================================
// First-healthy watchdog decision tests
// =========================================================================

/// Process gone — exit quietly regardless of other flags.
#[test]
fn first_healthy_abandon_when_untracked() {
    assert_eq!(
        decide_first_healthy(false, false, false),
        FirstHealthyDecision::Abandon
    );
    // Even if the port is "responding" and the deadline passed, an
    // untracked PID is not ours to act on.
    assert_eq!(
        decide_first_healthy(false, true, true),
        FirstHealthyDecision::Abandon
    );
}

/// HTTP /health responded — healthy outcome, even if the deadline just
/// elapsed on the same tick.
#[test]
fn first_healthy_healthy_wins_over_kill() {
    assert_eq!(
        decide_first_healthy(true, true, false),
        FirstHealthyDecision::Healthy
    );
    // Edge case the priority rule exists for: responsive AND past
    // deadline on the same poll. We do NOT kill — the runner made it.
    assert_eq!(
        decide_first_healthy(true, true, true),
        FirstHealthyDecision::Healthy
    );
}

/// Tracked, not responding, deadline passed — kill path.
#[test]
fn first_healthy_kill_when_deadline_passed_and_unresponsive() {
    assert_eq!(
        decide_first_healthy(true, false, true),
        FirstHealthyDecision::Kill
    );
}

/// Tracked, not responding, still within budget — keep waiting.
#[test]
fn first_healthy_wait_while_within_budget() {
    assert_eq!(
        decide_first_healthy(true, false, false),
        FirstHealthyDecision::Wait
    );
}

// =========================================================================
// Stop-reap escalation decision tests (Item 2: confirmed port-free stop)
// =========================================================================

/// Port free on the very first check — no escalation, stop confirmed.
#[test]
fn stop_reap_confirmed_when_port_free() {
    assert_eq!(decide_stop_reap(0, false), StopReapOutcome::Confirmed);
    // A free port short-circuits at every attempt index, even after
    // escalations have run.
    assert_eq!(decide_stop_reap(1, false), StopReapOutcome::Confirmed);
    assert_eq!(decide_stop_reap(2, false), StopReapOutcome::Confirmed);
}

/// First attempt with the port still held escalates to a tree-kill
/// (kills the runner's child processes a plain `/F` PID kill leaves alive).
#[test]
fn stop_reap_first_held_escalates_to_tree() {
    assert_eq!(decide_stop_reap(0, true), StopReapOutcome::EscalateTree);
}

/// Second attempt still held escalates to a blind kill-by-port.
#[test]
fn stop_reap_second_held_escalates_to_port() {
    assert_eq!(decide_stop_reap(1, true), StopReapOutcome::EscalatePort);
}

/// Both kill escalations exhausted — one bounded backoff retry before
/// giving up (covers a kill that landed but whose socket teardown was
/// slow to be reflected by the OS).
#[test]
fn stop_reap_third_held_retries_after_backoff() {
    assert_eq!(
        decide_stop_reap(2, true),
        StopReapOutcome::RetryAfterBackoff
    );
}

/// Every escalation (and the backoff retry) exhausted and still held —
/// stop must NOT be confirmed.
#[test]
fn stop_reap_exhausted_is_still_held() {
    assert_eq!(decide_stop_reap(3, true), StopReapOutcome::StillHeld);
    // Any attempt beyond the ladder also reports StillHeld (never loops
    // back to a kill it already tried).
    assert_eq!(decide_stop_reap(4, true), StopReapOutcome::StillHeld);
    assert_eq!(decide_stop_reap(99, true), StopReapOutcome::StillHeld);
}

/// The full escalation ladder visits each rung exactly once before
/// giving up — guards against an infinite reap loop on a wedged survivor.
#[test]
fn stop_reap_ladder_terminates() {
    // Simulate a survivor that never releases the port: the loop must
    // walk Tree → Port → RetryAfterBackoff → StillHeld and stop.
    assert_eq!(decide_stop_reap(0, true), StopReapOutcome::EscalateTree);
    assert_eq!(decide_stop_reap(1, true), StopReapOutcome::EscalatePort);
    assert_eq!(
        decide_stop_reap(2, true),
        StopReapOutcome::RetryAfterBackoff
    );
    assert_eq!(decide_stop_reap(3, true), StopReapOutcome::StillHeld);
}

// =========================================================================
// Crash-only ambient watchdog decision tests (Phase 1,
// plans/2026-07-03-primary-runner-crash-resilience.md)
// =========================================================================

/// Baseline crash: supervisor-spawned child, no stop intent, non-zero
/// exit, everything armed, empty window → restart attempt 1 after 5s.
#[test]
fn crash_restart_first_crash_restarts_after_5s() {
    assert_eq!(
        decide_crash_restart(false, false, true, true, false, 0),
        CrashRestartDecision::Restart {
            attempt: 1,
            delay_secs: 5
        }
    );
}

/// Exponential backoff ladder: 5s → 30s → 120s across the rolling window.
#[test]
fn crash_restart_backoff_is_exponential() {
    assert_eq!(
        decide_crash_restart(false, false, true, true, false, 1),
        CrashRestartDecision::Restart {
            attempt: 2,
            delay_secs: 30
        }
    );
    assert_eq!(
        decide_crash_restart(false, false, true, true, false, 2),
        CrashRestartDecision::Restart {
            attempt: 3,
            delay_secs: 120
        }
    );
}

/// Rolling-window budget exhausted (3 restarts already) → disarm, loudly.
#[test]
fn crash_restart_window_exhausted_disarms() {
    assert_eq!(
        decide_crash_restart(false, false, true, true, false, 3),
        CrashRestartDecision::Disarm
    );
    // Even further past the budget it stays Disarm, never Restart.
    assert_eq!(
        decide_crash_restart(false, false, true, true, false, 7),
        CrashRestartDecision::Disarm
    );
}

/// Operator stop intent wins over everything else armed — never restart
/// a runner the operator deliberately stopped.
#[test]
fn crash_restart_operator_stop_never_restarts() {
    assert_eq!(
        decide_crash_restart(true, false, true, true, false, 0),
        CrashRestartDecision::SkipOperatorStop
    );
    // Even a non-clean exit after a stop request (taskkill path) skips.
    assert_eq!(
        decide_crash_restart(true, true, true, true, false, 0),
        CrashRestartDecision::SkipOperatorStop
    );
}

/// A clean exit (code 0) is a deliberate shutdown (window close,
/// internal exit) — crash-only means we never restart it.
#[test]
fn crash_restart_clean_exit_skips() {
    assert_eq!(
        decide_crash_restart(false, true, true, true, false, 0),
        CrashRestartDecision::SkipCleanExit
    );
}

/// Global arm off (no --watchdog, or the env kill-switch) → skip.
#[test]
fn crash_restart_global_arm_gates() {
    assert_eq!(
        decide_crash_restart(false, false, false, true, false, 0),
        CrashRestartDecision::SkipNotArmed
    );
}

/// Per-runner enabled=false (default for named/temp/external) → skip.
#[test]
fn crash_restart_per_runner_enabled_gates() {
    assert_eq!(
        decide_crash_restart(false, false, true, false, false, 0),
        CrashRestartDecision::SkipDisabled
    );
}

/// Once disarmed (`disabled_reason` set), restarts stay off until an
/// operator resets — even with a fresh (pruned) window.
#[test]
fn crash_restart_disarmed_latch_holds() {
    assert_eq!(
        decide_crash_restart(false, false, true, true, true, 0),
        CrashRestartDecision::SkipDisarmed
    );
}

/// The two CRASH WARN-worthy skip variants (`SkipNotArmed` /
/// `SkipDisarmed`, promoted to `warn!` + persisted emit in
/// `maybe_crash_restart`) are ONLY reachable for a genuine crash — no
/// operator stop and an unclean exit. The benign skips for the same crash
/// inputs (clean exit, operator stop) classify away from the WARN set, so the CRASH promotion never fires on
/// a benign exit.
///
/// Note `SkipCleanExit` carries its OWN, separate warn arm for the
/// unrequested-clean-exit case (supervisor-spawned, code 0, no stop
/// requested — the 2026-08-06 self-exit). That arm is deliberately not part
/// of this crash set: it reports rather than restarts, and it is gated on
/// `!stop_requested_at_exit` in `maybe_crash_restart`, not here.
#[test]
fn crash_restart_warn_worthy_skips_are_genuine_crash_only() {
    // Genuine crash, global arm off → WARN-worthy SkipNotArmed.
    let not_armed = decide_crash_restart(false, false, false, true, false, 0);
    assert_eq!(not_armed, CrashRestartDecision::SkipNotArmed);
    assert!(matches!(
        not_armed,
        CrashRestartDecision::SkipNotArmed | CrashRestartDecision::SkipDisarmed
    ));
    // Genuine crash, armed+enabled but disarmed latch → WARN-worthy SkipDisarmed.
    let disarmed = decide_crash_restart(false, false, true, true, true, 0);
    assert_eq!(disarmed, CrashRestartDecision::SkipDisarmed);
    // Benign skips for the SAME arm state must NOT be in the WARN-worthy set.
    for benign in [
        decide_crash_restart(false, true, false, true, false, 0), // clean exit
        decide_crash_restart(true, false, false, true, false, 0), // operator stop
    ] {
        assert!(
            !matches!(
                benign,
                CrashRestartDecision::SkipNotArmed | CrashRestartDecision::SkipDisarmed
            ),
            "benign skip {benign:?} must not be WARN-worthy"
        );
    }
}

// =========================================================================
// Serving watchdog decision table
// =========================================================================

use crate::state::RunnerLiveness;

fn primary_kind() -> qontinui_types::wire::runner_kind::RunnerKind {
    qontinui_types::wire::runner_kind::RunnerKind::Primary
}

fn temp_kind() -> qontinui_types::wire::runner_kind::RunnerKind {
    qontinui_types::wire::runner_kind::RunnerKind::Temp {
        id: "test-1".to_string(),
    }
}

/// The happy path: a wedge past the threshold, armed, nothing else in the
/// way. This is the decision that ends the outage class the plan exists for.
#[test]
fn serving_restart_fires_on_a_wedge_past_the_threshold() {
    let wedged = RunnerLiveness::UnresponsiveSince(chrono::Utc::now());
    assert_eq!(
        decide_serving_restart(
            wedged,
            301,
            300,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::Restart { attempt: 1 }
    );
    // The attempt number is 1-based within the rolling window.
    assert_eq!(
        decide_serving_restart(
            wedged,
            301,
            300,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            2
        ),
        ServingRestartDecision::Restart { attempt: 3 }
    );
}

/// Silence counts from the later of the last answer and this spawn's start. The
/// stamp survives a restart, so it can be the previous boot's.
#[test]
fn serving_silence_counts_from_the_later_of_the_answer_and_the_spawn() {
    use crate::state::RunnerLiveness::{Responding, Unknown, UnresponsiveSince};
    let now = chrono::Utc::now();
    let ago = |secs| now - chrono::Duration::seconds(secs);

    // A previous boot's answer: this boot's own clock decides.
    assert_eq!(
        serving_silence_secs(UnresponsiveSince(ago(1200)), Some(ago(35)), now),
        35
    );
    // An answer during this spawn: the wedge counts from it, as `/runners` shows it.
    assert_eq!(
        serving_silence_secs(UnresponsiveSince(ago(400)), Some(ago(3600)), now),
        400
    );
    // No spawn known: the stamp is all there is.
    assert_eq!(
        serving_silence_secs(UnresponsiveSince(ago(400)), None, now),
        400
    );
    // A clock ahead of `now` reads as no silence, not as a huge unsigned one.
    assert_eq!(
        serving_silence_secs(UnresponsiveSince(ago(-5)), None, now),
        0
    );
    for not_wedged in [Responding, Unknown] {
        assert_eq!(serving_silence_secs(not_wedged, Some(ago(9)), now), 0);
    }
}

/// Every skip variant, in priority order. Classification and operator
/// intent win over arming; the loop guard is evaluated last so `Disarm`
/// only fires for a wedge that would otherwise have restarted.
#[test]
fn serving_restart_decision_table() {
    let wedged = RunnerLiveness::UnresponsiveSince(chrono::Utc::now());
    let d = |liveness,
             silent,
             kind: &qontinui_types::wire::runner_kind::RunnerKind,
             stop,
             restart,
             in_flight,
             armed,
             enabled,
             disarmed,
             window| {
        decide_serving_restart(
            liveness, silent, 300, kind, stop, restart, in_flight, armed, enabled, disarmed, window,
        )
    };

    // Not the wedge class. `Unknown` is excluded on purpose (Design
    // decision 4): a runner that has NEVER answered has nothing to prove it
    // is this failure mode, so it is alerted, never restarted.
    for liveness in [
        RunnerLiveness::Responding,
        RunnerLiveness::Stopped,
        RunnerLiveness::Unknown,
    ] {
        assert_eq!(
            d(
                liveness,
                99_999,
                &primary_kind(),
                false,
                false,
                false,
                true,
                true,
                false,
                0
            ),
            ServingRestartDecision::SkipNotWedged,
            "{liveness:?} must never reach a restart"
        );
    }

    // Temp runners belong to the max-age reaper.
    assert_eq!(
        d(
            wedged,
            99_999,
            &temp_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::SkipTempRunner
    );

    // Operator intent beats arming.
    assert_eq!(
        d(
            wedged,
            99_999,
            &primary_kind(),
            true,
            false,
            false,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::SkipOperatorIntent
    );
    assert_eq!(
        d(
            wedged,
            99_999,
            &primary_kind(),
            false,
            true,
            false,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::SkipOperatorIntent
    );

    // A restart already in flight: the tick that follows 2s later must not
    // schedule a second one. `restart_requested` cannot carry this — it is
    // set inside `restart_runner_by_id`, after the task is already spawned.
    assert_eq!(
        d(
            wedged,
            99_999,
            &primary_kind(),
            false,
            false,
            true,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::SkipInFlight
    );

    // Threshold boundary: strictly-below skips, equal fires.
    assert_eq!(
        d(
            wedged,
            299,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::SkipBelowThreshold
    );
    assert_eq!(
        d(
            wedged,
            300,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            0
        ),
        ServingRestartDecision::Restart { attempt: 1 }
    );

    // Arming.
    assert_eq!(
        d(
            wedged,
            301,
            &primary_kind(),
            false,
            false,
            false,
            false,
            true,
            false,
            0
        ),
        ServingRestartDecision::SkipNotArmed
    );
    assert_eq!(
        d(
            wedged,
            301,
            &primary_kind(),
            false,
            false,
            false,
            true,
            false,
            false,
            0
        ),
        ServingRestartDecision::SkipDisabled
    );
    assert_eq!(
        d(
            wedged,
            301,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            true,
            0
        ),
        ServingRestartDecision::SkipDisarmed
    );

    // Window exhaustion — evaluated LAST.
    assert_eq!(
        d(
            wedged,
            301,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            SERVING_RESTART_MAX_PER_WINDOW
        ),
        ServingRestartDecision::Disarm
    );
    // …but a runner that would have skipped for a benign reason never
    // disarms the arm for every other runner's sake.
    assert_eq!(
        d(
            RunnerLiveness::Responding,
            301,
            &primary_kind(),
            false,
            false,
            false,
            true,
            true,
            false,
            SERVING_RESTART_MAX_PER_WINDOW
        ),
        ServingRestartDecision::SkipNotWedged
    );
}

/// The two kill-switches are independent: `--watchdog` is shared (one
/// operator intent, "supervise the primary"), the env switches are not.
#[test]
fn the_two_arms_share_the_cli_flag_and_nothing_else() {
    let (mut config, _root) = crate::config::config_over_an_empty_runner_tree();
    config.watchdog_enabled_at_start = false;
    assert!(!serving_restart_globally_armed(&config));
    assert!(!crash_restart_globally_armed(&config));
    config.watchdog_enabled_at_start = true;
    // Both arms follow the shared flag; the env switches (read from the
    // process environment, which tests must not mutate) are asserted by
    // their own parse functions.
    assert_eq!(
        serving_restart_globally_armed(&config),
        !serving_restart_env_disabled()
    );
    assert_ne!(SERVING_LOOP_DISABLED_REASON, CRASH_LOOP_DISABLED_REASON);
}

/// The operator reset clears BOTH arms. A reset that cleared only the crash
/// arm would leave a serving-loop disarm latched with no route to clear it.
#[test]
fn the_operator_reset_clears_both_arms() {
    let mut wd = crate::state::WatchdogState::new(true);
    wd.restart_attempts = 2;
    wd.disabled_reason = Some(CRASH_LOOP_DISABLED_REASON.to_string());
    wd.crash_history.push(chrono::Utc::now());
    wd.serving_restart_attempts = 3;
    wd.last_serving_restart_at = Some(chrono::Utc::now());
    wd.serving_history.push(chrono::Utc::now());
    wd.serving_disabled_reason = Some(SERVING_LOOP_DISABLED_REASON.to_string());
    wd.serving_restart_in_flight = true;

    wd.reset_attempts();

    assert_eq!(wd.restart_attempts, 0);
    assert!(wd.disabled_reason.is_none());
    assert!(wd.crash_history.is_empty());
    assert_eq!(wd.serving_restart_attempts, 0);
    assert!(wd.last_serving_restart_at.is_none());
    assert!(wd.serving_history.is_empty());
    assert!(wd.serving_disabled_reason.is_none());
    assert!(!wd.serving_restart_in_flight);
    // The operator's intent bit is NOT what a reset clears.
    assert!(wd.enabled);
}

// =========================================================================
// Restart-of-stopped-test-id rejection (Item 3)
// =========================================================================

/// A `test-*` id is classified as a temp runner, so the restart-not-found
/// branch emits the "auto-removed, spawn a new one" guidance rather than a
/// bare "Runner not found". (The restart path itself needs SharedState, so
/// we assert the classification predicate that drives the message choice.)
#[test]
fn restart_temp_id_is_temp_runner() {
    assert!(
        is_temp_runner("test-abc123"),
        "test-* ids must classify as temp so restart returns the \
             auto-removed guidance"
    );
}

/// A non-temp id (primary / named) is NOT a temp runner, so the
/// restart-not-found branch falls back to the bare id (no auto-remove
/// guidance, and no doubled Display prefix).
#[test]
fn restart_non_temp_id_is_not_temp_runner() {
    assert!(!is_temp_runner("primary"));
    assert!(!is_temp_runner("named-staging"));
}

/// The temp-not-found error renders with a single "Runner not found:"
/// prefix (from the variant's Display) plus the spawn-test guidance —
/// guards against the doubled-prefix regression.
#[test]
fn restart_temp_not_found_message_shape() {
    let detail = format!(
        "{} — ephemeral test runners are auto-removed when stopped and \
             cannot be restarted; spawn a new one via POST /runners/spawn-test",
        "test-xyz"
    );
    let err = SupervisorError::RunnerNotFound(detail);
    let rendered = err.to_string();
    assert!(rendered.starts_with("Runner not found: test-xyz"));
    assert_eq!(
        rendered.matches("Runner not found").count(),
        1,
        "must not double the 'Runner not found' prefix"
    );
    assert!(rendered.contains("spawn-test"));
}

// =========================================================================
// Slot SHA drift detection (proj_supervisor_slot_resolution_order)
// =========================================================================

fn sha_a() -> String {
    "a".repeat(40)
}
fn sha_b() -> String {
    "b".repeat(40)
}
fn sha_c() -> String {
    "c".repeat(40)
}

/// Build a `(sha, source)` provenance key for a live-tree build.
fn live(sha: String) -> SlotProvenanceKey {
    (Some(sha), Some(BuildSource::LiveTree))
}
/// Build a `(sha, source)` provenance key for an override build.
fn over(sha: String) -> SlotProvenanceKey {
    (Some(sha), Some(BuildSource::Override))
}
/// A slot with no provenance sidecar at all.
fn absent() -> SlotProvenanceKey {
    (None, None)
}

/// Distinct SHAs across multiple slots — drift surfaces.
#[test]
fn drift_fires_when_two_slots_disagree() {
    let all = vec![(0usize, live(sha_a())), (1usize, live(sha_b()))];
    let d =
        detect_slot_sha_drift(0, &live(sha_a()), &all).expect("distinct SHAs must surface drift");
    assert_eq!(d.picked_slot_id, 0);
    assert_eq!(d.picked_sha, sha_a());
    assert_eq!(d.picked_source, BuildSource::LiveTree);
    assert_eq!(d.conflicting.len(), 1);
    assert_eq!(d.conflicting[0].0, 1);
    assert_eq!(d.conflicting[0].1, sha_b());
    assert_eq!(d.conflicting[0].2, Some(BuildSource::LiveTree));
}

/// Same SHA but DIFFERENT source tree (live vs override) — still drift,
/// because the bytes came from a different tree. This is the core 2026-06-05
/// incident guard.
#[test]
fn drift_fires_on_same_sha_different_source() {
    let all = vec![(0usize, live(sha_a())), (1usize, over(sha_a()))];
    let d = detect_slot_sha_drift(0, &live(sha_a()), &all)
        .expect("same sha, different source must surface drift");
    assert_eq!(d.conflicting.len(), 1);
    assert_eq!(d.conflicting[0].0, 1);
    assert_eq!(d.conflicting[0].2, Some(BuildSource::Override));
}

/// All sidecar-present slots share the same `(sha, source)` — no drift.
#[test]
fn drift_silent_when_all_slots_agree() {
    let all = vec![
        (0usize, live(sha_a())),
        (1usize, live(sha_a())),
        (2usize, live(sha_a())),
    ];
    assert!(detect_slot_sha_drift(0, &live(sha_a()), &all).is_none());
}

/// Picked slot has no sidecar — drift is silent (unknown provenance can't compare).
#[test]
fn drift_silent_when_picked_provenance_missing() {
    let all = vec![(0usize, absent()), (1usize, live(sha_b()))];
    assert!(detect_slot_sha_drift(0, &absent(), &all).is_none());
}

/// Other slots have no sidecar — drift is silent (no conflict to surface).
#[test]
fn drift_silent_when_other_slots_have_no_sidecar() {
    let all = vec![
        (0usize, live(sha_a())),
        (1usize, absent()),
        (2usize, absent()),
    ];
    assert!(detect_slot_sha_drift(0, &live(sha_a()), &all).is_none());
}

/// Three slots, two carry distinct provenance — both surface in `conflicting`.
#[test]
fn drift_collects_all_distinct_others() {
    let all = vec![
        (0usize, live(sha_a())),
        (1usize, live(sha_b())),
        (2usize, live(sha_c())),
    ];
    let d =
        detect_slot_sha_drift(0, &live(sha_a()), &all).expect("two distinct others must surface");
    assert_eq!(d.conflicting.len(), 2);
    // Sorted by slot id deterministically.
    assert_eq!(d.conflicting[0].0, 1);
    assert_eq!(d.conflicting[1].0, 2);
}

/// `format_drift_warning` includes the picked slot id, abbreviated SHA,
/// the source label, and the conflict count.
#[test]
fn drift_warning_message_shape() {
    let d = SlotShaDrift {
        picked_slot_id: 0,
        picked_sha: sha_a(),
        picked_source: BuildSource::LiveTree,
        conflicting: vec![(1, sha_b(), Some(BuildSource::Override))],
    };
    let msg = format_drift_warning(&d);
    assert!(msg.contains("picked slot 0"));
    assert!(msg.contains("aaaaaaaaaaaa"), "{}", msg);
    assert!(msg.contains("slot 1"), "{}", msg);
    assert!(msg.contains("bbbbbbbbbbbb"), "{}", msg);
    assert!(msg.contains("source live_tree"), "{}", msg);
    assert!(msg.contains("source override"), "{}", msg);
    assert!(
        msg.contains("proj_supervisor_slot_resolution_order"),
        "warning must point operator at the relevant memory: {}",
        msg
    );
}

/// Pluralization: multiple conflicting slots produce "provenances", not "provenance".
#[test]
fn drift_warning_pluralizes_multiple_conflicts() {
    let d = SlotShaDrift {
        picked_slot_id: 0,
        picked_sha: sha_a(),
        picked_source: BuildSource::LiveTree,
        conflicting: vec![
            (1, sha_b(), Some(BuildSource::LiveTree)),
            (2, sha_c(), Some(BuildSource::LiveTree)),
        ],
    };
    let msg = format_drift_warning(&d);
    assert!(msg.contains("distinct provenances"), "{}", msg);
}

// =========================================================================
// Provenance sidecar IO (read_slot_provenance / read_slot_sha)
// =========================================================================

fn write_provenance(dir: &std::path::Path, p: &BuildProvenance) {
    let debug = dir.join("debug");
    std::fs::create_dir_all(&debug).expect("mkdir debug");
    let sidecar = debug.join(SLOT_PROVENANCE_SIDECAR_FILENAME);
    std::fs::write(&sidecar, serde_json::to_string(p).expect("serialize")).expect("write");
}

/// Round-trip: write provenance JSON, read returns the identical struct,
/// and the serialized `source` uses the wire labels `live_tree`/`override`.
#[test]
fn read_slot_provenance_round_trip() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let prov = BuildProvenance {
        sha: Some(sha_a()),
        source: BuildSource::Override,
        built_from: "/some/abs/worktree".to_string(),
        built_at: "2026-06-05T12:00:00+00:00".to_string(),
    };
    write_provenance(dir.path(), &prov);

    // Raw JSON carries the wire shape we promised consumers.
    let raw = std::fs::read_to_string(
        dir.path()
            .join("debug")
            .join(SLOT_PROVENANCE_SIDECAR_FILENAME),
    )
    .expect("read raw");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("parse raw");
    assert_eq!(v["sha"], serde_json::json!(sha_a()));
    assert_eq!(v["source"], serde_json::json!("override"));
    assert_eq!(v["built_from"], serde_json::json!("/some/abs/worktree"));

    let got = read_slot_provenance(dir.path()).expect("must read");
    assert_eq!(got, prov);
    // The convenience SHA accessor mirrors the provenance sha.
    assert_eq!(read_slot_sha(dir.path()), Some(sha_a()));
}

/// A `live_tree` source serializes to `"live_tree"`.
#[test]
fn provenance_live_tree_source_wire_label() {
    let prov = BuildProvenance {
        sha: Some(sha_a()),
        source: BuildSource::LiveTree,
        built_from: "/live/tree".to_string(),
        built_at: "2026-06-05T12:00:00+00:00".to_string(),
    };
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&prov).unwrap()).unwrap();
    assert_eq!(v["source"], serde_json::json!("live_tree"));
}

/// `sha: null` round-trips (the git probe failed at build time).
#[test]
fn read_slot_provenance_null_sha() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let prov = BuildProvenance {
        sha: None,
        source: BuildSource::LiveTree,
        built_from: "/live/tree".to_string(),
        built_at: "2026-06-05T12:00:00+00:00".to_string(),
    };
    write_provenance(dir.path(), &prov);
    let got = read_slot_provenance(dir.path()).expect("must read");
    assert_eq!(got.sha, None);
    assert_eq!(read_slot_sha(dir.path()), None);
}

/// Missing sidecar — no error, returns None.
#[test]
fn read_slot_provenance_missing_returns_none() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    assert!(read_slot_provenance(dir.path()).is_none());
    assert!(read_slot_sha(dir.path()).is_none());
}

/// A legacy plain-SHA file (the old `qontinui-runner.exe.git_sha` content,
/// or any non-JSON) under the new filename is unparseable → treated as
/// absent. Slots self-heal on the next build.
#[test]
fn read_slot_provenance_legacy_plain_sha_returns_none() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let debug = dir.path().join("debug");
    std::fs::create_dir_all(&debug).expect("mkdir debug");
    // Old format: a bare 40-hex SHA, no JSON.
    std::fs::write(
        debug.join(SLOT_PROVENANCE_SIDECAR_FILENAME),
        sha_a().as_bytes(),
    )
    .expect("write");
    assert!(read_slot_provenance(dir.path()).is_none());
    assert!(read_slot_sha(dir.path()).is_none());
}

/// Empty / whitespace-only sidecar — returns None (unparseable).
#[test]
fn read_slot_provenance_blank_returns_none() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let debug = dir.path().join("debug");
    std::fs::create_dir_all(&debug).expect("mkdir debug");
    std::fs::write(debug.join(SLOT_PROVENANCE_SIDECAR_FILENAME), b"   \n\t  ").expect("write");
    assert!(read_slot_provenance(dir.path()).is_none());
}

// =========================================================================
// exe-copy parent-dir creation (start_exe_mode_for_runner copy step)
// =========================================================================

/// The copy-never-run-from-slot step in `start_exe_mode_for_runner` must
/// create the copy's parent before copying the slot/LKG exe into it — for
/// a temp runner `target/debug/runners/<pool-name>/`, which never exists
/// before its first spawn, and for the primary `target/debug/`.
/// Supervisor-managed trees only ever materialize `target-pool/`, so a
/// tree that has never had a default `cargo build` won't have
/// `target/debug/` either, and the copy would fail with `os error 3`
/// (path not found). Mirrors the inline mkdir-then-copy step at the same
/// abstraction level (the copy itself lives inside the async
/// process-spawning `start_exe_mode_for_runner`, which isn't unit-testable
/// without launching a real process).
#[test]
fn exe_copy_creates_missing_target_debug_parent() {
    let root = tempfile::TempDir::new().expect("tempdir");

    // Source exe lives where a build slot would have put it.
    let slot_debug = root.path().join("target-pool").join("slot-0").join("debug");
    std::fs::create_dir_all(&slot_debug).expect("mkdir slot debug");
    let source_exe = slot_debug.join("qontinui-runner.exe");
    std::fs::write(&source_exe, b"fake-exe-bytes").expect("write source exe");

    // Copy target's parent (`target/debug/runners/<pool-name>/`)
    // deliberately does NOT exist, nor does `target/debug/`.
    let copy_path = root
        .path()
        .join("target")
        .join("debug")
        .join("runners")
        .join("qontinui-runner-test-9877")
        .join("qontinui-runner.exe");
    let parent = copy_path.parent().expect("copy_path has a parent");
    assert!(
        !parent.exists(),
        "precondition: the copy dir must be absent"
    );

    // The fix: create_dir_all(parent) before the copy.
    std::fs::create_dir_all(parent).expect("create_dir_all must succeed");
    assert!(parent.is_dir(), "the per-runner copy dir should now exist");

    // And the copy then succeeds (previously failed with os error 3).
    std::fs::copy(&source_exe, &copy_path).expect("copy into freshly-created dir");
    assert!(copy_path.exists(), "exe copy should land in the new dir");
    assert_eq!(
        std::fs::read(&copy_path).expect("read copy"),
        b"fake-exe-bytes"
    );
}

// =========================================================================
// qontinui-shim sidecar deploy (start_exe_mode_for_runner copy step).
// The runner materializes identity shims from the stub next to its OWN
// exe, so the stub must ride along with every exe copy — 2026-07-03
// incident: a stale stub was re-materialized into every terminal.
// =========================================================================

/// Happy path: shim next to the slot exe is copied next to the per-runner
/// exe copy, replacing any stale stub already there.
#[test]
fn shim_sidecar_copies_next_to_dest_exe_and_replaces_stale() {
    let root = tempfile::TempDir::new().expect("tempdir");

    let slot_debug = root.path().join("target-pool").join("slot-0").join("debug");
    std::fs::create_dir_all(&slot_debug).expect("mkdir slot debug");
    let source_exe = slot_debug.join("qontinui-runner.exe");
    std::fs::write(&source_exe, b"exe").expect("write exe");
    std::fs::write(
        slot_debug.join(crate::build_monitor::SHIM_EXE_FILENAME),
        b"fresh-shim",
    )
    .expect("write shim");

    let target_debug = root
        .path()
        .join("target")
        .join("debug")
        .join("runners")
        .join("qontinui-runner-test-9877");
    std::fs::create_dir_all(&target_debug).expect("mkdir per-runner copy dir");
    let dest_exe = target_debug.join("qontinui-runner.exe");
    std::fs::write(&dest_exe, b"exe-copy").expect("write exe copy");
    // Pre-existing STALE stub — the exact incident artifact.
    let dest_shim = target_debug.join(crate::build_monitor::SHIM_EXE_FILENAME);
    std::fs::write(&dest_shim, b"stale-shim").expect("write stale shim");

    match deploy_sidecar(
        &source_exe,
        &dest_exe,
        crate::build_monitor::SHIM_EXE_FILENAME,
    ) {
        SidecarDeploy::Copied { to } => assert_eq!(to, dest_shim),
        other => panic!("expected Copied, got {:?}", other),
    }
    assert_eq!(
        std::fs::read(&dest_shim).expect("read deployed shim"),
        b"fresh-shim",
        "the stale stub must be replaced by the source's shim"
    );
    // No tmp litter left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&target_debug)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "tmp files left: {:?}", leftovers);
}

/// Legacy fallback resolution (`target/debug/qontinui-runner.exe`): source
/// and dest share a dir, so there is nothing to copy — and critically no
/// self-copy that could truncate the stub in place.
#[test]
fn shim_sidecar_same_dir_is_noop() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let debug = root.path().join("target").join("debug");
    std::fs::create_dir_all(&debug).expect("mkdir");
    let source_exe = debug.join("qontinui-runner.exe");
    std::fs::write(&source_exe, b"exe").expect("write");
    let shim = debug.join(crate::build_monitor::SHIM_EXE_FILENAME);
    std::fs::write(&shim, b"shim-bytes").expect("write shim");
    let dest_exe = debug.join("qontinui-runner-primary.exe");

    assert!(matches!(
        deploy_sidecar(
            &source_exe,
            &dest_exe,
            crate::build_monitor::SHIM_EXE_FILENAME
        ),
        SidecarDeploy::SameDir
    ));
    assert_eq!(
        std::fs::read(&shim).expect("read shim"),
        b"shim-bytes",
        "the in-place stub must be untouched"
    );
}

/// Source without a shim (pre-sidecar slot/LKG, or the fail-open shim
/// build failed): reports SourceMissing naming the expected path, and
/// leaves the destination dir untouched. The caller logs the
/// "identity shims will be stale" WARN — the start itself proceeds.
#[test]
fn shim_sidecar_missing_source_reports_and_writes_nothing() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let slot_debug = root.path().join("target-pool").join("slot-1").join("debug");
    std::fs::create_dir_all(&slot_debug).expect("mkdir");
    let source_exe = slot_debug.join("qontinui-runner.exe");
    std::fs::write(&source_exe, b"exe").expect("write");

    let target_debug = root.path().join("target").join("debug");
    std::fs::create_dir_all(&target_debug).expect("mkdir");
    let dest_exe = target_debug.join("qontinui-runner-primary.exe");

    match deploy_sidecar(
        &source_exe,
        &dest_exe,
        crate::build_monitor::SHIM_EXE_FILENAME,
    ) {
        SidecarDeploy::SourceMissing { expected, .. } => {
            assert_eq!(
                expected,
                slot_debug.join(crate::build_monitor::SHIM_EXE_FILENAME)
            );
        }
        other => panic!("expected SourceMissing, got {:?}", other),
    }
    assert!(
        !target_debug
            .join(crate::build_monitor::SHIM_EXE_FILENAME)
            .exists(),
        "no shim must be fabricated at the destination"
    );
}

// =========================================================================
// Zero-length sidecars (plan 2026-09-27-qontinui-pr-zero-byte-sidecar-
// placeholder-published-as-session-cli, Phase 3). The runner's build.rs
// placeholder is a 0-byte `qontinui-pr.exe` that tauri-build copies beside
// the runner exe; deployed, the runner puts it on every terminal's PATH and
// `qontinui-pr create` exits 0 having opened no PR. `deploy_sidecar` must
// refuse it and never leave a 0-byte copy beside a runner.
// =========================================================================

/// Stage a slot-0 source exe and a per-runner copy dir; returns
/// `(source_exe, dest_exe)`.
fn stage_sidecar_deploy_dirs(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let slot_debug = root.join("target-pool").join("slot-0").join("debug");
    std::fs::create_dir_all(&slot_debug).expect("mkdir slot debug");
    let source_exe = slot_debug.join("qontinui-runner.exe");
    std::fs::write(&source_exe, b"exe").expect("write exe");
    let own_dir = root
        .join("target")
        .join("debug")
        .join("runners")
        .join("qontinui-runner-test-9877");
    std::fs::create_dir_all(&own_dir).expect("mkdir per-runner copy dir");
    let dest_exe = own_dir.join("qontinui-runner.exe");
    std::fs::write(&dest_exe, b"exe-copy").expect("write exe copy");
    (source_exe, dest_exe)
}

fn tmp_litter(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().contains(".tmp-"))
        .collect()
}

/// An EMPTY source is refused: nothing is written, and the stale 0-byte
/// copy an earlier deploy left at the destination is removed.
#[test]
fn sidecar_empty_source_is_refused_and_a_stale_empty_dest_removed() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    let src = source_exe.with_file_name(cli);
    std::fs::write(&src, b"").expect("write 0-byte placeholder copy");
    let dst = dest_exe.with_file_name(cli);
    std::fs::write(&dst, b"").expect("write stale 0-byte deployed copy");

    match deploy_sidecar(&source_exe, &dest_exe, cli) {
        SidecarDeploy::SourceEmpty {
            path,
            removed_empty_dest,
        } => {
            assert_eq!(path, src);
            assert_eq!(removed_empty_dest, Some(dst.clone()));
        }
        other => panic!("expected SourceEmpty, got {:?}", other),
    }
    assert!(
        !dst.exists(),
        "a 0-byte session CLI must not be left beside the runner"
    );
    assert!(tmp_litter(dest_exe.parent().unwrap()).is_empty());
}

/// An EMPTY source with no destination file writes nothing at all.
#[test]
fn sidecar_empty_source_writes_nothing() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    std::fs::write(source_exe.with_file_name(cli), b"").expect("write placeholder");

    match deploy_sidecar(&source_exe, &dest_exe, cli) {
        SidecarDeploy::SourceEmpty {
            removed_empty_dest, ..
        } => assert_eq!(removed_empty_dest, None),
        other => panic!("expected SourceEmpty, got {:?}", other),
    }
    let dest_dir = dest_exe.parent().unwrap();
    assert!(!dest_dir.join(cli).exists(), "nothing may be fabricated");
    assert!(tmp_litter(dest_dir).is_empty());
}

/// The refusal is ZERO-LENGTH-ONLY on the destination side too: an empty
/// source never deletes a destination sidecar that has content.
#[test]
fn sidecar_empty_source_leaves_a_dest_with_content_alone() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    std::fs::write(source_exe.with_file_name(cli), b"").expect("write placeholder");
    let dst = dest_exe.with_file_name(cli);
    std::fs::write(&dst, b"earlier-real-cli").expect("write earlier real cli");

    assert!(matches!(
        deploy_sidecar(&source_exe, &dest_exe, cli),
        SidecarDeploy::SourceEmpty {
            removed_empty_dest: None,
            ..
        }
    ));
    assert_eq!(std::fs::read(&dst).unwrap(), b"earlier-real-cli");
}

/// A NON-EMPTY source keeps today's behaviour, and replaces a stale 0-byte
/// copy at the destination with the real bytes.
#[test]
fn sidecar_nonempty_source_replaces_a_stale_empty_dest() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    std::fs::write(source_exe.with_file_name(cli), b"real-cli").expect("write real cli");
    let dst = dest_exe.with_file_name(cli);
    std::fs::write(&dst, b"").expect("write stale 0-byte copy");

    match deploy_sidecar(&source_exe, &dest_exe, cli) {
        SidecarDeploy::Copied { to } => assert_eq!(to, dst),
        other => panic!("expected Copied, got {:?}", other),
    }
    assert_eq!(std::fs::read(&dst).unwrap(), b"real-cli");
    assert!(tmp_litter(dest_exe.parent().unwrap()).is_empty());
}

/// Same-dir layout (the primary copy beside a `target/debug/` source):
/// the file sitting there IS what the runner will publish, so a 0-byte one
/// is removed and reported, never silently left as `SameDir`.
#[test]
fn sidecar_same_dir_zero_length_is_removed() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let debug = root.path().join("target").join("debug");
    std::fs::create_dir_all(&debug).expect("mkdir");
    let source_exe = debug.join("qontinui-runner.exe");
    std::fs::write(&source_exe, b"exe").expect("write");
    let cli = debug.join(crate::build_monitor::SESSION_CLI_EXE_FILENAME);
    std::fs::write(&cli, b"").expect("write 0-byte cli");
    let dest_exe = debug.join("qontinui-runner-primary.exe");

    match deploy_sidecar(
        &source_exe,
        &dest_exe,
        crate::build_monitor::SESSION_CLI_EXE_FILENAME,
    ) {
        SidecarDeploy::SourceEmpty {
            path,
            removed_empty_dest,
        } => {
            assert_eq!(path, cli);
            assert_eq!(removed_empty_dest, Some(cli.clone()));
        }
        other => panic!("expected SourceEmpty, got {:?}", other),
    }
    assert!(!cli.exists(), "the in-place 0-byte CLI must be removed");
}

/// B1 (review round 1): a MISSING source must still remove a zero-length
/// copy at the destination. Reachable: a primary pinned to an LKG that has
/// no `qontinui-pr` yet deploys over a `target/debug/` that tauri-build
/// refilled with its 0-byte placeholder copy.
#[test]
fn sidecar_missing_source_removes_a_stale_empty_dest() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    let dst = dest_exe.with_file_name(cli);
    std::fs::write(&dst, b"").expect("write stale 0-byte copy");

    match deploy_sidecar(&source_exe, &dest_exe, cli) {
        SidecarDeploy::SourceMissing {
            expected,
            removed_empty_dest,
        } => {
            assert_eq!(expected, source_exe.with_file_name(cli));
            assert_eq!(
                removed_empty_dest,
                Some(dst.clone()),
                "the removal is reported"
            );
        }
        other => panic!("expected SourceMissing, got {:?}", other),
    }
    assert!(
        !dst.exists(),
        "a 0-byte session CLI must not be left beside the runner when the source is missing"
    );
}

/// A missing source never deletes a destination sidecar that has content.
#[test]
fn sidecar_missing_source_leaves_a_dest_with_content_alone() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    let dst = dest_exe.with_file_name(cli);
    std::fs::write(&dst, b"earlier-real-cli").expect("write earlier real cli");

    assert!(matches!(
        deploy_sidecar(&source_exe, &dest_exe, cli),
        SidecarDeploy::SourceMissing {
            removed_empty_dest: None,
            ..
        }
    ));
    assert_eq!(std::fs::read(&dst).unwrap(), b"earlier-real-cli");
}

/// B1: a copy that FAILS must still remove a zero-length copy at the
/// destination. A directory where the sidecar should be makes the copy
/// fail deterministically on every platform.
#[test]
fn sidecar_copy_failure_removes_a_stale_empty_dest() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let (source_exe, dest_exe) = stage_sidecar_deploy_dirs(root.path());
    let cli = crate::build_monitor::SESSION_CLI_EXE_FILENAME;
    std::fs::create_dir_all(source_exe.with_file_name(cli)).expect("dir as source");
    let dst = dest_exe.with_file_name(cli);
    std::fs::write(&dst, b"").expect("write stale 0-byte copy");

    match deploy_sidecar(&source_exe, &dest_exe, cli) {
        SidecarDeploy::CopyFailed {
            removed_empty_dest, ..
        } => assert_eq!(
            removed_empty_dest,
            Some(dst.clone()),
            "the removal is reported"
        ),
        other => panic!("expected CopyFailed, got {:?}", other),
    }
    assert!(
        !dst.exists(),
        "a 0-byte session CLI must not be left beside the runner when the copy fails"
    );
    assert!(tmp_litter(dest_exe.parent().unwrap()).is_empty());
}

/// A temp restart keeps the runner's per-instance state (pairing, config,
/// app-data trees, WebView2 profile); a plain stop reaps it. Pinned at the
/// pure gate and at its call sites, whose input must be the
/// `restart_requested` latch `restart_runner_by_id` sets — and every
/// per-instance removal in `stop_runner_by_id` must go through the one
/// gated helper, so a removal added beside it cannot escape the gate the
/// way the app-data reap did after #198.
#[test]
fn temp_restart_keeps_the_instance_state() {
    assert!(reap_instance_state_on_stop(false));
    assert!(!reap_instance_state_on_stop(true));

    // …and a restart that fails before re-registering the temp id reaps
    // the state it kept; a registered or non-temp runner is left alone.
    assert!(orphaned_by_failed_restart(true, false));
    assert!(!orphaned_by_failed_restart(true, true));
    assert!(!orphaned_by_failed_restart(false, false));

    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("process")
            .join("manager.rs"),
    )
    .expect("read manager.rs")
    .replace("\r\n", "\n");
    // Every body below is PRODUCTION code, so read only that: the same
    // text whether this module's tests are inline or extracted to
    // `process/manager/tests.rs`. `body_of` panics on a missing signature,
    // so the scan cannot pass on text that no longer holds the functions.
    let src = crate::source_scan::production_span(&src).to_string();
    // Calls are compared with their whitespace and trailing commas removed, so
    // an assertion pins the call and not where rustfmt broke its lines.
    let sq = |text: &str| {
        text.split_whitespace()
            .collect::<String>()
            .replace(",)", ")")
    };
    // Up to the function's own closing brace, by brace matching, so a match
    // cannot come from a neighbouring function.
    let body_of = |sig: &str| {
        sq(crate::source_scan::fn_body(&src, sig).unwrap_or_else(|| panic!("{sig} exists")))
    };

    let dirs = "&state.config.ambient_user_dirs";
    let stop = body_of("pub async fn stop_runner_by_id(");
    assert!(
        stop.contains(&sq(
            "let restarting = managed.runner.read().await.restart_requested;"
        )) && stop
            .split_once(&sq("if reap_instance_state_on_stop(restarting) {"))
            .and_then(|(_, gated)| gated.split('}').next())
            .is_some_and(|gated| {
                gated.contains(&sq(&format!(
                    "reap_runner_instance_state({dirs}, &runner_id, &runner_name)"
                )))
            }),
        "stop_runner_by_id must gate the per-instance reap on restart_requested"
    );
    assert_eq!(
        stop.matches("reap_runner_instance_state(").count(),
        1,
        "exactly one (gated) per-instance reap in stop_runner_by_id"
    );
    for direct in [
        "remove_instance_config_dir(",
        "remove_runner_app_data_dirs(",
        "remove_webview2_user_data_folder(",
    ] {
        assert!(
            !stop.contains(direct),
            "stop_runner_by_id calls {direct} directly, outside the restart gate"
        );
    }

    let restart = body_of("pub async fn run_admitted_restart(");
    assert!(
        restart.contains(&sq(
            "if orphaned_by_failed_restart(is_temp_runner(runner_id), still_registered)"
        )) && restart.contains(&sq(&format!(
            "reap_runner_instance_state({dirs}, runner_id, &managed.config.name)"
        ))),
        "a failed temp restart must reap the per-instance state its stop half kept"
    );

    // The other teardown paths must route through the helper too, not
    // re-spell its set: the max-age sweep here, and `DELETE
    // /runners/{id}` / `purge-stale` in routes/runners.rs.
    let sweep = body_of("pub async fn reap_stale_test_runners(");
    assert!(sweep.contains(&sq(&format!(
        "reap_runner_instance_state({dirs}, &id, &name)"
    ))));
    // Startup-window grace (plan
    // 2026-09-19-supervisor-reaper-purges-an-in-flight-spawn-test): the
    // endless sweep loop cannot be driven from a test, so pin that it
    // still consults the shared, unit-tested skip decision.
    assert!(
        sweep.contains(&sq("skip_for_startup_window(managed, is_running)")),
        "reap_stale_test_runners no longer consults skip_for_startup_window — a starting \
             temp runner whose port is not bound yet would be reaped as crashed"
    );
    let routes = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("routes")
            .join("runners.rs"),
    )
    .expect("read routes/runners.rs")
    .replace("\r\n", "\n");
    // Production only — the bans below police teardown call sites, and
    // that file's tests live in `routes/runners/tests.rs` once extracted.
    let routes = sq(crate::source_scan::production_span(&routes));
    assert_eq!(
        routes
            .matches(&sq(&format!(
                "manager::reap_runner_instance_state({dirs}, &id, &name)"
            )))
            .count(),
        2,
        "remove_runner and purge_stale_test_runners_core must use the helper"
    );
    for (site, body) in [
        ("reap_stale_test_runners", sweep.as_str()),
        ("routes/runners.rs", routes.as_str()),
    ] {
        for direct in [
            "remove_instance_config_dir(",
            "remove_runner_app_data_dirs(",
            "remove_webview2_user_data_folder(",
        ] {
            assert!(
                !body.contains(direct),
                "{site} calls {direct} directly instead of reap_runner_instance_state"
            );
        }
    }

    let helper = body_of("pub(crate) async fn reap_runner_instance_state(");
    for removal in [
        "remove_webview2_user_data_folder(runner_id, false)",
        "remove_runner_app_data_dirs(dirs, runner_name, false)",
        "remove_instance_config_dir(dirs, runner_id, false)",
    ] {
        assert!(
            helper.contains(&sq(removal)),
            "reap_runner_instance_state must include {removal}"
        );
    }
}

/// The helper keys each removal on the right half of the identity: the
/// instance config dir on the runner ID, the `instance-<name>` app-data
/// trees on `config.name`. For a temp runner the two are equal
/// ([`crate::process::temp_runner_instance_name`]), so the source scan
/// above cannot tell a swapped pair from a correct one; a named runner
/// carries an operator-supplied name, and a swap there would leak both
/// trees silently. Driven with deliberately DIFFERENT id and name.
#[tokio::test]
async fn reap_runner_instance_state_keys_config_on_id_and_app_data_on_name() {
    let (dirs, _user_dirs) = temp_user_dirs();
    let runner_id = "named-reap-selftest-id";
    let runner_name = "reap-selftest-name";

    let config_dir = instance_config_dir(&dirs, runner_id).expect("At resolves an instance root");
    let app_data = crate::process::app_data_dir_candidates(
        &dirs,
        &format!(
            "instance-{}",
            crate::process::sanitize_instance_name(runner_name)
        ),
    );
    assert_eq!(app_data.len(), 5, "{app_data:?}");
    for dir in std::iter::once(&config_dir).chain(app_data.iter()) {
        std::fs::create_dir_all(dir).expect("create dir");
        std::fs::write(dir.join("marker.json"), b"{}").expect("write marker");
    }

    reap_runner_instance_state(&dirs, runner_id, runner_name).await;

    assert!(
        !config_dir.exists(),
        "instance config dir {config_dir:?} (keyed on the id) survived the reap"
    );
    for dir in &app_data {
        assert!(
            !dir.exists(),
            "app-data tree {dir:?} (keyed on the name) survived the reap"
        );
    }
}

/// S-2: a `Temp` runner's copy lives in its own
/// `target/debug/runners/<pool-name>/` directory, so its sidecars land
/// there and NEVER in the shared `target/debug/` of the live tree (where a
/// temp spawn used to overwrite the primary's `qontinui-shim`). Drives the
/// real `runner_exe_copy_path` against a tempdir runner tree, then the
/// same mkdir → copy → sidecar-deploy sequence `start_exe_mode_for_runner`
/// runs, and finally the stop-time cleanup.
#[test]
fn temp_runner_sidecars_land_in_its_own_dir_not_target_debug() {
    use clap::Parser;
    let root = tempfile::TempDir::new().expect("tempdir");
    let runner = root.path().join("qontinui-runner");
    let src_tauri = runner.join("src-tauri");
    std::fs::create_dir_all(&src_tauri).expect("mkdir src-tauri");
    let project_dir = src_tauri.to_string_lossy().into_owned();
    let config = crate::config::SupervisorConfig::from_args(crate::config::CliArgs::parse_from([
        "test",
        "--project-dir",
        project_dir.as_str(),
    ]));

    // Source exe + both sidecars in a build slot.
    let slot_debug = runner.join("target-pool").join("slot-0").join("debug");
    std::fs::create_dir_all(&slot_debug).expect("mkdir slot debug");
    let source_exe = slot_debug.join(crate::config::RUNNER_BIN_NAME);
    std::fs::write(&source_exe, b"exe").expect("write exe");
    std::fs::write(
        slot_debug.join(crate::build_monitor::SHIM_EXE_FILENAME),
        b"fresh-shim",
    )
    .expect("write shim");
    std::fs::write(slot_debug.join(GIT_CREDENTIAL_EXE_FILENAME), b"cred")
        .expect("write credential helper");
    std::fs::write(
        slot_debug.join(crate::build_monitor::SESSION_CLI_EXE_FILENAME),
        b"real-cli",
    )
    .expect("write session cli");
    std::fs::write(
        slot_debug.join(crate::build_monitor::PTY_HOLDER_EXE_FILENAME),
        b"real-holder",
    )
    .expect("write pty holder");

    // The primary's shim already sits in the shared target/debug.
    let target_debug = runner.join("target").join("debug");
    std::fs::create_dir_all(&target_debug).expect("mkdir target debug");
    let shared_shim = target_debug.join(crate::build_monitor::SHIM_EXE_FILENAME);
    std::fs::write(&shared_shim, b"primary-shim").expect("write primary shim");

    let mut temp = crate::config::RunnerConfig::default_primary();
    temp.id = "test-1".to_string();
    temp.port = 9877;
    temp.kind = qontinui_types::wire::runner_kind::RunnerKind::Temp {
        id: "test-1".to_string(),
    };
    let dest_exe = config.runner_exe_copy_path(&temp);
    let own_dir = dest_exe.parent().expect("copy has a parent").to_path_buf();
    assert_ne!(
        own_dir.canonicalize().unwrap_or_else(|_| own_dir.clone()),
        target_debug.canonicalize().expect("canonical target/debug"),
        "a temp copy must not live directly in target/debug"
    );
    assert!(own_dir.ends_with("runners/qontinui-runner-test-9877"));

    std::fs::create_dir_all(&own_dir).expect("mkdir own dir");
    copy_executable(&source_exe, &dest_exe).expect("copy exe");
    // The same roster loop start_exe_mode_for_runner runs.
    for sidecar in crate::build_monitor::RUNNER_SIDECARS
        .iter()
        .filter(|s| s.deployed_beside_runner)
    {
        match deploy_sidecar(&source_exe, &dest_exe, sidecar.filename) {
            SidecarDeploy::Copied { to } => assert_eq!(to, own_dir.join(sidecar.filename)),
            other => panic!("expected Copied for {}, got {:?}", sidecar.bin, other),
        }
    }
    assert_eq!(
        std::fs::read(own_dir.join(crate::build_monitor::SESSION_CLI_EXE_FILENAME)).unwrap(),
        b"real-cli"
    );
    assert!(matches!(
        deploy_sidecar(&source_exe, &dest_exe, GIT_CREDENTIAL_EXE_FILENAME),
        SidecarDeploy::Copied { .. }
    ));
    assert_eq!(
        std::fs::read(own_dir.join(crate::build_monitor::SHIM_EXE_FILENAME)).unwrap(),
        b"fresh-shim"
    );
    assert_eq!(
        std::fs::read(&shared_shim).expect("read shared shim"),
        b"primary-shim",
        "the shared target/debug shim must be untouched by a temp deploy"
    );

    // Stop-time cleanup removes the whole per-runner directory and leaves
    // the shared target/debug alone.
    remove_runner_exe_copy(&config, &temp);
    assert!(!own_dir.exists(), "per-runner copy dir must be removed");
    assert!(
        shared_shim.exists(),
        "shared target/debug must survive cleanup"
    );
}

// =========================================================================
// Start provenance gate (Phase 3): non-temp start refuses a known-foreign
// (override) slot exe; temp stays permissive; unknown warns; live allows.
// =========================================================================

fn override_prov(sha: Option<String>) -> BuildProvenance {
    BuildProvenance {
        sha,
        source: BuildSource::Override,
        built_from: "/some/abs/.spawn-feat-x/qontinui-runner".to_string(),
        built_at: "2026-06-05T12:00:00+00:00".to_string(),
    }
}
fn live_prov(sha: Option<String>) -> BuildProvenance {
    BuildProvenance {
        sha,
        source: BuildSource::LiveTree,
        built_from: "/live/tree".to_string(),
        built_at: "2026-06-05T12:00:00+00:00".to_string(),
    }
}
fn origin_main_prov(sha: Option<String>) -> BuildProvenance {
    BuildProvenance {
        sha,
        source: BuildSource::OriginMain,
        built_from: "/ws/.spawn-origin-main/qontinui-runner".to_string(),
        built_at: "2026-06-07T12:00:00+00:00".to_string(),
    }
}

/// LKG-eligibility / start-eligibility predicate: `LiveTree` AND `OriginMain`
/// are vouched (true); `Override` is not (false). This is the single
/// predicate behind both the LKG promotion gate and the non-temp start gate
/// — Phase B widens it to include `OriginMain`.
#[test]
fn build_source_is_vouched_predicate() {
    assert!(
        BuildSource::LiveTree.is_vouched(),
        "live tree must be vouched"
    );
    assert!(
        BuildSource::OriginMain.is_vouched(),
        "origin/main must be vouched (LKG-eligible + startable as primary)"
    );
    assert!(
        !BuildSource::Override.is_vouched(),
        "override must NOT be vouched"
    );
}

/// Temp runner: always allowed, regardless of provenance. Temp runners
/// exist to run foreign refs.
#[test]
fn start_gate_temp_always_ok() {
    // override
    assert_eq!(
        start_provenance_gate(true, 0, Some(&override_prov(Some(sha_a())))).unwrap(),
        None
    );
    // live tree
    assert_eq!(
        start_provenance_gate(true, 1, Some(&live_prov(Some(sha_b())))).unwrap(),
        None
    );
    // unknown
    assert_eq!(start_provenance_gate(true, 2, None).unwrap(), None);
}

/// Non-temp + positive override evidence: refuse with an error naming the
/// slot, the provenance (built_from + sha), and the recovery path.
#[test]
fn start_gate_non_temp_override_refuses_with_recovery() {
    let err = start_provenance_gate(false, 2, Some(&override_prov(Some(sha_a()))))
        .expect_err("override must refuse");
    let msg = err.to_string();
    // Names the slot.
    assert!(msg.contains("slot 2"), "missing slot id: {msg}");
    // Names the provenance detail.
    assert!(msg.contains("source=override"), "missing source: {msg}");
    assert!(
        msg.contains(".spawn-feat-x/qontinui-runner"),
        "missing built_from: {msg}"
    );
    assert!(msg.contains(&sha_a()), "missing sha: {msg}");
    // Names the recovery.
    assert!(
        msg.contains("POST /runner/fix-and-rebuild"),
        "missing recovery: {msg}"
    );
    // Maps to a 500 through existing start-failure plumbing.
    assert_eq!(
        err.to_status_body().0,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
}

/// Non-temp + override with no sha still refuses and renders `(unknown)`.
#[test]
fn start_gate_non_temp_override_null_sha_still_refuses() {
    let err = start_provenance_gate(false, 0, Some(&override_prov(None)))
        .expect_err("override must refuse even without a sha");
    assert!(err.to_string().contains("sha=(unknown)"), "{err}");
}

/// Non-temp + unknown provenance (no sidecar): warn-and-proceed, NOT a
/// refusal. Avoids bricking the first watchdog auto-start after a deploy.
#[test]
fn start_gate_non_temp_unknown_warns_proceeds() {
    let out = start_provenance_gate(false, 1, None).expect("unknown must not error");
    let StartProvenanceWarning(msg) = out.expect("unknown must produce a warning");
    assert!(msg.contains("slot 1"), "{msg}");
    assert!(msg.to_lowercase().contains("unknown"), "{msg}");
}

/// Non-temp + live-tree provenance: allowed regardless of sha. main
/// advancing between build and start is staleness, not a provenance lie.
#[test]
fn start_gate_non_temp_live_tree_ok_regardless_of_sha() {
    assert_eq!(
        start_provenance_gate(false, 0, Some(&live_prov(Some(sha_a())))).unwrap(),
        None
    );
    // A different (stale) sha is still fine — no sha gating.
    assert_eq!(
        start_provenance_gate(false, 0, Some(&live_prov(Some(sha_c())))).unwrap(),
        None
    );
    // Even a null sha live-tree build is allowed.
    assert_eq!(
        start_provenance_gate(false, 0, Some(&live_prov(None))).unwrap(),
        None
    );
}

/// Non-temp + origin/main provenance: ALLOWED (Phase B). An origin/main
/// worktree build is canonical merged truth — folding it into the Override
/// refusal would brick every primary start. Allowed regardless of sha,
/// exactly like live-tree.
#[test]
fn start_gate_non_temp_origin_main_ok() {
    assert_eq!(
        start_provenance_gate(false, 0, Some(&origin_main_prov(Some(sha_a())))).unwrap(),
        None
    );
    // A different sha is still fine — no sha gating.
    assert_eq!(
        start_provenance_gate(false, 0, Some(&origin_main_prov(Some(sha_c())))).unwrap(),
        None
    );
    // Even a null sha origin/main build is allowed.
    assert_eq!(
        start_provenance_gate(false, 0, Some(&origin_main_prov(None))).unwrap(),
        None
    );
}

/// Integration-style: a slot whose on-disk provenance sidecar says
/// `override` makes a NON-temp (primary) start fail with the documented
/// recovery message, while a `test-*` spawn resolving the SAME slot still
/// works. Exercises the real `read_slot_provenance` read path + the gate
/// together, reusing the Phase 1 temp-dir slot fixture (`write_provenance`).
#[test]
fn start_gate_same_override_slot_refuses_primary_allows_temp() {
    let slot_dir = tempfile::TempDir::new().expect("tempdir");
    // Phase 1 fixture: write a real override provenance sidecar into the
    // slot's target dir.
    write_provenance(slot_dir.path(), &override_prov(Some(sha_a())));
    let prov = read_slot_provenance(slot_dir.path());
    assert!(prov.is_some(), "fixture must produce readable provenance");

    // Same slot id (7), same provenance. Primary (non-temp) is refused...
    let primary_is_temp = is_temp_runner("primary");
    assert!(!primary_is_temp, "primary must be non-temp");
    let primary = start_provenance_gate(primary_is_temp, 7, prov.as_ref());
    let err = primary.expect_err("primary start must be refused for an override slot");
    assert!(err.to_string().contains("slot 7"), "{err}");
    assert!(
        err.to_string().contains("POST /runner/fix-and-rebuild"),
        "{err}"
    );

    // ...while a test-* spawn resolving the SAME slot is allowed.
    let temp_is_temp = is_temp_runner("test-9877");
    assert!(temp_is_temp, "test-* must be temp");
    let temp = start_provenance_gate(temp_is_temp, 7, prov.as_ref());
    assert_eq!(temp.expect("temp start must be allowed"), None);
}

// =========================================================================
// unverified_exe_gate — "refuse stale" on the non-pool artifact
// =========================================================================

fn resolved(
    origin: ExeOrigin,
    provenance: Option<BuildProvenance>,
    mtime: Option<std::time::SystemTime>,
) -> ResolvedRunnerExe {
    ResolvedRunnerExe {
        path: std::path::PathBuf::from("/ws/qontinui-runner/target/debug/q.exe"),
        origin,
        mtime,
        provenance,
        unverified_warning: None,
    }
}

fn prov(source: BuildSource) -> BuildProvenance {
    BuildProvenance {
        sha: Some("a".repeat(40)),
        source,
        built_from: "/ws/qontinui-runner".to_string(),
        built_at: "2026-08-06T02:54:00Z".to_string(),
    }
}

/// The defect itself: a non-pool artifact with NO build identity must be
/// refused, not launched — and the refusal must name the path, the mtime
/// and the (absent) identity so the operator can see what was rejected.
#[test]
fn unverified_exe_gate_refuses_an_identity_less_non_pool_exe() {
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000);
    let r = resolved(
        ExeOrigin::CargoTargetDir(TargetDirSource::WorkspaceDefault),
        None,
        Some(mtime),
    );
    let err = unverified_exe_gate(false, &r).expect_err("unknown identity must refuse");
    match &err {
        SupervisorError::UnverifiedExe(info) => {
            let crate::error::UnverifiedExeInfo {
                path,
                mtime,
                build_sha,
                build_source,
                target_dir_source,
                detail,
            } = info.as_ref();
            assert!(path.contains("q.exe"), "names the path: {path}");
            assert!(mtime.is_some(), "carries the mtime");
            assert_eq!(*build_sha, None);
            assert_eq!(*build_source, None);
            assert_eq!(target_dir_source, "workspace_default");
            assert!(detail.contains("ABSENT"), "identity reads absent: {detail}");
            assert!(
                detail.contains("allow_stale_fallback"),
                "names the opt-in: {detail}"
            );
        }
        other => panic!("expected UnverifiedExe, got {other:?}"),
    }
    // 409, not 500: the request is fine, the on-disk state is not.
    let (status, body) = err.to_status_body();
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error"], "unverified_runner_exe");
}

/// Absence is refused; so is POSITIVE evidence of a foreign tree. Only a
/// vouched build passes silently.
#[test]
fn unverified_exe_gate_refuses_an_override_built_non_pool_exe() {
    let r = resolved(
        ExeOrigin::CargoTargetDir(TargetDirSource::CargoTargetDirEnv),
        Some(prov(BuildSource::Override)),
        None,
    );
    let err = unverified_exe_gate(false, &r).expect_err("override identity must refuse");
    let msg = err.to_string();
    assert!(msg.contains("override"), "names the foreign source: {msg}");
}

#[test]
fn unverified_exe_gate_allows_a_vouched_non_pool_exe() {
    for source in [BuildSource::LiveTree, BuildSource::OriginMain] {
        let r = resolved(
            ExeOrigin::CargoTargetDir(TargetDirSource::CargoTargetDirEnv),
            Some(prov(source)),
            None,
        );
        assert_eq!(
            unverified_exe_gate(false, &r).expect("vouched identity must pass"),
            ExeIdentityVerdict::Verified,
            "{source:?} is a vouched supervisor build"
        );
    }
}

/// The opt-in does not go quiet — it STATES the staleness, so a caller that
/// deliberately asked for "whatever exists" still cannot mistake the result
/// for evidence about a branch.
#[test]
fn unverified_exe_gate_opt_in_allows_but_states_the_staleness() {
    let r = resolved(
        ExeOrigin::CargoTargetDir(TargetDirSource::WorkspaceDefault),
        None,
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_780_000_000)),
    );
    match unverified_exe_gate(true, &r).expect("opt-in must allow") {
        ExeIdentityVerdict::AllowedByOptIn(msg) => {
            assert!(msg.contains("UNVERIFIED"), "states it is unverified: {msg}");
            assert!(msg.contains("q.exe"), "names the path: {msg}");
            assert!(
                msg.contains("workspace_default"),
                "names which path won: {msg}"
            );
        }
        other => panic!("expected AllowedByOptIn, got {other:?}"),
    }
}

/// Slot artifacts keep their existing posture — this gate must not
/// second-guess `start_provenance_gate` and brick the normal path.
#[test]
fn unverified_exe_gate_never_touches_slot_or_pinned_artifacts() {
    for origin in [ExeOrigin::Slot(1), ExeOrigin::PinnedOverride] {
        let r = resolved(origin, None, None);
        assert_eq!(
            unverified_exe_gate(false, &r).expect("must not refuse"),
            ExeIdentityVerdict::Verified,
            "{origin:?} is governed elsewhere"
        );
    }
}

#[test]
fn exe_origin_labels_name_which_path_won() {
    assert_eq!(ExeOrigin::Slot(2).label(), "slot-2");
    assert_eq!(
        ExeOrigin::CargoTargetDir(TargetDirSource::CargoTargetDirEnv).label(),
        "cargo_target_dir:cargo_target_dir_env"
    );
    assert_eq!(ExeOrigin::PinnedOverride.label(), "pinned_override");
}

// =========================================================================
// pick_slot_decision — guards that the sidecar instrumentation didn't shift
// resolution behavior. Slot selection must remain:
//   1. last_successful_slot (if its exe exists)
//   2. first slot by iteration order whose exe exists
//   3. None
// =========================================================================

fn fake_slots(ids_with_paths: &[(usize, &str)]) -> Vec<(usize, std::path::PathBuf)> {
    ids_with_paths
        .iter()
        .map(|(id, p)| (*id, std::path::PathBuf::from(p)))
        .collect()
}

/// last_successful_slot wins when its exe exists, even if other slots also have exes.
#[test]
fn pick_decision_prefers_last_successful_slot() {
    let slots = fake_slots(&[(0, "/a"), (1, "/b"), (2, "/c")]);
    let picked = pick_slot_decision(Some(1), &slots, |p| {
        p == std::path::Path::new("/a")
            || p == std::path::Path::new("/b")
            || p == std::path::Path::new("/c")
    });
    assert_eq!(picked, Some(1));
}

/// last_successful_slot is recorded but its exe is missing — fall through to
/// first-by-index scan. This is the multi-slot-staleness scenario the
/// memory was written about.
#[test]
fn pick_decision_falls_through_when_recorded_slot_missing() {
    let slots = fake_slots(&[(0, "/a"), (1, "/b"), (2, "/c")]);
    // Recorded slot is 2, but only slots 0 and 1 have exes.
    let picked = pick_slot_decision(Some(2), &slots, |p| {
        p == std::path::Path::new("/a") || p == std::path::Path::new("/b")
    });
    // Scan returns first-by-index, NOT newest-by-anything.
    assert_eq!(picked, Some(0));
}

/// No last_successful_slot, scan picks the lowest-id slot with an exe
/// (this is exactly the silent-staleness quirk the sidecar surfaces).
#[test]
fn pick_decision_scan_returns_first_by_index() {
    let slots = fake_slots(&[(0, "/a"), (1, "/b"), (2, "/c")]);
    let picked = pick_slot_decision(None, &slots, |p| p == std::path::Path::new("/b"));
    assert_eq!(picked, Some(1));
    // Even if multiple slots have exes, the lower id still wins.
    let picked2 = pick_slot_decision(None, &slots, |p| {
        p == std::path::Path::new("/b") || p == std::path::Path::new("/c")
    });
    assert_eq!(picked2, Some(1));
}

/// No exe anywhere — None, caller falls back to legacy.
#[test]
fn pick_decision_none_when_no_exe_exists() {
    let slots = fake_slots(&[(0, "/a"), (1, "/b")]);
    let picked = pick_slot_decision(Some(0), &slots, |_| false);
    assert_eq!(picked, None);
    let picked2 = pick_slot_decision(None, &slots, |_| false);
    assert_eq!(picked2, None);
}

/// last_successful_slot points at an id NOT in the slots list (e.g. stale
/// state after pool size shrink) — must fall through cleanly, not panic.
#[test]
fn pick_decision_handles_unknown_recorded_slot() {
    let slots = fake_slots(&[(0, "/a")]);
    let picked = pick_slot_decision(Some(99), &slots, |p| p == std::path::Path::new("/a"));
    assert_eq!(picked, Some(0));
}

// =========================================================================
// Legacy target/debug/ staleness detection
// (feedback_runner_manual_build — sibling failure mode of slot drift)
//
// The pure comparison logic (`compute_target_debug_staleness`) is exercised
// with synthetic SystemTime values so the staleness rule can be tested
// without depending on filesystem mtime resolution. The I/O wrapper
// (`detect_target_debug_staleness`) gets one round-trip sanity test
// against a real tempdir to guard the read path.
// =========================================================================

fn legacy_p() -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp/qontinui-runner/target/debug/qontinui-runner.exe")
}

/// Legacy mtime strictly older than every slot mtime — staleness fires.
#[test]
fn target_debug_staleness_fires_when_older_than_all_slots() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let legacy = Some(t0);
    // Two slots, both newer than legacy.
    let slots = vec![
        Some(t0 + Duration::from_secs(3600)),
        Some(t0 + Duration::from_secs(7200)),
    ];
    let s = compute_target_debug_staleness(&legacy_p(), legacy, &slots)
        .expect("legacy older than every slot must surface staleness");
    assert_eq!(s.legacy_mtime, t0);
    // oldest_slot_mtime is the OLDER of the two slot mtimes.
    assert_eq!(s.oldest_slot_mtime, t0 + Duration::from_secs(3600));
}

/// Legacy exe doesn't exist (or its mtime read failed) — silent.
#[test]
fn target_debug_staleness_silent_when_no_legacy() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slots = vec![Some(t0)];
    assert!(
        compute_target_debug_staleness(&legacy_p(), None, &slots).is_none(),
        "missing legacy must yield None"
    );
}

// ── "A rebuild must take the latest code" ──────────────────────────
//
// The inverse of the staleness check above: the operator built fresh code
// locally and the supervisor was about to run an older slot exe instead.
// Only the stale-legacy direction was ever detected, which is why two
// consecutive operator rebuilds silently ran 17.5-hour-old code with no
// log line anywhere saying so.

fn t(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

/// THE DEFECT. A local build newer than the picked slot must be detected.
#[test]
fn pool_behind_local_build_detects_a_newer_local_build() {
    let f = compute_pool_behind_local_build(&legacy_p(), Some(t(2_000)), 0, Some(t(1_000)))
        .expect("newer legacy than picked slot must be a finding");
    assert_eq!(f.picked_slot_id, 0);
    assert_eq!(f.legacy_mtime, t(2_000));
    assert_eq!(f.picked_slot_mtime, t(1_000));
}

/// The healthy case: the slot IS the freshest artifact. Silent.
#[test]
fn pool_behind_local_build_silent_when_slot_is_newer() {
    assert!(
        compute_pool_behind_local_build(&legacy_p(), Some(t(1_000)), 0, Some(t(2_000))).is_none()
    );
}

/// Equal mtimes are one build wave, not a finding (mirrors the staleness
/// check's strict comparison).
#[test]
fn pool_behind_local_build_equal_mtimes_are_not_a_finding() {
    assert!(
        compute_pool_behind_local_build(&legacy_p(), Some(t(1_000)), 0, Some(t(1_000))).is_none()
    );
}

/// An unknown timestamp is never reported as a finding.
#[test]
fn pool_behind_local_build_silent_on_unreadable_mtimes() {
    assert!(compute_pool_behind_local_build(&legacy_p(), None, 0, Some(t(1_000))).is_none());
    assert!(compute_pool_behind_local_build(&legacy_p(), Some(t(1_000)), 0, None).is_none());
}

/// A vouched sidecar at least as new as the exe is adoptable — this is what
/// makes the operator's rebuild actually run.
#[test]
fn a_vouched_sidecar_describing_this_exe_is_adoptable() {
    for source in [BuildSource::LiveTree, BuildSource::OriginMain] {
        assert!(
            local_build_is_adoptable(Some(&prov(source)), Some(t(2_000)), t(2_000)),
            "{source:?} sidecar written with the exe must be adoptable"
        );
        assert!(local_build_is_adoptable(
            Some(&prov(source)),
            Some(t(2_001)),
            t(2_000)
        ));
    }
}

/// No sidecar ⇒ never adopted. mtime says WHEN a file was written, not what
/// is in it, so an unidentified artifact is never promoted over a slot.
#[test]
fn an_unstamped_local_build_is_never_adopted() {
    assert!(!local_build_is_adoptable(None, Some(t(2_000)), t(2_000)));
    assert!(!local_build_is_adoptable(None, None, t(2_000)));
}

/// A foreign `override` tree is not vouched, so it cannot be adopted even
/// with a perfectly fresh sidecar.
#[test]
fn an_override_tree_build_is_not_adoptable() {
    assert!(!local_build_is_adoptable(
        Some(&prov(BuildSource::Override)),
        Some(t(2_000)),
        t(2_000)
    ));
}

/// **The `tauri dev` trap.** `npm run tauri dev` rebuilds the SAME path
/// without `custom-protocol` (blank window, frontendReady:false — observed
/// 2026-08-05) and writes no sidecar. If it overwrites an exe whose older
/// sidecar is still lying around, adopting on the strength of that stamp
/// would launch a dev-mode binary as the primary. A sidecar older than the
/// exe it claims to describe must be refused.
#[test]
fn a_sidecar_older_than_the_exe_is_refused() {
    assert!(!local_build_is_adoptable(
        Some(&prov(BuildSource::LiveTree)),
        Some(t(1_999)),
        t(2_000)
    ));
}

/// **The interaction that makes adoption actually work.** Resolution and
/// the START GATE are two separate predicates, and adoption is only useful
/// if they agree: `resolve_source_exe_detailed` hands back an adopted local
/// build as `ExeOrigin::CargoTargetDir`, and the start path routes exactly
/// that origin into [`unverified_exe_gate`], which REFUSES a non-pool exe
/// it cannot identify. If the gate refused what resolution adopted, the
/// primary would resolve a binary and then fail to launch it at all —
/// strictly worse than the stale-code bug this change fixes.
///
/// They agree because both key on the same fact: `local_build_is_adoptable`
/// requires `source.is_vouched()`, and that is precisely the gate's allow
/// condition. This test pins the agreement so a later tightening of the
/// gate cannot silently brick the start path.
#[test]
fn an_adopted_local_build_passes_the_unverified_exe_gate() {
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000);
    for source in [BuildSource::LiveTree, BuildSource::OriginMain] {
        let p = prov(source);
        assert!(
            local_build_is_adoptable(Some(&p), Some(mtime), mtime),
            "{source:?} must be adoptable"
        );
        let resolved = ResolvedRunnerExe {
            mtime: Some(mtime),
            path: legacy_p(),
            // The origin resolution ACTUALLY hands back for an adoption.
            // Pinning `CargoTargetDir` here would test the gate against a
            // shape the start path never sees.
            origin: ExeOrigin::AdoptedLocalBuild(TargetDirSource::WorkspaceDefault),
            provenance: Some(p),
            unverified_warning: None,
        };
        assert_eq!(
            unverified_exe_gate(false, &resolved).expect("must not refuse an adopted build"),
            ExeIdentityVerdict::Verified,
            "{source:?}: resolution adopted it, so the gate must not refuse it"
        );
    }
}

/// **Cross-repo contract.** This is the literal sidecar `dev-start.ps1`
/// writes (captured from a real run, 2026-08-29). If either side drifts —
/// a serde rename here, a key change there — adoption silently stops
/// firing and the operator is back to running stale code with no error
/// anywhere. Parsing the real bytes is the only thing that catches it.
#[test]
fn the_sidecar_dev_start_writes_deserializes_as_provenance() {
    let written = r#"{"sha":"65082b7f50fdeb26c2e9105695a017b40be4d764","source":"live_tree","built_from":"D:\\qontinui-root\\qontinui-runner","built_at":"2026-08-29T09:47:40.6091322Z"}"#;
    let p: BuildProvenance =
        serde_json::from_str(written).expect("dev-start.ps1's sidecar must parse");
    assert_eq!(p.source, BuildSource::LiveTree);
    assert!(p.source.is_vouched(), "live_tree must be adoptable");
    assert_eq!(
        p.sha.as_deref(),
        Some("65082b7f50fdeb26c2e9105695a017b40be4d764")
    );
    assert_eq!(p.built_from, r"D:\qontinui-root\qontinui-runner");
}

/// **The two slot-less origins must be distinguishable.** Adoption landed
/// reporting itself as `CargoTargetDir`, so nothing separated "the
/// operator's fresh vouched build is running" from "we fell through to the
/// artifact nobody maintains" — and `slot_id().is_none()`, which the
/// `LEGACY_EXE_FALLBACK` dev-state keys on, said the incident had happened
/// in both cases.
#[test]
fn the_two_slotless_origins_report_opposite_legacy_fallback_verdicts() {
    let src = TargetDirSource::WorkspaceDefault;
    let fallthrough = ExeOrigin::CargoTargetDir(src);
    let adopted = ExeOrigin::AdoptedLocalBuild(src);

    // Both are slot-less — which is exactly why the old rule conflated them.
    assert_eq!(fallthrough.slot_id(), None);
    assert_eq!(adopted.slot_id(), None);

    // ...and they now answer the incident question oppositely.
    assert!(fallthrough.is_legacy_fallback());
    assert!(!adopted.is_legacy_fallback());

    // A slot and a pin are never the fallthrough either.
    assert!(!ExeOrigin::Slot(0).is_legacy_fallback());
    assert!(!ExeOrigin::PinnedOverride.is_legacy_fallback());

    // Machine-readable labels differ, so logs and API responses can tell
    // them apart too.
    assert_ne!(fallthrough.label(), adopted.label());
    assert_eq!(adopted.label(), "adopted_local_build:workspace_default");
}

/// Both non-pool origins report WHICH cargo precedence level produced them.
/// `source_exe_json` renders this field, and it returned `null` for an
/// adoption — withholding the env-override-vs-workspace-default split on
/// precisely the path where an operator's own build is running.
#[test]
fn both_non_pool_origins_carry_their_target_dir_source() {
    for src in [
        TargetDirSource::CargoTargetDirEnv,
        TargetDirSource::CargoConfigBuildTargetDir,
        TargetDirSource::WorkspaceDefault,
    ] {
        assert_eq!(
            ExeOrigin::CargoTargetDir(src).target_dir_source(),
            Some(src)
        );
        assert_eq!(
            ExeOrigin::AdoptedLocalBuild(src).target_dir_source(),
            Some(src)
        );
    }
    assert_eq!(ExeOrigin::Slot(1).target_dir_source(), None);
    assert_eq!(ExeOrigin::PinnedOverride.target_dir_source(), None);
}

/// Write a fake local build plus an optional sidecar. The sidecar is
/// written AFTER the exe, so its mtime is >= the exe's — the freshness rule
/// adoption requires.
fn plant_local_build(dir: &std::path::Path, sidecar: Option<BuildSource>) -> std::path::PathBuf {
    let exe = dir.join(crate::config::RUNNER_BIN_NAME);
    std::fs::write(&exe, b"exe").expect("write exe");
    if let Some(source) = sidecar {
        let body = serde_json::to_string(&prov(source)).expect("serialize provenance");
        std::fs::write(dir.join(SLOT_PROVENANCE_SIDECAR_FILENAME), body).expect("write sidecar");
    }
    exe
}

/// Plant a slot exe and backdate it, so the comparison is deterministic
/// regardless of filesystem timestamp granularity.
fn plant_backdated_exe(path: &std::path::Path, body: &[u8]) {
    std::fs::write(path, body).expect("write exe");
    backdate(path);
}

/// Push a file's mtime an hour into the past. Explicit rather than relying
/// on write ORDER: NTFS timestamp granularity is coarse enough that two
/// writes microseconds apart can land on the same mtime, and the comparison
/// under test uses a strict `>`.
fn backdate(path: &std::path::Path) {
    let f = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for backdate");
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3_600))
        .expect("backdate exe");
}

/// **End-to-end composition, on a real tree.** `GET /builds` and the start
/// path both go through this, so a defect here is a defect in what the
/// operator is told AND in which binary runs. A vouched sidecar written
/// with the exe ⇒ adopted.
#[test]
fn evaluate_local_build_adoption_adopts_a_vouched_newer_local_build() {
    let root = tempfile::tempdir().expect("tempdir");
    let local = root.path().join("target").join("debug");
    let slot = root.path().join("slot-0").join("debug");
    std::fs::create_dir_all(&local).expect("mkdir local");
    std::fs::create_dir_all(&slot).expect("mkdir slot");

    let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
    plant_backdated_exe(&slot_exe, b"slot");
    let local_exe = plant_local_build(&local, Some(BuildSource::LiveTree));

    let a = evaluate_local_build_adoption_at(
        &local_exe,
        TargetDirSource::WorkspaceDefault,
        0,
        &slot_exe,
    )
    .expect("a newer local build must produce a finding");
    assert!(a.adopted, "vouched sidecar written with the exe must adopt");
    assert_eq!(a.finding.picked_slot_id, 0);
    assert_eq!(a.target_dir_source, TargetDirSource::WorkspaceDefault);
    assert_eq!(
        a.provenance.as_ref().map(|p| p.source),
        Some(BuildSource::LiveTree)
    );
    let msg = a.message();
    assert!(msg.contains("running the local build"), "{msg}");
}

/// The unstamped case — a hand-run `cargo build`, or `npm run tauri dev`
/// overwriting the path. Still REPORTED (the operator must learn their
/// build is not running), but not adopted.
#[test]
fn evaluate_local_build_adoption_refuses_an_unstamped_newer_local_build() {
    let root = tempfile::tempdir().expect("tempdir");
    let local = root.path().join("target").join("debug");
    let slot = root.path().join("slot-1").join("debug");
    std::fs::create_dir_all(&local).expect("mkdir local");
    std::fs::create_dir_all(&slot).expect("mkdir slot");

    let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
    plant_backdated_exe(&slot_exe, b"slot");
    let local_exe = plant_local_build(&local, None);

    let a = evaluate_local_build_adoption_at(
        &local_exe,
        TargetDirSource::CargoTargetDirEnv,
        1,
        &slot_exe,
    )
    .expect("the finding is reported even when adoption is refused");
    assert!(!a.adopted, "no sidecar must never adopt");
    assert!(a.provenance.is_none());
    let msg = a.message();
    assert!(msg.contains("running the SLOT exe"), "{msg}");
    assert!(msg.contains("NOT what is running"), "{msg}");
}

/// The healthy steady state: the slot IS the freshest artifact. Nothing to
/// report, so `GET /builds` renders `null` rather than a reassuring-looking
/// object.
#[test]
fn evaluate_local_build_adoption_is_silent_when_the_slot_is_newer() {
    let root = tempfile::tempdir().expect("tempdir");
    let local = root.path().join("target").join("debug");
    let slot = root.path().join("slot-0").join("debug");
    std::fs::create_dir_all(&local).expect("mkdir local");
    std::fs::create_dir_all(&slot).expect("mkdir slot");

    let local_exe = plant_local_build(&local, Some(BuildSource::LiveTree));
    // Backdate the LOCAL build this time; the slot is written after it.
    backdate(&local_exe);
    let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
    std::fs::write(&slot_exe, b"slot").expect("write slot exe");

    assert!(evaluate_local_build_adoption_at(
        &local_exe,
        TargetDirSource::WorkspaceDefault,
        0,
        &slot_exe,
    )
    .is_none());
}

/// A missing local build is not a finding — an unreadable mtime is UNKNOWN,
/// and unknown is never rendered as "your build is not running".
#[test]
fn evaluate_local_build_adoption_is_silent_without_a_local_build() {
    let root = tempfile::tempdir().expect("tempdir");
    let slot = root.path().join("slot-0").join("debug");
    std::fs::create_dir_all(&slot).expect("mkdir slot");
    let slot_exe = slot.join(crate::config::RUNNER_BIN_NAME);
    plant_backdated_exe(&slot_exe, b"slot");
    let absent = root.path().join("target").join("debug").join("nope.exe");

    assert!(evaluate_local_build_adoption_at(
        &absent,
        TargetDirSource::WorkspaceDefault,
        0,
        &slot_exe,
    )
    .is_none());
}

/// The two warning texts must be distinguishable: one says the local build
/// IS running, the other says it is NOT. Reporting the wrong one is worse
/// than silence, because the operator would stop looking.
#[test]
fn the_warning_states_which_binary_actually_runs() {
    let f = compute_pool_behind_local_build(&legacy_p(), Some(t(2_000)), 0, Some(t(1_000)))
        .expect("finding");
    let adopted = format_pool_behind_local_build_warning(&f, true);
    let refused = format_pool_behind_local_build_warning(&f, false);
    assert!(adopted.contains("running the local build"), "{adopted}");
    assert!(refused.contains("running the SLOT exe"), "{refused}");
    assert!(refused.contains("NOT what is running"), "{refused}");
    assert_ne!(adopted, refused);
}

/// No slot exes exist — silent (no baseline to compare against).
#[test]
fn target_debug_staleness_silent_when_no_slots() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    // All slot entries are None (no exe present in any slot).
    let slots_all_missing: Vec<Option<std::time::SystemTime>> = vec![None, None];
    assert!(
        compute_target_debug_staleness(&legacy_p(), Some(t0), &slots_all_missing).is_none(),
        "no slot exe means no baseline — must yield None"
    );
    // Truly empty slot list.
    let empty: Vec<Option<std::time::SystemTime>> = vec![];
    assert!(compute_target_debug_staleness(&legacy_p(), Some(t0), &empty).is_none());
}

/// Legacy is newer than at least one slot — silent. That other slot might
/// be stale (PR #34's drift surface, if SHA-distinct), but THIS check
/// only fires when legacy is older than EVERY slot.
#[test]
fn target_debug_staleness_silent_when_legacy_newer_than_any_slot() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let legacy = Some(t0 + Duration::from_secs(60));
    let slots = vec![
        Some(t0), // older than legacy — this is the one that prevents firing
        Some(t0 + Duration::from_secs(3600)),
    ];
    assert!(
        compute_target_debug_staleness(&legacy_p(), legacy, &slots).is_none(),
        "legacy newer than ANY slot must yield None"
    );
}

/// Equal mtimes (legacy == oldest slot) — silent. Strict `<` only.
#[test]
fn target_debug_staleness_silent_when_equal() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let slots = vec![Some(t0)];
    assert!(
        compute_target_debug_staleness(&legacy_p(), Some(t0), &slots).is_none(),
        "equal mtimes must yield None (strict ordering)"
    );
}

/// Mixed slot-readability: some slots have mtimes, some are None (failed
/// reads / missing exes). Only the readable ones contribute to the
/// staleness comparison.
#[test]
fn target_debug_staleness_skips_unreadable_slots() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let legacy = Some(t0);
    let slots = vec![
        None,                                 // slot-0 has no exe
        Some(t0 + Duration::from_secs(3600)), // slot-1 exists, newer
        None,                                 // slot-2 has no exe
    ];
    let s = compute_target_debug_staleness(&legacy_p(), legacy, &slots)
        .expect("legacy older than the one readable slot must fire");
    assert_eq!(s.oldest_slot_mtime, t0 + Duration::from_secs(3600));
}

/// Unreadable legacy mtime in the I/O wrapper — synthetic IO failure →
/// returns None (debug log, no panic). Driven by pointing the function
/// at a path inside a non-existent directory.
#[test]
fn target_debug_staleness_handles_unreadable_mtime() {
    let root = tempfile::TempDir::new().expect("tempdir");
    // legacy_path points inside a directory that doesn't exist —
    // `std::fs::metadata` returns Err with ErrorKind::NotFound.
    let bogus_legacy = root
        .path()
        .join("does-not-exist")
        .join("nested")
        .join("qontinui-runner.exe");
    // Also point slots at non-existent paths — verifies the wrapper
    // returns None without panicking when nothing is readable.
    let bogus_slot = root.path().join("slot-0").join("qontinui-runner.exe");
    let slots: Vec<(usize, &std::path::Path)> = vec![(0, &bogus_slot)];
    assert!(detect_target_debug_staleness(&bogus_legacy, &slots).is_none());
}

/// I/O wrapper sanity: real legacy file + a real slot file with legacy
/// strictly older. Verifies the wrapper threads filesystem reads through
/// to the pure helper correctly.
#[test]
fn target_debug_staleness_io_wrapper_roundtrip() {
    let root = tempfile::TempDir::new().expect("tempdir");
    let legacy = root.path().join("legacy.exe");
    std::fs::write(&legacy, b"old").expect("write legacy");
    // Force >= 50ms gap so even coarse filesystem mtime resolution
    // produces a strict-less-than ordering. NTFS mtime res ~100ns,
    // FAT32 ~2s; we don't ship on FAT32 dev machines.
    std::thread::sleep(Duration::from_millis(50));
    let slot0 = root.path().join("slot-0.exe");
    std::fs::write(&slot0, b"new").expect("write slot");
    let slots: Vec<(usize, &std::path::Path)> = vec![(0, &slot0)];
    let s = detect_target_debug_staleness(&legacy, &slots)
        .expect("legacy older than slot (file-write order) must fire");
    assert_eq!(s.legacy_path, legacy);
    assert!(s.legacy_mtime < s.oldest_slot_mtime);
}

/// Warning message includes the legacy path, both ISO timestamps, and the
/// pointer to feedback_runner_manual_build.
#[test]
fn target_debug_warning_message_shape() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let s = TargetDebugStaleness {
        legacy_path: legacy_p(),
        legacy_mtime: t0,
        oldest_slot_mtime: t0 + Duration::from_secs(3600),
    };
    let msg = format_target_debug_warning(&s);
    assert!(msg.contains("target_debug_staleness"), "{}", msg);
    assert!(msg.contains("qontinui-runner.exe"), "{}", msg);
    assert!(
        msg.contains("feedback_runner_manual_build"),
        "warning must point operator at the relevant memory: {}",
        msg
    );
    assert!(
        msg.contains("spawn-test {rebuild:false}"),
        "warning must name the failure mode: {}",
        msg
    );
}

// ── Phase 3 of `2026-09-03-runner-zombie-serving-watchdog`: the restart
//    funnel can stop-then-start a wedged, adopted primary (closes S2). ──

/// The non-temp automated-restart block, three ways: `Manual` admitted,
/// `Watchdog` (the wire value any caller can claim) still blocked,
/// `ServingWatchdog` (unconstructible from the wire) admitted. Temp
/// runners are never blocked for any source.
#[test]
fn automated_restart_block_admits_manual_and_serving_watchdog_only() {
    use crate::diagnostics::RestartSource;
    assert!(!automated_restart_blocked(false, &RestartSource::Manual));
    assert!(automated_restart_blocked(false, &RestartSource::Watchdog));
    assert!(!automated_restart_blocked(
        false,
        &RestartSource::ServingWatchdog
    ));
    for source in [
        RestartSource::Manual,
        RestartSource::Watchdog,
        RestartSource::ServingWatchdog,
    ] {
        assert!(
            !automated_restart_blocked(true, &source),
            "temp runners are never blocked ({source})"
        );
    }
}

/// The stop predicate keys on ANY evidence of life. The S2 shape —
/// `running=false` (overwritten from a silent `/health`), a PID, a held
/// port — must stop; only the all-negative row skips the stop.
#[test]
fn restart_stop_predicate_keys_on_evidence_of_life_not_on_running() {
    // The S2 wedge: tracked flag false, process alive on the port.
    assert!(restart_should_stop(false, Some(247_696), true, false));
    // Linux adopted wedge: no PID recovered (the health cache's PID
    // recovery is Windows-only), port still held.
    assert!(restart_should_stop(false, None, true, false));
    // A PID with no port: the process exists, stop it.
    assert!(restart_should_stop(false, Some(1), false, false));
    // Tracked running alone (the old predicate) still stops.
    assert!(restart_should_stop(true, None, false, false));
    // A primary booting before it binds: only its own spawn says it is alive.
    assert!(restart_should_stop(false, None, false, true));
    // Nothing to stop.
    assert!(!restart_should_stop(false, None, false, false));
}

/// Truth table of the pure half of the port-held guard: refusal needs BOTH
/// a listener and a positively identified runner image.
#[test]
fn port_held_by_live_runner_truth_table() {
    assert!(port_held_by_live_runner(true, true));
    assert!(!port_held_by_live_runner(true, false));
    assert!(!port_held_by_live_runner(false, true));
    assert!(!port_held_by_live_runner(false, false));
}

/// Negative branch, end to end: a listener on an ephemeral port that is
/// NOT a runner does not trigger the refusal. The test process holds the
/// listener (which the Unix `lsof` probe excludes by PID, so the probe
/// reports no holder) and a spawned `sleep` child stands in for "a live
/// process that is not a qontinui-runner" — `is_qontinui_runner_pid` is
/// false for it, which is the identity half of the same predicate. A
/// dead port is the trivially-allowed case.
#[tokio::test]
async fn port_held_guard_does_not_refuse_a_non_runner_listener() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(
        crate::process::port::is_port_listening(port),
        "the bind probe must see our own listener"
    );

    let mut sleeper = tokio::process::Command::new("sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn sleep");
    let sleep_pid = sleeper.id().expect("sleep has a pid");
    assert!(
        !crate::process::proc_kill::is_qontinui_runner_pid(sleep_pid),
        "`sleep` must not read as a qontinui-runner image"
    );

    match refuse_if_port_held_by_live_runner(port).await {
        Ok(()) => {}
        Err(SupervisorError::PortHeldByLiveRunner { .. }) => {
            panic!("a non-runner listener must not produce PortHeldByLiveRunner")
        }
        Err(e) => panic!("unexpected error: {e}"),
    }

    drop(listener);
    let _ = sleeper.kill().await;

    // A port nothing holds is allowed without consulting any probe.
    let dead_port = crate::test_ports::unheld_port();
    assert!(refuse_if_port_held_by_live_runner(dead_port).await.is_ok());
}

/// A supervisor state over an empty runner tree
/// ([`crate::config::config_over_an_empty_runner_tree`]), so a start fails at
/// exe resolution wherever the test process runs. Hold the returned `TempDir`
/// for the test's lifetime.
fn restart_test_state() -> (SharedState, tempfile::TempDir) {
    let (config, root) = crate::config::config_over_an_empty_runner_tree();
    (Arc::new(crate::state::SupervisorState::new(config)), root)
}

/// A registered, UNPROTECTED named runner on a port nothing holds
/// ([`crate::test_ports::unheld_port`]). Unprotected so the readiness gate skips
/// (`NotProtected`) and the funnel reaches the latch; an unheld port so any stop
/// confirms instantly, its reap finds no listener to kill, and the start fails
/// at exe resolution (this state has no build slot and no cargo target-dir
/// artifact).
async fn register_dead_named_runner(state: &SharedState, id: &str) -> Arc<ManagedRunner> {
    register_named_runner_on_port(state, id, crate::test_ports::unheld_port()).await
}

/// [`register_dead_named_runner`] on a port the caller chose.
async fn register_named_runner_on_port(
    state: &SharedState,
    id: &str,
    port: u16,
) -> Arc<ManagedRunner> {
    let config = crate::config::RunnerConfig {
        id: id.to_string(),
        name: format!("Runner {id}"),
        port,
        kind: qontinui_types::wire::runner_kind::RunnerKind::Named {
            name: id.to_string(),
        },
        protected: false,
        server_mode: false,
        restate_ingress_port: None,
        restate_admin_port: None,
        restate_service_port: None,
        external_restate_admin_url: None,
        external_restate_ingress_url: None,
        extra_env: Default::default(),
        fresh_device_identity: false,
    };
    let managed = Arc::new(ManagedRunner::new(config, false));
    state
        .runners
        .write()
        .await
        .insert(id.to_string(), managed.clone());
    managed
}

/// `restart_requested` is cleared on the FAILURE exit too. A manual
/// restart of a stopped runner whose exe cannot be resolved fails at
/// `start_managed_runner`; before Phase 3 the latch stayed `true` forever
/// (only the success path cleared it), which would have silenced the
/// serving watchdog's `SkipOperatorIntent` after one failed attempt.
#[tokio::test]
async fn failed_restart_clears_restart_requested() {
    use crate::diagnostics::RestartSource;
    let (state, _root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-p3-start-fail").await;

    let err = restart_runner_by_id(
        &state,
        "named-p3-start-fail",
        false,
        RestartSource::Manual,
        false,
        BuildTree::OriginMain,
        CloseIntent::Kill,
        None,
    )
    .await
    .expect_err("no exe can be resolved from an empty project dir");
    assert!(
        !matches!(err, SupervisorError::PortHeldByLiveRunner { .. }),
        "a dead port must not be reported as held: {err}"
    );

    let runner = managed.runner.read().await;
    assert!(
        !runner.restart_requested,
        "restart_requested must not stay latched after a failed start"
    );
    // No evidence of life → the stop was correctly skipped, so the
    // stop-intent marker was never latched.
    assert!(!runner.stop_requested);
}

/// The S2 fix end to end: with `running=false` but a held-port fact in
/// the cached snapshot, the funnel now REACHES `stop_runner_by_id`
/// (evidence: `stop_requested` latched, which only the stop path sets and
/// only a successful spawn clears) before the start — and the latch is
/// still cleared when that start fails.
#[tokio::test]
async fn restart_stops_on_held_port_evidence_even_when_running_is_false() {
    use crate::diagnostics::RestartSource;
    let (state, _root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-p3-s2").await;
    managed.cached_health.write().await.runner_port_open = true;
    assert!(!managed.runner.read().await.running);

    let err = restart_runner_by_id(
        &state,
        "named-p3-s2",
        false,
        RestartSource::Manual,
        false,
        BuildTree::OriginMain,
        CloseIntent::Kill,
        None,
    )
    .await
    .expect_err("the start still fails at exe resolution");
    assert!(
        !matches!(err, SupervisorError::PortHeldByLiveRunner { .. }),
        "{err}"
    );

    let runner = managed.runner.read().await;
    assert!(
        runner.stop_requested,
        "the stop must have been reached on port-held evidence alone"
    );
    assert!(
        !runner.restart_requested,
        "restart_requested must be cleared on the failure exit after a stop"
    );
}

/// No stop removes or stamps a spawn's ring entry. This restart reaches
/// `stop_runner_by_id` on port-held evidence and then fails at exe
/// resolution, before any spawn, so the ring must be exactly as it was.
#[tokio::test]
async fn a_stop_leaves_the_spawn_ring_as_it_found_it() {
    use crate::diagnostics::RestartSource;
    let (state, _root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-ring-stop").await;
    managed.cached_health.write().await.runner_port_open = true;
    let entry = crate::state::SpawnMarker::new(
        chrono::Utc::now() - chrono::Duration::seconds(30),
        Some(4242),
        None,
        None,
    );
    managed.recent_spawns.write().await.push(entry.clone());

    restart_runner_by_id(
        &state,
        "named-ring-stop",
        false,
        RestartSource::Manual,
        false,
        BuildTree::OriginMain,
        CloseIntent::Kill,
        None,
    )
    .await
    .expect_err("the start still fails at exe resolution");
    assert!(
        managed.runner.read().await.stop_requested,
        "the stop must have been reached"
    );
    let ring: Vec<crate::state::SpawnMarker> =
        managed.recent_spawns.read().await.iter().cloned().collect();
    assert_eq!(ring, [entry], "the stop changed the spawn ring");
}

/// A child process that exits 3 at once, killed if the test drops it first.
fn spawn_a_child_that_exits_3() -> tokio::process::Child {
    #[cfg(windows)]
    let child = tokio::process::Command::new("cmd")
        .args(["/C", "exit /b 3"])
        .kill_on_drop(true)
        .spawn();
    #[cfg(not(windows))]
    let child = tokio::process::Command::new("sh")
        .args(["-c", "exit 3"])
        .kill_on_drop(true)
        .spawn();
    child.expect("spawn a child that exits 3")
}

/// How the runner's stop request stands when its process exits.
#[derive(Clone, Copy, Debug)]
enum StopAtExit {
    /// Nobody asked for the exit.
    NotRequested,
    /// A stop latched at this time asked for it.
    RequestedAt(chrono::DateTime<chrono::Utc>),
    /// The flag was latched with no time, as no production path leaves it.
    LatchedWithNoTime,
}

/// The exit monitor stamps the exit on the exited spawn's ring entry with
/// the registry's stop-request time, `Some` when the stop flag was latched as
/// the monitor read it (a failed stop's flag included), and then attaches
/// the panic it parsed at the exit to that same entry. It removes no
/// entry. It names the entry by the spawn's `spawned_at`, handed to it with
/// the child: the registry's pid is no key, since the health cache clears a
/// booting primary's, as it is cleared here.
#[tokio::test]
async fn the_exit_monitor_stamps_the_exit_and_its_stop_request_then_attaches_its_panic() {
    let requested_at = chrono::Utc::now() - chrono::Duration::seconds(2);
    for stop in [
        StopAtExit::RequestedAt(requested_at),
        StopAtExit::NotRequested,
        StopAtExit::LatchedWithNoTime,
    ] {
        let (state, root) = restart_test_state();
        let id = "named-exit-stamp";
        let managed = register_dead_named_runner(&state, id).await;
        let child = spawn_a_child_that_exits_3();
        let pid = child.id().expect("a child not yet waited on has a pid");

        // The panic log that child's boot left, fresh at the exit.
        let panic_dir = root.path().join("panic-logs");
        std::fs::create_dir_all(&panic_dir).expect("mkdir panic-logs");
        let written_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
        std::fs::write(
            panic_dir.join("runner-panic.log"),
            format!(
                "[{written_at}] PANIC runner-id={id} pid={pid} version=1.0.0\n\
                 location: src/main.rs:10:5\n\
                 thread: main\n\
                 payload:\n\
                 boot failed\n"
            ),
        )
        .expect("write runner-panic.log");
        *managed.panic_log_dir.write().await = Some(panic_dir);

        // An earlier spawn of another pid, then the child's own spawn.
        let spawned_at = chrono::Utc::now();
        let earlier = crate::state::SpawnMarker::new(
            spawned_at - chrono::Duration::seconds(60),
            Some(pid.wrapping_add(1)),
            None,
            None,
        );
        managed.recent_spawns.write().await.push(earlier.clone());
        managed
            .recent_spawns
            .write()
            .await
            .push(crate::state::SpawnMarker::new(
                spawned_at,
                Some(pid),
                None,
                None,
            ));
        {
            let mut runner = managed.runner.write().await;
            runner.running = true;
            runner.started_at = Some(spawned_at);
            // A health-cache tick before the port bound.
            runner.pid = None;
            match stop {
                StopAtExit::NotRequested => {}
                StopAtExit::RequestedAt(at) => runner.latch_stop_request(at),
                StopAtExit::LatchedWithNoTime => runner.stop_requested = true,
            }
        }

        monitor_runner_process_exit_inner(state.clone(), managed.clone(), child, spawned_at).await;

        let ring: Vec<crate::state::SpawnMarker> =
            managed.recent_spawns.read().await.iter().cloned().collect();
        assert_eq!(
            ring.len(),
            2,
            "{stop:?}: the exit monitor removed an entry: {ring:?}"
        );
        assert_eq!(
            ring[0], earlier,
            "{stop:?}: another pid's entry was stamped"
        );
        let own = &ring[1];
        let exited_at = own
            .exited_at
            .unwrap_or_else(|| panic!("{stop:?}: the child's exit was not stamped: {own:?}"));
        assert!(exited_at >= spawned_at, "{stop:?}: {own:?}");
        let expected = match stop {
            StopAtExit::NotRequested => None,
            StopAtExit::RequestedAt(at) => Some(at),
            StopAtExit::LatchedWithNoTime => Some(exited_at),
        };
        assert_eq!(
            own.stop_requested_at, expected,
            "{stop:?}: the stamp does not carry the stop request: {own:?}"
        );
        assert_eq!(
            own.exit_panic.as_ref().and_then(|panic| panic.pid),
            Some(pid),
            "{stop:?}: the panic parsed at the exit was not attached to the entry its exit \
             stamped: {own:?}"
        );
    }
}

/// `start_managed_runner` records each spawn through
/// `record_spawn_in_registry`: the registry entry gets its pid and start time,
/// and the previous process's stop request is cleared; the ring's newest entry
/// is exactly `SpawnMarker::new(started_at, pid, early_log_path, initiated_by)`,
/// dated at that same instant, which is now and which the call returns.
#[tokio::test]
async fn a_spawn_is_recorded_on_its_registry_entry_and_in_its_ring_at_one_instant() {
    let (state, root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-record-spawn").await;
    // The previous process's stop request, still latched as its stop left it.
    managed
        .runner
        .write()
        .await
        .latch_stop_request(chrono::Utc::now() - chrono::Duration::seconds(30));
    let earlier = crate::state::SpawnMarker::new(
        chrono::Utc::now() - chrono::Duration::seconds(60),
        Some(1),
        None,
        None,
    );
    managed.recent_spawns.write().await.push(earlier.clone());
    let pid = 4242;
    let early_log = root.path().join("early.log");
    let action = uuid::Uuid::new_v4();

    let before = chrono::Utc::now();
    let spawned_at =
        record_spawn_in_registry(&managed, Some(pid), Some(early_log.clone()), Some(action)).await;
    let after = chrono::Utc::now();

    let started_at = {
        let runner = managed.runner.read().await;
        assert!(runner.running);
        assert_eq!(runner.pid, Some(pid));
        assert!(
            !runner.stop_requested,
            "the next start clears the stop flag"
        );
        assert_eq!(
            runner.stop_requested_at, None,
            "the next start clears the stop request's time"
        );
        runner.started_at.expect("the spawn's start time")
    };
    assert_eq!(spawned_at, started_at, "the identity the monitor is handed");
    assert!(
        before <= started_at && started_at <= after,
        "the spawn is dated {started_at}, not between {before} and {after}"
    );
    let ring: Vec<crate::state::SpawnMarker> =
        managed.recent_spawns.read().await.iter().cloned().collect();
    assert_eq!(
        ring,
        [
            earlier,
            crate::state::SpawnMarker::new(started_at, Some(pid), Some(early_log), Some(action)),
        ],
        "the ring's newest entry is not this spawn as the registry records it"
    );
}

/// `stop_runner_by_id`, through which every restart passes, latches the stop
/// with the time of the request; a second request keeps the first's time,
/// the earliest. A dead, unprotected named runner stops at once.
#[tokio::test]
async fn a_stop_latches_the_time_of_its_request_and_a_later_stop_keeps_it() {
    let (state, _root) = restart_test_state();
    let id = "named-stop-latch";
    let managed = register_dead_named_runner(&state, id).await;

    let before = chrono::Utc::now();
    stop_runner_by_id(&state, id, false, CloseIntent::Kill)
        .await
        .expect("a dead runner stops");
    let after = chrono::Utc::now();
    let first = {
        let runner = managed.runner.read().await;
        assert!(runner.stop_requested);
        runner
            .stop_requested_at
            .expect("the stop latched the time of its request")
    };
    assert!(
        before <= first && first <= after,
        "the request is dated {first}, not between {before} and {after}"
    );

    stop_runner_by_id(&state, id, false, CloseIntent::Kill)
        .await
        .expect("a dead runner stops");
    assert_eq!(
        managed.runner.read().await.stop_requested_at,
        Some(first),
        "a later stop must keep the earliest request's time"
    );
}

/// Both sites that ask a runner to stop latch the request through
/// `RunnerState::latch_stop_request`, which records its time with the flag:
/// `stop_runner_by_id` (driven above) and the max-age sweep's kill, whose
/// endless loop no test can drive. Without the latch, the exit would be
/// stamped as one nobody asked for. A bare `stop_requested = true` would be
/// stamped with the exit's own time instead of the request's (the exit
/// monitor's fallback).
#[test]
fn every_stop_request_latches_its_time() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("process")
            .join("manager.rs"),
    )
    .expect("read manager.rs")
    .replace("\r\n", "\n");
    let src = crate::source_scan::production_span(&src).to_string();
    for sig in [
        "pub async fn stop_runner_by_id(",
        "pub async fn reap_stale_test_runners(",
    ] {
        let body: String = crate::source_scan::fn_body(&src, sig)
            .unwrap_or_else(|| panic!("{sig} exists"))
            .split_whitespace()
            .collect();
        assert_eq!(
            body.matches(".latch_stop_request(chrono::Utc::now());")
                .count(),
            1,
            "{sig} must latch its stop request, with its time, exactly once"
        );
        assert!(
            !body.contains(".stop_requested=true"),
            "{sig} sets the stop flag without its time"
        );
    }
}

/// `start_managed_runner` is the one place a spawn's ring entry is written,
/// through `record_spawn_in_registry` (whose values
/// `a_spawn_is_recorded_on_its_registry_entry_and_in_its_ring_at_one_instant`
/// pins), and the dev-action attribution watcher relies on where. No test drives a
/// start to a successful spawn (every manager test that reaches it fails at
/// exe resolution), so this pins it at source level:
/// - one call per start, after the spawn re-pointed the panic-log dir,
///   cleared the stored panic and re-pointed the early log, and before the
///   exit monitor is spawned, so the entry exists before that monitor can
///   attempt a stamp for it; the start touches the ring nowhere else;
/// - no stop touches the ring;
/// - the exit monitor touches it twice: the exit stamp, before the
///   panic-log parse, then the panic attach, both before the crash-watchdog
///   hook can schedule the next spawn.
#[test]
fn a_spawn_is_recorded_after_its_re_points_and_no_stop_or_exit_removes_its_entry() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("process")
            .join("manager.rs"),
    )
    .expect("read manager.rs")
    .replace("\r\n", "\n");
    let src = crate::source_scan::production_span(&src).to_string();
    // Whitespace dropped, so the checks read the code and not where rustfmt
    // broke its lines.
    let body_of = |sig: &str| -> String {
        crate::source_scan::fn_body(&src, sig)
            .unwrap_or_else(|| panic!("{sig} exists"))
            .split_whitespace()
            .collect()
    };
    const RECORD: &str =
        "letspawned_at=record_spawn_in_registry(managed,pid,early_log_path,initiated_by).await;";

    let start = body_of("pub async fn start_managed_runner(");
    assert_eq!(
        start.matches(RECORD).count(),
        1,
        "one recorded spawn per start"
    );
    assert!(
        !start.contains("recent_spawns.write()"),
        "the start writes the ring only through record_spawn_in_registry"
    );
    let record = start.find(RECORD).expect("the call");
    for re_point in [
        "managed.panic_log_dir.write()",
        "managed.recent_panic.write()",
        "managed.early_log_path.write()",
    ] {
        let last = start
            .rfind(re_point)
            .unwrap_or_else(|| panic!("start_managed_runner re-points {re_point}"));
        assert!(
            last < record,
            "the spawn must be recorded after the start's last {re_point}"
        );
    }
    let monitor_spawned = start
        .find("monitor_runner_process_exit(")
        .expect("start_managed_runner spawns the exit monitor");
    assert!(
        record < monitor_spawned,
        "the spawn must be recorded before the exit monitor that stamps it is spawned"
    );

    let stop = body_of("pub async fn stop_runner_by_id(");
    assert!(
        !stop.contains("recent_spawns.write()"),
        "stop_runner_by_id must leave the spawn ring alone"
    );

    let monitor = body_of("async fn monitor_runner_process_exit_inner(");
    assert_eq!(
        monitor.matches("recent_spawns").count(),
        2,
        "the exit monitor touches the ring twice: the exit stamp and the panic attach"
    );
    let stamp = monitor
        .find(".recent_spawns.write().await.stamp_exit(")
        .expect("the exit monitor stamps the exited spawn's entry");
    let attach = monitor
        .find(".recent_spawns.write().await.attach_exit_panic(")
        .expect("the exit monitor attaches the panic parsed at the exit");
    let parse = monitor
        .find("check_and_record_panic_log(")
        .expect("the exit monitor parses the panic log");
    let watchdog = monitor
        .find("maybe_crash_restart(")
        .expect("the exit monitor calls the crash-watchdog hook");
    assert!(
        stamp < parse,
        "the exit stamp must come before the panic-log parse, which would widen the lag \
         in which an exited process reads as up"
    );
    assert!(
        stamp < watchdog && attach < watchdog,
        "both ring writes must come before the crash watchdog can push the next spawn's entry"
    );
}

/// A child that stays up until its stdin closes, killed if the test drops it.
fn spawn_a_child_that_exits_when_its_stdin_closes() -> (tokio::process::Child, u32) {
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("cmd");
        command.args(["/C", "set /p unused="]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "read unused"]);
        command
    };
    let child = command
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn a child that waits on its stdin");
    let pid = child.id().expect("a child not yet waited on has a pid");
    (child, pid)
}

/// The primary, registered on a port nothing holds
/// ([`crate::test_ports::unheld_port`]).
async fn register_dead_primary(state: &SharedState) -> Arc<ManagedRunner> {
    let mut config = crate::config::RunnerConfig::default_primary();
    config.port = crate::test_ports::unheld_port();
    let id = config.id.clone();
    let managed = Arc::new(ManagedRunner::new(config, false));
    state.runners.write().await.insert(id, managed.clone());
    managed
}

/// M10's root: the exit monitor of an old spawn reaches its critical section
/// after a new spawn of the runner was recorded. It leaves that spawn's
/// `running`, `pid` and `started_at`, and a primary's legacy state, alone, and
/// stamps its own entry. B's child is B's monitor's, so nothing A's monitor does
/// can drop it: B's exit is still observed. And the monitor of the current spawn
/// does reset both, the legacy state too, whose `started_at` is a separate clock
/// read that never equals `spawned_at`.
#[tokio::test]
async fn a_late_exit_monitor_leaves_a_newer_spawn_running() {
    for is_primary in [false, true] {
        let (state, _root) = restart_test_state();
        let managed = if is_primary {
            register_dead_primary(&state).await
        } else {
            register_dead_named_runner(&state, "named-late-exit").await
        };

        // Spawn A, and its monitor, waiting on A's child.
        let (mut child_a, pid_a) = spawn_a_child_that_exits_when_its_stdin_closes();
        let stdin_a = child_a.stdin.take().expect("A's stdin");
        let spawned_a = record_spawn_in_registry(&managed, Some(pid_a), None, None).await;
        let monitor_a = tokio::spawn(monitor_runner_process_exit_inner(
            state.clone(),
            managed.clone(),
            child_a,
            spawned_a,
        ));

        // Spawn B, which stays up. The ring keys the two apart, so make sure
        // the clock moved.
        while chrono::Utc::now() == spawned_a {
            tokio::task::yield_now().await;
        }
        let (mut child_b, pid_b) = spawn_a_child_that_exits_when_its_stdin_closes();
        let stdin_b = child_b.stdin.take().expect("B's stdin");
        let spawned_b = record_spawn_in_registry(&managed, Some(pid_b), None, None).await;
        let legacy_b = spawned_b + chrono::Duration::milliseconds(1);
        if is_primary {
            let mut legacy = state.runner.write().await;
            legacy.running = true;
            legacy.pid = Some(pid_b);
            legacy.started_at = Some(legacy_b);
        }

        // Only now does A exit.
        drop(stdin_a);
        monitor_a.await.expect("A's monitor");

        {
            let runner = managed.runner.read().await;
            assert_eq!(
                (runner.running, runner.pid, runner.started_at),
                (true, Some(pid_b), Some(spawned_b)),
                "primary={is_primary}: A's monitor reset the newer spawn B"
            );
        }
        if is_primary {
            let legacy = state.runner.read().await;
            assert_eq!(
                (legacy.running, legacy.pid, legacy.started_at),
                (true, Some(pid_b), Some(legacy_b)),
                "A's monitor reset the legacy primary state of the newer spawn B"
            );
        }
        {
            let ring = managed.recent_spawns.read().await;
            let entries: Vec<&crate::state::SpawnMarker> = ring.iter().collect();
            assert!(
                entries[0].exited_at.is_some(),
                "primary={is_primary}: A's exit was not stamped on A: {entries:?}"
            );
            assert_eq!(
                entries[1].exited_at, None,
                "primary={is_primary}: A's exit was stamped on B: {entries:?}"
            );
        }

        // B's own exit is observed, and resets both.
        let monitor_b = tokio::spawn(monitor_runner_process_exit_inner(
            state.clone(),
            managed.clone(),
            child_b,
            spawned_b,
        ));
        drop(stdin_b);
        monitor_b.await.expect("B's monitor");
        {
            let runner = managed.runner.read().await;
            assert_eq!(
                (runner.running, runner.pid),
                (false, None),
                "primary={is_primary}: the current spawn's monitor did not reset it"
            );
        }
        if is_primary {
            let legacy = state.runner.read().await;
            assert_eq!(
                (legacy.running, legacy.pid),
                (false, None),
                "the current spawn's monitor did not reset the legacy primary state"
            );
        }
        let ring = managed.recent_spawns.read().await;
        assert!(
            ring.newest().expect("B's entry").exited_at.is_some(),
            "primary={is_primary}: B's exit was not observed"
        );
    }
}

/// The crash watchdog's backoff re-check: someone starting the runner during
/// the backoff stops the restart, even when `running` reads false, as it does
/// for a primary that has not answered `/health` yet.
#[test]
fn the_crash_watchdog_skips_a_restart_when_the_runner_was_started_during_its_backoff() {
    let exited = chrono::Utc::now() - chrono::Duration::seconds(30);
    let newer = exited + chrono::Duration::seconds(10);
    // (running, stop_requested, started_at, skip)
    let table = [
        (false, false, Some(exited), false),
        (true, false, Some(exited), true),
        (false, true, Some(exited), true),
        (false, false, Some(newer), true),
        (true, false, Some(newer), true),
        (false, false, None, true),
    ];
    for (running, stop_requested, started_at, skip) in table {
        assert_eq!(
            crash_restart_backoff_skips(running, stop_requested, started_at, exited),
            skip,
            "running={running} stop_requested={stop_requested} started_at={started_at:?}"
        );
    }
}

/// A second start of a primary still booting is refused although `running`
/// reads false, as the health cache leaves it until `/health` answers and its
/// port guard passes before the primary binds. Only a spawn that is current and
/// up counts: past a stop's reset, a stamped exit or a dead pid, the start goes
/// on, here to fail at exe resolution.
#[tokio::test]
async fn a_second_start_of_a_primary_still_booting_is_refused() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Newest {
        UpAndCurrent,
        ResetByAStop,
        ExitStamped,
        PidDead,
    }
    for newest in [
        Newest::UpAndCurrent,
        Newest::ResetByAStop,
        Newest::ExitStamped,
        Newest::PidDead,
    ] {
        let (state, _root) = restart_test_state();
        let managed = register_dead_primary(&state).await;
        let (mut child, pid) = spawn_a_child_that_exits_when_its_stdin_closes();
        if newest == Newest::PidDead {
            drop(child.stdin.take());
            child.wait().await.expect("reap the child");
        }
        let spawned_at = record_spawn_in_registry(&managed, Some(pid), None, None).await;
        // A health-cache tick before the primary answers.
        managed.runner.write().await.running = false;
        match newest {
            Newest::ResetByAStop => managed.runner.write().await.started_at = None,
            Newest::ExitStamped => {
                managed.recent_spawns.write().await.stamp_exit(
                    spawned_at,
                    chrono::Utc::now(),
                    None,
                );
            }
            Newest::UpAndCurrent | Newest::PidDead => {}
        }

        let err = start_managed_runner(&state, &managed, None, StartAs::Registered)
            .await
            .expect_err("nothing can be started from an empty runner tree");
        assert_eq!(
            matches!(err, SupervisorError::RunnerAlreadyRunning),
            newest == Newest::UpAndCurrent,
            "{newest:?}: {err}"
        );
    }
}

/// A stop arriving mid-start latches only after that start has finished, and
/// then stops the spawn it recorded. The test's guard stands in for a start. The
/// paused clock runs out with nothing else to do once the stop has blocked as far
/// as it can. On an unheld port, because a dropped ephemeral one could be handed
/// to a concurrent test's listener, which this stop's reap would then tree-kill
/// along with this test binary.
#[tokio::test(start_paused = true)]
async fn a_stop_waits_for_a_start_to_record_its_spawn() {
    let (state, _root) = restart_test_state();
    let id = "named-stop-waits";
    let managed = register_dead_named_runner(&state, id).await;

    let start = managed.lifecycle.lock().await;
    let stop = {
        let state = state.clone();
        tokio::spawn(async move { stop_runner_by_id(&state, id, false, CloseIntent::Kill).await })
    };
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(
        !managed.runner.read().await.stop_requested,
        "the stop latched while a start held the lifecycle lock"
    );
    tokio::time::resume();

    // What the start records while the stop waits. Its stdin stays open past the
    // stop, because the monitor's wait would otherwise close it and end the child
    // with no help from the stop.
    let (mut child, pid) = spawn_a_child_that_exits_when_its_stdin_closes();
    let _stdin = child.stdin.take().expect("the child's stdin");
    let spawned_at = record_spawn_in_registry(&managed, Some(pid), None, None).await;
    let monitor = tokio::spawn(monitor_runner_process_exit_inner(
        state.clone(),
        managed.clone(),
        child,
        spawned_at,
    ));
    drop(start);

    stop.await.expect("the stop").expect("the stop succeeds");
    tokio::time::timeout(Duration::from_secs(20), monitor)
        .await
        .expect("the stop did not end the recorded spawn")
        .expect("its monitor");
    assert!(
        managed.runner.read().await.stop_requested,
        "the start's record cleared the stop's latch"
    );
    let ring = managed.recent_spawns.read().await;
    let entry = ring.newest().expect("the spawn");
    assert_eq!(entry.spawned_at, spawned_at);
    assert!(
        entry.stop_requested_at.is_some(),
        "the stop did not target the recorded spawn: {entry:?}"
    );
}

/// A stop that waited behind a `test-*` teardown finds the entry gone and acts
/// on nothing: a later spawn may hold that port by now.
#[tokio::test(start_paused = true)]
async fn a_stop_that_waited_behind_a_teardown_acts_on_nothing() {
    let (state, _root) = restart_test_state();
    let id = "named-stop-gone";
    let managed = register_dead_named_runner(&state, id).await;

    let teardown = managed.lifecycle.lock().await;
    let stop = {
        let state = state.clone();
        tokio::spawn(async move { stop_runner_by_id(&state, id, false, CloseIntent::Kill).await })
    };
    tokio::time::sleep(Duration::from_secs(600)).await;
    state.runners.write().await.remove(id);
    drop(teardown);

    let err = stop
        .await
        .expect("the stop")
        .expect_err("the entry is gone");
    assert!(matches!(err, SupervisorError::RunnerNotFound(_)), "{err}");
    assert!(
        !managed.runner.read().await.stop_requested,
        "the stop latched on a removed entry"
    );
}

/// The monitor hands `maybe_crash_restart` the latch whatever the stamp kept. A
/// stop dated after the exit leaves the entry's `stop_requested_at` `None`, and
/// the armed watchdog still skips the restart, as it does for an exit the
/// operator was stopping anyway; with no latch the same exit is a crash.
#[tokio::test]
async fn the_exit_monitor_hands_the_crash_watchdog_a_latch_the_stamp_dropped() {
    if crash_restart_env_disabled() {
        panic!("QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1 leaves this test nothing to observe");
    }
    for latched in [true, false] {
        let (mut config, _root) = crate::config::config_over_an_empty_runner_tree();
        config.watchdog_enabled_at_start = true;
        let state: SharedState = Arc::new(crate::state::SupervisorState::new(config));
        let managed = register_dead_named_runner(&state, "named-late-latch").await;
        managed.watchdog.write().await.enabled = true;

        let child = spawn_a_child_that_exits_3();
        let pid = child.id().expect("a child not yet waited on has a pid");
        let spawned_at = record_spawn_in_registry(&managed, Some(pid), None, None).await;
        if latched {
            managed
                .runner
                .write()
                .await
                .latch_stop_request(chrono::Utc::now() + chrono::Duration::hours(1));
        }

        monitor_runner_process_exit_inner(state.clone(), managed.clone(), child, spawned_at).await;

        let ring = managed.recent_spawns.read().await;
        let entry = ring.newest().expect("the spawn");
        assert!(entry.exited_at.is_some());
        assert_eq!(
            entry.stop_requested_at, None,
            "latched={latched}: a stop dated after the exit was stamped as its request"
        );
        assert_eq!(
            managed.watchdog.read().await.restart_attempts,
            u32::from(!latched),
            "latched={latched}: the watchdog must read the latch, not the stamp"
        );
    }
}

/// A runner whose pid is not the newest spawn's is one this supervisor did not
/// start. `RunnerState::process` used to answer this, and read `None` for every
/// runner, since the exit monitor took the child at once.
#[test]
fn a_runner_is_adopted_when_its_pid_is_not_the_newest_spawns() {
    let mut ring = crate::state::SpawnRing::default();
    assert!(runner_is_adopted(Some(7), &ring), "no spawn yet");
    assert!(runner_is_adopted(None, &ring), "no spawn yet, no pid");

    let at = chrono::Utc::now();
    ring.push(crate::state::SpawnMarker::new(at, Some(1), None, None));
    ring.push(crate::state::SpawnMarker::new(at, Some(7), None, None));
    assert!(!runner_is_adopted(Some(7), &ring), "the newest spawn's pid");
    assert!(runner_is_adopted(Some(1), &ring), "an older spawn's pid");
    assert!(runner_is_adopted(Some(9), &ring), "nobody's pid");
    assert!(
        !runner_is_adopted(None, &ring),
        "a cleared pid says nothing: PID recovery is Windows-only, so off Windows a spawned \
         runner would read as adopted for life"
    );
}

/// Every stretch that must hold a runner's lifecycle lock, at source level: no
/// unit test can start a real runner (#253), and the sweep is an endless loop.
#[test]
fn the_lifecycle_lock_spans_the_start_and_the_stop() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("process")
            .join("manager.rs"),
    )
    .expect("read manager.rs")
    .replace("\r\n", "\n");
    let src = crate::source_scan::production_span(&src).to_string();
    let body_of = |sig: &str| -> String {
        crate::source_scan::fn_body(&src, sig)
            .unwrap_or_else(|| panic!("{sig} exists"))
            .split_whitespace()
            .collect()
    };
    let in_order = |sig: &str, body: &str, marks: &[&str]| {
        let mut last = 0;
        for mark in marks {
            let at = body[last..]
                .find(mark)
                .unwrap_or_else(|| panic!("{sig}: no {mark} after byte {last}"));
            last += at + mark.len();
        }
    };

    // The guard is bound by this exact spelling, so `let _ = ...lock().await`, a bare
    // statement or another name fails to find it. That name appears once, so nothing
    // drops, moves or rebinds it. And no brace closes its scope before the last
    // thing it must span, so no block releases it early.
    //
    // What this cannot see: `managed` shadowed to another runner, a helper that took
    // the guard's mutex some other way, and anything at run time. A cancelled future
    // does release the guard, wherever it was. The DELETE route and the purge are
    // checked for their binding at the end of this test, not for their span.
    let guard_spans = |sig: &str, body: &str, lock: &str, last: &str| {
        assert_eq!(
            body.matches("_lifecycle").count(),
            1,
            "{sig} must bind its lifecycle guard once and not touch it again"
        );
        let from = body
            .find(lock)
            .unwrap_or_else(|| panic!("{sig}: no {lock}"))
            + lock.len();
        let to = body
            .rfind(last)
            .unwrap_or_else(|| panic!("{sig}: no {last}"));
        assert!(from < to, "{sig}: {last} must come after the guard");
        let mut depth = 0i32;
        for c in body[from..to].chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "{sig}: the guard's scope closes before {last}");
        }
    };

    const LOCK: &str = "let_lifecycle=managed.lifecycle.lock().await;";
    let start = body_of("pub async fn start_managed_runner(");
    assert_eq!(start.matches(LOCK).count(), 1, "the start takes it once");
    in_order(
        "start_managed_runner",
        &start,
        &[
            LOCK,
            ".or_insert_with(",
            "ifrunner.running||newest_spawn_is_up(",
            "record_spawn_in_registry(",
            "monitor_runner_process_exit(",
        ],
    );
    guard_spans(
        "start_managed_runner",
        &start,
        LOCK,
        "monitor_runner_process_exit(",
    );

    let stop = body_of("pub async fn stop_runner_by_id(");
    assert_eq!(stop.matches(LOCK).count(), 1, "the stop takes it once");
    in_order(
        "stop_runner_by_id",
        &stop,
        &[
            LOCK,
            ".latch_stop_request(",
            "runner.started_at=None;",
            "reap_instance_state_on_stop(",
            "remove_runner_exe_copy(",
        ],
    );
    guard_spans("stop_runner_by_id", &stop, LOCK, "remove_runner_exe_copy(");

    let sweep = body_of("pub async fn reap_stale_test_runners(");
    in_order(
        "reap_stale_test_runners",
        &sweep,
        &[
            "letSome(_lifecycle)=try_lock_registered(&state,managed).awaitelse{",
            "continue;",
            ".latch_stop_request(",
            "runners_map.remove(&id);",
            "remove_runner_exe_copy(",
        ],
    );
    assert!(
        !sweep.contains(".lifecycle.lock()"),
        "the sweep must never wait for a runner's lifecycle lock"
    );
    guard_spans(
        "reap_stale_test_runners",
        &sweep,
        "letSome(_lifecycle)=try_lock_registered(&state,managed).await",
        "remove_runner_exe_copy(",
    );

    for sig in [
        "async fn monitor_runner_process_exit_inner(",
        "async fn restart_after_gate(",
        "pub async fn restart_runner_by_id(",
        "pub async fn admit_restart(",
        "pub async fn run_admitted_restart(",
        "async fn serving_watchdog_restart(",
        "async fn maybe_crash_restart(",
        "async fn crash_watchdog_start(",
        "async fn start_as_spawn_trial(",
        "pub async fn boot_start_runner(",
    ] {
        assert!(
            !body_of(sig).contains("lifecycle."),
            "{sig} must not take the lifecycle lock: the monitor would stall a stop's \
             graceful wait, and the restart path takes it through its start and stop, \
             which are not reentrant"
        );
    }

    // The ring pid's two rules are table-tested as pure functions, so pin their calls.
    for call in [
        "publishes_discovered_pid(source)",
        "source_names_the_spawn(source)",
    ] {
        assert!(stop.contains(call), "the stop must call {call}");
    }

    // The DELETE route and the purge bind their guard by name, so a `let Some(_)`
    // that dropped it at once fails here.
    let routes = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("routes")
            .join("runners.rs"),
    )
    .expect("read routes/runners.rs");
    assert_eq!(
        routes
            .matches("let Some(_lifecycle) = manager::try_lock_registered(")
            .count(),
        2,
        "DELETE and the purge each hold their guard through their removal"
    );
}

/// Every start a dev action asks for names that action, or the attribution
/// watcher reads no boot for it and the window closes clean. Pinned at source
/// level: a stub exe gets `start_managed_runner` to a recorded spawn at manager
/// level, and a route test through one is yet to be built.
#[test]
fn every_minted_start_names_its_action() {
    let squeezed = |parts: &[&str]| -> String {
        let path = parts.iter().fold(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
            |dir, part| dir.join(part),
        );
        let src = std::fs::read_to_string(&path)
            .expect("read the source")
            .replace("\r\n", "\n");
        crate::source_scan::production_span(&src)
            .split_whitespace()
            .collect()
    };
    let calls = |src: &str, call: &str| -> Vec<String> {
        src.split(call)
            .skip(1)
            .map(|rest| rest.split(".await").next().unwrap_or(rest).to_string())
            .collect()
    };

    // rebuild_when_idle, the detached restart and the synchronous restart.
    let restarts = calls(
        &squeezed(&["src", "routes", "runner.rs"]),
        "manager::restart_runner(",
    );
    assert_eq!(restarts.len(), 3, "{restarts:?}");
    for call in &restarts {
        assert!(
            call.contains(".action_id),"),
            "a restart names no action: {call}"
        );
    }
    // spawn-test, spawn-named and `POST /runners/{id}/start`.
    let spawns = calls(
        &squeezed(&["src", "routes", "runners.rs"]),
        "manager::start_managed_runner(",
    );
    assert_eq!(spawns.len(), 3, "{spawns:?}");
    for call in &spawns {
        assert!(
            call.contains("Some(action_id)") || call.contains("Some(minted.record.action_id)"),
            "a spawn names no action: {call}"
        );
    }
    // The threading between the restart routes and the start.
    let manager = squeezed(&["src", "process", "manager.rs"]);
    for (sig, call, arg) in [
        (
            "pub async fn restart_runner(",
            "restart_runner_by_id(",
            "initiated_by,)",
        ),
        (
            "pub async fn restart_runner_by_id(",
            "run_admitted_restart(",
            "initiated_by)",
        ),
        (
            "pub async fn run_admitted_restart(",
            "restart_after_gate(",
            "initiated_by,&mutacted,)",
        ),
        (
            "async fn restart_after_gate(",
            "start_managed_runner(",
            "initiated_by,",
        ),
    ] {
        let sig: String = sig.split_whitespace().collect();
        let body = &manager[manager.find(&sig).unwrap_or_else(|| panic!("{sig} exists"))..];
        let body = body.split_once('{').expect("a body").1;
        let threaded = calls(body, call);
        assert!(
            threaded.first().is_some_and(|args| args.contains(arg)),
            "{sig} does not thread the initiating action into {call}: {:?}",
            threaded.first()
        );
    }
}

/// The spawn ring entry of every lifecycle door's start names the door's
/// action: each door threads the id it minted into the manager call, and each
/// manager path threads it into `start_managed_runner`, which records it on the
/// ring through `record_spawn_in_registry` (whose entry
/// `a_spawn_is_recorded_on_its_registry_entry_and_in_its_ring_at_one_instant`
/// pins). Pinned at source level because no route test reaches a recorded
/// spawn on every platform: the stub-exe start runs on unix only.
#[test]
fn every_lifecycle_door_names_its_action_on_the_spawn_ring() {
    let squeezed = |parts: &[&str]| -> String {
        let path = parts.iter().fold(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
            |dir, part| dir.join(part),
        );
        let src = std::fs::read_to_string(&path)
            .expect("read the source")
            .replace("\r\n", "\n");
        crate::source_scan::production_span(&src)
            .split_whitespace()
            .collect()
    };
    let body_of = |src: &str, sig: &str| -> String {
        let sig: String = sig.split_whitespace().collect();
        let at = src.find(&sig).unwrap_or_else(|| panic!("{sig} exists"));
        let rest = &src[at..];
        // Up to the next top-level item: good enough to bound the calls below.
        let end = rest[sig.len()..]
            .find("pubasyncfn")
            .or_else(|| rest[sig.len()..].find("asyncfn"))
            .map(|i| i + sig.len())
            .unwrap_or(rest.len());
        rest[..end].to_string()
    };
    let first_call = |body: &str, call: &str| -> String {
        body.split(call)
            .nth(1)
            .unwrap_or_else(|| panic!("no call to {call}"))
            .split(".await")
            .next()
            .unwrap_or_default()
            .to_string()
    };

    let routes = squeezed(&["src", "routes", "runners.rs"]);
    // The per-runner restart door: both arms name the minted action.
    let restart = body_of(&routes, "pub async fn restart_runner(");
    let restarts: Vec<&str> = restart
        .split("manager::restart_runner_by_id(")
        .skip(1)
        .map(|rest| rest.split(".await").next().unwrap_or(rest))
        .collect();
    assert_eq!(restarts.len(), 2, "{restarts:?}");
    for call in &restarts {
        assert!(
            call.contains(".action_id)") && !call.ends_with("None,)"),
            "a per-runner restart names no action: {call}"
        );
    }
    // The per-runner start door.
    let start = first_call(
        &body_of(&routes, "pub async fn start_runner("),
        "manager::start_managed_runner(",
    );
    assert!(
        start.contains("Some(minted.record.action_id)"),
        "the start door names no action: {start}"
    );
    // The rebuild-and-restart door, and its manager body into the start.
    let rar = first_call(
        &body_of(&routes, "pub async fn rebuild_and_restart_runner("),
        "manager::rebuild_and_restart_by_id(",
    );
    assert!(
        rar.contains("initiated_by)"),
        "rebuild-and-restart names no action: {rar}"
    );
    let manager = squeezed(&["src", "process", "manager.rs"]);
    let rar_start = first_call(
        &body_of(&manager, "pub async fn rebuild_and_restart_by_id("),
        "start_managed_runner(",
    );
    assert!(
        rar_start.contains("initiated_by,"),
        "rebuild_and_restart_by_id does not thread its action into the start: {rar_start}"
    );
    // The serving watchdog's restart.
    let watchdog = first_call(
        &body_of(&manager, "async fn serving_watchdog_restart("),
        "run_admitted_restart(",
    );
    assert!(
        watchdog.contains("Some(minted.record.action_id)"),
        "the serving watchdog's restart names no action: {watchdog}"
    );
    assert!(
        body_of(&manager, "pub async fn maybe_serving_restart(")
            .contains("serving_watchdog_restart(&state,&managed,attempt)"),
        "the serving watchdog must restart through its minting helper"
    );
    // The starts nobody waits on: the crash watchdog's and the boot auto-start.
    let trial = first_call(
        &body_of(&manager, "async fn start_as_spawn_trial("),
        "start_managed_runner(",
    );
    assert!(
        trial.contains("Some(minted.record.action_id)"),
        "a spawn trial's start names no action: {trial}"
    );
    assert!(
        body_of(&manager, "async fn maybe_crash_restart(")
            .contains("crash_watchdog_start(&state,&managed,exited_spawned_at,attempt)"),
        "the crash watchdog must restart through its minting helper"
    );
    assert!(
        squeezed(&["src", "main.rs"])
            .contains("process::manager::boot_start_runner(&state_for_autostart,&primary_id)"),
        "the boot auto-start must start through its minting helper"
    );
}

/// No production start or restart of a runner goes unminted, whatever door it
/// comes through: a door added later that writes `None` in a call to one of the
/// functions taking the initiating action fails here, where the door-by-door scan
/// above would not know to look. A `None` held in a variable is not caught. A
/// scan, not a required parameter, because a test driving a start with no trial
/// is legitimate.
#[test]
fn no_production_start_goes_unminted() {
    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("list the source dir") {
            let path = entry.expect("a dir entry").path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    let mut files = Vec::new();
    rs_files(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let sources: Vec<(String, String)> = files
        .iter()
        .map(|file| {
            let src = std::fs::read_to_string(file)
                .expect("read the source")
                .replace("\r\n", "\n");
            // Comment lines out: prose quoting a call, or a `None` beside one, is
            // not a call.
            let squeezed = crate::source_scan::production_span(&src)
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .flat_map(str::split_whitespace)
                .collect();
            (file.display().to_string(), squeezed)
        })
        .collect();
    for needle in [
        "start_managed_runner(",
        "record_spawn_in_registry(",
        "restart_after_gate(",
        "restart_runner_by_id(",
        "run_admitted_restart(",
        "rebuild_and_restart_by_id(",
        "restart_runner(",
    ] {
        let mut calls = 0;
        for (file, squeezed) in &sources {
            for (at, _) in squeezed.match_indices(needle) {
                // The definition, not a call.
                if squeezed[..at].ends_with("fn") {
                    continue;
                }
                let args = squeezed[at + needle.len()..]
                    .split(".await")
                    .next()
                    .unwrap_or_default();
                assert!(
                    !args.contains("None"),
                    "{file}: {needle} names no action: {args}"
                );
                calls += 1;
            }
        }
        // So a renamed function cannot turn this into a scan that finds nothing
        // and passes.
        assert!(calls > 0, "no production call of {needle}");
    }
}

/// A primary back from a long stop: its stamp is the previous boot's, older than any
/// threshold the clamp allows, and this boot started 35 s ago, under the clamp's floor.
/// Past the threshold by the stamp and nowhere near it by its own clock. A booting
/// runner's census is zero, so nothing after this decision would stop the restart.
#[tokio::test]
async fn a_booting_runner_is_not_restarted_for_the_silence_before_it_started() {
    assert!(
        !serving_restart_env_disabled(),
        "QONTINUI_SUPERVISOR_NO_SERVING_RESTART=1 leaves this test nothing to observe"
    );
    let (mut config, _root) = crate::config::config_over_an_empty_runner_tree();
    config.watchdog_enabled_at_start = true;
    let state: SharedState = Arc::new(crate::state::SupervisorState::new(config));
    let managed = register_dead_primary(&state).await;
    managed.watchdog.write().await.enabled = true;

    let previous_boot = chrono::Utc::now() - chrono::Duration::days(2);
    {
        let mut runner = managed.runner.write().await;
        runner.last_seen_responding_at = Some(previous_boot);
        runner.started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(35));
    }
    maybe_serving_restart(
        &state,
        &managed,
        crate::state::RunnerLiveness::UnresponsiveSince(previous_boot),
        true,
    )
    .await;

    let watchdog = managed.watchdog.read().await;
    assert_eq!(watchdog.serving_restart_attempts, 0);
    assert!(watchdog.serving_history.is_empty());
    assert!(!watchdog.serving_restart_in_flight);
}

/// A serving watchdog restart the readiness gate admits is a `restart` trial
/// requested by `watchdog`, minted against the wedged runner, and its failure
/// lands on the trial. A dead primary passes the gate (`runner_not_running`)
/// and fails at exe resolution, so nothing is stopped or started.
#[tokio::test]
async fn serving_watchdog_restart_mints_with_watchdog_initiator() {
    assert!(
        !serving_restart_env_disabled(),
        "QONTINUI_SUPERVISOR_NO_SERVING_RESTART=1 leaves this test nothing to observe"
    );
    let (mut config, _root) = crate::config::config_over_an_empty_runner_tree();
    config.watchdog_enabled_at_start = true;
    assert!(matches!(
        config.ambient_coord,
        crate::config::AmbientCoord::Ignore
    ));
    let state: SharedState = Arc::new(crate::state::SupervisorState::new(config));
    let managed = register_dead_primary(&state).await;
    managed.watchdog.write().await.enabled = true;

    // Silent for two days: past any threshold the clamp allows.
    let since = chrono::Utc::now() - chrono::Duration::days(2);
    maybe_serving_restart(
        &state,
        &managed,
        crate::state::RunnerLiveness::UnresponsiveSince(since),
        false,
    )
    .await;

    let budget = crate::test_clock::poll_budget(Duration::from_secs(30));
    let deadline = std::time::Instant::now() + budget;
    let record = loop {
        let done = state
            .dev_actions
            .read()
            .await
            .recent(10)
            .into_iter()
            .find(|record| record.run_result().is_some());
        if let Some(record) = done {
            break record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the serving watchdog recorded no restart trial within {budget:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(record.kind(), crate::dev_action::ActionKind::Restart);
    assert_eq!(
        record.requester_id.as_deref(),
        Some(SERVING_WATCHDOG_INITIATOR)
    );
    assert_eq!(SERVING_WATCHDOG_INITIATOR, "watchdog");
    assert!(
        record.params_digest.contains(&managed.config.id),
        "{}",
        record.params_digest
    );
    let run = record.run_result().expect("checked above");
    match &run {
        // The restart's own error, not the guard's drop record: an abandoned
        // guard is also `Errored`, and would pass a bare variant check.
        crate::dev_action::ActionRunResult::Errored { reason } => assert!(
            !reason.starts_with(crate::dev_action::record::ACTION_ABANDONED),
            "the trial carries the abandoned-guard record, not the restart's error: {reason}"
        ),
        other => panic!("the failed start must reach the trial: {other:?}"),
    }
    assert_eq!(state.dev_actions.read().await.recent(10).len(), 1);
}

/// A serving watchdog restart the readiness gate REFUSES mints nothing: the arm
/// retries a refusal on every escalating tick, so a trial per refusal would
/// flood the ledger for the length of a wedge. A primary believed running on a
/// refused port is the gate's UNKNOWN, which refuses.
#[tokio::test]
async fn serving_watchdog_refusal_mints_no_trial() {
    let (mut config, _root) = crate::config::config_over_an_empty_runner_tree();
    config.watchdog_enabled_at_start = true;
    let state: SharedState = Arc::new(crate::state::SupervisorState::new(config));
    let managed = register_dead_primary(&state).await;
    managed.watchdog.write().await.enabled = true;
    managed.runner.write().await.running = true;

    let result = serving_watchdog_restart(&state, &managed, 1).await;
    assert!(
        matches!(result, Err(SupervisorError::RestartUnsafe(_))),
        "the gate must refuse: {result:?}"
    );
    assert!(
        state.dev_actions.read().await.recent(10).is_empty(),
        "a refused serving restart minted a trial"
    );
}

/// The one completed trial in `state`'s ledger, waited for: a start that fails
/// records synchronously, but a success defers to the detached health wait.
async fn the_completed_trial(state: &SharedState) -> Arc<crate::dev_action::ActionRecord> {
    let budget = crate::test_clock::poll_budget(Duration::from_secs(30));
    let deadline = std::time::Instant::now() + budget;
    loop {
        let done = state
            .dev_actions
            .read()
            .await
            .recent(10)
            .into_iter()
            .find(|record| record.run_result().is_some());
        if let Some(record) = done {
            return record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no trial recorded a result within {budget:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The crash watchdog's start is a `spawn` trial under the `crash_watchdog`
/// requester, and a start that fails lands its own error on the trial. A dead
/// primary whose `started_at` is still the exited spawn's passes the
/// supersession check and fails at exe resolution, so nothing is started.
#[tokio::test]
async fn crash_watchdog_start_mints_a_spawn_with_crash_watchdog_initiator() {
    let (state, _root) = restart_test_state();
    let managed = register_dead_primary(&state).await;
    let exited_spawned_at = chrono::Utc::now() - chrono::Duration::seconds(10);
    managed.runner.write().await.started_at = Some(exited_spawned_at);

    let result = crash_watchdog_start(&state, &managed, exited_spawned_at, 2).await;
    assert!(
        matches!(&result, Err(SupervisorError::Process(m)) if m.contains("exe not found")),
        "the start must fail at exe resolution, past the supersession check: {result:?}"
    );

    let record = the_completed_trial(&state).await;
    assert_eq!(record.kind(), crate::dev_action::ActionKind::Spawn);
    assert_eq!(
        record.requester_id.as_deref(),
        Some(CRASH_WATCHDOG_INITIATOR)
    );
    assert_eq!(CRASH_WATCHDOG_INITIATOR, "crash_watchdog");
    assert!(
        record.params_digest.contains(&managed.config.id)
            && record.params_digest.contains("attempt=2"),
        "{}",
        record.params_digest
    );
    match record.run_result().expect("checked above") {
        crate::dev_action::ActionRunResult::Errored { reason } => assert!(
            !reason.starts_with(crate::dev_action::record::ACTION_ABANDONED),
            "the trial carries the abandoned-guard record, not the start's error: {reason}"
        ),
        other => panic!("the failed start must reach the trial as Errored: {other:?}"),
    }
    assert_eq!(state.dev_actions.read().await.recent(10).len(), 1);
}

/// A crash-watchdog start that finds the runner changed since the exit (a
/// latched operator stop here) ran nothing, so its trial records `Refused`.
#[tokio::test]
async fn a_superseded_crash_watchdog_start_records_refused() {
    let (state, _root) = restart_test_state();
    let managed = register_dead_primary(&state).await;
    let exited_spawned_at = chrono::Utc::now() - chrono::Duration::seconds(10);
    {
        let mut runner = managed.runner.write().await;
        runner.started_at = Some(exited_spawned_at);
        runner.stop_requested = true;
    }

    let result = crash_watchdog_start(&state, &managed, exited_spawned_at, 1).await;
    assert!(
        matches!(result, Err(SupervisorError::StartSuperseded(_))),
        "{result:?}"
    );
    let record = the_completed_trial(&state).await;
    assert!(
        matches!(
            record.run_result(),
            Some(crate::dev_action::ActionRunResult::Refused { .. })
        ),
        "{:?}",
        record.run_result()
    );
}

/// The boot auto-start is a `spawn` trial under the `boot_start` requester, and
/// a start that fails lands its own error on the trial. A dead primary fails at
/// exe resolution, so nothing is started.
#[tokio::test]
async fn boot_start_mints_a_spawn_with_boot_start_initiator() {
    let (state, _root) = restart_test_state();
    let managed = register_dead_primary(&state).await;

    let result = boot_start_runner(&state, &managed.config.id).await;
    assert!(
        matches!(&result, Err(SupervisorError::Process(m)) if m.contains("exe not found")),
        "{result:?}"
    );

    let record = the_completed_trial(&state).await;
    assert_eq!(record.kind(), crate::dev_action::ActionKind::Spawn);
    assert_eq!(record.requester_id.as_deref(), Some(BOOT_START_INITIATOR));
    assert_eq!(BOOT_START_INITIATOR, "boot_start");
    assert!(
        record.params_digest.contains(&managed.config.id),
        "{}",
        record.params_digest
    );
    match record.run_result().expect("checked above") {
        crate::dev_action::ActionRunResult::Errored { reason } => assert!(
            !reason.starts_with(crate::dev_action::record::ACTION_ABANDONED),
            "the trial carries the abandoned-guard record, not the start's error: {reason}"
        ),
        other => panic!("the failed start must reach the trial as Errored: {other:?}"),
    }
    assert_eq!(state.dev_actions.read().await.recent(10).len(), 1);
}

/// A boot start of an id the registry does not hold has no lifecycle act to
/// measure, so it mints nothing.
#[tokio::test]
async fn boot_start_of_an_unregistered_runner_mints_no_trial() {
    let (state, _root) = restart_test_state();

    let result = boot_start_runner(&state, "no-such-runner").await;
    assert!(
        matches!(&result, Err(SupervisorError::RunnerNotFound(_))),
        "{result:?}"
    );
    assert!(state.dev_actions.read().await.recent(10).is_empty());
}

/// An armed primary on a port nothing holds, for a test that drives the crash
/// watchdog.
async fn armed_primary() -> (SharedState, Arc<ManagedRunner>, tempfile::TempDir) {
    let (state, root) = armed_state();
    let managed = register_armed_primary(&state, Default::default()).await;
    (state, managed, root)
}

/// A state whose crash watchdog is armed.
fn armed_state() -> (SharedState, tempfile::TempDir) {
    assert!(
        !crash_restart_env_disabled(),
        "QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1 leaves this test nothing to observe"
    );
    let (mut config, root) = crate::config::config_over_an_empty_runner_tree();
    config.watchdog_enabled_at_start = true;
    (Arc::new(crate::state::SupervisorState::new(config)), root)
}

/// The primary on a port nothing holds ([`crate::test_ports::unheld_port`]):
/// these tests' stops reach the reap, which kills what holds the runner's port.
async fn register_armed_primary(
    state: &SharedState,
    extra_env: std::collections::HashMap<String, String>,
) -> Arc<ManagedRunner> {
    let mut runner = crate::config::RunnerConfig::default_primary();
    runner.port = crate::test_ports::unheld_port();
    runner.protected = false;
    runner.extra_env = extra_env;
    let id = runner.id.clone();
    let managed = Arc::new(ManagedRunner::new(runner, false));
    managed.watchdog.write().await.enabled = true;
    state.runners.write().await.insert(id, managed.clone());
    managed
}

/// How many times the crash watchdog began a restart.
async fn restarts_begun(state: &SharedState) -> usize {
    state
        .diagnostics
        .read()
        .await
        .events(usize::MAX, None)
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                DiagnosticEventKind::RestartStarted {
                    source: RestartSource::Watchdog,
                    ..
                }
            )
        })
        .count()
}

/// D10(d) through the backoff task itself. A start or a stop during the backoff
/// means no restart begins, whatever `running` reads, and an unchanged spawn
/// means one does.
#[tokio::test(start_paused = true)]
async fn the_backoff_task_begins_no_restart_of_a_runner_that_changed_during_it() {
    let exited = chrono::Utc::now();
    let newer = exited + chrono::Duration::seconds(1);
    for (started_at, stop_latched, begun) in [
        (Some(exited), false, 1),
        (Some(newer), false, 0),
        (None, false, 0),
        (Some(exited), true, 0),
    ] {
        let (state, managed, _root) = armed_primary().await;
        managed.runner.write().await.started_at = Some(exited);

        maybe_crash_restart(&state, &managed, false, false, exited).await;
        {
            let mut runner = managed.runner.write().await;
            runner.started_at = started_at;
            if stop_latched {
                runner.latch_stop_request(chrono::Utc::now());
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;

        assert_eq!(
            restarts_begun(&state).await,
            begun,
            "started_at={started_at:?} stop_latched={stop_latched}"
        );
        // The trial is minted after this check, so a skip leaves no ledger row.
        assert_eq!(
            state.dev_actions.read().await.recent(10).len(),
            begun,
            "started_at={started_at:?} stop_latched={stop_latched}"
        );
    }
}

/// The backoff task's own re-check can pass and the start then wait for the
/// lifecycle lock, behind a stop or a start. It decides again once it holds it.
#[tokio::test]
async fn a_crash_restart_that_waited_on_the_lock_finds_what_landed_meanwhile() {
    let exited = chrono::Utc::now();
    let newer = exited + chrono::Duration::seconds(1);
    for (started_at, stop_latched, removed, superseded) in [
        (Some(exited), false, false, false),
        (Some(newer), false, false, true),
        (None, true, false, true),
        (Some(exited), true, false, true),
        // A `DELETE`, a sweep or a purge during the backoff writes none of those.
        (Some(exited), false, true, true),
    ] {
        let (state, managed, _root) = restart_test_state_with_runner().await;
        managed.runner.write().await.started_at = Some(exited);

        let holder = managed.lifecycle.lock().await;
        let mut start = Box::pin(start_managed_runner(
            &state,
            &managed,
            None,
            StartAs::AfterCrash {
                exited_spawned_at: exited,
            },
        ));
        assert!(futures::poll!(&mut start).is_pending());
        {
            let mut runner = managed.runner.write().await;
            runner.started_at = started_at;
            if stop_latched {
                runner.latch_stop_request(chrono::Utc::now());
            }
        }
        if removed {
            state.runners.write().await.remove(&managed.config.id);
        }
        drop(holder);

        let err = start
            .await
            .expect_err("nothing can be started from an empty runner tree");
        assert_eq!(
            matches!(err, SupervisorError::StartSuperseded(_)),
            superseded,
            "started_at={started_at:?} stop_latched={stop_latched} removed={removed}: {err}"
        );
    }
}

/// A crash restart that a stop overtook while it waited on the lock is a correct
/// skip. The event pairing with `RestartStarted` says superseded, and nothing logs
/// the restart as failed.
#[tokio::test(start_paused = true)]
async fn a_superseded_crash_restart_says_so_and_logs_no_failure() {
    let (state, managed, _root) = armed_primary().await;
    let exited = chrono::Utc::now();
    managed.runner.write().await.started_at = Some(exited);

    let holder = managed.lifecycle.lock().await;
    maybe_crash_restart(&state, &managed, false, false, exited).await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        restarts_begun(&state).await,
        1,
        "the backoff task's re-check passes: nothing has changed yet"
    );
    managed
        .runner
        .write()
        .await
        .latch_stop_request(chrono::Utc::now());
    drop(holder);
    tokio::time::sleep(Duration::from_secs(3600)).await;

    let details: Vec<String> = state
        .diagnostics
        .read()
        .await
        .events(usize::MAX, None)
        .into_iter()
        .filter_map(|event| match event.kind {
            DiagnosticEventKind::RestartFailed { error, .. } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(details.len(), 1, "{details:?}");
    assert!(details[0].contains("superseded"), "{details:?}");
    let failed: Vec<String> = state
        .logs
        .history()
        .await
        .into_iter()
        .map(|entry| entry.message)
        .filter(|message| message.contains("FAILED"))
        .collect();
    assert_eq!(failed, Vec::<String>::new());
}

/// A registered, dead named runner, for a test that holds its lock.
async fn restart_test_state_with_runner() -> (SharedState, Arc<ManagedRunner>, tempfile::TempDir) {
    let (state, root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-held-start").await;
    (state, managed, root)
}

/// A start queued behind a `test-*` teardown must not re-register what that
/// teardown removed and spawn into the root it reaped.
#[tokio::test]
async fn a_start_queued_behind_a_teardown_does_not_re_register_the_runner() {
    let (state, managed, _root) = restart_test_state_with_runner().await;
    let id = managed.config.id.clone();

    let teardown = managed.lifecycle.lock().await;
    let mut start = Box::pin(start_managed_runner(
        &state,
        &managed,
        None,
        StartAs::Registered,
    ));
    assert!(futures::poll!(&mut start).is_pending());
    state.runners.write().await.remove(&id);
    drop(teardown);

    let err = start.await.expect_err("the runner was torn down");
    assert!(matches!(err, SupervisorError::RunnerNotFound(_)), "{err}");
    assert!(
        state.get_runner(&id).await.is_none(),
        "it was re-registered"
    );
}

/// A restart's start puts back the entry its own stop removed, and only once it
/// holds the lock. Re-inserting first would let a teardown it queued behind remove
/// the entry, and spawn a process absent from the registry.
#[tokio::test]
async fn a_restart_re_inserts_only_once_it_holds_the_lock() {
    let (state, managed, _root) = restart_test_state_with_runner().await;
    let id = managed.config.id.clone();

    let teardown = managed.lifecycle.lock().await;
    let mut start = Box::pin(start_managed_runner(
        &state,
        &managed,
        None,
        StartAs::Restart,
    ));
    assert!(futures::poll!(&mut start).is_pending());
    state.runners.write().await.remove(&id);
    drop(teardown);

    start
        .await
        .expect_err("nothing can be started from an empty runner tree");
    assert!(
        state.get_runner(&id).await.is_some(),
        "the entry stayed removed"
    );
}

/// The lock a remover takes: it neither waits nor acts on an entry something else
/// has removed since the caller's snapshot.
#[tokio::test]
async fn a_remover_gets_the_lock_only_uncontended_and_only_while_registered() {
    let (state, managed, _root) = restart_test_state_with_runner().await;

    {
        let holder = managed.lifecycle.lock().await;
        assert!(
            try_lock_registered(&state, &managed).await.is_none(),
            "held"
        );
        drop(holder);
    }
    assert!(try_lock_registered(&state, &managed).await.is_some());

    state.runners.write().await.remove(&managed.config.id);
    assert!(
        try_lock_registered(&state, &managed).await.is_none(),
        "the entry is gone"
    );
}

/// A late exit of a spawn another has replaced is no crash of the runner. It must
/// neither warn "crashed" nor take a crash-loop budget slot, since one exit too
/// many disarms the arm. The current spawn's exit still does both.
#[tokio::test]
async fn a_late_exit_burns_no_crash_budget() {
    let (state, managed, _root) = armed_primary().await;

    let spawn_a_child_and_record = || async {
        let child = spawn_a_child_that_exits_3();
        let pid = child.id().expect("a child not yet waited on has a pid");
        let spawned_at = record_spawn_in_registry(&managed, Some(pid), None, None).await;
        (child, spawned_at)
    };
    let (old, spawned_old) = spawn_a_child_and_record().await;
    while chrono::Utc::now() == spawned_old {
        tokio::task::yield_now().await;
    }
    let (current, spawned_current) = spawn_a_child_and_record().await;
    assert_ne!(spawned_old, spawned_current);

    monitor_runner_process_exit_inner(state.clone(), managed.clone(), old, spawned_old).await;
    {
        let watchdog = managed.watchdog.read().await;
        assert_eq!(
            (watchdog.restart_attempts, watchdog.crash_history.len()),
            (0, 0),
            "a replaced spawn's exit took a crash-loop budget slot"
        );
    }

    monitor_runner_process_exit_inner(state.clone(), managed.clone(), current, spawned_current)
        .await;
    let watchdog = managed.watchdog.read().await;
    assert_eq!(
        (watchdog.restart_attempts, watchdog.crash_history.len()),
        (1, 1),
        "the current spawn's crash must reach the watchdog"
    );
}

/// What `maybe_serving_restart` alerts about a runner that holds its port and has
/// never answered: never silence, and "adopted" only when it is. On Linux a
/// spawned primary's cleared pid never comes back, and it is still not adopted.
#[tokio::test]
async fn a_runner_that_never_answered_is_alerted_and_called_adopted_only_when_it_is() {
    use crate::diagnostics::ServingEvent;
    use crate::log_capture::LogLevel;

    const SPAWN_PID: u32 = 4242;
    for (spawned, pid, called_adopted) in [
        (false, Some(77), true),
        (true, Some(SPAWN_PID), false),
        (true, None, false),
        (true, Some(77), true),
    ] {
        for should_alert in [true, false] {
            let (state, _root) = restart_test_state();
            let managed = register_dead_primary(&state).await;
            if spawned {
                managed
                    .recent_spawns
                    .write()
                    .await
                    .push(crate::state::SpawnMarker::new(
                        chrono::Utc::now(),
                        Some(SPAWN_PID),
                        None,
                        None,
                    ));
            }
            managed.runner.write().await.pid = pid;

            maybe_serving_restart(
                &state,
                &managed,
                crate::state::RunnerLiveness::Unknown,
                should_alert,
            )
            .await;

            let events: Vec<ServingEvent> = state
                .diagnostics
                .read()
                .await
                .events(usize::MAX, None)
                .into_iter()
                .filter_map(|event| match event.kind {
                    DiagnosticEventKind::ServingWatchdog { event, .. } => Some(event),
                    _ => None,
                })
                .collect();
            let row = format!("spawned={spawned} pid={pid:?} should_alert={should_alert}");
            if !should_alert {
                assert_eq!(events, [], "{row}");
                continue;
            }
            let expected = if called_adopted {
                ServingEvent::NeverAnsweredSinceAdoption
            } else {
                ServingEvent::NeverAnsweredSinceSpawn
            };
            assert_eq!(events, [expected], "{row}");
            let logged: Vec<String> = state
                .logs
                .history()
                .await
                .into_iter()
                .filter(|entry| matches!(entry.level, LogLevel::Error))
                .map(|entry| entry.message)
                .collect();
            assert_eq!(logged.len(), 1, "{row}: {logged:?}");
            assert_eq!(
                logged[0].contains("adopted"),
                called_adopted,
                "{row}: {logged:?}"
            );
            assert!(logged[0].contains("NEVER answered"), "{row}: {logged:?}");
        }
    }
}

/// What a stub process runs: a builtin wait, so its image stays the stub's own and
/// its process satisfies a stop's image check. It ends itself after a minute, so a
/// failed test leaks nothing for long.
#[cfg(unix)]
const STUB_ENV_SCRIPT: &str = "read -t 60 <> <(:)\nexit 0\n";

/// A primary whose exe is a stub, so a test drives a real successful start. The
/// stub is a copy of bash, with `BASH_ENV` holding the script, since a runner is
/// spawned with no arguments. A script with a shebang would not do: its image is
/// the interpreter, which no stop's image check can match with the runner's copy.
#[cfg(unix)]
async fn stub_primary() -> (SharedState, Arc<ManagedRunner>, tempfile::TempDir) {
    let (state, root) = armed_state();
    let stub = root.path().join("stub-runner");
    std::fs::copy("/bin/bash", &stub).expect("copy bash as the stub");
    let script = root.path().join("stub-runner.env");
    std::fs::write(&script, STUB_ENV_SCRIPT).expect("write the stub's script");
    let env = [("BASH_ENV".to_string(), script.display().to_string())];
    let managed = register_armed_primary(&state, env.into_iter().collect()).await;
    *managed.source_exe_override.write().await = Some(stub);
    (state, managed, root)
}

/// Stop what a stub test left running and remove the early logs its starts opened.
/// A stop reaches only the current spawn, so also end any older stub process still
/// up, checked by image in case its pid has been reused.
async fn stop_the_stub(state: &SharedState, managed: &Arc<ManagedRunner>) {
    let _ = stop_runner_by_id(state, &managed.config.id, true, CloseIntent::Kill).await;
    let copy = state.config.runner_exe_copy_path(&managed.config);
    let spawns: Vec<crate::state::SpawnMarker> =
        managed.recent_spawns.read().await.iter().cloned().collect();
    for spawn in spawns {
        if let Some(pid) = spawn.pid {
            if crate::process::proc_kill::pid_exe_path(pid)
                .await
                .as_deref()
                == Some(copy.as_path())
            {
                let _ = crate::process::proc_kill::kill_by_pid(pid).await;
            }
        }
        if let Some(log) = spawn.early_log_path.as_deref() {
            crate::process::early_log::delete_early_log(log);
        }
    }
}

/// Run a stub test's body, then stop the stub on every exit path, a failed assertion
/// included, and only then let that failure through.
async fn with_the_stub_stopped_after<Fut>(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
    body: Fut,
) where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let outcome = tokio::spawn(body).await;
    stop_the_stub(state, managed).await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// How long a test gives an exit monitor to observe a kill it already issued.
#[cfg(unix)]
const MONITOR_BUDGET: Duration = Duration::from_secs(20);

#[cfg(unix)]
async fn wait_for_exit_stamp(managed: &ManagedRunner) -> crate::state::SpawnMarker {
    let deadline = std::time::Instant::now() + MONITOR_BUDGET;
    loop {
        {
            let ring = managed.recent_spawns.read().await;
            let entry = ring.newest().expect("the spawn");
            if entry.exited_at.is_some() {
                return entry.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no exit was stamped: the process is still running, or its monitor is gone"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A real start, on a stub exe. A stop that arrives while it runs is ordered
/// after it: the start's record cannot clear that stop's latch, and the stop ends
/// the process the start recorded. Both are queued on the lock before either runs,
/// in that order, then run on separate workers, so a start that releases the lock
/// early lets the stop overlap it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_queued_mid_start_targets_the_recorded_spawn() {
    let (state, managed, _root) = stub_primary().await;
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        async move {
            let id = managed.config.id.clone();
            let holder = managed.lifecycle.lock().await;
            let mut start = {
                let (state, managed) = (state.clone(), managed.clone());
                Box::pin(async move {
                    start_managed_runner(&state, &managed, None, StartAs::Registered).await
                })
            };
            assert!(futures::poll!(&mut start).is_pending());
            let mut stop = {
                let state = state.clone();
                Box::pin(
                    async move { stop_runner_by_id(&state, &id, false, CloseIntent::Kill).await },
                )
            };
            assert!(futures::poll!(&mut stop).is_pending());
            drop(holder);
            let start = tokio::spawn(start);
            let stop = tokio::spawn(stop);
            let (start, stop) = (
                start.await.expect("the start"),
                stop.await.expect("the stop"),
            );
            start.and(stop).expect("the start and the stop succeed");

            let entry = wait_for_exit_stamp(&managed).await;
            let runner = managed.runner.read().await;
            assert!(
                runner.stop_requested,
                "the start's record cleared a stop that overlapped it"
            );
            assert_eq!(runner.started_at, None, "the stop's reset is the last word");
            assert!(
                entry.stop_requested_at.is_some(),
                "the stop did not target the recorded spawn: {entry:?}"
            );
            assert!(
                !crate::process::proc_kill::pid_alive(entry.pid.expect("the spawn's pid")),
                "the recorded spawn survived its stop"
            );
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
}

/// Two real starts queued on the lock: the second finds the first's spawn. One
/// process, whatever `running` said when each was asked for.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_queued_behind_a_start_finds_its_spawn() {
    let (state, managed, _root) = stub_primary().await;
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        async move {
            let holder = managed.lifecycle.lock().await;
            let start = || {
                let (state, managed) = (state.clone(), managed.clone());
                Box::pin(async move {
                    start_managed_runner(&state, &managed, None, StartAs::Registered).await
                })
            };
            let (mut first, mut second) = (start(), start());
            assert!(futures::poll!(&mut first).is_pending());
            assert!(futures::poll!(&mut second).is_pending());
            drop(holder);
            let (first, second) = (tokio::spawn(first), tokio::spawn(second));
            let (first, second) = (
                first.await.expect("the first start"),
                second.await.expect("the second start"),
            );
            let spawned = managed.recent_spawns.read().await.iter().count();

            assert!(first.is_ok(), "{first:?}");
            assert!(
                matches!(second, Err(SupervisorError::RunnerAlreadyRunning)),
                "{second:?}"
            );
            assert_eq!(spawned, 1, "a second process of a live runner");
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
}

/// A real start through a door nobody waits on, on a stub exe: its spawn's ring
/// entry names the trial it minted, which the source scans can only infer. The
/// stub never answers `/health`, so the trial's result is still the health
/// wait's to record.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_watchdog_start_names_its_trial_on_the_spawn_ring() {
    let (state, managed, _root) = stub_primary().await;
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        async move {
            let exited_spawned_at = chrono::Utc::now() - chrono::Duration::seconds(10);
            managed.runner.write().await.started_at = Some(exited_spawned_at);

            crash_watchdog_start(&state, &managed, exited_spawned_at, 1)
                .await
                .expect("the start");

            let newest = state.dev_actions.read().await.recent(1);
            let trial = newest.first().expect("the trial");
            assert_eq!(trial.kind(), crate::dev_action::ActionKind::Spawn);
            assert_eq!(
                managed
                    .recent_spawns
                    .read()
                    .await
                    .newest()
                    .expect("the spawn")
                    .initiated_by,
                Some(trial.action_id)
            );
            assert_eq!(trial.run_result(), None, "nothing has checked its health");
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
}

/// A primary whose exe exits at once and cleanly, on every platform: its spawn's
/// exit is observed, and a clean exit is one no crash watchdog answers, so nothing
/// else starts behind the test's back.
async fn exiting_stub_primary() -> (SharedState, Arc<ManagedRunner>, tempfile::TempDir) {
    let (state, root) = armed_state();
    let stub = root
        .path()
        .join(format!("stub-runner{}", std::env::consts::EXE_SUFFIX));
    let env: std::collections::HashMap<String, String> = {
        #[cfg(unix)]
        {
            std::fs::copy("/bin/bash", &stub).expect("copy bash as the stub");
            let script = root.path().join("stub-runner.env");
            std::fs::write(&script, "exit 0\n").expect("write the stub's script");
            [("BASH_ENV".to_string(), script.display().to_string())]
                .into_iter()
                .collect()
        }
        #[cfg(windows)]
        {
            let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
            std::fs::copy(format!(r"{system}\System32\hostname.exe"), &stub)
                .expect("copy hostname as the stub");
            std::collections::HashMap::new()
        }
    };
    let managed = register_armed_primary(&state, env).await;
    *managed.source_exe_override.write().await = Some(stub);
    (state, managed, root)
}

/// Answer `managed`'s `/health` once its newest spawn has exited, as a crash-watchdog
/// respawn does on the same port. The port stays free until then, so the start finds
/// it unheld, and a boot that exited is all this answer can be read against.
///
/// The listener is THIS process, and a stop's ladder kills whatever holds the
/// runner's port. So end this task, and await it, before anything stops the runner.
async fn answer_the_port_after_the_boot_exits(managed: Arc<ManagedRunner>) {
    loop {
        let exited = managed
            .recent_spawns
            .read()
            .await
            .newest()
            .is_some_and(|spawn| spawn.exited_at.is_some());
        if exited {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", managed.config.port))
        .await
        .expect("bind the runner's port");
    let app = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
    axum::serve(listener, app).await.expect("serve /health");
}

/// Answer `managed`'s `/health` while its newest spawn is still up: the boot itself
/// is alive when the port first answers, so it is this boot the answer is read
/// against. Same lifetime rule as `answer_the_port_after_the_boot_exits`.
#[cfg(unix)]
async fn answer_the_port_while_the_boot_is_up(managed: Arc<ManagedRunner>) {
    loop {
        let up = managed
            .recent_spawns
            .read()
            .await
            .newest()
            .is_some_and(|spawn| {
                spawn.exited_at.is_none()
                    && spawn.pid.is_some_and(crate::process::proc_kill::pid_alive)
            });
        if up {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", managed.config.port))
        .await
        .expect("bind the runner's port");
    let app = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
    axum::serve(listener, app).await.expect("serve /health");
}

/// The trial the newest door minted must read as `Ran` when its own boot was up
/// when the port answered, and as a boot that did not come up otherwise.
async fn assert_the_newest_trial_read_its_own_boot(state: &SharedState, boot_was_up: bool) {
    let newest = state.dev_actions.read().await.recent(1);
    let trial = newest.first().expect("the trial");
    match (boot_was_up, trial.run_result()) {
        (true, Some(crate::dev_action::ActionRunResult::Ran)) => {}
        (true, other) => panic!("a boot that answered must read as up: {other:?}"),
        (false, Some(crate::dev_action::ActionRunResult::Errored { reason })) => {
            assert!(
                reason.starts_with(crate::dev_action::run_result::UNHEALTHY_AFTER_START),
                "{reason}"
            );
            assert!(reason.contains("had exited or been replaced"), "{reason}");
        }
        (false, other) => panic!("a boot that exited must not read as up: {other:?}"),
    }
}

/// Drive one held-open door with its port answered by `answer`, then check the door
/// and its trial: `boot_was_up` says whether the boot was alive when `answer` bound.
///
/// The listener is ended, and awaited, before anything else: before the assertions
/// and before the stub is stopped, so the stop's ladder never finds this process on
/// the runner's port, even when the door itself panicked. A listener that failed to
/// bind re-raises its panic, rather than surfacing as the health wait's timeout. The
/// stub is then stopped before the trial is read. The trial's result is fixed when
/// the door answers (a run result is written once), so that stop shows only that
/// nothing records over it.
async fn through_a_held_open_door<A, F, Fut>(
    state: SharedState,
    managed: Arc<ManagedRunner>,
    answer: A,
    boot_was_up: bool,
    door: F,
) where
    A: std::future::Future<Output = ()> + Send + 'static,
    F: FnOnce(SharedState) -> Fut,
    Fut: std::future::Future<Output = Result<axum::response::Response, SupervisorError>>
        + Send
        + 'static,
{
    let answering = tokio::spawn(answer);
    let answered = tokio::spawn(door(state.clone())).await;
    answering.abort();
    let listened = answering.await;
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        async move {
            if let Err(failure) = listened {
                if failure.is_panic() {
                    std::panic::resume_unwind(failure.into_panic());
                }
            }
            let response = match answered {
                Ok(response) => response.expect("the door"),
                Err(failure) => std::panic::resume_unwind(failure.into_panic()),
            };
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            stop_the_stub(&state, &managed).await;
            assert_the_newest_trial_read_its_own_boot(&state, boot_was_up).await;
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
}

/// The regression at `POST /runners/{id}/start`: the boot exits, and something else
/// answers its port inside the held-open wait. The caller asked whether the runner
/// is up, so its answer stays a 200. The trial asked whether its own boot came up, so
/// it is not `Ran`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_open_start_answers_the_port_and_its_trial_reads_its_own_boot() {
    let (state, managed, _root) = exiting_stub_primary().await;
    let id = managed.config.id.clone();
    let answer = answer_the_port_after_the_boot_exits(managed.clone());
    through_a_held_open_door(state, managed, answer, false, move |state| {
        crate::routes::runners::start_runner(
            axum::extract::State(state),
            axum::extract::Path(id),
            axum::extract::Query(crate::routes::runner::StartWaitQuery { wait: true }),
        )
    })
    .await;
}

/// The same at `POST /runner/restart` with no rebuild, the held-open door a primary
/// is restarted through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_open_restart_answers_the_port_and_its_trial_reads_its_own_boot() {
    let (state, managed, _root) = exiting_stub_primary().await;
    let answer = answer_the_port_after_the_boot_exits(managed.clone());
    through_a_held_open_door(state, managed, answer, false, |state| {
        crate::routes::runner::restart_runner(
            axum::extract::State(state),
            axum::extract::Query(crate::routes::runner::StartWaitQuery { wait: true }),
            axum::Json(crate::routes::runner::RestartRequest {
                rebuild: false,
                force: false,
                from_working_tree: false,
                use_lkg: Some(false),
                handoff: false,
            }),
        )
    })
    .await;
}

/// The complement at `POST /runners/{id}/start`: the boot is alive when its port
/// answers, so its trial reads `Ran`. This guards the over-correction direction: a
/// hold that demoted every held-open `Ran` turns it red. It does not stage a boot
/// that answered and was stopped before the door recorded; that timing is held by
/// `dev_action::run_result`'s unit test. Unix only: the one stub that stays up is
/// `stub_primary`'s bash.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_open_start_whose_boot_answered_reads_ran() {
    let (state, managed, _root) = stub_primary().await;
    let id = managed.config.id.clone();
    let answer = answer_the_port_while_the_boot_is_up(managed.clone());
    through_a_held_open_door(state, managed, answer, true, move |state| {
        crate::routes::runners::start_runner(
            axum::extract::State(state),
            axum::extract::Path(id),
            axum::extract::Query(crate::routes::runner::StartWaitQuery { wait: true }),
        )
    })
    .await;
}

/// A health-cache tick before the primary answers.
#[cfg(unix)]
async fn the_health_cache_clears_a_booting_primary(managed: &ManagedRunner) -> u32 {
    let mut runner = managed.runner.write().await;
    runner.running = false;
    runner.pid.take().expect("the stub's pid")
}

/// A real restart of a primary booting before it binds. The health cache has
/// cleared `running` and `pid`, and its port is closed, so only its own spawn says
/// it is alive. The restart must stop that process and start another, rather than
/// be refused as already running for as long as the process lives.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_replaces_a_primary_booting_before_it_binds() {
    use crate::diagnostics::RestartSource;
    let (state, managed, _root) = stub_primary().await;
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        async move {
            let id = managed.config.id.clone();
            start_managed_runner(&state, &managed, None, StartAs::Registered)
                .await
                .expect("the stub starts");
            let booting = the_health_cache_clears_a_booting_primary(&managed).await;

            restart_runner_by_id(
                &state,
                &id,
                false,
                RestartSource::Manual,
                false,
                BuildTree::OriginMain,
                CloseIntent::Kill,
                None,
            )
            .await
            .expect("the restart replaces it");

            let current = managed.runner.read().await.pid;
            assert!(current.is_some(), "the restart started nothing new");
            assert_ne!(current, Some(booting), "the restart started nothing new");
            let ring = managed.recent_spawns.read().await;
            assert_eq!(ring.iter().count(), 2, "the restart started nothing new");
            let deadline = std::time::Instant::now() + MONITOR_BUDGET;
            while crate::process::proc_kill::pid_alive(booting) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the booting process survived its restart"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
}

/// The ring's pid is what settles a stop when image identity cannot: here a second
/// process runs the same exe copy, so identity alone refuses to guess. The spawn the
/// ring names is stopped, and the other process is left alone.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ring_pid_settles_a_stop_two_processes_share_an_image_for() {
    let (state, managed, root) = stub_primary().await;
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        let script = root.path().join("stub-runner.env");
        async move {
            let id = managed.config.id.clone();
            start_managed_runner(&state, &managed, None, StartAs::Registered)
                .await
                .expect("the stub starts");
            the_health_cache_clears_a_booting_primary(&managed).await;

            let copy = state.config.runner_exe_copy_path(&managed.config);
            let mut bystander = tokio::process::Command::new(&copy)
                .env("BASH_ENV", &script)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn a second process of the exe copy");

            stop_runner_by_id(&state, &id, false, CloseIntent::Kill)
                .await
                .expect("the stop succeeds");

            let entry = wait_for_exit_stamp(&managed).await;
            assert!(entry.stop_requested_at.is_some(), "{entry:?}");
            assert!(
                matches!(bystander.try_wait(), Ok(None)),
                "the stop ended a process it was not asked to"
            );
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
}

/// A temp runner whose exe is `stub_primary`'s stub: a copy of bash, running
/// `STUB_ENV_SCRIPT` from `BASH_ENV`. A temp runner because no spawn path wraps one
/// in `systemd-run`, so on any box its start's own spawn is the exec a test reads.
/// Its `DISPLAY` only satisfies a Linux temp start's display check; the stub opens
/// no window. A fresh device identity, so its home is provisioned with no device
/// identity at all.
///
/// Nothing its start touches is the operator's or a live service. The state's
/// per-user dirs are its tempdir's ([`crate::config::config_over_an_empty_runner_tree`]),
/// so the start's instance root and ledger row land there and the teardown
/// removes the root from there. Its registered primary is on a refused port
/// ([`crate::test_ports::reserve_refused_port`]), so the start's window-placement
/// GET (`env_forwarders::WindowPositionEnv`, which asks the registered primary) is
/// refused at once instead of asking whatever primary runner serves 9876.
#[cfg(target_os = "linux")]
async fn stub_temp_runner() -> (SharedState, Arc<ManagedRunner>, tempfile::TempDir) {
    let (mut config, root) = crate::config::config_over_an_empty_runner_tree();
    let refused = crate::test_ports::reserve_refused_port();
    for primary in config.runners.iter_mut().filter(|r| r.kind().is_primary()) {
        primary.port = refused;
    }
    let state = Arc::new(crate::state::SupervisorState::new(config));
    let stub = root.path().join("stub-runner");
    std::fs::copy("/bin/bash", &stub).expect("copy bash as the stub");
    let script = root.path().join("stub-runner.env");
    std::fs::write(&script, STUB_ENV_SCRIPT).expect("write the stub's script");
    let id = format!("test-{}", uuid::Uuid::new_v4().simple());
    let config = crate::config::RunnerConfig {
        id: id.clone(),
        name: crate::process::temp_runner_instance_name(&id),
        port: crate::test_ports::unheld_port(),
        kind: qontinui_types::wire::runner_kind::RunnerKind::Temp { id: id.clone() },
        protected: true,
        server_mode: false,
        restate_ingress_port: None,
        restate_admin_port: None,
        restate_service_port: None,
        external_restate_admin_url: None,
        external_restate_ingress_url: None,
        extra_env: [
            ("BASH_ENV".to_string(), script.display().to_string()),
            ("DISPLAY".to_string(), ":0".to_string()),
        ]
        .into_iter()
        .collect(),
        fresh_device_identity: true,
    };
    let managed = Arc::new(ManagedRunner::new(config, false));
    *managed.source_exe_override.write().await = Some(stub);
    state.runners.write().await.insert(id, managed.clone());
    (state, managed, root)
}

/// Phase 3's arming test (plan `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`).
/// A start execs its copy while another process holds a write descriptor on the
/// inode the copy path names, which is what a sibling fork that inherited an earlier
/// in-process write's descriptor holds until that fork execs. The start's copy is a
/// fresh inode, so its exec is not refused. With the copy written in place, onto the
/// held inode, the start fails `Failed to spawn exe: Text file busy (os error 26)`.
///
/// The holder is a process of its own, `sh -c 'exec 3>>"$1"; exec sleep 60'`, not a
/// fork of this test binary parked before its exec, which would inherit every
/// descriptor open at that moment, other tests' listeners and pipe ends included. It
/// holds the descriptor until the test ends it, and the test reads `/proc/<pid>/fd/3`
/// before the start and again after it: a holder that let go before the exec makes
/// the run prove nothing, so that run fails. The start can spend seconds between its
/// copy and its exec (a temp start asks the primary for a window placement, for up
/// to 3 s), so the 60 s sleep only bounds a holder that a killed test binary leaks.
///
/// A temp start writes an instance root and a temp-runner ledger row and asks the
/// primary for a window placement. The test asserts where each went, so it would fail
/// a start that wrote into the operator's config or data dirs again or asked a live
/// primary: the root and the row under this test's tempdir ([`stub_temp_runner`]),
/// and the placement refused on the registered primary's refused port.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_execs_its_copy_while_an_inheritor_of_an_earlier_write_is_parked() {
    let (state, managed, tempdir) = stub_temp_runner().await;
    let tempdir_path = tempdir.path().to_path_buf();
    let body = {
        let (state, managed) = (state.clone(), managed.clone());
        async move {
            let copy = state.config.runner_exe_copy_path(&managed.config);
            std::fs::create_dir_all(copy.parent().expect("the copy's dir"))
                .expect("create the copy's dir");
            let mut holder = tokio::process::Command::new("sh")
                .args(["-c", r#"exec 3>>"$1"; exec sleep 60"#, "sh"])
                .arg(&copy)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn the holder");
            let held = std::path::PathBuf::from(format!(
                "/proc/{}/fd/3",
                holder.id().expect("the holder's pid")
            ));
            let deadline = std::time::Instant::now() + MONITOR_BUDGET;
            while !held.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the holder never opened the copy path"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let started = start_managed_runner(&state, &managed, None, StartAs::Registered).await;
            let held_across_the_exec = held.exists();
            let _ = holder.start_kill();
            let _ = holder.wait().await;

            started.expect("the start execs its copy");
            assert!(
                held_across_the_exec,
                "the holder let go of the copy path before the start returned, so this run \
                 proves nothing"
            );
            let pid = managed.runner.read().await.pid.expect("the spawn's pid");
            assert!(
                crate::process::proc_kill::pid_alive(pid),
                "the spawn is not alive"
            );
            assert_eq!(
                crate::process::proc_kill::pid_exe_path(pid)
                    .await
                    .as_deref(),
                Some(copy.as_path()),
                "the spawn runs something other than its copy"
            );

            // Where the start wrote, and whom it asked.
            let dirs = &state.config.ambient_user_dirs;
            let root = instance_config_dir(dirs, &managed.config.id).expect("an instance root");
            assert!(
                root.starts_with(&tempdir_path) && root.is_dir(),
                "the instance root {root:?} is not this test's, under {tempdir_path:?}"
            );
            let ledger = crate::process::temp_runner_ledger::ledger_path(dirs).expect("a ledger");
            assert!(ledger.starts_with(&tempdir_path), "{ledger:?}");
            assert!(
                crate::process::temp_runner_ledger::load(dirs)
                    .iter()
                    .any(|row| row.pid == pid && row.port == managed.config.port),
                "the spawn is recorded in this test's ledger at {ledger:?}"
            );
            let primary_port = super::primary_runner_port(&state).await;
            let placement: Vec<String> = state
                .logs
                .history()
                .await
                .into_iter()
                .map(|entry| entry.message)
                .filter(|message| message.starts_with("Spawn temp placement:"))
                .collect();
            assert!(
                placement.len() == 1
                    && placement[0].contains("skipping")
                    && placement[0].contains(&format!("127.0.0.1:{primary_port}")),
                "the placement GET must be refused on the registered primary's port \
                 {primary_port}: {placement:?}"
            );
        }
    };
    with_the_stub_stopped_after(&state, &managed, body).await;
    drop(tempdir);
}

/// A pid from the ring can have been reused, so the stop checks its image first. A
/// process running anything but the runner's exe copy is left alone, where the same
/// pid from the registry, which is trusted, would be killed.
#[tokio::test]
async fn a_stop_leaves_a_ring_pid_that_runs_another_image_alone() {
    let (state, managed, _root) = armed_primary().await;
    let (mut child, pid) = spawn_a_child_that_exits_when_its_stdin_closes();
    let _stdin = child.stdin.take().expect("the child's stdin");
    record_spawn_in_registry(&managed, Some(pid), None, None).await;
    {
        let mut runner = managed.runner.write().await;
        runner.running = false;
        runner.pid = None;
    }

    stop_runner_by_id(&state, &managed.config.id, false, CloseIntent::Kill)
        .await
        .expect("nothing holds the port, so the stop confirms");

    assert!(
        tokio::time::timeout(Duration::from_secs(1), child.wait())
            .await
            .is_err(),
        "the stop killed a pid whose image is not the runner's exe copy"
    );
}

/// A ring pid names the spawn, so a handoff waits for that process to go, and a
/// probe's candidate does not. It is never published: the registry's pid is exempt
/// from the image check, so a reused ring pid the check refused would be killed by
/// a retried stop that read it back.
#[test]
fn a_ring_pid_names_the_spawn_and_is_never_published() {
    use crate::process::manager::{publishes_discovered_pid, source_names_the_spawn};
    use crate::process::stop_ledger::PidSource;
    for (source, names_the_spawn, published) in [
        (PidSource::Registry, true, true),
        (PidSource::SpawnRing, true, false),
        (PidSource::ListenerProbe, false, true),
        (PidSource::ExeIdentity, false, true),
    ] {
        assert_eq!(source_names_the_spawn(&source), names_the_spawn, "{source}");
        assert_eq!(publishes_discovered_pid(&source), published, "{source}");
    }
}

/// The wire `watchdog` source is still refused for a non-temp runner, and
/// the refusal happens BEFORE anything is latched or emitted.
#[tokio::test]
async fn wire_watchdog_source_is_still_blocked_for_non_temp_runners() {
    use crate::diagnostics::RestartSource;
    let (state, _root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-p3-blocked").await;

    let err = restart_runner_by_id(
        &state,
        "named-p3-blocked",
        false,
        RestartSource::Watchdog,
        false,
        BuildTree::OriginMain,
        CloseIntent::Kill,
        None,
    )
    .await
    .expect_err("wire watchdog must be blocked");
    assert!(matches!(err, SupervisorError::Validation(_)), "{err}");
    assert!(err.to_string().contains("blocked"), "{err}");
    assert!(!managed.runner.read().await.restart_requested);
}

/// The restart 202 echo and the build decision share one predicate, so they
/// cannot disagree. Only the primary asking for `OriginMain` compiles
/// origin/main; named/temp runners build the live tree whatever is passed.
#[test]
fn build_tree_compiles_origin_main_only_for_the_primary() {
    assert!(BuildTree::OriginMain.compiles_origin_main(true));
    assert!(!BuildTree::LiveWorkingTree.compiles_origin_main(true));
    assert!(!BuildTree::OriginMain.compiles_origin_main(false));
    assert!(!BuildTree::LiveWorkingTree.compiles_origin_main(false));

    assert_eq!(
        BuildTree::OriginMain.build_source(true),
        BuildSource::OriginMain
    );
    assert_eq!(
        BuildTree::LiveWorkingTree.build_source(true),
        BuildSource::LiveTree
    );
    assert_eq!(
        BuildTree::OriginMain.build_source(false),
        BuildSource::LiveTree
    );
    assert_eq!(
        BuildTree::LiveWorkingTree.build_source(false),
        BuildSource::LiveTree
    );
}

/// The 202 bodies serialize this value directly, so pin the wire spelling
/// the frontend type (`'origin_main' | 'live_tree'`) declares.
#[test]
fn build_tree_build_source_serializes_to_the_documented_labels() {
    assert_eq!(
        serde_json::to_value(BuildTree::OriginMain.build_source(true)).unwrap(),
        serde_json::json!("origin_main")
    );
    assert_eq!(
        serde_json::to_value(BuildTree::LiveWorkingTree.build_source(true)).unwrap(),
        serde_json::json!("live_tree")
    );
}

/// The wire bool maps to the enum in exactly one place, with the polarity
/// the 2026-09-09 defect got wrong: omitted/`false` is origin/main.
#[test]
fn build_tree_from_working_tree_maps_the_wire_bool() {
    assert_eq!(BuildTree::from_working_tree(false), BuildTree::OriginMain);
    assert_eq!(
        BuildTree::from_working_tree(true),
        BuildTree::LiveWorkingTree
    );
}

/// The orphaned-postmaster stop never kills a process running an executable
/// outside the instance root — a recycled PID in a stale `postmaster.pid`.
/// The PID is a `sleep` child this test owns (never the test process itself),
/// whose image lies outside the tempdir root: the verdict must be a refusal and
/// the child must still be alive.
#[cfg(unix)]
#[tokio::test]
async fn orphaned_postmaster_stop_refuses_a_pid_outside_the_root() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    let _reap = scopeguard::guard((), |()| {
        let _ = child.kill();
        let _ = child.wait();
    });
    assert!(
        crate::process::proc_kill::pid_exe_path(pid).await.is_some(),
        "precondition: the child's image path must be readable, or the identity check is \
         never reached"
    );
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = crate::process::instance_root::InstanceRoot::at(tmp.path().to_path_buf());
    let data = root.embedded_pg_dir().join("pg-data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("postmaster.pid"),
        format!("{pid}\n{}\n", data.display()),
    )
    .unwrap();
    assert_eq!(
        super::stop_orphaned_embedded_pg("test-orphan-pg", &root).await,
        super::OrphanPgStop::RefusedOutsideRoot
    );
    assert!(
        crate::process::proc_kill::pid_exe_path(pid).await.is_some(),
        "the refused PID must still be alive"
    );
}

#[test]
fn folded_prefix_is_component_wise() {
    let base = std::path::Path::new("C").join("instances").join("test-1");
    assert!(super::starts_with_folded(&base.join("x"), &base));
    assert!(super::starts_with_folded(
        &std::path::Path::new("c")
            .join("INSTANCES")
            .join("Test-1")
            .join("x"),
        &base
    ));
    assert!(!super::starts_with_folded(
        &std::path::Path::new("C")
            .join("instances")
            .join("test-10")
            .join("x"),
        &base
    ));
}

/// The positive arm: an executable that lives INSIDE the root is recognised as
/// the subject's own (the identity check that gates the kill).
#[test]
fn exe_inside_the_root_is_recognised() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let root = tmp.path().join("root");
    let exe = root
        .join("embedded-pg")
        .join("pg-install")
        .join("bin")
        .join("postgres");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, b"").unwrap();
    assert!(super::exe_is_inside_root(&exe, &root));
    assert!(!super::exe_is_inside_root(
        &tmp.path().join("elsewhere"),
        &root
    ));
}

/// A primary spawned by a supervisor that inherited a subject's root drops the
/// root and the keychain switch; with no inherited root nothing is touched.
#[test]
fn primary_drops_an_inherited_subject_root() {
    let mut cmd = Command::new("cargo");
    super::drop_inherited_subject_env(&mut cmd, None);
    assert!(command_envs(&cmd).is_empty());

    let root = crate::process::instance_root::tests::abs("subject-root");
    let mut cmd = Command::new("cargo");
    super::drop_inherited_subject_env(&mut cmd, Some(root.into_os_string()));
    let envs = command_envs(&cmd);
    assert_eq!(envs.get("QONTINUI_INSTANCE_ROOT"), Some(&None));
    assert_eq!(envs.get("QONTINUI_DISABLE_KEYCHAIN"), Some(&None));
}

// ── Handoff close intent (plan
//    2026-10-21-graceful-runner-restart-detaches-holder-panes-instead-of-killing-them, D7)

/// What one close-request looked like on the wire.
#[derive(Debug)]
struct SeenCloseRequest {
    content_type: Option<String>,
    body: Vec<u8>,
}

/// A real HTTP server on an ephemeral port that records every
/// close-request it receives.
async fn spawn_close_request_recorder() -> (
    String,
    std::sync::Arc<tokio::sync::Mutex<Vec<SeenCloseRequest>>>,
) {
    use axum::routing::post;
    let seen = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<SeenCloseRequest>::new()));
    let sink = seen.clone();
    let app = axum::Router::new().route(
        CLOSE_REQUEST_PATH,
        post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let sink = sink.clone();
                async move {
                    sink.lock().await.push(SeenCloseRequest {
                        content_type: headers
                            .get(axum::http::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                        body: body.to_vec(),
                    });
                    axum::http::StatusCode::OK
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}

/// A handoff stop sends `{"handoff":"holders"}` as JSON on the close-request.
#[tokio::test]
async fn handoff_close_request_carries_the_handoff_body() {
    let (base, seen) = spawn_close_request_recorder().await;
    let client = reqwest::Client::new();
    let resp = post_close_request(&client, &base, CloseIntent::HandoffHolders)
        .await
        .expect("close-request sent");
    assert!(resp.status().is_success());

    let seen = seen.lock().await;
    assert_eq!(seen.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(&seen[0].body).expect("handoff body is JSON");
    assert_eq!(body, serde_json::json!({ "handoff": "holders" }));
    assert_eq!(seen[0].content_type.as_deref(), Some("application/json"));
}

/// A non-handoff stop sends exactly what it always did: a bodyless POST with
/// no `Content-Type`. The runner reads an absent body as a normal close.
#[tokio::test]
async fn normal_close_request_sends_no_body() {
    let (base, seen) = spawn_close_request_recorder().await;
    let client = reqwest::Client::new();
    post_close_request(&client, &base, CloseIntent::Kill)
        .await
        .expect("close-request sent");

    let seen = seen.lock().await;
    assert_eq!(seen.len(), 1);
    assert!(seen[0].body.is_empty(), "body was {:?}", seen[0].body);
    assert_eq!(seen[0].content_type, None);
    assert_eq!(CloseIntent::Kill.close_request_body(), None);
    assert_eq!(CloseIntent::default(), CloseIntent::Kill);
}

/// The graceful wait is extended in handoff mode ONLY: a normal stop keeps
/// the 3 s it always had, a handoff stop waits past the runner's own
/// teardown force-exit budget.
#[test]
fn graceful_wait_is_extended_only_for_a_handoff() {
    assert_eq!(
        CloseIntent::Kill.graceful_stop_timeout(),
        Duration::from_millis(RUNNER_GRACEFUL_STOP_TIMEOUT_MS)
    );
    assert_eq!(
        CloseIntent::HandoffHolders.graceful_stop_timeout(),
        Duration::from_millis(RUNNER_HANDOFF_GRACEFUL_STOP_TIMEOUT_MS)
    );
    assert!(
        CloseIntent::HandoffHolders.graceful_stop_timeout()
            > Duration::from_millis(crate::config::RUNNER_TEARDOWN_FORCE_EXIT_BUDGET_MS)
    );
    assert!(CloseIntent::from_handoff(true).is_handoff());
    assert!(!CloseIntent::from_handoff(false).is_handoff());
}

/// The stop's graceful poll is sized by the intent, and the intent reaches
/// the close-request. Pinned at source level because the stop path cannot
/// be driven end to end without a live runner process: a revert to the bare
/// constant would silently cut every handoff teardown at 3 s.
#[test]
fn stop_runner_by_id_sizes_its_wait_and_close_request_by_the_intent() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("process")
            .join("manager.rs"),
    )
    .expect("read manager.rs")
    .replace("\r\n", "\n");
    let src = crate::source_scan::production_span(&src).to_string();
    let stop = crate::source_scan::fn_body(&src, "pub async fn stop_runner_by_id(")
        .expect("stop_runner_by_id exists")
        .to_string();
    assert!(stop.contains("let graceful_timeout = intent.graceful_stop_timeout();"));
    assert!(stop.contains("request_graceful_stop(state, port, &runner_name, intent).await;"));
    assert!(
        !stop.contains("RUNNER_GRACEFUL_STOP_TIMEOUT_MS)"),
        "the graceful wait must come from the intent, not the bare default constant"
    );

    // And the restart funnel hands its intent to that stop.
    let after_gate = crate::source_scan::fn_body(&src, "async fn restart_after_gate(")
        .expect("restart_after_gate exists")
        .to_string();
    assert!(after_gate.contains("stop_runner_by_id(state, runner_id, true, intent).await?;"));
}

/// Under a handoff the graceful wait ends on the PID being gone, never on the
/// `running` flag alone — which, for a user-managed runner, one failed
/// `/health` probe during teardown would clear. A normal stop is unchanged.
#[test]
fn handoff_graceful_wait_ends_on_the_pid_not_the_running_flag() {
    let alive = |_: u32| true;
    let dead = |_: u32| false;
    let held = || true;
    let free = || false;

    // Registry PID: only the PID decides.
    let handoff = graceful_exit_signal(CloseIntent::HandoffHolders, Some(4242), true);
    assert_eq!(handoff, GracefulExitSignal::PidGone(4242));
    // The failed-probe case: `running` already reads false, the PID lives on.
    assert!(!graceful_exit_observed(handoff, false, alive, free));
    assert!(graceful_exit_observed(handoff, true, dead, held));

    // No known PID: falls back to today's flag (the stop logs that it did).
    let fallback = graceful_exit_signal(CloseIntent::HandoffHolders, None, false);
    assert_eq!(fallback, GracefulExitSignal::RunningFlag);

    // A normal stop keeps the flag whatever the PID says.
    let normal = graceful_exit_signal(CloseIntent::Kill, Some(4242), true);
    assert_eq!(normal, GracefulExitSignal::RunningFlag);
    assert!(graceful_exit_observed(normal, false, alive, held));
    assert!(!graceful_exit_observed(normal, true, dead, free));
}

/// A probe-derived PID is only a candidate: the wait also ends when `running`
/// is false AND the port is free — never on the flag alone, and never on a
/// free port while the runner is still believed running.
#[test]
fn a_probe_derived_pid_also_ends_on_a_cleared_flag_and_a_free_port() {
    let alive = |_: u32| true;
    let dead = |_: u32| false;
    let held = || true;
    let free = || false;

    let signal = graceful_exit_signal(CloseIntent::HandoffHolders, Some(7), false);
    assert_eq!(signal, GracefulExitSignal::PidGoneOrStoppedAndPortFree(7));
    assert!(graceful_exit_observed(signal, true, dead, held), "PID gone");
    assert!(
        graceful_exit_observed(signal, false, alive, free),
        "flag + port"
    );
    assert!(
        !graceful_exit_observed(signal, false, alive, held),
        "flag alone"
    );
    assert!(
        !graceful_exit_observed(signal, true, alive, free),
        "port alone"
    );
}

/// The liveness probe the handoff wait uses: this process is alive, and a
/// child that has exited and been reaped is not.
#[tokio::test]
async fn pid_alive_tracks_a_reaped_child() {
    assert!(crate::process::proc_kill::pid_alive(std::process::id()));
    let mut child = if cfg!(target_os = "windows") {
        tokio::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap()
    } else {
        tokio::process::Command::new("true").spawn().unwrap()
    };
    let pid = child.id().expect("child pid");
    child.wait().await.unwrap();
    assert!(!crate::process::proc_kill::pid_alive(pid));
}

/// The kill primitives each reap escalation step runs. Under a handoff only
/// the listener-only kill runs, at BOTH steps — every other primitive
/// (`kill_by_pid_tree`, and `kill_by_port`, which tree-kills each listener on
/// Unix) can reach the holder processes. A normal stop keeps today's ladder.
#[test]
fn handoff_reap_runs_only_the_listener_only_kill() {
    for outcome in [StopReapOutcome::EscalateTree, StopReapOutcome::EscalatePort] {
        let kills = reap_kills(outcome, CloseIntent::HandoffHolders);
        assert_eq!(kills, &[ReapKill::ListenersOnly], "{outcome:?}");
        assert!(
            kills.iter().all(|k| !k.reaches_beyond_the_listener()),
            "{outcome:?}: {kills:?}"
        );
    }
    assert_eq!(
        reap_kills(StopReapOutcome::EscalateTree, CloseIntent::Kill),
        &[ReapKill::TrackedPidTree, ReapKill::UntrackedListenerTree]
    );
    assert_eq!(
        reap_kills(StopReapOutcome::EscalatePort, CloseIntent::Kill),
        &[ReapKill::PortTree]
    );
    for outcome in [
        StopReapOutcome::Confirmed,
        StopReapOutcome::RetryAfterBackoff,
        StopReapOutcome::StillHeld,
    ] {
        for intent in [CloseIntent::Kill, CloseIntent::HandoffHolders] {
            assert!(reap_kills(outcome, intent).is_empty());
        }
    }
}

/// The stop's escalation runs exactly the kills `reap_kills` chose and no
/// other kill primitive: the only ones it names are inside `run_reap_kill`,
/// which dispatches on a [`ReapKill`]. This pins the WIRING between the
/// tested table and the loop, which cannot run without a live runner.
#[test]
fn stop_escalation_only_runs_kills_from_the_table() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("process")
            .join("manager.rs"),
    )
    .expect("read manager.rs")
    .replace("\r\n", "\n");
    let src = crate::source_scan::production_span(&src).to_string();
    let stop = crate::source_scan::fn_body(&src, "pub async fn stop_runner_by_id(")
        .expect("stop_runner_by_id exists")
        .to_string();
    assert!(stop.contains("let kills = reap_kills(outcome, intent);"));
    assert!(stop.contains("run_reap_kill("));
    for primitive in [
        "kill_by_pid_tree(",
        "kill_by_port(",
        "kill_listeners_by_port(",
    ] {
        assert!(
            !stop.contains(primitive),
            "stop_runner_by_id calls {primitive} directly"
        );
    }
    assert!(stop.contains("graceful_exit_observed("));
}

/// Phase 8b through the WHOLE non-primary block, against real `Command`s and
/// real instance roots: two flagged spawns get no harness `machine.json` and
/// have `QONTINUI_MACHINE_ID` removed; an unflagged spawn still gets the copy.
#[test]
fn fresh_device_identity_reaches_the_non_primary_env_block() {
    const PRIMARY_PORT: u16 = 9876;
    let (dirs, _user_dirs) = temp_user_dirs();
    let mut fresh_a = temp_config("test-fresh-id-a", 9877);
    let mut fresh_b = temp_config("test-fresh-id-b", 9878);
    fresh_a.fresh_device_identity = true;
    fresh_b.fresh_device_identity = true;
    let plain = temp_config("test-fresh-id-plain", 9879);
    let harness = tempfile::tempdir().unwrap();
    std::fs::write(
        harness.path().join("machine.json"),
        br#"{"device_id":"harness-device"}"#,
    )
    .unwrap();

    for cfg in [&fresh_a, &fresh_b] {
        let mut cmd = Command::new("cargo");
        cmd.env(
            crate::process::instance_root::MACHINE_ID_ENV,
            "harness-device",
        );
        let root = super::apply_non_primary_instance_env_with(
            &mut cmd,
            &dirs,
            cfg,
            PRIMARY_PORT,
            Some(harness.path()),
        )
        .expect("must resolve + create");
        assert!(
            !root.home_dir().join("machine.json").exists(),
            "{}: a fresh subject must hold no harness machine.json",
            cfg.id
        );
        assert_eq!(
            command_envs(&cmd).get(crate::process::instance_root::MACHINE_ID_ENV),
            Some(&None),
            "{}: QONTINUI_MACHINE_ID must be removed",
            cfg.id
        );
    }

    let mut cmd = Command::new("cargo");
    let root = super::apply_non_primary_instance_env_with(
        &mut cmd,
        &dirs,
        &plain,
        PRIMARY_PORT,
        Some(harness.path()),
    )
    .expect("must resolve + create");
    assert!(
        root.home_dir().join("machine.json").is_file(),
        "an unflagged subject still receives the harness machine.json"
    );
    assert_eq!(
        command_envs(&cmd).get(crate::process::instance_root::MACHINE_ID_ENV),
        None
    );
}

/// Mint a `restart` trial for a test restart of `managed`.
async fn mint_test_restart(
    state: &SharedState,
    managed: &Arc<ManagedRunner>,
) -> Arc<crate::dev_action::ActionRecord> {
    crate::dev_action::mint_action(
        state,
        crate::dev_action::ActionKind::Restart,
        format!("test;runner={}", managed.config.id),
        None,
        crate::dev_action::AttributionTargets::for_runner(managed.clone()),
        None,
    )
    .await
    .record
}

/// A restart whose stop RAN returns its failure marked
/// [`SupervisorError::AfterAct`], so the door's `run_result_for` records it
/// `Errored` on the door's own record (no store lookup), while the door's HTTP
/// answer is the wrapped error's, unchanged. A cached open port is evidence of
/// life, so the stop runs. The runner's port is one nothing holds
/// ([`crate::test_ports::unheld_port`]), so the stop's bind probe confirms it
/// free at once and the stop succeeds with nothing to kill. The start then fails
/// at exe resolution, and the failure is checked to be the start's: on a port a
/// socket holds, the stop's own confirmation never comes and its `StillHeld`
/// would satisfy `AfterAct` just as well.
#[tokio::test]
async fn a_restart_that_fails_after_its_stop_records_errored() {
    use crate::diagnostics::RestartSource;
    let (state, _root) = restart_test_state();
    let id = "named-after-stop";
    let managed = register_named_runner_on_port(&state, id, crate::test_ports::unheld_port()).await;
    managed.cached_health.write().await.runner_port_open = true;
    let record = mint_test_restart(&state, &managed).await;

    let err = restart_runner_by_id(
        &state,
        id,
        false,
        RestartSource::Manual,
        false,
        BuildTree::OriginMain,
        CloseIntent::Kill,
        Some(record.action_id),
    )
    .await
    .expect_err("no exe can be resolved from an empty project dir");
    assert!(
        managed.runner.read().await.stop_requested,
        "the stop must have run for this test to say anything"
    );
    let SupervisorError::AfterAct(inner) = &err else {
        panic!("a failure after the stop must be marked AfterAct: {err:?}");
    };
    assert!(
        matches!(&**inner, SupervisorError::Process(msg) if msg.starts_with("Runner exe not found")),
        "the failure after the stop must be the start's exe resolution, not the stop's: {inner:?}"
    );
    assert_eq!(err.to_string(), inner.to_string());
    assert_eq!(err.to_status_body(), inner.to_status_body());
    assert!(record.record_run_result(crate::dev_action::run_result_for(&err)));
    assert_eq!(
        record.run_result(),
        Some(crate::dev_action::ActionRunResult::Errored {
            reason: inner.to_string()
        })
    );
}

/// The same restart with NO evidence of life skips its stop, so nothing ran:
/// the error is not marked and the door's `run_result_for` maps it as it is.
#[tokio::test]
async fn a_restart_that_fails_without_a_stop_is_not_marked() {
    use crate::diagnostics::RestartSource;
    let (state, _root) = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-no-stop").await;

    let err = restart_runner_by_id(
        &state,
        "named-no-stop",
        false,
        RestartSource::Manual,
        false,
        BuildTree::OriginMain,
        CloseIntent::Kill,
        None,
    )
    .await
    .expect_err("no exe can be resolved from an empty project dir");
    assert!(!managed.runner.read().await.stop_requested);
    assert!(
        !matches!(err, SupervisorError::AfterAct(_)),
        "nothing ran, so nothing is marked: {err:?}"
    );
}

/// The precondition refusals a start can hit after its restart acted
/// (`RunnerAlreadyRunning` when a start slipped in, the non-primary port
/// `Validation`) map to `Refused` bare and to `Errored` once marked, carrying
/// the refusal's own message; the marker changes neither the message nor the
/// HTTP answer, and marking twice wraps once. Producing either refusal from a
/// real restart needs a live runner process.
#[test]
fn a_marked_precondition_refusal_is_errored_not_refused() {
    for err in [
        SupervisorError::RunnerAlreadyRunning,
        SupervisorError::Validation(
            "Runner 'x' is configured on port 9876, which a non-primary runner may not use"
                .to_string(),
        ),
    ] {
        let reason = err.to_string();
        let bare = err.to_status_body();
        assert_eq!(
            crate::dev_action::run_result_for(&err),
            crate::dev_action::ActionRunResult::Refused {
                reason: reason.clone()
            }
        );
        let marked = SupervisorError::after_act(SupervisorError::after_act(err));
        let SupervisorError::AfterAct(inner) = &marked else {
            panic!("after_act marks: {marked:?}");
        };
        assert!(
            !matches!(**inner, SupervisorError::AfterAct(_)),
            "marked twice: {marked:?}"
        );
        assert_eq!(
            crate::dev_action::run_result_for(&marked),
            crate::dev_action::ActionRunResult::Errored {
                reason: reason.clone()
            }
        );
        assert_eq!(marked.to_string(), reason);
        assert_eq!(marked.to_status_body(), bare);
    }
}

/// `rebuild_and_restart_by_id` builds before it starts, so its start failing
/// is a failure after the body acted: the start's error must leave marked.
/// Pinned at source level because a real build cannot run in a unit test.
#[test]
fn rebuild_and_restart_marks_a_failed_start_as_after_act() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("process")
        .join("manager.rs");
    let manager: String = std::fs::read_to_string(&path)
        .expect("manager.rs")
        .split_whitespace()
        .collect();
    let sig = "pubasyncfnrebuild_and_restart_by_id(";
    let body = &manager[manager.find(sig).expect("rebuild_and_restart_by_id exists")..];
    let body = body.split_once('{').expect("a body").1;
    let start = body
        .find("start_managed_runner(")
        .expect("rebuild_and_restart_by_id starts the runner");
    let after = &body[start..];
    let call = after.split_once(';').expect("the start statement ends").0;
    assert!(
        call.contains(".map_err(SupervisorError::after_act)?"),
        "rebuild_and_restart_by_id's start failure is not marked after_act: {call}"
    );
    assert!(
        body[..start].contains("Err(e)ifstopped=>returnErr(SupervisorError::after_act(e)),"),
        "rebuild_and_restart_by_id's build failure after a real stop is not marked after_act"
    );
}

/// A committed trial is dated at its commit, not at its preparation: the
/// serving watchdog prepares before the readiness gate and commits after it.
#[tokio::test]
async fn a_committed_trial_is_dated_at_its_commit() {
    let (state, _root) = restart_test_state();
    let prepared = crate::dev_action::prepare_action(
        &state,
        crate::dev_action::ActionKind::Restart,
        "test".to_string(),
        None,
    )
    .await;
    // Past any clock granularity, so a stamp taken at preparation cannot
    // equal one taken at the commit.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let before_commit = chrono::Utc::now();
    let instant_before_commit = tokio::time::Instant::now();
    let minted = crate::dev_action::commit_action(
        &state,
        prepared,
        crate::dev_action::AttributionTargets::default(),
        None,
    )
    .await;
    assert!(
        minted.record.started_at >= before_commit,
        "{} predates the commit at {before_commit}",
        minted.record.started_at
    );
    assert!(
        minted.record.started_instant >= instant_before_commit,
        "the watcher's window must start at the commit, not at preparation"
    );
}

/// Plan `2026-11-03-supervisor-dev-action-residuals-…` Phase 4.6:
/// [`live_early_log_paths`] returns the early log of a ring spawn whose exit is
/// not stamped and not that of one whose exit is, and the sweep's prune
/// ([`prune_early_logs`]) keeps the first and removes the second: the
/// snapshot's path form (`early_log_path_for` under the swept directory) matches
/// the `read_dir` entries it is compared against. The directory is the test's
/// own: this box's real one belongs to whichever supervisor is running on it,
/// whose sweep would race these back-dated files. That the writer and the sweep
/// resolve one directory is pinned apart: `early_log_dir` returns
/// `early_log_dir_path` (`early_log_dir_creates_subdir_under_temp`), and the
/// source scan beside the retention test in `routes::runners::tests` holds the
/// writer's and the sweep's calls to them.
#[tokio::test]
async fn live_early_log_paths_keeps_only_unexited_spawns_logs_end_to_end() {
    use crate::process::early_log::{early_log_path_for, EARLY_LOG_RETENTION};

    let (state, _root) = restart_test_state();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let mut config = crate::config::RunnerConfig::default_primary();
    config.id = format!("test-live-early-{suffix}");
    config.name = config.id.clone();
    let managed = Arc::new(ManagedRunner::new(config.clone(), false));
    state
        .runners
        .write()
        .await
        .insert(config.id.clone(), managed.clone());

    let dir = tempfile::tempdir().unwrap();
    let exited_log = early_log_path_for(dir.path(), &format!("{}-exited", config.id));
    let live_log = early_log_path_for(dir.path(), &format!("{}-live", config.id));
    let past_retention = SystemTime::now() - EARLY_LOG_RETENTION - Duration::from_secs(60);
    for path in [&exited_log, &live_log] {
        std::fs::write(path, "INFO booted, then silent\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(past_retention)
            .unwrap();
    }

    let first = chrono::Utc::now() - chrono::Duration::hours(3);
    let second = first + chrono::Duration::minutes(1);
    {
        let mut ring = managed.recent_spawns.write().await;
        ring.push(crate::state::SpawnMarker::new(
            first,
            Some(4242),
            Some(exited_log.clone()),
            None,
        ));
        assert!(ring.stamp_exit(first, first + chrono::Duration::seconds(30), None));
        ring.push(crate::state::SpawnMarker::new(
            second,
            Some(4243),
            Some(live_log.clone()),
            None,
        ));
    }

    let live = live_early_log_paths(&state).await;
    assert!(live.contains(&live_log), "{live:?}");
    assert!(!live.contains(&exited_log), "{live:?}");

    assert_eq!(prune_early_logs(&state, dir.path().to_path_buf()).await, 1);
    assert!(live_log.exists(), "the live spawn's early log was pruned");
    assert!(
        !exited_log.exists(),
        "the exited spawn's stale early log was kept"
    );

    // Once its runner leaves the registry the same file is pruned.
    state.runners.write().await.remove(&config.id);
    assert_eq!(prune_early_logs(&state, dir.path().to_path_buf()).await, 1);
    assert!(!live_log.exists());
}

/// The serving-watchdog events in the diagnostics ring, oldest first.
async fn serving_events(state: &SharedState) -> Vec<crate::diagnostics::ServingEvent> {
    state
        .diagnostics
        .read()
        .await
        .events(usize::MAX, None)
        .into_iter()
        .filter_map(|event| match event.kind {
            DiagnosticEventKind::ServingWatchdog { event, .. } => Some(event),
            _ => None,
        })
        .collect()
}

/// An answer counts for a spawn only when it came after that spawn started. With
/// no spawn recorded the stamp can only be this process's.
#[test]
fn an_answer_counts_only_for_the_spawn_it_came_after() {
    let started = chrono::Utc::now();
    let before = started - chrono::Duration::seconds(1);
    let after = started + chrono::Duration::seconds(1);
    assert!(!answered_during_spawn(Some(before), Some(started)));
    assert!(answered_during_spawn(Some(started), Some(started)));
    assert!(answered_during_spawn(Some(after), Some(started)));
    assert!(answered_during_spawn(Some(before), None));
    assert!(!answered_during_spawn(None, Some(started)));
    assert!(!answered_during_spawn(None, None));
}

/// A pre-bind alert is due once the spawn is alive, unanswered and past the
/// budget, then once per re-alert interval, and again at once for a new spawn.
#[test]
fn a_prebind_alert_is_due_past_the_budget_and_repeats_on_its_own_cadence() {
    let now = chrono::Utc::now();
    let spawn = now - chrono::Duration::seconds(200);
    let due =
        |up, answered, boot, last| prebind_alert_due(up, answered, boot, 90, spawn, last, now);
    assert!(due(true, false, 90, None));
    assert!(!due(true, false, 89, None), "inside the budget");
    assert!(!due(false, false, 200, None), "the spawn is not up");
    assert!(!due(true, true, 200, None), "it answered during this spawn");
    let recent = Some((
        spawn,
        now - chrono::Duration::seconds(PREBIND_REALERT_SECS - 1),
    ));
    assert!(
        !due(true, false, 200, recent),
        "alerted inside the interval"
    );
    let stale = Some((spawn, now - chrono::Duration::seconds(PREBIND_REALERT_SECS)));
    assert!(due(true, false, 200, stale));
    let older_spawn = Some((spawn - chrono::Duration::seconds(1), now));
    assert!(
        due(true, false, 200, older_spawn),
        "a new spawn starts over"
    );
}

/// A respawn that holds its port and never answers reads `UnresponsiveSince`
/// through the previous boot's stamp. It is alerted as never answered since its
/// spawn; a runner that answered during this spawn and went silent is not.
#[tokio::test]
async fn a_respawn_that_never_answered_is_alerted_as_never_answered() {
    use crate::diagnostics::ServingEvent;

    let started = chrono::Utc::now() - chrono::Duration::seconds(35);
    for (stamp, expected) in [
        (
            started - chrono::Duration::days(2),
            vec![ServingEvent::NeverAnsweredSinceSpawn],
        ),
        (started + chrono::Duration::seconds(5), vec![]),
    ] {
        let (state, _root) = restart_test_state();
        let managed = register_dead_primary(&state).await;
        {
            let mut runner = managed.runner.write().await;
            runner.last_seen_responding_at = Some(stamp);
            runner.started_at = Some(started);
        }
        maybe_serving_restart(
            &state,
            &managed,
            crate::state::RunnerLiveness::UnresponsiveSince(stamp),
            true,
        )
        .await;
        assert_eq!(serving_events(&state).await, expected, "stamp {stamp}");
        assert_eq!(managed.watchdog.read().await.serving_restart_attempts, 0);
    }
}

/// A primary alive on its newest spawn, silent and holding no port past the
/// first-healthy budget is alerted once, and not again inside the interval. One
/// whose spawn exited, or that answered during it, is not.
#[tokio::test]
async fn a_primary_hung_before_it_binds_is_alerted_once_per_interval() {
    use crate::diagnostics::ServingEvent;

    let started =
        chrono::Utc::now() - chrono::Duration::seconds(first_healthy_timeout_secs() as i64 + 5);
    let live_pid = std::process::id();
    for (exited, answered, stop_requested, expected) in [
        (
            false,
            false,
            false,
            vec![ServingEvent::NeverBoundSinceSpawn],
        ),
        (true, false, false, vec![]),
        (false, true, false, vec![]),
        (false, false, true, vec![]),
    ] {
        let (state, _root) = restart_test_state();
        let managed = register_dead_primary(&state).await;
        let mut marker = crate::state::SpawnMarker::new(started, Some(live_pid), None, None);
        if exited {
            marker.exited_at = Some(chrono::Utc::now());
        }
        managed.recent_spawns.write().await.push(marker);
        {
            let mut runner = managed.runner.write().await;
            runner.started_at = Some(started);
            runner.last_seen_responding_at = answered.then(chrono::Utc::now);
            runner.stop_requested = stop_requested;
        }
        maybe_alert_prebind_hang(&state, &managed).await;
        maybe_alert_prebind_hang(&state, &managed).await;
        let row = format!("exited={exited} answered={answered} stop_requested={stop_requested}");
        assert_eq!(serving_events(&state).await, expected, "{row}");
    }
}

/// A never-answered runner's silence counts from its boot's start, and is null
/// where no start is recorded. `serving_silence_secs` answers 0 for `Unknown`,
/// which the notice used to carry as if it had been measured.
#[test]
fn a_never_answered_silence_counts_from_the_start_or_is_null() {
    let now: chrono::DateTime<chrono::Utc> = "2026-11-04T10:06:40Z".parse().unwrap();
    let started: chrono::DateTime<chrono::Utc> = "2026-11-04T10:00:00Z".parse().unwrap();
    assert_eq!(never_answered_silence_secs(Some(started), now), Some(400));
    assert_eq!(never_answered_silence_secs(None, now), None);
    assert_eq!(
        never_answered_silence_secs(Some(now + chrono::Duration::seconds(5)), now),
        Some(0),
        "a start dated after now is clamped, not negative"
    );
    assert_eq!(
        serving_silence_secs(crate::state::RunnerLiveness::Unknown, Some(started), now),
        0,
        "the decision's silence is unchanged: an Unknown runner is never restarted"
    );
}

/// The never-answered notice for an `Unknown` runner carries that silence: the
/// seconds since its start, or null with no start recorded. Never 0 for a
/// runner 400 s into a boot.
#[tokio::test]
async fn a_never_answered_notice_carries_the_silence_since_its_start() {
    for started_ago in [Some(400_i64), None] {
        let (state, _root) = restart_test_state();
        let managed = register_dead_primary(&state).await;
        managed.runner.write().await.started_at =
            started_ago.map(|secs| chrono::Utc::now() - chrono::Duration::seconds(secs));
        maybe_serving_restart(
            &state,
            &managed,
            crate::state::RunnerLiveness::Unknown,
            true,
        )
        .await;
        let silences: Vec<Option<u64>> = state
            .diagnostics
            .read()
            .await
            .events(usize::MAX, None)
            .into_iter()
            .filter_map(|event| match event.kind {
                DiagnosticEventKind::ServingWatchdog {
                    silent_for_secs, ..
                } => Some(silent_for_secs),
                _ => None,
            })
            .collect();
        assert_eq!(silences.len(), 1, "started_ago={started_ago:?}");
        match started_ago {
            Some(_) => assert!(
                matches!(silences[0], Some(400..=402)),
                "counted from the start: {silences:?}"
            ),
            None => assert_eq!(silences[0], None, "nothing measured is null"),
        }
    }
}

/// The `RUNNER WEDGED` line says what the serving arm decided on that tick. It
/// used to say `Not restarting` on every tick, including the one where the arm
/// had just decided to restart.
#[test]
fn the_wedged_line_says_what_the_serving_arm_decided() {
    let restart = serving_decision_clause(Some(ServingRestartDecision::Restart { attempt: 2 }));
    assert_eq!(
        restart,
        "The serving watchdog decided on this tick to RESTART it (attempt 2/3), still gated \
         on a zero session census"
    );
    assert!(!restart.contains("Not restarting"));
    assert_eq!(
        serving_decision_clause(Some(ServingRestartDecision::SkipBelowThreshold)),
        "Not restarting yet: the silence is below the serving threshold"
    );
    assert_eq!(
        serving_decision_clause(Some(ServingRestartDecision::SkipNotArmed)),
        "Not restarting: the serving watchdog is not armed for it (--watchdog absent, or the \
         kill-switch is set)"
    );
    assert_eq!(
        serving_decision_clause(None),
        "The serving watchdog made no restart decision on this tick",
        "no decision evaluated is said, never a claimed one"
    );
}

/// Phase 4's arming test (plan `2026-11-05-supervisor-test-flakes-dead-ports-etxtbsy`):
/// a unit test never signals a process it did not spawn. A stop whose registry pid
/// names an orphan this test made but is not the parent of reaches its pid kill, and
/// the kill guard panics there before any signal: the orphan lives. With the guard
/// gone the stop kills the orphan and returns, and this test fails on both counts.
///
/// The orphan (`test_kill_guard::Orphan`) is `sh`'s background `sleep` on
/// Unix and `cmd /c start /b`'s `ping` on Windows: alive, this user's, and reparented
/// away from (Unix) or left with a dead parent pid by (Windows) this process. The
/// runner is named and unprotected on a port nothing holds, so the readiness gate
/// skips, the close-request is refused at once, and the ladder's first kill is the
/// registry pid's, which no image check stands in front of. The test ends the
/// orphan itself, directly and only while its pid still names it, never through
/// a guarded primitive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_unit_test_stop_never_signals_a_process_it_did_not_spawn() {
    use crate::process::test_kill_guard;
    let orphan_process = test_kill_guard::Orphan::spawn();
    let orphan = orphan_process.pid;
    assert!(
        crate::process::proc_kill::pid_alive(orphan)
            && !test_kill_guard::is_descendant_of_this_process(orphan),
        "the fixture: an orphan {orphan} that is alive and not this test's descendant"
    );

    let (state, _root) = restart_test_state();
    let id = "named-kill-guard-arming";
    let managed = register_dead_named_runner(&state, id).await;
    {
        let mut runner = managed.runner.write().await;
        runner.pid = Some(orphan);
        runner.running = true;
    }

    let stop = tokio::spawn({
        let state = state.clone();
        async move { stop_runner_by_id(&state, id, true, CloseIntent::Kill).await }
    })
    .await;
    let alive_after = crate::process::proc_kill::pid_alive(orphan);

    let refusal = match stop {
        Err(join) if join.is_panic() => {
            let payload = join.into_panic();
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default()
        }
        other => panic!(
            "the stop must panic in the kill guard before it signals pid {orphan}, but it \
             returned {other:?} (the orphan is {})",
            if alive_after {
                "alive"
            } else {
                "dead: the stop killed it"
            }
        ),
    };
    assert!(
        refusal.starts_with(test_kill_guard::REFUSAL)
            && refusal.contains(&format!("reaches pid {orphan} (")),
        "the stop panicked, but not in the kill guard over pid {orphan}: {refusal}"
    );
    assert!(
        alive_after,
        "the guard panicked, yet the orphan {orphan} is dead: something signalled it first"
    );
    drop(orphan_process);
}
