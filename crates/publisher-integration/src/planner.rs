//! Deterministic, provider-neutral reconciliation planning.
//!
//! This module only describes desired work. It never invokes providers or
//! performs remote I/O.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};
use photo_publisher_pipeline::journal::{read_journal, sha256_file, JournalPhase};
use photo_publisher_provider_contracts::{ObjectKey, RepositoryPath};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    ApplicationBundle, GitHubInventoryEntry, IntegrationLedger, OperationState,
    ProjectPublicationConfig, R2InventoryEntry, VercelPublication,
};

/// Verified local publication metadata produced by the existing pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPublication {
    pub generation: String,
    pub state_hash: String,
    pub manifest_hash: String,
}

impl LocalPublication {
    pub fn new(
        generation: impl Into<String>,
        state_hash: impl Into<String>,
        manifest_hash: impl Into<String>,
    ) -> Result<Self> {
        let publication = Self {
            generation: generation.into(),
            state_hash: state_hash.into(),
            manifest_hash: manifest_hash.into(),
        };
        publication.validate()?;
        Ok(publication)
    }

    /// Reads only local, committed pipeline artifacts and verifies their hashes
    /// against the pipeline journal. It does not run recovery or publish files.
    pub fn from_output(output_dir: impl AsRef<Path>) -> Result<Self> {
        let output_dir = output_dir.as_ref();
        let journal = read_journal(&output_dir.join(".publisher/journal.json"))?;
        if journal.phase != JournalPhase::Committed {
            bail!("local publication is not committed");
        }

        let manifest_hash = sha256_file(&output_dir.join(&journal.gallery_path))?;
        let state_hash = sha256_file(&output_dir.join(&journal.state_path))?;
        if manifest_hash != journal.gallery_sha256 || state_hash != journal.state_sha256 {
            bail!("local publication hashes do not match the committed journal");
        }
        Self::new(journal.generation, state_hash, manifest_hash)
    }

    fn validate(&self) -> Result<()> {
        if self.generation.trim().is_empty() {
            bail!("local publication generation is required");
        }
        validate_sha256("local state hash", &self.state_hash)?;
        validate_sha256("local manifest hash", &self.manifest_hash)
    }
}

/// A storage object that the future executor must make present remotely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredStorageObject {
    pub key: ObjectKey,
    pub sha256: String,
    pub size_bytes: u64,
    pub content_type: Option<String>,
}

impl DesiredStorageObject {
    pub fn new(
        key: ObjectKey,
        sha256: impl Into<String>,
        size_bytes: u64,
        content_type: Option<String>,
    ) -> Result<Self> {
        let object = Self {
            key,
            sha256: sha256.into(),
            size_bytes,
            content_type,
        };
        validate_sha256("desired storage object hash", &object.sha256)?;
        Ok(object)
    }
}

/// A repository file that the future executor must make present remotely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRepositoryFile {
    pub path: RepositoryPath,
    pub sha256: String,
}

impl DesiredRepositoryFile {
    pub fn new(path: RepositoryPath, sha256: impl Into<String>) -> Result<Self> {
        let file = Self {
            path,
            sha256: sha256.into(),
        };
        validate_sha256("desired repository file hash", &file.sha256)?;
        Ok(file)
    }
}

/// Full provider-neutral desired state for a single local publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredPublication {
    pub configuration_fingerprint: String,
    pub local: LocalPublication,
    pub bundle_fingerprint: String,
    storage: BTreeMap<String, DesiredStorageObject>,
    repository: BTreeMap<String, DesiredRepositoryFile>,
}

