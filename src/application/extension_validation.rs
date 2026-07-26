use serde::Serialize;
use serde_json::{Value, json};

const REQUIRED_AUDIT_STAGES: [&str; 6] = [
    "requested",
    "relay_created",
    "delivered",
    "accepted",
    "heartbeat_observed",
    "completed",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionValidationCheck {
    pub id: &'static str,
    pub passed: bool,
    pub expected: Value,
    pub actual: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionValidationReport {
    pub schema_version: u32,
    pub command: &'static str,
    pub status: &'static str,
    pub exit_code: u8,
    pub request_id: String,
    pub actor: Value,
    pub checks: Vec<ExtensionValidationCheck>,
    pub operation: Value,
}

impl ExtensionValidationReport {
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.exit_code == 0
    }
}

#[must_use]
pub fn validate_extension_release(
    request_id: &str,
    actor: Value,
    operation: Value,
    active_operation: Value,
    audit_records: &[Value],
) -> ExtensionValidationReport {
    let audit_stages = audit_records
        .iter()
        .filter_map(|record| record.get("status").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let audit_identity_matches = audit_records.iter().all(|record| {
        record.get("requestId").and_then(Value::as_str) == Some(request_id)
            && record.get("actor") == Some(&actor)
    });
    let expected_build = operation
        .get("expectedBuildId")
        .cloned()
        .unwrap_or(Value::Null);
    let observed_build = operation
        .get("observedBuildId")
        .cloned()
        .unwrap_or(Value::Null);
    let checks = vec![
        ExtensionValidationCheck {
            id: "cooperative_reload_completed",
            passed: operation.get("status").and_then(Value::as_str) == Some("completed")
                && operation.get("stage").and_then(Value::as_str) == Some("completed"),
            expected: json!({"status": "completed", "stage": "completed"}),
            actual: json!({
                "status": operation.get("status").cloned().unwrap_or(Value::Null),
                "stage": operation.get("stage").cloned().unwrap_or(Value::Null),
            }),
        },
        ExtensionValidationCheck {
            id: "six_stage_audit",
            passed: audit_stages == REQUIRED_AUDIT_STAGES,
            expected: json!(REQUIRED_AUDIT_STAGES),
            actual: json!(audit_stages),
        },
        ExtensionValidationCheck {
            id: "actor_and_request_identity",
            passed: operation.get("requestId").and_then(Value::as_str) == Some(request_id)
                && operation.get("actor") == Some(&actor)
                && audit_records.len() == REQUIRED_AUDIT_STAGES.len()
                && audit_identity_matches,
            expected: json!({"requestId": request_id, "actor": actor}),
            actual: json!({
                "operationRequestId": operation.get("requestId").cloned().unwrap_or(Value::Null),
                "operationActor": operation.get("actor").cloned().unwrap_or(Value::Null),
                "auditRecordCount": audit_records.len(),
                "auditIdentityMatches": audit_identity_matches,
            }),
        },
        ExtensionValidationCheck {
            id: "expected_observed_heartbeat",
            passed: !expected_build.is_null() && expected_build == observed_build,
            expected: expected_build.clone(),
            actual: observed_build,
        },
        ExtensionValidationCheck {
            id: "no_zombie_action",
            passed: active_operation.is_null(),
            expected: Value::Null,
            actual: active_operation,
        },
    ];
    let passed = checks.iter().all(|check| check.passed);
    ExtensionValidationReport {
        schema_version: 1,
        command: "extension_validate",
        status: if passed { "passed" } else { "failed" },
        exit_code: u8::from(!passed),
        request_id: request_id.to_owned(),
        actor,
        checks,
        operation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> Value {
        json!({"actorType": "agent", "actorId": "codex"})
    }

    fn operation() -> Value {
        json!({
            "requestId": "validate-1",
            "actor": actor(),
            "status": "completed",
            "stage": "completed",
            "expectedBuildId": "build-2",
            "observedBuildId": "build-2"
        })
    }

    fn audit() -> Vec<Value> {
        REQUIRED_AUDIT_STAGES
            .iter()
            .map(|stage| {
                json!({
                    "requestId": "validate-1",
                    "actor": actor(),
                    "status": stage,
                })
            })
            .collect()
    }

    #[test]
    fn complete_release_evidence_passes_with_exit_zero() {
        let report =
            validate_extension_release("validate-1", actor(), operation(), Value::Null, &audit());
        assert!(report.passed());
        assert_eq!(report.exit_code, 0);
        assert!(report.checks.iter().all(|check| check.passed));
    }

    #[test]
    fn missing_stage_or_active_operation_fails_deterministically() {
        let mut incomplete = audit();
        incomplete.remove(3);
        let report = validate_extension_release(
            "validate-1",
            actor(),
            operation(),
            json!({"requestId": "zombie"}),
            &incomplete,
        );
        assert!(!report.passed());
        assert_eq!(report.exit_code, 1);
        assert_eq!(report.status, "failed");
        assert_eq!(report.checks[1].id, "six_stage_audit");
        assert!(!report.checks[1].passed);
        assert!(!report.checks[4].passed);
    }
}
