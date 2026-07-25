use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::application::{
    ControlAction, ControlErrorKind, CooperativeActionControl, CooperativeOperationError,
    CooperativeOperationManager, CooperativeOperationStatus, RegistryReconciliationStatus,
    SupervisorControl,
};
use crate::domain::{Actor, Reason};

use super::config::McpConfig;
use super::journal::FileJournal;
use super::mcp::{self, McpResponse};
use super::runtime_token::RuntimeToken;
use super::service_logs::{
    LiveLogEvent, LiveLogSelection, LiveLogSubscription, LogStream, ServiceLogError,
    ServiceLogStore,
};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const LIVE_LOG_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

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
            Self::Codex => Actor::Codex,
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
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        host: &str,
        port: u16,
        token: RuntimeToken,
        mcp_config: McpConfig,
        control: Arc<dyn SupervisorControl>,
        cooperative: Option<Arc<dyn CooperativeActionControl>>,
        journal: Arc<FileJournal>,
        logs: Arc<ServiceLogStore>,
        reconciliation: Arc<RegistryReconciliationStatus>,
    ) -> Result<Self, ControlHttpError> {
        let listener = TcpListener::bind((host, port)).map_err(ControlHttpError::Bind)?;
        listener
            .set_nonblocking(true)
            .map_err(ControlHttpError::Configure)?;
        let address = listener.local_addr().map_err(ControlHttpError::Configure)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let cooperative = cooperative
            .map(CooperativeOperationManager::new)
            .map(Arc::new);
        let thread = thread::Builder::new()
            .name("aku-supervisor-control".to_owned())
            .spawn(move || {
                serve(
                    &listener,
                    &token,
                    &mcp_config,
                    control.as_ref(),
                    cooperative.as_deref(),
                    &journal,
                    &logs,
                    &reconciliation,
                    &thread_stop,
                )
            })
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

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: &TcpListener,
    token: &RuntimeToken,
    mcp_config: &McpConfig,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    journal: &FileJournal,
    logs: &Arc<ServiceLogStore>,
    reconciliation: &RegistryReconciliationStatus,
    stop: &Arc<AtomicBool>,
) -> Result<(), ControlHttpError> {
    let mut idempotency = IdempotencyStore::default();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _peer)) => {
                stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
                stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
                if let Err(error) = handle_connection(
                    &mut stream,
                    token,
                    mcp_config,
                    control,
                    cooperative,
                    journal,
                    logs,
                    reconciliation,
                    &mut idempotency,
                    stop,
                ) {
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

#[allow(clippy::too_many_arguments)]
fn handle_connection(
    stream: &mut TcpStream,
    token: &RuntimeToken,
    mcp_config: &McpConfig,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    journal: &FileJournal,
    logs: &Arc<ServiceLogStore>,
    reconciliation: &RegistryReconciliationStatus,
    idempotency: &mut IdempotencyStore,
    stop: &Arc<AtomicBool>,
) -> Result<(), RequestError> {
    let request = read_request(stream)?;
    if request.method == "GET"
        && let Some((service_id, selection, tail, after)) = parse_live_logs_target(&request.target)
    {
        if !request_is_authorized(&request, token) {
            let response = error_response(401, "unauthorized", "valid bearer token required");
            return write_response(stream, &response).map_err(RequestError::Write);
        }
        let subscription = match logs.subscribe(service_id, selection, tail, after) {
            Ok(subscription) => subscription,
            Err(ServiceLogError::ServiceNotFound(_)) => {
                let response = error_response(404, "service_not_found", "unknown service");
                return write_response(stream, &response).map_err(RequestError::Write);
            }
            Err(ServiceLogError::TooManySubscribers(_)) => {
                let response = error_response(
                    409,
                    "live_log_subscriber_limit",
                    "too many live-log subscribers for service",
                );
                return write_response(stream, &response).map_err(RequestError::Write);
            }
            Err(error) => {
                let response = error_response(500, "log_unavailable", &error.to_string());
                return write_response(stream, &response).map_err(RequestError::Write);
            }
        };
        let live_stream = stream.try_clone().map_err(RequestError::Write)?;
        let worker_stop = Arc::clone(stop);
        let hub_instance_id = logs.hub_instance_id().to_owned();
        let service_id = service_id.to_owned();
        thread::Builder::new()
            .name("aku-supervisor-live-log".to_owned())
            .spawn(move || {
                stream_live_logs(
                    live_stream,
                    &subscription,
                    &hub_instance_id,
                    &service_id,
                    &worker_stop,
                )
                .ok();
            })
            .map_err(RequestError::Write)?;
        return Ok(());
    }
    let response = route(
        &request,
        token,
        mcp_config,
        control,
        cooperative,
        journal,
        logs,
        reconciliation,
        idempotency,
    );
    write_response(stream, &response).map_err(RequestError::Write)
}

fn stream_live_logs(
    mut stream: TcpStream,
    subscription: &LiveLogSubscription,
    hub_instance_id: &str,
    service_id: &str,
    stop: &AtomicBool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )?;
    for event in &subscription.initial {
        write_live_log_event(&mut stream, event)?;
    }
    let mut last_write = Instant::now();
    while !stop.load(Ordering::Acquire) {
        match subscription.receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => {
                write_live_log_event(&mut stream, &event)?;
                last_write = Instant::now();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                if last_write.elapsed() >= LIVE_LOG_HEARTBEAT_INTERVAL =>
            {
                write_live_log_event(
                    &mut stream,
                    &LiveLogEvent::heartbeat(hub_instance_id, service_id),
                )?;
                last_write = Instant::now();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    stream.shutdown(Shutdown::Write)
}

fn write_live_log_event(stream: &mut TcpStream, event: &LiveLogEvent) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, event).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn route(
    request: &HttpRequest,
    token: &RuntimeToken,
    mcp_config: &McpConfig,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    journal: &FileJournal,
    logs: &ServiceLogStore,
    reconciliation: &RegistryReconciliationStatus,
    idempotency: &mut IdempotencyStore,
) -> Response {
    if request.target == mcp::MCP_ENDPOINT {
        return route_mcp(request, token, mcp_config, control, journal, logs);
    }

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

    if request.method == "GET" && request.target == "/v1/registry" {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        return json_response(200, &json!({ "registry": reconciliation.snapshot() }));
    }

    if request.method == "GET" && request.target == "/v1/cooperative-actions/aku-bridge/active" {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        let Some(cooperative) = cooperative else {
            return error_response(
                404,
                "cooperative_action_disabled",
                "AkuBridge reload_self is not configured",
            );
        };
        return match cooperative.active() {
            Ok(operation) => json_response(200, &json!({ "operation": operation })),
            Err(error) => error_response(500, "operation_registry_unavailable", &error.to_string()),
        };
    }

    if request.method == "GET"
        && let Some(request_id) = parse_cooperative_operation_target(&request.target)
    {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        let Some(cooperative) = cooperative else {
            return error_response(
                404,
                "cooperative_action_disabled",
                "AkuBridge reload_self is not configured",
            );
        };
        return match cooperative.get(request_id) {
            Ok(operation) => json_response(200, &json!({ "operation": operation })),
            Err(CooperativeOperationError::NotFound) => error_response(
                404,
                "operation_not_found",
                "cooperative operation was not found",
            ),
            Err(error) => error_response(500, "operation_registry_unavailable", &error.to_string()),
        };
    }

    if request.method == "GET"
        && let Some((after, limit)) = parse_events_target(&request.target)
    {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        return match journal.events(after, limit) {
            Ok(events) => json_response(200, &json!({ "events": events })),
            Err(error) => error_response(500, "journal_unavailable", &error.to_string()),
        };
    }

    if request.method == "GET"
        && let Some((service_id, stream, tail)) = parse_logs_target(&request.target)
    {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        return match logs.tail(service_id, stream, tail) {
            Ok(log) => json_response(200, &json!({ "log": log })),
            Err(ServiceLogError::ServiceNotFound(_)) => {
                error_response(404, "service_not_found", "unknown service")
            }
            Err(error) => error_response(500, "log_unavailable", &error.to_string()),
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
        return route_mutation(request, token, control, cooperative, idempotency);
    }

    error_response(404, "not_found", "route not found")
}

fn route_mcp(
    request: &HttpRequest,
    token: &RuntimeToken,
    config: &McpConfig,
    control: &dyn SupervisorControl,
    journal: &FileJournal,
    logs: &ServiceLogStore,
) -> Response {
    if !config.enabled {
        return error_response(404, "not_found", "route not found");
    }
    if !request_is_authorized(request, token) {
        return error_response(401, "unauthorized", "valid bearer token required");
    }
    if request.origin.as_ref().is_some_and(|origin| {
        !config
            .allowed_origins
            .iter()
            .any(|allowed| allowed == origin)
    }) {
        return json_response(
            403,
            &json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32000, "message": "Origin is not allowed"}
            }),
        );
    }
    if request.method != "POST" {
        return empty_response(405);
    }
    if request.content_type.as_deref() != Some("application/json") {
        return error_response(400, "invalid_content_type", "application/json required");
    }
    let accepts = request.accept.as_deref().unwrap_or_default();
    if !accepts
        .split(',')
        .map(str::trim)
        .any(|value| value == "application/json")
        || !accepts
            .split(',')
            .map(str::trim)
            .any(|value| value == "text/event-stream")
    {
        return error_response(
            400,
            "invalid_accept",
            "Accept must include application/json and text/event-stream",
        );
    }
    if request
        .mcp_protocol_version
        .as_deref()
        .is_some_and(|version| !mcp::supports_protocol_version(version))
    {
        return error_response(
            400,
            "unsupported_protocol_version",
            "unsupported MCP version",
        );
    }
    match mcp::handle_message(&request.body, control, journal, logs) {
        McpResponse::Json(value) => json_response(200, &value),
        McpResponse::Accepted => empty_response(202),
    }
}

fn request_is_authorized(request: &HttpRequest, token: &RuntimeToken) -> bool {
    request
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|candidate| token.bearer_matches(candidate))
}

