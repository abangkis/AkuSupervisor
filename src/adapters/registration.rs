//! Human-gated, revision-bound service registration persistence.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(test)]
use super::config::CONFIG_VERSION;
use super::config::{ConfigIssue, ServiceConfig, SupervisorConfig};
use super::config_path::{ConfigPathError, resolve_config_path};
use super::control_http::{ControlClientError, client_request, client_request_until};
use super::runtime_token::{RuntimeToken, resolve_token_path};
use crate::application::{RegistryReconciliationSnapshot, RegistryReconciliationState};

const DRAFT_LIFETIME_MS: u64 = 30 * 60 * 1_000;
const REGISTRATION_SCHEMA_VERSION: u32 = 1;
const RECONCILIATION_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const RECONCILIATION_ACK_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationOperation {
    Register,
    Update,
    Unregister,
}

impl fmt::Display for RegistrationOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register => formatter.write_str("register"),
            Self::Update => formatter.write_str("update"),
            Self::Unregister => formatter.write_str("unregister"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Prepared,
    Approved,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistrationDraft {
    pub schema_version: u32,
    pub draft_id: String,
    pub request_id: String,
    pub operation: RegistrationOperation,
    pub service_id: String,
    pub configuration_path: PathBuf,
    pub base_revision: String,
    pub proposed_revision: String,
    pub proposal_hash: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub status: DraftStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at_unix_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_at_unix_ms: Option<u64>,
    pub confirmation_phrase: String,
    pub warnings: Vec<String>,
    pub change_summary: Value,
    pub before_configuration: SupervisorConfig,
    pub after_configuration: SupervisorConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrepareRegistration {
    pub request_id: String,
    pub operation: RegistrationOperation,
    pub service_id: String,
    pub base_revision: String,
    #[serde(default)]
    pub service: Option<ServiceConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitResult {
    pub draft_id: String,
    pub service_id: String,
    pub operation: RegistrationOperation,
    pub previous_revision: String,
    pub configuration_revision: String,
    pub registered_state: &'static str,
    pub auto_started: bool,
    pub registry_reload_required: bool,
    pub registry_reconciliation: RegistrationReconciliationOutcome,
    pub runtime_active_revision: Option<String>,
    pub runtime_disk_revision: Option<String>,
    pub reconciliation_detail: Option<String>,
    pub unrelated_services_restarted: bool,
    pub next_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationReconciliationOutcome {
    Applied,
    Pending,
    Deferred,
    Rejected,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationResult {
    pub valid: bool,
    pub operation: RegistrationOperation,
    pub service_id: String,
    pub current_revision: String,
    pub proposed_revision: Option<String>,
    pub issues: Vec<ConfigIssue>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RegistrationAuthority {
    configuration_path: PathBuf,
    registration_directory: PathBuf,
}

#[must_use]
pub(crate) fn capability_contract() -> Value {
    json!({
        "schemaVersion": REGISTRATION_SCHEMA_VERSION,
        "authority": "human-gated-registration",
        "supportedOperations": ["register", "update", "unregister"],
        "workflow": [
            "Call supervisor_registration_get_schema and supervisor_registration_get_capabilities.",
            "Validate the complete service object with supervisor_registration_validate_service.",
            "Prepare a revision-bound draft with supervisor_registration_prepare_change.",
            "Ask the user to run the returned approvalCommand in a real interactive terminal; its --commit flag completes the mutation after approval.",
            "After the user reports completion, call supervisor_registration_commit_change idempotently to retrieve or confirm the final result; the mutation does not depend on this follow-up."
        ],
        "safety": {
            "approvalAvailableThroughMcp": false,
            "approvalRequiresInteractiveTerminal": true,
            "approvalShowsFullBeforeAndAfterConfiguration": true,
            "approvalCommandCommits": true,
            "agentCommitRequiredAfterApprovalCommand": false,
            "mcpCommitIsIdempotent": true,
            "approvalBoundToProposalHash": true,
            "draftLifetimeMs": DRAFT_LIFETIME_MS,
            "optimisticConcurrency": "exact base revision",
            "atomicConfigurationReplace": true,
            "registerInitialState": "stopped",
            "autoStart": false,
            "automaticRegistryReconciliation": true,
            "boundedRuntimeAcknowledgmentMs": RECONCILIATION_ACK_TIMEOUT.as_millis(),
            "reconciliationOutcomes": ["applied", "pending", "deferred", "rejected", "offline"],
            "unrelatedServicesRestarted": false,
            "updateAndUnregisterRequireObservedStoppedState": true,
            "filesystemValidationScope": "registration validates the full structural contract and only the register/update target paths; launch preflight checks paths for services selected by the runtime launch plan",
            "arbitraryShellCommands": false,
            "secretEnvironmentKeys": "rejected"
        },
        "mcpTools": [
            "supervisor_registration_get_capabilities",
            "supervisor_registration_get_schema",
            "supervisor_registration_validate_service",
            "supervisor_registration_prepare_change",
            "supervisor_registration_get_draft",
            "supervisor_registration_commit_change"
        ],
        "humanCommands": {
            "inspect": "aku-supervisor registration show <draft-id>",
            "approveAndCommit": "aku-supervisor registration approve <draft-id> --commit",
            "approveOnly": "aku-supervisor registration approve <draft-id>"
        }
    })
}

impl RegistrationAuthority {
    /// Opens the registration authority for one existing Supervisor profile.
    ///
    /// # Errors
    ///
    /// Returns a typed configuration or runtime-layout failure.
    pub fn open(explicit_config: Option<PathBuf>) -> Result<Self, RegistrationError> {
        let resolved = resolve_config_path(explicit_config).map_err(RegistrationError::from)?;
        let config = read_structural_config(resolved.path())?;
        ensure_registration_safe_environment(&config)?;
        let token_path = resolve_token_path(resolved.path(), &config.control.token_file);
        let runtime_directory = token_path.parent().ok_or_else(|| {
            RegistrationError::new(
                "runtime_layout_invalid",
                "control token path has no runtime directory",
            )
        })?;
        Ok(Self {
            configuration_path: resolved.path().to_owned(),
            registration_directory: runtime_directory.join("registration"),
        })
    }

    #[must_use]
    pub fn configuration_path(&self) -> &Path {
        &self.configuration_path
    }

    /// Returns the live revision, workflow, tools, commands, and safety policy.
    ///
    /// # Errors
    ///
    /// Returns a configuration read, validation, or fingerprint failure.
    pub fn capabilities(&self) -> Result<Value, RegistrationError> {
        let config = self.current_config()?;
        let revision = config.fingerprint().map_err(config_error)?;
        let mut capabilities = capability_contract();
        let Some(object) = capabilities.as_object_mut() else {
            return Err(RegistrationError::new(
                "registration_contract_invalid",
                "compiled registration capability contract is not an object",
            ));
        };
        object.insert(
            "configurationPath".to_owned(),
            json!(self.configuration_path),
        );
        object.insert("currentRevision".to_owned(), json!(revision));
        Ok(capabilities)
    }

    #[must_use]
    pub fn schema() -> Value {
        service_schema()
    }

    /// Validates a proposed service change without persisting a draft.
    ///
    /// # Errors
    ///
    /// Returns a typed input or current-configuration failure.
    pub fn validate_service(
        &self,
        operation: RegistrationOperation,
        service_id: &str,
        service: Option<ServiceConfig>,
    ) -> Result<ValidationResult, RegistrationError> {
        validate_service_id(service_id)?;
        let current = self.current_config()?;
        let current_revision = current.fingerprint().map_err(config_error)?;
        let (proposed, issues, warnings) =
            validate_registration_proposal(&current, operation, service_id, service)?;
        let proposed_revision = issues
            .is_empty()
            .then(|| proposed.fingerprint().map_err(config_error))
            .transpose()?;
        Ok(ValidationResult {
            valid: issues.is_empty(),
            operation,
            service_id: service_id.to_owned(),
            current_revision,
            proposed_revision,
            issues,
            warnings,
        })
    }

    /// Persists one idempotent, revision-bound registration draft.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, integrity, or persistence failure.
    pub fn prepare(
        &self,
        request: PrepareRegistration,
    ) -> Result<RegistrationDraft, RegistrationError> {
        validate_request_id(&request.request_id)?;
        validate_service_id(&request.service_id)?;
        let _registration_lock = CommitLock::acquire(&self.registration_directory)?;
        let draft_id = draft_id_for_request(&request.request_id);
        let draft_path = self.draft_path(&draft_id)?;
        if draft_path.exists() {
            let existing = self.get_draft(&draft_id)?;
            if request_matches_draft(&request, &existing) {
                return Ok(existing);
            }
            return Err(RegistrationError::new(
                "request_id_conflict",
                "requestId was already used for a different registration proposal",
            ));
        }
        let current = self.current_config()?;
        let current_revision = current.fingerprint().map_err(config_error)?;
        if request.base_revision != current_revision {
            return Err(RegistrationError::with_details(
                "configuration_revision_conflict",
                "baseRevision does not match the current configuration",
                json!({"expected": current_revision, "actual": request.base_revision}),
            ));
        }
        let (proposed, issues, warnings) = validate_registration_proposal(
            &current,
            request.operation,
            &request.service_id,
            request.service,
        )?;
        if !issues.is_empty() {
            return Err(RegistrationError::with_details(
                "service_validation_failed",
                "the proposed configuration is not valid",
                json!({"issues": issues, "warnings": warnings}),
            ));
        }
        let proposed_revision = proposed.fingerprint().map_err(config_error)?;
        let proposal = json!({
            "requestId": request.request_id,
            "operation": request.operation,
            "serviceId": request.service_id,
            "baseRevision": request.base_revision,
            "proposedRevision": proposed_revision,
            "afterConfiguration": proposed,
        });
        let proposal_hash = hash_value(&proposal)?;
        let confirmation_phrase = format!(
            "APPROVE {} {}",
            request.service_id,
            hash_suffix(&proposal_hash)
        );
        let now = now_ms()?;
        let draft = RegistrationDraft {
            schema_version: REGISTRATION_SCHEMA_VERSION,
            draft_id: draft_id.clone(),
            request_id: request.request_id,
            operation: request.operation,
            service_id: request.service_id.clone(),
            configuration_path: self.configuration_path.clone(),
            base_revision: current_revision,
            proposed_revision,
            proposal_hash,
            created_at_unix_ms: now,
            expires_at_unix_ms: now.saturating_add(DRAFT_LIFETIME_MS),
            status: DraftStatus::Prepared,
            approved_at_unix_ms: None,
            committed_at_unix_ms: None,
            confirmation_phrase,
            warnings,
            change_summary: change_summary(
                request.operation,
                &request.service_id,
                &current,
                &proposed,
            ),
            before_configuration: current,
            after_configuration: proposed,
        };
        fs::create_dir_all(self.drafts_directory()).map_err(io_error("draft_directory_create"))?;
        if draft_path.exists() {
            let existing = self.get_draft(&draft_id)?;
            if existing.proposal_hash == draft.proposal_hash {
                return Ok(existing);
            }
            return Err(RegistrationError::new(
                "request_id_conflict",
                "requestId was already used for a different registration proposal",
            ));
        }
        write_json_atomic(&draft_path, &draft)?;
        self.audit("prepared", &draft, Some("registration_mcp"))?;
        Ok(draft)
    }

    /// Loads and integrity-checks a persisted registration draft.
    ///
    /// # Errors
    ///
    /// Returns a lookup, parse, or integrity failure.
    pub fn get_draft(&self, draft_id: &str) -> Result<RegistrationDraft, RegistrationError> {
        validate_draft_id(draft_id)?;
        let path = self.draft_path(draft_id)?;
        let source = fs::read_to_string(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                RegistrationError::new("draft_not_found", "registration draft was not found")
            } else {
                RegistrationError::with_source(
                    "draft_read_failed",
                    "failed to read registration draft",
                    error,
                )
            }
        })?;
        let draft: RegistrationDraft = serde_json::from_str(&source).map_err(|error| {
            RegistrationError::with_source(
                "draft_parse_failed",
                "registration draft is invalid",
                error,
            )
        })?;
        self.verify_draft(&draft)?;
        Ok(draft)
    }

    /// Shows the full proposal and records exact interactive human approval.
    ///
    /// # Errors
    ///
    /// Returns a terminal, expiry, confirmation, integrity, or persistence failure.
    pub fn approve_interactive(
        &self,
        draft_id: &str,
    ) -> Result<RegistrationDraft, RegistrationError> {
        self.approve_interactive_mode(draft_id, false)
    }

    /// Shows the full proposal, records exact human approval, and immediately
    /// commits through the same revision and lifecycle checks used by MCP.
    ///
    /// An already-approved or committed draft is resumed idempotently without
    /// requiring an agent process to finish the transaction.
    ///
    /// # Errors
    ///
    /// Returns a terminal, approval, concurrency, runtime-state, persistence,
    /// or reconciliation failure.
    pub fn approve_and_commit_interactive(
        &self,
        draft_id: &str,
    ) -> Result<CommitResult, RegistrationError> {
        let draft = self.get_draft(draft_id)?;
        if draft.status == DraftStatus::Prepared {
            self.approve_interactive_mode(draft_id, true)?;
        }
        self.commit_with_actor(draft_id, "human_cli")
    }

    fn approve_interactive_mode(
        &self,
        draft_id: &str,
        commit_after_approval: bool,
    ) -> Result<RegistrationDraft, RegistrationError> {
        if !io::stdin().is_terminal() {
            return Err(RegistrationError::new(
                "interactive_terminal_required",
                "approval must run in a real interactive terminal; piped input and MCP approval are forbidden",
            ));
        }
        let draft = self.get_draft(draft_id)?;
        print_approval_review(&draft, commit_after_approval)?;
        print!("\nType exactly: {}\n> ", draft.confirmation_phrase);
        io::stdout()
            .flush()
            .map_err(io_error("approval_prompt_failed"))?;
        let mut confirmation = String::new();
        io::stdin()
            .read_line(&mut confirmation)
            .map_err(io_error("approval_read_failed"))?;
        self.approve_with_confirmation(draft, confirmation.trim())
    }

    fn approve_with_confirmation(
        &self,
        mut draft: RegistrationDraft,
        confirmation: &str,
    ) -> Result<RegistrationDraft, RegistrationError> {
        if draft.status != DraftStatus::Prepared {
            if draft.status == DraftStatus::Approved && confirmation == draft.confirmation_phrase {
                self.audit("approval_recovered", &draft, Some("human_cli"))?;
                return Ok(draft);
            }
            return Err(RegistrationError::new(
                "draft_not_prepared",
                "only a prepared draft can be approved",
            ));
        }
        if now_ms()? > draft.expires_at_unix_ms {
            return Err(RegistrationError::new(
                "draft_expired",
                "registration draft has expired; prepare a new draft",
            ));
        }
        if confirmation != draft.confirmation_phrase {
            return Err(RegistrationError::new(
                "approval_confirmation_mismatch",
                "typed confirmation did not exactly match the hash-bound phrase",
            ));
        }
        draft.status = DraftStatus::Approved;
        draft.approved_at_unix_ms = Some(now_ms()?);
        write_json_atomic(&self.draft_path(&draft.draft_id)?, &draft)?;
        self.audit("approved", &draft, Some("human_cli"))?;
        Ok(draft)
    }

    /// Approves a fixture draft without a terminal.
    ///
    /// This test-only boundary is absent from normal release builds and must
    /// never be exposed by either MCP server.
    #[cfg(feature = "test-fixtures")]
    #[doc(hidden)]
    pub fn approve_fixture(
        &self,
        draft: RegistrationDraft,
    ) -> Result<RegistrationDraft, RegistrationError> {
        let confirmation = draft.confirmation_phrase.clone();
        self.approve_with_confirmation(draft, &confirmation)
    }

    /// Atomically commits one approved draft after revision and state checks.
    ///
    /// # Errors
    ///
    /// Returns an approval, expiry, concurrency, runtime-state, or persistence failure.
    pub fn commit(&self, draft_id: &str) -> Result<CommitResult, RegistrationError> {
        self.commit_with_actor(draft_id, "registration_mcp")
    }

    fn commit_with_actor(
        &self,
        draft_id: &str,
        audit_actor: &'static str,
    ) -> Result<CommitResult, RegistrationError> {
        let _commit_lock = CommitLock::acquire(&self.registration_directory)?;
        let mut draft = self.get_draft(draft_id)?;
        if draft.status == DraftStatus::Committed {
            return Ok(self.commit_result(&draft));
        }
        if draft.status != DraftStatus::Approved {
            return Err(RegistrationError::new(
                "human_approval_required",
                "draft is not approved; commit cannot create its own approval",
            ));
        }
        if now_ms()? > draft.expires_at_unix_ms {
            return Err(RegistrationError::new(
                "draft_expired",
                "approved draft expired before commit; prepare a new draft",
            ));
        }
        let current = self.current_config()?;
        let current_revision = current.fingerprint().map_err(config_error)?;
        if current_revision == draft.proposed_revision {
            draft.status = DraftStatus::Committed;
            draft.committed_at_unix_ms = Some(now_ms()?);
            write_json_atomic(&self.draft_path(&draft.draft_id)?, &draft)?;
            self.audit("commit_recovered", &draft, Some(audit_actor))?;
            return Ok(self.commit_result(&draft));
        }
        if current_revision != draft.base_revision {
            return Err(RegistrationError::with_details(
                "configuration_revision_conflict",
                "configuration changed after this draft was prepared",
                json!({"expected": draft.base_revision, "actual": current_revision}),
            ));
        }
        if matches!(
            draft.operation,
            RegistrationOperation::Update | RegistrationOperation::Unregister
        ) {
            self.require_observed_stopped(&current, &draft.service_id)?;
        }
        let service = match draft.operation {
            RegistrationOperation::Register | RegistrationOperation::Update => Some(
                draft
                    .after_configuration
                    .services
                    .get(&draft.service_id)
                    .cloned()
                    .ok_or_else(|| {
                        RegistrationError::new(
                            "draft_integrity_failed",
                            "registration draft is missing its proposed service",
                        )
                    })?,
            ),
            RegistrationOperation::Unregister => None,
        };
        let (proposed, issues, warnings) =
            validate_registration_proposal(&current, draft.operation, &draft.service_id, service)?;
        if !issues.is_empty() {
            return Err(RegistrationError::with_details(
                "service_validation_failed",
                "the proposed configuration is no longer valid",
                json!({"issues": issues, "warnings": warnings}),
            ));
        }
        if proposed.fingerprint().map_err(config_error)? != draft.proposed_revision {
            return Err(RegistrationError::new(
                "draft_integrity_failed",
                "registration draft no longer matches its operation-bound proposal",
            ));
        }
        write_json_atomic(&self.configuration_path, &proposed)?;
        draft.status = DraftStatus::Committed;
        draft.committed_at_unix_ms = Some(now_ms()?);
        write_json_atomic(&self.draft_path(&draft.draft_id)?, &draft)?;
        self.audit("committed", &draft, Some(audit_actor))?;
        Ok(self.commit_result(&draft))
    }

    fn commit_result(&self, draft: &RegistrationDraft) -> CommitResult {
        let observation =
            self.observe_reconciliation(&draft.after_configuration, &draft.proposed_revision);
        commit_result(draft, observation)
    }

    fn observe_reconciliation(
        &self,
        config: &SupervisorConfig,
        proposed_revision: &str,
    ) -> ReconciliationObservation {
        let token_path = resolve_token_path(&self.configuration_path, &config.control.token_file);
        let token = match RuntimeToken::load(&token_path) {
            Ok(token) => token,
            Err(error) => return ReconciliationObservation::offline(&error.to_string()),
        };
        let address = match format!("{}:{}", config.control.host, config.control.port)
            .parse::<SocketAddr>()
        {
            Ok(address) => address,
            Err(error) => return ReconciliationObservation::offline(&error.to_string()),
        };
        let deadline = Instant::now() + RECONCILIATION_ACK_TIMEOUT;
        let mut last_snapshot = None;
        loop {
            match client_request_until(address, &token, "GET", "/v1/registry", None, deadline) {
                Ok(response) => {
                    let snapshot = response.get("registry").cloned().and_then(|value| {
                        serde_json::from_value::<RegistryReconciliationSnapshot>(value).ok()
                    });
                    let Some(snapshot) = snapshot else {
                        return ReconciliationObservation::offline(
                            "Supervisor returned an invalid registry reconciliation response",
                        );
                    };
                    last_snapshot = Some(snapshot.clone());
                    let matches_disk = snapshot.disk_revision.as_deref() == Some(proposed_revision);
                    if snapshot.state == RegistryReconciliationState::Current
                        && snapshot.active_revision == proposed_revision
                    {
                        return ReconciliationObservation::from_snapshot(
                            RegistrationReconciliationOutcome::Applied,
                            snapshot,
                        );
                    }
                    if matches_disk
                        && matches!(
                            snapshot.state,
                            RegistryReconciliationState::Deferred
                                | RegistryReconciliationState::Rejected
                        )
                    {
                        let outcome = if snapshot.state == RegistryReconciliationState::Deferred {
                            RegistrationReconciliationOutcome::Deferred
                        } else {
                            RegistrationReconciliationOutcome::Rejected
                        };
                        return ReconciliationObservation::from_snapshot(outcome, snapshot);
                    }
                    if Instant::now() >= deadline {
                        return ReconciliationObservation::from_snapshot(
                            RegistrationReconciliationOutcome::Pending,
                            snapshot,
                        );
                    }
                }
                Err(error) => {
                    return last_snapshot.map_or_else(
                        || ReconciliationObservation::offline(&error.to_string()),
                        |snapshot| {
                            ReconciliationObservation::from_snapshot(
                                RegistrationReconciliationOutcome::Pending,
                                snapshot,
                            )
                        },
                    );
                }
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return last_snapshot.map_or_else(
                    || ReconciliationObservation::offline("runtime acknowledgment timed out"),
                    |snapshot| {
                        ReconciliationObservation::from_snapshot(
                            RegistrationReconciliationOutcome::Pending,
                            snapshot,
                        )
                    },
                );
            };
            std::thread::sleep(RECONCILIATION_ACK_POLL.min(remaining));
        }
    }

    fn current_config(&self) -> Result<SupervisorConfig, RegistrationError> {
        let config = read_structural_config(&self.configuration_path)?;
        ensure_registration_safe_environment(&config)?;
        Ok(config)
    }

    fn require_observed_stopped(
        &self,
        config: &SupervisorConfig,
        service_id: &str,
    ) -> Result<(), RegistrationError> {
        let token_path = resolve_token_path(&self.configuration_path, &config.control.token_file);
        let token = RuntimeToken::load(&token_path).map_err(|error| {
            RegistrationError::with_source(
                "runtime_state_unverifiable",
                "cannot verify stopped state because the control token is unavailable",
                error,
            )
        })?;
        let address = format!("{}:{}", config.control.host, config.control.port)
            .parse::<SocketAddr>()
            .map_err(|error| {
                RegistrationError::with_source(
                    "control_address_invalid",
                    "invalid control address",
                    error,
                )
            })?;
        let response = client_request(
            address,
            &token,
            "GET",
            &format!("/v1/services/{service_id}"),
            None,
        )
        .map_err(|error| runtime_state_error(service_id, &error))?;
        let lifecycle = response
            .pointer("/service/lifecycle")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                RegistrationError::new(
                    "runtime_state_invalid",
                    "Supervisor returned no service lifecycle",
                )
            })?;
        if lifecycle != "stopped" {
            return Err(RegistrationError::with_details(
                "service_must_be_stopped",
                "update and unregister require the running Supervisor to observe the service as stopped",
                json!({"serviceId": service_id, "observedLifecycle": lifecycle}),
            ));
        }
        Ok(())
    }

    fn verify_draft(&self, draft: &RegistrationDraft) -> Result<(), RegistrationError> {
        if draft.schema_version != REGISTRATION_SCHEMA_VERSION
            || draft.configuration_path != self.configuration_path
            || draft.draft_id != draft_id_for_request(&draft.request_id)
        {
            return Err(RegistrationError::new(
                "draft_integrity_failed",
                "registration draft identity is invalid",
            ));
        }
        let proposal = json!({
            "requestId": draft.request_id,
            "operation": draft.operation,
            "serviceId": draft.service_id,
            "baseRevision": draft.base_revision,
            "proposedRevision": draft.proposed_revision,
            "afterConfiguration": draft.after_configuration,
        });
        if hash_value(&proposal)? != draft.proposal_hash
            || draft
                .after_configuration
                .fingerprint()
                .map_err(config_error)?
                != draft.proposed_revision
            || draft.confirmation_phrase
                != format!(
                    "APPROVE {} {}",
                    draft.service_id,
                    hash_suffix(&draft.proposal_hash)
                )
        {
            return Err(RegistrationError::new(
                "draft_integrity_failed",
                "registration draft content does not match its proposal hash",
            ));
        }
        Ok(())
    }

    fn drafts_directory(&self) -> PathBuf {
        self.registration_directory.join("drafts")
    }

    fn draft_path(&self, draft_id: &str) -> Result<PathBuf, RegistrationError> {
        validate_draft_id(draft_id)?;
        Ok(self.drafts_directory().join(format!("{draft_id}.json")))
    }

    fn audit(
        &self,
        event: &str,
        draft: &RegistrationDraft,
        actor: Option<&str>,
    ) -> Result<(), RegistrationError> {
        fs::create_dir_all(&self.registration_directory)
            .map_err(io_error("audit_directory_create"))?;
        let record = json!({
            "schemaVersion": REGISTRATION_SCHEMA_VERSION,
            "timestampUnixMs": now_ms()?,
            "event": event,
            "actor": actor,
            "draftId": draft.draft_id,
            "requestId": draft.request_id,
            "operation": draft.operation,
            "serviceId": draft.service_id,
            "baseRevision": draft.base_revision,
            "proposedRevision": draft.proposed_revision,
            "proposalHash": draft.proposal_hash,
            "status": draft.status,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.registration_directory.join("audit.jsonl"))
            .map_err(io_error("registration_audit_open_failed"))?;
        serde_json::to_writer(&mut file, &record).map_err(|error| {
            RegistrationError::with_source(
                "registration_audit_write_failed",
                "failed to serialize registration audit",
                error,
            )
        })?;
        file.write_all(b"\n")
            .and_then(|()| file.flush())
            .map_err(io_error("registration_audit_write_failed"))
    }
}

#[derive(Debug)]
struct CommitLock {
    path: PathBuf,
}

impl CommitLock {
    fn acquire(registration_directory: &Path) -> Result<Self, RegistrationError> {
        fs::create_dir_all(registration_directory)
            .map_err(io_error("commit_lock_directory_create"))?;
        let path = registration_directory.join("commit.lock");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    RegistrationError::new(
                        "registration_transaction_in_progress",
                        "another registration transaction owns the configuration lock; retry after it completes",
                    )
                } else {
                    RegistrationError::with_source(
                        "commit_lock_create_failed",
                        "failed to acquire the registration commit lock",
                        error,
                    )
                }
            })?;
        let lock = Self { path };
        let record = json!({"pid":std::process::id(),"createdAtUnixMs":now_ms()?});
        serde_json::to_writer(&mut file, &record).map_err(json_error)?;
        file.write_all(b"\n")
            .and_then(|()| file.sync_all())
            .map_err(io_error("commit_lock_write_failed"))?;
        Ok(lock)
    }
}

