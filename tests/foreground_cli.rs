#![cfg(all(windows, feature = "test-fixtures"))]

use std::fs;
use std::io::Write;
use std::net::TcpListener;
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
    let config_directory = directory.path.join("AkuSupervisor");
    fs::create_dir_all(&config_directory).expect("create default config directory");
    let config_path = config_directory.join("services.json");
    let control_port = available_port();
    let config = json!({
        "version": 1,
        "control": {
            "host": "127.0.0.1",
            "port": control_port,
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
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .env("LOCALAPPDATA", &directory.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start foreground supervisor");
    let mut guard = ChildGuard::new(child);
    verify_remote_control(&directory.path);

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
    assert!(stdout.contains(&format!("Configuration: {}", config_path.display())));
    assert!(stdout.contains("Configuration source: default user configuration"));
    assert!(stdout.contains("started fixture"));
    assert!(stdout.contains("restart completed for fixture: Restarted"));
    assert!(stdout.contains("stopped fixture"));
    assert!(stdout.contains("Owned service cleanup complete."));
    assert!(stderr.is_empty(), "unexpected supervisor error: {stderr}");
    let runtime = config_directory.join(".runtime");
    let journal = fs::read_to_string(runtime.join("supervisor.jsonl"))
        .expect("read persisted lifecycle journal");
    assert!(journal.lines().count() >= 7);
    assert!(journal.contains("\"configFingerprint\":\"sha256:"));
    assert!(
        fs::read_to_string(runtime.join("services/fixture.stdout.log"))
            .expect("read captured stdout")
            .contains("process fixture root started")
    );
    assert!(
        fs::read_to_string(runtime.join("services/fixture.stderr.log"))
            .expect("read captured stderr")
            .contains("process fixture stderr ready")
    );
}

fn verify_remote_control(local_app_data: &std::path::Path) {
    let initial_status = wait_for_control_client(local_app_data);
    assert!(String::from_utf8_lossy(&initial_status.stdout).contains("\"lifecycle\": \"stopped\""));

    let started = run_client(
        local_app_data,
        &[
            "start",
            "fixture",
            "--actor",
            "codex",
            "--reason",
            "remote integration start",
            "--request-id",
            "foreground-remote-start-1",
        ],
    );
    assert!(started.status.success(), "remote start failed: {started:?}");
    assert!(String::from_utf8_lossy(&started.stdout).contains("\"outcome\": \"started\""));
    let replayed = run_client(
        local_app_data,
        &[
            "start",
            "fixture",
            "--actor",
            "codex",
            "--reason",
            "remote integration start",
            "--request-id",
            "foreground-remote-start-1",
        ],
    );
    assert!(
        replayed.status.success(),
        "idempotent replay failed: {replayed:?}"
    );
    assert!(String::from_utf8_lossy(&replayed.stdout).contains("\"outcome\": \"started\""));

    let running = run_client(local_app_data, &["status"]);
    assert!(
        running.status.success(),
        "remote status failed: {running:?}"
    );
    assert!(String::from_utf8_lossy(&running.stdout).contains("\"lifecycle\": \"running\""));
    let logs = run_client(
        local_app_data,
        &["logs", "fixture", "--stream", "stdout", "--tail", "10"],
    );
    assert!(logs.status.success(), "remote logs failed: {logs:?}");
    assert!(String::from_utf8_lossy(&logs.stdout).contains("process fixture root started"));

    let stopped = run_client(
        local_app_data,
        &["stop", "fixture", "--reason", "remote user stop hold"],
    );
    assert!(stopped.status.success(), "remote stop failed: {stopped:?}");

    let blocked = run_client(
        local_app_data,
        &[
            "start",
            "fixture",
            "--actor",
            "codex",
            "--reason",
            "agent retry after user stop",
        ],
    );
    assert_eq!(blocked.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("HTTP 403"));

    let events = run_client(local_app_data, &["events", "--limit", "20"]);
    assert!(events.status.success(), "remote events failed: {events:?}");
    let events = String::from_utf8_lossy(&events.stdout);
    assert!(events.contains("\"sequence\": 1"));
    assert!(events.contains("\"errorCategory\": \"unauthorized\""));
}

fn wait_for_control_client(local_app_data: &std::path::Path) -> std::process::Output {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        let output = run_client(local_app_data, &["status"]);
        if output.status.success() {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "control client was not ready: {output:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn run_client(local_app_data: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    Command::new(supervisor())
        .args(arguments)
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .env("LOCALAPPDATA", local_app_data)
        .output()
        .expect("run control client")
}

fn available_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve ephemeral test port")
        .local_addr()
        .expect("ephemeral listener address")
        .port()
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
