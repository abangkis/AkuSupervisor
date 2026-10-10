use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::domain::{Actor, Reason};

use super::{
    ControlAction, ControlError, ControlMutationResult, ServiceSnapshot, SupervisorControl,
};

/// Adds dependency-aware start/restart planning without changing any concrete
/// process, HTTP, CLI, or platform adapter.
#[derive(Clone)]
pub struct DependencyAwareControl {
    inner: Arc<dyn SupervisorControl>,
    observer: Arc<dyn DependencyPlanObserver>,
}

impl DependencyAwareControl {
    #[must_use]
    pub fn new(inner: Arc<dyn SupervisorControl>) -> Self {
        Self {
            inner,
            observer: Arc::new(IgnoreDependencyPlans),
        }
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn DependencyPlanObserver>) -> Self {
        self.observer = observer;
        self
    }
}

/// Receives dependency-plan visibility without coupling lifecycle planning to
/// a console, logger, operating system, or protocol adapter.
pub trait DependencyPlanObserver: Send + Sync {
    fn planned(
        &self,
        action: ControlAction,
        target_service_id: &str,
        direct_dependencies: &[String],
        ordered_services: &[String],
    );
}

#[derive(Debug)]
struct IgnoreDependencyPlans;

impl DependencyPlanObserver for IgnoreDependencyPlans {
    fn planned(
        &self,
        _action: ControlAction,
        _target_service_id: &str,
        _direct_dependencies: &[String],
        _ordered_services: &[String],
    ) {
    }
}

impl SupervisorControl for DependencyAwareControl {
    fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
        self.inner.snapshots()
    }

    fn mutate(
        &self,
        action: ControlAction,
        service_id: &str,
        actor: Actor,
        reason: Reason,
    ) -> Result<ControlMutationResult, ControlError> {
        if matches!(action, ControlAction::Start | ControlAction::Restart) {
            let snapshots = self.inner.snapshots()?;
            let order = stable_start_order(&snapshots, &[service_id.to_owned()])?;
            let service_by_id = snapshots
                .iter()
                .map(|snapshot| (snapshot.id.as_str(), snapshot))
                .collect::<BTreeMap<_, _>>();
            let launch_ids = order
                .iter()
                .filter(|planned_id| {
                    (*planned_id == service_id && action == ControlAction::Restart)
                        || service_by_id
                            .get(planned_id.as_str())
                            .is_some_and(|snapshot| snapshot.root_pid.is_none())
                })
                .cloned()
                .collect::<Vec<_>>();
            for launch_id in &launch_ids {
                self.inner
                    .preflight_launches(std::slice::from_ref(launch_id))
                    .map_err(|error| {
                        error.with_context(format_args!(
                            "cannot prepare service {launch_id} before {action:?} of {service_id}"
                        ))
                    })?;
            }
            if order.len() > 1 {
                let direct_dependencies = snapshots
                    .iter()
                    .find(|snapshot| snapshot.id == service_id)
                    .map(|snapshot| snapshot.depends_on.as_slice())
                    .unwrap_or_default();
                self.observer
                    .planned(action, service_id, direct_dependencies, &order);
            }
            for dependency in order.iter().take(order.len().saturating_sub(1)) {
                self.inner
                    .mutate(ControlAction::Start, dependency, actor, reason.clone())
                    .map_err(|error| {
                        error.with_context(format_args!(
                            "cannot start dependency {dependency} for {service_id}"
                        ))
                    })?;
            }
        }
        self.inner.mutate(action, service_id, actor, reason)
    }
}

