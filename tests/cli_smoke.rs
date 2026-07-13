use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aku-supervisor"))
}

#[test]
fn help_is_visible_and_bounded() {
    let output = binary()
        .arg("--help")
        .output()
        .expect("AkuSupervisor binary should run");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help output should be UTF-8");
    assert!(stdout.contains("AkuSupervisor"));
    assert!(stdout.contains("run --config <path>"));
    assert!(output.stderr.is_empty());
}

#[test]
fn version_matches_cargo_package() {
    let output = binary()
        .arg("--version")
        .output()
        .expect("AkuSupervisor binary should run");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("version output should be UTF-8"),
        format!("aku-supervisor {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn unsupported_commands_fail_closed() {
    let output = binary()
        .arg("start")
        .output()
        .expect("AkuSupervisor binary should run");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("error output should be UTF-8");
    assert!(stderr.contains("unsupported argument"));
}
