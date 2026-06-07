mod bazel_remote;
mod build_monitor;
mod build_submissions;
mod cache_key;
mod cache_telemetry;
// Phase 3b (self-hosted CI runners): probes WSL-based GitHub Actions
// runner services via `wsl -e systemctl ...` every 30s. Stores status
// on SupervisorState for fleet heartbeat consumption and auto-restarts
// crashed services (rate-limited to 3/hour).
mod ci_runner_lifecycle;
mod ci_runner_probe;
mod config;
mod dev_action;
mod diagnostics;
mod error;
mod evaluation;
mod expo;
mod footprint;
// Row 2 Phase 1 (fleet topology): detects CPU/RAM/disk and POSTs
// `max_concurrent_builds` to coord on startup. See
// `plans/2026-05-14-fleet-topology-and-build-pool-design.md` §3.2.
mod fleet;
// Stream E (spec-check v1, plan 05 §10): nightly cron that composes
// /spec/proposals/scan → /execute (per proposal) → /sweep-pending on the
// primary runner. No-op when the runner's spec-authoring feature is off
// (returns 404 → warn-once + skip). See `src/flywheel.rs`.
mod flywheel;
mod fs_atomic;
mod git_provenance;
mod health_cache;
mod log_capture;
mod otel;
mod pii_scrub;
mod process;
mod reapi;
mod routes;
mod sdk_features;
mod server;
mod settings;
mod spawn_worktree;
mod state;
mod trace_propagation;
mod velocity;
mod velocity_improvement;
mod velocity_layer;
mod velocity_tests;
#[cfg(windows)]
mod webview;
mod wsl_util;

