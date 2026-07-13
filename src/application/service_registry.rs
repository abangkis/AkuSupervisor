use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::domain::{
    Actor, AuthorizationError, ControlPolicy, LifecycleAction, LifecycleState, OperatorHold,
    Reason, TransitionError,
};

use super::{
    LaunchSpec, ManagedProcessTree, PortInspector, PortOccupant, ProcessTreeSpawner,
    RestartOutcome, ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome,
};

/// Platform-neutral definition of one validated, registered service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRegistration {
    id: String,
    label: String,
    launch: LaunchSpec,
    ports: Vec<u16>,
    shutdown_grace: Duration,
}

impl ServiceRegistration {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        launch: LaunchSpec,
        ports: Vec<u16>,
        shutdown_grace: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            launch,
            ports,
            shutdown_grace,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// Most recent accepted lifecycle request for one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastAction {
    pub action: LifecycleAction,
    pub actor: Actor,
    pub reason: Reason,
}

/// Read-only service state returned to every control adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub id: String,
    pub label: String,
    pub lifecycle: LifecycleState,
    pub root_pid: Option<u32>,
    pub owned_pids: Vec<u32>,
    pub operator_hold: OperatorHold,
    pub last_action: Option<LastAction>,
}

/// Shared lifecycle registry used by CLI, HTTP, and future adapters.
#[derive(Debug)]
pub struct ServiceRegistry<Spawner, Inspector>
where
    Spawner: ProcessTreeSpawner,
    Inspector: PortInspector,
{
    services: BTreeMap<String, ServiceEntry<Spawner::Process>>,
    spawner: Spawner,
    port_inspector: Inspector,
}

