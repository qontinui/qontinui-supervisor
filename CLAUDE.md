# Qontinui Supervisor

Rust-based build server and fleet dashboard for qontinui-runner. Provides parallel cargo builds, temp/named runner spawning, runner lifecycle management, and a React SPA dashboard.

## CRITICAL: Runner Lifecycle Scope

The supervisor manages lifecycle for **temp runners** (`test-*`) and **named runners** (`named-*`). The primary runner and any user-started runners are **user-managed** — the supervisor tracks their health but never starts, stops, or restarts them unprompted.

With `--auto-start` / `--watchdog` the supervisor starts the **primary** once at boot (through the same `start_runner_by_id` funnel an operator `POST /runners/primary/start` uses, so the provenance start gate applies); it never auto-starts named/temp/external runners. If the startup orphan scan already adopted a surviving primary, boot-start is a no-op (`main::primary_to_boot_start`).

**Supervisor *exit* used to be the one path that took every runner with it — it explicitly no longer does.** Every spawned runner was assigned to a single Win32 JobObject carrying `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so when the supervisor process ended for any reason (graceful `/supervisor/shutdown`, `POST /supervisor/restart`, `Stop-Process`, panic, BSOD) the kernel silently terminated every assigned process. On 2026-07-27 a routine `restart-supervisor.ps1 -Build` therefore destroyed the operator's primary runner, ~80 `claude.exe` processes and 287 PTYs — with **no log line**, because nothing "stopped" it. Since 2026-07-28 the job holds **temp runners only** (`process::job::should_assign_to_ephemeral_job`, an `is_temp()` allowlist applied at the single assignment site in `start_managed_runner`):

- **Temp runners** are supervisor-owned and still die with the supervisor — deliberately, since orphaned temps hold open slot binaries and break subsequent cargo builds.
- **Primary, named, and external runners** are user-owned, are assigned to no job at all, and survive every supervisor exit path. A non-assignment is logged (`Runner 'Primary' (PID N) NOT assigned to the kill-on-exit JobObject`) so the next incident has forensics.
- The **startup orphan scan** (`process::orphan_scan`) then adopts the survivor back into the registry. It no longer kills an adoptable orphan for having a stale binary — staleness is reported as a `Warn` naming the gap and the remedy (`POST /runner/restart {"rebuild": true}`), and the operator decides.
- **Known gap:** an adopted runner has no `tokio::process::Child` handle (we did not spawn it), so the crash-only watchdog's **exit observation** does not cover an inherited primary until this supervisor starts it itself. HTTP health polling is unaffected. A `Child` is deliberately not synthesized.
- **Stopping an adopted runner does NOT depend on that handle, and no longer depends on the registry PID either** (fixed 2026-07-31). `stop_runner_by_id` resolves its target up front through `process::stop_ledger::resolve_stop_target`, in order: the registry PID → the netstat listener probe on the runner's port → **image-path identity** (sysinfo: which live process is running this runner's deterministic `runner_exe_copy_path`). The third source needs no subprocess, no listening socket and no locale, so a stop still has a target when the first two are blind — which is exactly how `POST /runners/primary/stop` came to return 500 while PID 8872 sat alive on port 9876. An **ambiguous** exe match (two processes on the same image) is reported as unresolved rather than guessed at. Resolution is identification-only: the recovered PID goes through the normal `request_drain` → close-request → kill ladder instead of being hard-killed on the spot, so an adopted primary still gets its drain.

**A failed stop reports what it ACTUALLY did.** The old message asserted a fixed "after PID kill, tree-kill, kill-by-port" regardless of whether any of those ran. The ladder now records each rung (`process::stop_ledger::StopLedger`) and derives the message from that record, with two distinct arms: a kill **ran and was refused** (a genuinely stubborn process) versus **no kill ever ran** ("the supervisor could not identify what to kill … NOT a process that resisted being killed"). The second arm names each rung's gap. Treat that wording as load-bearing — it is the difference between hunting an unkillable process and looking at the supervisor.

**Crash-only ambient watchdog** (plan `2026-07-03-primary-runner-crash-resilience`, Phase 1). Under `--watchdog`, a supervisor-spawned runner whose process **crashes** (exits non-zero / dies unexpectedly) is auto-restarted through the same `start_runner_by_id` funnel (provenance start gate applies). Hard rules:

- **Never restarts a *running* runner** — this is exit-observation only, not health-based resurrection.
- **Never restarts on operator stop.** Every operator-facing stop path latches `stop_requested` before the kill; the exit monitor reads it when the exit is observed. The flag is cleared on the next *start* (not at stop completion), so the marker is race-free — a failed stop (`StillHeld`) whose process dies later still counts as operator-intended.
- **Never restarts a clean exit** (code 0 — window close, internal shutdown).
- **Never restarts external/user-started runners** — restart requires the spawn provenance of a supervisor-held Child handle.
- **Scope: primary only by default.** Under `--watchdog` the primary's per-runner `WatchdogState.enabled` defaults true; named/temp/external default false. Arm any runner explicitly via `POST /runners/{id}/watchdog {"enabled": true}`.
- **Crash-loop guard:** exponential backoff 5s → 30s → 120s between attempts; max 3 auto-restarts per rolling 30 minutes, then the watchdog disarms itself (`disabled_reason: "crash loop — operator required"`, `enabled` left true so intent stays visible) with an ERROR log + diagnostics event. Reset via `POST /runners/{id}/watchdog {"enabled": true, "reset_attempts": true}`.
- **Kill-switch:** env `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1` disables all crash auto-restarts without a rebuild.
- **Observability:** live counters (`enabled`, `restart_attempts`, `last_restart_at`, `crash_count`, `disabled_reason`) on `GET /runners` (per runner), `GET /health` (top-level = primary's; per-runner in `runners[]`), and the SSE health stream. A runner that is alive but not answering is reported as `liveness.state: "wedged"` rather than as healthy, on all three — see "Liveness".

- **Temp runners** (`test-*`): Spawned via `POST /runners/spawn-test`, auto-cleaned on stop. Run with a visible Tauri window and an isolated WebView2 profile. The UI Bridge is fully functional on temp runners. They are also the **only** kind subject to a max-age bound — see "Temp runner isolation and max age" below.
- **Named runners** (`named-*`): Spawned via `POST /runners/spawn-named`, persistent across supervisor restarts. Saved to settings. Not auto-cleaned. Support start/stop/restart/protect.
- **User runners** (everything else): Started by the user with visible Tauri windows. The supervisor observes health only.

**First-healthy watchdog.** Every runner the supervisor spawns (via any of the `start_managed_runner` callers above) gets a per-spawn watchdog that polls its HTTP `/health`. If the process stays alive but never binds the API within the budget (default 90s), the supervisor kills the PID so a wedged start doesn't linger as a zombie on the port. Scope is strictly per-spawn — does not touch runners that were already up when the supervisor started. Budget override: env `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS` (seconds, must be > 0). Note: on a crash-watchdog-armed runner, the first-healthy kill reads as a crash (non-zero exit, no stop intent) — the crash-only watchdog will retry the start up to its loop-guard budget, then disarm.

## Temp runner isolation and max age

**`QONTINUI_INSTANCE_NAME` is the runner's unique id, never its port.** The
runner roots its entire `instance-<sanitized_name>` app-data tree on that string
(`qontinui-runner` `src-tauri/src/instance.rs:scope_path`) — dev logs, macros,
prompts, contexts, the Restate journal, **and `terminal-sessions.json`**. It used
to be `format!("test-{port}")`, and temp ports are recycled inside a 23-slot
range, so two sequential temps resolved to the same instance dir and the second
booted on the first's live terminal-session registry (283 inherited PTYs observed
2026-08-08; plan `2026-08-10-temp-runner-session-restore-isolation`). Plan
`2026-07-20-runner-port-keyed-state-inheritance` had moved that store off a
`-<port>` *filename* onto an instance key that was itself port-derived — the
inheritance was renamed, not removed.

`process::temp_runner_instance_name` now mints it from the
already-unique per-spawn runner id (`test-<hex-millis>-<hex-seq>`), which also
keys `instance_config_dir`, the WebView2 profile and `QONTINUI_RUNNER_ID`, so
name and id cannot drift apart again. **No second uuid is minted.** Teardown
follows automatically: all **four** removal sites (`remove_runner`,
`purge_stale_test_runners_core`, `manager::stop_runner_by_id`, and
`manager::reap_stale_test_runners` — the sweep, which can now kill a *live*
runner for age) hand `managed.config.name` to
`windows::remove_runner_app_data_dirs`, whose sanitizer
(`process::sanitize_instance_name`) mirrors the runner's and maps the id to
itself. `config::runner_exe_copy_path` stays **deliberately port-keyed** — a
per-spawn exe path re-triggers a Windows Firewall prompt on every cold spawn.

**Legacy `instance-test-<port>` trees are now permanently orphaned.** Up to 23
of them (one per port slot, with their stale `terminal-sessions.json`) exist on
machines that ran the old scheme. Nothing keys onto those names again, so
nothing reuses them — the point of the fix — and nothing removes them either.
Bounded and harmless, but it makes permanent the orphaned-file janitor that
`2026-07-20-runner-port-keyed-state-inheritance` §7 deferred.

**Temp runners have a max age; nothing else does.** Before this, a *healthy*
temp runner had no terminator at all short of supervisor exit — an unowned
(`requester_id: None`) temp was found alive with 31 live PTYs two days after it
was spawned. `process::manager::reap_stale_test_runners` now also reaps a
`RunnerKind::Temp` past `config::temp_runner_max_age()` (default **24h**,
`QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS`, `0` disables), logging the
runner's identity, its measured age, the bound and the knob at WARN **before**
killing.

- The predicate `manager::exceeds_temp_runner_max_age` is an explicit
  `kind.is_temp()` **allowlist**, exactly like
  `process::job::should_assign_to_ephemeral_job` — never `!is_primary()`.
  `RunnerKind` is `#[non_exhaustive]`; a variant added later must default to
  *not reaped*. Getting that inverted was the 2026-07-27 incident.
