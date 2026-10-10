#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

#[test]
fn watcher_runs_local_validation_before_start_or_graceful_handoff() {
    let fixture = TestDirectory::create();
    let checker = fixture.path().join("startup-checker.cmd");
    let harness = fixture.path().join("watcher-validation-harness.ps1");
    fs::write(&checker, fake_checker()).expect("write fixture checker");
    fs::write(
        &harness,
        include_str!("fixtures/watcher-startup-validation-harness.ps1"),
    )
    .expect("write watcher harness");

    let tool_temp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("tool-temp");
    fs::create_dir_all(&tool_temp).expect("create project-local temporary directory");
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&harness)
        .env("TEMP", &tool_temp)
        .env("TMP", &tool_temp)
        .env(
            "STARTUP_WATCHER_EVENTS_FILE",
            fixture.path().join("events.txt"),
        )
        .env("STARTUP_WATCHER_ARGS_FILE", fixture.path().join("args.txt"))
        .env("STARTUP_WATCHER_EXECUTABLE", &checker)
        .env("STARTUP_WATCHER_FIXTURE_ROOT", fixture.path())
        .env(
            "STARTUP_WATCHER_SOURCE",
            fixture.path().join("repository-dev.ps1"),
        )
        .output()
        .expect("run isolated PowerShell harness");

    assert!(
        output.status.success(),
        "PowerShell watcher harness failed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fake_checker() -> &'static str {
    r#"@echo off
>>"%STARTUP_WATCHER_EVENTS_FILE%" echo checker
>>"%STARTUP_WATCHER_ARGS_FILE%" echo %*
if /I "%STARTUP_WATCHER_MODE%"=="failure" (
  echo {"valid":false}
  exit /b 7
)
if /I "%STARTUP_WATCHER_MODE%"=="invalid" (
  echo {"valid":false}
  exit /b 0
)
echo {"valid":true}
exit /b 0
"#
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("startup-watcher-harness")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::SeqCst)
            ));
        fs::create_dir_all(&path).expect("create watcher harness fixture");
        fs::write(
            path.join("repository-dev.ps1"),
            include_str!("../scripts/dev.ps1"),
        )
        .expect("copy watcher source fixture");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