fn parse_logs_target(target: &str) -> Option<(&str, LogStream, usize)> {
    let suffix = target.strip_prefix("/v1/services/")?;
    let (service_id, query) = suffix.split_once("/logs?")?;
    if service_id.is_empty() || service_id.contains('/') {
        return None;
    }
    let mut stream = LogStream::Stdout;
    let mut tail = 100_usize;
    for field in query.split('&') {
        let (name, value) = field.split_once('=')?;
        match name {
            "stream" => stream = LogStream::parse(value)?,
            "tail" => tail = value.parse::<usize>().ok()?.clamp(1, 1_000),
            _ => return None,
        }
    }
    Some((service_id, stream, tail))
}

fn parse_live_logs_target(target: &str) -> Option<(&str, LiveLogSelection, usize, Option<u64>)> {
    let suffix = target.strip_prefix("/v1/services/")?;
    let (service_id, query) = suffix.split_once("/logs/live?")?;
    if service_id.is_empty() || service_id.contains('/') {
        return None;
    }
    let mut selection = LiveLogSelection::Both;
    let mut tail = 50_usize;
    let mut after = None;
    for field in query.split('&') {
        let (name, value) = field.split_once('=')?;
        match name {
            "stream" => selection = LiveLogSelection::parse(value)?,
            "tail" => tail = value.parse::<usize>().ok()?.clamp(0, 1_000),
            "after" => after = Some(value.parse::<u64>().ok()?),
            _ => return None,
        }
    }
    Some((service_id, selection, tail, after))
}

