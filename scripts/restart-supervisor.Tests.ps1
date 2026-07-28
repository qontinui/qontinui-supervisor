<#
Unit tests for the pure decision functions in restart-supervisor.ps1:
Get-RunnerList (payload normalization), Get-BlockingRunners, and
Test-GuardRequired (the pre-flight runner-safety guard added for the
2026-07-27 "supervisor restart reaps primary runner" incident - see plan
2026-07-27-supervisor-restart-reaps-primary-runner, Phase 1).

Test-GuardRequired is the self-retirement check: it reads the CURRENTLY
RUNNING supervisor's `GET /health` payload and decides whether the
`ephemeral_job_temp_only` field confirms the 2026-07-28 JobObject fix (guard
not required) or not (guard required - fail safe).

These functions are extracted from the script via its AST and defined
directly in this scope rather than dot-sourced (`. restart-supervisor.ps1`),
because the target script is a flat, unguarded top-level script: dot-sourcing
it would actually run the build, the graceful-shutdown POST, the
Stop-Process fallback, and the launch - exactly the side effects a unit test
must never trigger.

PowerShell 5.1 / Pester 3.4.0 compatible: legacy `Should <verb>` syntax (no
leading dash), no BeforeAll/BeforeEach (not available in Pester 3).

Run with: Invoke-Pester scripts\restart-supervisor.Tests.ps1
#>

$ScriptUnderTest = Join-Path $PSScriptRoot 'restart-supervisor.ps1'

# Pull just the named pure functions out of the script's AST.
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($ScriptUnderTest, [ref]$null, [ref]$parseErrors)
if ($parseErrors -and $parseErrors.Count -gt 0) {
    throw "Failed to parse $ScriptUnderTest for test extraction: $($parseErrors -join '; ')"
}

$functionsToExtract = @('Get-RunnerList', 'Get-BlockingRunners', 'Test-GuardRequired')
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

function New-TestRunner {
    param($Id, $Running, $ProcPid, $Port)
    [PSCustomObject]@{ id = $Id; running = $Running; pid = $ProcPid; port = $Port }
}

Describe 'Get-BlockingRunners' {

    It 'returns a running primary (guard would refuse the restart)' {
        $list = @( (New-TestRunner -Id 'primary' -Running $true -ProcPid 1111 -Port 9876) )
        $result = Get-BlockingRunners -RunnerList $list
        $result.Count | Should Be 1
        $result[0].id | Should Be 'primary'
    }

    It 'never blocks on a running temp runner (ephemeral by design)' {
        $list = @( (New-TestRunner -Id 'test-abc123' -Running $true -ProcPid 2222 -Port 9877) )
        $result = Get-BlockingRunners -RunnerList $list
        $result.Count | Should Be 0
    }

    It 'does not block on a stopped primary' {
        $list = @( (New-TestRunner -Id 'primary' -Running $false -ProcPid $null -Port 9876) )
        $result = Get-BlockingRunners -RunnerList $list
        $result.Count | Should Be 0
    }

    It 'blocks on a running named runner - it holds user sessions exactly like the primary' {
        $list = @( (New-TestRunner -Id 'named-9880-deadbeef' -Running $true -ProcPid 3333 -Port 9880) )
        $result = Get-BlockingRunners -RunnerList $list
        $result.Count | Should Be 1
        $result[0].id | Should Be 'named-9880-deadbeef'
    }

    It 'in a mixed fleet, blocks only the running non-temp entries' {
        $list = @(
            (New-TestRunner -Id 'primary'             -Running $true  -ProcPid 1111 -Port 9876),
            (New-TestRunner -Id 'test-abc123'          -Running $true  -ProcPid 2222 -Port 9877),
            (New-TestRunner -Id 'named-9880-deadbeef'  -Running $false -ProcPid $null -Port 9880)
        )
        $result = Get-BlockingRunners -RunnerList $list
        $result.Count | Should Be 1
        $result[0].id | Should Be 'primary'
    }

    It 'returns an empty list for an empty runner list' {
        $result = Get-BlockingRunners -RunnerList @()
        $result.Count | Should Be 0
    }
}

Describe 'Get-RunnerList' {

    It 'normalizes a bare-array payload' {
        $payload = '[{"id":"primary","running":true,"pid":1111,"port":9876}]' | ConvertFrom-Json
        $result = Get-RunnerList -RunnersPayload $payload
        $result.Count | Should Be 1
        $result[0].id | Should Be 'primary'
    }

    It 'normalizes an object-wrapped {runners: [...]} payload identically to the bare array' {
        $payload = '{"runners":[{"id":"primary","running":true,"pid":1111,"port":9876}]}' | ConvertFrom-Json
        $result = Get-RunnerList -RunnersPayload $payload
        $result.Count | Should Be 1
        $result[0].id | Should Be 'primary'
    }

    It 'returns an empty list for a null payload (e.g. supervisor already down)' {
        $result = Get-RunnerList -RunnersPayload $null
        $result.Count | Should Be 0
    }

    It 'returns an empty list for an empty {runners: []} payload' {
        $payload = '{"runners":[]}' | ConvertFrom-Json
        $result = Get-RunnerList -RunnersPayload $payload
        $result.Count | Should Be 0
    }
}

Describe 'Get-RunnerList + Get-BlockingRunners end-to-end (both payload shapes agree)' {

    It 'the bare-array shape and the {runners:[...]} shape produce the same blocking decision' {
        $bare = '[{"id":"primary","running":true,"pid":1111,"port":9876},{"id":"test-abc123","running":true,"pid":2222,"port":9877}]' | ConvertFrom-Json
        $wrapped = '{"runners":[{"id":"primary","running":true,"pid":1111,"port":9876},{"id":"test-abc123","running":true,"pid":2222,"port":9877}]}' | ConvertFrom-Json

        $bareResult = Get-BlockingRunners -RunnerList (Get-RunnerList -RunnersPayload $bare)
        $wrappedResult = Get-BlockingRunners -RunnerList (Get-RunnerList -RunnersPayload $wrapped)

        $bareResult.Count | Should Be 1
        $wrappedResult.Count | Should Be 1
        $bareResult[0].id | Should Be $wrappedResult[0].id
    }
}

Describe 'Test-GuardRequired' {

    It 'does NOT require the guard when ephemeral_job_temp_only is true (fixed supervisor confirmed)' {
        $payload = '{"ephemeral_job_temp_only":true}' | ConvertFrom-Json
        $result = Test-GuardRequired -HealthPayload $payload
        $result | Should Be $false
    }

    It 'REQUIRES the guard when ephemeral_job_temp_only is absent (pre-fix supervisor - fails safe)' {
        $payload = '{"status":"ok"}' | ConvertFrom-Json
        $result = Test-GuardRequired -HealthPayload $payload
        $result | Should Be $true
    }

    It 'REQUIRES the guard when ephemeral_job_temp_only is explicitly false' {
        $payload = '{"ephemeral_job_temp_only":false}' | ConvertFrom-Json
        $result = Test-GuardRequired -HealthPayload $payload
        $result | Should Be $true
    }

    It 'does NOT require the guard for a null payload (health unreachable - nothing running to reap)' {
        $result = Test-GuardRequired -HealthPayload $null
        $result | Should Be $false
    }
}
