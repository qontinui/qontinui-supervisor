# Qontinui Supervisor

Rust-based build server and fleet dashboard for qontinui-runner. Provides parallel cargo builds, temp/named runner spawning, runner lifecycle management, and a React SPA dashboard.

## CRITICAL: Runner Lifecycle Scope

The supervisor manages lifecycle for **temp runners** (`test-*`) and **named runners** (`named-*`). The primary runner and any user-started runners are **user-managed** — the supervisor tracks their health but never starts, stops, or restarts them unprompted.

- **Temp runners** (`test-*`): Spawned via `POST /runners/spawn-test`, auto-cleaned on stop. Run with a visible Tauri window and an isolated WebView2 profile. The UI Bridge is fully functional on temp runners.
- **Named runners** (`named-*`): Spawned via `POST /runners/spawn-named`, persistent across supervisor restarts. Saved to settings. Not auto-cleaned. Support start/stop/restart/protect.
- **User runners** (everything else): Started by the user with visible Tauri windows. The supervisor observes health only.

## Architecture

Standalone Axum HTTP server:
- **Parallel build pool** (3 concurrent cargo build slots, each with isolated `CARGO_TARGET_DIR`)
- **Temp runner spawning** for testing code changes
- **Named runner spawning** for persistent runners from the latest build
- **Runner lifecycle** start/stop/restart/protect for temp and named runners
- **Health cache** observes service port availability for the dashboard
- **Log capture** with SSE streaming and circular buffer
- **Expo process management** start/stop/monitor Expo/React Native dev server
- **Velocity system** HTTP span tracing, P50/P95/P99 latency, endpoint analysis
- **Evaluation system** test prompts with 6-dimension scoring and ground-truth comparison
- **React dashboard** SPA web UI at `GET /` for visual monitoring and control
- **Proxies** GraphQL, UI Bridge, and runner-api forwarded to port 9876
- **Diagnostics** build/restart event tracking
- **Supervisor bridge** UI Bridge relay for the dashboard's own webview
- **AI provider/model config** for evaluation and velocity (not for debug sessions)

## Building & Running

```bash
cargo build                    # Build debug binary
cargo check                    # Type-check only
cargo fmt                      # Format code
cargo clippy -- -D warnings    # Lint

# Basic start
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri

# Start with Expo dev server management
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri --expo-dir ../qontinui-mobile
```

## CLI Flags

| Flag | Description |
|------|-------------|
| `-p, --project-dir` | Path to `qontinui-runner/src-tauri` (required) |
| `-d, --dev-mode` | Run via `npm run tauri dev` instead of compiled exe |
| `-w, --watchdog` | Enable health monitoring (observe-only, implies `--auto-start`) |
| `-a, --auto-start` | Start runner on supervisor launch |
| `--expo-dir` | Path to Expo/React Native project directory |
| `-l, --log-file` | Log file for runner output |
| `--port` | Supervisor HTTP port (default: 9875) |
| `--no-prewarm` | Disable post-startup `cargo check` slot pre-warming (also `QONTINUI_SUPERVISOR_NO_PREWARM=1`) |

## API Endpoints

### Health & Dashboard

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | React SPA dashboard |
| GET | `/health` | Comprehensive status (runners, build, expo) |
| GET | `/health/stream` | SSE stream of real-time health data |
| POST | `/supervisor/restart` | Self-restart supervisor (runners are left running) |

### Runner Management

| Method | Path | Description |
|--------|------|-------------|
| GET | `/runners` | List all runners with status |
| POST | `/runners` | Add a runner config to the registry |
| POST | `/runners/spawn-test` | Spawn ephemeral test runner on next free port (9877-9899). Body: `{rebuild?, wait?, wait_timeout_secs?, requester_id?, queue_timeout_secs?}`. Returns `{id, port, api_url, ui_bridge_url}`. Auto-cleaned on stop. |
| POST | `/runners/spawn-named` | Spawn persistent named runner. Body: `{name, rebuild?, port?, wait?, wait_timeout_secs?, protected?, queue_timeout_secs?}`. Persisted to settings, NOT auto-cleaned. Name must not be empty, "primary", or start with "test-". Returns `{id, port, api_url, ui_bridge_url}`. |
| POST | `/runners/purge-stale` | Remove runners whose processes are no longer alive |
| DELETE | `/runners/{id}` | Remove a runner from the registry |
| POST | `/runners/{id}/start` | Start a runner |
| POST | `/runners/{id}/stop` | Stop a runner |
| POST | `/runners/{id}/restart` | Restart a runner |
| POST | `/runners/{id}/protect` | Toggle protection on a runner |
| POST | `/runners/{id}/watchdog` | Control watchdog for a specific runner |
| GET | `/runners/{id}/logs` | Log history for a specific runner |
| GET | `/runners/{id}/logs/stream` | SSE log stream for a specific runner |
| GET/POST | `/runners/{id}/ui-bridge/{*path}` | Proxy UI Bridge requests to a specific runner |

