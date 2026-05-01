# Qontinui Supervisor

Rust-based build server and fleet dashboard for qontinui-runner. Provides parallel cargo builds, temp/named runner spawning, runner lifecycle management, and a React SPA dashboard.

## CRITICAL: Runner Lifecycle Scope

The supervisor manages lifecycle for **temp runners** (`test-*`) and **named runners** (`named-*`). The primary runner and any user-started runners are **user-managed** — the supervisor tracks their health but never starts, stops, or restarts them unprompted.

- **Temp runners** (`test-*`): Spawned via `POST /runners/spawn-test`, auto-cleaned on stop. Run with a visible Tauri window and an isolated WebView2 profile. The UI Bridge is fully functional on temp runners.
- **Named runners** (`named-*`): Spawned via `POST /runners/spawn-named`, persistent across supervisor restarts. Saved to settings. Not auto-cleaned. Support start/stop/restart/protect.
- **User runners** (everything else): Started by the user with visible Tauri windows. The supervisor observes health only.

**First-healthy watchdog.** Every runner the supervisor spawns (via any of the `start_managed_runner` callers above) gets a per-spawn watchdog that polls its HTTP `/health`. If the process stays alive but never binds the API within the budget (default 90s), the supervisor kills the PID so a wedged start doesn't linger as a zombie on the port. Scope is strictly per-spawn — does not auto-restart, does not touch runners that were already up when the supervisor started. Budget override: env `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS` (seconds, must be > 0).

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
| `-w, --watchdog` | Enable health monitoring (observe-only, implies `--auto-start`) |
| `-a, --auto-start` | Start runner on supervisor launch |
| `--expo-dir` | Path to Expo/React Native project directory |
| `-l, --log-file` | Append the in-memory log buffer to this file (persistent supervisor log, no rotation). Overrides `<log-dir>/supervisor.log`. |
| `--log-dir` | Directory for persistent log files. Writes `<log-dir>/supervisor.log` plus one `<log-dir>/<runner-id>.log` per managed runner (tees runner stdout/stderr). Directory is created on startup. No rotation. |
| `--port` | Supervisor HTTP port (default: 9875) |
| `--no-prewarm` | Disable post-startup `cargo check` slot pre-warming (also `QONTINUI_SUPERVISOR_NO_PREWARM=1`) |

## Persistent Logs

The supervisor keeps only the last 500 log entries (configurable via `QONTINUI_SUPERVISOR_LOG_BUFFER_SIZE`, ~30 min of activity at default) in its in-memory circular buffer, which is not enough to diagnose a crash-loop after the fact. Pass `--log-dir` (or `--log-file`) to tee every entry into an append-only file on disk.

**Recommended defaults:**

- Windows: `%LOCALAPPDATA%\qontinui-supervisor\logs\` (e.g. `C:\Users\<you>\AppData\Local\qontinui-supervisor\logs`)
- Linux: `~/.local/state/qontinui-supervisor/logs/`
- macOS: `~/Library/Logs/qontinui-supervisor/`

**Usage:**

```bash
# Windows (PowerShell)
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri --log-dir $env:LOCALAPPDATA\qontinui-supervisor\logs

# Linux/macOS
./target/debug/qontinui-supervisor -p ../qontinui-runner/src-tauri --log-dir ~/.local/state/qontinui-supervisor/logs
```

**Files written:**

- `<log-dir>/supervisor.log` — every entry that also goes through the in-memory buffer (supervisor events, build output, expo, runner log lines routed through `state.logs.emit`).
- `<log-dir>/<runner-id>.log` — one file per managed runner, capturing that runner's stdout+stderr tee'd from `spawn_stdout_reader`/`spawn_stderr_reader`. Per-runner files are opened on `ManagedRunner::new_with_log_dir` (at startup, on `POST /runners`, or when a `test-*`/`named-*` runner is spawned).

**Format:** `<rfc3339-millis> [source] [LEVEL] <message>` — one line per entry, same content as the in-memory SSE stream.

**Precedence:** `--log-file <PATH>` overrides the default `<log-dir>/supervisor.log` location but does NOT affect per-runner files; for per-runner files you must set `--log-dir`.

**No rotation.** Files grow unbounded — rotate externally (logrotate, PowerShell scheduled task, etc.) if size becomes an issue. Supervisor reopens files with `O_APPEND` semantics, so rotating out-of-process with `copytruncate`-style tools is safe; rename+signal style rotation will keep writing to the moved file and you must restart the supervisor.

**Best-effort:** a missing/unwritable log path logs a warning once and continues — persistent logging never blocks supervisor startup.

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
| POST | `/runners/spawn-test` | Spawn ephemeral test runner on next free port (9877-9899). Body: `{rebuild?, use_lkg?, wait?, wait_timeout_secs?, requester_id?, queue_timeout_secs?}`. Returns `{id, port, api_url, ui_bridge_url}` plus `used_lkg`/`lkg` when `use_lkg: true`. See "Last-known-good (LKG) fallback for agents" below. Auto-cleaned on stop. |
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

### Spawn-Monitor Placement

The supervisor pulls placement config from the primary runner via `GET http://localhost:9876/spawn-placement/preview?slot=N&overflow=wrap` when spawning a temp runner. Configuration lives in the runner's Settings → Runner Instances UI.

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
| GET | `/supervisor-bridge/control/snapshot` | Full snapshot of dashboard UI. Includes `registration: {totalRegistered, everHadRegistrations, byRoute}` so callers can distinguish "no elements on this page" from "this app has no bridge coverage". |
| GET | `/supervisor-bridge/control/elements` | List dashboard UI elements |
| POST | `/supervisor-bridge/control/element/{id}/action` | Execute action on dashboard element |
| POST | `/supervisor-bridge/control/discover` | Trigger element discovery |
| GET | `/supervisor-bridge/control/console-errors` | Get console errors from dashboard |
| POST | `/supervisor-bridge/control/page/evaluate` | Evaluate JS in dashboard webview |
| POST | `/supervisor-bridge/control/page/navigate` | Navigate dashboard page. Body: `{url, mode?: "soft"\|"hard"}`. Default `"hard"` (full webview reload); `"soft"` uses `history.pushState` + synthetic `popstate` so injected globals (fetch patches, test state) survive. |
| POST | `/supervisor-bridge/control/page/refresh` | Refresh dashboard page |
| POST | `/supervisor-bridge/control/network/stubs` | Register a fetch stub. Body: `{urlPattern, method?, response: {status?, headers?, body\|bodyJson}, times?: 1\|"always"}`. Returns `{id}`. Stubs persist across soft navigations, cleared on hard reload. |
| GET | `/supervisor-bridge/control/network/stubs` | List active stubs with hit counts + remaining matches |
| DELETE | `/supervisor-bridge/control/network/stubs/{id}` | Remove one stub by id |
| DELETE | `/supervisor-bridge/control/network/stubs` | Clear all stubs. Returns `{cleared: <count>}` |

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