impl Drop for CommitLock {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

fn validate_registration_proposal(
    current: &SupervisorConfig,
    operation: RegistrationOperation,
    service_id: &str,
    service: Option<ServiceConfig>,
) -> Result<(SupervisorConfig, Vec<ConfigIssue>, Vec<String>), RegistrationError> {
    let exists = current.services.contains_key(service_id);
    match operation {
        RegistrationOperation::Register if exists => {
            return Err(RegistrationError::new(
                "service_already_registered",
                "register requires a service ID that does not exist",
            ));
        }
        RegistrationOperation::Update if !exists => {
            return Err(RegistrationError::new(
                "service_not_registered",
                "update requires an existing service ID",
            ));
        }
        RegistrationOperation::Unregister if !exists => {
            return Err(RegistrationError::new(
                "service_not_registered",
                "unregister requires an existing service ID",
            ));
        }
        _ => {}
    }
    let mut proposed = current.clone();
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    match operation {
        RegistrationOperation::Register | RegistrationOperation::Update => {
            let service = service.ok_or_else(|| {
                RegistrationError::new(
                    "service_definition_required",
                    "register and update require a complete service object",
                )
            })?;
            for key in sensitive_environment_keys(&service.environment) {
                issues.push(ConfigIssue {
                    path: format!("services.{service_id}.environment.{key}"),
                    code: "sensitive_environment_key_rejected".to_owned(),
                    message: "registration does not accept likely secrets; use the program's own protected configuration".to_owned(),
                });
            }
            if !service.environment.is_empty() {
                warnings.push("Environment values are persisted in plain JSON and displayed during approval; use only non-sensitive development values.".to_owned());
            }
            proposed.services.insert(service_id.to_owned(), service);
        }
        RegistrationOperation::Unregister => {
            if service.is_some() {
                return Err(RegistrationError::new(
                    "service_definition_forbidden",
                    "unregister must omit the service object",
                ));
            }
            proposed.services.remove(service_id);
        }
    }
    issues.extend(proposed.structural_contract_issues());
    if operation != RegistrationOperation::Unregister {
        for issue in proposed.selected_service_filesystem_issues(&[service_id.to_owned()]) {
            if !issues
                .iter()
                .any(|existing| existing.path == issue.path && existing.code == issue.code)
            {
                issues.push(issue);
            }
        }
    }
    Ok((proposed, issues, warnings))
}

fn draft_id_for_request(request_id: &str) -> String {
    let request_hash = format!("{:x}", Sha256::digest(request_id.as_bytes()));
    format!("registration-{}", &request_hash[..20])
}

fn request_matches_draft(request: &PrepareRegistration, draft: &RegistrationDraft) -> bool {
    if request.request_id != draft.request_id
        || request.operation != draft.operation
        || request.service_id != draft.service_id
        || request.base_revision != draft.base_revision
    {
        return false;
    }
    match request.operation {
        RegistrationOperation::Register | RegistrationOperation::Update => {
            request.service.as_ref().is_some_and(|service| {
                draft.after_configuration.services.get(&request.service_id) == Some(service)
            })
        }
        RegistrationOperation::Unregister => {
            request.service.is_none()
                && !draft
                    .after_configuration
                    .services
                    .contains_key(&request.service_id)
        }
    }
}

fn sensitive_environment_keys(environment: &BTreeMap<String, String>) -> Vec<String> {
    const MARKERS: [&str; 7] = [
        "SECRET",
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
    ];
    environment
        .keys()
        .filter(|key| {
            let upper = key.to_ascii_uppercase();
            MARKERS.iter().any(|marker| upper.contains(marker))
        })
        .cloned()
        .collect()
}

fn ensure_registration_safe_environment(
    config: &SupervisorConfig,
) -> Result<(), RegistrationError> {
    let unsafe_keys = config
        .services
        .iter()
        .flat_map(|(service_id, service)| {
            sensitive_environment_keys(&service.environment)
                .into_iter()
                .map(move |key| format!("services.{service_id}.environment.{key}"))
        })
        .collect::<Vec<_>>();
    if unsafe_keys.is_empty() {
        Ok(())
    } else {
        Err(RegistrationError::with_details(
            "sensitive_environment_key_rejected",
            "registration is disabled while the selected profile contains likely secrets",
            json!({"paths":unsafe_keys}),
        ))
    }
}

fn change_summary(
    operation: RegistrationOperation,
    service_id: &str,
    before: &SupervisorConfig,
    after: &SupervisorConfig,
) -> Value {
    json!({
        "operation": operation,
        "serviceId": service_id,
        "beforeService": before.services.get(service_id),
        "afterService": after.services.get(service_id),
        "serviceCountBefore": before.services.len(),
        "serviceCountAfter": after.services.len(),
    })
}

#[derive(Debug)]
struct ReconciliationObservation {
    outcome: RegistrationReconciliationOutcome,
    active_revision: Option<String>,
    disk_revision: Option<String>,
    detail: Option<String>,
}

impl ReconciliationObservation {
    fn from_snapshot(
        outcome: RegistrationReconciliationOutcome,
        snapshot: RegistryReconciliationSnapshot,
    ) -> Self {
        Self {
            outcome,
            active_revision: Some(snapshot.active_revision),
            disk_revision: snapshot.disk_revision,
            detail: snapshot.detail,
        }
    }

