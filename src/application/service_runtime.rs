use std::fmt;
use std::sync::{Mutex, MutexGuard};

use crate::domain::{LifecycleState, TransitionError};

/// Result of a serialized start request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    AlreadyRunning,
}

/// Result of a serialized stop request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped,
    AlreadyStopped,
}

/// Per-service process owner and lifecycle serialization boundary.
///
/// The start and stop callbacks execute while the service lock is held. This
/// intentionally prioritizes a single authoritative process tree over
/// concurrent lifecycle throughput.
#[derive(Debug)]
pub struct ServiceRuntime<Process> {
    inner: Mutex<RuntimeState<Process>>,
}

impl<Process> Default for ServiceRuntime<Process> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Process> ServiceRuntime<Process> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(RuntimeState {
                lifecycle: LifecycleState::Stopped,
                process: None,
            }),
        }
    }

    /// Returns the current observable lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceRuntimeError::Poisoned`] if a previous callback
    /// panicked while holding this service's mutation lock.
    pub fn lifecycle(&self) -> Result<LifecycleState, ServiceRuntimeError<()>> {
        Ok(self.lock()?.lifecycle)
    }

    /// Returns whether this runtime currently retains a process owner.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceRuntimeError::Poisoned`] if the service lock is
    /// poisoned.
    pub fn has_process(&self) -> Result<bool, ServiceRuntimeError<()>> {
        Ok(self.lock()?.process.is_some())
    }

    /// Starts the service exactly once while concurrent mutations serialize.
    ///
    /// The callback must return an authoritative process owner implementing
    /// [`crate::application::ManagedProcessTree`]. If spawning fails, the
    /// runtime enters `failed` without retaining an owner.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle transition error, the callback's spawn error, or a
    /// poisoned-lock error.
    pub fn start_with<Error>(
        &self,
        spawn: impl FnOnce() -> Result<Process, Error>,
    ) -> Result<StartOutcome, ServiceRuntimeError<Error>> {
        let mut inner = self.lock()?;
        if inner.process.is_some() {
            return Ok(StartOutcome::AlreadyRunning);
        }

        inner.lifecycle = inner
            .lifecycle
            .transition_to(LifecycleState::Starting)
            .map_err(ServiceRuntimeError::Transition)?;

        match spawn() {
            Ok(process) => {
                inner.process = Some(process);
                inner.lifecycle = inner
                    .lifecycle
                    .transition_to(LifecycleState::Running)
                    .map_err(ServiceRuntimeError::Transition)?;
                Ok(StartOutcome::Started)
            }
            Err(error) => {
                inner.lifecycle = inner
                    .lifecycle
                    .transition_to(LifecycleState::Failed)
                    .map_err(ServiceRuntimeError::Transition)?;
                Err(ServiceRuntimeError::Start(error))
            }
        }
    }

    /// Stops the retained process owner while concurrent mutations serialize.
    ///
    /// The callback borrows the owner instead of consuming it. A failed stop
    /// therefore retains the ownership boundary for inspection and retry.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle transition error, the callback's stop error, or a
    /// poisoned-lock error.
    pub fn stop_with<Error>(
        &self,
        stop: impl FnOnce(&mut Process) -> Result<(), Error>,
    ) -> Result<StopOutcome, ServiceRuntimeError<Error>> {
        let mut inner = self.lock()?;
        if inner.process.is_none() {
            return Ok(StopOutcome::AlreadyStopped);
        }

        inner.lifecycle = inner
            .lifecycle
            .transition_to(LifecycleState::Stopping)
            .map_err(ServiceRuntimeError::Transition)?;

        let stop_result = match inner.process.as_mut() {
            Some(process) => stop(process),
            None => return Ok(StopOutcome::AlreadyStopped),
        };
        match stop_result {
            Ok(()) => {
                inner.process = None;
                inner.lifecycle = inner
                    .lifecycle
                    .transition_to(LifecycleState::Stopped)
                    .map_err(ServiceRuntimeError::Transition)?;
                Ok(StopOutcome::Stopped)
            }
            Err(error) => {
                inner.lifecycle = inner
                    .lifecycle
                    .transition_to(LifecycleState::Failed)
                    .map_err(ServiceRuntimeError::Transition)?;
                Err(ServiceRuntimeError::Stop(error))
            }
        }
    }

    fn lock<Error>(
        &self,
    ) -> Result<MutexGuard<'_, RuntimeState<Process>>, ServiceRuntimeError<Error>> {
        self.inner.lock().map_err(|_| ServiceRuntimeError::Poisoned)
    }
}

#[derive(Debug)]
struct RuntimeState<Process> {
    lifecycle: LifecycleState,
    process: Option<Process>,
}

/// Failure from the per-service lifecycle serialization boundary.
#[derive(Debug, PartialEq, Eq)]
pub enum ServiceRuntimeError<Error> {
    Poisoned,
    Transition(TransitionError),
    Start(Error),
    Stop(Error),
}

impl<Error: fmt::Display> fmt::Display for ServiceRuntimeError<Error> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Poisoned => formatter.write_str("service lifecycle lock is poisoned"),
            Self::Transition(error) => error.fmt(formatter),
            Self::Start(error) => write!(formatter, "service start failed: {error}"),
            Self::Stop(error) => write!(formatter, "service stop failed: {error}"),
        }
    }
}

impl<Error> std::error::Error for ServiceRuntimeError<Error>
where
    Error: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transition(error) => Some(error),
            Self::Start(error) | Self::Stop(error) => Some(error),
            Self::Poisoned => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use crate::domain::LifecycleState;

    use super::{ServiceRuntime, ServiceRuntimeError, StartOutcome, StopOutcome};

    const CONCURRENT_REQUESTS: usize = 16;

    #[test]
    fn concurrent_starts_spawn_exactly_one_process_owner() {
        let runtime = Arc::new(ServiceRuntime::new());
        let barrier = Arc::new(Barrier::new(CONCURRENT_REQUESTS));
        let spawn_count = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..CONCURRENT_REQUESTS)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let barrier = Arc::clone(&barrier);
                let spawn_count = Arc::clone(&spawn_count);
                thread::spawn(move || {
                    barrier.wait();
                    runtime.start_with(|| {
                        spawn_count.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(10));
                        Ok::<_, Infallible>(7_u32)
                    })
                })
            })
            .collect();

        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("start thread did not panic"))
            .collect::<Result<_, ServiceRuntimeError<Infallible>>>()
            .expect("serialized starts succeed");

        assert_eq!(spawn_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == StartOutcome::Started)
                .count(),
            1
        );
        assert_eq!(runtime.lifecycle(), Ok(LifecycleState::Running));
    }

    #[test]
    fn failed_stop_retains_owner_for_a_later_retry() {
        let runtime = ServiceRuntime::new();
        assert_eq!(
            runtime.start_with(|| Ok::<_, &'static str>(7_u32)),
            Ok(StartOutcome::Started)
        );

        let failed = runtime.stop_with(|_| Err::<(), _>("fixture refuses first stop"));
        assert!(matches!(failed, Err(ServiceRuntimeError::Stop(_))));
        assert_eq!(runtime.has_process(), Ok(true));
        assert_eq!(runtime.lifecycle(), Ok(LifecycleState::Failed));

        assert_eq!(
            runtime.stop_with(|_| Ok::<_, &'static str>(())),
            Ok(StopOutcome::Stopped)
        );
        assert_eq!(runtime.has_process(), Ok(false));
        assert_eq!(runtime.lifecycle(), Ok(LifecycleState::Stopped));
    }
}
