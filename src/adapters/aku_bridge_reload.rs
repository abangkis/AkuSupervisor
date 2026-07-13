use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::application::{
    CooperativeActionControl, CooperativeActionError, CooperativeActionOutcome,
    CooperativeActionProgress, CooperativeActionStage, CooperativeActionStatus,
};
use crate::domain::{Actor, Reason};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct AkuBridgeReloadClient {
    address: SocketAddr,
    origin: String,
    timeout: Duration,
    poll_interval: Duration,
    audit: Mutex<CooperativeAudit>,
    configuration_fingerprint: String,
}

impl AkuBridgeReloadClient {
    /// Creates a loopback-only cooperative reload adapter and its audit sink.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-loopback origin or an unavailable audit path.
    pub fn new(
        origin: &str,
        timeout: Duration,
        poll_interval: Duration,
        audit_path: &Path,
        configuration_fingerprint: String,
    ) -> Result<Self, CooperativeActionError> {
        let address = parse_loopback_origin(origin)?;
        Ok(Self {
            address,
            origin: origin.trim_end_matches('/').to_owned(),
            timeout,
            poll_interval,
            audit: Mutex::new(CooperativeAudit::open(audit_path)?),
            configuration_fingerprint,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the relay state machine is kept contiguous so stage transitions remain auditable"
    )]
    fn execute(
        &self,
        actor: Actor,
        reason: &Reason,
        request_id: &str,
        progress: &(dyn Fn(CooperativeActionProgress) + Send + Sync),
    ) -> Result<CooperativeActionOutcome, CooperativeActionError> {
        let bootstrap = self.request("GET", "/api/bootstrap", &[], None)?;
        let token = bootstrap
            .get("bridgeToken")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CooperativeActionError::new(
                    "relay_contract",
                    "Sidecar bootstrap omitted bridgeToken",
                )
            })?;
        let contract = bootstrap
            .get("bridgeContractVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CooperativeActionError::new(
                    "relay_contract",
                    "Sidecar bootstrap omitted bridgeContractVersion",
                )
            })?;
        let headers = bridge_headers(token, contract);
        let created = self.request(
            "POST",
            "/api/operations/bridge/actions/reload-self",
            &headers,
            Some(&json!({
                "requestId": request_id,
                "actor": actor,
                "reason": reason.as_str(),
            })),
        )?;
        let action = created.get("action").ok_or_else(|| {
            CooperativeActionError::new("relay_contract", "Sidecar omitted cooperative action")
        })?;
        let action_id = action
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CooperativeActionError::new("relay_contract", "Sidecar action omitted ID")
            })?
            .to_owned();
        let mut last_stage = CooperativeActionStage::RelayCreated;
        let mut last_observed_build_id = string_field(action, "observedBuildId");
        self.report_progress(
            actor,
            reason,
            request_id,
            CooperativeActionProgress {
                stage: last_stage,
                relay_action_id: Some(action_id.clone()),
                expected_build_id: string_field(action, "expectedBuildId"),
                observed_build_id: last_observed_build_id.clone(),
                message: "Sidecar created the bounded reload_self relay action".to_owned(),
            },
            progress,
        )?;
        let started = Instant::now();
        loop {
            let response = self.request(
                "GET",
                &format!("/api/operations/bridge/actions/{action_id}"),
                &headers,
                None,
            )?;
            let action = response.get("action").ok_or_else(|| {
                CooperativeActionError::new("relay_contract", "Sidecar status omitted action")
            })?;
            match action.get("status").and_then(Value::as_str) {
                Some("completed") => {
                    let observed_build_id = string_field(action, "observedBuildId");
                    if observed_build_id.is_some() && observed_build_id != last_observed_build_id {
                        self.report_progress(
                            actor,
                            reason,
                            request_id,
                            progress_from_action(
                                action,
                                CooperativeActionStage::HeartbeatObserved,
                                "Sidecar observed the expected post-reload heartbeat",
                            ),
                            progress,
                        )?;
                    }
                    return Ok(outcome_from_action(
                        action,
                        CooperativeActionStatus::Completed,
                    ));
                }
                Some("failed") => {
                    return Err(CooperativeActionError::new(
                        sidecar_error_category(action),
                        action
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("AkuBridge reload_self failed"),
                    )
                    .with_context(
                        Some(action_id.clone()),
                        string_field(action, "observedBuildId"),
                    ));
                }
                Some("delivered") if last_stage == CooperativeActionStage::RelayCreated => {
                    last_stage = CooperativeActionStage::Delivered;
                    self.report_progress(
                        actor,
                        reason,
                        request_id,
                        progress_from_action(
                            action,
                            last_stage,
                            "AkuBrowser relay page claimed the cooperative action",
                        ),
                        progress,
                    )?;
                }
                Some("pending" | "delivered") => {}
                Some("accepted") => {
                    if matches!(
                        last_stage,
                        CooperativeActionStage::RelayCreated | CooperativeActionStage::Delivered
                    ) {
                        last_stage = CooperativeActionStage::Accepted;
                        self.report_progress(
                            actor,
                            reason,
                            request_id,
                            progress_from_action(
                                action,
                                last_stage,
                                "AkuBridge accepted reload_self",
                            ),
                            progress,
                        )?;
                    }
                    let observed_build_id = string_field(action, "observedBuildId");
                    if observed_build_id.is_some() && observed_build_id != last_observed_build_id {
                        last_observed_build_id = observed_build_id;
                        self.report_progress(
                            actor,
                            reason,
                            request_id,
                            progress_from_action(
                                action,
                                CooperativeActionStage::HeartbeatObserved,
                                "Sidecar observed a post-acceptance heartbeat",
                            ),
                            progress,
                        )?;
                    }
                }
                _ => {
                    return Err(CooperativeActionError::new(
                        "relay_contract",
                        "Sidecar returned an unknown action status",
                    ));
                }
            }
            if started.elapsed() >= self.timeout {
                return Err(CooperativeActionError::new(
                    "relay_timeout",
                    "AkuBridge reload_self did not produce the expected heartbeat before the supervisor deadline",
                )
                .with_context(Some(action_id.clone()), last_observed_build_id));
            }
            thread::sleep(self.poll_interval);
        }
    }

    fn request(
        &self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, CooperativeActionError> {
        let body = body.map_or_else(Vec::new, |value| {
            serde_json::to_vec(value).expect("JSON serialization cannot fail")
        });
        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(2))
            .map_err(|error| {
                CooperativeActionError::new(
                    "relay_unreachable",
                    format!("cannot connect to {}: {error}", self.origin),
                )
            })?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            self.address,
            body.len(),
        )
        .map_err(io_error)?;
        for (name, value) in headers {
            write!(stream, "{name}: {value}\r\n").map_err(io_error)?;
        }
        stream.write_all(b"\r\n").map_err(io_error)?;
        stream.write_all(&body).map_err(io_error)?;
        stream.flush().map_err(io_error)?;
        let mut response = Vec::new();
        stream
            .take(MAX_RESPONSE_BYTES as u64)
            .read_to_end(&mut response)
            .map_err(io_error)?;
        parse_response(&response)
    }

    fn audit_record(
        &self,
        actor: Actor,
        reason: &Reason,
        request_id: &str,
        status: &str,
        result: Option<&Result<CooperativeActionOutcome, CooperativeActionError>>,
    ) -> Result<(), CooperativeActionError> {
        let mut audit = self.audit.lock().map_err(|_| {
            CooperativeActionError::new("audit_unavailable", "cooperative audit lock is poisoned")
        })?;
        audit.append(CooperativeAuditRecord {
            sequence: 0,
            timestamp: unix_timestamp(),
            target: "aku-bridge".to_owned(),
            action: "reload_self".to_owned(),
            actor,
            reason: reason.as_str().to_owned(),
            request_id: request_id.to_owned(),
            configuration_fingerprint: self.configuration_fingerprint.clone(),
            status: status.to_owned(),
            relay_action_id: result.and_then(|value| match value {
                Ok(outcome) => outcome.relay_action_id.clone(),
                Err(error) => error.relay_action_id().map(str::to_owned),
            }),
            expected_build_id: result
                .and_then(|value| value.as_ref().ok())
                .and_then(|value| value.expected_build_id.clone()),
            observed_build_id: result.and_then(|value| match value {
                Ok(outcome) => outcome.observed_build_id.clone(),
                Err(error) => error.observed_build_id().map(str::to_owned),
            }),
            error_category: result
                .and_then(|value| value.as_ref().err())
                .map(|error| error.category().to_owned()),
            message: result.map_or_else(
                || "authenticated reload_self request accepted for relay".to_owned(),
                |value| {
                    value.as_ref().map_or_else(
                        |error| error.message().to_owned(),
                        |outcome| outcome.message.clone(),
                    )
                },
            ),
        })
    }

    fn report_progress(
        &self,
        actor: Actor,
        reason: &Reason,
        request_id: &str,
        update: CooperativeActionProgress,
        progress: &(dyn Fn(CooperativeActionProgress) + Send + Sync),
    ) -> Result<(), CooperativeActionError> {
        let mut audit = self.audit.lock().map_err(|_| {
            CooperativeActionError::new("audit_unavailable", "cooperative audit lock is poisoned")
        })?;
        audit.append(CooperativeAuditRecord {
            sequence: 0,
            timestamp: unix_timestamp(),
            target: "aku-bridge".to_owned(),
            action: "reload_self".to_owned(),
            actor,
            reason: reason.as_str().to_owned(),
            request_id: request_id.to_owned(),
            configuration_fingerprint: self.configuration_fingerprint.clone(),
            status: stage_name(update.stage).to_owned(),
            relay_action_id: update.relay_action_id.clone(),
            expected_build_id: update.expected_build_id.clone(),
            observed_build_id: update.observed_build_id.clone(),
            error_category: None,
            message: update.message.clone(),
        })?;
        drop(audit);
        progress(update);
        Ok(())
    }
}