    fn offline(detail: &str) -> Self {
        Self {
            outcome: RegistrationReconciliationOutcome::Offline,
            active_revision: None,
            disk_revision: None,
            detail: Some(detail.chars().take(512).collect()),
        }
    }
}

fn commit_result(
    draft: &RegistrationDraft,
    observation: ReconciliationObservation,
) -> CommitResult {
    let present = draft
        .after_configuration
        .services
        .contains_key(&draft.service_id);
    CommitResult {
        draft_id: draft.draft_id.clone(),
        service_id: draft.service_id.clone(),
        operation: draft.operation,
        previous_revision: draft.base_revision.clone(),
        configuration_revision: draft.proposed_revision.clone(),
        registered_state: if present { "stopped" } else { "unregistered" },
        auto_started: false,
        registry_reload_required: false,
        registry_reconciliation: observation.outcome,
        runtime_active_revision: observation.active_revision,
        runtime_disk_revision: observation.disk_revision,
        reconciliation_detail: observation.detail,
        unrelated_services_restarted: false,
        next_command: present.then(|| {
            format!(
                "aku-supervisor start {} --actor user --reason \"start registered service\"",
                draft.service_id
            )
        }),
    }
}

fn print_approval_review(
    draft: &RegistrationDraft,
    commit_after_approval: bool,
) -> Result<(), RegistrationError> {
    println!("AkuSupervisor registration approval\n");
    println!("Draft:            {}", draft.draft_id);
    println!("Operation:        {}", draft.operation);
    println!("Service:          {}", draft.service_id);
    println!("Configuration:    {}", draft.configuration_path.display());
    println!("Base revision:    {}", draft.base_revision);
    println!("Proposed revision:{}", draft.proposed_revision);
    println!("Proposal hash:    {}", draft.proposal_hash);
    println!("Expires at (ms):  {}", draft.expires_at_unix_ms);
    if !draft.warnings.is_empty() {
        println!("\nWARNINGS:");
        for warning in &draft.warnings {
            println!("- {warning}");
        }
    }
    println!(
        "\nCHANGE SUMMARY:\n{}",
        serde_json::to_string_pretty(&draft.change_summary).map_err(json_error)?
    );
    println!(
        "\nFULL CURRENT CONFIGURATION:\n{}",
        serde_json::to_string_pretty(&draft.before_configuration).map_err(json_error)?
    );
    println!(
        "\nFULL PROPOSED CONFIGURATION:\n{}",
        serde_json::to_string_pretty(&draft.after_configuration).map_err(json_error)?
    );
    if commit_after_approval {
        println!(
            "\nAFTER CONFIRMATION: this command will immediately commit the exact proposed configuration. The service will not auto-start. Revision and stopped-state checks still apply."
        );
    } else {
        println!(
            "\nAPPROVAL ONLY: the configuration will remain unchanged until a separate commit. Use --commit to complete approval and mutation in one human command."
        );
    }
    Ok(())
}

fn read_structural_config(path: &Path) -> Result<SupervisorConfig, RegistrationError> {
    let source = fs::read_to_string(path).map_err(|error| {
        RegistrationError::with_source(
            "configuration_read_failed",
            &format!("failed to read {}", path.display()),
            error,
        )
    })?;
    let config = SupervisorConfig::parse_json(&source).map_err(config_error)?;
    config.validate_structure().map_err(config_error)?;
    Ok(config)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), RegistrationError> {
    let parent = path.parent().ok_or_else(|| {
        RegistrationError::new(
            "atomic_write_path_invalid",
            "target path has no parent directory",
        )
    })?;
    fs::create_dir_all(parent).map_err(io_error("atomic_write_directory_create"))?;
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    let suffix = format!("{}.{}.tmp", std::process::id(), now_ms()?);
    let temp = parent.join(format!(
        ".{}.{suffix}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(io_error("atomic_write_create"))?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(io_error("atomic_write_flush"))?;
        crate::platform::atomic_replace_file(&temp, path).map_err(|error| {
            RegistrationError::with_source(
                "atomic_replace_failed",
                "failed to atomically replace registration data",
                error,
            )
        })
    })();
    if result.is_err() {
        fs::remove_file(&temp).ok();
    }
    result
}

fn runtime_state_error(service_id: &str, error: &ControlClientError) -> RegistrationError {
    RegistrationError::with_details(
        "runtime_state_unverifiable",
        "cannot prove that the target service is stopped; keep AkuSupervisor running and stop the service first",
        json!({"serviceId": service_id, "cause": error.to_string()}),
    )
}

fn hash_value(value: &Value) -> Result<String, RegistrationError> {
    let bytes = serde_json::to_vec(value).map_err(json_error)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn hash_suffix(hash: &str) -> &str {
    let length = hash.len();
    &hash[length.saturating_sub(12)..]
}

fn now_ms() -> Result<u64, RegistrationError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            RegistrationError::with_source(
                "system_clock_invalid",
                "system clock is before UNIX epoch",
                error,
            )
        })?
        .as_millis();
    u64::try_from(millis).map_err(|error| {
        RegistrationError::with_source(
            "system_clock_invalid",
            "system clock exceeds supported range",
            error,
        )
    })
}

