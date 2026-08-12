<#
Unit tests for the decision functions in verify-scoped-cleanup.ps1 - the
functions that turn `GET /diagnostics?filter=build_kill` plus a set of observed
processes into a verdict. The authoritative list is `$functionsToExtract`
below; do not maintain a duplicate list in this comment (an earlier version
listed six while the array held eight, which is exactly the kind of drift that
hides a coverage gap).

They must be correct even though the harness itself can only run on Windows
against a live supervisor - a bug here would report a scoping defect that does
not exist, or (worse) pass while the reaper is killing peer builds.

WHY Test-ProbeIdentity AND Resolve-CargoExe ARE COVERED HERE

    They are the two functions whose absence from this file let a structurally
    inert harness ship 100% green. `Test-ProbeAlive` name-checked `'cargo*'`
    while every probe was really `rustup.exe` (the ~\.cargo\bin\cargo.exe shim
    is a 0-byte symlink to rustup.exe and the proxy does not exec), so every
    live probe read as dead and every pid-correlated assertion went inert -
    while every test in this file hand-built pids (`New-KillEvent -ProcessId
    4242`) and therefore ASSUMED the exact invariant that was violated. The
    `Resolve-CargoExe` test below is two lines and would have caught it.

Functions are extracted from the script via its AST and defined directly in
this scope rather than dot-sourced (`. verify-scoped-cleanup.ps1`), because the
target script is a flat, unguarded top-level script: dot-sourcing it would
actually hit the supervisor, plant cargo processes and trigger a 30-minute
build - exactly the side effects a unit test must never cause. Same pattern as
restart-supervisor.Tests.ps1. Extracting a function only DEFINES it; nothing
here calls the ones with side effects.

PowerShell 5.1 / Pester 3.4.0 compatible: legacy `Should <verb>` syntax (no
leading dash), no BeforeAll/BeforeEach (not available in Pester 3).

Run with: Invoke-Pester scripts\verify-scoped-cleanup.Tests.ps1
#>

$ScriptUnderTest = Join-Path $PSScriptRoot 'verify-scoped-cleanup.ps1'

$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($ScriptUnderTest, [ref]$null, [ref]$parseErrors)
if ($parseErrors -and $parseErrors.Count -gt 0) {
    throw "Failed to parse $ScriptUnderTest for test extraction: $($parseErrors -join '; ')"
}

$functionsToExtract = @(
    'Test-PathEquals',
    'Test-TerritoryEquals',
    'Get-SlotTargetDirs',
    'Resolve-CargoExe',
    'Get-DiagnosticEvents',
    'Get-EventSignature',
    'Merge-DiagnosticEvents',
    'Select-EventsSince',
    'Get-PollDeadline',
    'Get-SparedPidAttribution',
    'Resolve-ClaimedSlotSummaries',
    'Get-BuildSlotIdFromSpawnBody',
    'Resolve-ClaimedSlot',
    'Test-AttributableCleanupPass',
    'Resolve-AbsenceCheckVerdict',
    'Find-KillEventsForPid',
    'Find-CrossSlotKills',
    'Test-KillEventForSlot',
    'Test-ProbeIdentity',
    'Test-ProbeAlive',
    'Start-Probe',
    'Get-TempRunnerIds',
    'Resolve-RunnerRepo',
    'Resolve-PoolSize',
    'Get-RunnersToStop',
    'Resolve-TeardownExitCode'
)
foreach ($name in $functionsToExtract) {
    $funcAst = $ast.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name },
        $true
    ) | Select-Object -First 1
    if (-not $funcAst) {
        throw "Could not find function '$name' in $ScriptUnderTest - has it been renamed or removed?"
    }
    Invoke-Expression $funcAst.Extent.Text
}

# --- payload builders -------------------------------------------------------
# Shapes match the wire format exactly: DiagnosticEvent serializes as
# {timestamp, kind, data} because DiagnosticEventKind is
# #[serde(tag = "kind", content = "data", rename_all = "snake_case")].

function New-KillEvent {
    param($ProcessId, $SlotId, $Territory, $MatchedBy = 'env', $Method = 'sysinfo', $Timestamp = '2026-07-28T12:00:00Z')
    [PSCustomObject]@{
        timestamp = $Timestamp
        kind      = 'build_process_killed'
        data      = [PSCustomObject]@{
            pid          = $ProcessId
            process_name = 'cargo.exe'
            cmd_snippet  = 'cargo.exe check'
            slot_id      = $SlotId
            territory    = $Territory
            matched_by   = $MatchedBy
            method       = $Method
        }
    }
}

function New-SummaryEvent {
    # -SparedPids $null omits the field entirely, which is the shape a
    # supervisor predating `spared_pids` emits - the count fallback must keep
    # working against it.
    param($SlotId, $Territory, $Killed, $Spared, $Timestamp = '2026-07-28T12:00:01Z', $SparedPids = $null)
    $data = [ordered]@{
        slot_id   = $SlotId
        territory = $Territory
        killed    = $Killed
        spared    = $Spared
    }
    if ($null -ne $SparedPids) { $data['spared_pids'] = @($SparedPids) }
    [PSCustomObject]@{
        timestamp = $Timestamp
        kind      = 'build_cleanup_summary'
        data      = [PSCustomObject]$data
    }
}

$Slot0 = 'D:\qontinui-root\qontinui-runner\target-pool\slot-0'
$Slot1 = 'D:\qontinui-root\qontinui-runner\target-pool\slot-1'
$Slot2 = 'D:\qontinui-root\qontinui-runner\target-pool\slot-2'
$AllSlots = @($Slot0, $Slot1, $Slot2)

Describe 'Test-PathEquals' {

    It 'matches identical paths' {
        Test-PathEquals -A $Slot0 -B $Slot0 | Should Be $true
    }

    It 'ignores case, separator style and a trailing separator' {
        Test-PathEquals -A 'd:/QONTINUI-ROOT/qontinui-runner/target-pool/slot-0/' -B $Slot0 | Should Be $true
    }

    It 'does not match a sibling slot' {
        Test-PathEquals -A $Slot0 -B $Slot1 | Should Be $false
    }

    It 'does not let slot-1 match slot-10 (prefix trap)' {
        $slot10 = 'D:\qontinui-root\qontinui-runner\target-pool\slot-10'
        Test-PathEquals -A $Slot1 -B $slot10 | Should Be $false
    }

    It 'treats a null or empty path as no match' {
        Test-PathEquals -A $null -B $Slot0 | Should Be $false
        Test-PathEquals -A $Slot0 -B '' | Should Be $false
    }
}

Describe 'Test-TerritoryEquals' {

    It 'matches a rooted territory exactly, like Test-PathEquals' {
        Test-TerritoryEquals -Territory $Slot1 -SlotDir $Slot1 | Should Be $true
    }

    It 'matches a RELATIVE territory from a supervisor launched with a relative -p' {
        # `slot.target_dir.display()` is emitted verbatim, so this is what a
        # `-p ..\qontinui-runner\src-tauri` supervisor really sends.
        Test-TerritoryEquals -Territory '..\qontinui-runner\target-pool\slot-1' -SlotDir $Slot1 | Should Be $true
    }

    It 'matches a relative forward-slash territory' {
        Test-TerritoryEquals -Territory '../qontinui-runner/target-pool/slot-2' -SlotDir $Slot2 | Should Be $true
    }

    It 'does not match a relative territory from a DIFFERENT checkout''s pool' {
        Test-TerritoryEquals -Territory '..\qontinui-runner-wt-e2e\target-pool\slot-1' -SlotDir $Slot1 | Should Be $false
    }

    It 'does not match a relative territory naming a sibling slot' {
        Test-TerritoryEquals -Territory '..\qontinui-runner\target-pool\slot-0' -SlotDir $Slot1 | Should Be $false
    }

    It 'still rejects a ROOTED territory that simply is not ours' {
        Test-TerritoryEquals -Territory 'D:\other-root\qontinui-runner\target-pool\slot-1' -SlotDir $Slot1 | Should Be $false
    }

    It 'rejects a relative territory too short to identify a pool' {
        Test-TerritoryEquals -Territory 'slot-1' -SlotDir $Slot1 | Should Be $false
    }
}

