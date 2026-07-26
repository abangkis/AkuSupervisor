use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};

#[cfg(test)]
use crate::adapters::http_response::decode_chunked_body;
use crate::adapters::http_response::{HttpResponse, parse_response};

use crate::application::{HealthCheckSpec, HealthProbe, JsonPathMode, TransportHealth};

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Small loopback-only TCP and HTTP/1.1 health adapter with no OS-specific behavior.
#[derive(Debug, Default)]
pub struct LoopbackTransportHealthProbe;

impl HealthProbe for LoopbackTransportHealthProbe {
    fn probe(&self, check: &HealthCheckSpec, timeout: std::time::Duration) -> TransportHealth {
        match check {
            HealthCheckSpec::Process => TransportHealth {
                transport_ready: true,
                healthy: true,
                detail: "owned process tree is ready".to_owned(),
                observed: BTreeMap::new(),
            },
            HealthCheckSpec::TcpConnect { host, port, .. } => {
                match connect_tcp(host, *port, timeout) {
                    Ok(()) => TransportHealth {
                        transport_ready: true,
                        healthy: true,
                        detail: format!("TCP connect to {host}:{port} succeeded"),
                        observed: BTreeMap::new(),
                    },
                    Err(detail) => failed_transport(detail),
                }
            }
            HealthCheckSpec::HttpStatus {
                url,
                expected_status,
                ..
            } => match request(url, timeout, "*/*") {
                Ok(response) => TransportHealth {
                    transport_ready: true,
                    healthy: response.status == *expected_status,
                    detail: if response.status == *expected_status {
                        format!("HTTP status {} matched", response.status)
                    } else {
                        format!(
                            "HTTP status mismatch: expected {expected_status}, observed {}",
                            response.status
                        )
                    },
                    observed: BTreeMap::new(),
                },
                Err(detail) => failed_transport(detail),
            },
            HealthCheckSpec::HttpJson {
                url,
                path_mode,
                expect,
                observe,
                ..
            } => match request(url, timeout, "application/json") {
                Ok(response) => {
                    if !(200..300).contains(&response.status) {
                        return TransportHealth {
                            transport_ready: true,
                            healthy: false,
                            detail: format!(
                                "HTTP JSON endpoint returned status {}",
                                response.status
                            ),
                            observed: BTreeMap::new(),
                        };
                    }
                    match serde_json::from_slice::<serde_json::Value>(&response.body) {
                        Ok(body) => evaluate_json_body(&body, *path_mode, expect, observe),
                        Err(error) => TransportHealth {
                            transport_ready: true,
                            healthy: false,
                            detail: format!("HTTP response was not valid JSON: {error}"),
                            observed: BTreeMap::new(),
                        },
                    }
                }
                Err(detail) => failed_transport(detail),
            },
        }
    }
}

fn evaluate_json_body(
    body: &serde_json::Value,
    path_mode: JsonPathMode,
    expect: &BTreeMap<String, serde_json::Value>,
    observe: &[String],
) -> TransportHealth {
    let mismatches = json_mismatches(body, path_mode, expect);
    TransportHealth {
        transport_ready: true,
        healthy: mismatches.is_empty(),
        detail: if mismatches.is_empty() {
            format!("HTTP JSON matched {} expected field(s)", expect.len())
        } else {
            format!("HTTP JSON required mismatch: {}", mismatches.join("; "))
        },
        observed: observed_values(body, path_mode, observe),
    }
}

fn observed_values(
    body: &serde_json::Value,
    path_mode: JsonPathMode,
    fields: &[String],
) -> BTreeMap<String, serde_json::Value> {
    fields
        .iter()
        .filter_map(|field| {
            let value = match path_mode {
                JsonPathMode::Shallow => body.get(field),
                JsonPathMode::JsonPointer => body.pointer(field),
            };
            value.cloned().map(|value| (field.clone(), value))
        })
        .collect()
}

