//! GitHub repository execution for an [`IntegrationPlan`] as ONE commit batch.
//!
//! The executor applies only repository operations (`WriteRepository`,
//! `DeleteRepository`) from an already computed plan; storage and hosting
//! operations belong to other executors and are ignored here. It never
//! recomputes desired state and never invokes the planner.
//!
//! Unlike storage, repository operations are not individually published:
//! `write()` only creates an unreferenced remote blob, `delete()` only stages
//! a local change in the provider, and the single `commit()` is the only
//! action that publishes — through its final non-forced ref update. The
//! transactional unit is therefore the whole batch:
//!
//! ```text
//! local validation of every write against its declared source
//! (application bundle member or committed publication file)
//!     -> Pending (base revision) recorded for every path and persisted
//!     -> staging: write()/delete() in plan order
//!     -> one commit()
//!     -> Confirmed (commit revision) / removal persisted, or Unknown persisted
//! ```
//!
//! The persisted `Pending` always precedes the first remote call (blob
//! creation). If the ref update cannot be confirmed (`Network`, or a 2xx
//! response that fails to parse, surfaced as `Integrity`), every path of the
//! batch becomes `Unknown` and execution stops: no retry, no second commit,
//! no automatic HEAD reconciliation. Rejections that provably left the ref
//! untouched (authentication, permission, missing repository, `Conflict`, and
//! 422 non-fast-forward surfaced as `Other`) roll the ledger back to the
//! previously recorded state instead: `Unknown` is reserved for genuinely
//! indeterminate ref updates.

use std::collections::HashSet;
use std::fmt;
use std::io::Cursor;
use std::path::PathBuf;

use photo_publisher_provider_contracts::{
    FileMetadata, ProviderError, RepositoryPath, RepositoryProvider,
};
use sha2::{Digest, Sha256};

use crate::file_source;
use crate::{
    ApplicationBundle, DesiredRepositoryFile, GitHubInventoryEntry, IntegrationLedger,
    IntegrationOperation, IntegrationPlan, OperationState, ReconciliationRequirement,
    RepositoryFileSource,
};

/// Why repository execution failed.
///
/// Every failure stops execution before any later step; the durable ledger
/// always reflects the last safely known state.
#[derive(Debug)]
pub enum RepositoryExecutionError {
    /// The plan carries reconciliation requirements; nothing was executed.
    ReconciliationRequired(Vec<ReconciliationRequirement>),
    /// A planned operation contradicts the ledger state handed to the
    /// executor (or repeats a path inside the batch); no ledger or remote
    /// change was made.
    InconsistentPlan(String),
    /// Bundle or publication file validation failed before any ledger or
    /// remote change.
    LocalValidation {
        path: RepositoryPath,
        message: String,
    },
    /// The publication output directory could not be resolved.
    InvalidRoot(std::io::Error),
    /// The repository check or the base revision probe failed before any
    /// `Pending` was recorded; nothing was staged and the ledger is
    /// untouched.
    Initialization(String),
    /// Staging or commit provably produced no publication; the previous
    /// ledger state was restored and persisted.
    Rejected(String),
    /// The ref update may have been applied; `Unknown` was persisted for the
    /// whole batch and execution stopped without retrying.
    Ambiguous(String),
    /// The ledger could not be mutated or persisted. On a persistence failure
    /// the in-memory ledger is reloaded from the last durable state when
    /// possible, so the durable file remains authoritative.
    Ledger(anyhow::Error),
}

impl fmt::Display for RepositoryExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReconciliationRequired(requirements) => write!(
                formatter,
                "repository execution requires reconciliation first ({} requirement(s))",
                requirements.len()
            ),
            Self::InconsistentPlan(message) => write!(formatter, "inconsistent plan: {message}"),
            Self::LocalValidation { path, message } => {
                write!(formatter, "{}: {message}", path.as_str())
            }
            Self::InvalidRoot(error) => write!(
                formatter,
                "failed to resolve the publication output directory: {error}"
            ),
            Self::Initialization(message) => write!(formatter, "{message}"),
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

impl std::error::Error for RepositoryExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidRoot(error) => Some(error),
            Self::Ledger(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Outcome of one [`RepositoryExecutor::execute`] run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepositoryExecutionReport {
    pub written: Vec<RepositoryPath>,
    pub deleted: Vec<RepositoryPath>,
    /// Real commit SHA that published the batch; `None` when the plan had no
    /// repository operations and no commit was created.
    pub revision: Option<String>,
}

/// One planned repository change, in plan order.
#[derive(Debug, Clone)]
enum RepositoryOp {
    Write(DesiredRepositoryFile),
    Delete(RepositoryPath),
}

impl RepositoryOp {
    fn path(&self) -> &RepositoryPath {
        match self {
            Self::Write(file) => &file.path,
            Self::Delete(path) => path,
        }
    }
}

/// Validated content of a planned write: borrowed bundle bytes for
/// application files, or an open and revalidated file handle for
/// publication assets (the handle is rewound and ready to stream).
enum Payload<'b> {
    Bundle(&'b [u8]),
    File(std::fs::File),
}

/// Executes the repository operations of an [`IntegrationPlan`] against a
/// [`RepositoryProvider`], as a single commit batch with a durably persisted
/// ledger transition before and after the remote work.
///
/// The executor borrows the provider, the ledger, the ledger's durable path,
/// and the committed publication's output directory. The bytes of every write
/// come from its declared source: the [`ApplicationBundle`] for
/// `BundleMember` application files, or a revalidated physical file under
/// `output_dir` for `PublicationFile` assets (previews). Every write is
/// re-validated against its desired SHA-256 and size before any ledger or
/// remote change.
pub struct RepositoryExecutor<'a> {
    provider: &'a mut dyn RepositoryProvider,
    ledger: &'a mut IntegrationLedger,
    ledger_path: PathBuf,
    output_dir: PathBuf,
    canonical_root: PathBuf,
}

impl<'a> RepositoryExecutor<'a> {
    pub fn new(
        provider: &'a mut dyn RepositoryProvider,
        ledger: &'a mut IntegrationLedger,
        ledger_path: impl AsRef<std::path::Path>,
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<Self, RepositoryExecutionError> {
        let output_dir = output_dir.as_ref().to_path_buf();
        let canonical_root = output_dir
            .canonicalize()
            .map_err(RepositoryExecutionError::InvalidRoot)?;
        Ok(Self {
            provider,
            ledger,
            ledger_path: ledger_path.as_ref().to_path_buf(),
            output_dir,
            canonical_root,
        })
    }

    /// Applies the plan's repository operations in order, as one commit.
    ///
    /// A plan that still carries reconciliation requirements is refused and
    /// nothing is executed. A plan without repository operations succeeds
    /// without any remote call (in particular, without `commit()`).
    pub fn execute(
        &mut self,
        plan: &IntegrationPlan,
        bundle: &ApplicationBundle,
    ) -> Result<RepositoryExecutionReport, RepositoryExecutionError> {
        if !plan.reconciliation_requirements.is_empty() {
            return Err(RepositoryExecutionError::ReconciliationRequired(
                plan.reconciliation_requirements.clone(),
            ));
        }

        let batch: Vec<RepositoryOp> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::WriteRepository(file) => {
                    Some(RepositoryOp::Write(file.clone()))
                }
                IntegrationOperation::DeleteRepository { path } => {
                    Some(RepositoryOp::Delete(path.clone()))
                }
                // Storage and hosting operations are out of scope; they are
                // executed (or will be) by their own executors.
                IntegrationOperation::PutStorage(_)
                | IntegrationOperation::DeleteStorage { .. }
                | IntegrationOperation::PublishHosting { .. } => None,
            })
            .collect();
        let mut report = RepositoryExecutionReport::default();
        if batch.is_empty() {
            return Ok(report);
        }

