//! Deterministic, provider-neutral reconciliation planning.
//!
//! This module only describes desired work. It never invokes providers or
//! performs remote I/O.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use photo_publisher_core::load_state;
use photo_publisher_pipeline::journal::{read_journal, sha256_file, JournalPhase};
use photo_publisher_provider_contracts::{copy_with_sha256, ObjectKey, RepositoryPath};
use serde_json::{json, Value};
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

/// A publication-relative asset path using `/` as the only separator.
///
/// The path is always interpreted relative to the publication root and must
/// live under `photos/`. Absolute paths, drive letters, UNC paths, URLs,
/// backslashes, `..` anywhere, and dot or empty components are rejected, so
/// the same value is portable between Windows and Linux.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicationPath(String);

impl PublicationPath {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let value = value.as_ref();
        RepositoryPath::new(value)
            .map_err(|error| anyhow::anyhow!("invalid publication path: {error}"))?;
        if value.contains("..") {
            bail!("publication path must not contain '..': {value}");
        }
        if !value.starts_with("photos/") {
            bail!("publication path must be under photos/: {value}");
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A storage object that the future executor must make present remotely.
///
/// `source_path` binds the remote key to the exact publication-relative JPEG
/// that supplies the bytes, so the executor never searches for, infers, or
/// reconstructs a local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredStorageObject {
    pub key: ObjectKey,
    pub source_path: PublicationPath,
    pub sha256: String,
    pub size_bytes: u64,
    pub content_type: Option<String>,
}

impl DesiredStorageObject {
    pub fn new(
        key: ObjectKey,
        source_path: PublicationPath,
        sha256: impl Into<String>,
        size_bytes: u64,
        content_type: Option<String>,
    ) -> Result<Self> {
        let object = Self {
            key,
            source_path,
            sha256: sha256.into(),
            size_bytes,
            content_type,
        };
        validate_sha256("desired storage object hash", &object.sha256)?;
        Ok(object)
    }
}

/// Where the bytes of a desired repository file come from when executed.
///
/// The plan stays fully self-describing: repository files either come from
/// the final application bundle (application/template files plus the public
/// `gallery.json`) or from a publication-relative physical file validated
/// against the committed output at planning time and revalidated against it
/// at execution time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryFileSource {
    /// Bytes come from the [`ApplicationBundle`] member with the same path.
    BundleMember,
    /// Bytes come from `output_dir/<source_path>`; the path is a validated
    /// publication-relative [`PublicationPath`] (never absolute, never a
    /// drive path or URL, never with `..` and always with `/` separators).
    PublicationFile { source_path: PublicationPath },
}

/// A repository file that the executor must make present remotely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRepositoryFile {
    pub path: RepositoryPath,
    pub sha256: String,
    pub size_bytes: u64,
    pub source: RepositoryFileSource,
}