use clap::Parser;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::{CliArgs, SupervisorConfig};
use log_capture::{LogLevel, LogSource};
use state::SupervisorState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse args early so we can resolve dev_logs path for velocity layer
    let args = CliArgs::parse();

    // Resolve dev_logs directory (sibling to project dir's grandparent)
    let dev_logs_dir = args
        .project_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&args.project_dir)
        .join(".dev-logs");
    let _ = std::fs::create_dir_all(&dev_logs_dir);

    // Clear previous velocity JSONL on startup
    velocity_layer::clear_velocity_jsonl(&dev_logs_dir);

    // Initialize tracing with velocity layer for HTTP span capture +
    // Row 9 Phase 5 OpenTelemetry export. The OTel guard must outlive
    // the subscriber so the batched span exporter flushes on shutdown;
    // we capture it into a let-binding below `init` and drop it at
    // program exit.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "qontinui_supervisor=info,tower_http=info".into());
    let fmt_layer = tracing_subscriber::fmt::layer();
    let velocity = velocity_layer::VelocityLayer::new(dev_logs_dir);
    let (_otel_guard, otel_tracer) = otel::init_otel("qontinui-supervisor");
    let otel_layer = otel_tracer.map(|t| tracing_opentelemetry::layer().with_tracer(t));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(otel_layer)
        .with(fmt_layer)
        .with(velocity)
        .init();

    // Route EVERY panic through tracing (in addition to the default stderr
    // hook), so a panic in a detached `tokio::spawn` build task — whose
    // `JoinError` nobody observes — is never silent in supervisor.log. The
    // build-pipeline silent-wedge regression (a panic in the Windows spawn
    // path unwinding the inline/detached build task, leaving the slot idle
    // with no log and no timeout) is invisible without this. Chains the
    // previous hook so the standard panic message + backtrace still print.
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown>".to_string());
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            error!(
                target: "qontinui_supervisor::panic",
                "PANIC at {location}: {payload}"
            );
            default_hook(info);
        }));
    }

    info!(
        "Starting qontinui-supervisor v{}",
        env!("CARGO_PKG_VERSION")
    );
    info!("Project dir: {:?}", args.project_dir);
    info!("Watchdog: {}", args.watchdog);
    info!("Auto-start: {}", args.auto_start || args.watchdog);
    info!("Auto-debug: {}", args.auto_debug);
    if let Some(ref expo_dir) = args.expo_dir {
        info!("Expo dir: {:?}", expo_dir);
    }

    // Validate project dir
    if !args.project_dir.exists() {
        error!("Project directory does not exist: {:?}", args.project_dir);
        std::process::exit(1);
    }

    let mut config = SupervisorConfig::from_args(args);

    // Load persistent settings to check for saved runner configs
    {
        // Per-instance settings path (namespaced under
        // dev_logs_dir/instances/<instance-key>/) — NOT the flat path. This is
        // what isolates each supervisor instance's runner registry; see
        // `settings::settings_path`. Also performs the one-shot legacy
        // migration for the default instance.
        let settings_path = settings::settings_path(&config);
        let saved = settings::load_settings(&settings_path);
        if !saved.runners.is_empty() {
            info!(
                "Loaded {} runner configs from settings",
                saved.runners.len()
            );
            config.runners = saved.runners;
            // Ensure there's always a primary
            if !config.runners.iter().any(|r| r.kind().is_primary()) {
                warn!("No primary runner in saved settings, adding default");
                config
                    .runners
                    .insert(0, crate::config::RunnerConfig::default_primary());
            }
        }
    }

    let port = config.port;
    // `--auto-start` (which `--watchdog` implies) requests that the supervisor
    // boot-start the PRIMARY runner once. Consumed below, after the orphan scan
    // has had a chance to adopt a surviving primary. Lost in c415c5f (an
    // `let _auto_start = ...` underscore-silenced the field); restored here.
    let auto_start = config.auto_start;

    let supervisor_state = SupervisorState::new(config);

    // Attach persistent log file writer for supervisor-wide logs (if configured).
    // Done before Arc-wrapping so any startup emits are captured. Per-runner
    // log files are attached inside SupervisorState::new / ManagedRunner::new.
    if let Some(ref path) = supervisor_state.config.log_file {
        if let Some(writer) = log_capture::open_append_log(path) {
            supervisor_state.logs.set_file_writer(Some(writer));
            info!("Supervisor log file: {}", path.display());
        } else {
            warn!(
                "Supervisor log file could not be opened at {} — continuing without persistent logging",
                path.display()
            );
        }
    }

    let state = Arc::new(supervisor_state);

    // Drain any messages captured during synchronous SupervisorState::new
    // construction (e.g. JobObject creation success/failure) into the
    // dashboard log stream. Done after Arc-wrapping so the messages flow
    // through the same persistent file writer attached above.
    state.flush_pending_startup_logs().await;

    // Visibility for the debug-endpoints gate. When enabled, log loudly so
    // an operator tailing the supervisor log can see that
    // `/control/dev/*` are accepting requests; when disabled (the default),
    // no log line is emitted to keep startup quiet for normal users.
    if state.debug_endpoints_enabled {
        let msg = format!(
            "Debug endpoints ENABLED ({}=1) — POST /control/dev/* are admitted on this supervisor. \
             This must NEVER be set in shared / multi-tenant deployments.",
            state::DEBUG_ENDPOINTS_ENV
        );
        warn!("{}", msg);
        state
            .logs
            .emit(LogSource::Supervisor, LogLevel::Warn, msg)
            .await;
    }

    // Load persistent settings and apply to state
    {
        let path = settings::settings_path(&state.config);
        let saved = settings::load_settings(&path);
        let mut ai = state.ai.write().await;
        if let Some(provider) = saved.ai_provider {
            ai.provider = provider;
        }
        if let Some(model) = saved.ai_model {
            ai.model = model;
        }
        if let Some(auto_debug) = saved.auto_debug_enabled {
            ai.auto_debug_enabled = auto_debug;
        }
        info!(
            "Loaded settings: provider={}, model={}, auto_debug={}",
            ai.provider, ai.model, ai.auto_debug_enabled
        );
    }

    // Log startup
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Supervisor starting on port {}", port),
        )
        .await;

    // Spawn health cache refresher (caches expensive port checks every 2s)
    let _health_cache_handle = health_cache::spawn_health_cache_refresher(state.clone());

    // Row 2 Phase 1: publish this supervisor's MachineBudget (role=build,
    // max_concurrent_builds derived from CPU + RAM per §3.2) to coord on
    // startup. Fire-and-forget — supervisor never blocks on coord
    // availability. See `fleet.rs::publish_on_startup`.
    tokio::spawn(async move {
        fleet::publish_on_startup().await;
    });

    // Phase 3b: CI runner health probe loop. Probes WSL-based GitHub
    // Actions runner services every 30s. Stores aggregate status on
    // SupervisorState for fleet heartbeat consumption. Auto-restarts
    // crashed services (rate-limited to 3/hour). Fire-and-forget —
    // probe failures are swallowed; the loop never panics.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            ci_runner_probe::ci_runner_probe_loop(state_clone).await;
        });
    }

    // Layer 3 of the orphan-runner safety net: scan for `qontinui-runner.exe`
    // processes left over from a prior supervisor instance and either adopt
    // them back into the registry (if a registered runner config claims their
    // port) or kill them so the next build can replace the slot binary.
    //
    // Awaited (not spawned) so it serializes with the rest of startup — no
    // prewarm or build can begin while orphans still hold slot binaries.
    process::orphan_scan::scan_orphans_at_startup(&state).await;

    // Clean up any orphaned temp runner processes from previous sessions
    // and detect already-running user runners for health tracking.
    //
    // The supervisor does NOT auto-start any runners UNLESS `--auto-start`
    // (or `--watchdog`, which implies it) was passed — an explicit operator
    // request. When that flag is set, the boot-start task spawned below starts
    // the PRIMARY once (and only the primary); named/temp/external runners stay
    // user-managed in every case.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            process::manager::cleanup_orphaned_runners(&state_clone).await;
        });
    }

    // `--auto-start` / `--watchdog`: boot-start the PRIMARY runner once.
    //
    // The orphan scan above (awaited) may have already adopted a surviving
    // primary back into the registry with `running = true`; in that case the
    // decision function returns None and we leave it alone. We give the HTTP
    // server + state a few seconds to settle first, mirroring the historical
    // behavior where the boot start showed up ~4s after "Supervisor starting".
    //
    // Auto-start funnels through `start_runner_by_id` exactly like an operator
    // `POST /runners/primary/start`, so the #65 provenance start gate applies:
    // if it refuses (e.g. a foreign slot binary), the error is logged as a
    // WARN and boot continues — it never crashes startup. This is one-shot:
    // the supervisor still never restarts a primary that crashes later.
    if auto_start {
        let state_for_autostart = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let decision = {
                let runners = state_for_autostart.runners.read().await;
                let primary = runners.values().find(|r| r.config.kind().is_primary());
                match primary {
                    Some(p) => {
                        let running = p.runner.read().await.running;
                        primary_to_boot_start(true, Some((p.config.id.as_str(), running)))
                    }
                    None => primary_to_boot_start(true, None),
                }
            };
            match decision {
                None => {
                    info!(
                        "auto-start: primary already running (or no primary registered) — nothing to boot-start"
                    );
                }
                Some(primary_id) => {
                    info!("auto-start: boot-starting primary runner '{}'", primary_id);
                    match process::manager::start_runner_by_id(&state_for_autostart, &primary_id)
                        .await
                    {
                        Ok(()) => {
                            info!("auto-start: primary runner '{}' started", primary_id);
                            state_for_autostart
                                .logs
                                .emit(
                                    LogSource::Supervisor,
                                    LogLevel::Info,
                                    format!("Auto-started primary runner '{}' at boot", primary_id),
                                )
                                .await;
                        }
                        Err(e) => {
                            warn!(
                                "auto-start: failed to boot-start primary runner '{}': {} \
                                 (supervisor stays healthy; start the primary manually via \
                                 POST /runners/{}/start)",
                                primary_id, e, primary_id
                            );
                            state_for_autostart
                                .logs
                                .emit(
                                    LogSource::Supervisor,
                                    LogLevel::Warn,
                                    format!(
                                        "Auto-start of primary runner '{}' failed: {}",
                                        primary_id, e
                                    ),
                                )
                                .await;
                        }
                    }
                }
            }
        });
    }

    // Background reaper: periodically purge stale/crashed test runners so they
    // don't exhaust the port range (9877-9899). Runs every 5 minutes.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            process::manager::reap_stale_test_runners(state_clone).await;
        });
    }

    // Background reaper: periodically clear build slots that have been "busy"
    // for longer than max_build_age_secs (default 15 minutes). This catches
    // slots leaked by crashed cargo builds or supervisor panics where the
    // SlotGuard RAII type didn't run its Drop. Without this, the pool fills
    // with phantom "building" entries that block spawn-test indefinitely.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            reap_stuck_build_slots(state_clone).await;
        });
    }

    // Also periodically sweep stale test-runner placeholders. `spawn-test`
    // reserves a placeholder before the build finishes; when the build fails
    // or the user aborts the supervisor mid-spawn, the placeholder never
    // reaches `running = true` and lingers in the registry, consuming a port
    // slot. The same `purge_stale_test_runners_core` helper that backs
    // `POST /runners/purge-stale` is called here on a 5-minute cadence so the
    // registry drains without operator intervention.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            reap_stale_test_runners(state_clone).await;
        });
    }

    // Spawn flywheel cron (Stream E — spec coverage-growth loop). Time-driven
    // (24h cadence by default, env-overridable); targets the primary runner
    // at port 9876 only. No-op when the runner's `spec-authoring` feature is
    // compiled out — the loop logs warn-once on 404 and continues to the
    // next tick. Errors are swallowed; the loop never panics. See
    // `src/flywheel.rs` for the cron contract.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            flywheel::flywheel_loop(state_clone).await;
        });
    }

    // Build and start HTTP server (with SO_REUSEADDR to handle lingering sockets)
    let router = server::build_router(state.clone());
    let bind_addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse()?;
    let listener = {
        let mut attempts = 0;
        loop {
            let socket = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )?;
            socket.set_reuse_address(true)?;
            socket.set_nonblocking(true)?;
            match socket.bind(&bind_addr.into()) {
                Ok(()) => {
                    socket.listen(1024)?;
                    let std_listener: std::net::TcpListener = socket.into();
                    break tokio::net::TcpListener::from_std(std_listener)?;
                }
                Err(e) if attempts < 30 => {
                    drop(socket);
                    attempts += 1;
                    if attempts == 1 || attempts % 5 == 0 {
                        warn!(
                            "Port {} busy ({}), retrying in 2s ({}/30)...",
                            port, e, attempts
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    info!("Supervisor listening on http://127.0.0.1:{}", port);

    // Spawn the ambient dashboard WebView2 window (item B of the post-3J UI
    // Bridge improvements plan). Runs on its own dedicated OS thread so it
    // can own the Win32 message pump without fighting the tokio runtime.
    // The webview loads the supervisor's own React SPA, which then
    // auto-registers with supervisor-bridge/* via CommandRelayListener —
    // making `supervisor-bridge/health` report `responsive: true` without
    // requiring a human-opened browser tab.
    //
    // Deliberately spawned AFTER the listener bind above so the initial page
    // load hits a live HTTP server instead of a connection-refused error
    // that some WebView2 versions latch onto permanently.
    #[cfg(windows)]
    {
        if state.config.no_webview {
            info!("Ambient dashboard webview disabled (--no-webview / QONTINUI_SUPERVISOR_NO_WEBVIEW)");
        } else {
            webview::spawn_webview_thread(format!("http://127.0.0.1:{}/", port));
        }
    }

    // Pre-warm build slots in the background so the first real build per slot
    // benefits from warm incremental artifacts. Best-effort and time-boxed.
    {
        let state_for_prewarm = state.clone();
        tokio::spawn(async move {
            build_monitor::prewarm_build_slots(state_for_prewarm).await;
        });
    }

    // One-shot startup sweep of stale `.spawn-*` scratch worktree containers
    // (deferred #64 Phase 4). Non-blocking — spawned as its own task so a slow
    // `git worktree remove` of a GB-scale checkout never delays serving. No
    // build is active at startup, so the active-container exclusion set is
    // empty. Best-effort: the pruner logs + swallows every failure internally.
    {
        let state_for_prune = state.clone();
        tokio::spawn(async move {
            let active = std::collections::HashSet::new();
            let report = spawn_worktree::prune_spawn_worktrees(
                &state_for_prune.config.project_dir,
                &active,
                std::time::SystemTime::now(),
            )
            .await;
            if !report.removed.is_empty() || !report.failed.is_empty() {
                state_for_prune
                    .logs
                    .emit(
                        LogSource::Supervisor,
                        LogLevel::Info,
                        format!(
                            "startup spawn-worktree prune: removed {} stale scratch worktree(s), kept {}, {} failed",
                            report.removed.len(),
                            report.kept.len(),
                            report.failed.len()
                        ),
                    )
                    .await;
            }
        });
    }

    // Background footprint refresh (plan
    // 2026-06-05-supervisor-build-artifact-footprint, Phase 1). Walks the
    // GB-scale `target-pool/slot-*` + `.spawn-*` trees off the hot path and
    // publishes a cached snapshot on `state.footprint`, consumed by
    // `GET /builds` and the pre-permit disk guard. An immediate first refresh
    // populates the cache at boot; thereafter on a timer (default 15 min,
    // override `QONTINUI_SUPERVISOR_FOOTPRINT_REFRESH_SECS`).
    {
        let state_for_footprint = state.clone();
        tokio::spawn(async move {
            let interval_secs = std::env::var("QONTINUI_SUPERVISOR_FOOTPRINT_REFRESH_SECS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|n| *n >= 1)
                .unwrap_or(900);
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                ticker.tick().await;
                let _ = state_for_footprint.refresh_footprint().await;
            }
        });
    }

    // Serve with graceful shutdown (no global timeout — eval benchmarks can run for hours)
    let serve_future =
        axum::serve(listener, router).with_graceful_shutdown(shutdown_signal(state.clone()));

    serve_future.await?;

    info!("Supervisor shutting down");

    // Hard-exit safety net: arm a watchdog that force-exits the process if
    // post-shutdown cleanup hangs for more than `HARD_EXIT_DEADLINE_SECS`
    // seconds. We *should* exit cleanly via the natural return from `main`,
    // but if some background task (a stuck `child.wait()`, an OS-level
    // blocking IO inside spawn_blocking, etc.) keeps the runtime alive past
    // its useful lifetime, this guarantees the process actually goes away.
    //
    // The supervisor's children are protected by the kill-on-job-close
    // JobObject (`state.runner_job`); they die when *this* process dies,
    // regardless of whether we stopped them gracefully first. So a hard
    // exit here cannot leak orphan runners — it just skips the polite
    // "ask everything to stop" step that's already redundant under the
    // JobObject contract.
    const HARD_EXIT_DEADLINE_SECS: u64 = 3;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(HARD_EXIT_DEADLINE_SECS)).await;
        eprintln!(
            "qontinui-supervisor: post-shutdown cleanup exceeded {HARD_EXIT_DEADLINE_SECS}s, \
             forcing exit"
        );
        std::process::exit(0);
    });

    // Stop Expo on shutdown. Bounded internally to a 5s timeout, but we
    // also wrap it in our own 2s ceiling to keep the total post-shutdown
    // window inside the hard-exit deadline above. Expo is *not* covered by
    // the runner JobObject (it's a Node process spawned outside the job),
    // so we still try to stop it politely — but if it doesn't react in
    // time, the hard-exit watchdog will reap it via process death.
    let expo_running = state.expo.read().await.running;
    if expo_running {
        info!("Stopping Expo before exit...");
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(2), expo::stop_expo(&state)).await;
    }

    // NOTE: We deliberately do NOT call `stop_all_temp_runners` here. The
    // Win32 `RunnerJob` (held in `state.runner_job`) has `KILL_ON_JOB_CLOSE`
    // set, so every supervisor-spawned runner is terminated by the kernel
    // the instant the last handle to the Job closes — which happens when
    // `state` drops at the end of `main`. The previous `stop_all_temp_runners`
    // call here was the dominant source of `POST /supervisor/shutdown`
    // latency: it iterated every temp runner with a 5s graceful-stop poll
    // plus a 5s port-free wait, easily 30+ seconds wall-clock with several
    // runners attached. The JobObject makes it redundant.

    Ok(())
}

