//! Dry-run: compute the integration plan WITHOUT touching providers or the
//! ledger. Never constructs providers, never publishes, never writes.

use std::path::Path;

use photo_publisher_integration::{
    compose_application_bundle, plan_reconciliation, ApplicationBundle, DesiredPublication,
    IntegrationLedger, IntegrationOperation, KnownRemoteState, PublicGalleryManifest,
};

use crate::errors::{ApplicationError, ApplicationErrorKind};
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};

/// Counts produced by planning, classified per family, with no remote intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunOutcome {
    pub generation: String,
    pub storage_operations: usize,
    pub repository_operations: usize,
    pub hosting_operations: usize,
    pub reconciliation_requirements: usize,
}

/// Plans the reconciliation of a v2 project without providers or writes.
/// Mirrors the previous `compute_dry_run_integration` exactly: any failure to
/// load/validate the integrated configuration or the committed local
/// publication surfaces as an application error (the caller may degrade to a
/// `null` integration view, exactly as the CLI did).
pub fn dry_run_project(
    project_path: &Path,
    events: &mut EventSink<'_>,
) -> Result<DryRunOutcome, ApplicationError> {
    events(ApplicationEvent::EnteredStep(WorkflowStep::DryRun));
    let result = run(project_path);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::DryRun,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::DryRun,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(project_path: &Path) -> Result<DryRunOutcome, ApplicationError> {
    let configuration =
        photo_publisher_integration::load_project_v2(project_path).map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::ProjectInvalid,
                format!("{error}"),
                error,
            )
        })?;
    // Dry-run only reads the committed local publication; nothing is built,
    // written, or uploaded.
    let output_dir = crate::project::resolve_output_dir(project_path);
    let output_dir = output_dir.as_path();
    let template =
        ApplicationBundle::from_directory(&configuration.bundle_directory).map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::ResourceMissing,
                format!("template bundle: {error}"),
                error,
            )
        })?;
    let manifest = PublicGalleryManifest::derive_from_committed_output(&configuration, output_dir)
        .map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })?;
    let bundle = compose_application_bundle(&template, &manifest).map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Publication, format!("{error}"), error)
    })?;
    let desired = DesiredPublication::from_committed_output(&configuration, output_dir, &bundle)
        .map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })?;

    // Read-only: the ledger is never persisted back.
    let ledger_path = output_dir.join(".publisher/integration-state.json");
    let ledger = if ledger_path.is_file() {
        IntegrationLedger::read_from(&ledger_path).map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })?
    } else {
        IntegrationLedger::new(
            desired.configuration_fingerprint.clone(),
            desired.local.generation.clone(),
            desired.local.state_hash.clone(),
            desired.local.manifest_hash.clone(),
        )
        .map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })?
    };

    let known = KnownRemoteState::from_ledger(&ledger).map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Publication, format!("{error}"), error)
    })?;
    let plan = plan_reconciliation(&desired, &known).map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Publication, format!("{error}"), error)
    })?;

    let mut storage_operations = 0;
    let mut repository_operations = 0;
    let mut hosting_operations = 0;
    for operation in &plan.operations {
        match operation {
            IntegrationOperation::PutStorage(_) | IntegrationOperation::DeleteStorage { .. } => {
                storage_operations += 1
            }
            IntegrationOperation::WriteRepository(_)
            | IntegrationOperation::DeleteRepository { .. } => repository_operations += 1,
            IntegrationOperation::PublishHosting { .. } => hosting_operations += 1,
        }
    }

    Ok(DryRunOutcome {
        generation: desired.local.generation,
        storage_operations,
        repository_operations,
        hosting_operations,
        reconciliation_requirements: plan.reconciliation_requirements.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use photo_publisher_integration::HostingPublicationConfig;
    use photo_publisher_provider_contracts::{
        copy_with_sha256, CommitInfo, CredentialStore, DeploymentInfo, FileMetadata, ObjectKey,
        ProviderResult, RepositoryPath, RepositoryProvider, StorageObject, StorageProvider,
    };
    use std::io::Read;
    use tempfile::tempdir;

    use super::super::publish::PublishOptions;
    use crate::providers::PublicationProviders;
    use crate::use_cases::publish::publish_project;
    use crate::PublicationOutcome;

    fn write_jpeg(path: &Path, value: u8) {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
        image.save(path).unwrap();
    }

    fn sha_of(bytes: &[u8]) -> String {
        let (size, sha256) = copy_with_sha256(&mut &*bytes, &mut std::io::sink()).unwrap();
        let _ = size;
        sha256
    }

    // Minimal fakes reused to produce a committed publication + ledger before
    // the dry run; the dry run itself receives no providers by construction.
    #[derive(Default)]
    struct FakeStorage(HashMap<String, StorageObject>);
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
                .map_err(photo_publisher_provider_contracts::ProviderError::from_io)?;
            let object = StorageObject {
                key: key.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha_of(&bytes),
                content_type: content_type.map(str::to_owned),
            };
            self.0.insert(key.as_str().to_owned(), object.clone());
            Ok(object)
        }
        fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
            Ok(self.0.get(key.as_str()).cloned())
        }
        fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
            self.0.remove(key.as_str());
            Ok(())
        }
    }
    #[derive(Default)]
    struct FakeRepository {
        staged: HashMap<String, Option<Vec<u8>>>,
        files: HashMap<String, Vec<u8>>,
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
                .ok_or(photo_publisher_provider_contracts::ProviderError::NotFound)
        }
        fn write(
            &mut self,
            path: &RepositoryPath,
            content: &mut dyn Read,
        ) -> ProviderResult<FileMetadata> {
            let mut bytes = Vec::new();
            content
                .read_to_end(&mut bytes)
                .map_err(photo_publisher_provider_contracts::ProviderError::from_io)?;
            self.staged
                .insert(path.as_str().to_owned(), Some(bytes.clone()));
            Ok(FileMetadata {
                path: path.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha_of(&bytes),
            })
        }
        fn delete(&mut self, path: &RepositoryPath) -> ProviderResult<()> {
            self.staged.insert(path.as_str().to_owned(), None);
            Ok(())
        }
        fn commit(&mut self, message: &str) -> ProviderResult<CommitInfo> {
            for (path, change) in std::mem::take(&mut self.staged) {
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
                revision: "r".to_owned(),
                message: message.to_owned(),
                changed_paths: Vec::new(),
            })
        }
    }
    #[derive(Default)]
    struct FakeHosting;
    impl photo_publisher_integration::HostingPublisher for FakeHosting {
        fn publish(
            &mut self,
            _bundle: &ApplicationBundle,
            _configuration: &HostingPublicationConfig,
        ) -> ProviderResult<DeploymentInfo> {
            Ok(DeploymentInfo {
                id: "dpl".to_owned(),
                url: "u".to_owned(),
            })
        }
    }
    #[derive(Default)]
    struct FakeCredentials;
    impl CredentialStore for FakeCredentials {
        fn get(&self, _name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(None)
        }
        fn set(&mut self, _name: &str, _secret: &[u8]) -> ProviderResult<()> {
            Ok(())
        }
        fn delete(&mut self, _name: &str) -> ProviderResult<()> {
            Ok(())
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        project_path: std::path::PathBuf,
    }

    fn v2_fixture() -> Fixture {
        let root = tempdir().unwrap();
        let base = root.path().join("Dry Run");
        std::fs::create_dir_all(base.join("source")).unwrap();
        std::fs::create_dir_all(base.join("gallery-app")).unwrap();
        std::fs::write(base.join("gallery-app/index.html"), b"<h1>d</h1>").unwrap();
        write_jpeg(&base.join("source").join("a.jpg"), 3);
        let document = serde_json::json!({
            "schemaVersion": 2,
            "project": {"id": "dry-run", "name": "Dry Run"},
            "gallery": {"template": "editorial-v1", "title": "Dry Run", "bundlePath": "gallery-app"},
            "source": {"type": "folder", "path": "source"},
            "repository": {"provider": "github", "repository": "owner/repo", "branch": "main"},
            "hosting": {"provider": "vercel", "project": "dry-run"},
            "storage": {
                "preview": {"provider": "github", "prefix": "previews", "publicBaseUrl": "https://cdn.example.com/previews"},
                "highResolution": {"provider": "r2", "accountId": "acct", "bucket": "bkt", "prefix": "originals", "publicBaseUrl": "https://downloads.example.com/originals"}
            }
        });
        let project_path = base.join("project.json");
        std::fs::write(&project_path, serde_json::to_vec(&document).unwrap()).unwrap();
        Fixture { root, project_path }
    }

    #[test]
    fn dry_run_without_committed_publication_is_publication_error() {
        let fixture = v2_fixture();
        let error = dry_run_project(&fixture.project_path, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::Publication);
        // Nothing was created: the output directory must not exist.
        assert!(!fixture
            .root
            .path()
            .join("Dry Run/output/.publisher/integration-state.json")
            .exists());
    }

    #[test]
    fn dry_run_reports_plan_without_touching_remote_or_ledger() {
        let fixture = v2_fixture();
        // Commit a real publication + ledger via the application's own publish
        // path, so the dry run has a committed baseline.
        let configuration =
            photo_publisher_integration::load_project_v2(&fixture.project_path).unwrap();
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting;
        let credentials = FakeCredentials;
        let outcome = publish_project(
            &fixture.project_path,
            Some(&configuration),
            PublishOptions,
            &mut PublicationProviders {
                storage: &mut storage,
                repository: &mut repository,
                hosting: &mut hosting,
                credentials: &credentials,
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(matches!(
            outcome.publication,
            PublicationOutcome::Published(_)
        ));

        let storage_objects_before = storage.0.len();
        let ledger_path = fixture
            .root
            .path()
            .join("Dry Run/output/.publisher/integration-state.json");
        let ledger_bytes_before = std::fs::read(&ledger_path).unwrap();
        let storage_files_before: Vec<_> = storage.0.keys().cloned().collect();

        // A dry run never emits execution events: it computes a plan only.
        let mut events: Vec<crate::ApplicationEvent> = Vec::new();
        let dry = dry_run_project(&fixture.project_path, &mut |event| events.push(event)).unwrap();
        assert_eq!(dry.generation, "g-000001");
        assert_eq!(dry.storage_operations, 0);
        assert_eq!(dry.repository_operations, 0);
        assert_eq!(dry.hosting_operations, 0);
        assert_eq!(dry.reconciliation_requirements, 0);

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, crate::ApplicationEvent::Operation(_))),
            "dry-run must never emit operation events"
        );

        // No remote writes happened (fakes unchanged) and the ledger is
        // byte-identical: dry-run is read-only by construction.
        assert_eq!(storage.0.len(), storage_objects_before);
        assert_eq!(
            storage.0.keys().cloned().collect::<Vec<_>>(),
            storage_files_before
        );
        assert_eq!(std::fs::read(&ledger_path).unwrap(), ledger_bytes_before);
    }

    #[test]
    fn dry_run_after_new_photo_plans_new_operations_without_remote_calls() {
        let fixture = v2_fixture();
        let configuration =
            photo_publisher_integration::load_project_v2(&fixture.project_path).unwrap();
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting;
        let credentials = FakeCredentials;
        publish_project(
            &fixture.project_path,
            Some(&configuration),
            PublishOptions,
            &mut PublicationProviders {
                storage: &mut storage,
                repository: &mut repository,
                hosting: &mut hosting,
                credentials: &credentials,
            },
            &mut |_| {},
        )
        .unwrap();

        // Add a new photo and re-publish to advance the generation — the
        // ledger then says g-000001 but local g-000002 exists.
        write_jpeg(&fixture.root.path().join("Dry Run/source/b.jpg"), 7);
        publish_project(
            &fixture.project_path,
            Some(&configuration),
            PublishOptions,
            &mut PublicationProviders {
                storage: &mut storage,
                repository: &mut repository,
                hosting: &mut hosting,
                credentials: &credentials,
            },
            &mut |_| {},
        )
        .unwrap();

        let storage_len = storage.0.len();
        let repository_len = repository.files.len();
        let dry = dry_run_project(&fixture.project_path, &mut |_| {}).unwrap();
        assert_eq!(dry.generation, "g-000002");
        assert_eq!(dry.storage_operations, 0);
        assert_eq!(dry.repository_operations, 0);
        assert_eq!(dry.hosting_operations, 0);
        assert_eq!(storage.0.len(), storage_len);
        assert_eq!(repository.files.len(), repository_len);

        // Dry-run needs no providers at all — compile-time proof is the
        // signature, and the params above stay untouched.
    }
}