- **The bound lives in `manager::reap_stale_test_runners` only.** The other
  sweeper (`main::reap_stale_test_runners` →
  `routes::runners::purge_stale_test_runners_core`) stays strictly
  liveness-based (`port_alive => continue`): it has no `created_at` input, no
  kill ladder for a live process, and it also backs the operator-facing
  `POST /runners/purge-stale`, whose contract is "remove runners whose processes
  are no longer alive".
- **Age is measured from `RunnerState::started_at`, not from the spawn
  request.** `ManagedRunner::created_at` starts when `spawn_test` reserves the
  placeholder, which on a cold `spawn-test {rebuild:true}` is 40-50 min before
  the child ever runs. `manager::resolve_temp_runner_age` prefers `started_at`,
  falls back to time-since-first-seen when it is absent or in the future (clock
  skew), and the kill log names which clock it used. The configurable floor
  (3600s) sits above the measured cold-build ceiling as a second, independent
  guard against the same mistake.
- **`protected: true` does NOT exempt a temp runner from the age bound.** Every
  temp runner is created `protected: true` (it is not an operator opt-in), so
  honouring the flag here would make the bound inert for 100% of its intended
  targets — a veto keyed on a value nothing ever varies. The flag continues to
  mean what it always meant (no stop/restart by smart rebuild, watchdog, AI
  session, or loop). If a per-runner age exemption is ever wanted, it needs its
  own opt-in field, not this one. The kill log states this inline so an operator
  who clicked "protect" is not left guessing.

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
| `-l, --log-file` | Append the in-memory log buffer to this file (persistent supervisor log; size+age rotated, see "Persistent Logs"). Overrides `<log-dir>/supervisor.log`. |
| `--log-dir` | Directory for persistent log files. Writes `<log-dir>/supervisor.log` plus one `<log-dir>/<runner-id>.log` per managed runner (tees runner stdout/stderr). Directory is created on startup. Every file is size+age rotated with a retained-segment cap. |
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
Params: `-Build`, `-Port` (9875), `-ProjectDir`, `-LogFile`, `-Watchdog`,
`-ForceKillRunners`.

**Runner lifetime across a supervisor restart.** The primary, named, and
external runners **keep running** — they are user-owned and are never assigned
to the kill-on-exit JobObject (see "CRITICAL: Runner Lifecycle Scope"). **Temp
runners (`test-*`) are reaped**, deliberately: they are supervisor-owned and
would otherwise linger holding slot binaries. The new supervisor's startup
orphan scan adopts the survivors back into its registry, so `GET /runners`
shows correct `pid` / `running` within a few seconds of the restart.

The script additionally refuses to proceed with **exit 4** when a non-temp
runner is running, unless `-ForceKillRunners` is passed. Keep
`-ForceKillRunners` out of any documented "normal" invocation.

**Why that guard still matters after the code fix:** a job assignment is made
by the supervisor that *spawned* the runner, and a process cannot be removed
from a Windows job afterwards. `restart-supervisor.ps1` stops the **currently
running** supervisor — so if that instance is a binary built before
2026-07-28, its job still holds the primary and stopping it still reaps.
The code fix takes effect only for runners spawned by a supervisor that
carries it; the guard covers the deploy window (and any peer running a stale
supervisor exe).

## Verifying scoped slot cleanup

`scripts/verify-scoped-cleanup.ps1` is the end-to-end proof that the build
pool's slot cleanup is territory-scoped. It plants a long-lived `cargo check`
probe in the foreign target dir and one in **every** pool slot, submits a real
`POST /runners/spawn-test {rebuild:true, async:true}`, then **polls**
`GET /build/{id}/status` and `GET /diagnostics?filter=build_kill` every 10s,
accumulating events into a run-local de-duplicated set, and asserts: the foreign
build survives and is recorded as `spared`; the probe in the slot the build
claimed is reaped with `matched_by: "env"`; the probes in the **other** slots are
untouched.

The `build_kill` filter category carries five event kinds:
`build_process_killed` and `build_kill_failed` (per-process, from the slot
sweep), `build_cleanup_summary` (one per consequential pass), and
`exe_lock_holder_killed` / `exe_lock_kill_failed` (from `free_slot_exe`'s
exe-lock path). The two `*_failed` kinds are the ones worth alerting on: a kill
that was *refused* (taskkill ran and the process survived) used to leave only a
`debug!` line, so a build would fail on a locked artifact with no queryable
trace — the exact blind spot this surface exists to remove. The harness ignores
kinds it does not assert on, so the set is additive.

The `spared` assertion compares **counts** against the number of probes planted
out of territory, and uses `build_cleanup_summary.spared_pids` only to
*strengthen* the evidence when it happens to name the foreign probe's pid.
Asserting containment would flake: `spared_pids` is a sample capped at
`PID_LIST_CAP = 5`, so on a box with ~9 concurrent sessions our probe can
legitimately fall outside it while being perfectly spared. The gap is
deliberate — see `Get-SparedPidAttribution`.

It polls rather than issuing one blocking call because
`DIAGNOSTICS_BUFFER_SIZE` (500) is shared across **all** event kinds and
filtering happens at read time: the cleanup pass emits at the *start* of a build
that then runs for 10–30 minutes, so a single read at the end can find its
events already evicted by unrelated `BuildStarted`/`Restart*` traffic — and the
eviction is invisible, because the route computes `total` after filter+limit, so
a filtered read just returns a small array. An absence-check would then report
PASS from an empty ring.