fn parse_events_target(target: &str) -> Option<(u64, usize)> {
    let query = target.strip_prefix("/v1/events")?;
    if query.is_empty() {
        return Some((0, 50));
    }
    let query = query.strip_prefix('?')?;
    let mut after = 0_u64;
    let mut limit = 50_usize;
    for field in query.split('&') {
        let (name, value) = field.split_once('=')?;
        match name {
            "after" => after = value.parse().ok()?,
            "limit" => limit = value.parse::<usize>().ok()?.clamp(1, 200),
            _ => return None,
        }
    }
    Some((after, limit))
}

fn route_mutation(
    request: &HttpRequest,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    idempotency: &mut IdempotencyStore,
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

    let mutation: MutationRequest = match serde_json::from_slice(&request.body) {
        Ok(mutation) => mutation,
        Err(_) => return error_response(400, "invalid_request", "invalid mutation body"),
    };
    if let Some(request_id) = mutation.request_id.as_deref()
        && !valid_request_id(request_id)
    {
        return error_response(
            400,
            "invalid_request_id",
            "requestId must be 1-128 URL-safe ASCII characters",
        );
    }
    let reason = match Reason::new(mutation.reason) {
        Ok(reason) => reason,
        Err(error) => return error_response(400, "invalid_reason", &error.to_string()),
    };

    if request.target == "/v1/cooperative-actions/aku-bridge/reload-self" {
        return route_cooperative_reload(
            cooperative,
            mutation.actor,
            reason,
            mutation.request_id.as_deref(),
        );
    }

    if let Some(request_id) = mutation.request_id.as_deref() {
        match idempotency.lookup(request_id, &request.target, &request.body) {
            IdempotencyLookup::Replay(response) => return response,
            IdempotencyLookup::Conflict => {
                return error_response(
                    409,
                    "idempotency_conflict",
                    "requestId was already used for a different mutation",
                );
            }
            IdempotencyLookup::Miss => {}
        }
    }

    let Some((service_id, action)) = parse_mutation_target(&request.target) else {
        return error_response(404, "not_found", "route not found");
    };

    let response = match control.mutate(action, service_id, mutation.actor.domain_actor(), reason) {
        Ok(result) => {
            let mut body = json!({
                "serviceId": service_id,
                "outcome": result.outcome
            });
            if let Some(shutdown) = result.shutdown {
                body["shutdown"] = json!(shutdown);
            }
            json_response(200, &body)
        }
        Err(error) => match error.kind() {
            ControlErrorKind::ServiceNotFound => {
                error_response(404, "service_not_found", error.message())
            }
            ControlErrorKind::Unauthorized => {
                error_response(403, "operation_forbidden", error.message())
            }
            ControlErrorKind::PortConflictExternal => {
                error_response(409, "port_conflict_external", error.message())
            }
            ControlErrorKind::SpawnFailed => error_response(500, "spawn_failed", error.message()),
            ControlErrorKind::HealthFailed => error_response(503, "health_failed", error.message()),
            ControlErrorKind::ShutdownTimeout => {
                error_response(500, "shutdown_timeout", error.message())
            }
            ControlErrorKind::OwnershipLost => {
                error_response(500, "ownership_lost", error.message())
            }
            ControlErrorKind::Internal => error_response(500, "internal", error.message()),
        },
    };
    if let Some(request_id) = mutation.request_id {
        idempotency.store(
            request_id,
            request.target.clone(),
            request.body.clone(),
            response.clone(),
        );
    }
    response
}