fn json_mismatches(
    body: &serde_json::Value,
    path_mode: JsonPathMode,
    expectations: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Vec<String> {
    expectations
        .iter()
        .filter_map(|(key, expected)| {
            let observed = match path_mode {
                JsonPathMode::Shallow => body.get(key),
                JsonPathMode::JsonPointer => body.pointer(key),
            };
            (observed != Some(expected)).then(|| match observed {
                Some(_) => format!("{key}: value did not match"),
                None => format!("{key}: field was missing"),
            })
        })
        .collect()
}

fn connect_tcp(host: &str, port: u16, timeout: std::time::Duration) -> Result<(), String> {
    if timeout.is_zero() {
        return Err("TCP health deadline was exhausted".to_owned());
    }
    let ip = host
        .parse::<IpAddr>()
        .map_err(|_| "TCP health host must be an explicit loopback IP".to_owned())?;
    if !ip.is_loopback() {
        return Err("TCP health host must target loopback".to_owned());
    }
    let address = SocketAddr::new(ip, port);
    TcpStream::connect_timeout(&address, timeout)
        .map(|_| ())
        .map_err(|error| format!("TCP connection failed: {error}"))
}

fn failed_transport(detail: String) -> TransportHealth {
    TransportHealth {
        transport_ready: false,
        healthy: false,
        detail,
        observed: BTreeMap::new(),
    }
}

fn request(url: &str, timeout: std::time::Duration, accept: &str) -> Result<HttpResponse, String> {
    if timeout.is_zero() {
        return Err("HTTP health deadline was exhausted".to_owned());
    }
    let started = std::time::Instant::now();
    let (address, authority, path) = parse_loopback_url(url)?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|error| format!("HTTP connection failed: {error}"))?;
    let write_timeout = timeout.checked_sub(started.elapsed()).unwrap_or_default();
    if write_timeout.is_zero() {
        return Err("HTTP health deadline was exhausted before write".to_owned());
    }
    stream
        .set_write_timeout(Some(write_timeout))
        .map_err(|error| format!("failed to set HTTP write timeout: {error}"))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    )
    .and_then(|()| stream.flush())
    .map_err(|error| format!("HTTP request failed: {error}"))?;

    let read_timeout = timeout.checked_sub(started.elapsed()).unwrap_or_default();
    if read_timeout.is_zero() {
        return Err("HTTP health deadline was exhausted before read".to_owned());
    }
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|error| format!("failed to set HTTP read timeout: {error}"))?;
    let mut bytes = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("HTTP response read failed: {error}"))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("HTTP health response exceeded 1 MiB".to_owned());
    }
    parse_response(&bytes)
}

