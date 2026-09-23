//! Hosting execution for an [`IntegrationPlan`] over the [`HostingPublisher`]
//! port.
//!
//! The executor applies only `PublishHosting` operations from an already
//! computed plan; storage and repository operations belong to their own
//! executors and are ignored here. It never recomputes desired state and never
//! invokes the planner.
//!
//! A deployment is not idempotent: every `publish()` may create a new remote
//! deployment. The safety protocol is therefore:
//!
//! ```text
//! reconciliation requirements refused
//!     -> provenance guard (desired fingerprints/generation == ledger header)
//!     -> local validation (bundle fingerprint matches the operation)
//!     -> Pending recorded (desired fingerprint, no identity) and persisted
//!     -> publish()
//!     -> known result recorded (Confirmed with id+url, rollback, or Unknown)
//!     -> durable persistence
//! ```
//!
//! The persisted `Pending` always precedes the remote call. `Other` from the
//! real Vercel provider is deliberately ambiguous: it can be a rejected
//! request, a terminal build failure, or a 2xx whose body could not be parsed
//! after the deployment was created. Together with `Network` and `Integrity`
//! it therefore becomes `Unknown`, the ledger is persisted, and execution
//! stops: no retry, no GET, no second deployment, no overwriting of `Unknown`.
//! Only errors that provably precede deployment creation (authentication,
//! permission, missing project, conflict) restore the previous ledger state.

use std::fmt;
use std::path::PathBuf;

use photo_publisher_provider_contracts::{DeploymentInfo, ProviderError};

use crate::{
    ApplicationBundle, DesiredPublication, HostingPublicationConfig, HostingPublisher,
    IntegrationLedger, IntegrationOperation, IntegrationPlan, OperationState,
    ReconciliationRequirement, VercelPublication,
};

/// Why hosting execution failed.
///
/// Every failure stops execution immediately; the durable ledger always
/// reflects the last safely known state. The four publication outcomes are
/// distinguishable through the type system exactly like the other executors:
/// `Ok(report)` = executed and confirmed, `Err(Rejected)` = deterministically
/// rejected and rolled back, `Err(Ambiguous)` = `Unknown` persisted,
/// `Err(ReconciliationRequired)` = not executed at all.
#[derive(Debug)]
pub enum HostingExecutionError {
    /// The plan carries reconciliation requirements; nothing was executed.
    ReconciliationRequired(Vec<ReconciliationRequirement>),
    /// The plan or the provenance of the desired state contradicts the
    /// ledger handed to the executor; no ledger or remote change was made.
    InconsistentPlan(String),
    /// Local validation (bundle fingerprint) failed before any ledger or
    /// remote change.
    LocalValidation(String),
    /// The provider provably created no deployment; the previous ledger
    /// state was restored and persisted.
    Rejected(String),
    /// The deployment may have been created; `Unknown` was persisted and
    /// execution stopped without retrying, polling, or deduplicating.
    Ambiguous(String),
    /// The ledger could not be mutated or persisted. On a persistence failure
    /// the in-memory ledger is reloaded from the last durable state when
    /// possible, so the durable file remains authoritative.
    Ledger(anyhow::Error),
}

impl fmt::Display for HostingExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReconciliationRequired(requirements) => write!(
                formatter,
                "hosting execution requires reconciliation first ({} requirement(s))",
                requirements.len()
            ),
            Self::InconsistentPlan(message) => write!(formatter, "inconsistent plan: {message}"),
            Self::LocalValidation(message) => write!(formatter, "{message}"),
            Self::Rejected(message) => write!(formatter, "{message}"),
            Self::Ambiguous(message) => write!(formatter, "{message}"),
            Self::Ledger(error) => {
                write!(
                    formatter,
                    "failed to persist the integration ledger: {error}"
                )
            }
        }
    }
}

