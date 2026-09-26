//! Composition root for the integrated publication.
//!
//! This module is the *only* place in the CLI that knows concrete provider
//! types: it wires the validated `project.json` (schema v2) to the existing
//! provider implementations and hands trait objects to the application
//! layer. Publication planning and all safety semantics stay in
//! `publisher-integration`; orchestration lives in `publisher-app`; this
//! module only constructs providers, performs the credential preflight, and
//! validates the provider matrix.
//!
//! Credentials are read from the process environment through the existing
//! `CredentialStore` trait. They are never written anywhere, never logged,
//! and never included in results or errors.

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

use crate::{CliError, CliResult, ErrorKind};

/// Reads provider credentials from process environment variables.
///
/// Read-only: `set`/`delete` are unsupported so this store can never mutate
/// secrets. Nothing is persisted, logged, or embedded in outputs.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EnvCredentialStore;

impl EnvCredentialStore {
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

impl CredentialStore for EnvCredentialStore {
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

/// Returns the environment-backed credential store used by this CLI's
/// composition root.
pub(crate) fn credential_store() -> EnvCredentialStore {
    EnvCredentialStore
}

/// Enforces the provider matrix supported by this phase, using only the raw
/// validated project document. Unknown providers are rejected before any
/// configuration object, credential read, or provider is constructed.
fn enforce_supported_providers(project: &Value) -> CliResult<()> {
    let check = |pointer: &str, expected: &str, what: &str| -> CliResult<()> {
        let actual = project.pointer(pointer).and_then(Value::as_str);
        match actual {
            Some(value) if value == expected => Ok(()),
            other => Err(CliError::new(
                ErrorKind::ProjectInvalid,
                format!(
                    "unsupported {what} provider {:?}: this CLI supports only '{expected}'",
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
fn preflight_credentials(credentials: &EnvCredentialStore) -> CliResult<()> {
    for name in REQUIRED_CREDENTIALS {
        let present = credentials
            .get(name)
            .map_err(|e| CliError::from_any(ErrorKind::Internal, e))?;
        if matches!(present, Some(value) if !value.is_empty()) {
            continue;
        }
        let key = EnvCredentialStore::env_key(name).unwrap_or(name);
        return Err(CliError::new(
            ErrorKind::ResourceMissing,
            format!("missing required credential: set the {key} environment variable"),
        ));
    }
    Ok(())
}

/// Splits the `owner/repository` string from the project configuration.
fn split_repository(value: &str) -> CliResult<(&str, &str)> {
    match value.split_once('/') {
        Some((owner, repository))
            if !owner.is_empty() && !repository.is_empty() && !repository.contains('/') =>
        {
            Ok((owner, repository))
        }
        _ => Err(CliError::new(
            ErrorKind::ProjectInvalid,
            format!("repository must be 'owner/repository': {value}"),
        )),
    }
}

/// Validates provider matrix, configuration, and credentials for a schema-v2
/// project — strictly before the local publication build and before any
/// remote effect.
pub(crate) fn preflight(
    project: &Value,
    project_path: &Path,
) -> CliResult<ProjectPublicationConfig> {
    enforce_supported_providers(project)?;
    let configuration = photo_publisher_integration::load_project_v2(project_path)
        .map_err(|e| CliError::from_any(ErrorKind::ProjectInvalid, e))?;
    preflight_credentials(&EnvCredentialStore)?;
    Ok(configuration)
}

/// Concrete providers constructed from the validated configuration. The
/// fields are consumed by the application layer exclusively through the
/// contract traits.
pub(crate) struct WireProviders {
    pub storage: R2StorageProvider<EnvCredentialStore>,
    pub repository: GitHubRepositoryProvider<EnvCredentialStore>,
    pub hosting: VercelHostingAdapter,
}

/// Constructs the concrete providers for a schema-v2 project. No remote
/// call happens here; construction is pure wiring.
pub(crate) fn compose_providers(
    configuration: &ProjectPublicationConfig,
) -> CliResult<WireProviders> {
    let credentials = EnvCredentialStore;

    let (owner, name) = split_repository(&configuration.repository)?;
    let mut github_config = GitHubRepositoryConfig::new(owner, name);
    if let Some(branch) = configuration.branch.as_deref() {
        github_config.branch = branch.to_owned();
    }
    let repository = GitHubRepositoryProvider::new(github_config, credentials)
        .map_err(|e| CliError::from_any(ErrorKind::ProjectInvalid, e))?;

    let bucket = configuration
        .high_resolution_bucket
        .clone()
        .ok_or_else(|| {
            CliError::new(
                ErrorKind::ProjectInvalid,
                "highResolution.bucket is required",
            )
        })?;
    // The R2 endpoint is derived by the provider configuration itself.
    let storage = R2StorageProvider::new(
        R2StorageConfig::new(configuration.high_resolution_account_id.clone(), bucket),
        credentials,
    )
    .map_err(|e| CliError::from_any(ErrorKind::ProjectInvalid, e))?;

    let hosting = VercelHostingAdapter { credentials };
    Ok(WireProviders {
        storage,
        repository,
        hosting,
    })
}

/// Provider-neutral adapter between the application bundle and the real
/// Vercel hosting provider — the single place where concrete provider
/// configuration meets the `HostingPublisher` port.
pub(crate) struct VercelHostingAdapter {
    credentials: EnvCredentialStore,
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const VALID_PROJECT: &str = r#"{
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

    fn patch_pointer(value: &mut Value, pointer: &str, replacement: &str) {
        let segments: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
        let mut current = value;
        for segment in &segments[..segments.len() - 1] {
            current = current.get_mut(*segment).unwrap();
        }
        *current.get_mut(segments[segments.len() - 1]).unwrap() = Value::String(replacement.into());
    }

    #[test]
    fn unknown_providers_are_rejected_before_anything_is_built() {
        for pointer in [
            "/repository/provider",
            "/storage/preview/provider",
            "/storage/highResolution/provider",
            "/hosting/provider",
        ] {
            let mut value: Value = serde_json::from_str(VALID_PROJECT).unwrap();
            patch_pointer(&mut value, pointer, "something-else");
            assert!(enforce_supported_providers(&value).is_err(), "{pointer}");
        }
    }

    #[test]
    fn valid_provider_matrix_is_accepted() {
        let value: Value = serde_json::from_str(VALID_PROJECT).unwrap();
        enforce_supported_providers(&value).unwrap();
    }

    #[test]
    fn split_repository_requires_owner_and_name() {
        assert_eq!(
            split_repository("fotografo/joao-maria-2026").unwrap(),
            ("fotografo", "joao-maria-2026")
        );
        for bad in ["fotografo", "/repo", "owner/", "a/b/c"] {
            assert!(split_repository(bad).is_err(), "accepted {bad}");
        }
    }

    /// Serializes tests that mutate process environment variables.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn missing_credentials_fail_before_any_provider_interaction() {
        let _guard = ENV_LOCK.lock().unwrap();
        for key in [
            "PHOTO_PUBLISHER_GITHUB_TOKEN",
            "PHOTO_PUBLISHER_R2_ACCESS_KEY_ID",
            "PHOTO_PUBLISHER_R2_SECRET_ACCESS_KEY",
            "PHOTO_PUBLISHER_VERCEL_TOKEN",
        ] {
            std::env::remove_var(key);
        }
        let store = EnvCredentialStore;
        let error = preflight_credentials(&store).unwrap_err();
        assert!(error.message.contains("PHOTO_PUBLISHER_GITHUB_TOKEN"));
        // The error names the variable, never any secret content.
        assert!(!error.message.contains("token value"));
    }

    #[test]
    fn environment_backed_store_reads_only_known_keys() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("PHOTO_PUBLISHER_VERCEL_TOKEN", "top-secret-value");
        let store = EnvCredentialStore;
        assert_eq!(
            store.get("vercel.token").unwrap().as_deref(),
            Some(b"top-secret-value".as_slice())
        );
        std::env::remove_var("PHOTO_PUBLISHER_VERCEL_TOKEN");
        assert!(store.get("vercel.token").unwrap().is_none());
        assert!(store.get("unknown.key").unwrap().is_none());
    }
}