impl CooperativeActionControl for AkuBridgeReloadClient {
    fn reload_aku_bridge(
        &self,
        actor: Actor,
        reason: Reason,
        request_id: &str,
        progress: &(dyn Fn(CooperativeActionProgress) + Send + Sync),
    ) -> Result<CooperativeActionOutcome, CooperativeActionError> {
        self.audit_record(actor, &reason, request_id, "requested", None)?;
        let result = self.execute(actor, &reason, request_id, progress);
        self.audit_record(
            actor,
            &reason,
            request_id,
            if result.is_ok() {
                "completed"
            } else {
                "failed"
            },
            Some(&result),
        )?;
        result
    }
}

fn bridge_headers<'a>(token: &'a str, contract: &'a str) -> [(&'static str, &'a str); 3] {
    [
        ("X-Aku-Bridge-Token", token),
        ("X-Aku-Bridge-Id", "aku-supervisor"),
        ("X-Aku-Bridge-Contract", contract),
    ]
}

fn outcome_from_action(
    action: &Value,
    status: CooperativeActionStatus,
) -> CooperativeActionOutcome {
    CooperativeActionOutcome {
        target: "aku-bridge".to_owned(),
        action: "reload_self".to_owned(),
        status,
        relay_action_id: string_field(action, "id"),
        previous_build_id: string_field(action, "previousBuildId"),
        expected_build_id: string_field(action, "expectedBuildId"),
        observed_build_id: string_field(action, "observedBuildId"),
        message: string_field(action, "message")
            .unwrap_or_else(|| "AkuBridge reload_self completed".to_owned()),
    }
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(Value::as_str).map(str::to_owned)
}

