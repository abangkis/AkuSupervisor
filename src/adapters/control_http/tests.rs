use std::sync::Mutex;

use crate::adapters::runtime_instance::{RuntimeInstance, RuntimeMode};
use crate::adapters::service_logs::LiveLogEventKind;
use crate::application::{
    ControlError, ControlMutationOutcome, ControlMutationResult, CooperativeActionError,
    CooperativeActionOutcome, CooperativeActionStatus, HealthSnapshot, ProcessIdentity,
    ServiceSnapshot,
};
use crate::domain::{DesiredState, LifecycleState, OperatorHold};

use super::dashboard::is_loopback_host;
use super::*;

fn test_instance() -> RuntimeInstance {
    RuntimeInstance {
        schema_version: 1,
        instance_id: "test-instance".to_owned(),
        process_id: std::process::id(),
        executable: "aku-supervisor".into(),
        mode: RuntimeMode::Stable,
        watcher_process_id: None,
        started_at_unix_ms: 1,
        version: crate::VERSION.to_owned(),
        config_fingerprint: "sha256:test".to_owned(),
        control_api: "http://127.0.0.1:0".to_owned(),
    }
}

#[test]
fn client_retry_taxonomy_excludes_rejections() {
    assert!(ControlClientError::MalformedResponse.is_transient());
    assert!(
        ControlClientError::Read(io::Error::from(io::ErrorKind::ConnectionReset)).is_transient()
    );
    assert!(
        !ControlClientError::Rejected {
            status: 409,
            body: json!({"error": "conflict"}),
        }
        .is_transient()
    );
}

#[test]
fn deadline_request_cannot_wait_for_the_ordinary_io_timeout() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind slow server");
    let address = listener.local_addr().expect("slow server address");
    let worker = thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("accept deadline request");
        thread::sleep(Duration::from_millis(300));
    });
    let token_path = std::env::temp_dir().join(format!(
        "aku-supervisor-deadline-token-{}",
        std::process::id()
    ));
    std::fs::remove_file(&token_path).ok();
    let token =
        RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
    let started = Instant::now();
    let error = client_request_until(
        address,
        &token,
        "GET",
        "/v1/registry",
        None,
        started + Duration::from_millis(75),
    )
    .expect_err("slow response must exceed total deadline");
    assert!(matches!(
        error,
        ControlClientError::Read(_) | ControlClientError::DeadlineExceeded
    ));
    assert!(started.elapsed() < Duration::from_millis(250));
    worker.join().expect("slow server worker");
    std::fs::remove_file(token_path).ok();
}

#[derive(Debug, Default)]
struct FakeControl {
    mutations: Mutex<u32>,
    mutation_calls: Mutex<Vec<(ControlAction, String, Actor, String)>>,
    snapshots: Mutex<Vec<ServiceSnapshot>>,
}

impl SupervisorControl for FakeControl {
    fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
        Ok(self.snapshots.lock().expect("snapshot lock").clone())
    }

    fn mutate(
        &self,
        action: ControlAction,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ControlMutationResult, ControlError> {
        if service_id == "missing" {
            return Err(ControlError::service_not_found(service_id));
        }
        *self.mutations.lock().expect("mutation lock") += 1;
        self.mutation_calls.lock().expect("calls lock").push((
            action,
            service_id.to_owned(),
            actor,
            reason.as_str().to_owned(),
        ));
        Ok(ControlMutationResult::new(
            ControlMutationOutcome::Started,
            None,
        ))
    }
}

fn dashboard_service(
    id: &str,
    ports: Vec<u16>,
    root_pid: Option<u32>,
    owned_pids: Vec<u32>,
) -> ServiceSnapshot {
    ServiceSnapshot {
        id: id.to_owned(),
        label: id.to_owned(),
        depends_on: Vec::new(),
        ports,
        lifecycle: if root_pid.is_some() {
            LifecycleState::Running
        } else {
            LifecycleState::Stopped
        },
        root_pid,
        owned_processes: owned_pids
            .iter()
            .map(|pid| ProcessIdentity {
                pid: *pid,
                name: Some("fixture.exe".to_owned()),
            })
            .collect(),
        owned_pids,
        health: HealthSnapshot::healthy(true, Some(true), "fixture healthy".to_owned()),
        desired_state: DesiredState::Running,
        started_at_unix_ms: Some(123),
        last_exit_code: None,
        last_exit_at_unix_ms: None,
        restart_count: 0,
        operator_hold: OperatorHold::None,
        last_action: None,
    }
}

