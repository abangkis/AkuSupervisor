use std::process::Command;

use crate::application::{LaunchSpec, ProcessTreeSpawner};

use super::{OwnedProcessTree, ProcessTreeError};

/// Windows adapter that turns a validated launch contract into a Job Object.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsProcessSpawner;

impl ProcessTreeSpawner for WindowsProcessSpawner {
    type Process = OwnedProcessTree;

    fn spawn(&self, launch: &LaunchSpec) -> Result<Self::Process, ProcessTreeError> {
        let mut command = Command::new(launch.executable());
        command
            .args(launch.args())
            .current_dir(launch.cwd())
            .envs(launch.environment());
        OwnedProcessTree::spawn(&mut command)
    }
}
