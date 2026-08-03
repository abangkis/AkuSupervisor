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
    [int] $ShutdownTimeoutSeconds = 30,
    [ValidateSet('local', 'utc')]
    [string] $Timezone = 'local',
    [switch] $Rebuild,
    [switch] $BootstrapOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$supervisorRepository = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $supervisorRepository
$sidecarBuildScript = Join-Path $workspaceRoot 'AkuSidecar\scripts\build-dev.ps1'
$supervisorDevScript = Join-Path $PSScriptRoot 'dev.ps1'
$requestedServices = @($StartService | Where-Object { $_ } | Sort-Object -Unique)

function Get-AkuSidecarSourceState {
    $sidecarRepository = Split-Path -Parent (Split-Path -Parent $sidecarBuildScript)
    $domainSourcePath = Join-Path $sidecarRepository 'internal\domain\types.go'
    $domainSource = Get-Content -LiteralPath $domainSourcePath -Raw
    if ($domainSource -notmatch 'ApplicationVersion\s*=\s*"([^"]+)"') {
        throw "AkuSidecar ApplicationVersion could not be read: $domainSourcePath"
    }
    $version = $Matches[1]
    $commit = (& git -C $sidecarRepository rev-parse HEAD | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($commit)) {
        throw 'AkuSidecar source commit could not be read.'
    }
    $dirty = -not [string]::IsNullOrWhiteSpace(
        (& git -C $sidecarRepository status --porcelain | Out-String).Trim())

    return [pscustomobject]@{
        Repository = $sidecarRepository
        Version = $version
        Commit = $commit
        Dirty = $dirty
    }
}

function Get-AkuSidecarRuntimeStatus {
    param([Parameter(Mandatory)] [object] $SourceState)

    $binaryPath = Join-Path $SourceState.Repository 'runtime\dev\aku-sidecar.exe'
    $provenancePath = "$binaryPath.runtime-state.json"
    if (-not (Test-Path $binaryPath -PathType Leaf)) {
        return [pscustomobject]@{ Current = $false; Reason = 'binary missing' }
    }
    if (-not (Test-Path $provenancePath -PathType Leaf)) {
        return [pscustomobject]@{ Current = $false; Reason = 'provenance missing' }
    }

    try {
        $provenance = Get-Content -LiteralPath $provenancePath -Raw | ConvertFrom-Json
    }
    catch {
        return [pscustomobject]@{ Current = $false; Reason = 'provenance unreadable' }
    }
    if ([string] $provenance.component -ne 'AkuSidecar') {
        return [pscustomobject]@{ Current = $false; Reason = 'provenance component mismatch' }
    }
    if ([string] $provenance.version -ne [string] $SourceState.Version) {
        return [pscustomobject]@{
            Current = $false
            Reason = "version mismatch ($($provenance.version) != $($SourceState.Version))"
        }
    }
    if ([string] $provenance.sourceCommit -ne [string] $SourceState.Commit) {
        return [pscustomobject]@{ Current = $false; Reason = 'source commit mismatch' }
    }
    if ($SourceState.Dirty -or [bool] $provenance.sourceDirty) {
        return [pscustomobject]@{ Current = $false; Reason = 'source working tree is or was dirty' }
    }
    $binaryHash = (Get-FileHash -LiteralPath $binaryPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $recordedHash = [string] $provenance.binarySha256
    if ([string]::IsNullOrWhiteSpace($recordedHash)) {
        return [pscustomobject]@{ Current = $false; Reason = 'provenance hash missing' }
    }
    if ($binaryHash -ne $recordedHash.ToLowerInvariant()) {
        return [pscustomobject]@{ Current = $false; Reason = 'binary hash mismatch' }
    }

    return [pscustomobject]@{ Current = $true; Reason = 'version, commit, cleanliness, and hash match' }
}

function Initialize-AkuSidecarDevelopmentRuntime {
    if (-not (Test-Path $sidecarBuildScript -PathType Leaf)) {
        throw "AkuSidecar development build script was not found: $sidecarBuildScript"
    }

    $sourceState = Get-AkuSidecarSourceState
    $runtimeStatus = Get-AkuSidecarRuntimeStatus -SourceState $sourceState
    if (-not $Rebuild -and $runtimeStatus.Current) {
        Write-Host "[workspace] AkuSidecar development runtime is current ($($sourceState.Version), $($sourceState.Commit.Substring(0, 7)))." -ForegroundColor Green
        return
    }

    $reason = if ($Rebuild) { 'forced by -Rebuild' } else { $runtimeStatus.Reason }
    Write-Host "[workspace] Rebuilding AkuSidecar development runtime: $reason." -ForegroundColor Cyan
    & $sidecarBuildScript
    if ($LASTEXITCODE -ne 0) {
        throw "AkuSidecar development build failed with exit code $LASTEXITCODE."
    }
}

if ('akusidecar' -in $requestedServices) {
    Initialize-AkuSidecarDevelopmentRuntime
}

if ($BootstrapOnly) {
    exit 0
}

if (-not (Test-Path $supervisorDevScript -PathType Leaf)) {
    throw "AkuSupervisor development watcher was not found: $supervisorDevScript"
}

$devParameters = @{
    PollMilliseconds = $PollMilliseconds
    DebounceMilliseconds = $DebounceMilliseconds
    ShutdownTimeoutSeconds = $ShutdownTimeoutSeconds
    Timezone = $Timezone
    Rebuild = $false
}
if ($Config) {
    $devParameters.Config = $Config
}

& $supervisorDevScript @requestedServices @devParameters
exit $LASTEXITCODE