fn raw_request(
    address: SocketAddr,
    method: &str,
    target: &str,
    host: &str,
    origin: Option<&str>,
) -> (u16, HashMap<String, String>, Vec<u8>) {
    raw_request_with_body(address, method, target, host, origin, &[], &[])
}

fn raw_request_with_body(
    address: SocketAddr,
    method: &str,
    target: &str,
    host: &str,
    origin: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, HashMap<String, String>, Vec<u8>) {
    let mut stream = TcpStream::connect(address).expect("connect to fixture control server");
    write!(stream, "{method} {target} HTTP/1.1\r\nHost: {host}\r\n")
        .expect("write request line and Host");
    if let Some(origin) = origin {
        write!(stream, "Origin: {origin}\r\n").expect("write request Origin");
    }
    for (name, value) in extra_headers {
        write!(stream, "{name}: {value}\r\n").expect("write fixture header");
    }
    write!(stream, "Content-Length: {}\r\n", body.len()).expect("write body length");
    stream
        .write_all(b"Connection: close\r\n\r\n")
        .expect("finish raw request");
    stream.write_all(body).expect("write fixture body");
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("read complete HTTP response");
    let response = String::from_utf8_lossy(&bytes);
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("response headers and body");
    let mut lines = headers.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("HTTP status");
    let parsed_headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (status, parsed_headers, body.as_bytes().to_vec())
}

fn assert_dashboard_asset_response(
    address: SocketAddr,
    host: &str,
    target: &str,
    mime: &str,
    asset: &[u8],
) {
    let (status, headers, body) = raw_request(address, "GET", target, host, None);
    assert_eq!(status, 200, "{target}");
    assert_eq!(body, asset, "embedded bytes must match {target}");
    assert_eq!(headers.get("content-type").map(String::as_str), Some(mime));
    assert_eq!(
        headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );
    assert_eq!(
        headers.get("x-content-type-options").map(String::as_str),
        Some("nosniff")
    );
    assert_eq!(
        headers.get("referrer-policy").map(String::as_str),
        Some("no-referrer")
    );
    let csp = headers
        .get("content-security-policy")
        .expect("dashboard CSP");
    assert!(csp.contains("default-src 'none'"));
    assert!(csp.contains("script-src 'self'"));
    assert!(csp.contains("connect-src 'self'"));
    assert!(!String::from_utf8_lossy(&body).contains("Bearer "));
}

