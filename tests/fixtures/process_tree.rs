use std::fs;
use std::io::Read;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

const SLEEP: Duration = Duration::from_mins(5);

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--root") => root(),
        Some("--child") => child(),
        Some("--exit-after") => exit_after(),
        Some("--capture-launch") => capture_launch(),
        _ => ExitCode::from(2),
    }
}

fn capture_launch() -> ExitCode {
    let mut args = std::env::args().skip(2);
    let Some(output_path) = args.next() else {
        return ExitCode::from(2);
    };
    let forwarded_args = args.collect::<Vec<_>>();
    let mut stdin = Vec::new();
    if std::io::stdin().read_to_end(&mut stdin).is_err() {
        return ExitCode::FAILURE;
    }
    let payload = serde_json::json!({
        "args": forwarded_args,
        "environment": std::env::var("AKU_SUPERVISOR_FIXTURE_ENV").ok(),
        "stdinBytes": stdin.len(),
    });
    if fs::write(output_path, payload.to_string()).is_err() {
        return ExitCode::FAILURE;
    }
    println!("process fixture launch captured");
    ExitCode::SUCCESS
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
