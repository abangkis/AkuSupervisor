use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::domain::{
    Actor, AuthorizationError, ControlPolicy, DesiredState, LifecycleAction, LifecycleState,
    OperatorHold, Reason, TransitionError,
};

use super::{
    HealthCheckSpec, HealthProbe, HealthSnapshot, LaunchSpec, ManagedProcessTree, PortInspector,
    PortOccupant, ProcessTreeSpawner, RestartOutcome, ServiceRuntime, ServiceRuntimeError,
    StartOutcome, StopOutcome, StopProgress, TreeStopReport,
};

const DEFAULT_RESTART_STABILITY_WINDOW: Duration = Duration::from_mins(1);

/// Bounded automatic restart policy mapped from validated configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceRestartPolicy {
    Manual,
    OnFailure,
}

/// One terminal owned-tree observation emitted exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExitEvent {
    pub service_id: String,
    pub previous_state: LifecycleState,
    pub owned_pids_before: Vec<u32>,
    pub exit_code: Option<i32>,
    pub successful: bool,
    pub automatic_restart_planned: bool,
    pub deferred_restart_planned: bool,
}

/// Result of one periodic service reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRefresh {
    pub health: HealthSnapshot,
    pub process_exit: Option<ProcessExitEvent>,
}

/// Topology changes applied without replacing retained service entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryReconcileOutcome {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

impl RegistryReconcileOutcome {
    #[must_use]
    pub fn changed(&self) -> bool {
        !(self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty())
    }
}

/// Stop outcome plus owned-tree shutdown evidence when a process was present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStopResult {
    pub outcome: StopOutcome,
    pub shutdown: Option<TreeStopReport>,
}

/// Restart outcome plus shutdown evidence for the replaced process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRestartResult {
    pub outcome: RestartOutcome,
    pub shutdown: Option<TreeStopReport>,
}

/// Platform-neutral definition of one validated, registered service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRegistration {
    id: String,
    label: String,
    launch: LaunchSpec,
    startup_prerequisites: Vec<HealthCheckSpec>,
    health: HealthCheckSpec,
    restart_policy: ServiceRestartPolicy,
    ports: Vec<u16>,
    shutdown_grace: Duration,
}

impl ServiceRegistration {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        launch: LaunchSpec,
        health: HealthCheckSpec,
        restart_policy: ServiceRestartPolicy,
        ports: Vec<u16>,
        shutdown_grace: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            launch,
            startup_prerequisites: Vec::new(),
            health,
            restart_policy,
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

    /// Adds bounded external readiness checks that must pass before spawning.
    #[must_use]
    pub fn with_startup_prerequisites(
        mut self,
        prerequisites: impl IntoIterator<Item = HealthCheckSpec>,
    ) -> Self {
        self.startup_prerequisites = prerequisites.into_iter().collect();
        self
    }
}

/// Most recent accepted lifecycle request for one service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastAction {
    pub action: LifecycleAction,
    pub actor: Actor,
    pub reason: Reason,
}

