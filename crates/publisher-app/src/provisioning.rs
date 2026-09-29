//! Provisioning application service (Phase 7-I).
//!
//! Orchestration only. This module translates the declarative
//! `project.json` (schema v2) into the provisioning contracts from
//! `publisher-provisioning` and executes them in a fixed, auditable order —
//! with the two boundaries this phase must never blur:
//!
//! * it depends on the four `*Provisioner` *traits* and the
//!   `CredentialStore` boundary — never on `provisioner-github`,
//!   `provisioner-r2`, or `provisioner-vercel` (composition of the concrete
//!   backends stays with the caller, i.e. a future composition root);
//! * it is never called by, and never calls, the publishing flow. Publishing
//!   operates on infrastructure that already exists; provisioning here is an
//!   explicit, separately-invoked operation.
//!
//! Partial failure policy (contractual): the first failing step stops the
//! run — later resources are NOT attempted, and completed ones are never
//! rolled back. Everything that completed is preserved in the returned
//! report (the run is safe to repeat because every provisioner is
//! idempotent: a second run converges to `Unchanged`).

use std::path::Path;

use photo_publisher_provider_contracts::CredentialStore;
use publisher_provisioning::{
    DomainProvisionConfig, DomainProvisioner, HostingProvisionConfig, HostingProvisioner,
    ProvisioningError, ProvisioningOutcome, ProvisioningResult, ProvisioningStatus,
    RepositoryProvisionConfig, RepositoryProvisioner, StorageProvisionConfig, StorageProvisioner,
};

use crate::config_validation::validate_setup;
use crate::errors::{ApplicationError, ApplicationErrorKind};
use crate::events::{ApplicationEvent, EventSink, ProvisioningResource, WorkflowStep};
use crate::setup::ProjectSetup;

/// The provisioner set consumed by the application layer: contracts and the
/// secrets boundary, nothing concrete.
pub struct ProvisioningProviders<'a> {
    pub repository: &'a mut dyn RepositoryProvisioner,
    pub storage: &'a mut dyn StorageProvisioner,
    pub hosting: &'a mut dyn HostingProvisioner,
    pub domain: &'a mut dyn DomainProvisioner,
    pub credentials: &'a dyn CredentialStore,
}

/// What happened to one resource: the contractual disposition plus the
/// stable public identity the provisioner returned. Never secrets, never
/// provider payloads, never timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedResource {
    pub status: ProvisioningStatus,
    pub identity: String,
}

/// The aggregated, deterministic report of one provisioning run.
///
/// Fields are filled in the fixed execution order (`repository → storage →
/// hosting → domain`); on a partial failure the completed prefix is present
/// and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionProjectReport {
    pub project_id: String,
    pub project_name: String,
    pub repository: Option<ProvisionedResource>,
    pub storage: Option<ProvisionedResource>,
    pub hosting: Option<ProvisionedResource>,
    /// `Some` only when the project declares a domain; the current contract
    /// implementation is `Unsupported`, surfacing as an explicit failure.
    pub domain: Option<ProvisionedResource>,
}

/// The failure of one provisioning run: the partial report (completed
/// prefix preserved, no rollback implied) plus the classified error. The
/// report is boxed so the `Result` of a run stays small.
#[derive(Debug)]
pub struct ProvisioningFailure {
    pub report: Box<ProvisionProjectReport>,
    pub error: ApplicationError,
}

impl std::fmt::Display for ProvisioningFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for ProvisioningFailure {}

fn failure(report: ProvisionProjectReport, error: ApplicationError) -> ProvisioningFailure {
    ProvisioningFailure {
        report: Box::new(report),
        error,
    }
}

/// A provisioning contract failure becomes an application failure of the
/// dedicated `Provisioning` kind; the safe public message crosses verbatim
/// and the typed provider error is preserved as the cause (the contract
/// forbids secret material inside it).
fn from_provisioner(error: ProvisioningError) -> ApplicationError {
    ApplicationError::with_source(
        ApplicationErrorKind::Provisioning,
        error.message.clone(),
        error,
    )
}

