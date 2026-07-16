//! Human-gated, revision-bound service registration persistence.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::config::{ConfigIssue, ServiceConfig, SupervisorConfig};
use super::config_path::{ConfigPathError, resolve_config_path};
use super::control_http::{ControlClientError, client_request};
use super::runtime_token::{RuntimeToken, resolve_token_path};

const DRAFT_LIFETIME_MS: u64 = 30 * 60 * 1_000;
const REGISTRATION_SCHEMA_VERSION: u32 = 1;

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
    pub registry_reconciliation: &'static str,
    pub unrelated_services_restarted: bool,
    pub next_command: Option<String>,
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

impl RegistrationAuthority {
    /// Opens the registration authority for one existing Supervisor profile.
    ///
    /// # Errors
    ///
    /// Returns a typed configuration or runtime-layout failure.
    pub fn open(explicit_config: Option<PathBuf>) -> Result<Self, RegistrationError> {
        let resolved = resolve_config_path(explicit_config).map_err(RegistrationError::from)?;
        let config = read_valid_config(resolved.path())?;
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
        Ok(json!({
            "schemaVersion": REGISTRATION_SCHEMA_VERSION,
            "authority": "human-gated-registration",
            "configurationPath": self.configuration_path,
            "currentRevision": revision,
            "supportedOperations": ["register", "update", "unregister"],
            "workflow": [
                "Call supervisor_registration_get_schema and supervisor_registration_get_capabilities.",
                "Validate the complete service object with supervisor_registration_validate_service.",
                "Prepare a revision-bound draft with supervisor_registration_prepare_change.",
                "Ask the user to run the returned approvalCommand in a real interactive terminal.",
                "After human approval, call supervisor_registration_commit_change exactly once."
            ],
            "safety": {
                "approvalAvailableThroughMcp": false,
                "approvalRequiresInteractiveTerminal": true,
                "approvalShowsFullBeforeAndAfterConfiguration": true,
                "approvalBoundToProposalHash": true,
                "draftLifetimeMs": DRAFT_LIFETIME_MS,
                "optimisticConcurrency": "exact base revision",
                "atomicConfigurationReplace": true,
                "registerInitialState": "stopped",
                "autoStart": false,
                "automaticRegistryReconciliation": true,
                "unrelatedServicesRestarted": false,
                "updateAndUnregisterRequireObservedStoppedState": true,
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
                "approve": "aku-supervisor registration approve <draft-id>"
            }
        }))
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
        let (proposed, mut issues, warnings) =
            proposed_configuration(&current, operation, service_id, service)?;
        issues.extend(proposed.validation_issues());
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
        let (proposed, mut issues, warnings) = proposed_configuration(
            &current,
            request.operation,
            &request.service_id,
            request.service,
        )?;
        issues.extend(proposed.validation_issues());
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
        self.audit("prepared", &draft, None)?;
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
        if !io::stdin().is_terminal() {
            return Err(RegistrationError::new(
                "interactive_terminal_required",
                "approval must run in a real interactive terminal; piped input and MCP approval are forbidden",
            ));
        }
        let draft = self.get_draft(draft_id)?;
        print_approval_review(&draft)?;
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

    /// Atomically commits one approved draft after revision and state checks.
    ///
    /// # Errors
    ///
    /// Returns an approval, expiry, concurrency, runtime-state, or persistence failure.
    pub fn commit(&self, draft_id: &str) -> Result<CommitResult, RegistrationError> {
        let _commit_lock = CommitLock::acquire(&self.registration_directory)?;
        let mut draft = self.get_draft(draft_id)?;
        if draft.status == DraftStatus::Committed {
            return Ok(commit_result(&draft));
        }
        if draft.status != DraftStatus::Approved {
            return Err(RegistrationError::new(
                "human_approval_required",
                "draft is not approved; MCP cannot approve it",
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
            self.audit("commit_recovered", &draft, Some("registration_mcp"))?;
            return Ok(commit_result(&draft));
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
        draft.after_configuration.validate().map_err(config_error)?;
        write_json_atomic(&self.configuration_path, &draft.after_configuration)?;
        draft.status = DraftStatus::Committed;
        draft.committed_at_unix_ms = Some(now_ms()?);
        write_json_atomic(&self.draft_path(&draft.draft_id)?, &draft)?;
        self.audit("committed", &draft, Some("registration_mcp"))?;
        Ok(commit_result(&draft))
    }

    fn current_config(&self) -> Result<SupervisorConfig, RegistrationError> {
        let config = read_valid_config(&self.configuration_path)?;
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

fn proposed_configuration(
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

fn commit_result(draft: &RegistrationDraft) -> CommitResult {
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
        registry_reconciliation: "automatic_when_supervisor_is_running",
        unrelated_services_restarted: false,
        next_command: present.then(|| {
            format!(
                "aku-supervisor start {} --actor user --reason \"start registered service\"",
                draft.service_id
            )
        }),
    }
}

fn print_approval_review(draft: &RegistrationDraft) -> Result<(), RegistrationError> {
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
    println!(
        "\nApproval does not start the service. Commit remains revision-bound and update/unregister require observed stopped state."
    );
    Ok(())
}

fn read_valid_config(path: &Path) -> Result<SupervisorConfig, RegistrationError> {
    let source = fs::read_to_string(path).map_err(|error| {
        RegistrationError::with_source(
            "configuration_read_failed",
            &format!("failed to read {}", path.display()),
            error,
        )
    })?;
    let config = SupervisorConfig::parse_json(&source).map_err(config_error)?;
    config.validate().map_err(config_error)?;
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
            "cwd": {"type":"string", "description":"Absolute existing working directory."},
            "command": {"type":"string", "description":"Absolute existing executable or command wrapper path; never a shell expression."},
            "args": {"type":"array", "items":{"type":"string"}, "default":[], "description":"Fixed argv entries, kept separate from command."},
            "environment": {"type":"object", "additionalProperties":{"type":"string"}, "default":{}, "description":"Non-sensitive fixed overrides only. Likely secret keys are rejected."},
            "health": {"oneOf":[
                {"type":"object", "additionalProperties":false, "required":["type"], "properties":{"type":{"const":"process"}}},
                {"type":"object", "additionalProperties":false, "required":["type","host","port","timeoutMs","startupDeadlineMs"], "properties":{"type":{"const":"tcp-connect"},"host":{"type":"string","description":"Explicit loopback IP."},"port":{"type":"integer","minimum":1,"maximum":65535},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1}}},
                {"type":"object", "additionalProperties":false, "required":["type","url","expectedStatus","timeoutMs","startupDeadlineMs"], "properties":{"type":{"const":"http-status"},"url":{"type":"string","description":"Loopback http:// URL."},"expectedStatus":{"type":"integer","minimum":100,"maximum":599},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1}}},
                {"type":"object", "additionalProperties":false, "required":["type","url","timeoutMs","startupDeadlineMs","expect"], "properties":{"type":{"const":"http-json"},"url":{"type":"string","description":"Loopback http:// URL."},"timeoutMs":{"type":"integer","minimum":1},"startupDeadlineMs":{"type":"integer","minimum":1},"expect":{"type":"object","minProperties":1,"additionalProperties":{"type":["string","number","integer","boolean","null"]}}}}
            ]},
            "ports": {"type":"array", "items":{"type":"integer","minimum":1,"maximum":65535}, "uniqueItems":true, "default":[]},
            "restartPolicy": {"type":"string", "enum":["manual","on-failure"]},
            "shutdownGraceMs": {"type":"integer", "minimum":1}
        },
        "examples": [{
            "label":"Example API", "cwd":"C:\\absolute\\project", "command":"C:\\absolute\\runtime\\server.exe",
            "args":["--port","8090"], "environment":{}, "health":{"type":"http-status","url":"http://127.0.0.1:8090/health","expectedStatus":200,"timeoutMs":3000,"startupDeadlineMs":20000},
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
            "automatic_when_supervisor_is_running"
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

    struct Fixture {
        directory: PathBuf,
        config_path: PathBuf,
        executable: PathBuf,
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
                health: HealthCheck::Process,
                ports: Vec::new(),
                restart_policy: RestartPolicy::Manual,
                shutdown_grace_ms: 1_000,
            };
            let config = SupervisorConfig {
                version: 1,
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
            read_valid_config(&self.config_path).expect("read fixture config")
        }

        fn service(&self, label: &str, port: u16) -> ServiceConfig {
            ServiceConfig {
                label: label.to_owned(),
                cwd: self.directory.clone(),
                command: self.executable.clone(),
                args: Vec::new(),
                environment: BTreeMap::new(),
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
