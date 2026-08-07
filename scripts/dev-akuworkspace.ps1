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
$bridgeIdentityRegistryPath = Join-Path $workspaceRoot 'AkuBrowser\config\bridge-identities.json'
$supervisorDevScript = Join-Path $PSScriptRoot 'dev.ps1'
$requestedServices = @($StartService | Where-Object { $_ } | Sort-Object -Unique)

function Resolve-AkuSupervisorConfigPath {
    if ($Config) {
        if ([IO.Path]::IsPathRooted($Config)) {
            return [IO.Path]::GetFullPath($Config)
        }
        return [IO.Path]::GetFullPath((Join-Path (Get-Location) $Config))
    }
    if ($env:AKU_SUPERVISOR_CONFIG) {
        return [IO.Path]::GetFullPath($env:AKU_SUPERVISOR_CONFIG)
    }
    if (-not $env:LOCALAPPDATA) {
        throw 'LOCALAPPDATA is unavailable; pass -Config explicitly.'
    }
    return Join-Path $env:LOCALAPPDATA 'AkuSupervisor\services.json'
}

function Get-BridgeIdentityProfile {
    param([Parameter(Mandatory)] [string] $ProfileName)

    if (-not (Test-Path -LiteralPath $bridgeIdentityRegistryPath -PathType Leaf)) {
        throw "Bridge identity registry was not found: $bridgeIdentityRegistryPath"
    }
    $registry = Get-Content -LiteralPath $bridgeIdentityRegistryPath -Raw | ConvertFrom-Json
    if ([int] $registry.schemaVersion -ne 1) {
        throw "Unsupported Bridge identity registry schema: $($registry.schemaVersion)"
    }
    $property = $registry.profiles.PSObject.Properties[$ProfileName]
    if ($null -eq $property) {
        throw "Bridge identity profile '$ProfileName' was not found in $bridgeIdentityRegistryPath"
    }
    $identity = $property.Value
    $extensionId = [string] $identity.extensionId
    if ($extensionId -notmatch '^[a-p]{32}$') {
        throw "Bridge identity profile '$ProfileName' does not contain an exact Chrome extension ID."
    }
    return [pscustomobject]@{
        Profile = $ProfileName
        Distribution = [string] $identity.distribution
        ExtensionId = $extensionId
        ExtensionOrigin = "chrome-extension://$extensionId/"
        Authority = $bridgeIdentityRegistryPath
    }
}

function Set-AkuSidecarDevelopmentBridgeIdentity {
    $identity = Get-BridgeIdentityProfile -ProfileName 'development'
    if ($identity.Distribution -ne 'unpacked') {
        throw "Development Bridge identity must use unpacked distribution, found '$($identity.Distribution)'."
    }

    $configurationPath = Resolve-AkuSupervisorConfigPath
    if (-not (Test-Path -LiteralPath $configurationPath -PathType Leaf)) {
        throw "AkuSupervisor configuration was not found: $configurationPath"
    }
    $configuration = Get-Content -LiteralPath $configurationPath -Raw | ConvertFrom-Json
    $serviceProperty = $configuration.services.PSObject.Properties['akusidecar']
    if ($null -eq $serviceProperty) {
        throw "AkuSupervisor configuration does not register service 'akusidecar': $configurationPath"
    }

    $currentArguments = @($serviceProperty.Value.args)
    $nextArguments = [Collections.Generic.List[string]]::new()
    for ($index = 0; $index -lt $currentArguments.Count; $index++) {
        if ([string] $currentArguments[$index] -eq '--bridge-extension-origin') {
            $index++
            continue
        }
        $nextArguments.Add([string] $currentArguments[$index])
    }
    $nextArguments.Add('--bridge-extension-origin')
    $nextArguments.Add($identity.ExtensionOrigin)
    $nextArgumentArray = @($nextArguments)

    $changed = (($currentArguments | ConvertTo-Json -Compress) -ne ($nextArgumentArray | ConvertTo-Json -Compress))
    if ($changed) {
        $serviceProperty.Value.args = $nextArgumentArray
        $temporaryPath = "$configurationPath.tmp-$PID"
        [IO.File]::WriteAllText(
            $temporaryPath,
            (($configuration | ConvertTo-Json -Depth 20) + "`n"),
            [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $temporaryPath -Destination $configurationPath -Force
    }

    Write-Host '[workspace] AkuBridge identity' -ForegroundColor Cyan
    Write-Host '[workspace]   Channel      : DEVELOPMENT' -ForegroundColor Cyan
    Write-Host "[workspace]   Distribution : $($identity.Distribution)" -ForegroundColor DarkGray
    Write-Host "[workspace]   Extension ID : $($identity.ExtensionId)" -ForegroundColor Green
    Write-Host "[workspace]   Authority    : $($identity.Authority)#profiles.$($identity.Profile)" -ForegroundColor DarkGray
    Write-Host "[workspace]   Projection   : $configurationPath (akusidecar args)" -ForegroundColor DarkGray
    if ($changed) {
        Write-Host '[workspace]   Status       : refreshed generated development projection' -ForegroundColor Green
    }
    else {
        Write-Host '[workspace]   Status       : current' -ForegroundColor Green
    }
}

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
    Set-AkuSidecarDevelopmentBridgeIdentity
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
