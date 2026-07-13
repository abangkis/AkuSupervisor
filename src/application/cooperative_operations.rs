use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::thread;

use serde::Serialize;

use crate::domain::{Actor, Reason};

use super::{CooperativeActionControl, CooperativeActionProgress, CooperativeActionStage};

const RETAINED_OPERATIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CooperativeOperationStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CooperativeOperationSnapshot {
    pub request_id: String,
    pub target: String,
    pub action: String,
    pub actor: Actor,
    pub reason: Reason,
    pub status: CooperativeOperationStatus,
    pub stage: CooperativeActionStage,
    pub relay_action_id: Option<String>,
    pub expected_build_id: Option<String>,
    pub observed_build_id: Option<String>,
    pub error_category: Option<String>,
    pub message: String,
}

#[derive(Clone)]
pub struct CooperativeOperationManager {
    control: Arc<dyn CooperativeActionControl>,
    inner: Arc<Mutex<OperationState>>,
}

impl fmt::Debug for CooperativeOperationManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CooperativeOperationManager")
            .finish_non_exhaustive()
    }
}

impl CooperativeOperationManager {
    #[must_use]
    pub fn new(control: Arc<dyn CooperativeActionControl>) -> Self {
        Self {
            control,
            inner: Arc::new(Mutex::new(OperationState::default())),
        }
    }

    /// Starts or replays the one bounded cooperative operation.
    ///
    /// # Errors
    ///
    /// Returns an idempotency, single-flight, worker, or registry error.
    pub fn begin(
        &self,
        actor: Actor,
        reason: Reason,
        request_id: &str,
    ) -> Result<CooperativeOperationSnapshot, CooperativeOperationError> {
        {
            let mut state = self.lock()?;
            if let Some(existing) = state.operations.get(request_id) {
                if existing.actor == actor && existing.reason == reason {
                    return Ok(existing.clone());
                }
                return Err(CooperativeOperationError::IdempotencyConflict);
            }
            if let Some(active) = &state.active_request_id {
                return Err(CooperativeOperationError::ActionInProgress(active.clone()));
            }
            let operation = CooperativeOperationSnapshot {
                request_id: request_id.to_owned(),
                target: "aku-bridge".to_owned(),
                action: "reload_self".to_owned(),
                actor,
                reason: reason.clone(),
                status: CooperativeOperationStatus::Running,
                stage: CooperativeActionStage::Requested,
                relay_action_id: None,
                expected_build_id: None,
                observed_build_id: None,
                error_category: None,
                message: "authenticated reload_self operation accepted".to_owned(),
            };
            state.active_request_id = Some(request_id.to_owned());
            state.insert(operation);
        }

        let manager = self.clone();
        let request_id_owned = request_id.to_owned();
        if let Err(error) = thread::Builder::new()
            .name("aku-supervisor-cooperative-action".to_owned())
            .spawn(move || manager.execute(actor, reason, &request_id_owned))
        {
            let mut state = self.lock()?;
            state.operations.remove(request_id);
            state.order.retain(|candidate| candidate != request_id);
            state.active_request_id = None;
            return Err(CooperativeOperationError::WorkerSpawn(error.to_string()));
        }
        self.get(request_id)
    }

    /// Returns the latest retained snapshot for a request ID.
    ///
    /// # Errors
    ///
    /// Returns [`CooperativeOperationError::NotFound`] for an unknown request
    /// or a registry error when the shared state is unavailable.
    pub fn get(
        &self,
        request_id: &str,
    ) -> Result<CooperativeOperationSnapshot, CooperativeOperationError> {
        self.lock()?
            .operations
            .get(request_id)
            .cloned()
            .ok_or(CooperativeOperationError::NotFound)
    }

    fn execute(&self, actor: Actor, reason: Reason, request_id: &str) {
        let progress_manager = self.clone();
        let progress_request_id = request_id.to_owned();
        let progress = move |update: CooperativeActionProgress| {
            progress_manager.update_progress(&progress_request_id, update);
        };
        let result = self
            .control
            .reload_aku_bridge(actor, reason, request_id, &progress);
        if let Ok(mut state) = self.inner.lock()
            && let Some(operation) = state.operations.get_mut(request_id)
        {
            match result {
                Ok(outcome) => {
                    operation.status = CooperativeOperationStatus::Completed;
                    operation.stage = CooperativeActionStage::Completed;
                    operation.relay_action_id = outcome.relay_action_id;
                    operation.expected_build_id = outcome.expected_build_id;
                    operation.observed_build_id = outcome.observed_build_id;
                    operation.message = outcome.message;
                }
                Err(error) => {
                    operation.status = CooperativeOperationStatus::Failed;
                    operation.stage = CooperativeActionStage::Failed;
                    if operation.relay_action_id.is_none() {
                        operation.relay_action_id = error.relay_action_id().map(str::to_owned);
                    }
                    if operation.observed_build_id.is_none() {
                        operation.observed_build_id = error.observed_build_id().map(str::to_owned);
                    }
                    operation.error_category = Some(error.category().to_owned());
                    error.message().clone_into(&mut operation.message);
                }
            }
            if state.active_request_id.as_deref() == Some(request_id) {
                state.active_request_id = None;
            }
        }
    }

