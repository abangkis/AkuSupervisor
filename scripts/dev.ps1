[CmdletBinding()]
param(
    [string] $Config,
    [ValidateRange(100, 5000)]
    [int] $PollMilliseconds = 300,
    [ValidateRange(100, 10000)]
    [int] $DebounceMilliseconds = 600,
    [ValidateRange(5, 120)]
    [int] $ShutdownTimeoutSeconds = 30
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = Split-Path -Parent $PSScriptRoot
$devDirectory = Join-Path $repository 'target\dev'
$buildDirectory = Join-Path $repository 'target\dev-build'
$devExecutable = Join-Path $devDirectory 'aku-supervisor.exe'
$shutdownRequest = Join-Path $devDirectory 'shutdown-request'
$stagedExecutable = Join-Path $buildDirectory 'debug\aku-supervisor.exe'
$supervisorProcess = $null

function Resolve-ConfigPath {
    if ($Config) {
        if ([IO.Path]::IsPathRooted($Config)) {
            return [IO.Path]::GetFullPath($Config)
        }
        return [IO.Path]::GetFullPath((Join-Path (Get-Location) $Config))
    }
    if ($env:AKU_SUPERVISOR_CONFIG) {
        return [IO.Path]::GetFullPath($env:AKU_SUPERVISOR_CONFIG)
    }
    if (-not $env:LOCALAPPDATA) {
        throw 'LOCALAPPDATA is unavailable; pass -Config explicitly.'
    }
    return Join-Path $env:LOCALAPPDATA 'AkuSupervisor\services.json'
}

function Resolve-Cargo {
    $command = Get-Command cargo -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $localToolchains = Join-Path $repository 'target\rustup-home\toolchains'
    if (Test-Path $localToolchains) {
        $candidate = Get-ChildItem $localToolchains -Directory |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName 'bin\cargo.exe' } |
            Where-Object { Test-Path $_ } |
            Select-Object -First 1
        if ($candidate) {
            return $candidate
        }
    }

    $userCargo = Join-Path $HOME '.cargo\bin\cargo.exe'
    if (Test-Path $userCargo) {
        return $userCargo
    }
    throw 'cargo was not found on PATH, in the project-local toolchain, or under ~/.cargo/bin.'
}

function Test-ControlPort {
    param(
        [Parameter(Mandatory)] [string] $ControlHost,
        [Parameter(Mandatory)] [int] $ControlPort,
        [int] $TimeoutMilliseconds = 250
    )

    $client = [Net.Sockets.TcpClient]::new()
    try {
        $connect = $client.ConnectAsync($ControlHost, $ControlPort)
        return $connect.Wait($TimeoutMilliseconds) -and $client.Connected
    }
    catch {
        return $false
    }
    finally {
        $client.Dispose()
    }
}

function Get-WatchFingerprint {
    $files = @()
    $sourceDirectory = Join-Path $repository 'src'
    if (Test-Path $sourceDirectory) {
        $files += Get-ChildItem $sourceDirectory -Recurse -File -Filter '*.rs'
    }
    foreach ($manifest in @('Cargo.toml', 'Cargo.lock')) {
        $path = Join-Path $repository $manifest
        if (Test-Path $path) {
            $files += Get-Item $path
        }
    }
    return (($files | Sort-Object FullName | ForEach-Object {
        '{0}|{1}|{2}' -f $_.FullName, $_.Length, $_.LastWriteTimeUtc.Ticks
    }) -join "`n")
}

function Invoke-DevelopmentBuild {
    Write-Host "`n[watch] Building staged AkuSupervisor..." -ForegroundColor Cyan
    & $script:cargo build --bin aku-supervisor
    if ($LASTEXITCODE -ne 0) {
        Write-Host '[watch] Build failed. The currently running supervisor remains active.' -ForegroundColor Red
        return $false
    }
    if (-not (Test-Path $stagedExecutable)) {
        throw "Cargo succeeded but did not produce $stagedExecutable"
    }
    return $true
}

