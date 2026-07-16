use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::adapters::config::ConsoleEvents;
use crate::application::{
    ControlAction, ControlError, ControlErrorKind, ControlMutationOutcome, ControlMutationResult,
    ProcessExitEvent, ServiceSnapshot, SupervisorControl, TreeStopReport,
};
use crate::domain::{Actor, LifecycleState, Reason};

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
    ProcessExited,
    Unauthorized,
    SupervisorInternalError,
}

/// Audited lifecycle event, including observations not initiated by a control
/// mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalAction {
    Start,
    Stop,
    Restart,
    ProcessExit,
}

/// One deterministic JSONL lifecycle record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalRecord {
    pub sequence: u64,
    pub timestamp: String,
    pub supervisor_instance_id: String,
    pub service_id: String,
    pub action: JournalAction,
    pub actor: Actor,
    pub reason: Reason,
    pub previous_state: LifecycleState,
    pub resulting_state: LifecycleState,
    pub owned_pids_before: Vec<u32>,
    pub owned_pids_after: Vec<u32>,
    pub result: JournalResult,
    pub error_category: Option<ErrorCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_restart_planned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown: Option<TreeStopReport>,
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
    console_events: Option<ConsoleEvents>,
    event_publication: Mutex<()>,
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
            console_events: None,
            event_publication: Mutex::new(()),
        }
    }

    /// Mirrors persisted canonical lifecycle records to the visible foreground
    /// console at the configured detail level.
    #[must_use]
    pub const fn with_console_events(mut self, console_events: ConsoleEvents) -> Self {
        self.console_events = Some(console_events);
        self
    }

    fn publish_console_event(&self, record: &JournalRecord) {
        let Some(mode) = self.console_events else {
            return;
        };
        let Some(line) = format_console_event(record, mode) else {
            return;
        };
        if record.result == JournalResult::Failure {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    /// Persists one terminal process-tree observation before any automatic
    /// recovery mutation is attempted.
    ///
    /// # Errors
    ///
    /// Returns a bounded control error if the audit record cannot be written.
    pub fn record_process_exit(&self, event: &ProcessExitEvent) -> Result<(), ControlError> {
        let exit = event.exit_code.map_or_else(
            || "without a numeric exit code".to_owned(),
            |code| format!("with code {code}"),
        );
        let recovery = if event.automatic_restart_planned {
            "automatic on-failure restart planned"
        } else {
            "no automatic restart planned"
        };
        let reason = Reason::new(format!("owned process tree exited {exit}; {recovery}")).map_err(
            |error| ControlError::internal(format!("invalid process-exit reason: {error}")),
        )?;
        let _publication = self
            .event_publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = self
            .journal
            .append(JournalRecord {
                sequence: 0,
                timestamp: format!("unix-ms:{}", unix_milliseconds()),
                supervisor_instance_id: self.supervisor_instance_id.clone(),
                service_id: event.service_id.clone(),
                action: JournalAction::ProcessExit,
                actor: Actor::Recovery,
                reason,
                previous_state: event.previous_state,
                resulting_state: LifecycleState::Failed,
                owned_pids_before: event.owned_pids_before.clone(),
                owned_pids_after: Vec::new(),
                result: JournalResult::Failure,
                error_category: Some(ErrorCategory::ProcessExited),
                exit_code: event.exit_code,
                automatic_restart_planned: Some(event.automatic_restart_planned),
                shutdown: None,
                config_fingerprint: self.config_fingerprint.clone(),
            })
            .map_err(|error| {
                ControlError::internal(format!("failed to persist process-exit journal: {error}"))
            })?;
        self.publish_console_event(&record);
        Ok(())
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
    ) -> Result<ControlMutationResult, ControlError> {
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
        let (result, error_category) = match outcome.as_ref().map(|result| result.outcome) {
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
            exit_code: None,
            automatic_restart_planned: None,
            shutdown: outcome
                .as_ref()
                .ok()
                .and_then(|result| result.shutdown.clone()),
            config_fingerprint: self.config_fingerprint.clone(),
        };
        let _publication = self
            .event_publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = self.journal.append(record).map_err(|error| {
            ControlError::internal(format!("failed to persist lifecycle journal: {error}"))
        })?;
        self.publish_console_event(&record);
        outcome
    }
}

fn lifecycle_action(action: ControlAction) -> JournalAction {
    match action {
        ControlAction::Start => JournalAction::Start,
        ControlAction::Stop => JournalAction::Stop,
        ControlAction::Restart => JournalAction::Restart,
    }
}

fn format_console_event(record: &JournalRecord, mode: ConsoleEvents) -> Option<String> {
    if mode == ConsoleEvents::Off && record.result == JournalResult::Success {
        return None;
    }

    let action = journal_action_label(record.action);
    let actor = actor_label(record.actor);
    let previous = lifecycle_state_label(record.previous_state);
    let resulting = lifecycle_state_label(record.resulting_state);
    let outcome = event_outcome_label(record);
    let timestamp = console_timestamp(&record.timestamp);

    if mode != ConsoleEvents::Verbose {
        return Some(format!(
            "[{timestamp}] [event #{}] {} {action}: {previous} -> {resulting} ({actor}, {outcome})",
            record.sequence, record.service_id
        ));
    }

    let mut details = vec![
        format!("service={}", record.service_id),
        format!("action={action}"),
        format!("actor={actor}"),
        format!("state={previous}->{resulting}"),
        format!("result={}", journal_result_label(record.result)),
        format!("reason={:?}", record.reason.as_str()),
        format!(
            "pids={}->{}",
            record.owned_pids_before.len(),
            record.owned_pids_after.len()
        ),
    ];
    if let Some(category) = record.error_category {
        details.push(format!("category={}", error_category_label(category)));
    }
    if let Some(shutdown) = &record.shutdown {
        details.push(format!(
            "shutdown={}",
            if shutdown.forced {
                "forced"
            } else if shutdown.graceful_signal_sent && shutdown.graceful_signal_error.is_none() {
                "graceful"
            } else {
                "completed"
            }
        ));
    }
    if let Some(exit_code) = record.exit_code {
        details.push(format!("exitCode={exit_code}"));
    }
    Some(format!(
        "[{timestamp}] [event #{}] {}",
        record.sequence,
        details.join(" ")
    ))
}

fn console_timestamp(value: &str) -> String {
    if let Some(milliseconds) = value
        .strip_prefix("unix-ms:")
        .and_then(|value| value.parse::<u128>().ok())
        && let Some(formatted) = format_unix_milliseconds_utc(milliseconds)
    {
        return formatted;
    }
    if !value.is_empty()
        && value.len() <= 40
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'.' | b'+' | b'_')
        })
    {
        value.to_owned()
    } else {
        "timestamp-invalid".to_owned()
    }
}

