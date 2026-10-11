use std::error::Error;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aku_supervisor::adapters::config::McpConfig;
use aku_supervisor::adapters::control_http::ControlHttpServer;
use aku_supervisor::adapters::journal::{AuditedControl, FileJournal};
use aku_supervisor::adapters::runtime_instance::{RuntimeInstance, RuntimeMode};
use aku_supervisor::adapters::runtime_token::RuntimeToken;
use aku_supervisor::adapters::service_logs::ServiceLogStore;
use aku_supervisor::adapters::supervisor_shutdown::SupervisorShutdown;
use aku_supervisor::application::{
    DependencyAwareControl, HealthCheckSpec, HealthProbe, LaunchSpec, ManagedProcessTree,
    PortDiagnostic, PortInspector, PortOccupant, ProcessTreeSpawner, RegistryReconciliationStatus,
    ServiceRegistration, ServiceRestartPolicy, SupervisorControl, TransportHealth, TreeStopReport,
};
use serde_json::{Value, json};

const TOKEN: &str = "c9";

#[test]
fn dashboard_start_stop_uses_launch_checks_user_holds_and_audit_without_primary_bearer() {
    let fixture = HttpFixture::new();
    let address = fixture.server.address();
    let session = request_with_headers(
        address,
        "GET",
        "/dashboard/v1/session",
        None,
        &[("X-Dashboard-Request", "1")],
    );
    assert_eq!(session.status, 200);
    let capability = session.body["token"]
        .as_str()
        .expect("dashboard capability");
    let origin = format!("http://{address}");
    let headers = [
        ("Origin", origin.as_str()),
        ("X-Dashboard-Token", capability),
    ];
    let start_path = "/dashboard/v1/services/target/start";
    let start_body = json!({"requestId":"dashboard-core-start"});
    let start = request_with_headers(
        address,
        "POST",
        start_path,
        Some(start_body.clone()),
        &headers,
    );
    assert_eq!(start.status, 200);
    assert_eq!(start.body["outcome"], "started");
    let started = fixture.snapshot("target");
    assert_eq!(started["lifecycle"], "running");
    assert_eq!(started["operatorHold"], "none");
    assert_eq!(
        started["lastAction"]["actor"],
        json!({"actorType":"user","actorId":"ui"})
    );
    assert_eq!(fixture.snapshot("dependency")["lifecycle"], "running");
    let spawn_count = fixture.metrics.spawned.lock().expect("spawn lock").len();
    assert_eq!(
        request_with_headers(address, "POST", start_path, Some(start_body), &headers).status,
        200
    );
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count
    );

    let stop = request_with_headers(
        address,
        "POST",
        "/dashboard/v1/services/target/stop",
        Some(json!({"requestId":"dashboard-core-stop"})),
        &headers,
    );
    assert_eq!(stop.status, 200);
    assert_eq!(stop.body["outcome"], "stopped");
    let stopped = fixture.snapshot("target");
    assert_eq!(stopped["lifecycle"], "stopped");
    assert_eq!(stopped["rootPid"], Value::Null);
    assert_eq!(stopped["ownedPids"], json!([]));
    assert_eq!(stopped["operatorHold"], "stopped");
    let dependency_before = fixture.snapshot("dependency");
    let journal = fs::read_to_string(fixture.directory.path().join("runtime/journal.jsonl"))
        .expect("read fixture audit");
    let events = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["serviceId"] == "target")
        .collect::<Vec<_>>();
    assert_eq!(
        events.len(),
        2,
        "replayed Start must not create another audit event"
    );
    for event in events {
        assert_eq!(event["actor"], json!({"actorType":"user","actorId":"ui"}));
        assert_eq!(event["reason"], "Dashboard service control");
    }
    assert!(
        !journal.contains(capability),
        "capability must not appear in the audit journal"
    );
    fs::remove_file(&fixture.paths.target_executable).expect("remove stopped fixture command");
    let failure = request_with_headers(
        address,
        "POST",
        start_path,
        Some(json!({"requestId":"dashboard-core-missing-command"})),
        &headers,
    );
    assert_eq!(failure.status, 500);
    assert_eq!(failure.body["error"]["code"], "spawn_failed");
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count
    );
    assert_unchanged_target(&dependency_before, &fixture.snapshot("dependency"));
}

