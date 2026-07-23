# Qontinui Supervisor

Rust-based build server and fleet dashboard for qontinui-runner. Provides parallel cargo builds, temp/named runner spawning, runner lifecycle management, and a React SPA dashboard.

## CRITICAL: Runner Lifecycle Scope

The supervisor manages lifecycle for **temp runners** (`test-*`) and **named runners** (`named-*`). The primary runner and any user-started runners are **user-managed** — the supervisor tracks their health but never starts, stops, or restarts them unprompted.

With `--auto-start` / `--watchdog` the supervisor starts the **primary** once at boot (through the same `start_runner_by_id` funnel an operator `POST /runners/primary/start` uses, so the provenance start gate applies); it never auto-starts named/temp/external runners.

**Crash-only ambient watchdog** (plan `2026-07-03-primary-runner-crash-resilience`, Phase 1). Under `--watchdog`, a supervisor-spawned runner whose process **crashes** (exits non-zero / dies unexpectedly) is auto-restarted through the same `start_runner_by_id` funnel (provenance start gate applies). Hard rules:

- **Never restarts a *running* runner** — this is exit-observation only, not health-based resurrection.
- **Never restarts on operator stop.** Every operator-facing stop path latches `stop_requested` before the kill; the exit monitor reads it when the exit is observed. The flag is cleared on the next *start* (not at stop completion), so the marker is race-free — a failed stop (`StillHeld`) whose process dies later still counts as operator-intended.
- **Never restarts a clean exit** (code 0 — window close, internal shutdown).
- **Never restarts external/user-started runners** — restart requires the spawn provenance of a supervisor-held Child handle.
- **Scope: primary only by default.** Under `--watchdog` the primary's per-runner `WatchdogState.enabled` defaults true; named/temp/external default false. Arm any runner explicitly via `POST /runners/{id}/watchdog {"enabled": true}`.
- **Crash-loop guard:** exponential backoff 5s → 30s → 120s between attempts; max 3 auto-restarts per rolling 30 minutes, then the watchdog disarms itself (`disabled_reason: "crash loop — operator required"`, `enabled` left true so intent stays visible) with an ERROR log + diagnostics event. Reset via `POST /runners/{id}/watchdog {"enabled": true, "reset_attempts": true}`.
- **Kill-switch:** env `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1` disables all crash auto-restarts without a rebuild.
- **Observability:** live counters (`enabled`, `restart_attempts`, `last_restart_at`, `crash_count`, `disabled_reason`) on `GET /runners` (per runner), `GET /health` (top-level = primary's; per-runner in `runners[]`), and the SSE health stream.

- **Temp runners** (`test-*`): Spawned via `POST /runners/spawn-test`, auto-cleaned on stop. Run with a visible Tauri window and an isolated WebView2 profile. The UI Bridge is fully functional on temp runners.
- **Named runners** (`named-*`): Spawned via `POST /runners/spawn-named`, persistent across supervisor restarts. Saved to settings. Not auto-cleaned. Support start/stop/restart/protect.
- **User runners** (everything else): Started by the user with visible Tauri windows. The supervisor observes health only.

**First-healthy watchdog.** Every runner the supervisor spawns (via any of the `start_managed_runner` callers above) gets a per-spawn watchdog that polls its HTTP `/health`. If the process stays alive but never binds the API within the budget (default 90s), the supervisor kills the PID so a wedged start doesn't linger as a zombie on the port. Scope is strictly per-spawn — does not touch runners that were already up when the supervisor started. Budget override: env `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS` (seconds, must be > 0). Note: on a crash-watchdog-armed runner, the first-healthy kill reads as a crash (non-zero exit, no stop intent) — the crash-only watchdog will retry the start up to its loop-guard budget, then disarm.

## Per-instance settings (runner registry isolation)

The runner registry + AI config persist to a **per-supervisor-instance** path, NOT a flat file. `settings::settings_path` returns:

```
<dev_logs_dir>/instances/<instance-key>/supervisor-settings.json
```

where `<instance-key>` = `<project-dir-basename>-<8-hex>`, the 8-hex being a stable SHA-256 of the **canonicalized absolute `project_dir`** (best-effort canonicalize; never panics). The basename is human-readable, the hash is collision-proofing for two same-named project dirs under different parents. Examples: the live instance → `qontinui-runner-<hash>`; an isolated E2E worktree → `qontinui-runner-wt-e2e-<hash>`. **Logs stay shared** in `.dev-logs/` (intentional, operator-friendly); only mutable STATE is namespaced.

**Why:** `dev_logs_dir` is `project_dir.parent().parent()/.dev-logs`, so a test supervisor under `D:\qontinui-root\qontinui-runner-wt-e2e\src-tauri` computed the same grandparent `.dev-logs` as the live instance and its runner registrations bled into (and persisted in) the live `:9875` registry (observed 2026-06-05).

**Legacy migration (one-shot, best-effort):** on first `settings_path` call, if no per-instance file exists but the legacy flat `<dev_logs_dir>/supervisor-settings.json` does, it is **copied** into the per-instance path — but ONLY when this instance's basename is the historical default `qontinui-runner` (the legacy file's contents belong to the live instance). Every other instance starts with a **fresh empty registry** rather than inheriting the live one's runners (inheriting is exactly the bug). The legacy file is left in place (older binaries keep reading it) with a `supervisor-settings.json.migrated-to-<key>` breadcrumb marker next to it. So on first post-deploy boot the live instance KEEPS all its runners via this migration.

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
| `-w, --watchdog` | Enable health monitoring + crash-only auto-restart of the primary (implies `--auto-start`; see "Crash-only ambient watchdog"). Kill-switch: `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1`. |
| `-a, --auto-start` | Start runner on supervisor launch |
| `--expo-dir` | Path to Expo/React Native project directory |
| `-l, --log-file` | Append the in-memory log buffer to this file (persistent supervisor log, no rotation). Overrides `<log-dir>/supervisor.log`. |
| `--log-dir` | Directory for persistent log files. Writes `<log-dir>/supervisor.log` plus one `<log-dir>/<runner-id>.log` per managed runner (tees runner stdout/stderr). Directory is created on startup. No rotation. |
| `--port` | Supervisor HTTP port (default: 9875) |
| `--no-prewarm` | Disable post-startup `cargo check` slot pre-warming (also `QONTINUI_SUPERVISOR_NO_PREWARM=1`) |

