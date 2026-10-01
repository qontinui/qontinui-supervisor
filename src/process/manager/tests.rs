#![cfg(test)]

use super::*;
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
    // Unbound port: bind ephemeral, read, drop.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
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

// Per-instance config/secure-storage dir, asserted at the site that
// actually decides where the child reads.
//
// `start_exe_mode_for_runner` applies `apply_instance_dir_env` to every
// non-primary child, and `spawn-test` only ever mints non-primary
// (`RunnerKind::Temp`) runners — so the dir this function sets IS the
// directory a spawn-test runner loads its pairing and token cache from.
// `routes::runners` writes the `paired_profile_id` snapshot into
// `instance_config_dir`'s output for the same id.
//
// The assertion is made over a REAL `Command`'s env map rather than over a
// re-derivation of the helper's body: re-inlining a divergent path at the
// spawn's `cmd.env(...)` site — the failure this test exists to catch —
// fails here. (It covers the spawn side only; the route side is pinned by
// `routes::runners::tests::instance_config_dir_is_where_apply_paired_profile_for_spawn_writes`.)
#[test]
fn apply_instance_dir_env_sets_both_vars_to_instance_config_dir() {
    let id = format!("test-apply-instance-dir-env-{}", std::process::id());
    let Some(expected) = instance_config_dir(&id) else {
        // No resolvable config dir on this platform — nothing to assert.
        return;
    };
    let _cleanup = scopeguard::guard(expected.clone(), |dir| {
        let _ = std::fs::remove_dir_all(dir);
    });

    let mut cmd = Command::new("cargo");
    let applied = super::apply_instance_dir_env(&mut cmd, &id).expect("must resolve + create");
    assert_eq!(applied, expected);

    let envs: std::collections::HashMap<String, Option<String>> = cmd
        .as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let want = expected.to_string_lossy().into_owned();
    for key in ["QONTINUI_CONFIG_DIR", "QONTINUI_SECURE_STORAGE_DIR"] {
        assert_eq!(
            envs.get(key).cloned().flatten().as_deref(),
            Some(want.as_str()),
            "{key} must be exported to the child as instance_config_dir({id}) = {expected:?}; \
                 a spawn that inlines a different path leaves the runner unpaired"
        );
    }
    assert!(expected.is_dir(), "the instance dir must have been created");
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
    }
}

/// The non-primary env block, asserted **as a whole** against a real
/// `Command` — `QONTINUI_INSTANCE_NAME`, `QONTINUI_PRIMARY_PORT` and
/// `WEBVIEW2_USER_DATA_FOLDER` together, because it is the combination
/// that isolates a secondary's state from the primary's and from other
/// secondaries'. Sibling of
/// `apply_instance_dir_env_sets_both_vars_to_instance_config_dir`, which
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
    let pid = std::process::id();
    let first = temp_config(&format!("test-envblock-a-{pid}"), PORT);
    let second = temp_config(&format!("test-envblock-b-{pid}"), PORT);

    if instance_config_dir(&first.id).is_none() {
        // No resolvable config dir on this platform — nothing to assert.
        return;
    }
    // Both dirs land under the operator's REAL app-data roots (the block
    // creates them), so they must not survive a failing assertion below.
    // Same guard shape as `apply_instance_dir_env_sets_both_vars_to_…`.
    let _cleanup = scopeguard::guard(
        vec![first.id.clone(), second.id.clone()],
        |ids: Vec<String>| {
            for id in ids {
                if let Some(dir) = instance_config_dir(&id) {
                    let _ = std::fs::remove_dir_all(dir);
                }
                #[cfg(target_os = "windows")]
                if let Some(dir) = webview2_user_data_folder(&id, false) {
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
        },
    );

    let mut cmd_a = Command::new("cargo");
    let mut cmd_b = Command::new("cargo");
    super::apply_non_primary_instance_env(&mut cmd_a, &first, PRIMARY_PORT)
        .expect("must resolve + create");
    super::apply_non_primary_instance_env(&mut cmd_b, &second, PRIMARY_PORT)
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

        #[cfg(target_os = "windows")]
        {
            let want = webview2_user_data_folder(&config.id, false)
                .expect("LOCALAPPDATA resolves on Windows")
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                envs.get("WEBVIEW2_USER_DATA_FOLDER").cloned().flatten(),
                Some(want),
                "the WebView2 profile must be keyed on the per-spawn runner id \
                     so two temps never share localStorage/IndexedDB/cookies"
            );
        }
        #[cfg(not(target_os = "windows"))]
        let _ = config;
    }

    #[cfg(target_os = "windows")]
    assert_ne!(
        envs_a.get("WEBVIEW2_USER_DATA_FOLDER"),
        envs_b.get("WEBVIEW2_USER_DATA_FOLDER"),
        "same-port spawns must not share a WebView2 profile"
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
        decide_crash_restart(true, false, false, true, true, false, 0),
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
        decide_crash_restart(true, false, false, true, true, false, 1),
        CrashRestartDecision::Restart {
            attempt: 2,
            delay_secs: 30
        }
    );
    assert_eq!(
        decide_crash_restart(true, false, false, true, true, false, 2),
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
        decide_crash_restart(true, false, false, true, true, false, 3),
        CrashRestartDecision::Disarm
    );
    // Even further past the budget it stays Disarm, never Restart.
    assert_eq!(
        decide_crash_restart(true, false, false, true, true, false, 7),
        CrashRestartDecision::Disarm
    );
}

