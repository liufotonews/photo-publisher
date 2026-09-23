//! Sequential storage execution for an [`IntegrationPlan`].
//!
//! The executor applies only storage operations (`PutStorage`, `DeleteStorage`)
//! from an already computed plan; repository and hosting operations belong to
//! later phases and are ignored here. It never recomputes desired state and
//! never invokes the planner.
//!
//! Safety protocol for every mutable remote operation:
//!
//! ```text
//! local validation
//!     -> Pending recorded and durably persisted
//!     -> remote operation
//!     -> known result recorded (Confirmed, removal, or Unknown)
//!     -> durable persistence
//! ```
//!
//! The persisted `Pending` always precedes the remote call. If the remote
//! outcome cannot be determined (network loss, timeout, a lost response, a
//! conflict, or any genuinely ambiguous provider error), the entry becomes
//! `Unknown`, the ledger is persisted, and execution stops immediately: no
//! automatic retry and no further operations.
//!
//! If the provider provably rejected the operation without producing a remote
//! effect (for example authentication or permission failures, a missing bucket
//! on PUT, or a pre-request failure such as an unreadable buffered source),
//! the previously known ledger state is restored and persisted instead of
//! `Unknown`: `Unknown` is reserved for outcomes that genuinely cannot be
//! determined safely.

use std::fmt;
use std::fs;
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use photo_publisher_provider_contracts::{
    copy_with_sha256, ObjectKey, ProviderError, StorageObject, StorageProvider,
};

use crate::{
    DesiredStorageObject, IntegrationLedger, IntegrationOperation, IntegrationPlan, OperationState,
    R2InventoryEntry, ReconciliationRequirement,
};

/// Why storage execution failed.
///
/// Every variant that refers to an object stops execution immediately; the
/// durable ledger always reflects the last safely known state.
#[derive(Debug)]
pub enum StorageExecutionError {
    /// The plan carries reconciliation requirements; nothing was executed.
    ReconciliationRequired(Vec<ReconciliationRequirement>),
    /// The publication output directory could not be resolved.
    InvalidRoot(io::Error),
    /// A planned operation contradicts the ledger state handed to the
    /// executor; no ledger or remote change was made.
    InconsistentPlan(String),
    /// Local source validation failed before any ledger or remote change.
    LocalValidation { key: ObjectKey, message: String },
    /// The provider provably produced no remote effect; the previous ledger
    /// state was restored and persisted.
    Rejected { key: ObjectKey, message: String },
    /// The remote outcome cannot be determined; `Unknown` was persisted and
    /// execution stopped without retrying.
    Ambiguous { key: ObjectKey, message: String },
    /// The provider acknowledged a PUT whose returned metadata differs from
    /// the desired object; the returned (actual) state was recorded as
    /// `Confirmed` and execution stopped.
    InconsistentUpload { key: ObjectKey, message: String },
    /// The ledger could not be mutated or persisted. On a persistence failure
    /// the in-memory ledger is reloaded from the last durable state when
    /// possible, so the durable file remains authoritative.
    Ledger(anyhow::Error),
}

impl fmt::Display for StorageExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReconciliationRequired(requirements) => write!(
                formatter,
                "storage execution requires reconciliation first ({} requirement(s))",
                requirements.len()
            ),
            Self::InvalidRoot(error) => write!(
                formatter,
                "failed to resolve the publication output directory: {error}"
            ),
            Self::InconsistentPlan(message) => write!(formatter, "inconsistent plan: {message}"),
            Self::LocalValidation { key, message }
            | Self::Rejected { key, message }
            | Self::Ambiguous { key, message }
            | Self::InconsistentUpload { key, message } => {
                write!(formatter, "{}: {message}", key.as_str())
            }
            Self::Ledger(error) => {
                write!(
                    formatter,
                    "failed to persist the integration ledger: {error}"
                )
            }
        }
    }
}

impl std::error::Error for StorageExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRoot(error) => Some(error),
            Self::Ledger(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Storage operations completed by one [`StorageExecutor::execute`] run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StorageExecutionReport {
    pub uploaded: Vec<ObjectKey>,
    pub deleted: Vec<ObjectKey>,
}

/// Executes the storage operations of an [`IntegrationPlan`] against a
/// [`StorageProvider`], persisting every ledger transition atomically.
///
/// The executor borrows the provider, the ledger, and the ledger's durable
/// path; on return the in-memory ledger and the durable file agree on every
/// successfully persisted transition.
pub struct StorageExecutor<'a> {
    output_dir: PathBuf,
    canonical_root: PathBuf,
    provider: &'a mut dyn StorageProvider,
    ledger: &'a mut IntegrationLedger,
    ledger_path: PathBuf,
}

impl<'a> StorageExecutor<'a> {
    pub fn new(
        output_dir: impl AsRef<Path>,
        provider: &'a mut dyn StorageProvider,
        ledger: &'a mut IntegrationLedger,
        ledger_path: impl AsRef<Path>,
    ) -> Result<Self, StorageExecutionError> {
        let output_dir = output_dir.as_ref().to_path_buf();
        let canonical_root = output_dir
            .canonicalize()
            .map_err(StorageExecutionError::InvalidRoot)?;
        Ok(Self {
            output_dir,
            canonical_root,
            provider,
            ledger,
            ledger_path: ledger_path.as_ref().to_path_buf(),
        })
    }

