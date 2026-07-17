use std::fmt;

use serde::{Deserialize, Serialize};

/// Observable state of one registered service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Stopped,
    Starting,
    Running,
    Stopping,
    TerminationPending,
    Unhealthy,
    Failed,
}

impl LifecycleState {
    /// Returns whether this state may transition directly to `next`.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Stopped | Self::Failed, Self::Starting)
                | (Self::Failed, Self::Stopping)
                | (
                    Self::Starting,
                    Self::Running | Self::Unhealthy | Self::Failed | Self::Stopping
                )
                | (
                    Self::Running,
                    Self::Stopping | Self::Unhealthy | Self::Failed
                )
                | (
                    Self::Unhealthy,
                    Self::Running | Self::Stopping | Self::Failed
                )
                | (
                    Self::Stopping,
                    Self::Stopped | Self::TerminationPending | Self::Failed
                )
                | (
                    Self::TerminationPending,
                    Self::Stopping | Self::Stopped | Self::Failed
                )
        )
    }

    /// Validates one direct lifecycle transition.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when `next` is not directly reachable from
    /// the current state.
    pub const fn transition_to(self, next: Self) -> Result<Self, TransitionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(TransitionError {
                previous: self,
                requested: next,
            })
        }
    }
}

/// Operator-requested steady state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

/// An attempted lifecycle transition that is not legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    pub previous: LifecycleState,
    pub requested: LifecycleState,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "illegal lifecycle transition from {:?} to {:?}",
            self.previous, self.requested
        )
    }
}

impl std::error::Error for TransitionError {}

#[cfg(test)]
mod tests {
    use super::LifecycleState;

    #[test]
    fn normal_start_and_stop_path_is_legal() {
        let state = LifecycleState::Stopped
            .transition_to(LifecycleState::Starting)
            .and_then(|state| state.transition_to(LifecycleState::Running))
            .and_then(|state| state.transition_to(LifecycleState::Stopping))
            .and_then(|state| state.transition_to(LifecycleState::Stopped));

        assert_eq!(state, Ok(LifecycleState::Stopped));
    }

    #[test]
    fn stopped_service_cannot_jump_directly_to_running() {
        let error = LifecycleState::Stopped
            .transition_to(LifecycleState::Running)
            .expect_err("spawn readiness must not be skipped");

        assert_eq!(error.previous, LifecycleState::Stopped);
        assert_eq!(error.requested, LifecycleState::Running);
    }

    #[test]
    fn unhealthy_service_can_recover_or_stop() {
        assert!(
            LifecycleState::Unhealthy
                .transition_to(LifecycleState::Running)
                .is_ok()
        );
        assert!(
            LifecycleState::Unhealthy
                .transition_to(LifecycleState::Stopping)
                .is_ok()
        );
    }

    #[test]
    fn forced_cleanup_can_complete_asynchronously() {
        let state = LifecycleState::Running
            .transition_to(LifecycleState::Stopping)
            .and_then(|state| state.transition_to(LifecycleState::TerminationPending))
            .and_then(|state| state.transition_to(LifecycleState::Stopped));

        assert_eq!(state, Ok(LifecycleState::Stopped));
        assert!(
            LifecycleState::TerminationPending
                .transition_to(LifecycleState::Stopping)
                .is_ok()
        );
    }
}
