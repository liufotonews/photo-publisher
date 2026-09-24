//! Provider-neutral contracts, durable state, reconciliation planning, and
//! storage, repository, and hosting execution for the integration layer.
//!
//! This crate deliberately contains no concrete provider implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use photo_publisher_contract_validator::{
    compile_embedded_schema, load_json, validate_value, EmbeddedSchema,
};
use photo_publisher_provider_contracts::{
    DeploymentInfo, ObjectKey, ProviderError, ProviderResult, RepositoryPath, StorageObject,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use url::{Host, Url};

pub mod coordinator;
pub mod executor;
pub mod hosting;
pub mod manifest;
pub mod planner;
pub mod repository;

pub(crate) mod file_source;

pub use coordinator::{CoordinationError, Coordinator, PublicationReport};
pub use executor::{StorageExecutionError, StorageExecutionReport, StorageExecutor};
pub use hosting::{HostingExecutionError, HostingExecutionReport, HostingExecutor};
pub use manifest::{
    compose_application_bundle, validate_public_gallery_manifest, PublicGalleryManifest,
};
pub use planner::{
    plan_reconciliation, publication_configuration_fingerprint, DesiredPublication,
    DesiredRepositoryFile, DesiredStorageObject, IntegrationOperation, IntegrationPlan,
    KnownRemoteState, LocalPublication, PublicationPath, ReconciliationRequirement,
    RepositoryFileSource,
};
pub use repository::{RepositoryExecutionError, RepositoryExecutionReport, RepositoryExecutor};

pub const INTEGRATION_LEDGER_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationBundleFile {
    path: RepositoryPath,
    content: Vec<u8>,
}

impl ApplicationBundleFile {
    pub fn path(&self) -> &RepositoryPath {
        &self.path
    }

    pub fn content(&self) -> &[u8] {
        &self.content
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationBundle {
    files: Vec<ApplicationBundleFile>,
    fingerprint: String,
}

impl ApplicationBundle {
    pub fn from_files(files: Vec<(String, Vec<u8>)>) -> ProviderResult<Self> {
        if files.is_empty() {
            return Err(ProviderError::InvalidPath(
                "application bundle must contain at least one file".to_owned(),
            ));
        }

        let mut paths = BTreeSet::new();
        let mut files = files
            .into_iter()
            .map(|(path, content)| {
                let path = RepositoryPath::new(path)?;
                if !paths.insert(path.as_str().to_owned()) {
                    return Err(ProviderError::InvalidPath(format!(
                        "duplicate application bundle path: {}",
                        path.as_str()
                    )));
                }
                Ok(ApplicationBundleFile { path, content })
            })
            .collect::<ProviderResult<Vec<_>>>()?;
        files.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));

        let fingerprint = bundle_fingerprint(&files);
        Ok(Self { files, fingerprint })
    }

    pub fn from_directory(path: impl AsRef<Path>) -> ProviderResult<Self> {
        let root = path.as_ref();
        if !root.is_dir() {
            return Err(ProviderError::NotFound);
        }
        let mut files = Vec::new();
        collect_bundle_files(root, root, &mut files)?;
        Self::from_files(files)
    }

    pub fn from_project_bundle(
        project_path: impl AsRef<Path>,
        bundle_path: &str,
    ) -> ProviderResult<Self> {
        let directory = resolve_bundle_directory(project_path.as_ref(), bundle_path)?;
        Self::from_directory(directory)
    }

    pub fn files(&self) -> &[ApplicationBundleFile] {
        &self.files
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn collect_bundle_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, Vec<u8>)>,
) -> ProviderResult<()> {
    for entry in fs::read_dir(directory).map_err(ProviderError::from_io)? {
        let entry = entry.map_err(ProviderError::from_io)?;
        let file_type = entry.file_type().map_err(ProviderError::from_io)?;
        if file_type.is_symlink() {
            return Err(ProviderError::InvalidPath(format!(
                "symbolic links are not allowed in application bundles: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            collect_bundle_files(root, &entry.path(), files)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(ProviderError::InvalidPath(format!(
                "application bundle entry is not a file: {}",
                entry.path().display()
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| ProviderError::Integrity)?
            .to_string_lossy()
            .replace('\\', "/");
        files.push((
            relative,
            fs::read(entry.path()).map_err(ProviderError::from_io)?,
        ));
    }
    Ok(())
}

fn bundle_fingerprint(files: &[ApplicationBundleFile]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"photo-publisher-application-bundle-v1\0");
    for file in files {
        let path = file.path.as_str().as_bytes();
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path);
        hasher.update((file.content.len() as u64).to_be_bytes());
        hasher.update(&file.content);
    }
    hex_digest(hasher.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostingPublicationConfig {
    pub project_id: String,
    pub team_id: Option<String>,
}

impl HostingPublicationConfig {
    pub fn new(project_id: impl Into<String>, team_id: Option<String>) -> ProviderResult<Self> {
        let project_id = project_id.into();
        if project_id.trim().is_empty()
            || team_id
                .as_deref()
                .is_some_and(|team_id| team_id.trim().is_empty())
        {
            return Err(ProviderError::Other);
        }
        Ok(Self {
            project_id,
            team_id,
        })
    }
}

/// Provider-neutral port implemented by the application's composition layer.
///
/// An adapter may translate this bundle into a provider-specific configuration
/// without exposing a concrete hosting provider here.
pub trait HostingPublisher {
    fn publish(
        &mut self,
        bundle: &ApplicationBundle,
        configuration: &HostingPublicationConfig,
    ) -> ProviderResult<DeploymentInfo>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicBaseUrl(String);

impl PublicBaseUrl {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_public_base_url(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub fn validate_public_base_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("public base URL is malformed")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("public base URL must be an absolute HTTP(S) URL without credentials, query, or fragment");
    }

    match url.host().expect("host was checked above") {
        Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            if domain == "localhost"
                || domain.ends_with(".localhost")
                || domain.ends_with(".local")
                || domain.ends_with(".internal")
            {
                bail!("public base URL must not use a local or internal host");
            }
        }
        Host::Ipv4(address) if is_private_ipv4(address.octets()) => {
            bail!("public base URL must not use a private or loopback address");
        }
        Host::Ipv4(_) => {}
        Host::Ipv6(address) if is_private_ipv6(address.segments()) => {
            bail!("public base URL must not use a private or loopback address");
        }
        Host::Ipv6(_) => {}
    }
    Ok(())
}

fn is_private_ipv4([first, second, ..]: [u8; 4]) -> bool {
    first == 0
        || first == 10
        || first == 127
        || (first == 100 && (64..=127).contains(&second))
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 168)
}

fn is_private_ipv6(segments: [u16; 8]) -> bool {
    let first = segments[0];
    segments == [0; 8]
        || segments == [0, 0, 0, 0, 0, 0, 0, 1]
        || first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPublicationConfig {
    pub project_id: String,
    pub bundle_directory: PathBuf,
    pub repository_provider: String,
    pub repository: String,
    pub branch: Option<String>,
    pub preview_prefix: Option<String>,
    pub preview_public_base_url: PublicBaseUrl,
    pub high_resolution_account_id: String,
    pub high_resolution_bucket: Option<String>,
    pub high_resolution_prefix: Option<String>,
    pub high_resolution_public_base_url: PublicBaseUrl,
    pub hosting: HostingPublicationConfig,
}

pub fn load_project_v2(path: impl AsRef<Path>) -> Result<ProjectPublicationConfig> {
    let path = path.as_ref();
    let validator = compile_embedded_schema(EmbeddedSchema::Project)?;
    let value = load_json(path)?;
    validate_value(&validator, &value)?;

    let document: ProjectDocument = serde_json::from_value(value)?;
    if document.schema_version != 2 {
        bail!("integrated publication requires project schemaVersion 2");
    }

    let bundle_directory = resolve_bundle_directory(path, &document.gallery.bundle_path)?;
    Ok(ProjectPublicationConfig {
        project_id: document.project.id,
        bundle_directory,
        repository_provider: document.repository.provider,
        repository: document.repository.repository,
        branch: document.repository.branch,
        preview_prefix: document.storage.preview.prefix,
        preview_public_base_url: PublicBaseUrl::parse(document.storage.preview.public_base_url)?,
        high_resolution_account_id: document.storage.high_resolution.account_id,
        high_resolution_bucket: document.storage.high_resolution.bucket,
        high_resolution_prefix: document.storage.high_resolution.prefix,
        high_resolution_public_base_url: PublicBaseUrl::parse(
            document.storage.high_resolution.public_base_url,
        )?,
        hosting: HostingPublicationConfig::new(document.hosting.project, document.hosting.team_id)?,
    })
}

fn resolve_bundle_directory(project_path: &Path, bundle_path: &str) -> ProviderResult<PathBuf> {
    let path = Path::new(bundle_path);
    if path.is_absolute() || bundle_path.contains(':') {
        return Err(ProviderError::InvalidPath(bundle_path.to_owned()));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(ProviderError::InvalidPath(bundle_path.to_owned()));
        }
    }
    let parent = project_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(path))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectDocument {
    schema_version: u32,
    project: ProjectIdentityDocument,
    gallery: GalleryDocument,
    repository: RepositoryDocument,
    hosting: HostingDocument,
    storage: StorageDocument,
}

#[derive(Debug, Deserialize)]
struct ProjectIdentityDocument {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GalleryDocument {
    bundle_path: String,
}

#[derive(Debug, Deserialize)]
struct RepositoryDocument {
    provider: String,
    repository: String,
    branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostingDocument {
    project: String,
    team_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageDocument {
    preview: PreviewStorageDocument,
    high_resolution: HighResolutionStorageDocument,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewStorageDocument {
    prefix: Option<String>,
    public_base_url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HighResolutionStorageDocument {
    account_id: String,
    bucket: Option<String>,
    prefix: Option<String>,
    public_base_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationState {
    Pending,
    Confirmed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct R2InventoryEntry {
    #[serde(rename = "sha256")]
    pub sha256: String,
    #[serde(rename = "sizeBytes")]
    pub size_bytes: u64,
    #[serde(rename = "contentType")]
    pub content_type: Option<String>,
    pub status: OperationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubInventoryEntry {
    #[serde(rename = "sha256")]
    pub sha256: String,
    pub revision: String,
    pub status: OperationState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VercelPublication {
    #[serde(rename = "bundleFingerprint")]
    pub bundle_fingerprint: String,
    pub status: OperationState,
    #[serde(rename = "deploymentId")]
    pub deployment_id: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationLedger {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "configurationFingerprint")]
    pub configuration_fingerprint: String,
    #[serde(rename = "localGeneration")]
    pub local_generation: String,
    #[serde(rename = "stateHash")]
    pub state_hash: String,
    #[serde(rename = "manifestHash")]
    pub manifest_hash: String,
    #[serde(rename = "r2Inventory")]
    pub r2_inventory: BTreeMap<String, R2InventoryEntry>,
    #[serde(rename = "githubInventory")]
    pub github_inventory: BTreeMap<String, GitHubInventoryEntry>,
    pub vercel: Option<VercelPublication>,
    /// SHA-256 of canonical JSON for every field above except `recordSha256`.
    /// This detects accidental corruption; it is not a keyed authentication tag.
    #[serde(rename = "recordSha256")]
    pub record_sha256: String,
}

impl IntegrationLedger {
    pub fn new(
        configuration_fingerprint: String,
        local_generation: String,
        state_hash: String,
        manifest_hash: String,
    ) -> Result<Self> {
        let mut ledger = Self {
            schema_version: INTEGRATION_LEDGER_SCHEMA_VERSION,
            configuration_fingerprint,
            local_generation,
            state_hash,
            manifest_hash,
            r2_inventory: BTreeMap::new(),
            github_inventory: BTreeMap::new(),
            vercel: None,
            record_sha256: String::new(),
        };
        ledger.refresh_integrity()?;
        Ok(ledger)
    }

    pub fn record_r2_object(
        &mut self,
        key: &ObjectKey,
        object: &StorageObject,
        status: OperationState,
    ) -> Result<()> {
        if object.key != *key {
            bail!("R2 inventory object key does not match its map key");
        }
        let mut next = self.clone();
        next.r2_inventory.insert(
            key.as_str().to_owned(),
            R2InventoryEntry {
                sha256: object.sha256.clone(),
                size_bytes: object.size_bytes,
                content_type: object.content_type.clone(),
                status,
            },
        );
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    /// Removes an R2 inventory entry after a confirmed remote deletion.
    ///
    /// The absence of a key means the object is no longer known to exist; no
    /// tombstone is kept. Removing a key that is not recorded is an error so
    /// callers cannot silently lose track of remote state.
    pub fn remove_r2_object(&mut self, key: &ObjectKey) -> Result<()> {
        if !self.r2_inventory.contains_key(key.as_str()) {
            bail!("R2 inventory does not contain key: {}", key.as_str());
        }
        let mut next = self.clone();
        next.r2_inventory.remove(key.as_str());
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    pub fn record_github_file(
        &mut self,
        path: &RepositoryPath,
        sha256: String,
        revision: String,
        status: OperationState,
    ) -> Result<()> {
        let mut next = self.clone();
        next.github_inventory.insert(
            path.as_str().to_owned(),
            GitHubInventoryEntry {
                sha256,
                revision,
                status,
            },
        );
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    /// Removes a GitHub inventory entry after a confirmed remote deletion.
    ///
    /// The absence of a path means the file is no longer known to exist; no
    /// tombstone is kept. Removing a path that is not recorded is an error so
    /// callers cannot silently lose track of remote state.
    pub fn remove_github_file(&mut self, path: &RepositoryPath) -> Result<()> {
        if !self.github_inventory.contains_key(path.as_str()) {
            bail!("GitHub inventory does not contain path: {}", path.as_str());
        }
        let mut next = self.clone();
        next.github_inventory.remove(path.as_str());
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    pub fn record_vercel(
        &mut self,
        bundle_fingerprint: String,
        status: OperationState,
        deployment_id: Option<String>,
        url: Option<String>,
    ) -> Result<()> {
        let mut next = self.clone();
        next.vercel = Some(VercelPublication {
            bundle_fingerprint,
            status,
            deployment_id,
            url,
        });
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    /// Removes the Vercel publication entry when a hosting operation was
    /// provably never published and no previous entry existed.
    ///
    /// The absence of the entry means no deployment is known; removing an
    /// absent entry is an error so callers cannot silently lose track of
    /// remote state.
    pub fn remove_vercel(&mut self) -> Result<()> {
        if self.vercel.is_none() {
            bail!("ledger does not contain a Vercel publication");
        }
        let mut next = self.clone();
        next.vercel = None;
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    /// Adopts the identity of a new safe publication in the ledger header.
    ///
    /// Only the header fields change; the R2/GitHub inventories and the
    /// Vercel entry are preserved exactly, because they describe remote
    /// reality by content and remain valid across generations. Integrity is
    /// recomputed, so the ledger remains writable and loadable.
    ///
    /// Callers must adopt a publication only when the ledger holds no
    /// `Pending`/`Unknown` entry of a previous, unfinished publication:
    /// adopting a header must never recontextualize an ambiguous state.
    pub fn adopt_publication(
        &mut self,
        configuration_fingerprint: String,
        local_generation: String,
        state_hash: String,
        manifest_hash: String,
    ) -> Result<()> {
        let mut next = self.clone();
        next.configuration_fingerprint = configuration_fingerprint;
        next.local_generation = local_generation;
        next.state_hash = state_hash;
        next.manifest_hash = manifest_hash;
        next.refresh_integrity()?;
        *self = next;
        Ok(())
    }

    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate()?;
        let path = path.as_ref();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temporary, self)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map(|_| ())
            .map_err(|error| anyhow::Error::from(error.error))
    }

    pub fn read_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let ledger: Self =
            serde_json::from_slice(&fs::read(path).with_context(|| {
                format!("failed to read integration ledger {}", path.display())
            })?)
            .with_context(|| format!("invalid integration ledger {}", path.display()))?;
        ledger.validate()?;
        Ok(ledger)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != INTEGRATION_LEDGER_SCHEMA_VERSION {
            bail!("unsupported integration ledger schema version");
        }
        for (name, hash) in [
            ("configurationFingerprint", &self.configuration_fingerprint),
            ("stateHash", &self.state_hash),
            ("manifestHash", &self.manifest_hash),
        ] {
            validate_sha256(name, hash)?;
        }
        if self.local_generation.trim().is_empty() {
            bail!("integration ledger localGeneration is required");
        }
        for (key, entry) in &self.r2_inventory {
            ObjectKey::new(key)?;
            validate_sha256("R2 object sha256", &entry.sha256)?;
        }
        for (path, entry) in &self.github_inventory {
            RepositoryPath::new(path)?;
            validate_sha256("GitHub file sha256", &entry.sha256)?;
            if entry.revision.trim().is_empty() {
                bail!("GitHub inventory revision is required");
            }
        }
        if let Some(vercel) = &self.vercel {
            validate_sha256("Vercel bundle fingerprint", &vercel.bundle_fingerprint)?;
            if vercel.status == OperationState::Confirmed
                && (vercel
                    .deployment_id
                    .as_deref()
                    .is_none_or(|id| id.trim().is_empty())
                    || vercel
                        .url
                        .as_deref()
                        .is_none_or(|url| url.trim().is_empty()))
            {
                bail!("confirmed Vercel publication requires deploymentId and url");
            }
        }
        if self.record_sha256 != self.compute_record_sha256()? {
            bail!("integration ledger recordSha256 does not match canonical content");
        }
        Ok(())
    }

    fn refresh_integrity(&mut self) -> Result<()> {
        self.record_sha256 = self.compute_record_sha256()?;
        self.validate()
    }

    fn compute_record_sha256(&self) -> Result<String> {
        let mut value = serde_json::to_value(self)?;
        value
            .as_object_mut()
            .context("integration ledger must serialize as an object")?
            .remove("recordSha256");
        Ok(hex_digest(Sha256::digest(serde_json::to_vec(&value)?)))
    }
}

fn validate_sha256(name: &str, value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{name} must be a SHA-256 hex digest");
    }
    Ok(())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn digest(value: &str) -> String {
        hex_digest(Sha256::digest(value.as_bytes()))
    }

    #[test]
    fn bundle_rejects_empty_and_unsafe_paths() {
        assert!(ApplicationBundle::from_files(vec![]).is_err());
        for path in [
            "/index.html",
            "../index.html",
            "a/../index.html",
            "C:/index.html",
        ] {
            assert!(ApplicationBundle::from_files(vec![(path.to_owned(), vec![])]).is_err());
        }
    }

    #[test]
    fn bundle_preserves_binary_and_unicode_with_stable_fingerprint() {
        let first = ApplicationBundle::from_files(vec![
            ("assets/ícone.bin".to_owned(), vec![0, 255, 1]),
            ("index.html".to_owned(), b"<h1>hello</h1>".to_vec()),
        ])
        .unwrap();
        let reordered = ApplicationBundle::from_files(vec![
            ("index.html".to_owned(), b"<h1>hello</h1>".to_vec()),
            ("assets/ícone.bin".to_owned(), vec![0, 255, 1]),
        ])
        .unwrap();
        assert_eq!(first, reordered);
        assert_eq!(first.files()[0].path().as_str(), "assets/ícone.bin");
        assert_eq!(first.files()[0].content(), &[0, 255, 1]);
        assert!(ApplicationBundle::from_files(vec![
            ("index.html".to_owned(), vec![]),
            ("index.html".to_owned(), vec![]),
        ])
        .is_err());
    }

    #[test]
    fn bundle_fingerprint_changes_for_content_and_paths() {
        let base = ApplicationBundle::from_files(vec![("index.html".to_owned(), b"one".to_vec())])
            .unwrap();
        let changed_content =
            ApplicationBundle::from_files(vec![("index.html".to_owned(), b"two".to_vec())])
                .unwrap();
        let changed_path =
            ApplicationBundle::from_files(vec![("app.html".to_owned(), b"one".to_vec())]).unwrap();
        assert_ne!(base.fingerprint(), changed_content.fingerprint());
        assert_ne!(base.fingerprint(), changed_path.fingerprint());
    }

    #[test]
    fn bundle_loads_a_relative_project_directory() {
        let root = tempdir().unwrap();
        let project = root.path().join("project.json");
        let bundle = root.path().join("site/assets");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("app.bin"), [0, 255]).unwrap();
        fs::write(root.path().join("site/index.html"), "ok").unwrap();
        let loaded = ApplicationBundle::from_project_bundle(&project, "site").unwrap();
        assert_eq!(loaded.files().len(), 2);
        assert!(ApplicationBundle::from_project_bundle(&project, "../site").is_err());
    }

    #[test]
    fn public_base_url_rejects_relative_and_internal_addresses() {
        assert!(PublicBaseUrl::parse("https://cdn.example.com/photos/").is_ok());
        for value in [
            "/photos",
            "localhost",
            "https://localhost/photos",
            "https://127.0.0.1/photos",
            "https://192.168.1.2/photos",
            "https://10.0.0.1/photos",
            "not a URL",
        ] {
            assert!(PublicBaseUrl::parse(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn project_v2_requires_valid_publication_configuration() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = load_project_v2(root.join("fixtures/valid/project.v2.valid.json")).unwrap();
        assert!(config.bundle_directory.ends_with("gallery-app"));
        assert_eq!(config.hosting.team_id.as_deref(), Some("team_example"));
        for fixture in [
            "project.v2.missing-bundle-path.json",
            "project.v2.absolute-bundle-path.json",
            "project.v2.relative-public-url.json",
            "project.v2.localhost-public-url.json",
            "project.v2.loopback-public-url.json",
            "project.v2.malformed-public-url.json",
            "project.v2.extra-property.json",
            "project.v2.missing-account-id.json",
            "project.v2.missing-bucket.json",
            "project.v2.missing-hosting-project.json",
        ] {
            assert!(load_project_v2(root.join("fixtures/invalid").join(fixture)).is_err());
        }
        assert!(load_project_v2(root.join("fixtures/valid/project.valid.json")).is_err());
    }

    #[test]
    fn ledger_round_trips_inventory_and_operation_states_atomically() {
        let mut ledger = IntegrationLedger::new(
            digest("config"),
            "g-000001".to_owned(),
            digest("state"),
            digest("manifest"),
        )
        .unwrap();
        let key = ObjectKey::new("originals/a.jpg").unwrap();
        let object = StorageObject {
            key: key.clone(),
            size_bytes: 3,
            sha256: digest("jpg"),
            content_type: Some("image/jpeg".to_owned()),
        };
        ledger
            .record_r2_object(&key, &object, OperationState::Pending)
            .unwrap();
        ledger
            .record_github_file(
                &RepositoryPath::new("public/photos/a.jpg").unwrap(),
                digest("preview"),
                "revision-1".to_owned(),
                OperationState::Confirmed,
            )
            .unwrap();
        ledger
            .record_vercel(digest("bundle"), OperationState::Unknown, None, None)
            .unwrap();
        let directory = tempdir().unwrap();
        let path = directory.path().join(".publisher/integration-state.json");
        ledger.write_to(&path).unwrap();
        assert_eq!(IntegrationLedger::read_from(&path).unwrap(), ledger);

        ledger
            .record_vercel(
                digest("bundle"),
                OperationState::Confirmed,
                Some("dpl_1".to_owned()),
                Some("deployment.vercel.app".to_owned()),
            )
            .unwrap();
        ledger.write_to(&path).unwrap();
        assert_eq!(IntegrationLedger::read_from(&path).unwrap(), ledger);
    }

    #[test]
    fn ledger_detects_corruption_unknown_versions_and_avoids_secrets() {
        let ledger = IntegrationLedger::new(
            digest("config"),
            "g-000001".to_owned(),
            digest("state"),
            digest("manifest"),
        )
        .unwrap();
        let directory = tempdir().unwrap();
        let path = directory.path().join("integration-state.json");
        ledger.write_to(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Authorization"));
        assert!(!text.contains("secret"));
        fs::write(&path, text.replacen("g-000001", "g-corrupt", 1)).unwrap();
        assert!(IntegrationLedger::read_from(&path).is_err());

        let mut value = serde_json::to_value(&ledger).unwrap();
        value["schemaVersion"] = serde_json::json!(999);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(IntegrationLedger::read_from(&path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn ledger_write_keeps_the_active_file_when_windows_denies_replacement() {
        use std::os::windows::fs::OpenOptionsExt;

        let original = IntegrationLedger::new(
            digest("config"),
            "g-000001".to_owned(),
            digest("state"),
            digest("manifest"),
        )
        .unwrap();
        let directory = tempdir().unwrap();
        let path = directory.path().join("integration-state.json");
        original.write_to(&path).unwrap();

        let mut updated = original.clone();
        updated
            .record_vercel(digest("bundle"), OperationState::Unknown, None, None)
            .unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(updated.write_to(&path).is_err());
        drop(locked);
        assert_eq!(IntegrationLedger::read_from(&path).unwrap(), original);

        updated.write_to(&path).unwrap();
        assert_eq!(IntegrationLedger::read_from(&path).unwrap(), updated);
    }

    #[test]
    fn hosting_publication_config_and_port_are_provider_neutral() {
        struct FakePublisher;
        impl HostingPublisher for FakePublisher {
            fn publish(
                &mut self,
                bundle: &ApplicationBundle,
                configuration: &HostingPublicationConfig,
            ) -> ProviderResult<DeploymentInfo> {
                assert_eq!(bundle.files().len(), 1);
                assert_eq!(configuration.project_id, "project");
                Ok(DeploymentInfo {
                    id: "deployment".to_owned(),
                    url: "deployment.example.com".to_owned(),
                })
            }
        }

        let bundle =
            ApplicationBundle::from_files(vec![("index.html".to_owned(), vec![])]).unwrap();
        let config = HostingPublicationConfig::new("project", None).unwrap();
        let deployment = FakePublisher.publish(&bundle, &config).unwrap();
        assert_eq!(deployment.id, "deployment");
        assert!(HostingPublicationConfig::new("", None).is_err());
    }

    #[test]
    fn confirmed_vercel_publication_requires_deployment_identity() {
        let mut ledger = IntegrationLedger::new(
            digest("config"),
            "g-000001".to_owned(),
            digest("state"),
            digest("manifest"),
        )
        .unwrap();
        assert!(ledger
            .record_vercel(digest("bundle"), OperationState::Confirmed, None, None)
            .is_err());
        assert!(ledger.validate().is_ok());
    }

    #[test]
    fn ledger_stays_schema_v1_without_storage_source_binding() {
        let key = ObjectKey::new("originals/photos/download/a.jpg").unwrap();
        let mut ledger = IntegrationLedger::new(
            digest("config"),
            "g-000001".to_owned(),
            digest("state"),
            digest("manifest"),
        )
        .unwrap();
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

        let text = serde_json::to_string(&ledger).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schemaVersion"], 1);
        assert!(!text.contains("source_path"));
        assert!(!text.contains("sourcePath"));
        assert!(!text.contains("PublicationPath"));
        assert_eq!(
            value["r2Inventory"]["originals/photos/download/a.jpg"]["sha256"],
            digest("jpg")
        );
    }
}