/// Read-only service state returned to every control adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSnapshot {
    pub id: String,
    pub label: String,
    pub lifecycle: LifecycleState,
    pub root_pid: Option<u32>,
    pub owned_pids: Vec<u32>,
    pub health: HealthSnapshot,
    pub desired_state: DesiredState,
    pub started_at_unix_ms: Option<u64>,
    pub last_exit_code: Option<i32>,
    pub last_exit_at_unix_ms: Option<u64>,
    pub restart_count: u32,
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
    services: RwLock<BTreeMap<String, ServiceEntry<Spawner::Process>>>,
    spawner: Spawner,
    port_inspector: Inspector,
    health_probe: Arc<dyn HealthProbe>,
    restart_stability_window: Duration,
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
        health_probe: Arc<dyn HealthProbe>,
    ) -> Result<Self, RegistryBuildError> {
        Self::new_with_restart_stability_window(
            registrations,
            spawner,
            port_inspector,
            health_probe,
            DEFAULT_RESTART_STABILITY_WINDOW,
        )
    }

    /// Builds a registry with an explicit stability window for deterministic
    /// fixtures and alternative compositions.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryBuildError::DuplicateServiceId`] for duplicate IDs.
    pub fn new_with_restart_stability_window(
        registrations: impl IntoIterator<Item = ServiceRegistration>,
        spawner: Spawner,
        port_inspector: Inspector,
        health_probe: Arc<dyn HealthProbe>,
        restart_stability_window: Duration,
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
            services: RwLock::new(services),
            spawner,
            port_inspector,
            health_probe,
            restart_stability_window,
        })
    }

    #[must_use]
    pub fn service_ids(&self) -> Vec<String> {
        self.services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Applies a validated service topology while retaining every unchanged
    /// entry, including its process owner, health, hold, and restart state.
    /// Updated and removed entries must be fully stopped.
    ///
    /// # Errors
    ///
    /// Returns a duplicate-ID, active-service, process-observation, or poisoned
    /// registry failure. No topology change is applied on error.
    pub fn reconcile_registrations(
        &self,
        registrations: impl IntoIterator<Item = ServiceRegistration>,
    ) -> Result<RegistryReconcileOutcome, RegistryReconcileError<ProcessError<Spawner>>> {
        let mut desired = BTreeMap::new();
        for registration in registrations {
            match desired.entry(registration.id.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(registration);
                }
                Entry::Occupied(entry) => {
                    return Err(RegistryReconcileError::DuplicateServiceId(
                        entry.key().clone(),
                    ));
                }
            }
        }

        let mut services = self
            .services
            .write()
            .map_err(|_| RegistryReconcileError::LockPoisoned)?;
        for (service_id, entry) in services.iter() {
            let changed_or_removed = desired
                .get(service_id)
                .is_none_or(|registration| registration != &entry.registration);
            if !changed_or_removed {
                continue;
            }
            let desired_state = entry
                .supervision
                .lock()
                .map_err(|_| RegistryReconcileError::LockPoisoned)?
                .desired_state;
            let has_process = entry.runtime.has_process().map_err(|error| match error {
                ServiceRuntimeError::Poisoned => RegistryReconcileError::LockPoisoned,
                ServiceRuntimeError::Transition(_)
                | ServiceRuntimeError::Start(())
                | ServiceRuntimeError::Stop(())
                | ServiceRuntimeError::Inspect(()) => RegistryReconcileError::InternalState,
            })?;
            if desired_state != DesiredState::Stopped || has_process {
                return Err(RegistryReconcileError::ServiceActive(service_id.clone()));
            }
        }

        let mut current = std::mem::take(&mut *services);
        let mut next = BTreeMap::new();
        let mut outcome = RegistryReconcileOutcome {
            added: Vec::new(),
            updated: Vec::new(),
            removed: Vec::new(),
        };
        for (service_id, registration) in desired {
            match current.remove(&service_id) {
                Some(entry) if entry.registration == registration => {
                    next.insert(service_id, entry);
                }
                Some(_) => {
                    outcome.updated.push(service_id.clone());
                    next.insert(service_id, ServiceEntry::new(registration));
                }
                None => {
                    outcome.added.push(service_id.clone());
                    next.insert(service_id, ServiceEntry::new(registration));
                }
            }
        }
        outcome.removed.extend(current.into_keys());
        *services = next;
        Ok(outcome)
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
        let services = self.read_services()?;
        let entry = Self::entry(&services, service_id)?;
        let _mutation = lock(&entry.mutation)?;
        let mut control = lock(&entry.control)?;
        control
            .policy
            .authorize(actor, LifecycleAction::Start)
            .map_err(RegistryError::Unauthorized)?;
        Self::authorize_recovery(entry, actor)?;

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
        lock(&entry.supervision)?.request_running(actor);

        let outcome = entry
            .runtime
            .start_with(|| {
                self.wait_until_startup_prerequisites(entry)?;
                self.ensure_ports_available(&entry.registration.ports)?;
                self.spawner
                    .spawn(&entry.registration.id, &entry.registration.launch)
                    .map_err(BackendOperationError::Process)
            })
            .map_err(RegistryError::Runtime)?;
        if outcome == StartOutcome::Started {
            lock(&entry.supervision)?.record_started(LifecycleAction::Start);
            self.wait_until_healthy(entry)?;
        }
        Ok(outcome)
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
        self.stop_with_report(service_id, actor, reason)
            .map(|result| result.outcome)
    }

    /// Stops one registered service and preserves the platform-neutral
    /// graceful/forced shutdown evidence returned by its owned process tree.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, authority, platform, or runtime errors as
    /// [`Self::stop`].
    pub fn stop_with_report(
        &self,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ServiceStopResult, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let services = self.read_services()?;
        let entry = Self::entry(&services, service_id)?;
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
        lock(&entry.supervision)?.request_stopped();
        let grace = entry.registration.shutdown_grace;

        let mut shutdown = None;
        let outcome = entry
            .runtime
            .stop_with(|process| {
                let report = process
                    .stop(grace)
                    .map_err(BackendOperationError::Process)?;
                let progress = if report.is_complete() {
                    StopProgress::Complete
                } else {
                    StopProgress::TerminationPending
                };
                shutdown = Some(report);
                Ok(progress)
            })
            .map_err(RegistryError::Runtime)?;
        if matches!(
            outcome,
            StopOutcome::Stopped | StopOutcome::TerminationPending
        ) {
            *lock(&entry.health)? = HealthSnapshot::unknown();
        }
        if outcome == StopOutcome::TerminationPending
            && let Some(report) = shutdown.as_ref()
        {
            lock(&entry.supervision)?
                .last_owned_pids
                .clone_from(&report.owned_pids_before);
        }
        Ok(ServiceStopResult { outcome, shutdown })
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
        self.restart_with_report(service_id, actor, reason)
            .map(|result| result.outcome)
    }

    /// Restarts one registered service and preserves shutdown evidence for the
    /// replaced owned process tree, when one existed.
    ///
    /// # Errors
    ///
    /// Returns the same typed lookup, authority, platform, or runtime errors as
    /// [`Self::restart`].
    pub fn restart_with_report(
        &self,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ServiceRestartResult, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let services = self.read_services()?;
        let entry = Self::entry(&services, service_id)?;
        let _mutation = lock(&entry.mutation)?;
        let mut control = lock(&entry.control)?;
        control
            .policy
            .authorize(actor, LifecycleAction::Restart)
            .map_err(RegistryError::Unauthorized)?;
        Self::authorize_recovery(entry, actor)?;
        control
            .policy
            .apply_user_action(actor, LifecycleAction::Restart);
        control.last_action = Some(LastAction {
            action: LifecycleAction::Restart,
            actor,
            reason,
        });
        lock(&entry.supervision)?.request_running(actor);
        let grace = entry.registration.shutdown_grace;

        let mut shutdown = None;
        let outcome = entry
            .runtime
            .restart_with(
                |process| {
                    let report = process
                        .stop(grace)
                        .map_err(BackendOperationError::Process)?;
                    let progress = if report.is_complete() {
                        StopProgress::Complete
                    } else {
                        StopProgress::TerminationPending
                    };
                    shutdown = Some(report);
                    Ok(progress)
                },
                || {
                    self.wait_until_startup_prerequisites(entry)?;
                    self.ensure_ports_available(&entry.registration.ports)?;
                    self.spawner
                        .spawn(&entry.registration.id, &entry.registration.launch)
                        .map_err(BackendOperationError::Process)
                },
            )
            .map_err(RegistryError::Runtime)?;
        if outcome == RestartOutcome::TerminationPending {
            let mut supervision = lock(&entry.supervision)?;
            supervision.defer_restart_after_termination();
            if let Some(report) = shutdown.as_ref() {
                supervision
                    .last_owned_pids
                    .clone_from(&report.owned_pids_before);
            }
            drop(supervision);
            *lock(&entry.health)? = HealthSnapshot::unknown();
        } else {
            lock(&entry.supervision)?.record_started(LifecycleAction::Restart);
            self.wait_until_healthy(entry)?;
        }
        Ok(ServiceRestartResult { outcome, shutdown })
    }

    /// Returns a consistent snapshot of all registered services.
    ///
    /// # Errors
    ///
    /// Returns a platform observation error or a poisoned-lock error.
    pub fn snapshots(
        &self,
    ) -> Result<Vec<ServiceSnapshot>, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        self.read_services()?.values().map(Self::snapshot).collect()
    }

    /// Refreshes health for every service while preserving per-service
    /// lifecycle serialization.
    ///
    /// # Errors
    ///
    /// Returns the first process observation or lifecycle failure.
    pub fn refresh_healths(
        &self,
    ) -> Result<Vec<HealthSnapshot>, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        Ok(self
            .refresh_services()?
            .into_iter()
            .map(|refresh| refresh.health)
            .collect())
    }

    /// Reconciles terminal process trees and refreshes health for every
    /// service. A terminal tree is emitted exactly once because its retained
    /// owner is released during reconciliation.
    ///
    /// # Errors
    ///
    /// Returns the first process observation or lifecycle failure.
    pub fn refresh_services(
        &self,
    ) -> Result<Vec<ServiceRefresh>, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        self.read_services()?
            .values()
            .map(|entry| {
                let _mutation = lock(&entry.mutation)?;
                let process_exit = self.reconcile_process_exit(entry)?;
                let health = if process_exit.is_some() {
                    lock(&entry.health)?.clone()
                } else {
                    self.observe_health(entry, None)?
                };
                Ok(ServiceRefresh {
                    health,
                    process_exit,
                })
            })
            .collect()
    }

    fn snapshot(
        entry: &ServiceEntry<Spawner::Process>,
    ) -> Result<ServiceSnapshot, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let _mutation = lock(&entry.mutation)?;
        let control = lock(&entry.control)?;
        let health = lock(&entry.health)?.clone();
        let supervision = lock(&entry.supervision)?;
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
            health,
            desired_state: supervision.desired_state,
            started_at_unix_ms: supervision.started_at_unix_ms,
            last_exit_code: supervision.last_exit_code,
            last_exit_at_unix_ms: supervision.last_exit_at_unix_ms,
            restart_count: supervision.restart_count,
            operator_hold: control.policy.operator_hold(),
            last_action: control.last_action.clone(),
        })
    }

    fn reconcile_process_exit(
        &self,
        entry: &ServiceEntry<Spawner::Process>,
    ) -> Result<Option<ProcessExitEvent>, RegistryError<ProcessError<Spawner>, Inspector::Error>>
    {
        let (previous_state, owned_pids) = entry
            .runtime
            .inspect_with(|lifecycle, process| match process {
                Some(process) => Ok((lifecycle, process.owned_pids()?)),
                None => Ok((lifecycle, Vec::new())),
            })
            .map_err(map_observation_error)?;
        if !owned_pids.is_empty() {
            lock(&entry.supervision)?.last_owned_pids = owned_pids;
            return Ok(None);
        }

        let status = entry
            .runtime
            .reconcile_exit_with(ManagedProcessTree::try_wait)
            .map_err(map_observation_error)?;
        let Some(status) = status else {
            return Ok(None);
        };

        let operator_hold = lock(&entry.control)?.policy.operator_hold();
        let mut supervision = lock(&entry.supervision)?;
        let (automatic_restart_planned, deferred_restart_planned) = supervision.record_exit(
            status,
            entry.registration.restart_policy,
            operator_hold,
            self.restart_stability_window,
        );
        let health = if previous_state == LifecycleState::TerminationPending {
            HealthSnapshot::unknown()
        } else {
            HealthSnapshot::unhealthy(
                false,
                match entry.registration.health {
                    HealthCheckSpec::Process => None,
                    _ => Some(false),
                },
                format!(
                    "owned process tree exited{}",
                    status
                        .code()
                        .map_or_else(String::new, |code| format!(" with code {code}"))
                ),
            )
        };
        *lock(&entry.health)? = health;

        Ok(Some(ProcessExitEvent {
            service_id: entry.registration.id.clone(),
            previous_state,
            owned_pids_before: std::mem::take(&mut supervision.last_owned_pids),
            exit_code: status.code(),
            successful: status.success(),
            automatic_restart_planned,
            deferred_restart_planned,
        }))
    }

    fn wait_until_healthy(
        &self,
        entry: &ServiceEntry<Spawner::Process>,
    ) -> Result<(), RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let deadline = entry.registration.health.startup_deadline();
        let started = Instant::now();
        loop {
            let remaining = deadline.checked_sub(started.elapsed()).unwrap_or_default();
            let observation = self.observe_health(entry, Some(remaining))?;
            if observation.is_healthy() {
                return Ok(());
            }
            let elapsed = started.elapsed();
            if elapsed >= deadline {
                return Err(RegistryError::HealthFailed {
                    detail: observation
                        .detail
                        .unwrap_or_else(|| "health expectation did not pass".to_owned()),
                });
            }
            let remaining = deadline.checked_sub(elapsed).unwrap_or_default();
            thread::sleep(Duration::from_millis(100).min(remaining));
        }
    }

    fn wait_until_startup_prerequisites(
        &self,
        entry: &ServiceEntry<Spawner::Process>,
    ) -> Result<(), BackendOperationError<ProcessError<Spawner>, Inspector::Error>> {
        for (index, prerequisite) in entry.registration.startup_prerequisites.iter().enumerate() {
            let deadline = prerequisite.startup_deadline();
            let started = Instant::now();
            loop {
                let remaining = deadline.checked_sub(started.elapsed()).unwrap_or_default();
                let timeout = prerequisite
                    .timeout()
                    .expect("startup prerequisites are transport checks")
                    .min(remaining);
                let observation = self.health_probe.probe(prerequisite, timeout);
                if observation.healthy {
                    break;
                }
                let elapsed = started.elapsed();
                if elapsed >= deadline {
                    return Err(BackendOperationError::StartupPrerequisite {
                        index,
                        detail: observation.detail,
                    });
                }
                let remaining = deadline.checked_sub(elapsed).unwrap_or_default();
                thread::sleep(Duration::from_millis(100).min(remaining));
            }
        }
        Ok(())
    }

    fn observe_health(
        &self,
        entry: &ServiceEntry<Spawner::Process>,
        timeout_cap: Option<Duration>,
    ) -> Result<HealthSnapshot, RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        let process_ready = entry
            .runtime
            .inspect_with(|_, process| match process {
                Some(process) => process.owned_pids().map(|pids| !pids.is_empty()),
                None => Ok(false),
            })
            .map_err(map_observation_error)?;
        let observation = if !entry
            .runtime
            .has_process()
            .map_err(|_| RegistryError::InternalState)?
        {
            HealthSnapshot::unknown()
        } else if !process_ready {
            HealthSnapshot::unhealthy(
                false,
                match entry.registration.health {
                    HealthCheckSpec::Process => None,
                    _ => Some(false),
                },
                "owned process tree is not ready".to_owned(),
            )
        } else {
            match &entry.registration.health {
                HealthCheckSpec::Process => {
                    HealthSnapshot::healthy(true, None, "owned process tree is ready".to_owned())
                }
                check => {
                    let timeout = timeout_cap.map_or_else(
                        || check.timeout().expect("HTTP health has a timeout"),
                        |cap| check.timeout().expect("HTTP health has a timeout").min(cap),
                    );
                    let transport = self.health_probe.probe(check, timeout);
                    let observed = transport.observed;
                    if transport.healthy {
                        HealthSnapshot::healthy(
                            true,
                            Some(transport.transport_ready),
                            transport.detail,
                        )
                        .with_observed(observed)
                    } else {
                        HealthSnapshot::unhealthy(
                            true,
                            Some(transport.transport_ready),
                            transport.detail,
                        )
                        .with_observed(observed)
                    }
                }
            }
        };
        entry
            .runtime
            .apply_health(observation.is_healthy())
            .map_err(|error| match error {
                ServiceRuntimeError::Poisoned => RegistryError::LockPoisoned,
                ServiceRuntimeError::Transition(error) => RegistryError::Transition(error),
                _ => RegistryError::InternalState,
            })?;
        *lock(&entry.health)? = observation.clone();
        Ok(observation)
    }

    #[allow(clippy::type_complexity)]
    fn entry<'a>(
        services: &'a BTreeMap<String, ServiceEntry<Spawner::Process>>,
        service_id: &str,
    ) -> Result<
        &'a ServiceEntry<Spawner::Process>,
        RegistryError<ProcessError<Spawner>, Inspector::Error>,
    > {
        services
            .get(service_id)
            .ok_or_else(|| RegistryError::ServiceNotFound(service_id.to_owned()))
    }

    #[allow(clippy::type_complexity)]
    fn read_services(
        &self,
    ) -> Result<
        RwLockReadGuard<'_, BTreeMap<String, ServiceEntry<Spawner::Process>>>,
        RegistryError<ProcessError<Spawner>, Inspector::Error>,
    > {
        self.services
            .read()
            .map_err(|_| RegistryError::LockPoisoned)
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

    fn authorize_recovery(
        entry: &ServiceEntry<Spawner::Process>,
        actor: Actor,
    ) -> Result<(), RegistryError<ProcessError<Spawner>, Inspector::Error>> {
        if actor == Actor::Recovery
            && lock(&entry.supervision)?.desired_state == DesiredState::Stopped
        {
            return Err(RegistryError::Unauthorized(
                AuthorizationError::RecoveryDesiredStateStopped,
            ));
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
    health: Mutex<HealthSnapshot>,
    supervision: Mutex<SupervisionState>,
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
            health: Mutex::new(HealthSnapshot::unknown()),
            supervision: Mutex::new(SupervisionState::new()),
        }
    }
}