impl DesiredPublication {
    /// Initializes desired repository files from the application bundle.
    /// The final gallery manifest is intentionally not added here: its public
    /// URLs are assembled by a later phase, not inferred from local paths.
    pub fn new(
        configuration: &ProjectPublicationConfig,
        local: LocalPublication,
        bundle: &ApplicationBundle,
    ) -> Result<Self> {
        let mut publication = Self {
            configuration_fingerprint: publication_configuration_fingerprint(configuration),
            local,
            bundle_fingerprint: bundle.fingerprint().to_owned(),
            storage: BTreeMap::new(),
            repository: BTreeMap::new(),
        };
        for file in bundle.files() {
            publication.insert_repository_file(DesiredRepositoryFile::new(
                file.path().clone(),
                sha256_bytes(file.content()),
            )?)?;
        }
        publication.validate()?;
        Ok(publication)
    }

    pub fn insert_storage_object(&mut self, object: DesiredStorageObject) -> Result<()> {
        let key = object.key.as_str().to_owned();
        if self.storage.contains_key(&key) {
            bail!("duplicate desired storage key: {key}");
        }
        self.storage.insert(key, object);
        Ok(())
    }

    pub fn insert_repository_file(&mut self, file: DesiredRepositoryFile) -> Result<()> {
        let path = file.path.as_str().to_owned();
        if self.repository.contains_key(&path) {
            bail!("duplicate desired repository path: {path}");
        }
        self.repository.insert(path, file);
        Ok(())
    }

    pub fn storage_objects(&self) -> impl Iterator<Item = &DesiredStorageObject> {
        self.storage.values()
    }

    pub fn repository_files(&self) -> impl Iterator<Item = &DesiredRepositoryFile> {
        self.repository.values()
    }

    fn validate(&self) -> Result<()> {
        validate_sha256(
            "desired configuration fingerprint",
            &self.configuration_fingerprint,
        )?;
        validate_sha256("desired bundle fingerprint", &self.bundle_fingerprint)?;
        self.local.validate()
    }
}

/// Deterministic fingerprint of all non-secret publication configuration.
pub fn publication_configuration_fingerprint(configuration: &ProjectPublicationConfig) -> String {
    let value = json!({
        "projectId": configuration.project_id,
        "repository": {
            "provider": configuration.repository_provider,
            "repository": configuration.repository,
            "branch": configuration.branch,
        },
        "preview": {
            "prefix": configuration.preview_prefix,
            "publicBaseUrl": configuration.preview_public_base_url.as_str(),
        },
        "highResolution": {
            "accountId": configuration.high_resolution_account_id,
            "bucket": configuration.high_resolution_bucket,
            "prefix": configuration.high_resolution_prefix,
            "publicBaseUrl": configuration.high_resolution_public_base_url.as_str(),
        },
        "hosting": {
            "projectId": configuration.hosting.project_id,
            "teamId": configuration.hosting.team_id,
        },
    });
    sha256_bytes(&serde_json::to_vec(&value).expect("publication configuration is serializable"))
}

/// Last state confirmed, pending, or unknown by previous remote publication work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownRemoteState {
    pub configuration_fingerprint: String,
    storage: BTreeMap<String, R2InventoryEntry>,
    repository: BTreeMap<String, GitHubInventoryEntry>,
    pub hosting: Option<VercelPublication>,
}

impl KnownRemoteState {
    pub fn from_ledger(ledger: &IntegrationLedger) -> Result<Self> {
        ledger.validate()?;
        Ok(Self {
            configuration_fingerprint: ledger.configuration_fingerprint.clone(),
            storage: ledger.r2_inventory.clone(),
            repository: ledger.github_inventory.clone(),
            hosting: ledger.vercel.clone(),
        })
    }

    pub fn storage_objects(&self) -> impl Iterator<Item = (&str, &R2InventoryEntry)> {
        self.storage
            .iter()
            .map(|(key, entry)| (key.as_str(), entry))
    }

