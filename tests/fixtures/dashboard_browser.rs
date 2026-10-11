//! Disposable browser QA fixture. This never starts Supervisor or managed services.
//! Compile with rustc --edition=2024 tests/fixtures/dashboard_browser.rs -o target/dashboard-browser-fixture.exe.
//! Run with the path to a recorded /v1/services JSON snapshot. Stops after three minutes.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

fn respond(mut stream: TcpStream, services: &[u8], fail_services: &mut bool) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = [0_u8; 8192];
    let count = stream.read(&mut request).unwrap_or(0);
    let request = String::from_utf8_lossy(&request[..count]);
    let target = request.split_whitespace().nth(1).unwrap_or("");
    let (status, mime, body): (u16, &str, &[u8]) = match target {
        "/" => (
            200,
            "text/html; charset=utf-8",
            include_bytes!("../../dashboard/index.html"),
        ),
        "/app.js" => (
            200,
            "text/javascript; charset=utf-8",
            include_bytes!("../../dashboard/app.js"),
        ),
        "/app.css" => (
            200,
            "text/css; charset=utf-8",
            include_bytes!("../../dashboard/app.css"),
        ),
        "/tests/dashboard-browser.html" => (
            200,
            "text/html; charset=utf-8",
            include_bytes!("../dashboard-browser.html"),
        ),
        "/tests/dashboard-browser.js" => (
            200,
            "text/javascript; charset=utf-8",
            include_bytes!("../dashboard-browser.js"),
        ),
        "/v1/services" if *fail_services => (
            503,
            "application/json",
            br#"{"error":"fixture unavailable"}"#,
        ),
        "/v1/services" => (200, "application/json", services),
        "/v1/health" => (
            200,
            "application/json",
            br#"{"status":"ok","version":"browser-fixture"}"#,
        ),
        "/fixture/unavailable" => {
            *fail_services = true;
            (
                200,
                "text/plain",
                b"Fixture now returns 503; real services unchanged.",
            )
        }
        "/fixture/available" => {
            *fail_services = false;
            (
                200,
                "text/plain",
                b"Fixture restored; real services unchanged.",
            )
        }
        _ => (404, "text/plain", b"Not found"),
    };
    let reason = match status {
        200 => "OK",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
}

fn main() {
    let snapshot = std::env::args()
        .nth(1)
        .expect("pass recorded service JSON path");
    let services = std::fs::read(snapshot).expect("read fixture snapshot");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    listener.set_nonblocking(true).unwrap();
    println!("http://{}/", listener.local_addr().unwrap());
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut fail_services = false;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => respond(stream, &services, &mut fail_services),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}
