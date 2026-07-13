//! User-visible command-line boundary.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::VERSION;
use crate::adapters::control_http::ApiActor;
use crate::adapters::service_logs::LogStream;
use crate::application::ControlAction;

const HELP: &str = "AkuSupervisor - local development service supervisor\n\n\
Usage:\n\
  aku-supervisor\n\
  aku-supervisor --config <path>\n\
  aku-supervisor run\n\
  aku-supervisor run --config <path>\n\
  aku-supervisor status [--config <path>]\n\
  aku-supervisor events [--after <sequence>] [--limit <n>] [--config <path>]\n\
  aku-supervisor logs <service> [--stream <stdout|stderr>] [--tail <n>] [--config <path>]\n\
  aku-supervisor <start|stop|restart> <service> --reason <text> [--actor <user|codex>] [--request-id <id>] [--config <path>]\n\
  aku-supervisor bridge reload --reason <text> --request-id <id> [--actor <user|codex>] [--config <path>]\n\
  aku-supervisor --help\n\
  aku-supervisor --version\n\n\
Without --config, AkuSupervisor checks AKU_SUPERVISOR_CONFIG and then the default user configuration.";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Run { config: Option<PathBuf> },
    Remote(RemoteCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteCommand {
    Status {
        config: Option<PathBuf>,
    },
    Events {
        after: u64,
        limit: usize,
        config: Option<PathBuf>,
    },
    Logs {
        service_id: String,
        stream: LogStream,
        tail: usize,
        config: Option<PathBuf>,
    },
    Mutate {
        action: ControlAction,
        service_id: String,
        reason: String,
        actor: ApiActor,
        request_id: Option<String>,
        config: Option<PathBuf>,
    },
    BridgeReload {
        reason: String,
        actor: ApiActor,
        request_id: String,
        config: Option<PathBuf>,
    },
}

/// Runs the CLI using an argument iterator that excludes the executable name.
pub fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    match parse(arguments) {
        Ok(Command::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("aku-supervisor {VERSION}");
            ExitCode::SUCCESS
        }
        Ok(Command::Run { config }) => run_foreground(config),
        Ok(Command::Remote(command)) => run_remote(command),
        Err(message) => {
            eprintln!("error: {message}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => Ok(Command::Run { config: None }),
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        [flag] if flag == "--version" || flag == "-V" => Ok(Command::Version),
        [config_flag, config] if config_flag == "--config" => Ok(Command::Run {
            config: Some(PathBuf::from(config)),
        }),
        [run] if run == "run" => Ok(Command::Run { config: None }),
        [run, config_flag, config] if run == "run" && config_flag == "--config" => {
            Ok(Command::Run {
                config: Some(PathBuf::from(config)),
            })
        }
        [status, rest @ ..] if status == "status" => parse_remote_status(rest),
        [events, rest @ ..] if events == "events" => parse_remote_events(rest),
        [logs, rest @ ..] if logs == "logs" => parse_remote_logs(rest),
        [bridge, reload, rest @ ..] if bridge == "bridge" && reload == "reload" => {
            parse_bridge_reload(rest)
        }
        [action, rest @ ..] if action == "start" || action == "stop" || action == "restart" => {
            parse_remote_mutation(action, rest)
        }
        [argument] => Err(format!(
            "unsupported argument: {}",
            argument.to_string_lossy()
        )),
        _ => Err("expected run, a lifecycle client command, --help, or --version".to_owned()),
    }
}

fn parse_bridge_reload(arguments: &[OsString]) -> Result<Command, String> {
    let mut mutation_arguments = vec![OsString::from("aku-bridge")];
    mutation_arguments.extend_from_slice(arguments);
    let parsed = parse_remote_mutation(&OsString::from("restart"), &mutation_arguments)?;
    let Command::Remote(RemoteCommand::Mutate {
        reason,
        actor,
        request_id,
        config,
        ..
    }) = parsed
    else {
        unreachable!("synthetic lifecycle parse must return a mutation")
    };
    let request_id = request_id.ok_or_else(|| {
        "bridge reload requires --request-id for relay idempotency and audit".to_owned()
    })?;
    Ok(Command::Remote(RemoteCommand::BridgeReload {
        reason,
        actor,
        request_id,
        config,
    }))
}

fn parse_remote_status(arguments: &[OsString]) -> Result<Command, String> {
    let config = match arguments {
        [] => None,
        [flag, path] if flag == "--config" => Some(PathBuf::from(path)),
        _ => return Err("status accepts only an optional --config <path>".to_owned()),
    };
    Ok(Command::Remote(RemoteCommand::Status { config }))
}

fn parse_remote_events(arguments: &[OsString]) -> Result<Command, String> {
    let mut after = 0_u64;
    let mut limit = 50_usize;
    let mut config = None;
    let mut index = 0;
    while index < arguments.len() {
        let flag = &arguments[index];
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--after") => {
                after = value
                    .to_str()
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| "--after must be a non-negative integer".to_owned())?;
            }
            Some("--limit") => {
                limit = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| (1..=200).contains(value))
                    .ok_or_else(|| "--limit must be between 1 and 200".to_owned())?;
            }
            Some("--config") if config.is_none() => config = Some(PathBuf::from(value)),
            Some("--config") => return Err("duplicate option: --config".to_owned()),
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    Ok(Command::Remote(RemoteCommand::Events {
        after,
        limit,
        config,
    }))
}

