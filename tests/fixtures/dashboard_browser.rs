//! Disposable mock server for dashboard browser QA. It never starts Supervisor,
//! managed services, or reads real configuration. All service state is in memory.
//! Compile with rustc --edition=2024 tests/fixtures/dashboard_browser.rs -o target/dashboard-browser-fixture.exe.
//! Run without arguments; stop after ten minutes or close the process.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

#[path = "../../src/adapters/dashboard_assets.rs"]
mod dashboard_assets;

const SESSION_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Clone)]
struct Service {
    id: String,
    label: String,
    lifecycle: String,
    health: String,
    owned_pids: Vec<u32>,
}

struct Fixture {
    services: Vec<Service>,
    services_unavailable: bool,
    session_unavailable: bool,
    fail_next_mutation: bool,
    fail_next_restart_stopped: bool,
    expire_next_mutation: bool,
    delay_next_mutation_ms: u64,
    delay_services_ms: u64,
}

impl Fixture {
    fn reset() -> Self {
        Self {
            services: mock_services(),
            services_unavailable: false,
            session_unavailable: false,
            fail_next_mutation: false,
            fail_next_restart_stopped: false,
            expire_next_mutation: false,
            delay_next_mutation_ms: 0,
            delay_services_ms: 0,
        }
    }
}

fn mock_service(id: &str, label: &str, lifecycle: &str, pids: &[u32], health: &str) -> Service {
    Service {
        id: id.to_owned(),
        label: label.to_owned(),
        lifecycle: lifecycle.to_owned(),
        health: health.to_owned(),
        owned_pids: pids.to_vec(),
    }
}

fn mock_services() -> Vec<Service> {
    vec![
        mock_service(
            "fixture-stopped",
            "Fixture stopped",
            "stopped",
            &[],
            "unknown",
        ),
        mock_service(
            "fixture-running",
            "Fixture running",
            "running",
            &[91002],
            "healthy",
        ),
        mock_service(
            "fixture-failed",
            "Fixture failed",
            "failed",
            &[],
            "unhealthy",
        ),
        mock_service(
            "fixture-failed-owned",
            "Fixture failed with owned process",
            "failed",
            &[91004],
            "unhealthy",
        ),
        mock_service(
            "fixture-starting",
            "Fixture starting",
            "starting",
            &[],
            "unknown",
        ),
        mock_service(
            "fixture-stopping",
            "Fixture stopping",
            "stopping",
            &[91006],
            "unknown",
        ),
        mock_service(
            "fixture-termination-pending",
            "Fixture termination pending",
            "termination_pending",
            &[91007],
            "unknown",
        ),
        mock_service(
            "fixture-unknown",
            "Fixture unknown state",
            "unknown",
            &[],
            "unknown",
        ),
    ]
}