## Restarting the supervisor

THE restart path is the checked-in `scripts/restart-supervisor.ps1` — use it
instead of ad-hoc per-session PowerShell. It stops the running instance
(graceful `POST /supervisor/shutdown`, falling back to `Stop-Process`), waits
for the port to free, copies `target\debug\qontinui-supervisor.exe` to
`target\debug\copies\`, relaunches it, and polls `/health`.

```powershell
# from the repo root
.\scripts\restart-supervisor.ps1 -Build   # -Build runs cargo build first
```

It launches the copy in a **visible** window on purpose: Windows Defender's
`PowhidSubExec.B` heuristic kills hidden (`-WindowStyle Hidden` +
`-ExecutionPolicy Bypass`) launches of the unsigned exe (2026-06-05 incident).
Params: `-Build`, `-Port` (9875), `-ProjectDir`, `-LogFile`, `-Watchdog`.

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
| GET | `/runners` | List all runners with status. Each entry carries **commit-based build provenance** for the exe it is actually running: `build_sha` (full 40-char SHA), `build_source` (`live_tree`/`origin_main`/`override`), `build_source_root`, `build_built_at`. `null` = unknown provenance (never started by this supervisor, or a legacy artifact with no sidecar) — do NOT read it as "current". Prefer these over the adjacent `stale_binary`, which is an **mtime** comparison and is blind to commit staleness. |
| POST | `/runners` | Add a runner config to the registry |
| POST | `/runners/spawn-test` | Spawn ephemeral test runner on next free port (9877-9899). Body: `{rebuild?, use_lkg?, wait?, wait_timeout_secs?, requester_id?, queue_timeout_secs?, git_ref?, worktree_path?, from_working_tree?, frontend_only?, async?}`. **`rebuild: true` builds a supervisor-owned `origin/main` worktree by default**, NOT the shared working checkout. Returns `{id, port, api_url, ui_bridge_url, build_id, source, build_sha, build_source_default, build_source_warning}` plus `used_lkg`/`lkg` when `use_lkg: true`. See "Build provenance: spawn-test builds `origin/main` by DEFAULT" and "Last-known-good (LKG) fallback for agents" below. Auto-cleaned on stop. |
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
| GET | `/builds` | Snapshot of the parallel build pool: pool size, available permits, queue depth, per-slot state (`idle` or `building` with `started_at`/`elapsed_secs`/`requester_id`/`rebuild_kind`), `last_successful_slot`, and a top-level `active_builds` array. **Invariant:** `pool_size == available_permits + active_builds.len()`. `active_builds` and `available_permits` are derived from the same per-slot iteration as `slots[]` so the three views can never disagree mid-release. A separate `semaphore_permits` field exposes the raw `Semaphore::available_permits()` value for debugging transient release-ordering divergence inside `run_cargo_build_with_dir` — at steady state it equals `available_permits`. |
| GET | `/builds/{slot_id}/log/stream` | SSE stream of cargo stderr lines for this slot's currently-running build. Events: `status` (one-shot prelude with `{state: "idle"\|"building", ...}`), `cargo` (one per stderr line, data is the raw line), `lagged` (broadcast drop count when the subscriber falls behind), `completed` (one frame on each building→idle transition, then the stream stays open for the next build). Returns 404 with `{error: "slot_not_found", ...}` if `slot_id` is out of range. Best for "tail cold cargo builds spawned via `POST /runners/spawn-test {rebuild: true}` so the user has progress visibility without polling `/builds/{slot_id}/log`." |
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
| POST | `/runner/restart` | Restart runner (legacy single-runner endpoint, targets the primary). Body `{rebuild?, force?, from_working_tree?}`. **`rebuild: true` is detached from the HTTP connection** — returns **202** `{status:"rebuilding", build_id, poll:"/builds"}` immediately and runs the stop→build→start sequence in a background task (a client disconnect / short HTTP timeout can no longer abandon the build mid-flight). Poll `GET /builds` (or `GET /build/{id}/status`) for the terminal outcome. **`from_working_tree` defaults to `false`** → the rebuild compiles a fresh `origin/main` worktree (provenance `origin_main`), so the primary runs latest-green-main; set `from_working_tree: true` to compile the live working tree (legacy `live_tree`). See "Primary rebuild builds origin/main by default". `rebuild: false` stays synchronous (fast restart, 200 on success / 503 if unhealthy after start). |
| POST | `/runner/watchdog` | Control watchdog (legacy single-runner endpoint) |
| POST | `/runner/fix-and-rebuild` | Rebuild the live runner tree, **detached from the HTTP connection**. Returns **202** `{status:"accepted", build_id, submission_id, poll}` immediately; the ~10-20min build runs in a background task (so a client disconnect can't cancel it mid-flight) and writes the provenance sidecar + LKG. Poll `GET /build/{id}/status` for the terminal outcome. A second call while one is in flight returns the existing submission id (`deduplicated: true`). |

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

## Build provenance: spawn-test builds `origin/main` by DEFAULT

**`POST /runners/spawn-test {rebuild: true}` compiles a supervisor-owned detached worktree pinned to `origin/main` — NOT the shared working checkout.** The supervisor fetches and hard-resets that worktree on every spawn, so a rebuild is reproducible and can never inherit whatever branch a peer session parked `qontinui-runner/` on, nor their uncommitted WIP.

**Why this is the default.** It used to build the shared checkout. On 2026-07-22 that checkout sat on `fix/runner-terminal-copy`, **72 commits behind `origin/main`** (it had been 45 behind at the previous incident — it drifts monotonically, it does not self-heal). A session that landed a fix on `origin/main` and spawned a test runner to verify it got a binary that did not contain the fix, with no warning. **A fix that is merely ABSENT reads as REGRESSED** — that misdiagnosis burned a full `/manual-test-loop` iteration.

To build something else, pass **exactly one** provenance selector:

| Param | Builds | Provenance class | Notes |
|-------|--------|------------------|-------|
| *(none)* | Managed detached worktree at `origin/main` | `origin_main` (vouched) | **The default.** `source: "origin_main"`. Requires `rebuild: true` (without it no build happens and the exe comes from a slot / the LKG). |
| `git_ref` | A managed detached worktree at the ref (branch/tag/SHA) | `origin_main` if the ref IS canonical `origin/main`, else `override` | Supervisor materializes `<ws>/.spawn-<ref>/` with a pinned `origin/main` schemas sibling. `source: "worktree"`. Requires `rebuild: true`. |
| `worktree_path` | An **existing caller-owned** checkout at that absolute path | `override` | Built in place, **never mutated, never cleaned up** (the caller owns it). `source: "worktree_path"`. Requires `rebuild: true`. |
| `from_working_tree: true` | The **shared live working checkout** | `live_tree` | **Explicit opt-in only.** Use when you deliberately want to test uncommitted edits in the shared checkout. `source: "live_tree"`. Requires `rebuild: true`. |

**Classification is by what was COMPILED, not by how the request was spelled.** An explicit `git_ref: "origin/main"` is classified `origin_main` exactly like the default, because the binary genuinely is merged truth. A **local** `main` is deliberately NOT canonical — it can lag `origin/main` or carry unpushed commits, so vouching for it would reopen the hole. Only `origin/main` and `refs/remotes/origin/main` count (`git_provenance::is_canonical_main_ref`).

**This also fixes LKG hygiene.** `origin_main` is a *vouched* source, so a default spawn-test rebuild is LKG-promotable. Previously LKG was advanced by `live_tree` builds — i.e. by whatever branch the shared checkout happened to be parked on, which is how `use_lkg: true` came to ship binaries built from feature branches.

**Rejected aliases (400, not silently ignored).** `branch`, `worktree`, and `ref` are **not** spawn-test fields. Passing any of them returns `400` naming the correct field. Always check the response `source` field (`"origin_main" | "worktree" | "worktree_path" | "live_tree"`) to confirm what you actually got.

**Mutual exclusion.** Setting more than one of `git_ref` / `worktree_path` / `from_working_tree` → `400 provenance_conflict` naming all of them. Any selector without `rebuild: true` → `400 <field> requires rebuild:true` (provenance is never inferred — without a recompile you'd get an existing exe while believing you got your tree).

**Staleness is loud now, and commit-based.** On a `rebuild: true` spawn the response carries:

- `build_sha` — the full 40-char SHA the binary was compiled from. This is the field that settles provenance definitively: `git merge-base --is-ancestor <fix-sha> <build_sha>`.
- `build_source_default` — `true` when the supervisor picked `origin/main` for you.
- `build_source_warning` — `null` when the compiled tree IS current merged truth; otherwise an object with `behind_count`, `diverged`, `origin_main_sha` and a message naming the remedy. With the default flipped this is unreachable in the normal case; it fires for `from_working_tree: true` and for an explicit stale `git_ref`.

**`worktree_path` validation (each a precise 400):**
- path must exist and be a directory;
- must NOT be the live runner tree (`worktree_path_is_live_tree`) — this param can never touch the live tree;
- must contain `src-tauri/Cargo.toml` (`not_a_runner_worktree`);
- must have a `../qontinui-schemas/rust/Cargo.toml` **sibling** (one level up from the worktree root, because the runner's path-deps are `../../qontinui-schemas/rust` relative to `src-tauri/`) → `path_deps_unresolved`.

The response echoes `schemas_path`, `schemas_sha`, and `schemas_is_shared` so you can see whether the build resolved its schemas path-dep against the **shared** `qontinui-root/qontinui-schemas` checkout (drift hazard — a peer may park it on a WIP branch) versus a pinned one. `git_ref_resolved_sha`/`_short` carry the worktree's HEAD when it is a git checkout.

**`frontend_only` fast path.** Requires a provenance selector (`git_ref` or `worktree_path`) — a `frontend_only` build of the live tree is refused (it would touch the shared tree). When true it **forces** a fresh `pnpm run build` in the isolated tree (re-embedding a TS change made after the tree's last build, which the default dist-present idempotency gate would otherwise skip), while skipping `pnpm install` **only when the installed deps are provably fresh** (see "Dep-install freshness gate" below).

**Dep-install freshness gate.** A `.spawn-<ref>` container is REUSED across refs — `prepare_worktree` force-resets the same dir to the new ref but does not touch `node_modules/`. The install is therefore **freshness-gated, not presence-gated**: `prebuild_worktree_frontend` hashes the dep-governing manifests (`pnpm-lock.yaml`, `package.json`, `pnpm-workspace.yaml` — the runner is a pnpm workspace; there is no `package-lock.json` and pnpm never reads one) and compares against a sidecar at `<worktree>/node_modules/.qontinui-supervisor-dep-hash` written after the last successful `pnpm install`. Reinstall fires when the marker is absent, the sidecar is absent, or the hash differs. Every failure mode degrades toward *reinstall* (slow, correct), never toward a stale skip. Without this, a container reused across a dep bump compiles the new ref's TypeScript against the **old ref's** `node_modules` — the 2026-07-12 P0, where a stale `@qontinui/navigation@0.1.5` against a `^0.2.0` pin produced a phantom `TS2339` that looked exactly like a red `origin/main` (runner `origin/main` built clean).

**Frontend build failures carry the compiler error.** `tsc`/`vite` print diagnostics (`error TS####`) to **stdout**; stderr usually holds only pnpm harness noise and is often empty. The prebuild captures **both** streams, embeds the tail in the returned error, and records it on the slot via the same plumbing a cargo failure uses (`<slot>/last-build.stderr` → the submission's `stderr_tail`; `last_build_stderr_capture` → `SlotHistory::last_error_detail` / `last_error_log`) — so a failed spawn-test never comes back with an empty error body.

