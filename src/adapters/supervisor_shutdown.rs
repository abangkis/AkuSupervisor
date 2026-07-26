//! Authenticated requests for foreground Supervisor shutdown.

use std::fmt;
use std::sync::Mutex;

use crate::domain::{Actor, Reason};

/// One accepted request that will enter the ordinary foreground cleanup path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorShutdownRequest {
    pub actor: Actor,
    pub reason: Reason,
    pub request_id: String,
}

/// Process-local shutdown request registry.
#[derive(Debug, Default)]
pub struct SupervisorShutdown {
    request: Mutex<Option<SupervisorShutdownRequest>>,
}

impl SupervisorShutdown {
    /// Accepts one request idempotently and rejects conflicting shutdowns.
    ///
    /// # Errors
    ///
    /// Returns a conflict or poisoned-lock error.
    pub fn request(
        &self,
        request: SupervisorShutdownRequest,
    ) -> Result<SupervisorShutdownOutcome, SupervisorShutdownError> {
        let mut current = self
            .request
            .lock()
            .map_err(|_| SupervisorShutdownError::LockPoisoned)?;
        match current.as_ref() {
            Some(existing) if existing == &request => {
                Ok(SupervisorShutdownOutcome::AlreadyAccepted)
            }
            Some(_) => Err(SupervisorShutdownError::AlreadyInProgress),
            None => {
                *current = Some(request);
                Ok(SupervisorShutdownOutcome::Accepted)
            }
        }
    }

    /// Returns the accepted request without clearing idempotency state.
    ///
    /// # Errors
    ///
    /// Returns a poisoned-lock error.
    pub fn accepted(&self) -> Result<Option<SupervisorShutdownRequest>, SupervisorShutdownError> {
        self.request
            .lock()
            .map(|request| request.clone())
            .map_err(|_| SupervisorShutdownError::LockPoisoned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorShutdownOutcome {
    Accepted,
    AlreadyAccepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorShutdownError {
    AlreadyInProgress,
    LockPoisoned,
}

impl fmt::Display for SupervisorShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInProgress => {
                formatter.write_str("a different Supervisor shutdown request is already active")
            }
            Self::LockPoisoned => {
                formatter.write_str("Supervisor shutdown registry is unavailable")
            }
        }
    }
}

impl std::error::Error for SupervisorShutdownError {}

#[cfg(test)]
mod tests {
    use crate::domain::{Actor, Reason};

    use super::{
        SupervisorShutdown, SupervisorShutdownError, SupervisorShutdownOutcome,
        SupervisorShutdownRequest,
    };

    fn request(id: &str) -> SupervisorShutdownRequest {
        SupervisorShutdownRequest {
            actor: Actor::Codex,
            reason: Reason::new("test shutdown").expect("valid reason"),
            request_id: id.to_owned(),
        }
    }

    #[test]
    fn shutdown_request_is_single_flight_and_idempotent() {
        let shutdown = SupervisorShutdown::default();
        assert_eq!(
            shutdown.request(request("one")).expect("accept request"),
            SupervisorShutdownOutcome::Accepted
        );
        assert_eq!(
            shutdown.request(request("one")).expect("replay request"),
            SupervisorShutdownOutcome::AlreadyAccepted
        );
        assert_eq!(
            shutdown.request(request("two")),
            Err(SupervisorShutdownError::AlreadyInProgress)
        );
        assert_eq!(
            shutdown.accepted().expect("read request"),
            Some(request("one"))
        );
    }
}
