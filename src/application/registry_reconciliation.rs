//! Runtime truth for configuration revision reconciliation.

use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::RegistryReconcileOutcome;

/// The relationship between the configuration on disk and the live registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryReconciliationState {
    Current,
    Pending,
    Deferred,
    Rejected,
}

/// A secret-free snapshot suitable for authenticated diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryReconciliationSnapshot {
    pub state: RegistryReconciliationState,
    pub active_revision: String,
    pub disk_revision: Option<String>,
    pub attempted_at_unix_ms: Option<u64>,
    pub applied_at_unix_ms: Option<u64>,
    pub detail: Option<String>,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

/// Thread-safe owner of the latest reconciliation result.
#[derive(Debug)]
pub struct RegistryReconciliationStatus {
    snapshot: RwLock<RegistryReconciliationSnapshot>,
}

impl RegistryReconciliationStatus {
    #[must_use]
    pub fn new(initial_revision: String) -> Self {
        let now = now_ms();
        Self {
            snapshot: RwLock::new(RegistryReconciliationSnapshot {
                state: RegistryReconciliationState::Current,
                active_revision: initial_revision.clone(),
                disk_revision: Some(initial_revision),
                attempted_at_unix_ms: Some(now),
                applied_at_unix_ms: Some(now),
                detail: None,
                added: Vec::new(),
                updated: Vec::new(),
                removed: Vec::new(),
            }),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> RegistryReconciliationSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn pending(&self, disk_revision: String) {
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.state = RegistryReconciliationState::Pending;
        snapshot.disk_revision = Some(disk_revision);
        snapshot.attempted_at_unix_ms = Some(now_ms());
        snapshot.detail = None;
        clear_changes(&mut snapshot);
    }

    pub fn applied(&self, revision: String, outcome: &RegistryReconcileOutcome) {
        let now = now_ms();
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.state = RegistryReconciliationState::Current;
        snapshot.active_revision.clone_from(&revision);
        snapshot.disk_revision = Some(revision);
        snapshot.attempted_at_unix_ms = Some(now);
        snapshot.applied_at_unix_ms = Some(now);
        snapshot.detail = None;
        snapshot.added.clone_from(&outcome.added);
        snapshot.updated.clone_from(&outcome.updated);
        snapshot.removed.clone_from(&outcome.removed);
    }

    pub fn deferred(&self, disk_revision: Option<String>, detail: &str) {
        self.failed(RegistryReconciliationState::Deferred, disk_revision, detail);
    }

    pub fn rejected(&self, disk_revision: Option<String>, detail: &str) {
        self.failed(RegistryReconciliationState::Rejected, disk_revision, detail);
    }

    fn failed(
        &self,
        state: RegistryReconciliationState,
        disk_revision: Option<String>,
        detail: &str,
    ) {
        let mut snapshot = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.state = state;
        snapshot.disk_revision = disk_revision;
        snapshot.attempted_at_unix_ms = Some(now_ms());
        snapshot.detail = Some(bounded_detail(detail));
        clear_changes(&mut snapshot);
    }
}

fn clear_changes(snapshot: &mut RegistryReconciliationSnapshot) {
    snapshot.added.clear();
    snapshot.updated.clear();
    snapshot.removed.clear();
}

fn bounded_detail(detail: &str) -> String {
    detail.chars().take(512).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_preserve_active_revision_until_apply() {
        let status = RegistryReconciliationStatus::new("sha256:old".to_owned());
        status.pending("sha256:new".to_owned());
        status.deferred(Some("sha256:new".to_owned()), "service must stop");
        let deferred = status.snapshot();
        assert_eq!(deferred.state, RegistryReconciliationState::Deferred);
        assert_eq!(deferred.active_revision, "sha256:old");
        assert_eq!(deferred.disk_revision.as_deref(), Some("sha256:new"));

        status.applied(
            "sha256:new".to_owned(),
            &RegistryReconcileOutcome {
                added: vec!["worker".to_owned()],
                updated: Vec::new(),
                removed: Vec::new(),
            },
        );
        let current = status.snapshot();
        assert_eq!(current.state, RegistryReconciliationState::Current);
        assert_eq!(current.active_revision, "sha256:new");
        assert_eq!(current.added, ["worker"]);
    }
}