**`frontend_only` still runs cargo.** A Tauri binary embeds `dist/` at `cargo build` time (`rust-embed`), so a fresh `dist/` is only picked up by recompiling. "Fast" means **don't re-fetch / don't reinstall unchanged node_modules / don't touch the live tree** — NOT "skip cargo." If you combine `frontend_only` with `use_lkg`/`allow_stale_fallback` and the spawned exe comes from LKG/stale reuse, the response carries `frontend_only_warning`: the reused exe embeds the OLD dist.

**`build_id`.** Every spawn-test response (sync 200 body and async poll body) carries `build_id` = the build submission id. It correlates with `GET /build/{build_id}/status`. Both the synchronous and `async: true` paths drive the same build-submissions state machine.

**`GET /build/{id}/status` reports the build's ACTUAL source root.** `worktree_path` on the submission is the cargo source root the build compiled — the `.spawn-<ref>/qontinui-runner/src-tauri` container for a `git_ref` build, `<caller-checkout>/src-tauri` for a `worktree_path` build, `project_dir` for a live-tree build — and `source` labels it with the same `worktree` / `worktree_path` / `live_tree` vocabulary the spawn-test response uses. It previously always reported the supervisor's live `project_dir` regardless of provenance, which made a worktree-spawned build look like it came from the live tree. Set before the build starts, so it is honest while the build is still in flight.