#[test]
#[allow(clippy::too_many_lines)]
fn embedded_dashboard_serves_assets_and_scoped_controls_without_exposing_primary_token() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("dashboard-http-fixtures")
        .join(std::process::id().to_string());
    std::fs::create_dir_all(&root).expect("create dashboard HTTP fixture");
    let token_path = root.join("control-token");
    let server_token = RuntimeToken::load_or_create(&token_path, || Ok("d".repeat(64)))
        .expect("create server token");
    let primary_token = server_token.clone();
    let control = Arc::new(FakeControl::default());
    *control.snapshots.lock().expect("snapshot lock") = vec![
        dashboard_service("running", vec![4321], Some(1234), vec![1234, 1235]),
        dashboard_service("stopped", Vec::new(), None, Vec::new()),
    ];
    let journal = Arc::new(
        FileJournal::open(root.join("journal.jsonl"), Vec::<String>::new())
            .expect("create dashboard test journal"),
    );
    let logs = Arc::new(ServiceLogStore::new(&root, std::iter::empty()));
    let mut server = ControlHttpServer::start(
        "127.0.0.1",
        0,
        server_token,
        McpConfig::default(),
        control.clone(),
        None,
        journal,
        logs,
        Arc::new(RegistryReconciliationStatus::new("sha256:test".to_owned())),
        Arc::new(test_instance()),
        Arc::new(SupervisorShutdown::default()),
    )
    .expect("start loopback fixture control server");
    let address = server.address();
    let host = format!("127.0.0.1:{}", address.port());

    for (target, mime, asset) in [
        (
            "/",
            "text/html; charset=utf-8",
            include_bytes!("../../../dashboard/index.html").as_slice(),
        ),
        (
            "/app.css",
            "text/css; charset=utf-8",
            include_bytes!("../../../dashboard/app.css").as_slice(),
        ),
        (
            "/app.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../../dashboard/app.js").as_slice(),
        ),
        (
            "/model.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../../dashboard/model.js").as_slice(),
        ),
        (
            "/view.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../../dashboard/view.js").as_slice(),
        ),
        (
            "/api.js",
            "text/javascript; charset=utf-8",
            include_bytes!("../../../dashboard/api.js").as_slice(),
        ),
    ] {
        assert_dashboard_asset_response(address, &host, target, mime, asset);
    }

    let (status, _, _) = raw_request(address, "GET", "/", "dashboard.example", None);
    assert_eq!(status, 404, "non-loopback Host is rejected for the UI");
    assert!(dashboard_request_is_loopback(address, Some(&host)));
    assert!(!dashboard_request_is_loopback(
        address,
        Some("dashboard.example")
    ));
    assert!(!dashboard_request_is_loopback(
        "192.0.2.8:1234".parse().expect("fixture peer address"),
        Some(&host)
    ));
    assert!(is_loopback_host("localhost:11121"));
    assert!(is_loopback_host("[::1]:11121"));
    assert!(!is_loopback_host("localhost.attacker.example:11121"));

    let (status, headers, _body) = raw_request(address, "POST", "/app.js", &host, None);
    assert_eq!(status, 405, "asset POST must be rejected");
    assert_eq!(
        headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);

    let (status, _, _) = raw_request(address, "GET", "/not-a-route", &host, None);
    assert_eq!(status, 404, "unknown paths remain not found");

    let (status, headers, body) = raw_request(address, "GET", "/v1/services", &host, None);
    assert_eq!(status, 200, "services API remains readable without a token");
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let services: Value = serde_json::from_slice(&body).expect("services API JSON");
    assert_eq!(services["services"][0]["rootPid"], 1234);
    assert_eq!(services["services"][0]["ownedPids"], json!([1234, 1235]));
    assert_eq!(services["services"][0]["ports"], json!([4321]));
    assert_eq!(services["services"][1]["rootPid"], Value::Null);
    assert_eq!(services["services"][1]["ports"], json!([]));

    let (status, _, body) = raw_request(address, "GET", "/v1/health", &host, None);
    assert_eq!(status, 200, "health API remains readable without a token");
    let health: Value = serde_json::from_slice(&body).expect("health API JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);

    let origin = format!("http://{host}");
    let session_path = "/dashboard/v1/session";
    let session_headers = [("X-Dashboard-Request", "1")];
    let (status, headers, body) = raw_request_with_body(
        address,
        "GET",
        session_path,
        &host,
        None,
        &session_headers,
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    assert!(!headers.contains_key("access-control-allow-origin"));
    let session: Value = serde_json::from_slice(&body).expect("session JSON");
    let scoped = session["token"].as_str().expect("dashboard token");
    assert_eq!(scoped.len(), 64);
    assert!(!primary_token.bearer_matches(scoped));
    assert_eq!(session["instanceId"], "test-instance");
    let mutation_headers = [
        ("Content-Type", "application/json"),
        ("X-Dashboard-Token", scoped),
    ];
    let payload = br#"{"requestId":"browser-1"}"#;
    let start_path = "/dashboard/v1/services/stopped/start";
    let stop_path = "/dashboard/v1/services/running/stop";

    // Browser security boundary: no ambient authorization, form POSTs, CORS,
    // DNS rebinding, wrong port, duplicate security headers or wider capabilities.
    for (method, path, test_host, test_origin, request_headers, request_body, expected) in [
        (
            "GET",
            session_path,
            host.as_str(),
            None,
            &[][..],
            &[][..],
            403,
        ),
        (
            "GET",
            session_path,
            "dashboard.example",
            None,
            &session_headers[..],
            &[][..],
            404,
        ),
        (
            "GET",
            session_path,
            "localhost:1",
            None,
            &session_headers[..],
            &[][..],
            404,
        ),
        (
            "GET",
            session_path,
            host.as_str(),
            Some("http://evil.example"),
            &session_headers[..],
            &[][..],
            403,
        ),
        (
            "POST",
            session_path,
            host.as_str(),
            Some(origin.as_str()),
            &session_headers[..],
            &[][..],
            405,
        ),
        (
            "GET",
            start_path,
            host.as_str(),
            Some(origin.as_str()),
            &mutation_headers[..],
            &payload[..],
            405,
        ),
        (
            "POST",
            start_path,
            host.as_str(),
            None,
            &mutation_headers[..],
            &payload[..],
            403,
        ),
        (
            "POST",
            start_path,
            host.as_str(),
            Some("http://localhost:1"),
            &mutation_headers[..],
            &payload[..],
            403,
        ),
        (
            "POST",
            start_path,
            host.as_str(),
            Some(origin.as_str()),
            &[][..],
            &payload[..],
            415,
        ),
        (
            "POST",
            "/dashboard/v1/services/running/restart",
            host.as_str(),
            None,
            &mutation_headers[..],
            &payload[..],
            403,
        ),
        (
            "POST",
            "/dashboard/v1/services/running/reload",
            host.as_str(),
            Some(origin.as_str()),
            &mutation_headers[..],
            &payload[..],
            404,
        ),
        (
            "POST",
            "/dashboard/v1/supervisor/shutdown",
            host.as_str(),
            Some(origin.as_str()),
            &mutation_headers[..],
            &payload[..],
            404,
        ),
        (
            "POST",
            "/dashboard/v1/registry",
            host.as_str(),
            Some(origin.as_str()),
            &mutation_headers[..],
            &payload[..],
            404,
        ),
        (
            "POST",
            start_path,
            host.as_str(),
            Some(origin.as_str()),
            &mutation_headers[..],
            &b"{}"[..],
            400,
        ),
        (
            "POST",
            start_path,
            host.as_str(),
            Some(origin.as_str()),
            &mutation_headers[..],
            &br#"{"requestId":"x","actor":"codex"}"#[..],
            400,
        ),
    ] {
        let (status, _, _) = raw_request_with_body(
            address,
            method,
            path,
            test_host,
            test_origin,
            request_headers,
            request_body,
        );
        assert_eq!(status, expected, "{method} {path} should be rejected");
    }
    for candidate in ["invalid", primary_token.expose_for_authorization_header()] {
        let headers = [
            ("Content-Type", "application/json"),
            ("X-Dashboard-Token", candidate),
        ];
        for path in [start_path, "/dashboard/v1/services/running/restart"] {
            assert_eq!(
                raw_request_with_body(
                    address,
                    "POST",
                    path,
                    &host,
                    Some(&origin),
                    &headers,
                    payload
                )
                .0,
                401
            );
        }
    }
    let duplicate_headers = [
        ("Content-Type", "application/json"),
        ("X-Dashboard-Token", scoped),
        ("X-Dashboard-Token", scoped),
    ];
    assert_eq!(
        raw_request_with_body(
            address,
            "POST",
            start_path,
            &host,
            Some(&origin),
            &duplicate_headers,
            payload
        )
        .0,
        400
    );
    let bearer = format!("Bearer {scoped}");
    assert_eq!(
        raw_request_with_body(
            address,
            "POST",
            "/v1/services/stopped/start",
            &host,
            Some(&origin),
            &[("Authorization", &bearer)],
            payload
        )
        .0,
        401
    );
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);

    for _ in 0..2 {
        let (status, _, body) = raw_request_with_body(
            address,
            "POST",
            start_path,
            &host,
            Some(&origin),
            &mutation_headers,
            payload,
        );
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["serviceId"],
            "stopped"
        );
    }
    assert_eq!(
        *control.mutations.lock().expect("mutation lock"),
        1,
        "replayed Start executes once"
    );
    assert_eq!(
        raw_request_with_body(
            address,
            "POST",
            stop_path,
            &host,
            Some(&origin),
            &mutation_headers,
            payload
        )
        .0,
        409,
        "same request ID with different operation conflicts"
    );
    assert_eq!(
        raw_request_with_body(
            address,
            "POST",
            stop_path,
            &host,
            Some(&origin),
            &mutation_headers,
            br#"{"requestId":"browser-2"}"#
        )
        .0,
        200
    );
    for _ in 0..2 {
        assert_eq!(
            raw_request_with_body(
                address,
                "POST",
                "/dashboard/v1/services/running/restart",
                &host,
                Some(&origin),
                &mutation_headers,
                br#"{"requestId":"browser-restart"}"#
            )
            .0,
            200
        );
    }
    let calls = control.mutation_calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 3, "replayed Restart executes once");
    assert_eq!(
        calls[0],
        (
            ControlAction::Start,
            "stopped".to_owned(),
            Actor::UserUi,
            "Dashboard service control".to_owned()
        )
    );
    assert_eq!(calls[1].0, ControlAction::Stop);
    assert_eq!(calls[1].2, Actor::UserUi);
    assert_eq!(calls[2].0, ControlAction::Restart);
    assert_eq!(calls[2].1, "running");
    assert_eq!(calls[2].2, Actor::UserUi);
    drop(calls);
    assert_eq!(
        raw_request_with_body(
            address,
            "POST",
            "/dashboard/v1/services/missing/start",
            &host,
            Some(&origin),
            &mutation_headers,
            br#"{"requestId":"missing-service"}"#
        )
        .0,
        404,
        "core errors propagate to the dashboard"
    );
    let request = HttpRequest {
        method: "POST".to_owned(),
        target: start_path.to_owned(),
        host: Some(host.clone()),
        authorization: None,
        accept: None,
        content_type: Some("application/json".to_owned()),
        origin: Some(origin),
        dashboard_request: None,
        dashboard_token: Some(scoped.to_owned()),
        mcp_protocol_version: None,
        body: br#"{"requestId":"stale-session"}"#.to_vec(),
    };
    let mut next_instance = test_instance();
    next_instance.instance_id = "next-instance".to_owned();
    let shutdown = SupervisorShutdown::default();
    let mut idempotency = IdempotencyStore::default();
    assert_eq!(
        route_dashboard_control(
            &request,
            address,
            address,
            &primary_token,
            control.as_ref(),
            &next_instance,
            &shutdown,
            &mut idempotency
        )
        .status,
        401,
        "old session is revoked on a new instance"
    );
    assert_eq!(
        route_dashboard_control(
            &request,
            "192.0.2.8:1234".parse().unwrap(),
            address,
            &primary_token,
            control.as_ref(),
            &test_instance(),
            &shutdown,
            &mut idempotency
        )
        .status,
        404,
        "non-loopback peer cannot mutate"
    );
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 3);
    server.shutdown().expect("stop fixture control server");
}

