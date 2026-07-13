use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    windows_fixture::run().unwrap_or(ExitCode::FAILURE)
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("signal-owner fixture requires the Windows platform adapter");
    ExitCode::from(2)
}

#[cfg(windows)]
mod windows_fixture {
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, ExitCode, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use aku_supervisor::platform::windows::{ConsoleShutdown, OwnedProcessTree};

    const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
    const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    pub fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
        let mut arguments = std::env::args_os().skip(1);
        let tree_executable = PathBuf::from(arguments.next().ok_or("missing tree executable")?);
        let ready_file = PathBuf::from(arguments.next().ok_or("missing ready file")?);
        if arguments.next().is_some() {
            return Err("unexpected fixture argument".into());
        }

        let shutdown = ConsoleShutdown::install()?;
        let mut command = Command::new(tree_executable);
        command
            .arg("--root")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut tree = OwnedProcessTree::spawn(&mut command)?;

        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let owned_pids = loop {
            let owned_pids = tree.owned_pids()?;
            if owned_pids.len() >= 2 {
                break owned_pids;
            }
            if Instant::now() >= deadline {
                return Err("owned fixture descendant did not start".into());
            }
            thread::sleep(POLL_INTERVAL);
        };

        let ready = owned_pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&ready_file, format!("{ready}\n"))?;

        while !shutdown.is_requested() {
            thread::sleep(POLL_INTERVAL);
        }

        tree.stop(SHUTDOWN_GRACE)?;
        fs::write(ready_file.with_extension("stopped"), b"stopped\n")?;
        Ok(ExitCode::SUCCESS)
    }
}
