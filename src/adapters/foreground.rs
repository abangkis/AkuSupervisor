use std::fmt;
use std::fs;
use std::io::{self, BufRead};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::adapters::config::{ConfigError, SupervisorConfig};
use crate::adapters::config_path::ResolvedConfigPath;
use crate::adapters::control_http::{ControlHttpError, ControlHttpServer};
use crate::adapters::runtime_token::{RuntimeToken, RuntimeTokenError, resolve_token_path};
use crate::application::{
    RegistryBuildError, ServiceRegistry, ServiceSnapshot, StartOutcome, StopOutcome,
    SupervisorControl,
};
use crate::domain::{Actor, Reason};
use crate::platform::windows::{
    ConsoleShutdown, ConsoleShutdownError, WindowsPortInspector, WindowsProcessSpawner,
};

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INTERACTIVE_HELP: &str = "Commands:\n\
  status\n\
  start <service> [reason]\n\
  stop <service> [reason]\n\
  restart <service> [reason]\n\
  help\n\
  quit";

type WindowsRegistry = ServiceRegistry<WindowsProcessSpawner, WindowsPortInspector>;

/// Runs the user-visible foreground supervisor until quit, EOF, or Ctrl+C.
///
/// # Errors
///
/// Returns a startup, configuration, console-handler, input, or cleanup error.
pub fn run(resolved_config: &ResolvedConfigPath) -> Result<(), ForegroundError> {
    let config_path = resolved_config.path();
    let source = fs::read_to_string(config_path).map_err(|source| ForegroundError::ReadConfig {
        path: config_path.to_owned(),
        source,
    })?;
    let config = SupervisorConfig::parse_json(&source).map_err(ForegroundError::Config)?;
    config.validate().map_err(ForegroundError::Config)?;
    let fingerprint = config.fingerprint().map_err(ForegroundError::Config)?;
    let token_path = resolve_token_path(config_path, &config.control.token_file);
    let token = RuntimeToken::load_or_create(
        &token_path,
        crate::platform::windows::generate_control_token,
    )
    .map_err(ForegroundError::RuntimeToken)?;
    let registry = Arc::new(
        WindowsRegistry::new(
            config.service_registrations(),
            WindowsProcessSpawner,
            WindowsPortInspector,
        )
        .map_err(ForegroundError::RegistryBuild)?,
    );
    let shutdown = ConsoleShutdown::install().map_err(ForegroundError::Console)?;
    let control: Arc<dyn SupervisorControl> = registry.clone();
    let mut control_server =
        ControlHttpServer::start(&config.control.host, config.control.port, token, control)
            .map_err(ForegroundError::ControlApi)?;

    println!("AkuSupervisor {}", crate::VERSION);
    println!("Configuration: {}", config_path.display());
    println!("Configuration source: {}", resolved_config.source());
    println!("Fingerprint: {fingerprint}");
    println!("Control API: http://{}", control_server.address());
    println!("Control token: {}", token_path.display());
    println!("Mode: visible interactive supervisor (Phase 3 local-control checkpoint)");
    print_status(&registry);
    println!("{INTERACTIVE_HELP}");

    let (input_sender, input_receiver) = mpsc::channel();
    thread::spawn(move || read_input(&input_sender));

    loop {
        if shutdown.is_requested() {
            println!("\nConsole shutdown requested.");
            break;
        }
        match input_receiver.recv_timeout(INPUT_POLL_INTERVAL) {
            Ok(InputEvent::Line(line)) => {
                if handle_line(&registry, &line) {
                    break;
                }
            }
            Ok(InputEvent::End) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(InputEvent::Error(error)) => return Err(ForegroundError::Input(error)),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    let server_result = control_server.shutdown();
    let cleanup_result = cleanup(&registry);
    if let Err(error) = server_result {
        cleanup_result?;
        return Err(ForegroundError::ControlApi(error));
    }
    cleanup_result
}

fn read_input(sender: &mpsc::Sender<InputEvent>) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(line) => {
                if sender.send(InputEvent::Line(line)).is_err() {
                    return;
                }
            }
            Err(error) => {
                sender.send(InputEvent::Error(error)).ok();
                return;
            }
        }
    }
    sender.send(InputEvent::End).ok();
}

fn handle_line(registry: &WindowsRegistry, line: &str) -> bool {
    match parse_interactive(line) {
        Ok(InteractiveCommand::Status) => print_status(registry),
        Ok(InteractiveCommand::Start { service_id, reason }) => {
            match registry.start(&service_id, Actor::UserCli, reason) {
                Ok(StartOutcome::Started) => println!("started {service_id}"),
                Ok(StartOutcome::AlreadyRunning) => println!("already running: {service_id}"),
                Err(error) => eprintln!("start failed for {service_id}: {error}"),
            }
        }
        Ok(InteractiveCommand::Stop { service_id, reason }) => {
            match registry.stop(&service_id, Actor::UserCli, reason) {
                Ok(StopOutcome::Stopped) => println!("stopped {service_id}"),
                Ok(StopOutcome::AlreadyStopped) => println!("already stopped: {service_id}"),
                Err(error) => eprintln!("stop failed for {service_id}: {error}"),
            }
        }
        Ok(InteractiveCommand::Restart { service_id, reason }) => {
            match registry.restart(&service_id, Actor::UserCli, reason) {
                Ok(outcome) => println!("restart completed for {service_id}: {outcome:?}"),
                Err(error) => eprintln!("restart failed for {service_id}: {error}"),
            }
        }
        Ok(InteractiveCommand::Help) => println!("{INTERACTIVE_HELP}"),
        Ok(InteractiveCommand::Quit) => return true,
        Err(error) => eprintln!("error: {error}"),
    }
    false
}

