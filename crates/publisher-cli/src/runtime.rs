//! Composition root for the integrated publication.
//!
//! This module is the CLI's application layer: it wires the validated
//! `project.json` (schema v2) to the existing integration components without
//! moving any business rule into the CLI. Publication planning and all safety
//! semantics stay in `publisher-integration`; this module only constructs
//! providers, performs a credential preflight, and calls the Coordinator.
//!
//! Credentials are read from the process environment through the existing
//! `CredentialStore` trait. They are never written anywhere, never logged,
//! and never included in results or errors.

use std::path::Path;

use photo_publisher_integration::{
    compose_application_bundle, plan_reconciliation, ApplicationBundle, Coordinator,
    DesiredPublication, HostingPublicationConfig, HostingPublisher, IntegrationLedger,
    IntegrationOperation, KnownRemoteState, ProjectPublicationConfig, PublicGalleryManifest,
    PublicationReport,
};
use photo_publisher_provider_contracts::{
    CredentialStore, DeploymentInfo, HostingProvider, ProviderError, ProviderResult,
    RepositoryProvider, StorageProvider,
};
use photo_publisher_provider_github::{GitHubRepositoryConfig, GitHubRepositoryProvider};
use photo_publisher_provider_r2::{R2StorageConfig, R2StorageProvider};
use photo_publisher_provider_vercel::{VercelConfig, VercelHostingProvider};
use serde_json::{json, Value};

use crate::{CliError, CliResult, ErrorKind};

/// Reads provider credentials from process environment variables.
///
/// Read-only: `set`/`delete` are unsupported so this store can never mutate
/// secrets. Nothing is persisted, logged, or embedded in outputs.
#[derive(Debug, Clone, Copy, Default)]
struct EnvCredentialStore;

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
    let configuration = load_configuration(project_path)?;
    preflight_credentials(&EnvCredentialStore)?;
    Ok(configuration)
}

/// Integrated publication for a schema-v2 project: constructs the real
/// providers from the configuration and runs the ordered coordinator. The
/// local publication must already be committed under `output_dir`.
pub(crate) fn publish_integrated(
    configuration: &ProjectPublicationConfig,
    output_dir: &Path,
) -> CliResult<Value> {
    let credentials = EnvCredentialStore;

    let (owner, name) = split_repository(&configuration.repository)?;
    let mut github_config = GitHubRepositoryConfig::new(owner, name);
    if let Some(branch) = configuration.branch.as_deref() {
        github_config.branch = branch.to_owned();
    }
    let mut repository = GitHubRepositoryProvider::new(github_config, credentials)
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
    let mut storage = R2StorageProvider::new(
        R2StorageConfig::new(configuration.high_resolution_account_id.clone(), bucket),
        credentials,
    )
    .map_err(|e| CliError::from_any(ErrorKind::ProjectInvalid, e))?;

    let mut hosting = VercelHostingAdapter { credentials };
    publish_integrated_with(
        configuration,
        output_dir,
        &mut storage,
        &mut repository,
        &mut hosting,
    )
}

/// Provider-neutral adapter between the application bundle and the real
/// Vercel hosting provider — the single place where concrete provider
/// configuration meets the `HostingPublisher` port.
struct VercelHostingAdapter {
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

/// The orchestration core, injectable for offline tests: everything after the
/// local publication, with no knowledge of how providers were built.
pub(crate) fn publish_integrated_with(
    configuration: &ProjectPublicationConfig,
    output_dir: &Path,
    storage: &mut dyn StorageProvider,
    repository: &mut dyn RepositoryProvider,
    hosting: &mut dyn HostingPublisher,
) -> CliResult<Value> {
    let template =
        ApplicationBundle::from_directory(&configuration.bundle_directory).map_err(|error| {
            CliError::from_any(
                ErrorKind::ResourceMissing,
                format!("template bundle: {error}"),
            )
        })?;
    let manifest = PublicGalleryManifest::derive_from_committed_output(configuration, output_dir)
        .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;
    let bundle = compose_application_bundle(&template, &manifest)
        .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;
    let desired = DesiredPublication::from_committed_output(configuration, output_dir, &bundle)
        .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;

    let ledger_path = output_dir.join(".publisher/integration-state.json");
    let mut ledger = if ledger_path.is_file() {
        IntegrationLedger::read_from(&ledger_path)
            .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?
    } else {
        IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )
        .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?
    };
    let known = KnownRemoteState::from_ledger(&ledger)
        .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;
    let plan = plan_reconciliation(&desired, &known)
        .map_err(|e| CliError::from_any(ErrorKind::Publication, e))?;