/// Maximum seconds a build slot may be "busy" before the reaper clears it.
/// Normal builds take 3-8 minutes; 15 minutes is a generous ceiling.
/// Margin added to the configured build timeout when the reaper decides a
/// slot is stuck. The reaper is a backstop for leaked slots; cargo's own
/// timeout (`build_timeout_secs()`) should always fire first on a real
/// long-running build. The margin gives cargo room to finish even if it's
/// slightly over its own timeout (e.g. a slow link step).
const REAPER_MARGIN_SECS: i64 = 600;

/// Compute the threshold at which the reaper considers a slot stuck.
/// Always strictly greater than `build_timeout_secs()` so cargo gets to
/// fail on its own first.
fn max_build_age_secs() -> i64 {
    config::build_timeout_secs() as i64 + REAPER_MARGIN_SECS
}

/// Decide which runner id (if any) the boot-start path should start.
///
/// Pure so the boot wiring in `main` (which is otherwise hard to unit-test)
/// has a testable seam. The caller passes the resolved primary as
/// `Some((id, running))` or `None` when no primary is registered.
///
/// Returns `Some(id)` only when auto-start was requested AND a primary exists
/// AND it is not already running (the orphan scan may have adopted it back).
fn primary_to_boot_start(auto_start: bool, primary: Option<(&str, bool)>) -> Option<String> {
    if !auto_start {
        return None;
    }
    match primary {
        Some((id, running)) if !running => Some(id.to_string()),
        _ => None,
    }
}

