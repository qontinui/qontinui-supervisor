mod ai_debug;
mod build_monitor;
mod code_activity;
mod config;
mod error;
mod expo;
mod health_cache;
mod log_capture;
mod process;
mod routes;
mod server;
mod settings;
mod state;
mod watchdog;
mod workflow_loop;

use clap::Parser;
use std::sync::Arc;
use tracing::{error, info, warn};

use config::{CliArgs, SupervisorConfig};
use log_capture::{LogLevel, LogSource};
use state::SupervisorState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qontinui_supervisor=info,tower_http=info".into()),
        )
        .init();

    let args = CliArgs::parse();
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

    // Build and start HTTP server (retry bind if port is lingering)
    let router = server::build_router(state.clone());
    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = {
        let mut attempts = 0;
        loop {
            match tokio::net::TcpListener::bind(&bind_addr).await {
                Ok(l) => break l,
                Err(e) if attempts < 30 => {
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

    // Serve with graceful shutdown + forced 5s timeout to prevent zombie sockets
    let serve_future =
        axum::serve(listener, router).with_graceful_shutdown(shutdown_signal(state.clone()));

    match tokio::time::timeout(std::time::Duration::from_secs(300), serve_future).await {
        Ok(result) => result?,
        Err(_) => {
            warn!("Server shutdown timed out, forcing exit");
        }
    }

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