    let mut coordinator = Coordinator::new(
        storage,
        repository,
        hosting,
        &mut ledger,
        &ledger_path,
        output_dir,
    );
    let report = coordinator
        .execute(&plan, &desired, &bundle, &configuration.hosting)
        .map_err(|error| CliError::from_any(ErrorKind::Publication, error))?;

    Ok(publication_json(&desired, &report))
}

fn load_configuration(project_path: &Path) -> CliResult<ProjectPublicationConfig> {
    photo_publisher_integration::load_project_v2(project_path)
        .map_err(|e| CliError::from_any(ErrorKind::ProjectInvalid, e))
}

/// Structured result built exclusively from the desired state identity and
/// the Coordinator report — public paths and deployment identity only; no
/// secrets and no duplicated status logic.
fn publication_json(desired: &DesiredPublication, report: &PublicationReport) -> Value {
    json!({
        "generation": desired.local.generation,
        "storage": report.storage.as_ref().map(|report| json!({
            "uploaded": report.uploaded.len(),
            "deleted": report.deleted.len(),
        })),
        "repository": report.repository.as_ref().map(|report| json!({
            "written": report.written.len(),
            "deleted": report.deleted.len(),
            "revision": report.revision,
        })),
        "hosting": report.hosting.as_ref().map(|report| json!({
            "deployment": report.deployment.as_ref().map(|deployment| json!({
                "id": deployment.id,
                "url": deployment.url,
            })),
        })),
    })
}

/// Dry-run integration view: computes the reconciliation plan without calling
/// providers and without writing the ledger. Returns `null` when the local
/// publication cannot produce a valid plan (for example, an older project
/// schema or nothing committed yet).
pub(crate) fn dry_run_integration(project_path: &Path, output_dir: &Path) -> Value {
    match compute_dry_run_integration(project_path, output_dir) {
        Ok(value) => value,
        Err(_) => Value::Null,
    }
}

fn compute_dry_run_integration(project_path: &Path, output_dir: &Path) -> anyhow::Result<Value> {
    let configuration = photo_publisher_integration::load_project_v2(project_path)?;
    let template = ApplicationBundle::from_directory(&configuration.bundle_directory)?;
    let manifest = PublicGalleryManifest::derive_from_committed_output(&configuration, output_dir)?;
    let bundle = compose_application_bundle(&template, &manifest)?;
    let desired = DesiredPublication::from_committed_output(&configuration, output_dir, &bundle)?;
    let ledger_path = output_dir.join(".publisher/integration-state.json");
    let ledger = if ledger_path.is_file() {
        IntegrationLedger::read_from(&ledger_path)?
    } else {
        IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )?
    };
    let known = KnownRemoteState::from_ledger(&ledger)?;
    let plan = plan_reconciliation(&desired, &known)?;
    let count = |predicate: fn(&IntegrationOperation) -> bool| {
        plan.operations.iter().filter(|op| predicate(op)).count()
    };
    Ok(json!({
        "planned": true,
        "operations": {
            "storage": count(|op| matches!(op, IntegrationOperation::PutStorage(_) | IntegrationOperation::DeleteStorage { .. })),
            "repository": count(|op| matches!(op, IntegrationOperation::WriteRepository(_) | IntegrationOperation::DeleteRepository { .. })),
            "hosting": count(|op| matches!(op, IntegrationOperation::PublishHosting { .. })),
        },
        "reconciliation_requirements": plan.reconciliation_requirements.len(),
    }))
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

/// Offline end-to-end composition: fake providers with the same semantics
/// proven by the executor suites (single-commit repository batches, one
/// hosting publication), driving the real composition path.
#[cfg(test)]
mod integration_tests {
    use super::tests::VALID_PROJECT;
    use super::*;
    use std::collections::HashMap;
    use std::io::Read;