#[test]
fn live_log_connection_does_not_block_other_control_requests() {
    use crate::application::{CapturedLogStream, ServiceLogSink};

    let root =
        std::env::temp_dir().join(format!("aku-supervisor-live-http-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create live HTTP fixture");
    let token_path = root.join("control-token");
    std::fs::remove_file(&token_path).ok();
    let server_token =
        RuntimeToken::load_or_create(&token_path, || Ok("c".repeat(64))).expect("server token");
    let client_token = RuntimeToken::load(&token_path).expect("client token");
    let journal = Arc::new(
        FileJournal::open(root.join("journal.jsonl"), Vec::<String>::new()).expect("test journal"),
    );
    let logs = Arc::new(ServiceLogStore::new(&root, ["api".to_owned()]));
    let reconciliation = Arc::new(RegistryReconciliationStatus::new("sha256:test".to_owned()));
    let mut server = ControlHttpServer::start(
        "127.0.0.1",
        0,
        server_token,
        McpConfig::default(),
        Arc::new(FakeControl::default()),
        None,
        journal,
        Arc::clone(&logs),
        reconciliation,
        Arc::new(test_instance()),
        Arc::new(SupervisorShutdown::default()),
    )
    .expect("start control server");

    let mut live = client_live_logs(
        server.address(),
        &client_token,
        "/v1/services/api/logs/live?stream=both&tail=0",
    )
    .expect("open live logs");
    let status = client_request(server.address(), &client_token, "GET", "/v1/services", None)
        .expect("status while live stream is open");
    assert_eq!(status["services"], json!([]));

    logs.publish("api", CapturedLogStream::Stderr, b"failure detail\n");
    let event = live
        .next_event()
        .expect("read event")
        .expect("event exists");
    assert_eq!(event.stream, Some(LogStream::Stderr));
    assert_eq!(event.text.as_deref(), Some("failure detail"));

    let mut reset = client_live_logs(
        server.address(),
        &client_token,
        "/v1/services/api/logs/live?stream=both&tail=1&afterHub=previous-hub&after=999",
    )
    .expect("resume with foreign hub cursor");
    let reset_event = reset
        .next_event()
        .expect("read reset event")
        .expect("reset event exists");
    let replayed = reset
        .next_event()
        .expect("read replayed event")
        .expect("replayed event exists");
    assert_eq!(reset_event.kind, LiveLogEventKind::HubReset);
    assert_eq!(
        reset_event.previous_hub_instance_id.as_deref(),
        Some("previous-hub")
    );
    assert_eq!(replayed.text.as_deref(), Some("failure detail"));

    drop(reset);
    drop(live);
    server.shutdown().expect("stop control server");
    std::fs::remove_dir_all(root).ok();
}

#[derive(Debug, Default)]
struct FakeCooperativeControl {
    reloads: Mutex<u32>,
}

impl CooperativeActionControl for FakeCooperativeControl {
    fn target(&self) -> &'static str {
        "fixture-extension"
    }

    fn execute(
        &self,
        _actor: Actor,
        _reason: Reason,
        request_id: &str,
        _progress: &(dyn Fn(crate::application::CooperativeActionProgress) + Send + Sync),
    ) -> Result<CooperativeActionOutcome, CooperativeActionError> {
        *self.reloads.lock().expect("reload lock") += 1;
        Ok(CooperativeActionOutcome {
            target: "fixture-extension".to_owned(),
            action: "reload_self".to_owned(),
            status: CooperativeActionStatus::Completed,
            relay_action_id: Some(request_id.to_owned()),
            previous_build_id: Some("old".to_owned()),
            expected_build_id: Some("new".to_owned()),
            observed_build_id: Some("new".to_owned()),
            message: "completed".to_owned(),
        })
    }
}