Describe 'Get-SlotTargetDirs' {

    # NB: no angle brackets in `It` names - Pester 4/5 read them as -ForEach
    # data placeholders, so a suite that outlives Pester 3 would render oddly.
    It 'derives repo\target-pool\slot-K for every slot' {
        $dirs = Get-SlotTargetDirs -RunnerRepoRoot 'D:\qontinui-root\qontinui-runner' -Size 3
        $dirs.Count | Should Be 3
        $dirs[0] | Should Be $Slot0
        $dirs[2] | Should Be $Slot2
    }

    It 'returns an array even for a single-slot pool' {
        $dirs = Get-SlotTargetDirs -RunnerRepoRoot 'D:\r' -Size 1
        $dirs.Count | Should Be 1
    }

    It 'returns an empty array for a zero pool rather than $null' {
        $dirs = Get-SlotTargetDirs -RunnerRepoRoot 'D:\r' -Size 0
        $dirs.Count | Should Be 0
    }
}

Describe 'Resolve-CargoExe' {

    # THE two-line test that would have caught the shipped blocker. The probe
    # must be the real ~30MB toolchain cargo.exe, never the 0-byte
    # ~\.cargo\bin\cargo.exe symlink to rustup.exe - a rustup-proxy probe is
    # named `rustup.exe`, which neither Test-ProbeAlive nor the supervisor's
    # cargo.exe/rustc.exe reaper will ever recognise.
    It 'resolves a real binary, not the 0-byte rustup shim' {
        $exe = Resolve-CargoExe
        $exe | Should Not Be 'cargo'
        (Test-Path $exe) | Should Be $true
        (Get-Item $exe).Length | Should Not Be 0
    }

    It 'resolves an exe actually named cargo.exe' {
        (Split-Path -Leaf (Resolve-CargoExe)) | Should Be 'cargo.exe'
    }
}

Describe 'Get-DiagnosticEvents' {

    It 'unwraps the {events, total} envelope' {
        $payload = [PSCustomObject]@{ events = @( (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 2) ); total = 1 }
        $events = Get-DiagnosticEvents -Payload $payload
        $events.Count | Should Be 1
        $events[0].kind | Should Be 'build_cleanup_summary'
    }

    It 'returns an empty array for a null payload' {
        (Get-DiagnosticEvents -Payload $null).Count | Should Be 0
    }

    It 'returns an empty array when the ring buffer was cleared' {
        $payload = [PSCustomObject]@{ events = @(); total = 0 }
        (Get-DiagnosticEvents -Payload $payload).Count | Should Be 0
    }
}

Describe 'Merge-DiagnosticEvents' {

    It 'accumulates across polls without duplicating a re-read event' {
        $bag = [ordered]@{}
        $e1 = New-KillEvent -ProcessId 4242 -SlotId 1 -Territory $Slot1 -Timestamp '2026-07-28T12:00:00Z'
        $e2 = New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 3 -Timestamp '2026-07-28T12:00:01Z'
        (Merge-DiagnosticEvents -Bag $bag -NewEvents @($e1, $e2)) | Should Be 2
        # Second poll re-reads the same ring contents.
        (Merge-DiagnosticEvents -Bag $bag -NewEvents @($e1, $e2)) | Should Be 0
        $bag.Count | Should Be 2
    }

    It 'RETAINS an event a later poll no longer returns - the ring-eviction fix' {
        # The whole point: the cleanup pass emits at build START and can be
        # evicted from the shared diagnostics ring long before a 30-minute build
        # ends. Once merged, it stays.
        $bag = [ordered]@{}
        $early = New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 1 -Spared 4 -Timestamp '2026-07-28T12:00:00Z'
        Merge-DiagnosticEvents -Bag $bag -NewEvents @($early) | Out-Null
        # A later poll returns a ring that has evicted it entirely.
        Merge-DiagnosticEvents -Bag $bag -NewEvents @() | Out-Null
        $bag.Count | Should Be 1
        (@($bag.Values)[0]).data.slot_id | Should Be 2
    }

    It 'distinguishes two kills that share a timestamp' {
        $bag = [ordered]@{}
        $a = New-KillEvent -ProcessId 111 -SlotId 1 -Territory $Slot1 -Timestamp '2026-07-28T12:00:00Z'
        $b = New-KillEvent -ProcessId 222 -SlotId 1 -Territory $Slot1 -Timestamp '2026-07-28T12:00:00Z'
        (Merge-DiagnosticEvents -Bag $bag -NewEvents @($a, $b)) | Should Be 2
    }

    It 'ignores null entries and a null bag' {
        $bag = [ordered]@{}
        (Merge-DiagnosticEvents -Bag $bag -NewEvents @($null, $null)) | Should Be 0
        (Merge-DiagnosticEvents -Bag $null -NewEvents @()) | Should Be 0
    }
}

Describe 'Select-EventsSince' {

    It 'keeps events at or after our build''s started_at' {
        $events = @(
            (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 0 -Spared 1 -Timestamp '2026-07-28T11:59:00Z'),
            (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 3 -Timestamp '2026-07-28T12:00:05Z')
        )
        $kept = Select-EventsSince -Events $events -SinceIso '2026-07-28T12:00:00Z'
        $kept.Count | Should Be 1
        $kept[0].data.slot_id | Should Be 1
    }

    It 'returns everything when no start time is known' {
        $events = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 0 -Spared 1) )
        (Select-EventsSince -Events $events -SinceIso $null).Count | Should Be 1
    }

    It 'KEEPS an event whose timestamp cannot be parsed rather than dropping ours' {
        $events = @( (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 3 -Timestamp 'not-a-timestamp') )
        (Select-EventsSince -Events $events -SinceIso '2026-07-28T12:00:00Z').Count | Should Be 1
    }
}

Describe 'Resolve-ClaimedSlotSummaries' {

    It 'finds the summary for the slot the pool build claimed' {
        $events = @(
            (New-KillEvent -ProcessId 4242 -SlotId 1 -Territory $Slot1),
            (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 3)
        )
        $found = Resolve-ClaimedSlotSummaries -Events $events -SlotDirs $AllSlots
        $found.Count | Should Be 1
        $found[0].slot_id | Should Be 1
        $found[0].spared | Should Be 3
    }

    It 'ignores a summary whose territory is not one of our slots (another checkout''s pool)' {
        $events = @( (New-SummaryEvent -SlotId 0 -Territory 'D:\other-root\qontinui-runner\target-pool\slot-0' -Killed 1 -Spared 0) )
        (Resolve-ClaimedSlotSummaries -Events $events -SlotDirs $AllSlots).Count | Should Be 0
    }

    It 'accepts a RELATIVE territory from a supervisor launched with a relative -p' {
        $events = @( (New-SummaryEvent -SlotId 1 -Territory '..\qontinui-runner\target-pool\slot-1' -Killed 1 -Spared 3) )
        (Resolve-ClaimedSlotSummaries -Events $events -SlotDirs $AllSlots).Count | Should Be 1
    }

    It 'ignores kill events - only the summary carries slot_id authoritatively' {
        $events = @( (New-KillEvent -ProcessId 1 -SlotId 2 -Territory $Slot2) )
        (Resolve-ClaimedSlotSummaries -Events $events -SlotDirs $AllSlots).Count | Should Be 0
    }

    It 'returns every matching summary when a peer build ran concurrently' {
        $events = @(
            (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 0 -Spared 4),
            (New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 1 -Spared 3)
        )
        (Resolve-ClaimedSlotSummaries -Events $events -SlotDirs $AllSlots).Count | Should Be 2
    }
}

