//! Integrated publication coordination as a saga ordered
//! `Storage → Repository → Hosting`.
//!
//! The Coordinator orchestrates the existing executors and nothing else. It
//! knows only the plan, the desired publication, the bundle, the hosting
//! configuration, the ledger, and the executors' contracts; it knows no
//! provider API, HTTP, credential, bucket, repository, or deployment detail.
//!
//! It is not a transaction: if any step fails, execution stops immediately,
//! earlier steps are never compensated automatically, and the durable ledger
//! state written by the executors is what governs the next cycle. The
//! Coordinator stores no state of its own: publication identity lives in the
//! ledger header (adopted here, once, before any remote effect), and every
//! resource state transition belongs to the executors.
//!
//! ```text
//! reconciliation requirements?  -> refuse, nothing remote
//! ledger Pending/Unknown?       -> refuse, nothing remote
//! provenance mismatch?          -> refuse, nothing remote
//! adopt publication header (when safe and needed) -> persist
//! StorageExecutor  (only if storage ops exist)
//! RepositoryExecutor (only if repository ops exist)
//! HostingExecutor  (only if a PublishHosting op exists)
//! ```

use std::fmt;
use std::path::{Path, PathBuf};

use photo_publisher_provider_contracts::{RepositoryProvider, StorageProvider};

use crate::{
    ApplicationBundle, DesiredPublication, HostingExecutionError, HostingExecutionReport,
    HostingExecutor, HostingPublicationConfig, HostingPublisher, IntegrationLedger,
    IntegrationOperation, IntegrationPlan, OperationState, ReconciliationRequirement,
    RepositoryExecutionError, RepositoryExecutionReport, RepositoryExecutor, StorageExecutionError,
    StorageExecutionReport, StorageExecutor,
};

/// Why an integrated publication run failed.
///
/// Executor errors are wrapped per phase, preserving their original
/// classification (for example a storage `Ambiguous` stays
/// `StorageExecutionError::Ambiguous` inside `CoordinationError::Storage`).
/// Nothing is reclassified, retried, resolved, or hidden.
#[derive(Debug)]
pub enum CoordinationError {
    /// The plan carries reconciliation requirements; nothing was adopted,
    /// persisted, or executed.
    Blocked(Vec<ReconciliationRequirement>),
    /// The durable ledger records a `Pending`/`Unknown` entry of a previous,
    /// unfinished publication; adopting a new header or executing anything
    /// would recontextualize an unsafe state, so nothing happened.
    UnsafeLedger(String),
    /// The desired publication and the ledger do not describe a consistent
    /// continuation of the same publication; nothing was executed.
    ProvenanceMismatch(String),
    /// The ledger header could not be adopted or persisted; no executor ran.
    Ledger(anyhow::Error),
    /// Storage phase failure, with its original classification.
    Storage(StorageExecutionError),
    /// Repository phase failure, with its original classification.
    Repository(RepositoryExecutionError),
    /// Hosting phase failure, with its original classification.
    Hosting(HostingExecutionError),
}

impl fmt::Display for CoordinationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blocked(requirements) => write!(
                formatter,
                "publication requires reconciliation first ({} requirement(s))",
                requirements.len()
            ),
            Self::UnsafeLedger(message) => write!(formatter, "unsafe ledger state: {message}"),
            Self::ProvenanceMismatch(message) => {
                write!(formatter, "publication provenance mismatch: {message}")
            }
            Self::Ledger(error) => {
                write!(
                    formatter,
                    "failed to persist the integration ledger: {error}"
                )
            }
            Self::Storage(error) => write!(formatter, "storage execution failed: {error}"),
            Self::Repository(error) => write!(formatter, "repository execution failed: {error}"),
            Self::Hosting(error) => write!(formatter, "hosting execution failed: {error}"),
        }
    }
}

impl std::error::Error for CoordinationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ledger(error) => Some(error.as_ref()),
            Self::Storage(error) => Some(error),
            Self::Repository(error) => Some(error),
            Self::Hosting(error) => Some(error),
            _ => None,
        }
    }
}

/// Outcome of one [`Coordinator::execute`] run.
///
/// Each field is `Some` exactly when that family had work in the plan and its
/// executor completed durably. "Published" is deliberately not stored here or
/// anywhere: it remains derivable from the durable ledger (all desired
/// entries Confirmed, hosting Confirmed with identity, header matching the
/// desired publication).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PublicationReport {
    pub storage: Option<StorageExecutionReport>,
    pub repository: Option<RepositoryExecutionReport>,
    pub hosting: Option<HostingExecutionReport>,
}

/// Runs one integrated publication as a strictly ordered saga.
///
/// The providers and the ledger are borrowed; executors are constructed only
/// for families that have operations in the plan, so no executor is ever
/// built without work to do.
pub struct Coordinator<'a> {
    storage: &'a mut dyn StorageProvider,
    repository: &'a mut dyn RepositoryProvider,
    hosting: &'a mut dyn HostingPublisher,
    ledger: &'a mut IntegrationLedger,
    ledger_path: PathBuf,
    output_dir: PathBuf,
}

impl<'a> Coordinator<'a> {
    pub fn new(
        storage: &'a mut dyn StorageProvider,
        repository: &'a mut dyn RepositoryProvider,
        hosting: &'a mut dyn HostingPublisher,
        ledger: &'a mut IntegrationLedger,
        ledger_path: impl AsRef<Path>,
        output_dir: impl AsRef<Path>,
    ) -> Self {
        Self {
            storage,
            repository,
            hosting,
            ledger,
            ledger_path: ledger_path.as_ref().to_path_buf(),
            output_dir: output_dir.as_ref().to_path_buf(),
        }
    }