/// Periodically scan build slots and clear any that have been "busy" for
/// longer than [`max_build_age_secs`]. Runs every 2 minutes. This catches
/// leaked slots from crashed cargo builds or supervisor panics where the
/// `SlotGuard` RAII type didn't execute its `Drop`.
///
/// Pre-2026-05-01 the threshold was a hard-coded 15 minutes, which was
/// shorter than the configured cargo build timeout (30 min default). On a
/// cold-cache Tauri build the reaper would kill cargo before cargo's own
/// timeout fired, leaving the slot's history without a record. The reaper
/// is now strictly above `build_timeout_secs()` so cargo always gets to
/// finish (or fail) on its own first.
async fn reap_stuck_build_slots(state: state::SharedState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(120));
    interval.tick().await; // skip the immediate first tick

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        let max_age = max_build_age_secs();

        for slot in &state.build_pool.slots {
            if let Ok(mut busy) = slot.busy.try_write() {
                if let Some(ref info) = *busy {
                    let elapsed = (now - info.started_at).num_seconds().max(0);
                    if elapsed > max_age {
                        warn!(
                            "Build slot {} stuck for {}s (max {}s) — auto-clearing. \
                             requester_id={:?}, rebuild_kind={}",
                            slot.id, elapsed, max_age, info.requester_id, info.rebuild_kind
                        );
                        state
                            .logs
                            .emit(
                                log_capture::LogSource::Supervisor,
                                log_capture::LogLevel::Warn,
                                format!(
                                    "Auto-cleared stuck build slot {} after {}s (limit {}s)",
                                    slot.id, elapsed, max_age
                                ),
                            )
                            .await;
                        *busy = None;
                    }
                }
            }
        }
    }
}