**Attribution gates every PASS, not just the FAILs.** A `build_cleanup_summary`
naming one of our slot territories is *not* evidence that **our** build ran a
pass — on this box a peer's `spawn-test` emits one routinely. So the four rows
whose evidence is an **absence** (V1-1 our probe is still alive, V1-2 no kill
event names it, V2-3 the siblings are still alive, V2-4 nobody was killed
cross-slot) gate their PASS arm on `Test-AttributableCleanupPass`: `build_slot_id`
came back **and** a summary for that claimed slot is in the bag. Missing either →
INCONCLUSIVE, with the detail naming *which* ("no pass ran at all" vs "a pass ran
but it was a peer's"). Otherwise a pass that never examined our probes would
report PASS — the same vacuity `sawAnyCleanupPass` was introduced to fix. **FAIL
arms are deliberately not gated:** a wrongful kill is a defect whoever's pass
emitted it, and V2-4 must still catch a machine-wide kill when the claimed slot
is unidentifiable. V1-3 is the one ungated row on purpose — its evidence is a
*positive* event from the code under test (a pass reporting it spared N
out-of-territory processes), so a peer's pass is still real evidence; its detail
names whose pass accounted for it.

It plants one probe per slot because slot selection is dynamic. The
**authoritative** claimed slot is the spawn-test response's `build_slot_id`
(from `BuildAttempt.slot_id`); the adjacent `build_result.slot_id` is *not* a
substitute — it is `last_successful_slot` read after the build and can name a
slot a peer finished into. When `build_slot_id` is absent the harness falls back
to inferring from the `build_cleanup_summary` events, and only from one that
actually killed something: a lone `killed: 0` summary is a peer's pass, not
ours. A claim it cannot attribute to its own build is reported INCONCLUSIVE,
never FAIL.

```powershell
# from the repo root
.\scripts\verify-scoped-cleanup.ps1 -DryRun   # connectivity + slot discovery only
.\scripts\verify-scoped-cleanup.ps1           # the real run
.\scripts\verify-scoped-cleanup.ps1 -Cleanup  # sweep leftovers after a hard kill
```

**A failed submission does not hold the pool for 40 minutes.** If the
`POST /runners/spawn-test` never yields a submission id (503 `build_pool_full`,
a 400, a supervisor restart mid-call, or a 2xx carrying neither `submission_id`
nor `build_id`) there is nothing to poll to a terminal state — `build_state`
stays `unsubmitted` and the terminal-state `break` is unreachable — so the loop
would otherwise run out the whole `-BuildTimeoutSec` while its probes hold the
cargo build lock on every pool slot **and** `-ForeignTargetDir`, stalling every
peer's `spawn-test`. `Get-PollDeadline` bounds that case to two poll intervals:
the harness names the submission failure, records a `SUBMIT` INCONCLUSIVE
assertion, tears down, and exits 3. The submitted case is unchanged.

**Runtime 10–30 minutes** — that is the compile itself; every HTTP call is now
short. `-BuildTimeoutSec` (default 2400) bounds the poll loop, and
`-ProbeLifetimeSecs` auto-derives to `-BuildTimeoutSec + 600` so a probe can
never expire before the telemetry is read. `-SkipV1` (foreign-survives half) and
`-SkipV2` (reap + sibling-isolation half) re-run one half alone after a failure.
Exit 0 = all assertions passed, 2 = a real scoping defect, 3 = inconclusive (no
cleanup pass ran, a pass ran but was a peer's and so is not attributable to our
build, the spawn-test submission failed, or a probe exited early — re-run),
1 = the run could not be performed.

**A leaked temp runner also exits 3, and says so.** Teardown runs in `finally`,
i.e. after the assertion table is printed and after the exit code is decided, so
a teardown failure used to be structurally invisible: on 2026-08-09 the harness
left a temp runner running and still printed "all assertions PASSED" and exited
0 (the cause was a 415 on the stop call, fixed client-side in #138 and
server-side in `OptionalJson` — see "Optional request bodies"). Teardown now
**proves the stop by re-reading `GET /runners`** rather than trusting the 200,
records each survivor on a ledger, prints a `TEARDOWN LEAKED` block naming the
exact stop command per runner, and escalates 0 → 3. Escalation only: a FAILED
assertion (2) or an aborted run (1) outranks a leak and is never demoted. Three
leak classes are distinguished — still listed after the stop; the confirming read
failed so the outcome is **UNKNOWN**; or the runner never appeared while the
build never reached a terminal state, so the supervisor may still spawn it after
the script exits.

Teardown also stops **the runner id the spawn response named** (`id` on the
async 202, assigned at port reservation before the build), not just the
snapshot diff. A failed pre-spawn baseline `GET /runners` therefore no longer
disables runner teardown entirely — it only disables the diff, which stays
unsafe without a baseline because every peer's runner would look new.

Probes are launched as the **real toolchain `cargo.exe`** resolved via `rustup
which cargo`, never `~\.cargo\bin\cargo.exe` — that is a 0-byte symlink to
`rustup.exe`, and the proxy does not `exec`, so launching it yields a process
named `rustup.exe` that neither the harness's liveness check nor the reaper
(which enumerates only `cargo.exe`/`rustc.exe`) will ever name. The script
asserts the spawned process image is `cargo.exe` and aborts loudly if not.

**Blast radius.** While it runs, every pool slot **and** `-ForeignTargetDir`
(default: the supervisor's own shared `target\`) is lock-held by a probe, so a
peer's `spawn-test` or `cargo build` blocks behind it; `POST /diagnostics/clear`
is global state. Ctrl-C is far safer than it was — the longest uninterruptible
window is now one 10s poll sleep rather than a 40-minute blocking call, so
`finally` runs and teardown happens. Every plant is still recorded in
`%TEMP%\qontinui-verify-scoped-cleanup\last-run.json` as it happens; run
`-Cleanup` to sweep a leak after a hard kill (probes also self-expire after
`-ProbeLifetimeSecs`). `-Cleanup` will **not** kill a recorded pid whose
`StartTimeIso` is missing — an unverifiable identity could be a peer's build
that inherited the pid. That posture applies to **in-session teardown** too, so
a probe whose `Process.StartTime` was unreadable at plant time is left running
until it self-expires; teardown now reports how many pids it had to leave
behind (and what is running on them) instead of leaking them silently.

**Preflight discovery has no defaults.** The runner repo comes from `GET
/health` → `supervisor.project_dir` and the pool size from `GET /builds` →
`pool_size` (retried, because that route flakes); if neither answers, the run
**aborts** rather than assuming one, and every run prints the provenance of both
(`source: ...`). A guessed pool size that is too low silently narrows V2-3 to
fewer slots than the pool really has, so the run reports PASS having never
looked at the slot where a cross-slot kill would show. Declare them with
`-RunnerRepo` / `-PoolSize` (or `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`) when the
supervisor cannot.

That route flakes because it *used* to run `git fetch` inline — see
"`/builds` must not run git" below, which removes the cause the retry works
around.

Pure decision helpers are unit-tested in
`scripts/verify-scoped-cleanup.Tests.ps1` (`Invoke-Pester`). The harness is
Windows-only and can never run on the `ubuntu-latest` gate, so everything it
string-matches is pinned by CI-enforced Rust tests: `"env"` / `"sysinfo"` and
the `KilledProcess` fields in `src/process/slot_territory.rs`; the
`{timestamp, kind, data}` envelope plus `slot_id` / `territory` / the
`build_kill` filter category in `src/diagnostics.rs`; and `pool_size` on the
`GET /builds` body in `src/routes/runners.rs`
(`list_builds_emits_the_pool_size_the_harness_refuses_to_run_without`) — that
one became a **hard** dependency when the fallback was deleted, since a rename
now aborts every run at preflight while the Rust suite stays green.

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

**Rotation is built in** (`log_capture::RotatingLogFile`). Every capture file
— `supervisor.log` and each per-runner `<runner-id>.log` — rolls over to
`<stem>.<YYYYMMDDTHHMMSSmmmZ>-<nnn>.<ext>` before it would exceed **64 MiB**, or
once the live segment has been open **24h**, whichever comes first; the **5**
newest rolled-over segments are retained and older ones are deleted. That is a
~384 MiB ceiling per log. Both parts of the rotated name are fixed-width so a
lexicographic sort is a chronological sort — which is what makes pruning delete
the OLDEST segment rather than an arbitrary one.

**Why it is in-process and not left to logrotate.** It wasn't bounded at all,
and `.dev-logs/primary.log` was measured at **1.85 GB** on 2026-08-30 after a
wedged runner spent ~14h flooding it. "Rotate externally" is advice nothing on
this fleet was following, on a box where every build slot shares the disk.

| Knob | Default | Clamp |
|---|---|---|
| `QONTINUI_SUPERVISOR_LOG_MAX_BYTES` | 67108864 (64 MiB) | [1 MiB, 8 GiB] |
| `QONTINUI_SUPERVISOR_LOG_MAX_AGE_SECS` | 86400 (24h) | [60, 2592000] |
| `QONTINUI_SUPERVISOR_LOG_MAX_RETAINED` | 5 | [1, 100] |

There is deliberately **no** value that disables rotation — unbounded growth is
the defect, not a supported configuration.

Details that are load-bearing rather than incidental:

- **An already-oversized file rolls on the first line written.** `written` is
  seeded from the file's length at open, so a restart onto a 1.85 GB file does
  not append to it for another day.
- **Rotation renames, never truncates**, and the age clock is a monotonic
  `Instant` taken when THIS process opened the file — so a wall-clock
  correction cannot make a segment immortal or roll every line. A supervisor
  restart starts a fresh age window; the size bound is what caps disk.
- **An empty segment never rolls**, or an expired-but-idle log would mint one
  empty file per line and evict the real history through the retained cap.
- **A line longer than `max_bytes` is written whole.** Truncating a log line
  corrupts the record to honour a bound whose purpose is disk, not tidiness.
- **Pruning only ever touches this log's own segments** (same directory, same
  `<stem>.` prefix, same `.<ext>` suffix, never the live file), so one runner's
  rotation cannot delete another's log.
- **Every failure is best-effort and reported once per segment**, on stderr —
  a `tracing::warn!` here can be captured back into the supervisor's own log
  buffer and recurse into this very writer.

Out-of-process `copytruncate`-style rotation still works alongside it (files are
opened `O_APPEND`); rename+signal style rotation does not, and is now
unnecessary.

**Best-effort:** a missing/unwritable log path logs a warning once and continues — persistent logging never blocks supervisor startup.

## API Endpoints

### Health & Dashboard

| Method | Path | Description |
|--------|------|-------------|
| GET | `/` | React SPA dashboard |
| GET | `/health` | Comprehensive status (runners, build, expo). The top-level `runner` block and every `runners[]` row carry `liveness` / `last_seen_responding_at` / `port_open` — **read `liveness`, not `running`**, see "Liveness". `supervisor.built_from_sha` is **the commit THIS supervisor binary was compiled from** — a bare lowercase-hex git sha (the full 40 characters for any build that read it from git; a 7-40 char unambiguous prefix is accepted only from a build-env override), or `null` when the build could not establish one (`null` = UNKNOWN, never "clean"). Stamped by `build.rs` at compile time, so it describes the running binary and not whatever the checkout says now. Feed it straight to `git merge-base --is-ancestor <fix-sha> <built_from_sha>` to answer "is the running supervisor newer than fix X?". `supervisor.built_from_dirty` (`true`/`false`/`null`) is a **separate** field so the sha never needs de-suffixing; `true` makes the sha a lower bound, and `null` means the dirtiness could not be measured at all — never "clean". Do NOT confuse it with the adjacent `buildId`, which is the **runner's** and is an ISO timestamp — and note the runner's own `/health` spells that same name as `<sha>-<epoch-ms>`. |
| GET | `/health/stream` | SSE stream of real-time health data |
| POST | `/supervisor/restart` | Self-restart supervisor (user-owned runners — primary, named, external — are left running; temp runners are reaped). Spawn-and-exit: the replacement process is spawned, the shutdown is latched so in-flight handlers get a window to answer, then `std::process::exit(0)` closes the kill-on-exit JobObject handle, which reaps every **assigned** (temp) runner. The latch happens only AFTER the replacement spawn succeeds — latching first would turn a failed restart into an outage. |

### Runner Management

| Method | Path | Description |
|--------|------|-------------|
| GET | `/runners` | List all runners with status. **Read `liveness`, not `running`** — see "Liveness" below. Each entry carries **commit-based build provenance** for the exe it is actually running: `build_sha` (full 40-char SHA), `build_source` (`live_tree`/`origin_main`/`override`), `build_source_root`, `build_built_at`. `null` = unknown provenance (never started by this supervisor, or a legacy artifact with no sidecar) — do NOT read it as "current". Prefer these over the adjacent `stale_binary`, which is an **mtime** comparison and is blind to commit staleness. |
| POST | `/runners` | Add a runner config to the registry |
| POST | `/runners/spawn-test` | Spawn ephemeral test runner on next free port (9877-9899). Body: `{rebuild?, use_lkg?, wait?, wait_timeout_secs?, requester_id?, queue_timeout_secs?, git_ref?, worktree_path?, from_working_tree?, frontend_only?, async?}`. **`rebuild: true` builds a supervisor-owned `origin/main` worktree by default**, NOT the shared working checkout. Returns `{id, port, api_url, ui_bridge_url, build_id, source, build_sha, build_source_default, build_source_warning}` plus `used_lkg`/`lkg` when `use_lkg: true`. See "Build provenance: spawn-test builds `origin/main` by DEFAULT" and "Last-known-good (LKG) fallback for agents" below. Auto-cleaned on stop. |
| POST | `/runners/spawn-named` | Spawn persistent named runner. Body: `{name, rebuild?, port?, wait?, wait_timeout_secs?, protected?, queue_timeout_secs?}`. Persisted to settings, NOT auto-cleaned. Name must not be empty, "primary", or start with "test-". Returns `{id, port, api_url, ui_bridge_url}`. |
| GET | `/runners/spawn-test/in-flight` | The spawn-test builds running right now, each with the `id` + `port` reserved for it plus `build_id` / `logs_url` / `stop_url`. Optional `?requester_id=` narrows to one caller. **Recovery for a lost spawn-test answer** — see "A lost spawn-test answer" below. |
| POST | `/runners/purge-stale` | Remove runners whose processes are no longer alive. Optional body `{requester_id?}` scopes the purge; a bodyless POST purges every stale test runner. |
| DELETE | `/runners/{id}` | Remove a runner from the registry |
| POST | `/runners/{id}/start` | Start a runner |
| POST | `/runners/{id}/stop` | Stop a runner. Optional body `{force?}`; a bodyless POST is fine (see "Optional request bodies" below). **`force` is the restart-readiness override** — see "The restart-readiness gate". |
| POST | `/runners/{id}/restart` | Restart a runner. Body `{rebuild?, source?, force?}`. **`force` is the restart-readiness override** — see "The restart-readiness gate". |
| POST | `/runners/{id}/protect` | Toggle protection on a runner |
| POST | `/runners/{id}/watchdog` | Control watchdog for a specific runner |
| GET | `/runners/{id}/logs` | Log history for a specific runner |
| GET | `/runners/{id}/logs/stream` | SSE log stream for a specific runner |
| GET/POST | `/runners/{id}/ui-bridge/{*path}` | Proxy UI Bridge requests to a specific runner |

### Liveness — read `liveness`, not `running`

Served on **`GET /runners`, `GET /health` (the top-level `runner` block AND
every `runners[]` row) and the SSE health stream** — the same object, the same
value, from the same refresher tick. Every row carries it as an **additive**
field beside the pre-existing `running` / `api_responding` / `pid`, which keep
their meanings and values:

```json
"liveness": { "state": "wedged", "unresponsive_since": "2026-08-30T04:11:07.918+00:00" },
"last_seen_responding_at": "2026-08-30T04:11:07.918Z",
"port_open": true
```

| `state` | Meaning |
|---|---|
| `responding` | The API answered on the most recent probe. |
| `wedged` | **The port is held and the API is silent** — the process is ALIVE and not answering. `unresponsive_since` is when it was last seen responding. |
| `stopped` | The API is silent, the port is not held, and it HAS been seen responding before. Positive evidence of absence. |
| `unknown` | No positive evidence either way — never seen responding, or the supervisor believes it spawned a process that has not bound its port yet. Never read this as "stopped". |

`unresponsive_since` is `null` for every state but `wedged`; the key is always
present, and `state` is always a string, so a consumer never branches on the
JSON type to read the verdict.

**Why this exists.** `running` is a two-state answer to a three-state question,
and it answered "healthy" for a runner that was neither. On 2026-08-30 the
primary sat alive for ~14h with 26178s of CPU, holding `:9876` and accepting
TCP connections it never replied to (3x30s, 0 bytes); the mobile "Account
Usage" widget errored and every liveness check on the box reported the runner
healthy, because they all read `running`. `RunnerState::liveness` had
classified it correctly since Phase 3b and the health refresher was already
escalating it (`RUNNER WEDGED: port is held but the HTTP API has not answered
for Ns`) — the classification simply never reached this response.

**It is not a second liveness system.** Both inputs (`port_open`,
`api_responding`) come from the same `managed.cached_health` snapshot the
health refresher wrote, so every surface, and the escalation log, cannot
disagree. A runner the refresher has not reached yet reads `unknown` — on the
SSE path a missing snapshot (first ticks after boot, a contended lock) is
`unknown` too, never healthy.

**The dashboard renders it.** The fleet table shows a red `Wedged` badge that
OVERRIDES `derived_status`, because `derived_status` is provably wrong here:
`derive_runner_status` maps "believed up, API silent" to `starting`, so a
runner wedged for 14 hours showed a blue *Starting* pill, and a wedged primary
(whose `running` is synced down to the probe) showed a grey *Offline* one for a
live process holding its port. The header line reads `WEDGED (alive, API
silent)` instead of falling through to `stopped`. Action gating treats a wedge
as UP — Stop and Restart are offered, Start is not — because the process is
alive.

**`running` is deliberately NOT redefined.** For a supervisor-managed
(`test-*` / `named-*`) runner it is latched `true` at spawn, so a wedged temp
runner reads `running: true, liveness.state: "wedged"`; for a user-managed
runner the refresher syncs it down to the probe, so the wedged primary reads
`running: false` with the same `wedged` verdict. Either way the wedge is now
visible without reinterpreting a field existing consumers depend on.

**The stamp and the escalation are not gated on ownership.** Both
`last_seen_responding_at` and the `RUNNER WEDGED` escalation used to live inside
the health refresher's `!is_supervisor_managed` guard — which exists for the
`running` sync (a latched supervisor-managed flag must never be synced to the
probe) and has nothing to do with either. The consequence was that a temp or
named runner never received a stamp, so a held port plus a silent API
classified `(true, None) => Unknown` and `wedged` was **unreachable** for the
whole class, with no escalation logged either. A supervisor-managed runner is
exactly the kind an agent spawns and then waits on. The guard now covers only
the `running` sync and the pid rules; `health_cache`'s
`the_wedge_signal_is_not_gated_on_runner_ownership` pins it by scanning the
guard's brace span, because a prose comment saying "every runner kind" cannot
notice being moved back inside one. The escalation's "not restarting" reason is
kind-aware: a supervisor-managed runner is not exempt because it is observed
only — it is exempt because a wedge is not an exit (the crash-only watchdog
does not cover it) and a restart destroys the in-flight work and the evidence.

**The escalation is keyed on the classification, not on the raw probes.** It
used to fire on `!responding && port_open`, which is BROADER than the `wedged`
verdict: with no `last_seen_responding_at` stamp that same pair classifies
`unknown`. Extending it to temp runners would therefore have reported
*"the process is ALIVE and not responding — capture a thread dump"* about
runners that had simply not finished booting, which every `spawn-test`
produces (a runner holds its port through a 30s-per-stage PG bootstrap). Both
states are still counted at 30s and re-escalated every ~5 min; they now say
what they are — `RUNNER WEDGED` at ERROR for a runner that answered before and
stopped, and a WARN naming the honest UNKNOWN (*"has held port N for Ns and has
NEVER answered its HTTP API"*) for one that never answered at all.

**One classification bug was fixed along the way.** `RunnerState::liveness`
short-circuited on `self.running`, which is only synced to the probe for
*user-managed* runners — so a wedged temp/named runner classified as
`Responding`, the exact false-healthy the enum exists to prevent. It now takes
the API-probe observation as a parameter: the bookkeeping flag says what the
supervisor *believes it started*, only the probe says what answered. Relatedly,
`stopped` is no longer claimed while the supervisor still believes it owns a
live process — that is `unknown`, since `stopped` asserts positive evidence of
absence.

**Queue behavior for spawn-test and spawn-named:**
- **Default (blocking):** If all build slots are busy, the HTTP request holds open until a slot frees. Optional `queue_timeout_secs` bounds the QUEUE wait (permit + frontend lock) and returns 504 on timeout; it stops counting once the build is doing work, so it can never 504 an in-flight compile.
- **`X-Queue-Mode: no-wait` header:** Returns immediately with **503 Service Unavailable** and body `{error: "build_pool_full", queue_position, active_builds: [...]}`.

### The restart-readiness gate

**Before the supervisor stops or restarts a runner it asks that runner whether
it is safe to**, by GETting `http://127.0.0.1:<runner-port>/restart-readiness`
(IPv4 loopback, never `localhost`). A verdict of `safe_to_restart: false`
**refuses** the operation with `409`; the long-advertised `force: true` on the
request body is the documented override. Code: `src/restart_readiness.rs`; plan
`2026-08-29-no-single-answer-to-is-it-safe-to-restart-the-runner`, Phase 3.

**What this replaced was not a weaker check — it was nothing at all.**
`manager::restart_runner_by_id` declared the parameter `_force: bool` and never
read it; `manager::stop_runner_by_id` took no force parameter; `stop_runner`'s
`_force` was dead the same way; `RebuildAndRestartRequest::force` was documented
as "currently a no-op"; and `ManagedRunner::is_protected()` was read only to
render a flag on `GET /runners`, never inside a stop/restart decision. The doc
comment on the restart handler nevertheless asserted *"Protected runners require
`force: true` in the request body."* An operator sending `force: false` got
exactly what one sending `force: true` got. The plan's "every advertised restart
protection in this fleet is inert" finding, instance 3 of 3.

**Every unknown refuses.** Unreachable, non-2xx, unparseable JSON, and JSON
missing `safe_to_restart` are all UNKNOWN — and UNKNOWN is a refusal, because
this gate is consulted precisely when the next step is destructive. `404` is
UNKNOWN too, but reported with its own message (*"THIS RUNNER'S BUILD PREDATES
the restart-readiness endpoint … This is an OLD RUNNER, not an idle one"*) so an
operator can tell an old build from a busy runner. **No error path resolves to
"safe."**

**The refusal never recommends a drain it cannot honour.** `POST /drain` acts on
the AI/task-run plane; the census that counts terminal-hosted agent sessions
explicitly exempts that plane, so a drain is a documented fast no-op for the
population most restarts destroy. With terminal sessions live the refusal says
so — *"there is NO graceful path for this plane … Do not drain and retry"* — and
names the drain as a remedy **only** when the runner itself reported a non-empty
AI plane that a drain covers. Recommending a no-op drain would manufacture the
false confidence the plan exists to destroy, carrying the runner's own
authority. Pinned by `terminal_sessions_refusal_never_recommends_a_drain`.

**Refusal bodies** are `409` with `{error, cause, runner_id, action, message,
would_be_lost: {terminal_sessions, ai_sessions}, drain, boundary, readiness,
readiness_url, override}`. `cause` is `sessions_live` |
`readiness_endpoint_absent` | `readiness_unreachable` | `readiness_error_status`
| `readiness_unparseable`. On an UNKNOWN, `would_be_lost` and `readiness` are
**`null`, not zero** — the counts were never learned.

**Three exemptions, each decided from supervisor state before any probe** (an
exemption is not an error path; a failed probe is UNKNOWN and refuses):

| Skip | Why |
|---|---|
| `temp_runner` | The supervisor's own lifecycle stops `test-*` runners constantly (failed-probe teardown, frontend-stale teardown, `purge-stale`, the max-age reaper, the build-slot exe-lock breaker). Gating those would make `spawn-test` unusable and leak a zombie on every failed spawn. Note every temp is minted `protected: true`, so a protection-keyed exemption would not work here — the same reasoning the max-age reaper already records. |
| `runner_not_protected` | `POST /runners/{id}/protect {"protected": false}` is an explicit operator statement that this runner may be stopped without ceremony. **This is the only place `is_protected()` has ever participated in a stop/restart decision.** |
| `runner_not_running` | Neither tracked-running nor API-responding: no in-flight work to destroy. Both signals must be negative, because an ADOPTED runner can sit at `running: false` while alive and its `/health` is what says so. |

**Every refusal, every override, and every allow is logged with the verdict**
that produced it — refusals at `Warn`, overrides at `Error` (the override is the
louder one: it is the line that destroys work), allows at `Info`. A gate that
recorded only what it blocked could not be audited for what it let through.

The probe timeout is **15s**, sized against the tail: `/health` on a loaded box
has been sampled between 296 ms and 10120 ms, and `/restart-readiness` computes
a fresh process-table cross-reference rather than serving the 600 s census
cache. An under-sized timeout converts load into false refusals.

### Optional request bodies — `OptionalJson`, not `Option<Json<T>>`

Three routes take a body every field of which defaults, so the body as a whole
is optional: `POST /runner/stop`, `POST /runners/{id}/stop`,
`POST /runners/purge-stale`. They use the **`OptionalJson<T>` extractor**
(`src/routes/optional_json.rs`). **An empty body is `None` whatever the
`Content-Type` says**; a non-empty body is handed to axum's `Json` verbatim, so
it keeps axum's content-type check and rejection bodies.

**Do not "simplify" these back to `Option<Json<T>>`.** Field-level
`#[serde(default)]` does not make a bodyless POST work: axum 0.8 branches on the
`Content-Type` header and never looks at whether there are bytes to parse.

| `Content-Type` | body | `Option<Json<T>>` (old) | `OptionalJson<T>` (now) |
|---|---|---|---|
| absent | empty | `None` | `None` |
| `application/json` | `{}` | `Some` | `Some` |
| `application/json` | *empty* | **400** EOF while parsing | `None` |
| `application/x-www-form-urlencoded` | *empty* | **415** | `None` |
| `application/x-www-form-urlencoded` | `{...}` | 415 | 415 (unchanged) |
| absent | `{"force":true}` | `None` — **silently dropped** | **415**, named |

The two empty-body rejections are what an ordinary client sends for a bodyless
POST: `.NET`'s `HttpWebRequest` — and so PowerShell 5.1's `Invoke-RestMethod
-Method Post` with no `-Body` — defaults to `application/x-www-form-urlencoded`,
and so does `curl -d ''`. That is not hypothetical: it is how
`scripts/verify-scoped-cleanup.ps1` came to leak the temp runner it spawned while
reporting all 9 assertions passed (2026-08-09, PR #138 fixed that one caller).

The last row is a deliberate behavior change: a body sent with no `Content-Type`
is now refused instead of vanishing. Silently dropping `force: true` on a stop
turns an operator's deliberate force-stop into a `409` restart-readiness refusal
they explicitly overrode — the same reason `spawn-test` answers 400 on a
misspelled provenance selector rather than ignoring it.

Browser `fetch(url, {method: 'POST'})` sends no `Content-Type` and no body, so
the dashboard's own stop/start buttons were never affected either way.

### Parallel Build Pool

The supervisor runs a fixed pool of **N concurrent cargo builds** (default 3, override via env `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`). Each slot has its own `CARGO_TARGET_DIR` at `qontinui-runner/target-pool/slot-{k}/` so concurrent builds do not contend on a shared `target/`. Frontend (`npm run build`) is serialized behind a dedicated mutex since Tauri embeds a single `dist/`.

**Pre-permit memory guard (2026-07-31).** Before acquiring a permit, a build waits while free **commit** is below `QONTINUI_SUPERVISOR_MIN_FREE_RAM_GB` (default 5). This mirrors `cargo-guard.sh`'s `MIN_FREE_GB` and `ci_node`'s `MIN_FREE_RAM_GB` — the supervisor was the only build lane without a memory floor, which is why it was the lane that OOM'd. It **defers, never rejects** (unlike the disk guard, which refuses with 507): memory pressure is transient, so failing the build would turn a recoverable condition into an error. After `QONTINUI_SUPERVISOR_MEM_WAIT_MAX_SECS` (900s) it builds anyway, and it fails open on an unreadable probe — a mis-measuring box degrades to the old behavior rather than wedging the lane. The wait happens **before** permit acquisition, so it never holds a slot.

It measures free COMMIT, not free physical RAM, deliberately: the binding constraint for a big rustc is the commit limit, and builds here have died at ~90% commit while free-physical looked healthy. On Windows it reads `GlobalMemoryStatusEx().ullAvailPageFile` — the same counter `cargo-guard.sh` reads via `Win32_OperatingSystem.FreeVirtualMemory`.

**sccache S3-backend degrade (2026-08-04, `src/sccache_guard.rs`).** All **four** compiling cargo spawns the supervisor makes in-process are guarded: the pool build, the `qontinui-shim` sidecar build and the slot prewarm go through `sccache_guard::guarded_cargo()`; the build-submission runner (`build_submissions::run_submission` — `POST /build/submit`, `submit_detached`, and the **spawn-test** `submit_spawn`) is a bare `tokio::process::Command`, so it applies the same decision via `sccache_guard::degrade_cargo_env_if_s3()`. Two cargo spawns are excluded deliberately: `process::manager`'s two are inside `#[cfg(test)]`, and `routes::runners`' `cargo clean --target-dir` compiles nothing so `RUSTC_WRAPPER` cannot affect it. If the **live** sccache server reports an S3 backend (the `s3,` marker in `sccache --show-stats`), the spawn gets `RUSTC_WRAPPER=""` and a loud WARN naming the regression and the fix; the build still runs, uncached. This closes the one hole in the two shipped guards: `cargo-guard.sh`'s `maybe_degrade_on_s3_backend` only fires when a **shell** invokes it, and the `sccache-backend-guard.sh` PreToolUse hook only intercepts an **agent's** Bash calls — so the most-built path on this machine had no guard at all.

Three properties are load-bearing, not style: (1) the predicate probes the server's **actual latched backend**, never `SCCACHE_BUCKET` — the env var misses a latched server whose spawning env was since cleaned and false-fires on a set-but-unused value; (2) the probe reads `--show-stats` **stdout only** (a bucket name in an sccache diagnostic on stderr must not read as a latched backend) and runs **only when a listener already holds the pinned sccache port** — resolved env → the nearest repo `.cargo/config.toml` `[env]` → **`$CARGO_HOME/config.toml`** (the user-level tier, which is where this machine's 4230 pin actually comes from) → 4226 — and never starts the daemon, because starting it from a polluted env is the exact mechanism that re-pins the machine to S3. That ordering is a strong mitigation rather than an absolute guarantee (sccache can idle out between the check and the probe), which is why the probe child also has the S3 selectors stripped from its env. The listener check binds the port to test it, so it now runs briefly before every guarded cargo spawn — no `SO_REUSEADDR`, never listened on, so it leaves no TIME_WAIT behind; (3) it **degrades, never fails**, and fails **open** on any inconclusive probe — withholding caching on a question it could not answer would be worse than the regression.

**It is a safety net, not a cure.** It stops a build entering an already-starved box and prevents the poisoned-cache cascade, but it cannot guarantee headroom 40 minutes later when the single `qontinui_runner` bin-crate rustc peaks (~5-6 GB). If that rustc's allocator fails, rustc aborts with `0xc0000409` (`STATUS_STACK_BUFFER_OVERRUN` — Rust's `__fastfail`, NOT a real buffer overrun) and the slot's incremental cache is corrupted. The durable fix on a 32 GB box is raising the Windows pagefile so the commit ceiling clears the peak.

### Resource telemetry (fleet resource sample)

The cached footprint snapshot is also the supervisor's **capacity sample** (plan
`2026-08-02-fleet-resource-telemetry-and-ci-allocation` §A2). `compute_snapshot`
adds, alongside the existing artifact sizes:

- `mem_total_bytes` / `mem_available_bytes` — physical, from sysinfo.
- `commit_total_bytes` / `commit_available_bytes` — **the pre-permit memory
  guard's own probe** (`build_monitor::available_commit_bytes`, and its ceiling
  `total_commit_bytes`), i.e. the same `ullAvailPageFile` number
  `cargo-guard.sh` reads. Published under its own name so a lane that drifts
  onto physical-available becomes *visible* rather than silently disagreeing.
