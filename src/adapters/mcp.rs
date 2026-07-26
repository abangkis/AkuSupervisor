//! Stateless, read-only Model Context Protocol adapter.
//!
//! This module deliberately depends on the application-facing control trait and
//! bounded persistence adapters only. It does not own services, bootstrap the
//! supervisor, or expose lifecycle mutations.

use serde_json::{Map, Value, json};

use crate::application::SupervisorControl;

use super::journal::FileJournal;
use super::service_logs::{LogStream, ServiceLogError, ServiceLogStore};

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
pub const MCP_ENDPOINT: &str = "/mcp";

const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-03-26", "2025-06-18", MCP_PROTOCOL_VERSION];

#[must_use]
pub fn supports_protocol_version(version: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

/// Result of handling one stateless MCP POST body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpResponse {
    Json(Value),
    Accepted,
}

/// Handles one JSON-RPC message without retaining an MCP session.
#[must_use]
pub fn handle_message(
    body: &[u8],
    control: &dyn SupervisorControl,
    journal: &FileJournal,
    logs: &ServiceLogStore,
) -> McpResponse {
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(_) => return McpResponse::Json(error(&Value::Null, -32700, "Parse error")),
    };
    let Some(object) = message.as_object() else {
        return McpResponse::Json(error(&Value::Null, -32600, "Invalid Request"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return McpResponse::Json(error(&request_id(object), -32600, "Invalid Request"));
    }

    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // JSON-RPC responses sent by a client are accepted by the transport.
        return McpResponse::Accepted;
    };
    let Some(id) = object.get("id") else {
        // The read-only adapter has no server-originated request state. Valid
        // client notifications, including notifications/initialized, are acked.
        return McpResponse::Accepted;
    };
    if !matches!(id, Value::String(_) | Value::Number(_)) {
        return McpResponse::Json(error(&Value::Null, -32600, "Invalid Request"));
    }
    let id = id.clone();
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

    let result = match method {
        "initialize" => initialize(&params),
        "ping" => require_empty_object(&params).map(|()| json!({})),
        "tools/list" => list_tools(&params),
        "tools/call" => call_tool(&params, control, journal, logs),
        _ => return McpResponse::Json(error(&id, -32601, "Method not found")),
    };
    match result {
        Ok(result) => McpResponse::Json(success(&id, &result)),
        Err(McpFailure::InvalidParams(message)) => McpResponse::Json(error(&id, -32602, &message)),
        Err(McpFailure::MethodNotFound) => {
            McpResponse::Json(error(&id, -32601, "Method not found"))
        }
    }
}

fn initialize(params: &Value) -> Result<Value, McpFailure> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_params("initialize params must be an object"))?;
    let requested = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("protocolVersion is required"))?;
    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        MCP_PROTOCOL_VERSION
    };
    Ok(json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "aku-supervisor",
            "title": "AkuSupervisor (read-only)",
            "version": crate::VERSION,
            "description": "Read-only visibility into an already-running AkuSupervisor"
        },
        "instructions": "Inspect registered services, recent lifecycle events, and bounded logs. This MCP endpoint cannot start, stop, restart, reload, or bootstrap processes."
    }))
}