impl DesiredRepositoryFile {
    pub fn new(
        path: RepositoryPath,
        sha256: impl Into<String>,
        size_bytes: u64,
        source: RepositoryFileSource,
    ) -> Result<Self> {
        let file = Self {
            path,
            sha256: sha256.into(),
            size_bytes,
            source,
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
                file.content().len() as u64,
                RepositoryFileSource::BundleMember,
            )?)?;
        }
        publication.validate()?;
        Ok(publication)
    }

    /// Builds the full desired state from a committed local publication.
    ///
    /// The journal must be `COMMITTED` and both the gallery manifest and the
    /// publisher state are authenticated against the committed journal before
    /// they are parsed. Every high-resolution download is bound to its
    /// publication-relative JPEG with verified SHA-256 and size, and every
    /// preview is bound to its publication-relative JPEG with a streamed
    /// SHA-256 (preview bytes are re-encoded by the pipeline, so the recorded
    /// hash is the hash of the file itself, while the file-name stem proves
    /// the source membership). This performs local reads only: no provider,
    /// network, or remote state is involved.
    pub fn from_committed_output(
        configuration: &ProjectPublicationConfig,
        output_dir: impl AsRef<Path>,
        bundle: &ApplicationBundle,
    ) -> Result<Self> {
        let output_dir = output_dir.as_ref();
        let local = LocalPublication::from_output(output_dir)?;
        let storage = committed_high_resolution_objects(configuration, output_dir, &local)?;
        let previews = committed_preview_files(configuration, output_dir, &local)?;
        let mut publication = Self::new(configuration, local, bundle)?;
        for object in storage {
            publication.insert_storage_object(object)?;
        }
        for file in previews {
            publication.insert_repository_file(file)?;
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

/// Builds the high-resolution storage bindings of one committed publication.
///
/// Scope of this phase: `gallery.photos[].download` → R2. The approved
/// `ObjectKey` rule is `<highResolution.prefix>/<download.url>` with exactly
/// one `/` separator; an absent or empty prefix means the URL alone. Each
/// bound file must remain inside `output_dir`, its SHA-256 is computed in
/// streaming and must match both the manifest file name and a
/// `PublisherState` source, and its size must match that source.
///
/// This function only reads local committed artifacts. It never contacts a
/// provider or reconstructs paths for a future executor.
fn committed_high_resolution_objects(
    configuration: &ProjectPublicationConfig,
    output_dir: &Path,
    local: &LocalPublication,
) -> Result<Vec<DesiredStorageObject>> {
    let journal = read_journal(&output_dir.join(".publisher/journal.json"))
        .context("committed journal is required to bind storage sources")?;
    if journal.phase != JournalPhase::Committed {
        bail!("local publication is not committed");
    }
    if journal.generation != local.generation
        || journal.gallery_sha256 != local.manifest_hash
        || journal.state_sha256 != local.state_hash
    {
        bail!("journal and local publication do not describe the same generation");
    }

    let gallery_path = output_dir.join(&journal.gallery_path);
    if sha256_file(&gallery_path)? != local.manifest_hash {
        bail!("gallery manifest does not match the committed local publication");
    }
    let gallery: Value = serde_json::from_slice(&fs::read(&gallery_path)?)
        .context("committed gallery manifest is not valid JSON")?;
    let photos = gallery
        .get("photos")
        .and_then(Value::as_array)
        .context("gallery photos must be an array")?;

    let state_path = output_dir.join(&journal.state_path);
    if sha256_file(&state_path)? != local.state_hash {
        bail!("publisher state does not match the committed local publication");
    }
    let state = load_state(&state_path).context("failed to load the committed publisher state")?;

    let prefix = configuration
        .high_resolution_prefix
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let canonical_root = output_dir
        .canonicalize()
        .context("failed to resolve the publication root")?;

    let mut objects = Vec::new();
    for photo in photos {
        let Some(download) = photo.get("download") else {
            continue;
        };
        let url = download
            .get("url")
            .and_then(Value::as_str)
            .context("gallery download asset URL is required")?;
        let source_path = PublicationPath::new(url)?;

        let filename = url
            .rsplit('/')
            .next()
            .context("download asset URL must contain a file name")?;
        let stem = filename
            .rsplit_once('.')
            .map(|(stem, _extension)| stem)
            .context("download asset has no file extension")?;
        validate_sha256("manifest download asset SHA-256", stem)?;

        let key = ObjectKey::new(if prefix.is_empty() {
            url.to_owned()
        } else {
            format!("{prefix}/{url}")
        })?;

        let physical = output_dir.join(source_path.as_str());
        let metadata = fs::metadata(&physical)
            .with_context(|| format!("gallery download asset does not exist: {url}"))?;
        if !metadata.is_file() {
            bail!("gallery download asset is not a regular file: {url}");
        }
        let canonical_asset = physical
            .canonicalize()
            .with_context(|| format!("failed to resolve gallery download asset: {url}"))?;
        if canonical_asset.strip_prefix(&canonical_root).is_err() {
            bail!("gallery download asset escapes the publication root: {url}");
        }

        let mut file = fs::File::open(&physical)
            .with_context(|| format!("failed to open gallery download asset: {url}"))?;
        let (bytes_read, sha256) = copy_with_sha256(&mut file, &mut io::sink())
            .with_context(|| format!("failed to hash gallery download asset: {url}"))?;
        if bytes_read != metadata.len() {
            bail!("gallery download asset changed while it was hashed: {url}");
        }
        if sha256 != stem {
            bail!("gallery download asset SHA-256 does not match its manifest path: {url}");
        }
        let source = state
            .photos
            .values()
            .find(|entry| entry.sha256 == sha256)
            .with_context(|| {
                format!("gallery download does not match any PublisherState source: {url}")
            })?;
        if metadata.len() != source.bytes {
            bail!("gallery download asset size does not match PublisherState: {url}");
        }

        objects.push(DesiredStorageObject::new(
            key,
            source_path,
            sha256,
            metadata.len(),
            Some("image/jpeg".to_owned()),
        )?);
    }
    Ok(objects)
}

/// Builds the preview bindings of one committed publication: each
/// `gallery.photos[].preview` asset becomes a desired **repository** file.
///
/// The repository path joins `preview.prefix` with the publication-relative
/// preview URL using the same single-slash rule as high-resolution object
/// keys; `publicBaseUrl` is never used to locate content. Previews are
/// re-encoded by the pipeline and named after the *source* SHA-256, so the
/// recorded desired hash is the streamed SHA-256 of the preview file itself
/// (the content that will be uploaded), while the stem must name a source
/// present in the committed `PublisherState` — proving the preview belongs to
/// this publication. The local manifest is never modified, and this performs
/// local reads only.
fn committed_preview_files(
    configuration: &ProjectPublicationConfig,
    output_dir: &Path,
    local: &LocalPublication,
) -> Result<Vec<DesiredRepositoryFile>> {
    let journal = read_journal(&output_dir.join(".publisher/journal.json"))
        .context("committed journal is required to bind preview sources")?;
    if journal.phase != JournalPhase::Committed {
        bail!("local publication is not committed");
    }
    if journal.generation != local.generation
        || journal.gallery_sha256 != local.manifest_hash
        || journal.state_sha256 != local.state_hash
    {
        bail!("journal and local publication do not describe the same generation");
    }

    let gallery_path = output_dir.join(&journal.gallery_path);
    if sha256_file(&gallery_path)? != local.manifest_hash {
        bail!("gallery manifest does not match the committed local publication");
    }
    let gallery: Value = serde_json::from_slice(&fs::read(&gallery_path)?)
        .context("committed gallery manifest is not valid JSON")?;
    let photos = gallery
        .get("photos")
        .and_then(Value::as_array)
        .context("gallery photos must be an array")?;

    let state_path = output_dir.join(&journal.state_path);
    if sha256_file(&state_path)? != local.state_hash {
        bail!("publisher state does not match the committed local publication");
    }
    let state = load_state(&state_path).context("failed to load the committed publisher state")?;

    let prefix = configuration
        .preview_prefix
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    let canonical_root = output_dir
        .canonicalize()
        .context("failed to resolve the publication root")?;

    let mut files = Vec::new();
    for photo in photos {
        let preview = photo
            .get("preview")
            .context("gallery preview asset is required")?;
        let url = preview
            .get("url")
            .and_then(Value::as_str)
            .context("gallery preview asset URL is required")?;
        let source_path = PublicationPath::new(url)?;

        let filename = url
            .rsplit('/')
            .next()
            .context("preview asset URL must contain a file name")?;
        let (stem, extension) = filename
            .rsplit_once('.')
            .context("preview asset has no file extension")?;
        if extension != "jpg" {
            bail!("preview asset is not a JPEG file: {url}");
        }
        // The pipeline names previews after the *source* photo hash; that
        // stem must name a source in the committed state.
        validate_sha256("manifest preview asset source SHA-256", stem)?;
        if !state.photos.values().any(|entry| entry.sha256 == stem) {
            bail!("gallery preview does not match any PublisherState source: {url}");
        }

        let path = RepositoryPath::new(if prefix.is_empty() {
            url.to_owned()
        } else {
            format!("{prefix}/{url}")
        })?;

        let physical = output_dir.join(source_path.as_str());
        let metadata = fs::metadata(&physical)
            .with_context(|| format!("gallery preview asset does not exist: {url}"))?;
        if !metadata.is_file() {
            bail!("gallery preview asset is not a regular file: {url}");
        }
        let canonical_asset = physical
            .canonicalize()
            .with_context(|| format!("failed to resolve gallery preview asset: {url}"))?;
        if canonical_asset.strip_prefix(&canonical_root).is_err() {
            bail!("gallery preview asset escapes the publication root: {url}");
        }

        let mut file = fs::File::open(&physical)
            .with_context(|| format!("failed to open gallery preview asset: {url}"))?;
        let (bytes_read, sha256) = copy_with_sha256(&mut file, &mut io::sink())
            .with_context(|| format!("failed to hash gallery preview asset: {url}"))?;
        if bytes_read != metadata.len() {
            bail!("gallery preview asset changed while it was hashed: {url}");
        }

        files.push(DesiredRepositoryFile::new(
            path,
            sha256,
            metadata.len(),
            RepositoryFileSource::PublicationFile {
                source_path: source_path.clone(),
            },
        )?);
    }
    Ok(files)
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
    use photo_publisher_core::{state_from, SourcePhoto};
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
                    PublicationPath::new("photos/download/a.jpg").unwrap(),
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

    const ASSET_BYTES: &[u8] = b"committed download bytes";

    fn download_url(sha: &str) -> String {
        format!("photos/download/{sha}.jpg")
    }

    fn bundle() -> ApplicationBundle {
        ApplicationBundle::from_files(vec![("index.html".to_owned(), b"index".to_vec())]).unwrap()
    }

    fn preview_url(sha: &str) -> String {
        format!("photos/preview/{sha}.jpg")
    }

    fn preview_bytes(sha: &str) -> Vec<u8> {
        format!("committed preview bytes {sha}").into_bytes()
    }

    fn write_asset(output: &Path, url: &str, bytes: &[u8]) {
        let path = output.join(url);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn gallery_manifest(urls: &[&str]) -> Value {
        json!({
            "schemaVersion": 1,
            "gallery": {"id": "gallery-local", "title": "Title"},
            "photos": urls
                .iter()
                .enumerate()
                .map(|(index, url)| {
                    let stem = url.rsplit('/').next().unwrap().rsplit_once('.').unwrap().0;
                    json!({
                        "id": format!("photo-{}", index + 1),
                        "filename": "a.jpg",
                        "sequence": index + 1,
                        "preview": {"url": format!("photos/preview/{stem}.jpg"), "width": 1, "height": 1},
                        "download": {"url": url, "width": 1, "height": 1}
                    })
                })
                .collect::<Vec<_>>()
        })
    }

    /// Writes a committed publication whose journal, manifest, and state agree
    /// about the recorded source (`state_sha`, `state_bytes`).
    fn commit(output: &Path, urls: &[&str], state_sha: &str, state_bytes: u64) {
        let state = state_from(&[SourcePhoto {
            relative_path: "a.jpg".to_owned(),
            bytes: state_bytes,
            sha256: state_sha.to_owned(),
        }]);
        let gallery_path = output.join("gallery.json");
        let state_path = output.join(".publisher/state.json");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(
            &gallery_path,
            serde_json::to_vec_pretty(&gallery_manifest(urls)).unwrap(),
        )
        .unwrap();
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        for url in urls {
            let stem = url.rsplit('/').next().unwrap().rsplit_once('.').unwrap().0;
            write_asset(output, &preview_url(stem), &preview_bytes(stem));
        }
        let journal = JournalRecord::new(
            1,
            "g-000001".to_owned(),
            None,
            JournalPhase::Committed,
            sha256_file(&gallery_path).unwrap(),
            sha256_file(&state_path).unwrap(),
            ".publisher/staging/g-000001".to_owned(),
            ".publisher/backups/none".to_owned(),
            None,
            None,
        );
        fs::write(
            output.join(".publisher/journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }

    /// Commits a multi-source publication: each `(sha, bytes)` becomes one
    /// photo whose download asset is a byte-exact copy of the source (like
    /// the pipeline) and whose preview is a distinct re-encoded byte string
    /// named after the source hash.
    fn commit_multi(output: &Path, sources: &[(&str, &[u8])], preview_stem: Option<&str>) {
        let photos: Vec<Value> = sources
            .iter()
            .enumerate()
            .map(|(index, (sha, _bytes))| {
                let stem = preview_stem.unwrap_or(sha);
                json!({
                    "id": format!("photo-{}", index + 1),
                    "filename": format!("{index}.jpg"),
                    "sequence": index + 1,
                    "preview": {"url": preview_url(stem), "width": 1, "height": 1},
                    "download": {"url": download_url(sha), "width": 1, "height": 1}
                })
            })
            .collect();
        let gallery = json!({
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
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&gallery_path, serde_json::to_vec_pretty(&gallery).unwrap()).unwrap();
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        for (sha, bytes) in sources {
            write_asset(output, &download_url(sha), bytes);
        }
        for (sha, _bytes) in sources {
            let stem = preview_stem.unwrap_or(sha);
            write_asset(output, &preview_url(stem), &preview_bytes(sha));
        }
        let journal = JournalRecord::new(
            1,
            "g-000001".to_owned(),
            None,
            JournalPhase::Committed,
            sha256_file(&gallery_path).unwrap(),
            sha256_file(&state_path).unwrap(),
            ".publisher/staging/g-000001".to_owned(),
            ".publisher/backups/none".to_owned(),
            None,
            None,
        );
        fs::write(
            output.join(".publisher/journal.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn committed_output_binds_each_download_to_its_publication_file() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let desired =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();
        let objects: Vec<_> = desired.storage_objects().collect();
        assert_eq!(objects.len(), 1);
        let object = objects[0];
        assert_eq!(object.source_path.as_str(), url);
        assert_eq!(object.key.as_str(), format!("originals/{url}"));
        assert_eq!(object.sha256, sha);
        assert_eq!(object.size_bytes, ASSET_BYTES.len() as u64);
        assert_eq!(object.content_type, Some("image/jpeg".to_owned()));
    }

    #[test]
    fn empty_prefix_uses_the_manifest_url_as_the_object_key() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let mut without_prefix = configuration();
        without_prefix.high_resolution_prefix = None;
        let desired =
            DesiredPublication::from_committed_output(&without_prefix, root.path(), &bundle())
                .unwrap();
        let object = desired.storage_objects().next().unwrap();
        assert_eq!(object.key.as_str(), url);
        assert!(!object.key.as_str().contains("//"));

        let mut trailing = configuration();
        trailing.high_resolution_prefix = Some("originals/".to_owned());
        let desired =
            DesiredPublication::from_committed_output(&trailing, root.path(), &bundle()).unwrap();
        let object = desired.storage_objects().next().unwrap();
        assert_eq!(object.key.as_str(), format!("originals/{url}"));
    }

    #[test]
    fn missing_download_file_fails_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn download_hash_mismatch_fails_the_binding() {
        let root = tempdir().unwrap();
        let declared = sha256_bytes(b"declared source");
        let url = download_url(&declared);
        let content = b"different bytes";
        commit(root.path(), &[&url], &declared, content.len() as u64);
        write_asset(root.path(), &url, content);

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn download_size_mismatch_fails_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64 + 1);
        write_asset(root.path(), &url, ASSET_BYTES);

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn download_absent_from_publisher_state_fails_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(
            root.path(),
            &[&url],
            &sha256_bytes(b"another source"),
            ASSET_BYTES.len() as u64,
        );
        write_asset(root.path(), &url, ASSET_BYTES);

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn publication_path_contract_rejects_absolute_urls_and_escapes() {
        for value in [
            "C:/Fotos/a.jpg",
            "C:\\Fotos\\a.jpg",
            "/photos/download/a.jpg",
            "https://example.com/a.jpg",
            "photos/../outside/a.jpg",
            "photos\\download\\a.jpg",
            "photos/download//a.jpg",
            "photos/../download/a.jpg",
            "outside/photos/download/a.jpg",
            "photos",
            "//server/share/a.jpg",
            "..",
            "photos/download/..jpg",
        ] {
            assert!(PublicationPath::new(value).is_err(), "accepted {value}");
        }
        for value in [
            "photos/download/abc123.jpg",
            "photos/download/1234567890abcdef.jpg",
        ] {
            assert!(PublicationPath::new(value).is_ok(), "rejected {value}");
        }
    }

    #[test]
    fn absolute_escaping_and_backslashed_manifest_urls_fail_the_binding() {
        for url in [
            "https://example.com/a.jpg",
            "C:/Fotos/a.jpg",
            "/photos/download/a.jpg",
            "photos/../outside/a.jpg",
            "photos\\download\\a.jpg",
            "photos/download//a.jpg",
        ] {
            let root = tempdir().unwrap();
            let sha = sha256_bytes(ASSET_BYTES);
            commit(root.path(), &[url], &sha, ASSET_BYTES.len() as u64);
            assert!(
                DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                    .is_err(),
                "accepted {url}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let output = root.path().join("output");
        let outside = root.path().join("outside.jpg");
        fs::write(&outside, ASSET_BYTES).unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(&output, &[&url], &sha, ASSET_BYTES.len() as u64);
        let asset = output.join(&url);
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        symlink(&outside, &asset).unwrap();

        assert!(
            DesiredPublication::from_committed_output(&configuration(), &output, &bundle())
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn symlink_escape_is_rejected_on_windows() {
        use std::os::windows::fs::symlink_file;

        let root = tempdir().unwrap();
        let output = root.path().join("output");
        let outside = root.path().join("outside.jpg");
        fs::write(&outside, ASSET_BYTES).unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(&output, &[&url], &sha, ASSET_BYTES.len() as u64);
        let asset = output.join(&url);
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        if symlink_file(&outside, &asset).is_err() {
            // Creating symbolic links requires a privilege that may be
            // disabled on this machine; the canonicalization guard itself is
            // unconditional in the builder and is exercised on unix.
            return;
        }

        assert!(
            DesiredPublication::from_committed_output(&configuration(), &output, &bundle())
                .is_err()
        );
    }

    #[test]
    fn duplicate_object_keys_fail_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(
            root.path(),
            &[url.as_str(), url.as_str()],
            &sha,
            ASSET_BYTES.len() as u64,
        );
        write_asset(root.path(), &url, ASSET_BYTES);

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn from_committed_output_is_deterministic() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let first =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();
        let second =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn confirmed_inventory_keeps_bound_objects_out_of_the_plan() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);
        let desired =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();

        let mut ledger = IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )
        .unwrap();
        for object in desired.storage_objects() {
            ledger
                .record_r2_object(
                    &object.key,
                    &StorageObject {
                        key: object.key.clone(),
                        size_bytes: object.size_bytes,
                        sha256: object.sha256.clone(),
                        content_type: object.content_type.clone(),
                    },
                    OperationState::Confirmed,
                )
                .unwrap();
        }
        let known = KnownRemoteState::from_ledger(&ledger).unwrap();
        let plan = plan_reconciliation(&desired, &known).unwrap();
        assert!(!plan
            .operations
            .iter()
            .any(|operation| matches!(operation, IntegrationOperation::PutStorage(_))));
    }

    #[test]
    fn planned_high_resolution_puts_use_image_jpeg() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);
        let desired =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();

        let plan = plan_reconciliation(&desired, &known(&desired)).unwrap();
        let puts: Vec<_> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::PutStorage(object) => Some(object),
                _ => None,
            })
            .collect();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].content_type, Some("image/jpeg".to_owned()));
        assert_eq!(puts[0].source_path.as_str(), url);
    }

    #[test]
    fn committed_output_binds_each_preview_to_its_repository_file() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let desired =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();
        let previews: Vec<_> = desired
            .repository_files()
            .filter(|file| file.path.as_str().starts_with("previews/"))
            .collect();
        assert_eq!(previews.len(), 1);
        // Repository path = preview.prefix + local preview URL.
        assert_eq!(
            previews[0].path.as_str(),
            format!("previews/{}", preview_url(&sha))
        );
        // The desired sha256 is the streamed hash of the preview bytes...
        assert_eq!(previews[0].sha256, sha256_bytes(&preview_bytes(&sha)));
        // ...deliberately not the source-hash stem: the pipeline re-encodes
        // previews, and the stem proves source membership instead.
        assert_ne!(previews[0].sha256, sha);
    }

    #[test]
    fn empty_preview_prefix_places_previews_at_their_publication_path() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let mut without_prefix = configuration();
        without_prefix.preview_prefix = None;
        let desired =
            DesiredPublication::from_committed_output(&without_prefix, root.path(), &bundle())
                .unwrap();
        let previews: Vec<_> = desired
            .repository_files()
            .filter(|file| !file.path.as_str().starts_with("index"))
            .collect();
        assert_eq!(previews.len(), 1);
        assert_eq!(previews[0].path.as_str(), preview_url(&sha));
    }

    #[test]
    fn unicode_preview_prefix_is_preserved_in_the_repository_path() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let mut unicode = configuration();
        unicode.preview_prefix = Some("álbum público".to_owned());
        let desired =
            DesiredPublication::from_committed_output(&unicode, root.path(), &bundle()).unwrap();
        let previews: Vec<_> = desired
            .repository_files()
            .filter(|file| file.path.as_str().starts_with('á'))
            .collect();
        assert_eq!(previews.len(), 1);
        assert_eq!(
            previews[0].path.as_str(),
            format!("álbum público/{}", preview_url(&sha))
        );
    }

    #[test]
    fn missing_preview_file_fails_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);
        fs::remove_file(root.path().join(preview_url(&sha))).unwrap();

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn non_regular_preview_file_fails_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);
        let preview = root.path().join(preview_url(&sha));
        fs::remove_file(&preview).unwrap();
        fs::create_dir_all(&preview).unwrap();

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[test]
    fn preview_from_an_unknown_source_fails_the_binding() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let foreign = sha256_bytes(b"foreign source");
        // The preview exists physically, but its stem names a source that is
        // not part of the committed publication.
        commit_multi(root.path(), &[(&sha, ASSET_BYTES)], Some(&foreign));

        assert!(DesiredPublication::from_committed_output(
            &configuration(),
            root.path(),
            &bundle()
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn preview_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let output = root.path().join("output");
        let outside = root.path().join("outside.jpg");
        fs::write(&outside, ASSET_BYTES).unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(&output, &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(&output, &url, ASSET_BYTES);
        let preview = output.join(preview_url(&sha));
        fs::remove_file(&preview).unwrap();
        symlink(&outside, &preview).unwrap();

        assert!(
            DesiredPublication::from_committed_output(&configuration(), &output, &bundle())
                .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn preview_symlink_escape_is_rejected_on_windows() {
        use std::os::windows::fs::symlink_file;

        let root = tempdir().unwrap();
        let output = root.path().join("output");
        let outside = root.path().join("outside.jpg");
        fs::write(&outside, ASSET_BYTES).unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(&output, &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(&output, &url, ASSET_BYTES);
        let preview = output.join(preview_url(&sha));
        fs::remove_file(&preview).unwrap();
        if symlink_file(&outside, &preview).is_err() {
            // Symlink creation requires a privilege that may be disabled on
            // this machine; the canonicalization guard itself is exercised on
            // unix above.
            return;
        }

        assert!(
            DesiredPublication::from_committed_output(&configuration(), &output, &bundle())
                .is_err()
        );
    }

    #[test]
    fn g1_to_g2_moves_previews_and_manifest_without_repeating_kept_content() {
        let template = bundle();
        let config = configuration();

        // G1: photos A and B fully published.
        let g1 = tempdir().unwrap();
        let sha_a = sha256_bytes(b"source-a");
        let sha_b = sha256_bytes(b"source-b");
        commit_multi(
            g1.path(),
            &[(&sha_a, b"source-a"), (&sha_b, b"source-b")],
            None,
        );
        let manifest1 =
            crate::PublicGalleryManifest::derive_from_committed_output(&config, g1.path()).unwrap();
        let bundle1 = crate::compose_application_bundle(&template, &manifest1).unwrap();
        let desired1 =
            DesiredPublication::from_committed_output(&config, g1.path(), &bundle1).unwrap();

        // The desired repository set: previews + gallery.json + template.
        let repo_paths: Vec<_> = desired1
            .repository_files()
            .map(|file| file.path.as_str().to_owned())
            .collect();
        assert!(repo_paths.contains(&format!("previews/{}", preview_url(&sha_a))));
        assert!(repo_paths.contains(&format!("previews/{}", preview_url(&sha_b))));
        assert!(repo_paths.contains(&"gallery.json".to_owned()));
        assert!(repo_paths.contains(&"index.html".to_owned()));

        // Ledger: everything from G1 is Confirmed.
        let mut ledger = IntegrationLedger::new(
            desired1.configuration_fingerprint.clone(),
            desired1.local.generation.clone(),
            desired1.local.state_hash.clone(),
            desired1.local.manifest_hash.clone(),
        )
        .unwrap();
        for object in desired1.storage_objects() {
            ledger
                .record_r2_object(
                    &object.key,
                    &StorageObject {
                        key: object.key.clone(),
                        size_bytes: object.size_bytes,
                        sha256: object.sha256.clone(),
                        content_type: object.content_type.clone(),
                    },
                    OperationState::Confirmed,
                )
                .unwrap();
        }
        for file in desired1.repository_files() {
            ledger
                .record_github_file(
                    &file.path,
                    file.sha256.clone(),
                    "commit-01".to_owned(),
                    OperationState::Confirmed,
                )
                .unwrap();
        }
        ledger
            .record_vercel(
                desired1.bundle_fingerprint.clone(),
                OperationState::Confirmed,
                Some("dpl-1".to_owned()),
                Some("g1.vercel.app".to_owned()),
            )
            .unwrap();
        let known = KnownRemoteState::from_ledger(&ledger).unwrap();

        // G1 already published: the plan is empty.
        assert!(plan_reconciliation(&desired1, &known).unwrap().is_empty());

        // G2: A stays, C arrives, B leaves.
        let g2 = tempdir().unwrap();
        let sha_c = sha256_bytes(b"source-c");
        commit_multi(
            g2.path(),
            &[(&sha_a, b"source-a"), (&sha_c, b"source-c")],
            None,
        );
        let manifest2 =
            crate::PublicGalleryManifest::derive_from_committed_output(&config, g2.path()).unwrap();
        let bundle2 = crate::compose_application_bundle(&template, &manifest2).unwrap();
        assert_ne!(manifest1, manifest2);
        assert_ne!(bundle1.fingerprint(), bundle2.fingerprint());
        let desired2 =
            DesiredPublication::from_committed_output(&config, g2.path(), &bundle2).unwrap();

        let plan = plan_reconciliation(&desired2, &known).unwrap();
        assert!(plan.reconciliation_requirements.is_empty());

        // Storage: C is uploaded, B is deleted, A is never touched.
        let puts: Vec<_> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::PutStorage(object) => Some(object.key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(puts, vec![format!("originals/{}", download_url(&sha_c))]);
        let deletes: Vec<_> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::DeleteStorage { key } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deletes, vec![format!("originals/{}", download_url(&sha_b))]);

        // Repository: preview C and the rewritten gallery.json are written;
        // preview B is deleted; preview A and the template are untouched.
        let writes: Vec<_> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::WriteRepository(file) => Some(file),
                _ => None,
            })
            .collect();
        let write_paths: Vec<_> = writes.iter().map(|file| file.path.as_str()).collect();
        assert!(write_paths.contains(&format!("previews/{}", preview_url(&sha_c)).as_str()));
        assert!(write_paths.contains(&"gallery.json"));
        assert!(!write_paths.contains(&format!("previews/{}", preview_url(&sha_a)).as_str()));
        assert!(!write_paths.contains(&"index.html"));
        let gallery_write = writes
            .iter()
            .find(|file| file.path.as_str() == "gallery.json")
            .unwrap();
        assert_eq!(gallery_write.sha256, manifest2.sha256());
        let repo_deletes: Vec<_> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::DeleteRepository { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            repo_deletes,
            vec![format!("previews/{}", preview_url(&sha_b))]
        );

        // Hosting: the fingerprint moved exactly because the manifest moved.
        let hosting: Vec<_> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                IntegrationOperation::PublishHosting { bundle_fingerprint } => {
                    Some(bundle_fingerprint.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(hosting, vec![bundle2.fingerprint().to_owned()]);

        // The public manifest of G2 references only A and C, never B.
        assert!(manifest2
            .as_str()
            .contains(&format!("photos/download/{sha_a}.jpg")));
        assert!(manifest2
            .as_str()
            .contains(&format!("photos/download/{sha_c}.jpg")));
        assert!(!manifest2.as_str().contains(&sha_b));
    }

    #[test]
    fn bundle_files_are_bundle_members_and_previews_are_publication_files() {
        let root = tempdir().unwrap();
        let sha = sha256_bytes(ASSET_BYTES);
        let url = download_url(&sha);
        commit(root.path(), &[&url], &sha, ASSET_BYTES.len() as u64);
        write_asset(root.path(), &url, ASSET_BYTES);

        let desired =
            DesiredPublication::from_committed_output(&configuration(), root.path(), &bundle())
                .unwrap();

        let index = desired
            .repository_files()
            .find(|file| file.path.as_str() == "index.html")
            .unwrap();
        assert_eq!(index.source, RepositoryFileSource::BundleMember);
        assert_eq!(index.size_bytes, b"index".len() as u64);

        let preview = desired
            .repository_files()
            .find(|file| file.path.as_str().starts_with("previews/"))
            .unwrap();
        assert_eq!(
            preview.source,
            RepositoryFileSource::PublicationFile {
                source_path: PublicationPath::new(preview_url(&sha)).unwrap(),
            }
        );
        assert_eq!(preview.size_bytes, preview_bytes(&sha).len() as u64);
    }
}
