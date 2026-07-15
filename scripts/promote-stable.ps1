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
$stableExecutable = Join-Path $repository 'target\aku-supervisor.exe'

function Stop-Promotion {
    param([Parameter(Mandatory)] [string] $Message)

    Write-Host "[release] ERROR: $Message" -ForegroundColor Red
    exit 1
}

function Get-StableExecutableUsers {
    if (-not (Test-Path $stableExecutable -PathType Leaf)) {
        return @()
    }

    $stableFullPath = [System.IO.Path]::GetFullPath($stableExecutable)
    return @(Get-Process -Name 'aku-supervisor' -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            if ([System.StringComparer]::OrdinalIgnoreCase.Equals($_.Path, $stableFullPath)) {
                [pscustomobject]@{
                    Id = $_.Id
                    Path = $_.Path
                }
            }
        } catch {
            # A process may exit between enumeration and path inspection.
        }
    })
}

function Assert-StableExecutableUnlocked {
    param([Parameter(Mandatory)] [string] $Stage)

    if (-not (Test-Path $stableExecutable -PathType Leaf)) {
        return
    }

    $handle = $null
    try {
        $handle = [System.IO.File]::Open(
            $stableExecutable,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
    } catch {
        $users = @(Get-StableExecutableUsers)
        Write-Host "[release] Stable executable is locked $Stage." -ForegroundColor Yellow
        foreach ($user in $users) {
            Write-Host "[release] Lock owner candidate: PID $($user.Id) $($user.Path)" -ForegroundColor Yellow
        }
        Write-Host '[release] Keep the development watcher and supervised AkuSidecar running.' -ForegroundColor Yellow
        Write-Host '[release] Stop or recycle only the process using target\aku-supervisor.exe, then rerun promotion.' -ForegroundColor Yellow
        Write-Host '[release] A long-lived mcp-proxy using the stable executable must be recycled; the watcher uses target\dev and should not be stopped.' -ForegroundColor Yellow
        Stop-Promotion -Message 'Stable executable is in use; bridge validation and promotion were not allowed to continue.'
    } finally {
        if ($null -ne $handle) {
            $handle.Dispose()
        }
    }
}

if (-not (Test-Path $devExecutable -PathType Leaf)) {
    Stop-Promotion -Message "Development executable not found: $devExecutable; stable was not changed."
}
Assert-StableExecutableUnlocked -Stage 'before release validation'
if (-not $RequestId) {
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $RequestId = "bridge-release-$stamp-$PID"
}

$statusArguments = @('status', '--json')
if ($Config) {
    $statusArguments += @('--config', $Config)
}

Write-Host '[release] Checking supervised AkuSidecar prerequisite...' -ForegroundColor Cyan
$statusOutput = & $devExecutable @statusArguments
$statusExitCode = $LASTEXITCODE
$statusJson = ($statusOutput | Out-String).Trim()
try {
    $status = $statusJson | ConvertFrom-Json -ErrorAction Stop
} catch {
    Stop-Promotion -Message 'Could not read valid Supervisor status; stable was not changed. Keep the development watcher running and retry.'
}
if ($statusExitCode -ne 0) {
    Stop-Promotion -Message "Could not reach the development Supervisor (exit code $statusExitCode); stable was not changed. Keep the watcher running and retry."
}

$sidecar = @($status.response.services | Where-Object { $_.id -eq 'akusidecar' } | Select-Object -First 1)
if ($sidecar.Count -eq 0) {
    Stop-Promotion -Message "The active configuration does not register service 'akusidecar'; stable was not changed."
}
$sidecar = $sidecar[0]
if ($sidecar.desiredState -ne 'running' -or $sidecar.lifecycle -ne 'running') {
    Write-Host '[release] AkuSidecar is stopped. Start it from a second terminal:' -ForegroundColor Yellow
    Write-Host ".\target\dev\aku-supervisor.exe start akusidecar --actor $Actor --reason `"prepare stable promotion`"" -ForegroundColor Yellow
    if ($Config) {
        Write-Host "Add: --config `"$Config`"" -ForegroundColor Yellow
    }
    Write-Host '[release] Keep the watcher, AkuSidecar, and the AkuBrowser tab with AkuBridge alive, then rerun promotion.' -ForegroundColor Yellow
    Stop-Promotion -Message 'AkuSidecar is not running; stable was not changed.'
}
if ($sidecar.health.status -ne 'healthy') {
    Write-Host "[release] AkuSidecar lifecycle is running but health is '$($sidecar.health.status)'." -ForegroundColor Yellow
    Write-Host '[release] Inspect: .\target\dev\aku-supervisor.exe status --json' -ForegroundColor Yellow
    Write-Host '[release] Inspect: .\target\dev\aku-supervisor.exe logs akusidecar --stream stderr --tail 100' -ForegroundColor Yellow
    Stop-Promotion -Message 'AkuSidecar is not healthy; stable was not changed.'
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

Write-Host "[release] Validating AkuBridge with request $RequestId..." -ForegroundColor Cyan
$validationOutput = & $devExecutable @arguments
$validationExitCode = $LASTEXITCODE
$validationJson = ($validationOutput | Out-String).Trim()
try {
    $validation = $validationJson | ConvertFrom-Json -ErrorAction Stop
} catch {
    Stop-Promotion -Message 'bridge validate returned invalid JSON; stable was not changed.'
}
if ($validationExitCode -ne 0 -or $validation.validation.status -ne 'passed') {
    $category = [string] $validation.validation.operation.errorCategory
    $message = [string] $validation.validation.operation.message
    if ($category -eq 'relay_unreachable') {
        Write-Host '[release] AkuSidecar became unreachable during bridge validation.' -ForegroundColor Yellow
        Write-Host '[release] Keep the watcher, AkuSidecar, and the AkuBrowser tab with AkuBridge alive, then retry.' -ForegroundColor Yellow
    }
    if ($category -eq 'relay_page_stale') {
        Write-Host '[release] AkuSidecar is healthy, but the open AkuBrowser page is not polling the cooperative relay.' -ForegroundColor Yellow
        Write-Host '[release] Reload only the existing http://127.0.0.1:47821 AkuBrowser tab.' -ForegroundColor Yellow
        Write-Host '[release] Wait until that page shows AkuSidecar ready and AkuBridge ready, then rerun promotion without stopping the watcher.' -ForegroundColor Yellow
    }
    if ($message) {
        Write-Host "[release] Detail: $message" -ForegroundColor Yellow
    }
    $failure = if ($category) { $category } else { 'validation_failed' }
    Stop-Promotion -Message "bridge validate failed ($failure) with exit code $validationExitCode; stable was not changed."
}

Assert-StableExecutableUnlocked -Stage 'after release validation'
Copy-Item -LiteralPath $devExecutable -Destination $stableExecutable -Force
Write-Host "[release] Promoted validated build to: $stableExecutable" -ForegroundColor Green
Write-Output $validationJson