fn route_cooperative_reload(
    cooperative: Option<&CooperativeOperationManager>,
    actor: ApiActor,
    reason: Reason,
    request_id: Option<&str>,
) -> Response {
    let Some(request_id) = request_id else {
        return error_response(
            400,
            "request_id_required",
            "reload_self requires requestId for relay idempotency and audit",
        );
    };
    let Some(cooperative) = cooperative else {
        return error_response(
            404,
            "cooperative_action_disabled",
            "AkuBridge reload_self is not configured",
        );
    };
    match cooperative.begin(actor.domain_actor(), reason, request_id) {
        Ok(operation) => {
            let status = if operation.status == CooperativeOperationStatus::Running {
                202
            } else {
                200
            };
            json_response(status, &json!({ "operation": operation }))
        }
        Err(CooperativeOperationError::IdempotencyConflict) => error_response(
            409,
            "idempotency_conflict",
            "requestId was already used for different cooperative-action input",
        ),
        Err(CooperativeOperationError::ActionInProgress(_)) => error_response(
            409,
            "action_in_progress",
            "another AkuBridge cooperative action is active",
        ),
        Err(error) => error_response(500, "operation_registry_unavailable", &error.to_string()),
    }
}

fn parse_cooperative_operation_target(target: &str) -> Option<&str> {
    let request_id = target.strip_prefix("/v1/cooperative-actions/aku-bridge/requests/")?;
    valid_request_id(request_id).then_some(request_id)
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MutationRequest {
    actor: ApiActor,
    reason: String,
    #[serde(default)]
    request_id: Option<String>,
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

const IDEMPOTENCY_CAPACITY: usize = 1_024;

#[derive(Debug, Default)]
struct IdempotencyStore {
    entries: HashMap<String, CachedMutation>,
    order: VecDeque<String>,
}

impl IdempotencyStore {
    fn lookup(&self, request_id: &str, target: &str, body: &[u8]) -> IdempotencyLookup {
        match self.entries.get(request_id) {
            Some(cached) if cached.target == target && cached.body == body => {
                IdempotencyLookup::Replay(cached.response.clone())
            }
            Some(_) => IdempotencyLookup::Conflict,
            None => IdempotencyLookup::Miss,
        }
    }

    fn store(&mut self, request_id: String, target: String, body: Vec<u8>, response: Response) {
        if self.entries.contains_key(&request_id) {
            return;
        }
        while self.entries.len() >= IDEMPOTENCY_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(request_id.clone());
        self.entries.insert(
            request_id,
            CachedMutation {
                target,
                body,
                response,
            },
        );
    }
}

#[derive(Debug)]
enum IdempotencyLookup {
    Replay(Response),
    Conflict,
    Miss,
}

#[derive(Debug)]
struct CachedMutation {
    target: String,
    body: Vec<u8>,
    response: Response,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    target: String,
    authorization: Option<String>,
    accept: Option<String>,
    content_type: Option<String>,
    origin: Option<String>,
    mcp_protocol_version: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, RequestError> {
    let mut bytes = Vec::new();
    let deadline = Instant::now() + IO_TIMEOUT;
    let header_end = loop {
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(RequestError::HeaderTooLarge);
        }
        let mut chunk = [0_u8; 1024];
        let count = read_with_deadline(stream, &mut chunk, deadline)?;
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
    let mut accept = None;
    let mut content_type = None;
    let mut origin = None;
    let mut mcp_protocol_version = None;
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
            "accept" => set_single_header(&mut accept, value, "accept")?,
            "content-type" => {
                let value = value.trim().split(';').next().unwrap_or_default().trim();
                set_single_header(&mut content_type, value, "content-type")?;
            }
            "origin" => set_single_header(&mut origin, value, "origin")?,
            "mcp-protocol-version" => {
                set_single_header(&mut mcp_protocol_version, value, "mcp-protocol-version")?;
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
        let count = read_with_deadline(stream, &mut chunk, deadline)?;
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
        accept,
        content_type,
        origin,
        mcp_protocol_version,
        body: bytes[header_end..header_end + content_length].to_vec(),
    })
}

fn read_with_deadline(
    stream: &mut TcpStream,
    chunk: &mut [u8],
    deadline: Instant,
) -> Result<usize, RequestError> {
    loop {
        match stream.read(chunk) {
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(RequestError::Read(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "request read deadline elapsed",
                    )));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(RequestError::Read(error)),
        }
    }
}

fn set_single_header(
    slot: &mut Option<String>,
    value: &str,
    name: &'static str,
) -> Result<(), RequestError> {
    if slot.replace(value.trim().to_owned()).is_some() {
        return Err(RequestError::Malformed(match name {
            "accept" => "duplicate accept header",
            "content-type" => "duplicate content-type header",
            "origin" => "duplicate origin header",
            "mcp-protocol-version" => "duplicate mcp-protocol-version header",
            _ => "duplicate header",
        }));
    }
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[derive(Debug, Clone)]
struct Response {
    status: u16,
    body: Vec<u8>,
    content_type: Option<&'static str>,
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&value).expect("JSON value serialization cannot fail"),
        content_type: Some("application/json"),
    }
}

