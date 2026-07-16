#![cfg(all(windows, feature = "test-fixtures"))]

use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use aku_supervisor::adapters::config::ServiceConfig;
use aku_supervisor::adapters::registration::{
    PrepareRegistration, RegistrationAuthority, RegistrationOperation,
    RegistrationReconciliationOutcome,
};

const TIMEOUT: Duration = Duration::from_secs(15);

fn supervisor() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor")
}

fn process_fixture() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor-process-fixture")
}

#[test]
fn terminal_tree_is_reaped_and_on_failure_is_capped_and_audited() {
    let directory = TestDirectory::create();
    let config_directory = directory.path.join("AkuSupervisor");
    fs::create_dir_all(&config_directory).expect("create config directory");
    let config_path = config_directory.join("services.json");
    let config = json!({
        "version": 1,
        "control": {
            "host": "127.0.0.1",
            "port": available_port(),
            "tokenFile": ".runtime/control-token"
        },
        "services": {
            "manual-crash": crash_service("manual"),
            "automatic-crash": crash_service("on-failure")
        }
    });
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("serialize config"),
    )
    .expect("write config");

    let child = Command::new(supervisor())
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .env("LOCALAPPDATA", &directory.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start supervisor");
    let mut guard = ChildGuard::new(child);
    wait_for_status(&directory.path);

    start_service(&directory.path, "manual-crash");
    let manual = wait_for_service(&directory.path, "manual-crash", |service| {
        service["lifecycle"] == "failed" && service["lastExitCode"] == 17
    });
    assert_eq!(manual["rootPid"], Value::Null);
    assert_eq!(manual["restartCount"], 0);
    start_service(&directory.path, "manual-crash");

    start_service(&directory.path, "automatic-crash");
    let automatic = wait_for_service(&directory.path, "automatic-crash", |service| {
        service["lifecycle"] == "failed"
            && service["lastExitCode"] == 17
            && service["restartCount"] == 1
    });
    assert_eq!(automatic["rootPid"], Value::Null);
    assert_eq!(automatic["desiredState"], "running");

    let journal = fs::read_to_string(config_directory.join(".runtime/supervisor.jsonl"))
        .expect("read lifecycle journal");
    assert!(journal.contains("\"action\":\"process_exit\""));
    assert!(journal.contains("\"exitCode\":17"));
    assert!(journal.contains("\"automaticRestartPlanned\":true"));
    assert!(journal.contains("\"actorType\":\"recovery\""));

    let mut stdin = guard.child_mut().stdin.take().expect("supervisor stdin");
    stdin.write_all(b"quit\n").expect("request quit");
    drop(stdin);
    let output = guard
        .disarm()
        .wait_with_output()
        .expect("collect supervisor output");
    assert!(output.status.success(), "supervisor failed: {output:?}");
}

#[test]
fn registration_change_preserves_running_service_and_supervisor_process() {
    let directory = TestDirectory::create();
    let config_directory = directory.path.join("AkuSupervisor");
    fs::create_dir_all(&config_directory).expect("create config directory");
    let registration_directory = config_directory.join(".runtime/registration");
    fs::create_dir_all(&registration_directory).expect("create registration directory");
    let registration_audit = registration_directory.join("audit.jsonl");
    fs::write(&registration_audit, []).expect("create empty registration audit");
    let config_path = config_directory.join("services.json");
    let config = json!({
        "version": 1,
        "control": {
            "host": "127.0.0.1",
            "port": available_port(),
            "tokenFile": ".runtime/control-token"
        },
        "services": {
            "retained": root_service("Retained")
        }
    });
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).expect("serialize config"),
    )
    .expect("write config");

    let child = Command::new(supervisor())
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .env("LOCALAPPDATA", &directory.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start supervisor");
    let mut guard = ChildGuard::new(child);
    let supervisor_pid = guard.child_mut().id();
    wait_for_status(&directory.path);
    start_service(&directory.path, "retained");
    let retained_before = wait_for_service(&directory.path, "retained", |service| {
        service["lifecycle"] == "running" && service["rootPid"].is_number()
    });
    let retained_pid = retained_before["rootPid"].clone();

    register_service_and_require_applied(&config_path);

    let registered = wait_for_service(&directory.path, "registered", |service| {
        service["lifecycle"] == "stopped" && service["desiredState"] == "stopped"
    });
    assert_eq!(registered["rootPid"], Value::Null);
    let retained_after = wait_for_service(&directory.path, "retained", |service| {
        service["rootPid"] == retained_pid && service["lifecycle"] == "running"
    });
    assert_eq!(retained_after["rootPid"], retained_pid);
    assert_eq!(guard.child_mut().id(), supervisor_pid);

    let logs = client(&directory.path)
        .args(["logs", "registered", "--stream", "stdout", "--tail", "5"])
        .output()
        .expect("registered log client");
    assert!(
        logs.status.success(),
        "dynamic log allowlist failed: {logs:?}"
    );

    let mut stdin = guard.child_mut().stdin.take().expect("supervisor stdin");
    stdin.write_all(b"quit\n").expect("request quit");
    drop(stdin);
    let output = guard
        .disarm()
        .wait_with_output()
        .expect("collect supervisor output");
    assert!(output.status.success(), "supervisor failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[registry] Applied revision"));
    assert!(stdout.contains("without Supervisor handoff"));
    assert!(
        stdout.contains("[registration] prepared register registered by agent/registration_mcp")
    );
    assert!(stdout.contains("[registration] approved register registered by user/human_cli"));
    assert!(
        stdout.contains("[registration] committed register registered by agent/registration_mcp")
    );
    assert!(stdout.contains("added=registered"));
    assert!(stdout.contains("unrelated services preserved"));
}