### Debug endpoints (gated)

Debug-only endpoints under `/control/dev/*` are admitted **only** when the supervisor is started with `QONTINUI_SUPERVISOR_DEBUG_ENDPOINTS=1`. The env var is read once at startup and cached on `SharedState`; an unset / `0` / empty value makes every endpoint here return `403 {"error": "debug_endpoints_disabled"}`. These are for local manual testing — never set this in shared deployments.

| Method | Path | Description |
|--------|------|-------------|
| POST | `/control/dev/emit-build-id` | Inject a synthetic `buildId` value (body: `{"buildId": "<string>"}`) into the live `/health/stream` SSE without rebuilding the supervisor. Returns `{"ok": true, "emitted": "<buildId>", "subscribers": <usize>}`. The on-disk `build_id` is unchanged; only the next streamed health event carries the override. Used by `/manual-test` to exercise `BuildRefreshBanner` without a full rebuild + restart. |

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

**Why `--features custom-protocol` is mandatory:** without it, the `tauri` crate compiles with `cfg(dev)` active and the binary loads the frontend from `devUrl` (`http://localhost:1420`) instead of embedding `dist/`. No Vite dev server is running, so the webview would show `ERR_CONNECTION_REFUSED`.

**Why you must build into a slot dir:** the supervisor's `resolve_source_exe` picks the source exe from `last_successful_slot` first, then scans other slots, and only falls back to `target/debug/qontinui-runner.exe` last. On every runner start it copies the source over `target/debug/qontinui-runner-{id}.exe` — so building into the default `target/` means the supervisor may overwrite it with a stale slot exe.

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

### Last-known-good (LKG) fallback for agents

The supervisor preserves the most recently successfully-built runner exe at `target-pool/lkg/qontinui-runner.exe` after every successful `cargo build`. Slot dirs can be clobbered by a subsequent failed build that overwrites or partially-deletes the slot's exe; the LKG copy is independent and survives those events. A sidecar at `target-pool/lkg/lkg.json` records `{built_at, source_slot, exe_size}` and is hydrated into `state.build_pool.last_known_good` at supervisor startup so it survives restarts.

**When this matters.** Multiple concurrent agents share the build pool. Agent A's broken build can leave the slots in a state where Agent B's `spawn-test {rebuild: false}` would either fail or run a worse binary than the LKG. If Agent B's own changes are *already in the LKG* (because Agent B's edits predate the most recent successful build), Agent B can pin to the LKG instead of waiting for the slots to recover.

**The comparison rule — agents MUST do this themselves; the supervisor does not enforce it.**

1. Read LKG metadata from `GET /health` → `build.lkg.built_at` (RFC3339), or `GET /builds` → `lkg.built_at`. Both surface the same value.
2. Take the maximum mtime across every file you've changed in the runner workspace (`stat -c %Y` on Linux, `(Get-Item path).LastWriteTime` in PowerShell, etc.).
3. Compare:
   - **`max(mtime of changed files) <= lkg.built_at`** → the LKG was built AFTER your changes, so those changes are already compiled into the LKG binary. Safe to spawn with `{rebuild: false, use_lkg: true}`.
   - **`max(mtime of changed files) > lkg.built_at`** → the LKG predates your changes. Pinning to it would silently run stale code. You must rebuild instead.