**Operational: the running supervisor binary must be rebuilt to carry newly-merged spawn code.** The motivating incident hit a supervisor binary that predated the merged `git_ref` support, so the param was unknown and silently dropped. After merging any spawn-test change, rebuild + restart the supervisor (`cargo build` then relaunch) before relying on the new behavior — the supervisor does not hot-reload its own code.

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
# 3. Also build the qontinui-shim sidecar into the same slot (seconds on the
#    warm target dir). The runner materializes terminal identity shims from
#    the stub next to its own exe; skipping this deploys a stale stub.
CARGO_TARGET_DIR=../target-pool/slot-0 \
    cargo build --bin qontinui-shim --features custom-protocol
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

Every runner start copies the resolved source exe to `target/debug/qontinui-runner-{id}.exe` so the build artifact is never locked by a running process. The `qontinui-shim.exe` sidecar rides along on every start: the supervisor builds it into the same slot right after the runner build (fail-open), preserves it in the LKG dir, and copies it from next to the source exe to next to the exe copy (`deploy_shim_sidecar` in `src/process/manager.rs`). The runner materializes terminal identity shims from the stub next to its own exe (`current_exe().parent()`), so a missing/stale sidecar breaks pane claude launches — a failed shim build/copy logs a WARN ("identity shims will be stale") but never fails the build or start.

