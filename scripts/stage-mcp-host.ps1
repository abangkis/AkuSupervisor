[CmdletBinding()]
param(
    [ValidateSet('stable', 'development')]
    [string] $Source = 'stable',

    [string] $SourcePath,

    [string] $DestinationPath,

    [string] $ExpectedSourceHash
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$sourceExecutable = if (-not [string]::IsNullOrWhiteSpace($SourcePath)) {
    [System.IO.Path]::GetFullPath($SourcePath)
} elseif ($Source -eq 'development') {
    Join-Path $repository 'target\dev\aku-supervisor.exe'
} else {
    Join-Path $repository 'target\aku-supervisor.exe'
}
$hostExecutable = if (-not [string]::IsNullOrWhiteSpace($DestinationPath)) {
    [System.IO.Path]::GetFullPath($DestinationPath)
} else {
    Join-Path $repository 'target\mcp\aku-supervisor-mcp.exe'
}
$hostDirectory = Split-Path -Parent $hostExecutable

function Stop-Staging {
    param([Parameter(Mandatory)] [string] $Message)

    Write-Host "[mcp-host] ERROR: $Message" -ForegroundColor Red
    exit 1
}

function Get-ExecutableUsers {
    param([Parameter(Mandatory)] [string] $Executable)

    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
        return @()
    }
    $fullPath = [System.IO.Path]::GetFullPath($Executable)
    return @(Get-Process -ErrorAction SilentlyContinue | ForEach-Object {
        try {
            if ([System.StringComparer]::OrdinalIgnoreCase.Equals($_.Path, $fullPath)) {
                [pscustomobject]@{ Id = $_.Id; Path = $_.Path }
            }
        } catch {
            # A process may exit between enumeration and path inspection.
        }
    })
}

if (-not (Test-Path -LiteralPath $sourceExecutable -PathType Leaf)) {
    Stop-Staging -Message "Source executable not found: $sourceExecutable"
}

Write-Host "[mcp-host] Checking source executable: $sourceExecutable" -ForegroundColor Cyan
$versionOutput = (& $sourceExecutable --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^aku-supervisor\s+\S+$') {
    Stop-Staging -Message 'Source executable failed its bounded version preflight.'
}
$sourceHash = (Get-FileHash -LiteralPath $sourceExecutable -Algorithm SHA256).Hash
if (-not [string]::IsNullOrWhiteSpace($ExpectedSourceHash)) {
    $normalizedExpectedHash = $ExpectedSourceHash.Replace('sha256:', '').ToUpperInvariant()
    if (-not [System.StringComparer]::Ordinal.Equals($sourceHash, $normalizedExpectedHash)) {
        Stop-Staging -Message 'Source executable hash no longer matches the approved proposal.'
    }
}

New-Item -ItemType Directory -Path $hostDirectory -Force | Out-Null
if (Test-Path -LiteralPath $hostExecutable -PathType Leaf) {
    $hostHash = (Get-FileHash -LiteralPath $hostExecutable -Algorithm SHA256).Hash
    if ($hostHash -eq $sourceHash) {
        Write-Host '[mcp-host] MCP host is already current; no copy is required.' -ForegroundColor Green
        Write-Host "[mcp-host] Executable: $hostExecutable" -ForegroundColor DarkGray
        exit 0
    }

    $users = @(Get-ExecutableUsers -Executable $hostExecutable)
    if ($users.Count -gt 0) {
        Write-Host '[mcp-host] The dedicated MCP host is active and cannot be updated in place.' -ForegroundColor Yellow
        foreach ($user in $users) {
            Write-Host "[mcp-host] Active MCP host: PID $($user.Id) $($user.Path)" -ForegroundColor Yellow
        }
        Write-Host '[mcp-host] Core stable promotion remains independent and may still run.' -ForegroundColor Green
        Write-Host '[mcp-host] Update this host only when MCP behavior changes: close Codex, rerun this script, then reopen Codex.' -ForegroundColor Yellow
        Stop-Staging -Message 'Dedicated MCP host is in use and was not changed.'
    }
}

$temporaryExecutable = "$hostExecutable.new-$PID"
try {
    Copy-Item -LiteralPath $sourceExecutable -Destination $temporaryExecutable -Force
    $temporaryHash = (Get-FileHash -LiteralPath $temporaryExecutable -Algorithm SHA256).Hash
    if ($temporaryHash -ne $sourceHash) {
        Stop-Staging -Message 'Staged MCP host hash does not match its source.'
    }
    Move-Item -LiteralPath $temporaryExecutable -Destination $hostExecutable -Force
} catch {
    Stop-Staging -Message "Could not stage the MCP host: $($_.Exception.Message)"
} finally {
    if (Test-Path -LiteralPath $temporaryExecutable -PathType Leaf) {
        Remove-Item -LiteralPath $temporaryExecutable -Force
    }
}

$hostVersion = (& $hostExecutable --version | Out-String).Trim()
$hostHash = (Get-FileHash -LiteralPath $hostExecutable -Algorithm SHA256).Hash
if ($LASTEXITCODE -ne 0 -or $hostVersion -ne $versionOutput -or $hostHash -ne $sourceHash) {
    Stop-Staging -Message 'Staged MCP host failed final version or hash verification.'
}

Write-Host "[mcp-host] Staged dedicated MCP host: $hostExecutable" -ForegroundColor Green
Write-Host "[mcp-host] Version: $hostVersion" -ForegroundColor DarkGray
Write-Host "[mcp-host] SHA-256: $hostHash" -ForegroundColor DarkGray
Write-Host '[mcp-host] Point Codex MCP entries at this executable. A Codex restart is required only after that configuration changes or this host is updated.' -ForegroundColor Yellow