    use photo_publisher_core::{state_from, SourcePhoto};
    use photo_publisher_pipeline::journal::{sha256_file, JournalPhase, JournalRecord};
    use photo_publisher_provider_contracts::{
        copy_with_sha256, CommitInfo, FileMetadata, ObjectKey, RepositoryPath, StorageObject,
    };
    use tempfile::tempdir;

    fn sha_of(bytes: &[u8]) -> String {
        let (size, sha256) = copy_with_sha256(&mut &*bytes, &mut std::io::sink()).unwrap();
        let _ = size;
        sha256
    }

    #[derive(Default)]
    struct FakeStorage {
        objects: HashMap<String, StorageObject>,
    }

    impl StorageProvider for FakeStorage {
        fn put(
            &mut self,
            key: &ObjectKey,
            content: &mut dyn Read,
            content_type: Option<&str>,
        ) -> ProviderResult<StorageObject> {
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(ProviderError::from_io)?;
            let object = StorageObject {
                key: key.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha_of(&bytes),
                content_type: content_type.map(str::to_owned),
            };
            self.objects.insert(key.as_str().to_owned(), object.clone());
            Ok(object)
        }

        fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
            Ok(self.objects.get(key.as_str()).cloned())
        }

        fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
            self.objects.remove(key.as_str());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRepository {
        files: HashMap<String, Vec<u8>>,
        staged: HashMap<String, Option<Vec<u8>>>,
        commits: usize,
    }

