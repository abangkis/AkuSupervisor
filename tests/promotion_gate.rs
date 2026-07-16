#[test]
fn stable_promotion_is_core_only_bounded_and_fail_closed() {
    let script = include_str!("../scripts/promote-stable.ps1");
    let preflight = script
        .find("& $devExecutable --version")
        .expect("promotion must execute a bounded candidate preflight");
    let lock = script
        .rfind("Assert-StableExecutableUnlocked")
        .expect("promotion must reject a locked stable executable");
    let copy = script
        .find("Copy-Item -LiteralPath $devExecutable -Destination $stableExecutable")
        .expect("promotion must copy development to stable");
    let hash_check = script
        .find("$promotedHash -ne $developmentHash")
        .expect("promotion must verify the copied bytes");

    assert!(preflight < lock);
    assert!(lock < copy);
    assert!(copy < hash_check);
    assert!(script.contains("Stable is already current; no copy is required."));
    assert!(script.contains("Get-StableExecutableUsers"));
    assert!(script.contains("target\\mcp"));
    assert!(script.contains("stage-mcp-host.ps1"));
    assert!(script.contains("function Stop-Promotion"));
    assert!(script.contains("exit 1"));
    assert!(script.contains("AkuWorkspace integration validation is separate and was not run."));
    assert!(script.contains("validate-akuworkspace-integration.ps1"));
    assert!(!script.contains("bridge validate"));
    assert!(!script.contains("$sidecar"));
}

#[test]
fn dedicated_mcp_host_is_staged_outside_the_core_promotion_target() {
    let script = include_str!("../scripts/stage-mcp-host.ps1");

    assert!(script.contains("target\\mcp"));
    assert!(script.contains("aku-supervisor-mcp.exe"));
    assert!(script.contains("Get-FileHash"));
    assert!(script.contains("Dedicated MCP host is in use and was not changed."));
    assert!(script.contains("Core stable promotion remains independent"));
    assert!(!script.contains("Stop-Process"));
    assert!(!script.contains("target\\dev\\shutdown-request"));
}

#[test]
fn akuworkspace_integration_gate_is_explicit_and_never_promotes() {
    let script = include_str!("../scripts/validate-akuworkspace-integration.ps1");
    let status_preflight = script
        .find("& $devExecutable @statusArguments")
        .expect("integration validation must inspect supervised AkuSidecar");
    let validation = script
        .find("'validate'")
        .expect("integration validation must invoke bridge validate");
    let exit_gate = script
        .find("$validationExitCode -ne 0")
        .expect("integration validation must enforce the command exit code");
    let json_gate = script
        .find("$validation.validation.status -ne 'passed'")
        .expect("integration validation must enforce the JSON result");

    assert!(status_preflight < validation);
    assert!(validation < exit_gate);
    assert!(script.contains("$sidecar.desiredState -ne 'running'"));
    assert!(script.contains("$sidecar.health.status -ne 'healthy'"));
    assert!(script.contains("function Stop-IntegrationValidation"));
    assert!(script.contains("$category -eq 'relay_page_stale'"));
    assert!(script.contains("Reload only the existing http://127.0.0.1:47821 AkuBrowser tab"));
    assert!(script.contains("AkuWorkspace integration validation passed."));
    assert!(!script.contains("Copy-Item"));
    assert!(!script.contains("target\\aku-supervisor.exe"));
    assert!(json_gate > validation);
}
