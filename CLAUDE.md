# Qontinui Supervisor

Rust-based process manager for the qontinui-runner. Replaces the Python `dev-supervisor.py` for core process lifecycle management.

## Architecture

Standalone Axum HTTP server that manages the runner process:
- **Start/stop/restart** with optional cargo rebuild
- **Watchdog** auto-recovery with crash loop detection
- **Log capture** with SSE streaming and circular buffer
- **Build error detection** during first 60s of runner startup
- **AI auto-debug** spawns Claude/Gemini to diagnose build failures and crash loops
- **Code activity detection** defers debug sessions when files are being edited or external Claude is running
- **Dev-start orchestration** HTTP endpoints to control `dev-start.ps1` services
- **Expo process management** start/stop/monitor Expo/React Native dev server
- **Workflow loop** orchestrates repeated workflow execution with exit strategies and runner restarts between iterations
- **UI Bridge proxy** transparent proxy to runner's UI Bridge SDK endpoints (control + SDK modes)
- **HTML dashboard** self-contained web UI at `GET /` for visual monitoring and control (embedded in binary via `include_str!`)

## Building & Running

```bash
cargo build                    # Build debug binary
cargo check                    # Type-check only
cargo fmt                      # Format code
cargo clippy -- -D warnings    # Lint

# Start in dev mode with watchdog
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -d -w

# Start with auto-debug enabled
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -d -w --auto-debug

# Start in exe mode (no Vite, runs compiled binary directly)
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -a

# Start with Expo dev server management
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri -d -w --expo-dir ../qontinui-mobile
```

## CLI Flags

| Flag | Description |
|------|-------------|
| `-p, --project-dir` | Path to `qontinui-runner/src-tauri` (required) |
| `-d, --dev-mode` | Run `npm run tauri dev` instead of compiled exe |
| `-w, --watchdog` | Enable watchdog (implies auto-start) |
| `-a, --auto-start` | Start runner on supervisor launch |
| `--auto-debug` | Enable AI auto-debug on startup |
| `--expo-dir` | Path to Expo/React Native project directory |
| `-l, --log-file` | Log file for runner output |
| `--port` | Supervisor HTTP port (default: 9875) |

## API Endpoints

### Runner Lifecycle

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | HTML dashboard (self-contained, embedded in binary) |
| GET | `/health` | Comprehensive status (runner, watchdog, build, AI, code activity, expo) |
| POST | `/runner/stop` | Stop runner + cleanup |
| POST | `/runner/restart` | Stop + rebuild + start. Body: `{"rebuild": bool}` |
| POST | `/runner/watchdog` | Control watchdog. Body: `{"enabled": bool, "reset_attempts": bool}` |
| POST | `/supervisor/restart` | Self-restart with same CLI args |

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

### Dev-Start Orchestration

| Method | Path | Description |
|--------|------|-------------|
| POST | `/dev-start/backend` | Start backend (60s timeout) |
| POST | `/dev-start/backend/stop` | Stop backend (30s timeout) |
| POST | `/dev-start/frontend` | Start frontend (180s timeout) |
| POST | `/dev-start/frontend/stop` | Stop frontend (30s timeout) |
| POST | `/dev-start/docker` | Start Docker services (60s timeout) |
| POST | `/dev-start/docker/stop` | Stop Docker services (30s timeout) |
| POST | `/dev-start/all` | Start everything (300s timeout) |
| POST | `/dev-start/stop` | Stop everything (30s timeout) |
| POST | `/dev-start/clean` | Clean caches (30s timeout) |
| POST | `/dev-start/fresh` | Clean + start everything (300s timeout) |
| POST | `/dev-start/migrate` | Run DB migrations (120s timeout) |
| GET | `/dev-start/status` | Check service ports (PostgreSQL, Redis, MinIO, Backend, Frontend, Runner, Vite) |

### Expo

| Method | Path | Description |
|--------|------|-------------|
| POST | `/expo/start` | Start Expo dev server (requires `--expo-dir`) |
| POST | `/expo/stop` | Stop Expo dev server |
| GET | `/expo/status` | Running state, PID, port, configured flag |
| GET | `/expo/logs/stream` | SSE stream filtered to Expo log source |

### Workflow Loop

Orchestrates repeated workflow execution with configurable exit strategies and between-iteration actions. Designed for scenarios where the runner must be restarted between iterations (e.g., verifying code changes to the runner itself).

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

The supervisor serves a self-contained HTML dashboard at `GET /`. Open `http://localhost:9875/` in a browser.

**Features:**
- Real-time status cards: Runner, Ports, Watchdog, Build, AI Debug, Code Activity, Expo
- Dev-start controls: start/stop backend, frontend, Docker, all services
- Log viewer with source/level filtering, pause/resume, auto-scroll
- AI output panel with live SSE streaming
- Action buttons for all supervisor operations

**Implementation:** Single `static/dashboard.html` file (~800 lines) with inline CSS+JS, compiled into the binary via `include_str!()`. No external dependencies, no CDN, no build step.

**Data flow:**
- Polls `GET /health` every 3s for card data
- Polls `GET /dev-start/status` every 5s for port list
- SSE `GET /logs/stream` for real-time log entries
- SSE `GET /ai/output/stream` for AI output
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
| Vite port | 1420 |
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

## Auto-Debug Flow

1. Watchdog detects crash loop or max restarts → calls `schedule_debug()`
2. Build monitor detects build error in runner output → calls `schedule_debug()`
3. `schedule_debug()` checks code activity:
   - If code being edited or external Claude session → defers to `pending_debug`
   - Otherwise → spawns AI debug session immediately
4. Code activity monitor (every 30s) checks for deferred debug:
   - If pending + quiet period elapsed + no external Claude → triggers `spawn_ai_debug()`
5. Debug prompt includes: runner logs, build errors, git changes, running tasks
6. Claude uses `--print` mode; Gemini uses piped stdin via PowerShell script

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
