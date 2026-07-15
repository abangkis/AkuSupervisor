//! Separate stdio MCP authority for human-gated service registration.
//!
//! This server never starts managed processes and deliberately exposes no
//! approval tool. Human approval is available only through the interactive
//! CLI in `registration.rs`.

use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::config::ServiceConfig;
use super::mcp::{MCP_PROTOCOL_VERSION, supports_protocol_version};
use super::registration::{
    PrepareRegistration, RegistrationAuthority, RegistrationError, RegistrationOperation,
};

const MAX_STDIO_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 512 * 1024;

/// Runs the separate newline-delimited stdio registration MCP server.
///
/// # Errors
///
/// Returns a configuration, registration persistence, stdin, or stdout failure.
pub fn run(explicit_config: Option<PathBuf>) -> Result<(), RegistrationMcpError> {
    let authority =
        RegistrationAuthority::open(explicit_config).map_err(RegistrationMcpError::Registration)?;
    serve(io::stdin().lock(), io::stdout().lock(), &authority)
}

fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    authority: &RegistrationAuthority,
) -> Result<(), RegistrationMcpError> {
    let mut line = String::new();
    loop {
        line.clear();
        let count = input
            .read_line(&mut line)
            .map_err(RegistrationMcpError::ReadStdin)?;
        if count == 0 {
            return Ok(());
        }
        let body = line.trim_end_matches(['\r', '\n']).as_bytes();
        let response = if body.len() > MAX_STDIO_MESSAGE_BYTES {
            rpc_error(
                &Value::Null,
                -32600,
                "MCP message exceeds bounded body size",
            )
        } else {
            match handle_message(body, authority) {
                McpResponse::Json(value) => value,
                McpResponse::Accepted => continue,
            }
        };
        serde_json::to_writer(&mut output, &response).map_err(RegistrationMcpError::Serialize)?;
        output
            .write_all(b"\n")
            .and_then(|()| output.flush())
            .map_err(RegistrationMcpError::WriteStdout)?;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpResponse {
    Json(Value),
    Accepted,
}

fn handle_message(body: &[u8], authority: &RegistrationAuthority) -> McpResponse {
    let message: Value = match serde_json::from_slice(body) {
        Ok(message) => message,
        Err(_) => return McpResponse::Json(rpc_error(&Value::Null, -32700, "Parse error")),
    };
    let Some(object) = message.as_object() else {
        return McpResponse::Json(rpc_error(&Value::Null, -32600, "Invalid Request"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return McpResponse::Json(rpc_error(&request_id(object), -32600, "Invalid Request"));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return McpResponse::Accepted;
    };
    let Some(id) = object.get("id") else {
        return McpResponse::Accepted;
    };
    if !matches!(id, Value::String(_) | Value::Number(_)) {
        return McpResponse::Json(rpc_error(&Value::Null, -32600, "Invalid Request"));
    }
    let id = id.clone();
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => initialize(&params),
        "ping" => empty_params(&params).map(|()| json!({})),
        "tools/list" => list_tools(&params),
        "tools/call" => call_tool(&params, authority),
        _ => return McpResponse::Json(rpc_error(&id, -32601, "Method not found")),
    };
    match result {
        Ok(value) => McpResponse::Json(json!({"jsonrpc":"2.0","id":id,"result":value})),
        Err(message) => McpResponse::Json(rpc_error(&id, -32602, &message)),
    }
}

fn initialize(params: &Value) -> Result<Value, String> {
    let object = object(params, "initialize params")?;
    let requested = required_string(object, "protocolVersion")?;
    let protocol_version = if supports_protocol_version(requested) {
        requested
    } else {
        MCP_PROTOCOL_VERSION
    };
    Ok(json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools":{}},
        "serverInfo": {
            "name":"aku-supervisor-registration",
            "title":"AkuSupervisor Registration (human-gated)",
            "version":crate::VERSION,
            "description":"Prepare and commit revision-bound service registration drafts. Human approval is intentionally unavailable through MCP."
        },
        "instructions":"Begin with get_capabilities and get_schema. Validate a complete service definition, prepare a draft, ask the user to run the exact approvalCommand in an interactive terminal, then commit. Registration never auto-starts a service. Update and unregister fail unless the running Supervisor proves the target is stopped."
    }))
}

fn list_tools(params: &Value) -> Result<Value, String> {
    let object = object(params, "tools/list params")?;
    if object.keys().any(|key| key != "_meta") {
        return Err("pagination is not required for the bounded tool list".to_owned());
    }
    Ok(json!({"tools":[
        tool("supervisor_registration_get_capabilities", "Get registration capabilities", "Get the complete workflow, current configuration revision, safety policy, commands, and available tools.", &json!({"type":"object","properties":{},"additionalProperties":false}), true, false, true),
        tool("supervisor_registration_get_schema", "Get service schema", "Get the complete JSON Schema and example for a service definition without reading external documentation.", &json!({"type":"object","properties":{},"additionalProperties":false}), true, false, true),
        tool("supervisor_registration_validate_service", "Validate service change", "Validate a register, update, or unregister proposal against the current complete configuration without creating a draft.", &change_input_schema(false), true, false, true),
        tool("supervisor_registration_prepare_change", "Prepare registration draft", "Create an expiring, revision-bound, hash-bound draft. This does not change the configuration and cannot approve itself.", &change_input_schema(true), false, false, false),
        tool("supervisor_registration_get_draft", "Get registration draft", "Inspect the complete persisted draft, full before/after configuration, warnings, status, confirmation phrase, and approval command.", &draft_input_schema(), true, false, true),
        tool("supervisor_registration_commit_change", "Commit approved registration", "Atomically commit a previously human-approved draft after exact revision and stopped-state checks. Never auto-starts the service.", &draft_input_schema(), false, true, false)
    ]}))
}

fn tool(
    name: &str,
    title: &str,
    description: &str,
    input_schema: &Value,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> Value {
    json!({
        "name":name,
        "title":title,
        "description":description,
        "inputSchema":input_schema,
        "execution":{"taskSupport":"forbidden"},
        "annotations":{
            "title":title,
            "readOnlyHint":read_only,
            "destructiveHint":destructive,
            "idempotentHint":idempotent,
            "openWorldHint":false
        }
    })
}

fn change_input_schema(prepare: bool) -> Value {
    let mut properties = json!({
        "operation":{"type":"string","enum":["register","update","unregister"]},
        "serviceId":{"type":"string","pattern":"^[a-z0-9-]+$"},
        "service":{"type":"object","description":"Complete service object from get_schema; omit for unregister."}
    });
    let mut required = vec!["operation", "serviceId"];
    properties["service"] = RegistrationAuthority::schema();
    if prepare {
        properties["requestId"] =
            json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.:-]+$"});
        properties["baseRevision"] = json!({"type":"string","pattern":"^sha256:[a-f0-9]{64}$"});
        required.extend(["requestId", "baseRevision"]);
    }
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn draft_input_schema() -> Value {
    json!({
        "type":"object",
        "properties":{"draftId":{"type":"string","pattern":"^registration-[a-f0-9]{20}$"}},
        "required":["draftId"],
        "additionalProperties":false
    })
}

fn call_tool(params: &Value, authority: &RegistrationAuthority) -> Result<Value, String> {
    let params_object = object(params, "tools/call params")?;
    reject_unknown(params_object, &["name", "arguments", "_meta"])?;
    let name = required_string(params_object, "name")?;
    let arguments = params_object
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let arguments = object(&arguments, "arguments")?;
    let result = match name {
        "supervisor_registration_get_capabilities" => {
            reject_unknown(arguments, &[])?;
            authority.capabilities()
        }
        "supervisor_registration_get_schema" => {
            reject_unknown(arguments, &[])?;
            Ok(json!({"serviceSchema":RegistrationAuthority::schema()}))
        }
        "supervisor_registration_validate_service" => validate_tool(authority, arguments),
        "supervisor_registration_prepare_change" => prepare_tool(authority, arguments),
        "supervisor_registration_get_draft" => {
            reject_unknown(arguments, &["draftId"])?;
            let draft_id = required_string(arguments, "draftId")?;
            authority.get_draft(draft_id).map(|draft| {
                json!({
                    "draft":draft,
                    "approvalCommand":format!("aku-supervisor registration approve {draft_id}"),
                    "approvalAvailableThroughMcp":false
                })
            })
        }
        "supervisor_registration_commit_change" => {
            reject_unknown(arguments, &["draftId"])?;
            authority
                .commit(required_string(arguments, "draftId")?)
                .map(|commit| json!({"commit":commit}))
        }
        _ => return Err("unknown registration tool".to_owned()),
    };
    Ok(match result {
        Ok(value) => tool_success(&value),
        Err(error) => tool_failure(&error),
    })
}

fn validate_tool(
    authority: &RegistrationAuthority,
    arguments: &Map<String, Value>,
) -> Result<Value, RegistrationError> {
    reject_registration_unknown(arguments, &["operation", "serviceId", "service"])?;
    let operation = registration_operation(arguments)?;
    let service = service_definition(arguments)?;
    authority
        .validate_service(
            operation,
            registration_string(arguments, "serviceId")?,
            service,
        )
        .map(|validation| json!({"validation":validation}))
}

fn prepare_tool(
    authority: &RegistrationAuthority,
    arguments: &Map<String, Value>,
) -> Result<Value, RegistrationError> {
    reject_registration_unknown(
        arguments,
        &[
            "requestId",
            "operation",
            "serviceId",
            "baseRevision",
            "service",
        ],
    )?;
    let request = PrepareRegistration {
        request_id: registration_string(arguments, "requestId")?.to_owned(),
        operation: registration_operation(arguments)?,
        service_id: registration_string(arguments, "serviceId")?.to_owned(),
        base_revision: registration_string(arguments, "baseRevision")?.to_owned(),
        service: service_definition(arguments)?,
    };
    authority.prepare(request).map(|draft| {
        let draft_id = draft.draft_id.clone();
        json!({
            "draft":draft,
            "approvalCommand":format!("aku-supervisor registration approve {draft_id}"),
            "approvalAvailableThroughMcp":false,
            "nextStep":"Ask the user to run approvalCommand in a real interactive terminal after reviewing the complete configuration."
        })
    })
}

fn registration_operation(
    arguments: &Map<String, Value>,
) -> Result<RegistrationOperation, RegistrationError> {
    match registration_string(arguments, "operation")? {
        "register" => Ok(RegistrationOperation::Register),
        "update" => Ok(RegistrationOperation::Update),
        "unregister" => Ok(RegistrationOperation::Unregister),
        _ => Err(registration_input_error(
            "operation must be register, update, or unregister",
        )),
    }
}

fn service_definition(
    arguments: &Map<String, Value>,
) -> Result<Option<ServiceConfig>, RegistrationError> {
    arguments
        .get("service")
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|error| {
                registration_input_error(&format!(
                    "service does not match the strict schema: {error}"
                ))
            })
        })
        .transpose()
}