fn validate_service_id(value: &str) -> Result<(), RegistrationError> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Ok(())
    } else {
        Err(RegistrationError::new(
            "service_id_invalid",
            "serviceId must contain lowercase ASCII letters, digits, or hyphens",
        ))
    }
}

fn validate_request_id(value: &str) -> Result<(), RegistrationError> {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(RegistrationError::new(
            "request_id_invalid",
            "requestId must be 1-128 URL-safe ASCII characters",
        ))
    }
}

fn validate_draft_id(value: &str) -> Result<(), RegistrationError> {
    if value.strip_prefix("registration-").is_some_and(|suffix| {
        suffix.len() == 20 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        Ok(())
    } else {
        Err(RegistrationError::new(
            "draft_id_invalid",
            "draft ID is invalid",
        ))
    }
}

fn service_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "AkuSupervisor service registration",
        "type": "object",
        "additionalProperties": false,
        "required": ["label", "cwd", "command", "health", "restartPolicy", "shutdownGraceMs"],
        "properties": {
            "label": {"type":"string", "minLength":1, "description":"Human-readable label."},
            "cwd": {"type":"string", "description":"Absolute working-directory path. It must currently exist when this service is registered or updated; unrelated service paths are not checked during registration."},
            "command": {"type":"string", "description":"Absolute executable or command-wrapper path; never a shell expression. It must currently exist when this service is registered or updated. Start preflight checks services selected by the runtime launch plan."},
            "args": {"type":"array", "items":{"type":"string"}, "default":[], "description":"Fixed argv entries, kept separate from command."},
            "environment": {"type":"object", "additionalProperties":{"type":"string"}, "default":{}, "description":"Non-sensitive fixed overrides only. Likely secret keys are rejected."},
            "dependsOn": {"type":"array", "items":{"type":"string", "pattern":"^[a-z0-9-]+$"}, "uniqueItems":true, "default":[], "description":"Managed service IDs that AkuSupervisor starts first. References, duplicates, self-dependencies, and cycles are rejected."},
            "startupPrerequisites": {
                "type":"array",
                "maxItems":8,
                "default":[],
                "description":"Bounded external loopback readiness checks evaluated before spawning. These checks do not start or manage the dependency.",
                "items":{"oneOf":[
                    {"type":"object", "additionalProperties":false, "required":["type","host","port","timeoutMs","startupDeadlineMs"], "properties":{"type":{"const":"tcp-connect"},"host":{"type":"string","description":"Explicit loopback IP."},"port":{"type":"integer","minimum":1,"maximum":65535},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1}}}
                ]}
            },
            "health": {"oneOf":[
                {"type":"object", "additionalProperties":false, "required":["type"], "properties":{"type":{"const":"process"}}},
                {"type":"object", "additionalProperties":false, "required":["type","host","port","timeoutMs","startupDeadlineMs"], "properties":{"type":{"const":"tcp-connect"},"host":{"type":"string","description":"Explicit loopback IP."},"port":{"type":"integer","minimum":1,"maximum":65535},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1}}},
                {"type":"object", "additionalProperties":false, "required":["type","url","expectedStatus","timeoutMs","startupDeadlineMs"], "properties":{"type":{"const":"http-status"},"url":{"type":"string","description":"Loopback http:// URL."},"expectedStatus":{"type":"integer","minimum":100,"maximum":599},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1}}},
                {"type":"object", "additionalProperties":false, "required":["type","url","timeoutMs","startupDeadlineMs","expect"], "properties":{"type":{"const":"http-json"},"url":{"type":"string","description":"Loopback http:// URL."},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1},"pathMode":{"type":"string","enum":["shallow","json-pointer"],"default":"shallow","description":"Omit for top-level scalar fields, or use json-pointer for RFC 6901 nested lookup."},"expect":{"type":"object","minProperties":1,"description":"Stable readiness entries; any mismatch fails health. Values must be scalar in shallow mode and may be any JSON value in json-pointer mode."},"observe":{"type":"array","uniqueItems":true,"items":{"type":"string"},"description":"Optional JSON fields exposed as observed metadata. They never affect health."}}}
            ]},
            "ports": {"type":"array", "items":{"type":"integer","minimum":1,"maximum":65535}, "uniqueItems":true, "default":[]},
            "restartPolicy": {"type":"string", "enum":["manual","on-failure"]},
            "shutdownGraceMs": {"type":"integer", "minimum":1}
        },
        "examples": [{
            "label":"Example API", "cwd":"C:\\absolute\\project", "command":"C:\\absolute\\runtime\\server.exe",
            "args":["--port","8090"], "environment":{}, "dependsOn":[],
            "startupPrerequisites":[{"type":"tcp-connect","host":"127.0.0.1","port":5432,"timeoutMs":1000,"startupDeadlineMs":30000}],
            "health":{"type":"http-status","url":"http://127.0.0.1:8090/health","expectedStatus":200,"timeoutMs":3000,"startupDeadlineMs":20000},
            "ports":[8090], "restartPolicy":"manual", "shutdownGraceMs":5000
        }]
    })
}

