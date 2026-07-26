#![cfg(windows)]

use std::fs::{self, OpenOptions, TryLockError};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn powershell_watcher_lock_blocks_and_releases_rust_file_lock() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "aku-supervisor-watcher-lock-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).expect("create lock fixture directory");
    let lock_path = directory.join("development-watcher.lock");
    let ready_path = directory.join("ready");
    let release_path = directory.join("release");
    let script = r"
$leasePath = $env:AKU_TEST_WATCHER_LOCK
$readyPath = $env:AKU_TEST_WATCHER_READY
$releasePath = $env:AKU_TEST_WATCHER_RELEASE
$stream = [IO.FileStream]::new(
    $leasePath,
    [IO.FileMode]::OpenOrCreate,
    [IO.FileAccess]::ReadWrite,
    [IO.FileShare]::ReadWrite)
try {
    $stream.Lock(0, 1)
    [IO.File]::WriteAllText($readyPath, 'ready')
    while (-not (Test-Path -LiteralPath $releasePath)) {
        Start-Sleep -Milliseconds 10
    }
} finally {
    try { $stream.Unlock(0, 1) } catch {}
    $stream.Dispose()
}
";
    let mut holder = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ])
        .env("AKU_TEST_WATCHER_LOCK", &lock_path)
        .env("AKU_TEST_WATCHER_READY", &ready_path)
        .env("AKU_TEST_WATCHER_RELEASE", &release_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start PowerShell lock holder");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready_path.is_file() {
        if let Some(status) = holder.try_wait().expect("query lock holder") {
            let output = holder
                .wait_with_output()
                .expect("collect early PowerShell exit");
            panic!(
                "PowerShell lock holder exited before readiness with {status}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            Instant::now() < deadline,
            "PowerShell lock holder did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open watcher lock from Rust");
    assert!(matches!(file.try_lock(), Err(TryLockError::WouldBlock)));

    fs::write(&release_path, b"release").expect("release PowerShell holder");
    let output = holder
        .wait_with_output()
        .expect("collect PowerShell lock holder");
    assert!(
        output.status.success(),
        "PowerShell lock holder failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    file.try_lock()
        .expect("Rust acquires released watcher lock");
    drop(file);
    fs::remove_dir_all(directory).ok();
}
