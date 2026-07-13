use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::application::{ControlAction, ControlErrorKind, SupervisorControl};
use crate::domain::{Actor, Reason};

use super::runtime_token::RuntimeToken;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Actor vocabulary accepted by the local control protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiActor {
    User,
    Codex,
}

impl ApiActor {
    const fn domain_actor(self) -> Actor {
        match self {
            Self::User => Actor::UserCli,
            Self::Codex => Actor::Agent,
        }
    }
}

/// A running loopback control server owned by the foreground supervisor.
pub struct ControlHttpServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), ControlHttpError>>>,
}

impl ControlHttpServer {
    /// Binds the configured address before spawning the request loop.
    ///
    /// # Errors
    ///
    /// Returns a bind, nonblocking-mode, or thread startup error.
    pub fn start(
        host: &str,
        port: u16,
        token: RuntimeToken,
        control: Arc<dyn SupervisorControl>,
    ) -> Result<Self, ControlHttpError> {
        let listener = TcpListener::bind((host, port)).map_err(ControlHttpError::Bind)?;
        listener
            .set_nonblocking(true)
            .map_err(ControlHttpError::Configure)?;
        let address = listener.local_addr().map_err(ControlHttpError::Configure)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("aku-supervisor-control".to_owned())
            .spawn(move || serve(&listener, &token, control.as_ref(), &thread_stop))
            .map_err(ControlHttpError::SpawnThread)?;
        Ok(Self {
            address,
            stop,
            thread: Some(thread),
        })
    }

    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Requests server termination and waits for the listener thread.
    ///
    /// # Errors
    ///
    /// Returns a request-loop failure or panic indication.
    pub fn shutdown(&mut self) -> Result<(), ControlHttpError> {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| ControlHttpError::ThreadPanicked)?
    }
}

impl fmt::Debug for ControlHttpServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControlHttpServer")
            .field("address", &self.address)
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for ControlHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn serve(
    listener: &TcpListener,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    stop: &AtomicBool,
) -> Result<(), ControlHttpError> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
                stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
                if let Err(error) = handle_connection(&mut stream, token, control) {
                    let response = error_response(400, "bad_request", &error.to_string());
                    write_response(&mut stream, &response).ok();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => return Err(ControlHttpError::Accept(error)),
        }
    }
    Ok(())
}

fn handle_connection(
    stream: &mut TcpStream,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
) -> Result<(), RequestError> {
    let request = read_request(stream)?;
    let response = route(&request, token, control);
    write_response(stream, &response).map_err(RequestError::Write)
}

fn route(request: &HttpRequest, token: &RuntimeToken, control: &dyn SupervisorControl) -> Response {
    if request.method == "GET" && request.target == "/v1/health" {
        return json_response(
            200,
            &json!({
                "status": "ok",
                "version": crate::VERSION
            }),
        );
    }

    if request.method == "GET" && request.target == "/v1/services" {
        return match control.snapshots() {
            Ok(services) => json_response(200, &json!({ "services": services })),
            Err(error) => error_response(500, "internal", error.message()),
        };
    }

    if request.method == "GET"
        && let Some(service_id) = request.target.strip_prefix("/v1/services/")
    {
        if service_id.is_empty() || service_id.contains('/') {
            return error_response(404, "not_found", "route not found");
        }
        return match control.snapshots() {
            Ok(services) => services
                .into_iter()
                .find(|service| service.id == service_id)
                .map_or_else(
                    || error_response(404, "service_not_found", "unknown service"),
                    |service| json_response(200, &json!({ "service": service })),
                ),
            Err(error) => error_response(500, "internal", error.message()),
        };
    }

    if request.method == "POST" {
        return route_mutation(request, token, control);
    }

    error_response(404, "not_found", "route not found")
}