impl<Spawner, Inspector> ServiceRegistry<Spawner, Inspector>
where
    Spawner: ProcessTreeSpawner,
    Inspector: PortInspector,
{
    /// Builds a registry while rejecting duplicate service IDs.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryBuildError::DuplicateServiceId`] for duplicate IDs.
    pub fn new(
        registrations: impl IntoIterator<Item = ServiceRegistration>,
        spawner: Spawner,
        port_inspector: Inspector,
    ) -> Result<Self, RegistryBuildError> {
        let mut services = BTreeMap::new();
        for registration in registrations {
            match services.entry(registration.id.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(ServiceEntry::new(registration));
                }
                Entry::Occupied(entry) => {
                    return Err(RegistryBuildError::DuplicateServiceId(entry.key().clone()));
                }
            }
        }
        Ok(Self {
            services,
            spawner,
            port_inspector,
        })
    }

    #[must_use]
    pub fn service_ids(&self) -> Vec<String> {
        self.services.keys().cloned().collect()
    }

    /// Starts one registered service after authority and port checks.
    ///
    /// # Errors
    ///
    /// Returns a typed lookup, authority, platform, or runtime error.
    pub fn start(
        &self,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<StartOutcome, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let entry = self.entry(service_id)?;
        let _mutation = lock(&entry.mutation)?;
        let mut control = lock(&entry.control)?;
        control
            .policy
            .authorize(actor, LifecycleAction::Start)
            .map_err(RegistryError::Unauthorized)?;

        if has_process(&entry.runtime)? {
            return Ok(StartOutcome::AlreadyRunning);
        }
        control
            .policy
            .apply_user_action(actor, LifecycleAction::Start);
        control.last_action = Some(LastAction {
            action: LifecycleAction::Start,
            actor,
            reason,
        });

        entry
            .runtime
            .start_with(|| {
                self.ensure_ports_available(&entry.registration.ports)?;
                self.spawner
                    .spawn(&entry.registration.launch)
                    .map_err(BackendOperationError::Process)
            })
            .map_err(RegistryError::Runtime)
    }

    /// Stops one registered service using only its retained owner.
    ///
    /// # Errors
    ///
    /// Returns a typed lookup, authority, platform, or runtime error.
    pub fn stop(
        &self,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<StopOutcome, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let entry = self.entry(service_id)?;
        let _mutation = lock(&entry.mutation)?;
        let mut control = lock(&entry.control)?;
        control
            .policy
            .authorize(actor, LifecycleAction::Stop)
            .map_err(RegistryError::Unauthorized)?;
        control
            .policy
            .apply_user_action(actor, LifecycleAction::Stop);
        control.last_action = Some(LastAction {
            action: LifecycleAction::Stop,
            actor,
            reason,
        });
        let grace = entry.registration.shutdown_grace;

        entry
            .runtime
            .stop_with(|process| {
                process
                    .stop(grace)
                    .map(|_| ())
                    .map_err(BackendOperationError::Process)
            })
            .map_err(RegistryError::Runtime)
    }

    /// Replaces one owned tree atomically, or starts it if currently stopped.
    ///
    /// # Errors
    ///
    /// Returns a typed lookup, authority, platform, or runtime error.
    pub fn restart(
        &self,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<RestartOutcome, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let entry = self.entry(service_id)?;
        let _mutation = lock(&entry.mutation)?;
        let mut control = lock(&entry.control)?;
        control
            .policy
            .authorize(actor, LifecycleAction::Restart)
            .map_err(RegistryError::Unauthorized)?;
        control
            .policy
            .apply_user_action(actor, LifecycleAction::Restart);
        control.last_action = Some(LastAction {
            action: LifecycleAction::Restart,
            actor,
            reason,
        });
        let grace = entry.registration.shutdown_grace;

        entry
            .runtime
            .restart_with(
                |process| {
                    process
                        .stop(grace)
                        .map(|_| ())
                        .map_err(BackendOperationError::Process)
                },
                || {
                    self.ensure_ports_available(&entry.registration.ports)?;
                    self.spawner
                        .spawn(&entry.registration.launch)
                        .map_err(BackendOperationError::Process)
                },
            )
            .map_err(RegistryError::Runtime)
    }

    /// Returns a consistent snapshot of all registered services.
    ///
    /// # Errors
    ///
    /// Returns a platform observation error or a poisoned-lock error.
    pub fn snapshots(
        &self,
    ) -> Result<Vec<ServiceSnapshot>, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        self.services.values().map(Self::snapshot).collect()
    }

    fn snapshot(
        entry: &ServiceEntry<Spawner::Process>,
    ) -> Result<ServiceSnapshot, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let _mutation = lock(&entry.mutation)?;
        let control = lock(&entry.control)?;
        let (lifecycle, root_pid, owned_pids) = entry
            .runtime
            .inspect_with(|lifecycle, process| match process {
                Some(process) => Ok((lifecycle, Some(process.root_pid()), process.owned_pids()?)),
                None => Ok((lifecycle, None, Vec::new())),
            })
            .map_err(map_observation_error)?;

        Ok(ServiceSnapshot {
            id: entry.registration.id.clone(),
            label: entry.registration.label.clone(),
            lifecycle,
            root_pid,
            owned_pids,
            operator_hold: control.policy.operator_hold(),
            last_action: control.last_action.clone(),
        })
    }

    #[allow(clippy::type_complexity)]
    fn entry(
        &self,
        service_id: &str,
    ) -> Result<
        &ServiceEntry<Spawner::Process>,
        RegistryError<ProcessError<Spawner>, Inspector::Error>,
    > {
        self.services
            .get(service_id)
            .ok_or_else(|| RegistryError::ServiceNotFound(service_id.to_owned()))
    }

    fn ensure_ports_available(
        &self,
        ports: &[u16],
    ) -> Result<(), BackendOperationError<ProcessError<Spawner>, Inspector::Error>> {
        for port in ports {
            let diagnostic = self
                .port_inspector
                .inspect_tcp_port(*port)
                .map_err(|source| BackendOperationError::PortInspection {
                    port: *port,
                    source,
                })?;
            if !diagnostic.is_available() {
                return Err(BackendOperationError::PortConflict {
                    port: *port,
                    occupants: diagnostic.occupants().to_vec(),
                });
            }
        }
        Ok(())
    }
}

type ProcessError<Spawner> =
    <<Spawner as ProcessTreeSpawner>::Process as ManagedProcessTree>::Error;

#[derive(Debug)]
struct ServiceEntry<Process> {
    registration: ServiceRegistration,
    runtime: ServiceRuntime<Process>,
    mutation: Mutex<()>,
    control: Mutex<ControlState>,
}

impl<Process> ServiceEntry<Process> {
    fn new(registration: ServiceRegistration) -> Self {
        Self {
            registration,
            runtime: ServiceRuntime::new(),
            mutation: Mutex::new(()),
            control: Mutex::new(ControlState {
                policy: ControlPolicy::default(),
                last_action: None,
            }),
        }
    }
}

