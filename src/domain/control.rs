use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Authenticated principal category used for policy and audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    UserCli,
    UserUi,
    Agent,
    Codex,
    Recovery,
}

impl Actor {
    #[must_use]
    pub const fn is_user(self) -> bool {
        matches!(self, Self::UserCli | Self::UserUi)
    }

    #[must_use]
    pub const fn is_agent(self) -> bool {
        matches!(self, Self::Agent | Self::Codex)
    }

    const fn identity(self) -> (&'static str, &'static str) {
        match self {
            Self::UserCli => ("user", "cli"),
            Self::UserUi => ("user", "ui"),
            Self::Agent => ("agent", "generic"),
            Self::Codex => ("agent", "codex"),
            Self::Recovery => ("recovery", "supervisor"),
        }
    }
}

impl Serialize for Actor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct ActorIdentity {
            actor_type: &'static str,
            actor_id: &'static str,
        }

        let (actor_type, actor_id) = self.identity();
        ActorIdentity {
            actor_type,
            actor_id,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct ActorIdentity {
            actor_type: String,
            actor_id: String,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ActorWire {
            Legacy(String),
            Identity(ActorIdentity),
        }

        let wire = ActorWire::deserialize(deserializer)?;
        let actor = match wire {
            ActorWire::Legacy(value) => match value.as_str() {
                "user_cli" => Some(Self::UserCli),
                "user_ui" => Some(Self::UserUi),
                "agent" => Some(Self::Agent),
                "codex" => Some(Self::Codex),
                "recovery" => Some(Self::Recovery),
                _ => None,
            },
            ActorWire::Identity(identity) => {
                match (identity.actor_type.as_str(), identity.actor_id.as_str()) {
                    ("user", "cli") => Some(Self::UserCli),
                    ("user", "ui") => Some(Self::UserUi),
                    ("agent", "generic") => Some(Self::Agent),
                    ("agent", "codex") => Some(Self::Codex),
                    ("recovery", "supervisor") => Some(Self::Recovery),
                    _ => None,
                }
            }
        };
        actor.ok_or_else(|| serde::de::Error::custom("unknown actor identity"))
    }
}

/// Typed lifecycle mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAction {
    Start,
    Stop,
    Restart,
}

/// Explicit operator override that agents cannot clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorHold {
    None,
    Running,
    Stopped,
}

/// Validated human-readable reason for a lifecycle mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Reason(String);

impl Reason {
    pub const MAX_BYTES: usize = 500;

    /// Creates a trimmed, bounded lifecycle reason.
    ///
    /// # Errors
    ///
    /// Returns [`ReasonError::Empty`] for blank text or
    /// [`ReasonError::TooLong`] when the byte limit is exceeded.
    pub fn new(value: impl Into<String>) -> Result<Self, ReasonError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ReasonError::Empty);
        }
        if trimmed.len() > Self::MAX_BYTES {
            return Err(ReasonError::TooLong {
                actual: trimmed.len(),
                maximum: Self::MAX_BYTES,
            });
        }
        Ok(Self(trimmed.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Reason validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonError {
    Empty,
    TooLong { actual: usize, maximum: usize },
}

impl fmt::Display for ReasonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("lifecycle reason must not be empty"),
            Self::TooLong { actual, maximum } => write!(
                formatter,
                "lifecycle reason is {actual} bytes; maximum is {maximum}"
            ),
        }
    }
}

impl std::error::Error for ReasonError {}

/// Per-service authority policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPolicy {
    operator_hold: OperatorHold,
    agent_start_allowed: bool,
}

impl Default for ControlPolicy {
    fn default() -> Self {
        Self {
            operator_hold: OperatorHold::None,
            agent_start_allowed: true,
        }
    }
}

impl ControlPolicy {
    #[must_use]
    pub const fn operator_hold(self) -> OperatorHold {
        self.operator_hold
    }

    #[must_use]
    pub const fn agent_start_allowed(self) -> bool {
        self.agent_start_allowed
    }

    pub const fn set_agent_start_allowed(&mut self, allowed: bool) {
        self.agent_start_allowed = allowed;
    }

