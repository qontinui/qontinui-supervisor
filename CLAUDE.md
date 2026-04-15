# Qontinui Supervisor

Rust-based build server and health monitor for qontinui-runner. Provides cargo builds, temp runner spawning for testing, and observe-only health monitoring.

## CRITICAL: The Supervisor NEVER Manages User Runner Lifecycle

The supervisor **only** manages the lifecycle of temp/test runners (`test-*` IDs). All other runners (primary, secondary, discovered) are **user-managed** — the supervisor never starts, stops, restarts, or kills them. Users start runners manually and close them by closing the window.

- **Temp runners** (`test-*`): Spawned via `POST /runners/spawn-test`, auto-cleaned on stop. These run with a **visible Tauri window** and an isolated WebView2 profile (via `WEBVIEW2_USER_DATA_FOLDER` → `data_directory()` in the runner's `.setup()`). The UI Bridge is fully functional on temp runners — use it for end-to-end testing.
- **User runners** (everything else): Started by the user with visible Tauri windows. The supervisor tracks their health but takes no lifecycle action.

## Architecture

Standalone Axum HTTP server:
- **Temp runner spawning** for testing code changes
- **Cargo build** without restarting any runner
- **Health monitoring** (observe-only) for all runners
- **Log capture** with SSE streaming and circular buffer
- **Expo process management** start/stop/monitor Expo/React Native dev server
- **Workflow loop** orchestrates repeated workflow execution with exit strategies
- **UI Bridge proxy** transparent proxy to runner's UI Bridge SDK endpoints (control + SDK modes)
- **React dashboard** SPA web UI at `GET /` for visual monitoring and control

## Building & Running

```bash
cargo build                    # Build debug binary
cargo check                    # Type-check only
cargo fmt                      # Format code
cargo clippy -- -D warnings    # Lint

# Start with watchdog (recommended)
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -w

# Start with Expo dev server management
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -w --expo-dir ../qontinui-mobile
```

## CLI Flags

| Flag | Description |
|------|-------------|
| `-p, --project-dir` | Path to `qontinui-runner/src-tauri` (required) |
| `-w, --watchdog` | Enable health monitoring (observe-only, no restarts) |
| `--expo-dir` | Path to Expo/React Native project directory |
| `-l, --log-file` | Log file for runner output |
| `--port` | Supervisor HTTP port (default: 9875) |

## API Endpoints

### Health & Dashboard

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | React SPA dashboard |
| GET | `/health` | Comprehensive status (runners, build, code activity, expo) |
| POST | `/supervisor/restart` | Self-restart supervisor (runners are left running) |

### Temp Runner Management (test-* only)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/runners` | List all runners with status (observe-only for user runners) |
| POST | `/runners/spawn-test` | Spawn ephemeral test runner on next free port (9877-9899). Body: `{rebuild?, wait?, wait_timeout_secs?, requester_id?, queue_timeout_secs?}`. When `rebuild: true` and all build pool slots are busy, blocks by default until a slot frees (bounded by `queue_timeout_secs` if set). Pass header `X-Queue-Mode: no-wait` to return 503 with queue info instead of blocking. Returns `{id, port, api_url, ui_bridge_url}`. Auto-cleaned up on stop. |
| POST | `/runners/{id}/start` | Start a temp runner (rejects non-temp) |
| POST | `/runners/{id}/stop` | Stop a temp runner (rejects non-temp) |
| POST | `/runners/{id}/restart` | Restart a temp runner (rejects non-temp) |
| GET/POST | `/runners/{id}/ui-bridge/{*path}` | Proxy UI Bridge requests to a specific runner |
| GET | `/builds` | Snapshot of the parallel build pool: pool size, available permits, queue depth, per-slot state (`idle` or `building` with `started_at`/`elapsed_secs`/`requester_id`/`rebuild_kind`), and `last_successful_slot`. Agents should hit this before `spawn-test` to decide whether to wait or bail. |

### Parallel Build Pool

The supervisor runs a fixed pool of **N concurrent cargo builds** (default 3, override via env `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`). Each slot has its own `CARGO_TARGET_DIR` at `qontinui-runner/target-pool/slot-{k}/` so concurrent builds do not contend on a shared `target/`. Frontend (`npm run build`) is still serialized behind a dedicated mutex since Tauri embeds a single `dist/` — but the lock is held only during the ~12s npm invocation, not the full ~3min cargo build.

**spawn-test queue behavior:**
- **Default (blocking):** If all slots are busy, the HTTP request holds open until a slot frees. Optional `queue_timeout_secs` bounds the wait and returns 504 on timeout.
- **`X-Queue-Mode: no-wait` header:** Returns immediately with **503 Service Unavailable** and body `{error: "build_pool_full", queue_position, active_builds: [{slot, started_at, elapsed_secs, requester_id, rebuild_kind}, ...]}`.

**Legacy 409 `BuildInProgress`** is still returned by a few unrelated code paths (reset endpoint, smart rebuild coordination) but `spawn-test` itself no longer produces it.

**`rebuild: false`** resolves the exe in preference order: (1) last successful slot's `target-pool/slot-{k}/debug/qontinui-runner.exe`, (2) any slot whose exe exists on disk, (3) legacy `target/debug/qontinui-runner.exe` for pre-pool builds.

### CRITICAL: Manually Building the Runner Binary

**Strongly prefer letting the supervisor build** (via `POST /runners/spawn-test {rebuild: true}` for temp runners, or by triggering an auto-rebuild). The supervisor handles the frontend build, feature flags, slot selection, and deploy copy correctly. Manual `cargo build` invocations are easy to get wrong and have historically caused broken webviews and stale-exe loops.

If you **must** build manually, the command MUST match what the supervisor runs — otherwise the produced exe will either (a) crash on startup, (b) load from a dead vite dev URL showing `ERR_CONNECTION_REFUSED`, or (c) never be picked up by the supervisor on restart.

**Correct manual build command (exe mode, what users actually run):**

```bash
cd qontinui-runner
# 1. Rebuild the frontend so dist/ is fresh — Tauri embeds this at cargo build time.
npm run build
# 2. Build the runner exe INTO a supervisor build-pool slot (so the supervisor
#    will pick it up on the next restart). Pick the slot reported as
#    last_successful_slot by GET /builds, or slot 0 if unsure.
cd src-tauri
CARGO_TARGET_DIR=../target-pool/slot-0 \
    cargo build --bin qontinui-runner --features custom-protocol
```

**Why `--features custom-protocol` is mandatory for exe mode:** without it, the `tauri` crate compiles with `cfg(dev)` active and the binary loads the frontend from `devUrl` (`http://localhost:1420`) instead of embedding `dist/`. If vite isn't running on 1420 (the normal case for users) the webview shows `ERR_CONNECTION_REFUSED`. The feature is defined in `qontinui-runner/src-tauri/Cargo.toml` as `custom-protocol = ["tauri/custom-protocol"]`.

**Why you must build into a slot dir, not the default `target/`:** the supervisor's `resolve_source_exe` (`qontinui-supervisor/src/process/manager.rs`) picks the source exe from `last_successful_slot` first, then scans other slots, and only falls back to `target/debug/qontinui-runner.exe` last. On every runner start it *copies* that source over `target/debug/qontinui-runner-{id}.exe` — so if you manually overwrite the per-runner copy path, the supervisor will silently overwrite it again with the stale slot exe on the next restart. Put the fresh build in the slot dir and the supervisor will deploy it for you.

**Quick deploy of an exe you already built elsewhere:** copy it to *all* of `target-pool/slot-{0,1,2}/debug/qontinui-runner.exe` so that whichever slot is selected picks up the fixed binary.

**When building dev mode** (the supervisor was started with `-d --dev-mode`, expecting a live vite dev server): omit `--features custom-protocol`. This is the only case where the bare `cargo build --bin qontinui-runner` is correct.

**Supervisor source of truth:** the exact args are assembled in `qontinui-supervisor/src/build_monitor.rs::run_build_inner` (look for the `if state.config.dev_mode { ... } else { ..., "--features", "custom-protocol" }` branch). If this doc ever drifts from that code, that file wins.

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

### Workflow Loop

Orchestrates repeated workflow execution with configurable exit strategies. Note: `restart_runner` and `restart_on_signal` between-iteration actions are no longer supported — the supervisor does not restart user runners.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/workflow-loop/start` | Start a workflow loop. Body: `WorkflowLoopConfig` (see below) |
| POST | `/workflow-loop/stop` | Graceful stop (current workflow completes, then loop exits) |
| GET | `/workflow-loop/status` | Current loop status (running, phase, iteration, error) |
| GET | `/workflow-loop/history` | Iteration results with exit check details |
| GET | `/workflow-loop/stream` | SSE stream of phase/iteration changes |
| POST | `/workflow-loop/signal-restart` | Signal that runner restart is needed between iterations |

**Two modes:** Simple mode (single workflow repeated) or Pipeline mode (build/execute/reflect/fix cycle).

**Simple mode start body:**
```json
{
  "workflow_id": "<unified-workflow-id>",
  "max_iterations": 5,
  "exit_strategy": { "type": "reflection" | "workflow_verification" | "fixed_iterations", "reflection_workflow_id": null },
  "between_iterations": { "type": "restart_on_signal" | "restart_runner" | "wait_healthy" | "none", "rebuild": true }
}
```

**Pipeline mode start body:**
```json
{
  "max_iterations": 5,
  "between_iterations": { "type": "restart_runner", "rebuild": true },
  "phases": {
    "build": {
      "description": "Create a workflow that tests the login flow...",
      "context": "Optional additional context",
      "context_ids": ["optional-context-id"]
    },
    "execute_workflow_id": "fallback-workflow-id-if-no-build",
    "reflect": { "reflection_workflow_id": null },
    "implement_fixes": {
      "provider": "claude",
      "model": "opus",
      "timeout_secs": 600,
      "additional_context": "Focus on runner code"
    }
  }
}
```

**Pipeline phases per iteration:**
1. **Build** (conditional) — Generate workflow from description via runner's `/unified-workflows/generate-async`. Runs on iteration 1 and when previous iteration's fixes were workflow-structural.
2. **Execute** — Run the workflow (generated or specified by `execute_workflow_id`).
3. **Reflect** — Trigger reflection, wait for completion, count fixes. Exit if 0 fixes found.
4. **Implement Fixes** (optional) — Spawn Claude Code (`claude --print`) to apply reflection findings. Checks if fixes are structural (triggers rebuild next iteration).

**Pipeline config fields:**
- `phases.build` — Optional. Requires `description`. If absent, `execute_workflow_id` is required.
- `phases.execute_workflow_id` — Fallback workflow ID when build phase is absent.
- `phases.reflect` — Always enabled in pipeline mode. Optional `reflection_workflow_id`.
- `phases.implement_fixes` — Optional. When present, spawns Claude to apply fixes. Defaults: provider/model from supervisor's AI config, timeout 600s.

**Rebuild trigger fix types:** `workflow_step_rewrite`, `instruction_clarification`, `context_addition` — these cause the build phase to re-run next iteration.

**Exit strategies (simple mode only):**
- `reflection` — Triggers reflection after each iteration; exits when 0 new fixes found
- `workflow_verification` — Exits when inner verification loop passes on first iteration
- `fixed_iterations` — Always runs `max_iterations` times

**Between-iteration actions (both modes):**
- `restart_on_signal` — Only restart runner if the workflow called `/workflow-loop/signal-restart` during execution; skip restart otherwise. Use this for workflows that may or may not modify runner code (e.g., Clean and Push across multiple repos).
- `restart_runner` — Always stop/rebuild/start runner, wait for healthy API
- `wait_healthy` — Wait for runner API to respond (no restart)
- `none` — Proceed immediately to next iteration

**Loop phases** (reported in status/stream): `idle`, `building_workflow`, `running_workflow`, `reflecting`, `implementing_fixes`, `evaluating_exit`, `between_iterations`, `waiting_for_runner`, `complete`, `stopped`, `error`

**Pipeline diagnostic events:**
- `pipeline_phase_started` / `pipeline_phase_completed` — Per-phase timing with iteration and phase name
- `fixes_implemented` — Fix count and duration when Claude applies fixes
- `rebuild_triggered` — When structural fixes trigger workflow regeneration

### UI Bridge Proxy

All `/ui-bridge/*` requests are transparently proxied to the runner at `http://127.0.0.1:9876/ui-bridge/*`. This gives the supervisor full access to the UI Bridge SDK without duplicating endpoint definitions.

| Method | Path | Description |
|--------|------|-------------|
| GET/POST | `/ui-bridge/control/*` | Runner's own webview (snapshot, elements, actions) |
| GET/POST | `/ui-bridge/sdk/*` | External SDK-connected apps (elements, actions, AI, page nav) |

Examples:
- `GET http://localhost:9875/ui-bridge/control/snapshot` — Full UI snapshot of runner webview
- `GET http://localhost:9875/ui-bridge/sdk/elements` — List elements in connected SDK app
- `POST http://localhost:9875/ui-bridge/sdk/element/{id}/action` — Execute action on SDK element

Returns `502 Bad Gateway` with descriptive error if the runner is not responding.

## Dashboard

The supervisor serves a React SPA dashboard at `GET /`. Open `http://localhost:9875/` in a browser.

**Features:**
- Real-time service table: Runner, Backend, Frontend, PostgreSQL, Redis, MinIO, Expo, Watchdog with status dots and action buttons
- Dev-start controls: start/stop/restart individual services, bulk actions (Docker, Start All, Stop All, Clean, Fresh, Migrate)
- Log viewer with source/level filtering, pause/resume, auto-scroll
- Workflow loop status panel with iteration tracking
- Confirmation dialogs for destructive actions

**Implementation:** React + TypeScript SPA in `frontend/` directory, built with Vite. Production build output in `dist/` is embedded into the binary via `rust-embed`. Falls back to legacy `static/dashboard.html` if the SPA dist is missing.

**Data flow:**
- SSE `GET /health/stream` for real-time health data (replaces polling)
- SSE `GET /logs/stream` for real-time log entries
- SSE `GET /workflow-loop/stream` for workflow loop status
- Fetches `GET /dev-start/status` for service port availability

## Key Constants

| Constant | Value |
|----------|-------|
| Supervisor port | 9875 |
| Runner API port | 9876 |
| Expo port | 8081 |
| Watchdog check interval | 10s |
| Max restart attempts | 3 |
| Crash loop threshold | 5 crashes in 10min |
| Restart cooldown | 60s |
| Build timeout | 5min |
| Log buffer | 500 entries |
| Smart rebuild quiet period | 10min |
| Smart rebuild fix attempts/cycle | 5 |
| Smart rebuild retry cooldown | 10min |

## Smart Rebuild Flow

When `--smart-rebuild` is enabled, the supervisor monitors source files and rebuilds after 10 minutes of inactivity. Only temp runners are stopped for rebuilds; user runners are left running (they use copied exes).

1. Source watcher polls every 10s for file changes in `src-tauri/src/` and `src/`
2. When changes detected and 10min quiet period elapses → stop temp runners → cargo build
3. If build fails → spawn Claude CLI to fix errors (up to 5 attempts per cycle)
4. If all fix attempts in a cycle fail → wait 10min cooldown → retry
5. On success → restart stopped temp runners

## Test Runner Binary Paths

### Target directories by spawn mode

| Directory | Purpose | Who writes it |
|-----------|---------|---------------|
| `target-pool/slot-{0,1,2}/debug/` | Supervisor parallel build pool (default 3 slots). Each `spawn-test {rebuild: true}` claims a slot and sets `CARGO_TARGET_DIR` to the slot dir. This is where the supervisor looks first when resolving the exe. | Supervisor via `run_cargo_build` |
| `target/debug/` | Legacy / manual `cargo build` output. The supervisor falls back here only if no slot exe exists (preference 3 in `resolve_source_exe`). Pre-pool builds and manual `cargo build` land here. | Manual `cargo build`, `cargo check` |
| `target/release/` | `cargo build --release` output. The supervisor never builds release mode and never looks here. Exists only if you ran a release build manually. | Manual only |

The directories listed in the user's question as `target/rebuild/debug/`, `target/test-build/debug/`, `target/test-runner/debug/`, and `target-tmp/debug/` are **not used by the current supervisor**. The supervisor exclusively uses the `target-pool/slot-{k}/` dirs (configurable pool size via `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`, default 3). If any of those other directories exist on disk, they are artifacts of older supervisor versions and can be safely deleted.

### Exe resolution order (when `rebuild: false`)

`resolve_source_exe()` in `src/process/manager.rs` picks the binary in this order:

1. `target-pool/slot-{last_successful_slot}/debug/qontinui-runner.exe` -- the most recently successful build slot
2. Any `target-pool/slot-{k}/debug/qontinui-runner.exe` that exists on disk (covers supervisor restart where `last_successful_slot` is lost)
3. `target/debug/qontinui-runner.exe` -- legacy fallback for pre-pool or manual builds

Every runner start (temp or user) copies the resolved source exe to `target/debug/qontinui-runner-{id}.exe` so the build artifact is never locked by a running process.

### Forcing a fresh build without spawn-test

To rebuild without spawning a test runner, use the supervisor's build endpoint indirectly through `spawn-test` with immediate cleanup, or build manually:

```bash
# Option 1: Let the supervisor build into the pool (recommended)
# This handles npm run build, --features custom-protocol, and slot selection.
curl -X POST http://localhost:9875/runners/spawn-test \
  -H 'Content-Type: application/json' \
  -d '{"rebuild": true, "wait": false}'
# Then stop the test runner immediately:
# curl -X POST http://localhost:9875/runners/{id}/stop

# Option 2: Manual build into a specific pool slot
cd qontinui-runner
npm run build
cd src-tauri
CARGO_TARGET_DIR=../target-pool/slot-0 cargo build --bin qontinui-runner --features custom-protocol
```

There is no standalone "rebuild only" endpoint; `spawn-test` is the only HTTP trigger for a build.

### Checking binary modtime before trusting a test run

The `spawn-test` response includes `binary_mtime` and `binary_size_bytes` fields (RFC 3339 timestamp and byte count). Always check these before trusting a test run:

```bash
# From the spawn-test response JSON:
# "binary_mtime": "2026-04-09T14:30:00Z"
# "binary_size_bytes": 52428800

# Or stat the exe directly to verify:
stat target-pool/slot-0/debug/qontinui-runner.exe
# Compare the modification time against your last code change.
# If the exe predates your changes, it's stale — rebuild with {rebuild: true}.
```

You can also hit `GET /builds` to see `last_successful_slot` and each slot's `last_completed_at` timestamp to determine which slot has the freshest binary.

### spawn-test {rebuild: true} behavior

**Synchronous.** The HTTP request blocks for the entire build+spawn cycle:

1. **Port reservation** -- atomically claims a free port (9877-9899) and inserts a placeholder into the registry (prevents double-allocation races).
2. **Build** -- acquires a build pool permit (blocks if all slots busy), runs `npm run build` (serialized across slots via `npm_lock`), then `cargo build --bin qontinui-runner --features custom-protocol` with `CARGO_TARGET_DIR` set to the slot dir.
3. **Spawn** -- copies the built exe to `target/debug/qontinui-runner-{id}.exe` and launches the process.
4. **Optional wait** -- if `wait: true`, polls `GET /health` on the spawned runner every 2s until healthy or `wait_timeout_secs` (default 120s) elapses.

**Timeouts:**
- **Build timeout:** 10 minutes (`BUILD_TIMEOUT_SECS = 600`). If cargo exceeds this, the build process is killed and the request returns an error.
- **Queue timeout:** configurable via `queue_timeout_secs` in the request body. If set and all pool slots are busy, the request returns 504 after the specified seconds. If unset, blocks indefinitely until a slot frees.
- **Wait timeout:** configurable via `wait_timeout_secs` (default 120s). Only applies when `wait: true`. The request still returns successfully even if the runner doesn't become healthy -- the `status` field will say `"timeout"` instead of `"healthy"`.
- **No-wait mode:** pass `X-Queue-Mode: no-wait` header to get an immediate 503 with queue info instead of blocking when all slots are busy.

If the build fails, the placeholder port reservation is cleaned up and the error is returned. No runner is spawned.

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
