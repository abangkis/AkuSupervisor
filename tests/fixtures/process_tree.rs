use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const SLEEP: Duration = Duration::from_mins(5);

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--root") => root(),
        Some("--child") => child(),
        Some("--exit-after") => exit_after(),
        _ => ExitCode::from(2),
    }
}

fn exit_after() -> ExitCode {
    let mut args = std::env::args().skip(2);
    let delay_ms = args
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(100);
    let code = args
        .next()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(17);
    println!("process fixture exits with code {code} after {delay_ms} ms");
    thread::sleep(Duration::from_millis(delay_ms));
    ExitCode::from(code)
}

fn root() -> ExitCode {
    println!("process fixture root started");
    eprintln!("process fixture stderr ready");
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
