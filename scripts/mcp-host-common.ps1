# Shared read-only MCP contract and host-selection helpers. No installation side effects.

function Get-SectionPattern {
    param([Parameter(Mandatory)] [string] $SectionName)

    $escaped = [System.Text.RegularExpressions.Regex]::Escape($SectionName)
    return "(?ms)^\[$escaped\][ \t]*(?:\r?\n|$).*?(?=^\[|\z)"
}

function Get-ConfiguredMcpHost {
    param([Parameter(Mandatory)] [AllowEmptyString()] [string] $Content)

    $paths = @()
    foreach ($sectionName in @('mcp_servers.aku_supervisor', 'mcp_servers.aku_supervisor_registration')) {
        $matches = [Text.RegularExpressions.Regex]::Matches(
            $Content,
            (Get-SectionPattern -SectionName $sectionName)
        )
        if ($matches.Count -ne 1) {
            return $null
        }
        $command = [Text.RegularExpressions.Regex]::Match(
            $matches[0].Value,
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

function Read-McpContract {
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
