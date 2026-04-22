//! Ambient dashboard WebView2 window.
//!
//! Spawns a minimal WebView2-backed `tao` window on a dedicated thread that
//! points at the supervisor's own React SPA (`http://127.0.0.1:{port}/`). The
//! SPA's [`CommandRelayListener`] then auto-registers with the
//! `supervisor-bridge/*` endpoints and keeps its 30s heartbeat alive, making
//! `supervisor-bridge/health` report `responsive: true` without a human-opened
//! browser tab. This unblocks UI Bridge–based verification of the dashboard
//! from automation agents.
//!
//! Item B of the post-3J UI Bridge improvements plan
//! (`C:\claude\.claude-hotmail\plans\elegant-fluttering-cerf.md`).
//!
//! # Thread model
//!
//! WebView2 on Windows requires a native `HWND` and a Win32 message pump
//! running on the thread that owns the window. Since the supervisor's tokio
//! runtime already occupies the main thread, we spawn a dedicated
//! [`std::thread`] that creates the event loop with
//! [`EventLoopBuilderExtWindows::with_any_thread(true)`](tao::platform::windows::EventLoopBuilderExtWindows::with_any_thread),
//! builds the window + webview, and blocks on `event_loop.run(...)` for the
//! lifetime of the process. When the supervisor exits, the OS tears the
//! thread down — no graceful shutdown plumbing needed for this single-purpose
//! ambient surface.
//!
//! # Visibility
//!
//! `tao`'s `with_visible(false)` is honored on Windows, but experience in
//! Tauri-land is that some WebView2 builds still briefly flash the window at
//! creation and some Windows input hooks object to invisible windows. To keep
//! behavior predictable we instead build a **tiny, minimized, skip-taskbar,
//! always-on-bottom** window (400×300, (-10000, -10000) position as a belt-and-
//! suspenders measure). The end result is indistinguishable from hidden for
//! the user: not in the taskbar, not in Alt-Tab, not painted anywhere on
//! screen. Users who want to peek at the dashboard can restore the window
//! manually via the supervisor's logs if they know its process — the normal
//! interaction path remains the HTTP dashboard at `http://localhost:9875/`.
//!
//! # Safety / lifetime
//!
//! The spawned thread holds the `EventLoop`, `Window`, and `WebView` for its
//! entire lifetime. None of those cross thread boundaries, so no `Send`/`Sync`
//! gymnastics are required. The thread is never `join`ed — it terminates
//! with the process.

#![cfg(windows)]

use std::thread;

use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::{EventLoopBuilderExtWindows, WindowBuilderExtWindows};
use tao::window::WindowBuilder;
use tracing::{error, info, warn};
use wry::WebViewBuilder;

/// Spawn the ambient dashboard WebView2 window on a dedicated OS thread.
///
/// Returns immediately after the thread is launched. The thread runs the
/// `tao` event loop for the lifetime of the process. On failure to create
/// the event loop / window / webview, an error is logged and the thread
/// exits — the supervisor itself keeps running (the webview is an
/// optional convenience, never a hard requirement).
///
/// `url` should already include scheme + host + port, e.g.
/// `"http://127.0.0.1:9875/"`. It is loaded verbatim via
/// [`WebViewBuilder::with_url`].
pub fn spawn_webview_thread(url: String) {
    thread::Builder::new()
        .name("supervisor-ambient-webview".to_string())
        .spawn(move || {
            if let Err(e) = run_event_loop(url) {
                error!("Ambient dashboard webview exited with error: {e}");
            }
        })
        .map(|_| info!("Ambient dashboard webview thread spawned"))
        .unwrap_or_else(|e| warn!("Failed to spawn ambient webview thread: {e}"));
}

/// Build the event loop, window, and webview, then block on the event loop
/// until the process exits. Only returns on error or `ControlFlow::Exit`.
fn run_event_loop(url: String) -> Result<(), Box<dyn std::error::Error>> {
    // `with_any_thread(true)` lets the event loop be created outside the
    // main thread. Without it tao panics at `EventLoopBuilder::build()`
    // because Win32 expects the message pump on the thread that registered
    // the window class.
    let event_loop = EventLoopBuilder::new().with_any_thread(true).build();

    let window = WindowBuilder::new()
        .with_title("Qontinui Supervisor Dashboard (ambient)")
        .with_inner_size(LogicalSize::new(400.0, 300.0))
        // Park the window far off-screen so even if some WebView2 build
        // paints a single frame before minimize takes effect, the user never
        // sees it.
        .with_position(LogicalPosition::new(-10_000.0, -10_000.0))
        .with_always_on_bottom(true)
        .with_skip_taskbar(true)
        .with_decorations(false)
        .with_resizable(false)
        .build(&event_loop)?;

    // `tao::WindowBuilder` has `with_maximized` but not `with_minimized`; the
    // equivalent start state is set post-construction. This runs before the
    // webview navigates, so there's no perceptible pre-minimize paint.
    window.set_minimized(true);

    info!("Ambient dashboard webview loading {url}");

    let webview = WebViewBuilder::new().with_url(&url).build(&window)?;
    // Keep the webview alive for the full event loop runtime. `_` would
    // drop it at the semicolon.
    let _ = &webview;

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            // The ambient window is hidden+minimized; a close here means
            // something external (e.g. Task Manager) killed it. Exit the
            // event loop — the supervisor keeps running, but without
            // supervisor-bridge heartbeats until next restart.
            info!("Ambient dashboard webview closed");
            *control_flow = ControlFlow::Exit;
        }
    });
}