#[test]
fn mutation_target_accepts_only_registered_action_shape() {
    assert_eq!(
        parse_mutation_target("/v1/services/api/restart"),
        Some(("api", ControlAction::Restart))
    );
    assert!(parse_mutation_target("/v1/services/api/shell").is_none());
    assert!(parse_mutation_target("/v1/services/api/restart/extra").is_none());
}

#[test]
fn live_log_target_defaults_to_merged_stream_and_bounded_tail() {
    assert_eq!(
        parse_live_logs_target("/v1/services/api/logs/live?stream=both&tail=50"),
        Some(("api", LiveLogSelection::Both, 50, None))
    );
    assert_eq!(
        parse_live_logs_target(
            "/v1/services/api/logs/live?stream=stderr&tail=5000&afterHub=123-old&after=42"
        ),
        Some((
            "api",
            LiveLogSelection::Stderr,
            1_000,
            Some(LiveLogCursor {
                hub_instance_id: "123-old".to_owned(),
                sequence: 42,
            })
        ))
    );
    assert!(
        parse_live_logs_target("/v1/services/api/logs/live?stream=stderr&tail=50&after=42")
            .is_none()
    );
    assert!(
        parse_live_logs_target("/v1/services/api/logs/live?stream=stderr&tail=50&afterHub=123-old")
            .is_none()
    );
    assert!(parse_live_logs_target("/v1/services/api/logs/live?stream=invalid&tail=50").is_none());
}

