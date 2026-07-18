[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $StartService = @(),
    [string] $Config,
    [ValidateRange(100, 5000)]
    [int] $PollMilliseconds = 300,
    [ValidateRange(100, 10000)]
    [int] $DebounceMilliseconds = 600,
    [ValidateRange(5, 120)]
    [int] $ShutdownTimeoutSeconds = 30
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$supervisorRepository = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $supervisorRepository
$sidecarBuildScript = Join-Path $workspaceRoot 'AkuSidecar\scripts\build-dev.ps1'
$supervisorDevScript = Join-Path $PSScriptRoot 'dev.ps1'
$requestedServices = @($StartService | Where-Object { $_ } | Sort-Object -Unique)

if ('akusidecar' -in $requestedServices) {
    if (-not (Test-Path $sidecarBuildScript -PathType Leaf)) {
        throw "AkuSidecar development build script was not found: $sidecarBuildScript"
    }

    Write-Host '[workspace] Bootstrapping AkuSidecar generated runtime...' -ForegroundColor Cyan
    & $sidecarBuildScript
    if ($LASTEXITCODE -ne 0) {
        throw "AkuSidecar development build failed with exit code $LASTEXITCODE."
    }
}

if (-not (Test-Path $supervisorDevScript -PathType Leaf)) {
    throw "AkuSupervisor development watcher was not found: $supervisorDevScript"
}

$devParameters = @{
    PollMilliseconds = $PollMilliseconds
    DebounceMilliseconds = $DebounceMilliseconds
    ShutdownTimeoutSeconds = $ShutdownTimeoutSeconds
}
if ($Config) {
    $devParameters.Config = $Config
}

& $supervisorDevScript @requestedServices @devParameters
exit $LASTEXITCODE