impl std::error::Error for HostingExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ledger(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Outcome of one [`HostingExecutor::execute`] run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostingExecutionReport {
    /// Present exactly when a deployment was published and its `Confirmed`
    /// state was durably persisted. Absent when the plan had no hosting
    /// operation (nothing was executed).
    pub deployment: Option<DeploymentInfo>,
}

/// Executes the `PublishHosting` operation of an [`IntegrationPlan`] against
/// a [`HostingPublisher`], persisting every ledger transition atomically.
///
/// The executor borrows the publisher, the ledger, and the ledger's durable
/// path. The bytes served come from the [`ApplicationBundle`] whose
/// fingerprint must match the planned operation exactly.
pub struct HostingExecutor<'a> {
    publisher: &'a mut dyn HostingPublisher,
    ledger: &'a mut IntegrationLedger,
    ledger_path: PathBuf,
}

impl<'a> HostingExecutor<'a> {
    pub fn new(
        publisher: &'a mut dyn HostingPublisher,
        ledger: &'a mut IntegrationLedger,
        ledger_path: impl AsRef<std::path::Path>,
    ) -> Self {
        Self {
            publisher,
            ledger,
            ledger_path: ledger_path.as_ref().to_path_buf(),
        }
    }

    /// Applies the plan's `PublishHosting` operation, if any.
    ///
    /// A plan that still carries reconciliation requirements is refused and
    /// nothing is executed. A plan without a hosting operation succeeds
    /// without any remote call.
    pub fn execute(
        &mut self,
        plan: &IntegrationPlan,
        desired: &DesiredPublication,
        bundle: &ApplicationBundle,
        configuration: &HostingPublicationConfig,
    ) -> Result<HostingExecutionReport, HostingExecutionError> {
        if !plan.reconciliation_requirements.is_empty() {
            return Err(HostingExecutionError::ReconciliationRequired(
                plan.reconciliation_requirements.clone(),
            ));
        }

        let hosting: Vec<&String> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::PublishHosting { bundle_fingerprint } => {
                    Some(bundle_fingerprint)
                }
                // Storage and repository operations are out of scope here.
                _ => None,
            })
            .collect();
        let mut report = HostingExecutionReport::default();
        let [fingerprint] = hosting.as_slice() else {
            if hosting.len() > 1 {
                return Err(HostingExecutionError::InconsistentPlan(
                    "plan contains more than one PublishHosting operation".to_owned(),
                ));
            }
            return Ok(report);
        };

        // Provenance guard: the ledger must describe the same publication the
        // plan was computed for. A mismatch means the pair was mixed up by the
        // caller; nothing remote may happen.
        if desired.configuration_fingerprint != self.ledger.configuration_fingerprint
            || desired.local.generation != self.ledger.local_generation
        {
            return Err(HostingExecutionError::InconsistentPlan(
                "desired state and integration ledger describe different publications".to_owned(),
            ));
        }

        // Guard: a Pending or Unknown hosting entry requires reconciliation;
        // the planner would already have refused this plan, and the executor
        // enforces the same invariant defensively.
        if let Some(entry) = &self.ledger.vercel {
            if entry.status != OperationState::Confirmed {
                return Err(HostingExecutionError::InconsistentPlan(format!(
                    "cannot publish hosting while the ledger marks it {}",
                    status_name(entry.status)
                )));
            }
        }

        // Local validation: publish exactly the bundle the planner saw.
        let fingerprint = fingerprint.as_str();
        if bundle.fingerprint() != fingerprint {
            return Err(HostingExecutionError::LocalValidation(format!(
                "application bundle fingerprint {} does not match the planned {fingerprint}",
                bundle.fingerprint()
            )));
        }

        let fingerprint = fingerprint.to_owned();
        let previous = self.ledger.vercel.clone();

        // Pending carries the desired fingerprint and no deployment identity:
        // the publication is in flight, not confirmed. It must be durable
        // before the remote call.
        self.record(fingerprint.clone(), OperationState::Pending, None, None)?;
        self.persist()?;

        match self.publisher.publish(bundle, configuration) {
            Ok(deployment) => {
                if deployment.id.trim().is_empty() || deployment.url.trim().is_empty() {
                    // The publish returned success but without a verifiable
                    // identity: the deployment exists remotely and we cannot
                    // prove which one it is.
                    self.record(fingerprint, OperationState::Unknown, None, None)?;
                    self.persist()?;
                    return Err(HostingExecutionError::Ambiguous(
                        "deployment published without a verifiable identity".to_owned(),
                    ));
                }
                let id = deployment.id.clone();
                let url = deployment.url.clone();
                self.record(fingerprint, OperationState::Confirmed, Some(id), Some(url))?;
                self.persist()?;
                report.deployment = Some(deployment);
                Ok(report)
            }
            Err(error) => match classify_publish_failure(&error) {
                FailureKind::NoEffect => {
                    self.rollback(previous)?;
                    self.persist()?;
                    Err(HostingExecutionError::Rejected(format!(
                        "hosting publication was rejected before any deployment was created: {error}"
                    )))
                }
                FailureKind::Ambiguous => {
                    self.record(fingerprint, OperationState::Unknown, None, None)?;
                    self.persist()?;
                    Err(HostingExecutionError::Ambiguous(format!(
                        "deployment outcome cannot be determined: {error}"
                    )))
                }
            },
        }
    }

    /// Restores the Vercel entry known before an operation that provably
    /// created no deployment: the previous entry, or no entry at all. The
    /// previous remote deployment, if any, is left untouched.
    fn rollback(
        &mut self,
        previous: Option<VercelPublication>,
    ) -> Result<(), HostingExecutionError> {
        match previous {
            None => self
                .ledger
                .remove_vercel()
                .map_err(HostingExecutionError::Ledger),
            Some(entry) => self.record(
                entry.bundle_fingerprint,
                entry.status,
                entry.deployment_id,
                entry.url,
            ),
        }
    }

    fn record(
        &mut self,
        bundle_fingerprint: String,
        status: OperationState,
        deployment_id: Option<String>,
        url: Option<String>,
    ) -> Result<(), HostingExecutionError> {
        self.ledger
            .record_vercel(bundle_fingerprint, status, deployment_id, url)
            .map_err(HostingExecutionError::Ledger)
    }

    fn persist(&mut self) -> Result<(), HostingExecutionError> {
        if let Err(error) = self.ledger.write_to(&self.ledger_path) {
            // The durable file is authoritative; drop the in-memory state that
            // could not be persisted by reloading the last durable ledger.
            if let Ok(durable) = IntegrationLedger::read_from(&self.ledger_path) {
                *self.ledger = durable;
            }
            return Err(HostingExecutionError::Ledger(error));
        }
        Ok(())
    }
}