fn route_mutation(
    request: &HttpRequest,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
) -> Response {
    let Some(candidate) = request
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return error_response(401, "unauthorized", "bearer token required");
    };
    if !token.bearer_matches(candidate) {
        return error_response(401, "unauthorized", "invalid bearer token");
    }

    let Some((service_id, action)) = parse_mutation_target(&request.target) else {
        return error_response(404, "not_found", "route not found");
    };
    let mutation: MutationRequest = match serde_json::from_slice(&request.body) {
        Ok(mutation) => mutation,
        Err(_) => return error_response(400, "invalid_request", "invalid mutation body"),
    };
    let reason = match Reason::new(mutation.reason) {
        Ok(reason) => reason,
        Err(error) => return error_response(400, "invalid_reason", &error.to_string()),
    };

    match control.mutate(action, service_id, mutation.actor.domain_actor(), reason) {
        Ok(outcome) => json_response(
            200,
            &json!({
                "serviceId": service_id,
                "outcome": outcome
            }),
        ),
        Err(error) => match error.kind() {
            ControlErrorKind::ServiceNotFound => {
                error_response(404, "service_not_found", error.message())
            }
            ControlErrorKind::Unauthorized => {
                error_response(403, "operation_forbidden", error.message())
            }
            ControlErrorKind::Internal => error_response(500, "internal", error.message()),
        },
    }
}

fn parse_mutation_target(target: &str) -> Option<(&str, ControlAction)> {
    let suffix = target.strip_prefix("/v1/services/")?;
    let mut fields = suffix.split('/');
    let service_id = fields.next()?;
    let action = match fields.next()? {
        "start" => ControlAction::Start,
        "stop" => ControlAction::Stop,
        "restart" => ControlAction::Restart,
        _ => return None,
    };
    if service_id.is_empty() || fields.next().is_some() {
        return None;
    }
    Some((service_id, action))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationRequest {
    actor: ApiActor,
    reason: String,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, RequestError> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(RequestError::HeaderTooLarge);
        }
        let mut chunk = [0_u8; 1024];
        let count = stream.read(&mut chunk).map_err(RequestError::Read)?;
        if count == 0 {
            return Err(RequestError::UnexpectedEnd);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
    };

    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| RequestError::Malformed("headers must be UTF-8"))?;
    let mut lines = headers[..headers.len() - 4].split("\r\n");
    let request_line = lines
        .next()
        .ok_or(RequestError::Malformed("request line is missing"))?;
    let fields = request_line.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || !matches!(fields[2], "HTTP/1.1" | "HTTP/1.0") {
        return Err(RequestError::Malformed("invalid request line"));
    }

    let mut authorization = None;
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(RequestError::Malformed("invalid header"))?;
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => {
                if authorization.replace(value.trim().to_owned()).is_some() {
                    return Err(RequestError::Malformed("duplicate authorization header"));
                }
            }
            "content-length" => {
                if content_length.is_some() {
                    return Err(RequestError::Malformed("duplicate content-length header"));
                }
                let length = value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| RequestError::Malformed("invalid content-length"))?;
                if length > MAX_BODY_BYTES {
                    return Err(RequestError::BodyTooLarge);
                }
                content_length = Some(length);
            }
            "transfer-encoding" => {
                return Err(RequestError::Malformed("transfer-encoding is unsupported"));
            }
            _ => {}
        }
    }

    let content_length = content_length.unwrap_or(0);
    let method = fields[0].to_owned();
    let target = fields[1].to_owned();
    while bytes.len() - header_end < content_length {
        let mut chunk = [0_u8; 1024];
        let count = stream.read(&mut chunk).map_err(RequestError::Read)?;
        if count == 0 {
            return Err(RequestError::UnexpectedEnd);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() - header_end > MAX_BODY_BYTES {
            return Err(RequestError::BodyTooLarge);
        }
    }

    Ok(HttpRequest {
        method,
        target,
        authorization,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug)]
struct Response {
    status: u16,
    body: Vec<u8>,
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&value).expect("JSON value serialization cannot fail"),
    }
}

fn error_response(status: u16, code: &str, message: &str) -> Response {
    json_response(
        status,
        &json!({ "error": { "code": code, "message": message } }),
    )
}