fn progress_from_action(
    action: &Value,
    stage: CooperativeActionStage,
    message: &str,
) -> CooperativeActionProgress {
    CooperativeActionProgress {
        stage,
        relay_action_id: string_field(action, "id"),
        expected_build_id: string_field(action, "expectedBuildId"),
        observed_build_id: string_field(action, "observedBuildId"),
        message: message.to_owned(),
    }
}

fn sidecar_error_category(action: &Value) -> &'static str {
    match action.get("errorCategory").and_then(Value::as_str) {
        Some("relay_page_stale") => "relay_page_stale",
        Some("relay_not_delivered") => "relay_not_delivered",
        Some("extension_not_accepted") => "extension_not_accepted",
        Some("reload_heartbeat_timeout") => "reload_heartbeat_timeout",
        Some("build_mismatch") => "build_mismatch",
        _ => "relay_failed",
    }
}

const fn stage_name(stage: CooperativeActionStage) -> &'static str {
    match stage {
        CooperativeActionStage::Requested => "requested",
        CooperativeActionStage::RelayCreated => "relay_created",
        CooperativeActionStage::Delivered => "delivered",
        CooperativeActionStage::Accepted => "accepted",
        CooperativeActionStage::HeartbeatObserved => "heartbeat_observed",
        CooperativeActionStage::Completed => "completed",
        CooperativeActionStage::Failed => "failed",
    }
}

