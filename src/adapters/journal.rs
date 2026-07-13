use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::application::{
    ControlAction, ControlError, ControlErrorKind, ControlMutationOutcome, ServiceSnapshot,
    SupervisorControl,
};
use crate::domain::{Actor, LifecycleAction, LifecycleState, Reason};

const MAX_EVENT_LIMIT: usize = 200;

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

/// Append-only JSONL persistence with a sequence that survives supervisor
/// restarts.
#[derive(Debug)]
pub struct FileJournal {
    path: PathBuf,
    state: Mutex<JournalState>,
    known_secrets: Vec<String>,
}

impl FileJournal {
    /// Opens or creates a journal, rejecting malformed or non-monotonic history.
    ///
    /// # Errors
    ///
    /// Returns a filesystem or schema error when existing audit history cannot
    /// be trusted.
    pub fn open(
        path: impl Into<PathBuf>,
        known_secrets: impl IntoIterator<Item = String>,
    ) -> Result<Self, FileJournalError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| FileJournalError::CreateDirectory {
                path: parent.to_owned(),
                source,
            })?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| FileJournalError::Open {
                path: path.clone(),
                source,
            })?;
        let records = read_records(&path)?;
        let mut previous = 0_u64;
        for record in &records {
            if record.sequence <= previous {
                return Err(FileJournalError::NonMonotonic {
                    path: path.clone(),
                    sequence: record.sequence,
                });
            }
            previous = record.sequence;
        }
        Ok(Self {
            path,
            state: Mutex::new(JournalState {
                next_sequence: previous.saturating_add(1).max(1),
            }),
            known_secrets: known_secrets.into_iter().collect(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Durably appends one canonical lifecycle record.
    ///
    /// # Errors
    ///
    /// Returns a lock, serialization, open, write, or flush error.
    pub fn append(&self, mut record: JournalRecord) -> Result<JournalRecord, FileJournalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| FileJournalError::LockPoisoned)?;
        record.sequence = state.next_sequence;
        let secrets = self
            .known_secrets
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let line = record
            .to_json_line(&secrets)
            .map_err(FileJournalError::Serialize)?;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|source| FileJournalError::Open {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.sync_data())
            .map_err(|source| FileJournalError::Write {
                path: self.path.clone(),
                source,
            })?;
        state.next_sequence = state.next_sequence.saturating_add(1);
        Ok(record)
    }

    /// Reads a bounded page of records after an exclusive sequence cursor.
    ///
    /// # Errors
    ///
    /// Returns a read or schema error if persisted history is malformed.
    pub fn events(&self, after: u64, limit: usize) -> Result<Vec<JournalRecord>, FileJournalError> {
        Ok(read_records(&self.path)?
            .into_iter()
            .filter(|record| record.sequence > after)
            .take(limit.clamp(1, MAX_EVENT_LIMIT))
            .collect())
    }
}

#[derive(Debug)]
struct JournalState {
    next_sequence: u64,
}

fn read_records(path: &Path) -> Result<Vec<JournalRecord>, FileJournalError> {
    let file = File::open(path).map_err(|source| FileJournalError::Open {
        path: path.to_owned(),
        source,
    })?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            result => Some((index, result)),
        })
        .map(|(index, line)| {
            let line = line.map_err(|source| FileJournalError::Read {
                path: path.to_owned(),
                source,
            })?;
            serde_json::from_str(&line).map_err(|source| FileJournalError::Parse {
                path: path.to_owned(),
                line: index + 1,
                source,
            })
        })
        .collect()
}

/// Shared lifecycle control decorator used by both interactive and HTTP
/// mutations so they produce identical canonical records.
pub struct AuditedControl {
    inner: Arc<dyn SupervisorControl>,
    journal: Arc<FileJournal>,
    supervisor_instance_id: String,
    config_fingerprint: String,
}

impl AuditedControl {
    #[must_use]
    pub fn new(
        inner: Arc<dyn SupervisorControl>,
        journal: Arc<FileJournal>,
        config_fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            journal,
            supervisor_instance_id: format!("{}-{}", std::process::id(), unix_milliseconds()),
            config_fingerprint: config_fingerprint.into(),
        }
    }
}

impl fmt::Debug for AuditedControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuditedControl")
            .field("journal", &self.journal.path())
            .field("supervisor_instance_id", &self.supervisor_instance_id)
            .field("config_fingerprint", &self.config_fingerprint)
            .finish_non_exhaustive()
    }
}