fn config_error(error: super::config::ConfigError) -> RegistrationError {
    RegistrationError::with_source("configuration_invalid", "configuration is invalid", error)
}

fn json_error(error: serde_json::Error) -> RegistrationError {
    RegistrationError::with_source(
        "json_serialization_failed",
        "failed to serialize registration data",
        error,
    )
}

fn io_error(code: &'static str) -> impl FnOnce(io::Error) -> RegistrationError {
    move |error| RegistrationError::with_source(code, "registration persistence failed", error)
}

#[derive(Debug)]
pub struct RegistrationError {
    code: &'static str,
    message: String,
    details: Option<Value>,
}

impl RegistrationError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    fn with_source(code: &'static str, message: &str, source: impl fmt::Display) -> Self {
        Self::new(code, format!("{message}: {source}"))
    }

    fn with_details(code: &'static str, message: impl Into<String>, details: Value) -> Self {
        Self {
            code,
            message: message.into(),
            details: Some(details),
        }
    }

    pub(crate) fn input(message: impl Into<String>) -> Self {
        Self::new("invalid_tool_arguments", message)
    }

    pub(crate) fn serialization(error: serde_json::Error) -> Self {
        Self::with_source(
            "json_serialization_failed",
            "failed to serialize registration output",
            error,
        )
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }

    #[must_use]
    pub fn structured(&self) -> Value {
        json!({"code": self.code, "message": self.message, "details": self.details})
    }
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RegistrationError {}

