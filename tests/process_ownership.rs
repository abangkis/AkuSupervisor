#![cfg(all(windows, feature = "test-fixtures"))]
#![allow(unsafe_code)]

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aku_supervisor::platform::windows::OwnedProcessTree;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

fn fixture() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor-process-fixture")
}

fn fixture_command(mode: &str) -> Command {
    let mut command = Command::new(fixture());
    command
        .arg(mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for_tree(tree: &OwnedProcessTree, minimum: usize) -> Vec<u32> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let pids = tree.owned_pids().expect("Job Object query should succeed");
        if pids.len() >= minimum {
            return pids;
        }
        assert!(
            Instant::now() < deadline,
            "fixture tree did not reach {minimum} processes"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn cleanup_unrelated(child: &mut Child) {
    child.kill().ok();
    child.wait().ok();
}

struct WaitHandle(HANDLE);

impl WaitHandle {
    fn open(pid: u32) -> Self {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null(), "fixture process handle should open");
        Self(handle)
    }

    fn wait_for_exit(&self) {
        assert_eq!(
            unsafe { WaitForSingleObject(self.0, 5_000) },
            WAIT_OBJECT_0,
            "owned root process should exit when its Job Object closes"
        );
    }
}

impl Drop for WaitHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[test]
fn owned_tree_contains_root_and_descendant_then_stops_completely() {
    let mut tree = OwnedProcessTree::spawn(&mut fixture_command("--root"))
        .expect("owned fixture tree should start");
    let observed = wait_for_tree(&tree, 2);

    assert!(observed.contains(&tree.root_pid()));
    assert!(tree.owns_pid(tree.root_pid()).expect("ownership query"));

    let report = tree
        .stop(Duration::from_millis(250))
        .expect("owned fixture tree should stop");
    assert!(report.owned_pids_before.len() >= 2);
    assert!(report.owned_pids_after.is_empty());
    assert!(tree.owned_pids().expect("final ownership query").is_empty());
}

#[test]
fn stopping_owned_tree_does_not_touch_unrelated_process() {
    let mut unrelated = fixture_command("--child")
        .spawn()
        .expect("unrelated fixture should start");
    let unrelated_pid = unrelated.id();
    let mut tree = OwnedProcessTree::spawn(&mut fixture_command("--root"))
        .expect("owned fixture tree should start");
    wait_for_tree(&tree, 2);

    assert!(!tree.owns_pid(unrelated_pid).expect("ownership query"));
    tree.stop(Duration::from_millis(100))
        .expect("owned fixture tree should stop");
    assert!(
        unrelated
            .try_wait()
            .expect("unrelated process status should be readable")
            .is_none(),
        "unrelated process must remain alive"
    );

    cleanup_unrelated(&mut unrelated);
}

#[test]
fn dropping_owner_closes_job_and_terminates_tree() {
    let tree = OwnedProcessTree::spawn(&mut fixture_command("--root"))
        .expect("owned fixture tree should start");
    wait_for_tree(&tree, 2);
    let root = WaitHandle::open(tree.root_pid());

    drop(tree);

    root.wait_for_exit();
}