Describe 'Get-BuildSlotIdFromSpawnBody' {

    It 'reads the authoritative build_slot_id off the spawn outcome body' {
        $body = [PSCustomObject]@{ id = 'test-abc'; build_slot_id = 2 }
        Get-BuildSlotIdFromSpawnBody -Body $body | Should Be 2
    }

    It 'accepts slot 0 rather than treating it as absent' {
        $body = [PSCustomObject]@{ build_slot_id = 0 }
        Get-BuildSlotIdFromSpawnBody -Body $body | Should Be 0
    }

    It 'returns null when the supervisor predates the field' {
        $body = [PSCustomObject]@{ id = 'test-abc'; build_result = [PSCustomObject]@{ slot_id = 1 } }
        (Get-BuildSlotIdFromSpawnBody -Body $body) | Should Be $null
    }

    It 'does NOT fall back to build_result.slot_id - that is last_successful_slot, read after the build and race-prone' {
        $body = [PSCustomObject]@{ build_result = [PSCustomObject]@{ slot_id = 1 } }
        (Get-BuildSlotIdFromSpawnBody -Body $body) | Should Be $null
    }

    It 'returns null for a null body or an explicit null field' {
        (Get-BuildSlotIdFromSpawnBody -Body $null) | Should Be $null
        (Get-BuildSlotIdFromSpawnBody -Body ([PSCustomObject]@{ build_slot_id = $null })) | Should Be $null
    }
}

Describe 'Resolve-ClaimedSlot' {

    It 'prefers build_slot_id and marks the claim ATTRIBUTABLE to our build' {
        $summaries = @(
            (New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 1 -Spared 3).data
        )
        $r = Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId 0 -SlotCount 3
        $r.SlotId | Should Be 0
        $r.Source | Should Be 'build_slot_id'
        $r.Attributable | Should Be $true
    }

    It 'does NOT claim a slot from a lone killed:0 summary - the benign-peer false FAIL' {
        # The reproduced scenario: a peer's spawn-test claims slot-2 in the
        # 2s plant window and spares a stray rustc, while OUR build blocks on a
        # permit for the whole deadline. Inferring slot 2 here made V2-1 report
        # FAIL / exit 2 with nothing broken.
        $summaries = @( (New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 0 -Spared 1).data )
        $r = Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId $null -SlotCount 3
        $r.SlotId | Should Be -1
        $r.Attributable | Should Be $false
    }

    It 'infers from the single summary that killed something, but NOT attributably' {
        $summaries = @( (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 3).data )
        $r = Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId $null -SlotCount 3
        $r.SlotId | Should Be 1
        $r.Source | Should Be 'summary_killed'
        $r.Attributable | Should Be $false
    }

    It 'refuses to guess when two summaries both killed something' {
        $summaries = @(
            (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 1 -Spared 2).data,
            (New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 1 -Spared 2).data
        )
        (Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId $null -SlotCount 3).SlotId | Should Be -1
    }

    It 'returns unknown when no summary was emitted at all' {
        $r = Resolve-ClaimedSlot -Summaries @() -BuildSlotId $null -SlotCount 3
        $r.SlotId | Should Be -1
        $r.Source | Should Be 'none'
    }

    It 'rejects a build_slot_id outside this pool rather than trusting it' {
        $r = Resolve-ClaimedSlot -Summaries @() -BuildSlotId 7 -SlotCount 3
        $r.SlotId | Should Be -1
        $r.Attributable | Should Be $false
    }

    It 'still prefers build_slot_id when summaries disagree with it' {
        $summaries = @( (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 3).data )
        (Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId 2 -SlotCount 3).SlotId | Should Be 2
    }
}

Describe 'Find-KillEventsForPid' {

    It 'finds the kill event naming the pid' {
        $events = @( (New-KillEvent -ProcessId 4242 -SlotId 1 -Territory $Slot1) )
        (Find-KillEventsForPid -Events $events -ProcessId 4242).Count | Should Be 1
    }

    It 'returns nothing for a pid that was spared - the V1 assertion' {
        $events = @( (New-KillEvent -ProcessId 4242 -SlotId 1 -Territory $Slot1) )
        (Find-KillEventsForPid -Events $events -ProcessId 9999).Count | Should Be 0
    }

    It 'returns nothing when no events were emitted at all' {
        (Find-KillEventsForPid -Events @() -ProcessId 4242).Count | Should Be 0
    }
}

Describe 'Find-CrossSlotKills' {

    # The probes the harness plants in each pool slot.
    function New-SlotProbe {
        param($SlotId, $ProcessId)
        [PSCustomObject]@{ Role = "slot-$SlotId"; SlotId = $SlotId; ProcessId = $ProcessId }
    }

    $probes = @(
        (New-SlotProbe -SlotId 0 -ProcessId 1000),
        (New-SlotProbe -SlotId 1 -ProcessId 1001),
        (New-SlotProbe -SlotId 2 -ProcessId 1002)
    )

    It 'reports nothing when no probe was killed at all' {
        (Find-CrossSlotKills -Events @() -SlotProbes $probes).Count | Should Be 0
    }

    It 'does NOT report a probe reaped by its OWN slot - that is the reaper working, or a peer build claiming that slot' {
        $events = @( (New-KillEvent -ProcessId 1001 -SlotId 1 -Territory $Slot1) )
        (Find-CrossSlotKills -Events $events -SlotProbes $probes).Count | Should Be 0
    }

    It 'reports a probe reaped by a DIFFERENT slot - the scoping defect' {
        $events = @( (New-KillEvent -ProcessId 1002 -SlotId 0 -Territory $Slot0) )
        $bad = Find-CrossSlotKills -Events $events -SlotProbes $probes
        $bad.Count | Should Be 1
        ($bad[0] -like '*slot-2*') | Should Be $true
        ($bad[0] -like "*killed by slot 0's cleanup*") | Should Be $true
    }

    It 'reports every victim of a machine-wide kill, without needing the claimed slot' {
        # One slot-0 pass that reaped all three probes: the worst regression,
        # and the case where no build_cleanup_summary may be trustworthy.
        $events = @(
            (New-KillEvent -ProcessId 1000 -SlotId 0 -Territory $Slot0),
            (New-KillEvent -ProcessId 1001 -SlotId 0 -Territory $Slot0),
            (New-KillEvent -ProcessId 1002 -SlotId 0 -Territory $Slot0)
        )
        # slot-0's own probe is legitimately reaped; the other two are not.
        (Find-CrossSlotKills -Events $events -SlotProbes $probes).Count | Should Be 2
    }

    It 'ignores non-kill events' {
        $events = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 3 -Spared 0) )
        (Find-CrossSlotKills -Events $events -SlotProbes $probes).Count | Should Be 0
    }
}