fn registration_string<'a>(
    arguments: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, RegistrationError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| registration_input_error(&format!("{name} must be a non-empty string")))
}

fn reject_registration_unknown(
    arguments: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), RegistrationError> {
    if let Some(key) = arguments
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        Err(registration_input_error(&format!("unknown field: {key}")))
    } else {
        Ok(())
    }
}

fn registration_input_error(message: &str) -> RegistrationError {
    RegistrationError::input(message)
}

fn tool_success(value: &Value) -> Value {
    let text = serde_json::to_string(&value).expect("JSON value serialization cannot fail");
    if text.len() > MAX_RESULT_BYTES {
        return json!({"content":[{"type":"text","text":"result exceeded the bounded registration MCP response size"}],"isError":true});
    }
    json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":false})
}

fn tool_failure(error: &RegistrationError) -> Value {
    let structured = json!({"error":error.structured()});
    json!({
        "content":[{"type":"text","text":error.to_string()}],
        "structuredContent":structured,
        "isError":true
    })
}

fn empty_params(value: &Value) -> Result<(), String> {
    let object = object(value, "params")?;
    reject_unknown(object, &[])
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))
}

fn required_string<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    object
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} must be a non-empty string"))
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Err(format!("unknown field: {key}"))
    } else {
        Ok(())
    }
}

