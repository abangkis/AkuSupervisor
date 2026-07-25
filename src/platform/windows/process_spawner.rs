use std::process::Command;
use std::sync::Arc;

use crate::application::{LaunchSpec, ProcessTreeSpawner, ServiceLogSink};

use super::{OwnedProcessTree, ProcessTreeError};

/// Windows adapter that turns a validated launch contract into a Job Object.
#[derive(Debug, Clone)]
pub struct WindowsProcessSpawner {
    log_sink: Arc<dyn ServiceLogSink>,
}

impl WindowsProcessSpawner {
    #[must_use]
    pub fn new(log_sink: Arc<dyn ServiceLogSink>) -> Self {
        Self { log_sink }
    }

    /// Creates a spawner for ownership-only demonstrations without log files.
    #[must_use]
    pub fn without_live_logs() -> Self {
        Self {
            log_sink: Arc::new(DiscardLogSink),
        }
    }
}

#[derive(Debug)]
struct DiscardLogSink;

impl ServiceLogSink for DiscardLogSink {
    fn publish(
        &self,
        _service_id: &str,
        _stream: crate::application::CapturedLogStream,
        _bytes: &[u8],
    ) {
    }

    fn close_stream(&self, _service_id: &str, _stream: crate::application::CapturedLogStream) {}
}

impl ProcessTreeSpawner for WindowsProcessSpawner {
    type Process = OwnedProcessTree;

    fn spawn(
        &self,
        service_id: &str,
        launch: &LaunchSpec,
    ) -> Result<Self::Process, ProcessTreeError> {
        let mut command = Command::new(launch.executable());
        command
            .args(launch.args())
            .current_dir(launch.cwd())
            .envs(launch.environment());
        OwnedProcessTree::spawn_with_logs(
            &mut command,
            launch.stdout_log(),
            launch.stderr_log(),
            Some(service_id),
            Some(&self.log_sink),
        )
    }
}
