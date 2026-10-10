#[test]
fn watcher_requires_executable_release_without_force_killing_an_owner() {
    let script = include_str!("../scripts/dev.ps1").replace("\r\n", "\n");

    assert!(script.contains("Wait-ForExecutableRelease -Path $devExecutable"));
    assert!(script.contains("[IO.FileShare]::None"));
    assert!(script.contains("Get-ExecutableOwnerPids"));
    assert!(script.contains("Install-StagedExecutable"));
    assert!(script.contains("Matching process PID(s)"));
    assert!(script.contains("Development executable is in use; waiting up to"));
    assert!(script.contains("Staged executable already matches target\\dev"));
    let identical_hash_gate = script
        .find("if ($developmentHash -eq $stagedHash)")
        .expect("identical staged bytes should not require replacing a locked executable");
    let release_wait = script
        .find("if (-not (Wait-ForExecutableRelease -Path $devExecutable))")
        .expect("different staged bytes must wait for executable release");
    assert!(identical_hash_gate < release_wait);
    assert!(!script.contains("Stop-Process"));
    assert!(!script.contains(".Kill("));
}

#[test]
fn watcher_keeps_requested_service_startup_and_reporting_explicit() {
    let script = include_str!("../scripts/dev.ps1");

    assert!(script.contains("ValueFromRemainingArguments = $true"));
    assert!(script.contains("HashSet[string]"));
    assert!(script.contains("$orderedServiceIds"));
    assert!(!script.contains("$StartService | Where-Object { $_ } | Sort-Object -Unique"));
    assert!(script.contains("Start-RequestedServices -ServiceIds $script:startServiceIds"));
    assert!(script.contains("Auto-started service: $serviceId"));
    assert!(script.contains("included in graceful shutdown"));
    assert!(script.contains("Show-RequestedServiceStartupSummary -Results $results"));
    assert!(script.contains("Requested service startup summary:"));
    assert!(script.contains("'SERVICE', 'REQUEST', 'STATE', 'HEALTH'"));
    assert!(script.contains("Failed service(s): $($failedServiceIds -join ', ')"));
    assert!(script.contains("retry a failed service from a second terminal"));
    assert!(script.contains("function Invoke-ServiceStart"));
    assert!(script.contains("Native stderr is captured per request"));
    assert!(script.contains("2>&1"));
    assert!(
        script.contains("The watcher and other services remain active; inspect status and logs.")
    );
    assert!(!script.contains("throw \"Could not start requested service '$serviceId'.\""));
    assert!(script.contains("AkuSupervisor itself is always started by dev.ps1"));
}

#[test]
fn watcher_uses_project_toolchain_and_scoped_temporary_directory() {
    let script = include_str!("../scripts/dev.ps1");
    let toolchain = include_str!("../scripts/rust-toolchain.ps1");

    assert!(script.contains("Resolve-AkuRustToolchain -Repository $repository"));
    assert!(script.contains("scripts\\rust-toolchain.ps1"));
    assert!(script.contains("'Cargo.toml', 'Cargo.lock', 'build.rs'"));
    assert!(script.contains("$null -eq $script:gitIdentityPaths"));
    assert!(script.contains("@('HEAD', 'logs/HEAD')"));
    assert!(script.contains("symbolic-ref -q HEAD"));
    assert!(script.contains("target\\tool-temp"));
    assert!(script.contains("$env:TEMP = $toolTempDirectory"));
    assert!(script.contains("$env:TMP = $toolTempDirectory"));
    assert!(toolchain.contains("Source = 'project-local'"));
    assert!(toolchain.contains("Test-AkuRustExecutable"));
    assert!(toolchain.contains("lib\\rustlib"));
}

