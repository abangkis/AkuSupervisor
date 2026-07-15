//! Windows process ownership boundary.
//!
//! Roadmap Phase 2 adds Job Object and process-identity implementations here.
//! Unsafe Win32 calls must remain isolated in this module and wrapped by safe,
//! ownership-aware interfaces.

mod atomic_file;
mod console_shutdown;
mod port_observer;
mod process_spawner;
mod process_tree;
mod secure_random;
mod token_permissions;

pub use atomic_file::atomic_replace_file;
pub use console_shutdown::{ConsoleShutdown, ConsoleShutdownError};
pub use port_observer::{PortObserverError, WindowsPortInspector, inspect_tcp_port};
pub use process_spawner::WindowsProcessSpawner;
pub use process_tree::{OwnedProcessTree, ProcessTreeError};
pub use secure_random::generate_control_token;
pub use token_permissions::{TokenPermissionError, harden_runtime_token_permissions};
