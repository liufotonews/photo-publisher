//! Desktop application commands (the Tauri-facing application bridge).
//!
//! These functions are pure adapters: they validate input shapes, delegate to
//! `publisher-app` use cases, and convert results into small serializable
//! types. They contain no business rules, never touch providers, credentials,
//! the network, or the ledger, and never write to the filesystem.
//!
//! The functions deliberately carry no `#[tauri::command]` macro: the library
//! stays free of the Tauri runtime (so tests link nothing webview-related),
//! and the desktop binary registers thin wrappers for each of them.
//!
//! Besides the adapters, this module hosts the provisioning exclusion
//! primitive (Phase 7-J concurrency fix): a gate whose single slot is
//! registered as Tauri-managed state, acquired atomically before any
//! blocking work is queued, and released only when that work finishes.
//! The UI's busy flag stays presentation-only; this is the real exclusion.

use publisher_app::events::EventSink;
use serde::Serialize;

/// Non-sensitive application identity, derived from the crate itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// Application identity for the desktop shell. No paths, no host info, no
/// environment variables — only the two public product facts.
pub fn get_app_info() -> AppInfo {
    AppInfo {
        name: "Photo Publisher",
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// The error surface of the desktop command layer: the stable application
/// classification plus a human message. Never a stack trace, never secrets,
/// never raw internal error types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandError {
    pub kind: String,
    pub message: String,
}

impl From<publisher_app::ApplicationError> for CommandError {
    fn from(error: publisher_app::ApplicationError) -> Self {
        Self {
            kind: error.kind.as_str().to_owned(),
            message: error.to_string(),
        }
    }
}

/// Successful validation outcome of a `project.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateProjectOutcome {
    pub valid: bool,
    pub project: ProjectSummary,
}

/// Public project identity facts published by the application layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    /// "v1" (local-only) or "v2" (integrated publication).
    pub kind: &'static str,
}

/// Validates a `project.json` through the application layer.
///
/// This command performs no preflight of providers and reads no credentials:
/// validation is intentionally independent of any publication setup, so it
/// works on a machine without GitHub/R2/Vercel configuration. The caller
/// provides the event sink (the desktop binary forwards it to the Tauri
/// event channel); nothing is emitted unless the caller does so.
pub fn validate_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<ValidateProjectOutcome, CommandError> {
    let trimmed = project_path.trim();
    if trimmed.is_empty() {
        return Err(CommandError {
            kind: publisher_app::ApplicationErrorKind::ProjectInvalid
                .as_str()
                .to_owned(),
            message: "project path is required".to_owned(),
        });
    }
    let handle = publisher_app::validate_project(std::path::Path::new(trimmed), events)?;
    Ok(ValidateProjectOutcome {
        valid: true,
        project: ProjectSummary {
            id: handle.project_id,
            name: handle.project_name,
            kind: match handle.kind {
                publisher_app::ProjectKind::Version1 => "v1",
                publisher_app::ProjectKind::Version2 => "v2",
            },
        },
    })
}

fn check_path(project_path: &str) -> Result<std::path::PathBuf, CommandError> {
    let trimmed = project_path.trim();
    if trimmed.is_empty() {
        return Err(CommandError {
            kind: publisher_app::ApplicationErrorKind::ProjectInvalid
                .as_str()
                .to_owned(),
            message: "project path is required".to_owned(),
        });
    }
    Ok(std::path::PathBuf::from(trimmed))
}

/// Publication outcome for the desktop shell: a stable, serializable view of
/// the application outcome. Counts only — never internal types, never
/// secrets, never provider details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishOutcomeDto {
    /// Stable outcome name: "published" | "no_change" | "blocked" |
    /// "needs_recovery".
    pub outcome: &'static str,
    /// Generation of the committed local publication.
    pub generation: String,
    /// Storage summary, present only when remote storage work was confirmed.
    pub storage: Option<OperationCountsDto>,
    /// Repository summary, present only when a commit was confirmed.
    pub repository: Option<RepositoryPublishDto>,
    /// Hosting summary, present only when a deployment was confirmed.
    pub hosting: Option<HostingPublishDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationCountsDto {
    pub written: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryPublishDto {
    pub written: usize,
    pub deleted: usize,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostingPublishDto {
    pub deployment_id: String,
    pub url: String,
}

/// Dry-run outcome for the desktop shell: counts of the reconciliation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DryRunOutcomeDto {
    pub generation: String,
    pub storage_operations: usize,
    pub repository_operations: usize,
    pub hosting_operations: usize,
    pub reconciliation_requirements: usize,
}

/// Recovery outcome for the desktop shell. Only public facts — no internals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoverOutcomeDto {
    /// The recovery flow was attempted (the use case ran).
    pub attempted: bool,
    /// True when the local publication is committed again after recovery.
    pub recovered: bool,
    /// Generation currently committed, when a recovered journal exists.
    pub generation: Option<String>,
}

/// Publishes the project through the application layer.
///
/// The desktop layer only: (1) reads the validated document, (2) runs the
/// existing publication preflight for v2 projects via the desktop composition
/// root, (3) delegates the orchestration to `publisher_app::publish_project`
/// with the composed providers. No business rule lives here.
pub fn publish_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<PublishOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let document = publisher_app::load_project_document(&path)?;
    let is_v2 = document["schemaVersion"].as_u64() == Some(2);

    // Providers exist only for the duration of this invocation, and only for
    // schema-v2 publications.
    let composed = if is_v2 {
        let configuration = crate::composition::preflight_publication(&document, &path)?;
        let providers = crate::composition::build_providers(&configuration)?;
        Some((configuration, providers))
    } else {
        None
    };

    let outcome = match composed {
        Some((configuration, mut providers)) => {
            let mut publication_providers = providers.as_publication_providers();
            publisher_app::publish_project(
                &path,
                Some(&configuration),
                publisher_app::PublishOptions,
                &mut publication_providers,
                events,
            )?
        }
        None => {
            // Schema v1 is local-only: the application layer never calls the
            // providers at all, so the composition root provides an inert set.
            let mut inert = crate::composition::NoopProviders::new();
            publisher_app::publish_project(
                &path,
                None,
                publisher_app::PublishOptions,
                &mut inert.as_providers(),
                events,
            )?
        }
    };
    Ok(publish_dto(&outcome))
}

fn publish_dto(outcome: &publisher_app::PublishOutcome) -> PublishOutcomeDto {
    let publish_outcome_name;
    let mut storage = None;
    let mut repository = None;
    let mut hosting = None;
    match &outcome.publication {
        publisher_app::PublicationOutcome::Published(report) => {
            publish_outcome_name = "published";
            storage = report.storage.as_ref().map(|report| OperationCountsDto {
                written: report.uploaded.len(),
                deleted: report.deleted.len(),
            });
            repository = report
                .repository
                .as_ref()
                .map(|report| RepositoryPublishDto {
                    written: report.written.len(),
                    deleted: report.deleted.len(),
                    revision: report.revision.clone(),
                });
            hosting = report.hosting.as_ref().map(|report| HostingPublishDto {
                deployment_id: report
                    .deployment
                    .as_ref()
                    .map(|deployment| deployment.id.clone())
                    .unwrap_or_default(),
                url: report
                    .deployment
                    .as_ref()
                    .map(|deployment| deployment.url.clone())
                    .unwrap_or_default(),
            });
        }
        publisher_app::PublicationOutcome::NoChange { .. } => {
            publish_outcome_name = "no_change";
        }
        publisher_app::PublicationOutcome::Blocked { .. } => {
            publish_outcome_name = "blocked";
        }
        publisher_app::PublicationOutcome::NeedsRecovery { .. } => {
            publish_outcome_name = "needs_recovery";
        }
    }
    PublishOutcomeDto {
        outcome: publish_outcome_name,
        generation: outcome.generation.clone(),
        storage,
        repository,
        hosting,
    }
}

/// Plans the integrated publication without performing it.
///
/// Delegates to `publisher_app::dry_run_project`, which by construction
/// cannot construct providers, write the ledger, or touch the network.
pub fn dry_run_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<DryRunOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let outcome = publisher_app::dry_run_project(&path, events)?;
    Ok(DryRunOutcomeDto {
        generation: outcome.generation,
        storage_operations: outcome.storage_operations,
        repository_operations: outcome.repository_operations,
        hosting_operations: outcome.hosting_operations,
        reconciliation_requirements: outcome.reconciliation_requirements,
    })
}

/// Runs the application-layer publication recovery for the project.
///
/// Pure adapter: all journal/ledger semantics stay in `publisher-app`;
/// this command only validates the input path and converts the outcome.
pub fn recover_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<RecoverOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let outcome = publisher_app::recover_publication(&path, events)?;
    let recovered = outcome
        .state
        .as_ref()
        .map(|state| !state.recovery_needed)
        .unwrap_or(false);
    Ok(RecoverOutcomeDto {
        attempted: outcome.attempted,
        recovered,
        generation: outcome.state.as_ref().map(|state| state.generation.clone()),
    })
}

/// Outcome of the project setup creation, published back to the desktop UI.
///
/// Public facts only: identity and the written path (the caller provided it).
/// Never providers.credentials/network or internal payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreateProjectSetupOutcomeDto {
    pub project_id: String,
    pub project_name: String,
    pub project_path: String,
}

