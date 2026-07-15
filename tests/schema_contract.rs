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
#[allow(clippy::too_many_lines)] // One end-to-end profile contract is easier to audit in one place.
fn checked_in_akuworkspace_profile_matches_the_typed_contract() {
    let config = SupervisorConfig::parse_json(include_str!("../config/akuworkspace.services.json"))
        .expect("checked-in AkuWorkspace profile must parse");

    assert!(config.cooperative_actions.aku_bridge_reload.is_some());
    assert!(config.control.mcp.enabled);
    assert_eq!(
        config.observability.console_events,
        ConsoleEvents::Lifecycle
    );
    assert!(config.control.mcp.allowed_origins.is_empty());
    assert_eq!(
        config
            .cooperative_actions
            .aku_bridge_reload
            .as_ref()
            .map(|reload| reload.sidecar_origin.as_str()),
        Some("http://127.0.0.1:47821")
    );
    assert_eq!(config.control.port, 47_820);
    assert_eq!(config.services.len(), 5);

    let sidecar = config
        .services
        .get("akusidecar")
        .expect("AkuSidecar service");
    assert_eq!(
        sidecar.command,
        PathBuf::from(r"C:\WorkspaceCodex\AkuWorkspace\AkuSidecar\runtime\dev\aku-sidecar.exe")
    );
    assert_eq!(
        sidecar.args,
        [
            "--config",
            r"C:\WorkspaceCodex\AkuWorkspace\AkuSidecar\config\sidecar.json",
            "--dev"
        ]
    );
    assert_eq!(sidecar.ports, vec![47_821]);
    assert_eq!(sidecar.restart_policy, RestartPolicy::Manual);
    assert_eq!(sidecar.shutdown_grace_ms, 5_000);
    match &sidecar.health {
        HealthCheck::HttpJson {
            url,
            startup_deadline_ms,
            expect,
            ..
        } => {
            assert_eq!(url, "http://127.0.0.1:47821/api/health");
            assert_eq!(*startup_deadline_ms, 60_000);
            assert_eq!(
                expect.get("version"),
                Some(&serde_json::json!("1.0.0-dev.2"))
            );
            assert_eq!(expect.get("runtime"), Some(&serde_json::json!("go")));
            assert_eq!(
                expect.get("bridgeContractVersion"),
                Some(&serde_json::json!("aku-browser.bridge.v2"))
            );
            assert_eq!(
                expect.get("provider"),
                Some(&serde_json::json!("codex-app-server"))
            );
        }
        other => panic!("expected AkuSidecar HTTP JSON health, got {other:?}"),
    }

    let service = config.services.get("geofu-be").expect("Geofu BE service");
    assert_eq!(
        service.cwd,
        PathBuf::from(r"C:\WorkspaceCodex\GeofuWorkspace\Geofu_be")
    );
    assert_eq!(
        service.command,
        PathBuf::from(r"C:\WorkspaceCodex\GeofuWorkspace\Geofu_be\output\geofu-server.exe")
    );
    assert_eq!(service.ports, vec![8_765]);
    assert_eq!(service.restart_policy, RestartPolicy::Manual);
    assert_eq!(service.shutdown_grace_ms, 5_000);
    assert_eq!(service.args.first().map(String::as_str), Some("--host"));
    assert!(service.environment.is_empty());

    match &service.health {
        HealthCheck::HttpJson {
            url,
            startup_deadline_ms,
            expect,
            ..
        } => {
            assert_eq!(url, "http://127.0.0.1:8765/catalog.json");
            assert_eq!(*startup_deadline_ms, 30_000);
            assert_eq!(expect.get("schemaVersion"), Some(&serde_json::json!(1)));
        }
        other => panic!("expected HTTP JSON health, got {other:?}"),
    }

    let plugin = config
        .services
        .get("geofu-plugin")
        .expect("Geofu plugin service");
    assert_eq!(
        plugin.cwd,
        PathBuf::from(r"C:\WorkspaceCodex\GeofuWorkspace\Geofu")
    );
    assert_eq!(plugin.command, PathBuf::from(r"C:\nvm4w\nodejs\npm.cmd"));
    assert_eq!(plugin.args, ["run", "dev"]);
    assert_eq!(plugin.ports, vec![8_766]);
    assert_eq!(plugin.restart_policy, RestartPolicy::Manual);
    assert_eq!(plugin.shutdown_grace_ms, 5_000);
    assert!(plugin.environment.is_empty());
    match &plugin.health {
        HealthCheck::HttpJson {
            url,
            startup_deadline_ms,
            expect,
            ..
        } => {
            assert_eq!(url, "http://127.0.0.1:8766/geofu/plugin.json");
            assert_eq!(*startup_deadline_ms, 30_000);
            assert_eq!(expect.get("id"), Some(&serde_json::json!("geofu")));
        }
        other => panic!("expected HTTP JSON health, got {other:?}"),
    }

    for (service_id, expected_script, expected_port, tcp_health) in [
        ("geolibre", "geofu:lan", 6_060, true),
        ("geolibre-locked", "geofu:locked-dev", 6_061, false),
    ] {
        assert_geolibre_service(
            &config,
            service_id,
            expected_script,
            expected_port,
            tcp_health,
        );
    }
}

fn assert_geolibre_service(
    config: &SupervisorConfig,
    service_id: &str,
    expected_script: &str,
    expected_port: u16,
    tcp_health: bool,
) {
    let geolibre = config
        .services
        .get(service_id)
        .unwrap_or_else(|| panic!("{service_id} service"));
    assert_eq!(
        geolibre.cwd,
        PathBuf::from(r"C:\WorkspaceCodex\GeofuWorkspace\GeoLibre")
    );
    assert_eq!(geolibre.command, PathBuf::from(r"C:\nvm4w\nodejs\npm.cmd"));
    assert_eq!(geolibre.args, ["run", expected_script]);
    assert_eq!(geolibre.ports, vec![expected_port]);
    assert_eq!(geolibre.restart_policy, RestartPolicy::Manual);
    assert_eq!(geolibre.shutdown_grace_ms, 5_000);
    assert!(
        !geolibre.environment.contains_key("GEOLIBRE_DEV_HOST"),
        "{service_id} must preserve the repository-native host binding"
    );
    let expected_port_string = expected_port.to_string();
    assert_eq!(
        geolibre
            .environment
            .get("GEOLIBRE_DEV_PORT")
            .map(String::as_str),
        Some(expected_port_string.as_str())
    );
    if tcp_health {
        match &geolibre.health {
            HealthCheck::TcpConnect {
                host,
                port,
                startup_deadline_ms,
                ..
            } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(*port, expected_port);
                assert_eq!(*startup_deadline_ms, 120_000);
            }
            other => panic!("expected TCP connect health, got {other:?}"),
        }
    } else {
        match &geolibre.health {
            HealthCheck::HttpStatus {
                url,
                expected_status,
                startup_deadline_ms,
                ..
            } => {
                assert_eq!(
                    url,
                    &format!("http://127.0.0.1:{expected_port}/favicon.png")
                );
                assert_eq!(*expected_status, 200);
                assert_eq!(*startup_deadline_ms, 120_000);
            }
            other => panic!("expected HTTP status health, got {other:?}"),
        }
    }
}

#[test]
fn immutable_windows_example_requires_no_cooperative_target_contract() {
    let config = SupervisorConfig::parse_json(include_str!(
        "../config/examples/immutable-windows.services.json"
    ))
    .expect("checked-in immutable-program example must parse");

    assert!(config.cooperative_actions.aku_bridge_reload.is_none());
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