/// Interval at which the stale-test-runner sweeper runs. 5 minutes is a
/// balance between responsiveness (port slots aren't tied up long) and cost
/// (the sweep walks the registry + probes ports).
const STALE_TEST_RUNNER_SWEEP_SECS: u64 = 5 * 60;

/// Periodically run `purge_stale_test_runners_core` to drain placeholders
/// left behind by failed or interrupted `spawn-test` calls. Best-effort —
/// errors are swallowed; the next tick retries.
async fn reap_stale_test_runners(state: state::SharedState) {
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(STALE_TEST_RUNNER_SWEEP_SECS));
    interval.tick().await; // skip immediate first tick

    loop {
        interval.tick().await;
        // `respect_active_builds=true`: cold cargo builds can run longer than
        // this 5-minute sweep cadence; if a build is in flight, leave its
        // placeholder alone so the spawn-test handler can promote it.
        let purged = crate::routes::runners::purge_stale_test_runners_core(&state, true).await;
        if !purged.is_empty() {
            info!(
                "reap_stale_test_runners: swept {} stale placeholder(s): {:?}",
                purged.len(),
                purged
                    .iter()
                    .map(|(id, _, port)| format!("{} ({})", id, port))
                    .collect::<Vec<_>>()
            );
            state
                .logs
                .emit(
                    log_capture::LogSource::Supervisor,
                    log_capture::LogLevel::Info,
                    format!(
                        "Periodic sweep purged {} stale test-runner placeholder(s)",
                        purged.len()
                    ),
                )
                .await;
        }
    }
}

