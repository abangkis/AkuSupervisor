[CmdletBinding()]
param(
    [string] $Config,
    [ValidateSet('user', 'codex')]
    [string] $Actor = 'user',
    [ValidatePattern('^[A-Za-z0-9_.:-]{1,128}$')]
    [string] $RequestId
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$devExecutable = Join-Path $repository 'target\dev\aku-supervisor.exe'

function Stop-IntegrationValidation {
    param([Parameter(Mandatory)] [string] $Message)

    Write-Host "[integration] ERROR: $Message" -ForegroundColor Red
    exit 1
}

if (-not (Test-Path $devExecutable -PathType Leaf)) {
    Stop-IntegrationValidation -Message "Development executable not found: $devExecutable"
}
if (-not $RequestId) {
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $RequestId = "akuworkspace-integration-$stamp-$PID"
}

$statusArguments = @('status', '--json')
if ($Config) {
    $statusArguments += @('--config', $Config)
}

Write-Host '[integration] Checking supervised AkuSidecar prerequisite...' -ForegroundColor Cyan
$statusOutput = & $devExecutable @statusArguments
$statusExitCode = $LASTEXITCODE
$statusJson = ($statusOutput | Out-String).Trim()
try {
    $status = $statusJson | ConvertFrom-Json -ErrorAction Stop
} catch {
    Stop-IntegrationValidation -Message 'Could not read valid Supervisor status. Keep the development watcher running and retry.'
}
if ($statusExitCode -ne 0) {
    Stop-IntegrationValidation -Message "Could not reach the development Supervisor (exit code $statusExitCode)."
}

$sidecar = @($status.response.services | Where-Object { $_.id -eq 'akusidecar' } | Select-Object -First 1)
if ($sidecar.Count -eq 0) {
    Stop-IntegrationValidation -Message "The active configuration does not register service 'akusidecar'."
}
$sidecar = $sidecar[0]
if ($sidecar.desiredState -ne 'running' -or $sidecar.lifecycle -ne 'running') {
    Write-Host '[integration] AkuSidecar is stopped. Start it from a second terminal:' -ForegroundColor Yellow
    Write-Host ".\target\dev\aku-supervisor.exe start akusidecar --actor $Actor --reason `"prepare AkuWorkspace integration validation`"" -ForegroundColor Yellow
    if ($Config) {
        Write-Host "Add: --config `"$Config`"" -ForegroundColor Yellow
    }
    Write-Host '[integration] Keep the watcher, AkuSidecar, and the AkuBrowser tab with AkuBridge alive, then retry.' -ForegroundColor Yellow
    Stop-IntegrationValidation -Message 'AkuSidecar is not running.'
}
if ($sidecar.health.status -ne 'healthy') {
    Write-Host "[integration] AkuSidecar lifecycle is running but health is '$($sidecar.health.status)'." -ForegroundColor Yellow
    Write-Host '[integration] Inspect: .\target\dev\aku-supervisor.exe status --json' -ForegroundColor Yellow
    Write-Host '[integration] Inspect: .\target\dev\aku-supervisor.exe logs akusidecar --stream stderr --tail 100' -ForegroundColor Yellow
    Stop-IntegrationValidation -Message 'AkuSidecar is not healthy.'
}

$arguments = @(
    'bridge',
    'validate',
    '--actor', $Actor,
    '--request-id', $RequestId
)
if ($Config) {
    $arguments += @('--config', $Config)
}

Write-Host "[integration] Validating AkuBridge with request $RequestId..." -ForegroundColor Cyan
$validationOutput = & $devExecutable @arguments
$validationExitCode = $LASTEXITCODE
$validationJson = ($validationOutput | Out-String).Trim()
try {
    $validation = $validationJson | ConvertFrom-Json -ErrorAction Stop
} catch {
    Stop-IntegrationValidation -Message 'bridge validate returned invalid JSON.'
}
if ($validationExitCode -ne 0 -or $validation.validation.status -ne 'passed') {
    $category = [string] $validation.validation.operation.errorCategory
    $message = [string] $validation.validation.operation.message
    if ($category -eq 'relay_unreachable') {
        Write-Host '[integration] AkuSidecar became unreachable during bridge validation.' -ForegroundColor Yellow
    }
    if ($category -eq 'relay_page_stale') {
        Write-Host '[integration] AkuSidecar is healthy, but the open AkuBrowser page is not polling the cooperative relay.' -ForegroundColor Yellow
        Write-Host '[integration] Reload only the existing http://127.0.0.1:11122 AkuBrowser tab.' -ForegroundColor Yellow
        Write-Host '[integration] Wait until the page shows AkuSidecar ready and AkuBridge ready, then retry.' -ForegroundColor Yellow
    }
    if ($message) {
        Write-Host "[integration] Detail: $message" -ForegroundColor Yellow
    }
    $failure = if ($category) { $category } else { 'validation_failed' }
    Stop-IntegrationValidation -Message "bridge validate failed ($failure) with exit code $validationExitCode."
}

Write-Host '[integration] AkuWorkspace integration validation passed.' -ForegroundColor Green
Write-Output $validationJson
