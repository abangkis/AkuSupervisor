Set-StrictMode -Version Latest

function Test-AkuRustExecutable {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [string[]] $Arguments
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $false
    }

    try {
        & $Path @Arguments *> $null
        return $LASTEXITCODE -eq 0
    }
    catch {
        return $false
    }
}

function Resolve-AkuRustToolchain {
    param(
        [Parameter(Mandatory)] [string] $Repository
    )

    $localToolchains = Join-Path $Repository 'target\rustup-home\toolchains'
    if (Test-Path -LiteralPath $localToolchains -PathType Container) {
        $local = Get-ChildItem -LiteralPath $localToolchains -Directory |
            Sort-Object Name -Descending |
            Where-Object {
                (Test-Path (Join-Path $_.FullName 'bin\cargo.exe') -PathType Leaf) -and
                (Test-Path (Join-Path $_.FullName 'bin\rustc.exe') -PathType Leaf) -and
                (Test-Path (Join-Path $_.FullName 'bin\rustfmt.exe') -PathType Leaf) -and
                (Test-Path (Join-Path $_.FullName 'lib\rustlib') -PathType Container)
            } |
            Select-Object -First 1

        if ($local) {
            $bin = Join-Path $local.FullName 'bin'
            return [pscustomobject]@{
                Source = 'project-local'
                Bin = $bin
                Cargo = Join-Path $bin 'cargo.exe'
                Rustc = Join-Path $bin 'rustc.exe'
                Rustfmt = Join-Path $bin 'rustfmt.exe'
            }
        }
    }

    $cargoCandidates = @()
    $pathCargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
    if ($pathCargo) {
        $cargoCandidates += $pathCargo.Source
    }
    $cargoCandidates += Join-Path $HOME '.cargo\bin\cargo.exe'

    foreach ($cargo in $cargoCandidates | Select-Object -Unique) {
        $bin = Split-Path -Parent $cargo
        $rustc = Join-Path $bin 'rustc.exe'
        if ((Test-AkuRustExecutable -Path $cargo -Arguments @('--version')) -and
            (Test-AkuRustExecutable -Path $rustc -Arguments @('--version')) -and
            (Test-AkuRustExecutable -Path (Join-Path $bin 'rustfmt.exe') -Arguments @('--version'))) {
            return [pscustomobject]@{
                Source = 'user-global'
                Bin = $bin
                Cargo = $cargo
                Rustc = $rustc
                Rustfmt = Join-Path $bin 'rustfmt.exe'
            }
        }
    }

    throw @'
No complete Rust toolchain is available. AkuSupervisor checked the project-local
target\rustup-home\toolchains directory and the user-level Cargo installation.
Repair rustup or install Rust before running this script.
'@
}