/// Creates a `project.json` from the wizard's payload.
///
/// The only work here is: deserialize the wizard payload into the Phase 7-A
/// contract (`ProjectSetup`), then hand it to the application-level service
/// (`create_project_setup`), which performs the schema-mandated write.
/// This function has no provider, no credential, and no network reach —
/// those live outside setup by design (see Phase 7-0).
pub fn create_project_setup(
    project_path: &str,
    setup: serde_json::Value,
) -> Result<CreateProjectSetupOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    if !setup.is_object() {
        return Err(CommandError {
            kind: publisher_app::ApplicationErrorKind::ProjectInvalid
                .as_str()
                .to_owned(),
            message: "the wizard payload must be a JSON object".to_owned(),
        });
    }
    let model: publisher_app::ProjectSetup =
        serde_json::from_value(setup).map_err(|error| CommandError {
            kind: publisher_app::ApplicationErrorKind::ProjectInvalid
                .as_str()
                .to_owned(),
            message: format!("the wizard payload does not match the project contract: {error}"),
        })?;
    let outcome = publisher_app::create_project_setup(&path, model)?;
    Ok(CreateProjectSetupOutcomeDto {
        project_id: outcome.project_id,
        project_name: outcome.project_name,
        project_path: outcome.project_path.display().to_string(),
    })
}

/// One diagnosed configuration issue, ready for presentation. The code is
/// the stable machine identifier; the message is presentation text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigurationIssueDto {
    pub code: String,
    pub field: String,
    pub message: String,
}

/// Outcome of the configuration validation (Phase 7-D): a deterministic,
/// secret-free diagnosis of the declared configuration. Reports only —
/// never publishes, never recovers, never creates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateConfigurationOutcomeDto {
    pub valid: bool,
    pub schema_version: u8,
    pub project_id: String,
    pub project_name: String,
    pub issues: Vec<ConfigurationIssueDto>,
}

/// Diagnoses the declared configuration of a `project.json`.
///
/// Pure adapter: all level-2 rules live in `publisher-app`. This command
/// holds no provider, credential, or network reach — it cannot read a
/// secret or call a remote API by construction.
pub fn validate_project_configuration(
    project_path: &str,
) -> Result<ValidateConfigurationOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let outcome = publisher_app::validate_project_configuration(&path)?;
    Ok(ValidateConfigurationOutcomeDto {
        valid: outcome.valid,
        schema_version: outcome.schema_version,
        project_id: outcome.project_id,
        project_name: outcome.project_name,
        issues: outcome
            .issues
            .iter()
            .map(|issue| ConfigurationIssueDto {
                code: issue.code.as_str().to_owned(),
                field: issue.field.clone(),
                message: issue.message.clone(),
            })
            .collect(),
    })
}

/// Public state of one credential: a stable name, a safe label, and the
/// configured bit. The value never appears in this surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialStatusDto {
    pub name: String,
    pub label: String,
    pub configured: bool,
}

/// Lists the configured bit of every supported credential (Phase 7-E).
/// Reads one bit per credential from the composition-root backend and
/// nothing else — the values are never returned.
pub fn get_credential_status() -> Result<Vec<CredentialStatusDto>, CommandError> {
    let store = crate::composition::credential_store();
    let statuses = publisher_app::credential_status(&store)?;
    Ok(statuses
        .iter()
        .map(|status| CredentialStatusDto {
            name: status.name.to_owned(),
            label: status.label.to_owned(),
            configured: status.configured,
        })
        .collect())
}

/// Stores one credential (Phase 7-E). The value is passed to the backend
/// for the duration of the operation only: it is never copied into a DTO,
/// an error, an event, a log, or any state of this layer. The name is
/// validated against the application allowlist by `publisher-app` itself.
pub fn set_credential(name: &str, value: String) -> Result<(), CommandError> {
    let mut store = crate::composition::credential_store();
    publisher_app::set_credential(&mut store, name, value.as_bytes())?;
    Ok(())
}

/// Removes one credential (Phase 7-E). Only the allowlisted name crosses
/// the boundary; there is nothing secret left to report afterwards.
pub fn delete_credential(name: &str) -> Result<(), CommandError> {
    let mut store = crate::composition::credential_store();
    publisher_app::delete_credential(&mut store, name)?;
    Ok(())
}

/// One provisioned resource, ready for presentation: the contractual
/// disposition (`created` / `unchanged` / `configured` / `changed`) plus
/// the stable public identity. Never secrets, never provider payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvisionedResourceDto {
    pub status: String,
    pub identity: String,
}

fn resource_dto(resource: &publisher_app::ProvisionedResource) -> ProvisionedResourceDto {
    ProvisionedResourceDto {
        status: resource.status.as_str().to_owned(),
        identity: resource.identity.clone(),
    }
}

/// The aggregated provisioning report (Phase 7-J) for the desktop UI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProvisionProjectOutcomeDto {
    pub project_id: String,
    pub project_name: String,
    pub repository: Option<ProvisionedResourceDto>,
    pub storage: Option<ProvisionedResourceDto>,
    pub hosting: Option<ProvisionedResourceDto>,
    pub domain: Option<ProvisionedResourceDto>,
}

fn report_dto(report: &publisher_app::ProvisionProjectReport) -> ProvisionProjectOutcomeDto {
    ProvisionProjectOutcomeDto {
        project_id: report.project_id.clone(),
        project_name: report.project_name.clone(),
        repository: report.repository.as_ref().map(resource_dto),
        storage: report.storage.as_ref().map(resource_dto),
        hosting: report.hosting.as_ref().map(resource_dto),
        domain: report.domain.as_ref().map(resource_dto),
    }
}

/// A provisioning failure (Phase 7-J): the application classification and
/// safe message, plus the *partial* report — everything that completed
/// before the failure stays visible; nothing was rolled back. The report is
/// boxed so the `Result` of the command stays a small type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvisionProjectFailureDto {
    pub kind: String,
    pub message: String,
    pub report: Box<ProvisionProjectOutcomeDto>,
}

// ---------------------------------------------------------------------------
// Provisioning exclusion (Phase 7-J concurrency fix)
//
// The UI releases its busy flag whenever the project path changes — but a
// provisioning already running in a blocking worker keeps running. The busy
// flag is therefore presentation only: real exclusion lives HERE, in the
// desktop backend, as Tauri-managed state. One slot exists per application:
// acquired atomically before any work is queued, held for the whole blocking
// execution, and released only by the worker finishing (success or error). A
// stale UI result can never release it, because the UI never reaches it.
// ---------------------------------------------------------------------------

/// Stable desktop classification of the concurrent-provisioning refusal.
/// This is deliberately not an application-layer failure kind: the refused
/// run never started, so nothing failed — the backend is saying "one
/// provisioning at a time".
pub const PROVISIONING_BUSY_KIND: &str = "provisioning_busy";

/// Tauri-managed exclusive slot for provisioning (Phase 7-J).
///
/// Registered once by the desktop bootstrap as managed state and never
/// recreated afterwards. The flag is `true` while exactly one provisioning
/// is running anywhere in the application. The inner mutex guards only the
/// instant check-and-set of [`ProvisioningGate::try_acquire`] and the
/// instant reset of the permit's drop — it is never held across any
/// provisioning work, so a second attempt can never wait on the running
/// operation: it is answered immediately.
#[derive(Debug, Default)]
pub struct ProvisioningGate {
    slot: std::sync::Arc<std::sync::Mutex<bool>>,
}

/// A live acquisition of [`ProvisioningGate`].
///
/// Owning this value is what "a provisioning is running" means. It owns its
/// share of the slot (so it can move into the blocking worker) and releases
/// on drop — the ONLY release path, which happens exactly when the worker
/// holding it finishes, with success or error. The value carries no data:
/// no path, no credentials, no results.
#[derive(Debug)]
pub struct ProvisioningPermit {
    slot: std::sync::Arc<std::sync::Mutex<bool>>,
}

impl ProvisioningGate {
    /// One free slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically acquires the single provisioning slot.
    ///
    /// `Some(permit)`: the caller now exclusively owns provisioning until
    /// the permit drops. `None`: a provisioning is already running and this
    /// attempt is refused immediately — no waiting, no provisioner
    /// construction, no application service call. The check-and-set runs
    /// under the slot mutex, which is held only for that instant.
    pub fn try_acquire(&self) -> Option<ProvisioningPermit> {
        let mut busy = Self::lock_slot(&self.slot);
        if *busy {
            return None;
        }
        *busy = true;
        Some(ProvisioningPermit {
            slot: std::sync::Arc::clone(&self.slot),
        })
    }

