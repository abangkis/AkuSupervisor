#[test]
fn stable_promotion_is_guarded_by_machine_readable_bridge_validation() {
    let script = include_str!("../scripts/promote-stable.ps1");
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

    assert!(validation < exit_gate);
    assert!(exit_gate < promotion);
    assert!(json_gate < promotion);
    assert!(script.contains("stable was not changed"));
}
