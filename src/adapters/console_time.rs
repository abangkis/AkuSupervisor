//! Human-facing timestamp rendering.
//!
//! Persistence and machine contracts remain UTC. This adapter converts only
//! terminal presentation into the reader's local offset when requested.

use chrono::{DateTime, Local, SecondsFormat, Utc};

/// Timezone used only for human-readable terminal output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DisplayTimezone {
    #[default]
    Local,
    Utc,
}

impl DisplayTimezone {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "utc" => Some(Self::Utc),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Utc => "utc",
        }
    }
}

/// Formats a Unix millisecond timestamp for human-facing output.
#[must_use]
pub fn format_unix_milliseconds(milliseconds: u128, timezone: DisplayTimezone) -> Option<String> {
    let milliseconds = i64::try_from(milliseconds).ok()?;
    let utc = DateTime::<Utc>::from_timestamp_millis(milliseconds)?;
    match timezone {
        DisplayTimezone::Utc => Some(utc.to_rfc3339_opts(SecondsFormat::Millis, true)),
        DisplayTimezone::Local => Some(
            utc.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S%.3f %:z")
                .to_string(),
        ),
    }
}

/// Formats a persisted canonical timestamp without accepting control characters.
#[must_use]
pub fn format_persisted_timestamp(value: &str, timezone: DisplayTimezone) -> String {
    if let Some(milliseconds) = value
        .strip_prefix("unix-ms:")
        .and_then(|value| value.parse::<u128>().ok())
    {
        return format_unix_milliseconds(milliseconds, timezone)
            .unwrap_or_else(|| "timestamp-invalid".to_owned());
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        let utc = timestamp.with_timezone(&Utc);
        return match timezone {
            DisplayTimezone::Utc => utc.to_rfc3339_opts(SecondsFormat::Millis, true),
            DisplayTimezone::Local => utc
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S%.3f %:z")
                .to_string(),
        };
    }
    "timestamp-invalid".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{DisplayTimezone, format_persisted_timestamp, format_unix_milliseconds};

    #[test]
    fn utc_rendering_is_stable_and_machine_compatible() {
        assert_eq!(
            format_unix_milliseconds(1_721_044_800_123, DisplayTimezone::Utc).as_deref(),
            Some("2024-07-15T12:00:00.123Z")
        );
        assert_eq!(
            format_persisted_timestamp("2024-07-15T12:00:00.123Z", DisplayTimezone::Utc),
            "2024-07-15T12:00:00.123Z"
        );
    }

    #[test]
    fn local_rendering_always_exposes_an_explicit_offset() {
        let rendered = format_unix_milliseconds(1_721_044_800_123, DisplayTimezone::Local)
            .expect("valid local time");
        assert_eq!(rendered.len(), 30);
        assert!(matches!(rendered.as_bytes()[24], b'+' | b'-'));
        assert_eq!(rendered.as_bytes()[27], b':');
        assert!(rendered.as_bytes()[29].is_ascii_digit());
    }

    #[test]
    fn malformed_persisted_value_cannot_reach_the_console() {
        assert_eq!(
            format_persisted_timestamp("malformed\ninjected", DisplayTimezone::Local),
            "timestamp-invalid"
        );
    }
}
