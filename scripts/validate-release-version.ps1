[CmdletBinding()]
param(
    [string] $Tag,
    [string] $Executable,
    [switch] $UsePackageVersionTag
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$rustToolchainScript = Join-Path $PSScriptRoot 'rust-toolchain.ps1'
. $rustToolchainScript

function Stop-ReleaseValidation {
    param([Parameter(Mandatory)][string] $Message)

    Write-Host "[release-version] ERROR: $Message" -ForegroundColor Red
    exit 1
}

function Invoke-CheckedText {
    param(
        [Parameter(Mandatory)][string] $FilePath,
        [Parameter(Mandatory)][string[]] $Arguments,
        [Parameter(Mandatory)][string] $Failure
    )

    $output = @(& $FilePath @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        Stop-ReleaseValidation -Message "$Failure $($output -join [Environment]::NewLine)"
    }
    return ($output -join "`n").Trim()
}

Push-Location $repository
try {
    if ($UsePackageVersionTag -and -not [string]::IsNullOrWhiteSpace($Tag)) {
        Stop-ReleaseValidation -Message 'Use either -Tag or -UsePackageVersionTag, not both.'
    }
    $toolchain = Resolve-AkuRustToolchain -Repository $repository
    $metadataText = Invoke-CheckedText `
        -FilePath $toolchain.Cargo `
        -Arguments @('metadata', '--no-deps', '--format-version', '1', '--locked') `
        -Failure 'Cargo metadata could not be read with the committed lock file.'
    $metadata = $metadataText | ConvertFrom-Json
    $package = @($metadata.packages | Where-Object { $_.name -eq 'aku-supervisor' })
    if ($package.Count -ne 1) {
        Stop-ReleaseValidation -Message 'Cargo metadata did not contain exactly one aku-supervisor package.'
    }

    $version = [string]$package[0].version
    $expectedTag = "v$version"
    if ($UsePackageVersionTag) {
        $Tag = $expectedTag
    } elseif ([string]::IsNullOrWhiteSpace($Tag)) {
        $Tag = Invoke-CheckedText `
            -FilePath 'git' `
            -Arguments @('describe', '--tags', '--exact-match', 'HEAD') `
            -Failure "HEAD has no exact release tag; pass -Tag $expectedTag before creating it."
    }
    if (-not [StringComparer]::Ordinal.Equals($Tag, $expectedTag)) {
        Stop-ReleaseValidation -Message "Tag '$Tag' does not match Cargo package version '$version'; expected '$expectedTag'."
    }

    $status = Invoke-CheckedText `
        -FilePath 'git' `
        -Arguments @('status', '--porcelain', '--untracked-files=normal') `
        -Failure 'Git working-tree status could not be inspected.'
    if (-not [string]::IsNullOrWhiteSpace($status)) {
        Stop-ReleaseValidation -Message 'The repository is not clean; commit the version and release changes before validation.'
    }

    $head = Invoke-CheckedText `
        -FilePath 'git' `
        -Arguments @('rev-parse', 'HEAD') `
        -Failure 'Git HEAD could not be resolved.'
    $headShort = Invoke-CheckedText `
        -FilePath 'git' `
        -Arguments @('rev-parse', '--short=7', 'HEAD') `
        -Failure 'The short Git revision could not be resolved.'

    $existingTag = (& git tag --list $Tag | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) {
        Stop-ReleaseValidation -Message 'Existing Git tags could not be inspected.'
    }
    if (-not [string]::IsNullOrWhiteSpace($existingTag)) {
        $tagCommit = Invoke-CheckedText `
            -FilePath 'git' `
            -Arguments @('rev-list', '-n', '1', $Tag) `
            -Failure "Existing tag '$Tag' could not be resolved."
        if (-not [StringComparer]::OrdinalIgnoreCase.Equals($tagCommit, $head)) {
            Stop-ReleaseValidation -Message "Existing tag '$Tag' does not point to HEAD."
        }
    }

    $executablePath = if ([string]::IsNullOrWhiteSpace($Executable)) {
        Join-Path $repository 'target\dev\aku-supervisor.exe'
    } elseif ([IO.Path]::IsPathRooted($Executable)) {
        [IO.Path]::GetFullPath($Executable)
    } else {
        [IO.Path]::GetFullPath((Join-Path (Get-Location) $Executable))
    }
    if (-not (Test-Path -LiteralPath $executablePath -PathType Leaf)) {
        Stop-ReleaseValidation -Message "Release candidate executable was not found: $executablePath"
    }

    $binaryVersion = Invoke-CheckedText `
        -FilePath $executablePath `
        -Arguments @('--version') `
        -Failure 'The release candidate failed its version preflight.'
    $versionPattern = '^aku-supervisor\s+(?<version>\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)\s+\((?<commit>[0-9a-f]{7,40})\)$'
    $versionMatch = [regex]::Match($binaryVersion, $versionPattern)
    if (-not $versionMatch.Success) {
        Stop-ReleaseValidation -Message "Unexpected binary identity: $binaryVersion"
    }
    if (-not [StringComparer]::Ordinal.Equals($versionMatch.Groups['version'].Value, $version)) {
        Stop-ReleaseValidation -Message "Binary version '$($versionMatch.Groups['version'].Value)' does not match Cargo version '$version'."
    }
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals($versionMatch.Groups['commit'].Value, $headShort)) {
        Stop-ReleaseValidation -Message "Binary revision '$($versionMatch.Groups['commit'].Value)' does not match HEAD '$headShort'. Rebuild it from the clean release commit."
    }

    Write-Host "[release-version] PASS: $binaryVersion matches Cargo $version and intended tag $expectedTag." -ForegroundColor Green
    if ([string]::IsNullOrWhiteSpace($existingTag)) {
        Write-Host "[release-version] Tag $expectedTag does not exist yet; validation did not create it." -ForegroundColor Yellow
    } else {
        Write-Host "[release-version] Existing tag $expectedTag points to HEAD $head." -ForegroundColor Green
    }
}
finally {
    Pop-Location
}