Describe 'Test-ProbeIdentity' {

    $now = Get-Date

    function New-Probe {
        param($StartTime, $ProcessId = 4242)
        [PSCustomObject]@{ Role = 'slot-0'; SlotId = 0; ProcessId = $ProcessId; StartTime = $StartTime }
    }

    It 'accepts a live cargo probe whose start time matches' {
        Test-ProbeIdentity -Probe (New-Probe -StartTime $now) -ObservedName 'cargo' -ObservedStartTime $now | Should Be $true
    }

    It 'REJECTS a probe with no recorded start time - unverifiable identity is NOT a kill' {
        # -Cleanup rehydrates from the manifest; before StartTimeIso was
        # persisted, every rehydrated probe had StartTime = $null and only the
        # name check remained. On a box churning pids across ~9 sessions that
        # let -Cleanup force-kill a PEER's 20-minute cargo build - the exact
        # harm this branch exists to prevent.
        Test-ProbeIdentity -Probe (New-Probe -StartTime $null) -ObservedName 'cargo' -ObservedStartTime $now | Should Be $false
    }

    It 'rejects a recycled pid now held by a process started much later' {
        Test-ProbeIdentity -Probe (New-Probe -StartTime $now) -ObservedName 'cargo' -ObservedStartTime $now.AddMinutes(7) | Should Be $false
    }

    It 'rejects the rustup shim, which is what the pid really was before the fix' {
        Test-ProbeIdentity -Probe (New-Probe -StartTime $now) -ObservedName 'rustup' -ObservedStartTime $now | Should Be $false
    }

    It 'rejects an unrelated process that inherited the pid' {
        Test-ProbeIdentity -Probe (New-Probe -StartTime $now) -ObservedName 'powershell' -ObservedStartTime $now | Should Be $false
    }

    It 'rejects an unreadable observed start time rather than trusting the name' {
        Test-ProbeIdentity -Probe (New-Probe -StartTime $now) -ObservedName 'cargo' -ObservedStartTime $null | Should Be $false
    }

    It 'tolerates sub-second round-tripping through the manifest' {
        $rehydrated = [datetime]::Parse($now.ToString('o'), [System.Globalization.CultureInfo]::InvariantCulture,
                                        [System.Globalization.DateTimeStyles]::RoundtripKind)
        Test-ProbeIdentity -Probe (New-Probe -StartTime $rehydrated) -ObservedName 'cargo' -ObservedStartTime $now | Should Be $true
    }

    It 'rejects a null probe' {
        Test-ProbeIdentity -Probe $null -ObservedName 'cargo' -ObservedStartTime $now | Should Be $false
    }
}

Describe 'Test-ProbeAlive' {

    It 'reports a manifest-rehydrated probe with no start time as NOT alive, so teardown never kills it' {
        # $PID is certainly a live process, so this isolates the StartTime guard
        # from the "process is gone" path.
        $probe = [PSCustomObject]@{ Role = 'foreign'; SlotId = -1; ProcessId = $PID; StartTime = $null }
        Test-ProbeAlive -Probe $probe | Should Be $false
    }

    It 'reports a pid that does not exist as not alive' {
        # Far outside the range Windows hands out, so it cannot collide with a
        # real process on a busy box.
        $probe = [PSCustomObject]@{ Role = 'foreign'; SlotId = -1; ProcessId = 999999999; StartTime = (Get-Date) }
        Test-ProbeAlive -Probe $probe | Should Be $false
    }

    It 'reports a null probe as not alive' {
        Test-ProbeAlive -Probe $null | Should Be $false
    }
}

Describe 'Start-Probe (launch shape - extracted, never invoked)' {

    # Start-Probe plants a real cargo process, so it is extracted but not run.
    # Its launch shape is asserted from the AST rather than by string-matching
    # the source, because the surrounding comments legitimately MENTION
    # `-WindowStyle` while explaining why it is not used - a naive `-like`
    # check would read those comments as the code.
    $startProbeAst = $ast.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Start-Probe' },
        $true) | Select-Object -First 1
    $src = $startProbeAst.Extent.Text
    $usedParams = @($startProbeAst.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.CommandParameterAst] },
        $true) | ForEach-Object { $_.ParameterName })

    It 'launches the RESOLVED cargo path, never the bare name that hits the rustup shim' {
        ($src -like '*-FilePath $script:CargoExe*') | Should Be $true
        ($src -like "*-FilePath 'cargo'*") | Should Be $false
    }

    It 'uses -NoNewWindow and never -WindowStyle, which implies UseShellExecute' {
        ($usedParams -contains 'NoNewWindow') | Should Be $true
        ($usedParams -contains 'WindowStyle') | Should Be $false
    }

    It 'captures the probe''s stderr so a plant failure reports evidence, not a guess' {
        ($usedParams -contains 'RedirectStandardError') | Should Be $true
    }

    It 'asserts the spawned process image is cargo.exe and throws otherwise' {
        ($src -like "*`$img -ne 'cargo.exe'*") | Should Be $true
        ($src -like '*throw*') | Should Be $true
    }

    It 'records StartTimeIso and CrateDir so -Cleanup can verify identity later' {
        ($src -like '*StartTimeIso*') | Should Be $true
        ($src -like '*CrateDir*') | Should Be $true
    }
}

Describe 'Get-TempRunnerIds' {

    It 'returns only test-* ids, so teardown never stops the primary or a named runner' {
        $payload = [PSCustomObject]@{ runners = @(
            [PSCustomObject]@{ id = 'primary' },
            [PSCustomObject]@{ id = 'named-9880-deadbeef' },
            [PSCustomObject]@{ id = 'test-abc123' }
        ) }
        $ids = Get-TempRunnerIds -RunnersPayload $payload
        $ids.Count | Should Be 1
        $ids[0] | Should Be 'test-abc123'
    }

    It 'accepts a bare array payload as well as the {runners} envelope' {
        $payload = @( [PSCustomObject]@{ id = 'test-one' }, [PSCustomObject]@{ id = 'test-two' } )
        (Get-TempRunnerIds -RunnersPayload $payload).Count | Should Be 2
    }

    It 'returns an empty array for a null payload rather than $null' {
        (Get-TempRunnerIds -RunnersPayload $null).Count | Should Be 0
    }

    It 'skips entries with no id property' {
        $payload = [PSCustomObject]@{ runners = @( [PSCustomObject]@{ port = 9877 } ) }
        (Get-TempRunnerIds -RunnersPayload $payload).Count | Should Be 0
    }
}

Describe 'Get-RunnersToStop' {

    It 'stops a runner that appeared during the run but not one that predates it' {
        $ids = Get-RunnersToStop -Baseline @('test-peer') -After @('test-peer', 'test-ours') -OwnIds @()
        $ids.Count | Should Be 1
        $ids[0] | Should Be 'test-ours'
    }

    It 'stops the id the spawn response named even with NO baseline - the leak #138 could not reach' {
        # $null baseline = the pre-spawn GET failed. The diff is unusable, but an
        # id the supervisor handed us is unambiguously ours.
        $ids = Get-RunnersToStop -Baseline $null -After @('test-peer', 'test-ours') -OwnIds @('test-ours')
        $ids.Count | Should Be 1
        $ids[0] | Should Be 'test-ours'
    }

    It 'still refuses to diff without a baseline, so a peer''s runner is never stopped' {
        $ids = Get-RunnersToStop -Baseline $null -After @('test-peer') -OwnIds @()
        $ids.Count | Should Be 0
    }

    It 'treats an EMPTY baseline as a real snapshot - the zero-peer case must not skip teardown' {
        $ids = Get-RunnersToStop -Baseline @() -After @('test-ours') -OwnIds @()
        $ids.Count | Should Be 1
        $ids[0] | Should Be 'test-ours'
    }

    It 'unions the two sources without duplicating the id they agree on' {
        $ids = Get-RunnersToStop -Baseline @() -After @('test-ours') -OwnIds @('test-ours')
        $ids.Count | Should Be 1
    }

    It 'keeps a named id the runners list has not caught up with yet' {
        # The async spawn reserves the id before the runner process exists, so
        # `After` can legitimately not contain it yet.
        $ids = Get-RunnersToStop -Baseline @() -After @() -OwnIds @('test-ours')
        $ids.Count | Should Be 1
        $ids[0] | Should Be 'test-ours'
    }

    It 'ignores a null or empty own-id rather than emitting a bogus target' {
        $ids = Get-RunnersToStop -Baseline @() -After @() -OwnIds @($null, '')
        $ids.Count | Should Be 0
    }

    It 'returns an array, not $null, when there is nothing to stop' {
        # The `return ,$out` idiom matters: a bare `return @()` collapses to
        # $null, and the caller's `@($targets).Count` would then be 1.
        $ids = Get-RunnersToStop -Baseline @() -After @() -OwnIds @()
        ($ids -is [array]) | Should Be $true
        $ids.Count | Should Be 0
    }
}

