use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::application::{
    HealthCheckSpec, JsonPathMode, LaunchSpec, ServiceRegistration, ServiceRestartPolicy,
};

pub const CONFIG_VERSION: u32 = 2;

/// Versioned `AkuSupervisor` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorConfig {
    pub version: u32,
    pub control: ControlConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default, rename = "cooperativeActions")]
    pub cooperative_actions: CooperativeActionsConfig,
    #[serde(deserialize_with = "deserialize_services")]
    pub services: BTreeMap<String, ServiceConfig>,
}

/// Supervisor-owned event visibility. Durable lifecycle journaling remains
/// mandatory regardless of this console presentation setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservabilityConfig {
    #[serde(default)]
    pub console_events: ConsoleEvents,
}

/// Amount of canonical lifecycle-event detail mirrored to the foreground
/// console after the audit record has been persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleEvents {
    Off,
    #[default]
    Lifecycle,
    Verbose,
}

impl ConsoleEvents {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Lifecycle => "lifecycle",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CooperativeActionsConfig {
    #[serde(default)]
    pub chrome_extension_reload: Option<ChromeExtensionReloadConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChromeExtensionReloadConfig {
    pub relay_origin: String,
    pub target: String,
    pub timeout_ms: u64,
    pub poll_interval_ms: u64,
}

/// Loopback control-server settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlConfig {
    pub host: String,
    pub port: u16,
    pub token_file: PathBuf,
    #[serde(default)]
    pub mcp: McpConfig,
}

/// Optional read-only MCP endpoint on the existing control listener.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Exact trusted browser origins. Native clients normally omit Origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

/// One registered service profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceConfig {
    pub label: String,
    pub cwd: PathBuf,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub startup_prerequisites: Vec<StartupPrerequisite>,
    pub health: HealthCheck,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub restart_policy: RestartPolicy,
    pub shutdown_grace_ms: u64,
}

/// External loopback readiness that must pass before a service is spawned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StartupPrerequisite {
    TcpConnect {
        host: String,
        port: u16,
        #[serde(rename = "timeoutMs")]
        timeout_ms: u64,
        #[serde(rename = "startupDeadlineMs")]
        startup_deadline_ms: u64,
    },
}

impl StartupPrerequisite {
    fn to_spec(&self) -> HealthCheckSpec {
        match self {
            Self::TcpConnect {
                host,
                port,
                timeout_ms,
                startup_deadline_ms,
            } => HealthCheckSpec::TcpConnect {
                host: host.clone(),
                port: *port,
                timeout: Duration::from_millis(*timeout_ms),
                startup_deadline: Duration::from_millis(*startup_deadline_ms),
            },
        }
    }
}

impl ServiceConfig {
    /// Maps a registered service to the platform-neutral launch contract.
    ///
    /// Callers must validate the containing [`SupervisorConfig`] before using
    /// this value to spawn a process.
    #[must_use]
    pub fn launch_spec(&self) -> LaunchSpec {
        LaunchSpec::new(
            self.command.clone(),
            self.args.iter().cloned(),
            self.cwd.clone(),
            self.environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        )
    }
}

/// Supported health-check configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HealthCheck {
    Process,
    TcpConnect {
        host: String,
        port: u16,
        #[serde(rename = "timeoutMs")]
        timeout_ms: u64,
        #[serde(rename = "startupDeadlineMs")]
        startup_deadline_ms: u64,
    },
    HttpStatus {
        url: String,
        #[serde(rename = "expectedStatus")]
        expected_status: u16,
        #[serde(rename = "timeoutMs")]
        timeout_ms: u64,
        #[serde(rename = "startupDeadlineMs")]
        startup_deadline_ms: u64,
    },
    HttpJson {
        url: String,
        #[serde(rename = "timeoutMs")]
        timeout_ms: u64,
        #[serde(rename = "startupDeadlineMs")]
        startup_deadline_ms: u64,
        #[serde(
            default,
            rename = "pathMode",
            skip_serializing_if = "JsonPathMode::is_shallow"
        )]
        path_mode: JsonPathMode,
        expect: BTreeMap<String, serde_json::Value>,
        #[serde(default)]
        observe: Vec<String>,
    },
}