#[derive(Debug)]
struct ControlState {
    policy: ControlPolicy,
    last_action: Option<LastAction>,
}

fn lock<Value, ProcessFailure, PortFailure>(
    mutex: &Mutex<Value>,
) -> Result<MutexGuard<'_, Value>, RegistryError<ProcessFailure, PortFailure>> {
    mutex.lock().map_err(|_| RegistryError::LockPoisoned)
}

fn has_process<Process, ProcessFailure, PortFailure>(
    runtime: &ServiceRuntime<Process>,
) -> Result<bool, RegistryError<ProcessFailure, PortFailure>> {
    runtime.has_process().map_err(|error| match error {
        ServiceRuntimeError::Poisoned => RegistryError::LockPoisoned,
        ServiceRuntimeError::Transition(error) => RegistryError::Transition(error),
        ServiceRuntimeError::Start(())
        | ServiceRuntimeError::Stop(())
        | ServiceRuntimeError::Inspect(()) => RegistryError::InternalState,
    })
}

fn map_observation_error<ProcessFailure, PortFailure>(
    error: ServiceRuntimeError<ProcessFailure>,
) -> RegistryError<ProcessFailure, PortFailure> {
    match error {
        ServiceRuntimeError::Poisoned => RegistryError::LockPoisoned,
        ServiceRuntimeError::Transition(error) => RegistryError::Transition(error),
        ServiceRuntimeError::Inspect(error) => RegistryError::Observation(error),
        ServiceRuntimeError::Start(_) | ServiceRuntimeError::Stop(_) => {
            RegistryError::InternalState
        }
    }
}

/// Failure while constructing the registered-service map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryBuildError {
    DuplicateServiceId(String),
}

impl fmt::Display for RegistryBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateServiceId(service_id) => {
                write!(formatter, "duplicate registered service ID: {service_id}")
            }
        }
    }
}

impl std::error::Error for RegistryBuildError {}

/// Platform failure that occurs inside a serialized lifecycle callback.
#[derive(Debug)]
pub enum BackendOperationError<ProcessFailure, PortFailure> {
    Process(ProcessFailure),
    PortInspection {
        port: u16,
        source: PortFailure,
    },
    PortConflict {
        port: u16,
        occupants: Vec<PortOccupant>,
    },
}

impl<ProcessFailure: fmt::Display, PortFailure: fmt::Display> fmt::Display
    for BackendOperationError<ProcessFailure, PortFailure>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Process(error) => error.fmt(formatter),
            Self::PortInspection { port, source } => {
                write!(formatter, "failed to inspect TCP port {port}: {source}")
            }
            Self::PortConflict { port, occupants } => {
                write!(formatter, "TCP port {port} is occupied by {occupants:?}")
            }
        }
    }
}

impl<ProcessFailure, PortFailure> std::error::Error
    for BackendOperationError<ProcessFailure, PortFailure>
where
    ProcessFailure: std::error::Error + 'static,
    PortFailure: std::error::Error + 'static,
{
}

/// Shared lifecycle operation failure returned to control adapters.
#[derive(Debug)]
pub enum RegistryError<ProcessFailure, PortFailure> {
    ServiceNotFound(String),
    Unauthorized(AuthorizationError),
    LockPoisoned,
    Transition(TransitionError),
    InternalState,
    Observation(ProcessFailure),
    Runtime(ServiceRuntimeError<BackendOperationError<ProcessFailure, PortFailure>>),
}

impl<ProcessFailure: fmt::Display, PortFailure: fmt::Display> fmt::Display
    for RegistryError<ProcessFailure, PortFailure>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceNotFound(service_id) => {
                write!(formatter, "unknown service: {service_id}")
            }
            Self::Unauthorized(error) => error.fmt(formatter),
            Self::LockPoisoned => formatter.write_str("service registry lock is poisoned"),
            Self::Transition(error) => error.fmt(formatter),
            Self::InternalState => formatter.write_str("service registry invariant was violated"),
            Self::Observation(error) => write!(formatter, "process observation failed: {error}"),
            Self::Runtime(error) => error.fmt(formatter),
        }
    }
}

