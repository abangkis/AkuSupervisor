use std::fmt;
use std::fs;
use std::io::{self, BufRead};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::adapters::aku_bridge_reload::AkuBridgeReloadClient;
use crate::adapters::config::{ConfigError, SupervisorConfig};
use crate::adapters::config_path::ResolvedConfigPath;
use crate::adapters::control_http::{ControlHttpError, ControlHttpServer};
use crate::adapters::development_shutdown::{DevelopmentShutdown, DevelopmentShutdownError};
use crate::adapters::http_health::LoopbackTransportHealthProbe;
use crate::adapters::journal::{AuditedControl, FileJournal, FileJournalError};
use crate::adapters::registration_events::RegistrationAuditTail;
use crate::adapters::runtime_token::{RuntimeToken, RuntimeTokenError, resolve_token_path};
use crate::adapters::service_logs::ServiceLogStore;
use crate::application::{
    ControlAction, ControlMutationOutcome, CooperativeActionControl, CooperativeActionError,
    RegistryBuildError, RegistryReconciliationStatus, ServiceRegistry, ServiceSnapshot,
    SupervisorControl,
};
use crate::domain::{Actor, Reason};
use crate::platform::windows::{
    ConsoleShutdown, ConsoleShutdownError, WindowsPortInspector, WindowsProcessSpawner,
};

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const CONFIG_RECONCILE_INTERVAL: Duration = Duration::from_millis(250);
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
#[allow(clippy::too_many_lines)]
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
    crate::platform::windows::harden_runtime_token_permissions(&token_path)
        .map_err(ForegroundError::TokenPermissions)?;
    let runtime_directory = token_path
        .parent()
        .ok_or_else(|| ForegroundError::RuntimeLayout(token_path.clone()))?;
    let journal_path = runtime_directory.join("supervisor.jsonl");
    let runtime_services_directory = runtime_directory.join("services");
    let journal = Arc::new(
        FileJournal::open(
            &journal_path,
            [token.expose_for_authorization_header().to_owned()],
        )
        .map_err(ForegroundError::Journal)?,
    );
    let registry = Arc::new(
        WindowsRegistry::new(
            config.service_registrations_with_logs(&runtime_services_directory),
            WindowsProcessSpawner,
            WindowsPortInspector,
            Arc::new(LoopbackTransportHealthProbe),
        )
        .map_err(ForegroundError::RegistryBuild)?,
    );
    let shutdown = ConsoleShutdown::install().map_err(ForegroundError::Console)?;
    let development_shutdown =
        DevelopmentShutdown::from_environment().map_err(ForegroundError::DevelopmentShutdown)?;
    let registry_control: Arc<dyn SupervisorControl> = registry.clone();
    let audited = Arc::new(
        AuditedControl::new(registry_control, Arc::clone(&journal), fingerprint.clone())
            .with_console_events(config.observability.console_events),
    );
    let control: Arc<dyn SupervisorControl> = audited.clone();
    let logs = Arc::new(ServiceLogStore::new(
        &runtime_services_directory,
        config.services.keys().cloned(),
    ));
    let (cooperative, cooperative_audit_path) =
        build_cooperative_control(&config, runtime_directory, &fingerprint)?;
    let reconciliation = Arc::new(RegistryReconciliationStatus::new(fingerprint.clone()));
    let mut control_server = ControlHttpServer::start(
        &config.control.host,
        config.control.port,
        token,
        config.control.mcp.clone(),
        Arc::clone(&control),
        cooperative,
        journal,
        Arc::clone(&logs),
        Arc::clone(&reconciliation),
    )
    .map_err(ForegroundError::ControlApi)?;
    let (mut config_monitor, mut registration_monitor, mut service_monitor) =
        start_runtime_monitors(
            &registry,
            audited,
            logs,
            config_path,
            &config,
            &fingerprint,
            &runtime_services_directory,
            reconciliation,
        );

    print_startup(
        resolved_config,
        &config,
        &fingerprint,
        control_server.address(),
        &token_path,
        &journal_path,
        &cooperative_audit_path,
        &runtime_services_directory,
        development_shutdown.request_path(),
    );
    print_status(control.as_ref());
    if development_shutdown.request_path().is_some() {
        println!(
            "Development watcher owns this process; use the control CLI from another terminal."
        );
    } else {
        println!("{INTERACTIVE_HELP}");
    }

    let foreground_result = wait_for_shutdown(control.as_ref(), &shutdown, &development_shutdown);
    let shutdown_cause = foreground_result.as_ref().map_or_else(
        |_| ShutdownCause::recovery("foreground shutdown after control-loop failure"),
        Clone::clone,
    );

    registration_monitor.shutdown();
    config_monitor.shutdown();
    service_monitor.shutdown();
    let server_result = control_server.shutdown();
    let cleanup_result = cleanup(control.as_ref(), &shutdown_cause);
    if let Err(error) = server_result {
        cleanup_result?;
        return Err(ForegroundError::ControlApi(error));
    }
    cleanup_result?;
    foreground_result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn start_runtime_monitors(
    registry: &Arc<WindowsRegistry>,
    audited: Arc<AuditedControl>,
    logs: Arc<ServiceLogStore>,
    config_path: &std::path::Path,
    config: &SupervisorConfig,
    fingerprint: &str,
    runtime_services_directory: &std::path::Path,
    reconciliation: Arc<RegistryReconciliationStatus>,
) -> (ConfigMonitor, RegistrationEventMonitor, ServiceMonitor) {
    let config_monitor = ConfigMonitor::start(
        Arc::clone(registry),
        Arc::clone(&audited),
        logs,
        config_path.to_owned(),
        config.clone(),
        fingerprint.to_owned(),
        runtime_services_directory.to_owned(),
        reconciliation,
    );
    let registration_audit_path = runtime_services_directory
        .parent()
        .expect("runtime services directory has a parent")
        .join("registration/audit.jsonl");
    let registration_monitor = RegistrationEventMonitor::start(registration_audit_path);
    let service_monitor = ServiceMonitor::start(Arc::clone(registry), audited);
    (config_monitor, registration_monitor, service_monitor)
}