impl HealthCheck {
    /// Maximum time allowed for a newly spawned service to become healthy.
    #[must_use]
    pub const fn startup_deadline_ms(&self) -> u64 {
        match self {
            Self::Process => 0,
            Self::TcpConnect {
                startup_deadline_ms,
                ..
            }
            | Self::HttpStatus {
                startup_deadline_ms,
                ..
            }
            | Self::HttpJson {
                startup_deadline_ms,
                ..
            } => *startup_deadline_ms,
        }
    }

    fn to_spec(&self) -> HealthCheckSpec {
        match self {
            Self::Process => HealthCheckSpec::Process,
            Self::TcpConnect {
                host,
                port,
                timeout_ms,
                startup_deadline_ms,
            } => HealthCheckSpec::TcpConnect {
                host: host.clone(),
                port: *port,
                timeout: Duration::from_millis(*timeout_ms),
                startup_deadline: Duration::from_millis(*startup_deadline_ms),
            },
            Self::HttpStatus {
                url,
                expected_status,
                timeout_ms,
                startup_deadline_ms,
            } => HealthCheckSpec::HttpStatus {
                url: url.clone(),
                expected_status: *expected_status,
                timeout: Duration::from_millis(*timeout_ms),
                startup_deadline: Duration::from_millis(*startup_deadline_ms),
            },
            Self::HttpJson {
                url,
                timeout_ms,
                startup_deadline_ms,
                path_mode,
                expect,
                observe,
            } => HealthCheckSpec::HttpJson {
                url: url.clone(),
                timeout: Duration::from_millis(*timeout_ms),
                startup_deadline: Duration::from_millis(*startup_deadline_ms),
                path_mode: *path_mode,
                expect: expect.clone(),
                observe: observe.clone(),
            },
        }
    }
}

/// Bounded automatic-restart behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Manual,
    OnFailure,
}

impl RestartPolicy {
    const fn to_spec(self) -> ServiceRestartPolicy {
        match self {
            Self::Manual => ServiceRestartPolicy::Manual,
            Self::OnFailure => ServiceRestartPolicy::OnFailure,
        }
    }
}

impl SupervisorConfig {
    /// Parses JSON while rejecting duplicate service IDs and unknown fields.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] when the input is not valid JSON or does
    /// not match the typed configuration contract.
    pub fn parse_json(input: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(input).map_err(|error| ConfigError::Parse(error.to_string()))
    }

