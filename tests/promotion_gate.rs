#[test]
fn stable_promotion_is_guarded_by_machine_readable_bridge_validation() {
    let script = include_str!("../scripts/promote-stable.ps1");
    let status_preflight = script
        .find("& $devExecutable @statusArguments")
        .expect("promotion script must inspect the supervised Sidecar first");
    let lock_preflight = script
        .find("Assert-StableExecutableUnlocked -Stage 'before release validation'")
        .expect("promotion script must reject a locked stable executable early");
    let validation = script
        .find("'validate'")
        .expect("promotion script must invoke bridge validate");
    let exit_gate = script
        .find("$validationExitCode -ne 0")
        .expect("promotion script must enforce the validation exit code");
    let json_gate = script
        .find("$validation.validation.status -ne 'passed'")
        .expect("promotion script must enforce the JSON status");
    let promotion = script
        .find("Copy-Item -LiteralPath $devExecutable -Destination $stableExecutable")
        .expect("promotion script must copy the development binary to stable");
    let lock_recheck = script
        .find("Assert-StableExecutableUnlocked -Stage 'after release validation'")
        .expect("promotion script must recheck the stable lock after validation");

    assert!(lock_preflight < status_preflight);
    assert!(status_preflight < validation);
    assert!(validation < exit_gate);
    assert!(exit_gate < promotion);
    assert!(json_gate < promotion);
    assert!(lock_recheck < promotion);
    assert!(script.contains("$sidecar.desiredState -ne 'running'"));
    assert!(script.contains("$sidecar.health.status -ne 'healthy'"));
    assert!(script.contains("AkuSidecar is stopped. Start it from a second terminal"));
    assert!(script.contains("function Stop-Promotion"));
    assert!(script.contains("exit 1"));
    assert!(script.contains("stable was not changed"));
    assert!(script.contains("Get-StableExecutableUsers"));
    assert!(script.contains("target\\aku-supervisor.exe"));
    assert!(script.contains("target\\dev"));
    assert!(script.contains("$category -eq 'relay_page_stale'"));
    assert!(script.contains("Reload only the existing http://127.0.0.1:47821 AkuBrowser tab"));
    assert!(script.contains("without stopping the watcher"));
}
