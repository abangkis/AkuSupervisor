#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

fn installer() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("install-codex-mcp.ps1")
}

fn supervisor() -> &'static str {
    env!("CARGO_BIN_EXE_aku-supervisor")
}

fn invoke_installer(config: &Path, host: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(installer())
        .arg("-CodexConfigPath")
        .arg(config)
        .arg("-SourcePath")
        .arg(supervisor())
        .arg("-HostPath")
        .arg(host)
        .arg("-Json")
        .args(extra);
    command.output().expect("Codex MCP installer should run")
}

fn invoke_installer_with_default_host(config: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(installer())
        .arg("-CodexConfigPath")
        .arg(config)
        .arg("-SourcePath")
        .arg(supervisor())
        .arg("-Json")
        .args(extra);
    command.output().expect("Codex MCP installer should run")
}

fn successful_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "installer failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("installer stdout should be one JSON document")
}

#[test]
fn codex_mcp_bootstrap_is_previewed_hash_bound_atomic_and_idempotent() {
    let directory = TestDirectory::create();
    let config = directory
        .path
        .join("workspace")
        .join(".codex")
        .join("config.toml");
    let host = directory.path.join("mcp").join("aku-supervisor-mcp.exe");
    fs::create_dir_all(config.parent().expect("config should have a parent"))
        .expect("create temporary Codex config directory");
    let original = concat!(
        "model = \"preserve-me\"\n\n",
        "[mcp_servers.unrelated]\n",
        "command = \"do-not-touch.exe\"\n",
    );
    fs::write(&config, original).expect("write temporary Codex config");

    let first_plan = successful_json(&invoke_installer(&config, &host, &[]));
    assert_eq!(first_plan["status"], "planned");
    assert_eq!(first_plan["configurationChanged"], true);
    assert_eq!(first_plan["hostChanged"], true);
    assert_eq!(first_plan["unrelatedEntriesPreserved"], true);
    assert_eq!(first_plan["restartCodexRequired"], true);
    assert_eq!(fs::read_to_string(&config).expect("read config"), original);
    assert!(!host.exists(), "plan mode must not stage a host executable");

    let stale_code = first_plan["approvalCode"]
        .as_str()
        .expect("plan should return an approval code");
    let concurrent_edit = format!("{original}\n# concurrent edit\n");
    fs::write(&config, &concurrent_edit).expect("simulate a concurrent config edit");
    let stale_apply = invoke_installer(&config, &host, &["-Apply", "-ApprovalCode", stale_code]);
    assert!(!stale_apply.status.success());
    assert_eq!(
        fs::read_to_string(&config).expect("read config after stale apply"),
        concurrent_edit
    );
    assert!(!host.exists(), "stale approval must fail before staging");

    let approved_plan = successful_json(&invoke_installer(&config, &host, &[]));
    let approval_code = approved_plan["approvalCode"]
        .as_str()
        .expect("updated plan should return an approval code");
    let applied = successful_json(&invoke_installer(
        &config,
        &host,
        &["-Apply", "-ApprovalCode", approval_code],
    ));
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["restartCodexRequired"], true);

    let installed = fs::read_to_string(&config).expect("read installed config");
    assert!(installed.contains("model = \"preserve-me\""));
    assert!(installed.contains("[mcp_servers.unrelated]"));
    assert!(installed.contains("command = \"do-not-touch.exe\""));
    assert!(installed.contains("[mcp_servers.aku_supervisor]"));
    assert!(installed.contains("[mcp_servers.aku_supervisor_registration]"));
    let escaped_host = host.display().to_string().replace('\\', "\\\\");
    assert_eq!(
        installed
            .matches(&format!("command = \"{escaped_host}\""))
            .count(),
        2
    );
    assert_eq!(
        fs::read(&host).expect("read staged host"),
        fs::read(supervisor()).expect("read source executable")
    );

    let idempotent_plan = successful_json(&invoke_installer(&config, &host, &[]));
    assert_eq!(idempotent_plan["configurationChanged"], false);
    assert_eq!(idempotent_plan["hostChanged"], false);
    assert_eq!(idempotent_plan["restartCodexRequired"], false);
    let idempotent_code = idempotent_plan["approvalCode"]
        .as_str()
        .expect("idempotent plan should return an approval code");
    let idempotent_apply = successful_json(&invoke_installer(
        &config,
        &host,
        &["-Apply", "-ApprovalCode", idempotent_code],
    ));
    assert_eq!(idempotent_apply["status"], "applied");
    assert_eq!(idempotent_apply["restartCodexRequired"], false);
}

