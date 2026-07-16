//! Lifecycle use cases shared by every control surface.
//!
//! CLI, HTTP, the future dashboard, and the deferred MCP adapter must call the
//! same application services instead of implementing lifecycle rules directly.

mod bridge_validation;
mod control_api;
mod cooperative_actions;
mod cooperative_operations;
mod health;
mod platform_ports;
mod registry_reconciliation;
mod service_registry;
mod service_runtime;

pub use bridge_validation::{
    BridgeValidationCheck, BridgeValidationReport, validate_bridge_release,
};
pub use control_api::{
    ControlAction, ControlError, ControlErrorKind, ControlMutationOutcome, ControlMutationResult,
    SupervisorControl,
};
pub use cooperative_actions::{
    CooperativeActionControl, CooperativeActionError, CooperativeActionOutcome,
    CooperativeActionProgress, CooperativeActionStage, CooperativeActionStatus,
};
pub use cooperative_operations::{
    CooperativeOperationError, CooperativeOperationManager, CooperativeOperationSnapshot,
    CooperativeOperationStatus,
};
pub use health::{HealthCheckSpec, HealthProbe, HealthSnapshot, HealthStatus, TransportHealth};
pub use platform_ports::{
    LaunchSpec, ManagedProcessTree, NetworkFamily, PortDiagnostic, PortInspector, PortOccupant,
    ProcessTreeSpawner, ShutdownSignal, TreeStopReport,
};
pub use registry_reconciliation::{
    RegistryReconciliationSnapshot, RegistryReconciliationState, RegistryReconciliationStatus,
};
pub use service_registry::{
    BackendOperationError, LastAction, ProcessExitEvent, RegistryBuildError, RegistryError,
    RegistryReconcileError, RegistryReconcileOutcome, ServiceRefresh, ServiceRegistration,
    ServiceRegistry, ServiceRestartPolicy, ServiceRestartResult, ServiceSnapshot,
    ServiceStopResult,
};
pub use service_runtime::{
    RestartOutcome, ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome,
};