    /// Validates the contract and local filesystem without mutating either.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] containing every discovered issue.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let issues = self.validation_issues();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(issues))
        }
    }

    #[must_use]
    pub fn validation_issues(&self) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        if self.version != CONFIG_VERSION {
            issues.push(ConfigIssue::new(
                "version",
                "version_unsupported",
                format!("expected version {CONFIG_VERSION}, got {}", self.version),
            ));
        }
        if self.control.host != "127.0.0.1" {
            issues.push(ConfigIssue::new(
                "control.host",
                "control_host_not_loopback",
                "version 0 requires host 127.0.0.1",
            ));
        }
        if self.control.port == 0 {
            issues.push(ConfigIssue::new(
                "control.port",
                "control_port_invalid",
                "control port must be non-zero",
            ));
        }
        if !is_runtime_token_path(&self.control.token_file) {
            issues.push(ConfigIssue::new(
                "control.tokenFile",
                "token_path_outside_runtime",
                "token file must be a relative path beneath .runtime",
            ));
        }
        if self.services.is_empty() {
            issues.push(ConfigIssue::new(
                "services",
                "services_empty",
                "at least one registered service is required",
            ));
        }
        validate_cooperative_actions(&self.cooperative_actions, &mut issues);

        let mut claimed_ports = BTreeMap::from([(self.control.port, "control".to_owned())]);
        for (service_id, service) in &self.services {
            let prefix = format!("services.{service_id}");
            if !valid_service_id(service_id) {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.id"),
                    "service_id_invalid",
                    "service ID must contain lowercase ASCII letters, digits, or hyphens",
                ));
            }
            if service.label.trim().is_empty() {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.label"),
                    "service_label_empty",
                    "service label must not be empty",
                ));
            }
            validate_directory(&service.cwd, &prefix, &mut issues);
            validate_executable(&service.command, &prefix, &mut issues);
            if service.shutdown_grace_ms == 0 {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.shutdownGraceMs"),
                    "shutdown_grace_invalid",
                    "shutdown grace must be greater than zero",
                ));
            }
            validate_health(&service.health, &prefix, &mut issues);
            validate_startup_prerequisites(&service.startup_prerequisites, &prefix, &mut issues);

            let mut local_ports = BTreeSet::new();
            for port in &service.ports {
                if *port == 0 {
                    issues.push(ConfigIssue::new(
                        format!("{prefix}.ports"),
                        "service_port_invalid",
                        "declared ports must be non-zero",
                    ));
                    continue;
                }
                if !local_ports.insert(*port) {
                    issues.push(ConfigIssue::new(
                        format!("{prefix}.ports"),
                        "service_port_duplicate",
                        format!("port {port} is declared more than once"),
                    ));
                    continue;
                }
                match claimed_ports.entry(*port) {
                    Entry::Vacant(entry) => {
                        entry.insert(service_id.clone());
                    }
                    Entry::Occupied(entry) => {
                        issues.push(ConfigIssue::new(
                            format!("{prefix}.ports"),
                            "declared_port_conflict",
                            format!("port {port} is already declared by {}", entry.get()),
                        ));
                    }
                }
            }
        }
        issues
    }

    /// Computes a deterministic fingerprint from the typed, ordered contract.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Fingerprint`] if the typed configuration cannot
    /// be serialized for hashing.
    pub fn fingerprint(&self) -> Result<String, ConfigError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| ConfigError::Fingerprint(error.to_string()))?;
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    /// Maps validated configuration to platform-neutral service definitions.
    ///
    /// Callers must invoke [`Self::validate`] before using these registrations.
    #[must_use]
    pub fn service_registrations(&self) -> Vec<ServiceRegistration> {
        self.services
            .iter()
            .map(|(service_id, service)| {
                ServiceRegistration::new(
                    service_id.clone(),
                    service.label.clone(),
                    service.launch_spec(),
                    service.health.to_spec(),
                    service.restart_policy.to_spec(),
                    service.ports.clone(),
                    Duration::from_millis(service.shutdown_grace_ms),
                )
                .with_startup_prerequisites(
                    service
                        .startup_prerequisites
                        .iter()
                        .map(StartupPrerequisite::to_spec),
                )
            })
            .collect()
    }

    /// Maps services to registrations whose output is captured beneath the
    /// supplied runtime services directory.
    #[must_use]
    pub fn service_registrations_with_logs(
        &self,
        runtime_services_directory: &Path,
    ) -> Vec<ServiceRegistration> {
        self.services
            .iter()
            .map(|(service_id, service)| {
                let launch = service.launch_spec().with_log_files(
                    runtime_services_directory.join(format!("{service_id}.stdout.log")),
                    runtime_services_directory.join(format!("{service_id}.stderr.log")),
                );
                ServiceRegistration::new(
                    service_id.clone(),
                    service.label.clone(),
                    launch,
                    service.health.to_spec(),
                    service.restart_policy.to_spec(),
                    service.ports.clone(),
                    Duration::from_millis(service.shutdown_grace_ms),
                )
                .with_startup_prerequisites(
                    service
                        .startup_prerequisites
                        .iter()
                        .map(StartupPrerequisite::to_spec),
                )
            })
            .collect()
    }
}

