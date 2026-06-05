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
      1. (optional) `cargo build` in the repo, aborting on failure.
      2. Graceful `POST /supervisor/shutdown`; if that fails or times out,
         fall back to Stop-Process on the process whose path matches the
         deployed exe.
      3. Wait for the supervisor port to become free.
      4. Copy `target\debug\qontinui-supervisor.exe` -> `target\debug\copies\`
         (so the source build artifact is never locked by the running process).
      5. Start-Process the COPY with --project-dir / --watchdog / --log-file,
         working dir = repo root, NO -WindowStyle Hidden.
      6. Poll `GET /health` up to 60s; exit 0 on HTTP 200, non-zero otherwise.
      7. When -Watchdog: poll `GET /runners` (up to 120s) until the `primary`
         entry reports `running: true` - the supervisor boot-starts it under
         --watchdog/--auto-start. Exit 0 with the primary's pid/port once it
         is up; exit 3 (distinct: the supervisor itself IS healthy) if it
         never comes up within the window.

.PARAMETER Build
    Run `cargo build` first. Abort the whole restart if the build fails.

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
    this script additionally verifies the primary returns (step 7). On by
    default; pass -Watchdog:$false to omit (and skip the primary check).

.EXAMPLE
    .\scripts\restart-supervisor.ps1 -Build

.NOTES
    PowerShell 5.1 compatible: no `&&`, no ternary, no null-coalescing.
#>
[CmdletBinding()]
param(
    [switch]$Build,
    [int]$Port = 9875,
    [string]$ProjectDir = 'D:\qontinui-root\qontinui-runner\src-tauri',
    [string]$LogFile = 'D:\qontinui-root\.dev-logs\runner-tauri.log',
    [switch]$Watchdog = $true
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

# --- 1. Optional build ----------------------------------------------------
if ($Build) {
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

$base = "http://127.0.0.1:$Port"

# --- 2. Graceful shutdown, fall back to Stop-Process ----------------------
# Resolve the deployed exe's canonical path so we can match the running
# process for the Stop-Process fallback. The running instance is the COPY.
$runningPath = $CopyExe
try { $runningPath = (Resolve-Path $CopyExe -ErrorAction Stop).Path } catch {}

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

# --- 3. Wait for the port to free -----------------------------------------
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

# --- 4. Copy the exe to the side-by-side copies dir -----------------------
if (-not (Test-Path $CopiesDir)) {
    New-Item -ItemType Directory -Path $CopiesDir | Out-Null
}
Write-Step "copying exe -> $CopyExe"
Copy-Item -Path $SourceExe -Destination $CopyExe -Force

# --- 5. Launch the copy in a VISIBLE window -------------------------------
# IMPORTANT: NO -WindowStyle Hidden. Windows Defender's PowhidSubExec.B
# heuristic kills hidden launches of unsigned exes (2026-06-05 incident).
$supArgs = @('--project-dir', $ProjectDir, '--port', "$Port", '--log-file', $LogFile)
if ($Watchdog) { $supArgs += '--watchdog' }

Write-Step "launching: $CopyExe $($supArgs -join ' ')"
Start-Process -FilePath $CopyExe -ArgumentList $supArgs -WorkingDirectory $RepoRoot | Out-Null

# --- 6. Poll /health up to 60s --------------------------------------------
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

# --- 7. Verify the primary runner boot-starts (only under --watchdog) ------
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
        # /runners may return an array directly or an object with a .runners
        # array - normalize to the list before searching.
        $list = $runners
        if ($runners -and ($runners.PSObject.Properties.Name -contains 'runners')) {
            $list = $runners.runners
        }
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
