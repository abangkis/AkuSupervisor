use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Platform-neutral health contract attached to a registered service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthCheckSpec {
    Process,
    HttpStatus {
        url: String,
        expected_status: u16,
        timeout: Duration,
        startup_deadline: Duration,
    },
    HttpJson {
        url: String,
        timeout: Duration,
        startup_deadline: Duration,
        expect: BTreeMap<String, serde_json::Value>,
    },
}

impl HealthCheckSpec {
    #[must_use]
    pub const fn startup_deadline(&self) -> Duration {
        match self {
            Self::Process => Duration::ZERO,
            Self::HttpStatus {
                startup_deadline, ..
            }
            | Self::HttpJson {
                startup_deadline, ..
            } => *startup_deadline,
        }
    }

    #[must_use]
    pub const fn timeout(&self) -> Option<Duration> {
        match self {
            Self::Process => None,
            Self::HttpStatus { timeout, .. } | Self::HttpJson { timeout, .. } => Some(*timeout),
        }
    }
}

/// Stable health state exposed through every control adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Unknown,
    Healthy,
    Unhealthy,
}

/// Latest bounded health observation for one service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthSnapshot {
    pub status: HealthStatus,
    pub process_ready: bool,
    pub transport_ready: Option<bool>,
    pub checked_at_unix_ms: Option<u64>,
    pub detail: Option<String>,
}

impl HealthSnapshot {
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            status: HealthStatus::Unknown,
            process_ready: false,
            transport_ready: None,
            checked_at_unix_ms: None,
            detail: None,
        }
    }

    #[must_use]
    pub fn healthy(process_ready: bool, transport_ready: Option<bool>, detail: String) -> Self {
        Self {
            status: HealthStatus::Healthy,
            process_ready,
            transport_ready,
            checked_at_unix_ms: Some(unix_milliseconds()),
            detail: Some(detail),
        }
    }

    #[must_use]
    pub fn unhealthy(process_ready: bool, transport_ready: Option<bool>, detail: String) -> Self {
        Self {
            status: HealthStatus::Unhealthy,
            process_ready,
            transport_ready,
            checked_at_unix_ms: Some(unix_milliseconds()),
            detail: Some(detail),
        }
    }

    #[must_use]
    pub const fn is_healthy(&self) -> bool {
        matches!(self.status, HealthStatus::Healthy)
    }
}

/// HTTP probe result before process readiness is combined by the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportHealth {
    pub transport_ready: bool,
    pub healthy: bool,
    pub detail: String,
}

/// Portable boundary for transport-specific health evaluation.
pub trait HealthProbe: fmt::Debug + Send + Sync {
    fn probe(&self, check: &HealthCheckSpec, timeout: Duration) -> TransportHealth;
}

fn unix_milliseconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