#[test]
fn unauthorized_mutation_never_reaches_shared_control() {
    let token_path =
        std::env::temp_dir().join(format!("aku-supervisor-http-token-{}", std::process::id()));
    std::fs::remove_file(&token_path).ok();
    let token =
        RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
    let journal_path = token_path.with_extension("jsonl");
    std::fs::remove_file(&journal_path).ok();
    let journal =
        FileJournal::open(&journal_path, Vec::<String>::new()).expect("create test journal");
    let logs = ServiceLogStore::new(&std::env::temp_dir(), ["api".to_owned()]);
    let control = FakeControl::default();
    let reconciliation = RegistryReconciliationStatus::new("sha256:test".to_owned());
    let request = HttpRequest {
        method: "POST".to_owned(),
        target: "/v1/services/api/start".to_owned(),
        host: None,
        authorization: Some(format!("Bearer {}", "b".repeat(64))),
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: br#"{"actor":"codex","reason":"source changed"}"#.to_vec(),
    };

    let response = route(
        &request,
        &token,
        &McpConfig::default(),
        &control,
        None,
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut IdempotencyStore::default(),
    );

    assert_eq!(response.status, 401);
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
    std::fs::remove_file(token_path).ok();
    std::fs::remove_file(journal_path).ok();
}

#[test]
fn supervisor_identity_and_shutdown_require_authentication_and_are_idempotent() {
    let token_path = std::env::temp_dir().join(format!(
        "aku-supervisor-shutdown-token-{}",
        std::process::id()
    ));
    std::fs::remove_file(&token_path).ok();
    let token =
        RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
    let journal_path = token_path.with_extension("jsonl");
    std::fs::remove_file(&journal_path).ok();
    let journal =
        FileJournal::open(&journal_path, Vec::<String>::new()).expect("create test journal");
    let logs = ServiceLogStore::new(&std::env::temp_dir(), ["api".to_owned()]);
    let control = FakeControl::default();
    let reconciliation = RegistryReconciliationStatus::new("sha256:test".to_owned());
    let instance = test_instance();
    let shutdown = SupervisorShutdown::default();
    let mut idempotency = IdempotencyStore::default();
    let request = |authorization: Option<String>, request_id: &str| HttpRequest {
        method: "POST".to_owned(),
        target: "/v1/supervisor/shutdown".to_owned(),
        host: None,
        authorization,
        accept: None,
        content_type: Some("application/json".to_owned()),
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: serde_json::to_vec(&json!({
            "actor": "codex",
            "reason": "replace orphaned Supervisor",
            "requestId": request_id
        }))
        .expect("serialize shutdown request"),
    };
    let route_request = |request: &HttpRequest,
                         shutdown: &SupervisorShutdown,
                         idempotency: &mut IdempotencyStore| {
        route(
            request,
            &token,
            &McpConfig::default(),
            &control,
            None,
            &journal,
            &logs,
            &reconciliation,
            &instance,
            shutdown,
            idempotency,
        )
    };

    let identity = HttpRequest {
        method: "GET".to_owned(),
        target: "/v1/instance".to_owned(),
        host: None,
        authorization: Some(format!("Bearer {}", "a".repeat(64))),
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: Vec::new(),
    };
    let identity_response = route_request(&identity, &shutdown, &mut idempotency);
    assert_eq!(identity_response.status, 200);
    let identity_payload: Value =
        serde_json::from_slice(&identity_response.body).expect("identity JSON");
    assert_eq!(identity_payload["instance"]["instanceId"], "test-instance");

    let unauthorized = route_request(&request(None, "shutdown-1"), &shutdown, &mut idempotency);
    assert_eq!(unauthorized.status, 401);
    assert!(shutdown.accepted().expect("read shutdown").is_none());

    let authorized_header = Some(format!("Bearer {}", "a".repeat(64)));
    let accepted = route_request(
        &request(authorized_header.clone(), "shutdown-1"),
        &shutdown,
        &mut idempotency,
    );
    assert_eq!(accepted.status, 202);
    let replayed = route_request(
        &request(authorized_header.clone(), "shutdown-1"),
        &shutdown,
        &mut idempotency,
    );
    let replayed_payload: Value =
        serde_json::from_slice(&replayed.body).expect("replayed shutdown JSON");
    assert_eq!(replayed_payload["status"], "already_accepted");
    let conflict = route_request(
        &request(authorized_header, "shutdown-2"),
        &shutdown,
        &mut idempotency,
    );
    assert_eq!(conflict.status, 409);

    std::fs::remove_file(token_path).ok();
    std::fs::remove_file(journal_path).ok();
}

