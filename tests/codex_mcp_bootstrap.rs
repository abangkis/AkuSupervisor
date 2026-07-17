#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn create() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aku-supervisor-codex-mcp-bootstrap-{}-{unique}",
            std::process::id()
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
