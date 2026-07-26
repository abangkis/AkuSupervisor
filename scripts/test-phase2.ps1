[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$rustToolchainScript = Join-Path $PSScriptRoot 'rust-toolchain.ps1'
. $rustToolchainScript
$rustToolchain = Resolve-AkuRustToolchain -Repository $repository
$cargo = $rustToolchain.Cargo
$toolTempDirectory = Join-Path $repository 'target\tool-temp'
$previousTemp = $env:TEMP
$previousTmp = $env:TMP
New-Item -ItemType Directory -Force $toolTempDirectory | Out-Null
$env:TEMP = $toolTempDirectory
$env:TMP = $toolTempDirectory
$env:PATH = "$($rustToolchain.Bin);$env:PATH"
$env:RUSTC = $rustToolchain.Rustc
$env:RUSTFMT = $rustToolchain.Rustfmt

function Invoke-CargoChecked {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments
    )

    Write-Host "`n> cargo $($Arguments -join ' ')" -ForegroundColor Cyan
    & $script:cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo command failed with exit code $LASTEXITCODE"
    }
}

Push-Location $repository
try {
    Write-Host 'AkuSupervisor verification through the Gate 4 AkuWorkspace MVP' -ForegroundColor Green
    Write-Host "Rust toolchain: $($rustToolchain.Source) ($cargo)" -ForegroundColor DarkGray
    Invoke-CargoChecked @('fmt', '--check')
    Invoke-CargoChecked @('clippy', '--all-targets', '--all-features', '--', '-D', 'warnings')
    Invoke-CargoChecked @('test', '--all-targets', '--all-features')

    Write-Host "`nPASS: formatting, linting, contracts, ACL, audit events, bounded logs, idempotency, ownership, process supervision, foreground lifecycle, concurrency, and cleanup." -ForegroundColor Green
}
finally {
    $env:TEMP = $previousTemp
    $env:TMP = $previousTmp
    Pop-Location
}