    /// Applies the plan's storage operations in order.
    ///
    /// A plan that still carries reconciliation requirements is refused and
    /// nothing is executed. Execution stops at the first error; an ambiguous
    /// remote outcome leaves `Unknown` persisted and is never retried.
    pub fn execute(
        &mut self,
        plan: &IntegrationPlan,
    ) -> Result<StorageExecutionReport, StorageExecutionError> {
        if !plan.reconciliation_requirements.is_empty() {
            return Err(StorageExecutionError::ReconciliationRequired(
                plan.reconciliation_requirements.clone(),
            ));
        }

        let mut report = StorageExecutionReport::default();
        for operation in &plan.operations {
            match operation {
                IntegrationOperation::PutStorage(desired) => {
                    self.put(desired)?;
                    report.uploaded.push(desired.key.clone());
                }
                IntegrationOperation::DeleteStorage { key } => {
                    self.delete(key)?;
                    report.deleted.push(key.clone());
                }
                // GitHub and Vercel operations are out of scope; later phases
                // execute them through their own providers.
                IntegrationOperation::WriteRepository(_)
                | IntegrationOperation::DeleteRepository { .. }
                | IntegrationOperation::PublishHosting { .. } => {}
            }
        }
        Ok(report)
    }

    fn put(&mut self, desired: &DesiredStorageObject) -> Result<(), StorageExecutionError> {
        let key = &desired.key;
        let previous = self.ledger.r2_inventory.get(key.as_str()).cloned();
        if let Some(entry) = &previous {
            if entry.status != OperationState::Confirmed {
                return Err(StorageExecutionError::InconsistentPlan(format!(
                    "cannot PUT {} while the ledger marks it {}",
                    key.as_str(),
                    status_name(entry.status)
                )));
            }
        }

        let mut file = open_validated_source(&self.output_dir, &self.canonical_root, desired)?;

        // Pending carries the desired metadata; it describes intent, not a
        // confirmed remote state, and must be durable before the remote call.
        let pending = desired_object(desired);
        self.record(key, &pending, OperationState::Pending)?;
        self.persist()?;

        match self
            .provider
            .put(key, &mut file, desired.content_type.as_deref())
        {
            Ok(returned) => self.confirm_put(desired, returned),
            Err(error) => match classify_failure(&error) {
                FailureKind::NoEffect => {
                    self.restore(key, previous)?;
                    self.persist()?;
                    Err(StorageExecutionError::Rejected {
                        key: key.clone(),
                        message: format!(
                            "storage PUT was rejected without a remote effect: {error}"
                        ),
                    })
                }
                FailureKind::Ambiguous => {
                    self.record(key, &pending, OperationState::Unknown)?;
                    self.persist()?;
                    Err(StorageExecutionError::Ambiguous {
                        key: key.clone(),
                        message: format!("storage PUT outcome cannot be determined: {error}"),
                    })
                }
            },
        }
    }

    fn confirm_put(
        &mut self,
        desired: &DesiredStorageObject,
        returned: StorageObject,
    ) -> Result<(), StorageExecutionError> {
        let key = &desired.key;
        if returned.key != *key {
            // The provider violated the contract; the state of the desired
            // key cannot be determined safely.
            let pending = desired_object(desired);
            self.record(key, &pending, OperationState::Unknown)?;
            self.persist()?;
            return Err(StorageExecutionError::Ambiguous {
                key: key.clone(),
                message: format!(
                    "storage provider returned {} for a PUT of this key",
                    returned.key.as_str()
                ),
            });
        }
        if returned.sha256 != desired.sha256
            || returned.size_bytes != desired.size_bytes
            || returned.content_type != desired.content_type
        {
            // The provider stored exactly what it returned (the R2 provider
            // hashes the bytes it uploads). Recording the returned metadata
            // keeps the ledger truthful and lets the next planning cycle see
            // the drift instead of hiding it.
            self.record(key, &returned, OperationState::Confirmed)?;
            self.persist()?;
            return Err(StorageExecutionError::InconsistentUpload {
                key: key.clone(),
                message: "stored object metadata does not match the desired object".to_owned(),
            });
        }
        self.record(key, &returned, OperationState::Confirmed)?;
        self.persist()
    }

