<#
.SYNOPSIS
    Canonical restart for the qontinui-supervisor: stop the running instance,
    copy the freshly-built exe to a side-by-side copies dir, and relaunch it
    in a VISIBLE window.

.DESCRIPTION
    THIS IS THE supervisor restart path. Use it instead of ad-hoc per-session
    PowerShell. Why a checked-in script + a visible window:

      Windows Defender's `PowhidSubExec.B` heuristic kills processes launched
      with `-ExecutionPolicy Bypass` + `Start-Process -WindowStyle Hidden` on
      an UNSIGNED exe. Two supervisor restarts were killed this way on
      2026-06-05. This script therefore launches WITHOUT `-WindowStyle Hidden`
      (a normal visible console window) so the heuristic does not fire.

    Flow:
      1. Pre-flight runner-safety guard, run FIRST (before any build) so a
         refusal never burns a multi-minute build: probe `GET /health` for
         `ephemeral_job_temp_only`. If the CURRENTLY RUNNING supervisor
         reports it `true`, its kill-on-exit JobObject already scopes to temp
         runners only and the guard is skipped entirely. Otherwise (the field
         is absent, `false`, or `/health` itself is unreachable while
         something is still listening) fall back to querying `GET /runners`
         and refuse (exit 4) if any non-temp runner (the primary, or a
         `named-*` runner) is currently running - see "runner-lifetime truth"
         below. Skipped/overridden with -ForceKillRunners.
      2. (optional) `cargo build` in the repo, aborting on failure.
      3. Graceful `POST /supervisor/shutdown`; if that fails or times out,
         fall back to Stop-Process on the process whose path matches the
         deployed exe.
      4. Wait for the supervisor port to become free.
      5. Copy `target\debug\qontinui-supervisor.exe` -> `target\debug\copies\`
         (so the source build artifact is never locked by the running process).
      6. Start-Process the COPY with --project-dir / --watchdog / --log-file,
         working dir = repo root, NO -WindowStyle Hidden.
      7. Poll `GET /health` up to 60s; exit 0 on HTTP 200, non-zero otherwise.
      8. When -Watchdog: poll `GET /runners` (up to 120s) until the `primary`
         entry reports `running: true` - the supervisor boot-starts it under
         --watchdog/--auto-start. Exit 0 with the primary's pid/port once it
         is up; exit 3 (distinct: the supervisor itself IS healthy) if it
         never comes up within the window.

    Runner-lifetime truth: a supervisor binary built BEFORE 2026-07-28
    assigns EVERY runner it spawns to a Win32 JobObject with
    KILL_ON_JOB_CLOSE, so when that OLD supervisor process exits (graceful
    shutdown OR the Stop-Process fallback in step 3), the KERNEL terminates
    every runner still assigned to that job - primary included - with no
    "Stopping runner" log line, no graceful stop, and no automatic recovery.
    A supervisor built from 2026-07-28 onward assigns ONLY temp runners
    (`test-*`) to that job and reports `ephemeral_job_temp_only: true` on
    `GET /health` so callers - including step 1's guard - can tell the
    difference; against that fixed binary, user-owned runners (primary,
    named, external) survive every exit path this script takes. Step 1 above
    is the guard against reaping by accident via THIS script when the
    CURRENTLY RUNNING supervisor is not (yet) the fixed one; it does not
    cover restarts done another way (hand Stop-Process, closing the console
    window, Ctrl-C).

.PARAMETER Build
    Build the frontend (`npm run build` in frontend\, producing the repo-root
    dist\ that Tauri's rust-embed bundles) AND `cargo build`, in that order.
    Abort the whole restart if either build fails. The frontend MUST build
    first: the supervisor embeds dist\ at cargo-build time via rust-embed
    (`#[folder = "dist/"]` in src\routes\dashboard.rs), so a cargo-only build
    silently ships whatever stale dist\ already exists on disk.

.PARAMETER SkipFrontend
    With -Build, skip the frontend `npm run build` and run cargo only. Use ONLY
    when you know dist\ is already fresh (e.g. you just built it by hand).

.PARAMETER Port
    Supervisor HTTP port. Default 9875.

