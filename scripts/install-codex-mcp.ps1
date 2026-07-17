[CmdletBinding()]
param(
    [string] $CodexConfigPath,

    [ValidateSet('stable', 'development')]
    [string] $Source = 'stable',

    [string] $SourcePath,

    [string] $HostPath,

    [switch] $Apply,

    [string] $ApprovalCode,

    [switch] $Json
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$workspace = Split-Path -Parent $repository
if ([string]::IsNullOrWhiteSpace($CodexConfigPath)) {
    $CodexConfigPath = Join-Path $workspace '.codex\config.toml'
}
if ([string]::IsNullOrWhiteSpace($HostPath)) {
    $HostPath = Join-Path $repository 'target\mcp\aku-supervisor-mcp.exe'
}
if ([string]::IsNullOrWhiteSpace($SourcePath)) {
    $SourcePath = if ($Source -eq 'development') {
        Join-Path $repository 'target\dev\aku-supervisor.exe'
    } else {
        Join-Path $repository 'target\aku-supervisor.exe'
    }
}

$configFullPath = [System.IO.Path]::GetFullPath($CodexConfigPath)
$sourceFullPath = [System.IO.Path]::GetFullPath($SourcePath)
$hostFullPath = [System.IO.Path]::GetFullPath($HostPath)
$sectionNames = @(
    'mcp_servers.aku_supervisor',
    'mcp_servers.aku_supervisor_registration'
)

function Get-TextHash {
    param([Parameter(Mandatory)] [AllowEmptyString()] [string] $Text)

    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = (New-Object System.Text.UTF8Encoding($false)).GetBytes($Text)
        return ([System.BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

function ConvertTo-TomlString {
    param([Parameter(Mandatory)] [string] $Value)

    return '"' + $Value.Replace('\', '\\').Replace('"', '\"') + '"'
}

function Get-SectionPattern {
    param([Parameter(Mandatory)] [string] $SectionName)

    $escaped = [System.Text.RegularExpressions.Regex]::Escape($SectionName)
    return "(?ms)^\[$escaped\][ \t]*(?:\r?\n|$).*?(?=^\[|\z)"
}

function Get-TargetSections {
    param([Parameter(Mandatory)] [AllowEmptyString()] [string] $Content)

    $found = New-Object System.Collections.Generic.List[string]
    foreach ($sectionName in $sectionNames) {
        $matches = [System.Text.RegularExpressions.Regex]::Matches(
            $Content,
            (Get-SectionPattern -SectionName $sectionName)
        )
        if ($matches.Count -gt 1) {
            throw "Codex configuration contains duplicate [$sectionName] sections."
        }
        if ($matches.Count -eq 1) {
            $found.Add($matches[0].Value.TrimEnd())
        }
    }
    if ($found.Count -eq 0) {
        return '<not installed>'
    }
    return ($found -join [Environment]::NewLine + [Environment]::NewLine)
}

function Remove-TargetSections {
    param([Parameter(Mandatory)] [AllowEmptyString()] [string] $Content)

    $preserved = $Content
    foreach ($sectionName in $sectionNames) {
        $pattern = Get-SectionPattern -SectionName $sectionName
        $matches = [System.Text.RegularExpressions.Regex]::Matches($preserved, $pattern)
        if ($matches.Count -gt 1) {
            throw "Codex configuration contains duplicate [$sectionName] sections."
        }
        $preserved = [System.Text.RegularExpressions.Regex]::Replace($preserved, $pattern, '')
    }
    return $preserved.TrimEnd()
}

function Stop-Install {
    param([Parameter(Mandatory)] [string] $Message)

    if ($Json) {
        [ordered]@{ status = 'failed'; error = $Message } | ConvertTo-Json -Compress
    } else {
        Write-Host "[codex-mcp] ERROR: $Message" -ForegroundColor Red
    }
    exit 1
}

if (-not (Test-Path -LiteralPath $sourceFullPath -PathType Leaf)) {
    Stop-Install -Message "Source executable not found: $sourceFullPath"
}

$sourceVersion = (& $sourceFullPath --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceVersion -notmatch '^aku-supervisor\s+\S+$') {
    Stop-Install -Message 'Source executable failed its bounded version preflight.'
}
$sourceHash = (Get-FileHash -LiteralPath $sourceFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
$currentContent = if (Test-Path -LiteralPath $configFullPath -PathType Leaf) {
    [System.IO.File]::ReadAllText($configFullPath)
} else {
    ''
}

try {
    $currentTargetSections = Get-TargetSections -Content $currentContent
    $preservedContent = Remove-TargetSections -Content $currentContent
} catch {
    Stop-Install -Message $_.Exception.Message
}

$hostToml = ConvertTo-TomlString -Value $hostFullPath
$targetSections = @"
[mcp_servers.aku_supervisor]
command = $hostToml
args = ["mcp-proxy"]
enabled = true
required = false
enabled_tools = [
  "supervisor_list_services",
  "supervisor_get_service",
  "supervisor_get_recent_events",
  "supervisor_read_logs",
]
default_tools_approval_mode = "auto"
startup_timeout_sec = 5
tool_timeout_sec = 10

[mcp_servers.aku_supervisor_registration]
command = $hostToml
args = ["registration-mcp"]
enabled = true
required = false
enabled_tools = [
  "supervisor_registration_get_capabilities",
  "supervisor_registration_get_schema",
  "supervisor_registration_validate_service",
  "supervisor_registration_prepare_change",
  "supervisor_registration_get_draft",
  "supervisor_registration_commit_change",
]
default_tools_approval_mode = "auto"
startup_timeout_sec = 5
tool_timeout_sec = 10
"@.Trim()

$newline = if ($currentContent.Contains("`r`n")) { "`r`n" } else { "`n" }
$targetSections = $targetSections.Replace("`r`n", "`n").Replace("`n", $newline)
$proposedContent = if ([string]::IsNullOrWhiteSpace($preservedContent)) {
    $targetSections + $newline
} else {
    $preservedContent + $newline + $newline + $targetSections + $newline
}

$currentHash = Get-TextHash -Text $currentContent
$proposedHash = Get-TextHash -Text $proposedContent
$configurationChanged = $currentHash -ne $proposedHash
$currentHostHash = if (Test-Path -LiteralPath $hostFullPath -PathType Leaf) {
    (Get-FileHash -LiteralPath $hostFullPath -Algorithm SHA256).Hash.ToLowerInvariant()
} else {
    $null
}
$hostChanged = $currentHostHash -ne $sourceHash
$restartCodexRequired = $configurationChanged -or $hostChanged
$approvalBinding = @(
    $configFullPath,
    $currentHash,
    $proposedHash,
    $sourceHash,
    $hostFullPath
) -join "`n"
$approvalHash = Get-TextHash -Text $approvalBinding
$expectedApprovalCode = "APPLY CODEX MCP $($approvalHash.Substring(0, 16))"
$escapedScript = $MyInvocation.MyCommand.Path.Replace('"', '`"')
$escapedConfig = $configFullPath.Replace('"', '`"')
$escapedSource = $sourceFullPath.Replace('"', '`"')
$escapedHost = $hostFullPath.Replace('"', '`"')
$approvalCommand = "& `"$escapedScript`" -CodexConfigPath `"$escapedConfig`" -SourcePath `"$escapedSource`" -HostPath `"$escapedHost`" -Apply -ApprovalCode `"$expectedApprovalCode`""

$plan = [ordered]@{
    status = if ($Apply) { 'ready_to_apply' } else { 'planned' }
    configPath = $configFullPath
    sourcePath = $sourceFullPath
    sourceVersion = $sourceVersion
    sourceHash = "sha256:$sourceHash"
    hostPath = $hostFullPath
    currentConfigHash = "sha256:$currentHash"
    proposedConfigHash = "sha256:$proposedHash"
    configurationChanged = $configurationChanged
    hostChanged = $hostChanged
    unrelatedEntriesPreserved = $true
    restartCodexRequired = $restartCodexRequired
    approvalCode = $expectedApprovalCode
    approvalCommand = $approvalCommand
    currentTargetSections = $currentTargetSections
    proposedTargetSections = $targetSections
}

if (-not $Apply) {
    if ($Json) {
        $plan | ConvertTo-Json -Depth 8 -Compress
    } else {
        Write-Host '[codex-mcp] PLAN ONLY: no files were changed.' -ForegroundColor Cyan
        Write-Host "[codex-mcp] Codex config: $configFullPath"
        Write-Host "[codex-mcp] Source: $sourceFullPath ($sourceVersion)"
        Write-Host "[codex-mcp] Dedicated host: $hostFullPath"
        Write-Host "[codex-mcp] Current config: sha256:$currentHash" -ForegroundColor DarkGray
        Write-Host "[codex-mcp] Proposed config: sha256:$proposedHash" -ForegroundColor DarkGray
        Write-Host "[codex-mcp] Codex restart required after apply: $restartCodexRequired"
        Write-Host '[codex-mcp] Unrelated config entries are preserved; only these two target sections are replaced:' -ForegroundColor Yellow
        Write-Host '--- current target sections ---' -ForegroundColor DarkGray
        Write-Host $currentTargetSections
        Write-Host '--- proposed target sections ---' -ForegroundColor DarkGray
        Write-Host $targetSections
        Write-Host '[codex-mcp] Review the proposal, then run this exact approval command:' -ForegroundColor Yellow
        Write-Host $approvalCommand -ForegroundColor Green
    }
    exit 0
}

if (-not [System.StringComparer]::Ordinal.Equals($ApprovalCode, $expectedApprovalCode)) {
    Stop-Install -Message "Approval code is missing or stale. Run the command without -Apply to review the current proposal."
}

$stageArguments = @{
    SourcePath = $sourceFullPath
    DestinationPath = $hostFullPath
    ExpectedSourceHash = $sourceHash
}
$stageOutput = @(& (Join-Path $PSScriptRoot 'stage-mcp-host.ps1') @stageArguments *>&1)
if ($LASTEXITCODE -ne 0) {
    $stageText = ($stageOutput | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
    Stop-Install -Message "Dedicated MCP host staging failed: $stageText"
}

$configDirectory = Split-Path -Parent $configFullPath
New-Item -ItemType Directory -Path $configDirectory -Force | Out-Null
$temporaryConfig = Join-Path $configDirectory (".{0}.new-{1}-{2}" -f (Split-Path -Leaf $configFullPath), $PID, [Guid]::NewGuid().ToString('N'))
$backupConfig = Join-Path $configDirectory (".{0}.backup-{1}-{2}" -f (Split-Path -Leaf $configFullPath), $PID, [Guid]::NewGuid().ToString('N'))
try {
    $latestContent = if (Test-Path -LiteralPath $configFullPath -PathType Leaf) {
        [System.IO.File]::ReadAllText($configFullPath)
    } else {
        ''
    }
    if ((Get-TextHash -Text $latestContent) -ne $currentHash) {
        throw 'Codex configuration changed after approval validation. Run plan again.'
    }
    $encoding = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($temporaryConfig, $proposedContent, $encoding)
    if (Test-Path -LiteralPath $configFullPath -PathType Leaf) {
        [System.IO.File]::Replace($temporaryConfig, $configFullPath, $backupConfig, $true)
    } else {
        Move-Item -LiteralPath $temporaryConfig -Destination $configFullPath
    }
} catch {
    Stop-Install -Message "Could not atomically update Codex configuration: $($_.Exception.Message)"
} finally {
    if (Test-Path -LiteralPath $temporaryConfig -PathType Leaf) {
        Remove-Item -LiteralPath $temporaryConfig -Force
    }
    if (Test-Path -LiteralPath $backupConfig -PathType Leaf) {
        Remove-Item -LiteralPath $backupConfig -Force
    }
}

$observedContent = [System.IO.File]::ReadAllText($configFullPath)
$observedHash = Get-TextHash -Text $observedContent
if ($observedHash -ne $proposedHash) {
    Stop-Install -Message 'Codex configuration hash does not match the approved proposal.'
}

$result = [ordered]@{
    status = 'applied'
    configPath = $configFullPath
    configHash = "sha256:$observedHash"
    hostPath = $hostFullPath
    hostHash = "sha256:$((Get-FileHash -LiteralPath $hostFullPath -Algorithm SHA256).Hash.ToLowerInvariant())"
    unrelatedEntriesPreserved = $true
    restartCodexRequired = $restartCodexRequired
}
if ($Json) {
    $result | ConvertTo-Json -Compress
} else {
    Write-Host '[codex-mcp] Dedicated MCP host staged and Codex configuration applied.' -ForegroundColor Green
    Write-Host "[codex-mcp] Configuration: $configFullPath"
    Write-Host "[codex-mcp] MCP host: $hostFullPath"
    if ($restartCodexRequired) {
        Write-Host '[codex-mcp] Restart Codex to activate the approved MCP configuration or host.' -ForegroundColor Yellow
    } else {
        Write-Host '[codex-mcp] Configuration and host were already current; a Codex restart is not required.' -ForegroundColor Green
    }
}