Describe 'Resolve-TeardownExitCode' {

    It 'raises a clean run to 3 when teardown leaked - the 2026-08-09 silent exit 0' {
        Resolve-TeardownExitCode -CurrentExitCode 0 -LeakedCount 1 | Should Be 3
    }

    It 'leaves a clean run at 0 when nothing leaked' {
        Resolve-TeardownExitCode -CurrentExitCode 0 -LeakedCount 0 | Should Be 0
    }

    It 'never demotes a FAILED assertion - a scoping defect outranks a leak' {
        Resolve-TeardownExitCode -CurrentExitCode 2 -LeakedCount 3 | Should Be 2
    }

    It 'never demotes an aborted run' {
        Resolve-TeardownExitCode -CurrentExitCode 1 -LeakedCount 1 | Should Be 1
    }

    It 'leaves an already-inconclusive run at 3' {
        Resolve-TeardownExitCode -CurrentExitCode 3 -LeakedCount 1 | Should Be 3
    }
}

Describe 'Test-KillEventForSlot' {

    It 'accepts a kill attributed to the claimed slot via the strong env match' {
        $e = New-KillEvent -ProcessId 4242 -SlotId 1 -Territory $Slot1 -MatchedBy 'env'
        Test-KillEventForSlot -KillEvent $e -SlotId 1 | Should Be $true
    }

    It 'rejects a kill attributed to a DIFFERENT slot (sibling-isolation guard)' {
        $e = New-KillEvent -ProcessId 4242 -SlotId 0 -Territory $Slot0 -MatchedBy 'env'
        Test-KillEventForSlot -KillEvent $e -SlotId 1 | Should Be $false
    }

    It 'rejects the weaker argv evidence - the orphan sets CARGO_TARGET_DIR, so env is the expected signal' {
        $e = New-KillEvent -ProcessId 4242 -SlotId 1 -Territory $Slot1 -MatchedBy 'argv'
        Test-KillEventForSlot -KillEvent $e -SlotId 1 | Should Be $false
    }

    It 'rejects a territory that does not end in slot-<id> even when slot_id agrees' {
        $e = New-KillEvent -ProcessId 4242 -SlotId 1 -Territory 'D:\qontinui-root\qontinui-runner\target-pool\slot-1\debug'
        Test-KillEventForSlot -KillEvent $e -SlotId 1 | Should Be $false
    }

    It 'tolerates a forward-slash territory (Path::display on a non-Windows build)' {
        $e = New-KillEvent -ProcessId 4242 -SlotId 1 -Territory '/srv/qontinui-runner/target-pool/slot-1'
        Test-KillEventForSlot -KillEvent $e -SlotId 1 | Should Be $true
    }

    It 'rejects a non-kill event' {
        $e = New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 0
        Test-KillEventForSlot -KillEvent $e -SlotId 1 | Should Be $false
    }
}

Describe 'Get-PollDeadline' {

    # The regression this whole function exists for: the poll loop's ONLY exit
    # condition is this deadline whenever there is no submission id, because
    # $buildState stays 'unsubmitted' and the terminal-state `break` is
    # unreachable. Before it was bounded, a spawn-test that 503'd on a full pool
    # made the harness sit for the full -BuildTimeoutSec (2400s) while its
    # planted probes held the cargo build lock on EVERY pool slot plus the
    # supervisor's shared target\ - stalling every peer's spawn-test for 40
    # minutes on behalf of a run that could no longer prove anything.
    $now = Get-Date

    It 'gives a SUBMITTED build the full -BuildTimeoutSec, unchanged' {
        $d = Get-PollDeadline -Now $now -SubmissionId 'sub-abc123' -BuildTimeoutSec 2400 -PollIntervalSec 10
        [int][math]::Round(($d - $now).TotalSeconds) | Should Be 2400
    }

    It 'bounds a FAILED submission to two poll intervals instead of running to the deadline' {
        $d = Get-PollDeadline -Now $now -SubmissionId $null -BuildTimeoutSec 2400 -PollIntervalSec 10
        $secs = [int][math]::Round(($d - $now).TotalSeconds)
        $secs | Should Be 20
        # The load-bearing half: it is nowhere near the 2400s deadline.
        ($secs -lt 60) | Should Be $true
    }

    It 'treats an empty-string submission id as no submission' {
        $d = Get-PollDeadline -Now $now -SubmissionId '' -BuildTimeoutSec 2400 -PollIntervalSec 10
        [int][math]::Round(($d - $now).TotalSeconds) | Should Be 20
    }

    It 'scales the bounded window with -PollIntervalSec so a slow poller still gets two reads' {
        $d = Get-PollDeadline -Now $now -SubmissionId $null -BuildTimeoutSec 2400 -PollIntervalSec 30
        [int][math]::Round(($d - $now).TotalSeconds) | Should Be 60
    }

    It 'never exceeds -BuildTimeoutSec, even when the poll interval is larger than it' {
        $d = Get-PollDeadline -Now $now -SubmissionId $null -BuildTimeoutSec 5 -PollIntervalSec 10
        [int][math]::Round(($d - $now).TotalSeconds) | Should Be 5
    }

    It 'always leaves at least one second so the loop takes one diagnostics read' {
        $d = Get-PollDeadline -Now $now -SubmissionId $null -BuildTimeoutSec 600 -PollIntervalSec 0
        [int][math]::Round(($d - $now).TotalSeconds) | Should Be 1
    }
}

Describe 'poll loop deadline wiring (AST/source - the loop itself is never run)' {

    # Get-PollDeadline being correct is worthless if the loop does not use it,
    # and the loop is top-level script code that a unit test cannot execute
    # (it does HTTP and plants cargo processes). Assert the wiring from source,
    # the same way the Start-Probe launch shape is asserted above.
    $scriptText = Get-Content -Raw $ScriptUnderTest

    It 'derives the poll deadline from Get-PollDeadline' {
        ($scriptText -like '*$deadline*= Get-PollDeadline*') | Should Be $true
    }

    It 'no longer pins the deadline to the raw build timeout regardless of submission' {
        ($scriptText -like '*$deadline*= (Get-Date).AddSeconds($BuildTimeoutSec)*') | Should Be $false
    }

    It 'names the submission failure in the output rather than hanging silently' {
        ($scriptText -like '*submission failed: $submitError*') | Should Be $true
    }
}

