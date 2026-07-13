#![cfg(all(windows, feature = "test-fixtures"))]
#![allow(unsafe_code)]

use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

fn signal_fixture() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor-signal-fixture")
}

fn tree_fixture() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor-process-fixture")
}

#[test]
fn console_break_causes_owned_tree_cleanup_before_supervisor_exit() {
    let files = FixtureFiles::new();
    let mut supervisor = Command::new(signal_fixture())
        .arg(tree_fixture())
        .arg(&files.ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .expect("spawn signal-owner fixture");
    let supervisor_guard = ChildGuard(&mut supervisor);

    let owned_pids = wait_for_ready(&files.ready);
    assert!(owned_pids.len() >= 2);
    let owned_handles: Vec<_> = owned_pids
        .into_iter()
        .map(WaitHandle::open)
        .collect::<Result<_, _>>()
        .expect("open owned process wait handles");

    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, supervisor_guard.0.id()) };
    assert_ne!(
        sent,
        0,
        "send targeted console break: {}",
        io::Error::last_os_error()
    );

    let deadline = Instant::now() + EXIT_TIMEOUT;
    let exit_status = loop {
        if let Some(status) = supervisor_guard
            .0
            .try_wait()
            .expect("query supervisor exit")
        {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "signal-owner fixture did not exit"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(exit_status.success());
    assert!(files.ready.with_extension("stopped").is_file());
    for handle in owned_handles {
        assert!(
            handle.wait(EXIT_TIMEOUT),
            "owned child survived console cleanup"
        );
    }

    supervisor_guard.disarm();
}

fn wait_for_ready(path: &Path) -> Vec<u32> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            return contents
                .lines()
                .map(|line| line.parse().expect("ready file contains a PID"))
                .collect();
        }
        assert!(
            Instant::now() < deadline,
            "signal-owner fixture was not ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

struct FixtureFiles {
    ready: PathBuf,
}

impl FixtureFiles {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        Self {
            ready: std::env::temp_dir().join(format!(
                "aku-supervisor-signal-{}-{unique}.ready",
                std::process::id()
            )),
        }
    }
}

impl Drop for FixtureFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.ready);
        let _ = fs::remove_file(self.ready.with_extension("stopped"));
    }
}

struct ChildGuard<'a>(&'a mut Child);

impl ChildGuard<'_> {
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct WaitHandle(HANDLE);

impl WaitHandle {
    fn open(pid: u32) -> io::Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn wait(&self, timeout: Duration) -> bool {
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        unsafe { WaitForSingleObject(self.0, milliseconds) == WAIT_OBJECT_0 }
    }
}

impl Drop for WaitHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
