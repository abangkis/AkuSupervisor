use serde::{Deserialize, Serialize};

use crate::domain::{Actor, LifecycleAction, LifecycleState, Reason};

/// Canonical lifecycle operation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalResult {
    Success,
    Failure,
}

/// Stable error categories exposed across every control adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    ConfigInvalid,
    AlreadyRunning,
    AlreadyStopped,
    PortConflictExternal,
    SpawnFailed,
    StartupTimeout,
    HealthFailed,
    ShutdownTimeout,
    OwnershipLost,
    Unauthorized,
    SupervisorInternalError,
}

/// One deterministic JSONL lifecycle record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalRecord {
    pub sequence: u64,
    pub timestamp: String,
    pub supervisor_instance_id: String,
    pub service_id: String,
    pub action: LifecycleAction,
    pub actor: Actor,
    pub reason: Reason,
    pub previous_state: LifecycleState,
    pub resulting_state: LifecycleState,
    pub owned_pids_before: Vec<u32>,
    pub owned_pids_after: Vec<u32>,
    pub result: JournalResult,
    pub error_category: Option<ErrorCategory>,
    pub config_fingerprint: String,
}

impl JournalRecord {
    /// Serializes exactly one newline-terminated JSONL record.
    ///
    /// # Errors
    ///
    /// Returns the underlying JSON serialization error.
    pub fn to_json_line(&self, known_secrets: &[&str]) -> Result<String, serde_json::Error> {
        let mut redacted = self.clone();
        let reason = redact_known_secrets(redacted.reason.as_str(), known_secrets);
        if let Ok(reason) = Reason::new(reason) {
            redacted.reason = reason;
        }
        let mut line = serde_json::to_string(&redacted)?;
        line.push('\n');
        Ok(line)
    }
}

#[must_use]
pub fn redact_known_secrets(value: &str, known_secrets: &[&str]) -> String {
    known_secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(value.to_owned(), |text, secret| {
            text.replace(secret, "[REDACTED]")
        })
}

#[cfg(test)]
mod tests {
    use crate::domain::{Actor, LifecycleAction, LifecycleState, Reason};

    use super::{ErrorCategory, JournalRecord, JournalResult};

    fn record(reason: &str) -> JournalRecord {
        JournalRecord {
            sequence: 7,
            timestamp: "2026-07-13T12:00:00.000Z".to_owned(),
            supervisor_instance_id: "supervisor-1".to_owned(),
            service_id: "akusidecar".to_owned(),
            action: LifecycleAction::Restart,
            actor: Actor::Agent,
            reason: Reason::new(reason).expect("valid reason"),
            previous_state: LifecycleState::Running,
            resulting_state: LifecycleState::Running,
            owned_pids_before: vec![100, 101],
            owned_pids_after: vec![200, 201],
            result: JournalResult::Success,
            error_category: None,
            config_fingerprint: "sha256:abc".to_owned(),
        }
    }

    #[test]
    fn journal_line_is_deterministic_and_newline_terminated() {
        let record = record("backend source changed");
        let first = record.to_json_line(&[]).expect("record should serialize");
        let second = record.to_json_line(&[]).expect("record should serialize");

        assert_eq!(first, second);
        assert!(first.ends_with('\n'));
        assert_eq!(first.lines().count(), 1);
    }

    #[test]
    fn journal_redacts_known_secrets_from_reason() {
        let record = record("restart after token secret-value expired");
        let line = record
            .to_json_line(&["secret-value"])
            .expect("record should serialize");

        assert!(!line.contains("secret-value"));
        assert!(line.contains("[REDACTED]"));
    }

    #[test]
    fn failure_category_uses_stable_snake_case_value() {
        let mut record = record("startup failed");
        record.result = JournalResult::Failure;
        record.error_category = Some(ErrorCategory::SpawnFailed);
        let line = record.to_json_line(&[]).expect("record should serialize");

        assert!(line.contains("\"errorCategory\":\"spawn_failed\""));
    }
}