.PARAMETER ProjectDir
    Path passed to the supervisor as --project-dir (the runner src-tauri).
    Default D:\qontinui-root\qontinui-runner\src-tauri.

.PARAMETER LogFile
    Path passed to the supervisor as --log-file.
    Default D:\qontinui-root\.dev-logs\runner-tauri.log.

.PARAMETER Watchdog
    Pass --watchdog (observe-only health monitoring, implies --auto-start).
    With --auto-start the supervisor boot-starts the PRIMARY runner once, so
    this script additionally verifies the primary returns (step 8). On by
    default; pass -Watchdog:$false to omit (and skip the primary check).

.PARAMETER ForceKillRunners
    Overrides the step-1 pre-flight runner-safety guard. The guard only
    fires when the CURRENTLY RUNNING supervisor does NOT confirm the
    2026-07-28 JobObject fix (i.e. `GET /health` omits `ephemeral_job_temp_only`
    or reports it `false`) - such a supervisor still scopes its kill-on-exit
    job to EVERY runner it spawned, so stopping it would kill any running
    non-temp runner too. Without this switch the script REFUSES (exit 4) in
    that situation: proceeding would kill the primary/named runner(s) and
    every session inside them (terminal panes, AI sessions, in-flight
    workflows) via the kernel JobObject teardown described under
    "Runner-lifetime truth" above, with no graceful stop and no recovery.

    Against a supervisor that already reports `ephemeral_job_temp_only:
    true`, the guard is skipped automatically and this switch is a no-op -
    do NOT pass it as a matter of habit, since it would still apply against
    whatever supervisor happens to be running the NEXT time this script
    runs, including a stale pre-fix one. This is not part of any normal
    invocation - it is an explicit, conscious opt-in for the case you must
    restart a pre-fix (or unconfirmed) supervisor while a primary/named
    runner is running and are willing to lose it. Temp runners (`test-*`)
    never trip the guard; they are ephemeral by design and are supposed to
    die with the supervisor.

.EXAMPLE
    .\scripts\restart-supervisor.ps1 -Build

.NOTES
    PowerShell 5.1 compatible: no `&&`, no ternary, no null-coalescing.

    Exit codes:
      0 - success (supervisor healthy; primary up when -Watchdog).
      1 - build failure / source exe missing / old process still alive
          after the force-kill grace window / port still bound.
      2 - supervisor did not report healthy on /health within 60s.
      3 - supervisor IS healthy, but the primary never reached
          running:true within 120s (-Watchdog only).
      4 - refused: a running non-temp runner (primary or named-*) would be
          reaped by this restart, because the CURRENTLY RUNNING supervisor
          did not confirm `ephemeral_job_temp_only: true` on `GET /health`
          (pre-2026-07-28 binary, or the field was unreadable) and would
          still scope its kill-on-exit JobObject to every runner it spawned.
          Against a supervisor that DOES confirm the fix, this guard never
          fires and exit 4 cannot happen. Re-run with -ForceKillRunners to
          proceed anyway.
#>
[CmdletBinding()]
param(
    [switch]$Build,
    [switch]$SkipFrontend,
    [int]$Port = 9875,
    [string]$ProjectDir = 'D:\qontinui-root\qontinui-runner\src-tauri',
    [string]$LogFile = 'D:\qontinui-root\.dev-logs\runner-tauri.log',
    [switch]$Watchdog = $true,
    [switch]$ForceKillRunners
)

$ErrorActionPreference = 'Stop'

# Repo root = the parent of this script's directory (scripts\ lives at the root).
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Split-Path -Parent $ScriptDir

$ExeName   = 'qontinui-supervisor.exe'
$SourceExe = Join-Path $RepoRoot (Join-Path 'target\debug' $ExeName)
$CopiesDir = Join-Path $RepoRoot 'target\debug\copies'
$CopyExe   = Join-Path $CopiesDir $ExeName

function Write-Step($msg)  { Write-Host "[restart-supervisor] $msg" }
function Write-Fail($msg)  { Write-Host "[restart-supervisor] ERROR: $msg" -ForegroundColor Red }