Describe 'Get-SparedPidAttribution' {

    $foreignPid = 4242

    It 'returns the strongest spared count across concurrent passes' {
        $s = @(
            (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 1 -Spared 2).data,
            (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 0 -Spared 4).data
        )
        (Get-SparedPidAttribution -Summaries $s -ProcessId $foreignPid).MaxSpared | Should Be 4
    }

    It 'reports NamesPid when a pass directly attributed our foreign pid' {
        $s = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 1 -Spared 3 -SparedPids @(11, $foreignPid, 13)).data )
        $a = Get-SparedPidAttribution -Summaries $s -ProcessId $foreignPid
        $a.NamesPid | Should Be $true
        $a.SampledPidCount | Should Be 3
    }

    It 'does NOT report NamesPid when the pid fell outside the capped sample - the count still stands' {
        # PID_LIST_CAP = 5 on the Rust side. On a box with ~9 concurrent
        # sessions our foreign probe can legitimately miss the sample while
        # being perfectly spared, which is exactly why the assertion asserts on
        # the COUNT and only strengthens on the list. Asserting containment
        # here would flake.
        $s = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 1 -Spared 9 -SparedPids @(11, 12, 13, 14, 15)).data )
        $a = Get-SparedPidAttribution -Summaries $s -ProcessId $foreignPid
        $a.NamesPid | Should Be $false
        $a.MaxSpared | Should Be 9
    }

    It 'tolerates a supervisor that emits no spared_pids at all' {
        $s = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 1 -Spared 2).data )
        $a = Get-SparedPidAttribution -Summaries $s -ProcessId $foreignPid
        $a.NamesPid | Should Be $false
        $a.SampledPidCount | Should Be 0
        $a.MaxSpared | Should Be 2
    }

    It 'returns MaxSpared -1 for no summaries, so the caller can tell "no pass" from "spared 0"' {
        $a = Get-SparedPidAttribution -Summaries @() -ProcessId $foreignPid
        $a.MaxSpared | Should Be (-1)
        $a.NamesPid | Should Be $false
    }

    It 'ignores null summaries and null pid entries' {
        $s = @( $null, (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 0 -Spared 1 -SparedPids @($null, $foreignPid)).data )
        $a = Get-SparedPidAttribution -Summaries $s -ProcessId $foreignPid
        $a.NamesPid | Should Be $true
        $a.SampledPidCount | Should Be 1
    }
}

Describe 'Test-AttributableCleanupPass' {

    # The gap this closes: a PEER's spawn-test emits a build_cleanup_summary
    # naming one of OUR slot territories, so $sawAnyCleanupPass goes true while
    # no pass of ours ever examined our probes. Every absence-check would then
    # report PASS off that peer's pass.
    function New-Resolution {
        param($SlotId, $Attributable)
        [PSCustomObject]@{ SlotId = $SlotId; Source = 'test'; Attributable = $Attributable; Note = 'test resolution' }
    }

    It 'accepts a summary for the slot our build ATTRIBUTABLY claimed' {
        $s = @( (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 2).data )
        Test-AttributableCleanupPass -Summaries $s -SlotResolution (New-Resolution -SlotId 1 -Attributable $true) | Should Be $true
    }

    It 'REJECTS a peer''s pass - build_slot_id absent means the claim was only INFERRED' {
        # Exactly the vacuity: a summary exists for slot 1, but nothing ties it
        # to our build, so an absence-check must not PASS off it.
        $s = @( (New-SummaryEvent -SlotId 1 -Territory $Slot1 -Killed 1 -Spared 2).data )
        Test-AttributableCleanupPass -Summaries $s -SlotResolution (New-Resolution -SlotId 1 -Attributable $false) | Should Be $false
    }

    It 'rejects an attributable claim whose pass was never observed' {
        # build_slot_id said slot 2, but no summary for slot 2 reached the bag
        # (emitted before /diagnostics/clear, evicted, or never run).
        $s = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 0 -Spared 3).data )
        Test-AttributableCleanupPass -Summaries $s -SlotResolution (New-Resolution -SlotId 2 -Attributable $true) | Should Be $false
    }

    It 'rejects an unknown claimed slot even when marked attributable' {
        $s = @( (New-SummaryEvent -SlotId 0 -Territory $Slot0 -Killed 1 -Spared 1).data )
        Test-AttributableCleanupPass -Summaries $s -SlotResolution (New-Resolution -SlotId -1 -Attributable $true) | Should Be $false
    }

    It 'rejects a null resolution and an empty summary set' {
        Test-AttributableCleanupPass -Summaries @() -SlotResolution $null | Should Be $false
        Test-AttributableCleanupPass -Summaries @() -SlotResolution (New-Resolution -SlotId 0 -Attributable $true) | Should Be $false
    }

    It 'composes with the real Resolve-ClaimedSlot: a lone peer summary is never an attributable pass' {
        $summaries = @( (New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 1 -Spared 4).data )
        # No build_slot_id -> Resolve-ClaimedSlot infers slot 2 but marks it
        # unattributable. The gate must agree.
        $res = Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId $null -SlotCount 3
        $res.Attributable | Should Be $false
        Test-AttributableCleanupPass -Summaries $summaries -SlotResolution $res | Should Be $false
    }

    It 'composes with the real Resolve-ClaimedSlot: build_slot_id plus its summary IS an attributable pass' {
        $summaries = @( (New-SummaryEvent -SlotId 2 -Territory $Slot2 -Killed 1 -Spared 4).data )
        $res = Resolve-ClaimedSlot -Summaries $summaries -BuildSlotId 2 -SlotCount 3
        $res.Attributable | Should Be $true
        Test-AttributableCleanupPass -Summaries $summaries -SlotResolution $res | Should Be $true
    }
}

Describe 'Resolve-AbsenceCheckVerdict' {

    It 'PASSES only when an attributable pass ran and no defect was seen' {
        $v = Resolve-AbsenceCheckVerdict -DefectSeen $false -SawAnyCleanupPass $true -AttributablePass $true
        $v.Result | Should Be 'PASS'
        $v.Reason | Should Be 'proved'
    }

    It 'does NOT pass off a PEER''s cleanup pass - the vacuity this gate closes' {
        $v = Resolve-AbsenceCheckVerdict -DefectSeen $false -SawAnyCleanupPass $true -AttributablePass $false
        $v.Result | Should Be 'INCONCLUSIVE'
        $v.Reason | Should Be 'unattributed'
    }

    It 'distinguishes "no pass ran at all" from "a pass ran but was not ours"' {
        (Resolve-AbsenceCheckVerdict -DefectSeen $false -SawAnyCleanupPass $false -AttributablePass $false).Reason | Should Be 'no_pass'
        (Resolve-AbsenceCheckVerdict -DefectSeen $false -SawAnyCleanupPass $true  -AttributablePass $false).Reason | Should Be 'unattributed'
    }

    It 'reports FAIL for a defect regardless of attribution - a wrongful kill is a defect whoever ran the pass' {
        $v = Resolve-AbsenceCheckVerdict -DefectSeen $true -SawAnyCleanupPass $false -AttributablePass $false
        $v.Result | Should Be 'FAIL'
        $v.Reason | Should Be 'defect'
    }

    It 'reports FAIL even when an attributable pass DID run' {
        (Resolve-AbsenceCheckVerdict -DefectSeen $true -SawAnyCleanupPass $true -AttributablePass $true).Result | Should Be 'FAIL'
    }

    It 'never returns PASS without attribution, for any combination' {
        foreach ($saw in @($true, $false)) {
            foreach ($attr in @($true, $false)) {
                $v = Resolve-AbsenceCheckVerdict -DefectSeen $false -SawAnyCleanupPass $saw -AttributablePass $attr
                if ($v.Result -eq 'PASS') { $attr | Should Be $true }
            }
        }
    }
}

