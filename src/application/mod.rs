//! Lifecycle use cases shared by every control surface.
//!
//! CLI, HTTP, the future dashboard, and the deferred MCP adapter must call the
//! same application services instead of implementing lifecycle rules directly.

mod control_api;
mod cooperative_actions;
mod cooperative_operations;
mod platform_ports;
mod service_registry;
mod service_runtime;

pub use control_api::{
    ControlAction, ControlError, ControlErrorKind, ControlMutationOutcome, SupervisorControl,
};
pub use cooperative_actions::{
    CooperativeActionControl, CooperativeActionError, CooperativeActionOutcome,
    CooperativeActionProgress, CooperativeActionStage, CooperativeActionStatus,
};
pub use cooperative_operations::{
    CooperativeOperationError, CooperativeOperationManager, CooperativeOperationSnapshot,
    CooperativeOperationStatus,
};
pub use platform_ports::{
    LaunchSpec, ManagedProcessTree, NetworkFamily, PortDiagnostic, PortInspector, PortOccupant,
    ProcessTreeSpawner, ShutdownSignal, TreeStopReport,
};
pub use service_registry::{
    BackendOperationError, LastAction, RegistryBuildError, RegistryError, ServiceRegistration,
    ServiceRegistry, ServiceSnapshot,
};
pub use service_runtime::{
    RestartOutcome, ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome,
};