**Queue behavior for spawn-test and spawn-named:**
- **Default (blocking):** If all build slots are busy, the HTTP request holds open until a slot frees. Optional `queue_timeout_secs` bounds the wait and returns 504 on timeout.
- **`X-Queue-Mode: no-wait` header:** Returns immediately with **503 Service Unavailable** and body `{error: "build_pool_full", queue_position, active_builds: [...]}`.

### Parallel Build Pool

The supervisor runs a fixed pool of **N concurrent cargo builds** (default 3, override via env `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`). Each slot has its own `CARGO_TARGET_DIR` at `qontinui-runner/target-pool/slot-{k}/` so concurrent builds do not contend on a shared `target/`. Frontend (`npm run build`) is serialized behind a dedicated mutex since Tauri embeds a single `dist/`.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/builds` | Snapshot of the parallel build pool: pool size, available permits, queue depth, per-slot state (`idle` or `building` with `started_at`/`elapsed_secs`/`requester_id`/`rebuild_kind`), and `last_successful_slot`. |
| DELETE | `/builds/caches` | Clear build caches across all pool slots |
| POST | `/build/reset` | Reset build state |

### Logs

| Method | Path | Description |
|--------|------|-------------|
| GET | `/logs/history` | Recent log entries from circular buffer |
| GET | `/logs/stream` | SSE stream of real-time log events |
| GET | `/logs/file/{type}` | Read `.dev-logs/` files |
| GET | `/logs/files` | List available log files |

### Expo

| Method | Path | Description |
|--------|------|-------------|
| POST | `/expo/start` | Start Expo dev server (requires `--expo-dir`) |
| POST | `/expo/stop` | Stop Expo dev server |
| GET | `/expo/status` | Running state, PID, port, configured flag |
| GET | `/expo/logs/stream` | SSE stream filtered to Expo log source |

### Velocity (HTTP Span Tracing)

| Method | Path | Description |
|--------|------|-------------|
| POST | `/velocity/ingest` | Ingest HTTP span data |
| GET | `/velocity/summary` | Aggregated latency summary (P50/P95/P99) |
| GET | `/velocity/endpoints` | Per-endpoint latency breakdown |
| GET | `/velocity/slow` | Slowest requests |
| GET | `/velocity/timeline` | Latency over time |
| GET | `/velocity/compare` | Before/after comparison |
| GET | `/velocity/trace/{request_id}` | Detailed trace for a single request |

### Velocity Tests

| Method | Path | Description |
|--------|------|-------------|
| POST | `/velocity-tests/start` | Start a velocity test run |
| POST | `/velocity-tests/stop` | Stop a running test |
| GET | `/velocity-tests/status` | Current test status |
| GET | `/velocity-tests/runs` | List past runs |
| GET | `/velocity-tests/runs/{id}` | Get a specific run |
| GET | `/velocity-tests/trend` | Performance trend across runs |

### Velocity Improvement

| Method | Path | Description |
|--------|------|-------------|
| POST | `/velocity-improvement/start` | Start improvement analysis |
| POST | `/velocity-improvement/stop` | Stop running analysis |
| GET | `/velocity-improvement/status` | Current analysis status |
| GET | `/velocity-improvement/history` | Past improvement results |

### Evaluation (AI Response Scoring)

| Method | Path | Description |
|--------|------|-------------|
| POST | `/eval/start` | Start an evaluation run |
| POST | `/eval/stop` | Stop a running evaluation |
| GET | `/eval/status` | Current evaluation status |
| POST | `/eval/continuous/start` | Start continuous evaluation |
| POST | `/eval/continuous/stop` | Stop continuous evaluation |
| GET | `/eval/runs` | List past evaluation runs |
| GET | `/eval/runs/{id}` | Get a specific run |
| GET | `/eval/test-suite` | List test prompts |
| POST | `/eval/test-suite` | Add a test prompt |
| PUT | `/eval/test-suite/{id}` | Update a test prompt |
| DELETE | `/eval/test-suite/{id}` | Delete a test prompt |

### AI Provider/Model Config

Used by the evaluation and velocity systems to select which AI provider and model to use. Not related to debug sessions.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/ai/provider` | Get current AI provider/model selection |
| POST | `/ai/provider` | Set AI provider/model |
| GET | `/ai/models` | List available AI models |

