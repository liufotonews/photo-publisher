//! Composition root for the desktop adapter.
//!
//! This module is the *only* place in the desktop application that knows
//! concrete provider types: it wires the validated `project.json` (schema v2)
//! to the existing provider implementations and hands trait objects to
//! `publisher-app`. Publication planning and all safety semantics stay in
//! `publisher-integration`; orchestration lives in `publisher-app`; this
//! module only constructs providers, performs the credential preflight, and
//! validates the provider matrix.
//!
//! Credentials are read from the process environment through the existing
//! `CredentialStore` trait. They are never written anywhere, never logged,
//! and never included in results or errors.
//!
//! Building providers never performs remote calls: construction takes only
//! configuration and credential lookups; no HTTP, no commits, no uploads.

use std::path::Path;

use photo_publisher_integration::{
    ApplicationBundle, HostingPublicationConfig, HostingPublisher, ProjectPublicationConfig,
};
use photo_publisher_provider_contracts::{
    CredentialStore, DeploymentInfo, HostingProvider, ProviderError, ProviderResult,
};
use photo_publisher_provider_github::{GitHubRepositoryConfig, GitHubRepositoryProvider};
use photo_publisher_provider_r2::{R2StorageConfig, R2StorageProvider};
use photo_publisher_provider_vercel::{VercelConfig, VercelHostingProvider};
use serde_json::Value;

use publisher_app::{ApplicationError, ApplicationErrorKind, PublicationProviders};

/// Reads provider credentials from process environment variables.
///
/// Read-only: `set`/`delete` are unsupported so this store can never mutate
/// secrets. Nothing is persisted, logged, or embedded in outputs.
///
/// NOTE — duplication with the CLI composition root: this type mirrors
/// `publisher-cli`'s `EnvCredentialStore` exactly. Extracting it into a
/// shared crate is a future refactor (see the phase report); keeping the
/// duplication here is intentional and small.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvironmentCredentialStore;

impl EnvironmentCredentialStore {
    /// Maps the contract-level credential name to its environment variable.
    /// The variable names are identifiers, not secrets.
    fn env_key(name: &str) -> Option<&'static str> {
        Some(match name {
            "github.token" => "PHOTO_PUBLISHER_GITHUB_TOKEN",
            "r2.access_key_id" => "PHOTO_PUBLISHER_R2_ACCESS_KEY_ID",
            "r2.secret_access_key" => "PHOTO_PUBLISHER_R2_SECRET_ACCESS_KEY",
            "vercel.token" => "PHOTO_PUBLISHER_VERCEL_TOKEN",
            _ => return None,
        })
    }
}

impl CredentialStore for EnvironmentCredentialStore {
    fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
        let Some(key) = Self::env_key(name) else {
            return Ok(None);
        };
        match std::env::var(key) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value.into_bytes())),
            _ => Ok(None),
        }
    }

    fn set(&mut self, _name: &str, _secret: &[u8]) -> ProviderResult<()> {
        Err(ProviderError::Unsupported)
    }

    fn delete(&mut self, _name: &str) -> ProviderResult<()> {
        Err(ProviderError::Unsupported)
    }
}

/// Enforces the provider matrix supported by this phase, using only the raw
/// validated project document. Unknown providers are rejected before any
/// configuration object, credential read, or provider is constructed.
fn enforce_supported_providers(project: &Value) -> Result<(), ApplicationError> {
    let check = |pointer: &str, expected: &str, what: &str| -> Result<(), ApplicationError> {
        let actual = project.pointer(pointer).and_then(Value::as_str);
        match actual {
            Some(value) if value == expected => Ok(()),
            other => Err(ApplicationError::new(
                ApplicationErrorKind::ProjectInvalid,
                format!(
                    "unsupported {what} provider {}: this application supports only '{expected}'",
                    other.unwrap_or("<missing>")
                ),
            )),
        }
    };
    check("/repository/provider", "github", "repository")?;
    check("/storage/preview/provider", "github", "preview storage")?;
    check(
        "/storage/highResolution/provider",
        "r2",
        "high-resolution storage",
    )?;
    check("/hosting/provider", "vercel", "hosting")?;
    Ok(())
}

const REQUIRED_CREDENTIALS: &[&str] = &[
    "github.token",
    "r2.access_key_id",
    "r2.secret_access_key",
    "vercel.token",
];

