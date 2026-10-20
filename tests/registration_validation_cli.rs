#![cfg(feature = "test-fixtures")]

use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use aku_supervisor::adapters::registration::{RegistrationAuthority, RegistrationDraft};
use serde_json::{Value, json};

#[test]
fn cli_and_mcp_registration_survive_two_unrelated_missing_executables() {
    let fixture = Fixture::new();
    let capabilities = successful_json(&invoke(
        &fixture.config,
        &["registration", "capabilities", "--json"],
    ));
    let mcp_capabilities = mcp(
        &fixture.config,
        "supervisor_registration_get_capabilities",
        &json!({}),
    );
    assert_eq!(
        mcp_capabilities["currentRevision"],
        capabilities["currentRevision"]
    );
    let service = service(&fixture.path, "New target");
    let validation = mcp(
        &fixture.config,
        "supervisor_registration_validate_service",
        &json!({"operation":"register","serviceId":"worker","service":service}),
    );
    assert_eq!(validation["validation"]["valid"], true);
    let prepared = mcp(
        &fixture.config,
        "supervisor_registration_prepare_change",
        &json!({
            "operation":"register","serviceId":"worker","service":service,
            "requestId":"isolated-cli-mcp-regression",
            "baseRevision":capabilities["currentRevision"]
        }),
    );
    let draft: RegistrationDraft =
        serde_json::from_value(prepared["draft"].clone()).expect("prepared draft");
    let shown = successful_json(&invoke(
        &fixture.config,
        &["registration", "show", &draft.draft_id, "--json"],
    ));
    assert_eq!(shown["draftId"], draft.draft_id);
    let read = mcp(
        &fixture.config,
        "supervisor_registration_get_draft",
        &json!({"draftId":draft.draft_id}),
    );
    assert_eq!(read["draft"]["draftId"], draft.draft_id);

    let before = fs::read(&fixture.config).expect("original fixture configuration");
    let denied = mcp_result(
        &fixture.config,
        "supervisor_registration_commit_change",
        &json!({"draftId":draft.draft_id}),
    );
    assert_eq!(denied["isError"], true);
    assert_eq!(
        denied["structuredContent"]["error"]["code"],
        "human_approval_required"
    );
    assert_eq!(
        fs::read(&fixture.config).expect("unchanged fixture"),
        before
    );

    // This approval boundary only exists in builds with test-fixtures enabled.
    RegistrationAuthority::open(Some(fixture.config.clone()))
        .expect("fixture authority")
        .approve_fixture(draft.clone())
        .expect("fixture approval");
    let committed = mcp(
        &fixture.config,
        "supervisor_registration_commit_change",
        &json!({"draftId":draft.draft_id}),
    );
    assert_eq!(committed["commit"]["registeredState"], "stopped");
    assert_eq!(committed["commit"]["autoStarted"], false);
    let replay = mcp(
        &fixture.config,
        "supervisor_registration_commit_change",
        &json!({"draftId":draft.draft_id}),
    );
    assert_eq!(
        replay["commit"]["configurationRevision"],
        committed["commit"]["configurationRevision"]
    );
    let after: Value = serde_json::from_slice(&fs::read(&fixture.config).expect("saved fixture"))
        .expect("saved fixture JSON");
    assert_eq!(after["services"]["worker"], service);
    for id in ["unavailable-a", "unavailable-b"] {
        let original: Value = serde_json::from_slice(&before).expect("original fixture JSON");
        assert_eq!(after["services"][id], original["services"][id]);
    }
}

fn invoke(config: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aku-supervisor"))
        .args(arguments)
        .arg("--config")
        .arg(config)
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .output()
        .expect("isolated CLI invocation")
}

fn successful_json(output: &Output) -> Value {
    assert!(output.status.success(), "CLI failed: {output:?}");
    serde_json::from_slice(&output.stdout).expect("CLI JSON")
}

fn mcp(config: &Path, name: &str, arguments: &Value) -> Value {
    let result = mcp_result(config, name, arguments);
    assert_eq!(result["isError"], false, "MCP failure: {result}");
    result["structuredContent"].clone()
}

fn mcp_result(config: &Path, name: &str, arguments: &Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aku-supervisor"))
        .args(["registration-mcp", "--config"])
        .arg(config)
        .env_remove("AKU_SUPERVISOR_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("isolated MCP process");
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}});
    writeln!(child.stdin.take().expect("MCP stdin"), "{request}").expect("MCP request");
    let response = successful_json(&child.wait_with_output().expect("MCP response"));
    assert!(response.get("error").is_none(), "RPC failure: {response}");
    response["result"].clone()
}

fn service(path: &Path, label: &str) -> Value {
    json!({
        "label":label,"cwd":path,"command":env!("CARGO_BIN_EXE_aku-supervisor"),
        "args":[],"environment":{},"health":{"type":"process"},"ports":[],
        "dependsOn":[],"startupPrerequisites":[],"restartPolicy":"manual","shutdownGraceMs":100
    })
}

struct Fixture {
    path: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "registration-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("fixture directory");
        let config = path.join("services.json");
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("isolated control port")
            .local_addr()
            .expect("isolated control address")
            .port();
        let mut unavailable = service(&path, "Unavailable idle service");
        unavailable["command"] = json!(path.join("unbuilt.exe"));
        let profile = json!({
            "version":2,
            "control":{"host":"127.0.0.1","port":port,"tokenFile":".runtime/control-token"},
            "services":{"unavailable-a":unavailable,"unavailable-b":unavailable}
        });
        fs::write(
            &config,
            serde_json::to_vec_pretty(&profile).expect("fixture JSON"),
        )
        .expect("fixture configuration");
        Self { path, config }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
