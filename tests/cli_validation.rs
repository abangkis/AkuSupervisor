use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

const CONTROL_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn create() -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("cli-validation-tests")
            .join(format!("{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).expect("isolated test directory should be created");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

#[derive(Clone)]
struct ServiceSpec {
    id: String,
    cwd: PathBuf,
    command: PathBuf,
    depends_on: Vec<String>,
}

fn existing_service(root: &Path, id: &str) -> ServiceSpec {
    let cwd = root.join("services").join(id);
    fs::create_dir_all(&cwd).expect("service working directory should be created");
    let command = cwd.join("runner.exe");
    fs::write(&command, b"fixture executable").expect("service executable fixture should exist");
    ServiceSpec {
        id: id.to_owned(),
        cwd,
        command,
        depends_on: Vec::new(),
    }
}

fn missing_service(root: &Path, id: &str) -> ServiceSpec {
    let missing = root.join("missing").join(id);
    ServiceSpec {
        id: id.to_owned(),
        cwd: missing.join("cwd"),
        command: missing.join("runner.exe"),
        depends_on: Vec::new(),
    }
}

fn config_value(port: u16, services: &[ServiceSpec]) -> Value {
    let services = services
        .iter()
        .map(|service| {
            (
                service.id.clone(),
                json!({
                    "label": service.id,
                    "cwd": service.cwd,
                    "command": service.command,
                    "dependsOn": service.depends_on,
                    "health": {"type": "process"},
                    "restartPolicy": "manual",
                    "shutdownGraceMs": 500
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "version": 2,
        "control": {
            "host": "127.0.0.1",
            "port": port,
            "tokenFile": ".runtime/control-token"
        },
        "services": services
    })
}

fn write_config(root: &Path, port: u16, services: &[ServiceSpec]) -> PathBuf {
    let runtime = root.join(".runtime");
    fs::create_dir_all(&runtime).expect("runtime directory should be created");
    fs::write(runtime.join("control-token"), format!("{CONTROL_TOKEN}\n"))
        .expect("control token fixture should be written");
    let path = root.join("services.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&config_value(port, services)).expect("config should serialize"),
    )
    .expect("test configuration should be written");
    path
}

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aku-supervisor"))
}

fn invoke(arguments: &[&str], config: &Path) -> Output {
    binary()
        .args(arguments)
        .arg("--config")
        .arg(config)
        .output()
        .expect("AkuSupervisor CLI should run")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "CLI should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_validation_error(output: &Output, code: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(code),
        "CLI should fail with validation code {code}; stdout={} stderr={stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
}

struct FakeControlServer {
    address: SocketAddr,
    stop: Sender<()>,
    thread: Option<JoinHandle<Vec<String>>>,
}

impl FakeControlServer {
    fn start(expected_requests: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake control endpoint should bind");
        listener
            .set_nonblocking(true)
            .expect("fake control endpoint should be nonblocking");
        let address = listener
            .local_addr()
            .expect("fake control endpoint should report its address");
        let (stop, stopped) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                if expected_requests > 0 && requests.len() >= expected_requests {
                    break;
                }
                if stopped.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        requests.push(handle_request(stream));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => panic!("fake endpoint accept failed: {error}"),
                }
            }
            requests
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn address(&self) -> SocketAddr {
        self.address
    }

    fn finish(mut self) -> Vec<String> {
        self.stop.send(()).ok();
        self.thread
            .take()
            .expect("fake server thread should exist")
            .join()
            .expect("fake server thread should finish")
    }
}

impl Drop for FakeControlServer {
    fn drop(&mut self) {
        self.stop.send(()).ok();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn handle_request(mut stream: TcpStream) -> String {
    stream
        .set_nonblocking(false)
        .expect("accepted control connection should be blocking");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("fake endpoint read timeout should be set");
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("client request should be read");
    let request = String::from_utf8(bytes).expect("client request should be UTF-8");
    let request_line = request.lines().next().unwrap_or_default().to_owned();
    let authorized = request.contains(&format!("Authorization: Bearer {CONTROL_TOKEN}"));
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    let body = if !authorized {
        r#"{"error":"unauthorized"}"#
    } else if target == "/v1/services" {
        r#"{"services":[]}"#
    } else if target.contains("/logs?") {
        r#"{"log":{"lines":[]}}"#
    } else {
        r#"{"operation":{"status":"completed"}}"#
    };
    let status = if authorized {
        "200 OK"
    } else {
        "401 Unauthorized"
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("fake endpoint response should be written");
    request_line
}

#[test]
fn status_and_logs_reach_control_api_when_unrelated_launch_files_are_missing() {
    let directory = TestDirectory::create();
    let server = FakeControlServer::start(2);
    let config = write_config(
        directory.path(),
        server.address().port(),
        &[
            existing_service(directory.path(), "fixture"),
            missing_service(directory.path(), "unrelated"),
        ],
    );

    let status = invoke(&["status", "--json"], &config);
    assert_success(&status);
    let logs = invoke(&["logs", "fixture", "--json"], &config);
    assert_success(&logs);

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], "GET /v1/services HTTP/1.1");
    assert!(requests[1].starts_with("GET /v1/services/fixture/logs?stream=stdout&tail="));
}

#[test]
fn stop_reaches_control_api_when_the_target_launch_files_are_missing() {
    let directory = TestDirectory::create();
    let server = FakeControlServer::start(1);
    let config = write_config(
        directory.path(),
        server.address().port(),
        &[missing_service(directory.path(), "fixture")],
    );

    let output = invoke(&["stop", "fixture", "--json"], &config);
    assert_success(&output);

    assert_eq!(server.finish(), ["POST /v1/services/fixture/stop HTTP/1.1"]);

    let unrelated_server = FakeControlServer::start(1);
    let unrelated_root = directory.path().join("unrelated-missing");
    let unrelated_config = write_config(
        &unrelated_root,
        unrelated_server.address().port(),
        &[
            existing_service(&unrelated_root, "fixture"),
            missing_service(&unrelated_root, "unrelated"),
        ],
    );
    let unrelated_output = invoke(&["stop", "fixture", "--json"], &unrelated_config);
    assert_success(&unrelated_output);
    assert_eq!(
        unrelated_server.finish(),
        ["POST /v1/services/fixture/stop HTTP/1.1"]
    );
}

#[test]
fn start_and_restart_check_requested_launch_files_without_blocking_on_unrelated_services() {
    let directory = TestDirectory::create();
    let server = FakeControlServer::start(2);
    let config = write_config(
        directory.path(),
        server.address().port(),
        &[
            existing_service(directory.path(), "fixture"),
            missing_service(directory.path(), "unrelated"),
        ],
    );

    assert_success(&invoke(&["start", "fixture", "--json"], &config));
    assert_success(&invoke(&["restart", "fixture", "--json"], &config));

    assert_eq!(
        server.finish(),
        [
            "POST /v1/services/fixture/start HTTP/1.1",
            "POST /v1/services/fixture/restart HTTP/1.1"
        ]
    );
}

#[test]
fn start_and_restart_fail_before_mutation_when_target_or_dependency_launch_files_are_missing() {
    let directory = TestDirectory::create();
    let target_server = FakeControlServer::start(0);
    let target_config = write_config(
        directory.path(),
        target_server.address().port(),
        &[missing_service(directory.path(), "fixture")],
    );

    let target_output = invoke(&["restart", "fixture", "--json"], &target_config);
    assert_validation_error(&target_output, "command_missing");
    assert!(String::from_utf8_lossy(&target_output.stderr).contains("cwd_missing"));
    let target_requests = target_server.finish();
    assert!(target_requests.is_empty());

    let dependency_server = FakeControlServer::start(0);
    let mut target = existing_service(directory.path(), "dependent");
    target.depends_on = vec!["dependency".to_owned()];
    let mut dependency = existing_service(directory.path(), "dependency");
    dependency.depends_on = vec!["leaf".to_owned()];
    let dependency_config = write_config(
        directory.path(),
        dependency_server.address().port(),
        &[
            target,
            dependency,
            missing_service(directory.path(), "leaf"),
        ],
    );

    let dependency_output = invoke(&["start", "dependent", "--json"], &dependency_config);
    assert_validation_error(
        &dependency_output,
        "services.leaf.command [command_missing]",
    );
    let dependency_requests = dependency_server.finish();
    assert!(dependency_requests.is_empty());

    let batch_server = FakeControlServer::start(0);
    let batch_root = directory.path().join("batch");
    let batch_config = write_config(
        &batch_root,
        batch_server.address().port(),
        &[
            existing_service(&batch_root, "first"),
            missing_service(&batch_root, "second"),
        ],
    );
    for action in ["start", "restart"] {
        let output = invoke(&[action, "first", "second", "--json"], &batch_config);
        assert_validation_error(&output, "services.second.command [command_missing]");
    }
    assert!(batch_server.finish().is_empty());
}

#[test]
fn control_client_rejects_static_invalid_configuration_before_network_access() {
    let directory = TestDirectory::create();
    let mut invalid_configs = Vec::new();

    let non_loopback_server = FakeControlServer::start(0);
    let non_loopback_root = directory.path().join("non-loopback");
    let non_loopback = write_config(
        &non_loopback_root,
        non_loopback_server.address().port(),
        &[existing_service(&non_loopback_root, "fixture")],
    );
    let mut value: Value = serde_json::from_slice(&fs::read(&non_loopback).expect("read config"))
        .expect("config should parse");
    value["control"]["host"] = json!("0.0.0.0");
    fs::write(&non_loopback, value.to_string()).expect("invalid config should be written");
    invalid_configs.push((
        non_loopback,
        non_loopback_server,
        "control_host_not_loopback",
    ));

    let token_path_server = FakeControlServer::start(0);
    let token_path_root = directory.path().join("token-path");
    let token_path = write_config(
        &token_path_root,
        token_path_server.address().port(),
        &[existing_service(&token_path_root, "fixture")],
    );
    let mut value: Value = serde_json::from_slice(&fs::read(&token_path).expect("read config"))
        .expect("config should parse");
    value["control"]["tokenFile"] = json!("../control-token");
    fs::write(&token_path, value.to_string()).expect("invalid config should be written");
    invalid_configs.push((token_path, token_path_server, "token_path_outside_runtime"));

    let relative_path_server = FakeControlServer::start(0);
    let relative_path_root = directory.path().join("relative-path");
    let relative_path = write_config(
        &relative_path_root,
        relative_path_server.address().port(),
        &[existing_service(&relative_path_root, "fixture")],
    );
    let mut value: Value = serde_json::from_slice(&fs::read(&relative_path).expect("read config"))
        .expect("config should parse");
    value["services"]["fixture"]["command"] = json!("runner.exe");
    fs::write(&relative_path, value.to_string()).expect("invalid config should be written");
    invalid_configs.push((relative_path, relative_path_server, "command_not_absolute"));

    let cycle_server = FakeControlServer::start(0);
    let mut fixture = existing_service(directory.path(), "fixture");
    fixture.depends_on = vec!["worker".to_owned()];
    let mut worker = existing_service(directory.path(), "worker");
    worker.depends_on = vec!["fixture".to_owned()];
    let cycle_root = directory.path().join("cycle");
    let cycle = write_config(
        &cycle_root,
        cycle_server.address().port(),
        &[fixture, worker],
    );
    invalid_configs.push((cycle, cycle_server, "service_dependency_cycle"));

    for (config, server, code) in invalid_configs {
        let output = invoke(&["status", "--json"], &config);
        assert_validation_error(&output, code);
        assert!(server.finish().is_empty());
    }
}

#[test]
fn startup_validator_is_local_read_only_and_scopes_launch_readiness() {
    let directory = TestDirectory::create();
    let idle = missing_service(directory.path(), "idle");
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("reserve an unused control port")
        .local_addr()
        .expect("unused control port address")
        .port();
    let config_path = directory.path().join("services.json");
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config_value(port, &[idle])).expect("serialize startup fixture"),
    )
    .expect("write startup fixture without runtime token");

    let output = binary()
        .args(["validate-startup", "--json", "--config"])
        .arg(&config_path)
        .output()
        .expect("run local startup validator");
    assert_success(&output);
    let result: Value = serde_json::from_slice(&output.stdout).expect("startup result JSON");
    assert_eq!(result["valid"], true);
    assert_eq!(result["requestedServiceIds"], json!([]));
    assert!(!directory.path().join(".runtime").exists());
    TcpListener::bind(("127.0.0.1", port))
        .expect("startup validator must not open a control listener");

    let readiness = binary()
        .args([
            "validate-startup",
            "--json",
            "--service",
            "idle",
            "--config",
        ])
        .arg(&config_path)
        .output()
        .expect("run scoped startup readiness check");
    assert!(!readiness.status.success());
    let result: Value = serde_json::from_slice(&readiness.stdout)
        .expect("failed readiness result remains machine-readable");
    assert_eq!(result["valid"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("command_missing")
    );
    assert!(!directory.path().join(".runtime").exists());
}

#[test]
fn startup_validator_rejects_structural_contract_errors_without_network_access() {
    let directory = TestDirectory::create();
    let service = existing_service(directory.path(), "fixture");
    let mut cases = Vec::new();

    let mut unsupported_version = config_value(0, std::slice::from_ref(&service));
    unsupported_version["version"] = json!(99);
    cases.push(("version", unsupported_version));

    let mut non_loopback = config_value(0, std::slice::from_ref(&service));
    non_loopback["control"]["host"] = json!("0.0.0.0");
    cases.push(("control_host_not_loopback", non_loopback));

    let mut invalid_token_path = config_value(0, std::slice::from_ref(&service));
    invalid_token_path["control"]["tokenFile"] = json!("../outside-token");
    cases.push(("token_path_outside_runtime", invalid_token_path));

    let mut first = existing_service(directory.path(), "first");
    first.depends_on = vec!["second".to_owned()];
    let mut second = existing_service(directory.path(), "second");
    second.depends_on = vec!["first".to_owned()];
    cases.push((
        "service_dependency_cycle",
        config_value(0, &[first, second]),
    ));

    for (code, value) in cases {
        let config_path = directory.path().join(format!("{code}.json"));
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&value).expect("serialize invalid config"),
        )
        .expect("write invalid config");
        let output = binary()
            .args(["validate-startup", "--json", "--config"])
            .arg(&config_path)
            .output()
            .expect("run structural validator");
        assert!(!output.status.success(), "{code} should fail");
        let result: Value = serde_json::from_slice(&output.stdout)
            .expect("structural validation error should be JSON");
        assert_eq!(result["valid"], false);
        assert!(
            result["error"].as_str().unwrap_or_default().contains(code),
            "expected {code}; got {result}"
        );
        assert!(!directory.path().join(".runtime").exists());
    }
}
