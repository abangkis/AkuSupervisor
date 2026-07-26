[CmdletBinding()]
param(
    [ValidateSet('stable', 'development')]
    [string] $Source = 'stable',

    [string] $SourcePath,

    [string] $DestinationPath,

    [string] $ExpectedSourceHash,

    [string] $ExpectedContractFingerprint
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

function Assert-RegistrationDiscovery {
    param([Parameter(Mandatory)] [string] $Executable)

    $missingConfig = Join-Path $repository "target\mcp-registration-probe-missing-$PID.json"
    $requests = @(
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"mcp-host-staging","version":"1"}}}',
        '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}',
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"supervisor_registration_get_schema","arguments":{}}}'
    )
    $raw = @($requests | & $Executable registration-mcp --config $missingConfig 2>&1)
    $discoveryExitCode = $LASTEXITCODE
    if ($discoveryExitCode -ne 0) {
        $detail = (($raw | Select-Object -First 1) -join '').Trim()
        if ($detail) {
            Write-Host "[mcp-host] Registration discovery detail: $detail" -ForegroundColor Yellow
        }
        Stop-Staging -Message 'Source executable cannot bootstrap registration MCP discovery without a runtime configuration.'
    }

    try {
        $responses = @(
            $raw |
                Where-Object { $_ -is [string] -and $_.TrimStart().StartsWith('{') } |
                ForEach-Object { $_ | ConvertFrom-Json }
        )
        $tools = @(
            ($responses | Where-Object id -eq 2).result.tools |
                ForEach-Object { $_.name }
        )
        $schemaResponse = $responses | Where-Object id -eq 3
        if ($responses.Count -ne 3 -or
            $tools -notcontains 'supervisor_registration_get_schema' -or
            $schemaResponse.result.isError -ne $false -or
            $null -eq $schemaResponse.result.structuredContent.serviceSchema) {
            throw 'registration discovery response contract mismatch'
        }
    } catch {
        Stop-Staging -Message "Source executable registration discovery preflight failed: $($_.Exception.Message)"
    }
}

if (-not (Test-Path -LiteralPath $sourceExecutable -PathType Leaf)) {
    Stop-Staging -Message "Source executable not found: $sourceExecutable"
}

Write-Host "[mcp-host] Checking source executable: $sourceExecutable" -ForegroundColor Cyan
$versionOutput = (& $sourceExecutable --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch '^aku-supervisor\s+\S+$') {
    Stop-Staging -Message 'Source executable failed its bounded version preflight.'
}
Assert-RegistrationDiscovery -Executable $sourceExecutable
$sourceContract = (& $sourceExecutable mcp-contract --json | Out-String).Trim() | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or $sourceContract.fingerprint -notmatch '^sha256:[a-f0-9]{64}$') {
    Stop-Staging -Message 'Source executable does not expose a valid MCP contract fingerprint.'
}
$sourceHash = (Get-FileHash -LiteralPath $sourceExecutable -Algorithm SHA256).Hash
if (-not [string]::IsNullOrWhiteSpace($ExpectedSourceHash)) {
    $normalizedExpectedHash = $ExpectedSourceHash.Replace('sha256:', '').ToUpperInvariant()
    if (-not [System.StringComparer]::Ordinal.Equals($sourceHash, $normalizedExpectedHash)) {
        Stop-Staging -Message 'Source executable hash no longer matches the approved proposal.'
    }
}
if (-not [string]::IsNullOrWhiteSpace($ExpectedContractFingerprint) -and
    -not [StringComparer]::Ordinal.Equals(
        $sourceContract.fingerprint,
        $ExpectedContractFingerprint.ToLowerInvariant()
    )) {
    Stop-Staging -Message 'Source MCP contract fingerprint no longer matches the approved proposal.'
}
$hostExecutable = if (-not [string]::IsNullOrWhiteSpace($DestinationPath)) {
    [System.IO.Path]::GetFullPath($DestinationPath)
} else {
    Join-Path $repository "target\mcp\sha256-$($sourceHash.ToLowerInvariant())\aku-supervisor-mcp.exe"
}
$hostDirectory = Split-Path -Parent $hostExecutable

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
        Write-Host '[mcp-host] The explicitly selected MCP host is active and cannot be updated in place.' -ForegroundColor Yellow
        foreach ($user in $users) {
            Write-Host "[mcp-host] Active MCP host: PID $($user.Id) $($user.Path)" -ForegroundColor Yellow
        }
        Write-Host '[mcp-host] Core stable promotion remains independent and may still run.' -ForegroundColor Green
        Write-Host '[mcp-host] Use the default content-addressed destination to stage changed bytes beside this active host.' -ForegroundColor Yellow
        Stop-Staging -Message 'Explicit MCP host destination is in use and was not changed.'
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
$hostContract = (& $hostExecutable mcp-contract --json | Out-String).Trim() | ConvertFrom-Json
if ($LASTEXITCODE -ne 0 -or
    $hostVersion -ne $versionOutput -or
    $hostHash -ne $sourceHash -or
    $hostContract.fingerprint -ne $sourceContract.fingerprint) {
    Stop-Staging -Message 'Staged MCP host failed final version, hash, or contract verification.'
}

Write-Host "[mcp-host] Staged dedicated MCP host: $hostExecutable" -ForegroundColor Green
Write-Host "[mcp-host] Version: $hostVersion" -ForegroundColor DarkGray
Write-Host "[mcp-host] SHA-256: $hostHash" -ForegroundColor DarkGray
Write-Host "[mcp-host] Contract: $($hostContract.fingerprint)" -ForegroundColor DarkGray
Write-Host '[mcp-host] Point Codex MCP entries at this immutable executable. Restart Codex only after its configuration is changed to select this version.' -ForegroundColor Yellow