fn list_tools(params: &Value) -> Result<Value, McpFailure> {
    let object = params
        .as_object()
        .ok_or_else(|| invalid_params("tools/list params must be an object"))?;
    if object.keys().any(|key| key != "_meta") {
        return Err(invalid_params(
            "pagination is not required for the bounded tool list",
        ));
    }
    Ok(json!({ "tools": [
        tool(
            "supervisor_list_services",
            "List services",
            "List bounded runtime snapshots for every registered service.",
            &json!({"type":"object","properties":{},"additionalProperties":false})
        ),
        tool(
            "supervisor_get_service",
            "Get service",
            "Inspect one registered service by its configured ID.",
            &json!({
                "type":"object",
                "properties":{"serviceId":{"type":"string","minLength":1}},
                "required":["serviceId"],
                "additionalProperties":false
            })
        ),
        tool(
            "supervisor_get_recent_events",
            "Get recent events",
            "Read a bounded page of canonical lifecycle journal events.",
            &json!({
                "type":"object",
                "properties":{
                    "after":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1,"maximum":200}
                },
                "additionalProperties":false
            })
        ),
        tool(
            "supervisor_read_logs",
            "Read service logs",
            "Read a bounded tail from one registered service stdout or stderr log.",
            &json!({
                "type":"object",
                "properties":{
                    "serviceId":{"type":"string","minLength":1},
                    "stream":{"type":"string","enum":["stdout","stderr"]},
                    "lines":{"type":"integer","minimum":1,"maximum":200}
                },
                "required":["serviceId"],
                "additionalProperties":false
            })
        )
    ] }))
}

pub(crate) fn contract_surface() -> Value {
    let mut initialized = initialize(&json!({"protocolVersion": MCP_PROTOCOL_VERSION}))
        .expect("the internal MCP contract initialize request is valid");
    initialized["serverInfo"]
        .as_object_mut()
        .expect("MCP serverInfo is an object")
        .remove("version");
    let tools = list_tools(&json!({})).expect("the internal MCP tool-list request is valid");
    json!({
        "initialize": initialized,
        "methods": ["initialize", "ping", "tools/list", "tools/call"],
        "supportedProtocolVersions": SUPPORTED_PROTOCOL_VERSIONS,
        "tools": tools["tools"]
    })
}

fn tool(name: &str, title: &str, description: &str, input_schema: &Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
        "execution": { "taskSupport": "forbidden" },
        "annotations": {
            "title": title,
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": false
        }
    })
}

fn call_tool(
    params: &Value,
    control: &dyn SupervisorControl,
    journal: &FileJournal,
    logs: &ServiceLogStore,
) -> Result<Value, McpFailure> {
    let object = exact_object(params, &["name", "arguments", "_meta"])?;
    let name = required_string(object, "name")?;
    let arguments = object
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(arguments) = arguments.as_object() else {
        return Ok(tool_failure("arguments must be an object"));
    };

    if !matches!(
        name,
        "supervisor_list_services"
            | "supervisor_get_service"
            | "supervisor_get_recent_events"
            | "supervisor_read_logs"
    ) {
        return Err(McpFailure::MethodNotFound);
    }

    let value = execute_tool(name, arguments, control, journal, logs);
    Ok(match value {
        Ok(value) => bounded_tool_success(&value),
        Err(message) => tool_failure(&message),
    })
}

fn execute_tool(
    name: &str,
    arguments: &Map<String, Value>,
    control: &dyn SupervisorControl,
    journal: &FileJournal,
    logs: &ServiceLogStore,
) -> Result<Value, String> {
    match name {
        "supervisor_list_services" => {
            tool_arguments(require_exact_keys(arguments, &[]))?;
            control
                .snapshots()
                .map(|services| json!({"services": services}))
                .map_err(|error| tool_error(&error.to_string()))
        }
        "supervisor_get_service" => {
            tool_arguments(require_exact_keys(arguments, &["serviceId"]))?;
            let service_id = tool_arguments(required_string(arguments, "serviceId"))?;
            control
                .snapshots()
                .map_err(|error| tool_error(&error.to_string()))
                .and_then(|services| {
                    services
                        .into_iter()
                        .find(|service| service.id == service_id)
                        .map(|service| json!({"service": service}))
                        .ok_or_else(|| tool_error("unknown service"))
                })
        }
        "supervisor_get_recent_events" => {
            tool_arguments(require_exact_keys(arguments, &["after", "limit"]))?;
            let after = tool_arguments(optional_u64(arguments, "after"))?.unwrap_or(0);
            let limit = tool_arguments(optional_usize(arguments, "limit"))?
                .unwrap_or(50)
                .clamp(1, 200);
            journal
                .events(after, limit)
                .map(|events| json!({"events": events}))
                .map_err(|error| tool_error(&error.to_string()))
        }
        "supervisor_read_logs" => {
            tool_arguments(require_exact_keys(
                arguments,
                &["serviceId", "stream", "lines"],
            ))?;
            let service_id = tool_arguments(required_string(arguments, "serviceId"))?;
            let stream_name =
                tool_arguments(optional_string(arguments, "stream"))?.unwrap_or("stdout");
            let lines = tool_arguments(optional_usize(arguments, "lines"))?
                .unwrap_or(100)
                .clamp(1, 200);
            LogStream::parse(stream_name)
                .ok_or_else(|| tool_error("stream must be stdout or stderr"))
                .and_then(|stream| {
                    logs.tail(service_id, stream, lines)
                        .map(|log| json!({"log": log}))
                        .map_err(|error| match error {
                            ServiceLogError::ServiceNotFound(_) => tool_error("unknown service"),
                            other => tool_error(&other.to_string()),
                        })
                })
        }
        _ => unreachable!("tool name checked before dispatch"),
    }
}