#[test]
fn dashboard_restart_is_one_audited_operation_and_preflights_before_stopping() {
    let fixture = HttpFixture::new();
    assert_eq!(fixture.mutate("target", "start").status, 200);
    let address = fixture.server.address();
    let session = request_with_headers(
        address,
        "GET",
        "/dashboard/v1/session",
        None,
        &[("X-Dashboard-Request", "1")],
    );
    assert_eq!(session.status, 200);
    let capability = session.body["token"]
        .as_str()
        .expect("dashboard capability");
    let origin = format!("http://{address}");
    let headers = [
        ("Origin", origin.as_str()),
        ("X-Dashboard-Token", capability),
    ];
    let path = "/dashboard/v1/services/target/restart";
    let body = json!({"requestId":"dashboard-core-restart"});
    let before = fixture.snapshot("target");
    let dependency = fixture.snapshot("dependency");
    let spawn_count = fixture.metrics.spawned.lock().expect("spawn lock").len();
    let stop_count = fixture.metrics.stopped.lock().expect("stop lock").len();
    for _ in 0..2 {
        let restarted = request_with_headers(address, "POST", path, Some(body.clone()), &headers);
        assert_eq!(restarted.status, 200);
        assert_eq!(restarted.body["outcome"], "restarted");
    }
    let restarted = fixture.snapshot("target");
    assert_eq!(restarted["lifecycle"], "running");
    assert_ne!(before["rootPid"], restarted["rootPid"]);
    assert_eq!(
        restarted["lastAction"]["actor"],
        json!({"actorType":"user","actorId":"ui"})
    );
    assert_unchanged_target(&dependency, &fixture.snapshot("dependency"));
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count + 1
    );
    assert_eq!(
        fixture.metrics.stopped.lock().expect("stop lock").len(),
        stop_count + 1
    );
    let journal = fs::read_to_string(fixture.directory.path().join("runtime/journal.jsonl"))
        .expect("read restart audit");
    let events = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["action"] == "restart")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "replayed Restart has one audit event");
    assert_eq!(
        events[0]["actor"],
        json!({"actorType":"user","actorId":"ui"})
    );
    assert!(!journal.contains(capability));
    fs::remove_file(&fixture.paths.target_executable).expect("remove running target command");
    let failed = request_with_headers(
        address,
        "POST",
        path,
        Some(json!({"requestId":"dashboard-restart-missing-command"})),
        &headers,
    );
    assert_eq!(failed.status, 500);
    assert_eq!(failed.body["error"]["code"], "spawn_failed");
    assert_unchanged_target(&restarted, &fixture.snapshot("target"));
    assert_unchanged_target(&dependency, &fixture.snapshot("dependency"));
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count + 1
    );
    assert_eq!(
        fixture.metrics.stopped.lock().expect("stop lock").len(),
        stop_count + 1
    );
}

#[test]
fn authenticated_http_restart_preserves_target_when_executable_or_cwd_disappears() {
    let fixture = HttpFixture::new();
    assert_eq!(fixture.mutate("target", "start").status, 200);

    let before = fixture.snapshot("target");
    let spawn_count = fixture.metrics.spawned.lock().expect("spawn lock").len();
    let stop_count = fixture.metrics.stopped.lock().expect("stop lock").len();
    fs::remove_file(&fixture.paths.target_executable).expect("remove target executable");
    let response = fixture.mutate("target", "restart");
    assert_eq!(response.status, 500);
    assert_eq!(response.body["error"]["code"], "spawn_failed");
    assert_unchanged_target(&before, &fixture.snapshot("target"));
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count
    );
    assert_eq!(
        fixture.metrics.stopped.lock().expect("stop lock").len(),
        stop_count
    );

    fs::write(&fixture.paths.target_executable, b"fixture command").expect("restore executable");
    fs::remove_dir(&fixture.paths.target_cwd).expect("remove target working directory");
    let response = fixture.mutate("target", "restart");
    assert_eq!(response.status, 500);
    assert_eq!(response.body["error"]["code"], "spawn_failed");
    assert_unchanged_target(&before, &fixture.snapshot("target"));
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count
    );
    assert_eq!(
        fixture.metrics.stopped.lock().expect("stop lock").len(),
        stop_count
    );
}