impl From<ConfigPathError> for RegistrationError {
    fn from(error: ConfigPathError) -> Self {
        Self::with_source(
            "configuration_path_failed",
            "failed to resolve configuration",
            error,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::adapters::config::{
        ControlConfig, CooperativeActionsConfig, HealthCheck, McpConfig, ObservabilityConfig,
        RestartPolicy,
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn register_requires_human_approval_then_commits_stopped() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let base_revision = fixture.config().fingerprint().expect("base revision");
        let request = PrepareRegistration {
            request_id: "register-worker-1".to_owned(),
            operation: RegistrationOperation::Register,
            service_id: "worker".to_owned(),
            base_revision,
            service: Some(fixture.service("Worker", 0)),
        };
        let draft = authority.prepare(request.clone()).expect("prepare");

        assert_eq!(draft.status, DraftStatus::Prepared);
        assert_eq!(
            authority
                .prepare(request.clone())
                .expect("idempotent prepare")
                .draft_id,
            draft.draft_id
        );
        let error = authority
            .commit(&draft.draft_id)
            .expect_err("unapproved commit fails");
        assert_eq!(error.code(), "human_approval_required");

        let approved = authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("approve");
        assert_eq!(approved.status, DraftStatus::Approved);
        let commit = authority.commit(&draft.draft_id).expect("commit");
        assert_eq!(commit.registered_state, "stopped");
        assert!(!commit.auto_started);
        assert!(!commit.registry_reload_required);
        assert_eq!(
            commit.registry_reconciliation,
            RegistrationReconciliationOutcome::Offline
        );
        assert!(!commit.unrelated_services_restarted);
        assert!(fixture.config().services.contains_key("worker"));

        let recovered = authority
            .commit(&draft.draft_id)
            .expect("idempotent commit");
        assert_eq!(
            recovered.configuration_revision,
            commit.configuration_revision
        );
        assert_eq!(
            authority
                .prepare(request)
                .expect("idempotent prepare after commit")
                .status,
            DraftStatus::Committed
        );
    }

    #[test]
    fn human_completed_transaction_is_audited_as_human_cli() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let draft = authority
            .prepare(PrepareRegistration {
                request_id: "human-commit-worker".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision: fixture.config().fingerprint().expect("base revision"),
                service: Some(fixture.service("Worker", 0)),
            })
            .expect("prepare");
        authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("approve");

        let committed = authority
            .commit_with_actor(&draft.draft_id, "human_cli")
            .expect("human commit");
        assert_eq!(committed.registered_state, "stopped");
        assert!(fixture.config().services.contains_key("worker"));

        let audit = fs::read_to_string(fixture.directory.join(".runtime/registration/audit.jsonl"))
            .expect("registration audit");
        let committed_event = audit
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"] == "committed")
            .expect("committed audit event");
        assert_eq!(committed_event["actor"], "human_cli");

        let confirmed = authority
            .commit(&draft.draft_id)
            .expect("MCP confirmation is idempotent");
        assert_eq!(
            confirmed.configuration_revision,
            committed.configuration_revision
        );
    }

    #[test]
    fn stale_revision_and_request_id_reuse_fail_closed() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let base_revision = fixture.config().fingerprint().expect("base revision");
        let request = PrepareRegistration {
            request_id: "same-request".to_owned(),
            operation: RegistrationOperation::Register,
            service_id: "worker".to_owned(),
            base_revision: base_revision.clone(),
            service: Some(fixture.service("Worker", 0)),
        };
        authority.prepare(request).expect("first prepare");
        let conflicting = PrepareRegistration {
            request_id: "same-request".to_owned(),
            operation: RegistrationOperation::Register,
            service_id: "other".to_owned(),
            base_revision,
            service: Some(fixture.service("Other", 0)),
        };
        let error = authority
            .prepare(conflicting)
            .expect_err("request ID conflict");
        assert_eq!(error.code(), "request_id_conflict");

        let stale = PrepareRegistration {
            request_id: "stale-request".to_owned(),
            operation: RegistrationOperation::Register,
            service_id: "stale".to_owned(),
            base_revision: "sha256:deadbeef".to_owned(),
            service: Some(fixture.service("Stale", 0)),
        };
        let error = authority.prepare(stale).expect_err("stale revision");
        assert_eq!(error.code(), "configuration_revision_conflict");
    }

    #[test]
    fn registration_checks_only_the_target_files_and_keeps_strict_diagnostics() {
        let fixture = Fixture::new();
        let unrelated_executable = fixture.directory.join("unrelated-api.exe");
        fs::write(&unrelated_executable, b"fixture executable").expect("write fixture executable");
        let missing_idle_executable = fixture.directory.join("missing-idle.exe");
        let missing_dependency_executable = fixture.directory.join("missing-dependency.exe");
        let mut config = fixture.config();
        config.services.get_mut("api").expect("api service").command = unrelated_executable.clone();
        for service_id in ["idle-one", "idle-two"] {
            let mut idle_service = fixture.service(service_id, 0);
            idle_service.command = missing_idle_executable.clone();
            config.services.insert(service_id.to_owned(), idle_service);
        }
        let mut dependency = fixture.service("Missing dependency", 0);
        dependency.command = missing_dependency_executable;
        config.services.insert("dependency".to_owned(), dependency);
        fixture.write_config(&config);

        let authority = fixture.authority();
        assert!(authority.capabilities().is_ok());
        let base_revision = fixture.config().fingerprint().expect("base revision");
        let mut worker = fixture.service("Worker", 0);
        worker.depends_on = vec!["dependency".to_owned()];
        let validation = authority
            .validate_service(
                RegistrationOperation::Register,
                "worker",
                Some(worker.clone()),
            )
            .expect("validate registration");
        assert!(
            validation.valid,
            "unexpected issues: {:?}",
            validation.issues
        );

        let draft = authority
            .prepare(PrepareRegistration {
                request_id: "missing-idle-files".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision,
                service: Some(worker),
            })
            .expect("prepare with unrelated and dependency executables missing");
        assert_eq!(
            authority
                .get_draft(&draft.draft_id)
                .expect("read draft despite missing idle executables")
                .draft_id,
            draft.draft_id
        );
        authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("fixture approval");

        fs::remove_file(&unrelated_executable).expect("remove unrelated executable after approval");
        let commit = authority
            .commit(&draft.draft_id)
            .expect("commit despite unrelated and dependency paths being unavailable");
        assert_eq!(commit.registered_state, "stopped");
        assert!(fixture.config().services.contains_key("worker"));

        let strict_issues = fixture.config().validation_issues();
        for (path, code) in [
            ("services.api.command", "command_missing"),
            ("services.idle-one.command", "command_missing"),
            ("services.idle-two.command", "command_missing"),
            ("services.dependency.command", "command_missing"),
        ] {
            assert!(
                strict_issues
                    .iter()
                    .any(|issue| issue.path == path && issue.code == code),
                "strict diagnostics should retain {path} {code}"
            );
        }
    }

    #[test]
    fn registration_rejects_missing_target_paths_and_structural_conflicts() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let base_revision = fixture.config().fingerprint().expect("base revision");
        let mut missing_target = fixture.service("Missing target", 0);
        missing_target.cwd = fixture.directory.join("missing-working-directory");
        missing_target.command = fixture.directory.join("missing-target.exe");

        let validation = authority
            .validate_service(
                RegistrationOperation::Register,
                "worker",
                Some(missing_target.clone()),
            )
            .expect("validation result");
        assert!(!validation.valid);
        for (path, code) in [
            ("services.worker.cwd", "cwd_missing"),
            ("services.worker.command", "command_missing"),
        ] {
            assert!(
                validation
                    .issues
                    .iter()
                    .any(|issue| issue.path == path && issue.code == code),
                "target validation should include {path} {code}"
            );
        }
        let error = authority
            .prepare(PrepareRegistration {
                request_id: "missing-target-paths".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision: base_revision.clone(),
                service: Some(missing_target),
            })
            .expect_err("missing target files must block draft preparation");
        assert_eq!(error.code(), "service_validation_failed");

