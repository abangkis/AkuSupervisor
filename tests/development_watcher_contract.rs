#[test]
fn watcher_requires_executable_release_without_force_killing_an_owner() {
    let script = include_str!("../scripts/dev.ps1");
    let toolchain = include_str!("../scripts/rust-toolchain.ps1");

    assert!(script.contains("Wait-ForExecutableRelease -Path $devExecutable"));
    assert!(script.contains("[IO.FileShare]::None"));
    assert!(script.contains("Get-ExecutableOwnerPids"));
    assert!(script.contains("Install-StagedExecutable"));
    assert!(script.contains("Matching process PID(s)"));
    assert!(script.contains("Development executable is in use; waiting up to"));
    assert!(script.contains("Staged executable already matches target\\dev"));
    assert!(script.contains("ValueFromRemainingArguments = $true"));
    assert!(script.contains("HashSet[string]"));
    assert!(script.contains("$orderedServiceIds"));
    assert!(!script.contains("$StartService | Where-Object { $_ } | Sort-Object -Unique"));
    assert!(script.contains("Start-RequestedServices -ServiceIds $script:startServiceIds"));
    assert!(script.contains("AkuSupervisor itself is always started by dev.ps1"));
    assert!(script.contains("Resolve-AkuRustToolchain -Repository $repository"));
    assert!(script.contains("scripts\\rust-toolchain.ps1"));
    assert!(script.contains("target\\tool-temp"));
    assert!(script.contains("$env:TEMP = $toolTempDirectory"));
    assert!(script.contains("$env:TMP = $toolTempDirectory"));
    assert!(toolchain.contains("Source = 'project-local'"));
    assert!(toolchain.contains("Test-AkuRustExecutable"));
    assert!(toolchain.contains("lib\\rustlib"));
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
    assert!(script.contains("Test-ConfigurationBeforeHandoff"));
    assert!(script.contains("Show-McpHostGuidance"));
    assert!(script.contains("get-mcp-host-status.ps1"));
    assert!(script.contains("MCP host status: CURRENT"));
    assert!(script.contains("MCP host status: CORE_ONLY_CHANGE"));
    assert!(script.contains("MCP restaging and Codex restart are not required."));
    assert!(script.contains("Registration tools or schema exposed to agents may be stale."));
    assert!(script.contains("install-codex-mcp.ps1"));
    assert!(script.contains(
        "Configuration validation failed. The current supervisor and services remain active."
    ));
    let auto_start = script
        .find("Start-RequestedServices -ServiceIds $script:startServiceIds")
        .expect("requested services must be started");
    let standard_guidance = script
        .find("Show-ExecutionModeGuidance\n")
        .expect("standard watcher guidance must remain enabled");
    assert!(auto_start < standard_guidance);
    let identical_hash_gate = script
        .find("if ($developmentHash -eq $stagedHash)")
        .expect("identical staged bytes should not require replacing a locked executable");
    let release_wait = script
        .find("if (-not (Wait-ForExecutableRelease -Path $devExecutable))")
        .expect("different staged bytes must wait for executable release");
    assert!(identical_hash_gate < release_wait);
    let configuration_gate = script
        .rfind("if (-not (Test-ConfigurationBeforeHandoff))")
        .expect("configuration must be validated before a live handoff");
    let graceful_handoff = script
        .rfind("Request-GracefulShutdown -Reason 'successful development build'")
        .expect("successful builds should request graceful handoff");
    assert!(configuration_gate < graceful_handoff);
    assert!(!script.contains("Stop-Process"));
    assert!(!script.contains(".Kill("));
}

#[test]
fn akuworkspace_entrypoint_bootstraps_sidecar_before_generic_supervision() {
    let script = include_str!("../scripts/dev-akuworkspace.ps1");

    assert!(script.contains("AkuSidecar\\scripts\\build-dev.ps1"));
    assert!(script.contains("'akusidecar' -in $requestedServices"));
    assert!(script.contains("& $sidecarBuildScript"));
    assert!(script.contains("& $supervisorDevScript @requestedServices @devParameters"));
    assert!(script.contains("Timezone = $Timezone"));

    let sidecar_build = script
        .find("& $sidecarBuildScript")
        .expect("workspace entrypoint must build the Sidecar");
    let supervisor_start = script
        .find("& $supervisorDevScript")
        .expect("workspace entrypoint must delegate to the generic watcher");
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
