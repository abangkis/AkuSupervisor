#![cfg(all(windows, feature = "test-fixtures"))]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

const EXIT_TIMEOUT: Duration = Duration::from_secs(10);

fn supervisor() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor")
}

fn process_fixture() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor-process-fixture")
}

#[test]
fn foreground_cli_runs_registered_lifecycle_and_cleans_up() {
    let directory = TestDirectory::create();
    let config_path = directory.path.join("services.json");
    let config = json!({
        "version": 1,
        "control": {
            "host": "127.0.0.1",
            "port": 47820,
            "tokenFile": ".runtime/control-token"
        },
        "services": {
            "fixture": {
                "label": "Process Fixture",
                "cwd": directory.path.clone(),
                "command": process_fixture(),
                "args": ["--root"],
                "environment": {},
                "health": { "type": "process" },
                "ports": [],
                "restartPolicy": "manual",
                "shutdownGraceMs": 250
            }
        }
    });
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("serialize fixture config"),
    )
    .expect("write fixture config");

    let child = Command::new(supervisor())
        .arg("run")
        .arg("--config")
        .arg(&config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start foreground supervisor");
    let mut guard = ChildGuard::new(child);
    let mut stdin = guard.child_mut().stdin.take().expect("supervisor stdin");
    stdin
        .write_all(
            b"start fixture integration start\nstatus\nrestart fixture integration restart\nstatus\nstop fixture integration stop\nstatus\nquit\n",
        )
        .expect("write interactive commands");
    drop(stdin);

    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        if guard
            .child_mut()
            .try_wait()
            .expect("query supervisor status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "foreground supervisor did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    }

    let child = guard.disarm();
    let output = child.wait_with_output().expect("collect supervisor output");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("supervisor stdout is UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("supervisor stderr is UTF-8");

    assert!(stdout.contains("visible interactive supervisor"));
    assert!(stdout.contains("started fixture"));
    assert!(stdout.contains("restart completed for fixture: Restarted"));
    assert!(stdout.contains("stopped fixture"));
    assert!(stdout.contains("Owned service cleanup complete."));
    assert!(stderr.is_empty(), "unexpected supervisor error: {stderr}");
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aku-supervisor-foreground-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create fixture directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).ok();
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("child guard is armed")
    }

    fn disarm(mut self) -> Child {
        self.child.take().expect("child guard is armed")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}
