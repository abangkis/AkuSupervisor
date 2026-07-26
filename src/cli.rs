//! User-visible command-line boundary.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crate::VERSION;
use crate::adapters::console_time::DisplayTimezone;
use crate::adapters::control_http::ApiActor;
use crate::adapters::service_logs::{LiveLogCursor, LiveLogEventKind, LiveLogSelection, LogStream};
use crate::application::ControlAction;

const HELP: &str = "AkuSupervisor - local development service supervisor\n\n\
Usage:\n\
  aku-supervisor [--timezone <local|utc>]\n\
  aku-supervisor --config <path> [--timezone <local|utc>]\n\
  aku-supervisor run [--timezone <local|utc>]\n\
  aku-supervisor run --config <path> [--timezone <local|utc>]\n\
  aku-supervisor status [--json] [--config <path>]\n\
  aku-supervisor simple-status [--config <path>]\n\
  aku-supervisor instance-status [--json] [--config <path>]\n\
  aku-supervisor registry-status [--json] [--config <path>]\n\
  aku-supervisor events [--after <sequence>] [--limit <n>] [--json] [--config <path>]\n\
  aku-supervisor logs <service> [--stream <stdout|stderr>] [--tail <n>] [--json] [--config <path>]\n\
  aku-supervisor live-logs <service> [--stream <both|stdout|stderr>] [--tail <n>] [--timezone <local|utc>] [--json] [--config <path>]\n\
  aku-supervisor <start|stop|restart> <service> [--reason <text>] [--actor <user|codex>] [--request-id <id>] [--json] [--config <path>]\n\
  aku-supervisor supervisor shutdown --reason <text> --request-id <id> [--actor <user|codex>] [--json] [--config <path>]\n\
  aku-supervisor extension reload --reason <text> --request-id <id> [--actor <user|codex>] [--wait|--no-wait] [--json] [--config <path>]\n\
  aku-supervisor extension status --request-id <id> [--json] [--config <path>]\n\
  aku-supervisor extension validate --request-id <id> [--actor <user|codex>] [--config <path>]\n\
  aku-supervisor mcp-proxy [--config <path>]\n\
  aku-supervisor registration-mcp [--config <path>]\n\
  aku-supervisor registration capabilities [--json] [--config <path>]\n\
  aku-supervisor registration show <draft-id> [--json] [--config <path>]\n\
  aku-supervisor registration approve <draft-id> [--commit] [--config <path>]\n\
  aku-supervisor --help\n\
  aku-supervisor --version\n\n\
Without --config, AkuSupervisor checks AKU_SUPERVISOR_CONFIG and then the default user configuration.";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Run {
        config: Option<PathBuf>,
        timezone: DisplayTimezone,
    },
    ExtensionValidate {
        actor: ApiActor,
        request_id: String,
        config: Option<PathBuf>,
    },
    McpProxy {
        config: Option<PathBuf>,
    },
    RegistrationMcp {
        config: Option<PathBuf>,
    },
    RegistrationCapabilities {
        config: Option<PathBuf>,
        json: bool,
    },
    RegistrationShow {
        draft_id: String,
        config: Option<PathBuf>,
        json: bool,
    },
    RegistrationApprove {
        draft_id: String,
        config: Option<PathBuf>,
        commit: bool,
    },
    LiveLogs {
        service_id: String,
        stream: LiveLogSelection,
        tail: usize,
        config: Option<PathBuf>,
        json: bool,
        timezone: DisplayTimezone,
    },
    Remote(RemoteCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteCommand {
    Status {
        config: Option<PathBuf>,
        json: bool,
    },
    SimpleStatus {
        config: Option<PathBuf>,
    },
    InstanceStatus {
        config: Option<PathBuf>,
        json: bool,
    },
    RegistryStatus {
        config: Option<PathBuf>,
        json: bool,
    },
    Events {
        after: u64,
        limit: usize,
        config: Option<PathBuf>,
        json: bool,
    },
    Logs {
        service_id: String,
        stream: LogStream,
        tail: usize,
        config: Option<PathBuf>,
        json: bool,
    },
    Mutate {
        action: ControlAction,
        service_id: String,
        reason: String,
        actor: ApiActor,
        request_id: Option<String>,
        config: Option<PathBuf>,
        json: bool,
    },
    ExtensionReload {
        reason: String,
        actor: ApiActor,
        request_id: String,
        config: Option<PathBuf>,
        wait: bool,
        json: bool,
    },
    ExtensionStatus {
        request_id: String,
        config: Option<PathBuf>,
        json: bool,
    },
    SupervisorShutdown {
        reason: String,
        actor: ApiActor,
        request_id: String,
        config: Option<PathBuf>,
        json: bool,
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
        Ok(Command::Run { config, timezone }) => run_foreground(config, timezone),
        Ok(Command::ExtensionValidate {
            actor,
            request_id,
            config,
        }) => run_extension_validate(actor, &request_id, config.as_ref()),
        Ok(Command::McpProxy { config }) => match crate::adapters::mcp_proxy::run(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        },
        Ok(Command::RegistrationMcp { config }) => {
            match crate::adapters::registration_mcp::run(config.as_deref()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::RegistrationCapabilities { config, json }) => {
            run_registration_capabilities(config, json)
        }
        Ok(Command::RegistrationShow {
            draft_id,
            config,
            json,
        }) => run_registration_show(config, &draft_id, json),
        Ok(Command::RegistrationApprove {
            draft_id,
            config,
            commit,
        }) => run_registration_approve(config, &draft_id, commit),
        Ok(Command::LiveLogs {
            service_id,
            stream,
            tail,
            config,
            json,
            timezone,
        }) => run_live_logs(&service_id, stream, tail, config, json, timezone),
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
        [] => Ok(Command::Run {
            config: None,
            timezone: DisplayTimezone::Local,
        }),
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        [flag] if flag == "--version" || flag == "-V" => Ok(Command::Version),
        [run, rest @ ..] if run == "run" => parse_run(rest),
        [flag, ..] if flag == "--config" || flag == "--timezone" => parse_run(&arguments),
        [proxy] if proxy == "mcp-proxy" => Ok(Command::McpProxy { config: None }),
        [proxy, config_flag, config] if proxy == "mcp-proxy" && config_flag == "--config" => {
            Ok(Command::McpProxy {
                config: Some(PathBuf::from(config)),
            })
        }
        [proxy] if proxy == "registration-mcp" => Ok(Command::RegistrationMcp { config: None }),
        [proxy, config_flag, config]
            if proxy == "registration-mcp" && config_flag == "--config" =>
        {
            Ok(Command::RegistrationMcp {
                config: Some(PathBuf::from(config)),
            })
        }
        [registration, rest @ ..] if registration == "registration" => parse_registration(rest),
        [status, rest @ ..] if status == "status" => parse_remote_status(rest),
        [status, rest @ ..] if status == "simple-status" => parse_simple_status(rest),
        [status, rest @ ..] if status == "instance-status" => parse_instance_status(rest),
        [status, rest @ ..] if status == "registry-status" => parse_registry_status(rest),
        [events, rest @ ..] if events == "events" => parse_remote_events(rest),
        [logs, rest @ ..] if logs == "logs" => parse_remote_logs(rest),
        [logs, rest @ ..] if logs == "live-logs" => parse_live_logs(rest),
        [extension, reload, rest @ ..] if extension == "extension" && reload == "reload" => {
            parse_extension_reload(rest)
        }
        [extension, status, rest @ ..] if extension == "extension" && status == "status" => {
            parse_extension_status(rest)
        }
        [extension, validate, rest @ ..] if extension == "extension" && validate == "validate" => {
            parse_extension_validate(rest)
        }
        [supervisor, shutdown, rest @ ..]
            if supervisor == "supervisor" && shutdown == "shutdown" =>
        {
            parse_supervisor_shutdown(rest)
        }
        [action, rest @ ..] if action == "start" || action == "stop" || action == "restart" => {
            parse_remote_mutation(action, rest, true)
        }
        [argument] => Err(format!(
            "unsupported argument: {}",
            argument.to_string_lossy()
        )),
        _ => Err("expected run, a lifecycle client command, --help, or --version".to_owned()),
    }
}

fn parse_run(arguments: &[OsString]) -> Result<Command, String> {
    let mut config = None;
    let mut timezone = DisplayTimezone::Local;
    let mut timezone_set = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
            }
            Some("--timezone") if !timezone_set => {
                timezone = parse_display_timezone(option_value(arguments, index)?)?;
                timezone_set = true;
            }
            Some("--config" | "--timezone") => {
                return Err(format!(
                    "duplicate option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
        }
        index += 2;
    }
    Ok(Command::Run { config, timezone })
}

fn parse_display_timezone(value: &OsString) -> Result<DisplayTimezone, String> {
    value
        .to_str()
        .and_then(DisplayTimezone::parse)
        .ok_or_else(|| "--timezone must be local or utc".to_owned())
}

fn parse_registration(arguments: &[OsString]) -> Result<Command, String> {
    let Some(action) = arguments.first().and_then(|value| value.to_str()) else {
        return Err("registration requires capabilities, show, or approve".to_owned());
    };
    match action {
        "capabilities" => {
            let (config, json) = parse_output_options(&arguments[1..])?;
            Ok(Command::RegistrationCapabilities { config, json })
        }
        "show" => {
            let draft_id = arguments
                .get(1)
                .and_then(|value| value.to_str())
                .ok_or_else(|| "registration show requires a draft ID".to_owned())?
                .to_owned();
            let (config, json) = parse_output_options(&arguments[2..])?;
            Ok(Command::RegistrationShow {
                draft_id,
                config,
                json,
            })
        }
        "approve" => {
            let draft_id = arguments
                .get(1)
                .and_then(|value| value.to_str())
                .ok_or_else(|| "registration approve requires a draft ID".to_owned())?
                .to_owned();
            let (config, commit) = parse_registration_approve_options(&arguments[2..])?;
            Ok(Command::RegistrationApprove {
                draft_id,
                config,
                commit,
            })
        }
        _ => Err("registration requires capabilities, show, or approve".to_owned()),
    }
}

fn parse_registration_approve_options(
    arguments: &[OsString],
) -> Result<(Option<PathBuf>, bool), String> {
    let mut config = None;
    let mut commit = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--commit") if !commit => {
                commit = true;
                index += 1;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
                index += 2;
            }
            Some("--json") => {
                return Err("interactive approval does not support --json".to_owned());
            }
            Some("--commit" | "--config") => {
                return Err(format!(
                    "duplicate option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
        }
    }
    Ok((config, commit))
}

fn run_registration_capabilities(config: Option<PathBuf>, json_output: bool) -> ExitCode {
    let result = crate::adapters::registration::RegistrationAuthority::open(config)
        .and_then(|authority| authority.capabilities());
    print_registration_result(result, json_output)
}

fn run_registration_show(config: Option<PathBuf>, draft_id: &str, json_output: bool) -> ExitCode {
    let result = crate::adapters::registration::RegistrationAuthority::open(config)
        .and_then(|authority| authority.get_draft(draft_id))
        .and_then(|draft| {
            serde_json::to_value(draft).map_err(|error| {
                crate::adapters::registration::RegistrationError::serialization(error)
            })
        });
    print_registration_result(result, json_output)
}

fn run_registration_approve(config: Option<PathBuf>, draft_id: &str, commit: bool) -> ExitCode {
    let authority = match crate::adapters::registration::RegistrationAuthority::open(config) {
        Ok(authority) => authority,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if commit {
        match authority.approve_and_commit_interactive(draft_id) {
            Ok(result) => {
                println!(
                    "\nAPPROVED AND COMMITTED: {}\n{}",
                    result.draft_id,
                    serde_json::to_string_pretty(&result)
                        .expect("commit result serialization cannot fail")
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        }
    } else {
        match authority.approve_interactive(draft_id) {
            Ok(draft) => {
                println!(
                    "\nAPPROVED ONLY: {}\nProposal hash: {}\nThe configuration has not changed. Run this command again with --commit or ask the MCP client to commit the approved draft.",
                    draft.draft_id, draft.proposal_hash
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        }
    }
}

fn print_registration_result(
    result: Result<serde_json::Value, crate::adapters::registration::RegistrationError>,
    json_output: bool,
) -> ExitCode {
    match result {
        Ok(value) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&value).expect("JSON value serialization cannot fail")
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value)
                        .expect("JSON value serialization cannot fail")
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"error":error.structured()}))
                        .expect("JSON value serialization cannot fail")
                );
            } else {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn parse_extension_validate(arguments: &[OsString]) -> Result<Command, String> {
    let mut actor = ApiActor::User;
    let mut actor_seen = false;
    let mut request_id = None;
    let mut config = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--actor") if !actor_seen => {
                actor = match option_value(arguments, index)?.to_str() {
                    Some("user") => ApiActor::User,
                    Some("codex") => ApiActor::Codex,
                    _ => return Err("actor must be user or codex".to_owned()),
                };
                actor_seen = true;
                index += 2;
            }
            Some("--request-id") if request_id.is_none() => {
                request_id = Some(parse_request_id(option_value(arguments, index)?)?);
                index += 2;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
                index += 2;
            }
            Some("--actor" | "--request-id" | "--config") => {
                return Err(format!(
                    "duplicate option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
        }
    }
    Ok(Command::ExtensionValidate {
        actor,
        request_id: request_id
            .ok_or_else(|| "extension validate requires --request-id".to_owned())?,
        config,
    })
}

fn parse_extension_reload(arguments: &[OsString]) -> Result<Command, String> {
    let mut wait = true;
    let mut wait_option_seen = false;
    let mut mutation_options = Vec::new();
    for argument in arguments {
        match argument.to_str() {
            Some("--wait" | "--no-wait") if wait_option_seen => {
                return Err("duplicate option: --wait/--no-wait".to_owned());
            }
            Some("--wait") => {
                wait = true;
                wait_option_seen = true;
            }
            Some("--no-wait") => {
                wait = false;
                wait_option_seen = true;
            }
            _ => mutation_options.push(argument.clone()),
        }
    }
    let mut mutation_arguments = vec![OsString::from("chrome-extension")];
    mutation_arguments.extend(mutation_options);
    let parsed = parse_remote_mutation(&OsString::from("restart"), &mutation_arguments, false)?;
    let Command::Remote(RemoteCommand::Mutate {
        reason,
        actor,
        request_id,
        config,
        json,
        ..
    }) = parsed
    else {
        unreachable!("synthetic lifecycle parse must return a mutation")
    };
    let request_id = request_id.ok_or_else(|| {
        "extension reload requires --request-id for relay idempotency and audit".to_owned()
    })?;
    Ok(Command::Remote(RemoteCommand::ExtensionReload {
        reason,
        actor,
        request_id,
        config,
        wait,
        json,
    }))
}

fn parse_remote_status(arguments: &[OsString]) -> Result<Command, String> {
    let (config, json) = parse_output_options(arguments)?;
    Ok(Command::Remote(RemoteCommand::Status { config, json }))
}

fn parse_simple_status(arguments: &[OsString]) -> Result<Command, String> {
    let (config, json) = parse_output_options(arguments)?;
    if json {
        return Err(
            "simple-status always uses the human-readable table; use status --json for machine output"
                .to_owned(),
        );
    }
    Ok(Command::Remote(RemoteCommand::SimpleStatus { config }))
}

fn parse_instance_status(arguments: &[OsString]) -> Result<Command, String> {
    let (config, json) = parse_output_options(arguments)?;
    Ok(Command::Remote(RemoteCommand::InstanceStatus {
        config,
        json,
    }))
}

fn parse_supervisor_shutdown(arguments: &[OsString]) -> Result<Command, String> {
    let mut mutation_arguments = vec![OsString::from("supervisor")];
    mutation_arguments.extend_from_slice(arguments);
    let parsed = parse_remote_mutation(&OsString::from("stop"), &mutation_arguments, false)?;
    let Command::Remote(RemoteCommand::Mutate {
        reason,
        actor,
        request_id,
        config,
        json,
        ..
    }) = parsed
    else {
        unreachable!("synthetic lifecycle parse must return a mutation")
    };
    Ok(Command::Remote(RemoteCommand::SupervisorShutdown {
        reason,
        actor,
        request_id: request_id.ok_or_else(|| {
            "supervisor shutdown requires --request-id for idempotency and audit".to_owned()
        })?,
        config,
        json,
    }))
}

fn parse_registry_status(arguments: &[OsString]) -> Result<Command, String> {
    let (config, json) = parse_output_options(arguments)?;
    Ok(Command::Remote(RemoteCommand::RegistryStatus {
        config,
        json,
    }))
}

fn parse_extension_status(arguments: &[OsString]) -> Result<Command, String> {
    let mut request_id = None;
    let mut config = None;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--json") if !json => {
                json = true;
                index += 1;
            }
            Some("--request-id") if request_id.is_none() => {
                let value = option_value(arguments, index)?;
                request_id = Some(parse_request_id(value)?);
                index += 2;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
                index += 2;
            }
            Some("--json" | "--request-id" | "--config") => {
                return Err(format!(
                    "duplicate option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
        }
    }
    Ok(Command::Remote(RemoteCommand::ExtensionStatus {
        request_id: request_id
            .ok_or_else(|| "extension status requires --request-id".to_owned())?,
        config,
        json,
    }))
}

fn parse_remote_events(arguments: &[OsString]) -> Result<Command, String> {
    let mut after = 0_u64;
    let mut limit = 50_usize;
    let mut config = None;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.to_str() {
            Some("--json") if !json => {
                json = true;
                index += 1;
                continue;
            }
            Some("--after") => {
                let value = option_value(arguments, index)?;
                after = value
                    .to_str()
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| "--after must be a non-negative integer".to_owned())?;
            }
            Some("--limit") => {
                let value = option_value(arguments, index)?;
                limit = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| (1..=200).contains(value))
                    .ok_or_else(|| "--limit must be between 1 and 200".to_owned())?;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
            }
            Some("--config" | "--json") => {
                return Err(format!("duplicate option: {}", flag.to_string_lossy()));
            }
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    Ok(Command::Remote(RemoteCommand::Events {
        after,
        limit,
        config,
        json,
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
    let mut json = false;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.to_str() {
            Some("--json") if !json => {
                json = true;
                index += 1;
                continue;
            }
            Some("--stream") => {
                let value = option_value(arguments, index)?;
                stream = value
                    .to_str()
                    .and_then(LogStream::parse)
                    .ok_or_else(|| "--stream must be stdout or stderr".to_owned())?;
            }
            Some("--tail") => {
                let value = option_value(arguments, index)?;
                tail = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| (1..=1_000).contains(value))
                    .ok_or_else(|| "--tail must be between 1 and 1000".to_owned())?;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
            }
            Some("--config" | "--json") => {
                return Err(format!("duplicate option: {}", flag.to_string_lossy()));
            }
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    Ok(Command::Remote(RemoteCommand::Logs {
        service_id,
        stream,
        tail,
        config,
        json,
    }))
}

fn parse_live_logs(arguments: &[OsString]) -> Result<Command, String> {
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
    let mut stream = LiveLogSelection::Both;
    let mut tail = 50_usize;
    let mut config = None;
    let mut json = false;
    let mut timezone = DisplayTimezone::Local;
    let mut timezone_set = false;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.to_str() {
            Some("--json") if !json => {
                json = true;
                index += 1;
                continue;
            }
            Some("--stream") => {
                let value = option_value(arguments, index)?;
                stream = value
                    .to_str()
                    .and_then(LiveLogSelection::parse)
                    .ok_or_else(|| "--stream must be both, stdout, or stderr".to_owned())?;
            }
            Some("--tail") => {
                let value = option_value(arguments, index)?;
                tail = value
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| *value <= 1_000)
                    .ok_or_else(|| "--tail must be between 0 and 1000".to_owned())?;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
            }
            Some("--timezone") if !timezone_set => {
                timezone = parse_display_timezone(option_value(arguments, index)?)?;
                timezone_set = true;
            }
            Some("--config" | "--json" | "--timezone") => {
                return Err(format!("duplicate option: {}", flag.to_string_lossy()));
            }
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    Ok(Command::LiveLogs {
        service_id,
        stream,
        tail,
        config,
        json,
        timezone,
    })
}

fn parse_remote_mutation(
    action: &OsString,
    arguments: &[OsString],
    allow_default_user_reason: bool,
) -> Result<Command, String> {
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
    let mut json = false;
    let mut index = 1;
    while index < arguments.len() {
        let flag = &arguments[index];
        match flag.to_str() {
            Some("--json") if !json => {
                json = true;
                index += 1;
                continue;
            }
            Some("--reason") if reason.is_none() => {
                let value = option_value(arguments, index)?;
                reason = Some(
                    value
                        .to_str()
                        .ok_or_else(|| "reason must be UTF-8".to_owned())?
                        .to_owned(),
                );
            }
            Some("--actor") => {
                let value = option_value(arguments, index)?;
                actor = match value.to_str() {
                    Some("user") => ApiActor::User,
                    Some("codex") => ApiActor::Codex,
                    _ => return Err("actor must be user or codex".to_owned()),
                };
            }
            Some("--request-id") if request_id.is_none() => {
                request_id = Some(parse_request_id(option_value(arguments, index)?)?);
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
            }
            Some("--reason" | "--request-id" | "--config" | "--json") => {
                return Err(format!("duplicate option: {}", flag.to_string_lossy()));
            }
            _ => return Err(format!("unsupported option: {}", flag.to_string_lossy())),
        }
        index += 2;
    }
    let action = match action.to_str() {
        Some("start") => ControlAction::Start,
        Some("stop") => ControlAction::Stop,
        Some("restart") => ControlAction::Restart,
        _ => unreachable!("caller selected a lifecycle action"),
    };
    let reason = match reason {
        Some(reason) => reason,
        None if allow_default_user_reason && actor == ApiActor::User => {
            let action = match action {
                ControlAction::Start => "start",
                ControlAction::Stop => "stop",
                ControlAction::Restart => "restart",
            };
            format!("user CLI {action} request")
        }
        None => return Err("--reason <text> is required".to_owned()),
    };
    Ok(Command::Remote(RemoteCommand::Mutate {
        action,
        service_id,
        reason,
        actor,
        request_id,
        config,
        json,
    }))
}

fn parse_output_options(arguments: &[OsString]) -> Result<(Option<PathBuf>, bool), String> {
    let mut config = None;
    let mut json = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--json") if !json => {
                json = true;
                index += 1;
            }
            Some("--config") if config.is_none() => {
                config = Some(PathBuf::from(option_value(arguments, index)?));
                index += 2;
            }
            Some("--json" | "--config") => {
                return Err(format!(
                    "duplicate option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
            _ => {
                return Err(format!(
                    "unsupported option: {}",
                    arguments[index].to_string_lossy()
                ));
            }
        }
    }
    Ok((config, json))
}

fn option_value(arguments: &[OsString], index: usize) -> Result<&OsString, String> {
    arguments
        .get(index + 1)
        .ok_or_else(|| format!("{} requires a value", arguments[index].to_string_lossy()))
}

fn parse_request_id(value: &OsString) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| "request ID must be UTF-8".to_owned())?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err("request ID must be 1-128 URL-safe ASCII characters".to_owned());
    }
    Ok(value.to_owned())
}

#[cfg(windows)]
fn run_foreground(explicit_config: Option<PathBuf>, timezone: DisplayTimezone) -> ExitCode {
    let resolved = match crate::adapters::config_path::resolve_config_path(explicit_config) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match crate::adapters::foreground::run(&resolved, timezone) {
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

fn run_extension_validate(
    actor: ApiActor,
    request_id: &str,
    explicit_config: Option<&PathBuf>,
) -> ExitCode {
    match extension_validation(actor, request_id, explicit_config.cloned()) {
        Ok((configuration, address, report)) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "configuration": configuration,
                    "controlApi": format!("http://{address}"),
                    "validation": report,
                }))
                .expect("extension validation JSON serialization cannot fail")
            );
            ExitCode::from(report.exit_code)
        }
        Err(message) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "configuration": explicit_config,
                    "controlApi": serde_json::Value::Null,
                    "validation": {
                        "schemaVersion": 1,
                        "command": "extension_validate",
                        "status": "error",
                        "exitCode": 1,
                        "requestId": request_id,
                        "actor": validation_actor(actor),
                        "checks": [],
                        "operation": serde_json::Value::Null,
                        "error": {
                            "code": "validation_execution_failed",
                            "message": message,
                        }
                    }
                }))
                .expect("extension validation error JSON serialization cannot fail")
            );
            ExitCode::FAILURE
        }
    }
}