fn validate_cooperative_actions(actions: &CooperativeActionsConfig, issues: &mut Vec<ConfigIssue>) {
    let Some(reload) = &actions.chrome_extension_reload else {
        return;
    };
    let valid_origin = reload
        .relay_origin
        .strip_prefix("http://")
        .filter(|authority| !authority.contains('/'))
        .and_then(|authority| authority.parse::<std::net::SocketAddr>().ok())
        .is_some_and(|address| address.ip().is_loopback());
    if !valid_origin {
        issues.push(ConfigIssue::new(
            "cooperativeActions.chromeExtensionReload.relayOrigin",
            "extension_relay_origin_invalid",
            "Chrome extension reload requires a pathless loopback HTTP relay origin with an explicit port",
        ));
    }
    if reload.target.trim().is_empty() {
        issues.push(ConfigIssue::new(
            "cooperativeActions.chromeExtensionReload.target",
            "extension_reload_target_invalid",
            "Chrome extension reload target must not be empty",
        ));
    }
    if !(1_000..=60_000).contains(&reload.timeout_ms) {
        issues.push(ConfigIssue::new(
            "cooperativeActions.chromeExtensionReload.timeoutMs",
            "extension_reload_timeout_invalid",
            "Chrome extension reload timeout must be between 1000 and 60000 milliseconds",
        ));
    }
    if reload.poll_interval_ms == 0 || reload.poll_interval_ms > reload.timeout_ms {
        issues.push(ConfigIssue::new(
            "cooperativeActions.chromeExtensionReload.pollIntervalMs",
            "extension_reload_poll_invalid",
            "Chrome extension reload poll interval must be non-zero and no greater than its timeout",
        ));
    }
}

fn validate_directory(path: &Path, prefix: &str, issues: &mut Vec<ConfigIssue>) {
    if !path.is_absolute() {
        issues.push(ConfigIssue::new(
            format!("{prefix}.cwd"),
            "cwd_not_absolute",
            "working directory must be absolute",
        ));
    } else if !path.is_dir() {
        issues.push(ConfigIssue::new(
            format!("{prefix}.cwd"),
            "cwd_missing",
            "working directory does not exist",
        ));
    }
}

fn validate_executable(path: &Path, prefix: &str, issues: &mut Vec<ConfigIssue>) {
    if !path.is_absolute() {
        issues.push(ConfigIssue::new(
            format!("{prefix}.command"),
            "command_not_absolute",
            "command must be an absolute executable path",
        ));
    } else if !path.is_file() {
        issues.push(ConfigIssue::new(
            format!("{prefix}.command"),
            "command_missing",
            "configured executable does not exist",
        ));
    }
}

fn validate_health(health: &HealthCheck, prefix: &str, issues: &mut Vec<ConfigIssue>) {
    match health {
        HealthCheck::Process => {}
        HealthCheck::TcpConnect {
            host,
            port,
            timeout_ms,
            startup_deadline_ms,
        } => {
            let valid_loopback_host = host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
            if !valid_loopback_host {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.health.host"),
                    "health_host_invalid",
                    "TCP health host must be an explicit loopback IP",
                ));
            }
            if *port == 0 {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.health.port"),
                    "health_port_invalid",
                    "TCP health port must be greater than zero",
                ));
            }
            validate_transport_deadlines(*timeout_ms, *startup_deadline_ms, prefix, issues);
        }
        HealthCheck::HttpStatus {
            url,
            expected_status,
            timeout_ms,
            startup_deadline_ms,
        } => {
            validate_http_fields(url, *timeout_ms, *startup_deadline_ms, prefix, issues);
            if !(100..=599).contains(expected_status) {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.health.expectedStatus"),
                    "health_status_invalid",
                    "expected HTTP status must be between 100 and 599",
                ));
            }
        }
        HealthCheck::HttpJson {
            url,
            timeout_ms,
            startup_deadline_ms,
            path_mode,
            expect,
            observe,
        } => {
            validate_http_fields(url, *timeout_ms, *startup_deadline_ms, prefix, issues);
            validate_http_json_expectations(*path_mode, expect, observe, prefix, issues);
        }
    }
}

