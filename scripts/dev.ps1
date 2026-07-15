[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $StartService = @(),
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
$stableExecutable = Join-Path $repository 'target\aku-supervisor.exe'
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
    # Prefer the project-local, complete toolchain. A rustup shim on PATH may
    # exist while its selected toolchain is missing or only partially installed.
    $localToolchains = Join-Path $repository 'target\rustup-home\toolchains'
    if (Test-Path $localToolchains) {
        $candidate = Get-ChildItem $localToolchains -Directory |
            Sort-Object Name -Descending |
            Where-Object {
                (Test-Path (Join-Path $_.FullName 'bin\cargo.exe')) -and
                (Test-Path (Join-Path $_.FullName 'bin\rustc.exe')) -and
                (Test-Path (Join-Path $_.FullName 'lib\rustlib'))
            } |
            ForEach-Object { Join-Path $_.FullName 'bin\cargo.exe' } |
            Select-Object -First 1
        if ($candidate) {
            return $candidate
        }
    }

    $command = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $userCargo = Join-Path $HOME '.cargo\bin\cargo.exe'
    if (Test-Path $userCargo) {
        return $userCargo
    }
    throw 'cargo was not found on PATH, in the project-local toolchain, or under ~/.cargo/bin.'
}

function Assert-RustToolchain {
    Write-Host "[watch] Cargo: $script:cargo" -ForegroundColor DarkGray
    Write-Host "[watch] Rustc: $env:RUSTC" -ForegroundColor DarkGray

    & $script:cargo --version
    if ($LASTEXITCODE -ne 0) {
        throw "Selected Cargo is not runnable: $script:cargo"
    }
    & $env:RUSTC --version
    if ($LASTEXITCODE -ne 0) {
        throw "Selected Rust compiler is not runnable: $env:RUSTC"
    }
}

