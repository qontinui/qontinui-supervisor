//! Spawn-monitor placement config.
//!
//! Read by `forward_window_position_env` in `process::manager` when
//! spawning a non-primary runner: the supervisor picks the next enabled
//! monitor in round-robin order and exports its rect as the
//! `QONTINUI_WINDOW_X/Y/WIDTH/HEIGHT` env vars. The runner reads these at
//! window-build time (see `qontinui-runner/src-tauri/src/main.rs`).

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::settings::{self, MonitorConfig};
use crate::state::SharedState;

#[derive(Serialize)]
pub struct DetectedMonitor {
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

#[derive(Serialize)]
pub struct DetectedMonitorsResponse {
    pub monitors: Vec<DetectedMonitor>,
}

#[derive(Serialize)]
pub struct SpawnMonitorsResponse {
    pub monitors: Vec<MonitorConfig>,
    pub next_index: usize,
}

#[derive(Deserialize)]
pub struct PutSpawnMonitorsRequest {
    pub monitors: Vec<MonitorConfig>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// GET /spawn-monitors — current monitor placement config + the next index
/// the round-robin will hand out (modulo enabled count).
pub async fn list_spawn_monitors(State(state): State<SharedState>) -> Json<SpawnMonitorsResponse> {
    use std::sync::atomic::Ordering;
    let monitors = state.spawn_monitors.read().await.clone();
    let next_index = state.next_monitor_index.load(Ordering::Relaxed);
    Json(SpawnMonitorsResponse {
        monitors,
        next_index,
    })
}

/// PUT /spawn-monitors — replace the full list and persist to disk.
/// Resets the round-robin counter to 0 so the next spawn lands on the first
/// enabled entry of the new list.
pub async fn put_spawn_monitors(
    State(state): State<SharedState>,
    Json(body): Json<PutSpawnMonitorsRequest>,
) -> Result<Json<SpawnMonitorsResponse>, (StatusCode, Json<ErrorResponse>)> {
    use std::sync::atomic::Ordering;

    for (i, m) in body.monitors.iter().enumerate() {
        if m.label.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("monitor[{i}]: label must not be empty"),
                }),
            ));
        }
        if m.width == 0 || m.height == 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("monitor[{i}] '{}': width/height must be > 0", m.label),
                }),
            ));
        }
    }

    let path = settings::settings_path(&state.config);
    if let Err(e) = settings::save_spawn_monitors(&path, &body.monitors) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to save settings: {e}"),
            }),
        ));
    }

    *state.spawn_monitors.write().await = body.monitors.clone();
    state.next_monitor_index.store(0, Ordering::Relaxed);

    Ok(Json(SpawnMonitorsResponse {
        monitors: body.monitors,
        next_index: 0,
    }))
}

/// GET /spawn-monitors/detected — query the OS for the current monitor
/// layout. The supervisor process is Per-Monitor-V2 DPI-aware (inherited
/// from a transitive crate manifest), so we temporarily downgrade the
/// calling thread to DPI-unaware around `EnumDisplayMonitors` to get
/// rects in the same system-DPI logical coordinate space Tauri's
/// `LogicalPosition`/`LogicalSize` consume.
pub async fn get_detected_monitors() -> Json<DetectedMonitorsResponse> {
    Json(DetectedMonitorsResponse {
        monitors: detect_monitors(),
    })
}

#[cfg(target_os = "windows")]
fn detect_monitors() -> Vec<DetectedMonitor> {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows_sys::Win32::UI::HiDpi::{
        SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_UNAWARE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    unsafe extern "system" fn enum_proc(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO) == 0 {
            return TRUE;
        }
        let r = info.monitorInfo.rcMonitor;
        let raw = RawMonRepr {
            x: r.left,
            y: r.top,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            is_primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
        };
        let list = &mut *(lparam as *mut Vec<RawMonRepr>);
        list.push(raw);
        TRUE
    }

    let mut raws: Vec<RawMonRepr> = Vec::new();
    unsafe {
        // Per-thread DPI awareness flip: the supervisor process is
        // Per-Monitor-V2 DPI-aware (inherited from a transitive crate
        // manifest), which makes EnumDisplayMonitors return physical pixel
        // rects. Tauri's LogicalPosition/LogicalSize on the runner side
        // expects system-DPI logical rects, so downgrade just this thread
        // to DPI-unaware around the call and restore the previous context
        // afterwards.
        let prev = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_proc),
            &mut raws as *mut _ as *mut c_void as LPARAM,
        );
        if !prev.is_null() {
            SetThreadDpiAwarenessContext(prev);
        }
    }

    label_monitors(raws)
}

#[cfg(not(target_os = "windows"))]
fn detect_monitors() -> Vec<DetectedMonitor> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn label_monitors(raws: Vec<RawMonRepr>) -> Vec<DetectedMonitor> {
    let primary_idx = raws.iter().position(|m| m.is_primary);
    let primary = primary_idx.map(|i| {
        let m = &raws[i];
        (m.x, m.y, m.width as i32, m.height as i32)
    });

    let mut sides: Vec<&'static str> = Vec::with_capacity(raws.len());
    let mut secondary_n = 0usize;
    for (i, m) in raws.iter().enumerate() {
        if Some(i) == primary_idx {
            sides.push("Primary");
            continue;
        }
        let side = match primary {
            Some((px, py, pw, ph)) => {
                if (m.x + m.width as i32) <= px {
                    "Left"
                } else if m.x >= px + pw {
                    "Right"
                } else if (m.y + m.height as i32) <= py {
                    "Above"
                } else if m.y >= py + ph {
                    "Below"
                } else {
                    "Secondary"
                }
            }
            None => "Secondary",
        };
        if side == "Secondary" {
            secondary_n += 1;
        }
        sides.push(side);
    }

    // Count occurrences of each side label so we know whether to suffix.
    let mut side_counts: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    for s in &sides {
        *side_counts.entry(*s).or_insert(0) += 1;
    }

    let mut side_seen: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    let mut secondary_seen = 0usize;
    let mut out = Vec::with_capacity(raws.len());
    for (i, m) in raws.iter().enumerate() {
        let side = sides[i];
        let label = if side == "Primary" {
            "Primary".to_string()
        } else if side == "Secondary" {
            secondary_seen += 1;
            if secondary_n > 1 {
                format!("Secondary {secondary_seen}")
            } else {
                "Secondary".to_string()
            }
        } else {
            let n = side_counts.get(side).copied().unwrap_or(1);
            let seen = side_seen.entry(side).or_insert(0);
            *seen += 1;
            if n > 1 {
                format!("{side} {seen}")
            } else {
                side.to_string()
            }
        };
        out.push(DetectedMonitor {
            label,
            x: m.x,
            y: m.y,
            width: m.width,
            height: m.height,
            is_primary: m.is_primary,
        });
    }
    out
}

#[cfg(target_os = "windows")]
struct RawMonRepr {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    is_primary: bool,
}