    /// Executes the plan as `Storage → Repository → Hosting`.
    ///
    /// Stops at the first error and never compensates, retries, or resolves
    /// earlier phases. Safety gates (reconciliation, unsafe ledger, and
    /// provenance) run before any executor or ledger adoption.
    pub fn execute(
        &mut self,
        plan: &IntegrationPlan,
        desired: &DesiredPublication,
        bundle: &ApplicationBundle,
        hosting_configuration: &HostingPublicationConfig,
    ) -> Result<PublicationReport, CoordinationError> {
        // 1. Global reconciliation block: before any adoption or executor.
        if !plan.reconciliation_requirements.is_empty() {
            return Err(CoordinationError::Blocked(
                plan.reconciliation_requirements.clone(),
            ));
        }

        // 2. The durable ledger must not carry an unfinished previous
        //    publication. Recontextualizing Pending/Unknown under a new
        //    header would erase the memory of an operation whose remote
        //    outcome is undetermined, so this is a hard stop.
        if let Some(message) = unsafe_ledger_state(self.ledger) {
            return Err(CoordinationError::UnsafeLedger(message));
        }

        // 3. Provenance: the configuration fingerprint identifies the
        //    publication family (project, providers, targets). A mismatch
        //    means caller input was mixed up; the planner normally surfaces
        //    this as a ConfigurationChanged requirement, and the Coordinator
        //    enforces it defensively. A generation change with the same
        //    configuration is a normal new publication and is adopted below;
        //    the same generation with different content hashes is incoherent.
        if desired.configuration_fingerprint != self.ledger.configuration_fingerprint {
            return Err(CoordinationError::ProvenanceMismatch(
                "configuration fingerprint differs between desired state and ledger".to_owned(),
            ));
        }
        if self.ledger.local_generation == desired.local.generation
            && (self.ledger.state_hash != desired.local.state_hash
                || self.ledger.manifest_hash != desired.local.manifest_hash)
        {
            return Err(CoordinationError::ProvenanceMismatch(
                "same generation with different publication hashes".to_owned(),
            ));
        }

        // 4. Adopt the new publication header when it diverges, and persist
        //    it before any executor can produce a remote effect. If the
        //    adoption cannot be persisted, nothing runs.
        let header_matches = self.ledger.local_generation == desired.local.generation
            && self.ledger.state_hash == desired.local.state_hash
            && self.ledger.manifest_hash == desired.local.manifest_hash;
        if !header_matches {
            self.ledger
                .adopt_publication(
                    desired.configuration_fingerprint.clone(),
                    desired.local.generation.clone(),
                    desired.local.state_hash.clone(),
                    desired.local.manifest_hash.clone(),
                )
                .map_err(CoordinationError::Ledger)?;
            self.persist()?;
        }

        // 5–7. Strictly sequential dispatch; never parallel, never reordered,
        // each executor at most once, only when the plan has work for it.
        let mut report = PublicationReport::default();

        let has_storage = plan.operations.iter().any(|operation| {
            matches!(
                operation,
                IntegrationOperation::PutStorage(_) | IntegrationOperation::DeleteStorage { .. }
            )
        });
        if has_storage {
            let storage = StorageExecutor::new(
                &self.output_dir,
                &mut *self.storage,
                &mut *self.ledger,
                &self.ledger_path,
            )
            .map_err(CoordinationError::Storage)?
            .execute(plan)
            .map_err(CoordinationError::Storage)?;
            report.storage = Some(storage);
        }

        let has_repository = plan.operations.iter().any(|operation| {
            matches!(
                operation,
                IntegrationOperation::WriteRepository(_)
                    | IntegrationOperation::DeleteRepository { .. }
            )
        });
        if has_repository {
            let repository = RepositoryExecutor::new(
                &mut *self.repository,
                &mut *self.ledger,
                &self.ledger_path,
                &self.output_dir,
            )
            .map_err(CoordinationError::Repository)?
            .execute(plan, bundle)
            .map_err(CoordinationError::Repository)?;
            report.repository = Some(repository);
        }

        let has_hosting = plan
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PublishHosting { .. }));
        if has_hosting {
            let hosting =
                HostingExecutor::new(&mut *self.hosting, &mut *self.ledger, &self.ledger_path)
                    .execute(plan, desired, bundle, hosting_configuration)
                    .map_err(CoordinationError::Hosting)?;
            report.hosting = Some(hosting);
        }

        Ok(report)
    }

    fn persist(&mut self) -> Result<(), CoordinationError> {
        if let Err(error) = self.ledger.write_to(&self.ledger_path) {
            // The durable file is authoritative; drop the in-memory state that
            // could not be persisted by reloading the last durable ledger.
            if let Ok(durable) = IntegrationLedger::read_from(&self.ledger_path) {
                *self.ledger = durable;
            }
            return Err(CoordinationError::Ledger(error));
        }
        Ok(())
    }
}