    /// Checks whether an authenticated principal may perform an action.
    ///
    /// # Errors
    ///
    /// Returns an authority error when an agent-start policy or explicit user
    /// stop hold blocks the action.
    pub fn authorize(
        self,
        actor: Actor,
        action: LifecycleAction,
    ) -> Result<(), AuthorizationError> {
        if matches!(action, LifecycleAction::Start | LifecycleAction::Restart) {
            if self.operator_hold == OperatorHold::Stopped && !actor.is_user() {
                return Err(AuthorizationError::OperatorHoldStopped);
            }
            if actor.is_agent() && !self.agent_start_allowed {
                return Err(AuthorizationError::AgentStartDisabled);
            }
        }
        Ok(())
    }

    /// Applies an authorized user action to the durable operator override.
    pub const fn apply_user_action(&mut self, actor: Actor, action: LifecycleAction) {
        if !actor.is_user() {
            return;
        }
        self.operator_hold = match action {
            LifecycleAction::Start | LifecycleAction::Restart => OperatorHold::None,
            LifecycleAction::Stop => OperatorHold::Stopped,
        };
    }

    /// Makes an explicit user request that the service remain running.
    pub const fn hold_running(&mut self, actor: Actor) {
        if actor.is_user() {
            self.operator_hold = OperatorHold::Running;
        }
    }
}

/// Authorization failure for a typed lifecycle action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationError {
    OperatorHoldStopped,
    AgentStartDisabled,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperatorHoldStopped => {
                formatter.write_str("a user stop hold blocks agent and recovery start actions")
            }
            Self::AgentStartDisabled => {
                formatter.write_str("agent start and restart actions are disabled")
            }
        }
    }
}

impl std::error::Error for AuthorizationError {}

#[cfg(test)]
mod tests {
    use super::{Actor, AuthorizationError, ControlPolicy, LifecycleAction, OperatorHold, Reason};

    #[test]
    fn user_stop_blocks_agent_and_recovery_start() {
        let mut policy = ControlPolicy::default();
        policy.apply_user_action(Actor::UserUi, LifecycleAction::Stop);

        assert_eq!(policy.operator_hold(), OperatorHold::Stopped);
        assert_eq!(
            policy.authorize(Actor::Agent, LifecycleAction::Restart),
            Err(AuthorizationError::OperatorHoldStopped)
        );
        assert_eq!(
            policy.authorize(Actor::Recovery, LifecycleAction::Start),
            Err(AuthorizationError::OperatorHoldStopped)
        );
    }

    #[test]
    fn only_user_action_clears_user_stop_hold() {
        let mut policy = ControlPolicy::default();
        policy.apply_user_action(Actor::UserCli, LifecycleAction::Stop);
        policy.apply_user_action(Actor::Agent, LifecycleAction::Start);
        assert_eq!(policy.operator_hold(), OperatorHold::Stopped);

        policy.apply_user_action(Actor::UserCli, LifecycleAction::Start);
        assert_eq!(policy.operator_hold(), OperatorHold::None);
    }

    #[test]
    fn agent_stop_remains_allowed_when_agent_start_is_disabled() {
        let mut policy = ControlPolicy::default();
        policy.set_agent_start_allowed(false);

        assert!(
            policy
                .authorize(Actor::Agent, LifecycleAction::Stop)
                .is_ok()
        );
        assert_eq!(
            policy.authorize(Actor::Agent, LifecycleAction::Start),
            Err(AuthorizationError::AgentStartDisabled)
        );
    }

    #[test]
    fn reason_is_trimmed_and_bounded() {
        let reason = Reason::new("  backend configuration changed  ").expect("valid reason");
        assert_eq!(reason.as_str(), "backend configuration changed");
        assert!(Reason::new("   ").is_err());
        assert!(Reason::new("x".repeat(Reason::MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn actor_serializes_as_identity_and_reads_legacy_journals() {
        assert_eq!(
            serde_json::to_value(Actor::Codex).expect("serialize actor"),
            serde_json::json!({"actorType": "agent", "actorId": "codex"})
        );
        assert_eq!(
            serde_json::from_str::<Actor>(r#""agent""#).expect("legacy actor"),
            Actor::Agent
        );
        assert_eq!(
            serde_json::from_value::<Actor>(
                serde_json::json!({"actorType": "agent", "actorId": "codex"})
            )
            .expect("structured actor"),
            Actor::Codex
        );
    }
}
