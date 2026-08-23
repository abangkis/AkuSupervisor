[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$supervisorRepository = Split-Path -Parent $PSScriptRoot
$workspaceRoot = Split-Path -Parent $supervisorRepository
$profilePath = Join-Path $workspaceRoot 'AkuSidecar\runtime\dev-chrome-profile'
$bridgeExtensionPath = Join-Path $workspaceRoot 'AkuBridge'

function Resolve-GoogleChromeStablePath {
    $candidates = [Collections.Generic.List[string]]::new()
    if ($env:PROGRAMFILES) {
        $candidates.Add((Join-Path $env:PROGRAMFILES 'Google\Chrome\Application\chrome.exe'))
    }
    if (${env:PROGRAMFILES(X86)}) {
        $candidates.Add((Join-Path ${env:PROGRAMFILES(X86)} 'Google\Chrome\Application\chrome.exe'))
    }
    if ($env:LOCALAPPDATA) {
        $candidates.Add((Join-Path $env:LOCALAPPDATA 'Google\Chrome\Application\chrome.exe'))
    }
    foreach ($candidate in $candidates) {
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            continue
        }
        $item = Get-Item -LiteralPath $candidate
        if ([string] $item.VersionInfo.ProductName -eq 'Google Chrome') {
            return $item.FullName
        }
    }
    throw 'Google Chrome Stable was not found in its supported per-machine or per-user installation locations.'
}

if (-not (Test-Path -LiteralPath (Join-Path $bridgeExtensionPath 'manifest.json') -PathType Leaf)) {
    throw "AkuBridge unpacked extension was not found: $bridgeExtensionPath"
}

$chromePath = Resolve-GoogleChromeStablePath
New-Item -ItemType Directory -Force -Path $profilePath | Out-Null

Write-Host '[AkuBrowser] Opening Chrome extension management in the dedicated development profile.' -ForegroundColor Cyan
Write-Host "[AkuBrowser] Profile: $profilePath" -ForegroundColor DarkGray
Write-Host '[AkuBrowser] In the window that opens:' -ForegroundColor Yellow
Write-Host '[AkuBrowser]   1. Enable Developer mode.' -ForegroundColor Yellow
Write-Host '[AkuBrowser]   2. Choose Load unpacked.' -ForegroundColor Yellow
Write-Host "[AkuBrowser]   3. Select: $bridgeExtensionPath" -ForegroundColor Yellow
Write-Host '[AkuBrowser] Do not select or sign into a personal Chrome profile.' -ForegroundColor Green

$arguments = @(
    "--user-data-dir=`"$profilePath`"",
    '--no-first-run',
    '--no-default-browser-check',
    'chrome://extensions'
)
Start-Process -FilePath $chromePath -ArgumentList $arguments