    pub fn repository_files(&self) -> impl Iterator<Item = (&str, &GitHubInventoryEntry)> {
        self.repository
            .iter()
            .map(|(path, entry)| (path.as_str(), entry))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationOperation {
    PutStorage(DesiredStorageObject),
    DeleteStorage { key: ObjectKey },
    WriteRepository(DesiredRepositoryFile),
    DeleteRepository { path: RepositoryPath },
    PublishHosting { bundle_fingerprint: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciliationRequirement {
    ConfigurationChanged,
    StoragePending { key: ObjectKey },
    StorageUnknown { key: ObjectKey },
    RepositoryPending { path: RepositoryPath },
    RepositoryUnknown { path: RepositoryPath },
    HostingPending { bundle_fingerprint: String },
    HostingUnknown { bundle_fingerprint: String },
}

/// An ordered, side-effect-free plan. Repository operations are deliberately
/// individual entries so a later executor can batch all writes/deletes into a
/// single RepositoryProvider commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationPlan {
    pub operations: Vec<IntegrationOperation>,
    pub reconciliation_requirements: Vec<ReconciliationRequirement>,
}

impl IntegrationPlan {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty() && self.reconciliation_requirements.is_empty()
    }
}

/// Computes an idempotent plan without provider calls or remote I/O.
pub fn plan_reconciliation(
    desired: &DesiredPublication,
    known: &KnownRemoteState,
) -> Result<IntegrationPlan> {
    desired.validate()?;
    if desired.configuration_fingerprint != known.configuration_fingerprint {
        return Ok(IntegrationPlan {
            operations: Vec::new(),
            reconciliation_requirements: vec![ReconciliationRequirement::ConfigurationChanged],
        });
    }

    let mut operations = Vec::new();
    let mut requirements = Vec::new();

    plan_storage(desired, known, &mut operations, &mut requirements)?;
    plan_repository(desired, known, &mut operations, &mut requirements)?;
    plan_hosting(desired, known, &mut operations, &mut requirements);

    Ok(IntegrationPlan {
        operations,
        reconciliation_requirements: requirements,
    })
}

fn plan_storage(
    desired: &DesiredPublication,
    known: &KnownRemoteState,
    operations: &mut Vec<IntegrationOperation>,
    requirements: &mut Vec<ReconciliationRequirement>,
) -> Result<()> {
    for object in desired.storage_objects() {
        match known.storage.get(object.key.as_str()) {
            None => operations.push(IntegrationOperation::PutStorage(object.clone())),
            Some(entry)
                if entry.status == OperationState::Confirmed && entry.sha256 == object.sha256 => {}
            Some(entry) if entry.status == OperationState::Confirmed => {
                operations.push(IntegrationOperation::PutStorage(object.clone()))
            }
            Some(entry) if entry.status == OperationState::Pending => {
                requirements.push(ReconciliationRequirement::StoragePending {
                    key: object.key.clone(),
                })
            }
            Some(_) => requirements.push(ReconciliationRequirement::StorageUnknown {
                key: object.key.clone(),
            }),
        }
    }
    for (key, entry) in known.storage_objects() {
        if desired.storage.contains_key(key) {
            continue;
        }
        let key = ObjectKey::new(key)?;
        match entry.status {
            OperationState::Confirmed => {
                operations.push(IntegrationOperation::DeleteStorage { key })
            }
            OperationState::Pending => {
                requirements.push(ReconciliationRequirement::StoragePending { key })
            }
            OperationState::Unknown => {
                requirements.push(ReconciliationRequirement::StorageUnknown { key })
            }
        }
    }
    Ok(())
}

fn plan_repository(
    desired: &DesiredPublication,
    known: &KnownRemoteState,
    operations: &mut Vec<IntegrationOperation>,
    requirements: &mut Vec<ReconciliationRequirement>,
) -> Result<()> {
    for file in desired.repository_files() {
        match known.repository.get(file.path.as_str()) {
            None => operations.push(IntegrationOperation::WriteRepository(file.clone())),
            Some(entry)
                if entry.status == OperationState::Confirmed && entry.sha256 == file.sha256 => {}
            Some(entry) if entry.status == OperationState::Confirmed => {
                operations.push(IntegrationOperation::WriteRepository(file.clone()))
            }
            Some(entry) if entry.status == OperationState::Pending => {
                requirements.push(ReconciliationRequirement::RepositoryPending {
                    path: file.path.clone(),
                })
            }
            Some(_) => requirements.push(ReconciliationRequirement::RepositoryUnknown {
                path: file.path.clone(),
            }),
        }
    }
    for (path, entry) in known.repository_files() {
        if desired.repository.contains_key(path) {
            continue;
        }
        let path = RepositoryPath::new(path)?;
        match entry.status {
            OperationState::Confirmed => {
                operations.push(IntegrationOperation::DeleteRepository { path })
            }
            OperationState::Pending => {
                requirements.push(ReconciliationRequirement::RepositoryPending { path })
            }
            OperationState::Unknown => {
                requirements.push(ReconciliationRequirement::RepositoryUnknown { path })
            }
        }
    }
    Ok(())
}

fn plan_hosting(
    desired: &DesiredPublication,
    known: &KnownRemoteState,
    operations: &mut Vec<IntegrationOperation>,
    requirements: &mut Vec<ReconciliationRequirement>,
) {
    match &known.hosting {
        None => operations.push(IntegrationOperation::PublishHosting {
            bundle_fingerprint: desired.bundle_fingerprint.clone(),
        }),
        Some(hosting)
            if hosting.status == OperationState::Confirmed
                && hosting.bundle_fingerprint == desired.bundle_fingerprint => {}
        Some(hosting) if hosting.status == OperationState::Confirmed => {
            operations.push(IntegrationOperation::PublishHosting {
                bundle_fingerprint: desired.bundle_fingerprint.clone(),
            })
        }
        Some(hosting) if hosting.status == OperationState::Pending => {
            requirements.push(ReconciliationRequirement::HostingPending {
                bundle_fingerprint: hosting.bundle_fingerprint.clone(),
            })
        }
        Some(hosting) => requirements.push(ReconciliationRequirement::HostingUnknown {
            bundle_fingerprint: hosting.bundle_fingerprint.clone(),
        }),
    }
}

fn validate_sha256(name: &str, value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{name} must be a SHA-256 hex digest");
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ApplicationBundle, HostingPublicationConfig, IntegrationLedger, PublicBaseUrl,
        StorageObject,
    };
    use photo_publisher_pipeline::journal::JournalRecord;
    use tempfile::tempdir;

    fn digest(value: &str) -> String {
        sha256_bytes(value.as_bytes())
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

    fn desired() -> DesiredPublication {
        let configuration = configuration();
        let bundle = ApplicationBundle::from_files(vec![
            ("assets/app.js".to_owned(), b"app".to_vec()),
            ("index.html".to_owned(), b"index".to_vec()),
        ])
        .unwrap();
        let mut desired = DesiredPublication::new(
            &configuration,
            LocalPublication::new("g-000001", digest("state"), digest("manifest")).unwrap(),
            &bundle,
        )
        .unwrap();
        desired
            .insert_storage_object(
                DesiredStorageObject::new(
                    ObjectKey::new("originals/a.jpg").unwrap(),
                    digest("a"),
                    1,
                    Some("image/jpeg".to_owned()),
                )
                .unwrap(),
            )
            .unwrap();
        desired
    }

    fn known(desired: &DesiredPublication) -> KnownRemoteState {
        KnownRemoteState {
            configuration_fingerprint: desired.configuration_fingerprint.clone(),
            storage: BTreeMap::new(),
            repository: BTreeMap::new(),
            hosting: None,
        }
    }

    #[test]
    fn absent_remote_resources_produce_put_write_and_publish() {
        let desired = desired();
        let plan = plan_reconciliation(&desired, &known(&desired)).unwrap();
        assert_eq!(plan.reconciliation_requirements, Vec::new());
        assert!(matches!(
            plan.operations[0],
            IntegrationOperation::PutStorage(_)
        ));
        assert!(matches!(
            plan.operations[1],
            IntegrationOperation::WriteRepository(_)
        ));
        assert!(matches!(
            plan.operations[2],
            IntegrationOperation::WriteRepository(_)
        ));
        assert!(matches!(
            plan.operations[3],
            IntegrationOperation::PublishHosting { .. }
        ));
    }

    #[test]
    fn confirmed_matching_resources_are_skipped() {
        let desired = desired();
        let mut known = known(&desired);
        known.storage.insert(
            "originals/a.jpg".to_owned(),
            R2InventoryEntry {
                sha256: digest("a"),
                size_bytes: 1,
                content_type: Some("image/jpeg".to_owned()),
                status: OperationState::Confirmed,
            },
        );
        for file in desired.repository_files() {
            known.repository.insert(
                file.path.as_str().to_owned(),
                GitHubInventoryEntry {
                    sha256: file.sha256.clone(),
                    revision: "revision".to_owned(),
                    status: OperationState::Confirmed,
                },
            );
        }
        known.hosting = Some(VercelPublication {
            bundle_fingerprint: desired.bundle_fingerprint.clone(),
            status: OperationState::Confirmed,
            deployment_id: Some("deployment".to_owned()),
            url: Some("deployment.example.com".to_owned()),
        });
        assert!(plan_reconciliation(&desired, &known).unwrap().is_empty());
    }

    #[test]
    fn changed_or_extra_resources_produce_explicit_updates_and_deletes() {
        let desired = desired();
        let mut known = known(&desired);
        known.storage.insert(
            "originals/a.jpg".to_owned(),
            R2InventoryEntry {
                sha256: digest("old"),
                size_bytes: 3,
                content_type: Some("image/jpeg".to_owned()),
                status: OperationState::Confirmed,
            },
        );
        known.storage.insert(
            "originals/removed.jpg".to_owned(),
            R2InventoryEntry {
                sha256: digest("removed"),
                size_bytes: 7,
                content_type: Some("image/jpeg".to_owned()),
                status: OperationState::Confirmed,
            },
        );
        let first = desired.repository_files().next().unwrap();
        known.repository.insert(
            first.path.as_str().to_owned(),
            GitHubInventoryEntry {
                sha256: digest("old"),
                revision: "revision".to_owned(),
                status: OperationState::Confirmed,
            },
        );
        known.repository.insert(
            "assets/removed.js".to_owned(),
            GitHubInventoryEntry {
                sha256: digest("removed"),
                revision: "revision".to_owned(),
                status: OperationState::Confirmed,
            },
        );
        known.hosting = Some(VercelPublication {
            bundle_fingerprint: digest("old-bundle"),
            status: OperationState::Confirmed,
            deployment_id: Some("old".to_owned()),
            url: Some("old.example.com".to_owned()),
        });
        let plan = plan_reconciliation(&desired, &known).unwrap();
        assert!(plan
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PutStorage(_))));
        assert!(plan.operations.iter().any(|operation| matches!(operation, IntegrationOperation::DeleteStorage { key } if key.as_str() == "originals/removed.jpg")));
        assert!(plan.operations.iter().any(|operation| matches!(operation, IntegrationOperation::WriteRepository(file) if file.path == first.path)));
        assert!(plan.operations.iter().any(|operation| matches!(operation, IntegrationOperation::DeleteRepository { path } if path.as_str() == "assets/removed.js")));
        assert!(matches!(
            plan.operations.last(),
            Some(IntegrationOperation::PublishHosting { .. })
        ));
    }