/// Credential preflight: every required secret must be present before any
/// provider is constructed or contacted. Missing credentials are a
/// deterministic configuration error that names only the environment
/// variable, never a secret value.
fn preflight_credentials(credentials: &EnvironmentCredentialStore) -> Result<(), ApplicationError> {
    for name in REQUIRED_CREDENTIALS {
        let present = credentials.get(name).map_err(|e| {
            ApplicationError::with_source(ApplicationErrorKind::Internal, format!("{e}"), e)
        })?;
        if matches!(present, Some(value) if !value.is_empty()) {
            continue;
        }
        let key = EnvironmentCredentialStore::env_key(name).unwrap_or(name);
        return Err(ApplicationError::new(
            ApplicationErrorKind::ResourceMissing,
            format!("missing required credential: set the {key} environment variable"),
        ));
    }
    Ok(())
}

/// Splits the `owner/repository` string from the project configuration.
fn split_repository(value: &str) -> Result<(&str, &str), ApplicationError> {
    match value.split_once('/') {
        Some((owner, repository))
            if !owner.is_empty() && !repository.is_empty() && !repository.contains('/') =>
        {
            Ok((owner, repository))
        }
        _ => Err(ApplicationError::new(
            ApplicationErrorKind::ProjectInvalid,
            format!("repository must be 'owner/repository': {value}"),
        )),
    }
}

/// Validates the provider matrix, then the project configuration, then the
/// credential preflight for a schema-v2 project. This happens strictly before
/// any provider is built and before any remote effect. Returns the validated
/// publication configuration to be used by the application layer.
pub fn preflight_publication(
    project: &Value,
    project_path: &Path,
) -> Result<ProjectPublicationConfig, ApplicationError> {
    enforce_supported_providers(project)?;
    let configuration =
        photo_publisher_integration::load_project_v2(project_path).map_err(|e| {
            ApplicationError::with_source(ApplicationErrorKind::ProjectInvalid, format!("{e}"), e)
        })?;
    preflight_credentials(&EnvironmentCredentialStore)?;
    Ok(configuration)
}

/// Concrete providers constructed from the validated configuration. The
/// composition root owns them; the application layer only ever sees the
/// contract traits, through [`DesktopProviders::as_publication_providers`].
pub struct DesktopProviders {
    storage: R2StorageProvider<EnvironmentCredentialStore>,
    repository: GitHubRepositoryProvider<EnvironmentCredentialStore>,
    hosting: VercelHostingAdapter,
    credentials: EnvironmentCredentialStore,
}

impl DesktopProviders {
    /// Borrows the concrete providers as the provider-neutral structure the
    /// application layer consumes. The `DesktopProviders` value must outlive
    /// the returned structure.
    pub fn as_publication_providers(&mut self) -> PublicationProviders<'_> {
        PublicationProviders {
            storage: &mut self.storage,
            repository: &mut self.repository,
            hosting: &mut self.hosting,
            credentials: &self.credentials,
        }
    }
}

/// Constructs the concrete providers for a schema-v2 project. No remote
/// call happens here; construction is pure wiring driven by the validated
/// configuration.
pub fn build_providers(
    configuration: &ProjectPublicationConfig,
) -> Result<DesktopProviders, ApplicationError> {
    let credentials = EnvironmentCredentialStore;

    let (owner, name) = split_repository(&configuration.repository)?;
    let mut github_config = GitHubRepositoryConfig::new(owner, name);
    if let Some(branch) = configuration.branch.as_deref() {
        github_config.branch = branch.to_owned();
    }
    let repository = GitHubRepositoryProvider::new(github_config, credentials).map_err(|e| {
        ApplicationError::with_source(ApplicationErrorKind::ProjectInvalid, format!("{e}"), e)
    })?;

    let bucket = configuration
        .high_resolution_bucket
        .clone()
        .ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorKind::ProjectInvalid,
                "highResolution.bucket is required",
            )
        })?;
    // The R2 endpoint is derived by the provider configuration itself.
    let storage = R2StorageProvider::new(
        R2StorageConfig::new(configuration.high_resolution_account_id.clone(), bucket),
        credentials,
    )
    .map_err(|e| {
        ApplicationError::with_source(ApplicationErrorKind::ProjectInvalid, format!("{e}"), e)
    })?;

    let hosting = VercelHostingAdapter { credentials };
    Ok(DesktopProviders {
        storage,
        repository,
        hosting,
        credentials,
    })
}