fn empty_response(status: u16) -> Response {
    Response {
        status,
        body: Vec::new(),
        content_type: None,
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
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    write!(stream, "HTTP/1.1 {} {reason}\r\n", response.status)?;
    if let Some(content_type) = response.content_type {
        write!(stream, "Content-Type: {content_type}\r\n")?;
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
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
    if status == 202 && response[header_end..].is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_slice(&response[header_end..]).map_err(ControlClientError::Deserialize)?;
    if !(200..300).contains(&status) {
        return Err(ControlClientError::Rejected {
            status,
            body: value,
        });
    }
    Ok(Some(value))
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::application::{
        ControlError, ControlMutationOutcome, ControlMutationResult, CooperativeActionError,
        CooperativeActionOutcome, CooperativeActionStatus, ServiceSnapshot,
    };

    use super::*;

    #[test]
    fn client_retry_taxonomy_excludes_rejections() {
        assert!(ControlClientError::MalformedResponse.is_transient());
        assert!(
            ControlClientError::Read(io::Error::from(io::ErrorKind::ConnectionReset))
                .is_transient()
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
        ) -> Result<ControlMutationResult, ControlError> {
            *self.mutations.lock().expect("mutation lock") += 1;
            Ok(ControlMutationResult::new(
                ControlMutationOutcome::Started,
                None,
            ))
        }
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
            FileJournal::open(root.join("journal.jsonl"), Vec::<String>::new())
                .expect("test journal"),
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

        drop(live);
        server.shutdown().expect("stop control server");
        std::fs::remove_dir_all(root).ok();
    }

    #[derive(Debug, Default)]
    struct FakeCooperativeControl {
        reloads: Mutex<u32>,
    }

    impl CooperativeActionControl for FakeCooperativeControl {
        fn reload_aku_bridge(
            &self,
            _actor: Actor,
            _reason: Reason,
            request_id: &str,
            _progress: &(dyn Fn(crate::application::CooperativeActionProgress) + Send + Sync),
        ) -> Result<CooperativeActionOutcome, CooperativeActionError> {
            *self.reloads.lock().expect("reload lock") += 1;
            Ok(CooperativeActionOutcome {
                target: "aku-bridge".to_owned(),
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
            parse_live_logs_target("/v1/services/api/logs/live?stream=stderr&tail=5000&after=42"),
            Some(("api", LiveLogSelection::Stderr, 1_000, Some(42)))
        );
        assert!(
            parse_live_logs_target("/v1/services/api/logs/live?stream=invalid&tail=50").is_none()
        );
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
            authorization: Some(format!("Bearer {}", "b".repeat(64))),
            accept: None,
            content_type: None,
            origin: None,
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
            &mut IdempotencyStore::default(),
        );

        assert_eq!(response.status, 401);
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
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
        let journal =
            FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
        let logs = ServiceLogStore::new(&std::env::temp_dir(), std::iter::empty());
        let control = FakeControl::default();
        let reconciliation = RegistryReconciliationStatus::new("sha256:active".to_owned());
        let request = |authorization| HttpRequest {
            method: "GET".to_owned(),
            target: "/v1/registry".to_owned(),
            authorization,
            accept: None,
            content_type: None,
            origin: None,
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
        let journal =
            FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
        let logs = ServiceLogStore::new(&std::env::temp_dir(), ["api".to_owned()]);
        let control = FakeControl::default();
        let reconciliation = RegistryReconciliationStatus::new("sha256:test".to_owned());
        let cooperative = Arc::new(FakeCooperativeControl::default());
        let manager = CooperativeOperationManager::new(cooperative.clone());
        let request = HttpRequest {
            method: "POST".to_owned(),
            target: "/v1/cooperative-actions/aku-bridge/reload-self".to_owned(),
            authorization: Some(format!("Bearer {}", "a".repeat(64))),
            accept: None,
            content_type: None,
            origin: None,
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
            target: "/v1/cooperative-actions/aku-bridge/requests/bridge-1".to_owned(),
            authorization: Some(format!("Bearer {}", "a".repeat(64))),
            accept: None,
            content_type: None,
            origin: None,
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
            target: "/v1/cooperative-actions/aku-bridge/active".to_owned(),
            authorization: Some(format!("Bearer {}", "a".repeat(64))),
            accept: None,
            content_type: None,
            origin: None,
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
        let journal =
            FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
        let logs = ServiceLogStore::new(&std::env::temp_dir(), ["api".to_owned()]);
        let control = FakeControl::default();
        let reconciliation = RegistryReconciliationStatus::new("sha256:test".to_owned());
        let request = HttpRequest {
            method: "POST".to_owned(),
            target: "/v1/services/api/start".to_owned(),
            authorization: Some(format!("Bearer {}", "a".repeat(64))),
            accept: None,
            content_type: None,
            origin: None,
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
            &mut idempotency,
        );

        assert_eq!(first.status, 200);
        assert_eq!(second.body, first.body);
        assert_eq!(conflict.status, 409);
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 1);
        std::fs::remove_file(token_path).ok();
        std::fs::remove_file(journal_path).ok();
    }
}
