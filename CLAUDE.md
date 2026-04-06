# Qontinui Supervisor

Rust-based build server and health monitor for qontinui-runner. Provides cargo builds, temp runner spawning for testing, and observe-only health monitoring.

## CRITICAL: The Supervisor NEVER Manages User Runner Lifecycle

The supervisor **only** manages the lifecycle of temp/test runners (`test-*` IDs). All other runners (primary, secondary, discovered) are **user-managed** — the supervisor never starts, stops, restarts, or kills them. Users start runners manually and close them by closing the window.

- **Temp runners** (`test-*`): Spawned via `POST /runners/spawn-test`, auto-cleaned on stop. These are headless (no window) and used for testing code changes.
- **User runners** (everything else): Started by the user with visible Tauri windows. The supervisor tracks their health but takes no lifecycle action.

## Architecture

Standalone Axum HTTP server:
- **Temp runner spawning** for testing code changes
- **Cargo build** without restarting any runner
- **Health monitoring** (observe-only) for all runners
- **Log capture** with SSE streaming and circular buffer
- **AI auto-debug** spawns Claude/Gemini to diagnose build failures
- **Code activity detection** defers debug sessions when files are being edited or external Claude is running
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

# Start with auto-debug enabled
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -w --auto-debug

# Start with Expo dev server management
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -w --expo-dir ../qontinui-mobile
```

## CLI Flags

| Flag | Description |
|------|-------------|
| `-p, --project-dir` | Path to `qontinui-runner/src-tauri` (required) |
| `-w, --watchdog` | Enable health monitoring (observe-only, no restarts) |
| `--auto-debug` | Enable AI auto-debug on startup |
| `--expo-dir` | Path to Expo/React Native project directory |
| `-l, --log-file` | Log file for runner output |
| `--port` | Supervisor HTTP port (default: 9875) |

## API Endpoints

### Health & Dashboard

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | React SPA dashboard |
| GET | `/health` | Comprehensive status (runners, build, AI, code activity, expo) |
| POST | `/supervisor/restart` | Self-restart supervisor (runners are left running) |

### Temp Runner Management (test-* only)

| Method | Path | Description |
|--------|------|-------------|
| GET | `/runners` | List all runners with status (observe-only for user runners) |
| POST | `/runners/spawn-test` | Spawn ephemeral test runner on next free port (9877-9899). Body: `{"rebuild": bool}`. Returns `{id, port, api_url, ui_bridge_url}`. Auto-cleaned up on stop. |
| POST | `/runners/{id}/start` | Start a temp runner (rejects non-temp) |
| POST | `/runners/{id}/stop` | Stop a temp runner (rejects non-temp) |
| POST | `/runners/{id}/restart` | Restart a temp runner (rejects non-temp) |
| GET/POST | `/runners/{id}/ui-bridge/{*path}` | Proxy UI Bridge requests to a specific runner |

### Logs

| Method | Path | Description |
|--------|------|-------------|
| GET | `/logs/history` | Recent log entries from circular buffer |
| GET | `/logs/stream` | SSE stream of real-time log events |
| GET | `/logs/file/{type}` | Read `.dev-logs/` files |
| GET | `/logs/files` | List available log files |

### AI Debug

| Method | Path | Description |
|--------|------|-------------|
| POST | `/ai/debug` | Manually trigger debug session. Body: `{"prompt": "..."}` |
| POST | `/ai/auto-debug` | Enable/disable auto-debug. Body: `{"enabled": bool}` |
| GET | `/ai/status` | AI session status + output tail |
| POST | `/ai/stop` | Kill running AI session |
| GET | `/ai/provider` | Current provider + model |
| POST | `/ai/provider` | Set provider/model. Body: `{"provider": "claude", "model": "opus"}` |
| GET | `/ai/models` | Available providers and models |
| GET | `/ai/output/stream` | SSE stream of AI output |
| POST | `/claude/debug` | Alias for `/ai/debug` |
| GET | `/claude/status` | Alias for `/ai/status` |
| POST | `/claude/stop` | Alias for `/ai/stop` |

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
- AI debug panel with live SSE streaming, provider/model selector
- Log viewer with source/level filtering, pause/resume, auto-scroll
- Workflow loop status panel with iteration tracking
- AI Fix buttons for down services (auto-sends context to debug agent)
- Confirmation dialogs for destructive actions

**Implementation:** React + TypeScript SPA in `frontend/` directory, built with Vite. Production build output in `dist/` is embedded into the binary via `rust-embed`. Falls back to legacy `static/dashboard.html` if the SPA dist is missing.

**Data flow:**
- SSE `GET /health/stream` for real-time health data (replaces polling)
- SSE `GET /logs/stream` for real-time log entries
- SSE `GET /ai/output/stream` for AI output
- SSE `GET /workflow-loop/stream` for workflow loop status
- Fetches `GET /dev-start/status` for service port availability
- Fetches `GET /ai/models` once on init for provider/model select

## AI Providers

| Provider | Key | Model ID | Display Name |
|----------|-----|----------|--------------|
| claude | opus | claude-opus-4-6 | Claude Opus 4.6 |
| claude | sonnet | claude-sonnet-4-5-20250929 | Claude Sonnet 4.5 |
| gemini | flash | gemini-3-flash-preview | Gemini 3 Flash |
| gemini | pro | gemini-3-pro-preview | Gemini 3 Pro |

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
| AI debug cooldown | 5min |
| AI output buffer | 2000 entries |
| Code quiet period | 5min |
| Code check interval | 30s |
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

## Auto-Debug Flow

1. Build monitor detects build error in runner output → calls `schedule_debug()`
2. `schedule_debug()` checks code activity:
   - If code being edited or external Claude session → defers to `pending_debug`
   - Otherwise → spawns AI debug session immediately
3. Code activity monitor (every 30s) checks for deferred debug
4. Debug prompt includes: runner logs, build errors, git changes, running tasks
5. Claude uses `--print` mode; Gemini uses piped stdin via PowerShell script

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