fn write_response(stream: &mut TcpStream, response: &Response) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()?;
    stream.shutdown(Shutdown::Write)
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
    let body = body
        .map(|value| serde_json::to_vec(&value))
        .transpose()
        .map_err(ControlClientError::Serialize)?
        .unwrap_or_default();
    let mut stream =
        TcpStream::connect_timeout(&address, IO_TIMEOUT).map_err(ControlClientError::Connect)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
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
    let header_end = find_bytes(&response, b"\r\n\r\n")
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
    let value: Value =
        serde_json::from_slice(&response[header_end..]).map_err(ControlClientError::Deserialize)?;
    if !(200..300).contains(&status) {
        return Err(ControlClientError::Rejected {
            status,
            body: value,
        });
    }
    Ok(value)
}

#[derive(Debug)]
pub enum ControlHttpError {
    Bind(io::Error),
    Configure(io::Error),
    SpawnThread(io::Error),
    Accept(io::Error),
    ThreadPanicked,
}

impl fmt::Display for ControlHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(error) => write!(formatter, "failed to bind control API: {error}"),
            Self::Configure(error) => write!(formatter, "failed to configure control API: {error}"),
            Self::SpawnThread(error) => write!(formatter, "failed to start control API: {error}"),
            Self::Accept(error) => write!(formatter, "control API accept failed: {error}"),
            Self::ThreadPanicked => formatter.write_str("control API thread panicked"),
        }
    }
}

impl std::error::Error for ControlHttpError {}

#[derive(Debug)]
enum RequestError {
    Read(io::Error),
    Write(io::Error),
    UnexpectedEnd,
    HeaderTooLarge,
    BodyTooLarge,
    Malformed(&'static str),
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "request read failed: {error}"),
            Self::Write(error) => write!(formatter, "response write failed: {error}"),
            Self::UnexpectedEnd => formatter.write_str("request ended unexpectedly"),
            Self::HeaderTooLarge => formatter.write_str("request headers exceed limit"),
            Self::BodyTooLarge => formatter.write_str("request body exceeds limit"),
            Self::Malformed(message) => formatter.write_str(message),
        }
    }
}

#[derive(Debug)]
pub enum ControlClientError {
    Connect(io::Error),
    Write(io::Error),
    Read(io::Error),
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    MalformedResponse,
    Rejected { status: u16, body: Value },
}

impl fmt::Display for ControlClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => write!(formatter, "cannot connect to AkuSupervisor: {error}"),
            Self::Write(error) => write!(formatter, "failed to send control request: {error}"),
            Self::Read(error) => write!(formatter, "failed to read control response: {error}"),
            Self::Serialize(error) => write!(formatter, "failed to serialize request: {error}"),
            Self::Deserialize(error) => write!(formatter, "invalid JSON response: {error}"),
            Self::MalformedResponse => formatter.write_str("malformed HTTP response"),
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::application::{ControlError, ControlMutationOutcome, ServiceSnapshot};

    use super::*;

    #[derive(Debug, Default)]
    struct FakeControl {
        mutations: Mutex<u32>,
    }

    impl SupervisorControl for FakeControl {
        fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
            Ok(Vec::new())
        }

        fn mutate(
            &self,
            _action: ControlAction,
            _service_id: &str,
            _actor: Actor,
            _reason: Reason,
        ) -> Result<ControlMutationOutcome, ControlError> {
            *self.mutations.lock().expect("mutation lock") += 1;
            Ok(ControlMutationOutcome::Started)
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
    fn unauthorized_mutation_never_reaches_shared_control() {
        let token_path =
            std::env::temp_dir().join(format!("aku-supervisor-http-token-{}", std::process::id()));
        std::fs::remove_file(&token_path).ok();
        let token =
            RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64))).expect("create token");
        let control = FakeControl::default();
        let request = HttpRequest {
            method: "POST".to_owned(),
            target: "/v1/services/api/start".to_owned(),
            authorization: Some(format!("Bearer {}", "b".repeat(64))),
            body: br#"{"actor":"codex","reason":"source changed"}"#.to_vec(),
        };

        let response = route(&request, &token, &control);

        assert_eq!(response.status, 401);
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
        std::fs::remove_file(token_path).ok();
    }
}