    /// Locks the slot flag. Poisoning could only originate in a panic
    /// inside the instant check-and-set/reset sections; the flag carries no
    /// invariant a panic could corrupt, so recovering the guard is always
    /// the correct (and deadlock-free) choice.
    fn lock_slot(slot: &std::sync::Mutex<bool>) -> std::sync::MutexGuard<'_, bool> {
        slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for ProvisioningPermit {
    /// The single release point: the worker finished (its closure dropped
    /// this permit), so the slot becomes free for the next provisioning.
    fn drop(&mut self) {
        *ProvisioningGate::lock_slot(&self.slot) = false;
    }
}

/// The immediate refusal returned when a provisioning is attempted while
/// another one is still running.
///
/// Pure fixed data: a stable kind, one public sentence, and an empty report
/// — nothing ran, so nothing completed. No path, no environment name, and
/// no secret material can ever appear here, because the refusal consults
/// nothing.
pub fn provisioning_busy_failure() -> ProvisionProjectFailureDto {
    ProvisionProjectFailureDto {
        kind: PROVISIONING_BUSY_KIND.to_owned(),
        message:
            "a provisioning operation is already in progress; wait for it to finish before starting another"
                .to_owned(),
        report: Box::new(ProvisionProjectOutcomeDto::default()),
    }
}

/// Runs one provisioning attempt holding the exclusively acquired slot
/// (Phase 7-J concurrency fix).
///
/// The permit crosses by value: it stays alive for the entire body — the
/// acquisition covers the whole blocking execution — and drops exactly when
/// `work` returns, with success or error, which is the only moment the slot
/// is released. `work` is the command adapter (the Tauri wrapper passes
/// [`provision_project`]); it is a parameter so the exclusion contract can
/// be tested without providers or network reach. A refused attempt never
/// reaches this function at all: without a permit there is nothing to run.
pub fn provision_exclusive<F>(
    permit: ProvisioningPermit,
    project_path: &str,
    work: F,
    events: &mut EventSink<'_>,
) -> Result<ProvisionProjectOutcomeDto, ProvisionProjectFailureDto>
where
    F: FnOnce(
        &str,
        &mut EventSink<'_>,
    ) -> Result<ProvisionProjectOutcomeDto, ProvisionProjectFailureDto>,
{
    let _held = permit;
    work(project_path, events)
}

// ---------------------------------------------------------------------------
// Publication exclusion (Phase 7-K.6)
//
// One publication at a time, enforced by the desktop backend — never by the
// JavaScript busy flag. This gate deliberately MIRRORS the provisioning one
// instead of sharing an abstraction: the two operations are independent
// (Provisioning and Publish never share a slot) and a premature
// generalization would grow this phase. Different responsibilities from the
// pipeline's execution lock: that one guards the local publication's
// integrity on disk; this one admits exactly one Publish run anywhere in
// the application, rejected before any provider, credential, network, or
// application-service work exists.
// ---------------------------------------------------------------------------

/// Stable desktop classification of the concurrent-publication refusal.
/// This is deliberately not an application-layer failure kind: the refused
/// run never started, so nothing failed — the backend is saying "one
/// publication at a time". It stays distinct from any publication failure.
pub const PUBLICATION_BUSY_KIND: &str = "publication_busy";

/// Tauri-managed exclusive slot for publication (Phase 7-K.6).
///
/// Registered once by the desktop bootstrap as managed state and never
/// recreated afterwards. The flag is `true` while exactly one publication
/// is running anywhere in the application. The inner mutex guards only the
/// instant check-and-set of [`PublicationGate::try_acquire`] and the
/// instant reset of the permit's drop — it is never held across any
/// publication work, so a second attempt can never wait on the running
/// operation: it is answered immediately.
#[derive(Debug, Default)]
pub struct PublicationGate {
    slot: std::sync::Arc<std::sync::Mutex<bool>>,
}

/// A live acquisition of [`PublicationGate`].
///
/// Owning this value is what "a publication is running" means. It owns its
/// share of the slot (so it can move into the blocking worker) and releases
/// on drop — the ONLY release path, which happens exactly when the worker
/// holding it finishes, with success or error. The value carries no data:
/// no path, no credentials, no results.
#[derive(Debug)]
pub struct PublicationPermit {
    slot: std::sync::Arc<std::sync::Mutex<bool>>,
}

impl PublicationGate {
    /// One free slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically acquires the single publication slot.
    ///
    /// `Some(permit)`: the caller now exclusively owns publication until the
    /// permit drops. `None`: a publication is already running and this
    /// attempt is refused immediately — no waiting, no provider
    /// construction, no credential read, no application service call. The
    /// check-and-set runs under the slot mutex, which is held only for that
    /// instant.
    pub fn try_acquire(&self) -> Option<PublicationPermit> {
        let mut busy = Self::lock_slot(&self.slot);
        if *busy {
            return None;
        }
        *busy = true;
        Some(PublicationPermit {
            slot: std::sync::Arc::clone(&self.slot),
        })
    }

    /// Locks the slot flag. Poisoning could only originate in a panic
    /// inside the instant check-and-set/reset sections; the flag carries no
    /// invariant a panic could corrupt, so recovering the guard is always
    /// the correct (and deadlock-free) choice.
    fn lock_slot(slot: &std::sync::Mutex<bool>) -> std::sync::MutexGuard<'_, bool> {
        slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for PublicationPermit {
    /// The single release point: the worker finished (its closure dropped
    /// this permit), so the slot becomes free for the next publication.
    fn drop(&mut self) {
        *PublicationGate::lock_slot(&self.slot) = false;
    }
}

/// The immediate refusal returned when a publication is attempted while
/// another one is still running.
///
/// Pure fixed data: a stable kind and one public sentence — nothing ran, so
/// nothing completed. No path, no environment name, and no secret material
/// can ever appear here, because the refusal consults nothing.
pub fn publication_busy_error() -> CommandError {
    CommandError {
        kind: PUBLICATION_BUSY_KIND.to_owned(),
        message:
            "Já existe uma publicação em andamento. Aguarde a conclusão antes de iniciar outra."
                .to_owned(),
    }
}

/// Runs one publication attempt holding the exclusively acquired slot
/// (Phase 7-K.6).
///
/// The permit crosses by value: it stays alive for the entire body — the
/// acquisition covers the whole blocking execution (load, local publication,
/// plan, providers) — and drops exactly when `work` returns, with success
/// or error, which is the only moment the slot is released. `work` is the
/// command adapter (the Tauri wrapper passes [`publish_project`]); it is a
/// parameter so the exclusion contract can be tested without providers or
/// network reach. A refused attempt never reaches this function at all:
/// without a permit there is nothing to run.
pub fn publish_exclusive<F>(
    permit: PublicationPermit,
    project_path: &str,
    work: F,
    events: &mut EventSink<'_>,
) -> Result<PublishOutcomeDto, CommandError>
where
    F: FnOnce(&str, &mut EventSink<'_>) -> Result<PublishOutcomeDto, CommandError>,
{
    let _held = permit;
    work(project_path, events)
}

/// Runs the explicit infrastructure provisioning for a project (Phase 7-J,
/// wired to the event channel in Phase 7-K.5).
///
/// Pure adapter: the composition root builds the concrete provisioners, the
/// application service owns every rule, and this function only converts
/// shapes. Never publishes, never plans, never reads a credential value.
/// The event sink is forwarded verbatim so the single desktop channel can
/// render the run live.
pub fn provision_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<ProvisionProjectOutcomeDto, ProvisionProjectFailureDto> {
    let path = match check_path(project_path) {
        Ok(path) => path,
        Err(error) => {
            return Err(ProvisionProjectFailureDto {
                kind: error.kind,
                message: error.message,
                report: Box::new(ProvisionProjectOutcomeDto::default()),
            });
        }
    };
    let mut provisioners = match crate::composition::build_provisioners() {
        Ok(provisioners) => provisioners,
        Err(error) => {
            return Err(ProvisionProjectFailureDto {
                kind: error.kind.as_str().to_owned(),
                message: error.to_string(),
                report: Box::new(ProvisionProjectOutcomeDto::default()),
            });
        }
    };
    match publisher_app::provision_project(
        &path,
        &mut provisioners.as_provisioning_providers(),
        events,
    ) {
        Ok(report) => Ok(report_dto(&report)),
        Err(failure) => Err(ProvisionProjectFailureDto {
            kind: failure.error.kind.as_str().to_owned(),
            message: failure.error.to_string(),
            report: Box::new(report_dto(&failure.report)),
        }),
    }
}

/// One preflight precondition failure, ready for presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreflightIssueDto {
    pub code: String,
    pub field: String,
    pub message: String,
}

/// Outcome of the standalone preflight (Phase 7-F): a deterministic,
/// secret-free precondition report. Reports only — never plans, never
/// publishes, never touches a provider remotely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreflightOutcomeDto {
    pub ready: bool,
    pub schema_version: u8,
    pub project_id: String,
    pub project_name: String,
    pub issues: Vec<PreflightIssueDto>,
}

/// Checks whether a project has every precondition for a publication
/// attempt (Phase 7-F). Pure adapter: all rules live in `publisher-app`;
/// the credential backend comes from the composition root. No providers
/// are built, no network is reached, no plan is computed.
pub fn preflight_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<PreflightOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let store = crate::composition::credential_store();
    let outcome = publisher_app::preflight_project(&path, &store, events)?;
    Ok(PreflightOutcomeDto {
        ready: outcome.ready,
        schema_version: outcome.schema_version,
        project_id: outcome.project_id,
        project_name: outcome.project_name,
        issues: outcome
            .issues
            .iter()
            .map(|issue| PreflightIssueDto {
                code: issue.code.as_str().to_owned(),
                field: issue.field.clone(),
                message: issue.message.clone(),
            })
            .collect(),
    })
}

/// Outcome of the explicit local bootstrap (Phase 7-K.2): public identity
/// facts plus the committed generation. Never secrets, never internal types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrepareLocalOutcomeDto {
    pub project_id: String,
    pub project_name: String,
    /// Generation of the committed local publication (e.g. `g-000001`).
    pub generation: String,
}