fn parse_remote_logs(arguments: &[OsString]) -> Result<Command, String> {
    let service_id = arguments
        .first()
        .and_then(|value| value.to_str())
        .filter(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        .ok_or_else(|| "valid service ID is required".to_owned())?
        .to_owned();
    let mut stream = LogStream::Stdout;
    let mut tail = 100_usize;
    let mut config = None;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--stream") => {
                stream = value
                    .to_str()
                    .and_then(LogStream::parse)
                    .ok_or_else(|| "--stream must be stdout or stderr".to_owned())?;
            }
            Some("--tail") => {
                tail = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| (1..=1_000).contains(value))
                    .ok_or_else(|| "--tail must be between 1 and 1000".to_owned())?;
            }
            Some("--config") if config.is_none() => config = Some(PathBuf::from(value)),
            Some("--config") => return Err("duplicate option: --config".to_owned()),
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    Ok(Command::Remote(RemoteCommand::Logs {
        service_id,
        stream,
        tail,
        config,
    }))
}

fn parse_remote_mutation(action: &OsString, arguments: &[OsString]) -> Result<Command, String> {
    let Some(service_id) = arguments.first() else {
        return Err("service ID is required".to_owned());
    };
    let service_id = service_id
        .to_str()
        .ok_or_else(|| "service ID must be UTF-8".to_owned())?
        .to_owned();
    if service_id.is_empty()
        || !service_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(
            "service ID must contain lowercase ASCII letters, digits, or hyphens".to_owned(),
        );
    }

    let mut reason = None;
    let mut actor = ApiActor::User;
    let mut request_id = None;
    let mut config = None;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("{} requires a value", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--reason") if reason.is_none() => {
                reason = Some(
                    value
                        .to_str()
                        .ok_or_else(|| "reason must be UTF-8".to_owned())?
                        .to_owned(),
                );
            }
            Some("--actor") => {
                actor = match value.to_str() {
                    Some("user") => ApiActor::User,
                    Some("codex") => ApiActor::Codex,
                    _ => return Err("actor must be user or codex".to_owned()),
                };
            }
            Some("--request-id") if request_id.is_none() => {
                let value = value
                    .to_str()
                    .ok_or_else(|| "request ID must be UTF-8".to_owned())?;
                if value.is_empty()
                    || value.len() > 128
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
                    })
                {
                    return Err("request ID must be 1-128 URL-safe ASCII characters".to_owned());
                }
                request_id = Some(value.to_owned());
            }
            Some("--config") if config.is_none() => config = Some(PathBuf::from(value)),
            Some("--reason" | "--request-id" | "--config") => {
                return Err(format!("duplicate option: {}", flag.to_string_lossy()));
            }
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    let reason = reason.ok_or_else(|| "--reason <text> is required".to_owned())?;
    let action = match action.to_str() {
        Some("start") => ControlAction::Start,
        Some("stop") => ControlAction::Stop,
        Some("restart") => ControlAction::Restart,
        _ => unreachable!("caller selected a lifecycle action"),
    };
    Ok(Command::Remote(RemoteCommand::Mutate {
        action,
        service_id,
        reason,
        actor,
        request_id,
        config,
    }))
}