function Show-ExecutionModeGuidance {
    Write-Host '[watch] Mode: DEVELOPMENT WATCHER' -ForegroundColor Cyan
    Write-Host "[watch] Active executable: $devExecutable" -ForegroundColor Cyan
    Write-Host "[watch] Normal stable executable: $stableExecutable" -ForegroundColor DarkGray

    $stableIsCurrent = $false
    if (Test-Path $stableExecutable -PathType Leaf) {
        $devHash = (Get-FileHash -LiteralPath $devExecutable -Algorithm SHA256).Hash
        $stableHash = (Get-FileHash -LiteralPath $stableExecutable -Algorithm SHA256).Hash
        $stableIsCurrent = $devHash -eq $stableHash
    }

    if ($stableIsCurrent) {
        Write-Host '[watch] Stable status: CURRENT (identical to this development build).' -ForegroundColor Green
        Write-Host '[watch] To run without the watcher later: press Ctrl+C here, then run .\target\aku-supervisor.exe.' -ForegroundColor Green
        Write-Host '[watch] Promotion is needed only after a newer development build is produced.' -ForegroundColor DarkGray
        return
    }

    $status = if (Test-Path $stableExecutable -PathType Leaf) { 'OUTDATED' } else { 'MISSING' }
    Write-Host "[watch] Stable status: $status (not the active development build)." -ForegroundColor Yellow
    Write-Host '[watch] To run this latest build without the watcher:' -ForegroundColor Yellow
    Write-Host '[watch]   1. AkuSidecar and the AkuBrowser tab with AkuBridge must be live.' -ForegroundColor Yellow
    Write-Host '[watch]      If AkuSidecar is stopped, use a second terminal:' -ForegroundColor Yellow
    Write-Host '[watch]      .\target\dev\aku-supervisor.exe start akusidecar --actor user --reason "prepare stable promotion"' -ForegroundColor Yellow
    Write-Host '[watch]   2. While this watcher and its services remain running:' -ForegroundColor Yellow
    Write-Host '[watch]      .\scripts\promote-stable.ps1' -ForegroundColor Yellow
    Write-Host '[watch]   3. Return here and press Ctrl+C for graceful cleanup.' -ForegroundColor Yellow
    Write-Host '[watch]   4. Start normal mode: .\target\aku-supervisor.exe' -ForegroundColor Yellow
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
    if ($script:configPath -and (Test-Path -LiteralPath $script:configPath -PathType Leaf)) {
        $files += Get-Item -LiteralPath $script:configPath
    }
    $watcherPath = Join-Path $repository 'scripts\dev.ps1'
    if (Test-Path -LiteralPath $watcherPath -PathType Leaf) {
        $files += Get-Item -LiteralPath $watcherPath
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
        # Complete the non-timed wait as well. This drains process bookkeeping
        # before the executable-release check below begins.
        $Process.WaitForExit()
        return $true
    }
    Write-Host "[watch] Supervisor did not complete graceful cleanup within $ShutdownTimeoutSeconds seconds." `
        -ForegroundColor Red
    Write-Host '[watch] The watcher will not force-kill it or replace the executable.' -ForegroundColor Red
    return $false
}

function Get-ExecutableOwnerPids {
    param([Parameter(Mandatory)] [string] $Path)

    $expectedPath = [IO.Path]::GetFullPath($Path)
    $owners = @()
    foreach ($candidate in @(Get-Process -Name 'aku-supervisor' -ErrorAction SilentlyContinue)) {
        try {
            if ($candidate.Path -and
                [StringComparer]::OrdinalIgnoreCase.Equals(
                    [IO.Path]::GetFullPath($candidate.Path),
                    $expectedPath)) {
                $owners += $candidate.Id
            }
        } catch {
            # Path access can be denied for a process owned by another account.
            # The exclusive file-open check remains authoritative.
        }
    }
    return @($owners | Sort-Object -Unique)
}

function Test-ExecutableReleased {
    param([Parameter(Mandatory)] [string] $Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $true
    }

    $stream = $null
    try {
        $stream = [IO.File]::Open(
            $Path,
            [IO.FileMode]::Open,
            [IO.FileAccess]::ReadWrite,
            [IO.FileShare]::None)
        return $true
    } catch [IO.IOException] {
        return $false
    } catch [UnauthorizedAccessException] {
        return $false
    } finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
    }
}

function Format-ExecutableLockDetail {
    param([Parameter(Mandatory)] [string] $Path)

    $ownerPids = @(Get-ExecutableOwnerPids -Path $Path)
    if ($ownerPids.Count -gt 0) {
        return " Matching process PID(s): $($ownerPids -join ', ')."
    }
    return ' No matching process PID was discoverable; a transient scanner or another account may own the file handle.'
}

function Wait-ForExecutableRelease {
    param([Parameter(Mandatory)] [string] $Path)

    if (-not (Test-ExecutableReleased -Path $Path)) {
        $initialDetail = Format-ExecutableLockDetail -Path $Path
        Write-Host "[watch] Development executable is in use; waiting up to $ShutdownTimeoutSeconds seconds: $Path.$initialDetail" -ForegroundColor Yellow
    }
    $deadline = [DateTime]::UtcNow.AddSeconds($ShutdownTimeoutSeconds)
    do {
        if (Test-ExecutableReleased -Path $Path) {
            return $true
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)

    $detail = Format-ExecutableLockDetail -Path $Path
    Write-Host "[watch] Development executable is still in use: $Path.$detail" -ForegroundColor Red
    Write-Host '[watch] The watcher will not force-kill the owner or replace the executable.' -ForegroundColor Red
    return $false
}

function Install-StagedExecutable {
    if ((Test-Path -LiteralPath $devExecutable -PathType Leaf) -and
        (Test-Path -LiteralPath $stagedExecutable -PathType Leaf)) {
        $developmentHash = (Get-FileHash -LiteralPath $devExecutable -Algorithm SHA256).Hash
        $stagedHash = (Get-FileHash -LiteralPath $stagedExecutable -Algorithm SHA256).Hash
        if ($developmentHash -eq $stagedHash) {
            Write-Host '[watch] Staged executable already matches target\dev; replacement is not required.' -ForegroundColor DarkGray
            return
        }
    }

    if (-not (Wait-ForExecutableRelease -Path $devExecutable)) {
        throw 'Development executable did not become replaceable within the bounded shutdown timeout.'
    }

    # The exclusive-open check is immediately followed by the copy. Retry a
    # transient race (for example, a scanner opening the file) within the same
    # bounded timeout, but never kill a process automatically.
    $deadline = [DateTime]::UtcNow.AddSeconds($ShutdownTimeoutSeconds)
    do {
        try {
            Copy-Item -LiteralPath $stagedExecutable -Destination $devExecutable -Force
            return
        } catch [IO.IOException] {
            Start-Sleep -Milliseconds 100
        } catch [UnauthorizedAccessException] {
            Start-Sleep -Milliseconds 100
        }
    } while ([DateTime]::UtcNow -lt $deadline)

    $detail = Format-ExecutableLockDetail -Path $devExecutable
    throw "Could not replace development executable: $devExecutable.$detail"
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
            Write-Host "[watch] Development Supervisor is ready: $devExecutable" -ForegroundColor Green
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

function Start-RequestedServices {
    param([string[]] $ServiceIds)

    foreach ($serviceId in $ServiceIds) {
        Write-Host "[watch] Starting requested service: $serviceId" -ForegroundColor Cyan
        & $devExecutable start $serviceId --actor user `
            --reason 'development watcher requested startup service' --config $script:configPath
        if ($LASTEXITCODE -ne 0) {
            throw "Could not start requested service '$serviceId'."
        }
        Write-Host "[watch] Auto-started service: $serviceId" -ForegroundColor Green
        Write-Host '[watch] This service is owned by the development Supervisor and is included in graceful shutdown.' -ForegroundColor Green
    }
}

