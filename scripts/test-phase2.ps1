[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot

function Invoke-CargoChecked {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments
    )

    Write-Host "`n> cargo $($Arguments -join ' ')" -ForegroundColor Cyan
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo command failed with exit code $LASTEXITCODE"
    }
}

Push-Location $repository
try {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $cargoBin = Join-Path $HOME '.cargo\bin'
        if (Test-Path (Join-Path $cargoBin 'cargo.exe')) {
            $env:PATH = "$cargoBin;$env:PATH"
        }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo is not available. Install Rust or restart Visual Studio after Rust installation.'
    }

    Write-Host 'AkuSupervisor Phase 2 verification' -ForegroundColor Green
    Invoke-CargoChecked @('fmt', '--check')
    Invoke-CargoChecked @('clippy', '--all-targets', '--all-features', '--', '-D', 'warnings')
    Invoke-CargoChecked @('test', '--all-targets', '--all-features')

    Write-Host "`nPASS: formatting, linting, contracts, process ownership, port diagnostics, concurrency, and console cleanup." -ForegroundColor Green
}
finally {
    Pop-Location
}