    impl RepositoryProvider for FakeRepository {
        fn ensure_repository(&mut self) -> ProviderResult<()> {
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
                sha256: sha_of(&bytes),
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
                // Base probe: publish nothing.
                return Ok(CommitInfo {
                    revision: "base".to_owned(),
                    message: message.to_owned(),
                    changed_paths: Vec::new(),
                });
            }
            self.commits += 1;
            let staged = std::mem::take(&mut self.staged);
            let mut changed_paths: Vec<RepositoryPath> = staged
                .keys()
                .map(|path| RepositoryPath::new(path).unwrap())
                .collect();
            changed_paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
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
            Ok(CommitInfo {
                revision: format!("commit-{:02}", self.commits),
                message: message.to_owned(),
                changed_paths,
            })
        }
    }

    #[derive(Default)]
    struct FakeHosting {
        published: Vec<String>,
    }

    impl HostingPublisher for FakeHosting {
        fn publish(
            &mut self,
            bundle: &ApplicationBundle,
            configuration: &HostingPublicationConfig,
        ) -> ProviderResult<DeploymentInfo> {
            self.published.push(bundle.fingerprint().to_owned());
            Ok(DeploymentInfo {
                id: "dpl-test".to_owned(),
                url: format!("{}.vercel.app", configuration.project_id),
            })
        }
    }

    /// Writes a journal-committed local publication into `output` with a
    /// byte-exact download asset and a re-encoded preview asset.
    fn commit_publication(output: &Path, source_bytes: &[u8]) {
        let sha = sha_of(source_bytes);
        let gallery = json!({
            "schemaVersion": 1,
            "gallery": {"id": "gallery-local", "title": "T"},
            "photos": [{
                "id": "photo-1",
                "filename": "a.jpg",
                "sequence": 1,
                "preview": {"url": format!("photos/preview/{sha}.jpg"), "width": 1, "height": 1},
                "download": {"url": format!("photos/download/{sha}.jpg"), "width": 1, "height": 1}
            }]
        });
        let state = state_from(&[SourcePhoto {
            relative_path: "a.jpg".to_owned(),
            bytes: source_bytes.len() as u64,
            sha256: sha.clone(),
        }]);
        let gallery_path = output.join("gallery.json");
        let state_path = output.join(".publisher/state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(output.join("photos/download")).unwrap();
        std::fs::create_dir_all(output.join("photos/preview")).unwrap();
        std::fs::write(&gallery_path, serde_json::to_vec_pretty(&gallery).unwrap()).unwrap();
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(
            output.join(format!("photos/download/{sha}.jpg")),
            source_bytes,
        )
        .unwrap();
        std::fs::write(
            output.join(format!("photos/preview/{sha}.jpg")),
            format!("preview {sha}").into_bytes(),
        )
        .unwrap();
        let journal = JournalRecord::new(
            1,
            "g-000001".to_owned(),
            None,
            JournalPhase::Committed,
            sha256_file(&gallery_path).unwrap(),
            sha256_file(&state_path).unwrap(),
            ".publisher/staging/x".to_owned(),
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

    fn build_bundle(configuration: &ProjectPublicationConfig, output: &Path) -> ApplicationBundle {
        let template = ApplicationBundle::from_directory(&configuration.bundle_directory).unwrap();
        let manifest =
            PublicGalleryManifest::derive_from_committed_output(configuration, output).unwrap();
        compose_application_bundle(&template, &manifest).unwrap()
    }

    #[test]
    fn composition_runs_the_coordinator_over_fake_providers() {
        let root = tempdir().unwrap();
        // Spaces and Unicode in the project directory must work end to end.
        let project_dir = root.path().join("João & Maria");
        std::fs::create_dir_all(project_dir.join("gallery-app")).unwrap();
        std::fs::write(project_dir.join("gallery-app/index.html"), b"<h1>g</h1>").unwrap();
        let project_path = project_dir.join("project.json");
        std::fs::write(&project_path, VALID_PROJECT).unwrap();
        let output = project_dir.join("output");
        commit_publication(&output, b"source-a");

        let configuration = load_configuration(&project_path).unwrap();
        let bundle = build_bundle(&configuration, &output);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();

        let result = publish_integrated_with(
            &configuration,
            &output,
            &mut storage,
            &mut repository,
            &mut hosting,
        )
        .unwrap();

        // The composition landed: one storage object, repository files on
        // their publication paths, one hosting deployment.
        assert!(storage
            .objects
            .values()
            .any(|object| object.sha256 == sha_of(b"source-a")));
        assert!(repository.files.contains_key("index.html"));
        assert!(repository.files.contains_key("gallery.json"));
        let preview_paths: Vec<_> = repository
            .files
            .keys()
            .filter(|path| path.starts_with("previews/photos/preview/"))
            .collect();
        assert_eq!(preview_paths.len(), 1);
        assert_eq!(
            repository.files[preview_paths[0]],
            format!("preview {}", sha_of(b"source-a")).as_bytes()
        );
        assert_eq!(repository.commits, 1);
        assert_eq!(hosting.published.len(), 1);
        assert_eq!(result["generation"], json!("g-000001"));
        assert_eq!(result["storage"]["uploaded"], json!(1));
        assert_eq!(result["repository"]["revision"], json!("commit-01"));
        assert_eq!(
            result["hosting"]["deployment"]["url"],
            json!("joao-maria-2026.vercel.app")
        );
        // The structured output must not carry any credential material.
        let text = result.to_string();
        assert!(!text.contains("PHOTO_PUBLISHER_"));
        assert!(!text.contains("token"));

        // Idempotency through the same composed path.
        let ledger =
            IntegrationLedger::read_from(output.join(".publisher/integration-state.json")).unwrap();
        let known = KnownRemoteState::from_ledger(&ledger).unwrap();
        let desired =
            DesiredPublication::from_committed_output(&configuration, &output, &bundle).unwrap();
        let plan = plan_reconciliation(&desired, &known).unwrap();
        assert!(plan.operations.is_empty() && plan.reconciliation_requirements.is_empty());
        let report = {
            let mut ledger =
                IntegrationLedger::read_from(output.join(".publisher/integration-state.json"))
                    .unwrap();
            let mut coordinator = Coordinator::new(
                &mut storage,
                &mut repository,
                &mut hosting,
                &mut ledger,
                output.join(".publisher/integration-state.json"),
                &output,
            );
            coordinator
                .execute(&plan, &desired, &bundle, &configuration.hosting)
                .unwrap()
        };
        assert!(report.storage.is_none());
        assert!(report.repository.is_none());
        assert!(report.hosting.is_none());
        assert_eq!(hosting.published.len(), 1);
    }
}
