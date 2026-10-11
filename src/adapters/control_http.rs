use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
use super::runtime_instance::RuntimeInstance;
use super::runtime_token::RuntimeToken;
use super::service_logs::{
    LiveLogCursor, LiveLogEvent, LiveLogSelection, LiveLogSubscription, LogStream, ServiceLogError,
    ServiceLogStore,
};
use super::supervisor_shutdown::{
    SupervisorShutdown, SupervisorShutdownError, SupervisorShutdownOutcome,
    SupervisorShutdownRequest,
};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const LIVE_LOG_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const CONTROL_CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONTROL_CONNECTIONS: usize = 64;
const DASHBOARD_HTML: &[u8] = include_bytes!("../../dashboard/index.html");
const DASHBOARD_CSS: &[u8] = include_bytes!("../../dashboard/app.css");
const DASHBOARD_JS: &[u8] = include_bytes!("../../dashboard/app.js");

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
    active_connections: Arc<AtomicUsize>,
    thread: Option<JoinHandle<Result<(), ControlHttpError>>>,
}

#[derive(Debug)]
struct ActiveConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
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
        instance: Arc<RuntimeInstance>,
        supervisor_shutdown: Arc<SupervisorShutdown>,
    ) -> Result<Self, ControlHttpError> {
        let listener = TcpListener::bind((host, port)).map_err(ControlHttpError::Bind)?;
        listener
            .set_nonblocking(true)
            .map_err(ControlHttpError::Configure)?;
        let address = listener.local_addr().map_err(ControlHttpError::Configure)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let token = Arc::new(token);
        let mcp_config = Arc::new(mcp_config);
        let cooperative = cooperative
            .map(CooperativeOperationManager::new)
            .map(Arc::new);
        let active_connections = Arc::new(AtomicUsize::new(0));
        let server_active_connections = Arc::clone(&active_connections);
        let thread = thread::Builder::new()
            .name("aku-supervisor-control".to_owned())
            .spawn(move || {
                serve(
                    &listener,
                    &token,
                    &mcp_config,
                    &control,
                    cooperative.as_ref(),
                    &journal,
                    &logs,
                    &reconciliation,
                    &instance,
                    &supervisor_shutdown,
                    &thread_stop,
                    &server_active_connections,
                )
            })
            .map_err(ControlHttpError::SpawnThread)?;
        Ok(Self {
            address,
            stop,
            active_connections: Arc::clone(&active_connections),
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
            .map_err(|_| ControlHttpError::ThreadPanicked)??;
        let deadline = Instant::now() + CONTROL_CONNECTION_DRAIN_TIMEOUT;
        while self.active_connections.load(Ordering::Acquire) != 0 {
            if Instant::now() >= deadline {
                return Err(ControlHttpError::ConnectionsStillActive(
                    self.active_connections.load(Ordering::Acquire),
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(())
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
    token: &Arc<RuntimeToken>,
    mcp_config: &Arc<McpConfig>,
    control: &Arc<dyn SupervisorControl>,
    cooperative: Option<&Arc<CooperativeOperationManager>>,
    journal: &Arc<FileJournal>,
    logs: &Arc<ServiceLogStore>,
    reconciliation: &Arc<RegistryReconciliationStatus>,
    instance: &Arc<RuntimeInstance>,
    supervisor_shutdown: &Arc<SupervisorShutdown>,
    stop: &Arc<AtomicBool>,
    active_connections: &Arc<AtomicUsize>,
) -> Result<(), ControlHttpError> {
    let idempotency = Arc::new(Mutex::new(IdempotencyStore::default()));
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
                stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
                if active_connections
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        (count < MAX_CONTROL_CONNECTIONS).then_some(count + 1)
                    })
                    .is_err()
                {
                    let mut stream = stream;
                    let response = error_response(
                        503,
                        "control_connection_limit",
                        "AkuSupervisor control connection capacity is temporarily full",
                    );
                    write_response(&mut stream, &response).ok();
                    continue;
                }
                let worker_active = Arc::clone(active_connections);
                let worker_token = Arc::clone(token);
                let worker_mcp_config = Arc::clone(mcp_config);
                let worker_control = Arc::clone(control);
                let worker_cooperative = cooperative.cloned();
                let worker_journal = Arc::clone(journal);
                let worker_logs = Arc::clone(logs);
                let worker_reconciliation = Arc::clone(reconciliation);
                let worker_instance = Arc::clone(instance);
                let worker_shutdown = Arc::clone(supervisor_shutdown);
                let worker_idempotency = Arc::clone(&idempotency);
                let worker_stop = Arc::clone(stop);
                let worker = thread::Builder::new()
                    .name("aku-supervisor-control-request".to_owned())
                    .spawn(move || {
                        let _active_guard = ActiveConnectionGuard {
                            active: worker_active,
                        };
                        let mut stream = stream;
                        if let Err(error) = handle_connection(
                            &mut stream,
                            peer,
                            &worker_token,
                            &worker_mcp_config,
                            worker_control.as_ref(),
                            worker_cooperative.as_deref(),
                            &worker_journal,
                            &worker_logs,
                            &worker_reconciliation,
                            &worker_instance,
                            &worker_shutdown,
                            &worker_idempotency,
                            &worker_stop,
                        ) {
                            let response = error_response(400, "bad_request", &error.to_string());
                            write_response(&mut stream, &response).ok();
                        }
                    });
                if let Err(error) = worker {
                    active_connections.fetch_sub(1, Ordering::AcqRel);
                    return Err(ControlHttpError::SpawnThread(error));
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
    peer: SocketAddr,
    token: &RuntimeToken,
    mcp_config: &McpConfig,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    journal: &FileJournal,
    logs: &Arc<ServiceLogStore>,
    reconciliation: &RegistryReconciliationStatus,
    instance: &RuntimeInstance,
    supervisor_shutdown: &SupervisorShutdown,
    idempotency: &Mutex<IdempotencyStore>,
    stop: &Arc<AtomicBool>,
) -> Result<(), RequestError> {
    let request = read_request(stream)?;
    if stop.load(Ordering::Acquire) {
        let response = error_response(
            503,
            "shutdown_in_progress",
            "AkuSupervisor is draining; retry after a new instance is ready",
        );
        return write_response(stream, &response).map_err(RequestError::Write);
    }
    if request.method == "GET"
        && let Some((service_id, selection, tail, cursor)) = parse_live_logs_target(&request.target)
    {
        if !request_is_authorized(&request, token) {
            let response = error_response(401, "unauthorized", "valid bearer token required");
            return write_response(stream, &response).map_err(RequestError::Write);
        }
        let subscription = match logs.subscribe(service_id, selection, tail, cursor.as_ref()) {
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
    let response = if dashboard_asset(&request.target).is_some()
        && !dashboard_request_is_loopback(peer, request.host.as_deref())
    {
        dashboard_response(404, b"not found", "text/plain; charset=utf-8")
    } else if request.method == "POST" && request.target.starts_with("/v1/services/") {
        let mut guard = idempotency
            .lock()
            .map_err(|_| RequestError::IdempotencyPoisoned)?;
        route(
            &request,
            token,
            mcp_config,
            control,
            cooperative,
            journal,
            logs,
            reconciliation,
            instance,
            supervisor_shutdown,
            &mut guard,
        )
    } else {
        let scratch = Mutex::new(IdempotencyStore::default());
        let mut scratch_guard = scratch.lock().expect("scratch idempotency lock");
        route(
            &request,
            token,
            mcp_config,
            control,
            cooperative,
            journal,
            logs,
            reconciliation,
            instance,
            supervisor_shutdown,
            &mut scratch_guard,
        )
    };
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
    instance: &RuntimeInstance,
    supervisor_shutdown: &SupervisorShutdown,
    idempotency: &mut IdempotencyStore,
) -> Response {
    if let Some(asset) = dashboard_asset(&request.target) {
        return if request.method == "GET" {
            dashboard_asset_response(asset)
        } else {
            dashboard_response(405, b"method not allowed", "text/plain; charset=utf-8")
        };
    }

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

    if request.method == "GET" && request.target == "/v1/instance" {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        return json_response(200, &json!({ "instance": instance }));
    }

    if request.method == "GET" && request.target == "/v1/registry" {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        return json_response(200, &json!({ "registry": reconciliation.snapshot() }));
    }

    if request.method == "GET"
        && request.target == "/v1/cooperative-actions/chrome-extension/active"
    {
        if !request_is_authorized(request, token) {
            return error_response(401, "unauthorized", "valid bearer token required");
        }
        let Some(cooperative) = cooperative else {
            return error_response(
                404,
                "cooperative_action_disabled",
                "Chrome extension reload is not configured",
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
                "Chrome extension reload is not configured",
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
        return route_mutation(
            request,
            token,
            control,
            cooperative,
            supervisor_shutdown,
            instance,
            idempotency,
        );
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

fn parse_live_logs_target(
    target: &str,
) -> Option<(&str, LiveLogSelection, usize, Option<LiveLogCursor>)> {
    let suffix = target.strip_prefix("/v1/services/")?;
    let (service_id, query) = suffix.split_once("/logs/live?")?;
    if service_id.is_empty() || service_id.contains('/') {
        return None;
    }
    let mut selection = LiveLogSelection::Both;
    let mut tail = 50_usize;
    let mut after = None;
    let mut after_hub = None;
    for field in query.split('&') {
        let (name, value) = field.split_once('=')?;
        match name {
            "stream" => selection = LiveLogSelection::parse(value)?,
            "tail" => tail = value.parse::<usize>().ok()?.clamp(0, 1_000),
            "after" => after = Some(value.parse::<u64>().ok()?),
            "afterHub" if valid_hub_instance_id(value) => after_hub = Some(value.to_owned()),
            _ => return None,
        }
    }
    let cursor = match (after_hub, after) {
        (Some(hub_instance_id), Some(sequence)) => Some(LiveLogCursor {
            hub_instance_id,
            sequence,
        }),
        (None, None) => None,
        _ => return None,
    };
    Some((service_id, selection, tail, cursor))
}

fn valid_hub_instance_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
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

#[allow(clippy::too_many_lines)]
fn route_mutation(
    request: &HttpRequest,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    supervisor_shutdown: &SupervisorShutdown,
    instance: &RuntimeInstance,
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

    if request.target == "/v1/cooperative-actions/chrome-extension/reload-self" {
        return route_cooperative_reload(
            cooperative,
            mutation.actor,
            reason,
            mutation.request_id.as_deref(),
        );
    }

    if request.target == "/v1/supervisor/shutdown" {
        let Some(request_id) = mutation.request_id else {
            return error_response(
                400,
                "request_id_required",
                "Supervisor shutdown requires requestId for idempotency and audit",
            );
        };
        return match supervisor_shutdown.request(SupervisorShutdownRequest {
            actor: mutation.actor.domain_actor(),
            reason,
            request_id: request_id.clone(),
        }) {
            Ok(outcome) => json_response(
                202,
                &json!({
                    "status": match outcome {
                        SupervisorShutdownOutcome::Accepted => "accepted",
                        SupervisorShutdownOutcome::AlreadyAccepted => "already_accepted",
                    },
                    "requestId": request_id,
                    "instance": instance,
                }),
            ),
            Err(SupervisorShutdownError::AlreadyInProgress) => error_response(
                409,
                "shutdown_in_progress",
                "a different Supervisor shutdown request is already active",
            ),
            Err(SupervisorShutdownError::LockPoisoned) => error_response(
                500,
                "shutdown_registry_unavailable",
                "Supervisor shutdown registry is unavailable",
            ),
        };
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
            "Chrome extension reload is not configured",
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
            "another Chrome extension cooperative action is active",
        ),
        Err(error) => error_response(500, "operation_registry_unavailable", &error.to_string()),
    }
}

fn parse_cooperative_operation_target(target: &str) -> Option<&str> {
    let request_id = target.strip_prefix("/v1/cooperative-actions/chrome-extension/requests/")?;
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
    host: Option<String>,
    authorization: Option<String>,
    accept: Option<String>,
    content_type: Option<String>,
    origin: Option<String>,
    mcp_protocol_version: Option<String>,
    body: Vec<u8>,
}

fn set_host_header(host: &mut Option<String>, value: &str) {
    if host.is_some() {
        *host = Some(String::new());
    } else {
        *host = Some(value.trim().to_owned());
    }
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

    let mut host = None;
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
            "host" => set_host_header(&mut host, value),
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
        host,
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
    dashboard_security_headers: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DashboardAsset {
    Html,
    Css,
    Javascript,
}

fn dashboard_asset(target: &str) -> Option<DashboardAsset> {
    match target {
        "/" => Some(DashboardAsset::Html),
        "/app.css" => Some(DashboardAsset::Css),
        "/app.js" => Some(DashboardAsset::Javascript),
        _ => None,
    }
}

fn dashboard_asset_response(asset: DashboardAsset) -> Response {
    let (body, content_type) = match asset {
        DashboardAsset::Html => (DASHBOARD_HTML, "text/html; charset=utf-8"),
        DashboardAsset::Css => (DASHBOARD_CSS, "text/css; charset=utf-8"),
        DashboardAsset::Javascript => (DASHBOARD_JS, "text/javascript; charset=utf-8"),
    };
    dashboard_response(200, body, content_type)
}

fn dashboard_response(status: u16, body: &[u8], content_type: &'static str) -> Response {
    Response {
        status,
        body: body.to_vec(),
        content_type: Some(content_type),
        dashboard_security_headers: true,
    }
}

fn dashboard_request_is_loopback(peer: SocketAddr, host: Option<&str>) -> bool {
    peer.ip().is_loopback() && host.is_some_and(is_loopback_host)
}

fn is_loopback_host(authority: &str) -> bool {
    let authority = authority.trim();
    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((address, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if !suffix.is_empty()
            && !suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return false;
        }
        return address
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    }

    let host = authority.split_once(':').map_or(authority, |(host, port)| {
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            ""
        } else {
            host
        }
    });
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn json_response(status: u16, value: &Value) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&value).expect("JSON value serialization cannot fail"),
        content_type: Some("application/json"),
        dashboard_security_headers: false,
    }
}

fn empty_response(status: u16) -> Response {
    Response {
        status,
        body: Vec::new(),
        content_type: None,
        dashboard_security_headers: false,
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
    if response.dashboard_security_headers {
        stream.write_all(
            b"Cache-Control: no-store\r\n\
              X-Content-Type-Options: nosniff\r\n\
              Referrer-Policy: no-referrer\r\n\
              Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n",
        )?;
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
    ConnectionsStillActive(usize),
}

impl fmt::Display for ControlHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(error) => write!(formatter, "failed to bind control API: {error}"),
            Self::Configure(error) => write!(formatter, "failed to configure control API: {error}"),
            Self::SpawnThread(error) => write!(formatter, "failed to start control API: {error}"),
            Self::Accept(error) => write!(formatter, "control API accept failed: {error}"),
            Self::ThreadPanicked => formatter.write_str("control API thread panicked"),
            Self::ConnectionsStillActive(count) => write!(
                formatter,
                "control API stopped accepting requests but {count} request handler(s) did not drain within the shutdown deadline"
            ),
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
    IdempotencyPoisoned,
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
            Self::IdempotencyPoisoned => {
                formatter.write_str("control request idempotency store is unavailable")
            }
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

    use crate::adapters::runtime_instance::{RuntimeInstance, RuntimeMode};
    use crate::adapters::service_logs::LiveLogEventKind;
    use crate::application::{
        ControlError, ControlMutationOutcome, ControlMutationResult, CooperativeActionError,
        CooperativeActionOutcome, CooperativeActionStatus, HealthSnapshot, ProcessIdentity,
        ServiceSnapshot,
    };
    use crate::domain::{DesiredState, LifecycleState, OperatorHold};

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
        snapshots: Mutex<Vec<ServiceSnapshot>>,
    }

    impl SupervisorControl for FakeControl {
        fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
            Ok(self.snapshots.lock().expect("snapshot lock").clone())
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
        let mut stream = TcpStream::connect(address).expect("connect to fixture control server");
        write!(stream, "{method} {target} HTTP/1.1\r\nHost: {host}\r\n")
            .expect("write request line and Host");
        if let Some(origin) = origin {
            write!(stream, "Origin: {origin}\r\n").expect("write request Origin");
        }
        stream
            .write_all(b"Connection: close\r\n\r\n")
            .expect("finish raw request");
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
    fn embedded_dashboard_serves_static_assets_safely_and_preserves_read_only_api() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("dashboard-http-fixtures")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&root).expect("create dashboard HTTP fixture");
        let token_path = root.join("control-token");
        let server_token = RuntimeToken::load_or_create(&token_path, || Ok("d".repeat(64)))
            .expect("create server token");
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
            ("/", "text/html; charset=utf-8", DASHBOARD_HTML),
            ("/app.css", "text/css; charset=utf-8", DASHBOARD_CSS),
            ("/app.js", "text/javascript; charset=utf-8", DASHBOARD_JS),
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
            parse_live_logs_target(
                "/v1/services/api/logs/live?stream=stderr&tail=50&afterHub=123-old"
            )
            .is_none()
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
            host: None,
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
        let journal =
            FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
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
        let journal =
            FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
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
        let journal =
            FileJournal::open(&journal_path, Vec::<String>::new()).expect("create journal");
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
}