### Proxies

| Method | Path | Description |
|--------|------|-------------|
| GET/POST | `/ui-bridge/{*path}` | Proxy to runner at `http://127.0.0.1:9876/ui-bridge/*` |
| GET/POST | `/runner-api/{*path}` | Proxy to runner at `http://127.0.0.1:9876/*` |
| POST | `/graphql` | Proxy GraphQL queries to runner |
| GET | `/graphql/ws` | Proxy GraphQL WebSocket subscriptions to runner |

Returns `502 Bad Gateway` with descriptive error if the runner is not responding.

### Supervisor Bridge

UI Bridge relay so the dashboard's own webview can be inspected/controlled by automation agents.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/supervisor-bridge/commands/stream` | SSE stream of pending commands |
| POST | `/supervisor-bridge/commands` | Submit a command response |
| POST | `/supervisor-bridge/heartbeat` | Dashboard heartbeat |
| GET | `/supervisor-bridge/health` | Bridge health status |
| GET | `/supervisor-bridge/control/snapshot` | Full snapshot of dashboard UI |
| GET | `/supervisor-bridge/control/elements` | List dashboard UI elements |
| POST | `/supervisor-bridge/control/element/{id}/action` | Execute action on dashboard element |
| POST | `/supervisor-bridge/control/discover` | Trigger element discovery |
| GET | `/supervisor-bridge/control/console-errors` | Get console errors from dashboard |
| POST | `/supervisor-bridge/control/page/evaluate` | Evaluate JS in dashboard webview |
| POST | `/supervisor-bridge/control/page/navigate` | Navigate dashboard page |
| POST | `/supervisor-bridge/control/page/refresh` | Refresh dashboard page |

### Diagnostics

| Method | Path | Description |
|--------|------|-------------|
| GET | `/diagnostics` | Build/restart event history |
| POST | `/diagnostics/clear` | Clear diagnostic events |

### Other

| Method | Path | Description |
|--------|------|-------------|
| GET/POST/DELETE | `/test-login` | Get/set/clear test login credentials for runner spawning |
| GET | `/ws` | WebSocket endpoint |
| POST/GET | `/runner/stop` | Stop runner (legacy single-runner endpoint) |
| POST | `/runner/restart` | Restart runner (legacy single-runner endpoint) |
| POST | `/runner/watchdog` | Control watchdog (legacy single-runner endpoint) |
| POST | `/runner/fix-and-rebuild` | Fix errors and rebuild (legacy single-runner endpoint) |

## Dashboard

The supervisor serves a React SPA dashboard at `GET /`. Open `http://localhost:9875/` in a browser.

**Features:**
- Real-time service health table with status dots and action buttons
- Runner fleet management: spawn, start, stop, restart, protect
- Build pool status: per-slot state, queue depth, last successful slot
- Log viewer with source/level filtering, pause/resume, auto-scroll
- Velocity and evaluation dashboards
- Confirmation dialogs for destructive actions

**Implementation:** React + TypeScript SPA in `frontend/` directory, built with Vite. Production build output in `dist/` is embedded into the binary via `rust-embed`. Falls back to legacy `static/dashboard.html` if the SPA dist is missing.

**Data flow:**
- SSE `GET /health/stream` for real-time health data
- SSE `GET /logs/stream` for real-time log entries
- Fetches runner, build, velocity, and evaluation data via REST

## CRITICAL: Manually Building the Runner Binary

**Strongly prefer letting the supervisor build** (via `POST /runners/spawn-test {rebuild: true}` or `POST /runners/spawn-named {name: "...", rebuild: true}`). The supervisor handles the frontend build, feature flags, slot selection, and deploy copy correctly.

