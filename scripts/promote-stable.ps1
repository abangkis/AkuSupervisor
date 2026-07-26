[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$devExecutable = Join-Path $repository 'target\dev\aku-supervisor.exe'
$stableExecutable = Join-Path $repository 'target\aku-supervisor.exe'
$mcpHostStatusScript = Join-Path $PSScriptRoot 'get-mcp-host-status.ps1'

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

function Show-McpHostStatus {
    try {
        $statusOutput = @(& $mcpHostStatusScript `
            -SourcePath $stableExecutable `
            -Json 2>&1)
        if ($LASTEXITCODE -ne 0) {
            throw ($statusOutput -join [Environment]::NewLine)
        }
        $mcpStatus = ($statusOutput -join "`n") | ConvertFrom-Json
    } catch {
        Write-Host "[release] MCP host status: UNKNOWN ($($_.Exception.Message))" -ForegroundColor Yellow
        return
    }

    if ($mcpStatus.status -eq 'CURRENT') {
        Write-Host '[release] MCP host status: CURRENT (matches stable core).' -ForegroundColor Green
        Write-Host "[release] MCP host: $($mcpStatus.hostPath)" -ForegroundColor DarkGray
        return
    }
    if ($mcpStatus.status -eq 'CORE_ONLY_CHANGE') {
        Write-Host '[release] MCP host status: CORE_ONLY_CHANGE (binary differs; agent contract is current).' -ForegroundColor Green
        Write-Host "[release] MCP contract: $($mcpStatus.sourceContractFingerprint)" -ForegroundColor DarkGray
        Write-Host '[release] MCP restaging and Codex restart are not required.' -ForegroundColor Green
        return
    }

    Write-Host "[release] MCP host status: $($mcpStatus.status) (agent contract does not match stable core)." -ForegroundColor Yellow
    Write-Host "[release] Expected immutable MCP host: $($mcpStatus.hostPath)" -ForegroundColor DarkGray
    Write-Host '[release] Registration tools or schema exposed to agents may be stale.' -ForegroundColor Yellow
    Write-Host '[release] Run .\scripts\install-codex-mcp.ps1, apply its reviewed command, and restart Codex when instructed.' -ForegroundColor Yellow
}

function Assert-StableExecutableUnlocked {
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
        Write-Host '[release] Stable executable is locked before promotion.' -ForegroundColor Yellow
        foreach ($user in $users) {
            Write-Host "[release] Lock owner candidate: PID $($user.Id) $($user.Path)" -ForegroundColor Yellow
        }
        Write-Host '[release] Stop or recycle only the process using target\aku-supervisor.exe, then rerun promotion.' -ForegroundColor Yellow
        Write-Host '[release] A correctly staged MCP host uses target\mcp and does not lock stable; this process may be using a legacy MCP configuration.' -ForegroundColor Yellow
        Write-Host '[release] After promotion, run .\scripts\stage-mcp-host.ps1 only when MCP behavior itself must be updated.' -ForegroundColor Yellow
        Stop-Promotion -Message 'Stable executable is in use and was not changed.'
    } finally {
        if ($null -ne $handle) {
            $handle.Dispose()
        }
    }
}

if (-not (Test-Path $devExecutable -PathType Leaf)) {
    Stop-Promotion -Message "Development executable not found: $devExecutable; stable was not changed."
}

Write-Host '[release] Checking development executable...' -ForegroundColor Cyan
$versionOutput = (& $devExecutable --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^aku-supervisor\s+\S+$') {
    Stop-Promotion -Message 'Development executable failed its bounded version preflight; stable was not changed.'
}
Write-Host "[release] Candidate: $versionOutput" -ForegroundColor DarkGray

$developmentHash = (Get-FileHash -LiteralPath $devExecutable -Algorithm SHA256).Hash
if (Test-Path $stableExecutable -PathType Leaf) {
    $stableHash = (Get-FileHash -LiteralPath $stableExecutable -Algorithm SHA256).Hash
    if ($stableHash -eq $developmentHash) {
        Write-Host '[release] Stable is already current; no copy is required.' -ForegroundColor Green
        Show-McpHostStatus
        Write-Host '[release] AkuWorkspace integration validation is separate and was not run.' -ForegroundColor DarkGray
        exit 0
    }
}

Assert-StableExecutableUnlocked
try {
    Copy-Item -LiteralPath $devExecutable -Destination $stableExecutable -Force
} catch {
    Stop-Promotion -Message "Could not replace stable executable: $($_.Exception.Message)"
}

$promotedHash = (Get-FileHash -LiteralPath $stableExecutable -Algorithm SHA256).Hash
if ($promotedHash -ne $developmentHash) {
    Stop-Promotion -Message 'Stable executable hash does not match the development candidate.'
}

Write-Host "[release] Promoted core AkuSupervisor build to: $stableExecutable" -ForegroundColor Green
Write-Host "[release] SHA-256: $promotedHash" -ForegroundColor DarkGray
Show-McpHostStatus
Write-Host '[release] AkuWorkspace integration validation is separate and was not run.' -ForegroundColor Yellow
Write-Host '[release] When a change affects AkuSidecar/AkuBridge integration, run .\scripts\validate-akuworkspace-integration.ps1 explicitly.' -ForegroundColor Yellow
