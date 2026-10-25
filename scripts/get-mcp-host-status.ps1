[CmdletBinding()]
param(
    [string] $SourcePath,
    [string] $HostPath,
    [string] $CodexConfigPath,
    [switch] $Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$utilityModule = Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1'
Import-Module -Name $utilityModule -Force -ErrorAction Stop

. (Join-Path $PSScriptRoot 'mcp-host-common.ps1')

$repository = Split-Path -Parent $PSScriptRoot
$workspace = Split-Path -Parent $repository
if ([string]::IsNullOrWhiteSpace($SourcePath)) {
    $SourcePath = Join-Path $repository 'target\aku-supervisor.exe'
}
if ([string]::IsNullOrWhiteSpace($CodexConfigPath)) {
    $CodexConfigPath = Join-Path $workspace '.codex\config.toml'
}

$sourceFullPath = [IO.Path]::GetFullPath($SourcePath)
$configFullPath = [IO.Path]::GetFullPath($CodexConfigPath)
if (-not (Test-Path -LiteralPath $sourceFullPath -PathType Leaf)) {
    Write-Error "MCP host status source executable was not found: $sourceFullPath"
    exit 1
}

$sourceHash = (Get-FileHash -LiteralPath $sourceFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
$sourceContract = Read-McpContract -Executable $sourceFullPath
$hostSelection = if ([string]::IsNullOrWhiteSpace($HostPath)) {
    $configuredHost = if (Test-Path -LiteralPath $configFullPath -PathType Leaf) {
        Get-ConfiguredMcpHost -Content ([IO.File]::ReadAllText($configFullPath))
    } else {
        $null
    }
    if ($null -ne $configuredHost) {
        $HostPath = $configuredHost
        'configured-default'
    } else {
        $HostPath = Join-Path $repository "target\mcp\sha256-$sourceHash\aku-supervisor-mcp.exe"
        'content-addressed-fallback'
    }
} else {
    'explicit'
}
$hostFullPath = [IO.Path]::GetFullPath($HostPath)
$hostHash = if (Test-Path -LiteralPath $hostFullPath -PathType Leaf) {
    (Get-FileHash -LiteralPath $hostFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
} else {
    $null
}
$hostContract = if ($null -eq $hostHash) {
    $null
} else {
    Read-McpContract -Executable $hostFullPath -AllowUnavailable
}
$status = if ($null -eq $hostHash) {
    'MISSING'
} elseif ([StringComparer]::Ordinal.Equals($sourceHash, $hostHash)) {
    'CURRENT'
} elseif ($null -ne $hostContract -and
    [StringComparer]::Ordinal.Equals($sourceContract.fingerprint, $hostContract.fingerprint)) {
    'CORE_ONLY_CHANGE'
} else {
    'OUTDATED'
}

$result = [ordered]@{
    schemaVersion = 1
    status = $status
    sourcePath = $sourceFullPath
    sourceHash = "sha256:$sourceHash"
    sourceContractFingerprint = $sourceContract.fingerprint
    hostPath = $hostFullPath
    hostHash = if ($null -eq $hostHash) { $null } else { "sha256:$hostHash" }
    hostContractFingerprint = if ($null -eq $hostContract) { $null } else { $hostContract.fingerprint }
    hostSelection = $hostSelection
    restartCodexRequired = $status -in @('OUTDATED', 'MISSING')
}

if ($Json) {
    $result | ConvertTo-Json -Compress
} else {
    Write-Host "MCP host status: $status"
    Write-Host "Source: $sourceFullPath"
    Write-Host "Host: $hostFullPath"
}