4. If you have NO uncommitted changes, the LKG always covers you (any clean checkout's tracked files have mtimes from the original git checkout, which is older than every build).

**Why timestamps and not git hashes.** With three concurrent agents touching files at different commits, hashes are noisy — what matters is "do the bytes I edited live in this binary." File mtime answers that directly. The risk case (agent edits file at T1, LKG at T0 < T1) is exactly the case where the comparison correctly says "do not use LKG."

**API.**

```bash
# Inspect LKG state
curl localhost:9875/health   | jq '.build.lkg'
curl localhost:9875/builds   | jq '.lkg'
# → {"built_at": "2026-04-26T15:30:00Z", "source_slot": 1, "exe_size": 253749760}
# → null if no successful build has happened yet on this checkout

# Spawn a test runner pinned to LKG (no rebuild)
curl -X POST localhost:9875/runners/spawn-test \
     -H 'content-type: application/json' \
     -d '{"rebuild": false, "use_lkg": true, "wait": true}'
# Response includes: "used_lkg": true, "lkg": {"built_at": ..., "source_slot": ..., "exe_size": ...}
```

**Interaction with `rebuild`.**

| `rebuild` | `use_lkg` | Behavior |
|-----------|-----------|----------|
| `false`   | `false`   | Default. Uses freshest slot exe via `resolve_source_exe`. |
| `false`   | `true`    | Skip the build, run from `lkg/qontinui-runner.exe`. |
| `true`    | `false`   | Build, then run the freshest slot exe (which is the just-built one). |
| `true`    | `true`    | Build, then run from LKG. On build success, LKG is updated first, so this runs your fresh build. On build failure the request fails — `use_lkg` is NOT an automatic build-failure fallback; the agent decides whether to retry without `rebuild`. |

**LKG capture happens after every successful build** in `build_monitor.rs::update_lkg_after_success`. Capture is best-effort — failures are logged but do not fail the build itself. The previous LKG stays intact if the new copy can't be written. Only real `cargo build` success updates LKG; the `cargo check` prewarm path does not.

### spawn-test / spawn-named `{rebuild: true}` behavior

**Synchronous.** The HTTP request blocks for the entire build+spawn cycle:

1. **Port reservation** — atomically claims a free port (9877-9899) and inserts a placeholder.
2. **Build** — acquires a build pool permit (blocks if all slots busy), runs `npm run build` (serialized via `npm_lock`), then `cargo build --bin qontinui-runner --features custom-protocol` with `CARGO_TARGET_DIR` set to the slot dir.
3. **Spawn** — copies the built exe to `target/debug/qontinui-runner-{id}.exe` and launches the process.
4. **Optional wait** — if `wait: true`, polls `GET /health` on the spawned runner every 2s until healthy or `wait_timeout_secs` (default 120s) elapses.

**Timeouts:**
- **Build timeout:** 30 minutes default (override via `QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS`, clamped [60, 7200]). If cargo exceeds this, the build process is killed.
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
| Build timeout | 30min (1800s) default, override `QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS` |
| Port wait timeout | 120s |
| Graceful kill timeout | 5s |
| Log buffer | 500 entries (override: `QONTINUI_SUPERVISOR_LOG_BUFFER_SIZE`, clamped [100, 10000]) |
| Build pool size | 3 (override: `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`) |
| Temp runner port range | 9877-9899 |
| First-healthy watchdog budget | 90s (override: `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS`); poll interval 3s |

## Diagnosing failed runner spawns

When a temp runner dies during startup (`spawn-test` returns `error: runner_died_during_startup`), the supervisor surfaces three diagnostic surfaces:

| Endpoint | Returns |
|----------|---------|
| spawn-test response itself | `recent_logs` (last ~10 lines) + `early_log_path` |
| `GET /runners/{id}/early-log` | Full early-log file content (capped at 1 MB tail) — survives in `stopped_runners` cache after the runner is purged |
| `GET /runners/{id}/crash-summary` | `{exit_code, duration_alive_ms, last_phase_log, panic_excerpt}` for already-stopped runners |

**Recurring pattern: runner hangs in PG bootstrap.** Force-killing runners (with `Stop-Process` or `taskkill /F`) leaves PG backend connections holding row-level locks for ~2 minutes until PG's idle-in-transaction sweeper times them out. The next runner's `apply_canonical_schema` / `run_migrations` hits these locks and hangs. The runner now has 30s per-stage timeouts (commit on `qontinui-runner` adds `PG bootstrap: <stage>...` bracketed logs + a `pg_stat_activity` dump on timeout). If a fresh runner spawn hangs at "applying canonical schema" within the first 6s, check for stuck PG sessions:

```sql
SELECT pid, query, wait_event_type, wait_event,
       EXTRACT(epoch FROM (now() - query_start)) AS elapsed_secs
  FROM pg_stat_activity
 WHERE datname = 'qontinui_runner' AND state != 'idle';
```

Either wait ~2 min for PG to clean up, or `SELECT pg_terminate_backend(<pid>)` the offending sessions. **Avoid force-killing runners** when the supervisor can stop them via API — graceful stop closes PG connections cleanly.

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