fn extension_validation(
    actor: ApiActor,
    request_id: &str,
    explicit_config: Option<PathBuf>,
) -> Result<
    (
        PathBuf,
        std::net::SocketAddr,
        crate::application::ExtensionValidationReport,
    ),
    String,
> {
    use std::fs;

    use crate::adapters::config::SupervisorConfig;
    use crate::adapters::config_path::resolve_config_path;
    use crate::adapters::runtime_token::{RuntimeToken, resolve_token_path};

    let resolved = resolve_config_path(explicit_config).map_err(|error| error.to_string())?;
    let source = fs::read_to_string(resolved.path())
        .map_err(|error| format!("failed to read {}: {error}", resolved.path().display()))?;
    let config = SupervisorConfig::parse_json(&source).map_err(|error| error.to_string())?;
    config.validate().map_err(|error| error.to_string())?;
    let token_path = resolve_token_path(resolved.path(), &config.control.token_file);
    let token = RuntimeToken::load(&token_path).map_err(|error| error.to_string())?;
    let address = format!("{}:{}", config.control.host, config.control.port)
        .parse::<std::net::SocketAddr>()
        .map_err(|error| format!("invalid control address: {error}"))?;
    ensure_fresh_extension_validation_request(address, &token, request_id)?;
    let reason = format!("release gate extension validation {request_id}");
    let initial = control_request_with_retry(
        address,
        &token,
        "POST",
        "/v1/cooperative-actions/chrome-extension/reload-self",
        Some(&serde_json::json!({
            "actor": actor,
            "reason": reason,
            "requestId": request_id,
        })),
        true,
    )?;
    let reload = config.cooperative_actions.chrome_extension_reload.as_ref();
    let timeout = reload.map_or(25_000, |value| value.timeout_ms.saturating_add(5_000));
    let poll_interval = reload.map_or(250, |value| value.poll_interval_ms);
    let terminal = wait_for_cooperative_operation(
        address,
        &token,
        request_id,
        timeout,
        poll_interval,
        initial,
    )?;
    let operation = terminal
        .get("operation")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let active = control_request_with_retry(
        address,
        &token,
        "GET",
        "/v1/cooperative-actions/chrome-extension/active",
        None,
        true,
    )?;
    let active_operation = active
        .get("operation")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let audit_path = token_path
        .parent()
        .ok_or_else(|| "control token path has no runtime directory".to_owned())?
        .join("cooperative-actions.jsonl");
    let audit_source = fs::read_to_string(&audit_path)
        .map_err(|error| format!("failed to read {}: {error}", audit_path.display()))?;
    let mut audit_records = Vec::new();
    for (index, line) in audit_source.lines().enumerate() {
        let record: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "invalid cooperative audit record {} in {}: {error}",
                index + 1,
                audit_path.display()
            )
        })?;
        if record.get("requestId").and_then(serde_json::Value::as_str) == Some(request_id) {
            audit_records.push(record);
        }
    }
    let report = crate::application::validate_extension_release(
        request_id,
        validation_actor(actor),
        operation,
        active_operation,
        &audit_records,
    );
    Ok((resolved.path().to_owned(), address, report))
}