/// Operator stop intent wins over everything else armed — never restart
/// a runner the operator deliberately stopped.
#[test]
fn crash_restart_operator_stop_never_restarts() {
    assert_eq!(
        decide_crash_restart(true, true, false, true, true, false, 0),
        CrashRestartDecision::SkipOperatorStop
    );
    // Even a non-clean exit after a stop request (taskkill path) skips.
    assert_eq!(
        decide_crash_restart(true, true, true, true, true, false, 0),
        CrashRestartDecision::SkipOperatorStop
    );
}

/// A clean exit (code 0) is a deliberate shutdown (window close,
/// internal exit) — crash-only means we never restart it.
#[test]
fn crash_restart_clean_exit_skips() {
    assert_eq!(
        decide_crash_restart(true, false, true, true, true, false, 0),
        CrashRestartDecision::SkipCleanExit
    );
}

/// No Child handle → the supervisor never spawned this process; no
/// provenance to restart it.
#[test]
fn crash_restart_requires_spawn_provenance() {
    assert_eq!(
        decide_crash_restart(false, false, false, true, true, false, 0),
        CrashRestartDecision::SkipNoChildHandle
    );
}

/// Global arm off (no --watchdog, or the env kill-switch) → skip.
#[test]
fn crash_restart_global_arm_gates() {
    assert_eq!(
        decide_crash_restart(true, false, false, false, true, false, 0),
        CrashRestartDecision::SkipNotArmed
    );
}

/// Per-runner enabled=false (default for named/temp/external) → skip.
#[test]
fn crash_restart_per_runner_enabled_gates() {
    assert_eq!(
        decide_crash_restart(true, false, false, true, false, false, 0),
        CrashRestartDecision::SkipDisabled
    );
}

/// Once disarmed (`disabled_reason` set), restarts stay off until an
/// operator resets — even with a fresh (pruned) window.
#[test]
fn crash_restart_disarmed_latch_holds() {
    assert_eq!(
        decide_crash_restart(true, false, false, true, true, true, 0),
        CrashRestartDecision::SkipDisarmed
    );
}

