//! Compile-time composition root for the active operating-system backend.
//!
//! Foreground interaction and control-plane code depend on this module rather
//! than importing a native adapter directly. A future Linux or macOS backend
//! supplies the same host-facing construction functions without copying the
//! foreground lifecycle loop.

#[cfg(windows)]
mod active {
    use std::io;
    use std::path::Path;
    use std::sync::Arc;

    use crate::application::{
        HealthProbe, RegistryBuildError, ServiceLogSink, ServiceRegistration, ServiceRegistry,
    };
    use crate::platform::windows::{ConsoleShutdown, WindowsPortInspector, WindowsProcessSpawner};

    pub use crate::platform::windows::{
        ConsoleShutdownError as HostShutdownError, TokenPermissionError as HostTokenPermissionError,
    };

    pub type HostRegistry = ServiceRegistry<WindowsProcessSpawner, WindowsPortInspector>;
    pub type HostShutdown = ConsoleShutdown;

    /// Creates the active host registry.
    ///
    /// # Errors
    ///
    /// Returns a registry construction error for invalid duplicate services.
    pub fn create_registry(
        registrations: Vec<ServiceRegistration>,
        health_probe: Arc<dyn HealthProbe>,
        log_sink: Arc<dyn ServiceLogSink>,
    ) -> Result<HostRegistry, RegistryBuildError> {
        HostRegistry::new(
            registrations,
            WindowsProcessSpawner::new(log_sink),
            WindowsPortInspector,
            health_probe,
        )
    }

    /// Installs the active host's process-wide shutdown observer.
    ///
    /// # Errors
    ///
    /// Returns the native observer installation error.
    pub fn install_shutdown() -> Result<HostShutdown, HostShutdownError> {
        ConsoleShutdown::install()
    }

    /// Generates a control token through native secure entropy.
    ///
    /// # Errors
    ///
    /// Returns the native entropy provider error.
    pub fn generate_control_token() -> io::Result<String> {
        crate::platform::windows::generate_control_token()
    }

    /// Restricts a persisted token to the current user.
    ///
    /// # Errors
    ///
    /// Returns the native token-permission error.
    pub fn harden_runtime_token_permissions(path: &Path) -> Result<(), HostTokenPermissionError> {
        crate::platform::windows::harden_runtime_token_permissions(path)
    }
}

#[cfg(windows)]
pub use active::{
    HostRegistry, HostShutdown, HostShutdownError, HostTokenPermissionError, create_registry,
    generate_control_token, harden_runtime_token_permissions, install_shutdown,
};
