use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{Actor, Reason};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooperativeActionStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CooperativeActionOutcome {
    pub target: String,
    pub action: String,
    pub status: CooperativeActionStatus,
    pub relay_action_id: Option<String>,
    pub previous_build_id: Option<String>,
    pub expected_build_id: Option<String>,
    pub observed_build_id: Option<String>,
    pub message: String,
}

pub trait CooperativeActionControl: Send + Sync {
    /// Requests the single bounded `AkuBridge` self-reload operation.
    ///
    /// # Errors
    ///
    /// Returns a categorized error when the authenticated relay, extension
    /// acknowledgement, or expected post-reload heartbeat cannot be proven.
    fn reload_aku_bridge(
        &self,
        actor: Actor,
        reason: Reason,
        request_id: &str,
    ) -> Result<CooperativeActionOutcome, CooperativeActionError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooperativeActionError {
    category: &'static str,
    message: String,
}

impl CooperativeActionError {
    #[must_use]
    pub fn new(category: &'static str, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn category(&self) -> &'static str {
        self.category
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CooperativeActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CooperativeActionError {}