/// Whether a publish failure provably created no deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    NoEffect,
    Ambiguous,
}

/// Classifies a `publish()` failure by what the real Vercel provider can have
/// done, not by the enum alone.
///
/// In `provider-vercel`, credential lookup happens before the POST and an HTTP
/// error response proves the request was rejected, so authentication,
/// permission, missing-project, and conflict failures cannot have created a
/// deployment. Everything else is genuinely indeterminate: `Network` may mean
/// the POST succeeded but its response was lost, or that polling timed out
/// while the deployment kept building; `Integrity` may follow a 2xx whose
/// identity could not be read; and `Other` deliberately collapses rejected
/// requests, terminal build failures, and unparseable 2xx bodies — all after
/// a deployment may exist. None of those may be retried automatically.
fn classify_publish_failure(error: &ProviderError) -> FailureKind {
    match error {
        ProviderError::AuthenticationRequired
        | ProviderError::AuthenticationFailed
        | ProviderError::PermissionDenied
        | ProviderError::NotFound
        | ProviderError::Conflict => FailureKind::NoEffect,
        ProviderError::Network
        | ProviderError::Integrity
        | ProviderError::Other
        | ProviderError::Io(_)
        | ProviderError::InvalidKey(_)
        | ProviderError::InvalidPath(_)
        | ProviderError::AlreadyExists
        | ProviderError::Unsupported => FailureKind::Ambiguous,
    }
}

