[CmdletBinding()]
param(
    [string] $Config,
    [ValidateSet('user', 'codex')]
    [string] $Actor = 'user',
    [ValidatePattern('^[A-Za-z0-9_.:-]{1,128}$')]
    [string] $RequestId
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$devExecutable = Join-Path $repository 'target\dev\aku-supervisor.exe'
$stableExecutable = Join-Path $repository 'target\aku-supervisor.exe'

if (-not (Test-Path $devExecutable -PathType Leaf)) {
    throw "Development executable not found: $devExecutable"
}
if (-not $RequestId) {
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $RequestId = "bridge-release-$stamp-$PID"
}

$arguments = @(
    'bridge',
    'validate',
    '--actor', $Actor,
    '--request-id', $RequestId
)
if ($Config) {
    $arguments += @('--config', $Config)
}

Write-Host "[release] Validating AkuBridge with request $RequestId..." -ForegroundColor Cyan
$validationOutput = & $devExecutable @arguments
$validationExitCode = $LASTEXITCODE
$validationJson = ($validationOutput | Out-String).Trim()
try {
    $validation = $validationJson | ConvertFrom-Json -ErrorAction Stop
} catch {
    throw "bridge validate returned invalid JSON and stable was not changed: $validationJson"
}
if ($validationExitCode -ne 0 -or $validation.validation.status -ne 'passed') {
    throw "bridge validate failed with exit code $validationExitCode and stable was not changed: $validationJson"
}

Copy-Item -LiteralPath $devExecutable -Destination $stableExecutable -Force
Write-Host "[release] Promoted validated build to: $stableExecutable" -ForegroundColor Green
Write-Output $validationJson
