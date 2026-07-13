//! Lifecycle use cases shared by every control surface.
//!
//! CLI, HTTP, the future dashboard, and the deferred MCP adapter must call the
//! same application services instead of implementing lifecycle rules directly.

mod platform_ports;
mod service_runtime;

pub use platform_ports::{
    LaunchSpec, ManagedProcessTree, NetworkFamily, PortDiagnostic, PortInspector, PortOccupant,
    ProcessTreeSpawner, ShutdownSignal, TreeStopReport,
};
pub use service_runtime::{ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome};