impl SupervisorControl for AuditedControl {
    fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
        self.inner.snapshots()
    }

    fn mutate(
        &self,
        action: ControlAction,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ControlMutationOutcome, ControlError> {
        let before = self
            .inner
            .snapshots()?
            .into_iter()
            .find(|service| service.id == service_id);
        let outcome = self.inner.mutate(action, service_id, actor, reason.clone());
        let after = self.inner.snapshots().ok().and_then(|services| {
            services
                .into_iter()
                .find(|service| service.id == service_id)
        });
        let (result, error_category) = match &outcome {
            Ok(ControlMutationOutcome::AlreadyRunning) => {
                (JournalResult::Success, Some(ErrorCategory::AlreadyRunning))
            }
            Ok(ControlMutationOutcome::AlreadyStopped) => {
                (JournalResult::Success, Some(ErrorCategory::AlreadyStopped))
            }
            Ok(_) => (JournalResult::Success, None),
            Err(error) => (
                JournalResult::Failure,
                Some(match error.kind() {
                    ControlErrorKind::Unauthorized => ErrorCategory::Unauthorized,
                    ControlErrorKind::PortConflictExternal => ErrorCategory::PortConflictExternal,
                    ControlErrorKind::SpawnFailed => ErrorCategory::SpawnFailed,
                    ControlErrorKind::HealthFailed => ErrorCategory::HealthFailed,
                    ControlErrorKind::ShutdownTimeout => ErrorCategory::ShutdownTimeout,
                    ControlErrorKind::OwnershipLost => ErrorCategory::OwnershipLost,
                    ControlErrorKind::ServiceNotFound | ControlErrorKind::Internal => {
                        ErrorCategory::SupervisorInternalError
                    }
                }),
            ),
        };
        let record = JournalRecord {
            sequence: 0,
            timestamp: format!("unix-ms:{}", unix_milliseconds()),
            supervisor_instance_id: self.supervisor_instance_id.clone(),
            service_id: service_id.to_owned(),
            action: lifecycle_action(action),
            actor,
            reason,
            previous_state: before
                .as_ref()
                .map_or(LifecycleState::Stopped, |snapshot| snapshot.lifecycle),
            resulting_state: after
                .as_ref()
                .or(before.as_ref())
                .map_or(LifecycleState::Stopped, |snapshot| snapshot.lifecycle),
            owned_pids_before: before
                .as_ref()
                .map_or_else(Vec::new, |snapshot| snapshot.owned_pids.clone()),
            owned_pids_after: after
                .as_ref()
                .map_or_else(Vec::new, |snapshot| snapshot.owned_pids.clone()),
            result,
            error_category,
            config_fingerprint: self.config_fingerprint.clone(),
        };
        self.journal.append(record).map_err(|error| {
            ControlError::internal(format!("failed to persist lifecycle journal: {error}"))
        })?;
        outcome
    }
}

fn lifecycle_action(action: ControlAction) -> LifecycleAction {
    match action {
        ControlAction::Start => LifecycleAction::Start,
        ControlAction::Stop => LifecycleAction::Stop,
        ControlAction::Restart => LifecycleAction::Restart,
    }
}

fn unix_milliseconds() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Debug)]
pub enum FileJournalError {
    CreateDirectory {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Parse {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    Serialize(serde_json::Error),
    NonMonotonic {
        path: PathBuf,
        sequence: u64,
    },
    LockPoisoned,
}

impl fmt::Display for FileJournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDirectory { path, source } => {
                write!(
                    formatter,
                    "failed to create journal directory {}: {source}",
                    path.display()
                )
            }
            Self::Open { path, source } => {
                write!(
                    formatter,
                    "failed to open journal {}: {source}",
                    path.display()
                )
            }
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read journal {}: {source}",
                    path.display()
                )
            }
            Self::Write { path, source } => {
                write!(
                    formatter,
                    "failed to write journal {}: {source}",
                    path.display()
                )
            }
            Self::Parse { path, line, source } => write!(
                formatter,
                "invalid journal record at {}:{line}: {source}",
                path.display()
            ),
            Self::Serialize(source) => write!(formatter, "failed to serialize journal: {source}"),
            Self::NonMonotonic { path, sequence } => write!(
                formatter,
                "journal {} has non-monotonic sequence {sequence}",
                path.display()
            ),
            Self::LockPoisoned => formatter.write_str("journal lock is poisoned"),
        }
    }
}

impl std::error::Error for FileJournalError {}

#[cfg(test)]
mod tests {
    use crate::domain::{Actor, LifecycleAction, LifecycleState, Reason};

    use super::{ErrorCategory, FileJournal, JournalRecord, JournalResult};

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

    #[test]
    fn file_journal_preserves_sequence_and_redaction_across_reopen() {
        let path = std::env::temp_dir().join(format!(
            "aku-supervisor-journal-{}-sequence.jsonl",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        let journal =
            FileJournal::open(&path, ["secret-value".to_owned()]).expect("create journal");
        let first = journal
            .append(record("restart after secret-value changed"))
            .expect("append first record");
        assert_eq!(first.sequence, 1);
        drop(journal);

        let reopened =
            FileJournal::open(&path, ["secret-value".to_owned()]).expect("reopen journal");
        let second = reopened
            .append(record("second request"))
            .expect("append second record");
        assert_eq!(second.sequence, 2);
        let events = reopened.events(1, 10).expect("read event page");
        assert_eq!(events, vec![second]);
        let persisted = std::fs::read_to_string(&path).expect("read journal file");
        assert!(!persisted.contains("secret-value"));
        std::fs::remove_file(path).ok();
    }
}