    fn delete(&mut self, key: &ObjectKey) -> Result<(), StorageExecutionError> {
        let Some(entry) = self.ledger.r2_inventory.get(key.as_str()).cloned() else {
            return Err(StorageExecutionError::InconsistentPlan(format!(
                "cannot DELETE {} because it is absent from the ledger",
                key.as_str()
            )));
        };
        if entry.status != OperationState::Confirmed {
            return Err(StorageExecutionError::InconsistentPlan(format!(
                "cannot DELETE {} while the ledger marks it {}",
                key.as_str(),
                status_name(entry.status)
            )));
        }
        let existing = StorageObject {
            key: key.clone(),
            size_bytes: entry.size_bytes,
            sha256: entry.sha256.clone(),
            content_type: entry.content_type.clone(),
        };
        self.record(key, &existing, OperationState::Pending)?;
        self.persist()?;

        match self.provider.delete(key) {
            // The R2 provider maps a missing object to success, so a surfaced
            // `NotFound` from another provider is the same idempotent success.
            // Removal of the ledger entry is the final state; no tombstone.
            Ok(()) | Err(ProviderError::NotFound) => {
                self.remove(key)?;
                self.persist()
            }
            Err(error) => match classify_failure(&error) {
                FailureKind::NoEffect => {
                    self.record(key, &existing, OperationState::Confirmed)?;
                    self.persist()?;
                    Err(StorageExecutionError::Rejected {
                        key: key.clone(),
                        message: format!(
                            "storage DELETE was rejected without a remote effect: {error}"
                        ),
                    })
                }
                FailureKind::Ambiguous => {
                    self.record(key, &existing, OperationState::Unknown)?;
                    self.persist()?;
                    Err(StorageExecutionError::Ambiguous {
                        key: key.clone(),
                        message: format!("storage DELETE outcome cannot be determined: {error}"),
                    })
                }
            },
        }
    }

    /// Restores the ledger state known before an operation that provably had
    /// no remote effect: the previous confirmed entry, or no entry at all.
    fn restore(
        &mut self,
        key: &ObjectKey,
        previous: Option<R2InventoryEntry>,
    ) -> Result<(), StorageExecutionError> {
        match previous {
            None => self.remove(key),
            Some(entry) => {
                let status = entry.status;
                let object = StorageObject {
                    key: key.clone(),
                    size_bytes: entry.size_bytes,
                    sha256: entry.sha256,
                    content_type: entry.content_type,
                };
                self.record(key, &object, status)
            }
        }
    }

    fn record(
        &mut self,
        key: &ObjectKey,
        object: &StorageObject,
        status: OperationState,
    ) -> Result<(), StorageExecutionError> {
        self.ledger
            .record_r2_object(key, object, status)
            .map_err(StorageExecutionError::Ledger)
    }

    fn remove(&mut self, key: &ObjectKey) -> Result<(), StorageExecutionError> {
        self.ledger
            .remove_r2_object(key)
            .map_err(StorageExecutionError::Ledger)
    }

    fn persist(&mut self) -> Result<(), StorageExecutionError> {
        if let Err(error) = self.ledger.write_to(&self.ledger_path) {
            // The durable file is authoritative; drop the in-memory state that
            // could not be persisted by reloading the last durable ledger.
            if let Ok(durable) = IntegrationLedger::read_from(&self.ledger_path) {
                *self.ledger = durable;
            }
            return Err(StorageExecutionError::Ledger(error));
        }
        Ok(())
    }
}

/// Whether a provider failure provably left the remote state untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    NoEffect,
    Ambiguous,
}

/// Classifies a provider failure by the point at which it can occur in the
/// real R2 provider, not by the enum alone.
///
/// The R2 provider resolves credentials and buffers (and hashes) the whole
/// request body before any request is sent, so I/O, key/path, missing
/// credential, and unsupported-operation failures are provably pre-request.
/// Authentication, permission, and missing-bucket rejections are evaluated by
/// the service before the mutation takes effect. Everything else (lost
/// responses, timeouts, conflicts, integrity or internal failures) may have
/// reached the service after the mutation was applied, so the remote state
/// cannot be determined safely.
fn classify_failure(error: &ProviderError) -> FailureKind {
    match error {
        ProviderError::Io(_)
        | ProviderError::InvalidKey(_)
        | ProviderError::InvalidPath(_)
        | ProviderError::AuthenticationRequired
        | ProviderError::Unsupported => FailureKind::NoEffect,
        ProviderError::AuthenticationFailed
        | ProviderError::PermissionDenied
        | ProviderError::NotFound => FailureKind::NoEffect,
        ProviderError::Network
        | ProviderError::Conflict
        | ProviderError::AlreadyExists
        | ProviderError::Integrity
        | ProviderError::Other => FailureKind::Ambiguous,
    }
}

/// Revalidates the physical source file of a desired object before any ledger
/// or remote change: the file must exist, be a regular file inside
/// `output_dir`, and match the declared size and SHA-256 exactly. On success
/// the returned handle is rewound and ready to stream to the provider.
fn open_validated_source(
    output_dir: &Path,
    canonical_root: &Path,
    desired: &DesiredStorageObject,
) -> Result<fs::File, StorageExecutionError> {
    let failure = |message: String| StorageExecutionError::LocalValidation {
        key: desired.key.clone(),
        message,
    };
    let source = desired.source_path.as_str();
    let physical = output_dir.join(source);
    let metadata = fs::metadata(&physical)
        .map_err(|error| failure(format!("source file does not exist: {source} ({error})")))?;
    if !metadata.is_file() {
        return Err(failure(format!(
            "source file is not a regular file: {source}"
        )));
    }
    let canonical = physical
        .canonicalize()
        .map_err(|error| failure(format!("failed to resolve source file: {source} ({error})")))?;
    if canonical.strip_prefix(canonical_root).is_err() {
        return Err(failure(format!(
            "source file escapes the publication root: {source}"
        )));
    }
    if metadata.len() != desired.size_bytes {
        return Err(failure(format!(
            "source file size {} does not match the desired {} bytes: {source}",
            metadata.len(),
            desired.size_bytes
        )));
    }
    let mut file = fs::File::open(&physical)
        .map_err(|error| failure(format!("failed to open source file: {source} ({error})")))?;
    let (bytes, sha256) = copy_with_sha256(&mut file, &mut io::sink())
        .map_err(|error| failure(format!("failed to hash source file: {source} ({error})")))?;
    if bytes != metadata.len() {
        return Err(failure(format!(
            "source file changed while it was hashed: {source}"
        )));
    }
    if sha256 != desired.sha256 {
        return Err(failure(format!(
            "source file SHA-256 {sha256} does not match the desired {}: {source}",
            desired.sha256
        )));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| failure(format!("failed to rewind source file: {source} ({error})")))?;
    Ok(file)
}