fn json_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn services_json(services: &[Service]) -> String {
    let entries = services
        .iter()
        .map(|service| {
            let id = json_string(&service.id);
            let label = json_string(&service.label);
            let lifecycle = json_string(&service.lifecycle);
            let health = json_string(&service.health);
            let root_pid = service
                .owned_pids
                .first()
                .map_or_else(|| "null".to_owned(), u32::to_string);
            let pids = service
                .owned_pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let processes = service
                .owned_pids
                .iter()
                .map(|pid| format!(r#"{{"pid":{pid},"name":"fixture-worker"}} "#))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                r#"{{"id":"{id}","label":"{label}","lifecycle":"{lifecycle}","desiredState":"{lifecycle}","health":{{"status":"{health}"}},"rootPid":{root_pid},"ownedPids":[{pids}],"ownedProcesses":[{processes}],"ports":[],"operatorHold":"none"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"services":[{entries}]}}"#)
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String, String)> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut bytes = Vec::new();
    let mut header_end = None;
    let mut content_length = 0_usize;
    loop {
        if let Some(end) = header_end {
            if bytes.len() >= end + content_length {
                break;
            }
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if header_end.is_none()
            && let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = end + 4;
            let header = String::from_utf8_lossy(&bytes[..end]);
            content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            header_end = Some(end);
        }
        if bytes.len() > 64 * 1024 {
            return None;
        }
    }
    let end = header_end?;
    let request = String::from_utf8_lossy(&bytes);
    let mut lines = request.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let header = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let body = String::from_utf8_lossy(&bytes[end..]).into_owned();
    Some((method, target, header, body))
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn write_response(stream: &mut TcpStream, status: u16, mime: &str, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Fixture Response",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body);
}

fn plain(stream: &mut TcpStream, message: &str) {
    write_response(stream, 200, "text/plain; charset=utf-8", message.as_bytes());
}

fn fixture_control(target: &str, fixture: &mut Fixture, stream: &mut TcpStream) -> bool {
    let path = target.split('?').next().unwrap_or(target);
    let query = target.split_once('?').map_or("", |(_, value)| value);
    match path {
        "/fixture/reset" => {
            *fixture = Fixture::reset();
            plain(
                stream,
                "Fixture reset. Mock services and session restored; no real services were changed.",
            );
        }
        "/fixture/unavailable" => {
            fixture.services_unavailable = true;
            plain(
                stream,
                "Service snapshots now return 503. No real services were changed.",
            );
        }
        "/fixture/available" => {
            fixture.services_unavailable = false;
            plain(stream, "Service snapshots are available again.");
        }
        "/fixture/session-unavailable" => {
            fixture.session_unavailable = true;
            plain(stream, "Dashboard session requests now return 503.");
        }
        "/fixture/session-available" => {
            fixture.session_unavailable = false;
            plain(stream, "Dashboard sessions are available again.");
        }
        "/fixture/fail-next-mutation" => {
            fixture.fail_next_mutation = true;
            plain(
                stream,
                "The next mock Start, Stop, or Restart request will return a simulated error.",
            );
        }
        "/fixture/fail-next-restart-stopped" => {
            fixture.fail_next_restart_stopped = true;
            plain(
                stream,
                "The next mock Restart will leave its service stopped and return a simulated error. No real services are involved.",
            );
        }
        "/fixture/expire-next-mutation" => {
            fixture.expire_next_mutation = true;
            plain(
                stream,
                "The next mock Start, Stop, or Restart request will return 401.",
            );
        }
        "/fixture/delay-next-mutation" => {
            fixture.delay_next_mutation_ms = query_milliseconds(query, 30_000);
            plain(
                stream,
                "The next mock Start, Stop, or Restart response is delayed. No real services are involved.",
            );
        }
        "/fixture/delay-services" => {
            fixture.delay_services_ms = query_milliseconds(query, 30_000);
            plain(
                stream,
                "Service snapshot delay updated. No real services are involved.",
            );
        }
        _ => return false,
    }
    true
}

fn query_milliseconds(query: &str, maximum: u64) -> u64 {
    query
        .split('&')
        .find_map(|part| part.strip_prefix("ms="))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .min(maximum)
}

fn mutation_route(path: &str) -> Option<(String, String)> {
    let tail = path.strip_prefix("/dashboard/v1/services/")?;
    let (id, action) = tail.rsplit_once('/')?;
    (matches!(action, "start" | "stop" | "restart") && !id.is_empty())
        .then(|| (id.to_owned(), action.to_owned()))
}

fn respond(mut stream: TcpStream, fixture: &mut Fixture) {
    let Some((method, target, headers, body)) = read_request(&mut stream) else {
        return;
    };
    let path = target.split('?').next().unwrap_or(&target);
    if method == "GET" && fixture_control(&target, fixture, &mut stream) {
        return;
    }
    if method == "GET" {
        if let Some(asset) = dashboard_assets::dashboard_asset(path) {
            write_response(&mut stream, 200, asset.content_type, asset.body);
            return;
        }
    }
    let (status, mime, response): (u16, &str, Vec<u8>) = match (method.as_str(), path) {
        ("GET", "/tests/dashboard-browser.html") => (
            200,
            "text/html; charset=utf-8",
            include_bytes!("../dashboard-browser.html").to_vec(),
        ),
        ("GET", "/tests/dashboard-browser.js") => (
            200,
            "text/javascript; charset=utf-8",
            include_bytes!("../dashboard-browser.js").to_vec(),
        ),
        ("GET", "/dashboard/v1/session")
            if header_value(&headers, "X-Dashboard-Request") != Some("1") =>
        {
            (400, "application/json", br#"{"error":{"code":"missing_header","message":"Dashboard session request header is required."}}"#.to_vec())
        }
        ("GET", "/dashboard/v1/session") if fixture.session_unavailable => (
            503,
            "application/json",
            br#"{"error":{"code":"fixture_session_unavailable","message":"Mock dashboard session is unavailable."}}"#.to_vec(),
        ),
        ("GET", "/dashboard/v1/session") => (
            200,
            "application/json",
            format!(
                r#"{{"token":"{SESSION_TOKEN}","instanceId":"dashboard-browser-fixture"}}"#
            )
            .into_bytes(),
        ),
        ("GET", "/v1/services") if fixture.services_unavailable => (
            503,
            "application/json",
            br#"{"error":{"code":"fixture_unavailable","message":"Mock service snapshot is unavailable."}}"#.to_vec(),
        ),
        ("GET", "/v1/services") => {
            if fixture.delay_services_ms > 0 {
                thread::sleep(Duration::from_millis(fixture.delay_services_ms));
            }
            (200, "application/json", services_json(&fixture.services).into_bytes())
        }
        ("GET", "/v1/health") => (
            200,
            "application/json",
            br#"{"status":"ok","version":"dashboard-browser-fixture"}"#.to_vec(),
        ),
        ("POST", _) if path.starts_with("/dashboard/v1/services/") => {
            if header_value(&headers, "X-Dashboard-Token") != Some(SESSION_TOKEN) {
                (401, "application/json", br#"{"error":{"code":"invalid_session","message":"Mock dashboard session is invalid."}}"#.to_vec())
            } else if !body.contains("\"requestId\"") {
                (400, "application/json", br#"{"error":{"code":"missing_request_id","message":"A requestId is required."}}"#.to_vec())
            } else if fixture.expire_next_mutation {
                fixture.expire_next_mutation = false;
                (401, "application/json", br#"{"error":{"code":"expired_session","message":"Mock dashboard session expired."}}"#.to_vec())
            } else if fixture.fail_next_mutation {
                fixture.fail_next_mutation = false;
                (503, "application/json", br#"{"error":{"code":"fixture_mutation_failure","message":"Simulated mock mutation failure."}}"#.to_vec())
            } else if let Some((service_id, action)) = mutation_route(path) {
                let fail_restart_stopped = action == "restart" && fixture.fail_next_restart_stopped;
                if fail_restart_stopped {
                    fixture.fail_next_restart_stopped = false;
                }
                if fixture.delay_next_mutation_ms > 0 {
                    thread::sleep(Duration::from_millis(fixture.delay_next_mutation_ms));
                    fixture.delay_next_mutation_ms = 0;
                }
                match fixture.services.iter_mut().find(|service| service.id == service_id) {
                    None => (404, "application/json", br#"{"error":{"code":"service_not_found","message":"Mock service was not found."}}"#.to_vec()),
                    Some(service) if action == "start" && service.owned_pids.is_empty()
                        && matches!(service.lifecycle.as_str(), "stopped" | "failed") =>
                    {
                        service.lifecycle = "running".to_owned();
                        service.health = "healthy".to_owned();
                        service.owned_pids = vec![91099];
                        let response = format!(
                            r#"{{"serviceId":"{}","outcome":"started"}}"#,
                            json_string(&service.id)
                        );
                        (200, "application/json", response.into_bytes())
                    }
                    Some(service) if action == "stop" && !service.owned_pids.is_empty()
                        && matches!(service.lifecycle.as_str(), "running" | "unhealthy" | "failed") =>
                    {
                        service.lifecycle = "stopped".to_owned();
                        service.health = "unknown".to_owned();
                        service.owned_pids.clear();
                        let response = format!(
                            r#"{{"serviceId":"{}","outcome":"stopped"}}"#,
                            json_string(&service.id)
                        );
                        (200, "application/json", response.into_bytes())
                    }
                    Some(service) if action == "restart" && service.lifecycle == "running"
                        && !service.owned_pids.is_empty() && fail_restart_stopped =>
                    {
                        service.lifecycle = "stopped".to_owned();
                        service.health = "unknown".to_owned();
                        service.owned_pids.clear();
                        (503, "application/json", br#"{"error":{"code":"fixture_restart_failure","message":"Simulated restart failure; the mock service stopped."}}"#.to_vec())
                    }
                    Some(service) if action == "restart" && service.lifecycle == "running"
                        && !service.owned_pids.is_empty() =>
                    {
                        service.health = "healthy".to_owned();
                        service.owned_pids = vec![91098];
                        let response = format!(
                            r#"{{"serviceId":"{}","outcome":"restarted"}}"#,
                            json_string(&service.id)
                        );
                        (200, "application/json", response.into_bytes())
                    }
                    Some(_) => (409, "application/json", br#"{"error":{"code":"invalid_fixture_transition","message":"The mock service cannot take that action in its current state."}}"#.to_vec()),
                }
            } else {
                (404, "application/json", br#"{"error":{"code":"invalid_route","message":"Mock mutation route was not found."}}"#.to_vec())
            }
        }
        _ => (404, "text/plain; charset=utf-8", b"Not found".to_vec()),
    };
    write_response(&mut stream, status, mime, &response);
}

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock fixture");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");
    println!(
        "http://{}/",
        listener.local_addr().expect("fixture address")
    );
    println!(
        "Fixture controls: /fixture/reset, /fixture/unavailable, /fixture/available, /fixture/fail-next-mutation, /fixture/fail-next-restart-stopped, /fixture/delay-next-mutation?ms=10000"
    );
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut fixture = Fixture::reset();
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => respond(stream, &mut fixture),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}
