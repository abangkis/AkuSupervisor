use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const SLEEP: Duration = Duration::from_mins(5);

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--root") => root(),
        Some("--child") => child(),
        _ => ExitCode::from(2),
    }
}

fn root() -> ExitCode {
    let Ok(executable) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let Ok(mut child) = Command::new(executable)
        .arg("--child")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return ExitCode::FAILURE;
    };

    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return ExitCode::FAILURE,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn child() -> ExitCode {
    thread::sleep(SLEEP);
    ExitCode::SUCCESS
}