#[test]
fn watcher_gates_initial_start_and_handoff_with_local_startup_validation() {
    let script = include_str!("../scripts/dev.ps1").replace("\r\n", "\n");

    assert!(script.contains("validate-startup"));
    assert!(script.contains("function Test-StartupConfiguration"));
    assert!(script.contains("function Start-InitialDevelopmentSupervisor"));
    assert!(script.contains("function Invoke-ValidatedHandoff"));
    assert!(script.contains("Start-InitialDevelopmentSupervisor"));
    assert!(script.contains("Invoke-ValidatedHandoff -Reason 'successful development build'"));
    assert!(script.contains(
        "Startup configuration validation failed. The candidate Supervisor will not be started and current services remain active."
    ));
    assert!(script.contains("owned services completed graceful shutdown"));
    assert!(script.contains("successful development build"));
    assert!(!script.contains("Get-Item -LiteralPath $script:configPath"));
    assert!(script.contains("development watcher stopped by user"));
    assert!(script.contains("Enter-DevelopmentWatcherLease"));
    assert!(script.contains("development-watcher.lock"));
    assert!(script.contains("AKU_SUPERVISOR_WATCHER_ID"));
    assert!(script.contains("Watched development Supervisor PID"));
    assert!(script.contains("but another process now owns"));
    assert!(script.contains("Active instance: PID"));
    assert!(script.contains("Use 'supervisor shutdown' from a second terminal"));
    assert!(script.contains("[ValidateSet('local', 'utc')]"));
    assert!(script.contains("--timezone {0} --config"));
    assert!(script.contains("Show-McpHostGuidance"));
    assert!(script.contains("get-mcp-host-status.ps1"));
    assert!(script.contains("MCP host status: CURRENT"));
    assert!(script.contains("MCP host status: CORE_ONLY_CHANGE"));
    assert!(script.contains("MCP restaging and Codex restart are not required."));
    assert!(script.contains("Registration tools or schema exposed to agents may be stale."));
    assert!(script.contains("install-codex-mcp.ps1"));
    assert!(script.contains("[switch] $ReleaseCheck"));
    assert!(script.contains("[switch] $PartialStart"));
    assert!(script.contains("Get-PartialStartupServiceIds"));
    assert!(script.contains("New-PartialStartupConfiguration"));
    assert!(script.contains("Remove-StalePartialStartupConfigurations"));
    assert!(script.contains("Full configuration startup failed fast"));
    assert!(script.contains("-PartialStart requires at least one startup service ID."));
    assert!(script.contains("Source configuration remains unchanged"));
    assert!(script.contains("Removed ephemeral partial startup configuration"));
    assert!(script.contains("partial-start.ps1"));
    assert!(script.contains("[string] $ReleaseTag"));
    assert!(script.contains("-UsePackageVersionTag -Executable $devExecutable"));
    assert!(script.contains("watcher and services will not be started"));
    let release_check = script
        .find("if ($ReleaseCheck)")
        .expect("release check must have a dedicated facade mode");
    let watcher_entry = script
        .find("Push-Location $repository")
        .expect("ordinary watcher flow must remain explicit");
    assert!(release_check < watcher_entry);
    let auto_start = script
        .find("Start-RequestedServices -ServiceIds $script:startServiceIds")
        .expect("requested services must be started");
    let standard_guidance = script
        .find("Show-ExecutionModeGuidance\n")
        .expect("standard watcher guidance must remain enabled");
    assert!(auto_start < standard_guidance);
}

