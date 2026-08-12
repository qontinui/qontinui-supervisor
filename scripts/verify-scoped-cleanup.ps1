<#
.SYNOPSIS
    End-to-end proof that the build pool's slot cleanup is TERRITORY-SCOPED:
    a foreign cargo build survives a pool build, an in-territory orphan is
    still reaped, and sibling pool slots are left alone.

.DESCRIPTION
    PR #119 ("build-kill telemetry" / scoped slot cleanup) asserts two things
    that nothing else in this repo verifies end-to-end:

      V1  A cargo build OUTSIDE every slot territory - a peer agent's worktree
          compile - SURVIVES a pool build's cleanup pass, and the pass records
          it as SPARED rather than silently doing nothing.

      V2  A cargo process INSIDE the claimed slot's territory is still REAPED
          (the cleanup did not degrade into a no-op), the kill is reported with
          the strong `matched_by: "env"` evidence, and the orphans sitting in
          the OTHER pool slots are NOT touched.

    V2's sibling half is the one with no coverage at all today: the unit tests
    in `src/process/slot_territory.rs` prove the PREDICATE is slot-boundary
    aware, but nothing proves the live reaper honours that boundary against a
    real concurrent pool build.

    HOW IT PROVES IT

      1. `POST /diagnostics/clear` so every assertion reads only this run's
         events.
      2. Discover the pool: `GET /builds` -> `pool_size`, `GET /health` ->
         `supervisor.project_dir`. The runner repo is the PARENT of
         `project_dir` (which points at `<runner-repo>/src-tauri`), and each
         slot territory is `<runner-repo>/target-pool/slot-<k>` - the same
         derivation `SupervisorConfig::runner_slot_target_dir` uses.
      3. Plant one long-lived `cargo check` "probe" per territory: one in the
         FOREIGN target dir (V1) and one in EVERY pool slot (V2).
      4. Trigger a real pool build: `POST /runners/spawn-test
         {"rebuild":true,"async":true}` - which returns a submission id in ~1s
         instead of blocking for the whole compile.
      5. POLL `GET /build/{id}/status` until terminal while ALSO polling
         `GET /diagnostics?filter=build_kill` every -PollIntervalSec, merging
         each read into a run-local de-duplicated event bag.
      6. Assert against the accumulated bag.

    WHY POLL INSTEAD OF ONE BLOCKING CALL

        Three separate defects collapse into this one restructure:

        * RING-BUFFER EVICTION. `DIAGNOSTICS_BUFFER_SIZE` is shared across ALL
          event kinds - `BuildStarted`, `BuildCompleted`, `Restart*` and the
          `build_kill` family all live in ONE ring, and filtering happens at
          READ time. The cleanup pass emits at the START of a build that then
          runs for 10-30 minutes, so a single read at the end can find the kill
          events already evicted by unrelated traffic. Worse, the eviction is
          INVISIBLE to a naive "did I hit the cap?" guard: the filtered read
          returns a SMALL array (only the surviving build_kill events), and the
          route computes `total` AFTER filter+limit so it just equals that same
          small count. An absence-check assertion would then report PASS -
          absence of evidence read as evidence of absence, on exactly the
          assertions whose whole job is to detect a wrongful kill. Polling every
          -PollIntervalSec captures each event long before a ring's worth of
          later events can evict it. (The ring was raised 200 -> 500 alongside
          this script, which widens the margin but does not remove the race -
          the poll loop is the actual fix. `limit=500` matches the ring; the
          route clamps `limit` to the ring size, so asking for more is
          harmless and asking for less silently under-reads.)

        * THE 40-MINUTE BLOCKING CALL. `rebuild:true` without `async` runs cargo
          INLINE on the HTTP connection.

        * THE CTRL-C LEAK. With a blocking call, >95% of the run's wall time sat
          inside one `Invoke-RestMethod`, and PowerShell 5.1 does not guarantee
          `finally` unwinds when the engine is interrupted inside a long .NET
          call - so a Ctrl-C leaked one detached `cargo.exe` per territory, each
          holding a build lock. The poll loop's longest uninterruptible window
          is now one `Start-Sleep -Seconds $PollIntervalSec`, so `finally` runs
          and teardown happens. The manifest + `-Cleanup` remain as a backstop.

    WHICH SLOT DID THE BUILD CLAIM

        Preferred: the spawn-test response's `build_slot_id` - the AUTHORITATIVE
        claimed slot, taken from `BuildAttempt.slot_id`. When it is present the
        claim is attributable to OUR build, and only then may the claimed-slot
        assertions (V2-1 / V2-2) report FAIL.

        Fallback, when the supervisor predates that field: infer from the
        `build_cleanup_summary` events, and ONLY when exactly one of them killed
        something. A summary with `killed: 0` is NOT evidence that we claimed
        that slot - on a box running ~9 concurrent sessions a peer's spawn-test
        routinely claims a slot and emits a `killed: 0` summary while our own
        build is still waiting on a permit. Treating that lone summary as "our"
        slot is how a harness reports exit 2 ("a real scoping defect") for
        nothing but benign peer concurrency. An unattributable claim is
        INCONCLUSIVE, never FAIL.

        The other reason to plant in EVERY slot rather than one guessed slot:
        the cross-slot check (V2-4) does NOT need the claimed slot to report a
        DEFECT. "No probe was killed by a cleanup pass belonging to a DIFFERENT
        slot" is FAIL-checkable unconditionally, so it still catches a
        machine-wide kill when no summary was emitted and no `build_slot_id`
        was returned. Its PASS arm is a different matter - see below.

    ATTRIBUTION GATES EVERY PASS, NOT JUST THE FAILS

        A `build_cleanup_summary` naming one of our slot territories is NOT
        evidence that OUR build ran a pass: on this box a peer's spawn-test
        emits one routinely. So "some pass was seen" (`$sawAnyCleanupPass`) is
        too weak to license a PASS on any check whose evidence is an ABSENCE -
        V1-1 (our probe is still alive), V1-2 (no kill event names it), V2-3
        (the siblings are still alive), V2-4 (nobody was killed cross-slot).
        Each of those is satisfied by a pass that never examined our probes,
        and would report PASS while proving nothing - the same vacuity as the
        bare absence-checks that `$sawAnyCleanupPass` was introduced to fix.

        The PASS arm of all four is therefore gated on
        `Test-AttributableCleanupPass`: `build_slot_id` came back (so the claim
        is OURS, not inferred over a peer's summary) AND a summary for that
        claimed slot is in the bag (so the pass demonstrably ran and was read).
        Missing either -> INCONCLUSIVE, with the detail naming WHICH was
        missing. FAIL arms are deliberately NOT gated: a kill that should not
        have happened is a defect whoever's pass emitted it.

        V1-3 is the one row left ungated, on purpose: its evidence is a
        POSITIVE event from the code under test (a pass reporting it spared N
        out-of-territory processes), not an absence, so a peer's pass is still
        real evidence. Its detail names whose pass accounted for it.

    THE PROBE: WHY NOT JUST `cargo check` THE RUNNER

        A probe has to be alive at the instant the cleanup pass runs. That pass
        is EARLY - `run_cargo_build_with_dir_detailed` reaps before it calls
        `run_build_inner`, so it precedes the whole `pnpm install` + `pnpm run
        build` frontend phase - but "early" is not "immediate": the probe must
        still outlive the build-pool permit wait plus the `origin/main`
        worktree fetch a default spawn-test performs first. A plain
        `cargo check` of the runner against a warm slot dir finishes in
        seconds; it would exit naturally and the run would prove nothing, and
        forcing it to do real work means either mutating the shared runner
        checkout or writing multi-GB of fresh artifacts.

        Instead each probe is `cargo check` on a throwaway crate whose
        `build.rs` SLEEPS. That is a genuine `cargo.exe` with
        `CARGO_TARGET_DIR` set to the territory - exactly what the reaper's
        predicate matches on (`process_targets_slot`, `SlotMatch::Env`) - with
        a deterministic lifetime, a few KB of disk, and zero mutation of any
        real checkout.

        NO fresh `CARGO_TARGET_DIR` is ever minted: every probe targets an
        ALREADY-EXISTING directory (a pool slot, or `-ForeignTargetDir`), every
        such path is normalized to a ROOTED absolute path first, and the script
        REFUSES to start if one of them is missing rather than creating it.
        Normalization is load-bearing, not hygiene: `GET /health` echoes
        `project_dir` verbatim and un-canonicalized (unlike
        `config.rs::runner_npm_dir`, which canonicalizes), so a supervisor
        launched with `-p ..\qontinui-runner\src-tauri` would yield a RELATIVE
        CARGO_TARGET_DIR - which cargo resolves against the probe crate's CWD
        under %TEMP%, minting exactly the virgin multi-GB target dir this
        preflight exists to prevent. The SAME hazard exists on the READ side:
        such a supervisor also emits a RELATIVE `territory` on every event, so
        `Test-TerritoryEquals` normalizes both sides and falls back to a
        trailing-segment compare when the event's path is not rooted.

    THE PROBE MUST REALLY BE `cargo.exe` - NOT THE RUSTUP SHIM

        `C:\Users\<you>\.cargo\bin\cargo.exe` is a 0-byte SYMLINK to
        `rustup.exe`, and the rustup proxy does NOT `exec` on Windows: it
        spawns the real toolchain `cargo.exe` as a CHILD and waits. Launching
        the shim therefore produces a process whose image is `rustup.exe`, and
        two independent things break at once:

          * `Test-ProbeAlive` name-checks `'cargo*'`, so EVERY live probe reads
            as dead; and
          * the reaper only enumerates `["cargo.exe", "rustc.exe"]`
            (`process/windows.rs`), so the pid it names in a kill event is the
            grandchild's, not the pid we recorded - and every pid-correlated
            assertion silently stops matching.

        Together those made every FAIL branch except V1-3 UNREACHABLE: a
        regression that reaped every cargo on the box would have printed PASS.
        So the script resolves the REAL binary via `rustup which cargo` and
        then ASSERTS the process image is `cargo.exe` right after spawning -
        turning this whole bug class into a loud preflight failure instead of a
        silent green run.

        Related, same root cause: `-WindowStyle` implies `UseShellExecute` and
        the rustup shim launched that way exits 1 IMMEDIATELY (reproduced 4/4).
        The probes are launched `-NoNewWindow` with `-RedirectStandardError`,
        so a plant failure prints the probe's own stderr instead of guessing at
        the cause.

    RUNTIME: 10-30 MINUTES.

        That is the compile itself; the HTTP calls are all short now. The poll
        deadline is `-BuildTimeoutSec` (default 2400s = 40min) to leave headroom
        over the supervisor's own 30-minute build timeout. Do not run this
        expecting a quick answer.

        UNLESS THE SUBMISSION FAILED. A spawn-test that never yields a
        submission id (503 `build_pool_full`, 400, supervisor restart mid-call)
        leaves nothing to poll to a terminal state, so the loop stops after two
        poll intervals instead of running out the deadline while every probe
        holds its territory's build lock. `Get-PollDeadline` makes that
        decision; the run reports the submission failure and exits INCONCLUSIVE
        rather than hanging for 40 minutes.

    BLAST RADIUS - READ BEFORE RUNNING ON A SHARED BOX

      * Every pool slot is lock-held. A probe holds its target dir's cargo
        build lock for as long as it lives, so a peer's `spawn-test` blocks
        behind it (the pool build we trigger frees only the slot it claims).
      * `-ForeignTargetDir` is lock-held too. The default is the supervisor's
        own SHARED `target\` - among the most contended dirs on the box. Any
        peer running `cargo check` / `cargo build` / `cargo-guard.sh` in
        `qontinui-supervisor` will sit on "Blocking waiting for file lock on
        build directory" for the whole run. Point `-ForeignTargetDir` at a
        quieter warm target dir if that matters.
      * `POST /diagnostics/clear` is GLOBAL supervisor state. Any peer session
        reading `GET /diagnostics` loses its history.
      * Probe artifacts (`<target>\debug\build\qontinui-cleanup-probe-*`) are
        left behind in every territory, once per run. A few KB each; teardown
        removes only the crate SOURCES.

      Teardown kills every process this script started. Ctrl-C is far safer
      than it used to be (see "WHY POLL INSTEAD OF ONE BLOCKING CALL"), but
      `-Cleanup` remains the recovery path for a hard kill or a crash.

      The script never touches the primary or any named runner. It spawns
      exactly one temp runner and stops it on the way out; teardown stops any
      `test-*` runner that appeared during the run but was not present before
      it, which covers the case where the build is still in flight when the
      poll deadline expires.

.PARAMETER Port
    Supervisor HTTP port. Default 9875.

.PARAMETER RunnerRepo
    Runner repo root (the parent of `src-tauri`), used to derive the slot
    territories as `<RunnerRepo>\target-pool\slot-<k>`. When omitted this is
    resolved from the live supervisor's `GET /health` ->
    `supervisor.project_dir`, which is authoritative; pass it only to override
    a supervisor that reports something unexpected. There is NO default: if
    /health carries no project_dir the script ABORTS rather than assuming a
    checkout, because this box holds several and probing the wrong pool makes
    V2-1 report a scoping defect that does not exist. Whatever the source, it
    is normalized to a rooted absolute path and the script refuses a path that
    cannot be rooted.

.PARAMETER ForeignTargetDir
    An EXISTING, warm target dir that is provably outside every slot
    territory - where V1's surviving foreign build is planted. Defaults to the
    supervisor's own D:\qontinui-root\qontinui-supervisor\target. The script
    refuses to run if it does not already exist (it will not mint one) or if
    it resolves inside `target-pool`. See BLAST RADIUS: the default is a
    contended dir.

.PARAMETER PoolSize
    Declare the build pool size. Normally read from `GET /builds` ->
    `pool_size`, else the QONTINUI_SUPERVISOR_BUILD_POOL_SIZE env var. There is
    NO default: if neither answers, the script ABORTS rather than assuming one.
    Pool size decides how many slot territories get a probe, and a guess that
    is too low makes assertion V2-3 (no cross-slot kills) pass while leaving a
    real slot unchecked. This parameter is the declared-intent escape hatch.

.PARAMETER SkipV1
    Skip the foreign-build-survives half. Use to re-run V2 alone after a
    failure.

.PARAMETER SkipV2
    Skip the in-territory-reap + sibling-isolation half. Use to re-run V1
    alone after a failure.

.PARAMETER DryRun
    Validate connectivity and slot discovery, print what WOULD be planted, and
    exit. Plants nothing, clears nothing, triggers no build.

.PARAMETER Cleanup
    Recovery mode: read the manifest the last run wrote, kill any probe still
    alive, sweep leftover probe build scripts, delete the scratch crates, and
    exit. Use after a Ctrl-C or a crash. Does nothing else - no discovery, no
    build, no supervisor calls, so it works while the supervisor is down.
    Unlike normal teardown this sweep is deliberately CROSS-RUN (it matches any
    `qontinui-cleanup-probe-*` build script), because recovering a leak whose
    manifest is itself missing is the entire job.

    A recorded probe is killed ONLY when its identity is verifiable - see
    `Test-ProbeIdentity`. A manifest entry with no start time is NOT killed.

.PARAMETER BuildTimeoutSec
    Deadline for the whole build, in seconds, measured from the moment the
    submission is accepted. Default 2400 (40 min). The build is polled, not
    awaited on one connection, so this bounds the POLL loop rather than an HTTP
    request. If it expires the build keeps running detached in the supervisor;
    the script says so and names the poll URL.

    It does NOT apply when the submission itself failed (a 503
    `build_pool_full`, a 400, a supervisor restart mid-call, or a 2xx carrying
    no submission id). There is no build to poll to a terminal state then, so
    the loop is bounded to two poll intervals and stops - see `Get-PollDeadline`.
    Waiting out the full deadline would hold a cargo build lock on every pool
    slot and on `-ForeignTargetDir` for 40 minutes on behalf of a run that can
    no longer prove anything about its own build.

.PARAMETER PollIntervalSec
    Seconds between `GET /build/{id}/status` + `GET /diagnostics` polls.
    Default 10. Lower it only if the box is emitting diagnostics fast enough
    to evict a `build_kill` event from the shared diagnostics ring inside the
    interval.

.PARAMETER ProbeLifetimeSecs
    How long each planted probe stays alive if nothing kills it. Default 0 =
    AUTO, which derives `-BuildTimeoutSec + 600` so a probe can never expire
    before the run finishes reading the telemetry. Setting it equal to
    -BuildTimeoutSec (the old default) guarantees expiry-before-read at the
    boundary: probes plant at T+0 and the deadline is at T+BuildTimeoutSec, so
    a slow-but-successful 39-minute build produced a full INCONCLUSIVE sweep
    after 40 minutes of compiling. An explicit value <= -BuildTimeoutSec is
    accepted but warned about.

.EXAMPLE
    .\scripts\verify-scoped-cleanup.ps1 -DryRun

.EXAMPLE
    .\scripts\verify-scoped-cleanup.ps1

.EXAMPLE
    .\scripts\verify-scoped-cleanup.ps1 -SkipV1

.EXAMPLE
    .\scripts\verify-scoped-cleanup.ps1 -Cleanup

.NOTES
    PowerShell 5.1 compatible: no `&&`, no ternary, no null-coalescing.

    Every plant is recorded in a manifest at
    %TEMP%\qontinui-verify-scoped-cleanup\last-run.json the moment it happens;
    run `-Cleanup` to sweep it. Probes also self-expire after
    -ProbeLifetimeSecs.

    Exit codes:
      0 - every executed assertion PASSED (or -DryRun / -Cleanup succeeded).
      1 - the run could not be performed or completed: supervisor unreachable,
          slot discovery failed, a required target dir does not exist, a probe
          could not be planted as a real cargo.exe, an unusable switch
          combination, or a mid-run abort (the `catch` path).
      2 - at least one assertion FAILED - a real scoping defect.
      3 - INCONCLUSIVE: nothing failed, but the run did not prove its claim.
          Typically: no cleanup pass ran at all; a pass ran but was a PEER's
          and so is not attributable to our build (the absence-checks cannot
          PASS off someone else's pass); the spawn-test submission failed; or a
          probe exited before the pass. Re-run.
          ALSO 3: every assertion passed but TEARDOWN LEAKED - a temp runner
          this run spawned is still listed in `GET /runners`, or could not be
          confirmed stopped. The assertion verdict is unaffected; the operator
          still has to clean up, so the run must not exit 0. The
          "TEARDOWN LEAKED" block names each runner and the exact stop command.
          A leak never DEMOTES 1 or 2 (see Resolve-TeardownExitCode).

    This script does NOT use taskkill /T anywhere. Tree kills on this machine
    have historically reaped sibling node/powershell processes - see the root
    CLAUDE.md "Never Kill Claude Code Processes" rule. Every teardown sweep is
    signature-scoped to this script's own probe crates.
#>
[CmdletBinding()]
param(
    [int]$Port = 9875,
    # No default on purpose -- see .PARAMETER RunnerRepo. An unbound value is
    # resolved from GET /health, and an unresolvable one aborts the preflight.
    [string]$RunnerRepo = '',
    [string]$ForeignTargetDir = 'D:\qontinui-root\qontinui-supervisor\target',
    [int]$PoolSize = 0,
    [switch]$SkipV1,
    [switch]$SkipV2,
    [switch]$DryRun,
    [switch]$Cleanup,
    [int]$BuildTimeoutSec = 2400,
    [int]$PollIntervalSec = 10,
    [int]$ProbeLifetimeSecs = 0
)

$ErrorActionPreference = 'Stop'

$base = "http://127.0.0.1:$Port"

function Write-Step($msg) { Write-Host "[verify-scoped-cleanup] $msg" }
function Write-Fail($msg) { Write-Host "[verify-scoped-cleanup] ERROR: $msg" -ForegroundColor Red }
function Write-Warn($msg) { Write-Host "[verify-scoped-cleanup] WARN: $msg" -ForegroundColor Yellow }

# ---------------------------------------------------------------------------
# Pure decision helpers.
#
# Kept free of HTTP and process I/O so they can be unit-tested with canned
# payloads - see scripts\verify-scoped-cleanup.Tests.ps1, which extracts them
# from this script's AST exactly the way restart-supervisor.Tests.ps1 does.
# ---------------------------------------------------------------------------

# Windows path equality: separator-agnostic, case-insensitive, trailing-sep
# tolerant. Mirrors the Rust predicate's normalization in
# `slot_territory::process_targets_slot`, so the harness and the code under
# test agree on what "same territory" means.
function Test-PathEquals {
    param([string]$A, [string]$B)
    if (-not $A) { return $false }
    if (-not $B) { return $false }
    $na = ($A -replace '/', '\').TrimEnd('\').ToLowerInvariant()
    $nb = ($B -replace '/', '\').TrimEnd('\').ToLowerInvariant()
    return ($na -eq $nb)
}

# Does an EVENT's `territory` denote the same territory as one of our rooted
# slot dirs?
#
# The event field is emitted verbatim from `slot.target_dir.display()`, so a
# supervisor launched with a relative `-p` (e.g. `-p ..\qontinui-runner\
# src-tauri`) emits a RELATIVE territory that can never string-equal our rooted
# path - and every summary would be discarded, silently costing us the claimed
# slot. The DESCRIPTION already flags this hazard for `project_dir` on the
# WRITE side; this is the read side of the same problem.
#
# We cannot root a relative territory ourselves (it is relative to the
# SUPERVISOR's cwd, not ours), so the fallback compares the trailing three
# segments - `<repo-basename>\target-pool\slot-<k>`, which is precisely the
# part that identifies a territory within a pool. A different checkout's pool
# has a different repo basename and still does not match.
function Test-TerritoryEquals {
    param([string]$Territory, [string]$SlotDir)
    if (-not $Territory) { return $false }
    if (-not $SlotDir) { return $false }
    if (Test-PathEquals -A $Territory -B $SlotDir) { return $true }

    $nt = ($Territory -replace '/', '\').TrimEnd('\')
    # A rooted territory that did not string-equal really is a different dir.
    if ([System.IO.Path]::IsPathRooted($nt)) { return $false }

    $tSegs = @($nt.Split('\') | Where-Object { $_ -and $_ -ne '.' -and $_ -ne '..' })
    $sSegs = @((($SlotDir -replace '/', '\').TrimEnd('\')).Split('\') | Where-Object { $_ })
    if ($tSegs.Count -lt 3) { return $false }
    if ($sSegs.Count -lt 3) { return $false }
    $tTail = ($tSegs[($tSegs.Count - 3)..($tSegs.Count - 1)] -join '\').ToLowerInvariant()
    $sTail = ($sSegs[($sSegs.Count - 3)..($sSegs.Count - 1)] -join '\').ToLowerInvariant()
    return ($tTail -eq $sTail)
}

# The slot territories a supervisor with this repo + pool size would use.
# Same derivation as SupervisorConfig::runner_slot_target_dir.
function Get-SlotTargetDirs {
    param([string]$RunnerRepoRoot, [int]$Size)
    $dirs = @()
    for ($k = 0; $k -lt $Size; $k++) {
        $dirs += (Join-Path $RunnerRepoRoot (Join-Path 'target-pool' "slot-$k"))
    }
    # Leading comma: force array semantics for 0/1 elements.
    return ,$dirs
}

# Decide the runner repo root, and record WHERE the answer came from.
#
# Same rule as Resolve-PoolSize below -- preflight discovery must never render
# an unknown as a confident-looking default. This used to WARN and fall back to
# a hardcoded `D:\qontinui-root\qontinui-runner` when /health carried no
# `supervisor.project_dir`. That guess decides where EVERY slot territory is,
# and this box holds several runner checkouts and worktrees: a wrong-but-
# existing repo plants V2's probes into a pool the build under test never
# touches, so the in-territory orphan is not reaped, V2-1 FAILs, and the run
# reports a scoping defect that does not exist -- the exact failure the unit
# tests exist to prevent.
#
# `SupervisorInfo.project_dir` (src/routes/health.rs) is a non-Option String, so
# every supervisor built from this tree answers. Reaching the abort means the
# responder is not one, which is precisely when a guess is wrong.
#
# Returns { Path, Source, Error }. A non-null Error is the caller's cue to fail
# the preflight; nothing here exits, so the function is safe to unit-test.
function Resolve-RunnerRepo {
    param(
        [string]$Override = '',
        [bool]$OverrideProvided = $false,
        $HealthBody = $null
    )
    if ($OverrideProvided) {
        return [PSCustomObject]@{
            Path   = $Override
            Source = '-RunnerRepo (explicit)'
            Error  = $null
        }
    }

    $projectDir = $null
    if ($HealthBody -and ($HealthBody.PSObject.Properties.Name -contains 'supervisor')) {
        $sup = $HealthBody.supervisor
        if ($sup -and ($sup.PSObject.Properties.Name -contains 'project_dir')) {
            $projectDir = $sup.project_dir
        }
    }

    if ($projectDir) {
        # project_dir points at <runner-repo>\src-tauri; the slot pool lives one
        # level up (SupervisorConfig::runner_npm_dir).
        $parent = Split-Path -Parent $projectDir
        if (-not $parent) {
            return [PSCustomObject]@{
                Path   = $null
                Source = $null
                Error  = "GET /health reported supervisor.project_dir = '$projectDir', which has no parent directory. The slot pool lives one level above project_dir, so this cannot be resolved. Declare the repo instead: -RunnerRepo <path>"
            }
        }
        return [PSCustomObject]@{
            Path   = $parent
            Source = "GET /health -> supervisor.project_dir ('$projectDir')"
            Error  = $null
        }
    }

    return [PSCustomObject]@{
        Path   = $null
        Source = $null
        Error  = @"
GET /health carried no supervisor.project_dir, so the runner repo is UNKNOWN.
  This script will NOT assume one. The repo root decides where every slot
  territory is; pointing at the wrong checkout plants V2's probes into a pool
  the build under test never touches, which makes V2-1 report a scoping defect
  that does not exist.
  Every supervisor built from this tree always reports project_dir, so a
  responder that omits it is not one. Declare it: -RunnerRepo <path>
"@
    }
}

# Decide the build pool size, and record WHERE the number came from.
#
# NO SILENT DEFAULT. This resolution used to end in a hardcoded pool size of
# three on a WARN -- a guess wearing the costume of a discovered value. Fleet
# policy `verification-and-evidence` / `unknown-must-not-render-as-a-default`:
# a read that cannot support its answer must render UNKNOWN, not a
# confident-looking value, and the consumer takes the conservative arm.
#
# Guessing LOW does not merely inconvenience, it manufactures a FALSE PASS on a
# safety assertion. Pool size decides how many slot territories get a probe.
# Guess 3 against a real pool of 4 and V2-3 ("orphans in the OTHER pool slots
# are still ALIVE") checks two siblings instead of three, so a wrongful kill in
# the unprobed slot goes unseen and the run reports PASS. The assertion whose
# whole job is to detect a cross-slot kill is exactly the one the guess blinds.
#
# `-ReadBuilds` is injected rather than called inline so this decision is unit-
# testable without a supervisor: extracting the function is what lets the tests
# drive every arm (flake-then-answer, contract miss, no source at all) instead
# of the arms being verified once by hand and never again.
#
# Returns { Size, Source, Error }, with the same contract as Resolve-RunnerRepo.
function Resolve-PoolSize {
    param(
        [int]$Override = 0,
        [scriptblock]$ReadBuilds = $null,
        [string]$EnvValue = '',
        [string]$BuildsUri = '/builds',
        [int]$Attempts = 3,
        [int]$RetryDelaySec = 2,
        [scriptblock]$OnWarn = $null
    )
    if ($Override -gt 0) {
        return [PSCustomObject]@{
            Size   = $Override
            Source = "-PoolSize $Override (explicit)"
            Error  = $null
        }
    }

    $size   = 0
    $source = $null

    if ($ReadBuilds) {
        # RETRY before concluding unknown. `GET /builds` is intermittent on this
        # fleet -- observed both timing out and answering within the same minute
        # -- and the silent-empty rule says to rerun with errors visible rather
        # than trust the first miss. Bounded attempts separate a flake from a
        # genuinely unavailable route; only the latter should stop the run.
        for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
            try {
                $builds = & $ReadBuilds
                if ($builds -and ($builds.PSObject.Properties.Name -contains 'pool_size')) {
                    $parsed = 0
                    if ([int]::TryParse("$($builds.pool_size)", [ref]$parsed)) {
                        $size   = $parsed
                        $source = "GET $BuildsUri -> pool_size (attempt $attempt)"
                        break
                    }
                    # A body that answered with a non-numeric pool_size is a
                    # CONTRACT miss like the missing-field case below, not a
                    # flake. Retrying cannot turn it into a number.
                    if ($OnWarn) { & $OnWarn "GET $BuildsUri returned pool_size = '$($builds.pool_size)', which is not an integer" }
                    break
                }
                # A 200 that omits the field is a CONTRACT miss, not a flake --
                # do not spend more attempts on a route that answered clearly.
                if ($OnWarn) { & $OnWarn "GET $BuildsUri returned no 'pool_size' property" }
                break
            } catch {
                if ($OnWarn) { & $OnWarn "GET $BuildsUri attempt $attempt/$Attempts failed: $($_.Exception.Message)" }
                if ($attempt -lt $Attempts -and $RetryDelaySec -gt 0) { Start-Sleep -Seconds $RetryDelaySec }
            }
        }
    }

    if ($size -le 0 -and $EnvValue) {
        # A bare [int] cast on a non-numeric value throws a raw cast stack trace
        # under $ErrorActionPreference = 'Stop'. Every other bad input in this
        # file gets a named preflight failure; so does this one.
        $parsedPool = 0
        if (-not [int]::TryParse($EnvValue, [ref]$parsedPool)) {
            return [PSCustomObject]@{
                Size   = 0
                Source = $null
                Error  = "QONTINUI_SUPERVISOR_BUILD_POOL_SIZE is '$EnvValue', which is not an integer. Unset it or pass -PoolSize."
            }
        }
        $size   = $parsedPool
        $source = 'QONTINUI_SUPERVISOR_BUILD_POOL_SIZE (env)'
    }

    if ($size -le 0 -or -not $source) {
        # Aborting is safe because the operator already has a documented
        # override: -PoolSize is a first-class parameter, so this is a
        # fix-or-declare prompt, not a dead end.
        return [PSCustomObject]@{
            Size   = 0
            Source = $null
            Error  = @"
could not establish the build pool size from any authoritative source.
  tried: GET $BuildsUri -> pool_size   x$Attempts (see the warnings above for why it did not answer)
         QONTINUI_SUPERVISOR_BUILD_POOL_SIZE  (unset, zero, or non-positive)
  This script will NOT assume a pool size. It decides how many slot
  territories get a probe, and guessing too low makes assertion V2-3
  (no cross-slot kills) pass while leaving a real slot unchecked.
  Fix the supervisor's /builds route, or declare it: -PoolSize <n>
"@
        }
    }

    return [PSCustomObject]@{ Size = $size; Source = $source; Error = $null }
}

# Resolve the REAL toolchain cargo.exe.
#
# `~\.cargo\bin\cargo.exe` is a 0-byte symlink to rustup.exe and the proxy does
# not exec - it spawns the real cargo as a child. Spawning the shim gives a
# process named `rustup`, which neither this script's liveness check nor the
# supervisor's reaper (which enumerates only cargo.exe / rustc.exe) will ever
# recognise. See "THE PROBE MUST REALLY BE cargo.exe" in the DESCRIPTION.
#
# Returns the bare string 'cargo' if rustup cannot answer - Start-Probe's
# post-spawn image assert then fails loudly rather than silently planting a
# process nothing can name.
function Resolve-CargoExe {
    $resolved = $null
    try {
        $out = & rustup which cargo
        if ($LASTEXITCODE -eq 0 -and $out) {
            $resolved = ([string](@($out)[0])).Trim()
        }
    } catch {
        $resolved = $null
    }
    if ($resolved -and (Test-Path $resolved)) { return $resolved }
    return 'cargo'
}

# Normalize the GET /diagnostics body to a flat event array.
function Get-DiagnosticEvents {
    param($Payload)
    if (-not $Payload) { return ,@() }
    $events = $Payload
    if ($Payload.PSObject.Properties.Name -contains 'events') { $events = $Payload.events }
    if (-not $events) { return ,@() }
    return ,@($events)
}

# Stable identity for one diagnostic event, so repeated polls of the same ring
# accumulate into a set instead of a pile of duplicates. `timestamp` alone is
# not enough (two kills inside the same millisecond are plausible), and the
# payload alone is not enough (a re-run can repeat a payload), so the signature
# is both.
function Get-EventSignature {
    param($Event)
    if (-not $Event) { return $null }
    $data = ''
    if (($Event.PSObject.Properties.Name -contains 'data') -and ($null -ne $Event.data)) {
        try { $data = ($Event.data | ConvertTo-Json -Depth 6 -Compress) } catch { $data = [string]$Event.data }
    }
    return "$($Event.timestamp)|$($Event.kind)|$data"
}

# Merge a poll's worth of events into the run-local bag. $Bag is an ordered
# dictionary (reference type) keyed by signature; returns how many were NEW.
#
# This is what makes the ring buffer's eviction a non-issue: each
# event only has to survive one -PollIntervalSec window rather than the whole
# 10-30 minute build.
function Merge-DiagnosticEvents {
    param($Bag, $NewEvents)
    if ($null -eq $Bag) { return 0 }
    $added = 0
    foreach ($e in @($NewEvents)) {
        if (-not $e) { continue }
        $sig = Get-EventSignature -Event $e
        if (-not $sig) { continue }
        if (-not $Bag.Contains($sig)) {
            $Bag[$sig] = $e
            $added++
        }
    }
    return $added
}

# Events at or after $SinceIso. Used to restrict cleanup summaries to the
# window in which OUR build could have emitted them: `submit_spawn` stamps
# `Running { started_at }` BEFORE awaiting the exec future (which contains the
# permit wait and the cleanup pass), so every summary of ours is >= started_at.
# Both timestamps come from the same supervisor process's clock, so there is no
# skew to allow for.
#
# An unparsable timestamp is KEPT, not dropped: safe-degrade toward including
# our own event rather than silently losing it.
function Select-EventsSince {
    param($Events, [string]$SinceIso)
    if (-not $SinceIso) { return ,@($Events) }
    $since = $null
    try {
        $since = [datetime]::Parse($SinceIso, [System.Globalization.CultureInfo]::InvariantCulture,
                                   [System.Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime()
    } catch {
        return ,@($Events)
    }
    $out = @()
    foreach ($e in @($Events)) {
        if (-not $e) { continue }
        $ts = $null
        try {
            $ts = [datetime]::Parse([string]$e.timestamp, [System.Globalization.CultureInfo]::InvariantCulture,
                                    [System.Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime()
        } catch {
            $out += $e
            continue
        }
        if ($ts -ge $since) { $out += $e }
    }
    return ,$out
}

# When the poll loop must stop.
#
# THE DEFECT THIS EXISTS TO PREVENT. `$submissionId` is $null whenever the
# `POST /runners/spawn-test` never yielded one - a 503 `build_pool_full`, a 400,
# a supervisor restart mid-call, or a response carrying neither `submission_id`
# nor `build_id`. In that state there is NOTHING to poll to a terminal state:
# `$buildState` stays pinned at 'unsubmitted', the `break` on
# 'succeeded'/'failed' is UNREACHABLE, and the loop's only remaining exit is the
# deadline. With the default -BuildTimeoutSec that kept every planted probe
# alive - and therefore the cargo build lock held on EVERY pool slot PLUS the
# supervisor's shared `target\` - for 40 minutes, for a run that can no longer
# say anything about our own build. On this box that stalls every peer's
# `spawn-test` for the whole window. Under the old blocking design a failed
# submission surfaced immediately; the async/polling restructure is what made it
# silent, so this is a regression guard, not a new feature.
#
# Submitted -> the full -BuildTimeoutSec, unchanged. Not submitted -> a couple
# of poll intervals: long enough to still capture a cleanup pass a PEER's build
# emits in that window (which is the only reason to poll at all once our own
# submission is gone), then fall through to the assertions, which correctly
# report INCONCLUSIVE because no cleanup pass is attributable to us.
function Get-PollDeadline {
    param($Now, $SubmissionId, [int]$BuildTimeoutSec, [int]$PollIntervalSec)
    $start = [datetime]$Now
    # Explicit null/empty test rather than a bare truthiness test: an id is an
    # opaque string and PowerShell's string->bool rules are not the contract we
    # want to depend on here.
    if (($null -ne $SubmissionId) -and ("$SubmissionId" -ne '')) {
        return $start.AddSeconds($BuildTimeoutSec)
    }
    $grace = 2 * $PollIntervalSec
    if ($grace -lt 1) { $grace = 1 }
    # A caller-shortened -BuildTimeoutSec must still be the ceiling: the failed
    # -submission window can never be LONGER than the window a real build gets.
    if ($grace -gt $BuildTimeoutSec) { $grace = $BuildTimeoutSec }
    return $start.AddSeconds($grace)
}

# Fold the cleanup summaries into the two facts V1-3 needs about `spared`: the
# strongest count any single pass reported, and whether any pass DIRECTLY named
# our foreign pid in its `spared_pids` sample.
#
# WHY THE COUNT STAYS THE ASSERTION AND THE PID LIST ONLY STRENGTHENS IT - THIS
# GAP IS DELIBERATE, DO NOT "FIX" IT INTO A FLAKE.
#
#   `spared_pids` exists because a COUNT is not attributable evidence: `spared`
#   counts every out-of-territory cargo/rustc on the box, so a peer's build
#   satisfies it just as well as our foreign probe does. The obvious
#   strengthening - assert `spared_pids -contains $foreignPid` - is WRONG here,
#   because the list is a SAMPLE, not the population: the Rust side caps it at
#   PID_LIST_CAP = 5. On this machine (~9 concurrent sessions, any of which can
#   have a cargo/rustc out of territory at the instant the pass runs) our
#   foreign probe can legitimately fall outside those 5 while being perfectly
#   spared, and the assertion would fail for a healthy reaper - on exactly the
#   busy box this harness is meant to run on.
#
#   So: presence UPGRADES the evidence string to a direct pid attribution;
#   absence falls back to the count comparison and is never itself a failure
#   signal.
function Get-SparedPidAttribution {
    param($Summaries, [int]$ProcessId)
    $maxSpared = -1
    $namesPid  = $false
    $sampled   = 0
    foreach ($s in @($Summaries)) {
        if (-not $s) { continue }
        if ([int]$s.spared -gt $maxSpared) { $maxSpared = [int]$s.spared }
        # Absent on a supervisor that predates the field - the count path below
        # is the fallback, so a missing list is not an error.
        if ($s.PSObject.Properties.Name -notcontains 'spared_pids') { continue }
        foreach ($sp in @($s.spared_pids)) {
            if ($null -eq $sp) { continue }
            $sampled++
            if ([int]$sp -eq $ProcessId) { $namesPid = $true }
        }
    }
    return [PSCustomObject]@{
        MaxSpared       = $maxSpared
        NamesPid        = $namesPid
        SampledPidCount = $sampled
    }
}

# Every `build_cleanup_summary` whose territory is one of OUR slot dirs. A
# concurrent peer build produces extras; the caller disambiguates rather than
# guessing.
function Resolve-ClaimedSlotSummaries {
    param($Events, $SlotDirs)
    $found = @()
    foreach ($e in $Events) {
        if (-not $e) { continue }
        if ($e.kind -ne 'build_cleanup_summary') { continue }
        if (-not $e.data) { continue }
        foreach ($dir in $SlotDirs) {
            if (Test-TerritoryEquals -Territory $e.data.territory -SlotDir $dir) {
                $found += $e.data
                break
            }
        }
    }
    return ,$found
}

# The authoritative claimed slot, if the supervisor reported one.
#
# `build_slot_id` comes from `BuildAttempt.slot_id` - the slot OUR build
# actually claimed. Deliberately NOT falling back to `build_result.slot_id`,
# which is `state.build_pool.last_successful_slot` read AFTER the build and can
# therefore name a slot a PEER's build finished into.
#
# The field may arrive on a build-FAILURE body as well as a success body (the
# supervisor now carries it on both, so attribution survives exactly when a
# cleanup pass demonstrably ran but the compile then failed). Nothing here cares
# which: the read is property-existence based, so an error-shaped body with the
# field parses, and one without it returns $null and falls through to inference.
function Get-BuildSlotIdFromSpawnBody {
    param($Body)
    if (-not $Body) { return $null }
    if ($Body.PSObject.Properties.Name -notcontains 'build_slot_id') { return $null }
    $v = $Body.build_slot_id
    if ($null -eq $v) { return $null }
    if ("$v" -eq '') { return $null }
    $parsed = 0
    if (-not [int]::TryParse("$v", [ref]$parsed)) { return $null }
    return $parsed
}

# Decide which slot the pool build claimed, and - critically - whether that
# claim is attributable to OUR build.
#
# Returns { SlotId; Source; Attributable; Note }.
#
#   Source 'build_slot_id'  -> authoritative, Attributable = $true.
#   Source 'summary_killed' -> INFERRED from the single cleanup summary that
#                              killed something. Not attributable: a peer build
#                              can emit that summary too.
#   Source 'none'           -> unknown.
#
# The `killed -gt 0` test is applied on the SINGLE-summary path as well as the
# multi-summary path. Without it, this exact benign sequence produced exit 2:
# `POST /diagnostics/clear`, probes plant over ~2s, in that window a PEER's
# spawn-test claims slot-2 and its cleanup spares a stray rustc, emitting
# `build_cleanup_summary{slot_id: 2, killed: 0}`; our own build then blocks on
# a permit for the full deadline. One summary -> claimedSlot = 2 -> our slot-2
# probe is alive because nothing ever claimed it -> V2-1 FAIL, "a real scoping
# defect", with nothing broken.
function Resolve-ClaimedSlot {
    param($Summaries, $BuildSlotId, [int]$SlotCount)

    if ($null -ne $BuildSlotId) {
        $b = [int]$BuildSlotId
        if ($b -ge 0 -and (($SlotCount -le 0) -or ($b -lt $SlotCount))) {
            return [PSCustomObject]@{
                SlotId       = $b
                Source       = 'build_slot_id'
                Attributable = $true
                Note         = "spawn-test reported build_slot_id=$b (authoritative: BuildAttempt.slot_id)"
            }
        }
        return [PSCustomObject]@{
            SlotId       = -1
            Source       = 'none'
            Attributable = $false
            Note         = "spawn-test reported build_slot_id=$b, outside this pool's 0..$($SlotCount - 1)"
        }
    }

    $withKills = @($Summaries | Where-Object { $_ -and ([int]$_.killed) -gt 0 })
    if ($withKills.Count -eq 1) {
        return [PSCustomObject]@{
            SlotId       = [int]$withKills[0].slot_id
            Source       = 'summary_killed'
            Attributable = $false
            Note         = "inferred from the only cleanup summary with kills (slot $([int]$withKills[0].slot_id), killed=$([int]$withKills[0].killed)); the supervisor reported no build_slot_id, so this is NOT attributable to our own build"
        }
    }
    if ($withKills.Count -gt 1) {
        return [PSCustomObject]@{
            SlotId       = -1
            Source       = 'none'
            Attributable = $false
            Note         = "$($withKills.Count) cleanup summaries killed something (concurrent peer build?); the claimed slot is ambiguous"
        }
    }
    return [PSCustomObject]@{
        SlotId       = -1
        Source       = 'none'
        Attributable = $false
        Note         = "no build_slot_id and no cleanup summary reported a kill in $(@($Summaries).Count) summary/summaries"
    }
}

# Did a cleanup pass that we can attribute to OUR OWN build actually run?
#
# Both halves are required, and neither implies the other:
#
#   * `Attributable` (from `Resolve-ClaimedSlot`) is true only when the
#     supervisor handed us `build_slot_id` - i.e. the claim came from OUR
#     `BuildAttempt.slot_id` rather than from inferring over summaries a peer
#     could equally have emitted; and
#   * a `build_cleanup_summary` for THAT slot must actually be in the bag, so
#     we know the pass ran and was observed rather than being emitted before
#     `POST /diagnostics/clear`, evicted from the ring, or never reached.
#
# THE VACUITY THIS CLOSES. `$sawAnyCleanupPass` alone only says "some pass
# naming one of our slot territories was seen" - and on a box with ~9
# concurrent sessions that pass is routinely a PEER's. Every absence-check
# ("no kill event names our foreign pid", "no probe was killed by a different
# slot's pass") is then satisfied by a pass that never examined our probes,
# reporting PASS while proving nothing. That is the same failure class as the
# original bare absence-checks: an assertion that passes because nothing
# relevant happened. So the PASS arm of every absence-check is gated on THIS,
# not on `$sawAnyCleanupPass`; unattributable is INCONCLUSIVE, exactly as
# V2-1/V2-2 already treat it.
function Test-AttributableCleanupPass {
    param($Summaries, $SlotResolution)
    if (-not $SlotResolution) { return $false }
    if (-not $SlotResolution.Attributable) { return $false }
    $claimed = [int]$SlotResolution.SlotId
    if ($claimed -lt 0) { return $false }
    foreach ($s in @($Summaries)) {
        if (-not $s) { continue }
        if ([int]$s.slot_id -eq $claimed) { return $true }
    }
    return $false
}

# The shared verdict for every ABSENCE-check in this harness - the rows whose
# evidence is "no event named X". One tested decision instead of four hand-rolled
# branch chains that can drift apart (they already had: V1-2 and V2-4 gated on
# `$sawAnyCleanupPass` while V1-1 and V2-3 gated on nothing at all).
#
# Reason codes exist so the CALLER can say which of the two preconditions was
# missing - "no cleanup pass ran at all" and "a pass ran but it was a peer's"
# are very different things to read off the assertion table, and collapsing
# them into one INCONCLUSIVE string is how a run gets re-run pointlessly.
#
# Order matters: a defect is a defect regardless of attribution. A kill event
# naming an out-of-territory pid, or a cross-slot kill, is a scoping failure
# whether OUR build or a peer's emitted it - so FAIL is decided before either
# gate.
function Resolve-AbsenceCheckVerdict {
    param($DefectSeen, $SawAnyCleanupPass, $AttributablePass)
    if ($DefectSeen)          { return [PSCustomObject]@{ Result = 'FAIL';         Reason = 'defect' } }
    if (-not $SawAnyCleanupPass) { return [PSCustomObject]@{ Result = 'INCONCLUSIVE'; Reason = 'no_pass' } }
    if (-not $AttributablePass)  { return [PSCustomObject]@{ Result = 'INCONCLUSIVE'; Reason = 'unattributed' } }
    return [PSCustomObject]@{ Result = 'PASS'; Reason = 'proved' }
}

# Every build_process_killed event naming this pid, regardless of slot. Used
# both to confirm the claimed-slot reap and to prove no foreign pid was
# touched.
function Find-KillEventsForPid {
    param($Events, [int]$ProcessId)
    $hits = @()
    foreach ($e in $Events) {
        if (-not $e) { continue }
        if ($e.kind -ne 'build_process_killed') { continue }
        if (-not $e.data) { continue }
        if ([int]$e.data.pid -eq $ProcessId) { $hits += $e }
    }
    return ,$hits
}

# THE core defect detector: a probe killed by a cleanup pass belonging to a
# DIFFERENT slot than the one it was planted in.
#
# Deliberately independent of which slot the build claimed, because that is
# only knowable from a `build_slot_id` an older supervisor does not send, or a
# `build_cleanup_summary` that a machine-wide-kill regression might never emit.
# A cleanup that reaped every slot's probe must surface as a FAIL even when the
# claimed slot is unidentifiable - otherwise the worst regression reports as a
# single bland INCONCLUSIVE.
#
# A probe killed by its OWN slot's pass is deliberately NOT reported here: that
# is either our pool build doing its job, or a peer's build legitimately
# claiming that slot. On a box running ~9 concurrent sessions the latter is
# routine, and treating it as a defect would make a false FAIL the common case.
function Find-CrossSlotKills {
    param($Events, $SlotProbes)
    $bad = @()
    foreach ($p in $SlotProbes) {
        if (-not $p) { continue }
        foreach ($e in $Events) {
            if (-not $e) { continue }
            if ($e.kind -ne 'build_process_killed') { continue }
            if (-not $e.data) { continue }
            if ([int]$e.data.pid -ne [int]$p.ProcessId) { continue }
            if ([int]$e.data.slot_id -ne [int]$p.SlotId) {
                $bad += "$($p.Role) pid=$($p.ProcessId) (planted in slot-$($p.SlotId)) was killed by slot $($e.data.slot_id)'s cleanup"
            }
        }
    }
    return ,$bad
}

# Does this kill event attribute the process to slot $SlotId's territory with
# the STRONG (`env`) evidence? The literal 'env' compare is why
# slot_territory.rs pins SlotMatch's serde spelling in a CI-enforced unit test:
# a rename would leave this returning $false forever.
function Test-KillEventForSlot {
    param($KillEvent, [int]$SlotId)
    if (-not $KillEvent) { return $false }
    if ($KillEvent.kind -ne 'build_process_killed') { return $false }
    $d = $KillEvent.data
    if (-not $d) { return $false }
    if ([int]$d.slot_id -ne $SlotId) { return $false }
    $terr = ($d.territory -replace '/', '\').TrimEnd('\').ToLowerInvariant()
    if (-not $terr.EndsWith("slot-$SlotId")) { return $false }
    if ($d.matched_by -ne 'env') { return $false }
    return $true
}

# Is the process we just observed at a recorded probe's pid REALLY that probe?
#
# Pure so it can be unit-tested with canned observations - the I/O wrapper is
# `Test-ProbeAlive`. Split out because the original inlined version was
# untestable, which is why finding #1 (`ProcessName` is 'rustup', not 'cargo')
# shipped green.
#
# Two guards, and BOTH must safe-degrade to $false:
#
#   * Name. A pid is recycled aggressively on a box that churns hundreds of
#     build processes.
#   * Start time. This is the actual PID-reuse guard. `Save-ProbeManifest`
#     persists it as `StartTimeIso` precisely so `-Cleanup` can rehydrate it;
#     a probe with NO recorded start time has an UNVERIFIABLE identity, and an
#     unverifiable identity is NOT a kill. That mirrors the Rust predicate's
#     posture ("Unknown is NOT a kill") and closes the concrete harm: pids
#     churn across ~9 concurrent sessions, a peer's `cargo build` lands on a
#     pid left in a stale manifest, and `-Cleanup` force-kills a peer's
#     20-minute build - exactly what this branch exists to prevent.
#
# The 2s tolerance absorbs Process.StartTime -> ISO -> DateTime round-tripping
# without meaningfully weakening the guard: a recycled pid's start time differs
# by the age of the run, not by milliseconds.
function Test-ProbeIdentity {
    param($Probe, [string]$ObservedName, $ObservedStartTime)
    if (-not $Probe) { return $false }
    if (-not $ObservedName) { return $false }
    if ($ObservedName -notlike 'cargo*') { return $false }
    # No recorded start time -> identity unverifiable -> never treat as ours.
    if (-not $Probe.StartTime) { return $false }
    # Unreadable observed start time (access denied => a DIFFERENT user's
    # process on this pid, or it exited between the two calls) -> also not ours.
    if ($null -eq $ObservedStartTime) { return $false }
    $delta = [math]::Abs(([datetime]$ObservedStartTime - [datetime]$Probe.StartTime).TotalSeconds)
    if ($delta -gt 2) { return $false }
    return $true
}

# `test-*` ids present in a GET /runners payload. Snapshotted before the spawn
# so teardown can stop the runners THIS run created without touching a peer's
# temp runner - the spawn response's id is unavailable when the build is still
# in flight at the poll deadline.
function Get-TempRunnerIds {
    param($RunnersPayload)
    if (-not $RunnersPayload) { return ,@() }
    $list = $RunnersPayload
    if ($RunnersPayload.PSObject.Properties.Name -contains 'runners') { $list = $RunnersPayload.runners }
    if (-not $list) { return ,@() }
    $ids = @()
    foreach ($r in @($list)) {
        if (-not $r) { continue }
        if ($r.PSObject.Properties.Name -notcontains 'id') { continue }
        if ($r.id -like 'test-*') { $ids += $r.id }
    }
    return ,$ids
}

# Which temp runners is this run allowed to stop?
#
# TWO INDEPENDENT SOURCES, deliberately unioned:
#
#   $OwnIds  - the `id` the spawn-test 202 handed back. Unambiguously OURS: the
#              supervisor assigns it at port reservation, BEFORE the build, so
#              it comes back even on the async path where the runner process
#              does not exist yet. This source needs no baseline.
#   diff     - ($After - $Baseline), the historical source. Needed because a
#              spawn whose response never arrived (a timeout on a request the
#              supervisor DID act on) still leaves a runner we own but cannot
#              name.
#
# $Baseline is $null for "we never got a snapshot" and an EMPTY ARRAY for
# "we snapshotted, there were none" - they cannot share a sentinel (see the
# declaration). With a $null baseline the diff is UNSAFE (every peer's runner
# would look new), so it is dropped - but $OwnIds still applies, which is the
# point of tracking it. Before, a failed baseline GET disabled teardown
# entirely and leaked the runner this script had just spawned by name.
function Get-RunnersToStop {
    param($Baseline, $After, $OwnIds)
    $out = @()
    foreach ($id in @($OwnIds)) {
        if ($null -eq $id) { continue }
        if ("$id" -eq '') { continue }
        if ($out -notcontains $id) { $out += $id }
    }
    if ($null -ne $Baseline) {
        foreach ($id in @($After)) {
            if ($null -eq $id) { continue }
            if (@($Baseline) -contains $id) { continue }
            if ($out -notcontains $id) { $out += $id }
        }
    }
    return ,$out
}

# Fold a teardown that left something behind into the process exit code.
#
# WHY A LEAK MUST MOVE THE EXIT CODE AT ALL. Teardown runs in `finally`, i.e.
# AFTER the assertion table has been printed and after $script:ExitCode has been
# decided. So on 2026-08-09 the harness leaked a temp runner and still reported
# "all assertions PASSED" and exited 0 - the leak was structurally invisible to
# every consumer of this script's result. #138 removed that run's CAUSE (a 415
# on the stop call); this removes the SILENCE, which is the part that let one
# bad call shape survive a green run.
#
# Escalation only, never demotion: a FAILED assertion (2) or an aborted run (1)
# is strictly more important than a leaked runner and must stay visible. 0 -> 3
# because 3 already means "the operator must look at this and re-run", which is
# exactly the disposition a leaked runner needs.
function Resolve-TeardownExitCode {
    param([int]$CurrentExitCode, [int]$LeakedCount)
    if ($LeakedCount -le 0) { return $CurrentExitCode }
    if ($CurrentExitCode -eq 0) { return 3 }
    return $CurrentExitCode
}

# ---------------------------------------------------------------------------
# Assertion ledger
# ---------------------------------------------------------------------------

$script:Assertions = @()

function Add-Assertion {
    param([string]$Id, [string]$Claim, [string]$Result, [string]$Detail)
    $script:Assertions += [PSCustomObject]@{
        Id     = $Id
        Claim  = $Claim
        Result = $Result
        Detail = $Detail
    }
}

# ---------------------------------------------------------------------------
# Probe lifecycle
# ---------------------------------------------------------------------------

$script:RunId        = [guid]::NewGuid().ToString('N').Substring(0, 8)
$script:HarnessRoot  = Join-Path $env:TEMP 'qontinui-verify-scoped-cleanup'
$script:ProbeRoot    = Join-Path $script:HarnessRoot $script:RunId
$script:ManifestPath = Join-Path $script:HarnessRoot 'last-run.json'
$script:Planted      = @()
# The real toolchain cargo.exe, NOT the rustup shim. Resolved once.
$script:CargoExe     = Resolve-CargoExe
# $null means "never snapshotted" (the GET failed) and disables runner teardown;
# an EMPTY ARRAY means "snapshotted, there were none", which must still allow
# teardown. They cannot share a sentinel: `-not @()` is $true in PowerShell, so
# a falsiness check would silently skip cleanup in the common zero-peer case.
$script:PreExistingTempRunners = $null
# The runner id(s) the spawn-test response named as ours. Independent of the
# baseline above, and the only source that survives a failed baseline GET.
$script:OurSpawnedRunnerIds = @()
# Populated by Stop-SpawnedRunners: one entry per temp runner this run may have
# left behind, each carrying WHY. Non-empty escalates the exit code (see
# Resolve-TeardownExitCode) and prints the teardown block after the report.
$script:LeakedRunners = @()
# Set to the build's terminal state so teardown can tell "our runner never
# appeared because the build was still compiling" (it may appear AFTER we exit)
# from "it never appeared and never will".
$script:BuildReachedTerminal = $false
# Run-scoped package name. The build-script exe path is derived from the
# PACKAGE name, not the crate source path, so a constant name would make one
# run's teardown sweep kill a concurrent run's build scripts.
$script:ProbePkg     = "qontinui-cleanup-probe-$script:RunId"
# Every probe package this harness has ever produced shares this stem - used
# only by -Cleanup's deliberate cross-run recovery sweep.
$script:ProbePkgStem = 'qontinui-cleanup-probe'
# Accumulated, de-duplicated build_kill events across every poll of this run.
$script:EventBag     = [ordered]@{}

# Persist the plant list the instant it changes. A hard kill (or a Ctrl-C in a
# window `finally` does not cover) is what makes this file the difference
# between a recoverable and an unrecoverable leak.
#
# `StartTimeIso` and `CrateDir` are load-bearing, not diagnostics: without the
# start time, `-Cleanup` rehydrates probes whose identity cannot be verified,
# and `Test-ProbeIdentity` then refuses to kill them (see its comment for the
# peer-build harm that refusal prevents).
function Save-ProbeManifest {
    try {
        if (-not (Test-Path $script:HarnessRoot)) {
            New-Item -ItemType Directory -Path $script:HarnessRoot -Force | Out-Null
        }
        $manifest = [PSCustomObject]@{
            run_id     = $script:RunId
            probe_root = $script:ProbeRoot
            probe_pkg  = $script:ProbePkg
            cargo_exe  = $script:CargoExe
            written_at = (Get-Date).ToString('o')
            probes     = @($script:Planted | Select-Object Role, SlotId, TargetDir, ProcessId, StartTimeIso, CrateDir)
        }
        $manifest | ConvertTo-Json -Depth 5 | Set-Content -Path $script:ManifestPath -Encoding utf8
    } catch {
        Write-Warn "could not write the probe manifest ($($_.Exception.Message)); a leak would not be recoverable via -Cleanup"
    }
}

# Materialize a throwaway crate whose build script sleeps. A FRESH directory
# per probe: cargo writes Cargo.lock into the package dir, so sharing one crate
# dir across concurrent invocations would race, and a fresh path also
# guarantees a cold fingerprint in the (already warm) target dir so the build
# script is really re-run instead of being skipped as up to date.
function New-ProbeCrate {
    param([string]$Name)

    $dir = Join-Path $script:ProbeRoot $Name
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $dir 'src') -Force | Out-Null

    # The bare `[workspace]` table is load-bearing: without it cargo walks
    # parent dirs looking for a workspace root, and any stray Cargo.toml above
    # %TEMP% makes every probe hard-fail with "current package believes it's in
    # a workspace when it's not" - which would surface only as
    # PLANT/INCONCLUSIVE.
    $cargoToml = @"
[package]
name = "$script:ProbePkg"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
path = "src/lib.rs"

[workspace]
"@

    $buildRs = @'
// Stand-in for a long-running cargo compile.
//
// `cargo check` runs this build script and waits for it, so the parent
// cargo.exe stays alive - with CARGO_TARGET_DIR pointing at the territory
// under test - for the whole sleep. That is exactly the process shape
// `slot_territory::process_targets_slot` matches via SlotMatch::Env.
fn main() {
    let secs: u64 = std::env::var("QONTINUI_PROBE_SLEEP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800);
    std::thread::sleep(std::time::Duration::from_secs(secs));
}
'@

    # UTF8 without BOM: a BOM in Cargo.toml is not reliably accepted by every
    # toml parser, and a silent parse failure here would look like "cargo never
    # started" at plant time.
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText((Join-Path $dir 'Cargo.toml'), $cargoToml, $utf8NoBom)
    [System.IO.File]::WriteAllText((Join-Path $dir 'build.rs'), $buildRs, $utf8NoBom)
    [System.IO.File]::WriteAllText((Join-Path $dir 'src\lib.rs'), "// intentionally empty`r`n", $utf8NoBom)

    return $dir
}

function Start-Probe {
    param([string]$Role, [int]$SlotId, [string]$TargetDir, [int]$LifetimeSecs)

    $crateDir = New-ProbeCrate -Name $Role
    $errLog   = Join-Path $script:ProbeRoot "$Role.stderr.log"

    # Start-Process cannot take an environment block, so set-and-restore in
    # this session; the child inherits it.
    $prevTarget = $env:CARGO_TARGET_DIR
    $prevSleep  = $env:QONTINUI_PROBE_SLEEP_SECS
    $proc = $null
    try {
        $env:CARGO_TARGET_DIR          = $TargetDir
        $env:QONTINUI_PROBE_SLEEP_SECS = "$LifetimeSecs"
        # -NoNewWindow, NOT -WindowStyle. -WindowStyle implies UseShellExecute,
        # and (a) that path cannot redirect stderr, so a plant failure has no
        # evidence to report, and (b) the rustup shim launched under
        # ShellExecute exits 1 IMMEDIATELY - reproduced 4/4 with these exact
        # args and cwd. The `PowhidSubExec.B` note in restart-supervisor.ps1
        # does NOT apply here: that Defender heuristic fires on
        # `powershell.exe -WindowStyle Hidden -ExecutionPolicy Bypass`
        # launching the UNSIGNED supervisor exe, not on cargo.exe.
        $proc = Start-Process -FilePath $script:CargoExe `
                              -ArgumentList @('check', '--offline', '--quiet') `
                              -WorkingDirectory $crateDir `
                              -NoNewWindow `
                              -RedirectStandardError $errLog `
                              -PassThru
    } finally {
        $env:CARGO_TARGET_DIR          = $prevTarget
        $env:QONTINUI_PROBE_SLEEP_SECS = $prevSleep
    }

    if (-not $proc) {
        throw "failed to start probe '$Role' from '$script:CargoExe'$(Get-ProbeStderrTail -ErrLog $errLog)"
    }

    # ASSERT the process image. This is the guard that turns finding #1's whole
    # bug class into a loud preflight failure: if we somehow launched the rustup
    # shim (or anything else), the reaper - which enumerates only cargo.exe /
    # rustc.exe - would never name this pid, and every pid-correlated assertion
    # below would be inert while still printing PASS.
    $img = $null
    try {
        $cim = Get-CimInstance Win32_Process -Filter "ProcessId = $($proc.Id)" -ErrorAction Stop
        if ($cim) { $img = $cim.Name }
    } catch {
        $img = $null
    }
    if ($img -ne 'cargo.exe') {
        $observed = $img
        if (-not $observed) { $observed = '<exited before it could be observed>' }
        # Kill the DIRECT CHILDREN first, then the process itself. If we
        # launched the rustup proxy it has already spawned the real cargo.exe
        # as a child and is merely waiting on it; killing only the parent would
        # orphan a cargo holding this territory's build lock - on the abort
        # path, which is precisely when nothing else will come along to sweep
        # it. Children are enumerated by ParentProcessId and filtered to the
        # two build images, so this is never a tree kill.
        try {
            $kids = Get-CimInstance Win32_Process -Filter "ParentProcessId = $($proc.Id)" -ErrorAction Stop
            foreach ($kid in $kids) {
                if ($kid.Name -in @('cargo.exe', 'rustc.exe')) {
                    Write-Warn "stopping orphaned probe child $($kid.Name) pid=$($kid.ProcessId)"
                    Stop-Process -Id $kid.ProcessId -Force -ErrorAction SilentlyContinue
                }
            }
        } catch { }
        try { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue } catch { }
        throw ("probe '$Role' pid $($proc.Id) is '$observed', not 'cargo.exe' - the reaper will never name it, " +
               "so every pid-correlated assertion would be inert. Launched '$script:CargoExe'." +
               (Get-ProbeStderrTail -ErrLog $errLog))
    }

    $startTime = $null
    try { $startTime = $proc.StartTime } catch { }
    $startIso = $null
    if ($startTime) { $startIso = $startTime.ToString('o') }

    $probe = [PSCustomObject]@{
        Role         = $Role
        SlotId       = $SlotId          # -1 for the foreign probe
        TargetDir    = $TargetDir
        ProcessId    = $proc.Id
        StartTime    = $startTime
        StartTimeIso = $startIso
        CrateDir     = $crateDir
        ErrLog       = $errLog
        PlantedAt    = (Get-Date)
    }
    $script:Planted += $probe
    Save-ProbeManifest
    Write-Step "planted probe '$Role' pid=$($proc.Id) image=cargo.exe CARGO_TARGET_DIR=$TargetDir"
    return $probe
}

# The probe's own stderr, for reporting a plant failure with evidence instead
# of the old guess ("is cargo on PATH?" - it always was).
function Get-ProbeStderrTail {
    param([string]$ErrLog, [int]$Lines = 12)
    if (-not $ErrLog) { return '' }
    if (-not (Test-Path $ErrLog)) { return " (no stderr log at $ErrLog)" }
    try {
        $tail = @(Get-Content -Path $ErrLog -Tail $Lines -ErrorAction Stop)
        if ($tail.Count -eq 0) { return ' (probe stderr was empty)' }
        return "`n  probe stderr tail:`n    " + ($tail -join "`n    ")
    } catch {
        return " (could not read $ErrLog : $($_.Exception.Message))"
    }
}

# Alive AND still the process we started. Delegates the decision to the pure
# `Test-ProbeIdentity` so the rules are unit-testable.
function Test-ProbeAlive {
    param($Probe)
    if (-not $Probe) { return $false }
    $proc = Get-Process -Id $Probe.ProcessId -ErrorAction SilentlyContinue
    if (-not $proc) { return $false }
    $observedStart = $null
    try { $observedStart = $proc.StartTime } catch { $observedStart = $null }
    return (Test-ProbeIdentity -Probe $Probe -ObservedName $proc.ProcessName -ObservedStartTime $observedStart)
}

# $PackageFilter scopes the build-script sweep. Normal teardown passes THIS
# run's package name so it cannot disturb a concurrent run; -Cleanup passes the
# shared stem on purpose, because recovering an orphan whose manifest is gone
# is the entire job.
function Stop-AllProbes {
    param([string]$PackageFilter = $script:ProbePkg, [string]$CrateRoot = $script:ProbeRoot)

    # 1. Our own cargo.exe probes, by the pid we captured at plant time.
    #
    # `Test-ProbeAlive` delegates to `Test-ProbeIdentity`, which refuses to
    # claim a pid whose identity it cannot VERIFY - no recorded start time (the
    # `Process.StartTime` read failed at plant time, or a -Cleanup manifest
    # entry predates `StartTimeIso`), or an unreadable start time on the process
    # observed now. That posture is correct and must stay: pids churn fast here
    # and a guess kills a peer's 20-minute build.
    #
    # But it applies to IN-SESSION teardown too, not just -Cleanup, and there
    # the consequence is a LEAK: such a probe is skipped here and goes on
    # holding its territory's cargo build lock until it self-expires after
    # -ProbeLifetimeSecs. Count and name them so that leak is visible rather
    # than silent.
    $unverified = @()
    foreach ($p in $script:Planted) {
        if (Test-ProbeAlive -Probe $p) {
            Write-Step "teardown: stopping probe '$($p.Role)' pid=$($p.ProcessId)"
            Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
            continue
        }
        # Not ours by identity. Separate "really gone" (the normal case, silent)
        # from "still present but unverifiable" (the leak, reported).
        $still = Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue
        if ($still) {
            $unverified += "$($p.Role) pid=$($p.ProcessId) (now running '$($still.ProcessName)')"
        }
    }
    if ($unverified.Count -gt 0) {
        Write-Warn "teardown LEFT $($unverified.Count) recorded probe pid(s) running because their identity could not be verified (unknown is NOT a kill - that pid may since have been recycled onto a peer's build): $($unverified -join '; '). If one of them really is our probe it self-expires after -ProbeLifetimeSecs; until then it holds that territory's cargo build lock. The signature-scoped build-script/rustc sweep below still runs."
    }

    # 2. The sleeping build-script child each probe leaves behind. The
    #    supervisor's reaper only kills cargo.exe/rustc.exe, so the claimed
    #    slot's build script SURVIVES its parent and must be swept here or it
    #    sleeps for the full -ProbeLifetimeSecs. Signature-scoped by exe path -
    #    never a tree kill.
    #
    #    The `Name = '...'` CIM filter is LOAD-BEARING, not stylistic: it is
    #    what keeps the `-like` sweep below from ever considering a process
    #    outside this one image. An un-narrowed `Get-CimInstance Win32_Process`
    #    piped into a `-like` match on a broad pattern will match the
    #    PowerShell host running this very script (its own command line
    #    contains the pattern) - a reviewing agent did exactly that and killed
    #    its own session. Never widen this filter without re-checking that.
    try {
        $scripts = Get-CimInstance Win32_Process -Filter "Name = 'build-script-build.exe'" -ErrorAction Stop
        foreach ($s in $scripts) {
            if ($s.ExecutablePath -and ($s.ExecutablePath -like "*$PackageFilter*")) {
                Write-Step "teardown: stopping probe build script pid=$($s.ProcessId)"
                Stop-Process -Id $s.ProcessId -Force -ErrorAction SilentlyContinue
            }
        }
    } catch {
        Write-Warn "could not enumerate build-script-build.exe for teardown: $($_.Exception.Message)"
    }

    # 3. Any rustc still compiling this run's probe crate. Matched on the FULL
    #    crate root path, which really is in the probe rustc's argv - an 8-hex
    #    run id alone would collide with the `-C metadata=` hashes that litter
    #    every peer's rustc command line. Same load-bearing `Name =` narrowing
    #    as step 2.
    if ($CrateRoot) {
        try {
            $rustcs = Get-CimInstance Win32_Process -Filter "Name = 'rustc.exe'" -ErrorAction Stop
            foreach ($r in $rustcs) {
                if ($r.CommandLine -and ($r.CommandLine -like "*$CrateRoot*")) {
                    Write-Step "teardown: stopping probe rustc pid=$($r.ProcessId)"
                    Stop-Process -Id $r.ProcessId -Force -ErrorAction SilentlyContinue
                }
            }
        } catch {
            Write-Warn "could not enumerate rustc.exe for teardown: $($_.Exception.Message)"
        }
    }

    # 4. The scratch crates.
    #
    # Retried: a probe's crate dir is its CWD and its stderr log is held open by
    # -RedirectStandardError, so for a moment after the kill Windows still
    # reports "being used by another process". One immediate attempt reliably
    # lost that race and left the scratch crates behind on every run.
    if ($CrateRoot -and (Test-Path $CrateRoot)) {
        $removed = $false
        $lastErr = ''
        for ($attempt = 1; $attempt -le 3; $attempt++) {
            try {
                Remove-Item -Recurse -Force $CrateRoot -ErrorAction Stop
                $removed = $true
                break
            } catch {
                $lastErr = $_.Exception.Message
                if ($attempt -lt 3) { Start-Sleep -Seconds 1 }
            }
        }
        if (-not $removed) {
            Write-Warn "could not remove $CrateRoot after 3 attempts : $lastErr (a few KB of scratch crate sources; -Cleanup sweeps them)"
        }
    }
}

# Stop every `test-*` runner this run is responsible for, then PROVE by read
# that they are gone. Anything still standing is recorded on
# $script:LeakedRunners, which moves the exit code (Resolve-TeardownExitCode).
#
# Targets come from Get-RunnersToStop: the id the spawn response named plus the
# snapshot diff, so a failed baseline GET no longer disables teardown for a
# runner we can name outright.
#
# WHY IT RE-READS INSTEAD OF TRUSTING THE 200. A stop that answers 200 has not
# necessarily finished: `stop_runner_by_id` can report a StillHeld process, and
# the whole reason this function is under review is that its previous failure
# mode was believing itself. "It returned without throwing" is not evidence the
# runner is gone; its absence from `GET /runners` is.
function Stop-SpawnedRunners {
    $after = @()
    $listFailed = $null
    try {
        $payload = Invoke-RestMethod -Method Get -Uri "$base/runners" -TimeoutSec 15
        $after = Get-TempRunnerIds -RunnersPayload $payload
    } catch {
        $listFailed = $_.Exception.Message
        Write-Warn "could not list runners for teardown: $listFailed"
    }

    if ($null -eq $script:PreExistingTempRunners) {
        Write-Warn 'no pre-spawn runner baseline; the snapshot diff is unusable (every peer''s runner would look new). Only runners the spawn response named as ours will be stopped.'
    }
    $baselineForDiff = $script:PreExistingTempRunners
    if ($null -ne $listFailed) {
        # No `after` list to diff against. Fall through with $null so
        # Get-RunnersToStop uses $OwnIds alone rather than diffing against @().
        $baselineForDiff = $null
    }

    $targets = Get-RunnersToStop -Baseline $baselineForDiff -After $after -OwnIds $script:OurSpawnedRunnerIds
    if (@($targets).Count -eq 0) {
        if ($null -ne $listFailed -and @($script:OurSpawnedRunnerIds).Count -eq 0) {
            # We can neither name nor discover a runner, and we cannot prove
            # there is none. UNKNOWN, not clean - the same rule the assertions
            # follow for an empty diagnostics read.
            $script:LeakedRunners += [PSCustomObject]@{
                Id     = '(unknown)'
                Reason = "could not list runners ($listFailed) and no spawn response named an id, so whether this run left a temp runner behind is UNKNOWN"
            }
        } else {
            Write-Step 'teardown: no temp runner of ours to stop'
        }
        return
    }

    foreach ($id in $targets) {
        Write-Step "teardown: stopping temp runner $id"
        try {
            # -ContentType + -Body are belt-and-braces, and the belt is the
            # interesting part. The stop route's body is OPTIONAL, but
            # `Invoke-RestMethod -Method Post` with no -Body still sends
            # `Content-Type: application/x-www-form-urlencoded` (the .NET
            # HttpWebRequest default), and axum's Option<Json<T>> answered 415
            # to any non-JSON content type even with zero bytes to parse. That
            # is what leaked a runner on 2026-08-09. The supervisor now accepts
            # an empty body whatever the content type (routes/optional_json.rs),
            # so this is no longer load-bearing against a current supervisor -
            # it is kept because this harness is routinely pointed at an OLDER
            # running supervisor, and spelling the call correctly costs nothing.
            Invoke-RestMethod -Method Post -Uri "$base/runners/$id/stop" `
                -ContentType 'application/json' -Body '{}' -TimeoutSec 60 | Out-Null
        } catch {
            Write-Warn "could not stop temp runner $id : $($_.Exception.Message)"
        }
    }

    # ---- Prove it, or say we could not ------------------------------------
    # Read TWICE with a settle in between, and only the second read can convict.
    # Registry removal is not guaranteed to be observable the instant the stop
    # response lands, and a leak report is loud (it escalates the exit code and
    # tells the operator to go clean up) — so a one-off race must not manufacture
    # one. The check is still "prove it by read": the settle only decides how
    # long we let the supervisor take, never whether we look.
    $stillListed = @()
    $readFailure = $null
    for ($attempt = 1; $attempt -le 2; $attempt++) {
        $readFailure = $null
        try {
            $payload = Invoke-RestMethod -Method Get -Uri "$base/runners" -TimeoutSec 15
            $stillListed = Get-TempRunnerIds -RunnersPayload $payload
        } catch {
            $readFailure = $_.Exception.Message
        }
        if ($null -ne $readFailure) { break }
        # Nothing of ours left: settled, no second read needed.
        $survivors = @($targets | Where-Object { @($stillListed) -contains $_ })
        if ($survivors.Count -eq 0) { break }
        if ($attempt -lt 2) {
            Write-Step "teardown: $($survivors.Count) runner(s) still listed; waiting 5s for the registry to settle before convicting"
            Start-Sleep -Seconds 5
        }
    }
    if ($null -ne $readFailure) {
        foreach ($id in $targets) {
            $script:LeakedRunners += [PSCustomObject]@{
                Id     = $id
                Reason = "stop was issued, but the confirming GET /runners failed ($readFailure), so whether it stopped is UNKNOWN"
            }
        }
        return
    }

    foreach ($id in $targets) {
        if (@($stillListed) -contains $id) {
            $script:LeakedRunners += [PSCustomObject]@{
                Id     = $id
                Reason = 'still present in GET /runners after the stop call AND after a 5s settle'
            }
        }
    }

    # A runner we NAMED but never saw listed, on a build that never reached a
    # terminal state, is the third leak class: the supervisor may still spawn it
    # after we exit, so "not listed" here does not mean "will never exist".
    if (-not $script:BuildReachedTerminal) {
        foreach ($id in @($script:OurSpawnedRunnerIds)) {
            if (@($stillListed) -contains $id) { continue }
            if (@($after) -contains $id) { continue }
            $script:LeakedRunners += [PSCustomObject]@{
                Id     = $id
                Reason = 'never appeared in GET /runners AND our build never reached a terminal state - the supervisor may still spawn it after this script exits'
            }
        }
    }
}

# ---------------------------------------------------------------------------
# -Cleanup: recovery mode. Runs before any discovery so it works even when the
# supervisor is down.
# ---------------------------------------------------------------------------

if ($Cleanup) {
    Write-Step 'cleanup mode: sweeping leftovers from a previous run'
    $manifest = $null
    if (Test-Path $script:ManifestPath) {
        try { $manifest = Get-Content -Raw $script:ManifestPath | ConvertFrom-Json } catch {
            Write-Warn "could not parse $script:ManifestPath : $($_.Exception.Message)"
        }
    } else {
        Write-Step "no manifest at $script:ManifestPath (nothing recorded); sweeping by signature only"
    }

    if ($manifest -and $manifest.probes) {
        $unverifiable = 0
        foreach ($p in @($manifest.probes)) {
            # Rehydrate the recorded start time - the PID-reuse guard. A
            # manifest entry without one is deliberately left with a $null
            # StartTime so `Test-ProbeIdentity` refuses to kill that pid.
            $st = $null
            if ($p.PSObject.Properties.Name -contains 'StartTimeIso' -and $p.StartTimeIso) {
                try {
                    $st = [datetime]::Parse([string]$p.StartTimeIso,
                                            [System.Globalization.CultureInfo]::InvariantCulture,
                                            [System.Globalization.DateTimeStyles]::RoundtripKind)
                } catch {
                    $st = $null
                }
            }
            if (-not $st) { $unverifiable++ }
            $crate = $null
            if ($p.PSObject.Properties.Name -contains 'CrateDir') { $crate = $p.CrateDir }
            $script:Planted += [PSCustomObject]@{
                Role         = $p.Role
                SlotId       = $p.SlotId
                TargetDir    = $p.TargetDir
                ProcessId    = [int]$p.ProcessId
                StartTime    = $st
                StartTimeIso = $p.StartTimeIso
                CrateDir     = $crate
                ErrLog       = $null
                PlantedAt    = $null
            }
        }
        Write-Step "manifest run_id=$($manifest.run_id) with $($script:Planted.Count) recorded probe(s)"
        if ($unverifiable -gt 0) {
            Write-Warn "$unverifiable recorded probe(s) carry no start time; their pids will NOT be killed (an unverifiable identity could be a peer's build that inherited the pid). The signature-scoped build-script/rustc sweep below still runs."
        }
    }

    $crateRoot = $script:HarnessRoot
    if ($manifest -and $manifest.probe_root) { $crateRoot = $manifest.probe_root }
    # Cross-run stem on purpose - see the -Cleanup parameter docs.
    Stop-AllProbes -PackageFilter $script:ProbePkgStem -CrateRoot $crateRoot

    if (Test-Path $script:ManifestPath) {
        try { Remove-Item -Force $script:ManifestPath -ErrorAction Stop } catch { }
    }
    Write-Step 'cleanup complete.'
    exit 0
}

# ---------------------------------------------------------------------------
# 1. Preflight: switch sanity, connectivity, slot discovery
# ---------------------------------------------------------------------------

$script:ExitCode = 0

# Checked BEFORE -DryRun so `-DryRun -SkipV1 -SkipV2` cannot report success for
# a configuration that could never run.
if ($SkipV1 -and $SkipV2) {
    Write-Fail '-SkipV1 and -SkipV2 together leave nothing to verify.'
    exit 1
}

if ($PollIntervalSec -lt 1) {
    Write-Fail "-PollIntervalSec must be >= 1 (got $PollIntervalSec)."
    exit 1
}

# Probe lifetime must OUTLIVE the read, not merely match the build deadline.
# The old default was equal to -BuildTimeoutSec, which guarantees
# expiry-before-read at the boundary: probes plant at T+0, the deadline is at
# T+BuildTimeoutSec, so a 39-minute build produced a full INCONCLUSIVE sweep.
if ($ProbeLifetimeSecs -le 0) {
    $ProbeLifetimeSecs = $BuildTimeoutSec + 600
    Write-Step "probe lifetime auto-derived: $ProbeLifetimeSecs s (-BuildTimeoutSec $BuildTimeoutSec + 600 headroom)"
} elseif ($ProbeLifetimeSecs -le $BuildTimeoutSec) {
    Write-Warn "-ProbeLifetimeSecs ($ProbeLifetimeSecs) is <= -BuildTimeoutSec ($BuildTimeoutSec): a probe can expire before the telemetry is read, turning a successful run into a full INCONCLUSIVE sweep. Omit it to auto-derive $($BuildTimeoutSec + 600)."
}

Write-Step "probe binary = $script:CargoExe"
if ($script:CargoExe -eq 'cargo') {
    Write-Warn "'rustup which cargo' did not resolve; falling back to PATH lookup of 'cargo', which on this box is a 0-byte symlink to rustup.exe. The post-spawn image assert will fail loudly if that is what gets launched."
}

# Normalize a discovered or operator-supplied path to a rooted absolute path.
# See the DESCRIPTION's note on /health echoing project_dir verbatim: a
# relative path here becomes a relative CARGO_TARGET_DIR, which cargo resolves
# against the probe crate's CWD under %TEMP% and thereby MINTS a target dir.
function ConvertTo-RootedPath {
    param([string]$Path, [string]$Label)
    if (-not $Path) {
        Write-Fail "$Label is empty."
        exit 1
    }
    $full = $null
    try { $full = [System.IO.Path]::GetFullPath($Path) } catch {
        Write-Fail "$Label '$Path' is not a valid path: $($_.Exception.Message)"
        exit 1
    }
    if (-not [System.IO.Path]::IsPathRooted($full)) {
        Write-Fail "$Label '$Path' does not resolve to a rooted absolute path."
        exit 1
    }
    return $full.TrimEnd('\')
}

Write-Step "supervisor base = $base"

$health = $null
try {
    $health = Invoke-RestMethod -Method Get -Uri "$base/health" -TimeoutSec 10
} catch {
    Write-Fail "GET $base/health failed: $($_.Exception.Message)"
    Write-Fail "Start the supervisor first (see CLAUDE.md 'Restarting the supervisor')."
    exit 1
}

# The supervisor is authoritative about its own project_dir; -RunnerRepo is an
# override, so only consult /health when the caller did NOT pass it. Both
# resolutions below are decided by extracted, unit-tested functions and neither
# has a fallback value: an unestablished repo or pool size aborts the preflight.
$repoRes = Resolve-RunnerRepo -Override $RunnerRepo `
                              -OverrideProvided ($PSBoundParameters.ContainsKey('RunnerRepo')) `
                              -HealthBody $health
if ($repoRes.Error) {
    Write-Fail $repoRes.Error
    exit 1
}
$RunnerRepo = $repoRes.Path
Write-Step "runner repo = $RunnerRepo (source: $($repoRes.Source))"

$RunnerRepo       = ConvertTo-RootedPath -Path $RunnerRepo -Label 'runner repo'
$ForeignTargetDir = ConvertTo-RootedPath -Path $ForeignTargetDir -Label '-ForeignTargetDir'

# Pool size MUST come from an authoritative source; the provenance is printed so
# the run log says on its face where the number came from.
$poolRes = Resolve-PoolSize -Override $PoolSize `
                            -ReadBuilds { Invoke-RestMethod -Method Get -Uri "$base/builds" -TimeoutSec 20 } `
                            -EnvValue "$env:QONTINUI_SUPERVISOR_BUILD_POOL_SIZE" `
                            -BuildsUri "$base/builds" `
                            -OnWarn { param($m) Write-Warn $m }
if ($poolRes.Error) {
    Write-Fail $poolRes.Error
    exit 1
}
$PoolSize = $poolRes.Size
Write-Step "pool size = $PoolSize (source: $($poolRes.Source))"

$slotDirs = Get-SlotTargetDirs -RunnerRepoRoot $RunnerRepo -Size $PoolSize

# HARD CONSTRAINT: never mint a target dir. A virgin multi-GB CARGO_TARGET_DIR
# is the known disk-full hazard on this machine, so a missing territory is a
# preflight failure, not something to create.
$missing = @()
foreach ($d in $slotDirs) {
    if (-not (Test-Path $d)) { $missing += $d }
}
if ($missing.Count -gt 0) {
    Write-Fail "these slot territories do not exist (this script will NOT create them):"
    foreach ($d in $missing) { Write-Fail "  $d" }
    Write-Fail "Run one pool build first (POST /runners/spawn-test {rebuild:true}), or pass -RunnerRepo / -PoolSize."
    exit 1
}

if (-not (Test-Path $ForeignTargetDir)) {
    Write-Fail "-ForeignTargetDir '$ForeignTargetDir' does not exist (this script will NOT create it)."
    Write-Fail "Point it at any already-warm target dir outside <runner-repo>\target-pool."
    exit 1
}

# The foreign probe is worthless if it is actually inside a slot territory.
foreach ($d in $slotDirs) {
    if (Test-PathEquals -A $ForeignTargetDir -B $d) {
        Write-Fail "-ForeignTargetDir '$ForeignTargetDir' IS slot territory '$d'; V1 would be vacuous."
        exit 1
    }
}
if ($ForeignTargetDir.ToLowerInvariant() -like '*\target-pool\*') {
    Write-Fail "-ForeignTargetDir '$ForeignTargetDir' is under target-pool\; pick a dir outside the build pool."
    exit 1
}

Write-Host ''
Write-Step 'discovered territories:'
Write-Host ("  foreign (V1) : {0}" -f $ForeignTargetDir)
for ($k = 0; $k -lt $slotDirs.Count; $k++) {
    Write-Host ("  slot-{0} (V2)  : {1}" -f $k, $slotDirs[$k])
}
Write-Host ''

if ($DryRun) {
    Write-Step '-DryRun: connectivity + discovery OK. Nothing planted, no build triggered.'
    exit 0
}

# ---------------------------------------------------------------------------
# 2..6. Clear -> plant -> submit -> poll -> assert. Everything from here is
# inside a try/catch/finally so a failure or a throw still tears down every
# process this script started.
# ---------------------------------------------------------------------------

try {
    # Snapshot peer temp runners BEFORE we spawn, so teardown can tell ours
    # apart even if the build is still in flight at the deadline.
    try {
        $script:PreExistingTempRunners = Get-TempRunnerIds -RunnersPayload (Invoke-RestMethod -Method Get -Uri "$base/runners" -TimeoutSec 15)
        Write-Step "pre-existing temp runners: $($script:PreExistingTempRunners.Count)"
    } catch {
        Write-Warn "could not snapshot existing temp runners: $($_.Exception.Message); teardown will skip runner cleanup"
    }

    Write-Step "POST $base/diagnostics/clear (GLOBAL supervisor state - see BLAST RADIUS)"
    Invoke-RestMethod -Method Post -Uri "$base/diagnostics/clear" -TimeoutSec 15 | Out-Null

    $foreignProbe = $null
    $slotProbes   = @()

    if (-not $SkipV1) {
        $foreignProbe = Start-Probe -Role 'foreign' -SlotId -1 -TargetDir $ForeignTargetDir -LifetimeSecs $ProbeLifetimeSecs
    }
    if (-not $SkipV2) {
        for ($k = 0; $k -lt $slotDirs.Count; $k++) {
            $slotProbes += (Start-Probe -Role "slot-$k" -SlotId $k -TargetDir $slotDirs[$k] -LifetimeSecs $ProbeLifetimeSecs)
        }
    }

    # Give cargo a moment to actually be up before we call the build, then
    # confirm every probe is live. A probe that never started makes its
    # assertions vacuous, so record that up front.
    Start-Sleep -Seconds 5
    $plantOk = $true
    $plantDetail = @()
    foreach ($p in $script:Planted) {
        if (-not (Test-ProbeAlive -Probe $p)) {
            $tail = Get-ProbeStderrTail -ErrLog $p.ErrLog
            Write-Warn "probe '$($p.Role)' (pid $($p.ProcessId)) is NOT alive 5s after planting$tail"
            $plantDetail += "$($p.Role) pid=$($p.ProcessId)"
            $plantOk = $false
        }
    }
    if (-not $plantOk) {
        Add-Assertion -Id 'PLANT' -Claim 'all probes alive before the pool build (precondition, not a claim about the reaper)' `
                      -Result 'INCONCLUSIVE' `
                      -Detail "died within 5s of planting: $($plantDetail -join ', '). Their stderr was printed above."
    } else {
        Add-Assertion -Id 'PLANT' -Claim 'all probes alive before the pool build (precondition, not a claim about the reaper)' `
                      -Result 'PASS' -Detail "$($script:Planted.Count) probe(s) live as cargo.exe"
    }

    # ---- Trigger a REAL pool build, ASYNC ----------------------------------
    # async:true returns a submission id in ~1s; the build runs detached in the
    # supervisor and we poll it. See "WHY POLL INSTEAD OF ONE BLOCKING CALL".
    # wait:false skips the post-build health poll; we only need the build, not
    # a healthy runner.
    Write-Step "POST $base/runners/spawn-test {rebuild:true, async:true}"
    $spawnBody = '{"rebuild": true, "wait": false, "async": true, "requester_id": "verify-scoped-cleanup"}'
    $spawnStart   = Get-Date
    $submissionId = $null
    $submitError  = $null
    try {
        $spawnResp = Invoke-RestMethod -Method Post -Uri "$base/runners/spawn-test" `
                                       -ContentType 'application/json' `
                                       -Body $spawnBody -TimeoutSec 120
        if ($spawnResp -and ($spawnResp.PSObject.Properties.Name -contains 'submission_id')) {
            $submissionId = $spawnResp.submission_id
        } elseif ($spawnResp -and ($spawnResp.PSObject.Properties.Name -contains 'build_id')) {
            # Defensive: a supervisor that ignored `async` answers synchronously.
            $submissionId = $spawnResp.build_id
        }
        # Record the runner id BEFORE anything else can fail. Both the async 202
        # and the sync 200 carry `id`; on the async path the supervisor assigns
        # it at port reservation, so it is known long before the runner process
        # exists. This is what lets teardown name our runner without a baseline.
        if ($spawnResp -and ($spawnResp.PSObject.Properties.Name -contains 'id') -and ("$($spawnResp.id)" -ne '')) {
            $script:OurSpawnedRunnerIds += $spawnResp.id
            Write-Step "spawn-test reserved runner id $($spawnResp.id) (recorded for teardown)"
        } else {
            Write-Warn 'spawn-test response carried no `id`; teardown falls back to the snapshot diff alone'
        }
        Write-Step "spawn-test accepted in $([int]((Get-Date) - $spawnStart).TotalSeconds)s (submission=$submissionId)"
    } catch {
        # A failed SUBMISSION still leaves whatever cleanup passes already ran
        # readable, so do not abort - but record that our build may never have
        # run, which is what keeps the absence-checks from reporting PASS.
        $submitError = $_.Exception.Message
        Write-Warn "spawn-test submission failed after $([int]((Get-Date) - $spawnStart).TotalSeconds)s: $submitError"
    }
    if (-not $submissionId -and -not $submitError) {
        # HTTP 2xx, but neither `submission_id` nor `build_id` came back. Same
        # consequence as a throw: nothing to poll to a terminal state.
        $submitError = 'spawn-test answered without a submission_id or build_id'
        Write-Warn $submitError
    }

    # ---- Poll: build status + diagnostics, accumulating events --------------
    # The deadline is NOT unconditionally -BuildTimeoutSec: with no submission
    # id there is no build to poll to a terminal state, so the loop is bounded
    # to a couple of poll intervals instead of pinning every slot's build lock
    # for 40 minutes. See Get-PollDeadline for the full rationale.
    $deadline          = Get-PollDeadline -Now (Get-Date) -SubmissionId $submissionId `
                                          -BuildTimeoutSec $BuildTimeoutSec -PollIntervalSec $PollIntervalSec
    $buildState        = 'unsubmitted'
    $buildStartedAtIso = $null
    $spawnOutcomeBody  = $null
    $pollCount         = 0
    $lastReport        = Get-Date

    if ($submissionId) {
        Write-Step "polling GET $base/build/$submissionId/status + /diagnostics every ${PollIntervalSec}s (deadline ${BuildTimeoutSec}s)..."
    } else {
        $graceSecs = [int][math]::Round(($deadline - (Get-Date)).TotalSeconds)
        Write-Warn "submission failed: $submitError; polling diagnostics for ~${graceSecs}s only (2 poll intervals), NOT the ${BuildTimeoutSec}s build deadline - every probe holds a build lock on its slot and on the shared target\, and a run with no build of its own can no longer prove anything by waiting."
        Add-Assertion -Id 'SUBMIT' -Claim 'the pool build we assert about was actually submitted' `
                      -Result 'INCONCLUSIVE' `
                      -Detail "POST /runners/spawn-test yielded no submission id ($submitError). Polled diagnostics for ~${graceSecs}s then stopped; nothing below is attributable to a build of ours."
    }

    while ((Get-Date) -lt $deadline) {
        $pollCount++

        # Accumulate telemetry FIRST, so the final poll after a terminal build
        # cannot miss an event emitted moments before it.
        try {
            $diagPayload = Invoke-RestMethod -Method Get -Uri "$base/diagnostics?filter=build_kill&limit=500" -TimeoutSec 20
            $added = Merge-DiagnosticEvents -Bag $script:EventBag -NewEvents (Get-DiagnosticEvents -Payload $diagPayload)
            if ($added -gt 0) {
                Write-Step "poll $pollCount : +$added new build_kill event(s) (bag = $($script:EventBag.Count))"
            }
        } catch {
            Write-Warn "poll $pollCount : GET /diagnostics failed: $($_.Exception.Message)"
        }

        if ($submissionId) {
            try {
                $st = Invoke-RestMethod -Method Get -Uri "$base/build/$submissionId/status" -TimeoutSec 20
                if ($st -and $st.status) {
                    if ($st.status.PSObject.Properties.Name -contains 'state') { $buildState = $st.status.state }
                    if (($st.status.PSObject.Properties.Name -contains 'started_at') -and $st.status.started_at) {
                        $buildStartedAtIso = $st.status.started_at
                    }
                }
                if ($st -and ($st.PSObject.Properties.Name -contains 'spawn') -and $st.spawn) {
                    if ($st.spawn.PSObject.Properties.Name -contains 'body') { $spawnOutcomeBody = $st.spawn.body }
                }
                if ($buildState -eq 'succeeded' -or $buildState -eq 'failed') {
                    Write-Step "build reached terminal state '$buildState' after $([int]((Get-Date) - $spawnStart).TotalSeconds)s"
                    break
                }
            } catch {
                Write-Warn "poll $pollCount : GET /build/$submissionId/status failed: $($_.Exception.Message)"
            }
        }

        if (((Get-Date) - $lastReport).TotalSeconds -ge 60) {
            Write-Step "still building: state=$buildState elapsed=$([int]((Get-Date) - $spawnStart).TotalSeconds)s bag=$($script:EventBag.Count) event(s)"
            $lastReport = Get-Date
        }
        Start-Sleep -Seconds $PollIntervalSec
    }

    # One last read so the terminal build's own events are certainly in the bag.
    try {
        $diagPayload = Invoke-RestMethod -Method Get -Uri "$base/diagnostics?filter=build_kill&limit=500" -TimeoutSec 20
        $added = Merge-DiagnosticEvents -Bag $script:EventBag -NewEvents (Get-DiagnosticEvents -Payload $diagPayload)
        if ($added -gt 0) { Write-Step "final read: +$added new build_kill event(s)" }
    } catch {
        Write-Warn "final GET /diagnostics failed: $($_.Exception.Message)"
    }

    $buildTerminal = ($buildState -eq 'succeeded' -or $buildState -eq 'failed')
    # Teardown reads this from `finally` to classify a runner we named but never
    # saw: a non-terminal build can still spawn it after we exit.
    $script:BuildReachedTerminal = $buildTerminal
    if (-not $submissionId) {
        Write-Warn "submission failed: $submitError; polled briefly ($pollCount poll(s), $([int]((Get-Date) - $spawnStart).TotalSeconds)s) then stopped rather than holding every slot's build lock to the ${BuildTimeoutSec}s deadline. Teardown runs now; re-run when the pool has a free slot."
    }
    if ($submissionId -and -not $buildTerminal) {
        Write-Warn "the ${BuildTimeoutSec}s deadline expired with the build in state '$buildState'. It is STILL RUNNING detached in the supervisor: poll GET $base/build/$submissionId/status, and stop the temp runner it eventually spawns if teardown below did not catch it."
    }

    $events = @($script:EventBag.Values)
    Write-Step "accumulated $($events.Count) distinct build_kill event(s) over $pollCount poll(s)"

    # Restrict cleanup summaries to the window our own build could have emitted
    # in. `Running { started_at }` is stamped BEFORE the permit wait and the
    # cleanup pass, so nothing of ours is excluded.
    $ourWindowEvents = Select-EventsSince -Events $events -SinceIso $buildStartedAtIso
    $summaries       = Resolve-ClaimedSlotSummaries -Events $ourWindowEvents -SlotDirs $slotDirs
    $sawAnyCleanupPass = ($summaries.Count -gt 0)

    $buildSlotId = Get-BuildSlotIdFromSpawnBody -Body $spawnOutcomeBody
    $slotRes     = Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId $buildSlotId -SlotCount $slotDirs.Count
    $claimedSlot = [int]$slotRes.SlotId

    # The gate every absence-check's PASS arm hangs on: a cleanup pass that is
    # BOTH observed AND attributable to our own build. `$sawAnyCleanupPass` is
    # kept separately only so the assertion details can distinguish "no pass at
    # all" from "a pass, but a peer's". See Test-AttributableCleanupPass.
    $attributablePass = Test-AttributableCleanupPass -Summaries $summaries -SlotResolution $slotRes

    Write-Step "claimed slot = $claimedSlot (source=$($slotRes.Source), attributable=$($slotRes.Attributable), attributable pass observed=$attributablePass)"
    if (-not $sawAnyCleanupPass) {
        Write-Warn 'NO build_cleanup_summary named one of our slot territories in this run - no cleanup pass is known to have executed, so every absence-check below is INCONCLUSIVE rather than PASS.'
    } elseif (-not $attributablePass) {
        Write-Warn "a build_cleanup_summary WAS observed, but none of them is attributable to our own build ($($slotRes.Note)). On this box a peer's spawn-test routinely emits one; a pass that never examined our probes cannot prove an absence-check, so those rows are INCONCLUSIVE rather than PASS."
    }

    # How many probes were legitimately OUT of the claimed slot's territory, and
    # therefore must appear in that pass's `spared` count.
    $expectedSpared = 0
    if (-not $SkipV1) { $expectedSpared += 1 }
    if (-not $SkipV2) {
        if ($claimedSlot -ge 0) { $expectedSpared += ($slotProbes.Count - 1) }
        else { $expectedSpared += $slotProbes.Count }
    }

    # ---- V1 assertions ------------------------------------------------------
    $foreignAlive = $false
    if ($SkipV1) {
        Add-Assertion -Id 'V1' -Claim 'foreign build survives a pool build' -Result 'SKIPPED' -Detail '-SkipV1'
    } else {
        $foreignAlive = Test-ProbeAlive -Probe $foreignProbe
        $foreignKills = Find-KillEventsForPid -Events $events -ProcessId $foreignProbe.ProcessId

        if ($foreignAlive) {
            # Liveness alone is NOT proof of the claim: "survives a pool build"
            # requires a pool build of OURS to have run its cleanup pass over
            # it. With no attributable pass the probe survived nothing, so this
            # row must not report PASS - same gate as the absence-checks below.
            $v11 = Resolve-AbsenceCheckVerdict -DefectSeen $false `
                                               -SawAnyCleanupPass $sawAnyCleanupPass `
                                               -AttributablePass $attributablePass
            if ($v11.Reason -eq 'no_pass') {
                $d11 = "pid $($foreignProbe.ProcessId) is alive, but NO cleanup pass ran in our pool this run - it survived nothing"
            } elseif ($v11.Reason -eq 'unattributed') {
                $d11 = "pid $($foreignProbe.ProcessId) is alive, but the only cleanup pass(es) observed are not attributable to our build ($($slotRes.Note)) - a peer's pass may never have examined it"
            } else {
                $d11 = "pid $($foreignProbe.ProcessId) alive after a cleanup pass OUR build ran (slot $claimedSlot)"
            }
            Add-Assertion -Id 'V1-1' -Claim 'foreign cargo (outside every slot) is still ALIVE after the pool build' `
                          -Result $v11.Result -Detail $d11
        } elseif ($foreignKills.Count -gt 0) {
            Add-Assertion -Id 'V1-1' -Claim 'foreign cargo (outside every slot) is still ALIVE after the pool build' `
                          -Result 'FAIL' `
                          -Detail "pid $($foreignProbe.ProcessId) was KILLED by slot $($foreignKills[0].data.slot_id) cleanup - scoping is broken"
        } else {
            Add-Assertion -Id 'V1-1' -Claim 'foreign cargo (outside every slot) is still ALIVE after the pool build' `
                          -Result 'INCONCLUSIVE' `
                          -Detail ("pid $($foreignProbe.ProcessId) is gone but no kill event names it - it likely exited on its own." + (Get-ProbeStderrTail -ErrLog $foreignProbe.ErrLog))
        }

        # Absence-check: only meaningful if a cleanup pass OF OURS actually ran.
        # Ungated this reports PASS ("unnamed in 0 event(s)") when spawn-test
        # 503'd on a full pool and NOTHING was ever checked; gated on
        # `$sawAnyCleanupPass` alone it still reports PASS off a PEER's pass
        # that never examined our probes. The per-assertion table is what gets
        # pasted into a PR body as proof, so both vacuities must read
        # INCONCLUSIVE.
        $v12 = Resolve-AbsenceCheckVerdict -DefectSeen ($foreignKills.Count -gt 0) `
                                           -SawAnyCleanupPass $sawAnyCleanupPass `
                                           -AttributablePass $attributablePass
        if ($v12.Reason -eq 'defect') {
            $d12 = "$($foreignKills.Count) kill event(s) name pid $($foreignProbe.ProcessId)"
        } elseif ($v12.Reason -eq 'no_pass') {
            $d12 = 'no cleanup pass is known to have run in our pool this run, so "no event names it" proves nothing'
        } elseif ($v12.Reason -eq 'unattributed') {
            $d12 = "$($summaries.Count) cleanup pass(es) were observed but NONE is attributable to our build ($($slotRes.Note)); a peer's pass not naming our pid proves nothing about ours"
        } else {
            $d12 = "pid $($foreignProbe.ProcessId) unnamed across $($events.Count) accumulated event(s), including the cleanup pass OUR build ran in slot $claimedSlot"
        }
        Add-Assertion -Id 'V1-2' -Claim 'no build_process_killed event names the foreign pid' `
                      -Result $v12.Result -Detail $d12

        # `spared` counts EVERY out-of-territory cargo/rustc, so a bare
        # `spared >= 1` would be satisfied by our own sibling probes and would
        # pass even if the foreign build HAD been killed. Compare against the
        # number of out-of-territory probes we actually planted instead.
        #
        # `spared_pids` (the pass's capped SAMPLE of the pids it spared) is used
        # only to STRENGTHEN the evidence when it happens to name our foreign
        # pid - never as the assertion itself. See Get-SparedPidAttribution for
        # why that gap is deliberate and must not be closed.
        #
        # V1-3 is deliberately NOT attribution-gated, unlike V1-1/V1-2/V2-3/V2-4.
        # It is not an absence-check: its evidence is a POSITIVE event emitted
        # by the code under test (a pass reporting it spared N out-of-territory
        # processes). A peer's pass is still a real pass by that same reaper, so
        # the count is real evidence rather than "nothing happened". The detail
        # string names whose pass accounted for it so the table never overclaims.
        $sparedAttr = Get-SparedPidAttribution -Summaries $summaries -ProcessId $foreignProbe.ProcessId
        $bestSpared = [int]$sparedAttr.MaxSpared
        if ($attributablePass) {
            $sparedWhose = "the accounting pass is OUR build's (slot $claimedSlot)"
        } else {
            $sparedWhose = "the accounting pass is NOT attributable to our build - a peer's pass sparing out-of-territory work is still real reaper evidence, which is why this row is not attribution-gated"
        }
        if ($sparedAttr.NamesPid) {
            $sparedEvidence = "spared_pids DIRECTLY names pid $($foreignProbe.ProcessId) - the pass ATTRIBUTED our foreign build as spared, it did not merely count something; max spared = $bestSpared (>= $expectedSpared); $sparedWhose"
        } else {
            $sparedEvidence = "max spared = $bestSpared (>= $expectedSpared); spared_pids did not name pid $($foreignProbe.ProcessId) in its $($sparedAttr.SampledPidCount)-pid sample, which is expected on a busy box (the Rust side caps the sample at 5) and is deliberately NOT asserted on; $sparedWhose"
        }
        if (-not $sawAnyCleanupPass) {
            Add-Assertion -Id 'V1-3' -Claim "a build_cleanup_summary accounts for all $expectedSpared out-of-territory probe(s) as spared" `
                          -Result 'INCONCLUSIVE' `
                          -Detail 'no build_cleanup_summary named one of our slot territories - did the pool build reach the cleanup pass?'
        } elseif ($bestSpared -ge $expectedSpared) {
            Add-Assertion -Id 'V1-3' -Claim "a build_cleanup_summary accounts for all $expectedSpared out-of-territory probe(s) as spared" `
                          -Result 'PASS' -Detail $sparedEvidence
        } elseif ($bestSpared -ge 1) {
            $shortfall = "max spared = $bestSpared but $expectedSpared probe(s) were out of territory - one may not have reached cargo before the pass"
            if ($sparedAttr.NamesPid) {
                $shortfall += "; spared_pids DOES name our foreign pid $($foreignProbe.ProcessId), so the V1 half is directly attributed and the shortfall is in the sibling count"
            }
            Add-Assertion -Id 'V1-3' -Claim "a build_cleanup_summary accounts for all $expectedSpared out-of-territory probe(s) as spared" `
                          -Result 'INCONCLUSIVE' -Detail $shortfall
        } elseif ($foreignAlive) {
            Add-Assertion -Id 'V1-3' -Claim "a build_cleanup_summary accounts for all $expectedSpared out-of-territory probe(s) as spared" `
                          -Result 'FAIL' `
                          -Detail 'a cleanup summary exists and reports spared 0 while our out-of-territory cargo was demonstrably live - the pass is not accounting for out-of-territory processes at all'
        } else {
            Add-Assertion -Id 'V1-3' -Claim "a build_cleanup_summary accounts for all $expectedSpared out-of-territory probe(s) as spared" `
                          -Result 'INCONCLUSIVE' `
                          -Detail 'spared 0, but our foreign probe was not alive at read time either - nothing to have spared'
        }
    }

    # ---- V2 assertions ------------------------------------------------------
    if ($SkipV2) {
        Add-Assertion -Id 'V2' -Claim 'in-territory orphan reaped, siblings spared' -Result 'SKIPPED' -Detail '-SkipV2'
    } else {
        # V2-4 first, and OUTSIDE the claimed-slot guard: it is the one check
        # that stays meaningful when neither build_slot_id nor a summary is
        # available, and it is the check that catches a machine-wide kill.
        #
        # The FAIL arm stays independent of attribution on purpose: a cross-slot
        # kill is a scoping defect whoever's build emitted the pass, and this is
        # the one row that must still catch a machine-wide kill when the claimed
        # slot is unidentifiable. Only the PASS arm is gated.
        $crossSlot = Find-CrossSlotKills -Events $events -SlotProbes $slotProbes
        $v24 = Resolve-AbsenceCheckVerdict -DefectSeen ($crossSlot.Count -gt 0) `
                                           -SawAnyCleanupPass $sawAnyCleanupPass `
                                           -AttributablePass $attributablePass
        if ($v24.Reason -eq 'defect') {
            $d24 = ($crossSlot -join '; ')
        } elseif ($v24.Reason -eq 'no_pass') {
            $d24 = "no cleanup pass is known to have run in our pool this run, so this absence-check over $($slotProbes.Count) probe(s) proves nothing"
        } elseif ($v24.Reason -eq 'unattributed') {
            $d24 = "$($summaries.Count) cleanup pass(es) were observed but NONE is attributable to our build ($($slotRes.Note)); a peer's pass leaving our $($slotProbes.Count) probe(s) alone proves nothing about ours"
        } else {
            $d24 = "checked $($slotProbes.Count) slot probe(s) against $($events.Count) event(s), including the cleanup pass OUR build ran in slot $claimedSlot"
        }
        Add-Assertion -Id 'V2-4' -Claim 'no probe was killed by a DIFFERENT slot''s cleanup pass' `
                      -Result $v24.Result -Detail $d24

        if ($claimedSlot -lt 0) {
            Add-Assertion -Id 'V2-0' -Claim 'the claimed slot is attributable to OUR build' `
                          -Result 'INCONCLUSIVE' -Detail "$($slotRes.Note). V2-4 above still ran."
        } else {
            if ($slotRes.Attributable) {
                Add-Assertion -Id 'V2-0' -Claim 'the claimed slot is attributable to OUR build' `
                              -Result 'PASS' -Detail $slotRes.Note
            } else {
                Add-Assertion -Id 'V2-0' -Claim 'the claimed slot is attributable to OUR build' `
                              -Result 'INCONCLUSIVE' `
                              -Detail "$($slotRes.Note). V2-1/V2-2 below therefore CANNOT report FAIL this run; rebuild the supervisor so spawn-test returns build_slot_id."
            }

            $claimedProbe = $slotProbes | Where-Object { $_.SlotId -eq $claimedSlot } | Select-Object -First 1
            if (-not $claimedProbe) {
                Add-Assertion -Id 'V2-1' -Claim 'the orphan planted in the CLAIMED slot is dead' `
                              -Result 'INCONCLUSIVE' -Detail "no probe was planted for slot $claimedSlot"
            } else {
                $claimedAlive = Test-ProbeAlive -Probe $claimedProbe
                $claimedKills = Find-KillEventsForPid -Events $events -ProcessId $claimedProbe.ProcessId

                if (-not $claimedAlive) {
                    Add-Assertion -Id 'V2-1' -Claim 'the orphan planted in the CLAIMED slot is dead' `
                                  -Result 'PASS' -Detail "pid $($claimedProbe.ProcessId) gone"
                } elseif ($slotRes.Attributable) {
                    Add-Assertion -Id 'V2-1' -Claim 'the orphan planted in the CLAIMED slot is dead' `
                                  -Result 'FAIL' `
                                  -Detail "pid $($claimedProbe.ProcessId) is STILL ALIVE in slot $claimedSlot, which OUR build claimed (build_slot_id) - the cleanup degraded into a no-op for its own territory"
                } else {
                    Add-Assertion -Id 'V2-1' -Claim 'the orphan planted in the CLAIMED slot is dead' `
                                  -Result 'INCONCLUSIVE' `
                                  -Detail "pid $($claimedProbe.ProcessId) is still alive in slot $claimedSlot, but that slot was only INFERRED - a peer build may own it while ours never got a permit. Not a defect claim."
                }

                $goodKill = $null
                foreach ($k in $claimedKills) {
                    if (Test-KillEventForSlot -KillEvent $k -SlotId $claimedSlot) { $goodKill = $k; break }
                }
                if ($goodKill) {
                    Add-Assertion -Id 'V2-2' -Claim "a build_process_killed event names that pid with matched_by=env and territory ending slot-$claimedSlot" `
                                  -Result 'PASS' `
                                  -Detail "pid $($goodKill.data.pid) method=$($goodKill.data.method) territory=$($goodKill.data.territory)"
                } elseif ($claimedKills.Count -gt 0) {
                    Add-Assertion -Id 'V2-2' -Claim "a build_process_killed event names that pid with matched_by=env and territory ending slot-$claimedSlot" `
                                  -Result 'FAIL' `
                                  -Detail "pid named, but slot_id=$($claimedKills[0].data.slot_id) matched_by=$($claimedKills[0].data.matched_by) territory=$($claimedKills[0].data.territory)"
                } elseif (-not $claimedAlive) {
                    Add-Assertion -Id 'V2-2' -Claim "a build_process_killed event names that pid with matched_by=env and territory ending slot-$claimedSlot" `
                                  -Result 'INCONCLUSIVE' `
                                  -Detail ("pid $($claimedProbe.ProcessId) is gone but NO kill event names it - it probably exited on its own before the cleanup pass." + (Get-ProbeStderrTail -ErrLog $claimedProbe.ErrLog))
                } elseif ($slotRes.Attributable) {
                    Add-Assertion -Id 'V2-2' -Claim "a build_process_killed event names that pid with matched_by=env and territory ending slot-$claimedSlot" `
                                  -Result 'FAIL' -Detail "no kill event names pid $($claimedProbe.ProcessId), which sat in the slot OUR build claimed"
                } else {
                    Add-Assertion -Id 'V2-2' -Claim "a build_process_killed event names that pid with matched_by=env and territory ending slot-$claimedSlot" `
                                  -Result 'INCONCLUSIVE' `
                                  -Detail "no kill event names pid $($claimedProbe.ProcessId), but slot $claimedSlot was only INFERRED as ours"
                }
            }

            # Sibling isolation - the half of #119 nothing else covers.
            #
            # A sibling killed by its OWN slot's pass is NOT a defect: on a box
            # with ~9 concurrent sessions a peer build routinely claims another
            # slot and correctly reaps our probe there. That case is recorded as
            # INCONCLUSIVE, never FAIL. Cross-slot kills are V2-4's job, above.
            #
            # The FAIL branch here is the one V2-4 structurally cannot see: a
            # sibling that is DEAD, that NO kill event names, in a slot NO
            # cleanup summary claims, whose own stderr is empty (so it did not
            # error out) and which had plenty of lifetime left (so it did not
            # expire). That is a kill with no telemetry at all - the reaper
            # reaching into a sibling territory without emitting the event that
            # would have made it a V2-4 FAIL.
            $siblings = @($slotProbes | Where-Object { $_.SlotId -ne $claimedSlot })
            if ($siblings.Count -eq 0) {
                Add-Assertion -Id 'V2-3' -Claim 'orphans in the OTHER pool slots are still ALIVE' `
                              -Result 'INCONCLUSIVE' -Detail 'pool size 1 - there are no sibling slots to isolate from'
            } else {
                $siblingPeerKilled = @()
                $siblingSelfExit   = @()
                $siblingSilentKill = @()
                foreach ($s in $siblings) {
                    $kills = Find-KillEventsForPid -Events $events -ProcessId $s.ProcessId
                    $ownSlotKill = $false
                    foreach ($k in $kills) {
                        if (Test-KillEventForSlot -KillEvent $k -SlotId $s.SlotId) { $ownSlotKill = $true; break }
                    }
                    if ($ownSlotKill) {
                        $siblingPeerKilled += "$($s.Role) pid=$($s.ProcessId) reaped by its OWN slot (concurrent peer build)"
                        continue
                    }
                    if (Test-ProbeAlive -Probe $s) { continue }

                    # Dead, unnamed. Which of the three benign causes?
                    $sawSummaryForSlot = $false
                    foreach ($sm in $summaries) {
                        if ([int]$sm.slot_id -eq [int]$s.SlotId) { $sawSummaryForSlot = $true; break }
                    }
                    $stderrEmpty = $true
                    if ($s.ErrLog -and (Test-Path $s.ErrLog)) {
                        try { if ((Get-Item $s.ErrLog).Length -gt 0) { $stderrEmpty = $false } } catch { $stderrEmpty = $false }
                    }
                    $expired = $false
                    if ($s.PlantedAt) {
                        if (((Get-Date) - $s.PlantedAt).TotalSeconds -ge ($ProbeLifetimeSecs - 60)) { $expired = $true }
                    } else {
                        $expired = $true
                    }

                    if ($stderrEmpty -and -not $expired -and -not $sawSummaryForSlot -and $sawAnyCleanupPass) {
                        $siblingSilentKill += "$($s.Role) pid=$($s.ProcessId) (slot $($s.SlotId)): dead, no kill event, no cleanup summary for its slot, empty stderr, lifetime not expired"
                    } else {
                        $siblingSelfExit += ("$($s.Role) pid=$($s.ProcessId)" + (Get-ProbeStderrTail -ErrLog $s.ErrLog -Lines 4))
                    }
                }

                # Same exposure as V1-1: "the siblings are alive" only proves
                # ISOLATION if a pass of ours ran and had the chance to reach
                # them. Alive siblings with no attributable pass are alive
                # because nothing happened, so the PASS arm is gated too. The
                # FAIL arm (silent kill) and the two benign INCONCLUSIVE causes
                # are decided first and are unchanged.
                if ($siblingSilentKill.Count -gt 0) {
                    Add-Assertion -Id 'V2-3' -Claim 'orphans in the OTHER pool slots are still ALIVE' `
                                  -Result 'FAIL' `
                                  -Detail "killed with NO telemetry (no build_process_killed event, no cleanup summary for that slot): $($siblingSilentKill -join '; ')"
                } elseif ($siblingPeerKilled.Count -gt 0) {
                    Add-Assertion -Id 'V2-3' -Claim 'orphans in the OTHER pool slots are still ALIVE' `
                                  -Result 'INCONCLUSIVE' `
                                  -Detail "not a scoping defect, but isolation was unobservable for: $($siblingPeerKilled -join '; ')"
                } elseif ($siblingSelfExit.Count -gt 0) {
                    Add-Assertion -Id 'V2-3' -Claim 'orphans in the OTHER pool slots are still ALIVE' `
                                  -Result 'INCONCLUSIVE' `
                                  -Detail "no kill event names them and they show a benign cause (stderr / expiry): $($siblingSelfExit -join '; ')"
                } else {
                    $v23 = Resolve-AbsenceCheckVerdict -DefectSeen $false `
                                                       -SawAnyCleanupPass $sawAnyCleanupPass `
                                                       -AttributablePass $attributablePass
                    if ($v23.Reason -eq 'no_pass') {
                        $d23 = "$($siblings.Count) sibling probe(s) alive, but NO cleanup pass ran in our pool this run - nothing exercised the isolation they are supposed to demonstrate"
                    } elseif ($v23.Reason -eq 'unattributed') {
                        $d23 = "$($siblings.Count) sibling probe(s) alive, but no observed cleanup pass is attributable to our build ($($slotRes.Note)) - slot $claimedSlot was only INFERRED, so their survival may mean nothing ever claimed a slot at all"
                    } else {
                        $d23 = "$($siblings.Count) sibling probe(s) alive across the cleanup pass OUR build ran in slot $claimedSlot"
                    }
                    Add-Assertion -Id 'V2-3' -Claim 'orphans in the OTHER pool slots are still ALIVE' `
                                  -Result $v23.Result -Detail $d23
                }
            }
        }
    }

    # ---- Report -------------------------------------------------------------
    Write-Host ''
    Write-Host '================ verify-scoped-cleanup: assertions ================'
    $script:Assertions | Format-Table -AutoSize -Wrap Id, Result, Claim, Detail | Out-String -Width 200 | Write-Host

    $failed = @($script:Assertions | Where-Object { $_.Result -eq 'FAIL' })
    $inconc = @($script:Assertions | Where-Object { $_.Result -eq 'INCONCLUSIVE' })

    if ($failed.Count -gt 0) {
        Write-Fail "$($failed.Count) assertion(s) FAILED - the slot cleanup is not correctly scoped."
        $script:ExitCode = 2
    } elseif ($inconc.Count -gt 0) {
        Write-Warn "$($inconc.Count) assertion(s) INCONCLUSIVE - nothing failed, but the run did not prove its claim."
        $script:ExitCode = 3
    } else {
        Write-Host "[verify-scoped-cleanup] all assertions PASSED." -ForegroundColor Green
        $script:ExitCode = 0
    }

    # ---- Raw events, for pasting into a PR body ------------------------------
    Write-Host ''
    Write-Host '================ build_kill events (paste into the PR) ================'
    if ($events.Count -eq 0) {
        Write-Host '(none)'
    } else {
        # -Depth 6 so the nested `data` object is rendered rather than elided as
        # a type name.
        $events | ConvertTo-Json -Depth 6 | Write-Host
    }
    Write-Host '======================================================================'
} catch {
    # $ErrorActionPreference = 'Stop' turns any cmdlet failure in the block
    # above into a terminating error. Catch it so the operator gets a named
    # cause and a deterministic exit code instead of a bare stack trace - and,
    # more importantly, so `finally` is reached and nothing is leaked.
    Write-Fail "aborted: $($_.Exception.Message)"
    Write-Fail $_.ScriptStackTrace
    $script:ExitCode = 1
} finally {
    Write-Host ''
    Write-Step 'teardown...'
    # PROBES FIRST, and each step in its own try/catch.
    #
    # Probes are the harmful leak: each holds a cargo build lock on a pool slot
    # or on the supervisor's shared target\, blocking every peer's build until
    # it expires. Stopping runners can block 15s+60s EACH, and a terminating
    # error inside it (an unexpected payload shape under
    # $ErrorActionPreference = 'Stop') used to abort the whole `finally` before
    # a single probe was killed.
    # NOTE: a probe whose identity is unverifiable (no readable StartTime) is
    # deliberately NOT killed here either - Stop-AllProbes reports how many it
    # had to leave running, and each of those holds a build lock until it
    # self-expires after -ProbeLifetimeSecs.
    try { Stop-AllProbes } catch { Write-Warn "probe teardown failed: $($_.Exception.Message)" }
    try { Stop-SpawnedRunners } catch {
        Write-Warn "temp-runner teardown failed: $($_.Exception.Message)"
        # A throw here means the function did not reach its own leak accounting,
        # so the state it left behind is UNKNOWN. Record it as a leak rather
        # than letting a crashed teardown read as a clean one.
        $script:LeakedRunners += [PSCustomObject]@{
            Id     = '(unknown)'
            Reason = "temp-runner teardown threw before it could account for itself: $($_.Exception.Message)"
        }
    }
    try {
        if (Test-Path $script:ManifestPath) { Remove-Item -Force $script:ManifestPath -ErrorAction Stop }
    } catch { }

    # ---- Teardown outcome: the one thing the report above CANNOT show -------
    # The assertion table and $script:ExitCode were decided before this block
    # ran, so a leak here would otherwise be invisible to every consumer of this
    # script (2026-08-09: leaked a runner, printed "all assertions PASSED",
    # exited 0). Escalate, and say exactly what to paste.
    if (@($script:LeakedRunners).Count -gt 0) {
        $before = $script:ExitCode
        $script:ExitCode = Resolve-TeardownExitCode -CurrentExitCode $script:ExitCode -LeakedCount @($script:LeakedRunners).Count
        Write-Host ''
        Write-Host '================ TEARDOWN LEAKED - operator action required ================' -ForegroundColor Red
        Write-Fail "$(@($script:LeakedRunners).Count) temp runner(s) may still be running. The assertion table above was printed BEFORE teardown and does not reflect this."
        foreach ($leak in $script:LeakedRunners) {
            Write-Fail "  $($leak.Id): $($leak.Reason)"
            if ($leak.Id -ne '(unknown)') {
                Write-Host "    stop it by hand: curl -X POST $base/runners/$($leak.Id)/stop -H 'Content-Type: application/json' -d '{}'"
            }
        }
        Write-Host "    then confirm: curl -s $base/runners | ConvertFrom-Json | %{ `$_.runners } | ? { `$_.id -like 'test-*' }"
        if ($script:ExitCode -ne $before) {
            Write-Fail "exit code raised $before -> $($script:ExitCode) because this run left state behind."
        } else {
            Write-Fail "exit code stays $($script:ExitCode) - a failed/aborted run outranks a leak, but the leak is real and still needs cleaning up."
        }
        Write-Host '===========================================================================' -ForegroundColor Red
    }
    Write-Step 'teardown complete.'
}

exit $script:ExitCode