#[cfg(windows)]
fn run_foreground(explicit_config: Option<PathBuf>) -> ExitCode {
    let resolved = match crate::adapters::config_path::resolve_config_path(explicit_config) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match crate::adapters::foreground::run(&resolved) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_remote(command: RemoteCommand) -> ExitCode {
    match remote_request(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn remote_request(command: RemoteCommand) -> Result<(), String> {
    use std::fs;
    use std::net::SocketAddr;

    use serde_json::json;

    use crate::adapters::config::SupervisorConfig;
    use crate::adapters::config_path::resolve_config_path;
    use crate::adapters::control_http::client_request;
    use crate::adapters::runtime_token::{RuntimeToken, resolve_token_path};

    let explicit_config = match &command {
        RemoteCommand::Status { config }
        | RemoteCommand::Events { config, .. }
        | RemoteCommand::Logs { config, .. }
        | RemoteCommand::Mutate { config, .. }
        | RemoteCommand::BridgeReload { config, .. } => config.clone(),
    };
    let resolved = resolve_config_path(explicit_config).map_err(|error| error.to_string())?;
    let source = fs::read_to_string(resolved.path())
        .map_err(|error| format!("failed to read {}: {error}", resolved.path().display()))?;
    let config = SupervisorConfig::parse_json(&source).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    let token_path = resolve_token_path(resolved.path(), &config.control.token_file);
    let token = RuntimeToken::load(&token_path).map_err(|error| error.to_string())?;
    let address: SocketAddr = format!("{}:{}", config.control.host, config.control.port)
        .parse()
        .map_err(|error| format!("invalid control address: {error}"))?;

    let (method, target, body) = match command {
        RemoteCommand::Status { .. } => ("GET", "/v1/services".to_owned(), None),
        RemoteCommand::Events { after, limit, .. } => (
            "GET",
            format!("/v1/events?after={after}&limit={limit}"),
            None,
        ),
        RemoteCommand::Logs {
            service_id,
            stream,
            tail,
            ..
        } => (
            "GET",
            format!(
                "/v1/services/{service_id}/logs?stream={}&tail={tail}",
                match stream {
                    LogStream::Stdout => "stdout",
                    LogStream::Stderr => "stderr",
                }
            ),
            None,
        ),
        RemoteCommand::Mutate {
            action,
            service_id,
            reason,
            actor,
            request_id,
            ..
        } => {
            let action = match action {
                ControlAction::Start => "start",
                ControlAction::Stop => "stop",
                ControlAction::Restart => "restart",
            };
            (
                "POST",
                format!("/v1/services/{service_id}/{action}"),
                Some(json!({
                    "actor": actor,
                    "reason": reason,
                    "requestId": request_id
                })),
            )
        }
        RemoteCommand::BridgeReload {
            reason,
            actor,
            request_id,
            ..
        } => (
            "POST",
            "/v1/cooperative-actions/aku-bridge/reload-self".to_owned(),
            Some(json!({
                "actor": actor,
                "reason": reason,
                "requestId": request_id
            })),
        ),
    };
    let response = client_request(address, &token, method, &target, body)
        .map_err(|error| error.to_string())?;
    println!("Configuration: {}", resolved.path().display());
    println!("Control API: http://{address}");
    println!(
        "{}",
        serde_json::to_string_pretty(&response)
            .expect("control response JSON serialization cannot fail")
    );
    Ok(())
}

#[cfg(not(windows))]
fn run_foreground(_explicit_config: Option<PathBuf>) -> ExitCode {
    eprintln!("error: no lifecycle platform adapter is implemented for this operating system");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Command, RemoteCommand, parse};
    use crate::adapters::control_http::ApiActor;
    use crate::application::ControlAction;

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn no_argument_starts_with_discovered_configuration() {
        assert_eq!(parse(args(&[])), Ok(Command::Run { config: None }));
        assert_eq!(parse(args(&["run"])), Ok(Command::Run { config: None }));
    }

    #[test]
    fn version_flag_selects_version() {
        assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
    }

    #[test]
    fn lifecycle_client_requires_service_and_reason() {
        assert!(parse(args(&["start"])).is_err());
        assert!(parse(args(&["start", "api"])).is_err());
        assert!(parse(args(&["start", "../api", "--reason", "bad"])).is_err());
    }

    #[test]
    fn lifecycle_client_keeps_actor_and_config_explicit() {
        assert_eq!(
            parse(args(&[
                "restart",
                "akusidecar",
                "--actor",
                "codex",
                "--reason",
                "source changed",
                "--config",
                "services.json",
            ])),
            Ok(Command::Remote(RemoteCommand::Mutate {
                action: ControlAction::Restart,
                service_id: "akusidecar".to_owned(),
                reason: "source changed".to_owned(),
                actor: ApiActor::Codex,
                request_id: None,
                config: Some(PathBuf::from("services.json")),
            }))
        );
    }

    #[test]
    fn bridge_reload_requires_a_bounded_request_id() {
        assert!(
            parse(args(&[
                "bridge",
                "reload",
                "--reason",
                "load extension build",
            ]))
            .is_err()
        );
        assert_eq!(
            parse(args(&[
                "bridge",
                "reload",
                "--reason",
                "load extension build",
                "--actor",
                "codex",
                "--request-id",
                "bridge-reload-1",
            ])),
            Ok(Command::Remote(RemoteCommand::BridgeReload {
                reason: "load extension build".to_owned(),
                actor: ApiActor::Codex,
                request_id: "bridge-reload-1".to_owned(),
                config: None,
            }))
        );
    }

    #[test]
    fn events_and_idempotency_arguments_are_bounded() {
        assert!(parse(args(&["events", "--limit", "0"])).is_err());
        assert_eq!(
            parse(args(&["events", "--after", "7", "--limit", "20"])),
            Ok(Command::Remote(RemoteCommand::Events {
                after: 7,
                limit: 20,
                config: None,
            }))
        );
        assert!(
            parse(args(&[
                "start",
                "api",
                "--reason",
                "retry",
                "--request-id",
                "contains space",
            ]))
            .is_err()
        );
        assert_eq!(
            parse(args(
                &["logs", "api", "--stream", "stderr", "--tail", "25",]
            )),
            Ok(Command::Remote(RemoteCommand::Logs {
                service_id: "api".to_owned(),
                stream: crate::adapters::service_logs::LogStream::Stderr,
                tail: 25,
                config: None,
            }))
        );
    }

    #[test]
    fn explicit_configuration_path_is_supported() {
        let expected = Ok(Command::Run {
            config: Some(PathBuf::from("services.json")),
        });
        assert_eq!(parse(args(&["--config", "services.json"])), expected);
        assert_eq!(
            parse(args(&["run", "--config", "services.json"])),
            Ok(Command::Run {
                config: Some(PathBuf::from("services.json"))
            })
        );
        assert_eq!(
            parse(args(&["status", "--config", "services.json"])),
            Ok(Command::Remote(RemoteCommand::Status {
                config: Some(PathBuf::from("services.json"))
            }))
        );
    }
}