If you **must** build manually, the command MUST match what the supervisor runs:

**Correct manual build command (exe mode):**

```bash
cd qontinui-runner
# 1. Rebuild the frontend so dist/ is fresh — Tauri embeds this at cargo build time.
npm run build
# 2. Build into a supervisor build-pool slot so the supervisor picks it up.
cd src-tauri
CARGO_TARGET_DIR=../target-pool/slot-0 \
    cargo build --bin qontinui-runner --features custom-protocol
```

**Why `--features custom-protocol` is mandatory for exe mode:** without it, the `tauri` crate compiles with `cfg(dev)` active and the binary loads the frontend from `devUrl` (`http://localhost:1420`) instead of embedding `dist/`. If vite isn't running on 1420 the webview shows `ERR_CONNECTION_REFUSED`.

**Why you must build into a slot dir:** the supervisor's `resolve_source_exe` picks the source exe from `last_successful_slot` first, then scans other slots, and only falls back to `target/debug/qontinui-runner.exe` last. On every runner start it copies the source over `target/debug/qontinui-runner-{id}.exe` — so building into the default `target/` means the supervisor may overwrite it with a stale slot exe.

**When building dev mode** (supervisor started with `-d --dev-mode`): omit `--features custom-protocol`.

**Supervisor source of truth:** the exact args are assembled in `src/build_monitor.rs::run_build_inner`. If this doc drifts from that code, that file wins.

## Test Runner Binary Paths

### Target directories by spawn mode

| Directory | Purpose | Who writes it |
|-----------|---------|---------------|
| `target-pool/slot-{0,1,2}/debug/` | Supervisor parallel build pool (default 3 slots). Each spawn with `rebuild: true` claims a slot and sets `CARGO_TARGET_DIR` to the slot dir. | Supervisor via `run_cargo_build` |
| `target/debug/` | Legacy / manual `cargo build` output. Fallback only if no slot exe exists. | Manual `cargo build` |

### Exe resolution order (when `rebuild: false`)

`resolve_source_exe()` in `src/process/manager.rs` picks the binary in this order:

1. `target-pool/slot-{last_successful_slot}/debug/qontinui-runner.exe`
2. Any `target-pool/slot-{k}/debug/qontinui-runner.exe` that exists on disk
3. `target/debug/qontinui-runner.exe` — legacy fallback

Every runner start copies the resolved source exe to `target/debug/qontinui-runner-{id}.exe` so the build artifact is never locked by a running process.

### spawn-test / spawn-named `{rebuild: true}` behavior

**Synchronous.** The HTTP request blocks for the entire build+spawn cycle:

1. **Port reservation** — atomically claims a free port (9877-9899) and inserts a placeholder.
2. **Build** — acquires a build pool permit (blocks if all slots busy), runs `npm run build` (serialized via `npm_lock`), then `cargo build --bin qontinui-runner --features custom-protocol` with `CARGO_TARGET_DIR` set to the slot dir.
3. **Spawn** — copies the built exe to `target/debug/qontinui-runner-{id}.exe` and launches the process.
4. **Optional wait** — if `wait: true`, polls `GET /health` on the spawned runner every 2s until healthy or `wait_timeout_secs` (default 120s) elapses.

**Timeouts:**
- **Build timeout:** 10 minutes (`BUILD_TIMEOUT_SECS = 600`). If cargo exceeds this, the build process is killed.
- **Queue timeout:** configurable via `queue_timeout_secs`. Returns 504 after the specified seconds if all slots are busy.
- **Wait timeout:** configurable via `wait_timeout_secs` (default 120s). Only applies when `wait: true`. Returns successfully even if the runner doesn't become healthy — `status` field will say `"timeout"`.
- **No-wait mode:** pass `X-Queue-Mode: no-wait` header for immediate 503 with queue info instead of blocking.

If the build fails, the placeholder port reservation is cleaned up and the error is returned.

## Key Constants

| Constant | Value |
|----------|-------|
| Supervisor port | 9875 |
| Runner API port | 9876 |
| Expo port | 8081 |
| Runner Vite port | 1420 |
| Build timeout | 10min (600s) |
| Port wait timeout | 120s |
| Graceful kill timeout | 5s |
| Log buffer | 500 entries |
| Build pool size | 3 (override: `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`) |
| Temp runner port range | 9877-9899 |

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