fn parse_loopback_origin(origin: &str) -> Result<SocketAddr, CooperativeActionError> {
    let authority = origin
        .strip_prefix("http://")
        .and_then(|value| (!value.contains('/')).then_some(value))
        .ok_or_else(|| {
            CooperativeActionError::new(
                "invalid_configuration",
                "AkuBridge Sidecar origin must be an HTTP origin without a path",
            )
        })?;
    let address = authority.parse::<SocketAddr>().map_err(|_| {
        CooperativeActionError::new(
            "invalid_configuration",
            "AkuBridge Sidecar origin must contain an explicit loopback port",
        )
    })?;
    if !address.ip().is_loopback() {
        return Err(CooperativeActionError::new(
            "invalid_configuration",
            "AkuBridge Sidecar origin must be loopback",
        ));
    }
    Ok(address)
}

fn parse_response(response: &[u8]) -> Result<Value, CooperativeActionError> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| {
            CooperativeActionError::new(
                "relay_protocol",
                "Sidecar returned an invalid HTTP response",
            )
        })?;
    let headers = String::from_utf8_lossy(&response[..separator]);
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| {
            CooperativeActionError::new("relay_protocol", "Sidecar response omitted HTTP status")
        })?;
    let payload: Value = serde_json::from_slice(&response[separator + 4..]).map_err(|error| {
        CooperativeActionError::new(
            "relay_protocol",
            format!("Sidecar returned invalid JSON: {error}"),
        )
    })?;
    if !(200..300).contains(&status) {
        let message = payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Sidecar rejected the cooperative action");
        return Err(CooperativeActionError::new("relay_rejected", message));
    }
    Ok(payload)
}

fn io_error(error: impl std::fmt::Display) -> CooperativeActionError {
    CooperativeActionError::new("relay_io", error.to_string())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CooperativeAuditRecord {
    sequence: u64,
    timestamp: u64,
    target: String,
    action: String,
    actor: Actor,
    reason: String,
    request_id: String,
    configuration_fingerprint: String,
    status: String,
    relay_action_id: Option<String>,
    expected_build_id: Option<String>,
    observed_build_id: Option<String>,
    error_category: Option<String>,
    message: String,
}

#[derive(Debug)]
struct CooperativeAudit {
    path: PathBuf,
    next_sequence: u64,
}

impl CooperativeAudit {
    fn open(path: &Path) -> Result<Self, CooperativeActionError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let next_sequence = if path.exists() {
            let source = fs::read_to_string(path).map_err(io_error)?;
            source
                .lines()
                .filter_map(|line| serde_json::from_str::<CooperativeAuditRecord>(line).ok())
                .map(|record| record.sequence)
                .max()
                .unwrap_or(0)
                + 1
        } else {
            1
        };
        File::options()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io_error)?;
        Ok(Self {
            path: path.to_owned(),
            next_sequence,
        })
    }

    fn append(&mut self, mut record: CooperativeAuditRecord) -> Result<(), CooperativeActionError> {
        record.sequence = self.next_sequence;
        let line = serde_json::to_vec(&record)
            .map_err(|error| CooperativeActionError::new("audit_unavailable", error.to_string()))?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(audit_error)?;
        file.write_all(&line).map_err(audit_error)?;
        file.write_all(b"\n").map_err(audit_error)?;
        file.flush().map_err(audit_error)?;
        self.next_sequence += 1;
        Ok(())
    }
}

fn audit_error(error: impl std::fmt::Display) -> CooperativeActionError {
    CooperativeActionError::new("audit_unavailable", error.to_string())
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_loopback_origin, parse_response, sidecar_error_category};

    #[test]
    fn relay_accepts_only_pathless_loopback_http_origins() {
        assert!(parse_loopback_origin("http://127.0.0.1:47821").is_ok());
        assert!(parse_loopback_origin("http://127.0.0.1:47821/path").is_err());
        assert!(parse_loopback_origin("https://127.0.0.1:47821").is_err());
        assert!(parse_loopback_origin("http://192.168.1.2:47821").is_err());
    }

    #[test]
    fn relay_rejects_non_success_json_responses() {
        let response = b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\r\n{\"message\":\"denied\"}";
        assert_eq!(parse_response(response).unwrap_err().message(), "denied");
    }

    #[test]
    fn relay_preserves_sidecar_stage_error_taxonomy() {
        for category in [
            "relay_page_stale",
            "relay_not_delivered",
            "extension_not_accepted",
            "reload_heartbeat_timeout",
            "build_mismatch",
        ] {
            assert_eq!(
                sidecar_error_category(&json!({"errorCategory": category})),
                category
            );
        }
        assert_eq!(sidecar_error_category(&json!({})), "relay_failed");
    }
}