/// Returns a description of the first ledger entry that is not `Confirmed`,
/// if any exists. Such entries mean a previous publication did not conclude
/// safely and must block any new run.
fn unsafe_ledger_state(ledger: &IntegrationLedger) -> Option<String> {
    if let Some((key, entry)) = ledger
        .r2_inventory
        .iter()
        .find(|(_, entry)| entry.status != OperationState::Confirmed)
    {
        return Some(format!(
            "storage object {} is {}",
            key,
            status_name(entry.status)
        ));
    }
    if let Some((path, entry)) = ledger
        .github_inventory
        .iter()
        .find(|(_, entry)| entry.status != OperationState::Confirmed)
    {
        return Some(format!(
            "repository file {} is {}",
            path,
            status_name(entry.status)
        ));
    }
    if let Some(vercel) = &ledger.vercel {
        if vercel.status != OperationState::Confirmed {
            return Some(format!(
                "hosting publication is {}",
                status_name(vercel.status)
            ));
        }
    }
    None
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
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::io::Read;
    use std::rc::Rc;

    use photo_publisher_core::{state_from, SourcePhoto};
    use photo_publisher_pipeline::journal::{sha256_file, JournalPhase, JournalRecord};
    use photo_publisher_provider_contracts::{
        CommitInfo, DeploymentInfo, FileMetadata, ObjectKey, ProviderError, ProviderResult,
        RepositoryPath, StorageObject,
    };
    use sha2::{Digest, Sha256};
    use tempfile::{tempdir, TempDir};

    use crate::{
        plan_reconciliation, DesiredRepositoryFile, DesiredStorageObject, LocalPublication,
        PublicationPath, RepositoryFileSource,
    };

    fn digest(value: &str) -> String {
        format!("{:x}", Sha256::digest(value.as_bytes()))
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// Shared cross-provider call log used to prove the execution order.
    type SharedLog = Rc<RefCell<Vec<&'static str>>>;

    #[derive(Default)]
    struct FakeStorage {
        log: SharedLog,
        objects: HashMap<String, (u64, String, Option<String>)>,
        put_scripts: VecDeque<ProviderResult<()>>,
        delete_scripts: VecDeque<ProviderResult<()>>,
    }

    impl StorageProvider for FakeStorage {
        fn put(
            &mut self,
            key: &ObjectKey,
            content: &mut dyn Read,
            content_type: Option<&str>,
        ) -> ProviderResult<StorageObject> {
            self.log.borrow_mut().push("storage");
            self.put_scripts.pop_front().unwrap_or(Ok(()))?;
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(ProviderError::from_io)?;
            let object = StorageObject {
                key: key.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_bytes(&bytes),
                content_type: content_type.map(str::to_owned),
            };
            self.objects.insert(
                key.as_str().to_owned(),
                (
                    object.size_bytes,
                    object.sha256.clone(),
                    object.content_type.clone(),
                ),
            );
            Ok(object)
        }

        fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
            Ok(self
                .objects
                .get(key.as_str())
                .map(|(size_bytes, sha256, content_type)| StorageObject {
                    key: key.clone(),
                    size_bytes: *size_bytes,
                    sha256: sha256.clone(),
                    content_type: content_type.clone(),
                }))
        }

        fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
            self.log.borrow_mut().push("storage");
            self.delete_scripts.pop_front().unwrap_or(Ok(()))?;
            self.objects.remove(key.as_str());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRepository {
        log: SharedLog,
        files: HashMap<String, Vec<u8>>,
        revision: String,
        staged: HashMap<String, Option<Vec<u8>>>,
        commit_scripts: VecDeque<ProviderResult<()>>,
        commit_count: usize,
    }

    impl FakeRepository {
        fn new(log: &SharedLog) -> Self {
            Self {
                log: Rc::clone(log),
                revision: "base-revision".to_owned(),
                ..Self::default()
            }
        }
    }

    impl RepositoryProvider for FakeRepository {
        fn ensure_repository(&mut self) -> ProviderResult<()> {
            self.log.borrow_mut().push("repository");
            Ok(())
        }

        fn exists(&self, path: &RepositoryPath) -> ProviderResult<bool> {
            Ok(self.files.contains_key(path.as_str()))
        }

        fn read(&self, path: &RepositoryPath) -> ProviderResult<Vec<u8>> {
            self.files
                .get(path.as_str())
                .cloned()
                .ok_or(ProviderError::NotFound)
        }

        fn write(
            &mut self,
            path: &RepositoryPath,
            content: &mut dyn Read,
        ) -> ProviderResult<FileMetadata> {
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(ProviderError::from_io)?;
            let metadata = FileMetadata {
                path: path.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_bytes(&bytes),
            };
            self.staged.insert(path.as_str().to_owned(), Some(bytes));
            Ok(metadata)
        }

        fn delete(&mut self, path: &RepositoryPath) -> ProviderResult<()> {
            self.staged.insert(path.as_str().to_owned(), None);
            Ok(())
        }

        fn commit(&mut self, message: &str) -> ProviderResult<CommitInfo> {
            if self.staged.is_empty() {
                // Base revision probe: reads only, publishes nothing.
                return Ok(CommitInfo {
                    revision: self.revision.clone(),
                    message: message.to_owned(),
                    changed_paths: Vec::new(),
                });
            }
            self.commit_count += 1;
            // Like the real provider, staged changes survive a failed commit.
            self.commit_scripts.pop_front().unwrap_or(Ok(()))?;
            let staged = std::mem::take(&mut self.staged);
            let mut changed_paths: Vec<RepositoryPath> = staged
                .keys()
                .map(|path| RepositoryPath::new(path).unwrap())
                .collect();
            changed_paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            for (path, change) in staged {
                match change {
                    Some(bytes) => {
                        self.files.insert(path, bytes);
                    }
                    None => {
                        self.files.remove(&path);
                    }
                }
            }
            self.revision = format!("commit-{:02}", self.commit_count);
            Ok(CommitInfo {
                revision: self.revision.clone(),
                message: message.to_owned(),
                changed_paths,
            })
        }
    }

    #[derive(Default)]
    struct FakeHosting {
        log: SharedLog,
        results: VecDeque<ProviderResult<DeploymentInfo>>,
    }

    impl HostingPublisher for FakeHosting {
        fn publish(
            &mut self,
            bundle: &ApplicationBundle,
            _configuration: &HostingPublicationConfig,
        ) -> ProviderResult<DeploymentInfo> {
            self.log.borrow_mut().push("hosting");
            let _ = bundle;
            self.results.pop_front().expect("scripted publish result")
        }
    }

    struct Providers {
        storage: FakeStorage,
        repository: FakeRepository,
        hosting: FakeHosting,
    }

    impl Providers {
        fn new(log: &SharedLog) -> Self {
            Self {
                storage: FakeStorage {
                    log: Rc::clone(log),
                    ..FakeStorage::default()
                },
                repository: FakeRepository::new(log),
                hosting: FakeHosting {
                    log: Rc::clone(log),
                    ..FakeHosting::default()
                },
            }
        }
    }

    struct Fixture {
        root: TempDir,
        output: std::path::PathBuf,
        ledger_path: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempdir().unwrap();
            let output = root.path().join("output");
            std::fs::create_dir_all(&output).unwrap();
            let ledger_path = root.path().join(".publisher/integration-state.json");
            Self {
                root,
                output,
                ledger_path,
            }
        }

        fn durable_ledger(&self) -> IntegrationLedger {
            IntegrationLedger::read_from(&self.ledger_path).unwrap()
        }

        fn source(&self, name: &str, bytes: &[u8]) -> DesiredStorageObject {
            let relative = format!("photos/download/{name}.jpg");
            let path = self.output.join(&relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
            DesiredStorageObject::new(
                ObjectKey::new(format!("originals/{name}.jpg")).unwrap(),
                PublicationPath::new(relative).unwrap(),
                sha256_bytes(bytes),
                bytes.len() as u64,
                Some("image/jpeg".to_owned()),
            )
            .unwrap()
        }
    }

    fn bundle_of(files: &[(&str, &[u8])]) -> ApplicationBundle {
        ApplicationBundle::from_files(
            files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_vec()))
                .collect(),
        )
        .unwrap()
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
        desired_gen(bundle, "g-000001")
    }

    fn desired_gen(bundle: &ApplicationBundle, generation: &str) -> DesiredPublication {
        DesiredPublication::new(
            &configuration(),
            LocalPublication::new(
                generation,
                digest(&format!("state-{generation}")),
                digest(&format!("manifest-{generation}")),
            )
            .unwrap(),
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

    fn empty_plan() -> IntegrationPlan {
        IntegrationPlan {
            operations: Vec::new(),
            reconciliation_requirements: Vec::new(),
        }
    }

    fn execute(
        fixture: &Fixture,
        plan: &IntegrationPlan,
        desired: &DesiredPublication,
        bundle: &ApplicationBundle,
        providers: &mut Providers,
        ledger: &mut IntegrationLedger,
    ) -> Result<PublicationReport, CoordinationError> {
        Coordinator::new(
            &mut providers.storage,
            &mut providers.repository,
            &mut providers.hosting,
            ledger,
            &fixture.ledger_path,
            &fixture.output,
        )
        .execute(
            plan,
            desired,
            bundle,
            &HostingPublicationConfig::new("hosting-project", None).unwrap(),
        )
    }

    #[test]
    fn empty_plan_with_matching_header_calls_nothing() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        ledger.write_to(&fixture.ledger_path).unwrap();
        let before = ledger.clone();
        let mut providers = Providers::new(&log);

        let report = execute(
            &fixture,
            &empty_plan(),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report, PublicationReport::default());
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert_eq!(providers.repository.commit_count, 0);
        assert!(providers.storage.objects.is_empty());
        assert_eq!(ledger, before);
        assert_eq!(fixture.durable_ledger(), before);
    }

    #[test]
    fn empty_plan_with_divergent_header_adopts_without_executors() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let old_desired = desired(&bundle);
        let mut ledger = ledger_for(&old_desired); // header: g-000001
        let new_desired = desired_gen(&bundle, "g-000002");
        let mut providers = Providers::new(&log);

        let report = execute(
            &fixture,
            &empty_plan(),
            &new_desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report, PublicationReport::default());
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert_eq!(providers.repository.commit_count, 0);
        // Only the header moved to the new publication.
        assert_eq!(ledger.local_generation, "g-000002");
        assert_eq!(ledger.state_hash, new_desired.local.state_hash);
        assert_eq!(ledger.manifest_hash, new_desired.local.manifest_hash);
        assert_eq!(
            ledger.configuration_fingerprint,
            new_desired.configuration_fingerprint
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn adoption_preserves_all_inventories_and_ledger_integrity() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let old_desired = desired(&bundle);
        let mut ledger = ledger_for(&old_desired);
        let key = ObjectKey::new("originals/a.jpg").unwrap();
        ledger
            .record_r2_object(
                &key,
                &StorageObject {
                    key: key.clone(),
                    size_bytes: 3,
                    sha256: digest("jpg"),
                    content_type: Some("image/jpeg".to_owned()),
                },
                OperationState::Confirmed,
            )
            .unwrap();
        ledger
            .record_github_file(
                &RepositoryPath::new("index.html").unwrap(),
                sha256_bytes(b"index"),
                "commit-01".to_owned(),
                OperationState::Confirmed,
            )
            .unwrap();
        ledger
            .record_vercel(
                old_desired.bundle_fingerprint.clone(),
                OperationState::Confirmed,
                Some("dpl-1".to_owned()),
                Some("project.vercel.app".to_owned()),
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();
        let before = ledger.clone();

        let new_desired = desired_gen(&bundle, "g-000002");
        let mut providers = Providers::new(&log);
        execute(
            &fixture,
            &empty_plan(),
            &new_desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        // Inventories and hosting entry are untouched by the adoption.
        assert_eq!(ledger.r2_inventory, before.r2_inventory);
        assert_eq!(ledger.github_inventory, before.github_inventory);
        assert_eq!(ledger.vercel, before.vercel);
        // Header moved; schema and integrity hold across writes and reloads.
        assert_eq!(ledger.local_generation, "g-000002");
        assert_eq!(ledger.schema_version, 1);
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        let reloaded = fixture.durable_ledger();
        assert_eq!(reloaded, ledger);
        assert_eq!(
            serde_json::to_value(&reloaded).unwrap()["schemaVersion"],
            serde_json::json!(1)
        );
    }

    #[test]
    fn reconciliation_requirements_block_everything_without_adopting() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let old_desired = desired(&bundle);
        let mut ledger = ledger_for(&old_desired); // header stays g-000001
        let new_desired = desired_gen(&bundle, "g-000002");
        let mut providers = Providers::new(&log);
        let blocked = IntegrationPlan {
            operations: vec![
                IntegrationOperation::PutStorage(object),
                IntegrationOperation::WriteRepository(
                    DesiredRepositoryFile::new(
                        RepositoryPath::new("index.html").unwrap(),
                        sha256_bytes(b"index"),
                        5,
                        RepositoryFileSource::BundleMember,
                    )
                    .unwrap(),
                ),
                IntegrationOperation::PublishHosting {
                    bundle_fingerprint: new_desired.bundle_fingerprint.clone(),
                },
            ],
            reconciliation_requirements: vec![ReconciliationRequirement::ConfigurationChanged],
        };

        let error = execute(
            &fixture,
            &blocked,
            &new_desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, CoordinationError::Blocked(_)));
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert_eq!(providers.repository.commit_count, 0);
        // The header is not adopted and nothing is persisted.
        assert_eq!(ledger.local_generation, "g-000001");
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn adoption_persistence_failure_blocks_the_run() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let old_desired = desired(&bundle);
        let mut ledger = ledger_for(&old_desired);
        let new_desired = desired_gen(&bundle, "g-000002");
        // Put the ledger path under an existing FILE so the atomic write
        // cannot create its parent directory.
        let blocker = fixture.root.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let ledger_path = blocker.join("ledger.json");
        let mut providers = Providers::new(&log);
        providers.hosting.results.push_back(Ok(DeploymentInfo {
            id: "dpl-1".to_owned(),
            url: "project.vercel.app".to_owned(),
        }));

        let error = Coordinator::new(
            &mut providers.storage,
            &mut providers.repository,
            &mut providers.hosting,
            &mut ledger,
            &ledger_path,
            &fixture.output,
        )
        .execute(
            &empty_plan(),
            &new_desired,
            &bundle,
            &HostingPublicationConfig::new("hosting-project", None).unwrap(),
        )
        .unwrap_err();

        assert!(matches!(error, CoordinationError::Ledger(_)));
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert_eq!(providers.repository.commit_count, 0);
        assert!(!ledger_path.exists());
    }

    #[test]
    fn previous_pending_blocks_adoption_and_execution() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let old_desired = desired(&bundle);
        let mut ledger = ledger_for(&old_desired);
        ledger
            .record_r2_object(
                &object.key,
                &StorageObject {
                    key: object.key.clone(),
                    size_bytes: object.size_bytes,
                    sha256: object.sha256.clone(),
                    content_type: object.content_type.clone(),
                },
                OperationState::Pending,
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();
        let before = ledger.clone();
        let new_desired = desired_gen(&bundle, "g-000002");
        let mut providers = Providers::new(&log);
        // Adversarial: a hand-built plan pretending nothing is pending.
        let silent_plan = IntegrationPlan {
            operations: vec![IntegrationOperation::PutStorage(object)],
            reconciliation_requirements: Vec::new(),
        };

        let error = execute(
            &fixture,
            &silent_plan,
            &new_desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, CoordinationError::UnsafeLedger(_)));
        // No adoption, no persist, no remote call; Pending preserved.
        assert_eq!(ledger, before);
        assert_eq!(fixture.durable_ledger(), before);
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert!(providers.storage.objects.is_empty());
    }

    #[test]
    fn previous_unknown_blocks_adoption_and_execution() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let old_desired = desired(&bundle);
        let mut ledger = ledger_for(&old_desired);
        ledger
            .record_vercel(
                old_desired.bundle_fingerprint.clone(),
                OperationState::Unknown,
                None,
                None,
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();
        let before = ledger.clone();
        let new_desired = desired_gen(&bundle, "g-000002");
        let mut providers = Providers::new(&log);
        let silent_plan = IntegrationPlan {
            operations: vec![IntegrationOperation::PublishHosting {
                bundle_fingerprint: new_desired.bundle_fingerprint.clone(),
            }],
            reconciliation_requirements: Vec::new(),
        };

        let error = execute(
            &fixture,
            &silent_plan,
            &new_desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, CoordinationError::UnsafeLedger(_)));
        assert_eq!(ledger, before);
        assert_eq!(fixture.durable_ledger(), before);
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
    }

    #[test]
    fn configuration_mismatch_is_a_provenance_error() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let mut ledger = ledger_for(&desired(&bundle));
        ledger.configuration_fingerprint = digest("other-configuration");
        let desired = desired(&bundle);
        let mut providers = Providers::new(&log);

        let error = execute(
            &fixture,
            &empty_plan(),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, CoordinationError::ProvenanceMismatch(_)));
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn same_generation_with_different_hashes_is_a_provenance_error() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let mut ledger = ledger_for(&desired(&bundle));
        ledger.state_hash = digest("tampered-state");
        let desired = desired(&bundle);
        let mut providers = Providers::new(&log);

        let error = execute(
            &fixture,
            &empty_plan(),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, CoordinationError::ProvenanceMismatch(_)));
        assert_eq!(*log.borrow(), Vec::<&'static str>::new());
        assert!(!fixture.ledger_path.exists());
    }

    /// A plan exercising all three families in one run.
    fn full_plan(object: &DesiredStorageObject, bundle_fingerprint: &str) -> IntegrationPlan {
        IntegrationPlan {
            operations: vec![
                IntegrationOperation::PutStorage(object.clone()),
                IntegrationOperation::WriteRepository(
                    DesiredRepositoryFile::new(
                        RepositoryPath::new("index.html").unwrap(),
                        sha256_bytes(b"index"),
                        5,
                        RepositoryFileSource::BundleMember,
                    )
                    .unwrap(),
                ),
                IntegrationOperation::PublishHosting {
                    bundle_fingerprint: bundle_fingerprint.to_owned(),
                },
            ],
            reconciliation_requirements: Vec::new(),
        }
    }

    fn ready_hosting() -> DeploymentInfo {
        DeploymentInfo {
            id: "dpl-1".to_owned(),
            url: "project.vercel.app".to_owned(),
        }
    }

    #[test]
    fn full_success_runs_storage_then_repository_then_hosting() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers.hosting.results.push_back(Ok(ready_hosting()));

        let report = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        // Structural order proof: one storage call, one repository execution
        // (single commit), one publish — in exactly this order, once each.
        assert_eq!(*log.borrow(), vec!["storage", "repository", "hosting"]);
        assert_eq!(providers.repository.commit_count, 1);
        assert_eq!(providers.hosting.results.len(), 0);
        assert!(providers.storage.objects.contains_key(object.key.as_str()));
        assert!(report.storage.is_some());
        assert!(report.repository.is_some());
        let hosting = report.hosting.unwrap();
        assert_eq!(
            hosting.deployment.as_ref().map(|d| d.id.as_str()),
            Some("dpl-1")
        );

        // Every family is Confirmed and durably persisted; nothing Pending or
        // Unknown remains.
        let storage_entry = &ledger.r2_inventory[object.key.as_str()];
        assert_eq!(storage_entry.status, OperationState::Confirmed);
        assert_eq!(storage_entry.sha256, object.sha256);
        let repo_entry = &ledger.github_inventory["index.html"];
        assert_eq!(repo_entry.status, OperationState::Confirmed);
        assert_eq!(repo_entry.revision, "commit-01");
        let hosting_entry = ledger.vercel.as_ref().unwrap();
        assert_eq!(hosting_entry.status, OperationState::Confirmed);
        assert_eq!(hosting_entry.deployment_id.as_deref(), Some("dpl-1"));
        assert_eq!(hosting_entry.url.as_deref(), Some("project.vercel.app"));
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn storage_failure_stops_before_repository_and_hosting() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers
            .storage
            .put_scripts
            .push_back(Err(ProviderError::PermissionDenied));

        let error = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        // The original classification is preserved, not reclassified.
        assert!(matches!(
            error,
            CoordinationError::Storage(StorageExecutionError::Rejected { .. })
        ));
        assert_eq!(*log.borrow(), vec!["storage"]);
        // Later phases never ran: no commit, no publish.
        assert_eq!(providers.repository.commit_count, 0);
        assert_eq!(providers.hosting.results.len(), 0);
        // The deterministic rejection was rolled back: nothing is pending.
        assert!(ledger.r2_inventory.is_empty());
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn storage_ambiguity_stops_before_repository_and_hosting() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers
            .storage
            .put_scripts
            .push_back(Err(ProviderError::Network));

        let error = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CoordinationError::Storage(StorageExecutionError::Ambiguous { .. })
        ));
        assert_eq!(*log.borrow(), vec!["storage"]);
        assert_eq!(providers.repository.commit_count, 0);
        assert_eq!(providers.hosting.results.len(), 0);
        assert_eq!(
            ledger.r2_inventory[object.key.as_str()].status,
            OperationState::Unknown
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn repository_failure_stops_before_hosting_without_compensating_storage() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers
            .repository
            .commit_scripts
            .push_back(Err(ProviderError::PermissionDenied));

        let error = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CoordinationError::Repository(RepositoryExecutionError::Rejected { .. })
        ));
        assert_eq!(*log.borrow(), vec!["storage", "repository"]);
        assert_eq!(providers.hosting.results.len(), 0);
        // Storage stays Confirmed: earlier phases are never compensated.
        assert_eq!(
            ledger.r2_inventory[object.key.as_str()].status,
            OperationState::Confirmed
        );
        // Repository rolled back to its previous (empty) state.
        assert!(!ledger.github_inventory.contains_key("index.html"));
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn repository_ambiguity_stops_before_hosting() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers
            .repository
            .commit_scripts
            .push_back(Err(ProviderError::Network));

        let error = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CoordinationError::Repository(RepositoryExecutionError::Ambiguous { .. })
        ));
        assert_eq!(*log.borrow(), vec!["storage", "repository"]);
        assert_eq!(providers.hosting.results.len(), 0);
        assert_eq!(
            ledger.github_inventory["index.html"].status,
            OperationState::Unknown
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn hosting_failure_terminates_without_compensation() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers
            .hosting
            .results
            .push_back(Err(ProviderError::PermissionDenied));

        let error = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CoordinationError::Hosting(HostingExecutionError::Rejected { .. })
        ));
        assert_eq!(*log.borrow(), vec!["storage", "repository", "hosting"]);
        // Earlier phases remain Confirmed; nothing is rolled back remotely.
        assert_eq!(
            ledger.r2_inventory[object.key.as_str()].status,
            OperationState::Confirmed
        );
        assert_eq!(
            ledger.github_inventory["index.html"].status,
            OperationState::Confirmed
        );
        // The rejected publish leaves no hosting entry at all.
        assert!(ledger.vercel.is_none());
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn hosting_ambiguity_terminates_without_compensation() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let mut providers = Providers::new(&log);
        providers
            .hosting
            .results
            .push_back(Err(ProviderError::Network));

        let error = execute(
            &fixture,
            &full_plan(&object, &desired.bundle_fingerprint),
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CoordinationError::Hosting(HostingExecutionError::Ambiguous { .. })
        ));
        assert_eq!(
            ledger.vercel.as_ref().unwrap().status,
            OperationState::Unknown
        );
        assert_eq!(
            ledger.r2_inventory[object.key.as_str()].status,
            OperationState::Confirmed
        );
        assert_eq!(
            ledger.github_inventory["index.html"].status,
            OperationState::Confirmed
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn only_storage_runs() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let plan = IntegrationPlan {
            operations: vec![IntegrationOperation::PutStorage(object.clone())],
            reconciliation_requirements: Vec::new(),
        };
        let mut providers = Providers::new(&log);

        let report = execute(
            &fixture,
            &plan,
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert!(*log.borrow() == vec!["storage"]);
        assert!(report.storage.is_some());
        assert!(report.repository.is_none());
        assert!(report.hosting.is_none());
        assert_eq!(providers.repository.commit_count, 0);
    }

    #[test]
    fn only_repository_runs() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let plan = IntegrationPlan {
            operations: vec![IntegrationOperation::WriteRepository(
                DesiredRepositoryFile::new(
                    RepositoryPath::new("index.html").unwrap(),
                    sha256_bytes(b"index"),
                    5,
                    RepositoryFileSource::BundleMember,
                )
                .unwrap(),
            )],
            reconciliation_requirements: Vec::new(),
        };
        let mut providers = Providers::new(&log);

        let report = execute(
            &fixture,
            &plan,
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(*log.borrow(), vec!["repository"]);
        assert!(report.storage.is_none());
        assert!(report.repository.is_some());
        assert!(report.hosting.is_none());
        assert!(providers.storage.objects.is_empty());
        assert_eq!(providers.repository.commit_count, 1);
    }

    #[test]
    fn only_hosting_runs() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let plan = IntegrationPlan {
            operations: vec![IntegrationOperation::PublishHosting {
                bundle_fingerprint: desired.bundle_fingerprint.clone(),
            }],
            reconciliation_requirements: Vec::new(),
        };
        let mut providers = Providers::new(&log);
        providers.hosting.results.push_back(Ok(ready_hosting()));

        let report = execute(
            &fixture,
            &plan,
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(*log.borrow(), vec!["hosting"]);
        assert!(report.storage.is_none());
        assert!(report.repository.is_none());
        assert!(report.hosting.is_some());
        assert!(providers.storage.objects.is_empty());
        assert_eq!(providers.repository.commit_count, 0);
    }

    #[test]
    fn storage_and_hosting_skip_repository() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let object = fixture.source("a", b"bytes-a");
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let plan = IntegrationPlan {
            operations: vec![
                IntegrationOperation::PutStorage(object),
                IntegrationOperation::PublishHosting {
                    bundle_fingerprint: desired.bundle_fingerprint.clone(),
                },
            ],
            reconciliation_requirements: Vec::new(),
        };
        let mut providers = Providers::new(&log);
        providers.hosting.results.push_back(Ok(ready_hosting()));

        execute(
            &fixture,
            &plan,
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(*log.borrow(), vec!["storage", "hosting"]);
        assert_eq!(providers.repository.commit_count, 0);
    }

    #[test]
    fn repository_and_hosting_skip_storage() {
        let fixture = Fixture::new();
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let bundle = bundle_of(&[("index.html", b"index")]);
        let desired = desired(&bundle);
        let mut ledger = ledger_for(&desired);
        let plan = IntegrationPlan {
            operations: vec![
                IntegrationOperation::WriteRepository(
                    DesiredRepositoryFile::new(
                        RepositoryPath::new("index.html").unwrap(),
                        sha256_bytes(b"index"),
                        5,
                        RepositoryFileSource::BundleMember,
                    )
                    .unwrap(),
                ),
                IntegrationOperation::PublishHosting {
                    bundle_fingerprint: desired.bundle_fingerprint.clone(),
                },
            ],
            reconciliation_requirements: Vec::new(),
        };
        let mut providers = Providers::new(&log);
        providers.hosting.results.push_back(Ok(ready_hosting()));

        execute(
            &fixture,
            &plan,
            &desired,
            &bundle,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(*log.borrow(), vec!["repository", "hosting"]);
        assert!(providers.storage.objects.is_empty());
    }

    /// Writes a journal-committed publication into `output`: N sources with
    /// byte-exact download assets and distinct re-encoded preview bytes,
    /// exactly as the real pipeline produces.
    fn commit_publication(output: &std::path::Path, generation: &str, sources: &[(&str, &[u8])]) {
        let preview_url = |sha: &str| format!("photos/preview/{sha}.jpg");
        let download_url = |sha: &str| format!("photos/download/{sha}.jpg");
        let photos: Vec<serde_json::Value> = sources
            .iter()
            .enumerate()
            .map(|(index, (sha, _bytes))| {
                serde_json::json!({
                    "id": format!("photo-{}", index + 1),
                    "filename": format!("{index}.jpg"),
                    "sequence": index + 1,
                    "preview": {"url": preview_url(sha), "width": 1, "height": 1},
                    "download": {"url": download_url(sha), "width": 1, "height": 1}
                })
            })
            .collect();
        let gallery = serde_json::json!({
            "schemaVersion": 1,
            "gallery": {"id": "gallery-local", "title": "Title"},
            "photos": photos
        });
        let state = state_from(
            &sources
                .iter()
                .enumerate()
                .map(|(index, (sha, bytes))| SourcePhoto {
                    relative_path: format!("{index}.jpg"),
                    bytes: bytes.len() as u64,
                    sha256: (*sha).to_owned(),
                })
                .collect::<Vec<_>>(),
        );
        let gallery_path = output.join("gallery.json");
        let state_path = output.join(".publisher/state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(output.join("photos/download")).unwrap();
        std::fs::create_dir_all(output.join("photos/preview")).unwrap();
        std::fs::write(&gallery_path, serde_json::to_vec_pretty(&gallery).unwrap()).unwrap();
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        for (sha, bytes) in sources {
            std::fs::write(output.join(download_url(sha)), bytes).unwrap();
            std::fs::write(
                output.join(preview_url(sha)),
                format!("committed preview bytes {sha}"),
            )
            .unwrap();
        }
        let journal = JournalRecord::new(
            1,
            generation.to_owned(),
            None,
            JournalPhase::Committed,
            sha256_file(&gallery_path).unwrap(),
            sha256_file(&state_path).unwrap(),
            ".publisher/staging/g-x".to_owned(),
            ".publisher/backups/none".to_owned(),
            None,
            None,
        );
        std::fs::write(
            output.join(".publisher/journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn end_to_end_g1_to_g2_executes_previews_and_stays_idempotent() {
        let log: SharedLog = Rc::new(RefCell::new(Vec::new()));
        let mut providers = Providers::new(&log);
        let template = bundle_of(&[("index.html", b"index")]);
        let config = configuration();
        providers.hosting.results.push_back(Ok(DeploymentInfo {
            id: "dpl-1".to_owned(),
            url: "g1.vercel.app".to_owned(),
        }));
        providers.hosting.results.push_back(Ok(DeploymentInfo {
            id: "dpl-2".to_owned(),
            url: "g2.vercel.app".to_owned(),
        }));

        // G1: A and B published end to end.
        let fixture_g1 = Fixture::new();
        let sha_a = digest("source-a");
        let sha_b = digest("source-b");
        commit_publication(
            &fixture_g1.output,
            "g-000001",
            &[(&sha_a, b"source-a"), (&sha_b, b"source-b")],
        );
        let manifest1 =
            crate::PublicGalleryManifest::derive_from_committed_output(&config, &fixture_g1.output)
                .unwrap();
        let bundle1 = crate::compose_application_bundle(&template, &manifest1).unwrap();
        let desired1 =
            crate::DesiredPublication::from_committed_output(&config, &fixture_g1.output, &bundle1)
                .unwrap();
        let mut ledger = ledger_for(&desired1);
        let plan = plan_reconciliation(
            &desired1,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        execute(
            &fixture_g1,
            &plan,
            &desired1,
            &bundle1,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        let phases: Vec<&'static str> = {
            let mut phases = log.borrow().clone();
            phases.dedup();
            phases
        };
        assert_eq!(phases, vec!["storage", "repository", "hosting"]);
        // Previews were written to the repository with the exact physical
        // bytes; downloads went to storage; nothing preview touched storage.
        for sha in [&sha_a, &sha_b] {
            assert_eq!(
                providers.repository.files[format!("previews/photos/preview/{sha}.jpg").as_str()],
                format!("committed preview bytes {sha}").as_bytes()
            );
            assert!(providers
                .storage
                .objects
                .contains_key(format!("originals/photos/download/{sha}.jpg").as_str()));
        }
        assert!(!providers
            .repository
            .files
            .keys()
            .any(|path| path.contains("download")));
        let vercel = ledger.vercel.clone().unwrap();
        assert_eq!(vercel.status, OperationState::Confirmed);
        assert_eq!(vercel.deployment_id.as_deref(), Some("dpl-1"));

        // G2: A stays, C arrives, B leaves.
        let fixture_g2 = Fixture::new();
        let sha_c = digest("source-c");
        commit_publication(
            &fixture_g2.output,
            "g-000002",
            &[(&sha_a, b"source-a"), (&sha_c, b"source-c")],
        );
        let manifest2 =
            crate::PublicGalleryManifest::derive_from_committed_output(&config, &fixture_g2.output)
                .unwrap();
        let bundle2 = crate::compose_application_bundle(&template, &manifest2).unwrap();
        let desired2 =
            crate::DesiredPublication::from_committed_output(&config, &fixture_g2.output, &bundle2)
                .unwrap();
        let plan = plan_reconciliation(
            &desired2,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        // A is never rewritten or re-uploaded; B is removed; C is added.
        assert!(!plan.operations.iter().any(|operation| matches!(
            operation,
            IntegrationOperation::PutStorage(object)
                if object.key.as_str().contains(&sha_a)
        )));
        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            IntegrationOperation::DeleteStorage { key }
                if key.as_str() == format!("originals/photos/download/{sha_b}.jpg")
        )));
        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            IntegrationOperation::DeleteRepository { path }
                if path.as_str() == format!("previews/photos/preview/{sha_b}.jpg")
        )));
        execute(
            &fixture_g2,
            &plan,
            &desired2,
            &bundle2,
            &mut providers,
            &mut ledger,
        )
        .unwrap();

        let phases: Vec<&'static str> = {
            let mut phases = log.borrow().clone();
            phases.dedup();
            phases
        };
        assert_eq!(
            phases,
            vec![
                "storage",
                "repository",
                "hosting",
                "storage",
                "repository",
                "hosting"
            ]
        );
        // After G2: previews B are gone, the repository pubilcation set is A+C.
        assert!(!providers
            .repository
            .files
            .contains_key(format!("previews/photos/preview/{sha_b}.jpg").as_str()));
        assert!(providers
            .repository
            .files
            .contains_key(format!("previews/photos/preview/{sha_c}.jpg").as_str()));
        assert!(providers
            .storage
            .objects
            .contains_key(format!("originals/photos/download/{sha_a}.jpg").as_str()));
        assert!(!providers
            .storage
            .objects
            .contains_key(format!("originals/photos/download/{sha_b}.jpg").as_str()));
        assert_eq!(
            ledger.vercel.as_ref().unwrap().deployment_id.as_deref(),
            Some("dpl-2")
        );

        // Idempotency: replanning G2 over the executed ledger is empty and
        // executing it touches nothing.
        let plan = plan_reconciliation(
            &desired2,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        assert!(plan.operations.is_empty() && plan.reconciliation_requirements.is_empty());
        let calls_before = log.borrow().len();
        execute(
            &fixture_g2,
            &plan,
            &desired2,
            &bundle2,
            &mut providers,
            &mut ledger,
        )
        .unwrap();
        assert_eq!(log.borrow().len(), calls_before);
    }
}