fn request_id(object: &Map<String, Value>) -> Value {
    object
        .get("id")
        .filter(|id| matches!(id, Value::String(_) | Value::Number(_)))
        .cloned()
        .unwrap_or(Value::Null)
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

#[derive(Debug)]
pub enum RegistrationMcpError {
    Registration(RegistrationError),
    ReadStdin(io::Error),
    WriteStdout(io::Error),
    Serialize(serde_json::Error),
}

impl fmt::Display for RegistrationMcpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registration(error) => error.fmt(formatter),
            Self::ReadStdin(error) => {
                write!(formatter, "failed to read registration MCP stdin: {error}")
            }
            Self::WriteStdout(error) => write!(
                formatter,
                "failed to write registration MCP stdout: {error}"
            ),
            Self::Serialize(error) => write!(
                formatter,
                "failed to serialize registration MCP response: {error}"
            ),
        }
    }
}

impl std::error::Error for RegistrationMcpError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_discovery_is_complete_but_has_no_approval_tool() {
        let listed = list_tools(&json!({})).expect("tool list");
        let tools = listed["tools"].as_array().expect("tools array");
        let names = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();

        assert_eq!(names.len(), 6);
        assert!(names.contains(&"supervisor_registration_get_capabilities"));
        assert!(names.contains(&"supervisor_registration_get_schema"));
        assert!(names.contains(&"supervisor_registration_validate_service"));
        assert!(names.contains(&"supervisor_registration_prepare_change"));
        assert!(names.contains(&"supervisor_registration_get_draft"));
        assert!(names.contains(&"supervisor_registration_commit_change"));
        assert!(!names.iter().any(|name| name.contains("approve")));
        let commit = tools
            .iter()
            .find(|tool| tool["name"] == "supervisor_registration_commit_change")
            .expect("commit tool");
        assert_eq!(commit["annotations"]["destructiveHint"], true);
        assert_eq!(commit["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn service_schema_documents_every_strict_service_field() {
        let schema = RegistrationAuthority::schema();
        let required = schema["required"].as_array().expect("required fields");
        for field in [
            "label",
            "cwd",
            "command",
            "health",
            "restartPolicy",
            "shutdownGraceMs",
        ] {
            assert!(required.iter().any(|value| value == field));
        }
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["health"]["oneOf"]
                .as_array()
                .map(Vec::len),
            Some(4)
        );
    }
}
