// Library crate for integration tests.
// main.rs has its own mod declarations; this re-exports all modules.

pub mod ai_debug;
pub mod build_monitor;
pub mod code_activity;
pub mod config;
pub mod error;
pub mod expo;
pub mod health_cache;
pub mod log_capture;
pub mod process;
pub mod routes;
pub mod server;
pub mod settings;
pub mod state;
pub mod watchdog;
pub mod workflow_loop;