- `swap_total_bytes` / `swap_used_bytes` — **not published on Windows.**
  sysinfo derives Windows swap from the commit counters
  (`CommitLimit/CommitTotal − PhysicalTotal`), so `swap_total − swap_used` is
  identically `commit_available_bytes` in the same row: publishing it would be
  the commit reading under a second name, and a consumer that ranks
  `swap_used / swap_total` reads ~0.77 on an **idle** box and calls it
  saturated. The fields are left null (`footprint::SWAP_IS_DERIVED_FROM_COMMIT`),
  and the commit pair above is the lead saturation metric here.

  **The rule is per-platform, not per-lane.** A `host` lane on a Linux machine
  has a real, independent swap device and publishes it — a future Linux
  supervisor must not inherit this omission. The measured
  swap-leads-`mem_available` finding (mem-available pinned by the kernel reserve
  at −13.5 ± 11.2 M/day while swap moves +138.6 ± 41.7 M/day) still governs the
  `wsl` / `container` rows other publishers write. The sibling runner publisher
  applies the same platform rule, so a `swap_*` value's presence is a property
  of the machine, never of which publisher wrote the row.
- `disk_total_bytes` / `disk_mount` next to the existing `disk_free_bytes`, so a
  free-byte figure is readable against its own capacity and attributable to a
  volume.