#[derive(Debug)]
struct ControlState {
    policy: ControlPolicy,
    last_action: Option<LastAction>,
}

#[derive(Debug)]
struct SupervisionState {
    desired_state: DesiredState,
    started_at_unix_ms: Option<u64>,
    started_at: Option<Instant>,
    last_exit_code: Option<i32>,
    last_exit_at_unix_ms: Option<u64>,
    restart_count: u32,
    automatic_restarts_in_episode: u32,
    restart_after_termination: bool,
    last_owned_pids: Vec<u32>,
}

impl SupervisionState {
    const fn new() -> Self {
        Self {
            desired_state: DesiredState::Stopped,
            started_at_unix_ms: None,
            started_at: None,
            last_exit_code: None,
            last_exit_at_unix_ms: None,
            restart_count: 0,
            automatic_restarts_in_episode: 0,
            restart_after_termination: false,
            last_owned_pids: Vec::new(),
        }
    }

    fn request_running(&mut self, actor: Actor) {
        self.desired_state = DesiredState::Running;
        self.restart_after_termination = false;
        if actor != Actor::Recovery {
            self.automatic_restarts_in_episode = 0;
        }
    }

    fn request_stopped(&mut self) {
        self.desired_state = DesiredState::Stopped;
        self.started_at = None;
        self.started_at_unix_ms = None;
        self.automatic_restarts_in_episode = 0;
        self.restart_after_termination = false;
        self.last_owned_pids.clear();
    }