### Last-known-good (LKG) fallback for agents

The supervisor preserves the most recently successfully-built runner exe at `target-pool/lkg/qontinui-runner.exe` after every successful **vouched** (`live_tree` or `origin_main`) `cargo build`. Slot dirs can be clobbered by a subsequent failed build that overwrites or partially-deletes the slot's exe; the LKG copy is independent and survives those events. A sidecar at `target-pool/lkg/lkg.json` records `{built_at, source_slot, exe_size, sha, source}` and is hydrated into `state.build_pool.last_known_good` at supervisor startup so it survives restarts.

**Override builds are never promoted to LKG; vouched builds are.** A `spawn-test {git_ref}` / `{worktree_path}` build of a foreign tree carries `provenance.source == override`; `update_lkg_after_success` skips LKG promotion for it entirely (the slot's exe + provenance sidecar are still written — only LKG is gated) and logs `skipping LKG promotion (override build of <path>)`. This is the root fix for the 2026-06-05 incident where a branch exe became LKG and a restart deployed it to the primary. The gate is `BuildSource::is_vouched()` — it promotes BOTH `live_tree` AND `origin_main` (the default primary rebuild path, which materializes an `origin/main` worktree — see "Primary rebuild builds origin/main by default" below) and skips only `override`. So `lkg.json` records `source: "live_tree"` **or** `source: "origin_main"` (taken from `provenance.source`, never hard-coded). `sha` is the git SHA of the built tree (the `origin/main` resolved sha for an `origin_main` build; the live tree's HEAD for a `live_tree` build; `null` if the git probe failed). Legacy `lkg.json` files predating these fields still hydrate: missing `sha` → `null`, missing `source` → `live_tree`.