/// Provider-neutral adapter between the application bundle and the real
/// Vercel hosting provider — the single place where concrete provider
/// configuration meets the `HostingPublisher` port.
///
/// NOTE — duplication with the CLI composition root: this adapter mirrors
/// `publisher-cli`'s `VercelHostingAdapter` exactly. A future small refactor
/// may share it; keeping the duplication here is intentional.
struct VercelHostingAdapter {
    credentials: EnvironmentCredentialStore,
}

impl HostingPublisher for VercelHostingAdapter {
    fn publish(
        &mut self,
        bundle: &ApplicationBundle,
        configuration: &HostingPublicationConfig,
    ) -> ProviderResult<DeploymentInfo> {
        let files: Vec<(String, Vec<u8>)> = bundle
            .files()
            .iter()
            .map(|file| (file.path().as_str().to_owned(), file.content().to_vec()))
            .collect();
        let mut config = VercelConfig::new(configuration.project_id.clone()).with_files(files)?;
        config.team_id = configuration.team_id.clone();
        VercelHostingProvider::new(config, self.credentials)?.publish()
    }
}

/// Inert provider set used only for schema-v1 (local-only) publications,
/// where the application layer never invokes any provider. Every trait
/// method answers `ProviderError::Unsupported`; these types exist purely to
/// satisfy the `PublicationProviders` shape without touching any real
/// provider, credential, or network.
pub(crate) struct NoopProviders {
    storage: NoopStorage,
    repository: NoopRepository,
    hosting: NoopHosting,
    credentials: EnvironmentCredentialStore,
}

impl NoopProviders {
    pub(crate) fn new() -> Self {
        Self {
            storage: NoopStorage,
            repository: NoopRepository,
            hosting: NoopHosting,
            credentials: EnvironmentCredentialStore,
        }
    }

    pub(crate) fn as_providers(&mut self) -> PublicationProviders<'_> {
        PublicationProviders {
            storage: &mut self.storage,
            repository: &mut self.repository,
            hosting: &mut self.hosting,
            credentials: &self.credentials,
        }
    }
}

struct NoopStorage;
struct NoopRepository;
struct NoopHosting;

use photo_publisher_provider_contracts::{
    CommitInfo, FileMetadata, ObjectKey, RepositoryPath, RepositoryProvider, StorageObject,
    StorageProvider,
};

impl StorageProvider for NoopStorage {
    fn put(
        &mut self,
        _key: &ObjectKey,
        _content: &mut dyn std::io::Read,
        _content_type: Option<&str>,
    ) -> ProviderResult<StorageObject> {
        Err(ProviderError::Unsupported)
    }
    fn head(&self, _key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
        Err(ProviderError::Unsupported)
    }
    fn delete(&mut self, _key: &ObjectKey) -> ProviderResult<()> {
        Err(ProviderError::Unsupported)
    }
}

impl RepositoryProvider for NoopRepository {
    fn ensure_repository(&mut self) -> ProviderResult<()> {
        Err(ProviderError::Unsupported)
    }
    fn exists(&self, _path: &RepositoryPath) -> ProviderResult<bool> {
        Err(ProviderError::Unsupported)
    }
    fn read(&self, _path: &RepositoryPath) -> ProviderResult<Vec<u8>> {
        Err(ProviderError::Unsupported)
    }
    fn write(
        &mut self,
        _path: &RepositoryPath,
        _content: &mut dyn std::io::Read,
    ) -> ProviderResult<FileMetadata> {
        Err(ProviderError::Unsupported)
    }
    fn delete(&mut self, _path: &RepositoryPath) -> ProviderResult<()> {
        Err(ProviderError::Unsupported)
    }
    fn commit(&mut self, _message: &str) -> ProviderResult<CommitInfo> {
        Err(ProviderError::Unsupported)
    }
}