fn format_unix_milliseconds_utc(milliseconds: u128) -> Option<String> {
    let total_seconds = milliseconds / 1_000;
    let days = i64::try_from(total_seconds / 86_400).ok()?;
    let seconds_in_day = u64::try_from(total_seconds % 86_400).ok()?;
    let shifted_days = days.checked_add(719_468)?;
    let era = shifted_days / 146_097;
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    let second = seconds_in_day % 60;
    let millisecond = milliseconds % 1_000;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}Z"
    ))
}

fn event_outcome_label(record: &JournalRecord) -> &'static str {
    if record.result == JournalResult::Failure {
        return record
            .error_category
            .map_or("failure", error_category_label);
    }
    if matches!(
        record.error_category,
        Some(ErrorCategory::AlreadyRunning | ErrorCategory::AlreadyStopped)
    ) {
        return record
            .error_category
            .map_or("success", error_category_label);
    }
    match &record.shutdown {
        Some(shutdown) if shutdown.forced => "forced",
        Some(shutdown)
            if shutdown.graceful_signal_sent && shutdown.graceful_signal_error.is_none() =>
        {
            "graceful"
        }
        _ => "success",
    }
}

const fn journal_action_label(action: JournalAction) -> &'static str {
    match action {
        JournalAction::Start => "start",
        JournalAction::Stop => "stop",
        JournalAction::Restart => "restart",
        JournalAction::ProcessExit => "process_exit",
    }
}