fn tool_arguments<T>(result: Result<T, McpFailure>) -> Result<T, String> {
    match result {
        Ok(value) => Ok(value),
        Err(McpFailure::InvalidParams(message)) => Err(message),
        Err(McpFailure::MethodNotFound) => Err("invalid tool arguments".to_owned()),
    }
}

fn bounded_tool_success(value: &Value) -> Value {
    let text = serde_json::to_string(&value).expect("JSON value serialization cannot fail");
    if text.len() > MAX_TOOL_RESULT_BYTES {
        return tool_failure("result exceeded the bounded MCP response size");
    }
    json!({
        "content": [{"type":"text","text":text}],
        "structuredContent": value,
        "isError": false
    })
}

fn tool_failure(message: &str) -> Value {
    json!({
        "content": [{"type":"text","text":message}],
        "isError": true
    })
}

fn success(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn request_id(object: &Map<String, Value>) -> Value {
    object
        .get("id")
        .filter(|id| matches!(id, Value::String(_) | Value::Number(_)))
        .cloned()
        .unwrap_or(Value::Null)
}

fn require_empty_object(value: &Value) -> Result<(), McpFailure> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_params("params must be an object"))?;
    require_exact_keys(object, &[])
}

fn exact_object<'a>(
    value: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, McpFailure> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_params("params must be an object"))?;
    require_exact_keys(object, allowed)?;
    Ok(object)
}

fn require_exact_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), McpFailure> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid_params(&format!("unknown field: {key}")));
    }
    Ok(())
}

fn required_string<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, McpFailure> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_params(&format!("{name} must be a non-empty string")))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a str>, McpFailure> {
    object
        .get(name)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| invalid_params(&format!("{name} must be a string")))
        })
        .transpose()
}

fn optional_u64(object: &Map<String, Value>, name: &str) -> Result<Option<u64>, McpFailure> {
    object
        .get(name)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| invalid_params(&format!("{name} must be a non-negative integer")))
        })
        .transpose()
}

fn optional_usize(object: &Map<String, Value>, name: &str) -> Result<Option<usize>, McpFailure> {
    optional_u64(object, name)?.map_or(Ok(None), |value| {
        usize::try_from(value)
            .map(Some)
            .map_err(|_| invalid_params(&format!("{name} is too large")))
    })
}

fn invalid_params(message: &str) -> McpFailure {
    McpFailure::InvalidParams(message.to_owned())
}