#[test]
fn default_mcp_host_path_is_content_addressed_and_plan_only_is_read_only() {
    let directory = TestDirectory::create();
    let config = directory.path.join("workspace").join("config.toml");
    fs::create_dir_all(config.parent().expect("config should have a parent"))
        .expect("create temporary config directory");
    fs::write(&config, "model = \"preserve-me\"\n").expect("write temporary Codex config");

    let plan = successful_json(&invoke_installer_with_default_host(&config, &[]));
    let source_hash = plan["sourceHash"]
        .as_str()
        .expect("plan should expose source hash")
        .strip_prefix("sha256:")
        .expect("source hash should be normalized");
    let host_path = PathBuf::from(
        plan["hostPath"]
            .as_str()
            .expect("plan should expose host path"),
    );
    assert_eq!(
        host_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str()),
        Some(format!("sha256-{source_hash}").as_str())
    );
    assert_eq!(
        host_path.file_name().and_then(|value| value.to_str()),
        Some("aku-supervisor-mcp.exe")
    );
    assert!(!host_path.exists(), "plan mode must not create the host");
    assert_eq!(
        fs::read_to_string(&config).expect("read config"),
        "model = \"preserve-me\"\n"
    );
}

#[test]
fn new_mcp_host_is_staged_beside_a_locked_previous_host() {
    let directory = TestDirectory::create();
    let config = directory.path.join("workspace").join("config.toml");
    let old_host = directory
        .path
        .join("mcp")
        .join("old")
        .join("aku-supervisor-mcp.exe");
    let new_host = directory
        .path
        .join("mcp")
        .join("new")
        .join("aku-supervisor-mcp.exe");
    fs::create_dir_all(old_host.parent().expect("old host should have a parent"))
        .expect("create old host directory");
    fs::create_dir_all(config.parent().expect("config should have a parent"))
        .expect("create temporary config directory");
    fs::copy(supervisor(), &old_host).expect("copy previous MCP host");
    let old_bytes = fs::read(&old_host).expect("read previous MCP host");

    let old_toml_path = old_host.display().to_string().replace('\\', "\\\\");
    fs::write(
        &config,
        format!(
            "[mcp_servers.aku_supervisor]\ncommand = \"{old_toml_path}\"\nargs = [\"mcp-proxy\"]\n\n\
             [mcp_servers.aku_supervisor_registration]\ncommand = \"{old_toml_path}\"\nargs = [\"registration-mcp\"]\n"
        ),
    )
    .expect("write previous MCP configuration");

    let missing_config = directory.path.join("missing-services.json");
    let child = Command::new(&old_host)
        .arg("registration-mcp")
        .arg("--config")
        .arg(&missing_config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start previous MCP host");
    let mut old_process = ChildGuard::new(child);
    thread::sleep(Duration::from_millis(200));
    assert!(
        old_process
            .child
            .try_wait()
            .expect("inspect previous MCP host")
            .is_none(),
        "previous MCP host should remain active"
    );

    let plan = successful_json(&invoke_installer(&config, &new_host, &[]));
    let approval_code = plan["approvalCode"]
        .as_str()
        .expect("plan should return approval code");
    let applied = successful_json(&invoke_installer(
        &config,
        &new_host,
        &["-Apply", "-ApprovalCode", approval_code],
    ));

    assert_eq!(applied["status"], "applied");
    assert_eq!(
        fs::read(&old_host).expect("read locked old host"),
        old_bytes
    );
    assert_eq!(
        fs::read(&new_host).expect("read staged new host"),
        fs::read(supervisor()).expect("read source executable")
    );
    assert!(
        old_process
            .child
            .try_wait()
            .expect("inspect previous MCP host after apply")
            .is_none(),
        "staging must not recycle the previous MCP host"
    );
    let installed = fs::read_to_string(&config).expect("read updated config");
    let escaped_new_host = new_host.display().to_string().replace('\\', "\\\\");
    assert_eq!(
        installed
            .matches(&format!("command = \"{escaped_new_host}\""))
            .count(),
        2
    );
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aku-supervisor-codex-mcp-bootstrap-{}-{unique}-{sequence}",
            std::process::id(),
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