#[test]
fn events_query_is_bounded_and_rejects_unknown_fields() {
    assert_eq!(parse_events_target("/v1/events"), Some((0, 50)));
    assert_eq!(
        parse_events_target("/v1/events?after=7&limit=999"),
        Some((7, 200))
    );
    assert!(parse_events_target("/v1/events?cursor=7").is_none());
}

#[test]
fn registry_revision_status_requires_authentication() {
    let token_path = std::env::temp_dir().join(format!(
        "aku-supervisor-registry-token-{}",
        std::process::id()
    ));
    let journal_path = token_path.with_extension("jsonl");
    std::fs::remove_file(&token_path).ok();
    std::fs::remove_file(&journal_path).ok();
    let token =
        RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
    let journal = FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
    let logs = ServiceLogStore::new(&std::env::temp_dir(), std::iter::empty());
    let control = FakeControl::default();
    let reconciliation = RegistryReconciliationStatus::new("sha256:active".to_owned());
    let request = |authorization| HttpRequest {
        method: "GET".to_owned(),
        target: "/v1/registry".to_owned(),
        host: None,
        authorization,
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: Vec::new(),
    };

    let unauthorized = route(
        &request(None),
        &token,
        &McpConfig::default(),
        &control,
        None,
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut IdempotencyStore::default(),
    );
    assert_eq!(unauthorized.status, 401);

    let authorized = route(
        &request(Some(format!("Bearer {}", "a".repeat(64)))),
        &token,
        &McpConfig::default(),
        &control,
        None,
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut IdempotencyStore::default(),
    );
    assert_eq!(authorized.status, 200);
    let payload: Value = serde_json::from_slice(&authorized.body).expect("registry JSON");
    assert_eq!(payload["registry"]["state"], "current");
    assert_eq!(payload["registry"]["activeRevision"], "sha256:active");
    std::fs::remove_file(token_path).ok();
    std::fs::remove_file(journal_path).ok();
}