async fn shutdown_signal(state: Arc<SupervisorState>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };
    // Allow an HTTP-initiated shutdown by racing the ctrl_c future against a
    // broadcast receiver on `shutdown_tx`. `POST /supervisor/shutdown` sends to
    // this channel so scripted callers can trigger a graceful drain instead of
    // resorting to `Stop-Process -Force` (which kills mid-request and leaves
    // in-flight `spawn-test` callers with an empty response body).
    let mut shutdown_rx = state.shutdown_tx.subscribe();
    let http_trigger = async {
        let _ = shutdown_rx.recv().await;
    };

    let reason = tokio::select! {
        _ = ctrl_c => "ctrl_c",
        _ = http_trigger => "http_endpoint",
    };
    info!("Received shutdown signal ({})", reason);
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!("Shutdown signal received ({})", reason),
        )
        .await;

    // Notify all WS/SSE clients to close cleanly (prevents zombie sockets).
    // `signal_shutdown` flips the latched bool *and* broadcasts: handlers
    // that subscribe after this point still observe shutdown (broadcast
    // does not replay), and existing subscribers get the wake-up event.
    // Idempotent — safe even when the HTTP endpoint already called this
    // (the latched bool is monotonic and the broadcast is best-effort).
    state.signal_shutdown();

    // Give clients a moment to receive the shutdown message and close
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[cfg(test)]
mod tests {
    use super::primary_to_boot_start;

    #[test]
    fn no_auto_start_never_boot_starts() {
        // Even with a stopped primary, auto_start=false => None.
        assert_eq!(primary_to_boot_start(false, Some(("primary", false))), None);
        assert_eq!(primary_to_boot_start(false, None), None);
    }

    #[test]
    fn auto_start_with_running_primary_is_noop() {
        // Orphan adoption already brought it back => leave it alone.
        assert_eq!(primary_to_boot_start(true, Some(("primary", true))), None);
    }

    #[test]
    fn auto_start_with_stopped_primary_returns_id() {
        assert_eq!(
            primary_to_boot_start(true, Some(("primary", false))),
            Some("primary".to_string())
        );
    }

    #[test]
    fn auto_start_with_no_primary_is_noop() {
        assert_eq!(primary_to_boot_start(true, None), None);
    }
}