        // Guards: a write requires an absent or Confirmed entry; a delete
        // requires a Confirmed entry. The previous entries are captured for a
        // possible rollback.
        let mut seen = HashSet::new();
        let mut previous = Vec::with_capacity(batch.len());
        for operation in &batch {
            let path = operation.path();
            if !seen.insert(path.as_str()) {
                return Err(RepositoryExecutionError::InconsistentPlan(format!(
                    "duplicate repository operation for {}",
                    path.as_str()
                )));
            }
            let entry = self.ledger.github_inventory.get(path.as_str());
            match (operation, entry) {
                (RepositoryOp::Write(_), None)
                | (
                    RepositoryOp::Write(_),
                    Some(GitHubInventoryEntry {
                        status: OperationState::Confirmed,
                        ..
                    }),
                ) => {}
                (RepositoryOp::Write(file), Some(entry)) => {
                    return Err(RepositoryExecutionError::InconsistentPlan(format!(
                        "cannot WRITE {} while the ledger marks it {}",
                        file.path.as_str(),
                        status_name(entry.status)
                    )));
                }
                (RepositoryOp::Delete(path), None) => {
                    return Err(RepositoryExecutionError::InconsistentPlan(format!(
                        "cannot DELETE {} because it is absent from the ledger",
                        path.as_str()
                    )));
                }
                (
                    RepositoryOp::Delete(_),
                    Some(GitHubInventoryEntry {
                        status: OperationState::Confirmed,
                        ..
                    }),
                ) => {}
                (RepositoryOp::Delete(path), Some(entry)) => {
                    return Err(RepositoryExecutionError::InconsistentPlan(format!(
                        "cannot DELETE {} while the ledger marks it {}",
                        path.as_str(),
                        status_name(entry.status)
                    )));
                }
            }
            previous.push(entry.cloned());
        }

        // Validate the full batch before any provider call and before
        // touching the ledger: on invalid content nothing remote happens and
        // no Pending is ever recorded. No partial staging is ever started for
        // invalid content. Each write resolves its bytes through its declared
        // source: the application bundle for BundleMember, or a physically
        // revalidated publication file for PublicationFile.
        let mut write_payloads: Vec<Option<Payload>> = Vec::with_capacity(batch.len());
        for operation in &batch {
            match operation {
                RepositoryOp::Write(file) => match &file.source {
                    RepositoryFileSource::BundleMember => {
                        let content = bundle_content(bundle, &file.path).ok_or_else(|| {
                            RepositoryExecutionError::LocalValidation {
                                path: file.path.clone(),
                                message: "file is not present in the application bundle".to_owned(),
                            }
                        })?;
                        let sha256 = sha256_bytes(content);
                        if sha256 != file.sha256 {
                            return Err(RepositoryExecutionError::LocalValidation {
                                path: file.path.clone(),
                                message: format!(
                                    "bundle content SHA-256 {sha256} does not match the desired {}",
                                    file.sha256
                                ),
                            });
                        }
                        if content.len() as u64 != file.size_bytes {
                            return Err(RepositoryExecutionError::LocalValidation {
                                path: file.path.clone(),
                                message: format!(
                                    "bundle content size {} does not match the desired {} bytes",
                                    content.len(),
                                    file.size_bytes
                                ),
                            });
                        }
                        write_payloads.push(Some(Payload::Bundle(content)));
                    }
                    RepositoryFileSource::PublicationFile { source_path } => {
                        let handle = file_source::open_validated_source(
                            &self.output_dir,
                            &self.canonical_root,
                            source_path,
                            &file.sha256,
                            file.size_bytes,
                        )
                        .map_err(|error| {
                            RepositoryExecutionError::LocalValidation {
                                path: file.path.clone(),
                                message: format!("invalid publication file: {error:#}"),
                            }
                        })?;
                        write_payloads.push(Some(Payload::File(handle)));
                    }
                },
                RepositoryOp::Delete(_) => write_payloads.push(None),
            }
        }

        // Repository existence and branch resolution are checked before any
        // Pending is recorded.
        self.provider.ensure_repository().map_err(|error| {
            RepositoryExecutionError::Initialization(format!(
                "repository is not available before staging: {error}"
            ))
        })?;

        // Deterministic commit message derived from the committed local
        // generation; no timestamps or randomness.
        let message = format!("photo-publisher publish {}", self.ledger.local_generation);

        // Base revision probe: with no staged changes the provider's commit()
        // only reads the current base and publishes nothing.
        let base_revision = self.provider.commit(&message).map_err(|error| {
            RepositoryExecutionError::Initialization(format!(
                "failed to read the repository base revision: {error}"
            ))
        })?;
        let base_revision = base_revision.revision;

        // Pending for every path, carrying the batch base revision: "a batch
        // is in flight; publication is not confirmed". Persisted atomically
        // before the first remote call (blob creation).
        for (operation, previous_entry) in batch.iter().zip(&previous) {
            let (path, sha256) = match operation {
                RepositoryOp::Write(file) => (&file.path, file.sha256.clone()),
                RepositoryOp::Delete(path) => (
                    path,
                    previous_entry
                        .as_ref()
                        .expect("guard ensured a Confirmed entry")
                        .sha256
                        .clone(),
                ),
            };
            self.record(path, sha256, base_revision.clone(), OperationState::Pending)?;
        }
        self.persist()?;