fn register_service_and_require_applied(config_path: &Path) {
    let authority = RegistrationAuthority::open(Some(config_path.to_owned()))
        .expect("open registration authority");
    let capabilities = authority.capabilities().expect("registration capabilities");
    let base_revision = capabilities["currentRevision"]
        .as_str()
        .expect("current revision")
        .to_owned();
    let service: ServiceConfig =
        serde_json::from_value(root_service("Registered")).expect("registered service config");
    let draft = authority
        .prepare(PrepareRegistration {
            request_id: "registration-live-test".to_owned(),
            operation: RegistrationOperation::Register,
            service_id: "registered".to_owned(),
            base_revision,
            service: Some(service),
        })
        .expect("prepare registration");
    authority
        .approve_fixture(draft.clone())
        .expect("approve registration fixture");
    let commit = authority
        .commit(&draft.draft_id)
        .expect("commit registration");
    assert_eq!(
        commit.registry_reconciliation,
        RegistrationReconciliationOutcome::Applied
    );
    assert_eq!(
        commit.runtime_active_revision.as_deref(),
        Some(commit.configuration_revision.as_str())
    );
    assert_eq!(
        commit.runtime_disk_revision.as_deref(),
        Some(commit.configuration_revision.as_str())
    );
}

fn crash_service(restart_policy: &str) -> Value {
    json!({
        "label": "Crash Fixture",
        "cwd": std::env::temp_dir(),
        "command": process_fixture(),
        "args": ["--exit-after", "150", "17"],
        "environment": {},
        "health": { "type": "process" },
        "ports": [],
        "restartPolicy": restart_policy,
        "shutdownGraceMs": 250
    })
}

fn root_service(label: &str) -> Value {
    json!({
        "label": label,
        "cwd": std::env::temp_dir(),
        "command": process_fixture(),
        "args": ["--root"],
        "environment": {},
        "health": { "type": "process" },
        "ports": [],
        "restartPolicy": "manual",
        "shutdownGraceMs": 250
    })
}

fn start_service(local_app_data: &Path, service_id: &str) {
    let output = client(local_app_data)
        .args([
            "start",
            service_id,
            "--reason",
            "process supervision integration test",
        ])
        .output()
        .expect("start service client");
    assert!(output.status.success(), "start failed: {output:?}");
}

fn wait_for_status(local_app_data: &Path) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if client(local_app_data)
            .args(["status", "--json"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "control API did not become ready"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_service(
    local_app_data: &Path,
    service_id: &str,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let output = client(local_app_data)
            .args(["status", "--json"])
            .output()
            .expect("status client");
        if output.status.success() {
            let body: Value = serde_json::from_slice(&output.stdout).expect("status JSON");
            if let Some(service) = body["response"]["services"]
                .as_array()
                .and_then(|services| services.iter().find(|item| item["id"] == service_id))
                && matches(service)
            {
                return service.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "service {service_id} did not reach expected state"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn client(local_app_data: &Path) -> Command {
    let mut command = Command::new(supervisor());
    command
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .env("LOCALAPPDATA", local_app_data);
    command
}

fn available_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("ephemeral address")
        .port()
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child guard active")
    }

    fn disarm(mut self) -> Child {
        self.0.take().expect("child guard active")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            child.kill().ok();
            child.wait().ok();
        }
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aku-supervisor-process-supervision-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).ok();
    }
}