#[test]
fn dependency_restart_preflights_before_mutation_and_skips_running_dependencies() {
    let fixture = HttpFixture::new();
    assert_eq!(fixture.mutate("target", "start").status, 200);
    let dependency_before = fixture.snapshot("dependency");
    let target_before = fixture.snapshot("target");

    fs::remove_file(&fixture.paths.dependency_executable)
        .expect("remove running dependency executable");
    let response = fixture.mutate("target", "restart");
    assert_eq!(response.status, 200);
    assert_eq!(response.body["outcome"], "restarted");
    assert_eq!(
        fixture.snapshot("dependency")["rootPid"],
        dependency_before["rootPid"]
    );
    assert_ne!(
        fixture.snapshot("target")["rootPid"],
        target_before["rootPid"]
    );

    fs::write(&fixture.paths.dependency_executable, b"fixture dependency")
        .expect("restore dependency");
    assert_eq!(fixture.mutate("dependency", "stop").status, 200);
    let target_before_failed_restart = fixture.snapshot("target");
    let dependency_before_failed_restart = fixture.snapshot("dependency");
    let spawn_count = fixture.metrics.spawned.lock().expect("spawn lock").len();
    let stop_count = fixture.metrics.stopped.lock().expect("stop lock").len();
    fs::remove_file(&fixture.paths.dependency_executable)
        .expect("remove stopped dependency executable");

    let response = fixture.mutate("target", "restart");
    assert_eq!(response.status, 500);
    assert_eq!(response.body["error"]["code"], "spawn_failed");
    assert_unchanged_target(&target_before_failed_restart, &fixture.snapshot("target"));
    assert_unchanged_target(
        &dependency_before_failed_restart,
        &fixture.snapshot("dependency"),
    );
    assert_eq!(
        fixture.metrics.spawned.lock().expect("spawn lock").len(),
        spawn_count
    );
    assert_eq!(
        fixture.metrics.stopped.lock().expect("stop lock").len(),
        stop_count
    );
}

fn assert_unchanged_target(before: &Value, after: &Value) {
    for field in [
        "lifecycle",
        "rootPid",
        "ownedPids",
        "desiredState",
        "operatorHold",
        "lastAction",
        "restartCount",
    ] {
        assert_eq!(
            before[field], after[field],
            "snapshot field {field} changed"
        );
    }
}

struct HttpFixture {
    server: ControlHttpServer,
    metrics: Arc<ProcessMetrics>,
    paths: FixturePaths,
    directory: TestDirectory,
}