/// Produces a stable topological order for requested services and their
/// transitive dependencies. Requested services with no ordering relationship
/// retain their caller-provided order.
///
/// # Errors
///
/// Returns an error when a requested service or dependency is absent, or when
/// the supplied snapshots contain a dependency cycle.
pub fn stable_start_order(
    snapshots: &[ServiceSnapshot],
    requested: &[String],
) -> Result<Vec<String>, ControlError> {
    let services = snapshots
        .iter()
        .map(|snapshot| (snapshot.id.as_str(), snapshot))
        .collect::<BTreeMap<_, _>>();
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();
    for service_id in requested {
        visit_start(
            service_id,
            &services,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }
    Ok(ordered)
}

fn visit_start(
    service_id: &str,
    services: &BTreeMap<&str, &ServiceSnapshot>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    ordered: &mut Vec<String>,
) -> Result<(), ControlError> {
    if visited.contains(service_id) {
        return Ok(());
    }
    let Some(service) = services.get(service_id) else {
        return Err(ControlError::service_not_found(service_id));
    };
    if !visiting.insert(service_id.to_owned()) {
        return Err(ControlError::internal(format!(
            "service dependency cycle reached at {service_id}"
        )));
    }
    for dependency in &service.depends_on {
        visit_start(dependency, services, visiting, visited, ordered)?;
    }
    visiting.remove(service_id);
    visited.insert(service_id.to_owned());
    ordered.push(service_id.to_owned());
    Ok(())
}

/// Returns a stable dependent-before-dependency order for graceful cleanup.
/// Services without an ordering relationship retain their snapshot order.
///
/// # Errors
///
/// Returns an error when a dependency is absent or the supplied snapshots
/// contain a dependency cycle.
pub fn stable_shutdown_order(snapshots: &[ServiceSnapshot]) -> Result<Vec<String>, ControlError> {
    let positions = snapshots
        .iter()
        .enumerate()
        .map(|(position, snapshot)| (snapshot.id.as_str(), position))
        .collect::<BTreeMap<_, _>>();
    let mut remaining_dependents = snapshots
        .iter()
        .map(|snapshot| (snapshot.id.as_str(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut dependencies = snapshots
        .iter()
        .map(|snapshot| (snapshot.id.as_str(), Vec::<&str>::new()))
        .collect::<BTreeMap<_, _>>();
    for service in snapshots {
        for dependency in &service.depends_on {
            let Some(dependent_count) = remaining_dependents.get_mut(dependency.as_str()) else {
                return Err(ControlError::service_not_found(dependency));
            };
            *dependent_count += 1;
            let Some(service_dependencies) = dependencies.get_mut(service.id.as_str()) else {
                return Err(ControlError::service_not_found(&service.id));
            };
            service_dependencies.push(dependency);
        }
    }

    let mut ordered = Vec::new();
    let mut ready = Vec::new();
    for snapshot in snapshots {
        if remaining_dependents.get(snapshot.id.as_str()).copied() == Some(0) {
            ready.push(snapshot.id.as_str());
        }
    }
    while !ready.is_empty() {
        ready.sort_by_key(|service_id| positions.get(service_id).copied().unwrap_or(usize::MAX));
        let service_id = ready.remove(0);
        ordered.push(service_id.to_owned());
        let Some(service_dependencies) = dependencies.get(service_id) else {
            return Err(ControlError::service_not_found(service_id));
        };
        for dependency in service_dependencies {
            let count = remaining_dependents
                .get_mut(dependency)
                .ok_or_else(|| ControlError::service_not_found(dependency))?;
            *count -= 1;
            if *count == 0 {
                ready.push(dependency);
            }
        }
    }
    if ordered.len() != snapshots.len() {
        return Err(ControlError::internal(
            "service dependency cycle reached while planning shutdown",
        ));
    }
    Ok(ordered)
}

impl std::fmt::Debug for DependencyAwareControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DependencyAwareControl")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::application::{ControlMutationOutcome, HealthSnapshot, ServiceSnapshot};
    use crate::domain::{DesiredState, LifecycleState, OperatorHold};

    use super::*;

    #[derive(Debug)]
    struct FakeControl {
        snapshots: Vec<ServiceSnapshot>,
        calls: Mutex<Vec<(ControlAction, String)>>,
        operations: Mutex<Vec<String>>,
        preflight_failure: Option<String>,
    }

    #[derive(Debug, Default)]
    struct RecordingObserver {
        plans: Mutex<Vec<RecordedPlan>>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedPlan {
        action: ControlAction,
        target_service_id: String,
        direct_dependencies: Vec<String>,
        ordered_services: Vec<String>,
    }

    impl DependencyPlanObserver for RecordingObserver {
        fn planned(
            &self,
            action: ControlAction,
            target_service_id: &str,
            direct_dependencies: &[String],
            ordered_services: &[String],
        ) {
            self.plans.lock().expect("plan lock").push(RecordedPlan {
                action,
                target_service_id: target_service_id.to_owned(),
                direct_dependencies: direct_dependencies.to_vec(),
                ordered_services: ordered_services.to_vec(),
            });
        }
    }

    impl SupervisorControl for FakeControl {
        fn snapshots(&self) -> Result<Vec<ServiceSnapshot>, ControlError> {
            Ok(self.snapshots.clone())
        }

        fn preflight_launches(&self, service_ids: &[String]) -> Result<(), ControlError> {
            self.operations
                .lock()
                .expect("operation lock")
                .push(format!("preflight:{}", service_ids.join(",")));
            if let Some(failed_id) = self.preflight_failure.as_deref()
                && service_ids.iter().any(|service_id| service_id == failed_id)
            {
                return Err(ControlError::internal(format!(
                    "launch paths unavailable for {failed_id}"
                )));
            }
            Ok(())
        }

        fn mutate(
            &self,
            action: ControlAction,
            service_id: &str,
            _actor: Actor,
            _reason: Reason,
        ) -> Result<ControlMutationResult, ControlError> {
            self.operations
                .lock()
                .expect("operation lock")
                .push(format!("{action:?}:{service_id}"));
            self.calls
                .lock()
                .expect("fake call lock")
                .push((action, service_id.to_owned()));
            Ok(ControlMutationResult::new(
                match action {
                    ControlAction::Start => ControlMutationOutcome::Started,
                    ControlAction::Stop => ControlMutationOutcome::Stopped,
                    ControlAction::Restart => ControlMutationOutcome::Restarted,
                },
                None,
            ))
        }
    }

    fn snapshot(id: &str, dependencies: &[&str]) -> ServiceSnapshot {
        ServiceSnapshot {
            id: id.to_owned(),
            label: id.to_owned(),
            depends_on: dependencies
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            ports: Vec::new(),
            lifecycle: LifecycleState::Stopped,
            root_pid: None,
            owned_pids: Vec::new(),
            owned_processes: Vec::new(),
            health: HealthSnapshot::unknown(),
            desired_state: DesiredState::Stopped,
            started_at_unix_ms: None,
            last_exit_code: None,
            last_exit_at_unix_ms: None,
            restart_count: 0,
            operator_hold: OperatorHold::None,
            last_action: None,
        }
    }

    #[test]
    fn stable_order_preserves_independent_request_order() {
        let snapshots = vec![
            snapshot("database", &[]),
            snapshot("frontend", &[]),
            snapshot("platform", &["koc"]),
            snapshot("koc", &["database"]),
        ];
        let order = stable_start_order(&snapshots, &["frontend".to_owned(), "platform".to_owned()])
            .expect("valid order");
        assert_eq!(order, ["frontend", "database", "koc", "platform"]);
        assert_eq!(
            stable_shutdown_order(&snapshots).expect("shutdown order"),
            ["frontend", "platform", "koc", "database"]
        );
    }

    #[test]
    fn services_without_dependencies_keep_exact_caller_and_snapshot_order() {
        let snapshots = vec![
            snapshot("zeta", &[]),
            snapshot("alpha", &[]),
            snapshot("middle", &[]),
        ];
        assert_eq!(
            stable_start_order(
                &snapshots,
                &["middle".to_owned(), "zeta".to_owned(), "alpha".to_owned()]
            )
            .expect("independent start order"),
            ["middle", "zeta", "alpha"]
        );
        assert_eq!(
            stable_shutdown_order(&snapshots).expect("independent shutdown order"),
            ["zeta", "alpha", "middle"]
        );
    }

    #[test]
    fn start_ensures_transitive_dependencies_before_target() {
        let inner = Arc::new(FakeControl {
            snapshots: vec![
                snapshot("database", &[]),
                snapshot("koc", &["database"]),
                snapshot("platform", &["koc"]),
            ],
            calls: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            preflight_failure: None,
        });
        let observer = Arc::new(RecordingObserver::default());
        let control = DependencyAwareControl::new(inner.clone()).with_observer(observer.clone());
        control
            .mutate(
                ControlAction::Start,
                "platform",
                Actor::UserCli,
                Reason::new("test dependency order").expect("reason"),
            )
            .expect("dependency-aware start");
        assert_eq!(
            *inner.calls.lock().expect("calls"),
            [
                (ControlAction::Start, "database".to_owned()),
                (ControlAction::Start, "koc".to_owned()),
                (ControlAction::Start, "platform".to_owned()),
            ]
        );
        assert_eq!(
            *observer.plans.lock().expect("plans"),
            [RecordedPlan {
                action: ControlAction::Start,
                target_service_id: "platform".to_owned(),
                direct_dependencies: vec!["koc".to_owned()],
                ordered_services: vec![
                    "database".to_owned(),
                    "koc".to_owned(),
                    "platform".to_owned()
                ],
            }]
        );
    }

    #[test]
    fn restart_preflights_every_service_that_will_launch_before_dependency_mutations() {
        let mut target = snapshot("target", &["dependency"]);
        target.root_pid = Some(44);
        let inner = Arc::new(FakeControl {
            snapshots: vec![snapshot("dependency", &[]), target],
            calls: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            preflight_failure: None,
        });
        let control = DependencyAwareControl::new(inner.clone());

        control
            .mutate(
                ControlAction::Restart,
                "target",
                Actor::UserCli,
                Reason::new("preflight dependency before restart").expect("reason"),
            )
            .expect("restart should proceed after successful preflight");

        assert_eq!(
            *inner.operations.lock().expect("operations"),
            [
                "preflight:dependency",
                "preflight:target",
                "Start:dependency",
                "Restart:target"
            ]
        );
    }

    #[test]
    fn failed_dependency_preflight_prevents_all_mutations_in_restart_plan() {
        let mut target = snapshot("target", &["dependency"]);
        target.root_pid = Some(44);
        let inner = Arc::new(FakeControl {
            snapshots: vec![snapshot("dependency", &[]), target],
            calls: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            preflight_failure: Some("dependency".to_owned()),
        });
        let control = DependencyAwareControl::new(inner.clone());

        let error = control
            .mutate(
                ControlAction::Restart,
                "target",
                Actor::UserCli,
                Reason::new("failed dependency preflight").expect("reason"),
            )
            .expect_err("missing dependency launch paths should reject restart");

        assert!(error.message().contains("dependency"));
        assert!(inner.calls.lock().expect("calls").is_empty());
        assert_eq!(
            *inner.operations.lock().expect("operations"),
            ["preflight:dependency"]
        );
    }

    #[test]
    fn failed_target_preflight_after_dependency_readiness_prevents_all_mutations() {
        let mut target = snapshot("target", &["dependency"]);
        target.root_pid = Some(44);
        let inner = Arc::new(FakeControl {
            snapshots: vec![snapshot("dependency", &[]), target],
            calls: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            preflight_failure: Some("target".to_owned()),
        });
        let control = DependencyAwareControl::new(inner.clone());

        let error = control
            .mutate(
                ControlAction::Restart,
                "target",
                Actor::UserCli,
                Reason::new("target launch path disappeared").expect("reason"),
            )
            .expect_err("target preflight should reject the restart plan");

        assert!(error.message().contains("target"));
        assert!(inner.calls.lock().expect("calls").is_empty());
        assert_eq!(
            *inner.operations.lock().expect("operations"),
            ["preflight:dependency", "preflight:target"]
        );
    }

    #[test]
    fn restart_does_not_preflight_a_dependency_that_is_already_running() {
        let mut dependency = snapshot("dependency", &[]);
        dependency.root_pid = Some(31);
        let mut target = snapshot("target", &["dependency"]);
        target.root_pid = Some(44);
        let inner = Arc::new(FakeControl {
            snapshots: vec![dependency, target],
            calls: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            preflight_failure: Some("dependency".to_owned()),
        });
        let control = DependencyAwareControl::new(inner.clone());

        control
            .mutate(
                ControlAction::Restart,
                "target",
                Actor::UserCli,
                Reason::new("running dependency has no launch requirement").expect("reason"),
            )
            .expect("missing dependency launch files must not block a running dependency");

        assert_eq!(
            *inner.operations.lock().expect("operations"),
            ["preflight:target", "Start:dependency", "Restart:target"]
        );
        assert_eq!(
            *inner.calls.lock().expect("calls"),
            [
                (ControlAction::Start, "dependency".to_owned()),
                (ControlAction::Restart, "target".to_owned())
            ]
        );
    }

    #[test]
    fn service_without_dependencies_keeps_single_mutation_behavior() {
        let inner = Arc::new(FakeControl {
            snapshots: vec![snapshot("frontend", &[])],
            calls: Mutex::new(Vec::new()),
            operations: Mutex::new(Vec::new()),
            preflight_failure: None,
        });
        let observer = Arc::new(RecordingObserver::default());
        let control = DependencyAwareControl::new(inner.clone()).with_observer(observer.clone());
        control
            .mutate(
                ControlAction::Restart,
                "frontend",
                Actor::UserCli,
                Reason::new("test independent service").expect("reason"),
            )
            .expect("independent restart");
        assert_eq!(
            *inner.calls.lock().expect("calls"),
            [(ControlAction::Restart, "frontend".to_owned())]
        );
        assert!(observer.plans.lock().expect("plans").is_empty());
    }
}
