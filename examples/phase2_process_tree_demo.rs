use std::process::ExitCode;

#[cfg(windows)]
fn main() -> ExitCode {
    windows_demo::run().unwrap_or_else(|error| {
        eprintln!("Phase 2 demo failed: {error}");
        ExitCode::FAILURE
    })
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("The Phase 2 process-tree demo currently requires the Windows adapter.");
    ExitCode::from(2)
}

#[cfg(windows)]
mod windows_demo {
    use std::io::{self, Write};
    use std::process::{Child, Command, ExitCode, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use aku_supervisor::application::{LaunchSpec, ProcessTreeSpawner};
    use aku_supervisor::platform::windows::{ConsoleShutdown, WindowsProcessSpawner};

    const DEMO_DURATION: Duration = Duration::from_secs(30);
    const CHILD_DURATION: Duration = Duration::from_mins(5);
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    pub fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
        match std::env::args().nth(1).as_deref() {
            Some("--root") => run_root(),
            Some("--child") => Ok(run_child()),
            Some(_) => Err("unknown internal demo mode".into()),
            None => run_supervisor(),
        }
    }

    fn run_supervisor() -> Result<ExitCode, Box<dyn std::error::Error>> {
        let shutdown = ConsoleShutdown::install()?;
        let executable = std::env::current_exe()?;
        let launch = LaunchSpec::new(
            executable,
            ["--root"],
            std::env::current_dir()?,
            std::iter::empty::<(&str, &str)>(),
        );
        let mut tree = WindowsProcessSpawner.spawn(&launch)?;
        let deadline = Instant::now() + DEMO_DURATION;

        let owned_pids = loop {
            let pids = tree.owned_pids()?;
            if pids.len() >= 2 {
                break pids;
            }
            if Instant::now() >= deadline {
                return Err("demo child process did not start".into());
            }
            thread::sleep(POLL_INTERVAL);
        };

        println!("AkuSupervisor Phase 2 process ownership demo");
        println!("Supervisor PID : {}", std::process::id());
        println!("Owned tree PIDs: {owned_pids:?}");
        println!("Press Ctrl+C to test cleanup, or wait 30 seconds for automatic cleanup.");
        io::stdout().flush()?;

        while !shutdown.is_requested() && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }

        let report = tree.stop(Duration::from_secs(2))?;
        println!("Cleanup complete.");
        println!("Before: {:?}", report.owned_pids_before);
        println!("After : {:?}", report.owned_pids_after);
        println!("Forced: {}", report.forced);
        Ok(ExitCode::SUCCESS)
    }

    fn run_root() -> Result<ExitCode, Box<dyn std::error::Error>> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--child")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        wait_for_child(&mut child)
    }

    fn wait_for_child(child: &mut Child) -> Result<ExitCode, Box<dyn std::error::Error>> {
        loop {
            match child.try_wait()? {
                Some(_) => return Ok(ExitCode::FAILURE),
                None => thread::sleep(POLL_INTERVAL),
            }
        }
    }

    fn run_child() -> ExitCode {
        thread::sleep(CHILD_DURATION);
        ExitCode::SUCCESS
    }
}