    #[test]
    fn pending_and_unknown_entries_require_reconciliation_without_retrying() {
        let desired = desired();
        let mut known = known(&desired);
        known.storage.insert(
            "originals/a.jpg".to_owned(),
            R2InventoryEntry {
                sha256: digest("a"),
                size_bytes: 1,
                content_type: Some("image/jpeg".to_owned()),
                status: OperationState::Pending,
            },
        );
        let first = desired.repository_files().next().unwrap();
        known.repository.insert(
            first.path.as_str().to_owned(),
            GitHubInventoryEntry {
                sha256: first.sha256.clone(),
                revision: "revision".to_owned(),
                status: OperationState::Pending,
            },
        );
        known.hosting = Some(VercelPublication {
            bundle_fingerprint: desired.bundle_fingerprint.clone(),
            status: OperationState::Unknown,
            deployment_id: None,
            url: None,
        });
        let plan = plan_reconciliation(&desired, &known).unwrap();
        assert!(plan
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::StoragePending { .. }
            )));
        assert!(plan
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::RepositoryPending { .. }
            )));
        assert!(plan
            .reconciliation_requirements
            .iter()
            .any(|requirement| matches!(
                requirement,
                ReconciliationRequirement::HostingUnknown { .. }
            )));
        assert!(!plan
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PublishHosting { .. })));
        assert!(!plan.operations.iter().any(|operation| matches!(operation, IntegrationOperation::PutStorage(object) if object.key.as_str() == "originals/a.jpg")));
    }

    #[test]
    fn planner_is_idempotent_and_ordered() {
        let desired = desired();
        let known = known(&desired);
        let first = plan_reconciliation(&desired, &known).unwrap();
        let second = plan_reconciliation(&desired, &known).unwrap();
        assert_eq!(first, second);
        let paths: Vec<_> = first
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::WriteRepository(file) => Some(file.path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, vec!["assets/app.js", "index.html"]);
    }

    #[test]
    fn changed_configuration_blocks_reconciliation() {
        let desired = desired();
        let mut known = known(&desired);
        known.configuration_fingerprint = digest("other configuration");
        let plan = plan_reconciliation(&desired, &known).unwrap();
        assert!(plan.operations.is_empty());
        assert_eq!(
            plan.reconciliation_requirements,
            vec![ReconciliationRequirement::ConfigurationChanged]
        );
    }

    #[test]
    fn local_publication_reads_committed_pipeline_artifacts() {
        let output = tempdir().unwrap();
        let gallery = output.path().join("gallery.json");
        let state = output.path().join(".publisher/state.json");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        std::fs::write(&gallery, b"manifest").unwrap();
        std::fs::write(&state, b"state").unwrap();
        let journal = JournalRecord::new(
            1,
            "g-000001".to_owned(),
            None,
            JournalPhase::Committed,
            digest("manifest"),
            digest("state"),
            ".publisher/staging/g-000001".to_owned(),
            ".publisher/backups/none".to_owned(),
            None,
            None,
        );
        std::fs::write(
            output.path().join(".publisher/journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();

        assert_eq!(
            LocalPublication::from_output(output.path()).unwrap(),
            LocalPublication::new("g-000001", digest("state"), digest("manifest")).unwrap()
        );
    }

    #[test]
    fn known_remote_state_is_a_validated_projection_of_the_ledger() {
        let desired = desired();
        let mut ledger = IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )
        .unwrap();
        let key = ObjectKey::new("originals/a.jpg").unwrap();
        ledger
            .record_r2_object(
                &key,
                &StorageObject {
                    key: key.clone(),
                    size_bytes: 1,
                    sha256: digest("a"),
                    content_type: Some("image/jpeg".to_owned()),
                },
                OperationState::Confirmed,
            )
            .unwrap();
        ledger
            .record_github_file(
                &RepositoryPath::new("index.html").unwrap(),
                digest("index"),
                "revision".to_owned(),
                OperationState::Confirmed,
            )
            .unwrap();
        ledger
            .record_vercel(
                desired.bundle_fingerprint.clone(),
                OperationState::Unknown,
                None,
                None,
            )
            .unwrap();

        let known = KnownRemoteState::from_ledger(&ledger).unwrap();
        assert_eq!(known.storage_objects().count(), 1);
        assert_eq!(known.repository_files().count(), 1);
        assert_eq!(known.hosting.unwrap().status, OperationState::Unknown);
    }
}