fn tool_error(message: &str) -> String {
    message.to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpFailure {
    InvalidParams(String),
    MethodNotFound,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::application::{
        ControlAction, ControlError, ControlMutationOutcome, ControlMutationResult, HealthSnapshot,
        ServiceSnapshot,
    };
    use crate::domain::{Actor, DesiredState, LifecycleState, OperatorHold, Reason};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    #[derive(Debug, Default)]
    struct FakeControl {
        mutations: Mutex<usize>,
    }

    impl SupervisorControl for FakeControl {
        fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
            Ok(vec![ServiceSnapshot {
                id: "api".to_owned(),
                label: "API".to_owned(),
                lifecycle: LifecycleState::Running,
                desired_state: DesiredState::Running,
                operator_hold: OperatorHold::None,
                root_pid: Some(42),
                owned_pids: vec![42],
                last_action: None,
                health: HealthSnapshot::healthy(true, None, "owned process is running".to_owned()),
                started_at_unix_ms: Some(1),
                last_exit_code: None,
                last_exit_at_unix_ms: None,
                restart_count: 0,
            }])
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

    fn fixtures() -> (std::path::PathBuf, FileJournal, ServiceLogStore) {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("aku-supervisor-mcp-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create fixture root");
        let journal = FileJournal::open(root.join("events.jsonl"), Vec::<String>::new())
            .expect("open journal");
        let logs = ServiceLogStore::new(&root, ["api".to_owned()]);
        (root, journal, logs)
    }

    #[test]
    fn lists_only_four_read_only_tools() {
        let (root, journal, logs) = fixtures();
        let control = FakeControl::default();
        let response = handle_message(
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            &control,
            &journal,
            &logs,
        );
        let McpResponse::Json(value) = response else {
            panic!("expected JSON response");
        };
        let tools = value["result"]["tools"].as_array().expect("tool list");
        assert_eq!(tools.len(), 4);
        assert!(
            tools
                .iter()
                .all(|tool| tool["annotations"]["readOnlyHint"] == true)
        );
        assert!(
            tools
                .iter()
                .all(|tool| tool["execution"]["taskSupport"] == "forbidden")
        );
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
        let installer = include_str!("../../scripts/install-codex-mcp.ps1");
        for tool in tools {
            let name = tool["name"].as_str().expect("tool name");
            assert!(
                installer.contains(&format!("\"{name}\"")),
                "Codex installer is missing read-only tool {name}"
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn calls_service_read_without_reaching_mutation_boundary() {
        let (root, journal, logs) = fixtures();
        let control = FakeControl::default();
        let response = handle_message(
            br#"{"jsonrpc":"2.0","id":"read-1","method":"tools/call","params":{"name":"supervisor_get_service","arguments":{"serviceId":"api"}}}"#,
            &control,
            &journal,
            &logs,
        );
        let McpResponse::Json(value) = response else {
            panic!("expected JSON response");
        };
        assert_eq!(value["result"]["structuredContent"]["service"]["id"], "api");
        assert_eq!(value["result"]["isError"], false);
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rejects_unknown_tool_and_mutation_shaped_input() {
        let (root, journal, logs) = fixtures();
        let control = FakeControl::default();
        let response = handle_message(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"supervisor_restart_service","arguments":{"serviceId":"api","command":"evil"}}}"#,
            &control,
            &journal,
            &logs,
        );
        let McpResponse::Json(value) = response else {
            panic!("expected JSON response");
        };
        assert_eq!(value["error"]["code"], -32601);
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn invalid_tool_arguments_are_self_correctable_tool_errors() {
        let (root, journal, logs) = fixtures();
        let control = FakeControl::default();
        let response = handle_message(
            br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"supervisor_read_logs","arguments":{"serviceId":"api","stream":"invalid"}}}"#,
            &control,
            &journal,
            &logs,
        );
        let McpResponse::Json(value) = response else {
            panic!("expected JSON response");
        };
        assert!(value.get("error").is_none());
        assert_eq!(value["result"]["isError"], true);
        assert!(
            value["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|message| message.contains("stdout or stderr"))
        );
        assert_eq!(*control.mutations.lock().expect("mutation lock"), 0);
        std::fs::remove_dir_all(root).ok();
    }
}