$base = "http://127.0.0.1:$Port"

# --- Helpers: runner list normalization + guard decision --------------------
# GET /runners may return a bare array or an object with a `.runners`
# property; every caller that reads it needs the same normalization, so it
# lives here once and is reused by the pre-flight guard (step 1, below) and
# the post-launch primary check (step 8, at the bottom of this script).
function Get-RunnerList {
    param($RunnersPayload)
    if (-not $RunnersPayload) { return @() }
    $list = $RunnersPayload
    if ($RunnersPayload.PSObject.Properties.Name -contains 'runners') {
        $list = $RunnersPayload.runners
    }
    if (-not $list) { return @() }
    # Force array semantics on return - `return @($list)` alone still
    # enumerates through the output pipeline and unwraps a single-element
    # array back to a scalar (the same PowerShell gotcha Get-BlockingRunners
    # guards against below with its own leading comma).
    return ,@($list)
}

# Pure decision function: given a normalized runner list, return the ones
# that a restart would REAP - i.e. currently running AND not a temp runner
# (`test-*`). Temp runners are ephemeral by design and are supposed to die
# with the supervisor, so they never block. Kept pure (no HTTP, no I/O) so
# it can be unit-tested with a canned payload - see
# scripts\restart-supervisor.Tests.ps1.
function Get-BlockingRunners {
    param($RunnerList)
    $blocking = @()
    foreach ($r in $RunnerList) {
        if (-not $r) { continue }
        $isRunning = $false
        if ($r.PSObject.Properties.Name -contains 'running') {
            $isRunning = ($r.running -eq $true)
        }
        $id = $null
        if ($r.PSObject.Properties.Name -contains 'id') { $id = $r.id }
        $isTemp = $false
        if ($id -and ($id -like 'test-*')) { $isTemp = $true }
        if ($isRunning -and (-not $isTemp)) {
            $blocking += $r
        }
    }
    # Force array semantics even for 0/1 results (PowerShell would otherwise
    # unwrap a single-element array to a scalar on return).
    return ,$blocking
}

# Pure decision function: given the parsed `GET /health` payload from the
# CURRENTLY RUNNING supervisor (or $null when /health was unreachable),
# decide whether the runner-safety guard (the /runners query + exit-4 logic)
# still needs to run.
#
#   - $null payload (health unreachable): there is nothing to reap - either
#     the supervisor is already down, or it never came up - so the guard is
#     not required. This mirrors the pre-existing "could not query
#     /runners -> skip the guard" fallback the /runners call itself already
#     had.
#   - Payload present WITHOUT `ephemeral_job_temp_only`, or with it `false`:
#     a pre-2026-07-28 binary (or one that can't confirm the fix) - fail
#     SAFE, guard stays active.
#   - Payload present WITH `ephemeral_job_temp_only: true`: the running
#     supervisor already scopes its kill-on-exit JobObject to temp runners
#     only, so nothing this script does can reap a user-owned runner - guard
#     not required.
#
# Kept pure (no HTTP, no I/O) so it can be unit-tested with a canned payload
# - see scripts\restart-supervisor.Tests.ps1.
function Test-GuardRequired {
    param($HealthPayload)   # $null when /health was unreachable
    if (-not $HealthPayload) {
        return $false
    }
    if ($HealthPayload.PSObject.Properties.Name -contains 'ephemeral_job_temp_only') {
        if ($HealthPayload.ephemeral_job_temp_only -eq $true) {
            return $false
        }
    }
    return $true
}