fn print_status(registry: &WindowsRegistry) {
    match registry.snapshots() {
        Ok(snapshots) => {
            println!();
            println!("SERVICE              STATE       ROOT PID   OWNED PIDS       HOLD");
            for snapshot in snapshots {
                print_snapshot(&snapshot);
            }
            println!();
        }
        Err(error) => eprintln!("status failed: {error}"),
    }
}

fn print_snapshot(snapshot: &ServiceSnapshot) {
    let root_pid = snapshot
        .root_pid
        .map_or_else(|| "-".to_owned(), |pid| pid.to_string());
    let owned_pids = if snapshot.owned_pids.is_empty() {
        "-".to_owned()
    } else {
        snapshot
            .owned_pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "{:<20} {:<11} {:<10} {:<16} {:?}",
        snapshot.id,
        format!("{:?}", snapshot.lifecycle).to_lowercase(),
        root_pid,
        owned_pids,
        snapshot.operator_hold
    );
    if let Some(last_action) = &snapshot.last_action {
        println!(
            "  last action: {:?} by {:?} - {}",
            last_action.action,
            last_action.actor,
            last_action.reason.as_str()
        );
    }
}

fn cleanup(registry: &WindowsRegistry) -> Result<(), ForegroundError> {
    let mut failures = Vec::new();
    for service_id in registry.service_ids() {
        let reason = Reason::new("visible supervisor shutdown").expect("static reason is valid");
        if let Err(error) = registry.stop(&service_id, Actor::UserCli, reason) {
            failures.push(format!("{service_id}: {error}"));
        }
    }
    if failures.is_empty() {
        println!("Owned service cleanup complete.");
        Ok(())
    } else {
        Err(ForegroundError::Cleanup(failures))
    }
}

fn parse_interactive(line: &str) -> Result<InteractiveCommand, String> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    match fields.as_slice() {
        [] | ["status"] => Ok(InteractiveCommand::Status),
        ["help"] => Ok(InteractiveCommand::Help),
        ["quit" | "exit"] => Ok(InteractiveCommand::Quit),
        [
            action @ ("start" | "stop" | "restart"),
            service_id,
            reason @ ..,
        ] => {
            let reason = if reason.is_empty() {
                format!("interactive CLI {action} request")
            } else {
                reason.join(" ")
            };
            let reason = Reason::new(reason).map_err(|error| error.to_string())?;
            Ok(match *action {
                "start" => InteractiveCommand::Start {
                    service_id: (*service_id).to_owned(),
                    reason,
                },
                "stop" => InteractiveCommand::Stop {
                    service_id: (*service_id).to_owned(),
                    reason,
                },
                "restart" => InteractiveCommand::Restart {
                    service_id: (*service_id).to_owned(),
                    reason,
                },
                _ => unreachable!("matched lifecycle action"),
            })
        }
        ["start" | "stop" | "restart"] => Err("service ID is required".to_owned()),
        _ => Err("unknown command; type help".to_owned()),
    }
}

#[derive(Debug)]
enum InteractiveCommand {
    Status,
    Start { service_id: String, reason: Reason },
    Stop { service_id: String, reason: Reason },
    Restart { service_id: String, reason: Reason },
    Help,
    Quit,
}

#[derive(Debug)]
enum InputEvent {
    Line(String),
    End,
    Error(io::Error),
}

/// Foreground supervisor startup or cleanup failure.
#[derive(Debug)]
pub enum ForegroundError {
    ReadConfig { path: PathBuf, source: io::Error },
    Config(ConfigError),
    RegistryBuild(RegistryBuildError),
    RuntimeToken(RuntimeTokenError),
    ControlApi(ControlHttpError),
    Console(ConsoleShutdownError),
    Input(io::Error),
    Cleanup(Vec<String>),
}

impl fmt::Display for ForegroundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadConfig { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Config(error) => error.fmt(formatter),
            Self::RegistryBuild(error) => error.fmt(formatter),
            Self::RuntimeToken(error) => error.fmt(formatter),
            Self::ControlApi(error) => error.fmt(formatter),
            Self::Console(error) => error.fmt(formatter),
            Self::Input(error) => write!(formatter, "interactive input failed: {error}"),
            Self::Cleanup(failures) => {
                write!(formatter, "service cleanup failed: {}", failures.join("; "))
            }
        }
    }
}

impl std::error::Error for ForegroundError {}

#[cfg(test)]
mod tests {
    use super::{InteractiveCommand, parse_interactive};

    #[test]
    fn interactive_commands_keep_service_and_reason_bounded() {
        let command = parse_interactive("restart api source changed")
            .expect("valid interactive restart command");
        assert!(matches!(
            command,
            InteractiveCommand::Restart { service_id, reason }
                if service_id == "api" && reason.as_str() == "source changed"
        ));
        assert!(parse_interactive("restart").is_err());
        assert!(parse_interactive("shell arbitrary command").is_err());
    }
}
