mod build_monitor;
mod config;
mod error;
mod log_capture;
mod process;
mod routes;
mod server;
mod state;
mod watchdog;

use clap::Parser;
use std::sync::Arc;
use tracing::{error, info};

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
    info!("Starting qontinui-supervisor v{}", env!("CARGO_PKG_VERSION"));
    info!("Project dir: {:?}", args.project_dir);
    info!("Dev mode: {}", args.dev_mode);
    info!("Watchdog: {}", args.watchdog);
    info!("Auto-start: {}", args.auto_start || args.watchdog);

    // Validate project dir
    if !args.project_dir.exists() {
        error!("Project directory does not exist: {:?}", args.project_dir);
        std::process::exit(1);
    }

    let config = SupervisorConfig::from_args(args);
    let port = config.port;
    let auto_start = config.auto_start;

    let state = Arc::new(SupervisorState::new(config));

    // Log startup
    state.logs.emit(
        LogSource::Supervisor,
        LogLevel::Info,
        format!("Supervisor starting on port {}", port),
    ).await;

    // Spawn watchdog background task
    let _watchdog_handle = watchdog::spawn_watchdog(state.clone());

    // Auto-start runner if configured
    if auto_start {
        let state_clone = state.clone();
        tokio::spawn(async move {
            info!("Auto-starting runner...");
            state_clone.logs.emit(
                LogSource::Supervisor,
                LogLevel::Info,
                "Auto-starting runner",
            ).await;

            match process::manager::start_runner(&state_clone).await {
                Ok(()) => {
                    info!("Runner auto-started successfully");
                    // Start build error monitor
                    build_monitor::spawn_build_error_monitor(state_clone);
                }
                Err(e) => {
                    error!("Failed to auto-start runner: {}", e);
                    state_clone.logs.emit(
                        LogSource::Supervisor,
                        LogLevel::Error,
                        format!("Failed to auto-start runner: {}", e),
                    ).await;
                }
            }
        });
    }

    // Build and start HTTP server
    let router = server::build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!("Supervisor listening on http://0.0.0.0:{}", port);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await?;

    info!("Supervisor shutting down");

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
    state.logs.emit(
        LogSource::Supervisor,
        LogLevel::Info,
        "Shutdown signal received",
    ).await;
}