**When this matters.** Multiple concurrent agents share the build pool. Agent A's broken build can leave the slots in a state where Agent B's `spawn-test {rebuild: false}` would either fail or run a worse binary than the LKG. If Agent B's own changes are *already in the LKG* (because Agent B's edits predate the most recent successful build), Agent B can pin to the LKG instead of waiting for the slots to recover.

**The comparison rule — agents MUST do this themselves; the supervisor does not enforce it.**

1. Read LKG metadata from `GET /health` → `build.lkg.built_at` (RFC3339), or `GET /builds` → `lkg.built_at`. Both surface the same value.
2. Take the maximum mtime across every file you've changed in the runner workspace (`stat -c %Y` on Linux, `(Get-Item path).LastWriteTime` in PowerShell, etc.).
3. Compare:
   - **`max(mtime of changed files) <= lkg.built_at`** → the LKG was built AFTER your changes, so those changes are already compiled into the LKG binary. Safe to spawn with `{rebuild: false, use_lkg: true}`.
   - **`max(mtime of changed files) > lkg.built_at`** → the LKG predates your changes. Pinning to it would silently run stale code. You must rebuild instead.
4. If you have NO uncommitted changes, the LKG always covers you (any clean checkout's tracked files have mtimes from the original git checkout, which is older than every build).

**Why timestamps for UNCOMMITTED edits.** For bytes you edited in a working tree and never committed, there is no sha to compare — mtime answers "do the bytes I edited live in this binary" directly. The risk case (agent edits file at T1, LKG at T0 < T1) is exactly the case where the comparison correctly says "do not use LKG."

**But mtime CANNOT answer "does this binary contain commit X" — use `commit_provenance`.** `git checkout` rewrites mtimes wholesale, so a binary built from a branch parked 72 commits behind `origin/main` has perfectly fresh mtimes while missing all 72 commits. That is precisely how a landed fix came to read as a regression. `GET /lkg/coverage` therefore also returns a **commit-based** block:

```bash
# What commit is the LKG built from, and is it merged truth?
curl -s 'localhost:9875/lkg/coverage' | jq '.data.commit_provenance'
# → {"lkg_sha": "…", "lkg_source": "origin_main",
#    "behind_origin_main": 0, "is_ancestor_of_origin_main": true,
#    "contains_query": null, "contains": null}

# Definitive: is MY fix in the LKG binary?
curl -s 'localhost:9875/lkg/coverage?contains=<fix-sha>' | jq '.data.commit_provenance.contains'
# → true  = the commit IS in the binary
# → false = provably absent
# → null  = NOT COMPUTABLE (no LKG sha, or a sha unknown to the local object db).
#           null must NEVER be read as false.
```

`contains` is `git merge-base --is-ancestor <contains> <lkg_sha>` computed server-side. Prefer it over `file_newer_than_lkg_secs` whenever the question is about a commit rather than an uncommitted edit.

**API.**

```bash
# Inspect LKG state
curl localhost:9875/health   | jq '.build.lkg'
curl localhost:9875/builds   | jq '.lkg'
# → {"built_at": "2026-04-26T15:30:00Z", "source_slot": 1, "exe_size": 253749760, "sha": "a1b2c3d4e5f6", "source": "live_tree"}
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
- **Queue timeout:** configurable via `queue_timeout_secs`. **It bounds the WHOLE build (cargo permit wait + frontend `pnpm run build` + cargo compile), not just slot acquisition.** A spawn can therefore time out while `GET /builds` shows free cargo permits, because the frontend build is serialized behind `npm_lock` and a concurrent `pnpm run build` (this supervisor's other slots, or an EXTERNAL build on a multi-agent machine) can hold that lock with all cargo slots idle. The timeout message now NAMES the blocked phase — "waited Ns, blocked on the frontend (pnpm) lock with M cargo permits free" vs "waited Ns for a cargo build slot" — so the error itself tells you whether it was slot exhaustion or frontend serialization. `GET /builds` surfaces the same contention via `npm_lock_held` (bool, best-effort sample) and `npm_lock_waiters` (count of spawns blocked on the frontend lock); free `available_permits` with `npm_lock_held: true` does NOT guarantee a prompt start. A spawn waiting >60s on the frontend lock while cargo permits are free emits a `tracing::warn!` in supervisor.log.
- **Wait timeout:** configurable via `wait_timeout_secs` (default 120s). Only applies when `wait: true`. Returns successfully even if the runner doesn't become healthy — `status` field will say `"timeout"`.
- **No-wait mode:** pass `X-Queue-Mode: no-wait` header for immediate 503 with queue info instead of blocking.

If the build fails, the placeholder port reservation is cleaned up and the error is returned.

**FIXED (2026-07-22): spawn-test `rebuild: true` no longer compiles the shared working checkout.** It builds a supervisor-owned `origin/main` worktree — see "Build provenance: spawn-test builds `origin/main` by DEFAULT" above. The old hazard (the supervisor didn't fetch, didn't compare against any expected ref, and silently produced a binary from whichever feature branch a peer session had `git switch`ed the shared checkout to) is structurally gone: the build source is no longer the shared checkout at all, so no amount of branch-parking by a peer can reach it.

The old workaround — "build locally in a worktree off `origin/main` and `cp` the exe into `target-pool/slot-N/debug/`" — is **obsolete**; that is now exactly what the default does. **Do not `git switch` the shared checkout to work around a stale spawn**; it touches state another agent's session may be using, and there is nothing left to work around.

Verification surfaces (still the right thing to check before drawing conclusions from a spawned runner):

1. **`build_sha` on the spawn-test response** — the full 40-char SHA the binary was compiled from. Settle containment exactly with `git merge-base --is-ancestor <fix-sha> <build_sha>` rather than reasoning from timestamps.
2. **`build_source` / `source`** — `origin_main` for the default. Anything else means you (or a stale caller) asked for a different tree.
3. **`build_source_warning`** — non-null only when the compiled tree is behind or diverged from `origin/main`, with the behind-count and the remedy.
4. **`GET /runners`** — every runner carries `build_sha` / `build_source` / `build_source_root` / `build_built_at` for the exe it is actually running. (`stale_binary`, next to it, is an **mtime** comparison and cannot see commit staleness — prefer `build_sha`.)

### Primary rebuild builds origin/main by default

A **primary** rebuild-restart (`POST /runner/restart {rebuild: true}` → detached `manager::restart_runner`) does **not** compile the contested working checkout by default. It materializes a fresh `origin/main` worktree via `spawn_worktree::prepare_worktree(project_dir, "origin/main")` — which fetches origin itself and pins the `qontinui-schemas` sibling to `origin/main` — and compiles that worktree's `src-tauri`. The resulting build is provenance-classified `origin_main` (a third `BuildSource` alongside `live_tree` and `override`): it is LKG-eligible and allowed to start as the primary, so the primary always runs latest-green-main and the 2026-06-07 silent-stale-build incident cannot recur. A `log::info!` (and a `Build` log entry) names the chosen source + resolved sha before the build so the next operator restart self-documents which commit the primary will run.

- **Escape hatch:** `POST /runner/restart {rebuild: true, from_working_tree: true}` reverts to the legacy behavior — compile the live working tree (`project_dir`, provenance `live_tree`) — for the rare case the operator deliberately wants the primary to run uncommitted local changes. **The canonical WIP-test path is a temp runner via spawn-test, not the primary.**
- **Scope:** only the **primary** rebuild path applies this origin/main policy. `/runners/{id}/restart {rebuild: true}` for named/temp runners keeps the legacy live-tree build, and spawn-test is unchanged (see the spawn-test hazard above).
- **Provenance signal:** the build path can't tell a primary origin/main build from a spawn-test `git_ref` override by path alone (both arrive as `Some(src_tauri)`). The caller threads an explicit `BuildSourceKind` (`LiveTree` / `OriginMain { resolved_sha }` / `Override`) into `run_cargo_build_with_dir` → `compute_build_provenance`; the kind alone decides the recorded `BuildSource` (and, for `OriginMain`, the recorded sha = `prepare_worktree`'s `resolved_sha`).

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
| Stopped-runners cache cap | 1000 entries (override: `QONTINUI_SUPERVISOR_STOPPED_CACHE_CAP`, clamped [100, 100000]) |
| Stopped-runners cache TTL | 3600s / 60min (override: `QONTINUI_SUPERVISOR_STOPPED_CACHE_TTL_SECS`, clamped [60, 86400]) |
| Build pool size | 3 (override: `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`) |
| Temp runner port range | 9877-9899 |
| First-healthy watchdog budget | 90s (override: `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS`); poll interval 3s |
| Crash-restart backoff ladder | 5s → 30s → 120s; max 3 auto-restarts per rolling 30min, then disarm (kill-switch: `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1`) |

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
