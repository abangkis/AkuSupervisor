//! Lifecycle use cases shared by every control surface.
//!
//! CLI, HTTP, the future dashboard, and the deferred MCP adapter must call the
//! same application services instead of implementing lifecycle rules directly.

mod service_runtime;

pub use service_runtime::{ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome};
