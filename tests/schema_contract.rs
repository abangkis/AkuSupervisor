use aku_supervisor::adapters::config::{
    CONFIG_VERSION, ControlConfig, CooperativeActionsConfig, HealthCheck, RestartPolicy,
    ServiceConfig, SupervisorConfig,
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
}

#[test]
fn checked_in_akuworkspace_profile_matches_the_typed_contract() {
    let config = SupervisorConfig::parse_json(include_str!("../config/akuworkspace.services.json"))
        .expect("checked-in AkuWorkspace profile must parse");

    assert!(config.cooperative_actions.aku_bridge_reload.is_some());
    assert_eq!(
        config
            .cooperative_actions
            .aku_bridge_reload
            .as_ref()
            .map(|reload| reload.sidecar_origin.as_str()),
        Some("http://127.0.0.1:47821")
    );
}

#[test]
fn typed_configuration_serializes_with_contract_field_names() {
    let config = SupervisorConfig {
        version: CONFIG_VERSION,
        control: ControlConfig {
            host: "127.0.0.1".to_owned(),
            port: 47_820,
            token_file: PathBuf::from(".runtime/control-token"),
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
