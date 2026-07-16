#![cfg(all(windows, feature = "test-fixtures"))]
#![allow(unsafe_code)]

use std::fs;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aku_supervisor::application::{LaunchSpec, ProcessTreeSpawner};
use aku_supervisor::platform::windows::{OwnedProcessTree, WindowsProcessSpawner};
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

fn fixture_launch(mode: &str) -> LaunchSpec {
    LaunchSpec::new(
        fixture(),
        [mode],
        std::env::current_dir().expect("current test directory"),
        std::iter::empty::<(&str, &str)>(),
    )
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

fn unique_test_directory(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "aku-supervisor-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&directory).expect("test directory should be created");
    directory
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
    let mut tree = WindowsProcessSpawner
        .spawn(&fixture_launch("--root"))
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

#[test]
fn native_launch_preserves_arguments_environment_logs_and_noninteractive_stdin() {
    let directory = unique_test_directory("native-launch");
    let evidence = directory.join("launch evidence.json");
    let stdout = directory.join("service.stdout.log");
    let stderr = directory.join("service.stderr.log");
    let expected_arguments = [
        "plain",
        "two words",
        "embedded\"quote",
        r"trailing slash\",
        "",
    ];
    let mut arguments = vec![
        "--capture-launch".to_owned(),
        evidence.to_string_lossy().into_owned(),
    ];
    arguments.extend(expected_arguments.map(str::to_owned));
    let launch = LaunchSpec::new(
        fixture(),
        arguments,
        std::env::current_dir().expect("current test directory"),
        [("AKU_SUPERVISOR_FIXTURE_ENV", "forwarded value")],
    )
    .with_log_files(&stdout, &stderr);
    let mut tree = WindowsProcessSpawner
        .spawn(&launch)
        .expect("native launch should start");

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while !evidence.exists() {
        assert!(Instant::now() < deadline, "launch evidence was not written");
        thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while tree
        .try_wait()
        .expect("process status should be readable")
        .is_none()
    {
        assert!(Instant::now() < deadline, "fixture did not exit naturally");
        thread::sleep(Duration::from_millis(20));
    }

    let payload: serde_json::Value =
        serde_json::from_slice(&fs::read(&evidence).expect("launch evidence should be readable"))
            .expect("launch evidence should be JSON");
    assert_eq!(payload["args"], serde_json::json!(expected_arguments));
    assert_eq!(payload["environment"], "forwarded value");
    assert_eq!(payload["stdinBytes"], 0);
    assert!(
        fs::read_to_string(&stdout)
            .expect("stdout log should be readable")
            .contains("launch captured")
    );
    assert!(tree.owned_pids().expect("final ownership query").is_empty());

    fs::remove_dir_all(directory).expect("test directory should be removed");
}
