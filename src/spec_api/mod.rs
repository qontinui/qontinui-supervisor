//! Spec API — Section 3 (Phase B5c) of the UI Bridge redesign.
//!
//! Per-app Spec API for the supervisor. Mirrors the runner's `/spec/...`
//! surface (which lives in `qontinui-runner/src-tauri/src/spec_api/`) so the
//! supervisor's own dashboard can be authored and consumed through the same
//! IR-based pipeline as runner pages.
//!
//! Storage root defaults to `<supervisor>/frontend/specs/` (NOT
//! `<supervisor>/specs/` — supervisor specs live under the frontend tree).
//! Override with `QONTINUI_SPECS_ROOT`.
//!
//! Submodules:
//! - [`types`]      Rust mirrors of `IrPageSpec` + legacy spec types
//! - [`projection`] Pure `IrPageSpec -> LegacySpec` projection (Rust port of
//!   the TS `projectIRToBundledPage`)
//! - [`storage`]    Filesystem layer for the storage layout under the specs root
//! - [`responses`]  Empty/error envelope shapes — every empty response carries `reason`
//! - [`events`]     Broadcast channel for `spec.changed` SSE events
//! - [`handlers`]   Axum handlers; one per endpoint
//!
//! Entry point: [`routes`]. Merge into the main router from `server.rs`.

pub mod events;
pub mod handlers;
pub mod projection;
pub mod responses;
pub mod storage;
pub mod types;

#[cfg(test)]
mod tests;

use axum::routing::{get, post};
use axum::Router;

use crate::state::SharedState;

/// Routes for the Spec API. Mounted alongside the supervisor's existing
/// routes. Same `/spec/...` prefix as the runner so consumers can target
/// either app uniformly.
pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/spec/health", get(handlers::get_health))
        .route("/spec/get", get(handlers::get_file))
        .route("/spec/page/{id}", get(handlers::get_page))
        .route("/spec/graph", get(handlers::get_graph))
        .route("/spec/query", post(handlers::post_query))
        .route("/spec/derive", post(handlers::post_derive))
        .route("/spec/diff", get(handlers::get_diff))
        .route("/spec/author", post(handlers::post_author))
        .route("/spec/subscribe", get(handlers::get_subscribe))
}