impl HttpFixture {
    fn new() -> Self {
        let directory = TestDirectory::create();
        let paths = FixturePaths::create(directory.path());
        let metrics = Arc::new(ProcessMetrics::default());
        let registrations = [
            registration(
                "dependency",
                &paths.dependency_executable,
                &paths.dependency_cwd,
                &[],
            ),
            registration(
                "target",
                &paths.target_executable,
                &paths.target_cwd,
                &["dependency"],
            ),
        ];
        let registry = Arc::new(
            aku_supervisor::application::ServiceRegistry::new(
                registrations,
                FixtureSpawner {
                    metrics: Arc::clone(&metrics),
                    next_pid: Arc::new(AtomicU32::new(20_000)),
                },
                FixturePortInspector,
                Arc::new(FixtureHealthProbe),
            )
            .expect("build service registry"),
        );
        let registry_control: Arc<dyn SupervisorControl> = registry;
        let journal = Arc::new(
            FileJournal::open(
                directory.path().join("runtime/journal.jsonl"),
                [TOKEN.repeat(32)],
            )
            .expect("open fixture journal"),
        );
        let audited = Arc::new(AuditedControl::new(
            registry_control,
            journal.clone(),
            "sha256:fixture",
        ));
        let control: Arc<dyn SupervisorControl> = Arc::new(DependencyAwareControl::new(audited));
        let token =
            RuntimeToken::load_or_create(&directory.path().join("runtime/control-token"), || {
                Ok(TOKEN.repeat(32))
            })
            .expect("create fixture token");
        let instance = RuntimeInstance {
            schema_version: 1,
            instance_id: "fixture-instance".to_owned(),
            process_id: std::process::id(),
            executable: PathBuf::from("fixture-supervisor.exe"),
            mode: RuntimeMode::Stable,
            watcher_process_id: None,
            started_at_unix_ms: 0,
            version: "fixture".to_owned(),
            config_fingerprint: "sha256:fixture".to_owned(),
            control_api: "http://127.0.0.1:0".to_owned(),
        };
        let server = ControlHttpServer::start(
            "127.0.0.1",
            0,
            token,
            McpConfig::default(),
            control,
            None,
            journal,
            Arc::new(ServiceLogStore::new(
                &directory.path().join("runtime/services"),
                ["dependency".to_owned(), "target".to_owned()],
            )),
            Arc::new(RegistryReconciliationStatus::new(
                "sha256:fixture".to_owned(),
            )),
            Arc::new(instance),
            Arc::new(SupervisorShutdown::default()),
        )
        .expect("start fixture HTTP server");
        Self {
            server,
            metrics,
            paths,
            directory,
        }
    }

    fn snapshot(&self, service_id: &str) -> Value {
        let response = request(
            self.server.address(),
            "GET",
            &format!("/v1/services/{service_id}"),
            None,
        );
        assert_eq!(response.status, 200, "status response: {}", response.body);
        response.body["service"].clone()
    }

    fn mutate(&self, service_id: &str, action: &str) -> HttpResponse {
        request(
            self.server.address(),
            "POST",
            &format!("/v1/services/{service_id}/{action}"),
            Some(json!({"actor":"user","reason":"fixture lifecycle request"})),
        )
    }
}

fn registration(
    id: &str,
    executable: &Path,
    cwd: &Path,
    dependencies: &[&str],
) -> ServiceRegistration {
    ServiceRegistration::new(
        id,
        id,
        LaunchSpec::new(
            executable,
            std::iter::empty::<&str>(),
            cwd,
            std::iter::empty::<(&str, &str)>(),
        ),
        HealthCheckSpec::Process,
        ServiceRestartPolicy::Manual,
        Vec::new(),
        Duration::from_millis(100),
    )
    .with_dependencies(
        dependencies
            .iter()
            .map(|dependency| (*dependency).to_owned()),
    )
}

#[derive(Debug)]
struct ProcessMetrics {
    spawned: Mutex<Vec<(String, u32)>>,
    stopped: Mutex<Vec<u32>>,
}

impl Default for ProcessMetrics {
    fn default() -> Self {
        Self {
            spawned: Mutex::new(Vec::new()),
            stopped: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Debug)]
struct FixtureSpawner {
    metrics: Arc<ProcessMetrics>,
    next_pid: Arc<AtomicU32>,
}

impl ProcessTreeSpawner for FixtureSpawner {
    type Process = FixtureProcess;

    fn preflight_launch(
        &self,
        service_id: &str,
        launch: &LaunchSpec,
    ) -> Result<(), FixtureProcessError> {
        if !launch.cwd().is_dir() {
            return Err(FixtureProcessError(format!(
                "{service_id} working directory unavailable"
            )));
        }
        if !launch.executable().is_file() {
            return Err(FixtureProcessError(format!(
                "{service_id} executable unavailable"
            )));
        }
        Ok(())
    }

    fn spawn(
        &self,
        service_id: &str,
        launch: &LaunchSpec,
    ) -> Result<Self::Process, FixtureProcessError> {
        self.preflight_launch(service_id, launch)?;
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        self.metrics
            .spawned
            .lock()
            .expect("spawn lock")
            .push((service_id.to_owned(), pid));
        Ok(FixtureProcess {
            pid,
            metrics: Arc::clone(&self.metrics),
        })
    }
}

#[derive(Debug)]
struct FixtureProcess {
    pid: u32,
    metrics: Arc<ProcessMetrics>,
}

impl ManagedProcessTree for FixtureProcess {
    type Error = FixtureProcessError;

