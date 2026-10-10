$ErrorActionPreference = 'Stop'
$script:fixtureRoot = $env:STARTUP_WATCHER_FIXTURE_ROOT
$script:configPath = Join-Path $script:fixtureRoot 'services.json'
$script:stagedExecutable = $env:STARTUP_WATCHER_EXECUTABLE
$script:startServiceIds = @('initial-service')
$script:snapshotFails = $false
$script:eventsPath = $env:STARTUP_WATCHER_EVENTS_FILE
$script:argsPath = $env:STARTUP_WATCHER_ARGS_FILE

$tokens = $null
$parseErrors = $null
$source = $env:STARTUP_WATCHER_SOURCE
$ast = [System.Management.Automation.Language.Parser]::ParseFile($source, [ref] $tokens, [ref] $parseErrors)
if ($parseErrors.Count -gt 0) { throw "Cannot parse watcher source: $($parseErrors[0].Message)" }
foreach ($functionName in @('Test-StartupConfiguration', 'Start-InitialDevelopmentSupervisor', 'Invoke-ValidatedHandoff')) {
    $functionAst = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $functionName }, $true) | Select-Object -First 1
    if ($null -eq $functionAst) { throw "Missing watcher function: $functionName" }
    . ([scriptblock]::Create($functionAst.Extent.Text))
}

function Get-RunningServiceIds {
    Add-Content -LiteralPath $script:eventsPath 'snapshot'
    if ($script:snapshotFails) { throw 'fixture service snapshot failed' }
    return @('running-api', 'running-worker')
}
function Request-GracefulShutdown {
    param([Parameter(Mandatory)] [string] $Reason)
    Add-Content -LiteralPath $script:eventsPath 'shutdown'
}
function Start-DevelopmentSupervisor {
    Add-Content -LiteralPath $script:eventsPath 'start'
    return 'fixture-started'
}

function Reset-HarnessEvents {
    Remove-Item -LiteralPath $script:eventsPath, $script:argsPath -Force -ErrorAction SilentlyContinue
}
function Assert-EventSequence {
    param([string[]] $Expected)
    $actual = @(Get-Content -LiteralPath $script:eventsPath)
    if (($actual -join ',') -ne ($Expected -join ',')) {
        throw "Expected events [$($Expected -join ',')], got [$($actual -join ',')]"
    }
}

$env:STARTUP_WATCHER_MODE = 'success'
Reset-HarnessEvents
$handoff = Invoke-ValidatedHandoff -Reason 'fixture handoff'
if (-not $handoff.Approved) { throw 'Valid candidate was rejected during handoff.' }
Assert-EventSequence @('snapshot', 'checker', 'shutdown')
$handoffArgs = Get-Content -LiteralPath $script:argsPath -Raw
if ($handoffArgs -notmatch 'validate-startup' -or
    $handoffArgs -notmatch '--service running-api' -or
    $handoffArgs -notmatch '--service running-worker') {
    throw "Handoff did not use the local checker with the running-service snapshot: $handoffArgs"
}

Reset-HarnessEvents
$script:startServiceIds = @('requested-service')
$initial = Start-InitialDevelopmentSupervisor
if ($initial -ne 'fixture-started') { throw 'Initial path did not invoke the Supervisor start function.' }
Assert-EventSequence @('checker', 'start')
$initialArgs = Get-Content -LiteralPath $script:argsPath -Raw
if ($initialArgs -notmatch 'validate-startup' -or $initialArgs -notmatch '--service requested-service') {
    throw "Initial path did not request local validation for its service IDs: $initialArgs"
}

$env:STARTUP_WATCHER_MODE = 'failure'
Reset-HarnessEvents
$failedHandoff = Invoke-ValidatedHandoff -Reason 'invalid candidate'
if ($failedHandoff.Approved) { throw 'A nonzero candidate checker was approved.' }
Assert-EventSequence @('snapshot', 'checker')
$failedHandoffArgs = Get-Content -LiteralPath $script:argsPath -Raw
if ($failedHandoffArgs -notmatch '--service running-api' -or
    $failedHandoffArgs -notmatch '--service running-worker') {
    throw "Failed handoff did not preflight the running services: $failedHandoffArgs"
}

$script:snapshotFails = $true
Reset-HarnessEvents
$snapshotRejected = Invoke-ValidatedHandoff -Reason 'snapshot unavailable'
if ($snapshotRejected.Approved) { throw 'A failed running-service snapshot was approved.' }
Assert-EventSequence @('snapshot')
if (Test-Path -LiteralPath $script:argsPath) { throw 'The checker ran without a running-service snapshot.' }
$script:snapshotFails = $false

$script:stagedExecutable = Join-Path $script:fixtureRoot 'missing-checker.cmd'
Reset-HarnessEvents
$missingCheckerRejected = Invoke-ValidatedHandoff -Reason 'checker missing'
if ($missingCheckerRejected.Approved) { throw 'A missing checker executable was approved.' }
Assert-EventSequence @('snapshot')
if (Test-Path -LiteralPath $script:argsPath) { throw 'Arguments were recorded for a missing checker.' }
$script:stagedExecutable = $env:STARTUP_WATCHER_EXECUTABLE

Reset-HarnessEvents
$initialRejected = $false
try { Start-InitialDevelopmentSupervisor } catch { $initialRejected = $true }
if (-not $initialRejected) { throw 'Initial startup accepted a failed local checker.' }
Assert-EventSequence @('checker')

$env:STARTUP_WATCHER_MODE = 'invalid'
Reset-HarnessEvents
$invalidHandoff = Invoke-ValidatedHandoff -Reason 'invalid JSON result'
if ($invalidHandoff.Approved) { throw 'A structurally invalid validation result was approved.' }
Assert-EventSequence @('snapshot', 'checker')
Write-Output 'watcher startup validation harness passed'