# --- 1. Pre-flight runner-safety guard --------------------------------------
# A supervisor built BEFORE 2026-07-28 assigns EVERY runner it spawns to a
# Win32 JobObject with KILL_ON_JOB_CLOSE. When that supervisor process exits
# (graceful shutdown below, or the Stop-Process fallback), the KERNEL
# terminates every runner still in that job - silently, with no "Stopping
# runner" log line and no recovery. On 2026-07-27 a routine restart destroyed
# the primary runner, ~80 claude.exe processes, and 287 PTYs this way.
#
# This runs BEFORE the optional build (step 2) - it is a couple of 5-second
# HTTP calls, and a refusal here should never cost a multi-minute build.
#
# SELF-RETIRING VIA /health. A fixed (2026-07-28+) supervisor reports
# `ephemeral_job_temp_only: true` on `GET /health` - a pre-fix binary has no
# such code and omits the field entirely, so presence + `true` is a reliable
# signal that the CURRENTLY RUNNING supervisor already scopes its
# kill-on-exit job to temp runners only. When that's confirmed, the guard is
# skipped outright: no /runners query, no exit 4, -ForceKillRunners is not
# needed and is a no-op. Only when the field is absent/false/unreadable does
# this fall back to the original behavior - query `GET /runners` and refuse
# (exit 4) if a non-temp runner (primary or named-*) is currently running,
# unless -ForceKillRunners is passed.
#
# WHY THIS GUARD SURVIVES THE CODE FIX. A job assignment is made by the
# supervisor that SPAWNED the process, and Windows cannot remove a process
# from a job afterwards. This script stops the CURRENTLY RUNNING supervisor -
# and if that instance is a pre-fix binary, its job still holds the primary
# and stopping it still reaps, no matter how fixed today's checked-out code
# is. The /health probe above is what lets the guard tell those two cases
# apart and retire itself automatically once the running binary is the fixed
# one - so a routine `.\scripts\restart-supervisor.ps1 -Build` against a
# healthy, already-fixed fleet is never refused.
#
# NOTE: this guard only covers a restart done THROUGH THIS SCRIPT. A peer
# who restarts the supervisor another way (hand Stop-Process, closing the
# console window, Ctrl-C in the visible console) is still exposed on a
# pre-fix supervisor. Treat this as a safety net, not a complete fix.
Write-Step "checking $base/health for JobObject scoping (ephemeral_job_temp_only)..."
$healthPayload = $null
$healthQueryOk = $false
try {
    $healthPayload = Invoke-RestMethod -Method Get -Uri "$base/health" -TimeoutSec 5
    $healthQueryOk = $true
} catch {
    Write-Step "could not query $base/health ($($_.Exception.Message)); supervisor may already be down. Nothing running to reap; skipping pre-flight guard."
}

$guardRequired = Test-GuardRequired -HealthPayload $healthPayload

if ($healthQueryOk -and (-not $guardRequired)) {
    Write-Step "running supervisor reports ephemeral_job_temp_only:true - its kill-on-exit JobObject is scoped to temp runners only, so user-owned runners (primary/named/external) survive this restart. Skipping the runner-safety guard."
}