    fn record_started(&mut self, action: LifecycleAction) {
        self.started_at = Some(Instant::now());
        self.started_at_unix_ms = Some(unix_milliseconds());
        self.last_owned_pids.clear();
        self.restart_after_termination = false;
        if action == LifecycleAction::Restart {
            self.restart_count = self.restart_count.saturating_add(1);
        }
    }

    fn defer_restart_after_termination(&mut self) {
        self.restart_after_termination = true;
    }

    fn record_exit(
        &mut self,
        status: std::process::ExitStatus,
        restart_policy: ServiceRestartPolicy,
        operator_hold: OperatorHold,
        stability_window: Duration,
    ) -> (bool, bool) {
        if self
            .started_at
            .is_some_and(|started| started.elapsed() >= stability_window)
        {
            self.automatic_restarts_in_episode = 0;
        }
        self.started_at = None;
        self.started_at_unix_ms = None;
        self.last_exit_code = status.code();
        self.last_exit_at_unix_ms = Some(unix_milliseconds());

        let deferred_restart_planned = self.restart_after_termination
            && self.desired_state == DesiredState::Running
            && operator_hold != OperatorHold::Stopped;
        self.restart_after_termination = false;
        let automatic_restart_planned = !deferred_restart_planned
            && restart_policy == ServiceRestartPolicy::OnFailure
            && !status.success()
            && self.desired_state == DesiredState::Running
            && operator_hold != OperatorHold::Stopped
            && self.automatic_restarts_in_episode == 0;
        if automatic_restart_planned {
            self.automatic_restarts_in_episode = 1;
        }
        (automatic_restart_planned, deferred_restart_planned)
    }
}