fn validate_startup_prerequisite(
    prerequisite: &StartupPrerequisite,
    prefix: &str,
    issues: &mut Vec<ConfigIssue>,
) {
    match prerequisite {
        StartupPrerequisite::TcpConnect {
            host,
            port,
            timeout_ms,
            startup_deadline_ms,
        } => {
            let valid_loopback_host = host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
            if !valid_loopback_host {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.host"),
                    "startup_prerequisite_host_invalid",
                    "TCP startup prerequisite host must be an explicit loopback IP",
                ));
            }
            if *port == 0 {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.port"),
                    "startup_prerequisite_port_invalid",
                    "TCP startup prerequisite port must be greater than zero",
                ));
            }
            if *timeout_ms == 0 {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.timeoutMs"),
                    "startup_prerequisite_timeout_invalid",
                    "startup prerequisite timeout must be greater than zero",
                ));
            }
            if *startup_deadline_ms < *timeout_ms {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.startupDeadlineMs"),
                    "startup_prerequisite_deadline_invalid",
                    "startup prerequisite deadline must be at least its timeout",
                ));
            }
        }
    }
}

fn validate_startup_prerequisites(
    prerequisites: &[StartupPrerequisite],
    prefix: &str,
    issues: &mut Vec<ConfigIssue>,
) {
    if prerequisites.len() > 8 {
        issues.push(ConfigIssue::new(
            format!("{prefix}.startupPrerequisites"),
            "startup_prerequisites_too_many",
            "at most 8 startup prerequisites are allowed",
        ));
    }
    for (index, prerequisite) in prerequisites.iter().enumerate() {
        validate_startup_prerequisite(
            prerequisite,
            &format!("{prefix}.startupPrerequisites.{index}"),
            issues,
        );
    }
}

fn validate_http_json_expectations(
    path_mode: JsonPathMode,
    expect: &BTreeMap<String, serde_json::Value>,
    observe: &[String],
    prefix: &str,
    issues: &mut Vec<ConfigIssue>,
) {
    if expect.is_empty() {
        issues.push(ConfigIssue::new(
            format!("{prefix}.health.expect"),
            "health_expect_empty",
            "HTTP JSON health expectations must not be empty",
        ));
    }
    match path_mode {
        JsonPathMode::Shallow => {
            if expect
                .values()
                .any(|value| value.is_array() || value.is_object())
            {
                issues.push(ConfigIssue::new(
                    format!("{prefix}.health.expect"),
                    "health_expect_not_shallow",
                    "shallow HTTP JSON expectations support scalar fields only",
                ));
            }
        }
        JsonPathMode::JsonPointer => {
            for pointer in expect.keys() {
                if !valid_json_pointer(pointer) {
                    issues.push(ConfigIssue::new(
                        format!("{prefix}.health.expect"),
                        "health_json_pointer_invalid",
                        format!("invalid RFC 6901 JSON Pointer: {pointer}"),
                    ));
                }
            }
            for pointer in observe {
                if !valid_json_pointer(pointer) {
                    issues.push(ConfigIssue::new(
                        format!("{prefix}.health.observe"),
                        "health_json_pointer_invalid",
                        format!("invalid RFC 6901 JSON Pointer: {pointer}"),
                    ));
                }
            }
        }
    }
    if path_mode == JsonPathMode::Shallow && observe.iter().any(String::is_empty) {
        issues.push(ConfigIssue::new(
            format!("{prefix}.health.observe"),
            "health_observe_empty",
            "shallow HTTP JSON observation fields must not be empty",
        ));
    }
    let unique = observe.iter().collect::<BTreeSet<_>>();
    if unique.len() != observe.len() {
        issues.push(ConfigIssue::new(
            format!("{prefix}.health.observe"),
            "health_observe_duplicate",
            "HTTP JSON observation fields must be unique",
        ));
    }
}

fn valid_json_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }
    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            index += 1;
            if index >= bytes.len() || !matches!(bytes[index], b'0' | b'1') {
                return false;
            }
        }
        index += 1;
    }
    true
}

fn validate_http_fields(
    url: &str,
    timeout_ms: u64,
    startup_deadline_ms: u64,
    prefix: &str,
    issues: &mut Vec<ConfigIssue>,
) {
    let valid_loopback_url = url
        .strip_prefix("http://")
        .and_then(|remainder| remainder.split('/').next())
        .and_then(|authority| authority.parse::<std::net::SocketAddr>().ok())
        .is_some_and(|address| address.ip().is_loopback());
    if !valid_loopback_url {
        issues.push(ConfigIssue::new(
            format!("{prefix}.health.url"),
            "health_url_invalid",
            "health URL must use an explicit loopback HTTP IP and port",
        ));
    }
    validate_transport_deadlines(timeout_ms, startup_deadline_ms, prefix, issues);
}

