use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

use crate::application::{HealthCheckSpec, HealthProbe, TransportHealth};

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Small loopback-only HTTP/1.1 health adapter with no OS-specific behavior.
#[derive(Debug, Default)]
pub struct LoopbackHttpHealthProbe;

impl HealthProbe for LoopbackHttpHealthProbe {
    fn probe(&self, check: &HealthCheckSpec, timeout: std::time::Duration) -> TransportHealth {
        match check {
            HealthCheckSpec::Process => TransportHealth {
                transport_ready: true,
                healthy: true,
                detail: "owned process tree is ready".to_owned(),
            },
            HealthCheckSpec::HttpStatus {
                url,
                expected_status,
                ..
            } => match request(url, timeout) {
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
                },
                Err(detail) => failed_transport(detail),
            },
            HealthCheckSpec::HttpJson { url, expect, .. } => match request(url, timeout) {
                Ok(response) => {
                    if !(200..300).contains(&response.status) {
                        return TransportHealth {
                            transport_ready: true,
                            healthy: false,
                            detail: format!(
                                "HTTP JSON endpoint returned status {}",
                                response.status
                            ),
                        };
                    }
                    match serde_json::from_slice::<serde_json::Value>(&response.body) {
                        Ok(body) => {
                            let mismatches = expect
                                .iter()
                                .filter_map(|(key, expected)| {
                                    let observed = body.get(key);
                                    (observed != Some(expected)).then(|| match observed {
                                        Some(_) => format!("{key}: value did not match"),
                                        None => format!("{key}: field was missing"),
                                    })
                                })
                                .collect::<Vec<_>>();
                            TransportHealth {
                                transport_ready: true,
                                healthy: mismatches.is_empty(),
                                detail: if mismatches.is_empty() {
                                    format!("HTTP JSON matched {} expected field(s)", expect.len())
                                } else {
                                    format!("HTTP JSON mismatch: {}", mismatches.join("; "))
                                },
                            }
                        }
                        Err(error) => TransportHealth {
                            transport_ready: true,
                            healthy: false,
                            detail: format!("HTTP response was not valid JSON: {error}"),
                        },
                    }
                }
                Err(detail) => failed_transport(detail),
            },
        }
    }
}

fn failed_transport(detail: String) -> TransportHealth {
    TransportHealth {
        transport_ready: false,
        healthy: false,
        detail,
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn request(url: &str, timeout: std::time::Duration) -> Result<HttpResponse, String> {
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
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
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

fn parse_response(bytes: &[u8]) -> Result<HttpResponse, String> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response headers were incomplete".to_owned())?;
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "HTTP response headers were not UTF-8".to_owned())?;
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| "HTTP response status was invalid".to_owned())?;
    let body = bytes[(header_end + 4)..].to_vec();
    Ok(HttpResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::{parse_loopback_url, parse_response};

    #[test]
    fn loopback_url_parser_keeps_query_and_path() {
        let (address, authority, path) =
            parse_loopback_url("http://127.0.0.1:47821/api/health?full=1").expect("valid URL");
        assert!(address.ip().is_loopback());
        assert_eq!(authority, "127.0.0.1:47821");
        assert_eq!(path, "/api/health?full=1");
        assert!(parse_loopback_url("http://192.0.2.1:80/health").is_err());
        assert!(parse_loopback_url("https://127.0.0.1:443/health").is_err());
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
}
