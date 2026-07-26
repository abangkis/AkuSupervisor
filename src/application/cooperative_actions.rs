use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{Actor, Reason};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooperativeActionStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CooperativeActionStage {
    Requested,
    RelayCreated,
    Delivered,
    Accepted,
    HeartbeatObserved,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CooperativeActionProgress {
    pub stage: CooperativeActionStage,
    pub relay_action_id: Option<String>,
    pub expected_build_id: Option<String>,
    pub observed_build_id: Option<String>,
    pub message: String,
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
    /// Returns the configured cooperative target.
    fn target(&self) -> &str;

    /// Requests the single bounded cooperative action.
    ///
    /// # Errors
    ///
    /// Returns a categorized error when the authenticated relay, target
    /// acknowledgement, or expected completion evidence cannot be proven.
    fn execute(
        &self,
        actor: Actor,
        reason: Reason,
        request_id: &str,
        progress: &(dyn Fn(CooperativeActionProgress) + Send + Sync),
    ) -> Result<CooperativeActionOutcome, CooperativeActionError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooperativeActionError {
    category: &'static str,
    message: String,
    relay_action_id: Option<String>,
    observed_build_id: Option<String>,
}

impl CooperativeActionError {
    #[must_use]
    pub fn new(category: &'static str, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            relay_action_id: None,
            observed_build_id: None,
        }
    }

    #[must_use]
    pub fn with_context(
        mut self,
        relay_action_id: Option<String>,
        observed_build_id: Option<String>,
    ) -> Self {
        self.relay_action_id = relay_action_id;
        self.observed_build_id = observed_build_id;
        self
    }

    #[must_use]
    pub const fn category(&self) -> &'static str {
        self.category
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn relay_action_id(&self) -> Option<&str> {
        self.relay_action_id.as_deref()
    }

    #[must_use]
    pub fn observed_build_id(&self) -> Option<&str> {
        self.observed_build_id.as_deref()
    }
}

impl fmt::Display for CooperativeActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CooperativeActionError {}