fn validate_transport_deadlines(
    timeout_ms: u64,
    startup_deadline_ms: u64,
    prefix: &str,
    issues: &mut Vec<ConfigIssue>,
) {
    if timeout_ms == 0 {
        issues.push(ConfigIssue::new(
            format!("{prefix}.health.timeoutMs"),
            "health_timeout_invalid",
            "health timeout must be greater than zero",
        ));
    }
    if startup_deadline_ms < timeout_ms {
        issues.push(ConfigIssue::new(
            format!("{prefix}.health.startupDeadlineMs"),
            "startup_deadline_invalid",
            "startup deadline must be at least the health timeout",
        ));
    }
}

fn valid_service_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(crate) fn is_runtime_token_path(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    let mut components = path.components();
    if components.next() != Some(Component::Normal(".runtime".as_ref())) {
        return false;
    }
    let remaining = components.collect::<Vec<_>>();
    !remaining.is_empty()
        && remaining
            .iter()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn deserialize_services<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, ServiceConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ServiceMapVisitor;

    impl<'de> Visitor<'de> for ServiceMapVisitor {
        type Value = BTreeMap<String, ServiceConfig>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object containing unique service IDs")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut services = BTreeMap::new();
            while let Some((service_id, service)) = map.next_entry::<String, ServiceConfig>()? {
                if services.insert(service_id.clone(), service).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate service ID: {service_id}"
                    )));
                }
            }
            Ok(services)
        }
    }

    deserializer.deserialize_map(ServiceMapVisitor)
}

/// One structured configuration validation issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigIssue {
    pub path: String,
    pub code: String,
    pub message: String,
}

