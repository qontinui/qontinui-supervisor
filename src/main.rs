mod ai_debug;
mod build_monitor;
mod code_activity;
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
mod watchdog;
mod workflow_loop;

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

    let config = SupervisorConfig::from_args(args);
    let port = config.port;
    let auto_start = config.auto_start;

    let state = Arc::new(SupervisorState::new(config));

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

    // Spawn watchdog background task
    let _watchdog_handle = watchdog::spawn_watchdog(state.clone());

    // Spawn code activity monitor
    let _code_activity_handle = code_activity::spawn_code_activity_monitor(state.clone());

    // Auto-start runner if configured
    if auto_start {
        let state_clone = state.clone();
        tokio::spawn(async move {
            info!("Auto-starting runner...");
            state_clone
                .logs
                .emit(
                    LogSource::Supervisor,
                    LogLevel::Info,
                    "Auto-starting runner",
                )
                .await;

            match process::manager::start_runner(&state_clone).await {
                Ok(()) => {
                    info!("Runner auto-started successfully");
                    // Start build error monitor
                    build_monitor::spawn_build_error_monitor(state_clone);
                }
                Err(e) => {
                    error!("Failed to auto-start runner: {}", e);
                    state_clone
                        .logs
                        .emit(
                            LogSource::Supervisor,
                            LogLevel::Error,
                            format!("Failed to auto-start runner: {}", e),
                        )
                        .await;
                }
            }
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

    // Stop runner on shutdown
    let runner = state.runner.read().await;
    if runner.running {
        drop(runner);
        info!("Stopping runner before exit...");
        let _ = process::manager::stop_runner(&state).await;
    }

    Ok(())
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
