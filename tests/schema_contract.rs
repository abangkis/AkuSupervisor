use aku_supervisor::adapters::config::{
    CONFIG_VERSION, ConsoleEvents, ControlConfig, CooperativeActionsConfig, HealthCheck, McpConfig,
    ObservabilityConfig, RestartPolicy, ServiceConfig, SupervisorConfig,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
fn checked_in_schema_is_valid_json_schema_document() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schemas/service-config.schema.json"))
            .expect("checked-in schema must be valid JSON");

    assert_eq!(
        schema.get("$schema").and_then(serde_json::Value::as_str),
        Some("https://json-schema.org/draft/2020-12/schema")
    );
    assert_eq!(
        schema
            .pointer("/properties/version/const")
            .and_then(serde_json::Value::as_u64),
        Some(u64::from(CONFIG_VERSION))
    );
    assert_eq!(
        schema
            .pointer("/properties/control/$ref")
            .and_then(serde_json::Value::as_str),
        Some("#/$defs/control")
    );
    assert_eq!(
        schema
            .pointer("/properties/observability/$ref")
            .and_then(serde_json::Value::as_str),
        Some("#/$defs/observability")
    );
}

#[test]
fn generic_development_example_matches_the_typed_contract() {
    let config = SupervisorConfig::parse_json(include_str!(
        "../config/examples/development-workspace.services.json"
    ))
    .expect("generic development example must parse");

    assert!(config.cooperative_actions.chrome_extension_reload.is_some());
    assert!(!config.control.mcp.enabled);
    assert_eq!(
        config.observability.console_events,
        ConsoleEvents::Lifecycle
    );
    assert!(config.control.mcp.allowed_origins.is_empty());
    assert_eq!(
        config
            .cooperative_actions
            .chrome_extension_reload
            .as_ref()
            .map(|reload| reload.relay_origin.as_str()),
        Some("http://127.0.0.1:47911")
    );
    assert_eq!(config.control.port, 47_910);
    assert_eq!(config.services.len(), 2);

    let api = config
        .services
        .get("example-api")
        .expect("generic API service");
    assert_eq!(
        api.command,
        PathBuf::from(r"C:\Workspace\ExampleApi\example-api.exe")
    );
    assert_eq!(api.args, ["--port", "49001"]);
    assert_eq!(api.ports, vec![49_001]);
    assert_eq!(api.restart_policy, RestartPolicy::Manual);
    assert_eq!(api.shutdown_grace_ms, 5_000);
    match &api.health {
        HealthCheck::HttpJson {
            url,
            startup_deadline_ms,
            expect,
            ..
        } => {
            assert_eq!(url, "http://127.0.0.1:49001/health");
            assert_eq!(*startup_deadline_ms, 30_000);
            assert_eq!(expect.get("status"), Some(&serde_json::json!("ok")));
        }
        other => panic!("expected generic HTTP JSON health, got {other:?}"),
    }

    let web = config
        .services
        .get("example-web")
        .expect("generic web service");
    assert_eq!(
        web.command,
        PathBuf::from(r"C:\Program Files\nodejs\npm.cmd")
    );
    assert_eq!(web.args, ["run", "dev"]);
    assert_eq!(web.ports, vec![49_002]);
    assert_eq!(
        web.environment.get("EXAMPLE_PORT").map(String::as_str),
        Some("49002")
    );
    match &web.health {
        HealthCheck::TcpConnect {
            host,
            port,
            startup_deadline_ms,
            ..
        } => {
            assert_eq!(host, "127.0.0.1");
            assert_eq!(*port, 49_002);
            assert_eq!(*startup_deadline_ms, 60_000);
        }
        other => panic!("expected generic TCP connect health, got {other:?}"),
    }
}

#[test]
fn immutable_windows_example_requires_no_cooperative_target_contract() {
    let config = SupervisorConfig::parse_json(include_str!(
        "../config/examples/immutable-windows.services.json"
    ))
    .expect("checked-in immutable-program example must parse");

    assert!(config.cooperative_actions.chrome_extension_reload.is_none());
    assert!(!config.control.mcp.enabled);
    let service = config
        .services
        .get("legacy-api")
        .expect("immutable example service");
    assert!(matches!(service.health, HealthCheck::Process));
    assert_eq!(
        config.observability.console_events,
        ConsoleEvents::Lifecycle
    );
    assert_eq!(service.restart_policy, RestartPolicy::Manual);
    assert_eq!(service.shutdown_grace_ms, 5_000);
}

#[test]
fn typed_configuration_serializes_with_contract_field_names() {
    let config = SupervisorConfig {
        version: CONFIG_VERSION,
        control: ControlConfig {
            host: "127.0.0.1".to_owned(),
            port: 47_820,
            token_file: PathBuf::from(".runtime/control-token"),
            mcp: McpConfig::default(),
        },
        observability: ObservabilityConfig {
            console_events: ConsoleEvents::Verbose,
        },
        cooperative_actions: CooperativeActionsConfig::default(),
        services: BTreeMap::from([(
            "fixture".to_owned(),
            ServiceConfig {
                label: "Fixture".to_owned(),
                cwd: PathBuf::from(r"C:\fixture"),
                command: PathBuf::from(r"C:\fixture\service.exe"),
                args: Vec::new(),
                environment: BTreeMap::new(),
                health: HealthCheck::HttpStatus {
                    url: "http://127.0.0.1:49001/health".to_owned(),
                    expected_status: 200,
                    timeout_ms: 5_000,
                    startup_deadline_ms: 15_000,
                },
                ports: vec![49_001],
                restart_policy: RestartPolicy::Manual,
                shutdown_grace_ms: 3_000,
            },
        )]),
    };

    let value = serde_json::to_value(config).expect("typed configuration should serialize");
    assert_eq!(
        value
            .pointer("/observability/consoleEvents")
            .and_then(serde_json::Value::as_str),
        Some("verbose")
    );
    assert_eq!(
        value
            .pointer("/control/tokenFile")
            .and_then(serde_json::Value::as_str),
        Some(".runtime/control-token")
    );
    assert_eq!(
        value
            .pointer("/services/fixture/health/expectedStatus")
            .and_then(serde_json::Value::as_u64),
        Some(200)
    );
    assert_eq!(
        value
            .pointer("/services/fixture/restartPolicy")
            .and_then(serde_json::Value::as_str),
        Some("manual")
    );
}
