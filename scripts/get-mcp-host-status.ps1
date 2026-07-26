[CmdletBinding()]
param(
    [string] $SourcePath,
    [string] $HostPath,
    [switch] $Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($SourcePath)) {
    $SourcePath = Join-Path $repository 'target\aku-supervisor.exe'
}

$sourceFullPath = [IO.Path]::GetFullPath($SourcePath)
if (-not (Test-Path -LiteralPath $sourceFullPath -PathType Leaf)) {
    Write-Error "MCP host status source executable was not found: $sourceFullPath"
    exit 1
}

$sourceHash = (Get-FileHash -LiteralPath $sourceFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
$hostSelection = if ([string]::IsNullOrWhiteSpace($HostPath)) {
    $HostPath = Join-Path $repository "target\mcp\sha256-$sourceHash\aku-supervisor-mcp.exe"
    'content-addressed-default'
} else {
    'explicit'
}
$hostFullPath = [IO.Path]::GetFullPath($HostPath)
$hostHash = if (Test-Path -LiteralPath $hostFullPath -PathType Leaf) {
    (Get-FileHash -LiteralPath $hostFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
} else {
    $null
}
$status = if ($null -eq $hostHash) {
    'MISSING'
} elseif ([StringComparer]::Ordinal.Equals($sourceHash, $hostHash)) {
    'CURRENT'
} else {
    'OUTDATED'
}

$result = [ordered]@{
    schemaVersion = 1
    status = $status
    sourcePath = $sourceFullPath
    sourceHash = "sha256:$sourceHash"
    hostPath = $hostFullPath
    hostHash = if ($null -eq $hostHash) { $null } else { "sha256:$hostHash" }
    hostSelection = $hostSelection
    restartCodexRequired = $status -ne 'CURRENT'
}

if ($Json) {
    $result | ConvertTo-Json -Compress
} else {
    Write-Host "MCP host status: $status"
    Write-Host "Source: $sourceFullPath"
    Write-Host "Host: $hostFullPath"
}
