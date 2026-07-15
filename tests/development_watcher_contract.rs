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
    assert!(script.contains("Start-RequestedServices -ServiceIds $script:startServiceIds"));
    assert!(script.contains("AkuSupervisor itself is always started by dev.ps1"));
    assert!(script.contains("Resolve-AkuRustToolchain -Repository $repository"));
    assert!(script.contains("scripts\\rust-toolchain.ps1"));
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
    assert!(
        script.contains("The watcher and other services remain active; inspect status and logs.")
    );
    assert!(!script.contains("throw \"Could not start requested service '$serviceId'.\""));
    assert!(script.contains("owned services completed graceful shutdown"));
    assert!(script.contains("successful build or configuration change"));
    assert!(script.contains("development watcher stopped by user"));
    assert!(script.contains("Test-ConfigurationBeforeHandoff"));
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
        .rfind("Request-GracefulShutdown -Reason 'successful build or configuration change'")
        .expect("successful changes should request graceful handoff");
    assert!(configuration_gate < graceful_handoff);
    assert!(!script.contains("Stop-Process"));
    assert!(!script.contains(".Kill("));
}