/// The two CRASH WARN-worthy skip variants (`SkipNotArmed` /
/// `SkipDisarmed`, promoted to `warn!` + persisted emit in
/// `maybe_crash_restart`) are ONLY reachable for a genuine crash — a held
/// Child handle, no operator stop, and an unclean exit. The benign skips
/// for the same crash inputs (clean exit, operator stop, no child handle)
/// classify away from the WARN set, so the CRASH promotion never fires on
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
    let not_armed = decide_crash_restart(true, false, false, false, true, false, 0);
    assert_eq!(not_armed, CrashRestartDecision::SkipNotArmed);
    assert!(matches!(
        not_armed,
        CrashRestartDecision::SkipNotArmed | CrashRestartDecision::SkipDisarmed
    ));
    // Genuine crash, armed+enabled but disarmed latch → WARN-worthy SkipDisarmed.
    let disarmed = decide_crash_restart(true, false, false, true, true, true, 0);
    assert_eq!(disarmed, CrashRestartDecision::SkipDisarmed);
    // Benign skips for the SAME arm state must NOT be in the WARN-worthy set.
    for benign in [
        decide_crash_restart(true, false, true, false, true, false, 0), // clean exit
        decide_crash_restart(true, true, false, false, true, false, 0), // operator stop
        decide_crash_restart(false, false, false, false, true, false, 0), // no child handle
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
    let mut config = crate::config::SupervisorConfig::from_args(
        <crate::config::CliArgs as clap::Parser>::parse_from(["test", "--project-dir", "."]),
    );
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
    // Up to the function's own closing brace, by brace matching, so a match
    // cannot come from a neighbouring function.
    let body_of = |sig: &str| {
        crate::source_scan::fn_body(&src, sig)
            .unwrap_or_else(|| panic!("{sig} exists"))
            .to_string()
    };

    let stop = body_of("pub async fn stop_runner_by_id(");
    assert!(
        stop.contains("let restarting = managed.runner.read().await.restart_requested;")
            && stop
                .split_once("if reap_instance_state_on_stop(restarting) {")
                .and_then(|(_, gated)| gated.split('}').next())
                .is_some_and(
                    |gated| gated.contains("reap_runner_instance_state(&runner_id, &runner_name)")
                ),
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

    let restart = body_of("pub async fn restart_runner_by_id(");
    assert!(
        restart
            .contains("if orphaned_by_failed_restart(is_temp_runner(runner_id), still_registered)")
            && restart.contains("reap_runner_instance_state(runner_id, &managed.config.name)"),
        "a failed temp restart must reap the per-instance state its stop half kept"
    );

    // The other teardown paths must route through the helper too, not
    // re-spell its set: the max-age sweep here, and `DELETE
    // /runners/{id}` / `purge-stale` in routes/runners.rs.
    let sweep = body_of("pub async fn reap_stale_test_runners(");
    assert!(sweep.contains("reap_runner_instance_state(&id, &name)"));
    // Startup-window grace (plan
    // 2026-09-19-supervisor-reaper-purges-an-in-flight-spawn-test): the
    // endless sweep loop cannot be driven from a test, so pin that it
    // still consults the shared, unit-tested skip decision.
    assert!(
        sweep.contains("skip_for_startup_window(managed, is_running)"),
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
    let routes = crate::source_scan::production_span(&routes).to_string();
    assert_eq!(
        routes
            .matches("manager::reap_runner_instance_state(&id, &name)")
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
        "remove_runner_app_data_dirs(runner_name, false)",
        "remove_instance_config_dir(runner_id, false)",
    ] {
        assert!(
            helper.contains(removal),
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
    let pid = std::process::id();
    let runner_id = format!("named-reap-selftest-id-{pid}");
    let runner_name = format!("reap-selftest-name-{pid}");

    let Some(config_dir) = instance_config_dir(&runner_id) else {
        // No resolvable config dir on this platform — nothing to assert.
        return;
    };
    let mut app_data = crate::process::app_data_dir_candidates(&format!(
        "instance-{}",
        crate::process::sanitize_instance_name(&runner_name)
    ));
    app_data.sort();
    app_data.dedup();

    // These live under the operator's REAL config/data dirs, so they must
    // not survive a failing assertion (same guard as process::tests).
    let mut cleanups = Vec::new();
    for dir in std::iter::once(&config_dir).chain(app_data.iter()) {
        std::fs::create_dir_all(dir).expect("create dir");
        cleanups.push(scopeguard::guard(dir.clone(), |d| {
            let _ = std::fs::remove_dir_all(d);
        }));
        std::fs::write(dir.join("marker.json"), b"{}").expect("write marker");
    }

    reap_runner_instance_state(&runner_id, &runner_name).await;

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
    std::fs::copy(&source_exe, &dest_exe).expect("copy exe");
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
    assert!(restart_should_stop(false, Some(247_696), true));
    // Linux adopted wedge: no PID recovered (the health cache's PID
    // recovery is Windows-only), port still held.
    assert!(restart_should_stop(false, None, true));
    // A PID with no port: the process exists, stop it.
    assert!(restart_should_stop(false, Some(1), false));
    // Tracked running alone (the old predicate) still stops.
    assert!(restart_should_stop(true, None, false));
    // Nothing to stop.
    assert!(!restart_should_stop(false, None, false));
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
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    assert!(refuse_if_port_held_by_live_runner(dead_port).await.is_ok());
}

fn restart_test_state() -> SharedState {
    use crate::config::{CliArgs, SupervisorConfig};
    use clap::Parser;
    let args = CliArgs::parse_from(["test", "--project-dir", "."]);
    Arc::new(crate::state::SupervisorState::new(
        SupervisorConfig::from_args(args),
    ))
}

/// A registered, UNPROTECTED named runner on a port nothing listens on.
/// Unprotected so the readiness gate skips (`NotProtected`) and the
/// funnel reaches the latch; a dead port so any stop confirms instantly
/// and the start fails at exe resolution (this state has no build slot
/// and no cargo target-dir artifact).
async fn register_dead_named_runner(state: &SharedState, id: &str) -> Arc<ManagedRunner> {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
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
    let state = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-p3-start-fail").await;

    let err = restart_runner_by_id(
        &state,
        "named-p3-start-fail",
        false,
        RestartSource::Manual,
        false,
        BuildTree::OriginMain,
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
    let state = restart_test_state();
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

/// The wire `watchdog` source is still refused for a non-temp runner, and
/// the refusal happens BEFORE anything is latched or emitted.
#[tokio::test]
async fn wire_watchdog_source_is_still_blocked_for_non_temp_runners() {
    use crate::diagnostics::RestartSource;
    let state = restart_test_state();
    let managed = register_dead_named_runner(&state, "named-p3-blocked").await;

    let err = restart_runner_by_id(
        &state,
        "named-p3-blocked",
        false,
        RestartSource::Watchdog,
        false,
        BuildTree::OriginMain,
    )
    .await
    .expect_err("wire watchdog must be blocked");
    assert!(matches!(err, SupervisorError::Validation(_)), "{err}");
    assert!(err.to_string().contains("blocked"), "{err}");
    assert!(!managed.runner.read().await.restart_requested);
}