#[derive(Debug)]
struct RegistrationEventMonitor {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RegistrationEventMonitor {
    fn start(audit_path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut tail = RegistrationAuditTail::follow(audit_path);
            let mut last_error = None;
            while !worker_stop.load(Ordering::Acquire) {
                match tail.poll() {
                    Ok(events) => {
                        last_error = None;
                        for event in events {
                            println!("{}", event.console_line());
                        }
                    }
                    Err(error) => {
                        let current = error.to_string();
                        if last_error.as_ref() != Some(&current) {
                            eprintln!(
                                "[registration] Audit visibility failed for {}: {current}",
                                tail.path().display()
                            );
                        }
                        last_error = Some(current);
                    }
                }
                for _ in 0..5 {
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(CONFIG_RECONCILE_INTERVAL / 5);
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().ok();
        }
    }
}

impl Drop for RegistrationEventMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct ConfigMonitor {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ConfigMonitor {
    #[allow(clippy::too_many_arguments)]
    fn start(
        registry: Arc<WindowsRegistry>,
        audited: Arc<AuditedControl>,
        logs: Arc<ServiceLogStore>,
        config_path: PathBuf,
        mut active_config: SupervisorConfig,
        mut active_fingerprint: String,
        runtime_services_directory: PathBuf,
        reconciliation: Arc<RegistryReconciliationStatus>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut last_error = None;
            while !worker_stop.load(Ordering::Acquire) {
                let result = reconcile_configuration(
                    &registry,
                    &audited,
                    &logs,
                    &config_path,
                    &active_config,
                    &active_fingerprint,
                    &runtime_services_directory,
                    &reconciliation,
                );
                match result {
                    Ok(Some((config, fingerprint, outcome))) => {
                        last_error = None;
                        active_config = config;
                        active_fingerprint = fingerprint;
                        reconciliation.applied(active_fingerprint.clone(), &outcome);
                        println!(
                            "[{}] [registry] Applied revision {} without Supervisor handoff: added={}, updated={}, removed={}; unrelated services preserved.",
                            console_timestamp_now(),
                            active_fingerprint,
                            display_service_ids(&outcome.added),
                            display_service_ids(&outcome.updated),
                            display_service_ids(&outcome.removed),
                        );
                    }
                    Ok(None) => {
                        last_error = None;
                    }
                    Err(error) => {
                        if last_error.as_ref() != Some(&error) {
                            let state = match reconciliation.snapshot().state {
                                crate::application::RegistryReconciliationState::Deferred => {
                                    "deferred"
                                }
                                crate::application::RegistryReconciliationState::Rejected => {
                                    "rejected"
                                }
                                crate::application::RegistryReconciliationState::Pending => {
                                    "pending"
                                }
                                crate::application::RegistryReconciliationState::Current => {
                                    "current"
                                }
                            };
                            eprintln!(
                                "[{}] [registry] Configuration reconciliation {state}: {error}",
                                console_timestamp_now()
                            );
                        }
                        last_error = Some(error);
                    }
                }
                for _ in 0..5 {
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(CONFIG_RECONCILE_INTERVAL / 5);
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().ok();
        }
    }
}

impl Drop for ConfigMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_configuration(
    registry: &WindowsRegistry,
    audited: &AuditedControl,
    logs: &ServiceLogStore,
    config_path: &std::path::Path,
    active_config: &SupervisorConfig,
    active_fingerprint: &str,
    runtime_services_directory: &std::path::Path,
    reconciliation: &RegistryReconciliationStatus,
) -> Result<
    Option<(
        SupervisorConfig,
        String,
        crate::application::RegistryReconcileOutcome,
    )>,
    String,
> {
    let source = fs::read_to_string(config_path).map_err(|error| {
        let message = format!("cannot read {}: {error}", config_path.display());
        reconciliation.rejected(None, &message);
        message
    })?;
    let config = SupervisorConfig::parse_json(&source).map_err(|error| {
        let message = error.to_string();
        reconciliation.rejected(None, &message);
        message
    })?;
    config.validate().map_err(|error| {
        let message = error.to_string();
        reconciliation.rejected(None, &message);
        message
    })?;
    let fingerprint = config.fingerprint().map_err(|error| {
        let message = error.to_string();
        reconciliation.rejected(None, &message);
        message
    })?;
    if fingerprint == active_fingerprint {
        if reconciliation.snapshot().state
            != crate::application::RegistryReconciliationState::Current
        {
            reconciliation.applied(
                fingerprint,
                &crate::application::RegistryReconcileOutcome {
                    added: Vec::new(),
                    updated: Vec::new(),
                    removed: Vec::new(),
                },
            );
        }
        return Ok(None);
    }
    reconciliation.pending(fingerprint.clone());
    if !same_foreground_contract(active_config, &config) {
        let message = "non-service configuration changed; control, observability, cooperative actions, and version require an explicit Supervisor restart".to_owned();
        reconciliation.rejected(Some(fingerprint), &message);
        return Err(message);
    }
    let outcome = registry
        .reconcile_registrations(config.service_registrations_with_logs(runtime_services_directory))
        .map_err(|error| {
            let message = error.to_string();
            reconciliation.deferred(Some(fingerprint.clone()), &message);
            message
        })?;
    logs.reconcile_service_ids(config.services.keys().cloned());
    audited.set_config_fingerprint(fingerprint.clone());
    Ok(Some((config, fingerprint, outcome)))
}

fn same_foreground_contract(left: &SupervisorConfig, right: &SupervisorConfig) -> bool {
    left.version == right.version
        && left.control == right.control
        && left.observability == right.observability
        && left.cooperative_actions == right.cooperative_actions
}

fn display_service_ids(service_ids: &[String]) -> String {
    if service_ids.is_empty() {
        "-".to_owned()
    } else {
        service_ids.join(",")
    }
}

fn console_timestamp_now() -> String {
    let milliseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    crate::adapters::journal::format_unix_milliseconds_utc(milliseconds)
        .unwrap_or_else(|| "timestamp-invalid".to_owned())
}

#[derive(Debug)]
struct ServiceMonitor {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ServiceMonitor {
    fn start(registry: Arc<WindowsRegistry>, audited: Arc<AuditedControl>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut last_error = None;
            while !worker_stop.load(Ordering::Acquire) {
                match registry.refresh_services() {
                    Ok(refreshes) => {
                        last_error = None;
                        for process_exit in refreshes
                            .into_iter()
                            .filter_map(|refresh| refresh.process_exit)
                        {
                            if let Err(error) = audited.record_process_exit(&process_exit) {
                                eprintln!(
                                    "process exit audit failed for {}: {error}; planned restart suppressed",
                                    process_exit.service_id
                                );
                                continue;
                            }
                            if process_exit.automatic_restart_planned
                                || process_exit.deferred_restart_planned
                            {
                                let exit = process_exit.exit_code.map_or_else(
                                    || "without a numeric exit code".to_owned(),
                                    |code| format!("with code {code}"),
                                );
                                let restart_kind = if process_exit.deferred_restart_planned {
                                    "deferred restart after forced termination completed"
                                } else {
                                    "automatic on-failure restart"
                                };
                                let reason = Reason::new(format!(
                                    "{restart_kind} after process tree exited {exit}"
                                ))
                                .expect("bounded automatic restart reason");
                                if let Err(error) = audited.mutate(
                                    ControlAction::Restart,
                                    &process_exit.service_id,
                                    Actor::Recovery,
                                    reason,
                                ) {
                                    eprintln!(
                                        "automatic restart failed for {}: {error}",
                                        process_exit.service_id
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => {
                        let current = error.to_string();
                        if last_error.as_ref() != Some(&current) {
                            eprintln!("health refresh failed: {current}");
                        }
                        last_error = Some(current);
                    }
                }
                for _ in 0..10 {
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(HEALTH_REFRESH_INTERVAL / 10);
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().ok();
        }
    }
}

impl Drop for ServiceMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wait_for_shutdown(
    control: &dyn SupervisorControl,
    shutdown: &ConsoleShutdown,
    development_shutdown: &DevelopmentShutdown,
) -> Result<ShutdownCause, ForegroundError> {
    let input_receiver = if development_shutdown.request_path().is_none() {
        let (input_sender, input_receiver) = mpsc::channel();
        thread::spawn(move || read_input(&input_sender));
        Some(input_receiver)
    } else {
        None
    };

    loop {
        if shutdown.is_requested() {
            println!("\nConsole shutdown requested.");
            return Ok(ShutdownCause::user(
                "user requested Ctrl+C foreground shutdown",
            ));
        }
        match development_shutdown.take_request() {
            Ok(Some(reason)) => {
                println!("\nDevelopment restart requested: {reason}");
                return Ok(shutdown_cause_for_development(&reason));
            }
            Ok(None) => {}
            Err(error) => return Err(ForegroundError::DevelopmentShutdown(error)),
        }
        if let Some(input_receiver) = &input_receiver {
            match input_receiver.recv_timeout(INPUT_POLL_INTERVAL) {
                Ok(InputEvent::Line(line)) => {
                    if handle_line(control, &line) {
                        return Ok(ShutdownCause::user("user requested interactive quit"));
                    }
                }
                Ok(InputEvent::End) | Err(RecvTimeoutError::Disconnected) => {
                    return Ok(ShutdownCause::user("foreground input closed"));
                }
                Ok(InputEvent::Error(error)) => return Err(ForegroundError::Input(error)),
                Err(RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::sleep(INPUT_POLL_INTERVAL);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_startup(
    resolved_config: &ResolvedConfigPath,
    config: &SupervisorConfig,
    fingerprint: &str,
    address: std::net::SocketAddr,
    token_path: &std::path::Path,
    journal_path: &std::path::Path,
    cooperative_audit_path: &std::path::Path,
    services_directory: &std::path::Path,
    development_shutdown_path: Option<&std::path::Path>,
) {
    println!("AkuSupervisor {}", crate::VERSION);
    println!("Configuration: {}", resolved_config.path().display());
    println!("Configuration source: {}", resolved_config.source());
    println!("Fingerprint: {fingerprint}");
    println!("Control API: http://{address}");
    if config.control.mcp.enabled {
        println!(
            "Read-only MCP: http://{address}{}",
            crate::adapters::mcp::MCP_ENDPOINT
        );
    }
    println!("Control token: {}", token_path.display());
    println!("Lifecycle journal: {}", journal_path.display());
    if let Some(runtime_directory) = journal_path.parent() {
        println!(
            "Registration audit: {}",
            runtime_directory.join("registration/audit.jsonl").display()
        );
    }
    println!(
        "Console lifecycle events: {}",
        config.observability.console_events.as_str()
    );
    if config.cooperative_actions.aku_bridge_reload.is_some() {
        println!(
            "Cooperative action audit: {}",
            cooperative_audit_path.display()
        );
    }
    println!("Service logs: {}", services_directory.display());
    if let Some(path) = development_shutdown_path {
        println!("Development watcher signal: {}", path.display());
    }
    println!("Mode: visible interactive supervisor (Phase 5 cooperative-action checkpoint)");
}

fn build_cooperative_control(
    config: &SupervisorConfig,
    runtime_directory: &std::path::Path,
    fingerprint: &str,
) -> Result<(Option<Arc<dyn CooperativeActionControl>>, PathBuf), ForegroundError> {
    let audit_path = runtime_directory.join("cooperative-actions.jsonl");
    let control = config
        .cooperative_actions
        .aku_bridge_reload
        .as_ref()
        .map(|reload| {
            AkuBridgeReloadClient::new(
                &reload.sidecar_origin,
                Duration::from_millis(reload.timeout_ms),
                Duration::from_millis(reload.poll_interval_ms),
                &audit_path,
                fingerprint.to_owned(),
            )
            .map(|client| Arc::new(client) as Arc<dyn CooperativeActionControl>)
        })
        .transpose()
        .map_err(ForegroundError::CooperativeAction)?;
    Ok((control, audit_path))
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

fn handle_line(control: &dyn SupervisorControl, line: &str) -> bool {
    match parse_interactive(line) {
        Ok(InteractiveCommand::Status) => print_status(control),
        Ok(InteractiveCommand::Start { service_id, reason }) => {
            match control.mutate(ControlAction::Start, &service_id, Actor::UserCli, reason) {
                Ok(result) => match result.outcome {
                    ControlMutationOutcome::Started => println!("started {service_id}"),
                    ControlMutationOutcome::AlreadyRunning => {
                        println!("already running: {service_id}");
                    }
                    ControlMutationOutcome::TerminationPending => println!(
                        "termination pending for {service_id}; ownership is retained until cleanup completes"
                    ),
                    outcome => println!("start completed for {service_id}: {outcome:?}"),
                },
                Err(error) => eprintln!("start failed for {service_id}: {error}"),
            }
        }
        Ok(InteractiveCommand::Stop { service_id, reason }) => {
            match control.mutate(ControlAction::Stop, &service_id, Actor::UserCli, reason) {
                Ok(result) => {
                    match result.outcome {
                        ControlMutationOutcome::Stopped => println!("stopped {service_id}"),
                        ControlMutationOutcome::AlreadyStopped => {
                            println!("already stopped: {service_id}");
                        }
                        ControlMutationOutcome::TerminationPending => println!(
                            "termination pending for {service_id}; ownership is retained until cleanup completes"
                        ),
                        outcome => println!("stop completed for {service_id}: {outcome:?}"),
                    }
                    print_shutdown_report(result.shutdown.as_ref());
                }
                Err(error) => eprintln!("stop failed for {service_id}: {error}"),
            }
        }
        Ok(InteractiveCommand::Restart { service_id, reason }) => {
            match control.mutate(ControlAction::Restart, &service_id, Actor::UserCli, reason) {
                Ok(result) => {
                    println!("restart completed for {service_id}: {:?}", result.outcome);
                    print_shutdown_report(result.shutdown.as_ref());
                }
                Err(error) => eprintln!("restart failed for {service_id}: {error}"),
            }
        }
        Ok(InteractiveCommand::Help) => println!("{INTERACTIVE_HELP}"),
        Ok(InteractiveCommand::Quit) => return true,
        Err(error) => eprintln!("error: {error}"),
    }
    false
}

fn print_shutdown_report(report: Option<&crate::application::TreeStopReport>) {
    if let Some(report) = report {
        println!(
            "shutdown: gracefulSignalSent={}, forced={}, ownedPidsAfter={:?}",
            report.graceful_signal_sent, report.forced, report.owned_pids_after
        );
        if let Some(error) = report.graceful_signal_error.as_deref() {
            println!("shutdown signal detail: {error}");
        }
    }
}

fn print_status(control: &dyn SupervisorControl) {
    match control.snapshots() {
        Ok(snapshots) => {
            println!();
            println!(
                "SERVICE              STATE                DESIRED     HEALTH      ROOT PID   OWNED PIDS       HOLD"
            );
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
        "{:<20} {:<20} {:<11} {:<11} {:<10} {:<16} {:?}",
        snapshot.id,
        format!("{:?}", snapshot.lifecycle).to_lowercase(),
        format!("{:?}", snapshot.desired_state).to_lowercase(),
        format!("{:?}", snapshot.health.status).to_lowercase(),
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
    if let Some(detail) = &snapshot.health.detail {
        println!(
            "  health: processReady={} transportReady={} - {detail}",
            snapshot.health.process_ready,
            snapshot
                .health
                .transport_ready
                .map_or_else(|| "n/a".to_owned(), |ready| ready.to_string())
        );
    }
    if snapshot.last_exit_at_unix_ms.is_some() || snapshot.restart_count > 0 {
        println!(
            "  supervision: startedAt={} lastExitAt={} lastExitCode={} restartCount={}",
            snapshot
                .started_at_unix_ms
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            snapshot
                .last_exit_at_unix_ms
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            snapshot
                .last_exit_code
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            snapshot.restart_count
        );
    }
}

fn cleanup(
    control: &dyn SupervisorControl,
    shutdown_cause: &ShutdownCause,
) -> Result<(), ForegroundError> {
    let mut failures = Vec::new();
    let service_ids = control
        .snapshots()
        .map_err(|error| ForegroundError::Cleanup(vec![error.to_string()]))?
        .into_iter()
        .map(|snapshot| snapshot.id)
        .collect::<Vec<_>>();
    for service_id in service_ids {
        if let Err(error) = control.mutate(
            ControlAction::Stop,
            &service_id,
            shutdown_cause.actor,
            shutdown_cause.reason.clone(),
        ) {
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

#[derive(Debug, Clone)]
struct ShutdownCause {
    actor: Actor,
    reason: Reason,
}

impl ShutdownCause {
    fn user(reason: &'static str) -> Self {
        Self {
            actor: Actor::UserCli,
            reason: Reason::new(reason).expect("static user shutdown reason is valid"),
        }
    }

    fn recovery(reason: &'static str) -> Self {
        Self {
            actor: Actor::Recovery,
            reason: Reason::new(reason).expect("static recovery shutdown reason is valid"),
        }
    }
}

fn shutdown_cause_for_development(reason: &str) -> ShutdownCause {
    if reason == "development watcher stopped by user" {
        return ShutdownCause::user("user stopped development watcher");
    }
    let reason =
        Reason::new(format!("development watcher handoff: {reason}")).unwrap_or_else(|_| {
            Reason::new("development watcher requested bounded handoff")
                .expect("static watcher handoff reason is valid")
        });
    ShutdownCause {
        actor: Actor::Recovery,
        reason,
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
    RuntimeLayout(PathBuf),
    Config(ConfigError),
    RegistryBuild(RegistryBuildError),
    RuntimeToken(RuntimeTokenError),
    TokenPermissions(crate::platform::windows::TokenPermissionError),
    Journal(FileJournalError),
    CooperativeAction(CooperativeActionError),
    ControlApi(ControlHttpError),
    Console(ConsoleShutdownError),
    DevelopmentShutdown(DevelopmentShutdownError),
    Input(io::Error),
    Cleanup(Vec<String>),
}

impl fmt::Display for ForegroundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadConfig { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::RuntimeLayout(path) => write!(
                formatter,
                "runtime token path has no parent directory: {}",
                path.display()
            ),
            Self::Config(error) => error.fmt(formatter),
            Self::RegistryBuild(error) => error.fmt(formatter),
            Self::RuntimeToken(error) => error.fmt(formatter),
            Self::TokenPermissions(error) => error.fmt(formatter),
            Self::Journal(error) => error.fmt(formatter),
            Self::CooperativeAction(error) => error.fmt(formatter),
            Self::ControlApi(error) => error.fmt(formatter),
            Self::Console(error) => error.fmt(formatter),
            Self::DevelopmentShutdown(error) => error.fmt(formatter),
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
    use super::{InteractiveCommand, parse_interactive, shutdown_cause_for_development};
    use crate::domain::Actor;

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

    #[test]
    fn development_handoff_and_user_stop_keep_distinct_audit_identity() {
        let handoff = shutdown_cause_for_development("successful build or configuration change");
        assert_eq!(handoff.actor, Actor::Recovery);
        assert_eq!(
            handoff.reason.as_str(),
            "development watcher handoff: successful build or configuration change",
        );

        let user_stop = shutdown_cause_for_development("development watcher stopped by user");
        assert_eq!(user_stop.actor, Actor::UserCli);
        assert_eq!(
            user_stop.reason.as_str(),
            "user stopped development watcher"
        );
    }
}