Describe 'absence-check PASS arms are attribution-gated (source)' {

    # The helpers being correct is worthless if a row still hard-codes PASS.
    # These four rows are top-level script code that a unit test cannot execute,
    # so the wiring is asserted from source - same posture as the Start-Probe
    # launch shape and the poll-deadline wiring above.
    $scriptText = Get-Content -Raw $ScriptUnderTest

    It 'routes exactly the four absence-checks through the gated verdict helper' {
        ([regex]::Matches($scriptText, 'Resolve-AbsenceCheckVerdict -DefectSeen')).Count | Should Be 4
    }

    It 'computes the gate from Test-AttributableCleanupPass rather than reusing $sawAnyCleanupPass' {
        ($scriptText -like '*$attributablePass = Test-AttributableCleanupPass*') | Should Be $true
    }

    It 'never hard-codes PASS on V1-1, V1-2, V2-3 or V2-4' {
        foreach ($id in @('V1-1', 'V1-2', 'V2-3', 'V2-4')) {
            $pattern = "(?s)-Id '" + [regex]::Escape($id) + "'.{0,220}?-Result 'PASS'"
            ($scriptText -match $pattern) | Should Be $false
        }
    }

    It 'still hard-codes the FAIL arms, which are deliberately NOT attribution-gated' {
        # V2-3's silent-kill FAIL and V1-1's killed-foreign FAIL must survive
        # the refactor as unconditional literals.
        ($scriptText -match "(?s)-Id 'V2-3'.{0,220}?-Result 'FAIL'") | Should Be $true
        ($scriptText -match "(?s)-Id 'V1-1'.{0,220}?-Result 'FAIL'") | Should Be $true
    }
}

Describe 'teardown leak visibility (source - Stop-AllProbes is never run)' {

    # Test-ProbeIdentity's "no start time => never kill" posture is correct, but
    # it applies to IN-SESSION teardown as well as -Cleanup: such a probe is
    # skipped and goes on holding its territory's cargo build lock. The
    # consequence has to be visible, not silent.
    $stopAst = $ast.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Stop-AllProbes' },
        $true) | Select-Object -First 1
    $src = $stopAst.Extent.Text

    It 'counts the probes it could not verify' {
        ($src -like '*$unverified*') | Should Be $true
    }

    It 'warns about them rather than dropping them silently' {
        ($src -like '*$unverified.Count -gt 0*') | Should Be $true
        ($src -like '*teardown LEFT*') | Should Be $true
    }
}

Describe 'Resolve-PoolSize - the pool size is never guessed' {

    # WHY THIS BLOCK EXISTS
    #
    #     The pool size used to end in a hardcoded `$PoolSize = 3` after a WARN.
    #     Guessing LOW manufactures a FALSE PASS on the one assertion whose job
    #     is to catch a cross-slot kill: pool size decides how many territories
    #     get a probe, so a guess of 3 against a real pool of 4 leaves slot 3
    #     unprobed and V2-3 ("orphans in the OTHER pool slots are still ALIVE")
    #     passes having never looked at it.
    #
    #     The fallback is gone, but "gone" is a claim like any other. These
    #     tests drive every arm so it stays gone - the arms were verified once
    #     by hand against a live supervisor, which proves nothing about the
    #     next edit.
    #
    # -RetryDelaySec 0 everywhere: the retry SLEEP is not under test and a real
    # one would put seconds of dead time in the suite.

    It 'takes an explicit -PoolSize without touching the route at all' {
        $script:probeCalls = 0
        $r = Resolve-PoolSize -Override 4 -RetryDelaySec 0 -ReadBuilds {
            $script:probeCalls++
            [PSCustomObject]@{ pool_size = 9 }
        }
        $r.Size | Should Be 4
        $r.Error | Should BeNullOrEmpty
        $r.Source | Should Match 'explicit'
        # The route must not even be consulted - an explicit declaration is
        # authoritative and /builds is a contended call.
        $script:probeCalls | Should Be 0
    }

    It 'reads pool_size from the route and names it as the source' {
        $r = Resolve-PoolSize -RetryDelaySec 0 -ReadBuilds { [PSCustomObject]@{ pool_size = 5 } }
        $r.Size | Should Be 5
        $r.Error | Should BeNullOrEmpty
        $r.Source | Should Match 'pool_size \(attempt 1\)'
    }

    It 'retries a flaking route and succeeds on a later attempt' {
        # The observed failure was a TIMEOUT on a route that answered fine
        # moments later, which is exactly what the retry is for.
        $script:attempts = 0
        $r = Resolve-PoolSize -RetryDelaySec 0 -ReadBuilds {
            $script:attempts++
            if ($script:attempts -lt 3) { throw 'The operation has timed out' }
            [PSCustomObject]@{ pool_size = 4 }
        }
        $r.Size | Should Be 4
        $r.Source | Should Match 'attempt 3'
        $script:attempts | Should Be 3
    }

    It 'surfaces each failed attempt through OnWarn rather than swallowing it' {
        $script:warnings = @()
        $null = Resolve-PoolSize -RetryDelaySec 0 -Attempts 3 `
                                 -ReadBuilds { throw 'The operation has timed out' } `
                                 -OnWarn { param($m) $script:warnings += $m }
        $script:warnings.Count | Should Be 3
        ($script:warnings[0] -like '*attempt 1/3 failed*') | Should Be $true
    }

    It 'treats a 200 with no pool_size as a contract miss and does NOT burn more attempts' {
        $script:attempts = 0
        $r = Resolve-PoolSize -RetryDelaySec 0 -ReadBuilds {
            $script:attempts++
            [PSCustomObject]@{ builds = @() }
        }
        $script:attempts | Should Be 1
        $r.Error | Should Not BeNullOrEmpty
    }

    It 'treats a non-numeric pool_size as a contract miss, not a flake' {
        $script:attempts = 0
        $r = Resolve-PoolSize -RetryDelaySec 0 -ReadBuilds {
            $script:attempts++
            [PSCustomObject]@{ pool_size = 'lots' }
        }
        $script:attempts | Should Be 1
        $r.Size | Should Be 0
        $r.Error | Should Not BeNullOrEmpty
    }

    It 'falls back to the env var when the route cannot answer' {
        $r = Resolve-PoolSize -RetryDelaySec 0 -EnvValue '6' -ReadBuilds { throw 'no route' }
        $r.Size | Should Be 6
        $r.Error | Should BeNullOrEmpty
        $r.Source | Should Match 'env'
    }

    It 'names a non-integer env var instead of throwing a raw cast' {
        $r = Resolve-PoolSize -RetryDelaySec 0 -EnvValue 'three' -ReadBuilds { throw 'no route' }
        $r.Size | Should Be 0
        ($r.Error -like '*QONTINUI_SUPERVISOR_BUILD_POOL_SIZE*') | Should Be $true
        ($r.Error -like '*not an integer*') | Should Be $true
    }

    It 'lets the env var answer when the route reports a nonsense pool_size of 0' {
        $r = Resolve-PoolSize -RetryDelaySec 0 -EnvValue '2' -ReadBuilds { [PSCustomObject]@{ pool_size = 0 } }
        $r.Size | Should Be 2
        $r.Source | Should Match 'env'
    }

    It 'refuses a non-positive size from either source' {
        foreach ($bad in @(0, -1)) {
            $viaRoute = Resolve-PoolSize -RetryDelaySec 0 -ReadBuilds ({ [PSCustomObject]@{ pool_size = $bad } }.GetNewClosure())
            $viaRoute.Error | Should Not BeNullOrEmpty
            $viaEnv = Resolve-PoolSize -RetryDelaySec 0 -EnvValue "$bad" -ReadBuilds { throw 'no route' }
            $viaEnv.Error | Should Not BeNullOrEmpty
        }
    }

    It 'ABORTS instead of defaulting when no source can answer - the whole point' {
        $r = Resolve-PoolSize -RetryDelaySec 0 -ReadBuilds { throw 'The operation has timed out' }
        $r.Source | Should BeNullOrEmpty
        $r.Error | Should Not BeNullOrEmpty
        # 3 was the old silent default. A future edit that reinstates ANY
        # default fails on Size, and this pins the exact value that caused the
        # incident.
        $r.Size | Should Be 0
        $r.Size | Should Not Be 3
        # The failure has to be actionable, not just loud.
        ($r.Error -like '*-PoolSize*') | Should Be $true
    }

    It 'still aborts when there is no route to read at all' {
        $r = Resolve-PoolSize
        $r.Size | Should Be 0
        $r.Error | Should Not BeNullOrEmpty
    }
}