    fn root_pid(&self) -> u32 {
        self.pid
    }

    fn owned_pids(&self) -> Result<Vec<u32>, Self::Error> {
        Ok(vec![self.pid])
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, Self::Error> {
        Ok(None)
    }

    fn stop(&mut self, _grace: Duration) -> Result<TreeStopReport, Self::Error> {
        self.metrics
            .stopped
            .lock()
            .expect("stop lock")
            .push(self.pid);
        Ok(TreeStopReport {
            owned_pids_before: vec![self.pid],
            owned_pids_after: Vec::new(),
            graceful_signal_sent: true,
            graceful_signal_error: None,
            forced: false,
        })
    }
}

#[derive(Debug)]
struct FixtureProcessError(String);

impl fmt::Display for FixtureProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for FixtureProcessError {}

#[derive(Debug)]
struct FixturePortInspector;

impl PortInspector for FixturePortInspector {
    type Error = FixtureProcessError;

    fn inspect_tcp_port(&self, port: u16) -> Result<PortDiagnostic, Self::Error> {
        Ok(PortDiagnostic::new(port, Vec::<PortOccupant>::new()))
    }
}

#[derive(Debug)]
struct FixtureHealthProbe;

impl HealthProbe for FixtureHealthProbe {
    fn probe(&self, _check: &HealthCheckSpec, _timeout: Duration) -> TransportHealth {
        TransportHealth {
            transport_ready: true,
            healthy: true,
            detail: "fixture healthy".to_owned(),
            observed: std::collections::BTreeMap::new(),
        }
    }
}

struct HttpResponse {
    status: u16,
    body: Value,
}

fn request(address: SocketAddr, method: &str, target: &str, body: Option<Value>) -> HttpResponse {
    let bearer = format!("Bearer {}", TOKEN.repeat(32));
    request_with_headers(
        address,
        method,
        target,
        body,
        &[("Authorization", bearer.as_str())],
    )
}

fn request_with_headers(
    address: SocketAddr,
    method: &str,
    target: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> HttpResponse {
    let body = body.map_or_else(Vec::new, |value| {
        serde_json::to_vec(&value).expect("JSON body")
    });
    let mut stream = TcpStream::connect(address).expect("connect to fixture HTTP server");
    let mut request =
        format!("{method} {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    for (name, value) in headers {
        write!(request, "{name}: {value}\r\n").expect("append fixture header");
    }
    if !body.is_empty() {
        request.push_str("Content-Type: application/json\r\n");
    }
    write!(request, "Content-Length: {}\r\n\r\n", body.len()).expect("append HTTP content length");
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.write_all(&body))
        .expect("send authenticated request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read HTTP response");
    let response = String::from_utf8(response).expect("HTTP response is UTF-8");
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response separator");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("HTTP status code");
    HttpResponse {
        status,
        body: serde_json::from_str(body).expect("JSON response body"),
    }
}

struct FixturePaths {
    target_executable: PathBuf,
    target_cwd: PathBuf,
    dependency_executable: PathBuf,
    dependency_cwd: PathBuf,
}

impl FixturePaths {
    fn create(root: &Path) -> Self {
        let commands = root.join("commands");
        let target_cwd = root.join("target-cwd");
        let dependency_cwd = root.join("dependency-cwd");
        fs::create_dir_all(&commands).expect("create command directory");
        fs::create_dir_all(&target_cwd).expect("create target cwd");
        fs::create_dir_all(&dependency_cwd).expect("create dependency cwd");
        let target_executable = commands.join("target.exe");
        let dependency_executable = commands.join("dependency.exe");
        fs::write(&target_executable, b"fixture target").expect("write target command");
        fs::write(&dependency_executable, b"fixture dependency").expect("write dependency command");
        Self {
            target_executable,
            target_cwd,
            dependency_executable,
            dependency_cwd,
        }
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/startup-isolation-http")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::SeqCst)
            ));
        fs::create_dir_all(&path).expect("create HTTP fixture directory");
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