/// Records one completed step in the report's fixed slot.
fn record<R: ToString>(slot: &mut Option<ProvisionedResource>, outcome: ProvisioningOutcome<R>) {
    *slot = Some(ProvisionedResource {
        status: outcome.status(),
        identity: outcome.resource().to_string(),
    });
}

/// Runs one provisioning step wrapped in its granular lifecycle events
/// (Phase 7-K.5): exactly one started marker and one finished marker per
/// call, and a failure is always marked `ok: false` — never silently. The
/// events carry the resource kind only; identities and dispositions belong
/// to the run report.
fn provision_resource<R>(
    resource: ProvisioningResource,
    events: &mut EventSink<'_>,
    work: impl FnOnce() -> ProvisioningResult<ProvisioningOutcome<R>>,
) -> ProvisioningResult<ProvisioningOutcome<R>> {
    events(ApplicationEvent::ProvisioningResourceStarted(resource));
    let result = work();
    events(ApplicationEvent::ProvisioningResourceFinished {
        resource,
        ok: result.is_ok(),
    });
    result
}

/// Provisions the infrastructure declared by an integrated (schema v2)
/// project, in the fixed order repository → high-resolution storage →
/// hosting → domain. Explicit, sequential, and stop-on-first-failure.
///
/// Emits the existing event lifecycle (`WorkflowStep::Provisioning`, plus
/// one granular marker pair per resource) so interfaces can render the run
/// live; events are observation-only and never change the outcome.
pub fn provision_project(
    project_path: &Path,
    providers: &mut ProvisioningProviders<'_>,
    events: &mut EventSink<'_>,
) -> Result<ProvisionProjectReport, ProvisioningFailure> {
    events(ApplicationEvent::EnteredStep(WorkflowStep::Provisioning));
    let result = run(project_path, providers, events);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::Provisioning,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::Provisioning,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(
    project_path: &Path,
    providers: &mut ProvisioningProviders<'_>,
    events: &mut EventSink<'_>,
) -> Result<ProvisionProjectReport, ProvisioningFailure> {
    let mut report = ProvisionProjectReport::default();

    // Level 1: the document must load through the existing contract.
    let setup = ProjectSetup::load(project_path)
        .map_err(|error| failure(ProvisionProjectReport::default(), error))?;
    report.project_id = setup.project.id.clone();
    report.project_name = setup.project.name.clone();

    // v1 is the local-only contract: it has no provisioning semantics, and
    // this service invents none.
    if setup.schema_version != 2 {
        return Err(failure(
            report,
            ApplicationError::new(
                ApplicationErrorKind::ProjectInvalid,
                "provisioning requires an integrated (schema v2) project",
            ),
        ));
    }

    // Level 2: the Phase 7-D coherence rules must pass before any provider
    // is ever called. Issues are reported by their stable codes.
    let validation = validate_setup(&setup);
    if !validation.valid {
        let codes = validation
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(failure(
            report,
            ApplicationError::new(
                ApplicationErrorKind::Validation,
                format!("the project configuration is not coherent for provisioning: {codes}"),
            ),
        ));
    }

    // repository.repository is "owner/name"; split it through the same rule
    // the publication composition uses — never inventing identity data.
    let Some((owner, name)) = setup
        .repository
        .repository
        .split_once('/')
        .filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/'))
    else {
        return Err(failure(
            report,
            ApplicationError::new(
                ApplicationErrorKind::Validation,
                "repository must be declared as 'owner/name' before provisioning",
            ),
        ));
    };
    // The declared branch is infrastructure identity for the repository
    // provisioner (Phase 7-K.4); absent means the contract default.
    let repository_config = RepositoryProvisionConfig::new(owner, name)
        .and_then(|config| config.with_branch(setup.repository.branch.clone()))
        .map_err(|error| failure(report.clone(), from_provisioner(error)))?;

    let (Some(account_id), Some(bucket)) = (
        setup.storage.high_resolution.account_id.as_deref(),
        setup.storage.high_resolution.bucket.as_deref(),
    ) else {
        return Err(failure(
            report,
            ApplicationError::new(
                ApplicationErrorKind::Validation,
                "high-resolution storage needs accountId and bucket before provisioning",
            ),
        ));
    };
    let storage_config = StorageProvisionConfig::new(account_id, bucket)
        .map_err(|error| failure(report.clone(), from_provisioner(error)))?;

    let Some(project) = setup.hosting.project.as_deref() else {
        return Err(failure(
            report,
            ApplicationError::new(
                ApplicationErrorKind::Validation,
                "hosting.project is required before provisioning",
            ),
        ));
    };
    let hosting_config = HostingProvisionConfig::new(project, setup.hosting.team_id.clone())
        .map_err(|error| failure(report.clone(), from_provisioner(error)))?;

    let domain_config = setup
        .domain
        .as_ref()
        .map(|domain| hostname_of(&domain.url))
        .transpose()
        .map_err(|message| {
            failure(
                report.clone(),
                ApplicationError::new(ApplicationErrorKind::Validation, message),
            )
        })?
        .map(|hostname| {
            DomainProvisionConfig::new(hostname)
                .map_err(|error| failure(report.clone(), from_provisioner(error)))
        })
        .transpose()?;

    // 1. Repository. The declared matrix keeps exactly ONE repository; the
    //    github preview storage rides on it (no second resource exists).
    let outcome = provision_resource(ProvisioningResource::Repository, events, || {
        providers
            .repository
            .provision_repository(&repository_config, providers.credentials)
    })
    .map_err(|error| failure(report.clone(), from_provisioner(error)))?;
    record(&mut report.repository, outcome);

    // 2. High-resolution storage bucket (prefix/publicBaseUrl are publishing
    //    concerns and deliberately never provisioning input).
    let outcome = provision_resource(ProvisioningResource::Storage, events, || {
        providers
            .storage
            .provision_storage(&storage_config, providers.credentials)
    })
    .map_err(|error| failure(report.clone(), from_provisioner(error)))?;
    record(&mut report.storage, outcome);

    // 3. Hosting project (no deployments, no files, no domain attachment).
    let outcome = provision_resource(ProvisioningResource::Hosting, events, || {
        providers
            .hosting
            .provision_hosting(&hosting_config, providers.credentials)
    })
    .map_err(|error| failure(report.clone(), from_provisioner(error)))?;
    record(&mut report.hosting, outcome);

    // 4. Domain, only when declared. Today's boundary is explicitly
    //    `Unsupported`; that error surfaces as an explicit failure — never
    //    silently downgraded to success, never worked around via Vercel.
    if let Some(domain_config) = domain_config {
        let outcome = provision_resource(ProvisioningResource::Domain, events, || {
            providers
                .domain
                .provision_domain(&domain_config, providers.credentials)
        })
        .map_err(|error| failure(report.clone(), from_provisioner(error)))?;
        record(&mut report.domain, outcome);
    }

    Ok(report)
}

/// The hostname part of a declared domain URL (scheme, path, and port are
/// not part of a domain identity). The Phase 7-D rules already guarantee
/// the URL is absolute http(s) when the project validates.
fn hostname_of(url: &str) -> Result<&str, String> {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|authority| authority.split(':').next())
        .filter(|host| !host.is_empty());
    host.ok_or_else(|| {
        "domain.url must contain a hostname before domain provisioning can run".to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contracts::ProviderResult;
    use publisher_provisioning::{
        DomainIdentity, HostingIdentity, ProvisioningErrorKind, ProvisioningResult,
        RepositoryIdentity, StorageIdentity,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use tempfile::tempdir;

    const SENTINEL: &str = "TEST_SECRET_SHOULD_NEVER_ESCAPE";

    /// In-memory credential boundary; every read is recorded by name so the
    /// tests can prove the *same* boundary was forwarded to each provisioner.
    #[derive(Default)]
    struct FakeCredentials {
        values: HashMap<String, Vec<u8>>,
        reads: RefCell<Vec<String>>,
    }

    impl CredentialStore for FakeCredentials {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            self.reads.borrow_mut().push(name.to_owned());
            Ok(self.values.get(name).cloned())
        }
        fn set(&mut self, name: &str, secret: &[u8]) -> ProviderResult<()> {
            self.values.insert(name.to_owned(), secret.to_vec());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> ProviderResult<()> {
            self.values.remove(name);
            Ok(())
        }
    }

    /// Scripted outcomes for the stubbed provisioners.
    #[derive(Clone, Copy)]
    enum Script {
        Created,
        Unchanged,
        Conflict,
        Unsupported,
    }

    /// Each stub records its invocation in the shared log and reads its
    /// expected credential name through the boundary (a handshake that is
    /// observable, and that never retains the value). The repository stub
    /// also records the branch the configuration carried.
    struct StubRepository {
        script: Script,
        log: Rc<RefCell<Vec<&'static str>>>,
        branches: Rc<RefCell<Vec<String>>>,
    }
    struct StubStorage {
        script: Script,
        log: Rc<RefCell<Vec<&'static str>>>,
    }
    struct StubHosting {
        script: Script,
        log: Rc<RefCell<Vec<&'static str>>>,
    }
    struct StubDomain {
        script: Script,
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    fn scripted<T>(script: Script, value: T) -> ProvisioningResult<ProvisioningOutcome<T>> {
        match script {
            Script::Created => Ok(ProvisioningOutcome::Created(value)),
            Script::Unchanged => Ok(ProvisioningOutcome::Unchanged(value)),
            Script::Conflict => Err(ProvisioningError::new(
                ProvisioningErrorKind::Conflict,
                "stub conflict",
            )),
            Script::Unsupported => Err(ProvisioningError::new(
                ProvisioningErrorKind::Unsupported,
                "stub unsupported",
            )),
        }
    }

    impl RepositoryProvisioner for StubRepository {
        fn provision_repository(
            &mut self,
            config: &RepositoryProvisionConfig,
            credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<RepositoryIdentity>> {
            self.log.borrow_mut().push("repository");
            self.branches.borrow_mut().push(config.branch().to_owned());
            let _ = credentials.get("github.token").map_err(|_| {
                ProvisioningError::new(ProvisioningErrorKind::Internal, "store read failed")
            })?;
            scripted(self.script, config.identity())
        }
    }

    impl StorageProvisioner for StubStorage {
        fn provision_storage(
            &mut self,
            config: &StorageProvisionConfig,
            credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<StorageIdentity>> {
            self.log.borrow_mut().push("storage");
            let _ = credentials.get("r2.secret_access_key").map_err(|_| {
                ProvisioningError::new(ProvisioningErrorKind::Internal, "store read failed")
            })?;
            scripted(self.script, config.identity())
        }
    }

    impl HostingProvisioner for StubHosting {
        fn provision_hosting(
            &mut self,
            config: &HostingProvisionConfig,
            credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<HostingIdentity>> {
            self.log.borrow_mut().push("hosting");
            let _ = credentials.get("vercel.token").map_err(|_| {
                ProvisioningError::new(ProvisioningErrorKind::Internal, "store read failed")
            })?;
            scripted(self.script, config.identity())
        }
    }

    impl DomainProvisioner for StubDomain {
        fn provision_domain(
            &mut self,
            config: &DomainProvisionConfig,
            credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<DomainIdentity>> {
            self.log.borrow_mut().push("domain");
            let _ = credentials.get("vercel.token").map_err(|_| {
                ProvisioningError::new(ProvisioningErrorKind::Internal, "store read failed")
            })?;
            scripted(self.script, config.identity())
        }
    }

    struct Rig {
        repository: StubRepository,
        storage: StubStorage,
        hosting: StubHosting,
        domain: StubDomain,
        credentials: FakeCredentials,
        log: Rc<RefCell<Vec<&'static str>>>,
        branches: Rc<RefCell<Vec<String>>>,
    }

    impl Rig {
        fn new(scripts: [Script; 4]) -> Self {
            let log = Rc::new(RefCell::new(Vec::new()));
            let branches = Rc::new(RefCell::new(Vec::new()));
            Self {
                repository: StubRepository {
                    script: scripts[0],
                    log: Rc::clone(&log),
                    branches: Rc::clone(&branches),
                },
                storage: StubStorage {
                    script: scripts[1],
                    log: Rc::clone(&log),
                },
                hosting: StubHosting {
                    script: scripts[2],
                    log: Rc::clone(&log),
                },
                domain: StubDomain {
                    script: scripts[3],
                    log: Rc::clone(&log),
                },
                credentials: FakeCredentials::default(),
                log,
                branches,
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.log.borrow().clone()
        }

        fn branches(&self) -> Vec<String> {
            self.branches.borrow().clone()
        }
    }

    fn project_document(with_domain: bool) -> serde_json::Value {
        let mut document = serde_json::json!({
            "schemaVersion": 2,
            "project": {"id": "joao-maria-2026", "name": "João & Maria"},
            "gallery": {"template": "editorial-v1", "title": "João & Maria", "bundlePath": "gallery-app"},
            "source": {"type": "folder", "path": "fotos"},
            "repository": {"provider": "github", "repository": "fotografo/joao-maria-2026", "branch": "main"},
            "hosting": {"provider": "vercel", "project": "joao-maria-2026", "teamId": "team_example"},
            "storage": {
                "preview": {"provider": "github", "prefix": "previews", "publicBaseUrl": "https://cdn.example.com/previews"},
                "highResolution": {"provider": "r2", "accountId": "account-example", "bucket": "fotografia", "publicBaseUrl": "https://d.example.com/originals"}
            }
        });
        if with_domain {
            document["domain"] = serde_json::json!({"url": "https://galeria.exemplo.com"});
        }
        document
    }

    fn write_project(root: &tempfile::TempDir, with_domain: bool) -> std::path::PathBuf {
        let path = root.path().join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&project_document(with_domain)).unwrap(),
        )
        .unwrap();
        path
    }

    fn provision(
        path: &Path,
        rig: &mut Rig,
    ) -> Result<ProvisionProjectReport, ProvisioningFailure> {
        let mut providers = ProvisioningProviders {
            repository: &mut rig.repository,
            storage: &mut rig.storage,
            hosting: &mut rig.hosting,
            domain: &mut rig.domain,
            credentials: &rig.credentials,
        };
        provision_project(path, &mut providers, &mut |_| {})
    }

    /// Same run, capturing every application event in order.
    fn provision_capturing(
        path: &Path,
        rig: &mut Rig,
        captured: &mut Vec<ApplicationEvent>,
    ) -> Result<ProvisionProjectReport, ProvisioningFailure> {
        let mut providers = ProvisioningProviders {
            repository: &mut rig.repository,
            storage: &mut rig.storage,
            hosting: &mut rig.hosting,
            domain: &mut rig.domain,
            credentials: &rig.credentials,
        };
        provision_project(path, &mut providers, &mut |event| captured.push(event))
    }

    #[test]
    fn first_run_creates_everything_in_the_fixed_order() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let report = provision(&path, &mut rig).unwrap();
        assert_eq!(rig.calls(), ["repository", "storage", "hosting"]);
        assert_eq!(
            report.repository.unwrap().status,
            ProvisioningStatus::Created
        );
        assert_eq!(report.storage.unwrap().status, ProvisioningStatus::Created);
        assert_eq!(report.hosting.unwrap().status, ProvisioningStatus::Created);
        assert!(
            report.domain.is_none(),
            "an undeclared domain is never touched"
        );
        assert_eq!(report.project_id, "joao-maria-2026");
        assert_eq!(report.project_name, "João & Maria");
    }

    #[test]
    fn a_rerun_is_unchanged_and_successful() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut rig = Rig::new([
            Script::Unchanged,
            Script::Unchanged,
            Script::Unchanged,
            Script::Unchanged,
        ]);
        let report = provision(&path, &mut rig).unwrap();
        assert_eq!(
            report.repository.unwrap().status,
            ProvisioningStatus::Unchanged
        );
        assert_eq!(
            report.storage.unwrap().status,
            ProvisioningStatus::Unchanged
        );
        assert_eq!(
            report.hosting.unwrap().status,
            ProvisioningStatus::Unchanged
        );
    }

    #[test]
    fn a_first_step_failure_stops_everything_after_it() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut rig = Rig::new([
            Script::Conflict,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let failure = provision(&path, &mut rig).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::Provisioning);
        assert_eq!(rig.calls(), ["repository"]);
        assert!(failure.report.repository.is_none());
        assert!(failure.report.storage.is_none());
        assert!(failure.report.hosting.is_none());
        assert!(failure.report.domain.is_none());
    }

    #[test]
    fn a_late_failure_preserves_the_completed_prefix_without_rollback() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Conflict,
            Script::Created,
        ]);
        let failure = provision(&path, &mut rig).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::Provisioning);
        assert_eq!(rig.calls(), ["repository", "storage", "hosting"]);
        assert_eq!(
            failure.report.repository.unwrap().status,
            ProvisioningStatus::Created
        );
        assert_eq!(
            failure.report.storage.unwrap().status,
            ProvisioningStatus::Created
        );
        assert!(failure.report.hosting.is_none());
        assert!(failure.report.domain.is_none());
    }

    #[test]
    fn an_absent_domain_never_calls_the_domain_provisioner() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Unsupported,
        ]);
        let report = provision(&path, &mut rig).unwrap();
        assert!(!rig.calls().contains(&"domain"));
        assert!(report.domain.is_none());
    }

    #[test]
    fn a_declared_domain_runs_the_boundary_and_unsupported_is_an_error() {
        let root = tempdir().unwrap();
        let path = write_project(&root, true);
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Unsupported,
        ]);
        let failure = provision(&path, &mut rig).unwrap_err();
        // Unsupported must surface as a failure, never as success.
        assert_eq!(failure.error.kind, ApplicationErrorKind::Provisioning);
        assert!(failure.error.to_string().contains("unsupported"));
        assert_eq!(rig.calls(), ["repository", "storage", "hosting", "domain"]);
        // The earlier steps completed and are preserved — no rollback.
        assert!(failure.report.repository.is_some());
        assert!(failure.report.hosting.is_some());
        assert!(failure.report.domain.is_none());
    }

    #[test]
    fn provisioners_receive_the_same_credential_boundary_and_no_secret_escapes() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut rig = Rig::new([
            Script::Conflict,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        rig.credentials
            .set("github.token", SENTINEL.as_bytes())
            .unwrap();
        rig.credentials
            .set("r2.secret_access_key", SENTINEL.as_bytes())
            .unwrap();
        rig.credentials
            .set("vercel.token", SENTINEL.as_bytes())
            .unwrap();

        let failure = provision(&path, &mut rig).unwrap_err();
        // The stubs did reach the injected boundary (handshake reads landed).
        assert!(rig
            .credentials
            .reads
            .borrow()
            .contains(&"github.token".to_owned()));
        // The sentinel never reaches any application-facing surface.
        let rendered = format!("{failure:?} {failure}");
        assert!(!rendered.contains(SENTINEL), "failure leaked the sentinel");
        assert!(!format!("{:?}", failure.report).contains(SENTINEL));
    }

    #[test]
    fn v1_projects_are_rejected_without_any_provider_call() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "project": {"id": "joao-maria-2026", "name": "João & Maria"},
                "gallery": {"template": "editorial-v1", "title": "João & Maria"},
                "source": {"type": "folder", "path": "fotos"},
                "repository": {"provider": "local", "repository": "local"},
                "hosting": {"provider": "local"},
                "storage": {"preview": {"provider": "local"}, "highResolution": {"provider": "local"}}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let failure = provision(&path, &mut rig).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::ProjectInvalid);
        assert!(
            rig.calls().is_empty(),
            "no provisioner may be called for v1"
        );
    }

    #[test]
    fn incoherent_configuration_is_rejected_before_any_call() {
        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["hosting"]["provider"] = serde_json::Value::String("other-host".to_owned());
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let failure = provision(&path, &mut rig).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::Validation);
        assert!(failure
            .error
            .to_string()
            .contains("unsupported_configuration"));
        assert!(rig.calls().is_empty());
    }

    #[test]
    fn level_1_failures_keep_their_classification_and_call_nothing() {
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let failure = provision(&missing, &mut rig).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::ResourceMissing);
        assert!(rig.calls().is_empty());
        assert_eq!(*failure.report, ProvisionProjectReport::default());
    }

    #[test]
    fn the_declared_repository_branch_reaches_the_repository_provisioner() {
        // Phase 7-K.4: repository.branch in project.json is infrastructure
        // identity and must reach the repository provisioner verbatim; an
        // undeclared branch means the contract default ("main").
        let root = tempdir().unwrap();
        let path = write_project(&root, false); // fixture declares "main"
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        provision(&path, &mut rig).unwrap();
        assert_eq!(rig.branches(), ["main"]);

        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["repository"]["branch"] = serde_json::Value::String("production".to_owned());
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        provision(&path, &mut rig).unwrap();
        assert_eq!(rig.branches(), ["production"]);

        let root = tempdir().unwrap();
        let path = write_project(&root, false);
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["repository"]
            .as_object_mut()
            .unwrap()
            .remove("branch");
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        provision(&path, &mut rig).unwrap();
        assert_eq!(rig.branches(), ["main"], "absent branch → contract default");
    }

    #[test]
    fn a_successful_run_emits_the_provisioning_lifecycle_in_fixed_order() {
        // Phase 7-K.5: the envelope is the existing use-case lifecycle, and
        // every resource gets exactly one started + one finished marker, in
        // the contractual order repository → storage → hosting → domain
        // (domain only when declared).
        let root = tempdir().unwrap();
        let path = write_project(&root, true);
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let mut events: Vec<ApplicationEvent> = Vec::new();
        provision_capturing(&path, &mut rig, &mut events).unwrap();
        use crate::events::ProvisioningResource as R;
        assert_eq!(
            events,
            vec![
                ApplicationEvent::EnteredStep(WorkflowStep::Provisioning),
                ApplicationEvent::ProvisioningResourceStarted(R::Repository),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Repository,
                    ok: true
                },
                ApplicationEvent::ProvisioningResourceStarted(R::Storage),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Storage,
                    ok: true
                },
                ApplicationEvent::ProvisioningResourceStarted(R::Hosting),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Hosting,
                    ok: true
                },
                ApplicationEvent::ProvisioningResourceStarted(R::Domain),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Domain,
                    ok: true
                },
                ApplicationEvent::LeftStep {
                    step: WorkflowStep::Provisioning,
                    ok: true
                },
                ApplicationEvent::Finished,
            ]
        );
    }

    #[test]
    fn a_failing_step_emits_its_failure_marker_and_never_a_final_finished() {
        // Stop-on-first-failure is preserved by the events too: completed
        // resources report ok, the failed one reports not-ok, nothing later
        // starts, and the run ends with Failed — never Finished.
        let root = tempdir().unwrap();
        let path = write_project(&root, true);
        let mut rig = Rig::new([
            Script::Created,
            Script::Unchanged,
            Script::Conflict,
            Script::Created,
        ]);
        let mut events: Vec<ApplicationEvent> = Vec::new();
        let failure = provision_capturing(&path, &mut rig, &mut events).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::Provisioning);
        use crate::events::ProvisioningResource as R;
        assert_eq!(
            events,
            vec![
                ApplicationEvent::EnteredStep(WorkflowStep::Provisioning),
                ApplicationEvent::ProvisioningResourceStarted(R::Repository),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Repository,
                    ok: true
                },
                ApplicationEvent::ProvisioningResourceStarted(R::Storage),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Storage,
                    ok: true
                },
                ApplicationEvent::ProvisioningResourceStarted(R::Hosting),
                ApplicationEvent::ProvisioningResourceFinished {
                    resource: R::Hosting,
                    ok: false
                },
                ApplicationEvent::LeftStep {
                    step: WorkflowStep::Provisioning,
                    ok: false
                },
                ApplicationEvent::Failed,
            ]
        );
        assert!(!events.contains(&ApplicationEvent::Finished));
        assert!(!events.iter().any(|event| matches!(
            event,
            ApplicationEvent::ProvisioningResourceStarted(R::Domain)
        ) || matches!(
            event,
            ApplicationEvent::ProvisioningResourceFinished {
                resource: R::Domain,
                ..
            }
        )));
    }

    #[test]
    fn a_v1_rejection_emits_the_envelope_without_any_resource_marker() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "project": {"id": "joao-maria-2026", "name": "João & Maria"},
                "gallery": {"template": "editorial-v1", "title": "João & Maria"},
                "source": {"type": "folder", "path": "fotos"},
                "repository": {"provider": "local", "repository": "local"},
                "hosting": {"provider": "local"},
                "storage": {"preview": {"provider": "local"}, "highResolution": {"provider": "local"}}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Created,
            Script::Created,
        ]);
        let mut events: Vec<ApplicationEvent> = Vec::new();
        let failure = provision_capturing(&path, &mut rig, &mut events).unwrap_err();
        assert_eq!(failure.error.kind, ApplicationErrorKind::ProjectInvalid);
        assert_eq!(
            events,
            vec![
                ApplicationEvent::EnteredStep(WorkflowStep::Provisioning),
                ApplicationEvent::LeftStep {
                    step: WorkflowStep::Provisioning,
                    ok: false
                },
                ApplicationEvent::Failed,
            ]
        );
        assert!(rig.calls().is_empty());
    }

    #[test]
    fn provisioning_events_carry_no_secrets_paths_or_generated_data() {
        // Events are plain taxonomy: they must never expose credentials,
        // absolute machine paths, identities, or non-deterministic data.
        let root = tempdir().unwrap();
        let path = write_project(&root, true);
        let mut rig = Rig::new([
            Script::Created,
            Script::Created,
            Script::Conflict,
            Script::Created,
        ]);
        rig.credentials
            .set("github.token", SENTINEL.as_bytes())
            .unwrap();
        let mut events: Vec<ApplicationEvent> = Vec::new();
        let _ = provision_capturing(&path, &mut rig, &mut events);
        let rendered = format!("{events:?}");
        assert!(!rendered.contains(SENTINEL));
        assert!(!rendered.contains(root.path().display().to_string().as_str()));
        for needle in ["token", "secret", "PHOTO_PUBLISHER_", "C:\\"] {
            assert!(
                !rendered.contains(needle),
                "provisioning events leaked {needle}: {rendered}"
            );
        }
    }

    #[test]
    fn the_service_never_reaches_publishing_or_concrete_providers() {
        // Structural proof: the module knows only the provisioning contracts
        // plus the credential boundary — no publishing trait, no publishing
        // crate, no concrete provisioner.
        let source = include_str!("provisioning.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "RepositoryProvider",
            "StorageProvider",
            "HostingProvider",
            "HostingPublisher",
            "provider_github",
            "provider_r2",
            "provider_vercel",
            "publisher_core",
            "photo_publisher_core",
            "photo_publisher_pipeline",
            "photo_publisher_integration",
            "publish_project",
            "dry_run_project",
            "preflight_project",
            "std::env",
            "reqwest",
            "TcpStream",
            "println!",
            "dbg!",
        ] {
            assert!(
                !source.contains(forbidden),
                "provisioning service must not reference {forbidden}"
            );
        }
        // The sanctioned seam is exactly: provisioning contracts + the
        // credential boundary.
        assert!(source.contains("publisher_provisioning::"));
        assert!(source.contains("CredentialStore"));
    }
}