- `build_slots_total` / `build_slots_busy` / `build_queue_depth`, from
  `BuildPool::occupancy()` — derived from the same per-slot `busy` scan and
  `queue_depth` counter `GET /builds` already renders. There is exactly one
  accounting of the pool; "busy" means what the dashboard means by it.

Every field is `null` when its probe failed. **Null is UNKNOWN, never zero** — a
zero would render as "no headroom" or "idle pool", the false-safe class this
telemetry exists to remove.

The same footprint timer (default 15 min,
`QONTINUI_SUPERVISOR_FOOTPRINT_REFRESH_SECS`) POSTs the snapshot to coord's
`POST /coord/devices/:device_id/resource-sample` as a `lane="host"`,
`source="supervisor"` row (`src/resource_sample.rs`). It is deliberately **not**
a second timer: the sample is a projection of this snapshot, so a separate
cadence could only publish numbers that disagree with `GET /builds`.
`lane_instance` is NULL (the supervisor is the sole publisher for the host
lane); `ci_jobs_running` is NULL (each host runs two Actions runner services in
one WSL VM, so the supervisor cannot count jobs — deriving one from
idle/busy would be a fabricated number in a column an allocator reads). The WSL
lane is a different pool and is not the supervisor's to report.

Publishing is **best-effort and silent on failure**: machine identity and coord
URL resolve exactly as `fleet::publish_budget`'s do (`~/.qontinui/machine.json`,
`~/.qontinui/profiles.json`), a device bearer is attached when
`$COORD_DEVICE_JWT` or `~/.qontinui/coord-device-jwt` exists, and every failure
path returns after one WARN (subsequent ones DEBUG). A coord outage must never
touch the build lane.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/builds` | Snapshot of the parallel build pool: pool size, available permits, queue depth, per-slot state (`idle` or `building` with `started_at`/`elapsed_secs`/`requester_id`/`rebuild_kind`), `last_successful_slot`, and a top-level `active_builds` array. **Invariant:** `pool_size == available_permits + active_builds.len()`. `active_builds` and `available_permits` are derived from the same per-slot iteration as `slots[]` so the three views can never disagree mid-release. A separate `semaphore_permits` field exposes the raw `Semaphore::available_permits()` value for debugging transient release-ordering divergence inside `run_cargo_build_with_dir` — at steady state it equals `available_permits`. **This handler performs no network I/O** — see "`/builds` must not run git" below. |
| GET | `/builds/{slot_id}/log/stream` | SSE stream of cargo stderr lines for this slot's currently-running build. Events: `status` (one-shot prelude with `{state: "idle"\|"building", ...}`), `cargo` (one per stderr line, data is the raw line), `lagged` (broadcast drop count when the subscriber falls behind), `completed` (one frame on each building→idle transition, then the stream stays open for the next build). Returns 404 with `{error: "slot_not_found", ...}` if `slot_id` is out of range. Best for "tail cold cargo builds spawned via `POST /runners/spawn-test {rebuild: true}` so the user has progress visibility without polling `/builds/{slot_id}/log`." |
| DELETE | `/builds/caches` | Clear build caches across all pool slots |
| POST | `/build/reset` | Reset build state |

#### `/builds` must not run git

`GET /builds` used to compute `origin_main_drift` **inline**, and computing it
runs `git fetch origin` — a network call, with no deadline of its own. Measured
2026-08-10 on this fleet: `/builds` took **4.07s–27.12s** across 12 samples
while `/health` answered in 0.0–0.1s and the disk queue sat at 0. The cost was
neither disk nor memory; it was a per-request fetch. Any consumer with a
timeout under ~30s therefore saw the route as intermittent — which is exactly
how `verify-scoped-cleanup.ps1` came to fall back to a **guessed** pool size
(PR #139).

The drift is now computed by a background ticker (`refresh_origin_drift`,
default every 120s, override `QONTINUI_SUPERVISOR_ORIGIN_DRIFT_REFRESH_SECS`)
and the handler serves whatever is cached — the same stale-while-revalidate
shape as `footprint`. `fetch_origin` is additionally bounded by
`QONTINUI_SUPERVISOR_GIT_FETCH_TIMEOUT_SECS` (default 20s); an abandoned fetch
lands as `fetched: false`, which already means "compared against a possibly
stale local `origin/main`".

Because the reading is now cached, `origin_main_drift: null` alone cannot say
whether it means *up to date* or *never computed* — the confident-looking
default fleet policy `verification-and-evidence`
`unknown-must-not-render-as-a-default` forbids. A sibling
**`origin_main_drift_probe`** makes the two distinguishable:

| `state` | Meaning |
|---|---|
| `pending` | No reading has ever been computed (`computed_at`/`age_secs` null). |
| `fresh` | The cached reading answers for the CURRENT LKG sha. |
| `superseded_lkg_moved` | The LKG advanced since the reading; it answers a superseded question, and `origin_main_drift` is withheld. |
| `not_computable` | `origin/main` could not be resolved (no remote / not a repo). |

It also carries `computed_at`, `age_secs`, and `computed_for_sha` so a reader
can judge the reading's age instead of assuming it is current.

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
| POST/GET | `/runner/stop` | Stop runner (legacy single-runner endpoint, targets the primary). Optional body `{force?}`; a bodyless POST is fine (see "Optional request bodies"). `force` is the restart-readiness override — it reaches `manager::stop_runner`, where it used to be a dead `_force`. |
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

**Why you must build into a slot dir:** the supervisor's `resolve_source_exe` picks the source exe from `last_successful_slot` first, then scans other slots, and reaches the non-pool cargo target dir only as a last-resort fallback — or by adopting it over a staler slot, which requires a vouched provenance sidecar your manual build does not write. On every runner start it copies the source over `target/debug/qontinui-runner-{id}.exe` — so building into a non-pool `target/` means the supervisor may overwrite it with a stale slot exe. It is also the only build that produces **no provenance sidecar**, so if resolution ever does reach it, the start is refused as unverified (see "Exe resolution order" below) unless you opt in explicitly.

**Supervisor source of truth:** the exact args are assembled in `src/build_monitor.rs::run_build_inner`. If this doc drifts from that code, that file wins.

## Test Runner Binary Paths

### Target directories by spawn mode

| Directory | Purpose | Who writes it |
|-----------|---------|---------------|
| `target-pool/slot-{0,1,2}/debug/` | Supervisor parallel build pool (default 3 slots). Each spawn with `rebuild: true` claims a slot and sets `CARGO_TARGET_DIR` to the slot dir. | Supervisor via `run_cargo_build` |
| `target/debug/` | Non-pool build output — a manual `cargo build`, and everything `dev-start.ps1` compiles. Used as a last-resort fallback when no slot exe exists, **or** adopted over a staler slot when it proves what it is (see "A newer local build wins" below). | Manual `cargo build`, `dev-start.ps1` |

### Exe resolution order (when `rebuild: false`)

`resolve_source_exe()` in `src/process/manager.rs` picks the binary in this order:

1. `target-pool/slot-{last_successful_slot}/debug/qontinui-runner.exe`
2. Any `target-pool/slot-{k}/debug/qontinui-runner.exe` that exists on disk
3. The **non-pool cargo target dir**, resolved through cargo's OWN target-dir
   precedence — `CARGO_TARGET_DIR` → `build.target-dir` from the applicable
   `.cargo/config.toml` → the workspace default `<runner>/target` — taking the
   first candidate that actually holds a runner exe.

…with one **override on 1/2**: a non-pool build that is NEWER than the picked
slot and can prove what it is beats that slot. See the next section.

#### A newer local build wins over a staler slot

`dev-start.ps1` compiles the runner with **no `CARGO_TARGET_DIR`**, so its
artifact lands in the non-pool `target/debug/`. While that path was reachable
only as preference 3 ("no slot has an exe"), and slots effectively always
exist, an operator rebuild was discarded *by construction* while the console
printed "Runner binary rebuilt". Measured 2026-08-29: the operator's build
landed at 15:59Z, the supervisor resolved slot 0 (built 22:31Z the previous
day), and two rebuilds in a row silently ran 17.5-hour-old code.

Resolution now compares the two and **prefers the newer local build**, but only
when `local_build_is_adoptable` — a vouched (`live_tree` / `origin_main`)
provenance sidecar that is **at least as new as the exe it describes**. mtime
says *when* a file was written, not *what is in it*, so an unidentified artifact
is never promoted over a slot on mtime alone. The sidecar-freshness half is what
stops `npm run tauri dev` — which rewrites the same path without
`custom-protocol`, writes no sidecar, and shows "refused to connect" when
launched standalone — from being adopted on the strength of an older stamp it
did not write.

When the local build is newer but NOT adoptable, the slot still wins and the
operator is told plainly, at WARN, that their build is not what is running.
Adoption itself logs at INFO — it is the healthy outcome, and putting it at WARN
would train the reader to skim past the line that matters.

**The two outcomes are different `ExeOrigin`s, and the difference is
load-bearing.** Adoption reports `adopted_local_build:<precedence-level>`;
the last-resort fallthrough reports `cargo_target_dir:<precedence-level>`. They
are the same *path* under opposite *conditions* — "something better existed and
proved itself" versus "nothing better existed and this has no identity" — and
both are slot-less. The `LEGACY_EXE_FALLBACK` dev-state
(`src/dev_action/states.rs`) keys on the FALLTHROUGH origin via
`ExeOrigin::is_legacy_fallback()`, **never** on `slot_id.is_none()`: that
spelling reported the 2026-06-07 white-screen incident state on every healthy
`dev-start.ps1` rebuild, feeding a risk model (`dev_action::policy`) that can
route a start to the LKG binary — i.e. to code OLDER than the operator just
built, re-creating the defect adoption exists to fix.

An explicit LKG request never reaches any of this: `source_exe_override`
short-circuits resolution upstream.

**`GET /builds` answers "is my build what runs?" without starting anything.**
`pool_behind_local_build` is non-null whenever the picked slot is older than the
local build, and carries `adopted` (true = the local build runs; false = the
slot runs and your build does not), plus `legacy_path`, both mtimes,
`picked_slot_id`, `target_dir_source`, `local_build_sha` / `local_build_source`
(`null` = no sidecar, which is also *why* `adopted` is false) and the same
message the log carries. It is derived from the same evaluator the start path
runs, so the two can never disagree. The adjacent `legacy_target_debug_warning`
is the **opposite** direction (legacy older than every slot) and the two are
mutually exclusive by construction.

**Preference 3 used to be a hardcoded `<runner>/target/debug/`, and that was a
verification-integrity defect.** Every build on this fleet exports
`CARGO_TARGET_DIR=<runner>/src-tauri/target` (cargo-guard.sh, the dev docs, and
every agent told to reuse the warm shared target dir), so cargo wrote to the
override while the supervisor read the un-overridden default — where a
**54-day-old** artifact was still sitting. Measured 2026-08-06:

```
qontinui-runner/target/debug/qontinui-runner.exe            2026-06-12 17:29  259,355,136 B
qontinui-runner/src-tauri/target/debug/qontinui-runner.exe  2026-08-06 02:54  340,434,432 B
```

A `spawn-test {rebuild:false}` meant to verify a branch launched the June
binary; it came up healthy and served the UI Bridge, so nothing looked wrong —
the test simply measured the wrong code. Repointing the constant at
`src-tauri/target` would have inverted the bug onto every environment that sets
no override, hence the precedence ladder.

**The preference-3 FALLTHROUGH refuses rather than spawning silently.** (An
adopted local build is unaffected — it carries a vouched sidecar by
construction, which is this gate's own allow condition.) A fallthrough artifact
carries no provenance sidecar, so its build identity is unknown — and unknown
reads as *refuse*, not as *fine* (`unverified_exe_gate`). The start fails with
`409 {"error": "unverified_runner_exe"}` naming the resolved path, its mtime,
its build identity and which target-dir precedence level produced it. Slot
artifacts are unaffected — they keep `start_provenance_gate`'s posture (refuse
only on positive evidence of a foreign tree). Two explicit opt-ins keep the
"run whatever exists" case available, and **both state the staleness in the
response** instead of going quiet:

- `spawn-test` / `spawn-named` body `{"allow_stale_fallback": true}`;
- supervisor env `QONTINUI_SUPERVISOR_ALLOW_UNVERIFIED_EXE=1`.

**Every spawn response (and `GET /runners`) now carries `source_exe`** —
`{path, origin, slot_id, target_dir_source, mtime, build_sha, build_source,
unverified_warning}`. `origin` distinguishes `slot-N` /
`adopted_local_build:<level>` / `cargo_target_dir:<level>` / `pinned_override`;
`target_dir_source` is non-null for BOTH non-pool origins, so the
env-override-vs-workspace-default split is visible on the adoption path too. Nothing reported the path before, which is the whole
reason the stale spawn survived a full manual-test iteration.

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
- **Build watchdogs: NO-PROGRESS, not wall-clock** (2026-08-03). A cargo build is bounded by two budgets. The **no-progress watchdog** (default 20 min, override `QONTINUI_SUPERVISOR_BUILD_NO_PROGRESS_SECS`, clamped [60, 7200]) kills the build only after it has produced *nothing* for that long — no new cargo output **and** no new artifact under the slot's `CARGO_TARGET_DIR` (`debug/deps`, `debug/incremental`, `debug/build`, `debug/.fingerprint` mtimes). The **absolute backstop** (default 6h, override `QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS` — the historical name, clamped [300, 86400]) only catches a build that churns forever. The error names which budget fired.

  The single wall-clock cap is gone because it measured **load, not stuckness**: 5400s killed a build at 2650 of ~2800 compile units with rustc actively working, on a box carrying 6-7 peer cargo builds. Worse, the kill classified as an environmental failure, so the poisoned-slot self-heal wiped the slot and the retry started **cold**. Two consequences are now pinned by tests: the artifact probe is what keeps a silent multi-minute single-crate `rustc` from reading as wedged, and **a timeout never triggers the slot wipe** (`build_monitor::should_self_heal_slot`) — a timed-out build's incremental cache is kept so the retry starts warm.
- **Queue timeout: bounds the QUEUE only.** `queue_timeout_secs` bounds the phases where the request is blocked on a lock — waiting for a cargo permit (`AwaitingSlot`) and then for the serialized frontend lock (`AwaitingNpmLock`). **The clock stops the moment the build starts working** (`BuildingFrontend` / `Compiling`), so no value of `queue_timeout_secs` can 504 a build that is already compiling. Until 2026-08-03 it wrapped the whole build+spawn future despite its name and docs, so "don't make me queue >60s" also abandoned the caller's live compile at 60s, discarding everything it had built. The timeout message still NAMES the blocked phase — "waited Ns, blocked on the frontend (pnpm) lock with M cargo permits free" vs "waited Ns for a cargo build slot" — so the error tells you whether it was slot exhaustion or frontend serialization. `GET /builds` surfaces the same contention via `npm_lock_held` (bool, best-effort sample) and `npm_lock_waiters` (count of spawns blocked on the frontend lock); free `available_permits` with `npm_lock_held: true` does NOT guarantee a prompt start. A spawn waiting >60s on the frontend lock while cargo permits are free emits a `tracing::warn!` in supervisor.log.
- **Wait timeout:** configurable via `wait_timeout_secs` (default 120s). Only applies when `wait: true`. Returns successfully even if the runner doesn't become healthy — `status` field will say `"timeout"`.
- **No-wait mode:** pass `X-Queue-Mode: no-wait` header for immediate 503 with queue info instead of blocking.

If the build fails, the placeholder port reservation is cleaned up and the error is returned.

**Every outcome names the runner it reserved.** `spawn_test` mints the runner id
and reserves its port *before* the build begins, but only the success body used
to name them: every failure body rendered from a `SupervisorError` carried
`{"error": …}` and nothing else. Since the id is the caller's only handle on
`GET /runners/{id}/logs` and `POST /runners/{id}/stop`, a caller whose spawn
failed could not read why or clean up after itself.
`routes::runners::attach_spawn_identity` now merges `id` / `port` / `api_url` /
`ui_bridge_url` / `logs_url` / `stop_url` into **both** arms of
`execute_spawn_build` — the same uniformity contract `attach_build_slot_id`
already has, and pinned by the same style of test.

### A lost spawn-test answer

A synchronous `spawn-test {rebuild: true}` holds its HTTP connection open for
the **entire** build (40-50 min cold) without writing a byte. That made a
supervisor restart during a build produce a **split brain**, observed twice:

1. Axum's `with_graceful_shutdown` drops the **listener** the instant a shutdown
   is signalled, so port 9875 frees immediately and the **replacement**
   supervisor binds it — answering `GET /runners/<id>/logs` with
   `Runner not found` from its own empty in-memory registry.
2. It then waits for in-flight **connections**, which this handler holds for the
   rest of the build. The OLD process therefore stayed alive, still compiling,
   addressable by nobody — the "orphaned build".
3. When that zombie was finally force-killed (`restart-supervisor.ps1`
   escalates ~10s after the graceful POST), the caller's connection closed with
   no response written at all: `curl` reports `HTTP=000`.

Three changes close it, and they are load-bearing together:

- **`main::shutdown_signal` arms a bounded drain** (`SHUTDOWN_DRAIN_DEADLINE_SECS`,
  10s) at **signal** time. The pre-existing `HARD_EXIT_DEADLINE_SECS` watchdog is
  armed only after `serve_future.await` returns — i.e. after the very thing that
  hangs — so it could never bound this. Exiting is also what **reaps the build**:
  `GuardedCommand` wraps every build tree in a `KILL_ON_JOB_CLOSE` job, so
  *staying alive* is what orphaned it.
- **The synchronous wait is shutdown-aware.** It races `await_terminal` against
  `state.shutdown_signal()` and answers **503** `spawn_abandoned` naming the
  reserved `id`, `port` and `build_id`. "Abandoned" is deliberately distinct
  from both "failed" and "succeeded" — the caller must be able to tell
  *it did not happen* from *it worked and I was not told*.
- **`GET /runners/spawn-test/in-flight`** recovers the id when no answer arrived
  at all (a client-side timeout, a hard kill). It reads the single-flight index,
  which already holds `(submission_id, runner_id, port)` keyed by requester, so
  it is exact — unlike polling `GET /runners` and guessing by `requester_id`.

**Registrations are deliberately NOT persisted across a restart.** A temp runner
is terminated by the kill-on-exit JobObject when the supervisor process exits, so
a persisted registration would point at a dead process — a different way of
losing the state, dressed up as durability. The contract is instead: bound the
abandonment window, reap the build with the process, and *tell the caller*.

**Nothing in-process restarts the supervisor.** There is no watchdog, self-update
or config-reload path that does — `POST /supervisor/restart` is reachable only
over HTTP, and `symbol_watcher` is a separate binary that never touches
supervisor lifetime. A restart observed around a spawn-test came from outside
(`scripts/restart-supervisor.ps1`, `dev-start.ps1 -Supervisor`).

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
| Build no-progress watchdog | 20min (1200s) of no output AND no new artifact, override `QONTINUI_SUPERVISOR_BUILD_NO_PROGRESS_SECS` (clamped [60, 7200]) |
| Build absolute backstop | 6h (21600s), override `QONTINUI_SUPERVISOR_BUILD_TIMEOUT_SECS` (clamped [300, 86400]) |
| Pre-permit memory floor | 5 GB free **commit**, override `QONTINUI_SUPERVISOR_MIN_FREE_RAM_GB` (0 disables); defers up to `QONTINUI_SUPERVISOR_MEM_WAIT_MAX_SECS` (900s) then builds anyway |
| Port wait timeout | 120s |
| Graceful kill timeout | 5s |
| Log buffer | 500 entries (override: `QONTINUI_SUPERVISOR_LOG_BUFFER_SIZE`, clamped [100, 10000]) |
| Log file segment size | 64 MiB (override: `QONTINUI_SUPERVISOR_LOG_MAX_BYTES`, clamped [1 MiB, 8 GiB]) |
| Log file segment age | 24h (override: `QONTINUI_SUPERVISOR_LOG_MAX_AGE_SECS`, clamped [60, 2592000]) |
| Retained rotated segments | 5 per log, live file excluded (override: `QONTINUI_SUPERVISOR_LOG_MAX_RETAINED`, clamped [1, 100]) |
| Stopped-runners cache cap | 1000 entries (override: `QONTINUI_SUPERVISOR_STOPPED_CACHE_CAP`, clamped [100, 100000]) |
| Stopped-runners cache TTL | 3600s / 60min (override: `QONTINUI_SUPERVISOR_STOPPED_CACHE_TTL_SECS`, clamped [60, 86400]) |
| Build pool size | 3 (override: `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`) |
| Temp runner port range | 9877-9899 |
| Temp runner max age | 24h (86400s) default, override `QONTINUI_SUPERVISOR_TEMP_RUNNER_MAX_AGE_SECS` (clamped [3600, 604800]; **0 disables**). `RunnerKind::Temp` only; measured from `started_at`; `protected` does not exempt. |
| First-healthy watchdog budget | 90s (override: `QONTINUI_SUPERVISOR_FIRST_HEALTHY_TIMEOUT_SECS`); poll interval 3s |
| Crash-restart backoff ladder | 5s → 30s → 120s; max 3 auto-restarts per rolling 30min, then disarm (kill-switch: `QONTINUI_SUPERVISOR_NO_CRASH_RESTART=1`) |
| origin/main drift refresh | 120s (override: `QONTINUI_SUPERVISOR_ORIGIN_DRIFT_REFRESH_SECS`, min 1) — background ticker; `GET /builds` never computes it inline |
| `git fetch` deadline | 20s (override: `QONTINUI_SUPERVISOR_GIT_FETCH_TIMEOUT_SECS`, min 1) — an abandoned fetch reads as `fetched: false`, not an error |

## Diagnosing failed builds

**A failed build's stored error names its own cause.** `build_diagnostics::render_build_failure` composes it in three parts, in this order:

1. the headline (exit status, or the lines matching `BUILD_ERROR_PATTERNS`);
2. **fatal signatures scanned across the WHOLE log**, each with its line number, ranked most-causal-first (`ResourceExhaustion` → `ProcessAbort` → `CompilerDiagnostic` → `LinkFailure` → `GenericError`);
3. a **head + tail** excerpt (first 6 KB + last 4 KB) with the elided byte/line count marked explicitly.

**Why not just the tail.** It used to be the last **2 KB** and nothing else. For the runner's bin crate the final `rustc` command line alone is longer than that, so the window held only linker flags plus `(exit code: 0xc0000409, STATUS_STACK_BUFFER_OVERRUN)`. The real cause — `memory allocation of 2097152 bytes failed` — was **line 18**, matched no `BUILD_ERROR_PATTERNS` entry, and was cropped out entirely; the diagnosis followed the exit code and cost hours (2026-07-31). Compiler diagnostics and abort reasons are emitted when they happen, i.e. EARLY, so a window anchored only at the end is anchored at the wrong end. The signature scan is the redundant second mechanism: it hoists a known-fatal line out of the elided middle where no positional window would reach it.

The signature table is deliberately wider than rustc diagnostics — it covers allocator aborts, Windows commit-limit exhaustion (`os error 1455` / "paging file is too small"), disk-full, `LLVM ERROR`, stack overflow, panics, `exited abnormally`, and the `0xc0000409`/`0xc0000005` status codes. Add to it rather than widening the 2 KB window.

**Timeout-killed builds persist their output too** (same fix). The `TimedOut` / `Cancelled` / spawn-failure arms of `run_build_inner` used to early-return before any persistence, so a build the supervisor's own timeout killed wrote **no** `<slot>/last-build.stderr`, no `last_build_stderr_capture` and no `last_build_log` — leaving the sidecar dated from whatever build last exited normally (two failing builds on 2026-07-31 were diagnosed against a day-old file). All three arms now recover the partial stderr — taking whichever of `GuardedOutcome`'s `PartialOutput` bytes (a complete read-back of the redirect file) and the live line consumer (a bounded broadcast channel that *drops* frames when it lags) is **longer** — write `<slot>/last-build.stderr`, `last_build_stderr_capture` and `last_build_log`, and render through `render_incomplete_build`, which labels the excerpt **PARTIAL** so a missing terminal "could not compile" summary is not misread as "the build was fine until we killed it". They still do **not** set `state.build.last_build_stderr` (the smart-rebuild AI fix prompt's input); that remains exit-status-path only.

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

## Claude CLI spawning — strip inherited markers at EVERY spawn site

**Every `Command` the supervisor spawns must call
`process::claude_env::StripInheritedClaudeMarkers::strip_inherited_claude_markers()`.**
Never open-code `.env_remove("CLAUDECODE")` or
`.env_remove("CLAUDE_CODE_CHILD_SESSION")` — a unit test
(`every_spawn_site_goes_through_the_shared_strip`) fails the build if you do.

Claude Code sets `CLAUDECODE` and `CLAUDE_CODE_CHILD_SESSION` to describe a
process's place in a session tree. Both are inherited by the whole process tree
and **nothing clears them**. The supervisor is normally launched from a Claude
Code session, so it carries both and would hand them to every child — the
`claude` processes it launches directly (`evaluation/judge.rs`,
`velocity_improvement.rs`), the runners that go on to launch more, and the
replacement supervisor spawned by `POST /supervisor/restart`. A genuine
top-level session then claims to be the child of a session that exited long ago.

`INHERITED_CLAUDE_MARKERS` (`src/process/claude_env.rs`) is the single source of
truth for the list; adding a marker there covers every site at once.

**Why the rule is code and not just prose.** It used to be prose plus a
copy-pasted `env_remove` line. That is exactly how `CLAUDE_CODE_CHILD_SESSION`
came to be missing from all six spawn sites for months while `CLAUDECODE` was
stripped at every one of them: nothing could notice the omission, and the leak
was then misdiagnosed for a week as a transcript-persistence failure (it is
not — see plan `2026-07-28-runner-transcript-persistence-env-leak` §0/§7a; the
marker does **not** suppress transcript persistence). The grep-level test is the
durable fix; the `strip_inherited_claude_markers()` calls are just today's
instance of it.

**The escape hatch is deliberate and must be preserved.** The strip goes on the
base `Command`, *not* into `process::env_forwarders` — that registry is for
things to *set* and is applied afterwards, with `ExtraEnv` last. So
`POST /runners/spawn-test {extra_env: {"CLAUDE_CODE_CHILD_SESSION": "1"}}` can
still re-inject a marker to test the marked-child case on purpose.

The supervisor's own inherited marker is reported once at startup
(`claude_env::warn_if_child_session_marker_inherited`), naming where the marker
entered the fleet. Clearing that requires launching the supervisor from a shell
that does not carry it — a self-restart no longer propagates it.

## Code Standards

- Idiomatic Rust, `Result` types for errors
- `tracing` for logging, `thiserror` for error types
- `cargo fmt` and `cargo clippy -D warnings` enforced via pre-commit hooks