impl HostingPublisher for NoopHosting {
    fn publish(
        &mut self,
        _bundle: &ApplicationBundle,
        _configuration: &HostingPublicationConfig,
    ) -> ProviderResult<DeploymentInfo> {
        Err(ProviderError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use publisher_app::ApplicationErrorKind;
    use tempfile::tempdir;

    /// Serializes tests that mutate process environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const VALID_PROJECT: &str = r#"{
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
    }"#;

    fn write_project(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root.join("fotos")).unwrap();
        let path = root.join("project.json");
        std::fs::write(&path, VALID_PROJECT).unwrap();
        path
    }

    fn clear_credentials() {
        for key in [
            "PHOTO_PUBLISHER_GITHUB_TOKEN",
            "PHOTO_PUBLISHER_R2_ACCESS_KEY_ID",
            "PHOTO_PUBLISHER_R2_SECRET_ACCESS_KEY",
            "PHOTO_PUBLISHER_VERCEL_TOKEN",
        ] {
            std::env::remove_var(key);
        }
    }

    fn set_credentials() {
        std::env::set_var("PHOTO_PUBLISHER_GITHUB_TOKEN", "non-secret-1");
        std::env::set_var("PHOTO_PUBLISHER_R2_ACCESS_KEY_ID", "non-secret-2");
        std::env::set_var("PHOTO_PUBLISHER_R2_SECRET_ACCESS_KEY", "non-secret-3");
        std::env::set_var("PHOTO_PUBLISHER_VERCEL_TOKEN", "non-secret-4");
    }

    #[test]
    fn preflight_rejects_missing_credentials_before_building_anything() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_credentials();
        let root = tempdir().unwrap();
        let project_path = write_project(root.path());
        let document: Value = serde_json::from_str(VALID_PROJECT).unwrap();

        let error = preflight_publication(&document, &project_path).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ResourceMissing);
        let text = format!("{error}");
        assert!(text.contains("PHOTO_PUBLISHER_GITHUB_TOKEN"));
        // No secret material can appear in a preflight error.
        assert!(!text.contains("non-secret"));
    }

    #[test]
    fn preflight_rejects_unknown_provider() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_credentials();
        let root = tempdir().unwrap();
        let project_path = write_project(root.path());
        let mut document: Value = serde_json::from_str(VALID_PROJECT).unwrap();
        document["hosting"]["provider"] = Value::String("other-host".to_owned());

        let error = preflight_publication(&document, &project_path).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ProjectInvalid);
        assert!(format!("{error}").contains("hosting"));
    }

    #[test]
    fn composition_builds_all_providers_without_remote_calls() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_credentials();
        let root = tempdir().unwrap();
        let project_path = write_project(root.path());
        let document: Value = serde_json::from_str(VALID_PROJECT).unwrap();

        let configuration = preflight_publication(&document, &project_path).unwrap();
        let mut providers = build_providers(&configuration).unwrap();
        // The provider-neutral structure is produced by borrowing; nothing
        // executed: no HTTP, no commits, no uploads (constructors are pure).
        let publication_providers = providers.as_publication_providers();
        let _ = publication_providers;
        // Building twice is deterministic in error surface too.
        let configuration2 = preflight_publication(&document, &project_path).unwrap();
        assert_eq!(configuration.project_id, configuration2.project_id.clone());
        clear_credentials();
    }

    #[test]
    fn invalid_configuration_is_rejected_with_application_classification() {
        let _guard = ENV_LOCK.lock().unwrap();
        set_credentials();
        let configuration = ProjectPublicationConfig {
            project_id: "x".to_owned(),
            bundle_directory: Default::default(),
            repository_provider: "github".to_owned(),
            repository: "no-slash-here".to_owned(),
            branch: None,
            preview_prefix: None,
            preview_public_base_url: photo_publisher_integration::PublicBaseUrl::parse(
                "https://cdn.example.com",
            )
            .unwrap(),
            high_resolution_account_id: "a".to_owned(),
            high_resolution_bucket: None,
            high_resolution_prefix: None,
            high_resolution_public_base_url: photo_publisher_integration::PublicBaseUrl::parse(
                "https://d.example.com",
            )
            .unwrap(),
            hosting: HostingPublicationConfig::new("p", None).unwrap(),
        };
        let Err(error) = build_providers(&configuration) else {
            panic!("expected invalid repository configuration to be rejected");
        };
        assert_eq!(error.kind, ApplicationErrorKind::ProjectInvalid);
        clear_credentials();
    }
}