    fn update_progress(&self, request_id: &str, update: CooperativeActionProgress) {
        if let Ok(mut state) = self.inner.lock()
            && let Some(operation) = state.operations.get_mut(request_id)
        {
            operation.stage = update.stage;
            if update.relay_action_id.is_some() {
                operation.relay_action_id = update.relay_action_id;
            }
            if update.expected_build_id.is_some() {
                operation.expected_build_id = update.expected_build_id;
            }
            if update.observed_build_id.is_some() {
                operation.observed_build_id = update.observed_build_id;
            }
            operation.message = update.message;
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, OperationState>, CooperativeOperationError> {
        self.inner
            .lock()
            .map_err(|_| CooperativeOperationError::Poisoned)
    }
}

#[derive(Debug, Default)]
struct OperationState {
    operations: BTreeMap<String, CooperativeOperationSnapshot>,
    order: VecDeque<String>,
    active_request_id: Option<String>,
}

impl OperationState {
    fn insert(&mut self, operation: CooperativeOperationSnapshot) {
        while self.operations.len() >= RETAINED_OPERATIONS {
            if let Some(oldest) = self.order.pop_front() {
                self.operations.remove(&oldest);
            }
        }
        self.order.push_back(operation.request_id.clone());
        self.operations
            .insert(operation.request_id.clone(), operation);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CooperativeOperationError {
    NotFound,
    IdempotencyConflict,
    ActionInProgress(String),
    WorkerSpawn(String),
    Poisoned,
}

impl fmt::Display for CooperativeOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("cooperative operation was not found"),
            Self::IdempotencyConflict => {
                formatter.write_str("requestId was already used with different input")
            }
            Self::ActionInProgress(request_id) => {
                write!(
                    formatter,
                    "cooperative action is already active: {request_id}"
                )
            }
            Self::WorkerSpawn(message) => {
                write!(formatter, "failed to start operation worker: {message}")
            }
            Self::Poisoned => formatter.write_str("cooperative operation registry is poisoned"),
        }
    }
}

impl std::error::Error for CooperativeOperationError {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::application::{
        CooperativeActionError, CooperativeActionOutcome, CooperativeActionStatus,
    };

    #[derive(Debug, Default)]
    struct BlockingControl {
        calls: AtomicUsize,
        gate: (Mutex<(bool, bool)>, Condvar),
    }

    impl BlockingControl {
        fn wait_until_started(&self) {
            let (lock, condition) = &self.gate;
            let state = lock.lock().expect("gate lock");
            let (_state, timeout) = condition
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
                .expect("gate wait");
            assert!(!timeout.timed_out(), "operation did not start");
        }

        fn release(&self) {
            let (lock, condition) = &self.gate;
            lock.lock().expect("gate lock").1 = true;
            condition.notify_all();
        }
    }

    impl CooperativeActionControl for BlockingControl {
        fn reload_aku_bridge(
            &self,
            _actor: Actor,
            _reason: Reason,
            request_id: &str,
            progress: &(dyn Fn(CooperativeActionProgress) + Send + Sync),
        ) -> Result<CooperativeActionOutcome, CooperativeActionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            progress(CooperativeActionProgress {
                stage: CooperativeActionStage::RelayCreated,
                relay_action_id: Some("relay-1".to_owned()),
                expected_build_id: Some("build-2".to_owned()),
                observed_build_id: None,
                message: "relay created".to_owned(),
            });
            let (lock, condition) = &self.gate;
            let mut state = lock.lock().expect("gate lock");
            state.0 = true;
            condition.notify_all();
            while !state.1 {
                state = condition.wait(state).expect("gate wait");
            }
            Ok(CooperativeActionOutcome {
                target: "aku-bridge".to_owned(),
                action: "reload_self".to_owned(),
                status: CooperativeActionStatus::Completed,
                relay_action_id: Some("relay-1".to_owned()),
                previous_build_id: Some("build-1".to_owned()),
                expected_build_id: Some("build-2".to_owned()),
                observed_build_id: Some("build-2".to_owned()),
                message: format!("completed {request_id}"),
            })
        }
    }

    #[test]
    fn operation_registry_is_non_blocking_idempotent_and_single_flight() {
        let control = Arc::new(BlockingControl::default());
        let manager = CooperativeOperationManager::new(control.clone());
        let reason = Reason::new("load extension build").expect("reason");

        let accepted = manager
            .begin(Actor::Codex, reason.clone(), "request-1")
            .expect("begin operation");
        assert_eq!(accepted.status, CooperativeOperationStatus::Running);
        control.wait_until_started();

        let progress = manager.get("request-1").expect("operation progress");
        assert_eq!(progress.stage, CooperativeActionStage::RelayCreated);
        assert_eq!(progress.relay_action_id.as_deref(), Some("relay-1"));
        assert_eq!(
            manager
                .begin(Actor::Codex, reason.clone(), "request-1")
                .expect("idempotent replay")
                .stage,
            CooperativeActionStage::RelayCreated
        );
        assert_eq!(
            manager.begin(Actor::Codex, reason.clone(), "request-2"),
            Err(CooperativeOperationError::ActionInProgress(
                "request-1".to_owned()
            ))
        );

        control.release();
        for _ in 0..100 {
            if manager.get("request-1").expect("operation").status
                == CooperativeOperationStatus::Completed
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let completed = manager.get("request-1").expect("completed operation");
        assert_eq!(completed.status, CooperativeOperationStatus::Completed);
        assert_eq!(completed.stage, CooperativeActionStage::Completed);
        assert_eq!(control.calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            manager
                .begin(Actor::Codex, reason, "request-1")
                .expect("terminal replay")
                .status,
            CooperativeOperationStatus::Completed
        );
        assert_eq!(control.calls.load(Ordering::Relaxed), 1);
    }
}