fn unix_milliseconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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

/// Failure while atomically reconciling the live service topology.
#[derive(Debug)]
pub enum RegistryReconcileError<ProcessFailure> {
    DuplicateServiceId(String),
    ServiceActive(String),
    LockPoisoned,
    InternalState,
    Observation(ProcessFailure),
}

impl<ProcessFailure: fmt::Display> fmt::Display for RegistryReconcileError<ProcessFailure> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateServiceId(service_id) => {
                write!(formatter, "duplicate registered service ID: {service_id}")
            }
            Self::ServiceActive(service_id) => write!(
                formatter,
                "service must be stopped before its registration can change: {service_id}"
            ),
            Self::LockPoisoned => formatter.write_str("service registry lock is poisoned"),
            Self::InternalState => formatter.write_str("service registry invariant was violated"),
            Self::Observation(error) => write!(formatter, "process observation failed: {error}"),
        }
    }
}

impl<ProcessFailure> std::error::Error for RegistryReconcileError<ProcessFailure> where
    ProcessFailure: std::error::Error + 'static
{
}

/// Platform failure that occurs inside a serialized lifecycle callback.
#[derive(Debug)]
pub enum BackendOperationError<ProcessFailure, PortFailure> {
    Process(ProcessFailure),
    StartupPrerequisite {
        index: usize,
        detail: String,
    },
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
            Self::StartupPrerequisite { index, detail } => {
                write!(
                    formatter,
                    "startup prerequisite {} failed: {detail}",
                    index + 1
                )
            }
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
    HealthFailed { detail: String },
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
            Self::HealthFailed { detail } => write!(formatter, "service health failed: {detail}"),
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
    use std::process::{Command, ExitStatus};
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::application::{
        BackendOperationError, HealthCheckSpec, HealthProbe, LaunchSpec, ManagedProcessTree,
        NetworkFamily, PortDiagnostic, PortInspector, PortOccupant, ProcessTreeSpawner,
        ServiceRegistration, ServiceRestartPolicy, ServiceRuntimeError, TransportHealth,
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

        fn spawn(
            &self,
            _service_id: &str,
            _launch: &LaunchSpec,
        ) -> Result<Self::Process, FakeError> {
            let pid = self.spawn_count.fetch_add(1, Ordering::SeqCst) + 1_000;
            Ok(FakeProcess { pid })
        }
    }

    #[derive(Debug, Clone)]
    struct ProcessControl {
        owned_pids: Arc<Mutex<Vec<u32>>>,
        exit_code: Arc<AtomicI32>,
        try_wait_count: Arc<AtomicU32>,
        defer_stop_completion: Arc<AtomicBool>,
    }

    impl ProcessControl {
        fn launcher_exited_with_descendants(&self, code: i32, descendants: Vec<u32>) {
            self.exit_code.store(code, Ordering::SeqCst);
            *self.owned_pids.lock().expect("control lock") = descendants;
        }

        fn tree_exited(&self, code: i32) {
            self.exit_code.store(code, Ordering::SeqCst);
            self.owned_pids.lock().expect("control lock").clear();
        }