        // Staging: writes create unreferenced remote blobs; deletes are local
        // to the provider. A staging failure proves nothing was published
        // (only commit() publishes), so the previous state is restored.
        let mut staged: Vec<Option<FileMetadata>> = Vec::with_capacity(batch.len());
        for (operation, payload) in batch.iter().zip(write_payloads.iter_mut()) {
            match operation {
                RepositoryOp::Write(file) => {
                    let payload = payload.as_mut().expect("writes were validated above");
                    let result = match payload {
                        Payload::Bundle(content) => {
                            let mut reader = Cursor::new(*content);
                            self.provider.write(&file.path, &mut reader)
                        }
                        Payload::File(handle) => self.provider.write(&file.path, handle),
                    };
                    match result {
                        Ok(metadata) => {
                            if metadata.path != file.path
                                || metadata.sha256 != file.sha256
                                || metadata.size_bytes != file.size_bytes
                            {
                                self.rollback(&batch, &previous)?;
                                self.persist()?;
                                return Err(RepositoryExecutionError::Rejected(format!(
                                    "write of {} returned divergent metadata; the batch was not committed",
                                    file.path.as_str()
                                )));
                            }
                            staged.push(Some(metadata));
                        }
                        Err(error) => {
                            self.rollback(&batch, &previous)?;
                            self.persist()?;
                            return Err(RepositoryExecutionError::Rejected(format!(
                                "staging of {} failed before publication: {error}",
                                file.path.as_str()
                            )));
                        }
                    }
                }
                RepositoryOp::Delete(path) => {
                    if let Err(error) = self.provider.delete(path) {
                        self.rollback(&batch, &previous)?;
                        self.persist()?;
                        return Err(RepositoryExecutionError::Rejected(format!(
                            "staging the deletion of {} failed before publication: {error}",
                            path.as_str()
                        )));
                    }
                    staged.push(None);
                }
            }
        }