if ($guardRequired) {
    Write-Step "running supervisor did not confirm the JobObject fix via /health; checking $base/runners for runners this restart would reap..."
    $existingRunnersPayload = $null
    $runnersQueryOk = $false
    try {
        $existingRunnersPayload = Invoke-RestMethod -Method Get -Uri "$base/runners" -TimeoutSec 5
        $runnersQueryOk = $true
    } catch {
        Write-Step "could not query $base/runners ($($_.Exception.Message)); supervisor may already be down. Skipping pre-flight guard."
    }

    if ($runnersQueryOk) {
        $existingRunnerList = Get-RunnerList -RunnersPayload $existingRunnersPayload
        $blockingRunners = Get-BlockingRunners -RunnerList $existingRunnerList

        if ($blockingRunners.Count -gt 0) {
            if (-not $ForceKillRunners) {
                Write-Fail "REFUSING to restart: $($blockingRunners.Count) runner(s) are RUNNING and would be REAPED."
                Write-Fail ""
                Write-Fail "The currently running supervisor did not confirm the 2026-07-28 fix on"
                Write-Fail "$base/health (ephemeral_job_temp_only:true) - either it predates the fix"
                Write-Fail "or the field could not be read. A supervisor built before 2026-07-28"
                Write-Fail "assigns EVERY runner it spawns to a Win32 JobObject with"
                Write-Fail "KILL_ON_JOB_CLOSE. When that supervisor process exits, the KERNEL"
                Write-Fail "terminates every runner still in that job - there is NO 'Stopping"
                Write-Fail "runner' log line, NO graceful stop, and NO automatic recovery. Every"
                Write-Fail "session inside each runner below - terminal panes, AI sessions,"
                Write-Fail "in-flight workflows - would be destroyed along with it."
                Write-Fail ""
                Write-Fail "Newer supervisors assign ONLY temp runners, so user-owned runners"
                Write-Fail "survive, and report that fact on /health so this guard retires itself"
                Write-Fail "automatically. But a process cannot be removed from a Windows job after"
                Write-Fail "assignment, and this script stops the CURRENTLY RUNNING supervisor -"
                Write-Fail "if that one is a pre-fix binary, its job still holds these runners."
                Write-Fail ""
                Write-Fail "Runner(s) that would be killed:"
                foreach ($r in $blockingRunners) {
                    $rId = '?'
                    if ($r.PSObject.Properties.Name -contains 'id') { $rId = $r.id }
                    $rPid = '?'
                    if ($r.PSObject.Properties.Name -contains 'pid') { $rPid = $r.pid }
                    $rPort = '?'
                    if ($r.PSObject.Properties.Name -contains 'port') { $rPort = $r.port }
                    Write-Fail "  - id=$rId pid=$rPid port=$rPort"
                }
                Write-Fail ""
                Write-Fail "NOTE: this guard only covers a restart done THROUGH THIS SCRIPT. A peer"
                Write-Fail "who restarts the supervisor another way (hand Stop-Process, closing the"
                Write-Fail "console window, Ctrl-C) still reaps the runners - only the Rust-level"
                Write-Fail "JobObject-scoping fix covers those paths. This is a safety net, not a"
                Write-Fail "complete fix."
                Write-Fail ""
                Write-Fail "To proceed anyway, re-run with -ForceKillRunners."
                exit 4
            } else {
                Write-Step "-ForceKillRunners passed: proceeding, will kill $($blockingRunners.Count) running runner(s)."
            }
        } else {
            Write-Step "no running non-temp runners found; safe to proceed."
        }
    }
}

