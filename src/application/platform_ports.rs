use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

/// Fully resolved process launch request produced from validated configuration.
///
/// Control surfaces must never construct this value from request-supplied shell
/// text. An input adapter first resolves a registered service configuration;
/// the application then passes this contract to the active platform adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}

impl LaunchSpec {
    #[must_use]
    pub fn new(
        executable: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        cwd: impl Into<PathBuf>,
        environment: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Self {
        Self {
            executable: executable.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            environment: environment
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn environment(&self) -> impl Iterator<Item = (&OsStr, &OsStr)> {
        self.environment
            .iter()
            .map(|(key, value)| (key.as_os_str(), value.as_os_str()))
    }
}

/// Platform-neutral outcome of stopping one owned process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeStopReport {
    pub owned_pids_before: Vec<u32>,
    pub owned_pids_after: Vec<u32>,
    pub graceful_signal_sent: bool,
    pub graceful_signal_error: Option<String>,
    pub forced: bool,
}

/// An operating-system-owned process tree used by lifecycle application code.
pub trait ManagedProcessTree: Debug + Send {
    type Error: std::error::Error + Send + Sync + 'static;

    fn root_pid(&self) -> u32;

    /// Returns process IDs with current ownership evidence.
    ///
    /// # Errors
    ///
    /// Returns a platform error if membership cannot be observed safely.
    fn owned_pids(&self) -> Result<Vec<u32>, Self::Error>;

    /// Observes launcher exit without blocking.
    ///
    /// # Errors
    ///
    /// Returns a platform error if the launcher status cannot be queried.
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, Self::Error>;

    /// Stops this owned tree using graceful then bounded forced cleanup.
    ///
    /// # Errors
    ///
    /// Returns a platform error if cleanup cannot be completed or confirmed.
    fn stop(&mut self, grace: Duration) -> Result<TreeStopReport, Self::Error>;

    /// Checks current ownership evidence for one process ID.
    ///
    /// # Errors
    ///
    /// Returns a platform error if membership cannot be observed safely.
    fn owns_pid(&self, pid: u32) -> Result<bool, Self::Error> {
        Ok(self.owned_pids()?.contains(&pid))
    }
}

/// Platform adapter capable of creating an authoritative owned process tree.
pub trait ProcessTreeSpawner: Debug + Send + Sync {
    type Process: ManagedProcessTree;
    type Error: std::error::Error + Send + Sync + 'static;

    /// Creates an owned process tree from a validated launch contract.
    ///
    /// # Errors
    ///
    /// Returns a platform error if the ownership boundary cannot be active
    /// before the root process begins normal execution.
    fn spawn(&self, launch: &LaunchSpec) -> Result<Self::Process, Self::Error>;
}

/// Platform-neutral network address family used in port diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetworkFamily {
    V4,
    V6,
}

/// Read-only evidence that a process currently has a local TCP endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortOccupant {
    pid: u32,
    family: NetworkFamily,
}

impl PortOccupant {
    #[must_use]
    pub const fn new(pid: u32, family: NetworkFamily) -> Self {
        Self { pid, family }
    }

    #[must_use]
    pub const fn pid(self) -> u32 {
        self.pid
    }

    #[must_use]
    pub const fn family(self) -> NetworkFamily {
        self.family
    }
}

/// Current read-only diagnostic result for one declared TCP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortDiagnostic {
    port: u16,
    occupants: Vec<PortOccupant>,
}

impl PortDiagnostic {
    #[must_use]
    pub const fn new(port: u16, occupants: Vec<PortOccupant>) -> Self {
        Self { port, occupants }
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    #[must_use]
    pub fn occupants(&self) -> &[PortOccupant] {
        &self.occupants
    }

    #[must_use]
    pub fn is_available(&self) -> bool {
        self.occupants.is_empty()
    }
}

/// Read-only platform port for mapping a declared TCP port to current PIDs.
pub trait PortInspector: Debug + Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Collects diagnostic-only TCP port occupancy evidence.
    ///
    /// # Errors
    ///
    /// Returns a platform error if the TCP ownership table cannot be queried.
    fn inspect_tcp_port(&self, port: u16) -> Result<PortDiagnostic, Self::Error>;
}

/// Process-wide, read-only shutdown request observed by the lifecycle loop.
pub trait ShutdownSignal: Debug + Send + Sync {
    fn is_requested(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::LaunchSpec;

    #[test]
    fn launch_spec_keeps_executable_and_arguments_separate() {
        let launch = LaunchSpec::new(
            "C:\\tools\\npm.cmd",
            ["run", "dev"],
            "C:\\workspace\\service",
            [("NODE_ENV", "development")],
        );

        assert_eq!(launch.executable(), OsStr::new("C:\\tools\\npm.cmd"));
        assert_eq!(launch.args(), [OsStr::new("run"), OsStr::new("dev")]);
        assert_eq!(
            launch.environment().collect::<Vec<_>>(),
            [(OsStr::new("NODE_ENV"), OsStr::new("development"))]
        );
    }
}