impl ConfigIssue {
    fn new(path: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Configuration parse, validation, or fingerprint failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Parse(String),
    Invalid(Vec<ConfigIssue>),
    Fingerprint(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "configuration parse failed: {message}"),
            Self::Invalid(issues) => {
                write!(
                    formatter,
                    "configuration has {} validation issue(s)",
                    issues.len()
                )?;
                for issue in issues {
                    write!(
                        formatter,
                        "; {} [{}] {}",
                        issue.path, issue.code, issue.message
                    )?;
                }
                Ok(())
            }
            Self::Fingerprint(message) => {
                write!(formatter, "configuration fingerprint failed: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use crate::application::JsonPathMode;

    use super::{
        CONFIG_VERSION, ConfigError, ConfigIssue, ConsoleEvents, ControlConfig,
        CooperativeActionsConfig, HealthCheck, McpConfig, ObservabilityConfig, RestartPolicy,
        ServiceConfig, StartupPrerequisite, SupervisorConfig,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aku-supervisor-config-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn valid_config() -> (TestDirectory, SupervisorConfig) {
        let directory = TestDirectory::create();
        let command = directory.0.join("service.exe");
        fs::write(&command, b"fixture").expect("fixture executable should be created");
        let service = ServiceConfig {
            label: "Fixture service".to_owned(),
            cwd: directory.0.clone(),
            command,
            args: vec!["--serve".to_owned()],
            environment: BTreeMap::new(),
            startup_prerequisites: Vec::new(),
            health: HealthCheck::Process,
            ports: vec![49_001],
            restart_policy: RestartPolicy::Manual,
            shutdown_grace_ms: 3_000,
        };
        let config = SupervisorConfig {
            version: CONFIG_VERSION,
            control: ControlConfig {
                host: "127.0.0.1".to_owned(),
                port: 47_820,
                token_file: PathBuf::from(".runtime/control-token"),
                mcp: McpConfig::default(),
            },
            observability: ObservabilityConfig::default(),
            cooperative_actions: CooperativeActionsConfig::default(),
            services: BTreeMap::from([("fixture".to_owned(), service)]),
        };
        (directory, config)
    }

    #[test]
    fn valid_local_configuration_passes() {
        let (_directory, config) = valid_config();
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(
            config.observability.console_events,
            ConsoleEvents::Lifecycle
        );
    }

    #[test]
    fn omitted_observability_defaults_to_lifecycle_console_events() {
        let (_directory, config) = valid_config();
        let mut value = serde_json::to_value(config).expect("serialize fixture configuration");
        value
            .as_object_mut()
            .expect("configuration object")
            .remove("observability");

        let parsed = SupervisorConfig::parse_json(&value.to_string())
            .expect("legacy configuration without observability must parse");

        assert_eq!(
            parsed.observability.console_events,
            ConsoleEvents::Lifecycle
        );
    }

    #[test]
    fn validated_service_maps_to_platform_neutral_launch_spec() {
        let (_directory, config) = valid_config();
        config.validate().expect("fixture configuration is valid");
        let service = config.services.get("fixture").expect("fixture service");

        let launch = service.launch_spec();

        assert_eq!(launch.executable(), service.command);
        assert_eq!(launch.cwd(), service.cwd);
        assert_eq!(launch.args(), [OsStr::new("--serve")]);
    }

    #[test]
    fn validated_config_maps_every_registered_service() {
        let (_directory, config) = valid_config();
        config.validate().expect("fixture configuration is valid");

        let registrations = config.service_registrations();

        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].id(), "fixture");
        assert_eq!(registrations[0].label(), "Fixture service");
    }

    #[test]
    fn rejects_duplicate_service_ids_during_parse() {
        let service = json!({
            "label": "fixture",
            "cwd": "C:\\fixture",
            "command": "C:\\fixture\\service.exe",
            "health": { "type": "process" },
            "restartPolicy": "manual",
            "shutdownGraceMs": 1000
        });
        let input = format!(
            r#"{{"version":2,"control":{{"host":"127.0.0.1","port":11121,"tokenFile":".runtime/control-token"}},"services":{{"fixture":{service},"fixture":{service}}}}}"#
        );

        let error = SupervisorConfig::parse_json(&input).expect_err("duplicate ID must fail");
        assert!(
            matches!(error, ConfigError::Parse(message) if message.contains("duplicate service ID"))
        );
    }

    #[test]
    fn rejects_external_control_host_and_token_path() {
        let (_directory, mut config) = valid_config();
        config.control.host = "0.0.0.0".to_owned();
        config.control.token_file = PathBuf::from("../token");

        let codes = config
            .validation_issues()
            .into_iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>();
        assert!(codes.contains(&"control_host_not_loopback".to_owned()));
        assert!(codes.contains(&"token_path_outside_runtime".to_owned()));
    }

    #[test]
    fn tcp_health_requires_a_bounded_loopback_listener() {
        let (_directory, mut config) = valid_config();
        let service = config.services.get_mut("fixture").expect("fixture service");
        service.health = HealthCheck::TcpConnect {
            host: "0.0.0.0".to_owned(),
            port: 0,
            timeout_ms: 0,
            startup_deadline_ms: 0,
        };

        let codes = config
            .validation_issues()
            .into_iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>();

        assert!(codes.contains(&"health_host_invalid".to_owned()));
        assert!(codes.contains(&"health_port_invalid".to_owned()));
        assert!(codes.contains(&"health_timeout_invalid".to_owned()));
    }

    #[test]
    fn startup_prerequisite_requires_a_bounded_loopback_listener() {
        let (_directory, mut config) = valid_config();
        let service = config.services.get_mut("fixture").expect("fixture service");
        service.startup_prerequisites = vec![StartupPrerequisite::TcpConnect {
            host: "0.0.0.0".to_owned(),
            port: 0,
            timeout_ms: 0,
            startup_deadline_ms: 0,
        }];

        let codes = config
            .validation_issues()
            .into_iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>();

        assert!(codes.contains(&"startup_prerequisite_host_invalid".to_owned()));
        assert!(codes.contains(&"startup_prerequisite_port_invalid".to_owned()));
        assert!(codes.contains(&"startup_prerequisite_timeout_invalid".to_owned()));
    }

    #[test]
    fn omitted_startup_prerequisites_remain_backward_compatible() {
        let (_directory, config) = valid_config();
        let mut value = serde_json::to_value(config).expect("serialize fixture configuration");
        value["services"]["fixture"]
            .as_object_mut()
            .expect("service object")
            .remove("startupPrerequisites");

        let parsed = SupervisorConfig::parse_json(&value.to_string())
            .expect("configuration without startup prerequisites must parse");

        assert!(parsed.services["fixture"].startup_prerequisites.is_empty());
    }

    #[test]
    fn http_json_observe_is_optional() {
        let health: HealthCheck = serde_json::from_value(json!({
            "type": "http-json",
            "url": "http://127.0.0.1:49001/health",
            "timeoutMs": 1000,
            "startupDeadlineMs": 5000,
            "expect": {"status": "ok"}
        }))
        .expect("HTTP JSON health without observations must parse");

        match health {
            HealthCheck::HttpJson { observe, .. } => assert!(observe.is_empty()),
            other => panic!("expected HTTP JSON health, got {other:?}"),
        }
    }

    #[test]
    fn http_json_pointer_accepts_nested_values_and_rejects_invalid_escape() {
        let (_directory, mut config) = valid_config();
        let service = config.services.get_mut("fixture").expect("fixture service");
        service.health = HealthCheck::HttpJson {
            url: "http://127.0.0.1:49001/health".to_owned(),
            timeout_ms: 1_000,
            startup_deadline_ms: 5_000,
            path_mode: JsonPathMode::JsonPointer,
            expect: BTreeMap::from([("/runtime/version".to_owned(), json!({"major": 1}))]),
            observe: vec!["/runtime/build".to_owned()],
        };
        assert!(config.validation_issues().is_empty());

        let service = config.services.get_mut("fixture").expect("fixture service");
        let HealthCheck::HttpJson { expect, .. } = &mut service.health else {
            panic!("expected HTTP JSON health");
        };
        expect.insert(String::new(), json!({"status": "ok"}));
        assert!(config.validation_issues().is_empty());

        let service = config.services.get_mut("fixture").expect("fixture service");
        let HealthCheck::HttpJson { expect, .. } = &mut service.health else {
            panic!("expected HTTP JSON health");
        };
        expect.insert("/runtime/~2invalid".to_owned(), json!(true));
        assert!(
            config
                .validation_issues()
                .iter()
                .any(|issue| issue.code == "health_json_pointer_invalid")
        );
    }

    #[test]
    fn rejects_missing_paths_and_duplicate_ports() {
        let (_directory, mut config) = valid_config();
        config.control.port = 49_001;
        let service = config.services.get_mut("fixture").expect("fixture service");
        service.cwd = PathBuf::from("relative");
        service.command = PathBuf::from("relative.exe");

        let codes = config
            .validation_issues()
            .into_iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>();
        assert!(codes.contains(&"cwd_not_absolute".to_owned()));
        assert!(codes.contains(&"command_not_absolute".to_owned()));
        assert!(codes.contains(&"declared_port_conflict".to_owned()));
    }

    #[test]
    fn invalid_configuration_display_includes_structured_issue_details() {
        let error = ConfigError::Invalid(vec![ConfigIssue::new(
            "services.fixture.ports",
            "declared_port_conflict",
            "port 49001 is already declared by control",
        )]);

        assert_eq!(
            error.to_string(),
            "configuration has 1 validation issue(s); services.fixture.ports [declared_port_conflict] port 49001 is already declared by control"
        );
    }

    #[test]
    fn fingerprint_is_stable_for_ordered_maps() {
        let (_directory, mut first) = valid_config();
        first
            .services
            .get_mut("fixture")
            .expect("fixture service")
            .environment = BTreeMap::from([
            ("ZETA".to_owned(), "last".to_owned()),
            ("ALPHA".to_owned(), "first".to_owned()),
        ]);
        let serialized = serde_json::to_string(&first).expect("configuration should serialize");
        let second = SupervisorConfig::parse_json(&serialized).expect("configuration should parse");

        assert_eq!(first.fingerprint(), second.fingerprint());
        assert!(
            first
                .fingerprint()
                .expect("fingerprint should succeed")
                .starts_with("sha256:")
        );
    }
}
