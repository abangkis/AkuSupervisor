//! Bounded HTTP clients used by the CLI and MCP proxy.
use super::{IO_TIMEOUT, MAX_BODY_BYTES, find_bytes};
use crate::adapters::{mcp, runtime_token::RuntimeToken, service_logs::LiveLogEvent};
use serde_json::Value;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::{Duration, Instant};
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

fn response_parts(response: &[u8]) -> Result<(u16, &[u8]), ControlClientError> {
    let header_end = find_bytes(response, b"\r\n\r\n")
        .map(|position| position + 4)
        .ok_or(ControlClientError::MalformedResponse)?;
    let headers = std::str::from_utf8(&response[..header_end])
        .map_err(|_| ControlClientError::MalformedResponse)?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or(ControlClientError::MalformedResponse)?;
    Ok((status, &response[header_end..]))
}

/// Sends one bounded request to a running local supervisor.
///
/// # Errors
///
/// Returns connection, protocol, serialization, or non-success response errors.
pub fn client_request(
    address: SocketAddr,
    token: &RuntimeToken,
    method: &str,
    target: &str,
    body: Option<Value>,
) -> Result<Value, ControlClientError> {
    client_request_with_response_timeout(address, token, method, target, body, IO_TIMEOUT)
}

/// Opens one authenticated, close-delimited NDJSON live-log response.
///
/// # Errors
///
/// Returns connection, protocol, authorization, or JSON decoding errors.
pub fn client_live_logs(
    address: SocketAddr,
    token: &RuntimeToken,
    target: &str,
) -> Result<LiveLogClient, ControlClientError> {
    let mut stream =
        TcpStream::connect_timeout(&address, IO_TIMEOUT).map_err(ControlClientError::Connect)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nAccept: application/x-ndjson\r\nConnection: close\r\n\r\n",
        token.expose_for_authorization_header(),
    )
    .map_err(ControlClientError::Write)?;
    stream.flush().map_err(ControlClientError::Write)?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(ControlClientError::Read)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or(ControlClientError::MalformedResponse)?;
    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(ControlClientError::Read)?;
        if header.is_empty() {
            return Err(ControlClientError::MalformedResponse);
        }
        if header == "\r\n" {
            break;
        }
    }
    if status != 200 {
        let mut body = String::new();
        reader
            .take(MAX_RESPONSE_BYTES)
            .read_to_string(&mut body)
            .map_err(ControlClientError::Read)?;
        return Err(ControlClientError::Rejected {
            status,
            body: serde_json::from_str(&body).unwrap_or(Value::String(body)),
        });
    }
    reader.get_mut().set_read_timeout(None).ok();
    Ok(LiveLogClient { reader })
}

/// Reader for one long-lived live-log connection.
#[derive(Debug)]
pub struct LiveLogClient {
    reader: BufReader<TcpStream>,
}

impl LiveLogClient {
    /// Waits for the next NDJSON event, returning `None` on clean disconnect.
    ///
    /// # Errors
    ///
    /// Returns a bounded read or JSON decoding error.
    pub fn next_event(&mut self) -> Result<Option<LiveLogEvent>, ControlClientError> {
        let mut line = String::new();
        let bytes = self
            .reader
            .read_line(&mut line)
            .map_err(ControlClientError::Read)?;
        if bytes == 0 {
            return Ok(None);
        }
        if u64::try_from(bytes).unwrap_or(u64::MAX) > MAX_RESPONSE_BYTES {
            return Err(ControlClientError::MessageTooLarge);
        }
        serde_json::from_str(line.trim_end())
            .map(Some)
            .map_err(ControlClientError::Deserialize)
    }
}