# --- 2. Optional build ------------------------------------------------------
# Frontend FIRST, then cargo: the supervisor embeds the repo-root dist\ at
# cargo-build time via rust-embed (#[folder = "dist/"] in
# src\routes\dashboard.rs). If we only ran cargo, it would silently re-embed
# whatever stale dist\ already exists, shipping an old frontend even though the
# Rust recompiled (the 2026-06-07 Lineage-page-auth stale-embed incident).
if ($Build) {
    if (-not $SkipFrontend) {
        $FrontendDir = Join-Path $RepoRoot 'frontend'
        Write-Step "npm run build (frontend, in $FrontendDir) -> repo-root dist\ ..."
        Push-Location $FrontendDir
        try {
            # Install deps only if missing (fast no-op when node_modules is present).
            if (-not (Test-Path (Join-Path $FrontendDir 'node_modules'))) {
                Write-Step "node_modules missing; running npm ci..."
                & npm ci
                if ($LASTEXITCODE -ne 0) {
                    Write-Fail "npm ci failed (exit $LASTEXITCODE); aborting restart."
                    Pop-Location
                    exit 1
                }
            }
            & npm run build
            $feExit = $LASTEXITCODE
        } finally {
            Pop-Location
        }
        if ($feExit -ne 0) {
            Write-Fail "frontend build failed (exit $feExit); aborting restart (would embed stale dist\)."
            exit 1
        }
        Write-Step "frontend build ok (dist\ refreshed)."
    } else {
        Write-Step "-SkipFrontend: skipping frontend build (using existing dist\)."
    }

    Write-Step "cargo build (in $RepoRoot)..."
    Push-Location $RepoRoot
    try {
        & cargo build
        $buildExit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($buildExit -ne 0) {
        Write-Fail "cargo build failed (exit $buildExit); aborting restart."
        exit 1
    }
    Write-Step "build ok."
}

# --- Safety: source exe must exist (unless -Build was supposed to make it) -
if (-not (Test-Path $SourceExe)) {
    Write-Fail "$SourceExe is missing. Run with -Build, or build the supervisor first."
    exit 1
}

# Print the git SHA being deployed (best-effort).
try {
    $sha = & git -C $RepoRoot log -1 --format=%h
    if ($LASTEXITCODE -eq 0 -and $sha) {
        Write-Step "deploying supervisor at git $sha"
    }
} catch {
    Write-Step "git SHA unavailable (not a git checkout?)"
}

# --- Helpers: process lookup -------------------------------------------------
# Resolve the deployed exe's canonical path so we can match the running
# process for the Stop-Process fallback. The running instance is the COPY.
$runningPath = $CopyExe
try { $runningPath = (Resolve-Path $CopyExe -ErrorAction Stop).Path } catch {}

function Get-SupervisorPids {
    $found = @()
    try {
        $procs = Get-CimInstance Win32_Process -Filter "Name = '$ExeName'" -ErrorAction Stop
        foreach ($p in $procs) {
            if ($p.ExecutablePath -and ($p.ExecutablePath -ieq $runningPath)) {
                $found += $p.ProcessId
            }
        }
    } catch {
        # enumeration failure -> treat as none found (best effort)
    }
    return $found
}

# --- 3. Graceful shutdown, fall back to Stop-Process ----------------------
Write-Step "requesting graceful shutdown ($base/supervisor/shutdown)..."
$gracefulOk = $false
try {
    Invoke-RestMethod -Method Post -Uri "$base/supervisor/shutdown" -TimeoutSec 8 | Out-Null
    $gracefulOk = $true
    Write-Step "graceful shutdown acknowledged."
} catch {
    Write-Step "graceful shutdown POST failed ($($_.Exception.Message)); will fall back to Stop-Process."
}

# Fallback: kill any process whose executable path is the copy we run.
if (-not $gracefulOk) {
    $killed = $false
    try {
        $procs = Get-CimInstance Win32_Process -Filter "Name = '$ExeName'" -ErrorAction Stop
        foreach ($p in $procs) {
            if ($p.ExecutablePath -and ($p.ExecutablePath -ieq $runningPath)) {
                Write-Step "Stop-Process PID $($p.ProcessId) ($($p.ExecutablePath))"
                Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
                $killed = $true
            }
        }
    } catch {
        Write-Step "process enumeration failed: $($_.Exception.Message)"
    }
    if (-not $killed) {
        Write-Step "no matching running supervisor process found (already stopped?)."
    }
}

# --- 3b. Wait for the OLD PROCESS to exit (not just the port) -------------
# A graceful shutdown releases the listener socket BEFORE process teardown
# finishes, so "port free" does NOT mean "process gone". On 2026-06-06 a
# restart hit exactly this: the port freed, but the old process still held
# the copies\ exe lock and the Copy-Item below failed, leaving the
# supervisor DOWN. Wait for the matching process to actually exit; escalate
# to Stop-Process -Force if it lingers. (Get-SupervisorPids is defined
# earlier, alongside Get-RunnerList / Get-BlockingRunners.)
Write-Step "waiting for the old supervisor process to exit..."
$procGone = $false
for ($i = 0; $i -lt 40; $i++) {
    $pids = Get-SupervisorPids
    if (-not $pids -or $pids.Count -eq 0) { $procGone = $true; break }
    if ($i -eq 20) {
        # ~10s grace expired -> escalate to force kill
        foreach ($zpid in $pids) {
            Write-Step "old process PID $zpid still alive after grace; Stop-Process -Force"
            Stop-Process -Id $zpid -Force -ErrorAction SilentlyContinue
        }
    }
    Start-Sleep -Milliseconds 500
}
if (-not $procGone) {
    Write-Fail "old supervisor process still alive after 20s (incl. force kill); aborting before the exe copy would fail."
    exit 1
}
Write-Step "old process exited."

# --- 4. Wait for the port to free -----------------------------------------
Write-Step "waiting for port $Port to free..."
$portFree = $false
for ($i = 0; $i -lt 30; $i++) {
    $inUse = $false
    try {
        $conns = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop
        if ($conns) { $inUse = $true }
    } catch {
        # Get-NetTCPConnection throws when nothing is listening => port free.
        $inUse = $false
    }
    if (-not $inUse) { $portFree = $true; break }
    Start-Sleep -Milliseconds 500
}
if (-not $portFree) {
    Write-Fail "port $Port still in use after wait; aborting to avoid a double-bind."
    exit 1
}
Write-Step "port $Port free."

# --- 5. Copy the exe to the side-by-side copies dir -----------------------
if (-not (Test-Path $CopiesDir)) {
    New-Item -ItemType Directory -Path $CopiesDir | Out-Null
}
Write-Step "copying exe -> $CopyExe"
Copy-Item -Path $SourceExe -Destination $CopyExe -Force

# --- 6. Launch the copy in a VISIBLE window -------------------------------
# IMPORTANT: NO -WindowStyle Hidden. Windows Defender's PowhidSubExec.B
# heuristic kills hidden launches of unsigned exes (2026-06-05 incident).
$supArgs = @('--project-dir', $ProjectDir, '--port', "$Port", '--log-file', $LogFile)
if ($Watchdog) { $supArgs += '--watchdog' }

Write-Step "launching: $CopyExe $($supArgs -join ' ')"
Start-Process -FilePath $CopyExe -ArgumentList $supArgs -WorkingDirectory $RepoRoot | Out-Null

# --- 7. Poll /health up to 60s --------------------------------------------
Write-Step "polling $base/health (up to 60s)..."
$healthy = $false
for ($i = 0; $i -lt 60; $i++) {
    try {
        $resp = Invoke-WebRequest -Uri "$base/health" -TimeoutSec 3 -UseBasicParsing
        if ($resp.StatusCode -eq 200) { $healthy = $true; break }
    } catch {
        # not up yet
    }
    Start-Sleep -Seconds 1
}

if (-not $healthy) {
    Write-Fail "supervisor did not report healthy on $base/health within 60s."
    Write-Fail "Check the window it launched in, and $LogFile."
    exit 2
}
Write-Step "supervisor healthy on port $Port."

# --- 8. Verify the primary runner boot-starts (only under --watchdog) ------
# Under --watchdog/--auto-start the supervisor boot-starts the PRIMARY runner
# once. Tonight's incident: a restart left the primary DOWN until a human
# manually started it. Verify it actually returns; exit 3 (distinct from the
# supervisor-unhealthy exit 2) if it doesn't - the supervisor IS healthy, only
# the primary boot-start failed.
if (-not $Watchdog) {
    Write-Step "-Watchdog:`$false - no primary auto-start expected; skipping primary check. Done."
    exit 0
}

Write-Step "verifying primary runner boot-starts ($base/runners) up to 120s..."
$primaryUp = $false
$primaryInfo = $null
for ($i = 0; $i -lt 40; $i++) {
    try {
        $runners = Invoke-RestMethod -Method Get -Uri "$base/runners" -TimeoutSec 5
        $list = Get-RunnerList -RunnersPayload $runners
        $primary = $list | Where-Object { $_.id -eq 'primary' } | Select-Object -First 1
        if ($primary -and $primary.running -eq $true) {
            $primaryUp = $true
            $primaryInfo = $primary
            break
        }
    } catch {
        # supervisor may briefly 5xx /runners while state settles; retry.
    }
    Start-Sleep -Seconds 3
}

if ($primaryUp) {
    $pidText  = if ($primaryInfo.PSObject.Properties.Name -contains 'pid')  { $primaryInfo.pid }  else { '?' }
    $portText = if ($primaryInfo.PSObject.Properties.Name -contains 'port') { $primaryInfo.port } else { '?' }
    Write-Step "primary runner is running (pid=$pidText port=$portText). Done."
    exit 0
} else {
    Write-Fail "primary runner did not reach running:true within 120s, though the supervisor is healthy."
    Write-Fail "Likely causes:"
    Write-Fail "  - the #65 provenance start gate refused the slot binary (check the supervisor window / $LogFile for an 'Auto-start of primary runner ... failed' WARN);"
    Write-Fail "  - no runner binary is built yet (build the runner via POST /runners/spawn-test {rebuild:true} or check target-pool/ slots)."
    Write-Fail "Start it manually with: Invoke-RestMethod -Method Post -Uri $base/runners/primary/start"
    exit 3
}
