param()
$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$root = Join-Path $repository ('target\mcp-sync-test-' + [Guid]::NewGuid().ToString('N'))
$fixture = Join-Path $root 'AkuSupervisor'
$source = Join-Path $fixture 'target\dev\aku-supervisor.exe'
$config = Join-Path $root '.codex\config.toml'
$dev = Join-Path $fixture 'scripts\dev.ps1'
$installer = Join-Path $fixture 'scripts\install-codex-mcp.ps1'
$oldHost = Join-Path $fixture 'target\mcp\old-host.ps1'
function Assert-True([bool] $Condition, [string] $Message) {
    if (-not $Condition) { throw $Message }
}
foreach ($directory in @((Split-Path -Parent $dev), (Split-Path -Parent $source), (Split-Path -Parent $config), (Split-Path -Parent $oldHost))) {
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
}
foreach ($name in @('dev.ps1', 'rust-toolchain.ps1', 'partial-start.ps1', 'install-codex-mcp.ps1', 'stage-mcp-host.ps1', 'mcp-host-common.ps1', 'get-mcp-host-status.ps1')) {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination (Join-Path $fixture "scripts\$name")
}
Copy-Item -LiteralPath (Join-Path $repository 'target\dev\aku-supervisor.exe') -Destination $source
$report = (& $source mcp-contract --json | Out-String) | ConvertFrom-Json
Assert-True ($LASTEXITCODE -eq 0) 'Fixture source contract report failed.'
$oldReport = @{ schemaVersion = 1; fingerprint = $report.fingerprint } | ConvertTo-Json -Compress
[IO.File]::WriteAllText($oldHost, "param([Parameter(ValueFromRemainingArguments=`$true)][string[]]`$Rest)`n'$oldReport'`n")
$initial = (& $installer -SourcePath $source -HostPath $oldHost -Json | Out-String) | ConvertFrom-Json
Assert-True ($LASTEXITCODE -eq 0) 'Initial fixture plan failed.'
[IO.File]::WriteAllText($config, "# Preserve unrelated configuration`nmodel = `"fixture-model`"`n`n$($initial.proposedTargetSections)`n")
$savedConfig = $env:AKU_SUPERVISOR_CONFIG
try {
    # A deliberately absent runtime configuration proves maintenance never
    # enters configuration discovery, Rust builds, watcher leases, or services.
    $env:AKU_SUPERVISOR_CONFIG = Join-Path $root 'absent-services.json'
    $configHash = (Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash
    $oldHash = (Get-FileHash -LiteralPath $oldHost -Algorithm SHA256).Hash
    $ordinary = (& $installer -SourcePath $source -Json | Out-String) | ConvertFrom-Json
    Assert-True ($ordinary.hostSelection -eq 'configured-contract-match' -and $ordinary.hostStatus -eq 'CORE_ONLY_CHANGE') 'Fixture must reproduce the compatible but different binary case.'
    $hash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
    $expectedHost = Join-Path $fixture "target\mcp\sha256-$hash\aku-supervisor-mcp.exe"
    $sync = (& $installer -SourcePath $source -HostPath $expectedHost -Json | Out-String) | ConvertFrom-Json
    $review = (& $dev -SyncBinary 6>&1 | Out-String -Width 32767)
    Assert-True ($review.Contains($sync.approvalCommand)) 'Maintenance did not show the exact approval command for the content-addressed host.'
    $normalizedReview = $review.Replace("`r`n", "`n")
    Assert-True ($normalizedReview.Contains($initial.proposedTargetSections.Replace("`r`n", "`n")) -and $normalizedReview.Contains($sync.proposedTargetSections.Replace("`r`n", "`n"))) 'Review omitted current or proposed MCP sections.'
    Assert-True (-not (Test-Path -LiteralPath $expectedHost)) 'Review staged a binary.'
    Assert-True ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -eq $configHash) 'Review changed Codex configuration.'
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $fixture 'target\dev-build'))) 'Review started a Rust build.'

    foreach ($parameters in @(@{ StartService = @('akusidecar') }, @{ Config = 'unused.json' }, @{ Rebuild = $true }, @{ PartialStart = $true }, @{ ReleaseCheck = $true }, @{ ReleaseTag = 'v0.0.0' })) {
        $rejected = $false
        try { $null = & $dev -SyncBinary @parameters 6>$null } catch { $rejected = $true }
        Assert-True $rejected 'Maintenance accepted incompatible startup or release parameters.'
    }

    # Exercise the real installation flow only against the isolated workspace.
    $applied = (& $installer -SourcePath $source -HostPath $expectedHost -Apply -ApprovalCode $sync.approvalCode -Json | Out-String) | ConvertFrom-Json
    Assert-True ($LASTEXITCODE -eq 0) 'Fixture approval apply failed.'
    $currentHash = (Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash
    $current = (& $dev -SyncBinary 6>&1 | Out-String -Width 32767)
    Assert-True ($current.Contains('MCP host status: CURRENT') -and -not $current.Contains('APPLY CODEX MCP')) 'Identical host did not become idempotent CURRENT.'
    $status = (& (Join-Path $fixture 'scripts\get-mcp-host-status.ps1') -SourcePath $source -Json | Out-String) | ConvertFrom-Json
    Assert-True ($status.status -eq 'CURRENT' -and -not $status.restartCodexRequired) 'Configured host does not match the development binary.'
    Assert-True ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -eq $currentHash) 'Idempotent maintenance changed configuration.'
    Assert-True ((Get-FileHash -LiteralPath $oldHost -Algorithm SHA256).Hash -eq $oldHash) 'Old host was overwritten.'
    Assert-True ([IO.File]::ReadAllText($config).Contains('model = "fixture-model"')) 'Unrelated configuration was lost.'

    [IO.File]::AppendAllText($expectedHost, 'tamper')
    $rejected = $false
    try { $null = & $dev -SyncBinary 6>$null } catch { $rejected = $_.Exception.Message -like '*unexpected bytes*' }
    Assert-True $rejected 'Tampered content-addressed host was accepted.'
    Remove-Item -LiteralPath $source
    $rejected = $false
    try { $null = & $dev -SyncBinary 6>$null } catch { $rejected = $_.Exception.Message -like '*Development executable is missing*' }
    Assert-True $rejected 'Missing development binary did not fail before watcher startup.'
    Write-Output "PASS: dev.ps1 MCP sync review, fixture apply, CURRENT status, idempotence, mode guards, and tamper/missing-source rejection. Fixtures: $root"
} finally {
    $env:AKU_SUPERVISOR_CONFIG = $savedConfig
}