        // Single commit for the whole batch.
        match self.provider.commit(&message) {
            Ok(info) => {
                for (operation, metadata) in batch.iter().zip(&staged) {
                    match operation {
                        RepositoryOp::Write(file) => {
                            let sha256 = metadata.as_ref().expect("staged above").sha256.clone();
                            self.record(
                                &file.path,
                                sha256,
                                info.revision.clone(),
                                OperationState::Confirmed,
                            )?;
                        }
                        RepositoryOp::Delete(path) => {
                            self.ledger
                                .remove_github_file(path)
                                .map_err(RepositoryExecutionError::Ledger)?;
                        }
                    }
                }
                self.persist()?;
                for operation in &batch {
                    match operation {
                        RepositoryOp::Write(file) => report.written.push(file.path.clone()),
                        RepositoryOp::Delete(path) => report.deleted.push(path.clone()),
                    }
                }
                report.revision = Some(info.revision);
                Ok(report)
            }
            Err(error) => match classify_commit_failure(&error) {
                FailureKind::NoEffect => {
                    self.rollback(&batch, &previous)?;
                    self.persist()?;
                    Err(RepositoryExecutionError::Rejected(format!(
                        "commit was rejected and the ref was not updated: {error}"
                    )))
                }
                FailureKind::Ambiguous => {
                    self.record_batch_pending_state(
                        &batch,
                        &previous,
                        &base_revision,
                        OperationState::Unknown,
                    )?;
                    self.persist()?;
                    Err(RepositoryExecutionError::Ambiguous(format!(
                        "commit outcome cannot be determined; the ref may have been updated: {error}"
                    )))
                }
            },
        }
    }

    /// Restores the ledger state known before the batch for every path.
    fn rollback(
        &mut self,
        batch: &[RepositoryOp],
        previous: &[Option<GitHubInventoryEntry>],
    ) -> Result<(), RepositoryExecutionError> {
        for (operation, previous_entry) in batch.iter().zip(previous) {
            let path = operation.path();
            match previous_entry {
                None => self
                    .ledger
                    .remove_github_file(path)
                    .map_err(RepositoryExecutionError::Ledger)?,
                Some(entry) => {
                    self.record(
                        path,
                        entry.sha256.clone(),
                        entry.revision.clone(),
                        entry.status,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Records the batch state (Pending before staging, Unknown after an
    /// ambiguous commit) with the captured base revision.
    fn record_batch_pending_state(
        &mut self,
        batch: &[RepositoryOp],
        previous: &[Option<GitHubInventoryEntry>],
        base_revision: &str,
        status: OperationState,
    ) -> Result<(), RepositoryExecutionError> {
        for (operation, previous_entry) in batch.iter().zip(previous) {
            let (path, sha256) = match operation {
                RepositoryOp::Write(file) => (&file.path, file.sha256.clone()),
                RepositoryOp::Delete(path) => (
                    path,
                    previous_entry
                        .as_ref()
                        .expect("guard ensured a Confirmed entry")
                        .sha256
                        .clone(),
                ),
            };
            self.record(path, sha256, base_revision.to_owned(), status)?;
        }
        Ok(())
    }

    fn record(
        &mut self,
        path: &RepositoryPath,
        sha256: String,
        revision: String,
        status: OperationState,
    ) -> Result<(), RepositoryExecutionError> {
        self.ledger
            .record_github_file(path, sha256, revision, status)
            .map_err(RepositoryExecutionError::Ledger)
    }

    fn persist(&mut self) -> Result<(), RepositoryExecutionError> {
        if let Err(error) = self.ledger.write_to(&self.ledger_path) {
            // The durable file is authoritative; drop the in-memory state that
            // could not be persisted by reloading the last durable ledger.
            if let Ok(durable) = IntegrationLedger::read_from(&self.ledger_path) {
                *self.ledger = durable;
            }
            return Err(RepositoryExecutionError::Ledger(error));
        }
        Ok(())
    }
}

/// Whether a commit failure provably left the branch ref untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    NoEffect,
    Ambiguous,
}

/// Classifies a `commit()` failure by what the real GitHub provider can have
/// done, not by the enum alone.
///
/// Inside the provider's commit, every step before the last one (base read,
/// tree creation, commit creation) cannot publish: only the final non-forced
/// `PATCH` of the ref publishes. An HTTP error response (`4xx`) proves the
/// request was rejected, so wherever it occurred the ref was not updated —
/// this includes 422 non-fast-forward, surfaced by the provider as `Other`.
/// Only a missing response (`Network`) or a successful response that could
/// not be parsed (`Integrity`) leaves the ref update genuinely indeterminate.
fn classify_commit_failure(error: &ProviderError) -> FailureKind {
    match error {
        ProviderError::AuthenticationFailed
        | ProviderError::AuthenticationRequired
        | ProviderError::PermissionDenied
        | ProviderError::NotFound
        | ProviderError::Conflict
        | ProviderError::Other
        | ProviderError::InvalidKey(_)
        | ProviderError::InvalidPath(_)
        | ProviderError::AlreadyExists
        | ProviderError::Unsupported
        | ProviderError::Io(_) => FailureKind::NoEffect,
        ProviderError::Network | ProviderError::Integrity => FailureKind::Ambiguous,
    }
}

/// Returns the bundle content of `path`, if the bundle contains the file.
fn bundle_content<'b>(bundle: &'b ApplicationBundle, path: &RepositoryPath) -> Option<&'b [u8]> {
    bundle
        .files()
        .iter()
        .find(|file| file.path() == path)
        .map(|file| file.content())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
    use std::collections::{HashMap, VecDeque};
    use std::io::Read;

    use photo_publisher_provider_contracts::{CommitInfo, ProviderResult};
    use tempfile::{tempdir, TempDir};

    use crate::plan_reconciliation;
    use crate::PublicationPath;

    const BASE_REVISION: &str = "base-revision";

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RepoCall {
        Ensure,
        Probe,
        Write(String),
        Delete(String),
        Commit(usize),
    }

    enum CommitScript {
        Succeed,
        Fail(ProviderError),
        /// Simulates a ref update applied remotely whose response was lost.
        ApplyThenFail,
    }

    #[derive(Default)]
    struct FakeRepository {
        files: HashMap<String, Vec<u8>>,
        revision: String,
        staged: HashMap<String, Option<Vec<u8>>>,
        commit_seq: u64,
        calls: Vec<RepoCall>,
        messages: Vec<String>,
        write_scripts: VecDeque<ProviderResult<Option<FileMetadata>>>,
        commit_scripts: VecDeque<CommitScript>,
        ensure_failure: Option<ProviderError>,
        probe_failure: Option<ProviderError>,
        on_write: Option<Box<dyn FnMut()>>,
        on_commit: Option<Box<dyn FnMut()>>,
    }

    impl FakeRepository {
        fn new() -> Self {
            Self {
                revision: BASE_REVISION.to_owned(),
                ..Self::default()
            }
        }

        fn commit_count(&self) -> usize {
            self.calls
                .iter()
                .filter(|call| matches!(call, RepoCall::Commit(_)))
                .count()
        }
    }

    impl RepositoryProvider for FakeRepository {
        fn ensure_repository(&mut self) -> ProviderResult<()> {
            self.calls.push(RepoCall::Ensure);
            match self.ensure_failure.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
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
            self.calls.push(RepoCall::Write(path.as_str().to_owned()));
            let script = self.write_scripts.pop_front().unwrap_or(Ok(None));
            if let Some(hook) = self.on_write.as_mut() {
                hook();
            }
            // Like the real provider, a failed blob creation stages nothing.
            let script = script?;
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(ProviderError::from_io)?;
            self.staged
                .insert(path.as_str().to_owned(), Some(bytes.clone()));
            Ok(script.unwrap_or(FileMetadata {
                path: path.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_bytes(&bytes),
            }))
        }

        fn delete(&mut self, path: &RepositoryPath) -> ProviderResult<()> {
            self.calls.push(RepoCall::Delete(path.as_str().to_owned()));
            self.staged.insert(path.as_str().to_owned(), None);
            Ok(())
        }

        fn commit(&mut self, message: &str) -> ProviderResult<CommitInfo> {
            self.messages.push(message.to_owned());
            if self.staged.is_empty() {
                // Probe path: reads the base revision and publishes nothing.
                self.calls.push(RepoCall::Probe);
                return match self.probe_failure.take() {
                    Some(error) => Err(error),
                    None => Ok(CommitInfo {
                        revision: self.revision.clone(),
                        message: message.to_owned(),
                        changed_paths: Vec::new(),
                    }),
                };
            }
            self.calls.push(RepoCall::Commit(self.staged.len()));
            if let Some(hook) = self.on_commit.as_mut() {
                hook();
            }
            let script = self
                .commit_scripts
                .pop_front()
                .unwrap_or(CommitScript::Succeed);
            match script {
                // Like the real provider, staged changes survive a failed
                // commit; nothing was published.
                CommitScript::Fail(error) => Err(error),
                CommitScript::Succeed | CommitScript::ApplyThenFail => {
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
                    self.commit_seq += 1;
                    self.revision = format!("commit-{:02}", self.commit_seq);
                    let info = CommitInfo {
                        revision: self.revision.clone(),
                        message: message.to_owned(),
                        changed_paths,
                    };
                    match script {
                        CommitScript::Succeed => Ok(info),
                        CommitScript::ApplyThenFail => Err(ProviderError::Network),
                        CommitScript::Fail(_) => unreachable!("handled above"),
                    }
                }
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

        fn snapshot_path(&self) -> std::path::PathBuf {
            self.root.path().join("ledger-snapshot.json")
        }
    }

    fn digest(value: &str) -> String {
        sha256_bytes(value.as_bytes())
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

    fn bundle(files: &[(&str, &[u8])]) -> ApplicationBundle {
        ApplicationBundle::from_files(
            files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_vec()))
                .collect(),
        )
        .unwrap()
    }

    fn desired_file(path: &str, content: &[u8]) -> DesiredRepositoryFile {
        DesiredRepositoryFile::new(
            RepositoryPath::new(path).unwrap(),
            sha256_bytes(content),
            content.len() as u64,
            RepositoryFileSource::BundleMember,
        )
        .unwrap()
    }

    fn plan(operations: Vec<IntegrationOperation>) -> IntegrationPlan {
        IntegrationPlan {
            operations,
            reconciliation_requirements: Vec::new(),
        }
    }

    fn execute(
        fixture: &Fixture,
        plan: &IntegrationPlan,
        bundle: &ApplicationBundle,
        provider: &mut FakeRepository,
        ledger: &mut IntegrationLedger,
    ) -> Result<RepositoryExecutionReport, RepositoryExecutionError> {
        RepositoryExecutor::new(provider, ledger, &fixture.ledger_path, &fixture.output)
            .unwrap()
            .execute(plan, bundle)
    }

    #[test]
    fn single_write_is_committed_once_and_confirmed() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"<h1>ok</h1>")]);
        let file = desired_file("index.html", b"<h1>ok</h1>");
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let report = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(file.clone())]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report.written, vec![file.path.clone()]);
        assert_eq!(report.revision.as_deref(), Some("commit-01"));
        // One remote batch: ensure, base probe, one write, one commit.
        assert_eq!(
            provider.calls,
            vec![
                RepoCall::Ensure,
                RepoCall::Probe,
                RepoCall::Write("index.html".to_owned()),
                RepoCall::Commit(1),
            ]
        );
        // Deterministic message for both the probe and the batch commit.
        assert_eq!(
            provider.messages,
            vec!["photo-publisher publish g-000001".to_owned(); 2]
        );
        let entry = &ledger.github_inventory["index.html"];
        assert_eq!(entry.status, OperationState::Confirmed);
        assert_eq!(entry.sha256, file.sha256);
        assert_eq!(entry.revision, "commit-01");
        assert_eq!(provider.files["index.html"], b"<h1>ok</h1>");
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn multiple_writes_produce_a_single_commit() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("a.txt", b"a"), ("assets/b.txt", b"b")]);
        let first = desired_file("a.txt", b"a");
        let second = desired_file("assets/b.txt", b"b");
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let report = execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::WriteRepository(first.clone()),
                IntegrationOperation::WriteRepository(second.clone()),
            ]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(provider.commit_count(), 1);
        assert_eq!(report.revision.as_deref(), Some("commit-01"));
        assert_eq!(
            report.written,
            vec![first.path.clone(), second.path.clone()]
        );
        for file in [&first, &second] {
            let entry = &ledger.github_inventory[file.path.as_str()];
            assert_eq!(entry.status, OperationState::Confirmed);
            assert_eq!(entry.sha256, file.sha256);
            assert_eq!(entry.revision, "commit-01");
        }
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn multiple_deletes_produce_a_single_commit_and_remove_entries() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let first = RepositoryPath::new("a.txt").unwrap();
        let second = RepositoryPath::new("assets/b.txt").unwrap();
        let mut ledger = empty_ledger();
        for (path, content) in [(&first, b"a".as_slice()), (&second, b"b".as_slice())] {
            ledger
                .record_github_file(
                    path,
                    sha256_bytes(content),
                    "rev-0".to_owned(),
                    OperationState::Confirmed,
                )
                .unwrap();
        }
        ledger.write_to(&fixture.ledger_path).unwrap();
        let mut provider = FakeRepository::new();
        provider.files.insert("a.txt".to_owned(), b"a".to_vec());
        provider
            .files
            .insert("assets/b.txt".to_owned(), b"b".to_vec());

        let report = execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::DeleteRepository {
                    path: first.clone(),
                },
                IntegrationOperation::DeleteRepository {
                    path: second.clone(),
                },
            ]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(provider.commit_count(), 1);
        assert_eq!(report.deleted, vec![first.clone(), second.clone()]);
        // No tombstones: absence is the final state.
        assert!(!ledger.github_inventory.contains_key("a.txt"));
        assert!(!ledger.github_inventory.contains_key("assets/b.txt"));
        assert!(!provider.files.contains_key("a.txt"));
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn mixed_batch_stages_in_plan_order_and_commits_once() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("a.txt", b"a"), ("b.txt", b"b"), ("d.txt", b"d")]);
        let mut ledger = empty_ledger();
        ledger
            .record_github_file(
                &RepositoryPath::new("c.txt").unwrap(),
                digest("c"),
                "rev-0".to_owned(),
                OperationState::Confirmed,
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();
        let mut provider = FakeRepository::new();
        provider.files.insert("c.txt".to_owned(), b"c".to_vec());

        execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::WriteRepository(desired_file("a.txt", b"a")),
                IntegrationOperation::WriteRepository(desired_file("b.txt", b"b")),
                IntegrationOperation::DeleteRepository {
                    path: RepositoryPath::new("c.txt").unwrap(),
                },
                IntegrationOperation::WriteRepository(desired_file("d.txt", b"d")),
            ]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(
            provider.calls,
            vec![
                RepoCall::Ensure,
                RepoCall::Probe,
                RepoCall::Write("a.txt".to_owned()),
                RepoCall::Write("b.txt".to_owned()),
                RepoCall::Delete("c.txt".to_owned()),
                RepoCall::Write("d.txt".to_owned()),
                RepoCall::Commit(4),
            ]
        );
        assert_eq!(provider.commit_count(), 1);
        assert!(!provider.files.contains_key("c.txt"));
        assert!(!ledger.github_inventory.contains_key("c.txt"));
        assert_eq!(ledger.github_inventory["d.txt"].revision, "commit-01");
    }

    #[test]
    fn plan_without_repository_operations_makes_no_remote_calls() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let object = crate::DesiredStorageObject::new(
            photo_publisher_provider_contracts::ObjectKey::new("originals/x.jpg").unwrap(),
            crate::PublicationPath::new("photos/download/x.jpg").unwrap(),
            digest("x"),
            1,
            None,
        )
        .unwrap();
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let report = execute(
            &fixture,
            &plan(vec![IntegrationOperation::PutStorage(object)]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(report, RepositoryExecutionReport::default());
        assert_eq!(provider.calls, Vec::new());
        assert!(provider.messages.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn plan_with_reconciliation_requirements_is_refused() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let file = desired_file("index.html", b"index");
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        for requirement in [
            ReconciliationRequirement::ConfigurationChanged,
            ReconciliationRequirement::RepositoryPending {
                path: file.path.clone(),
            },
            ReconciliationRequirement::RepositoryUnknown {
                path: file.path.clone(),
            },
        ] {
            let blocked = IntegrationPlan {
                operations: vec![IntegrationOperation::WriteRepository(file.clone())],
                reconciliation_requirements: vec![requirement],
            };
            let error =
                execute(&fixture, &blocked, &bundle, &mut provider, &mut ledger).unwrap_err();
            assert!(matches!(
                error,
                RepositoryExecutionError::ReconciliationRequired(_)
            ));
        }
        assert_eq!(provider.calls, Vec::new());
        assert!(ledger.github_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn write_over_a_pending_or_unknown_entry_is_refused() {
        for status in [OperationState::Pending, OperationState::Unknown] {
            let fixture = Fixture::new();
            let bundle = bundle(&[("index.html", b"index")]);
            let file = desired_file("index.html", b"index");
            let mut provider = FakeRepository::new();
            let mut ledger = empty_ledger();
            ledger
                .record_github_file(
                    &file.path,
                    file.sha256.clone(),
                    BASE_REVISION.to_owned(),
                    status,
                )
                .unwrap();

            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::WriteRepository(file.clone())]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(
                error,
                RepositoryExecutionError::InconsistentPlan(_)
            ));
            assert_eq!(provider.calls, Vec::new());
            assert_eq!(ledger.github_inventory["index.html"].status, status);
            assert!(!fixture.ledger_path.exists());
        }
    }

    #[test]
    fn delete_of_a_path_not_confirmed_is_refused() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let path = RepositoryPath::new("a.txt").unwrap();
        let mut provider = FakeRepository::new();

        // Absent from the ledger.
        let mut ledger = empty_ledger();
        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::DeleteRepository {
                path: path.clone(),
            }]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            RepositoryExecutionError::InconsistentPlan(_)
        ));

        // Recorded, but not Confirmed.
        for status in [OperationState::Pending, OperationState::Unknown] {
            let mut ledger = empty_ledger();
            ledger
                .record_github_file(&path, digest("a"), BASE_REVISION.to_owned(), status)
                .unwrap();
            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::DeleteRepository {
                    path: path.clone(),
                }]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                RepositoryExecutionError::InconsistentPlan(_)
            ));
        }
        assert_eq!(provider.calls, Vec::new());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn bundle_content_mismatch_changes_nothing() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"actual content")]);
        let mismatched = desired_file("index.html", b"expected different content");
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(mismatched)]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::LocalValidation { .. }
        ));
        assert_eq!(provider.calls, Vec::new());
        assert!(ledger.github_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn bundle_missing_file_changes_nothing() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let missing = desired_file("missing.txt", b"whatever");
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(missing)]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::LocalValidation { .. }
        ));
        assert_eq!(provider.calls, Vec::new());
        assert!(ledger.github_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn write_returning_divergent_metadata_rolls_back_without_commit() {
        for pre_existing in [false, true] {
            let fixture = Fixture::new();
            let bundle = bundle(&[("index.html", b"index")]);
            let file = desired_file("index.html", b"index");
            let mut provider = FakeRepository::new();
            provider.write_scripts.push_back(Ok(Some(FileMetadata {
                path: file.path.clone(),
                size_bytes: 5,
                sha256: digest("bogus"),
            })));
            let mut ledger = empty_ledger();
            if pre_existing {
                ledger
                    .record_github_file(
                        &file.path,
                        digest("old"),
                        "rev-0".to_owned(),
                        OperationState::Confirmed,
                    )
                    .unwrap();
                ledger.write_to(&fixture.ledger_path).unwrap();
            }
            let before = ledger.clone();

            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::WriteRepository(file.clone())]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, RepositoryExecutionError::Rejected(_)));
            // The batch was staged but never committed.
            assert_eq!(provider.commit_count(), 0);
            assert!(provider.files.is_empty());
            // The ledger is back to its previous state, durably.
            assert_eq!(ledger, before);
            assert_eq!(fixture.durable_ledger(), before);
        }
    }

    #[test]
    fn first_write_failure_rolls_back_without_commit() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("a.txt", b"a"), ("b.txt", b"b")]);
        let mut provider = FakeRepository::new();
        provider
            .write_scripts
            .push_back(Err(ProviderError::Network));
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::WriteRepository(desired_file("a.txt", b"a")),
                IntegrationOperation::WriteRepository(desired_file("b.txt", b"b")),
            ]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, RepositoryExecutionError::Rejected(_)));
        assert_eq!(provider.commit_count(), 0);
        assert!(!provider.calls.contains(&RepoCall::Commit(0)));
        assert!(ledger.github_inventory.is_empty());
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn second_write_failure_rolls_back_without_commit() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("a.txt", b"a"), ("b.txt", b"b")]);
        let mut provider = FakeRepository::new();
        provider.write_scripts.push_back(Ok(None));
        provider
            .write_scripts
            .push_back(Err(ProviderError::Conflict));
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::WriteRepository(desired_file("a.txt", b"a")),
                IntegrationOperation::WriteRepository(desired_file("b.txt", b"b")),
            ]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, RepositoryExecutionError::Rejected(_)));
        assert_eq!(provider.commit_count(), 0);
        assert!(provider.files.is_empty());
        assert!(ledger.github_inventory.is_empty());
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn pending_with_the_base_revision_is_durable_before_the_first_write() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let file = desired_file("index.html", b"index");
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();
        let observed: std::rc::Rc<std::cell::RefCell<Vec<(OperationState, String, String)>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let ledger_path = fixture.ledger_path.clone();
        let observed_in_hook = std::rc::Rc::clone(&observed);
        let path = "index.html".to_owned();
        provider.on_write = Some(Box::new(move || {
            // The fundamental safety rule: a persisted Pending (carrying the
            // base revision captured by the probe) must exist before the
            // first remote call of the batch.
            let durable = IntegrationLedger::read_from(&ledger_path).unwrap();
            let entry = durable.github_inventory[&path].clone();
            observed_in_hook
                .borrow_mut()
                .push((entry.status, entry.sha256, entry.revision));
        }));

        execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(file.clone())]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert_eq!(
            *observed.borrow(),
            vec![(
                OperationState::Pending,
                file.sha256.clone(),
                BASE_REVISION.to_owned()
            )]
        );
        // The probe published nothing: the revision only advanced with the
        // real commit.
        assert_eq!(
            fixture.durable_ledger().github_inventory["index.html"].revision,
            "commit-01"
        );
    }

    #[test]
    fn commit_network_or_integrity_marks_the_whole_batch_unknown() {
        for failure in [ProviderError::Network, ProviderError::Integrity] {
            let fixture = Fixture::new();
            let bundle = bundle(&[("a.txt", b"a"), ("b.txt", b"b")]);
            let mut provider = FakeRepository::new();
            // ProviderError is not Clone: rebuild per iteration.
            let commit_failure = match failure {
                ProviderError::Network => ProviderError::Network,
                _ => ProviderError::Integrity,
            };
            provider
                .commit_scripts
                .push_back(CommitScript::Fail(commit_failure));
            let mut ledger = empty_ledger();

            let error = execute(
                &fixture,
                &plan(vec![
                    IntegrationOperation::WriteRepository(desired_file("a.txt", b"a")),
                    IntegrationOperation::WriteRepository(desired_file("b.txt", b"b")),
                ]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, RepositoryExecutionError::Ambiguous(_)));
            for path in ["a.txt", "b.txt"] {
                let entry = &ledger.github_inventory[path];
                assert_eq!(entry.status, OperationState::Unknown);
                // The Unknown entry keeps the pending intent and the base
                // revision: the ref update may have been applied.
                assert_eq!(entry.revision, BASE_REVISION);
                assert_eq!(
                    entry.sha256,
                    sha256_bytes(match path {
                        "a.txt" => b"a",
                        _ => b"b",
                    })
                );
            }
            assert_eq!(fixture.durable_ledger(), ledger);
            assert_eq!(provider.commit_count(), 1);
            // A lost commit never applied the staged changes in this fake.
            assert!(provider.files.is_empty());
            let _ = failure;
        }
    }

    #[test]
    fn commit_rejections_roll_back_the_previous_state() {
        for failure in [
            ProviderError::Conflict,
            ProviderError::Other, // 422 non-fast-forward surfaces as Other
            ProviderError::AuthenticationFailed,
            ProviderError::PermissionDenied,
            ProviderError::NotFound,
        ] {
            let fixture = Fixture::new();
            let bundle = bundle(&[("index.html", b"index")]);
            let file = desired_file("index.html", b"index");
            let mut provider = FakeRepository::new();
            provider
                .commit_scripts
                .push_back(CommitScript::Fail(failure));
            provider.files.insert("old.txt".to_owned(), b"old".to_vec());
            let mut ledger = empty_ledger();
            ledger
                .record_github_file(
                    &RepositoryPath::new("old.txt").unwrap(),
                    digest("old"),
                    "rev-0".to_owned(),
                    OperationState::Confirmed,
                )
                .unwrap();
            ledger.write_to(&fixture.ledger_path).unwrap();
            let before = ledger.clone();

            let error = execute(
                &fixture,
                &plan(vec![
                    IntegrationOperation::WriteRepository(file),
                    IntegrationOperation::DeleteRepository {
                        path: RepositoryPath::new("old.txt").unwrap(),
                    },
                ]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(error, RepositoryExecutionError::Rejected(_)));
            assert_eq!(provider.commit_count(), 1);
            // Nothing published: the reject proves the ref was untouched.
            assert!(!provider.files.contains_key("index.html"));
            assert!(provider.files.contains_key("old.txt"));
            assert_eq!(provider.revision, BASE_REVISION);
            assert_eq!(ledger, before);
            assert_eq!(fixture.durable_ledger(), before);
        }
    }

    #[test]
    fn ensure_repository_failure_aborts_before_pending() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        provider.ensure_failure = Some(ProviderError::NotFound);
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(desired_file(
                "index.html",
                b"index",
            ))]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, RepositoryExecutionError::Initialization(_)));
        assert_eq!(provider.calls, vec![RepoCall::Ensure]);
        assert!(ledger.github_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn base_probe_failure_aborts_before_pending_and_publishes_nothing() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        provider.probe_failure = Some(ProviderError::Network);
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(desired_file(
                "index.html",
                b"index",
            ))]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(error, RepositoryExecutionError::Initialization(_)));
        assert_eq!(provider.calls, vec![RepoCall::Ensure, RepoCall::Probe]);
        assert_eq!(provider.revision, BASE_REVISION);
        assert!(provider.files.is_empty());
        assert!(ledger.github_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn crash_after_pending_blocks_the_next_cycle() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index"), ("app.js", b"app")]);
        let desired = desired_publication(&bundle);
        // Durable state left by a crash after the batch Pending was persisted
        // and before any staging happened.
        let mut crashed = ledger_for(&desired);
        for file in desired.repository_files() {
            crashed
                .record_github_file(
                    &file.path,
                    file.sha256.clone(),
                    BASE_REVISION.to_owned(),
                    OperationState::Pending,
                )
                .unwrap();
        }
        crashed.write_to(&fixture.ledger_path).unwrap();

        let mut ledger = fixture.durable_ledger();
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        assert!(planned
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::RepositoryPending { .. }
            )));
        assert!(!planned
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::WriteRepository(_))));

        let mut provider = FakeRepository::new();
        let error = execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::ReconciliationRequired(_)
        ));
        assert_eq!(provider.calls, Vec::new());
        assert!(provider.files.is_empty());
        assert_eq!(
            fixture.durable_ledger().github_inventory["index.html"].status,
            OperationState::Pending
        );
    }

    #[test]
    fn published_commit_with_lost_response_is_never_repeated() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let desired = desired_publication(&bundle);
        let mut ledger = ledger_for(&desired);
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        assert!(planned
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::WriteRepository(_))));

        // The ref update was applied remotely, but the response was lost.
        let mut provider = FakeRepository::new();
        provider
            .commit_scripts
            .push_back(CommitScript::ApplyThenFail);
        let error = execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap_err();
        assert!(matches!(error, RepositoryExecutionError::Ambiguous(_)));
        assert_eq!(provider.commit_count(), 1);
        // The remote really moved.
        assert_eq!(provider.revision, "commit-01");
        assert_eq!(provider.files["index.html"], b"index");
        // The durable ledger honestly records the indeterminacy.
        let durable = fixture.durable_ledger();
        assert_eq!(
            durable.github_inventory["index.html"].status,
            OperationState::Unknown
        );

        // Next cycle: the planner blocks and no second commit is attempted.
        let mut ledger = durable;
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        assert!(planned
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::RepositoryUnknown { .. }
            )));
        let mut provider = FakeRepository::new();
        let error = execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap_err();
        assert!(matches!(
            error,
            RepositoryExecutionError::ReconciliationRequired(_)
        ));
        assert_eq!(provider.calls, Vec::new());
        assert_eq!(provider.commit_count(), 0);
    }

    #[test]
    fn published_commit_without_persisted_confirmation_is_never_repeated() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let desired = desired_publication(&bundle);
        let mut ledger = ledger_for(&desired);
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();

        // Snapshot the durable Pending while the commit is in flight.
        let mut provider = FakeRepository::new();
        let ledger_path = fixture.ledger_path.clone();
        let snapshot_path = fixture.snapshot_path();
        provider.on_commit = Some(Box::new(move || {
            std::fs::copy(&ledger_path, &snapshot_path).unwrap();
        }));
        execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap();
        assert_eq!(provider.commit_count(), 1);
        assert_eq!(provider.files["index.html"], b"index");

        // Emulate a crash that lost the Confirmed write: the durable ledger is
        // back at Pending while the remote branch points at the new commit.
        std::fs::copy(fixture.snapshot_path(), &fixture.ledger_path).unwrap();
        let durable = fixture.durable_ledger();
        assert_eq!(
            durable.github_inventory["index.html"].status,
            OperationState::Pending
        );

        let mut ledger = durable;
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        let mut provider = FakeRepository::new();
        let error = execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::ReconciliationRequired(_)
        ));
        // No second commit, no remote call at all.
        assert_eq!(provider.calls, Vec::new());
        assert_eq!(provider.commit_count(), 0);
    }

    #[test]
    fn idempotent_second_cycle_makes_no_commit() {
        let fixture = Fixture::new();
        let bundle = bundle(&[("index.html", b"index")]);
        let desired = desired_publication(&bundle);
        let mut ledger = ledger_for(&desired);
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        let mut provider = FakeRepository::new();
        execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap();

        // Same desired state + Confirmed ledger: the planner emits no
        // repository operations, so the executor performs no remote call.
        let planned = plan_reconciliation(
            &desired,
            &crate::KnownRemoteState::from_ledger(&ledger).unwrap(),
        )
        .unwrap();
        assert!(!planned.operations.iter().any(|operation| matches!(
            operation,
            IntegrationOperation::WriteRepository(_)
                | IntegrationOperation::DeleteRepository { .. }
        )));
        let mut provider = FakeRepository::new();
        let report = execute(&fixture, &planned, &bundle, &mut provider, &mut ledger).unwrap();
        assert_eq!(report, RepositoryExecutionReport::default());
        assert_eq!(provider.calls, Vec::new());
        assert_eq!(provider.commit_count(), 0);
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
            hosting: crate::HostingPublicationConfig::new("hosting-project", None).unwrap(),
        }
    }

    fn desired_publication(bundle: &ApplicationBundle) -> crate::DesiredPublication {
        crate::DesiredPublication::new(
            &configuration(),
            crate::LocalPublication::new("g-000001", digest("state"), digest("manifest")).unwrap(),
            bundle,
        )
        .unwrap()
    }

    fn ledger_for(desired: &crate::DesiredPublication) -> IntegrationLedger {
        IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )
        .unwrap()
    }

    /// A repository-sourced publication asset: bytes committed physically
    /// under the publication output, declared with its planned identity.
    fn publication_file(fixture: &Fixture, name: &str, bytes: &[u8]) -> DesiredRepositoryFile {
        let relative = format!("photos/preview/{name}.jpg");
        let physical = fixture.output.join(&relative);
        std::fs::create_dir_all(physical.parent().unwrap()).unwrap();
        std::fs::write(&physical, bytes).unwrap();
        DesiredRepositoryFile::new(
            RepositoryPath::new(format!("public/{relative}")).unwrap(),
            sha256_bytes(bytes),
            bytes.len() as u64,
            RepositoryFileSource::PublicationFile {
                source_path: PublicationPath::new(relative).unwrap(),
            },
        )
        .unwrap()
    }

    #[test]
    fn publication_file_write_is_staged_and_confirmed() {
        let fixture = Fixture::new();
        let file = publication_file(&fixture, "a", b"preview-a-bytes");
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let report = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(file.clone())]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        // The bytes staged remotely are exactly the publication file bytes.
        assert_eq!(
            provider.files["public/photos/preview/a.jpg"],
            b"preview-a-bytes"
        );
        assert_eq!(report.written, vec![file.path.clone()]);
        let entry = &ledger.github_inventory["public/photos/preview/a.jpg"];
        assert_eq!(entry.status, OperationState::Confirmed);
        assert_eq!(entry.sha256, file.sha256);
        assert_eq!(fixture.durable_ledger(), ledger);
    }

    #[test]
    fn mixed_bundle_and_publication_files_commit_once() {
        let fixture = Fixture::new();
        let preview_a = publication_file(&fixture, "a", b"preview-a");
        let preview_b = publication_file(&fixture, "b", b"preview-b");
        let gallery = DesiredRepositoryFile::new(
            RepositoryPath::new("gallery.json").unwrap(),
            sha256_bytes(b"{\"photos\":[]}"),
            13,
            RepositoryFileSource::BundleMember,
        )
        .unwrap();
        let bundle = bundle(&[
            ("index.html", b"index".as_slice()),
            ("gallery.json", b"{\"photos\":[]}".as_slice()),
        ] as &[(&str, &[u8])]);
        let mut provider = FakeRepository::new();
        provider.files.insert("old.txt".to_owned(), b"old".to_vec());
        let mut ledger = empty_ledger();
        ledger
            .record_github_file(
                &RepositoryPath::new("old.txt").unwrap(),
                digest("old"),
                BASE_REVISION.to_owned(),
                OperationState::Confirmed,
            )
            .unwrap();

        execute(
            &fixture,
            &plan(vec![
                IntegrationOperation::WriteRepository(desired_file("index.html", b"index")),
                IntegrationOperation::WriteRepository(preview_a),
                IntegrationOperation::WriteRepository(preview_b),
                IntegrationOperation::WriteRepository(gallery),
                IntegrationOperation::DeleteRepository {
                    path: RepositoryPath::new("old.txt").unwrap(),
                },
            ]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        // One commit covered bundle members, publication files, and deletes.
        assert_eq!(provider.commit_count(), 1);
        assert_eq!(provider.files["public/photos/preview/a.jpg"], b"preview-a");
        assert_eq!(provider.files["public/photos/preview/b.jpg"], b"preview-b");
        assert_eq!(provider.files["gallery.json"], b"{\"photos\":[]}");
        assert!(!provider.files.contains_key("old.txt"));
        assert!(!ledger.github_inventory.contains_key("old.txt"));
    }

    #[test]
    fn publication_file_missing_at_execution_fails_locally_without_remote_calls() {
        let fixture = Fixture::new();
        let file = publication_file(&fixture, "a", b"preview-a");
        std::fs::remove_file(fixture.output.join("photos/preview/a.jpg")).unwrap();
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(file)]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::LocalValidation { .. }
        ));
        assert_eq!(provider.calls, Vec::new());
        assert!(ledger.github_inventory.is_empty());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn publication_file_size_or_hash_mismatch_fails_locally_without_remote_calls() {
        for wrong in ["size", "sha"] {
            let fixture = Fixture::new();
            let mut file = publication_file(&fixture, "a", b"preview-a");
            if wrong == "size" {
                file.size_bytes += 1;
            } else {
                file.sha256 = digest("different content");
            }
            let bundle = bundle(&[("index.html", b"index")]);
            let mut provider = FakeRepository::new();
            let mut ledger = empty_ledger();

            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::WriteRepository(file)]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            assert!(matches!(
                error,
                RepositoryExecutionError::LocalValidation { .. }
            ));
            assert_eq!(provider.calls, Vec::new());
            assert!(ledger.github_inventory.is_empty());
            assert!(!fixture.ledger_path.exists());
        }
    }

    #[test]
    fn non_regular_publication_file_fails_locally_without_remote_calls() {
        let fixture = Fixture::new();
        let file = publication_file(&fixture, "a", b"preview-a");
        let physical = fixture.output.join("photos/preview/a.jpg");
        std::fs::remove_file(&physical).unwrap();
        std::fs::create_dir_all(&physical).unwrap();
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(file)]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::LocalValidation { .. }
        ));
        assert_eq!(provider.calls, Vec::new());
        assert!(!fixture.ledger_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn publication_file_symlink_escape_fails_locally_without_remote_calls() {
        let fixture = Fixture::new();
        let file = publication_file(&fixture, "a", b"preview-a");
        let physical = fixture.output.join("photos/preview/a.jpg");
        let outside = fixture.root.path().join("outside.jpg");
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::remove_file(&physical).unwrap();
        std::os::unix::fs::symlink(&outside, &physical).unwrap();
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        let mut ledger = empty_ledger();

        let error = execute(
            &fixture,
            &plan(vec![IntegrationOperation::WriteRepository(file)]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RepositoryExecutionError::LocalValidation { .. }
        ));
        assert_eq!(provider.calls, Vec::new());
        assert!(!fixture.ledger_path.exists());
    }

    #[test]
    fn divergent_write_metadata_for_publication_file_rolls_back_without_commit() {
        for field in ["sha256", "size", "path"] {
            let fixture = Fixture::new();
            let file = publication_file(&fixture, "a", b"preview-a");
            let bundle = bundle(&[("index.html", b"index")]);
            let mut provider = FakeRepository::new();
            let mut divergent = FileMetadata {
                path: file.path.clone(),
                size_bytes: file.size_bytes,
                sha256: file.sha256.clone(),
            };
            match field {
                "sha256" => divergent.sha256 = digest("different"),
                "size" => divergent.size_bytes += 1,
                _ => divergent.path = RepositoryPath::new("other.txt").unwrap(),
            }
            provider.write_scripts.push_back(Ok(Some(divergent)));
            let mut ledger = empty_ledger();

            let error = execute(
                &fixture,
                &plan(vec![IntegrationOperation::WriteRepository(file)]),
                &bundle,
                &mut provider,
                &mut ledger,
            )
            .unwrap_err();

            // Divergent metadata: detected after the write; the batch is
            // never committed and the ledger rolls back (no commit happened).
            assert!(matches!(error, RepositoryExecutionError::Rejected(_)));
            assert_eq!(provider.commit_count(), 0);
            assert!(ledger.github_inventory.is_empty());
            assert_eq!(fixture.durable_ledger(), ledger);
        }
    }

    #[test]
    fn delete_of_a_publication_file_needs_no_source_content() {
        let fixture = Fixture::new();
        let path = RepositoryPath::new("public/photos/preview/a.jpg").unwrap();
        let mut ledger = empty_ledger();
        ledger
            .record_github_file(
                &path,
                digest("preview-a"),
                BASE_REVISION.to_owned(),
                OperationState::Confirmed,
            )
            .unwrap();
        ledger.write_to(&fixture.ledger_path).unwrap();
        let bundle = bundle(&[("index.html", b"index")]);
        let mut provider = FakeRepository::new();
        provider.files.insert(
            "public/photos/preview/a.jpg".to_owned(),
            b"preview-a".to_vec(),
        );

        execute(
            &fixture,
            &plan(vec![IntegrationOperation::DeleteRepository {
                path: path.clone(),
            }]),
            &bundle,
            &mut provider,
            &mut ledger,
        )
        .unwrap();

        assert!(!ledger
            .github_inventory
            .contains_key("public/photos/preview/a.jpg"));
        assert!(!provider.files.contains_key("public/photos/preview/a.jpg"));
    }
}
