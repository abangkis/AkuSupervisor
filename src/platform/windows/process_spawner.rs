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

    fn preflight_launch(
        &self,
        service_id: &str,
        launch: &LaunchSpec,
    ) -> Result<(), ProcessTreeError> {
        if !launch.cwd().is_dir() {
            return Err(ProcessTreeError::LaunchPathUnavailable {
                service_id: service_id.to_owned(),
                field: "working directory",
                path: launch.cwd().to_owned(),
            });
        }
        if !launch.executable().is_file() {
            return Err(ProcessTreeError::LaunchPathUnavailable {
                service_id: service_id.to_owned(),
                field: "executable file",
                path: launch.executable().to_owned(),
            });
        }
        Ok(())
    }

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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::application::{LaunchSpec, ProcessTreeSpawner};

    use super::WindowsProcessSpawner;
    use crate::platform::windows::ProcessTreeError;

    #[test]
    fn windows_preflight_requires_working_directory_and_executable() {
        let fixture = FixtureDirectory::create();
        let executable = fixture.path().join("fixture.exe");
        let cwd = fixture.path().join("service-cwd");
        fs::create_dir_all(&cwd).expect("create fixture working directory");
        fs::write(&executable, b"fixture command").expect("create fixture executable");
        let launch = LaunchSpec::new(
            &executable,
            std::iter::empty::<&str>(),
            &cwd,
            std::iter::empty::<(&str, &str)>(),
        );
        let spawner = WindowsProcessSpawner::without_live_logs();

        assert!(spawner.preflight_launch("fixture", &launch).is_ok());

        fs::remove_file(&executable).expect("remove executable fixture");
        assert!(matches!(
            spawner.preflight_launch("fixture", &launch),
            Err(ProcessTreeError::LaunchPathUnavailable {
                field: "executable file",
                ..
            })
        ));

        fs::write(&executable, b"fixture command").expect("restore fixture executable");
        fs::remove_dir(&cwd).expect("remove working directory fixture");
        assert!(matches!(
            spawner.preflight_launch("fixture", &launch),
            Err(ProcessTreeError::LaunchPathUnavailable {
                field: "working directory",
                ..
            })
        ));
    }

    struct FixtureDirectory {
        path: PathBuf,
    }

    impl FixtureDirectory {
        fn create() -> Self {
            static NEXT_ID: AtomicU32 = AtomicU32::new(0);
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("windows-launch-preflight")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT_ID.fetch_add(1, Ordering::SeqCst)
                ));
            fs::create_dir_all(&path).expect("create Windows preflight fixture");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for FixtureDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