Push-Location $repository
try {
    $script:configPath = Resolve-ConfigPath
    if (-not (Test-Path $script:configPath -PathType Leaf)) {
        throw "Configuration not found: $script:configPath"
    }
    $configuration = Get-Content $script:configPath -Raw | ConvertFrom-Json
    $configuredServiceIds = @($configuration.services.PSObject.Properties.Name)
    $script:startServiceIds = @($StartService | Where-Object { $_ } | Sort-Object -Unique)
    foreach ($serviceId in $script:startServiceIds) {
        if ($serviceId -notin $configuredServiceIds) {
            $available = if ($configuredServiceIds.Count -gt 0) {
                $configuredServiceIds -join ', '
            } else {
                '<none>'
            }
            throw "Unknown startup service '$serviceId'. Configured service IDs: $available. AkuSupervisor itself is always started by dev.ps1."
        }
    }
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
    } else {
        $rustcCommand = Get-Command rustc -CommandType Application -ErrorAction SilentlyContinue
        if (-not $rustcCommand) {
            throw "rustc was not found beside the selected Cargo or on PATH: $script:cargo"
        }
        $env:RUSTC = $rustcCommand.Source
    }
    Assert-RustToolchain
    $env:CARGO_TARGET_DIR = $buildDirectory
    $env:AKU_SUPERVISOR_DEV_SHUTDOWN_FILE = $shutdownRequest

    New-Item -ItemType Directory -Force $devDirectory | Out-Null
    if (-not (Invoke-DevelopmentBuild)) {
        throw 'Initial development build failed. Fix the errors and start the watcher again.'
    }
    Install-StagedExecutable
    $supervisorProcess = Start-DevelopmentSupervisor
    Start-RequestedServices -ServiceIds $script:startServiceIds
    Show-ExecutionModeGuidance

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
        Request-GracefulShutdown -Reason 'successful build or configuration change'
        if (-not (Wait-ForExit -Process $supervisorProcess)) {
            throw 'Graceful development restart timed out.'
        }
        Install-StagedExecutable
        $supervisorProcess = Start-DevelopmentSupervisor
        Restore-RunningServices -ServiceIds $runningServices
        Show-ExecutionModeGuidance
    }
}
finally {
    if ($null -ne $supervisorProcess -and -not $supervisorProcess.HasExited) {
        Write-Host "`n[watch] Stopping development supervisor gracefully..." -ForegroundColor Cyan
        Request-GracefulShutdown -Reason 'development watcher stopped by user'
        if (-not (Wait-ForExit -Process $supervisorProcess)) {
            Write-Warning 'AkuSupervisor is still running; it was not force-killed.'
        } else {
            Write-Host '[watch] Development Supervisor and its owned services completed graceful shutdown.' -ForegroundColor Green
        }
    }
    Pop-Location
}