/// Sends one bounded request while allowing a caller-defined response timeout.
///
/// Connection establishment and request writes retain the short control-plane
/// timeout. Only the response read may use the longer lifecycle-operation
/// budget derived from a service profile.
///
/// # Errors
///
/// Returns connection, protocol, serialization, or non-success response errors.
pub fn client_request_with_response_timeout(
    address: SocketAddr,
    token: &RuntimeToken,
    method: &str,
    target: &str,
    body: Option<Value>,
    response_timeout: Duration,
) -> Result<Value, ControlClientError> {
    let body = body
        .map(|value| serde_json::to_vec(&value))
        .transpose()
        .map_err(ControlClientError::Serialize)?
        .unwrap_or_default();
    let mut stream =
        TcpStream::connect_timeout(&address, IO_TIMEOUT).map_err(ControlClientError::Connect)?;
    stream.set_read_timeout(Some(response_timeout)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    write!(
        stream,
        "{method} {target} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        token.expose_for_authorization_header(),
        body.len()
    )
    .and_then(|()| stream.write_all(&body))
    .map_err(ControlClientError::Write)?;
    stream.shutdown(Shutdown::Write).ok();

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .map_err(ControlClientError::Read)?;
    parse_json_client_response(&response)
}

/// Sends one request within a caller-owned total deadline.
///
/// Serialization, connect, write, and response read all consume the same
/// budget. This is used by bounded reconciliation acknowledgment rather than
/// ordinary lifecycle requests, whose response timeout is operation-derived.
///
/// # Errors
///
/// Returns a deadline, connection, protocol, serialization, or non-success
/// response error.
pub(crate) fn client_request_until(
    address: SocketAddr,
    token: &RuntimeToken,
    method: &str,
    target: &str,
    body: Option<Value>,
    deadline: Instant,
) -> Result<Value, ControlClientError> {
    let body = body
        .map(|value| serde_json::to_vec(&value))
        .transpose()
        .map_err(ControlClientError::Serialize)?
        .unwrap_or_default();
    let connect_timeout = remaining_budget(deadline)?.min(IO_TIMEOUT);
    let mut stream = TcpStream::connect_timeout(&address, connect_timeout)
        .map_err(ControlClientError::Connect)?;
    let header = format!(
        "{method} {target} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        token.expose_for_authorization_header(),
        body.len()
    );
    let mut request = header.into_bytes();
    request.extend_from_slice(&body);
    stream
        .set_write_timeout(Some(remaining_budget(deadline)?))
        .ok();
    stream
        .write_all(&request)
        .map_err(ControlClientError::Write)?;
    stream.shutdown(Shutdown::Write).ok();
    stream
        .set_read_timeout(Some(remaining_budget(deadline)?))
        .ok();

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .map_err(ControlClientError::Read)?;
    remaining_budget(deadline)?;
    parse_json_client_response(&response)
}

fn remaining_budget(deadline: Instant) -> Result<Duration, ControlClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ControlClientError::DeadlineExceeded)
}

fn parse_json_client_response(response: &[u8]) -> Result<Value, ControlClientError> {
    let (status, body) = response_parts(response)?;
    decode_json_response(status, body)
}

fn decode_json_response(status: u16, body: &[u8]) -> Result<Value, ControlClientError> {
    let value: Value = serde_json::from_slice(body).map_err(ControlClientError::Deserialize)?;
    if !(200..300).contains(&status) {
        return Err(ControlClientError::Rejected {
            status,
            body: value,
        });
    }
    Ok(value)
}

/// Forwards one bounded JSON-RPC message to the read-only MCP endpoint.
///
/// Notifications return `Ok(None)` after HTTP 202. Requests return exactly
/// one JSON value.
///
/// # Errors
///
/// Returns connection, protocol, serialization, or non-success response errors.
pub fn mcp_client_request(
    address: SocketAddr,
    token: &RuntimeToken,
    message: &Value,
) -> Result<Option<Value>, ControlClientError> {
    let body = serde_json::to_vec(message).map_err(ControlClientError::Serialize)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(ControlClientError::MessageTooLarge);
    }
    let mut stream =
        TcpStream::connect_timeout(&address, IO_TIMEOUT).map_err(ControlClientError::Connect)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    write!(
        stream,
        "POST {} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        mcp::MCP_ENDPOINT,
        token.expose_for_authorization_header(),
        mcp::MCP_PROTOCOL_VERSION,
        body.len()
    )
    .and_then(|()| stream.write_all(&body))
    .map_err(ControlClientError::Write)?;
    stream.shutdown(Shutdown::Write).ok();

    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .map_err(ControlClientError::Read)?;
    let (status, body) = response_parts(&response)?;
    if status == 202 && body.is_empty() {
        return Ok(None);
    }
    decode_json_response(status, body).map(Some)
}

#[derive(Debug)]
pub enum ControlClientError {
    Connect(io::Error),
    Write(io::Error),
    Read(io::Error),
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    MessageTooLarge,
    MalformedResponse,
    DeadlineExceeded,
    Rejected { status: u16, body: Value },
}

impl ControlClientError {
    /// Returns whether an idempotent request may be retried after this client-side failure.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        !matches!(
            self,
            Self::Serialize(_) | Self::MessageTooLarge | Self::Rejected { .. }
        )
    }
}

impl fmt::Display for ControlClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(formatter, "cannot connect to AkuSupervisor: {error}"),
            Self::Write(error) => write!(formatter, "failed to send control request: {error}"),
            Self::Read(error) => write!(formatter, "failed to read control response: {error}"),
            Self::Serialize(error) => write!(formatter, "failed to serialize request: {error}"),
            Self::Deserialize(error) => write!(formatter, "invalid JSON response: {error}"),
            Self::MessageTooLarge => formatter.write_str("MCP message exceeds bounded body size"),
            Self::MalformedResponse => formatter.write_str("malformed HTTP response"),
            Self::DeadlineExceeded => {
                formatter.write_str("control request exceeded its total deadline")
            }
            Self::Rejected { status, body } => {
                write!(
                    formatter,
                    "control request rejected with HTTP {status}: {body}"
                )
            }
        }
    }
}

impl std::error::Error for ControlClientError {}