/// The desired remote object. Recorded as `Pending` it describes the intent
/// of the operation that has not been confirmed remotely yet.
fn desired_object(desired: &DesiredStorageObject) -> StorageObject {
    StorageObject {
        key: desired.key.clone(),
        size_bytes: desired.size_bytes,
        sha256: desired.sha256.clone(),
        content_type: desired.content_type.clone(),
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
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::rc::Rc;

    use photo_publisher_provider_contracts::ProviderResult;
    use sha2::{Digest, Sha256};
    use tempfile::{tempdir, TempDir};

    use crate::{
        plan_reconciliation, ApplicationBundle, DesiredPublication, HostingPublicationConfig,
        KnownRemoteState, LocalPublication, ProjectPublicationConfig, PublicBaseUrl,
        PublicationPath,
    };

    fn digest(value: &str) -> String {
        format!("{:x}", Sha256::digest(value.as_bytes()))
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Put(String),
        Delete(String),
    }

    /// Scriptable in-memory provider. Every call is logged, and any call
    /// without a scripted result panics so unexpected remote work fails loudly.
    #[derive(Default)]
    struct FakeStorage {
        objects: HashMap<String, (u64, String, Option<String>)>,
        put_results: VecDeque<ProviderResult<()>>,
        delete_results: VecDeque<ProviderResult<()>>,
        calls: Vec<Call>,
        on_put: Option<Box<dyn FnMut()>>,
        on_delete: Option<Box<dyn FnMut()>>,
    }

    impl StorageProvider for FakeStorage {
        fn put(
            &mut self,
            key: &ObjectKey,
            content: &mut dyn io::Read,
            content_type: Option<&str>,
        ) -> ProviderResult<StorageObject> {
            self.calls.push(Call::Put(key.as_str().to_owned()));
            let result = self.put_results.pop_front().expect("scripted PUT result");
            if let Some(hook) = self.on_put.as_mut() {
                hook();
            }
            result?;
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(ProviderError::from_io)?;
            let object = StorageObject {
                key: key.clone(),
                size_bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
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
            let Some((size_bytes, sha256, content_type)) = self.objects.get(key.as_str()) else {
                return Ok(None);
            };
            Ok(Some(StorageObject {
                key: key.clone(),
                size_bytes: *size_bytes,
                sha256: sha256.clone(),
                content_type: content_type.clone(),
            }))
        }

        fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
            self.calls.push(Call::Delete(key.as_str().to_owned()));
            let result = self
                .delete_results
                .pop_front()
                .expect("scripted DELETE result");
            if let Some(hook) = self.on_delete.as_mut() {
                hook();
            }
            result?;
            self.objects.remove(key.as_str());
            Ok(())
        }
    }

    struct Fixture {
        root: TempDir,
        output: PathBuf,
        ledger_path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempdir().unwrap();
            let output = root.path().join("output");
            let ledger_path = root.path().join(".publisher/integration-state.json");
            fs::create_dir_all(&output).unwrap();
            Self {
                root,
                output,
                ledger_path,
            }
        }

        /// A desired object bound to a real source file under `output`.
        fn source(&self, name: &str, bytes: &[u8]) -> DesiredStorageObject {
            let relative = format!("photos/download/{name}.jpg");
            let path = self.output.join(&relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            DesiredStorageObject::new(
                ObjectKey::new(format!("originals/{name}.jpg")).unwrap(),
                PublicationPath::new(relative).unwrap(),
                format!("{:x}", Sha256::digest(bytes)),
                bytes.len() as u64,
                Some("image/jpeg".to_owned()),
            )
            .unwrap()
        }

        fn durable_ledger(&self) -> IntegrationLedger {
            IntegrationLedger::read_from(&self.ledger_path).unwrap()
        }

        fn snapshot_path(&self) -> PathBuf {
            self.root.path().join("ledger-snapshot.json")
        }
    }

    fn plan(operations: Vec<IntegrationOperation>) -> IntegrationPlan {
        IntegrationPlan {
            operations,
            reconciliation_requirements: Vec::new(),
        }
    }

    fn empty_ledger() -> IntegrationLedger {
        IntegrationLedger::new(
            digest("config"),
            "g-000001".to_owned(),
            digest("state"),
            digest("manifest"),
        )
        .unwrap()
    }

    fn storage_object(desired: &DesiredStorageObject) -> StorageObject {
        StorageObject {
            key: desired.key.clone(),
            size_bytes: desired.size_bytes,
            sha256: desired.sha256.clone(),
            content_type: desired.content_type.clone(),
        }
    }

    fn record(ledger: &mut IntegrationLedger, object: &StorageObject, status: OperationState) {
        ledger
            .record_r2_object(&object.key, object, status)
            .unwrap();
    }

    fn execute(
        fixture: &Fixture,
        plan: &IntegrationPlan,
        provider: &mut FakeStorage,
        ledger: &mut IntegrationLedger,
    ) -> Result<StorageExecutionReport, StorageExecutionError> {
        StorageExecutor::new(&fixture.output, provider, ledger, &fixture.ledger_path)?.execute(plan)
    }

    fn configuration() -> ProjectPublicationConfig {
        ProjectPublicationConfig {
            project_id: "project".to_owned(),
            bundle_directory: "site".into(),
            repository_provider: "repository".to_owned(),
            repository: "owner/project".to_owned(),
            branch: Some("main".to_owned()),
            preview_prefix: Some("previews".to_owned()),
            preview_public_base_url: PublicBaseUrl::parse("https://cdn.example.com/previews")
                .unwrap(),
            high_resolution_account_id: "account".to_owned(),
            high_resolution_bucket: Some("bucket".to_owned()),
            high_resolution_prefix: Some("originals".to_owned()),
            high_resolution_public_base_url: PublicBaseUrl::parse(
                "https://downloads.example.com/originals",
            )
            .unwrap(),
            hosting: HostingPublicationConfig::new("hosting-project", None).unwrap(),
        }
    }

    fn desired_publication(object: Option<&DesiredStorageObject>) -> DesiredPublication {
        let bundle =
            ApplicationBundle::from_files(vec![("index.html".to_owned(), b"index".to_vec())])
                .unwrap();
        let mut desired = DesiredPublication::new(
            &configuration(),
            LocalPublication::new("g-000001", digest("state"), digest("manifest")).unwrap(),
            &bundle,
        )
        .unwrap();
        if let Some(object) = object {
            desired.insert_storage_object(object.clone()).unwrap();
        }
        desired
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

    #[test]
    fn put_uploads_and_confirms_a_new_key() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Ok(()));

        let report = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report.uploaded, vec![desired.key.clone()]);
        assert_eq!(
            provider.calls,
            vec![Call::Put(desired.key.as_str().to_owned())]
        );
        let entry = &ledger.r2_inventory[desired.key.as_str()];
        assert_eq!(entry.status, OperationState::Confirmed);
        assert_eq!(entry.sha256, desired.sha256);
        assert_eq!(entry.size_bytes, desired.size_bytes);
        assert_eq!(entry.content_type, desired.content_type);
        assert_eq!(
            provider.objects[desired.key.as_str()],
            (
                desired.size_bytes,
                desired.sha256.clone(),
                desired.content_type.clone()
            )
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn put_persists_pending_before_calling_the_provider_and_confirmed_afterwards() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Ok(()));
        let observed: Rc<RefCell<Vec<(OperationState, String)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let ledger_path = fixture.ledger_path.clone();
        let observed_in_hook = Rc::clone(&observed);
        let key = desired.key.as_str().to_owned();
        provider.on_put = Some(Box::new(move || {
            // The fundamental safety rule: the persisted Pending must exist
            // before the remote operation starts.
            let durable = IntegrationLedger::read_from(&ledger_path).unwrap();
            let entry = durable.r2_inventory[&key].clone();
            observed_in_hook
                .borrow_mut()
                .push((entry.status, entry.sha256));
        }));

        execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(
            *observed.borrow(),
            vec![(OperationState::Pending, desired.sha256.clone())]
        );
        assert_eq!(
            fixture.durable_ledger().r2_inventory[desired.key.as_str()].status,
            OperationState::Confirmed
        );
    }

    #[test]
    fn put_replaces_a_confirmed_key_when_the_desired_content_changed() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"new-bytes");
        let mut ledger = empty_ledger();
        let mut predecessor = storage_object(&desired);
        predecessor.sha256 = digest("old-bytes");
        predecessor.size_bytes = 9;
        record(&mut ledger, &predecessor, OperationState::Confirmed);
        ledger.write_to(&fixture.ledger_path).unwrap();
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Ok(()));
        let observed: Rc<RefCell<Vec<(OperationState, String)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let ledger_path = fixture.ledger_path.clone();
        let observed_in_hook = Rc::clone(&observed);
        let key = desired.key.as_str().to_owned();
        provider.on_put = Some(Box::new(move || {
            // The Pending carries the NEW desired metadata, not the old entry.
            let durable = IntegrationLedger::read_from(&ledger_path).unwrap();
            let entry = durable.r2_inventory[&key].clone();
            observed_in_hook
                .borrow_mut()
                .push((entry.status, entry.sha256));
        }));

        execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(
            *observed.borrow(),
            vec![(OperationState::Pending, desired.sha256.clone())]
        );
        let entry = &ledger.r2_inventory[desired.key.as_str()];
        assert_eq!(entry.status, OperationState::Confirmed);
        assert_eq!(entry.sha256, desired.sha256);
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn put_with_a_local_sha_mismatch_changes_nothing() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        // Same length, different content: only the SHA-256 diverges.
        fs::write(fixture.output.join("photos/download/a.jpg"), b"xxxxxxx").unwrap();
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            StorageExecutionError::LocalValidation { .. }
        ));
        assert!(provider.calls.is_empty());
        assert!(ledger.r2_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn put_with_a_local_size_mismatch_changes_nothing() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let wrong_size = DesiredStorageObject::new(
            desired.key.clone(),
            desired.source_path.clone(),
            desired.sha256.clone(),
            desired.size_bytes + 1,
            desired.content_type.clone(),
        )
        .unwrap();
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(wrong_size)]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            StorageExecutionError::LocalValidation { .. }
        ));
        assert!(provider.calls.is_empty());
        assert!(ledger.r2_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn put_with_a_missing_source_changes_nothing() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        fs::remove_file(fixture.output.join("photos/download/a.jpg")).unwrap();
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            StorageExecutionError::LocalValidation { .. }
        ));
        assert!(provider.calls.is_empty());
        assert!(ledger.r2_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn put_with_a_network_failure_marks_unknown_and_stops() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Err(ProviderError::Network));

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, StorageExecutionError::Ambiguous { .. }));
        assert_eq!(
            provider.calls,
            vec![Call::Put(desired.key.as_str().to_owned())]
        );
        let entry = &ledger.r2_inventory[desired.key.as_str()];
        assert_eq!(entry.status, OperationState::Unknown);
        assert_eq!(entry.sha256, desired.sha256);
        assert_eq!(entry.size_bytes, desired.size_bytes);
        assert_eq!(fixture.durable_ledger(), ledger);
        // The remote state is genuinely indeterminate: nothing was stored by
        // the fake, yet the ledger must not assume the PUT never happened.
        assert!(!provider.objects.contains_key(desired.key.as_str()));
    }

    #[test]
    fn put_with_a_conflict_marks_unknown_and_stops() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Err(ProviderError::Conflict));

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, StorageExecutionError::Ambiguous { .. }));
        assert_eq!(
            ledger.r2_inventory[desired.key.as_str()].status,
            OperationState::Unknown
        );
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn put_rejected_without_remote_effect_leaves_no_entry_for_a_new_key() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();
        provider
            .put_results
            .push_back(Err(ProviderError::PermissionDenied));

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, StorageExecutionError::Rejected { .. }));
        assert!(ledger.r2_inventory.is_empty());
        assert_eq!(fixture.durable_ledger(), ledger);
        assert_eq!(provider.calls.len(), 1);
        assert!(!provider.objects.contains_key(desired.key.as_str()));
    }

    #[test]
    fn put_rejected_restores_the_confirmed_predecessor() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"new-bytes");
        let mut ledger = empty_ledger();
        let mut predecessor = storage_object(&desired);
        predecessor.sha256 = digest("old-bytes");
        predecessor.size_bytes = 9;
        record(&mut ledger, &predecessor, OperationState::Confirmed);
        ledger.write_to(&fixture.ledger_path).unwrap();
        let snapshot = ledger.clone();
        let mut provider = FakeStorage::default();
        provider
            .put_results
            .push_back(Err(ProviderError::AuthenticationFailed));

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, StorageExecutionError::Rejected { .. }));
        assert_eq!(ledger, snapshot);
        assert_eq!(fixture.durable_ledger(), snapshot);
    }

    #[test]
    fn delete_removes_a_confirmed_key_and_persists_the_transitions() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        record(
            &mut ledger,
            &storage_object(&desired),
            OperationState::Confirmed,
        );
        ledger.write_to(&fixture.ledger_path).unwrap();
        let mut provider = FakeStorage {
            objects: HashMap::from([(
                desired.key.as_str().to_owned(),
                (
                    desired.size_bytes,
                    desired.sha256.clone(),
                    desired.content_type.clone(),
                ),
            )]),
            ..FakeStorage::default()
        };
        provider.delete_results.push_back(Ok(()));
        let observed: Rc<RefCell<Vec<(OperationState, String)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let ledger_path = fixture.ledger_path.clone();
        let observed_in_hook = Rc::clone(&observed);
        let key = desired.key.as_str().to_owned();
        provider.on_delete = Some(Box::new(move || {
            // Pending is persisted before the remote DELETE and preserves the
            // existing confirmed metadata.
            let durable = IntegrationLedger::read_from(&ledger_path).unwrap();
            let entry = durable.r2_inventory[&key].clone();
            observed_in_hook
                .borrow_mut()
                .push((entry.status, entry.sha256));
        }));

        let report = execute(
            &fixture,
            &plan(vec![IntegrationOperation::DeleteStorage {
                key: desired.key.clone(),
            }]),
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report.deleted, vec![desired.key.clone()]);
        assert_eq!(
            *observed.borrow(),
            vec![(OperationState::Pending, desired.sha256.clone())]
        );
        assert_eq!(
            provider.calls,
            vec![Call::Delete(desired.key.as_str().to_owned())]
        );
        // No tombstone: absence of the key is the final state.
        assert!(!ledger.r2_inventory.contains_key(desired.key.as_str()));
        assert!(!provider.objects.contains_key(desired.key.as_str()));
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn delete_treats_not_found_as_idempotent_success() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        record(
            &mut ledger,
            &storage_object(&desired),
            OperationState::Confirmed,
        );
        ledger.write_to(&fixture.ledger_path).unwrap();
        let mut provider = FakeStorage::default();
        provider
            .delete_results
            .push_back(Err(ProviderError::NotFound));

        execute(
            &fixture,
            &plan(vec![IntegrationOperation::DeleteStorage {
                key: desired.key.clone(),
            }]),
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert!(!ledger.r2_inventory.contains_key(desired.key.as_str()));
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn delete_with_a_network_failure_marks_unknown_and_stops() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut ledger = empty_ledger();
        record(
            &mut ledger,
            &storage_object(&desired),
            OperationState::Confirmed,
        );
        ledger.write_to(&fixture.ledger_path).unwrap();
        let mut provider = FakeStorage::default();
        provider
            .delete_results
            .push_back(Err(ProviderError::Network));

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::DeleteStorage {
                key: desired.key.clone(),
            }]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, StorageExecutionError::Ambiguous { .. }));
        let entry = &ledger.r2_inventory[desired.key.as_str()];
        assert_eq!(entry.status, OperationState::Unknown);
        // The Unknown entry preserves the last confirmed metadata.
        assert_eq!(entry.sha256, desired.sha256);
        assert_eq!(entry.size_bytes, desired.size_bytes);
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn plan_with_reconciliation_requirements_is_refused() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut provider = FakeStorage::default();
        let mut ledger = empty_ledger();

        for requirement in [
            ReconciliationRequirement::ConfigurationChanged,
            ReconciliationRequirement::StoragePending {
                key: desired.key.clone(),
            },
            ReconciliationRequirement::StorageUnknown {
                key: desired.key.clone(),
            },
        ] {
            let blocked = IntegrationPlan {
                operations: vec![IntegrationOperation::PutStorage(desired.clone())],
                reconciliation_requirements: vec![requirement],
            };
            let error = execute(&fixture, &blocked, &mut provider, &mut ledger).unwrap_err();
            assert!(matches!(
                error,
                StorageExecutionError::ReconciliationRequired(_)
            ));
        }
        assert!(provider.calls.is_empty());
        assert!(ledger.r2_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn put_over_a_pending_or_unknown_ledger_entry_is_refused() {
        for status in [OperationState::Pending, OperationState::Unknown] {
            let fixture = Fixture::new();
            let desired = fixture.source("a", b"bytes-a");
            let mut ledger = empty_ledger();
            record(&mut ledger, &storage_object(&desired), status);
            let mut provider = FakeStorage::default();

            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::PutStorage(desired.clone())]),
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, StorageExecutionError::InconsistentPlan(_)));
            assert!(provider.calls.is_empty());
            assert_eq!(ledger.r2_inventory[desired.key.as_str()].status, status);
        }
    }

    #[test]
    fn delete_of_a_key_not_confirmed_in_the_ledger_is_refused() {
        let fixture = Fixture::new();
        let desired = fixture.source("a", b"bytes-a");
        let mut provider = FakeStorage::default();

        // Absent from the ledger.
        let mut ledger = empty_ledger();
        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::DeleteStorage {
                key: desired.key.clone(),
            }]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();
        assert!(matches!(error, StorageExecutionError::InconsistentPlan(_)));

        // Recorded, but not Confirmed.
        for status in [OperationState::Pending, OperationState::Unknown] {
            let mut ledger = empty_ledger();
            record(&mut ledger, &storage_object(&desired), status);
            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::DeleteStorage {
                    key: desired.key.clone(),
                }]),
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();
            assert!(matches!(error, StorageExecutionError::InconsistentPlan(_)));
        }
        assert!(provider.calls.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn execution_stops_immediately_after_an_ambiguous_result() {
        let fixture = Fixture::new();
        let first = fixture.source("a", b"bytes-a");
        let second = fixture.source("b", b"bytes-b");
        let mut ledger = empty_ledger();
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Err(ProviderError::Network));
        // No second scripted result: a call for `second` would panic.

        let error = execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::PutStorage(first.clone()),
                IntegrationOperation::PutStorage(second.clone()),
            ]),
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, StorageExecutionError::Ambiguous { .. }));
        assert_eq!(
            provider.calls,
            vec![Call::Put(first.key.as_str().to_owned())]
        );
        assert_eq!(
            ledger.r2_inventory[first.key.as_str()].status,
            OperationState::Unknown
        );
        assert!(!ledger.r2_inventory.contains_key(second.key.as_str()));
    }

    #[test]
    fn crash_after_pending_before_the_put_blocks_the_next_cycle() {
        let fixture = Fixture::new();
        let object = fixture.source("a", b"bytes-a");
        let desired = desired_publication(Some(&object));
        // Durable state left by a crash after Pending was persisted and
        // before the remote PUT was attempted.
        let mut crashed = ledger_for(&desired);
        record(
            &mut crashed,
            &storage_object(&object),
            OperationState::Pending,
        );
        crashed.write_to(&fixture.ledger_path).unwrap();

        let mut ledger = fixture.durable_ledger();
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(planned
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::StoragePending { .. }
            )));
        assert!(!planned
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PutStorage(_))));

        let mut provider = FakeStorage::default();
        let error = execute(&fixture, &planned, &mut provider, &mut ledger).unwrap_err();

        assert!(matches!(
            error,
            StorageExecutionError::ReconciliationRequired(_)
        ));
        assert!(provider.calls.is_empty());
        assert!(provider.objects.is_empty());
        assert_eq!(
            fixture.durable_ledger().r2_inventory[object.key.as_str()].status,
            OperationState::Pending
        );
    }

    #[test]
    fn crash_after_the_remote_put_without_a_persisted_confirmation_is_not_repeated() {
        let fixture = Fixture::new();
        let object = fixture.source("a", b"bytes-a");
        let desired = desired_publication(Some(&object));
        let mut ledger = ledger_for(&desired);
        let mut provider = FakeStorage::default();
        provider.put_results.push_back(Ok(()));
        // Snapshot the durable Pending while the remote PUT is in flight.
        let ledger_path = fixture.ledger_path.clone();
        let snapshot_path = fixture.snapshot_path();
        provider.on_put = Some(Box::new(move || {
            fs::copy(&ledger_path, &snapshot_path).unwrap();
        }));

        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(planned.reconciliation_requirements.is_empty());
        execute(&fixture, &planned, &mut provider, &mut ledger).unwrap();
        assert_eq!(provider.calls.len(), 1);
        assert!(provider.objects.contains_key(object.key.as_str()));

        // Emulate a crash that lost the Confirmed write: the durable ledger is
        // back at Pending while the remote object exists.
        fs::copy(fixture.snapshot_path(), &fixture.ledger_path).unwrap();
        let durable = fixture.durable_ledger();
        assert_eq!(
            durable.r2_inventory[object.key.as_str()].status,
            OperationState::Pending
        );

        let mut ledger = durable;
        let mut provider = FakeStorage {
            objects: provider.objects.clone(),
            ..FakeStorage::default()
        };
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        let error = execute(&fixture, &planned, &mut provider, &mut ledger).unwrap_err();

        assert!(matches!(
            error,
            StorageExecutionError::ReconciliationRequired(_)
        ));
        // The PUT is never repeated automatically.
        assert!(provider.calls.is_empty());
        assert!(provider.objects.contains_key(object.key.as_str()));
    }

    #[test]
    fn crash_after_the_remote_delete_without_a_persisted_removal_is_not_repeated() {
        let fixture = Fixture::new();
        // The desired publication has no storage objects; the ledger knows one
        // Confirmed key that must be deleted.
        let desired = desired_publication(None);
        let key = ObjectKey::new("originals/removed.jpg").unwrap();
        let removed = StorageObject {
            key: key.clone(),
            size_bytes: 7,
            sha256: digest("old"),
            content_type: Some("image/jpeg".to_owned()),
        };
        let mut ledger = ledger_for(&desired);
        record(&mut ledger, &removed, OperationState::Confirmed);
        let mut provider = FakeStorage {
            objects: HashMap::from([(
                key.as_str().to_owned(),
                (
                    removed.size_bytes,
                    removed.sha256.clone(),
                    removed.content_type.clone(),
                ),
            )]),
            ..FakeStorage::default()
        };
        provider.delete_results.push_back(Ok(()));
        // Snapshot the durable Pending while the remote DELETE is in flight.
        let ledger_path = fixture.ledger_path.clone();
        let snapshot_path = fixture.snapshot_path();
        provider.on_delete = Some(Box::new(move || {
            fs::copy(&ledger_path, &snapshot_path).unwrap();
        }));

        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        assert!(planned.operations.iter().any(
            |operation| matches!(operation, IntegrationOperation::DeleteStorage { key: candidate } if *candidate == key)
        ));
        execute(&fixture, &planned, &mut provider, &mut ledger).unwrap();
        assert!(provider.objects.is_empty());

        // Crash: the durable ledger is back at Pending; the remote object is
        // already gone.
        fs::copy(fixture.snapshot_path(), &fixture.ledger_path).unwrap();
        let durable = fixture.durable_ledger();
        assert_eq!(
            durable.r2_inventory[key.as_str()].status,
            OperationState::Pending
        );

        let mut ledger = durable;
        let mut provider = FakeStorage::default();
        let planned =
            plan_reconciliation(&desired, &KnownRemoteState::from_ledger(&ledger).unwrap())
                .unwrap();
        let error = execute(&fixture, &planned, &mut provider, &mut ledger).unwrap_err();

        assert!(matches!(
            error,
            StorageExecutionError::ReconciliationRequired(_)
        ));
        // DELETE is idempotent, but it is still never reissued automatically.
        assert!(provider.calls.is_empty());
    }
}