Describe 'Resolve-RunnerRepo - the repo root is never guessed either' {

    # Same class as the pool size, and it was left behind when that one was
    # fixed: a missing `supervisor.project_dir` used to WARN and fall back to a
    # hardcoded D:\qontinui-root\qontinui-runner. This box holds several runner
    # checkouts and worktrees, so the wrong-but-existing one plants V2's probes
    # into a pool the build under test never touches - the orphan is not reaped
    # and V2-1 reports a scoping defect that does not exist, which is the
    # failure mode this whole file was written to prevent.

    function New-Health {
        param($ProjectDir)
        if ($null -eq $ProjectDir) { return [PSCustomObject]@{ status = 'ok' } }
        [PSCustomObject]@{
            status     = 'ok'
            supervisor = [PSCustomObject]@{ version = '0.1.0'; project_dir = $ProjectDir }
        }
    }

    It 'derives the repo root one level above project_dir' {
        $r = Resolve-RunnerRepo -HealthBody (New-Health 'D:\ws\qontinui-runner\src-tauri')
        $r.Path | Should Be 'D:\ws\qontinui-runner'
        $r.Error | Should BeNullOrEmpty
        $r.Source | Should Match 'project_dir'
    }

    It 'prefers an explicit -RunnerRepo and does not consult /health' {
        $r = Resolve-RunnerRepo -Override 'D:\other\runner' -OverrideProvided $true `
                                -HealthBody (New-Health 'D:\ws\qontinui-runner\src-tauri')
        $r.Path | Should Be 'D:\other\runner'
        $r.Source | Should Match 'explicit'
    }

    It 'ABORTS when /health carries no supervisor block' {
        $r = Resolve-RunnerRepo -HealthBody (New-Health $null)
        $r.Path | Should BeNullOrEmpty
        $r.Error | Should Not BeNullOrEmpty
        ($r.Error -like '*-RunnerRepo*') | Should Be $true
    }

    It 'ABORTS when the supervisor block carries no project_dir' {
        $r = Resolve-RunnerRepo -HealthBody ([PSCustomObject]@{ supervisor = [PSCustomObject]@{ version = '0.1.0' } })
        $r.Error | Should Not BeNullOrEmpty
    }

    It 'ABORTS on a project_dir with no parent rather than deriving an empty root' {
        # An empty root would make every slot territory a bare relative
        # `target-pool\slot-k`, which resolves against OUR cwd.
        $r = Resolve-RunnerRepo -HealthBody (New-Health 'src-tauri')
        $r.Path | Should BeNullOrEmpty
        $r.Error | Should Not BeNullOrEmpty
    }

    It 'never returns the old hardcoded checkout on any failure arm' {
        foreach ($body in @((New-Health $null), $null, ([PSCustomObject]@{ supervisor = $null }))) {
            $r = Resolve-RunnerRepo -HealthBody $body
            $r.Error | Should Not BeNullOrEmpty
            ("$($r.Path)" -like '*qontinui-runner*') | Should Be $false
        }
    }
}

Describe 'preflight discovery is wired to the resolvers (source)' {

    # The resolvers being correct is worthless if the script still carries a
    # fallback of its own - same posture as the absence-check wiring block.
    $scriptText = Get-Content -Raw $ScriptUnderTest

    It 'routes both discoveries through the extracted resolvers' {
        ($scriptText -like '*$repoRes = Resolve-RunnerRepo*') | Should Be $true
        ($scriptText -like '*$poolRes = Resolve-PoolSize*') | Should Be $true
    }

    It 'aborts the preflight on either resolver Error' {
        # Both guards, and both report the resolver's own named cause rather
        # than a generic "discovery failed".
        ([regex]::Matches($scriptText, 'if \(\$(repo|pool)Res\.Error\)')).Count | Should Be 2
        ([regex]::Matches($scriptText, 'Write-Fail \$(repo|pool)Res\.Error')).Count | Should Be 2
    }

    It 'carries no hardcoded pool-size default anywhere' {
        ($scriptText -match '\$PoolSize\s*=\s*3\b') | Should Be $false
    }

    It 'carries no hardcoded runner-repo default in the param block' {
        # The parameter default is the last place the old guess could hide: an
        # unbound -RunnerRepo is resolved from /health, so a value here would be
        # a guess that never announces itself.
        ($scriptText -match '\[string\]\$RunnerRepo\s*=\s*''D:') | Should Be $false
    }

    It 'prints the provenance of both numbers so a run log can be audited' {
        ($scriptText -like '*runner repo = $RunnerRepo (source:*') | Should Be $true
        ($scriptText -like '*pool size = $PoolSize (source:*') | Should Be $true
    }
}

Describe 'runner-leak accounting is WIRED to the exit code (source - never run)' {

    # Resolve-TeardownExitCode being correct in isolation is worth nothing if
    # `finally` does not call it: the 2026-08-09 leak was a wiring silence, not a
    # arithmetic bug. These assert the plumbing the unit tests above cannot see.
    $stopRunnersAst = $ast.FindAll(
        { param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Stop-SpawnedRunners' },
        $true) | Select-Object -First 1
    $stopSrc = $stopRunnersAst.Extent.Text
    $wholeSrc = $ast.Extent.Text

    It 'records leaks on a script-scoped ledger rather than only warning' {
        ($stopSrc -like '*$script:LeakedRunners +=*') | Should Be $true
    }

    It 'proves the stop by RE-READING /runners instead of trusting the 200' {
        # Two GETs: the pre-stop list and the confirming post-stop list.
        $gets = ([regex]::Matches($stopSrc, [regex]::Escape('Invoke-RestMethod -Method Get -Uri "$base/runners"'))).Count
        ($gets -ge 2) | Should Be $true
        ($stopSrc -like '*still present in GET /runners*') | Should Be $true
    }

    It 'settles before convicting, so a removal race is not reported as a leak' {
        # A leak report escalates the exit code and sends the operator cleaning
        # up; a one-off registry race must not manufacture one. The settle bounds
        # how long the supervisor gets, never WHETHER we re-read.
        ($stopSrc -like '*settle before convicting*') | Should Be $true
        ($stopSrc -like '*Start-Sleep -Seconds 5*') | Should Be $true
        ($stopSrc -like '*after a 5s settle*') | Should Be $true
    }

    It 'treats a failed confirming read as UNKNOWN, not as stopped' {
        ($stopSrc -like '*UNKNOWN*') | Should Be $true
    }

    It 'stops the id the spawn response named, not only the snapshot diff' {
        ($stopSrc -like '*$script:OurSpawnedRunnerIds*') | Should Be $true
        ($wholeSrc -like '*$script:OurSpawnedRunnerIds += $spawnResp.id*') | Should Be $true
    }

    It 'escalates the exit code from the finally block' {
        ($wholeSrc -like '*$script:ExitCode = Resolve-TeardownExitCode*') | Should Be $true
    }

    It 'exits AFTER the finally block, so the escalation can still take effect' {
        # `exit $script:ExitCode` must be the last statement in the file, outside
        # try/catch/finally. Inside the try it would run before teardown.
        $lines = @($wholeSrc -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne '' })
        $lines[-1] | Should Be 'exit $script:ExitCode'
    }
}
