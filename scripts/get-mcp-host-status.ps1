[CmdletBinding()]
param(
    [string] $SourcePath,
    [string] $HostPath,
    [string] $CodexConfigPath,
    [switch] $Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$workspace = Split-Path -Parent $repository
if ([string]::IsNullOrWhiteSpace($SourcePath)) {
    $SourcePath = Join-Path $repository 'target\aku-supervisor.exe'
}
if ([string]::IsNullOrWhiteSpace($CodexConfigPath)) {
    $CodexConfigPath = Join-Path $workspace '.codex\config.toml'
}

function Get-McpContract {
    param(
        [Parameter(Mandatory)] [string] $Executable,
        [switch] $AllowUnavailable
    )

    $previousErrorPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $output = @(& $Executable mcp-contract --json 2>&1)
        $contractExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousErrorPreference
    }
    $raw = (($output | Where-Object { $_ -is [string] -and $_.TrimStart().StartsWith('{') }) -join "`n").Trim()
    if ($contractExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($raw)) {
        if ($AllowUnavailable) {
            return $null
        }
        throw "Source executable does not expose the MCP contract report: $Executable"
    }
    try {
        $report = $raw | ConvertFrom-Json
        if ($report.schemaVersion -ne 1 -or
            $report.fingerprint -notmatch '^sha256:[a-f0-9]{64}$') {
            throw 'unexpected MCP contract report'
        }
        return $report
    } catch {
        if ($AllowUnavailable) {
            return $null
        }
        throw "Invalid MCP contract report from ${Executable}: $($_.Exception.Message)"
    }
}

function Get-ConfiguredMcpHost {
    param([Parameter(Mandatory)] [string] $ConfigPath)

    if (-not (Test-Path -LiteralPath $ConfigPath -PathType Leaf)) {
        return $null
    }
    $content = [IO.File]::ReadAllText($ConfigPath)
    $paths = @()
    foreach ($sectionName in @('mcp_servers.aku_supervisor', 'mcp_servers.aku_supervisor_registration')) {
        $escaped = [Text.RegularExpressions.Regex]::Escape($sectionName)
        $section = [Text.RegularExpressions.Regex]::Match(
            $content,
            "(?ms)^\[$escaped\][ \t]*(?:\r?\n|$).*?(?=^\[|\z)"
        )
        if (-not $section.Success) {
            return $null
        }
        $command = [Text.RegularExpressions.Regex]::Match(
            $section.Value,
            '(?m)^command[ \t]*=[ \t]*"((?:\\.|[^"])*)"'
        )
        if (-not $command.Success) {
            return $null
        }
        $paths += $command.Groups[1].Value.Replace('\\', '\').Replace('\"', '"')
    }
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals($paths[0], $paths[1])) {
        throw 'AkuSupervisor read-only and registration MCP entries select different hosts.'
    }
    return [IO.Path]::GetFullPath($paths[0])
}

$sourceFullPath = [IO.Path]::GetFullPath($SourcePath)
$configFullPath = [IO.Path]::GetFullPath($CodexConfigPath)
if (-not (Test-Path -LiteralPath $sourceFullPath -PathType Leaf)) {
    Write-Error "MCP host status source executable was not found: $sourceFullPath"
    exit 1
}

$sourceHash = (Get-FileHash -LiteralPath $sourceFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
$sourceContract = Get-McpContract -Executable $sourceFullPath
$hostSelection = if ([string]::IsNullOrWhiteSpace($HostPath)) {
    $configuredHost = Get-ConfiguredMcpHost -ConfigPath $configFullPath
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
    Get-McpContract -Executable $hostFullPath -AllowUnavailable
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