        fn defer_stop_completion(&self) {
            self.defer_stop_completion.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct ControllableProcess {
        pid: u32,
        control: ProcessControl,
    }

    impl ManagedProcessTree for ControllableProcess {
        type Error = FakeError;

        fn root_pid(&self) -> u32 {
            self.pid
        }

        fn owned_pids(&self) -> Result<Vec<u32>, Self::Error> {
            self.control
                .owned_pids
                .lock()
                .map(|pids| pids.clone())
                .map_err(|_| FakeError)
        }

        fn try_wait(&mut self) -> Result<Option<ExitStatus>, Self::Error> {
            self.control.try_wait_count.fetch_add(1, Ordering::SeqCst);
            if self.owned_pids()?.is_empty() {
                Ok(Some(exit_status(
                    self.control.exit_code.load(Ordering::SeqCst),
                )))
            } else {
                Ok(None)
            }
        }

        fn stop(&mut self, _grace: Duration) -> Result<TreeStopReport, Self::Error> {
            let before = self.owned_pids()?;
            if !self.control.defer_stop_completion.load(Ordering::SeqCst) {
                self.control
                    .owned_pids
                    .lock()
                    .map_err(|_| FakeError)?
                    .clear();
            }
            let after = self.owned_pids()?;
            Ok(TreeStopReport {
                owned_pids_before: before,
                owned_pids_after: after,
                graceful_signal_sent: true,
                graceful_signal_error: None,
                forced: self.control.defer_stop_completion.load(Ordering::SeqCst),
            })
        }
    }

    #[derive(Debug, Clone)]
    struct ControllableSpawner {
        next_pid: Arc<AtomicU32>,
        controls: Arc<Mutex<Vec<ProcessControl>>>,
    }

    impl ControllableSpawner {
        fn new() -> Self {
            Self {
                next_pid: Arc::new(AtomicU32::new(2_000)),
                controls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn latest(&self) -> ProcessControl {
            self.controls
                .lock()
                .expect("controls lock")
                .last()
                .expect("spawned control")
                .clone()
        }

        fn spawn_count(&self) -> usize {
            self.controls.lock().expect("controls lock").len()
        }
    }

    impl ProcessTreeSpawner for ControllableSpawner {
        type Process = ControllableProcess;

        fn spawn(
            &self,
            _service_id: &str,
            _launch: &LaunchSpec,
        ) -> Result<Self::Process, FakeError> {
            let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
            let control = ProcessControl {
                owned_pids: Arc::new(Mutex::new(vec![pid])),
                exit_code: Arc::new(AtomicI32::new(0)),
                try_wait_count: Arc::new(AtomicU32::new(0)),
                defer_stop_completion: Arc::new(AtomicBool::new(false)),
            };
            self.controls
                .lock()
                .map_err(|_| FakeError)?
                .push(control.clone());
            Ok(ControllableProcess { pid, control })
        }
    }

    fn exit_status(code: i32) -> ExitStatus {
        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", "exit", &code.to_string()])
                .status()
                .expect("create Windows fixture exit status")
        }
        #[cfg(unix)]
        {
            Command::new("sh")
                .args(["-c", &format!("exit {code}")])
                .status()
                .expect("create Unix fixture exit status")
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct FakePortInspector {
        occupied: bool,
    }

    #[derive(Debug)]
    struct FakeHealthProbe;

    impl HealthProbe for FakeHealthProbe {
        fn probe(&self, _check: &HealthCheckSpec, _timeout: Duration) -> TransportHealth {
            TransportHealth {
                transport_ready: true,
                healthy: true,
                detail: "fixture healthy".to_owned(),
                observed: std::collections::BTreeMap::new(),
            }
        }
    }

    #[derive(Debug)]
    struct ToggleHealthProbe {
        healthy: Arc<AtomicBool>,
    }

    impl HealthProbe for ToggleHealthProbe {
        fn probe(&self, _check: &HealthCheckSpec, _timeout: Duration) -> TransportHealth {
            let healthy = self.healthy.load(Ordering::SeqCst);
            TransportHealth {
                transport_ready: healthy,
                healthy,
                detail: if healthy {
                    "fixture healthy"
                } else {
                    "fixture health mismatch"
                }
                .to_owned(),
                observed: std::collections::BTreeMap::new(),
            }
        }
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
        registration_with_policy(ports, ServiceRestartPolicy::Manual)
    }

    fn registration_with_policy(
        ports: Vec<u16>,
        restart_policy: ServiceRestartPolicy,
    ) -> ServiceRegistration {
        ServiceRegistration::new(
            "fixture",
            "Fixture",
            LaunchSpec::new(
                "fixture",
                std::iter::empty::<&str>(),
                ".",
                std::iter::empty::<(&str, &str)>(),
            ),
            HealthCheckSpec::Process,
            restart_policy,
            ports,
            Duration::from_millis(10),
        )
    }

    fn named_registration(service_id: &str, label: &str) -> ServiceRegistration {
        ServiceRegistration::new(
            service_id,
            label,
            LaunchSpec::new(
                service_id,
                std::iter::empty::<&str>(),
                ".",
                std::iter::empty::<(&str, &str)>(),
            ),
            HealthCheckSpec::Process,
            ServiceRestartPolicy::Manual,
            Vec::new(),
            Duration::from_millis(10),
        )
    }

    fn http_registration() -> ServiceRegistration {
        ServiceRegistration::new(
            "fixture",
            "Fixture",
            LaunchSpec::new(
                "fixture",
                std::iter::empty::<&str>(),
                ".",
                std::iter::empty::<(&str, &str)>(),
            ),
            HealthCheckSpec::HttpStatus {
                url: "http://127.0.0.1:49001/health".to_owned(),
                expected_status: 200,
                timeout: Duration::from_millis(1),
                startup_deadline: Duration::ZERO,
            },
            ServiceRestartPolicy::Manual,
            Vec::new(),
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
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");

        registry
            .start("fixture", Actor::UserCli, reason("user start"))
            .expect("user starts fixture");
        let result = registry
            .stop_with_report("fixture", Actor::UserCli, reason("user stop"))
            .expect("user stops fixture");
        assert_eq!(result.outcome, super::StopOutcome::Stopped);
        let report = result
            .shutdown
            .expect("stopped owner reports shutdown evidence");
        assert!(report.graceful_signal_sent);
        assert!(!report.forced);
        assert!(report.owned_pids_after.is_empty());
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
            Arc::new(FakeHealthProbe),
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

    #[test]
    fn failed_startup_prerequisite_never_invokes_process_spawner() {
        let spawn_count = Arc::new(AtomicU32::new(0));
        let healthy = Arc::new(AtomicBool::new(false));
        let registration =
            registration(Vec::new()).with_startup_prerequisites([HealthCheckSpec::TcpConnect {
                host: "127.0.0.1".to_owned(),
                port: 54_321,
                timeout: Duration::from_millis(1),
                startup_deadline: Duration::ZERO,
            }]);
        let registry = ServiceRegistry::new(
            [registration],
            FakeSpawner {
                spawn_count: Arc::clone(&spawn_count),
            },
            FakePortInspector { occupied: false },
            Arc::new(ToggleHealthProbe { healthy }),
        )
        .expect("valid registry");

        let result = registry.start("fixture", Actor::UserCli, reason("user start"));

        assert!(matches!(
            result,
            Err(RegistryError::Runtime(ServiceRuntimeError::Start(
                BackendOperationError::StartupPrerequisite { index: 0, .. }
            )))
        ));
        assert_eq!(spawn_count.load(Ordering::SeqCst), 0);
        let snapshot = registry.snapshots().expect("failed snapshot").remove(0);
        assert_eq!(snapshot.lifecycle, crate::domain::LifecycleState::Failed);
        assert_eq!(snapshot.root_pid, None);
    }

    #[test]
    fn health_failure_retains_owner_and_later_snapshot_recovers() {
        let healthy = Arc::new(AtomicBool::new(false));
        let registry = ServiceRegistry::new(
            [http_registration()],
            FakeSpawner {
                spawn_count: Arc::new(AtomicU32::new(0)),
            },
            FakePortInspector { occupied: false },
            Arc::new(ToggleHealthProbe {
                healthy: Arc::clone(&healthy),
            }),
        )
        .expect("valid registry");

        let failed = registry.start("fixture", Actor::UserCli, reason("user start"));
        assert!(matches!(failed, Err(RegistryError::HealthFailed { .. })));
        let unhealthy = registry.snapshots().expect("unhealthy snapshot").remove(0);
        assert_eq!(
            unhealthy.lifecycle,
            crate::domain::LifecycleState::Unhealthy
        );
        assert_eq!(unhealthy.root_pid, Some(1_000));
        assert!(!unhealthy.health.is_healthy());

        healthy.store(true, Ordering::SeqCst);
        registry.refresh_healths().expect("health refresh");
        let recovered = registry.snapshots().expect("recovered snapshot").remove(0);
        assert_eq!(recovered.lifecycle, crate::domain::LifecycleState::Running);
        assert!(recovered.health.is_healthy());
    }

    #[test]
    fn terminal_tree_is_released_and_manual_start_is_available_again() {
        let spawner = ControllableSpawner::new();
        let registry = ServiceRegistry::new(
            [registration(Vec::new())],
            spawner.clone(),
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("initial start"))
            .expect("start fixture");
        spawner.latest().tree_exited(17);

        let refresh = registry.refresh_services().expect("exit refresh").remove(0);
        let exit = refresh.process_exit.expect("one process exit event");
        assert_eq!(exit.exit_code, Some(17));
        assert!(!exit.automatic_restart_planned);
        let failed = registry.snapshots().expect("failed snapshot").remove(0);
        assert_eq!(failed.lifecycle, crate::domain::LifecycleState::Failed);
        assert_eq!(failed.desired_state, crate::domain::DesiredState::Running);
        assert_eq!(failed.root_pid, None);
        assert_eq!(failed.last_exit_code, Some(17));

        assert!(matches!(
            registry.start("fixture", Actor::UserCli, reason("manual recovery")),
            Ok(crate::application::StartOutcome::Started)
        ));
        assert_eq!(spawner.controls.lock().expect("controls lock").len(), 2);
    }

    #[test]
    fn launcher_exit_does_not_release_owner_while_descendant_remains() {
        let spawner = ControllableSpawner::new();
        let registry = ServiceRegistry::new(
            [registration(Vec::new())],
            spawner.clone(),
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("initial start"))
            .expect("start fixture");
        let control = spawner.latest();
        control.launcher_exited_with_descendants(23, vec![9_001]);

        let refresh = registry
            .refresh_services()
            .expect("descendant refresh")
            .remove(0);
        assert!(refresh.process_exit.is_none());
        assert_eq!(control.try_wait_count.load(Ordering::SeqCst), 0);
        let running = registry.snapshots().expect("running snapshot").remove(0);
        assert_eq!(running.lifecycle, crate::domain::LifecycleState::Running);
        assert_eq!(running.owned_pids, vec![9_001]);

        control.tree_exited(23);
        let terminal = registry
            .refresh_services()
            .expect("terminal refresh")
            .remove(0);
        assert_eq!(
            terminal.process_exit.expect("terminal event").exit_code,
            Some(23)
        );
        assert_eq!(control.try_wait_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn on_failure_allows_only_one_automatic_restart_in_unstable_episode() {
        let spawner = ControllableSpawner::new();
        let registry = ServiceRegistry::new(
            [registration_with_policy(
                Vec::new(),
                ServiceRestartPolicy::OnFailure,
            )],
            spawner.clone(),
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("initial start"))
            .expect("start fixture");
        spawner.latest().tree_exited(31);
        let first = registry
            .refresh_services()
            .expect("first exit refresh")
            .remove(0)
            .process_exit
            .expect("first exit");
        assert!(first.automatic_restart_planned);

        registry
            .restart(
                "fixture",
                Actor::Recovery,
                reason("automatic on-failure restart"),
            )
            .expect("automatic restart");
        spawner.latest().tree_exited(32);
        let second = registry
            .refresh_services()
            .expect("second exit refresh")
            .remove(0)
            .process_exit
            .expect("second exit");
        assert!(!second.automatic_restart_planned);
        let failed = registry.snapshots().expect("failed snapshot").remove(0);
        assert_eq!(failed.restart_count, 1);
        assert_eq!(failed.last_exit_code, Some(32));
    }

    #[test]
    fn stable_runtime_starts_a_new_on_failure_episode() {
        let spawner = ControllableSpawner::new();
        let registry = ServiceRegistry::new_with_restart_stability_window(
            [registration_with_policy(
                Vec::new(),
                ServiceRestartPolicy::OnFailure,
            )],
            spawner.clone(),
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
            Duration::ZERO,
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("initial start"))
            .expect("start fixture");
        spawner.latest().tree_exited(41);
        assert!(
            registry
                .refresh_services()
                .expect("first exit")
                .remove(0)
                .process_exit
                .expect("first event")
                .automatic_restart_planned
        );
        registry
            .restart(
                "fixture",
                Actor::Recovery,
                reason("automatic on-failure restart"),
            )
            .expect("automatic restart");
        spawner.latest().tree_exited(42);
        assert!(
            registry
                .refresh_services()
                .expect("stable episode exit")
                .remove(0)
                .process_exit
                .expect("stable episode event")
                .automatic_restart_planned
        );
    }

    #[test]
    fn explicit_stop_wins_race_with_planned_recovery() {
        let spawner = ControllableSpawner::new();
        let registry = ServiceRegistry::new(
            [registration_with_policy(
                Vec::new(),
                ServiceRestartPolicy::OnFailure,
            )],
            spawner.clone(),
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("initial start"))
            .expect("start fixture");
        spawner.latest().tree_exited(51);
        assert!(
            registry
                .refresh_services()
                .expect("exit refresh")
                .remove(0)
                .process_exit
                .expect("exit event")
                .automatic_restart_planned
        );

        registry
            .stop("fixture", Actor::Agent, reason("explicit agent stop"))
            .expect("stop already exited service");
        let recovery = registry.restart(
            "fixture",
            Actor::Recovery,
            reason("automatic on-failure restart"),
        );
        assert!(matches!(
            recovery,
            Err(RegistryError::Unauthorized(
                AuthorizationError::RecoveryDesiredStateStopped
            ))
        ));
        assert_eq!(
            registry.snapshots().expect("stopped snapshot")[0].desired_state,
            crate::domain::DesiredState::Stopped
        );
    }

    #[test]
    fn deferred_restart_waits_for_owned_tree_to_be_empty() {
        let spawner = ControllableSpawner::new();
        let registry = ServiceRegistry::new(
            [registration(Vec::new())],
            spawner.clone(),
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("initial start"))
            .expect("start fixture");
        let old_tree = spawner.latest();
        old_tree.defer_stop_completion();

        let outcome = registry
            .restart("fixture", Actor::UserCli, reason("replace slow tree"))
            .expect("restart accepted");

        assert_eq!(outcome, super::RestartOutcome::TerminationPending);
        assert_eq!(
            spawner.spawn_count(),
            1,
            "replacement must not overlap old tree"
        );
        let pending = &registry.snapshots().expect("pending snapshot")[0];
        assert_eq!(
            pending.lifecycle,
            crate::domain::LifecycleState::TerminationPending
        );
        assert_eq!(pending.desired_state, crate::domain::DesiredState::Running);

        old_tree.tree_exited(1);
        let event = registry
            .refresh_services()
            .expect("reconcile terminal tree")
            .remove(0)
            .process_exit
            .expect("completion event");
        assert!(event.deferred_restart_planned);
        assert!(!event.automatic_restart_planned);
        assert_eq!(
            event.previous_state,
            crate::domain::LifecycleState::TerminationPending
        );
        let completed = &registry.snapshots().expect("completed snapshot")[0];
        assert_eq!(completed.lifecycle, crate::domain::LifecycleState::Stopped);
        assert_eq!(
            completed.health.status,
            crate::application::HealthStatus::Unknown
        );

        assert_eq!(
            registry
                .restart(
                    "fixture",
                    Actor::Recovery,
                    reason("deferred restart after termination")
                )
                .expect("start replacement"),
            super::RestartOutcome::Started
        );
        assert_eq!(spawner.spawn_count(), 2);
        assert_eq!(
            registry.snapshots().expect("replacement snapshot")[0].lifecycle,
            crate::domain::LifecycleState::Running
        );
    }

    #[test]
    fn registration_reconciliation_retains_unrelated_running_owner() {
        let spawner = ControllableSpawner::new();
        let existing = named_registration("fixture", "Fixture");
        let registry = ServiceRegistry::new(
            [existing.clone()],
            spawner,
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("start retained service"))
            .expect("start fixture");
        let before = registry.snapshots().expect("snapshot before reconcile")[0].clone();

        let outcome = registry
            .reconcile_registrations([
                existing,
                named_registration("registered", "Registered service"),
            ])
            .expect("add stopped registration");

        assert_eq!(outcome.added, ["registered"]);
        assert!(outcome.updated.is_empty());
        assert!(outcome.removed.is_empty());
        let snapshots = registry.snapshots().expect("snapshot after reconcile");
        let retained = snapshots
            .iter()
            .find(|snapshot| snapshot.id == "fixture")
            .expect("retained fixture");
        assert_eq!(retained.root_pid, before.root_pid);
        assert_eq!(retained.owned_pids, before.owned_pids);
        assert_eq!(retained.lifecycle, before.lifecycle);
        let added = snapshots
            .iter()
            .find(|snapshot| snapshot.id == "registered")
            .expect("new registration");
        assert_eq!(added.lifecycle, crate::domain::LifecycleState::Stopped);
        assert_eq!(added.desired_state, crate::domain::DesiredState::Stopped);
    }

    #[test]
    fn registration_reconciliation_rejects_active_target_atomically() {
        let registry = ServiceRegistry::new(
            [named_registration("fixture", "Fixture")],
            FakeSpawner {
                spawn_count: Arc::new(AtomicU32::new(0)),
            },
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");
        registry
            .start("fixture", Actor::UserCli, reason("start fixture"))
            .expect("start fixture");

        let error = registry
            .reconcile_registrations([
                named_registration("fixture", "Changed fixture"),
                named_registration("new", "Must not be added"),
            ])
            .expect_err("active target rejects reconcile");

        assert!(matches!(
            error,
            super::RegistryReconcileError::ServiceActive(service_id)
                if service_id == "fixture"
        ));
        assert_eq!(registry.service_ids(), ["fixture"]);
        assert_eq!(
            registry.snapshots().expect("unchanged registry")[0].label,
            "Fixture"
        );
    }

    #[test]
    fn stopped_registration_can_be_updated_and_removed() {
        let registry = ServiceRegistry::new(
            [named_registration("fixture", "Fixture")],
            FakeSpawner {
                spawn_count: Arc::new(AtomicU32::new(0)),
            },
            FakePortInspector { occupied: false },
            Arc::new(FakeHealthProbe),
        )
        .expect("valid registry");

        let updated = registry
            .reconcile_registrations([named_registration("fixture", "Updated fixture")])
            .expect("update stopped registration");
        assert_eq!(updated.updated, ["fixture"]);
        assert_eq!(
            registry.snapshots().expect("updated registry")[0].label,
            "Updated fixture"
        );

        let removed = registry
            .reconcile_registrations([named_registration("other", "Other")])
            .expect("remove stopped registration");
        assert_eq!(removed.added, ["other"]);
        assert_eq!(removed.removed, ["fixture"]);
        assert_eq!(registry.service_ids(), ["other"]);
    }
}