        let mut conflicting_target = fixture.service("Port conflict", 0);
        conflicting_target.ports = vec![fixture.config().control.port];
        let validation = authority
            .validate_service(
                RegistrationOperation::Register,
                "worker",
                Some(conflicting_target.clone()),
            )
            .expect("validation result");
        assert!(
            validation
                .issues
                .iter()
                .any(|issue| issue.code == "declared_port_conflict"),
            "structural port conflicts remain registration errors"
        );
        let error = authority
            .prepare(PrepareRegistration {
                request_id: "structural-port-conflict".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision,
                service: Some(conflicting_target),
            })
            .expect_err("structural conflict must block draft preparation");
        assert_eq!(error.code(), "service_validation_failed");
    }

    #[test]
    fn update_and_unregister_can_repair_a_broken_old_target() {
        let fixture = Fixture::new();
        let mut config = fixture.config();
        let old_target = config.services.get_mut("api").expect("api service");
        old_target.cwd = fixture.directory.join("missing-old-cwd");
        old_target.command = fixture.directory.join("missing-old-command.exe");
        config
            .services
            .insert("spare".to_owned(), fixture.service("Spare", 0));
        fixture.write_config(&config);

        let authority = fixture.authority();
        let base_revision = fixture.config().fingerprint().expect("base revision");
        let updated = authority
            .validate_service(
                RegistrationOperation::Update,
                "api",
                Some(fixture.service("Repaired API", 0)),
            )
            .expect("update validation");
        assert!(updated.valid, "unexpected issues: {:?}", updated.issues);
        authority
            .prepare(PrepareRegistration {
                request_id: "repair-broken-target".to_owned(),
                operation: RegistrationOperation::Update,
                service_id: "api".to_owned(),
                base_revision: base_revision.clone(),
                service: Some(fixture.service("Repaired API", 0)),
            })
            .expect("prepare update for broken old target");

        let unregistered = authority
            .validate_service(RegistrationOperation::Unregister, "api", None)
            .expect("unregister validation");
        assert!(
            unregistered.valid,
            "unregister must not require paths for the removed target: {:?}",
            unregistered.issues
        );
        authority
            .prepare(PrepareRegistration {
                request_id: "remove-broken-target".to_owned(),
                operation: RegistrationOperation::Unregister,
                service_id: "api".to_owned(),
                base_revision,
                service: None,
            })
            .expect("prepare unregister for broken old target");
    }

    #[test]
    fn unregister_preserves_dependency_and_nonempty_registry_constraints() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        for with_dependent in [false, true] {
            let mut config = fixture.config();
            if with_dependent {
                let mut dependent = fixture.service("Dependent", 0);
                dependent.depends_on = vec!["api".to_owned()];
                config.services.insert("dependent".to_owned(), dependent);
                fixture.write_config(&config);
            }
            let before = fs::read(&fixture.config_path).expect("fixture configuration");
            let expected_code = if with_dependent {
                "service_dependency_unknown"
            } else {
                "services_empty"
            };
            let validation = authority
                .validate_service(RegistrationOperation::Unregister, "api", None)
                .expect("unregister validation");
            assert!(!validation.valid);
            assert!(
                validation
                    .issues
                    .iter()
                    .any(|issue| issue.code == expected_code)
            );
            let error = authority
                .prepare(PrepareRegistration {
                    request_id: format!("unregister-constraint-{with_dependent}"),
                    operation: RegistrationOperation::Unregister,
                    service_id: "api".to_owned(),
                    base_revision: config.fingerprint().expect("current revision"),
                    service: None,
                })
                .expect_err("unregister cannot break registry structure");
            assert_eq!(error.code(), "service_validation_failed");
            assert_eq!(
                fs::read(&fixture.config_path).expect("unchanged fixture"),
                before
            );
        }
    }

    #[test]
    fn approval_expiry_confirmation_and_tamper_checks_leave_config_unchanged() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let original_revision = fixture.config().fingerprint().expect("base revision");
        let draft = authority
            .prepare(PrepareRegistration {
                request_id: "approval-boundaries".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision: original_revision.clone(),
                service: Some(fixture.service("Worker", 0)),
            })
            .expect("prepare");

        let error = authority
            .approve_with_confirmation(draft.clone(), "APPROVE worker incorrect")
            .expect_err("wrong confirmation must fail");
        assert_eq!(error.code(), "approval_confirmation_mismatch");
        let mut expired = draft.clone();
        expired.expires_at_unix_ms = 0;
        let error = authority
            .approve_with_confirmation(expired, &draft.confirmation_phrase)
            .expect_err("expired approval must fail");
        assert_eq!(error.code(), "draft_expired");
        assert_eq!(
            authority
                .get_draft(&draft.draft_id)
                .expect("wrong and expired attempts leave prepared draft intact")
                .status,
            DraftStatus::Prepared
        );

        let approved = authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("approve with exact phrase");
        let mut expired_approved = approved.clone();
        expired_approved.expires_at_unix_ms = 0;
        write_json_atomic(
            &authority.draft_path(&draft.draft_id).expect("draft path"),
            &expired_approved,
        )
        .expect("persist expired approved fixture");
        let error = authority
            .commit(&draft.draft_id)
            .expect_err("expired approved draft must not commit");
        assert_eq!(error.code(), "draft_expired");

        let mut tampered = expired_approved;
        tampered
            .after_configuration
            .services
            .get_mut("worker")
            .expect("proposed worker")
            .label = "tampered after approval".to_owned();
        write_json_atomic(
            &authority.draft_path(&draft.draft_id).expect("draft path"),
            &tampered,
        )
        .expect("persist tampered fixture draft");
        let error = authority
            .get_draft(&draft.draft_id)
            .expect_err("tampered proposal must fail integrity verification");
        assert_eq!(error.code(), "draft_integrity_failed");
        let error = authority
            .commit(&draft.draft_id)
            .expect_err("tampered proposal must not commit");
        assert_eq!(error.code(), "draft_integrity_failed");
        assert_eq!(
            fixture.config().fingerprint().expect("final revision"),
            original_revision
        );
    }

    #[test]
    fn approved_registration_rechecks_target_files_and_retry_recovers() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let target_working_directory = fixture.directory.join("temporary-worker-cwd");
        let target_executable = fixture.directory.join("temporary-worker.exe");
        fs::create_dir(&target_working_directory).expect("create target working directory");
        fs::write(&target_executable, b"fixture executable").expect("write target executable");
        let mut worker = fixture.service("Worker", 0);
        worker.cwd = target_working_directory.clone();
        worker.command = target_executable.clone();
        let draft = authority
            .prepare(PrepareRegistration {
                request_id: "target-file-recheck".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision: fixture.config().fingerprint().expect("base revision"),
                service: Some(worker),
            })
            .expect("prepare");
        authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("fixture approval");

        fs::remove_file(&target_executable).expect("remove approved target executable");
        fs::remove_dir(&target_working_directory)
            .expect("remove approved target working directory");
        let error = authority
            .commit(&draft.draft_id)
            .expect_err("fresh commit must reject missing target paths");
        assert_eq!(error.code(), "service_validation_failed");
        let issues = error
            .details()
            .and_then(|details| details["issues"].as_array())
            .expect("structured target issues");
        assert!(issues.iter().any(|issue| {
            issue["path"] == "services.worker.command" && issue["code"] == "command_missing"
        }));
        assert!(issues.iter().any(|issue| {
            issue["path"] == "services.worker.cwd" && issue["code"] == "cwd_missing"
        }));
        assert_eq!(
            authority
                .get_draft(&draft.draft_id)
                .expect("approved draft remains readable")
                .status,
            DraftStatus::Approved
        );
        assert!(!fixture.config().services.contains_key("worker"));

        fs::create_dir(&target_working_directory).expect("restore target working directory");
        fs::write(&target_executable, b"fixture executable").expect("restore target executable");
        authority
            .commit(&draft.draft_id)
            .expect("the same approval can be retried while it remains valid");
        assert!(fixture.config().services.contains_key("worker"));
    }

    #[test]
    fn interrupted_commit_recovery_does_not_recheck_disappeared_paths() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let target_executable = fixture.directory.join("interrupted-worker.exe");
        fs::write(&target_executable, b"fixture executable").expect("write target executable");
        let mut worker = fixture.service("Worker", 0);
        worker.command = target_executable.clone();
        let draft = authority
            .prepare(PrepareRegistration {
                request_id: "interrupted-commit-recovery".to_owned(),
                operation: RegistrationOperation::Register,
                service_id: "worker".to_owned(),
                base_revision: fixture.config().fingerprint().expect("base revision"),
                service: Some(worker),
            })
            .expect("prepare");
        authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("fixture approval");

        fixture.write_config(&draft.after_configuration);
        fs::remove_file(&target_executable).expect("remove target after config replacement");
        let recovered = authority
            .commit(&draft.draft_id)
            .expect("recover already-written configuration despite missing paths");
        assert_eq!(recovered.registered_state, "stopped");
        assert_eq!(
            authority
                .get_draft(&draft.draft_id)
                .expect("recovered draft")
                .status,
            DraftStatus::Committed
        );
        authority
            .commit(&draft.draft_id)
            .expect("committed replay must not recheck launch files");
    }

    #[test]
    fn likely_secret_environment_keys_are_rejected() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let mut service = fixture.service("Worker", 0);
        service
            .environment
            .insert("API_TOKEN".to_owned(), "do-not-store".to_owned());
        let validation = authority
            .validate_service(RegistrationOperation::Register, "worker", Some(service))
            .expect("validation result");

        assert!(!validation.valid);
        assert!(
            validation
                .issues
                .iter()
                .any(|issue| issue.code == "sensitive_environment_key_rejected")
        );
    }

    #[test]
    fn existing_profile_secret_gate_survives_structural_only_reads() {
        let fixture = Fixture::new();
        let mut config = fixture.config();
        config
            .services
            .get_mut("api")
            .expect("api service")
            .environment
            .insert("API_TOKEN".to_owned(), "fixture-only-secret".to_owned());
        fixture.write_config(&config);

        let Err(error) = RegistrationAuthority::open(Some(fixture.config_path.clone())) else {
            panic!("existing profile secrets must keep registration disabled");
        };
        assert_eq!(error.code(), "sensitive_environment_key_rejected");
    }

    #[test]
    fn update_commit_requires_live_stopped_evidence() {
        let fixture = Fixture::new();
        let authority = fixture.authority();
        let base_revision = fixture.config().fingerprint().expect("base revision");
        let draft = authority
            .prepare(PrepareRegistration {
                request_id: "update-api".to_owned(),
                operation: RegistrationOperation::Update,
                service_id: "api".to_owned(),
                base_revision,
                service: Some(fixture.service("Updated API", 0)),
            })
            .expect("prepare");
        authority
            .approve_with_confirmation(draft.clone(), &draft.confirmation_phrase)
            .expect("approve");

        let error = authority
            .commit(&draft.draft_id)
            .expect_err("no running supervisor evidence");
        assert_eq!(error.code(), "runtime_state_unverifiable");
        assert_eq!(fixture.config().services["api"].label, "API");
    }

    #[test]
    fn stopped_runtime_allows_update_and_unregister() {
        let fixture = Fixture::new();
        let mut config = fixture.config();
        let api = config.services.get_mut("api").expect("api service");
        api.cwd = fixture.directory.join("missing-old-api-cwd");
        api.command = fixture.directory.join("missing-old-api-command.exe");
        config
            .services
            .insert("spare".to_owned(), fixture.service("Spare", 0));
        fixture.write_config(&config);
        let server = start_runtime_mock(&fixture, "stopped", 4);
        let authority = fixture.authority();

        let update = authority
            .prepare(PrepareRegistration {
                request_id: "stopped-update".to_owned(),
                operation: RegistrationOperation::Update,
                service_id: "api".to_owned(),
                base_revision: fixture.config().fingerprint().expect("base revision"),
                service: Some(fixture.service("Updated API", 0)),
            })
            .expect("prepare update");
        authority
            .approve_with_confirmation(update.clone(), &update.confirmation_phrase)
            .expect("approve update in fixture");
        authority
            .commit(&update.draft_id)
            .expect("stopped update commit");
        assert_eq!(fixture.config().services["api"].label, "Updated API");

        let mut broken_before_unregister = fixture.config();
        let api = broken_before_unregister
            .services
            .get_mut("api")
            .expect("updated api service");
        api.cwd = fixture.directory.join("missing-api-cwd-before-unregister");
        api.command = fixture
            .directory
            .join("missing-api-command-before-unregister.exe");
        fixture.write_config(&broken_before_unregister);

        let unregister = authority
            .prepare(PrepareRegistration {
                request_id: "stopped-unregister".to_owned(),
                operation: RegistrationOperation::Unregister,
                service_id: "api".to_owned(),
                base_revision: fixture.config().fingerprint().expect("updated revision"),
                service: None,
            })
            .expect("prepare unregister");
        authority
            .approve_with_confirmation(unregister.clone(), &unregister.confirmation_phrase)
            .expect("approve unregister in fixture");
        let result = authority
            .commit(&unregister.draft_id)
            .expect("stopped unregister commit");
        assert_eq!(result.registered_state, "unregistered");
        assert!(!fixture.config().services.contains_key("api"));
        server.join().expect("fixture runtime server");
    }

    #[test]
    fn running_runtime_still_blocks_update_commit() {
        let fixture = Fixture::new();
        let server = start_runtime_mock(&fixture, "running", 1);
        let authority = fixture.authority();
        let update = authority
            .prepare(PrepareRegistration {
                request_id: "running-update".to_owned(),
                operation: RegistrationOperation::Update,
                service_id: "api".to_owned(),
                base_revision: fixture.config().fingerprint().expect("base revision"),
                service: Some(fixture.service("Updated API", 0)),
            })
            .expect("prepare update");
        authority
            .approve_with_confirmation(update.clone(), &update.confirmation_phrase)
            .expect("approve update in fixture");

        let error = authority
            .commit(&update.draft_id)
            .expect_err("running target must block update");
        assert_eq!(error.code(), "service_must_be_stopped");
        assert_eq!(fixture.config().services["api"].label, "API");
        server.join().expect("fixture runtime server");
    }

    struct Fixture {
        directory: PathBuf,
        config_path: PathBuf,
        executable: PathBuf,
    }

    fn start_runtime_mock(
        fixture: &Fixture,
        lifecycle: &str,
        request_count: usize,
    ) -> std::thread::JoinHandle<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture runtime port");
        let port = listener
            .local_addr()
            .expect("fixture runtime address")
            .port();
        let mut config = fixture.config();
        config.control.port = port;
        fixture.write_config(&config);
        let token_path = fixture.directory.join(".runtime/control-token");
        fs::create_dir_all(token_path.parent().expect("token parent"))
            .expect("create runtime token directory");
        fs::write(&token_path, "a".repeat(64)).expect("write fixture runtime token");

        let lifecycle = lifecycle.to_owned();
        std::thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept fixture runtime request");
                let mut request = Vec::new();
                stream
                    .read_to_end(&mut request)
                    .expect("read fixture runtime request");
                let request_line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let body = if request_line.starts_with("GET /v1/services/") {
                    json!({"service":{"lifecycle":lifecycle.clone()}}).to_string()
                } else {
                    "{}".to_owned()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write fixture runtime response");
                stream.flush().expect("flush fixture runtime response");
            }
        })
    }

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "aku-supervisor-registration-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).expect("fixture directory");
            let executable = std::env::current_exe().expect("test executable");
            let config_path = directory.join("services.json");
            let service = ServiceConfig {
                label: "API".to_owned(),
                cwd: directory.clone(),
                command: executable.clone(),
                args: Vec::new(),
                environment: BTreeMap::new(),
                depends_on: Vec::new(),
                startup_prerequisites: Vec::new(),
                health: HealthCheck::Process,
                ports: Vec::new(),
                restart_policy: RestartPolicy::Manual,
                shutdown_grace_ms: 1_000,
            };
            let config = SupervisorConfig {
                version: CONFIG_VERSION,
                control: ControlConfig {
                    host: "127.0.0.1".to_owned(),
                    port: 47_999,
                    token_file: PathBuf::from(".runtime/control-token"),
                    mcp: McpConfig::default(),
                },
                observability: ObservabilityConfig::default(),
                cooperative_actions: CooperativeActionsConfig::default(),
                services: BTreeMap::from([("api".to_owned(), service)]),
            };
            fs::write(
                &config_path,
                serde_json::to_vec_pretty(&config).expect("serialize fixture"),
            )
            .expect("write fixture config");
            Self {
                directory,
                config_path,
                executable,
            }
        }

        fn authority(&self) -> RegistrationAuthority {
            RegistrationAuthority::open(Some(self.config_path.clone())).expect("authority")
        }

        fn config(&self) -> SupervisorConfig {
            read_structural_config(&self.config_path).expect("read fixture config")
        }

        fn write_config(&self, config: &SupervisorConfig) {
            fs::write(
                &self.config_path,
                serde_json::to_vec_pretty(config).expect("serialize fixture config"),
            )
            .expect("write fixture config");
        }

        fn service(&self, label: &str, port: u16) -> ServiceConfig {
            ServiceConfig {
                label: label.to_owned(),
                cwd: self.directory.clone(),
                command: self.executable.clone(),
                args: Vec::new(),
                environment: BTreeMap::new(),
                depends_on: Vec::new(),
                startup_prerequisites: Vec::new(),
                health: HealthCheck::Process,
                ports: (port != 0).then_some(port).into_iter().collect(),
                restart_policy: RestartPolicy::Manual,
                shutdown_grace_ms: 1_000,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.directory).ok();
        }
    }
}