/// Prepares the local publication explicitly (Phase 7-K.2).
///
/// Pure adapter: delegates to `publisher_app::prepare_local_publication`,
/// which resolves the project and runs the exact local-publication step
/// Publish uses. Exclusively local — this command builds no providers,
/// touches no credential, and reaches no network.
pub fn prepare_local_publication(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<PrepareLocalOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let outcome = publisher_app::prepare_local_publication(&path, events)?;
    Ok(PrepareLocalOutcomeDto {
        project_id: outcome.project_id,
        project_name: outcome.project_name,
        generation: outcome.generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_project(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root.join("source")).unwrap();
        let path = root.join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "project": {"id": "app-test", "name": "App Test"},
                "gallery": {"template": "local", "title": "App Test"},
                "source": {"type": "folder", "path": "source"},
                "repository": {"provider": "local", "repository": "local"},
                "hosting": {"provider": "local"},
                "storage": {
                    "preview": {"provider": "local"},
                    "highResolution": {"provider": "local"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn get_app_info_exposes_only_public_product_facts() {
        let info = get_app_info();
        assert_eq!(info.name, "Photo Publisher");
        assert_eq!(info.version, "0.1.0");
        let json = serde_json::to_value(&info).unwrap();
        let text = json.to_string();
        for forbidden in [
            std::env::temp_dir().to_string_lossy().as_ref(),
            "PHOTO_PUBLISHER_",
            "token",
            "secret",
        ] {
            assert!(!text.contains(forbidden), "get_app_info leaked {forbidden}");
        }
    }

    #[test]
    fn validate_project_accepts_a_valid_fixture() {
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        let outcome = validate_project(project.to_str().unwrap(), &mut |_| {}).unwrap();
        assert!(outcome.valid);
        assert_eq!(outcome.project.id, "app-test");
        assert_eq!(outcome.project.name, "App Test");
        assert_eq!(outcome.project.kind, "v1");
    }

    #[test]
    fn validate_project_reports_invalid_projects_without_internals() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(&path, b"{not json").unwrap();
        let error = validate_project(path.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert!(!error.message.contains("panicked"));
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["kind"], "project_invalid");
    }

    #[test]
    fn validate_project_rejects_empty_paths_deterministically() {
        let error = validate_project("   ", &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert_eq!(error.message, "project path is required");
    }

    #[test]
    fn validate_project_never_needs_credentials() {
        // Validation never consults the environment: the command runs
        // identically regardless of any PHOTO_PUBLISHER_* configuration.
        // (No mutation here — environment changes race with the composition
        // tests in the same process; see tests/bootstrap.rs for the structural
        // guarantee that no credential lookup happens at all.)
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        validate_project(project.to_str().unwrap(), &mut |_| {}).unwrap();
    }

    #[test]
    fn validate_project_emits_events_without_changing_the_outcome() {
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        let mut captured: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let outcome =
            validate_project(project.to_str().unwrap(), &mut |event| captured.push(event)).unwrap();
        // Same result regardless of whether events are observed.
        assert!(outcome.valid);
        assert_eq!(outcome.project.id, "app-test");
        assert_eq!(outcome.project.kind, "v1");
        // The use case emitted its workflow lifecycle; the adapter observed it.
        assert_eq!(
            captured.first(),
            Some(&publisher_app::ApplicationEvent::EnteredStep(
                publisher_app::WorkflowStep::ValidateProject
            ))
        );
        assert_eq!(
            captured.last(),
            Some(&publisher_app::ApplicationEvent::Finished)
        );
    }

    fn write_jpeg(path: &Path, value: u8) {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
        image.save(path).unwrap();
    }

    fn write_v1_project_with_photo(root: &Path) -> std::path::PathBuf {
        let project = write_project(root);
        write_jpeg(&root.join("source").join("a.jpg"), 1);
        project
    }

    #[test]
    fn publish_v1_publishes_locally_without_providers_or_credentials() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let outcome = publish_project(project.to_str().unwrap(), &mut |_| {}).unwrap();
        assert_eq!(outcome.outcome, "no_change");
        assert_eq!(outcome.generation, "g-000001");
        assert!(outcome.storage.is_none());
        assert!(outcome.repository.is_none());
        assert!(outcome.hosting.is_none());
    }

    #[test]
    fn dry_run_v1_returns_a_stable_application_error() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let error = dry_run_project(project.to_str().unwrap(), &mut |_| {}).unwrap_err();
        // v1 projects have no integrated publication configuration.
        assert_eq!(error.kind, "project_invalid");
        assert!(!error.message.contains("PHOTO_PUBLISHER_"));
        assert!(!error.message.contains("panicked"));
    }

    #[test]
    fn dry_run_emits_zero_operation_events() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let mut captured: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let _ = dry_run_project(project.to_str().unwrap(), &mut |event| captured.push(event));
        assert!(
            !captured
                .iter()
                .any(|event| matches!(event, publisher_app::ApplicationEvent::Operation(_))),
            "dry-run must never emit operation events"
        );
    }

    #[test]
    fn publish_dto_serializes_deterministically() {
        let dto = PublishOutcomeDto {
            outcome: "no_change",
            generation: "g-000001".to_owned(),
            storage: None,
            repository: None,
            hosting: None,
        };
        assert_eq!(
            serde_json::to_string(&dto).unwrap(),
            serde_json::to_string(&dto).unwrap()
        );
        let json = serde_json::to_string(&dto).unwrap();
        for needle in ["PHOTO_PUBLISHER_", "token", "secret", "password", "C:\\"] {
            assert!(!json.contains(needle), "DTO leaked {needle}: {json}");
        }
    }

    #[test]
    fn recover_project_delegates_without_touching_state() {
        // Without a prior publication, recovery completes with "recovered"
        // equal to false — and the command still uses the existing sink.
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        let mut captured: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let outcome =
            recover_project(project.to_str().unwrap(), &mut |event| captured.push(event)).unwrap();
        assert!(outcome.attempted);
        assert!(!outcome.recovered);
        assert_eq!(outcome.generation, None);
        assert_eq!(
            captured.first(),
            Some(&publisher_app::ApplicationEvent::EnteredStep(
                publisher_app::WorkflowStep::RecoverPublication
            ))
        );
    }

    #[test]
    fn recover_rejects_empty_paths_deterministically() {
        let error = recover_project("   ", &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert_eq!(error.message, "project path is required");
    }

    #[test]
    fn recover_outcome_dto_is_deterministic_and_never_carries_secrets() {
        let outcome = RecoverOutcomeDto {
            attempted: true,
            recovered: true,
            generation: Some("g-000002".to_owned()),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(
            json,
            serde_json::to_string(&RecoverOutcomeDto {
                attempted: true,
                recovered: true,
                generation: Some("g-000002".to_owned()),
            })
            .unwrap()
        );
        for needle in ["PHOTO_PUBLISHER_", "token", "secret", "password"] {
            assert!(!json.contains(needle), "DTO leaked {needle}: {json}");
        }
    }

    #[test]
    fn create_project_setup_writes_exactly_one_document_via_the_contract() {
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        let payload = serde_json::json!({
            "schemaVersion": 2,
            "project": {"id": "joao-maria-2026", "name": "João & Maria"},
            "gallery": {"template": "editorial-v1", "title": "João & Maria", "bundlePath": "gallery-app"},
            "source": {"type": "folder", "path": "fotos"},
            "repository": {"provider": "github", "repository": "fotografo/joao-maria-2026", "branch": "main"},
            "hosting": {"provider": "vercel", "project": "joao-maria-2026"},
            "storage": {
                "preview": {"provider": "github", "publicBaseUrl": "https://cdn.example.com/previews"},
                "highResolution": {"provider": "r2", "accountId": "acct", "bucket": "fotografia", "publicBaseUrl": "https://downloads.example.com/originals"}
            }
        });
        let outcome = create_project_setup(target.to_str().unwrap(), payload).unwrap();
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert_eq!(outcome.project_name, "João & Maria");
        assert_eq!(outcome.project_path, target.display().to_string());
        assert!(target.is_file());
        // The resulting document must pass the same schema as the contract.
        let reloaded = publisher_app::ProjectSetup::load(&target).unwrap();
        assert_eq!(reloaded.project.id, "joao-maria-2026");
    }

    #[test]
    fn create_project_setup_rejects_a_contract_invalid_setup() {
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        let invalid = serde_json::json!({
            "schemaVersion": 2,
            "project": {"id": "has spaces", "name": "T"}
        });
        let error = create_project_setup(target.to_str().unwrap(), invalid).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert!(!target.exists(), "no document may be left behind");
    }

    #[test]
    fn create_project_setup_rejects_non_object_payloads() {
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        let error =
            create_project_setup(target.to_str().unwrap(), serde_json::json!("not an object"))
                .unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert!(!target.exists());
    }

    #[test]
    fn create_project_setup_never_reclassifies_io_failures() {
        // The destination is an existing directory: the write must fail and
        // be classified as Internal (not ResourceMissing), and the public
        // message must not contain the full user path. Same rule as Phase 7-A.
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        std::fs::create_dir(&target).unwrap();
        let payload = serde_json::json!({
            "schemaVersion": 2,
            "project": {"id": "joao-maria-2026", "name": "N"},
            "gallery": {"template": "editorial-v1", "title": "N", "bundlePath": "gallery-app"},
            "source": {"type": "folder", "path": "fotos"},
            "repository": {"provider": "github", "repository": "o/r"},
            "hosting": {"provider": "vercel", "project": "p"},
            "storage": {
                "preview": {"provider": "github", "publicBaseUrl": "https://cdn.example.com/p"},
                "highResolution": {"provider": "r2", "accountId": "a", "bucket": "b", "publicBaseUrl": "https://d.example.com/o"}
            }
        });
        let error = create_project_setup(target.to_str().unwrap(), payload).unwrap_err();
        assert_eq!(error.kind, "internal");
        assert!(!error
            .message
            .contains(target.display().to_string().as_str()));
    }

    /// A schema-valid, fully coherent v2 document for the 7-D command tests.
    fn write_coherent_v2_project(root: &Path) -> std::path::PathBuf {
        // Configuration validation reads only the document — no source
        // directory, credentials, or network are needed. The declared bundle
        // is materialized as well: Preflight (7-K.3) checks the template
        // resource on disk, and a coherent project has it available.
        let bundle = root.join("gallery-app");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("index.html"), b"<h1>template</h1>").unwrap();
        let path = root.join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "project": {"id": "joao-maria-2026", "name": "João & Maria"},
                "gallery": {"template": "editorial-v1", "title": "João & Maria", "bundlePath": "gallery-app"},
                "source": {"type": "folder", "path": "fotos"},
                "repository": {"provider": "github", "repository": "fotografo/joao-maria-2026", "branch": "main"},
                "hosting": {"provider": "vercel", "project": "joao-maria-2026", "teamId": "team_example"},
                "storage": {
                    "preview": {"provider": "github", "prefix": "previews", "publicBaseUrl": "https://cdn.example.com/previews"},
                    "highResolution": {"provider": "r2", "accountId": "account-example", "bucket": "fotografia", "prefix": "originals", "publicBaseUrl": "https://downloads.example.com/originals"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn validate_project_configuration_reports_a_coherent_v2_project() {
        let root = tempdir().unwrap();
        let path = write_coherent_v2_project(root.path());
        let outcome = validate_project_configuration(path.to_str().unwrap()).unwrap();
        assert!(outcome.valid);
        assert!(outcome.issues.is_empty());
        assert_eq!(outcome.schema_version, 2);
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert_eq!(outcome.project_name, "João & Maria");
    }

    #[test]
    fn validate_project_configuration_keeps_v1_local_projects_valid() {
        // The existing v1 fixture style declares inert provider names; v1 is
        // local-only, so no matrix rule may fire on it here either.
        let root = tempdir().unwrap();
        let path = write_project(root.path());
        let outcome = validate_project_configuration(path.to_str().unwrap()).unwrap();
        assert!(outcome.valid, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 1);
    }

    #[test]
    fn validate_project_configuration_diagnoses_an_unknown_provider() {
        let root = tempdir().unwrap();
        let path = write_coherent_v2_project(root.path());
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["repository"]["provider"] = serde_json::Value::String("gitlab".to_owned());
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let outcome = validate_project_configuration(path.to_str().unwrap()).unwrap();
        assert!(!outcome.valid);
        assert_eq!(outcome.issues.len(), 1);
        assert_eq!(outcome.issues[0].code, "unsupported_configuration");
        assert_eq!(outcome.issues[0].field, "repository.provider");
    }

    #[test]
    fn validate_project_configuration_is_deterministic_and_secret_free() {
        let root = tempdir().unwrap();
        let path = write_coherent_v2_project(root.path());
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["storage"]["highResolution"]["bucket"] = serde_json::Value::String(String::new());
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let first = validate_project_configuration(path.to_str().unwrap()).unwrap();
        let second = validate_project_configuration(path.to_str().unwrap()).unwrap();
        assert_eq!(first, second);
        assert!(!first.valid);
        assert_eq!(
            first.issues[0].code,
            "invalid_high_resolution_storage_configuration"
        );
        let json = serde_json::to_string(&first).unwrap().to_lowercase();
        for needle in ["photo_publisher_", "token", "secret", "password"] {
            assert!(!json.contains(needle), "DTO leaked {needle}");
        }
    }

    #[test]
    fn validate_project_configuration_keeps_level_1_classifications() {
        // Empty path.
        let error = validate_project_configuration("   ").unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        // Missing file.
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let error = validate_project_configuration(missing.to_str().unwrap()).unwrap_err();
        assert_eq!(error.kind, "resource_missing");
        // Schema-invalid document (source.type outside the enum): level 1
        // stays an error, never a level-2 issue list.
        let invalid = root.path().join("invalid.json");
        let mut document: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(write_coherent_v2_project(root.path())).unwrap(),
        )
        .unwrap();
        document["source"]["type"] = serde_json::Value::String("s3".to_owned());
        std::fs::write(&invalid, serde_json::to_vec(&document).unwrap()).unwrap();
        let error = validate_project_configuration(invalid.to_str().unwrap()).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
    }

    // --- Credential UX (Phase 7-E) -----------------------------------------
    //
    // These tests mutate the process environment through the composition
    // backend; they serialize on the crate-wide ENV_LOCK and always restore
    // the absent state afterwards (publication tests in this same binary
    // depend on missing credentials).

    fn clear_all_credentials() {
        for name in publisher_app::SUPPORTED_CREDENTIALS {
            let mut store = crate::composition::credential_store();
            publisher_app::delete_credential(&mut store, name).unwrap();
        }
    }

    #[test]
    fn credential_status_reports_only_the_allowlisted_names_and_bits() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        let statuses = get_credential_status().unwrap();
        assert_eq!(statuses.len(), 4);
        assert!(statuses.iter().all(|entry| !entry.configured));
        assert!(statuses.iter().all(|entry| !entry.label.is_empty()));
        assert_eq!(
            statuses
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            publisher_app::SUPPORTED_CREDENTIALS
        );
        clear_all_credentials();
    }

    #[test]
    fn set_status_and_delete_roundtrip_never_exposes_the_secret() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        let sentinel = "TEST_SECRET_SHOULD_NEVER_ESCAPE";
        set_credential("vercel.token", sentinel.to_owned()).unwrap();
        let statuses = get_credential_status().unwrap();
        assert!(
            statuses
                .iter()
                .find(|s| s.name == "vercel.token")
                .unwrap()
                .configured
        );
        let json = serde_json::to_string(&statuses).unwrap();
        assert!(!json.contains(sentinel), "status DTO leaked the secret");
        assert!(json.contains("vercel.token"), "names are public, safe data");

        let error = set_credential("foo.secret", sentinel.to_owned()).unwrap_err();
        assert_eq!(error.kind, "validation");
        assert!(!error.message.contains(sentinel));

        delete_credential("vercel.token").unwrap();
        let statuses = get_credential_status().unwrap();
        assert!(
            !statuses
                .iter()
                .find(|s| s.name == "vercel.token")
                .unwrap()
                .configured
        );
        clear_all_credentials();
    }

    #[test]
    fn credential_commands_reject_unknown_names_and_blank_secrets() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        let error = set_credential("other.token", "x".to_owned()).unwrap_err();
        assert_eq!(error.kind, "validation");
        let error = delete_credential("other.token").unwrap_err();
        assert_eq!(error.kind, "validation");
        let error = set_credential("github.token", "   ".to_owned()).unwrap_err();
        assert_eq!(error.kind, "validation");
        // Nothing was stored as a side effect of the rejected operations.
        assert!(get_credential_status()
            .unwrap()
            .iter()
            .all(|entry| !entry.configured));
        clear_all_credentials();
    }

    // --- Preflight (Phase 7-F) ----------------------------------------------
    //
    // Environment-backed credential bits are required here, so these tests
    // serialize on the crate-wide ENV_LOCK and always restore the absent
    // state afterwards.
    fn set_all_credentials(value: &str) {
        for name in publisher_app::SUPPORTED_CREDENTIALS {
            set_credential(name, value.to_owned()).unwrap();
        }
    }

    #[test]
    fn preflight_project_reports_ready_for_a_configured_v2_project() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        set_all_credentials("test-value");
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("fotos")).unwrap();
        let path = write_coherent_v2_project(root.path());
        let outcome = preflight_project(path.to_str().unwrap(), &mut |_| {}).unwrap();
        assert!(outcome.ready, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 2);
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert!(outcome.issues.is_empty());
        clear_all_credentials();
    }

    #[test]
    fn preflight_project_reports_template_unavailability_without_exposing_paths() {
        // Phase 7-K.3: the declared gallery template is a local resource the
        // integrated publication needs; when it is absent the outcome is
        // ready=false with the stable `template_unavailable` diagnosis.
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        set_all_credentials("test-value");
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("fotos")).unwrap();
        let path = write_coherent_v2_project(root.path());
        std::fs::remove_dir_all(root.path().join("gallery-app")).unwrap();
        let outcome = preflight_project(path.to_str().unwrap(), &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        assert_eq!(outcome.issues.len(), 1, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.issues[0].code, "template_unavailable");
        assert_eq!(outcome.issues[0].field, "gallery.bundlePath");
        let json = serde_json::to_string(&outcome).unwrap();
        // The declared path is the user's own configuration and may appear;
        // the resolved machine path, credential values, and environment
        // names may never appear.
        assert!(json.contains("'gallery-app'"));
        assert!(!json.contains(root.path().display().to_string().as_str()));
        for needle in ["test-value", "PHOTO_PUBLISHER_"] {
            assert!(!json.contains(needle), "preflight DTO leaked {needle}");
        }
        clear_all_credentials();
    }

    #[test]
    fn preflight_project_reports_missing_credentials_without_values() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("fotos")).unwrap();
        let path = write_coherent_v2_project(root.path());
        let outcome = preflight_project(path.to_str().unwrap(), &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        assert_eq!(outcome.issues.len(), 4);
        assert!(outcome
            .issues
            .iter()
            .all(|issue| issue.code == "credential_missing"));
        let json = serde_json::to_string(&outcome).unwrap();
        // Credential *names* are safe identifiers; values and environment
        // variable names never appear.
        assert!(json.contains("github.token"));
        assert!(json.contains("vercel.token"));
        for needle in ["test-value", "PHOTO_PUBLISHER_GITHUB_TOKEN"] {
            assert!(!json.contains(needle), "preflight DTO leaked {needle}");
        }
        clear_all_credentials();
    }

    #[test]
    fn preflight_project_needs_no_credentials_for_a_v1_project() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        let root = tempdir().unwrap();
        let path = write_project(root.path()); // existing v1 fixture (with source dir)
        let outcome = preflight_project(path.to_str().unwrap(), &mut |_| {}).unwrap();
        assert!(outcome.ready, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 1);
        clear_all_credentials();
    }

    #[test]
    fn preflight_project_keeps_level_1_classifications() {
        let _guard = crate::composition::ENV_LOCK.lock().unwrap();
        clear_all_credentials();
        let error = preflight_project("   ", &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let error = preflight_project(missing.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "resource_missing");
        clear_all_credentials();
    }

    // --- Provisioning (Phase 7-J) --------------------------------------------
    //
    // The command itself is exercised only along paths that fail BEFORE any
    // provisioner is constructed or any network is possible (the happy path
    // would require real HTTP; it is covered at the application layer with
    // stubs, and construction is covered in composition tests).

    #[test]
    fn provision_project_rejects_invalid_or_missing_documents_before_any_work() {
        // NOTE: these paths fail before any provisioner is constructed or
        // any network could happen — exactly what the adapter guarantees.
        let error = provision_project("   ", &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert!(error.report.repository.is_none());
        assert!(error.report.project_id.is_empty());

        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let error = provision_project(missing.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "resource_missing");
        assert!(error.report.repository.is_none());
    }

    #[test]
    fn provision_project_rejects_v1_and_incoherent_configs_without_network() {
        let root = tempdir().unwrap();
        // v1: local-only contract — provisioning is an explicit No here.
        let v1 = write_project(root.path());
        let error = provision_project(v1.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert!(error.report.repository.is_none());

        // Coherent shape but an unsupported provider: stopped at level 2.
        let root2 = tempdir().unwrap();
        let v2 = write_coherent_v2_project(root2.path());
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&v2).unwrap()).unwrap();
        document["hosting"]["provider"] = serde_json::Value::String("other-host".to_owned());
        std::fs::write(&v2, serde_json::to_vec(&document).unwrap()).unwrap();
        let error = provision_project(v2.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "validation");
        assert!(error.report.hosting.is_none());
    }

    #[test]
    fn provision_project_forwards_application_events_to_the_caller_sink() {
        // Phase 7-K.5: even on a failure closed before any network, the
        // command already forwards the application lifecycle — the desktop
        // channel can render "Provisioning started / failed" from it. A v1
        // project proves it without any provider contact.
        let root = tempdir().unwrap();
        let v1 = write_project(root.path());
        let mut events: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let error =
            provision_project(v1.to_str().unwrap(), &mut |event| events.push(event)).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert_eq!(
            events,
            vec![
                publisher_app::ApplicationEvent::EnteredStep(
                    publisher_app::WorkflowStep::Provisioning
                ),
                publisher_app::ApplicationEvent::LeftStep {
                    step: publisher_app::WorkflowStep::Provisioning,
                    ok: false
                },
                publisher_app::ApplicationEvent::Failed,
            ]
        );
        assert!(!events.contains(&publisher_app::ApplicationEvent::Finished));
    }

    #[test]
    fn partial_failure_reports_keep_the_completed_prefix() {
        // report_dto is the pure adapter; a partial application report maps
        // to the same partial DTO — the completed prefix is preserved, the
        // rest stays absent.
        let report = publisher_app::ProvisionProjectReport {
            project_id: "joao-maria-2026".to_owned(),
            repository: Some(publisher_app::ProvisionedResource {
                status: publisher_provisioning::ProvisioningStatus::Created,
                identity: "fotografo/joao-maria-2026".to_owned(),
            }),
            storage: Some(publisher_app::ProvisionedResource {
                status: publisher_provisioning::ProvisioningStatus::Unchanged,
                identity: "account-example/fotografia".to_owned(),
            }),
            ..Default::default()
        };
        let dto = report_dto(&report);
        assert_eq!(dto.project_id, "joao-maria-2026");
        assert_eq!(dto.repository.unwrap().status, "created");
        assert_eq!(dto.storage.unwrap().status, "unchanged");
        assert!(dto.hosting.is_none());
        assert!(dto.domain.is_none());
    }

    #[test]
    fn provisioning_dto_maps_every_disposition_and_never_carries_secrets() {
        let report = ProvisionProjectOutcomeDto {
            project_id: "p".to_owned(),
            project_name: "n".to_owned(),
            repository: Some(ProvisionedResourceDto {
                status: "created".to_owned(),
                identity: "fotografo/joao-maria-2026".to_owned(),
            }),
            storage: Some(ProvisionedResourceDto {
                status: "unchanged".to_owned(),
                identity: "account-example/fotografia".to_owned(),
            }),
            hosting: Some(ProvisionedResourceDto {
                status: "configured".to_owned(),
                identity: "joao-maria-2026".to_owned(),
            }),
            domain: Some(ProvisionedResourceDto {
                status: "changed".to_owned(),
                identity: "galeria.exemplo.com".to_owned(),
            }),
        };
        let json = serde_json::to_string(&report).unwrap();
        for expected in ["created", "unchanged", "configured", "changed"] {
            assert!(json.contains(expected), "DTO must carry {expected}");
        }
        for needle in ["PHOTO_PUBLISHER_", "secret", "token", "password"] {
            assert!(!json.to_lowercase().contains(needle), "DTO leaked {needle}");
        }
        // Deterministic serialization.
        assert_eq!(json, serde_json::to_string(&report).unwrap());
    }

    // --- Provisioning exclusion (Phase 7-J concurrency fix) ------------------
    //
    // The desktop backend — not the JavaScript — enforces "one provisioning
    // at a time". The tests exercise the exact wrapper shape: acquire the
    // single slot atomically, run the work holding the permit, and release
    // only when the work finishes.

    /// Mirrors the Tauri wrapper's acquire-then-run shape, so the tests
    /// exercise the same composition the binary performs.
    fn gated_attempt(
        gate: &ProvisioningGate,
        project_path: &str,
        work: impl FnOnce(
            &str,
            &mut EventSink<'_>,
        ) -> Result<ProvisionProjectOutcomeDto, ProvisionProjectFailureDto>,
    ) -> Result<ProvisionProjectOutcomeDto, ProvisionProjectFailureDto> {
        match gate.try_acquire() {
            Some(permit) => provision_exclusive(permit, project_path, work, &mut |_| {}),
            None => Err(provisioning_busy_failure()),
        }
    }

    fn sample_outcome() -> ProvisionProjectOutcomeDto {
        ProvisionProjectOutcomeDto {
            project_id: "joao-maria-2026".to_owned(),
            ..ProvisionProjectOutcomeDto::default()
        }
    }

    #[test]
    fn second_provisioning_is_rejected_while_the_first_is_active() {
        let gate = ProvisioningGate::new();
        // The first provisioning acquires the slot — from this moment the
        // refusal is already in force, before its blocking worker even
        // starts, exactly like the queued worker of the desktop runtime.
        let permit = gate
            .try_acquire()
            .expect("the first provisioning must acquire the slot");
        assert!(
            gate.try_acquire().is_none(),
            "a second attempt must be refused before the worker starts"
        );

        // The first provisioning runs in a worker thread and stays
        // mid-"remote call" until the test lets it finish.
        let (resume, hold) = std::sync::mpsc::channel::<()>();
        let (started, started_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            provision_exclusive(
                permit,
                "first",
                move |_, _| {
                    hold.recv().unwrap(); // the remote provisioning is still running
                    Ok(sample_outcome())
                },
                &mut |_| {},
            )
        });
        started_rx.recv().unwrap();

        // While the first provisioning is active, the second is refused
        // immediately, with the fixed busy DTO — not a worker result.
        let failure = gated_attempt(&gate, "second", |_, _| Ok(sample_outcome()))
            .expect_err("an active provisioning must block a second one");
        assert_eq!(failure.kind, PROVISIONING_BUSY_KIND);
        assert_eq!(*failure.report, ProvisionProjectOutcomeDto::default());

        // Let the first provisioning finish: the slot is released exactly
        // when its worker completes, with success.
        resume.send(()).unwrap();
        let first = worker
            .join()
            .unwrap()
            .expect("the first provisioning completed successfully");
        assert_eq!(first.project_id, "joao-maria-2026");

        // After the first finished, the slot is free and a new provisioning
        // runs normally — end to end.
        let second_run = gated_attempt(&gate, "second", |_, _| Ok(sample_outcome()))
            .expect("a new provisioning must run after the previous one finishes");
        assert_eq!(second_run.project_id, "joao-maria-2026");
    }

    #[test]
    fn the_refused_attempt_never_calls_the_application_service() {
        let gate = ProvisioningGate::new();
        // The first provisioning is active: its permit is alive.
        let _active = gate.try_acquire().expect("the first attempt acquires");

        // The refusal is the fixed busy DTO — NOT the outcome the real
        // command would have produced (a v1 project answers project_invalid
        // through the application service). Observing the busy kind proves
        // the service was never invoked for the refused attempt.
        let root = tempdir().unwrap();
        let v1 = write_project(root.path());
        let failure = gated_attempt(&gate, v1.to_str().unwrap(), provision_project)
            .expect_err("the second attempt must be refused");
        assert_eq!(failure.kind, PROVISIONING_BUSY_KIND);
        assert_eq!(*failure.report, ProvisionProjectOutcomeDto::default());

        // A counting seam confirms the same for the generic work: a refusal
        // never runs anything.
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let counted = {
            let calls = std::sync::Arc::clone(&calls);
            move |_: &str, _: &mut EventSink<'_>| {
                *calls.lock().unwrap() += 1;
                Ok(sample_outcome())
            }
        };
        let again = gated_attempt(&gate, "second", counted).expect_err("still refused");
        assert_eq!(again.kind, PROVISIONING_BUSY_KIND);
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "no work may run for a refused attempt"
        );
    }

    #[test]
    fn the_slot_is_released_after_a_successful_run() {
        let gate = ProvisioningGate::new();
        let outcome = gated_attempt(&gate, "project", |_, _| Ok(sample_outcome()))
            .expect("the first run succeeds");
        assert_eq!(outcome.project_id, "joao-maria-2026");
        // The worker finished successfully: the slot is free again...
        assert!(
            gate.try_acquire().is_some(),
            "the slot must be free after a successful run"
        );
        // ...and a full new provisioning runs normally.
        assert!(
            gated_attempt(&gate, "project", |_, _| Ok(sample_outcome())).is_ok(),
            "a new provisioning must run after the previous one finishes"
        );
    }

    #[test]
    fn the_slot_is_released_after_a_failed_run() {
        let gate = ProvisioningGate::new();
        let failure = gated_attempt(&gate, "project", |_, _| {
            Err(ProvisionProjectFailureDto {
                kind: "provisioning_failed".to_owned(),
                message: "simulated provisioning failure".to_owned(),
                report: Box::new(sample_outcome()),
            })
        })
        .expect_err("the run fails");
        assert_eq!(failure.kind, "provisioning_failed");
        // The worker finished with an error: the slot is free again.
        assert!(
            gate.try_acquire().is_some(),
            "the slot must be free after a failed run"
        );
    }

    #[test]
    fn the_slot_is_released_after_the_real_command_fails_before_any_network() {
        // The real production composition: the work is the actual command,
        // which fails at the application layer before any remote call (the
        // project document is missing). The slot must free on that error.
        let gate = ProvisioningGate::new();
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let failure = gated_attempt(&gate, missing.to_str().unwrap(), provision_project)
            .expect_err("the real command must fail on a missing document");
        assert_eq!(failure.kind, "resource_missing");
        assert!(
            gate.try_acquire().is_some(),
            "the slot must be free after the real command errored"
        );
    }

    #[test]
    fn stale_results_never_release_the_slot() {
        let gate = ProvisioningGate::new();
        let _active = gate
            .try_acquire()
            .expect("the first provisioning is active");
        // Everything the UI side can do with a stale result — receive it,
        // render it, discard it unread — operates on plain data. None of it
        // can touch the slot: the permit is unreachable from any result, so
        // the slot stays held.
        let stale_ok =
            Ok::<ProvisionProjectOutcomeDto, ProvisionProjectFailureDto>(sample_outcome());
        let stale_refusal = provisioning_busy_failure();
        let _ = format!("{stale_ok:?} {stale_refusal:?}");
        let _ = serde_json::to_string(&stale_refusal).unwrap();
        drop(stale_ok);
        drop(stale_refusal);
        assert!(
            gate.try_acquire().is_none(),
            "stale results can never release the slot"
        );
        // Only the worker finishing releases it — the guarantee the UI
        // staleness guard could never provide on its own.
        drop(_active);
        assert!(
            gate.try_acquire().is_some(),
            "the worker finishing releases the slot"
        );
    }

    #[test]
    fn acquisition_is_atomic_exactly_one_attempt_wins_across_threads() {
        // Eight concurrent attempts race for the single slot; the barrier
        // guarantees nobody releases before every attempt has happened, so
        // the outcome is deterministic: exactly one winner.
        const ATTEMPTS: usize = 8;
        let gate = std::sync::Arc::new(ProvisioningGate::new());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(ATTEMPTS));
        let wins = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let mut threads = Vec::new();
        for _ in 0..ATTEMPTS {
            let gate = std::sync::Arc::clone(&gate);
            let barrier = std::sync::Arc::clone(&barrier);
            let wins = std::sync::Arc::clone(&wins);
            threads.push(std::thread::spawn(move || {
                let permit = gate.try_acquire();
                barrier.wait(); // every attempt has now happened; nobody released yet
                if let Some(permit) = permit {
                    drop(permit); // release only after all attempts completed
                    *wins.lock().unwrap() += 1;
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            *wins.lock().unwrap(),
            1,
            "exactly one concurrent attempt may acquire the slot"
        );
        // And after the winner released, the slot works again.
        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn the_busy_refusal_carries_no_sensitive_information() {
        let failure = provisioning_busy_failure();
        assert_eq!(failure.kind, PROVISIONING_BUSY_KIND);
        // The refusal consults nothing: no path is read, no environment
        // name is touched, so nothing sensitive can leak into it.
        let json = serde_json::to_string(&failure).unwrap();
        for needle in [
            "PHOTO_PUBLISHER_",
            "token",
            "secret",
            "password",
            "c:\\",
            "http",
        ] {
            assert!(
                !json.to_lowercase().contains(needle),
                "the refusal must not carry {needle}: {json}"
            );
        }
        // Deterministic: the refusal is the same fixed data every time.
        assert_eq!(
            json,
            serde_json::to_string(&provisioning_busy_failure()).unwrap()
        );
        // Nothing ran: the report is empty and the message is public text.
        assert_eq!(*failure.report, ProvisionProjectOutcomeDto::default());
        assert!(!failure.message.is_empty());
    }

    // --- Publication exclusion (Phase 7-K.6) ---------------------------------
    //
    // The desktop backend — not the JavaScript — enforces "one publication
    // at a time". The tests mirror the provisioning exclusion contract:
    // acquire the single slot atomically, run the work holding the permit,
    // and release only when the work finishes.

    fn publish_sample_outcome() -> PublishOutcomeDto {
        PublishOutcomeDto {
            outcome: "no_change",
            generation: "g-000001".to_owned(),
            storage: None,
            repository: None,
            hosting: None,
        }
    }

    /// Mirrors the Tauri wrapper's acquire-then-run shape, so the tests
    /// exercise the same composition the binary performs.
    fn gated_publish_attempt(
        gate: &PublicationGate,
        project_path: &str,
        work: impl FnOnce(&str, &mut EventSink<'_>) -> Result<PublishOutcomeDto, CommandError>,
    ) -> Result<PublishOutcomeDto, CommandError> {
        match gate.try_acquire() {
            Some(permit) => publish_exclusive(permit, project_path, work, &mut |_| {}),
            None => Err(publication_busy_error()),
        }
    }

    #[test]
    fn second_publication_is_rejected_while_the_first_is_active() {
        let gate = PublicationGate::new();
        // The first publication acquires the slot — from this moment the
        // refusal is already in force, before its blocking worker even
        // starts, exactly like the queued worker of the desktop runtime.
        let permit = gate
            .try_acquire()
            .expect("the first publication must acquire the slot");
        assert!(
            gate.try_acquire().is_none(),
            "a second attempt must be refused before the worker starts"
        );

        // The first publication runs in a worker thread and stays
        // mid-"remote call" until the test lets it finish.
        let (resume, hold) = std::sync::mpsc::channel::<()>();
        let (started, started_rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            started.send(()).unwrap();
            publish_exclusive(
                permit,
                "first",
                move |_, _| {
                    hold.recv().unwrap(); // the remote publication is still running
                    Ok(publish_sample_outcome())
                },
                &mut |_| {},
            )
        });
        started_rx.recv().unwrap();

        // While the first publication is active, the second is refused
        // immediately, with the fixed busy error — not a worker result.
        let failure = gated_publish_attempt(&gate, "second", |_, _| Ok(publish_sample_outcome()))
            .expect_err("an active publication must block a second one");
        assert_eq!(failure.kind, PUBLICATION_BUSY_KIND);

        // Let the first publication finish: the slot is released exactly
        // when its worker completes, with success.
        resume.send(()).unwrap();
        let first = worker
            .join()
            .unwrap()
            .expect("the first publication completed successfully");
        assert_eq!(first.generation, "g-000001");

        // After the first finished, the slot is free and a new publication
        // runs normally — end to end.
        let second_run =
            gated_publish_attempt(&gate, "second", |_, _| Ok(publish_sample_outcome()))
                .expect("a new publication must run after the previous one finishes");
        assert_eq!(second_run.generation, "g-000001");
    }

    #[test]
    fn the_refused_publication_never_calls_the_application_service() {
        let gate = PublicationGate::new();
        // The first publication is active: its permit is alive.
        let _active = gate.try_acquire().expect("the first attempt acquires");

        // The refusal is the fixed busy error — NOT the error the real
        // command would have produced (a missing project answers
        // resource_missing through the application service). Observing the
        // busy kind proves the service was never invoked for the refused
        // attempt: no provider was built, no credential was read, no
        // network was reached.
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let failure = gated_publish_attempt(&gate, missing.to_str().unwrap(), publish_project)
            .expect_err("the second attempt must be refused");
        assert_eq!(failure.kind, PUBLICATION_BUSY_KIND);

        // A counting seam confirms the same for the generic work: a refusal
        // never runs anything.
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let counted = {
            let calls = std::sync::Arc::clone(&calls);
            move |_: &str, _: &mut EventSink<'_>| {
                *calls.lock().unwrap() += 1;
                Ok(publish_sample_outcome())
            }
        };
        let again = gated_publish_attempt(&gate, "second", counted).expect_err("still refused");
        assert_eq!(again.kind, PUBLICATION_BUSY_KIND);
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "no work may run for a refused attempt"
        );
    }

    #[test]
    fn the_publication_slot_is_released_after_a_successful_run() {
        let gate = PublicationGate::new();
        let outcome = gated_publish_attempt(&gate, "project", |_, _| Ok(publish_sample_outcome()))
            .expect("the first run succeeds");
        assert_eq!(outcome.generation, "g-000001");
        // The worker finished successfully: the slot is free again...
        assert!(
            gate.try_acquire().is_some(),
            "the slot must be free after a successful run"
        );
        // ...and a full new publication runs normally.
        assert!(
            gated_publish_attempt(&gate, "project", |_, _| Ok(publish_sample_outcome())).is_ok(),
            "a new publication must run after the previous one finishes"
        );
    }

    #[test]
    fn the_publication_slot_is_released_after_a_failed_run() {
        let gate = PublicationGate::new();
        let failure = gated_publish_attempt(&gate, "project", |_, _| {
            Err(CommandError {
                kind: "publication".to_owned(),
                message: "simulated publication failure".to_owned(),
            })
        })
        .expect_err("the run fails");
        assert_eq!(failure.kind, "publication");
        // The worker finished with an error: the slot is free again.
        assert!(
            gate.try_acquire().is_some(),
            "the slot must be free after a failed run"
        );
    }

    #[test]
    fn the_publication_slot_is_released_after_the_real_command_fails_before_any_network() {
        // The real production composition: the work is the actual command,
        // which fails at the application layer before any remote call (the
        // project document is missing). The slot must free on that error.
        let gate = PublicationGate::new();
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let failure = gated_publish_attempt(&gate, missing.to_str().unwrap(), publish_project)
            .expect_err("the real command must fail on a missing document");
        assert_eq!(failure.kind, "resource_missing");
        assert!(
            gate.try_acquire().is_some(),
            "the slot must be free after the real command errored"
        );
    }

    #[test]
    fn stale_publication_results_never_release_the_slot() {
        let gate = PublicationGate::new();
        let _active = gate.try_acquire().expect("the first publication is active");
        // Everything the UI side can do with a stale result — receive it,
        // render it, discard it unread — operates on plain data. None of it
        // can touch the slot: the permit is unreachable from any result, so
        // the slot stays held.
        let stale_ok = Ok::<PublishOutcomeDto, CommandError>(publish_sample_outcome());
        let stale_refusal = publication_busy_error();
        let _ = format!("{stale_ok:?} {stale_refusal:?}");
        let _ = serde_json::to_string(&stale_refusal).unwrap();
        drop(stale_ok);
        drop(stale_refusal);
        assert!(
            gate.try_acquire().is_none(),
            "stale results can never release the slot"
        );
        // Only the worker finishing releases it — the guarantee the UI
        // staleness guard could never provide on its own.
        drop(_active);
        assert!(
            gate.try_acquire().is_some(),
            "the worker finishing releases the slot"
        );
    }

    #[test]
    fn publication_acquisition_is_atomic_exactly_one_attempt_wins_across_threads() {
        // Eight concurrent attempts race for the single slot; the barrier
        // guarantees nobody releases before every attempt has happened, so
        // the outcome is deterministic: exactly one winner.
        const ATTEMPTS: usize = 8;
        let gate = std::sync::Arc::new(PublicationGate::new());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(ATTEMPTS));
        let wins = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let mut threads = Vec::new();
        for _ in 0..ATTEMPTS {
            let gate = std::sync::Arc::clone(&gate);
            let barrier = std::sync::Arc::clone(&barrier);
            let wins = std::sync::Arc::clone(&wins);
            threads.push(std::thread::spawn(move || {
                let permit = gate.try_acquire();
                barrier.wait(); // every attempt has now happened; nobody released yet
                if let Some(permit) = permit {
                    drop(permit); // release only after all attempts completed
                    *wins.lock().unwrap() += 1;
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            *wins.lock().unwrap(),
            1,
            "exactly one concurrent attempt may acquire the slot"
        );
        // And after the winner released, the slot works again.
        assert!(gate.try_acquire().is_some());
    }

    #[test]
    fn the_publication_busy_refusal_carries_no_sensitive_information() {
        let error = publication_busy_error();
        assert_eq!(error.kind, PUBLICATION_BUSY_KIND);
        // The refusal kind stays distinct from a real publication failure…
        assert_ne!(error.kind, "publication");
        // …and from the provisioning refusal (independent operations).
        assert_ne!(error.kind, PROVISIONING_BUSY_KIND);
        // The refusal consults nothing: no path is read, no environment
        // name is touched, so nothing sensitive can leak into it.
        let json = serde_json::to_string(&error).unwrap();
        for needle in [
            "PHOTO_PUBLISHER_",
            "token",
            "secret",
            "password",
            "c:\\",
            "http",
            "authorization",
        ] {
            assert!(
                !json.to_lowercase().contains(needle),
                "the publication refusal must not carry {needle}: {json}"
            );
        }
        // Deterministic: the refusal is the same fixed data every time.
        assert_eq!(
            json,
            serde_json::to_string(&publication_busy_error()).unwrap()
        );
        assert!(!error.message.is_empty());
    }

    // --- Local bootstrap (Phase 7-K.2) ----------------------------------------

    #[test]
    fn prepare_local_bootstraps_a_committed_publication_and_reports_it() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let mut captured: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let outcome =
            prepare_local_publication(project.to_str().unwrap(), &mut |event| captured.push(event))
                .unwrap();
        assert_eq!(outcome.project_id, "app-test");
        assert_eq!(outcome.project_name, "App Test");
        assert_eq!(outcome.generation, "g-000001");
        assert!(root.path().join("output").join("gallery.json").is_file());
        // The workflow lifecycle of the shared local-publication step.
        assert_eq!(
            captured,
            vec![
                publisher_app::ApplicationEvent::EnteredStep(
                    publisher_app::WorkflowStep::LocalPublication
                ),
                publisher_app::ApplicationEvent::LeftStep {
                    step: publisher_app::WorkflowStep::LocalPublication,
                    ok: true
                },
                publisher_app::ApplicationEvent::Finished,
            ]
        );
        // Idempotent: a second run reports the same generation (no new one).
        let again = prepare_local_publication(project.to_str().unwrap(), &mut |_| {}).unwrap();
        assert_eq!(again.generation, "g-000001");
    }

    #[test]
    fn prepare_local_works_for_v2_projects_too() {
        // The bootstrap is the same local operation for every schema
        // version — never artificially limited to v2 (or vice versa).
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("fotos")).unwrap();
        let path = write_coherent_v2_project(root.path());
        let outcome = prepare_local_publication(path.to_str().unwrap(), &mut |_| {}).unwrap();
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert_eq!(outcome.generation, "g-000001");
    }

    #[test]
    fn prepare_local_classifies_failures_without_internals() {
        let error = prepare_local_publication("   ", &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let error = prepare_local_publication(missing.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "resource_missing");
        assert!(!error.message.contains("panicked"));
    }

    #[test]
    fn prepare_local_rejects_concurrent_executions_through_the_pipeline_lock() {
        // The domain protection is the pipeline's own execution lock: with
        // the lock held, the bootstrap is refused without any UI guard.
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        let publisher_dir = root.path().join("output").join(".publisher");
        std::fs::create_dir_all(&publisher_dir).unwrap();
        std::fs::write(publisher_dir.join("run.lock"), b"").unwrap();
        let error = prepare_local_publication(project.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "publication_failed");
        assert!(error
            .message
            .contains("another publisher execution is active"));
        assert!(!root.path().join("output").join("gallery.json").exists());
    }
}
