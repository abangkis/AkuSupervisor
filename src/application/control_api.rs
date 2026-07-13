use std::fmt;

use serde::Serialize;

use crate::domain::{Actor, Reason};

use super::{
    PortInspector, ProcessTreeSpawner, RegistryError, RestartOutcome, ServiceRegistry,
    ServiceSnapshot, StartOutcome, StopOutcome,
};

/// Registered lifecycle operation accepted by external control adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    Start,
    Stop,
    Restart,
}

/// Stable successful result shared by HTTP and future control adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMutationOutcome {
    Started,
    AlreadyRunning,
    Stopped,
    AlreadyStopped,
    Restarted,
}

/// Adapter-safe failure category that does not expose platform error types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorKind {
    ServiceNotFound,
    Unauthorized,
    PortConflictExternal,
    SpawnFailed,
    HealthFailed,
    ShutdownTimeout,
    OwnershipLost,
    Internal,
}

/// Bounded lifecycle failure returned across a control protocol boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlError {
    kind: ControlErrorKind,
    message: String,
}

impl ControlError {
    #[must_use]
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ControlErrorKind::Internal,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ControlErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ControlError {}

/// Platform-neutral operations exposed by the authenticated control server.
pub trait SupervisorControl: Send + Sync {
    /// Returns all currently registered service snapshots.
    ///
    /// # Errors
    ///
    /// Returns a bounded control error if process state cannot be observed.
    fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError>;

    /// Applies one typed action to one registered service.
    ///
    /// # Errors
    ///
    /// Returns a lookup, authorization, lifecycle, or platform control error.
    fn mutate(
        &self,
        action: ControlAction,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ControlMutationOutcome, ControlError>;
}

impl<Spawner, Inspector> SupervisorControl for ServiceRegistry<Spawner, Inspector>
where
    Spawner: ProcessTreeSpawner + Send + Sync,
    Spawner::Process: Send,
    <Spawner::Process as super::ManagedProcessTree>::Error: fmt::Display + Send,
    Inspector: PortInspector + Send + Sync,
    Inspector::Error: fmt::Display + Send,
{
    fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
        ServiceRegistry::snapshots(self).map_err(|error| control_error(&error))
    }

    fn mutate(
        &self,
        action: ControlAction,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ControlMutationOutcome, ControlError> {
        match action {
            ControlAction::Start => self
                .start(service_id, actor, reason)
                .map(|outcome| match outcome {
                    StartOutcome::Started => ControlMutationOutcome::Started,
                    StartOutcome::AlreadyRunning => ControlMutationOutcome::AlreadyRunning,
                })
                .map_err(|error| control_error(&error)),
            ControlAction::Stop => self
                .stop(service_id, actor, reason)
                .map(|outcome| match outcome {
                    StopOutcome::Stopped => ControlMutationOutcome::Stopped,
                    StopOutcome::AlreadyStopped => ControlMutationOutcome::AlreadyStopped,
                })
                .map_err(|error| control_error(&error)),
            ControlAction::Restart => self
                .restart(service_id, actor, reason)
                .map(|outcome| match outcome {
                    RestartOutcome::Restarted => ControlMutationOutcome::Restarted,
                    RestartOutcome::Started => ControlMutationOutcome::Started,
                })
                .map_err(|error| control_error(&error)),
        }
    }
}

fn control_error<ProcessFailure, PortFailure>(
    error: &RegistryError<ProcessFailure, PortFailure>,
) -> ControlError
where
    ProcessFailure: fmt::Display,
    PortFailure: fmt::Display,
{
    let kind = match &error {
        RegistryError::ServiceNotFound(_) => ControlErrorKind::ServiceNotFound,
        RegistryError::Unauthorized(_) => ControlErrorKind::Unauthorized,
        RegistryError::Runtime(super::ServiceRuntimeError::Start(
            super::BackendOperationError::PortConflict { .. },
        )) => ControlErrorKind::PortConflictExternal,
        RegistryError::Runtime(super::ServiceRuntimeError::Start(
            super::BackendOperationError::Process(_),
        )) => ControlErrorKind::SpawnFailed,
        RegistryError::HealthFailed { .. } => ControlErrorKind::HealthFailed,
        RegistryError::Runtime(super::ServiceRuntimeError::Stop(_)) => {
            ControlErrorKind::ShutdownTimeout
        }
        RegistryError::Observation(_)
        | RegistryError::Runtime(super::ServiceRuntimeError::Inspect(_)) => {
            ControlErrorKind::OwnershipLost
        }
        RegistryError::LockPoisoned
        | RegistryError::Transition(_)
        | RegistryError::InternalState
        | RegistryError::Runtime(
            super::ServiceRuntimeError::Poisoned
            | super::ServiceRuntimeError::Transition(_)
            | super::ServiceRuntimeError::Start(super::BackendOperationError::PortInspection {
                ..
            }),
        ) => ControlErrorKind::Internal,
    };
    ControlError {
        kind,
        message: error.to_string(),
    }
}
