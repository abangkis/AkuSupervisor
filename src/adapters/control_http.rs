//! HTTP listener, routing, and authenticated lifecycle dispatch.
mod client;
mod dashboard;
#[cfg(test)]
mod tests;

use super::dashboard_assets::dashboard_asset;
pub(crate) use client::client_request_until;
pub use client::{
    ControlClientError, LiveLogClient, client_live_logs, client_request,
    client_request_with_response_timeout, mcp_client_request,
};
use dashboard::{dashboard_request_is_loopback, dashboard_response, route_dashboard_control};

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
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
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const LIVE_LOG_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const CONTROL_CONNECTION_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONTROL_CONNECTIONS: usize = 64;

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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    if request.target.starts_with("/dashboard/") {
        let local_address = stream.local_addr().map_err(RequestError::Read)?;
        let mut guard = idempotency
            .lock()
            .map_err(|_| RequestError::IdempotencyPoisoned)?;
        let response = route_dashboard_control(
            &request,
            peer,
            local_address,
            token,
            control,
            instance,
            supervisor_shutdown,
            &mut guard,
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
            dashboard_response(200, asset.body, asset.content_type)
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

fn route_mutation(
    request: &HttpRequest,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    supervisor_shutdown: &SupervisorShutdown,
    instance: &RuntimeInstance,
    idempotency: &mut IdempotencyStore,
) -> Response {
    route_mutation_with_service_actor(
        request,
        token,
        control,
        cooperative,
        supervisor_shutdown,
        instance,
        idempotency,
        None,
    )
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn route_mutation_with_service_actor(
    request: &HttpRequest,
    token: &RuntimeToken,
    control: &dyn SupervisorControl,
    cooperative: Option<&CooperativeOperationManager>,
    supervisor_shutdown: &SupervisorShutdown,
    instance: &RuntimeInstance,
    idempotency: &mut IdempotencyStore,
    service_actor: Option<Actor>,
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

    let actor = service_actor.unwrap_or_else(|| mutation.actor.domain_actor());
    let response = match control.mutate(action, service_id, actor, reason) {
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

#[derive(Debug, Clone)]
struct HttpRequest {
    method: String,
    target: String,
    host: Option<String>,
    authorization: Option<String>,
    accept: Option<String>,
    content_type: Option<String>,
    origin: Option<String>,
    dashboard_request: Option<String>,
    dashboard_token: Option<String>,
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

#[allow(clippy::too_many_lines)]
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
    let mut dashboard_request = None;
    let mut dashboard_token = None;
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
            "x-dashboard-request" => {
                set_single_header(&mut dashboard_request, value, "x-dashboard-request")?;
            }
            "x-dashboard-token" => {
                set_single_header(&mut dashboard_token, value, "x-dashboard-token")?;
            }
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
        dashboard_request,
        dashboard_token,
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
        415 => "Unsupported Media Type",
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