fn parse_loopback_url(url: &str) -> Result<(SocketAddr, &str, &str), String> {
    let remainder = url
        .strip_prefix("http://")
        .ok_or_else(|| "health URL must use loopback HTTP".to_owned())?;
    let (authority, path) = remainder
        .split_once('/')
        .map_or((remainder, "/"), |(authority, path)| {
            (authority, &url[url.len() - path.len() - 1..])
        });
    let address = authority
        .parse::<SocketAddr>()
        .map_err(|_| "health URL requires an explicit loopback IP and port".to_owned())?;
    if !address.ip().is_loopback() {
        return Err("health URL must target loopback".to_owned());
    }
    Ok((address, authority, path))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use crate::application::{HealthCheckSpec, HealthProbe, JsonPathMode};

    use super::{
        LoopbackTransportHealthProbe, connect_tcp, decode_chunked_body, evaluate_json_body,
        parse_loopback_url, parse_response, request,
    };

    #[test]
    fn tcp_probe_accepts_only_a_live_loopback_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let check = HealthCheckSpec::TcpConnect {
            host: "127.0.0.1".to_owned(),
            port,
            timeout: Duration::from_millis(100),
            startup_deadline: Duration::from_secs(1),
        };

        let result = LoopbackTransportHealthProbe.probe(&check, Duration::from_millis(100));

        assert!(result.transport_ready);
        assert!(result.healthy);
        assert!(result.detail.contains(&format!("127.0.0.1:{port}")));
        assert!(connect_tcp("192.0.2.1", port, Duration::from_millis(1)).is_err());
    }

    #[test]
    fn loopback_url_parser_keeps_query_and_path() {
        let (address, authority, path) =
            parse_loopback_url("http://127.0.0.1:11122/api/health?full=1").expect("valid URL");
        assert!(address.ip().is_loopback());
        assert_eq!(authority, "127.0.0.1:11122");
        assert_eq!(path, "/api/health?full=1");
        assert!(parse_loopback_url("http://192.0.2.1:80/health").is_err());
        assert!(parse_loopback_url("https://127.0.0.1:443/health").is_err());
    }

    #[test]
    fn status_and_json_probes_send_distinct_accept_headers() {
        assert_eq!(captured_accept_header("*/*"), "Accept: */*");
        assert_eq!(
            captured_accept_header("application/json"),
            "Accept: application/json"
        );
    }

    fn captured_accept_header(accept: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept health request");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("set fixture timeout");
            let mut request_bytes = Vec::new();
            let mut buffer = [0_u8; 256];
            while !request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("read health request");
                if read == 0 {
                    break;
                }
                request_bytes.extend_from_slice(&buffer[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .expect("write fixture response");
            String::from_utf8(request_bytes).expect("health request is ASCII")
        });

        request(
            &format!("http://{address}/health"),
            Duration::from_secs(1),
            accept,
        )
        .expect("health request succeeds");

        server
            .join()
            .expect("fixture server completes")
            .lines()
            .find(|line| line.starts_with("Accept:"))
            .expect("Accept header is present")
            .to_owned()
    }

    #[test]
    fn response_parser_separates_status_and_body() {
        let response = parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}",
        )
        .expect("valid response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"status":"ok"}"#);
    }

    #[test]
    fn response_parser_decodes_chunked_bodies_case_insensitively() {
        let response = parse_response(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: Chunked\r\nContent-Type: application/json\r\n\r\n7\r\n{\"id\":\"\r\n6;source=node\r\ngeofu\"\r\n1\r\n}\r\n0\r\nX-Probe: complete\r\n\r\n",
        )
        .expect("valid chunked response");

        assert_eq!(response.status, 200);
        assert_eq!(response.body, br#"{"id":"geofu"}"#);
    }

    #[test]
    fn chunked_decoder_rejects_invalid_or_truncated_framing() {
        assert!(decode_chunked_body(b"not-hex\r\ndata\r\n0\r\n\r\n").is_err());
        assert!(decode_chunked_body(b"4\r\nabc\r\n0\r\n\r\n").is_err());
        assert!(decode_chunked_body(b"0\r\nmissing trailer terminator").is_err());
    }

    #[test]
    fn observed_json_values_do_not_affect_required_readiness() {
        let body = serde_json::json!({
            "status": "ok",
            "runtime": "go",
            "version": "1.0.0-dev.9"
        });
        let required = BTreeMap::from([
            ("runtime".to_owned(), serde_json::json!("go")),
            ("status".to_owned(), serde_json::json!("ok")),
        ]);
        let observe = vec!["version".to_owned()];

        let result = evaluate_json_body(&body, JsonPathMode::Shallow, &required, &observe);

        assert!(result.transport_ready);
        assert!(result.healthy);
        assert_eq!(result.detail, "HTTP JSON matched 2 expected field(s)");
        assert_eq!(
            result.observed.get("version"),
            Some(&serde_json::json!("1.0.0-dev.9"))
        );
    }

    #[test]
    fn required_json_mismatch_still_fails_while_observing_values() {
        let body = serde_json::json!({"status": "starting", "version": "dev.9"});
        let required = BTreeMap::from([("status".to_owned(), serde_json::json!("ok"))]);
        let observe = vec!["version".to_owned()];

        let result = evaluate_json_body(&body, JsonPathMode::Shallow, &required, &observe);

        assert!(!result.healthy);
        assert_eq!(
            result.detail,
            "HTTP JSON required mismatch: status: value did not match"
        );
        assert_eq!(
            result.observed.get("version"),
            Some(&serde_json::json!("dev.9"))
        );
    }

    #[test]
    fn json_pointer_mode_matches_nested_objects_and_escaped_tokens() {
        let body = serde_json::json!({
            "runtime": {"version": {"major": 1}},
            "bridge/status": "ready"
        });
        let required = BTreeMap::from([
            (String::new(), body.clone()),
            (
                "/runtime/version".to_owned(),
                serde_json::json!({"major": 1}),
            ),
            ("/bridge~1status".to_owned(), serde_json::json!("ready")),
        ]);

        let result = evaluate_json_body(&body, JsonPathMode::JsonPointer, &required, &[]);

        assert!(result.healthy);
        assert_eq!(result.detail, "HTTP JSON matched 3 expected field(s)");
    }
}