impl<ProcessFailure, PortFailure> std::error::Error for RegistryError<ProcessFailure, PortFailure>
where
    ProcessFailure: std::error::Error + 'static,
    PortFailure: std::error::Error + 'static,
{
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::process::ExitStatus;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use crate::application::{
        BackendOperationError, LaunchSpec, ManagedProcessTree, NetworkFamily, PortDiagnostic,
        PortInspector, PortOccupant, ProcessTreeSpawner, ServiceRegistration, ServiceRuntimeError,
        TreeStopReport,
    };
    use crate::domain::{Actor, AuthorizationError, Reason};

    use super::{RegistryError, ServiceRegistry};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake platform error")
        }
    }

    impl std::error::Error for FakeError {}

    #[derive(Debug)]
    struct FakeProcess {
        pid: u32,
    }

    impl ManagedProcessTree for FakeProcess {
        type Error = FakeError;

        fn root_pid(&self) -> u32 {
            self.pid
        }

        fn owned_pids(&self) -> Result<Vec<u32>, Self::Error> {
            Ok(vec![self.pid])
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, Self::Error> {
            Ok(None)
        }

        fn stop(&mut self, _grace: Duration) -> Result<TreeStopReport, Self::Error> {
            Ok(TreeStopReport {
                owned_pids_before: vec![self.pid],
                owned_pids_after: Vec::new(),
                graceful_signal_sent: true,
                graceful_signal_error: None,
                forced: false,
            })
        }
    }

    #[derive(Debug, Clone)]
    struct FakeSpawner {
        spawn_count: Arc<AtomicU32>,
    }

    impl ProcessTreeSpawner for FakeSpawner {
        type Process = FakeProcess;

        fn spawn(&self, _launch: &LaunchSpec) -> Result<Self::Process, FakeError> {
            let pid = self.spawn_count.fetch_add(1, Ordering::SeqCst) + 1_000;
            Ok(FakeProcess { pid })
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct FakePortInspector {
        occupied: bool,
    }

    impl PortInspector for FakePortInspector {
        type Error = FakeError;

        fn inspect_tcp_port(&self, port: u16) -> Result<PortDiagnostic, Self::Error> {
            let occupants = if self.occupied {
                vec![PortOccupant::new(77, NetworkFamily::V4)]
            } else {
                Vec::new()
            };
            Ok(PortDiagnostic::new(port, occupants))
        }
    }

    fn registration(ports: Vec<u16>) -> ServiceRegistration {
        ServiceRegistration::new(
            "fixture",
            "Fixture",
            LaunchSpec::new(
                "fixture",
                std::iter::empty::<&str>(),
                ".",
                std::iter::empty::<(&str, &str)>(),
            ),
            ports,
            Duration::from_millis(10),
        )
    }

    fn reason(value: &str) -> Reason {
        Reason::new(value).expect("valid test reason")
    }

    #[test]
    fn user_stop_in_registry_blocks_later_agent_start() {
        let registry = ServiceRegistry::new(
            [registration(Vec::new())],
            FakeSpawner {
                spawn_count: Arc::new(AtomicU32::new(0)),
            },
            FakePortInspector { occupied: false },
        )
        .expect("valid registry");

        registry
            .start("fixture", Actor::UserCli, reason("user start"))
            .expect("user starts fixture");
        registry
            .stop("fixture", Actor::UserCli, reason("user stop"))
            .expect("user stops fixture");
        let blocked = registry.start("fixture", Actor::Agent, reason("agent retry"));

        assert!(matches!(
            blocked,
            Err(RegistryError::Unauthorized(
                AuthorizationError::OperatorHoldStopped
            ))
        ));
    }

    #[test]
    fn port_conflict_never_invokes_process_spawner() {
        let spawn_count = Arc::new(AtomicU32::new(0));
        let registry = ServiceRegistry::new(
            [registration(vec![49_100])],
            FakeSpawner {
                spawn_count: Arc::clone(&spawn_count),
            },
            FakePortInspector { occupied: true },
        )
        .expect("valid registry");

        let result = registry.start("fixture", Actor::UserCli, reason("user start"));

        assert!(matches!(
            result,
            Err(RegistryError::Runtime(ServiceRuntimeError::Start(
                BackendOperationError::PortConflict { port: 49_100, .. }
            )))
        ));
        assert_eq!(spawn_count.load(Ordering::SeqCst), 0);
    }
}