#[test]
#[allow(clippy::too_many_lines)]
fn authenticated_bridge_reload_uses_the_narrow_cooperative_boundary() {
    let token_path = std::env::temp_dir().join(format!(
        "aku-supervisor-bridge-token-{}",
        std::process::id()
    ));
    let journal_path = token_path.with_extension("jsonl");
    std::fs::remove_file(&token_path).ok();
    std::fs::remove_file(&journal_path).ok();
    let token =
        RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
    let journal = FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
    let logs = ServiceLogStore::new(&std::env::temp_dir(), ["api".to_owned()]);
    let control = FakeControl::default();
    let reconciliation = RegistryReconciliationStatus::new("sha256:test".to_owned());
    let cooperative = Arc::new(FakeCooperativeControl::default());
    let manager = CooperativeOperationManager::new(cooperative.clone());
    let request = HttpRequest {
        method: "POST".to_owned(),
        target: "/v1/cooperative-actions/chrome-extension/reload-self".to_owned(),
        host: None,
        authorization: Some(format!("Bearer {}", "a".repeat(64))),
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: br#"{"actor":"codex","reason":"load build","requestId":"bridge-1"}"#.to_vec(),
    };

    let response = route(
        &request,
        &token,
        &McpConfig::default(),
        &control,
        Some(&manager),
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut IdempotencyStore::default(),
    );

    assert!(matches!(response.status, 200 | 202));
    for _ in 0..100 {
        if *cooperative.reloads.lock().expect("reload lock") == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*cooperative.reloads.lock().expect("reload lock"), 1);
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
    let status_request = HttpRequest {
        method: "GET".to_owned(),
        target: "/v1/cooperative-actions/chrome-extension/requests/bridge-1".to_owned(),
        host: None,
        authorization: Some(format!("Bearer {}", "a".repeat(64))),
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: Vec::new(),
    };
    let status = route(
        &status_request,
        &token,
        &McpConfig::default(),
        &control,
        Some(&manager),
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut IdempotencyStore::default(),
    );
    assert_eq!(status.status, 200);
    let payload: Value = serde_json::from_slice(&status.body).expect("status JSON");
    assert_eq!(payload["operation"]["status"], "completed");
    assert_eq!(
        payload["operation"]["actor"],
        json!({"actorType": "agent", "actorId": "codex"})
    );
    let active_request = HttpRequest {
        method: "GET".to_owned(),
        target: "/v1/cooperative-actions/chrome-extension/active".to_owned(),
        host: None,
        authorization: Some(format!("Bearer {}", "a".repeat(64))),
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: Vec::new(),
    };
    let active = route(
        &active_request,
        &token,
        &McpConfig::default(),
        &control,
        Some(&manager),
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut IdempotencyStore::default(),
    );
    assert_eq!(active.status, 200);
    let active_payload: Value = serde_json::from_slice(&active.body).expect("active JSON");
    assert!(active_payload["operation"].is_null());
    std::fs::remove_file(token_path).ok();
    std::fs::remove_file(journal_path).ok();
}

#[test]
fn identical_request_id_replays_without_a_second_mutation() {
    let token_path = std::env::temp_dir().join(format!(
        "aku-supervisor-idempotency-token-{}",
        std::process::id()
    ));
    let journal_path = token_path.with_extension("jsonl");
    std::fs::remove_file(&token_path).ok();
    std::fs::remove_file(&journal_path).ok();
    let token =
        RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
    let journal = FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
    let logs = ServiceLogStore::new(&std::env::temp_dir(), ["api".to_owned()]);
    let control = FakeControl::default();
    let reconciliation = RegistryReconciliationStatus::new("sha256:test".to_owned());
    let request = HttpRequest {
        method: "POST".to_owned(),
        target: "/v1/services/api/start".to_owned(),
        host: None,
        authorization: Some(format!("Bearer {}", "a".repeat(64))),
        accept: None,
        content_type: None,
        origin: None,
        dashboard_request: None,
        dashboard_token: None,
        mcp_protocol_version: None,
        body: br#"{"actor":"codex","reason":"source changed","requestId":"same-1"}"#.to_vec(),
    };
    let mut idempotency = IdempotencyStore::default();

    let first = route(
        &request,
        &token,
        &McpConfig::default(),
        &control,
        None,
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut idempotency,
    );
    let second = route(
        &request,
        &token,
        &McpConfig::default(),
        &control,
        None,
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut idempotency,
    );
    let conflicting = HttpRequest {
        body: br#"{"actor":"codex","reason":"different","requestId":"same-1"}"#.to_vec(),
        ..request
    };
    let conflict = route(
        &conflicting,
        &token,
        &McpConfig::default(),
        &control,
        None,
        &journal,
        &logs,
        &reconciliation,
        &test_instance(),
        &SupervisorShutdown::default(),
        &mut idempotency,
    );

    assert_eq!(first.status, 200);
    assert_eq!(second.body, first.body);
    assert_eq!(conflict.status, 409);
    assert_eq!(*control.mutations.lock().expect("mutation lock"), 1);
    std::fs::remove_file(token_path).ok();
    std::fs::remove_file(journal_path).ok();
}
