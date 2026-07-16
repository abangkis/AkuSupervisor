#![cfg(all(windows, feature = "test-fixtures"))]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
            "tokenFile": ".runtime/control-token",
            "mcp": {
                "enabled": true,
                "allowedOrigins": []
            }
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
    verify_read_only_mcp(&directory.path, control_port);

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
    assert!(stdout.contains(&format!(
        "Read-only MCP: http://127.0.0.1:{control_port}/mcp"
    )));
    assert!(stdout.contains(&format!("Configuration: {}", config_path.display())));
    assert!(stdout.contains("Configuration source: default user configuration"));
    assert!(stdout.contains("started fixture"));
    assert!(stdout.contains("restart completed for fixture: Restarted"));
    assert!(stdout.contains("stopped fixture"));
    assert_console_events(&stdout, &stderr);
    assert!(stdout.contains("Owned service cleanup complete."));
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

fn assert_console_events(stdout: &str, stderr: &str) {
    assert!(
        stdout.contains("] [event #1] fixture start: stopped -> running (agent/codex, success)")
    );
    let stderr_lines = stderr.lines().collect::<Vec<_>>();
    assert_eq!(
        stderr_lines.len(),
        1,
        "stderr may contain only the expected canonical authorization failure event"
    );
    assert!(stderr_lines[0].starts_with('['));
    assert!(
        stderr_lines[0]
            .contains("] [event #3] fixture start: stopped -> stopped (agent/codex, unauthorized)")
    );
}

fn verify_read_only_mcp(local_app_data: &std::path::Path, port: u16) {
    let token = fs::read_to_string(local_app_data.join("AkuSupervisor/.runtime/control-token"))
        .expect("read MCP bearer token");

    let (status, initialized) = mcp_request(
        port,
        token.trim(),
        None,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name":"integration-test","version":"1"}
            }
        }),
    );
    assert_eq!(status, 200);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(initialized["result"]["capabilities"], json!({"tools":{}}));

    let (status, listed) = mcp_request(
        port,
        token.trim(),
        None,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    assert_eq!(status, 200);
    let tools = listed["result"]["tools"].as_array().expect("MCP tools");
    assert_eq!(tools.len(), 4);
    assert!(
        tools
            .iter()
            .all(|tool| tool["annotations"]["readOnlyHint"] == true)
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"] == "supervisor_restart_service")
    );

    let (status, service) = mcp_request(
        port,
        token.trim(),
        None,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{
                "name":"supervisor_get_service",
                "arguments":{"serviceId":"fixture"}
            }
        }),
    );
    assert_eq!(status, 200);
    assert_eq!(
        service["result"]["structuredContent"]["service"]["lifecycle"],
        "stopped"
    );

    let (status, mutation) = mcp_request(
        port,
        token.trim(),
        None,
        &json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params":{
                "name":"supervisor_restart_service",
                "arguments":{"serviceId":"fixture"}
            }
        }),
    );
    assert_eq!(status, 200);
    assert_eq!(mutation["error"]["code"], -32601);

    let (status, rejected_origin) = mcp_request(
        port,
        token.trim(),
        Some("https://attacker.example"),
        &json!({"jsonrpc":"2.0","id":5,"method":"tools/list"}),
    );
    assert_eq!(status, 403);
    assert_eq!(rejected_origin["error"]["code"], -32000);
    verify_stdio_proxy(local_app_data);
}

fn verify_stdio_proxy(local_app_data: &std::path::Path) {
    let mut child = Command::new(supervisor())
        .arg("mcp-proxy")
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .env("LOCALAPPDATA", local_app_data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start MCP stdio proxy");
    let mut stdin = child.stdin.take().expect("proxy stdin");
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc":"2.0",
            "id":"proxy-init",
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"proxy-test","version":"1"}
            }
        })
    )
    .expect("write proxy initialize");
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":"proxy-list","method":"tools/list"})
    )
    .expect("write proxy tools/list");
    drop(stdin);

    let output = child.wait_with_output().expect("collect proxy output");
    assert!(output.status.success(), "proxy failed: {output:?}");
    assert!(output.stderr.is_empty(), "proxy stderr: {output:?}");
    let lines = String::from_utf8(output.stdout).expect("proxy UTF-8 stdout");
    let responses = lines
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("proxy JSON line"))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], "proxy-init");
    assert_eq!(responses[1]["id"], "proxy-list");
    assert_eq!(
        responses[1]["result"]["tools"]
            .as_array()
            .expect("proxy tool list")
            .len(),
        4
    );
}

fn mcp_request(
    port: u16,
    token: &str,
    origin: Option<&str>,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let body = serde_json::to_vec(&body).expect("serialize MCP request");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect MCP endpoint");
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-11-25\r\nContent-Length: {}\r\n",
        body.len()
    )
    .expect("write MCP headers");
    if let Some(origin) = origin {
        write!(stream, "Origin: {origin}\r\n").expect("write Origin");
    }
    stream
        .write_all(b"Connection: close\r\n\r\n")
        .expect("finish headers");
    // Force separate header/body delivery so Windows accepted sockets must
    // honor their blocking timeout instead of leaking listener nonblocking mode.
    thread::sleep(Duration::from_millis(20));
    stream.write_all(&body).expect("write MCP body");
    stream.flush().expect("flush MCP request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read MCP response");
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response separator");
    let headers = std::str::from_utf8(&response[..separator]).expect("UTF-8 response headers");
    let status = headers
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status");
    let value = serde_json::from_slice(&response[separator + 4..]).expect("MCP JSON response");
    (status, value)
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