#[test]
fn canonical_watcher_bootstraps_only_stale_workspace_runtime() {
    let watcher = include_str!("../scripts/dev.ps1");
    let workspace = include_str!("../scripts/dev-akuworkspace.ps1");

    assert!(watcher.contains("[switch] $Rebuild"));
    assert!(workspace.contains("[switch] $PartialStart"));
    assert!(workspace.contains("PartialStart = $PartialStart"));
    assert!(watcher.contains("Invoke-WorkspaceServiceBootstrap"));
    assert!(watcher.contains("& $akuWorkspaceBootstrapScript @ServiceIds -BootstrapOnly"));
    assert!(watcher.contains("-Config $script:configPath"));
    let active_port_gate = watcher
        .find("if (Test-ControlPort -ControlHost $script:controlHost")
        .expect("active Supervisor ownership must be checked first");
    let workspace_bootstrap = watcher
        .rfind("Invoke-WorkspaceServiceBootstrap -ServiceIds $bootstrapServiceIds")
        .expect("workspace bootstrap must be invoked by the canonical watcher");
    assert!(active_port_gate < workspace_bootstrap);
    assert!(workspace.contains("AkuSidecar\\scripts\\build-dev.ps1"));
    assert!(workspace.contains("[switch] $BootstrapOnly"));
    assert!(workspace.contains("[switch] $BrowserProjectionOnly"));
    assert!(workspace.contains("-BrowserProjectionOnly requires akusidecar"));
    assert!(workspace.contains("if ('akusidecar' -in $requestedServices)"));
    assert!(workspace.contains("Get-AkuSidecarRuntimeStatus"));
    assert!(workspace.contains("$provenance.version"));
    assert!(workspace.contains("$provenance.sourceCommit"));
    assert!(workspace.contains("$provenance.sourceDirty"));
    assert!(workspace.contains("$provenance.binarySha256"));
    assert!(workspace.contains("if (-not $Rebuild -and $runtimeStatus.Current)"));
    assert!(workspace.contains("& $sidecarBuildScript"));
    assert!(workspace.contains("& $supervisorDevScript @requestedServices @devParameters"));
    assert!(workspace.contains("Timezone = $Timezone"));
    assert!(workspace.contains("AkuBrowser\\config\\bridge-identities.json"));
    assert!(workspace.contains("Set-AkuSidecarDevelopmentBrowserProjection"));
    assert!(workspace.contains("if (-not $BrowserProjectionOnly)"));
    assert!(workspace.contains("Resolve-GoogleChromeStable"));
    assert!(workspace.contains("Get-BridgeIdentityProfile -ProfileName 'development'"));
    assert!(workspace.contains("--bridge-extension-origin"));
    assert!(workspace.contains("--chromium-path"));
    assert!(workspace.contains("--browser-profile"));
    assert!(workspace.contains("AkuSidecar\\runtime\\dev-chrome-profile"));
    assert!(workspace.contains("Google\\Chrome\\Application\\chrome.exe"));
    assert!(
        workspace.contains("dedicated workspace profile; personal Chrome profiles are not used")
    );
    assert!(workspace.contains("requires one-time Developer mode > Load unpacked"));
    assert!(workspace.contains("Channel      : DEVELOPMENT"));
    assert!(workspace.contains("Authority    :"));
    assert!(workspace.contains("Projection   :"));

    let status_gate = workspace
        .find("if (-not $Rebuild -and $runtimeStatus.Current)")
        .expect("workspace bootstrap must retain a current generated runtime");
    let sidecar_build = workspace
        .find("& $sidecarBuildScript")
        .expect("workspace adapter must be able to build the Sidecar");
    let supervisor_start = workspace
        .find("& $supervisorDevScript")
        .expect("workspace entrypoint must delegate to the generic watcher");
    assert!(status_gate < sidecar_build);
    assert!(sidecar_build < supervisor_start);
}

#[test]
fn verification_and_watcher_keep_tool_temporary_files_inside_the_repository() {
    let watcher = include_str!("../scripts/dev.ps1");
    let verification = include_str!("../scripts/test-phase2.ps1");

    for script in [watcher, verification] {
        assert!(script.contains("target\\tool-temp"));
        assert!(script.contains("$env:TEMP = $toolTempDirectory"));
        assert!(script.contains("$env:TMP = $toolTempDirectory"));
        assert!(script.contains("$env:TEMP = $previousTemp"));
        assert!(script.contains("$env:TMP = $previousTmp"));
    }
}

#[test]
fn akubrowser_development_profile_setup_is_isolated_and_manual() {
    let setup = include_str!("../scripts/setup-akubrowser-dev-profile.ps1");

    assert!(setup.contains("AkuSidecar\\runtime\\dev-chrome-profile"));
    assert!(setup.contains("Google\\Chrome\\Application\\chrome.exe"));
    assert!(setup.contains("chrome://extensions"));
    assert!(setup.contains("Enable Developer mode"));
    assert!(setup.contains("Choose Load unpacked"));
    assert!(setup.contains("AkuBridge"));
    assert!(setup.contains("Do not select or sign into a personal Chrome profile"));
    assert!(!setup.contains("--load-extension"));
}
