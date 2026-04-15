mod build_monitor;
mod config;
mod diagnostics;
mod error;
mod evaluation;
mod expo;
mod health_cache;
mod log_capture;
mod process;
mod routes;
mod server;
mod settings;
mod state;
mod velocity;
mod velocity_improvement;
mod velocity_layer;
mod velocity_tests;

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

    // Initialize tracing with velocity layer for HTTP span capture
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "qontinui_supervisor=info,tower_http=info".into());
    let fmt_layer = tracing_subscriber::fmt::layer();
    let velocity = velocity_layer::VelocityLayer::new(dev_logs_dir);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(velocity)
        .init();
    info!(
        "Starting qontinui-supervisor v{}",
        env!("CARGO_PKG_VERSION")
    );
    info!("Project dir: {:?}", args.project_dir);
    info!("Dev mode: {}", args.dev_mode);
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
        let dev_logs_dir_for_settings = config.dev_logs_dir.clone();
        let settings_path = dev_logs_dir_for_settings.join("supervisor-settings.json");
        let saved = settings::load_settings(&settings_path);
        if !saved.runners.is_empty() {
            info!(
                "Loaded {} runner configs from settings",
                saved.runners.len()
            );
            config.runners = saved.runners;
            // Ensure there's always a primary
            if !config.runners.iter().any(|r| r.is_primary) {
                warn!("No primary runner in saved settings, adding default");
                config
                    .runners
                    .insert(0, crate::config::RunnerConfig::default_primary());
            }
        }
    }

    let port = config.port;
    let _auto_start = config.auto_start;

    let supervisor_state = SupervisorState::new(config);

    let state = Arc::new(supervisor_state);

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

    // Clean up any orphaned temp runner processes from previous sessions
    // and detect already-running user runners for health tracking.
    // The supervisor does NOT auto-start any runners — users start their own.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            process::manager::cleanup_orphaned_runners(&state_clone).await;
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

    // Build and start HTTP server (with SO_REUSEADDR to handle lingering sockets)
    let router = server::build_router(state.clone());
    let bind_addr: std::net::SocketAddr = format!("0.0.0.0:{}", port).parse()?;
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
    info!("Supervisor listening on http://0.0.0.0:{}", port);

    // Pre-warm build slots in the background so the first real build per slot
    // benefits from warm incremental artifacts. Best-effort and time-boxed.
    {
        let state_for_prewarm = state.clone();
        tokio::spawn(async move {
            build_monitor::prewarm_build_slots(state_for_prewarm).await;
        });
    }

    // Serve with graceful shutdown (no global timeout — eval benchmarks can run for hours)
    let serve_future =
        axum::serve(listener, router).with_graceful_shutdown(shutdown_signal(state.clone()));

    serve_future.await?;

    info!("Supervisor shutting down");

    // Stop Expo on shutdown
    let expo_running = state.expo.read().await.running;
    if expo_running {
        info!("Stopping Expo before exit...");
        let _ = expo::stop_expo(&state).await;
    }

    // Only stop temp runners on shutdown — user runners are left running
    info!("Stopping temp runners before exit...");
    let _ = process::manager::stop_all_temp_runners(&state).await;

    Ok(())
}

/// Maximum seconds a build slot may be "busy" before the reaper clears it.
/// Normal builds take 3-8 minutes; 15 minutes is a generous ceiling.
const MAX_BUILD_AGE_SECS: i64 = 15 * 60;

/// Periodically scan build slots and clear any that have been "busy" for
/// longer than [`MAX_BUILD_AGE_SECS`]. Runs every 2 minutes. This catches
/// leaked slots from crashed cargo builds or supervisor panics where the
/// `SlotGuard` RAII type didn't execute its `Drop`.
async fn reap_stuck_build_slots(state: state::SharedState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(120));
    interval.tick().await; // skip the immediate first tick

    loop {
        interval.tick().await;
        let now = chrono::Utc::now();

        for slot in &state.build_pool.slots {
            if let Ok(mut busy) = slot.busy.try_write() {
                if let Some(ref info) = *busy {
                    let elapsed = (now - info.started_at).num_seconds().max(0);
                    if elapsed > MAX_BUILD_AGE_SECS {
                        warn!(
                            "Build slot {} stuck for {}s (max {}s) — auto-clearing. \
                             requester_id={:?}, rebuild_kind={}",
                            slot.id,
                            elapsed,
                            MAX_BUILD_AGE_SECS,
                            info.requester_id,
                            info.rebuild_kind
                        );
                        state
                            .logs
                            .emit(
                                log_capture::LogSource::Supervisor,
                                log_capture::LogLevel::Warn,
                                format!(
                                    "Auto-cleared stuck build slot {} after {}s (limit {}s)",
                                    slot.id, elapsed, MAX_BUILD_AGE_SECS
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

async fn shutdown_signal(state: Arc<SupervisorState>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    ctrl_c.await;
    info!("Received shutdown signal");
    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            "Shutdown signal received",
        )
        .await;

    // Notify all WS/SSE clients to close cleanly (prevents zombie sockets)
    let _ = state.shutdown_tx.send(());

    // Give clients a moment to receive the shutdown message and close
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}