fn ensure_fresh_extension_validation_request(
    address: std::net::SocketAddr,
    token: &crate::adapters::runtime_token::RuntimeToken,
    request_id: &str,
) -> Result<(), String> {
    use crate::adapters::control_http::{ControlClientError, client_request};

    const MAX_ATTEMPTS: usize = 5;
    let target = format!("/v1/cooperative-actions/chrome-extension/requests/{request_id}");
    for attempt in 1..=MAX_ATTEMPTS {
        match client_request(address, token, "GET", &target, None) {
            Err(ControlClientError::Rejected { status: 404, .. }) => return Ok(()),
            Ok(_) => {
                return Err(format!(
                    "extension validate requires a fresh request ID; {request_id} already exists"
                ));
            }
            Err(error) if error.is_transient() && attempt < MAX_ATTEMPTS => {
                let backoff_ms = 100_u64 << (attempt - 1);
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!("bounded validation preflight returns from every final attempt")
}

fn run_live_logs(
    service_id: &str,
    selection: LiveLogSelection,
    tail: usize,
    explicit_config: Option<PathBuf>,
    json_output: bool,
    timezone: DisplayTimezone,
) -> ExitCode {
    match live_logs(
        service_id,
        selection,
        tail,
        explicit_config,
        json_output,
        timezone,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn live_logs(
    service_id: &str,
    selection: LiveLogSelection,
    tail: usize,
    explicit_config: Option<PathBuf>,
    json_output: bool,
    timezone: DisplayTimezone,
) -> Result<(), String> {
    use std::fs;
    use std::net::SocketAddr;

    use crate::adapters::config::SupervisorConfig;
    use crate::adapters::config_path::resolve_config_path;
    use crate::adapters::control_http::client_live_logs;
    use crate::adapters::runtime_token::{RuntimeToken, resolve_token_path};

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
    let stream_name = match selection {
        LiveLogSelection::Both => "both",
        LiveLogSelection::Stdout => "stdout",
        LiveLogSelection::Stderr => "stderr",
    };
    if !json_output {
        println!("Configuration: {}", resolved.path().display());
        println!("Control API: http://{address}");
        println!(
            "Live logs: {service_id} (stream={stream_name}, initial tail={tail}); press Ctrl+C to stop viewing."
        );
        println!(
            "Human timestamp display: {} (NDJSON remains UTC Unix milliseconds)",
            timezone.as_str()
        );
    }

    let mut cursor: Option<LiveLogCursor> = None;
    let mut connected_once = false;
    let mut last_heartbeat_notice = None;
    loop {
        let target = live_log_target(service_id, stream_name, tail, cursor.as_ref());
        let mut client = match client_live_logs(address, &token, &target) {
            Ok(client) => client,
            Err(error) if connected_once && error.is_transient() => {
                if !json_output {
                    eprintln!("[live-logs] connection unavailable; retrying...");
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        connected_once = true;
        loop {
            let event = match client.next_event() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) if error.is_transient() => {
                    if !json_output {
                        eprintln!("[live-logs] connection interrupted; reconnecting...");
                    }
                    break;
                }
                Err(error) => return Err(error.to_string()),
            };
            advance_live_log_cursor(&mut cursor, &event);
            let show_heartbeat = json_output
                || event.kind != LiveLogEventKind::Heartbeat
                || last_heartbeat_notice.is_none_or(|instant: std::time::Instant| {
                    instant.elapsed() >= Duration::from_mins(1)
                });
            if event.kind == LiveLogEventKind::Heartbeat && show_heartbeat && !json_output {
                last_heartbeat_notice = Some(std::time::Instant::now());
            }
            print_live_log_event(&event, json_output, show_heartbeat, timezone);
        }
        if !json_output {
            eprintln!("[live-logs] connection closed; reconnecting...");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn advance_live_log_cursor(
    cursor: &mut Option<LiveLogCursor>,
    event: &crate::adapters::service_logs::LiveLogEvent,
) {
    let Some(sequence) = event.sequence else {
        return;
    };
    match cursor.as_mut() {
        Some(cursor) if cursor.hub_instance_id == event.hub_instance_id => {
            cursor.sequence = cursor.sequence.max(sequence);
        }
        _ => {
            *cursor = Some(LiveLogCursor {
                hub_instance_id: event.hub_instance_id.clone(),
                sequence,
            });
        }
    }
}

fn live_log_target(
    service_id: &str,
    stream_name: &str,
    tail: usize,
    cursor: Option<&LiveLogCursor>,
) -> String {
    cursor.map_or_else(
        || format!("/v1/services/{service_id}/logs/live?stream={stream_name}&tail={tail}"),
        |cursor| {
            format!(
                "/v1/services/{service_id}/logs/live?stream={stream_name}&tail={tail}&afterHub={}&after={}",
                cursor.hub_instance_id, cursor.sequence
            )
        },
    )
}

fn print_live_log_event(
    event: &crate::adapters::service_logs::LiveLogEvent,
    json_output: bool,
    show_heartbeat: bool,
    timezone: DisplayTimezone,
) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(event).expect("live log event JSON serialization cannot fail")
        );
        return;
    }
    match event.kind {
        LiveLogEventKind::Line => {
            let timestamp = crate::adapters::console_time::format_unix_milliseconds(
                u128::from(event.observed_at_unix_ms),
                timezone,
            )
            .unwrap_or_else(|| event.observed_at_unix_ms.to_string());
            let stream = match event.stream {
                Some(LogStream::Stdout) => "stdout",
                Some(LogStream::Stderr) => "stderr",
                None => "unknown",
            };
            let stream = if event.replayed {
                format!("{stream}/replay")
            } else {
                stream.to_owned()
            };
            println!(
                "[{timestamp}] [{stream}] {}",
                event.text.as_deref().unwrap_or_default()
            );
        }
        LiveLogEventKind::Gap => {
            eprintln!(
                "[live-logs] viewer fell behind; {} event(s) were dropped.",
                event.dropped.unwrap_or(0)
            );
        }
        LiveLogEventKind::Heartbeat if show_heartbeat => {
            let timestamp = crate::adapters::console_time::format_unix_milliseconds(
                u128::from(event.observed_at_unix_ms),
                timezone,
            )
            .unwrap_or_else(|| event.observed_at_unix_ms.to_string());
            eprintln!(
                "[{timestamp}] [live-logs] stream connected; service output is idle (hub={})",
                event.hub_instance_id
            );
        }
        LiveLogEventKind::HubReset => {
            eprintln!(
                "[live-logs] reconnected to a new AkuSupervisor instance: {} -> {}; replay cursor reset.",
                event
                    .previous_hub_instance_id
                    .as_deref()
                    .unwrap_or("unknown"),
                event.hub_instance_id
            );
        }
        LiveLogEventKind::Heartbeat => {}
    }
}

fn validation_actor(actor: ApiActor) -> serde_json::Value {
    match actor {
        ApiActor::User => serde_json::json!({"actorType": "user", "actorId": "cli"}),
        ApiActor::Codex => serde_json::json!({"actorType": "agent", "actorId": "codex"}),
    }
}

fn remote_request(command: RemoteCommand) -> Result<(), String> {
    use std::fs;
    use std::net::SocketAddr;

    use crate::adapters::config::SupervisorConfig;
    use crate::adapters::config_path::resolve_config_path;
    use crate::adapters::runtime_token::{RuntimeToken, resolve_token_path};

    let explicit_config = match &command {
        RemoteCommand::Status { config, .. }
        | RemoteCommand::SimpleStatus { config }
        | RemoteCommand::InstanceStatus { config, .. }
        | RemoteCommand::RegistryStatus { config, .. }
        | RemoteCommand::Events { config, .. }
        | RemoteCommand::Logs { config, .. }
        | RemoteCommand::Mutate { config, .. }
        | RemoteCommand::ExtensionReload { config, .. }
        | RemoteCommand::ExtensionStatus { config, .. }
        | RemoteCommand::SupervisorShutdown { config, .. } => config.clone(),
    };
    let json_output = match &command {
        RemoteCommand::Status { json, .. }
        | RemoteCommand::RegistryStatus { json, .. }
        | RemoteCommand::InstanceStatus { json, .. }
        | RemoteCommand::Events { json, .. }
        | RemoteCommand::Logs { json, .. }
        | RemoteCommand::Mutate { json, .. }
        | RemoteCommand::ExtensionReload { json, .. }
        | RemoteCommand::ExtensionStatus { json, .. }
        | RemoteCommand::SupervisorShutdown { json, .. } => *json,
        RemoteCommand::SimpleStatus { .. } => false,
    };
    let simple_status = matches!(&command, RemoteCommand::SimpleStatus { .. });
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

    let response_timeout = lifecycle_response_timeout(&config, &command);
    let (method, target, body, wait_request_id, retry_safe) = prepare_remote_request(command);
    let waited_for_operation = wait_request_id.is_some();
    let mut response = control_request_with_retry_timeout(
        address,
        &token,
        method,
        &target,
        body.as_ref(),
        retry_safe,
        response_timeout,
    )?;
    if simple_status {
        let registry =
            control_request_with_retry(address, &token, "GET", "/v1/registry", None, false)
                .ok()
                .and_then(|value| value.get("registry").cloned())
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "state": "unavailable",
                        "activeRevision": null,
                        "diskRevision": null,
                        "detail": "registry reconciliation status is unavailable"
                    })
                });
        if let Some(object) = response.as_object_mut() {
            object.insert("registry".to_owned(), registry);
        }
    }
    if let Some(request_id) = wait_request_id {
        let reload = config.cooperative_actions.chrome_extension_reload.as_ref();
        let timeout = reload.map_or(25_000, |value| value.timeout_ms.saturating_add(5_000));
        let poll_interval = reload.map_or(250, |value| value.poll_interval_ms);
        response = wait_for_cooperative_operation(
            address,
            &token,
            &request_id,
            timeout,
            poll_interval,
            response,
        )?;
    }
    print_remote_response(
        resolved.path(),
        address,
        &response,
        json_output,
        simple_status,
    )?;
    if waited_for_operation && response["operation"]["status"].as_str() == Some("failed") {
        let category = response["operation"]["errorCategory"]
            .as_str()
            .unwrap_or("cooperative_action_failed");
        let message = response["operation"]["message"]
            .as_str()
            .unwrap_or("Chrome extension cooperative reload failed");
        return Err(format!("{category}: {message}"));
    }
    Ok(())
}

fn lifecycle_response_timeout(
    config: &crate::adapters::config::SupervisorConfig,
    command: &RemoteCommand,
) -> Option<Duration> {
    const OPERATION_MARGIN_MS: u64 = 10_000;

    let RemoteCommand::Mutate {
        action, service_id, ..
    } = command
    else {
        return None;
    };
    let service = config.services.get(service_id)?;
    let startup_ms = service.health.startup_deadline_ms();
    let operation_ms = match action {
        ControlAction::Start => startup_ms,
        ControlAction::Stop => service.shutdown_grace_ms,
        ControlAction::Restart => service.shutdown_grace_ms.saturating_add(startup_ms),
    };
    Some(Duration::from_millis(
        operation_ms.saturating_add(OPERATION_MARGIN_MS),
    ))
}

#[allow(clippy::too_many_lines)]
fn prepare_remote_request(
    command: RemoteCommand,
) -> (
    &'static str,
    String,
    Option<serde_json::Value>,
    Option<String>,
    bool,
) {
    match command {
        RemoteCommand::Status { .. } | RemoteCommand::SimpleStatus { .. } => {
            ("GET", "/v1/services".to_owned(), None, None, true)
        }
        RemoteCommand::InstanceStatus { .. } => {
            ("GET", "/v1/instance".to_owned(), None, None, true)
        }
        RemoteCommand::RegistryStatus { .. } => {
            ("GET", "/v1/registry".to_owned(), None, None, true)
        }
        RemoteCommand::Events { after, limit, .. } => (
            "GET",
            format!("/v1/events?after={after}&limit={limit}"),
            None,
            None,
            true,
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
            None,
            true,
        ),
        RemoteCommand::Mutate {
            action,
            service_id,
            reason,
            actor,
            request_id,
            ..
        } => {
            let retry_safe = request_id.is_some();
            let action = match action {
                ControlAction::Start => "start",
                ControlAction::Stop => "stop",
                ControlAction::Restart => "restart",
            };
            (
                "POST",
                format!("/v1/services/{service_id}/{action}"),
                Some(serde_json::json!({
                    "actor": actor,
                    "reason": reason,
                    "requestId": request_id
                })),
                None,
                retry_safe,
            )
        }
        RemoteCommand::ExtensionReload {
            reason,
            actor,
            request_id,
            wait,
            ..
        } => {
            let wait_request_id = wait.then(|| request_id.clone());
            (
                "POST",
                "/v1/cooperative-actions/chrome-extension/reload-self".to_owned(),
                Some(serde_json::json!({
                    "actor": actor,
                    "reason": reason,
                    "requestId": request_id
                })),
                wait_request_id,
                true,
            )
        }
        RemoteCommand::ExtensionStatus { request_id, .. } => (
            "GET",
            format!("/v1/cooperative-actions/chrome-extension/requests/{request_id}"),
            None,
            None,
            true,
        ),
        RemoteCommand::SupervisorShutdown {
            reason,
            actor,
            request_id,
            ..
        } => (
            "POST",
            "/v1/supervisor/shutdown".to_owned(),
            Some(serde_json::json!({
                "actor": actor,
                "reason": reason,
                "requestId": request_id
            })),
            None,
            true,
        ),
    }
}

fn control_request_with_retry(
    address: std::net::SocketAddr,
    token: &crate::adapters::runtime_token::RuntimeToken,
    method: &str,
    target: &str,
    body: Option<&serde_json::Value>,
    retry_safe: bool,
) -> Result<serde_json::Value, String> {
    control_request_with_retry_timeout(address, token, method, target, body, retry_safe, None)
}

fn control_request_with_retry_timeout(
    address: std::net::SocketAddr,
    address_token: &crate::adapters::runtime_token::RuntimeToken,
    method: &str,
    target: &str,
    body: Option<&serde_json::Value>,
    retry_safe: bool,
    response_timeout: Option<Duration>,
) -> Result<serde_json::Value, String> {
    const MAX_ATTEMPTS: usize = 5;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let response = if let Some(timeout) = response_timeout {
            crate::adapters::control_http::client_request_with_response_timeout(
                address,
                address_token,
                method,
                target,
                body.cloned(),
                timeout,
            )
        } else {
            crate::adapters::control_http::client_request(
                address,
                address_token,
                method,
                target,
                body.cloned(),
            )
        };
        match response {
            Ok(response) => return Ok(response),
            Err(error) if retry_safe && error.is_transient() && attempt < MAX_ATTEMPTS => {
                let backoff_ms = 100_u64 << (attempt - 1);
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn wait_for_cooperative_operation(
    address: std::net::SocketAddr,
    token: &crate::adapters::runtime_token::RuntimeToken,
    request_id: &str,
    timeout_ms: u64,
    poll_interval_ms: u64,
    mut response: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while response
        .get("operation")
        .and_then(|operation| operation.get("status"))
        .and_then(serde_json::Value::as_str)
        == Some("running")
    {
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for cooperative operation {request_id}; query it with extension status"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(poll_interval_ms));
        response = control_request_with_retry(
            address,
            token,
            "GET",
            &format!("/v1/cooperative-actions/chrome-extension/requests/{request_id}"),
            None,
            true,
        )?;
    }
    Ok(response)
}

fn print_remote_response(
    configuration: &std::path::Path,
    address: std::net::SocketAddr,
    response: &serde_json::Value,
    json_output: bool,
    simple_status: bool,
) -> Result<(), String> {
    if simple_status {
        println!("{}", format_simple_status(response)?);
        return Ok(());
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "configuration": configuration,
                "controlApi": format!("http://{address}"),
                "response": response,
            }))
            .expect("control response JSON serialization cannot fail")
        );
        return Ok(());
    }
    println!("Configuration: {}", configuration.display());
    println!("Control API: http://{address}");
    println!(
        "{}",
        serde_json::to_string_pretty(response)
            .expect("control response JSON serialization cannot fail")
    );
    Ok(())
}

fn format_simple_status(response: &serde_json::Value) -> Result<String, String> {
    let services = response
        .get("services")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "service status response omitted the services array".to_owned())?;
    let mut lines = Vec::new();
    if let Some(registry) = response.get("registry") {
        let field = |name: &str| {
            registry
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
        };
        let state = field("state");
        let active = field("activeRevision");
        let disk = field("diskRevision");
        if state != "current" || active == "-" || active != disk {
            let detail = field("detail").chars().take(160).collect::<String>();
            lines.push(format!(
                "REGISTRY WARNING: state={state} active={active} disk={disk} detail={detail}"
            ));
        }
    }
    lines.push(
        "SERVICE              STATE                DESIRED     HEALTH      ROOT PID   OWNED PIDS       HOLD"
            .to_owned(),
    );
    for service in services {
        let text = |field: &str| {
            service
                .get(field)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
        };
        let root_pid = service
            .get("rootPid")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "-".to_owned(), |pid| pid.to_string());
        let owned_pids = service
            .get("ownedPids")
            .and_then(serde_json::Value::as_array)
            .map(|pids| {
                pids.iter()
                    .filter_map(serde_json::Value::as_u64)
                    .map(|pid| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|pids| !pids.is_empty())
            .unwrap_or_else(|| "-".to_owned());
        let health = service
            .get("health")
            .and_then(|health| health.get("status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        lines.push(format!(
            "{:<20} {:<20} {:<11} {:<11} {:<10} {:<16} {}",
            text("id"),
            text("lifecycle"),
            text("desiredState"),
            health,
            root_pid,
            owned_pids,
            text("operatorHold")
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(not(windows))]
fn run_foreground(_explicit_config: Option<PathBuf>, _timezone: DisplayTimezone) -> ExitCode {
    eprintln!("error: no lifecycle platform adapter is implemented for this operating system");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        Command, RemoteCommand, advance_live_log_cursor, format_simple_status,
        lifecycle_response_timeout, live_log_target, parse,
    };
    use crate::adapters::config::SupervisorConfig;
    use crate::adapters::console_time::DisplayTimezone;
    use crate::adapters::control_http::ApiActor;
    use crate::adapters::service_logs::LiveLogSelection;
    use crate::application::ControlAction;

    fn args(values: &[&str]) -> Vec<std::ffi::OsString> {
        values.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn no_argument_starts_with_discovered_configuration() {
        let expected = Ok(Command::Run {
            config: None,
            timezone: DisplayTimezone::Local,
        });
        assert_eq!(parse(args(&[])), expected);
        assert_eq!(
            parse(args(&["run"])),
            Ok(Command::Run {
                config: None,
                timezone: DisplayTimezone::Local,
            })
        );
        assert_eq!(
            parse(args(&["run", "--timezone", "utc"])),
            Ok(Command::Run {
                config: None,
                timezone: DisplayTimezone::Utc,
            })
        );
        assert!(parse(args(&["run", "--timezone", "WIB"])).is_err());
    }

    #[test]
    fn version_flag_selects_version() {
        assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
    }

    #[test]
    fn live_logs_defaults_to_both_streams_and_accepts_a_filter() {
        assert_eq!(
            parse(args(&["live-logs", "api"])),
            Ok(Command::LiveLogs {
                service_id: "api".to_owned(),
                stream: LiveLogSelection::Both,
                tail: 50,
                config: None,
                json: false,
                timezone: DisplayTimezone::Local,
            })
        );
        assert_eq!(
            parse(args(&[
                "live-logs",
                "api",
                "--stream",
                "stderr",
                "--tail",
                "0",
                "--timezone",
                "utc",
                "--json",
            ])),
            Ok(Command::LiveLogs {
                service_id: "api".to_owned(),
                stream: LiveLogSelection::Stderr,
                tail: 0,
                config: None,
                json: true,
                timezone: DisplayTimezone::Utc,
            })
        );
        let cursor = crate::adapters::service_logs::LiveLogCursor {
            hub_instance_id: "123-current".to_owned(),
            sequence: 77,
        };
        assert_eq!(
            live_log_target("api", "both", 50, Some(&cursor)),
            "/v1/services/api/logs/live?stream=both&tail=50&afterHub=123-current&after=77"
        );

        let reset: crate::adapters::service_logs::LiveLogEvent =
            serde_json::from_value(serde_json::json!({
                "schemaVersion": 1,
                "kind": "hub_reset",
                "hubInstanceId": "456-new",
                "serviceId": "api",
                "sequence": 41,
                "observedAtUnixMs": 1,
                "previousHubInstanceId": "123-current"
            }))
            .expect("deserialize reset event");
        let mut resumed_cursor = Some(cursor);
        advance_live_log_cursor(&mut resumed_cursor, &reset);
        let replayed_line: crate::adapters::service_logs::LiveLogEvent =
            serde_json::from_value(serde_json::json!({
                "schemaVersion": 1,
                "kind": "line",
                "hubInstanceId": "456-new",
                "serviceId": "api",
                "sequence": 12,
                "stream": "stdout",
                "observedAtUnixMs": 1,
                "text": "persisted line",
                "replayed": true
            }))
            .expect("deserialize replayed line");
        advance_live_log_cursor(&mut resumed_cursor, &replayed_line);
        assert_eq!(
            resumed_cursor,
            Some(crate::adapters::service_logs::LiveLogCursor {
                hub_instance_id: "456-new".to_owned(),
                sequence: 41,
            })
        );
    }

    #[test]
    fn registration_authority_keeps_mcp_and_human_approval_distinct() {
        assert_eq!(
            parse(args(&["registration-mcp", "--config", "services.json"])),
            Ok(Command::RegistrationMcp {
                config: Some(PathBuf::from("services.json"))
            })
        );
        assert_eq!(
            parse(args(&[
                "registration",
                "show",
                "registration-0123456789abcdef0123",
                "--json"
            ])),
            Ok(Command::RegistrationShow {
                draft_id: "registration-0123456789abcdef0123".to_owned(),
                config: None,
                json: true,
            })
        );
        assert!(
            parse(args(&[
                "registration",
                "approve",
                "registration-0123456789abcdef0123",
                "--json"
            ]))
            .is_err()
        );
        assert_eq!(
            parse(args(&[
                "registration",
                "approve",
                "registration-0123456789abcdef0123",
                "--commit",
                "--config",
                "services.json",
            ])),
            Ok(Command::RegistrationApprove {
                draft_id: "registration-0123456789abcdef0123".to_owned(),
                config: Some(PathBuf::from("services.json")),
                commit: true,
            })
        );
        assert_eq!(
            parse(args(&[
                "registration",
                "approve",
                "registration-0123456789abcdef0123",
            ])),
            Ok(Command::RegistrationApprove {
                draft_id: "registration-0123456789abcdef0123".to_owned(),
                config: None,
                commit: false,
            })
        );
        assert!(
            parse(args(&[
                "registration",
                "approve",
                "registration-0123456789abcdef0123",
                "--commit",
                "--commit",
            ]))
            .is_err()
        );
        assert!(parse(args(&["registration", "commit", "anything"])).is_err());
    }

    #[test]
    fn lifecycle_timeout_tracks_the_registered_service_budget() {
        let config = SupervisorConfig::parse_json(include_str!(
            "../config/examples/development-workspace.services.json"
        ))
        .expect("generic development example must parse");
        let command = |action| RemoteCommand::Mutate {
            action,
            service_id: "example-api".to_owned(),
            reason: "timeout contract".to_owned(),
            actor: ApiActor::User,
            request_id: None,
            config: None,
            json: false,
        };

        assert_eq!(
            lifecycle_response_timeout(&config, &command(ControlAction::Start)),
            Some(Duration::from_secs(40))
        );
        assert_eq!(
            lifecycle_response_timeout(&config, &command(ControlAction::Stop)),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            lifecycle_response_timeout(&config, &command(ControlAction::Restart)),
            Some(Duration::from_secs(45))
        );
    }

    #[test]
    fn lifecycle_client_requires_service_and_defaults_user_reason() {
        assert!(parse(args(&["start"])).is_err());
        assert!(parse(args(&["start", "../api", "--reason", "bad"])).is_err());
        assert_eq!(
            parse(args(&["start", "api"])),
            Ok(Command::Remote(RemoteCommand::Mutate {
                action: ControlAction::Start,
                service_id: "api".to_owned(),
                reason: "user CLI start request".to_owned(),
                actor: ApiActor::User,
                request_id: None,
                config: None,
                json: false,
            }))
        );
        assert!(parse(args(&["start", "api", "--actor", "codex"])).is_err());
    }

    #[test]
    fn supervisor_shutdown_requires_explicit_auditable_input() {
        assert!(parse(args(&["supervisor", "shutdown"])).is_err());
        assert!(
            parse(args(&[
                "supervisor",
                "shutdown",
                "--reason",
                "replace orphan",
            ]))
            .is_err()
        );
        assert_eq!(
            parse(args(&[
                "supervisor",
                "shutdown",
                "--reason",
                "replace orphan",
                "--actor",
                "codex",
                "--request-id",
                "supervisor-shutdown-1",
                "--json",
            ])),
            Ok(Command::Remote(RemoteCommand::SupervisorShutdown {
                reason: "replace orphan".to_owned(),
                actor: ApiActor::Codex,
                request_id: "supervisor-shutdown-1".to_owned(),
                config: None,
                json: true,
            }))
        );
        assert_eq!(
            parse(args(&["instance-status", "--json"])),
            Ok(Command::Remote(RemoteCommand::InstanceStatus {
                config: None,
                json: true,
            }))
        );
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
                json: false,
            }))
        );
    }

    #[test]
    fn extension_reload_requires_a_bounded_request_id() {
        assert!(
            parse(args(&[
                "extension",
                "reload",
                "--reason",
                "load extension build",
            ]))
            .is_err()
        );
        assert_eq!(
            parse(args(&[
                "extension",
                "reload",
                "--reason",
                "load extension build",
                "--actor",
                "codex",
                "--request-id",
                "bridge-reload-1",
            ])),
            Ok(Command::Remote(RemoteCommand::ExtensionReload {
                reason: "load extension build".to_owned(),
                actor: ApiActor::Codex,
                request_id: "bridge-reload-1".to_owned(),
                config: None,
                wait: true,
                json: false,
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
                json: false,
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
                json: false,
            }))
        );
    }

    #[test]
    fn explicit_configuration_path_is_supported() {
        let expected = Ok(Command::Run {
            config: Some(PathBuf::from("services.json")),
            timezone: DisplayTimezone::Local,
        });
        assert_eq!(parse(args(&["--config", "services.json"])), expected);
        assert_eq!(
            parse(args(&["run", "--config", "services.json"])),
            Ok(Command::Run {
                config: Some(PathBuf::from("services.json")),
                timezone: DisplayTimezone::Local,
            })
        );
        assert_eq!(
            parse(args(&["status", "--config", "services.json"])),
            Ok(Command::Remote(RemoteCommand::Status {
                config: Some(PathBuf::from("services.json")),
                json: false,
            }))
        );
    }

    #[test]
    fn extension_status_and_machine_output_are_explicit() {
        assert_eq!(
            parse(args(&[
                "extension",
                "status",
                "--request-id",
                "bridge-reload-1",
                "--json",
            ])),
            Ok(Command::Remote(RemoteCommand::ExtensionStatus {
                request_id: "bridge-reload-1".to_owned(),
                config: None,
                json: true,
            }))
        );
        assert_eq!(
            parse(args(&["status", "--json"])),
            Ok(Command::Remote(RemoteCommand::Status {
                config: None,
                json: true,
            }))
        );
        assert!(parse(args(&["status", "--json", "--json"])).is_err());
    }

    #[test]
    fn simple_status_is_an_explicit_human_table_command() {
        assert_eq!(
            parse(args(&["simple-status"])),
            Ok(Command::Remote(RemoteCommand::SimpleStatus {
                config: None
            }))
        );
        assert_eq!(
            parse(args(&["simple-status", "--config", "services.json"])),
            Ok(Command::Remote(RemoteCommand::SimpleStatus {
                config: Some(PathBuf::from("services.json"))
            }))
        );
        assert!(parse(args(&["simple-status", "--json"])).is_err());

        let table = format_simple_status(&serde_json::json!({
            "registry": {
                "state": "current",
                "activeRevision": "sha256:same",
                "diskRevision": "sha256:same",
                "detail": null
            },
            "services": [{
                "id": "api",
                "lifecycle": "running",
                "desiredState": "running",
                "health": { "status": "healthy" },
                "rootPid": 1234,
                "ownedPids": [1234, 5678],
                "operatorHold": "none"
            }]
        }))
        .expect("valid service table");
        assert!(table.contains("SERVICE              STATE"));
        assert!(table.contains("api                  running"));
        assert!(table.contains("1234,5678"));
        assert!(!table.contains("REGISTRY WARNING"));

        let warning = format_simple_status(&serde_json::json!({
            "registry": {
                "state": "deferred",
                "activeRevision": "sha256:active",
                "diskRevision": "sha256:disk",
                "detail": "target is still running"
            },
            "services": []
        }))
        .expect("deferred registry table");
        assert!(warning.starts_with("REGISTRY WARNING: state=deferred"));
        assert!(warning.contains("active=sha256:active disk=sha256:disk"));
        assert!(warning.contains("target is still running"));
    }

    #[test]
    fn registry_status_exposes_human_and_machine_output() {
        assert_eq!(
            parse(args(&["registry-status"])),
            Ok(Command::Remote(RemoteCommand::RegistryStatus {
                config: None,
                json: false,
            }))
        );
        assert_eq!(
            parse(args(&[
                "registry-status",
                "--json",
                "--config",
                "services.json"
            ])),
            Ok(Command::Remote(RemoteCommand::RegistryStatus {
                config: Some(PathBuf::from("services.json")),
                json: true,
            }))
        );
    }

    #[test]
    fn extension_validate_requires_identity_and_has_machine_contract() {
        assert!(parse(args(&["extension", "validate"])).is_err());
        assert_eq!(
            parse(args(&[
                "extension",
                "validate",
                "--actor",
                "codex",
                "--request-id",
                "release-gate-1",
                "--config",
                "services.json",
            ])),
            Ok(Command::ExtensionValidate {
                actor: ApiActor::Codex,
                request_id: "release-gate-1".to_owned(),
                config: Some(PathBuf::from("services.json")),
            })
        );
        assert!(
            parse(args(&[
                "extension",
                "validate",
                "--request-id",
                "release-gate-1",
                "--actor",
                "user",
                "--actor",
                "codex",
            ]))
            .is_err()
        );
    }
}