fn status_name(status: OperationState) -> &'static str {
    match status {
        OperationState::Pending => "pending",
        OperationState::Confirmed => "confirmed",
        OperationState::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use photo_publisher_provider_contracts::ProviderResult;
    use sha2::{Digest, Sha256};
    use tempfile::{tempdir, TempDir};

    use crate::{plan_reconciliation, KnownRemoteState};

    fn digest(value: &str) -> String {
        format!("{:x}", Sha256::digest(value.as_bytes()))
    }

    /// Scriptable hosting port. Each publish() is logged and any call without
    /// a scripted result panics, so duplicated or unexpected publishes fail
    /// loudly. There is intentionally no status/poll API: the port has none.
    #[derive(Default)]
    struct FakeHosting {
        results: VecDeque<ProviderResult<DeploymentInfo>>,
        calls: Vec<(String, String)>,
        on_publish: Option<Box<dyn FnMut()>>,
    }

    impl HostingPublisher for FakeHosting {
        fn publish(
            &mut self,
            bundle: &ApplicationBundle,
            configuration: &HostingPublicationConfig,
        ) -> ProviderResult<DeploymentInfo> {
            self.calls.push((
                bundle.fingerprint().to_owned(),
                configuration.project_id.clone(),
            ));
            if let Some(hook) = self.on_publish.as_mut() {
                hook();
            }
            self.results.pop_front().expect("scripted publish result")
        }
    }

    struct Fixture {
        root: TempDir,
        ledger_path: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempdir().unwrap();
            let ledger_path = root.path().join(".publisher/integration-state.json");
            Self { root, ledger_path }
        }

        fn durable_ledger(&self) -> IntegrationLedger {
            IntegrationLedger::read_from(&self.ledger_path).unwrap()
        }

        fn snapshot_path(&self) -> std::path::PathBuf {
            self.root.path().join("ledger-snapshot.json")
        }
    }

    fn bundle() -> ApplicationBundle {
        ApplicationBundle::from_files(vec![("index.html".to_owned(), b"index".to_vec())]).unwrap()
    }

    fn hosting_config() -> HostingPublicationConfig {
        HostingPublicationConfig::new("hosting-project", None).unwrap()
    }

    fn configuration() -> crate::ProjectPublicationConfig {
        crate::ProjectPublicationConfig {
            project_id: "project".to_owned(),
            bundle_directory: "site".into(),
            repository_provider: "repository".to_owned(),
            repository: "owner/project".to_owned(),
            branch: Some("main".to_owned()),
            preview_prefix: Some("previews".to_owned()),
            preview_public_base_url: crate::PublicBaseUrl::parse(
                "https://cdn.example.com/previews",
            )
            .unwrap(),
            high_resolution_account_id: "account".to_owned(),
            high_resolution_bucket: Some("bucket".to_owned()),
            high_resolution_prefix: Some("originals".to_owned()),
            high_resolution_public_base_url: crate::PublicBaseUrl::parse(
                "https://downloads.example.com/originals",
            )
            .unwrap(),
            hosting: HostingPublicationConfig::new("hosting-project", None).unwrap(),
        }
    }

    fn desired(bundle: &ApplicationBundle) -> DesiredPublication {
        DesiredPublication::new(
            &configuration(),
            crate::LocalPublication::new("g-000001", digest("state"), digest("manifest")).unwrap(),
            bundle,
        )
        .unwrap()
    }

    fn ledger_for(desired: &DesiredPublication) -> IntegrationLedger {
        IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )
        .unwrap()
    }

    fn hosting_plan(fingerprint: &str) -> IntegrationPlan {
        IntegrationPlan {
            operations: vec![IntegrationOperation::PublishHosting {
                bundle_fingerprint: fingerprint.to_owned(),
            }],
            reconciliation_requirements: Vec::new(),
        }
    }

    fn execute(
        fixture: &Fixture,
        plan: &IntegrationPlan,
        desired: &DesiredPublication,
        bundle: &ApplicationBundle,
        publisher: &mut FakeHosting,
        ledger: &mut IntegrationLedger,
    ) -> Result<HostingExecutionReport, HostingExecutionError> {
        HostingExecutor::new(publisher, ledger, &fixture.ledger_path).execute(
            plan,
            desired,
            bundle,
            &hosting_config(),
        )
    }

    #[test]
    fn plan_with_reconciliation_requirements_is_refused() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut publisher = FakeHosting::default();
        let mut ledger = ledger_for(&desired);

        for requirement in [
            ReconciliationRequirement::ConfigurationChanged,
            ReconciliationRequirement::HostingPending {
                bundle_fingerprint: desired.bundle_fingerprint.clone(),
            },
            ReconciliationRequirement::HostingUnknown {
                bundle_fingerprint: desired.bundle_fingerprint.clone(),
            },
        ] {
            let blocked = IntegrationPlan {
                operations: vec![IntegrationOperation::PublishHosting {
                    bundle_fingerprint: desired.bundle_fingerprint.clone(),
                }],
                reconciliation_requirements: vec![requirement],
            };
            let error = execute(
                &fixture,
                &blocked,
                &desired,
                &bundle,
                &mut publisher,
                &mut ledger,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                HostingExecutionError::ReconciliationRequired(_)
            ));
        }
        assert_eq!(publisher.calls, Vec::new());
        assert!(ledger.vercel.is_none());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn provenance_mismatch_refuses_remote_calls() {
        for divergent_generation in [true, false] {
            let fixture = Fixture::new();
            let bundle = bundle();
            let desired = desired(&bundle);
            let mut ledger = ledger_for(&desired);
            if divergent_generation {
                ledger.local_generation = "g-000002".to_owned();
            } else {
                ledger.configuration_fingerprint = digest("other-configuration");
            }
            let mut publisher = FakeHosting::default();

            let error = execute(
                &fixture,
                &hosting_plan(&desired.bundle_fingerprint),
                &desired,
                &bundle,
                &mut publisher,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, HostingExecutionError::InconsistentPlan(_)));
            assert_eq!(publisher.calls, Vec::new());
            assert!(!fixture.ledger_path.exists());
        }
    }

    #[test]
    fn bundle_fingerprint_mismatch_is_local_and_remote_free() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut publisher = FakeHosting::default();

        let error = execute(
            &fixture,
            &hosting_plan(&digest("another-bundle")),
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, HostingExecutionError::LocalValidation(_)));
        assert_eq!(publisher.calls, Vec::new());
        assert!(ledger.vercel.is_none());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn plan_without_hosting_operation_is_a_noop() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut publisher = FakeHosting::default();
        let mut ledger = ledger_for(&desired);
        let empty = IntegrationPlan {
            operations: Vec::new(),
            reconciliation_requirements: Vec::new(),
        };

        let report = execute(
            &fixture,
            &empty,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report, HostingExecutionReport::default());
        assert_eq!(publisher.calls, Vec::new());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn multiple_publish_hosting_operations_are_rejected() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut publisher = FakeHosting::default();
        let duplicated = IntegrationPlan {
            operations: vec![
                IntegrationOperation::PublishHosting {
                    bundle_fingerprint: desired.bundle_fingerprint.clone(),
                },
                IntegrationOperation::PublishHosting {
                    bundle_fingerprint: desired.bundle_fingerprint.clone(),
                },
            ],
            reconciliation_requirements: Vec::new(),
        };

        let error = execute(
            &fixture,
            &duplicated,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, HostingExecutionError::InconsistentPlan(_)));
        // No publish attempt, no ledger mutation, nothing persisted.
        assert_eq!(publisher.calls, Vec::new());
        assert!(ledger.vercel.is_none());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn pending_or_unknown_entry_is_blocked() {
        for status in [OperationState::Pending, OperationState::Unknown] {
            let fixture = Fixture::new();
            let bundle = bundle();
            let desired = desired(&bundle);
            let mut ledger = ledger_for(&desired);
            ledger
                .record_vercel(desired.bundle_fingerprint.clone(), status, None, None)
                .unwrap();
            let mut publisher = FakeHosting::default();

            let error = execute(
                &fixture,
                &hosting_plan(&desired.bundle_fingerprint),
                &desired,
                &bundle,
                &mut publisher,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, HostingExecutionError::InconsistentPlan(_)));
            assert_eq!(publisher.calls, Vec::new());
            assert_eq!(ledger.vercel.as_ref().unwrap().status, status);
            assert!(!fixture.ledger_path.exists());
        }
    }

    /// (status, fingerprint, deployment_id, url) observed on disk mid-publish.
    type Observed = std::rc::Rc<
        std::cell::RefCell<Vec<(OperationState, String, Option<String>, Option<String>)>>,
    >;

    #[test]
    fn successful_publish_is_confirmed_with_identity_and_pending_was_durable_first() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut publisher = FakeHosting::default();
        publisher.results.push_back(Ok(DeploymentInfo {
            id: "dpl-1".to_owned(),
            url: "project.vercel.app".to_owned(),
        }));
        let observed: Observed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ledger_path = fixture.ledger_path.clone();
        let observed_in_hook = std::rc::Rc::clone(&observed);
        publisher.on_publish = Some(Box::new(move || {
            // The fundamental safety rule: the persisted Pending (desired
            // fingerprint, no identity) must exist before publish() runs.
            let durable = IntegrationLedger::read_from(&ledger_path).unwrap();
            let entry = durable.vercel.unwrap();
            observed_in_hook.borrow_mut().push((
                entry.status,
                entry.bundle_fingerprint,
                entry.deployment_id,
                entry.url,
            ));
        }));

        let report = execute(
            &fixture,
            &hosting_plan(&desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(
            *observed.borrow(),
            vec![(
                OperationState::Pending,
                desired.bundle_fingerprint.clone(),
                None,
                None
            )]
        );
        assert_eq!(publisher.calls.len(), 1);
        assert_eq!(
            publisher.calls[0],
            (
                desired.bundle_fingerprint.clone(),
                "hosting-project".to_owned()
            )
        );
        let entry = ledger.vercel.as_ref().unwrap();
        assert_eq!(entry.status, OperationState::Confirmed);
        assert_eq!(entry.bundle_fingerprint, desired.bundle_fingerprint);
        assert_eq!(entry.deployment_id.as_deref(), Some("dpl-1"));
        assert_eq!(entry.url.as_deref(), Some("project.vercel.app"));
        assert_eq!(
            report.deployment,
            Some(DeploymentInfo {
                id: "dpl-1".to_owned(),
                url: "project.vercel.app".to_owned(),
            })
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn publish_without_identity_is_ambiguous_and_recorded_unknown() {
        for deployment in [
            DeploymentInfo {
                id: String::new(),
                url: "project.vercel.app".to_owned(),
            },
            DeploymentInfo {
                id: "dpl-1".to_owned(),
                url: String::new(),
            },
        ] {
            let fixture = Fixture::new();
            let bundle = bundle();
            let desired = desired(&bundle);
            let mut ledger = ledger_for(&desired);
            let mut publisher = FakeHosting::default();
            publisher.results.push_back(Ok(deployment));

            let error = execute(
                &fixture,
                &hosting_plan(&desired.bundle_fingerprint),
                &desired,
                &bundle,
                &mut publisher,
                &mut ledger,
            )
            .unwrap_err();

            // The deployment exists remotely (publish returned Ok) but its
            // identity is unusable: this is indeterminate, not a rejection.
            assert!(matches!(error, HostingExecutionError::Ambiguous(_)));
            let entry = ledger.vercel.as_ref().unwrap();
            assert_eq!(entry.status, OperationState::Unknown);
            assert_eq!(entry.bundle_fingerprint, desired.bundle_fingerprint);
            assert_eq!(entry.deployment_id, None);
            assert_eq!(entry.url, None);
            assert_eq!(publisher.calls.len(), 1);
            assert_eq!(fixture.durable_ledger(), ledger);
        }
    }

    #[test]
    fn network_integrity_and_other_mark_unknown_and_stop() {
        for failure in [
            ProviderError::Network,
            ProviderError::Integrity,
            ProviderError::Other,
        ] {
            let fixture = Fixture::new();
            let bundle = bundle();
            let desired = desired(&bundle);
            let mut ledger = ledger_for(&desired);
            let mut publisher = FakeHosting::default();
            publisher.results.push_back(Err(failure));

            let error = execute(
                &fixture,
                &hosting_plan(&desired.bundle_fingerprint),
                &desired,
                &bundle,
                &mut publisher,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, HostingExecutionError::Ambiguous(_)));
            let entry = ledger.vercel.as_ref().unwrap();
            assert_eq!(entry.status, OperationState::Unknown);
            assert_eq!(entry.bundle_fingerprint, desired.bundle_fingerprint);
            assert_eq!(entry.deployment_id, None);
            assert_eq!(entry.url, None);
            // Exactly one publish attempt; no retry, no GET, no second deploy.
            assert_eq!(publisher.calls.len(), 1);
            assert_eq!(fixture.durable_ledger(), ledger);
        }
    }

    #[test]
    fn deterministic_rejections_roll_back_to_no_prior_entry() {
        for failure in [
            ProviderError::AuthenticationRequired,
            ProviderError::AuthenticationFailed,
            ProviderError::PermissionDenied,
            ProviderError::NotFound,
            ProviderError::Conflict,
        ] {
            let fixture = Fixture::new();
            let bundle = bundle();
            let desired = desired(&bundle);
            let mut ledger = ledger_for(&desired);
            let mut publisher = FakeHosting::default();
            publisher.results.push_back(Err(failure));

            let error = execute(
                &fixture,
                &hosting_plan(&desired.bundle_fingerprint),
                &desired,
                &bundle,
                &mut publisher,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, HostingExecutionError::Rejected(_)));
            // No prior deployment: the Pending entry is removed, durably.
            assert!(ledger.vercel.is_none());
            assert_eq!(fixture.durable_ledger(), ledger);
            assert_eq!(publisher.calls.len(), 1);
        }
    }

    #[test]
    fn deterministic_rejections_restore_the_previous_confirmed_entry() {
        let fixture = Fixture::new();
        let new_bundle =
            ApplicationBundle::from_files(vec![("index.html".to_owned(), b"new".to_vec())])
                .unwrap();
        let desired = desired(&new_bundle);
        let mut ledger = ledger_for(&desired);
        let previous = VercelPublication {
            bundle_fingerprint: digest("old-bundle"),
            status: OperationState::Confirmed,
            deployment_id: Some("dpl-old".to_owned()),
            url: Some("old.vercel.app".to_owned()),
        };
        ledger
            .record_vercel(
                previous.bundle_fingerprint.clone(),
                previous.status,
                previous.deployment_id.clone(),
                previous.url.clone(),
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();
        let snapshot = ledger.clone();
        let mut publisher = FakeHosting::default();
        publisher
            .results
            .push_back(Err(ProviderError::PermissionDenied));

        let error = execute(
            &fixture,
            &hosting_plan(&desired.bundle_fingerprint),
            &desired,
            &new_bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, HostingExecutionError::Rejected(_)));
        // The previous Confirmed deployment is restored exactly; nothing
        // remote is touched.
        assert_eq!(ledger.vercel, Some(previous));
        assert_eq!(ledger, snapshot);
        assert_eq!(fixture.durable_ledger(), snapshot);
        assert_eq!(publisher.calls.len(), 1);
    }

    #[test]
    fn published_deployment_with_lost_response_is_never_repeated() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(planned
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PublishHosting { .. })));

        let mut publisher = FakeHosting::default();
        publisher.results.push_back(Err(ProviderError::Network));
        let error = execute(
            &fixture,
            &planned,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();
        assert!(matches!(error, HostingExecutionError::Ambiguous(_)));
        assert_eq!(publisher.calls.len(), 1);
        let durable = fixture.durable_ledger();
        assert_eq!(
            durable.vercel.as_ref().unwrap().status,
            OperationState::Unknown
        );

        // Next cycle: the planner blocks on HostingUnknown; no new publish.
        let mut ledger = durable;
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(planned
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::HostingUnknown { .. }
            )));
        let mut publisher = FakeHosting::default();
        let error = execute(
            &fixture,
            &planned,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            HostingExecutionError::ReconciliationRequired(_)
        ));
        assert_eq!(publisher.calls, Vec::new());
    }

    #[test]
    fn successful_publish_without_persisted_confirmation_is_never_repeated() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();

        // Snapshot the durable Pending while publish() is in flight.
        let mut publisher = FakeHosting::default();
        publisher.results.push_back(Ok(DeploymentInfo {
            id: "dpl-1".to_owned(),
            url: "project.vercel.app".to_owned(),
        }));
        let ledger_path = fixture.ledger_path.clone();
        let snapshot_path = fixture.snapshot_path();
        publisher.on_publish = Some(Box::new(move || {
            std::fs::copy(&ledger_path, &snapshot_path).unwrap();
        }));
        execute(
            &fixture,
            &planned,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap();
        assert_eq!(publisher.calls.len(), 1);

        // Emulate a crash that lost the Confirmed write: the durable ledger is
        // back at Pending while the deployment exists remotely.
        std::fs::copy(fixture.snapshot_path(), &fixture.ledger_path).unwrap();
        let durable = fixture.durable_ledger();
        assert_eq!(
            durable.vercel.as_ref().unwrap().status,
            OperationState::Pending
        );

        let mut ledger = durable;
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(planned
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::HostingPending { .. }
            )));
        let mut publisher = FakeHosting::default();
        let error = execute(
            &fixture,
            &planned,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            HostingExecutionError::ReconciliationRequired(_)
        ));
        // No second deployment, no remote call at all.
        assert_eq!(publisher.calls, Vec::new());
    }

    #[test]
    fn crash_after_pending_blocks_the_next_cycle() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        // Durable state left by a crash after Pending was persisted and
        // before publish() was attempted.
        let mut crashed = ledger_for(&desired);
        crashed
            .record_vercel(
                desired.bundle_fingerprint.clone(),
                OperationState::Pending,
                None,
                None,
            )
            .unwrap();
        crashed.write_to(&fixture.ledger_path).unwrap();

        let mut ledger = fixture.durable_ledger();
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(!planned
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PublishHosting { .. })));

        let mut publisher = FakeHosting::default();
        let error = execute(
            &fixture,
            &planned,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            HostingExecutionError::ReconciliationRequired(_)
        ));
        assert_eq!(publisher.calls, Vec::new());
        assert_eq!(
            fixture.durable_ledger().vercel.as_ref().unwrap().status,
            OperationState::Pending
        );
    }

    #[test]
    fn confirmed_matching_fingerprint_is_never_republished() {
        let fixture = Fixture::new();
        let bundle = bundle();
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        ledger
            .record_vercel(
                desired.bundle_fingerprint.clone(),
                OperationState::Confirmed,
                Some("dpl-1".to_owned()),
                Some("project.vercel.app".to_owned()),
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();

        // The real planner sees a Confirmed hosting with the same fingerprint
        // and emits no PublishHosting operation.
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(!planned
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PublishHosting { .. })));

        let mut publisher = FakeHosting::default();
        let mut ledger = fixture.durable_ledger();
        let report = execute(
            &fixture,
            &planned,
            &desired,
            &bundle,
            &mut publisher,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report, HostingExecutionReport::default());
        assert_eq!(publisher.calls, Vec::new());
    }
}
