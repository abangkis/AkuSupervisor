//! Portable tailing and console formatting for the append-only registration audit.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::console_time::{DisplayTimezone, format_unix_milliseconds};

const MAX_EVENT_BYTES: usize = 16 * 1024;
const MAX_POLL_BYTES: u64 = 256 * 1024;

/// Secret-free registration identity persisted by the registration authority.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationAuditEvent {
    pub schema_version: u32,
    pub timestamp_unix_ms: u64,
    pub event: String,
    pub actor: Option<String>,
    pub draft_id: String,
    pub request_id: String,
    pub operation: String,
    pub service_id: String,
    pub proposed_revision: String,
}

impl RegistrationAuditEvent {
    /// Formats one bounded, single-line control-plane event for the foreground console.
    #[must_use]
    pub fn console_line(&self, timezone: DisplayTimezone) -> String {
        let timestamp = format_unix_milliseconds(u128::from(self.timestamp_unix_ms), timezone)
            .unwrap_or_else(|| "timestamp-invalid".to_owned());
        format!(
            "[{timestamp}] [registration] {} {} {} by {} (draft={}, request={}, revision={}){}",
            console_token(&self.event),
            console_token(&self.operation),
            console_token(&self.service_id),
            console_actor(self.actor.as_deref()),
            console_token(&self.draft_id),
            console_token(&self.request_id),
            short_revision(&self.proposed_revision),
            event_state_note(&self.event),
        )
    }
}

fn event_state_note(event: &str) -> &'static str {
    match event {
        "approved" | "approval_recovered" => {
            "; authorization recorded, configuration unchanged until commit"
        }
        "committed" | "commit_recovered" => "; transaction finalized",
        _ => "",
    }
}

/// Follows only records appended after the foreground Supervisor starts.
#[derive(Debug)]
pub struct RegistrationAuditTail {
    path: PathBuf,
    offset: u64,
}

impl RegistrationAuditTail {
    /// Opens a follower at the current end of the audit, avoiding historical replay.
    #[must_use]
    pub fn follow(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let offset = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        Self { path, offset }
    }

    /// Reads complete newly appended JSONL records without consuming a partial line.
    ///
    /// # Errors
    ///
    /// Returns a bounded file, oversized-record, or JSON parse failure.
    pub fn poll(&mut self) -> Result<Vec<RegistrationAuditEvent>, RegistrationEventError> {
        let length = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(RegistrationEventError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if length < self.offset {
            self.offset = 0;
        }
        if length == self.offset {
            return Ok(Vec::new());
        }

        let mut file = File::open(&self.path).map_err(|source| RegistrationEventError::Io {
            path: self.path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| RegistrationEventError::Io {
                path: self.path.clone(),
                source,
            })?;
        let mut bytes = Vec::new();
        file.take((length - self.offset).min(MAX_POLL_BYTES))
            .read_to_end(&mut bytes)
            .map_err(|source| RegistrationEventError::Io {
                path: self.path.clone(),
                source,
            })?;
        let Some(complete_length) = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|at| at + 1)
        else {
            if bytes.len() > MAX_EVENT_BYTES {
                return Err(RegistrationEventError::Oversized);
            }
            return Ok(Vec::new());
        };

        let mut events = Vec::new();
        for line in bytes[..complete_length].split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            if line.len() > MAX_EVENT_BYTES {
                return Err(RegistrationEventError::Oversized);
            }
            let event = serde_json::from_slice::<RegistrationAuditEvent>(line)
                .map_err(RegistrationEventError::Parse)?;
            if event.schema_version != 1 {
                return Err(RegistrationEventError::UnsupportedSchema(
                    event.schema_version,
                ));
            }
            events.push(event);
        }
        self.offset += u64::try_from(complete_length).unwrap_or(u64::MAX);
        Ok(events)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn console_actor(actor: Option<&str>) -> &'static str {
    match actor {
        Some("human_cli") => "user/human_cli",
        Some("registration_mcp") => "agent/registration_mcp",
        _ => "unknown/legacy",
    }
}

fn console_token(value: &str) -> String {
    value
        .chars()
        .take(96)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '/')
            {
                character
            } else {
                '?'
            }
        })
        .collect()
}

fn short_revision(revision: &str) -> String {
    if revision.len() <= 19 {
        return console_token(revision);
    }
    format!("{}...", console_token(&revision[..19]))
}

/// Failure while following the append-only registration audit.
#[derive(Debug)]
pub enum RegistrationEventError {
    Io { path: PathBuf, source: io::Error },
    Parse(serde_json::Error),
    Oversized,
    UnsupportedSchema(u32),
}

impl fmt::Display for RegistrationEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Parse(error) => {
                write!(formatter, "registration audit record is invalid: {error}")
            }
            Self::Oversized => formatter.write_str("registration audit record exceeds 16 KiB"),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported registration audit schema: {version}"
                )
            }
        }
    }
}

impl std::error::Error for RegistrationEventError {}

#[cfg(test)]
mod tests {
    use crate::adapters::console_time::DisplayTimezone;

    use std::fs::{self, OpenOptions};
    use std::io::Write;

    use super::RegistrationAuditTail;

    #[test]
    fn follower_skips_history_and_defers_partial_lines() {
        let path = std::env::temp_dir().join(format!(
            "aku-supervisor-registration-events-{}.jsonl",
            std::process::id()
        ));
        fs::write(
            &path,
            record("prepared", "registration_mcp", 1_721_044_800_123),
        )
        .expect("write historical record");
        let mut tail = RegistrationAuditTail::follow(&path);
        assert!(tail.poll().expect("skip history").is_empty());

        let approved = record("approved", "human_cli", 1_721_044_800_124);
        let split = approved.len() / 2;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open audit");
        file.write_all(&approved.as_bytes()[..split])
            .expect("write partial record");
        file.flush().expect("flush partial record");
        assert!(tail.poll().expect("defer partial record").is_empty());
        file.write_all(&approved.as_bytes()[split..])
            .expect("complete record");
        file.flush().expect("flush complete record");

        let events = tail.poll().expect("read appended event");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].console_line(DisplayTimezone::Utc),
            "[2024-07-15T12:00:00.124Z] [registration] approved register api by user/human_cli (draft=registration-abc, request=request-1, revision=sha256:123456789abc...); authorization recorded, configuration unchanged until commit"
        );
        drop(file);
        fs::remove_file(path).ok();
    }

    fn record(event: &str, actor: &str, timestamp: u64) -> String {
        format!(
            "{{\"schemaVersion\":1,\"timestampUnixMs\":{timestamp},\"event\":\"{event}\",\"actor\":\"{actor}\",\"draftId\":\"registration-abc\",\"requestId\":\"request-1\",\"operation\":\"register\",\"serviceId\":\"api\",\"proposedRevision\":\"sha256:123456789abcdef\"}}\n"
        )
    }
}