function Get-RunningServiceIds {
    try {
        $response = Invoke-RestMethod -Uri "http://${script:controlHost}:$script:controlPort/v1/services" `
            -Method Get -TimeoutSec 2
        return @($response.services |
            Where-Object { $_.lifecycle -eq 'running' } |
            ForEach-Object { [string] $_.id })
    }
    catch {
        Write-Warning "Could not snapshot running services before restart: $($_.Exception.Message)"
        return @()
    }
}

function Request-GracefulShutdown {
    param([Parameter(Mandatory)] [string] $Reason)

    New-Item -ItemType Directory -Force $devDirectory | Out-Null
    $temporaryRequest = "$shutdownRequest.tmp-$PID"
    [IO.File]::WriteAllText($temporaryRequest, $Reason, [Text.UTF8Encoding]::new($false))
    if (Test-Path $shutdownRequest) {
        Remove-Item -LiteralPath $shutdownRequest -Force
    }
    Move-Item -LiteralPath $temporaryRequest -Destination $shutdownRequest
}

function Wait-ForExit {
    param([Parameter(Mandatory)] [Diagnostics.Process] $Process)

    if ($Process.WaitForExit($ShutdownTimeoutSeconds * 1000)) {
        return $true
    }
    Write-Host "[watch] Supervisor did not complete graceful cleanup within $ShutdownTimeoutSeconds seconds." `
        -ForegroundColor Red
    Write-Host '[watch] The watcher will not force-kill it or replace the executable.' -ForegroundColor Red
    return $false
}

function Start-DevelopmentSupervisor {
    if (Test-Path $shutdownRequest) {
        Remove-Item -LiteralPath $shutdownRequest -Force
    }
    $arguments = '--config "{0}"' -f $script:configPath
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $devExecutable
    $startInfo.Arguments = $arguments
    $startInfo.WorkingDirectory = $repository
    $startInfo.UseShellExecute = $false
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "Failed to start $devExecutable"
    }

    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($process.HasExited) {
            throw "Development supervisor exited during startup with code $($process.ExitCode)."
        }
        if (Test-ControlPort -ControlHost $script:controlHost -ControlPort $script:controlPort) {
            Write-Host "[watch] Running stable dev executable: $devExecutable" -ForegroundColor Green
            return $process
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Development supervisor did not open $script:controlHost`:$script:controlPort within 10 seconds."
}

function Restore-RunningServices {
    param([string[]] $ServiceIds)

    foreach ($serviceId in $ServiceIds) {
        Write-Host "[watch] Restoring service: $serviceId" -ForegroundColor Cyan
        & $devExecutable start $serviceId --actor user `
            --reason 'development watcher restored running service' --config $script:configPath
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "Could not restore service '$serviceId'."
        }
    }
}

Push-Location $repository
try {
    $script:configPath = Resolve-ConfigPath
    if (-not (Test-Path $script:configPath -PathType Leaf)) {
        throw "Configuration not found: $script:configPath"
    }
    $configuration = Get-Content $script:configPath -Raw | ConvertFrom-Json
    $script:controlHost = [string] $configuration.control.host
    $script:controlPort = [int] $configuration.control.port
    if (-not $script:controlHost -or -not $script:controlPort) {
        throw "Configuration does not contain control.host and control.port: $script:configPath"
    }
    if (Test-ControlPort -ControlHost $script:controlHost -ControlPort $script:controlPort) {
        throw "Control port $script:controlHost`:$script:controlPort is already active. Type 'quit' in the current AkuSupervisor terminal before starting the watcher."
    }

    $script:cargo = Resolve-Cargo
    $toolchainBin = Split-Path -Parent $script:cargo
    if (Test-Path (Join-Path $toolchainBin 'rustc.exe')) {
        $env:RUSTC = Join-Path $toolchainBin 'rustc.exe'
        $env:RUSTFMT = Join-Path $toolchainBin 'rustfmt.exe'
    }
    $env:CARGO_TARGET_DIR = $buildDirectory
    $env:AKU_SUPERVISOR_DEV_SHUTDOWN_FILE = $shutdownRequest

    New-Item -ItemType Directory -Force $devDirectory | Out-Null
    if (-not (Invoke-DevelopmentBuild)) {
        throw 'Initial development build failed. Fix the errors and start the watcher again.'
    }
    Copy-Item -LiteralPath $stagedExecutable -Destination $devExecutable -Force
    $supervisorProcess = Start-DevelopmentSupervisor

    Write-Host "[watch] Watching Rust sources and Cargo manifests every $PollMilliseconds ms." -ForegroundColor Green
    Write-Host '[watch] A failed rebuild keeps the current supervisor and services running.' -ForegroundColor Green
    Write-Host '[watch] Press Ctrl+C to stop the watcher through graceful cleanup.' -ForegroundColor Green

    $fingerprint = Get-WatchFingerprint
    $pendingSince = $null
    while ($true) {
        Start-Sleep -Milliseconds $PollMilliseconds
        if ($supervisorProcess.HasExited) {
            throw "Development supervisor exited unexpectedly with code $($supervisorProcess.ExitCode)."
        }

        $latestFingerprint = Get-WatchFingerprint
        if ($latestFingerprint -ne $fingerprint) {
            $fingerprint = $latestFingerprint
            $pendingSince = [DateTime]::UtcNow
            continue
        }
        if ($null -eq $pendingSince -or
            ([DateTime]::UtcNow - $pendingSince).TotalMilliseconds -lt $DebounceMilliseconds) {
            continue
        }
        $pendingSince = $null

        if (-not (Invoke-DevelopmentBuild)) {
            continue
        }

        $runningServices = @(Get-RunningServiceIds)
        Request-GracefulShutdown -Reason 'Rust source or Cargo manifest changed'
        if (-not (Wait-ForExit -Process $supervisorProcess)) {
            throw 'Graceful development restart timed out.'
        }
        Copy-Item -LiteralPath $stagedExecutable -Destination $devExecutable -Force
        $supervisorProcess = Start-DevelopmentSupervisor
        Restore-RunningServices -ServiceIds $runningServices
    }
}
finally {
    if ($null -ne $supervisorProcess -and -not $supervisorProcess.HasExited) {
        Write-Host "`n[watch] Stopping development supervisor gracefully..." -ForegroundColor Cyan
        Request-GracefulShutdown -Reason 'development watcher stopped'
        if (-not (Wait-ForExit -Process $supervisorProcess)) {
            Write-Warning 'AkuSupervisor is still running; it was not force-killed.'
        }
    }
    Pop-Location
}