const fn journal_result_label(result: JournalResult) -> &'static str {
    match result {
        JournalResult::Success => "success",
        JournalResult::Failure => "failure",
    }
}

const fn actor_label(actor: Actor) -> &'static str {
    match actor {
        Actor::UserCli => "user/cli",
        Actor::UserUi => "user/ui",
        Actor::Agent => "agent/generic",
        Actor::Codex => "agent/codex",
        Actor::Recovery => "recovery/supervisor",
    }
}

const fn lifecycle_state_label(state: LifecycleState) -> &'static str {
    match state {
        LifecycleState::Stopped => "stopped",
        LifecycleState::Starting => "starting",
        LifecycleState::Running => "running",
        LifecycleState::Stopping => "stopping",
        LifecycleState::Unhealthy => "unhealthy",
        LifecycleState::Failed => "failed",
    }
}

const fn error_category_label(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::ConfigInvalid => "config_invalid",
        ErrorCategory::AlreadyRunning => "already_running",
        ErrorCategory::AlreadyStopped => "already_stopped",
        ErrorCategory::PortConflictExternal => "port_conflict_external",
        ErrorCategory::SpawnFailed => "spawn_failed",
        ErrorCategory::StartupTimeout => "startup_timeout",
        ErrorCategory::HealthFailed => "health_failed",
        ErrorCategory::ShutdownTimeout => "shutdown_timeout",
        ErrorCategory::OwnershipLost => "ownership_lost",
        ErrorCategory::ProcessExited => "process_exited",
        ErrorCategory::Unauthorized => "unauthorized",
        ErrorCategory::SupervisorInternalError => "supervisor_internal_error",
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
    use crate::adapters::config::ConsoleEvents;
    use crate::application::TreeStopReport;
    use crate::domain::{Actor, LifecycleState, Reason};

    use super::{
        ErrorCategory, FileJournal, JournalAction, JournalRecord, JournalResult,
        format_console_event, format_unix_milliseconds_utc,
    };

    fn record(reason: &str) -> JournalRecord {
        JournalRecord {
            sequence: 7,
            timestamp: "2026-07-13T12:00:00.000Z".to_owned(),
            supervisor_instance_id: "supervisor-1".to_owned(),
            service_id: "akusidecar".to_owned(),
            action: JournalAction::Restart,
            actor: Actor::Agent,
            reason: Reason::new(reason).expect("valid reason"),
            previous_state: LifecycleState::Running,
            resulting_state: LifecycleState::Running,
            owned_pids_before: vec![100, 101],
            owned_pids_after: vec![200, 201],
            result: JournalResult::Success,
            error_category: None,
            exit_code: None,
            automatic_restart_planned: None,
            shutdown: None,
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
    fn lifecycle_console_event_is_concise_and_sequence_backed() {
        let record = record("backend source changed");

        assert_eq!(
            format_console_event(&record, ConsoleEvents::Lifecycle).as_deref(),
            Some(
                "[2026-07-13T12:00:00.000Z] [event #7] akusidecar restart: running -> running (agent/generic, success)"
            )
        );
        assert_eq!(format_console_event(&record, ConsoleEvents::Off), None);
    }

    #[test]
    fn verbose_console_event_keeps_auditable_details_bounded() {
        let mut stopped = record("operator requested\nshutdown");
        stopped.action = JournalAction::Stop;
        stopped.actor = Actor::UserCli;
        stopped.resulting_state = LifecycleState::Stopped;
        stopped.owned_pids_after.clear();
        stopped.shutdown = Some(TreeStopReport {
            owned_pids_before: vec![100, 101],
            owned_pids_after: Vec::new(),
            graceful_signal_sent: true,
            graceful_signal_error: None,
            forced: false,
        });

        let lifecycle = format_console_event(&stopped, ConsoleEvents::Lifecycle)
            .expect("lifecycle console event");
        assert_eq!(
            lifecycle,
            "[2026-07-13T12:00:00.000Z] [event #7] akusidecar stop: running -> stopped (user/cli, graceful)"
        );

        let verbose =
            format_console_event(&stopped, ConsoleEvents::Verbose).expect("verbose console event");
        assert!(verbose.contains("actor=user/cli"));
        assert!(verbose.contains(r#"reason="operator requested\nshutdown""#));
        assert!(verbose.contains("pids=2->0"));
        assert!(verbose.contains("shutdown=graceful"));
        assert_eq!(verbose.lines().count(), 1);
    }

    #[test]
    fn off_console_mode_still_surfaces_failures() {
        let mut failed = record("spawn failed");
        failed.result = JournalResult::Failure;
        failed.resulting_state = LifecycleState::Failed;
        failed.error_category = Some(ErrorCategory::SpawnFailed);

        assert_eq!(
            format_console_event(&failed, ConsoleEvents::Off).as_deref(),
            Some(
                "[2026-07-13T12:00:00.000Z] [event #7] akusidecar restart: running -> failed (agent/generic, spawn_failed)"
            )
        );
    }

    #[test]
    fn unix_millisecond_console_timestamp_is_portable_rfc3339_utc() {
        assert_eq!(
            format_unix_milliseconds_utc(0).as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            format_unix_milliseconds_utc(1_721_044_800_123).as_deref(),
            Some("2024-07-15T12:00:00.123Z")
        );
    }

    #[test]
    fn malformed_timestamp_cannot_inject_an_extra_console_line() {
        let mut record = record("bounded console timestamp");
        record.timestamp = "malformed\ninjected".to_owned();

        let line = format_console_event(&record, ConsoleEvents::Lifecycle)
            .expect("lifecycle console event");

        assert!(line.starts_with("[timestamp-invalid] [event #7]"));
        assert_eq!(line.lines().count(), 1);
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
    fn process_exit_metadata_is_explicit_and_backward_compatible() {
        let mut exit_record = record("owned process tree exited with code 17");
        exit_record.action = JournalAction::ProcessExit;
        exit_record.exit_code = Some(17);
        exit_record.automatic_restart_planned = Some(true);
        exit_record.error_category = Some(ErrorCategory::ProcessExited);
        let line = exit_record
            .to_json_line(&[])
            .expect("record should serialize");

        assert!(line.contains("\"action\":\"process_exit\""));
        assert!(line.contains("\"exitCode\":17"));
        assert!(line.contains("\"automaticRestartPlanned\":true"));
        assert!(line.contains("\"errorCategory\":\"process_exited\""));
        let legacy = record("legacy lifecycle event")
            .to_json_line(&[])
            .expect("legacy-shaped record");
        assert!(!legacy.contains("exitCode"));
        assert!(!legacy.contains("automaticRestartPlanned"));
    }

    #[test]
    fn shutdown_evidence_is_explicit_and_backward_compatible() {
        let mut stopped = record("service stopped by operator");
        stopped.action = JournalAction::Stop;
        stopped.shutdown = Some(TreeStopReport {
            owned_pids_before: vec![100, 101],
            owned_pids_after: Vec::new(),
            graceful_signal_sent: true,
            graceful_signal_error: None,
            forced: false,
        });
        let line = stopped.to_json_line(&[]).expect("serialize stop record");

        assert!(line.contains("\"gracefulSignalSent\":true"));
        assert!(line.contains("\"forced\":false"));
        let legacy = record("legacy lifecycle event")
            .to_json_line(&[])
            .expect("legacy-shaped record");
        assert!(!legacy.contains("\"shutdown\""));
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
