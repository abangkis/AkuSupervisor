//! Windows process ownership boundary.
//!
//! Roadmap Phase 2 adds Job Object and process-identity implementations here.
//! Unsafe Win32 calls must remain isolated in this module and wrapped by safe,
//! ownership-aware interfaces.

mod console_shutdown;
mod port_observer;
mod process_tree;

pub use console_shutdown::{ConsoleShutdown, ConsoleShutdownError};
pub use port_observer::{
    IpFamily, PortDiagnostic, PortObserverError, PortOccupant, inspect_tcp_port,
};
pub use process_tree::{OwnedProcessTree, ProcessTreeError, StopReport};
