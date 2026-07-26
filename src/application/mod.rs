//! Lifecycle use cases shared by every control surface.
//!
//! CLI, HTTP, the future dashboard, and the deferred MCP adapter must call the
//! same application services instead of implementing lifecycle rules directly.

mod control_api;
mod cooperative_actions;
mod cooperative_operations;
mod extension_validation;
mod health;
mod platform_ports;
mod registry_reconciliation;
mod service_registry;
mod service_runtime;

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
pub use extension_validation::{
    ExtensionValidationCheck, ExtensionValidationReport, validate_extension_release,
};
pub use health::{
    HealthCheckSpec, HealthProbe, HealthSnapshot, HealthStatus, JsonPathMode, TransportHealth,
};
pub use platform_ports::{
    CapturedLogStream, LaunchSpec, ManagedProcessTree, NetworkFamily, PortDiagnostic,
    PortInspector, PortOccupant, ProcessTreeSpawner, ServiceLogSink, ShutdownSignal,
    TreeStopReport,
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
    RestartOutcome, ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome, StopProgress,
};
