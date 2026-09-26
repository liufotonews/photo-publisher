//! Publish: orchestrate the integrated publication through existing crates.
//!
//! Provider-neutral by construction: this use case orchestrates the
//! integration layer (planner + coordinator) but never names GitHub, R2, or
//! Vercel. All remote outcomes, reconciliation guarantees, and credential
//! safety are preserved by passing through trait objects only.
//!
//! Event order (unchanged from the previous runtime order):
//! validate → local publication → configuration → bundle/manifest → plan →
//! coordinator.

use std::path::Path;

use photo_publisher_integration::{
    compose_application_bundle, plan_reconciliation, ApplicationBundle, Coordinator,
    DesiredPublication, IntegrationLedger, IntegrationPlan, KnownRemoteState,
    ProjectPublicationConfig, PublicGalleryManifest,
};

use crate::errors::{ApplicationError, ApplicationErrorKind};
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};
use crate::outcome::PublicationOutcome;
use crate::project::{ProjectHandle, ProjectKind};
use crate::providers::PublicationProviders;

use super::validate::validate_project_inner;

/// Options for an application-level publish run. No behavior-changing option
/// exists yet; this stays empty until a real one appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PublishOptions;

/// Outcome of an application publish: the project handle plus the outcome;
/// nothing presentational (JSON/cli text) is carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    /// Generation of the committed local publication.
    pub generation: String,
    /// Project identity and resolved paths (for the caller to format).
    pub handle: ProjectHandle,
    /// The application outcome of the publish run.
    pub publication: PublicationOutcome,
}

/// Runs a step with its lifecycle events.
fn step<T>(
    events: &mut EventSink<'_>,
    step: WorkflowStep,
    body: impl FnOnce() -> Result<T, ApplicationError>,
) -> Result<T, ApplicationError> {
    events(ApplicationEvent::EnteredStep(step));
    let result = body();
    events(ApplicationEvent::LeftStep {
        step,
        ok: result.is_ok(),
    });
    result
}

/// Runs the integrated publication through the existing composition.
///
/// The configuration is produced by the composition root (CLI today, GUI
/// later); nothing here constructs providers.
pub fn publish_project(
    project_path: &Path,
    configuration: Option<&ProjectPublicationConfig>,
    options: PublishOptions,
    providers: &mut PublicationProviders<'_>,
    events: &mut EventSink<'_>,
) -> Result<PublishOutcome, ApplicationError> {
    let _ = options;
    events(ApplicationEvent::EnteredStep(WorkflowStep::LoadProject));
    let result = run(project_path, configuration, providers, events);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::LoadProject,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::LoadProject,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(
    project_path: &Path,
    configuration: Option<&ProjectPublicationConfig>,
    providers: &mut PublicationProviders<'_>,
    events: &mut EventSink<'_>,
) -> Result<PublishOutcome, ApplicationError> {
    // Validate the document and resolve paths exactly as ValidateProject
    // does, without re-emitting lifecycle events from the inner use case.
    let handle = validate_project_inner(project_path)?;

    // Local publication build. This intentionally stays in this layer: the
    // pipeline is already published/committed underneath; we do not change it.
    step(events, WorkflowStep::LocalPublication, || {
        build_local_publication(&handle)
    })?;

    if handle.kind != ProjectKind::Version2 {
        // v1: local-only production; no remote effect, no provider touch.
        let generation = read_generation_shared(&handle)?;
        return Ok(PublishOutcome {
            handle,
            generation: generation.clone(),
            publication: PublicationOutcome::NoChange { generation },
        });
    }

    // v2: events Published flow. The concrete providers arrived ready-made.
    let configuration = configuration.ok_or_else(|| {
        ApplicationError::new(
            ApplicationErrorKind::ProjectInvalid,
            "integrated publication requires project schemaVersion 2",
        )
    })?;

    let mut publication = step(events, WorkflowStep::BuildPlan, || {
        prepare_v2_publication(&handle, configuration)
    })?;

    // The granular events are forwarded to the same single application sink
    // while the coordinator runs. Emission stays observation-only.
    events(ApplicationEvent::EnteredStep(
        WorkflowStep::PublishIntegrate,
    ));
    let report = {
        let mut coordinator = Coordinator::new(
            &mut *providers.storage,
            &mut *providers.repository,
            &mut *providers.hosting,
            &mut publication.ledger,
            publication.ledger_path.as_path(),
            handle.output_dir.as_path(),
        );
        let result = coordinator.execute_with_observer(
            &publication.plan,
            &publication.desired,
            &publication.bundle,
            &configuration.hosting,
            &mut |event| events(ApplicationEvent::Operation(event)),
        );
        events(ApplicationEvent::LeftStep {
            step: WorkflowStep::PublishIntegrate,
            ok: result.is_ok(),
        });
        result.map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })?
    };

    if publication.plan.operations.is_empty() {
        let generation = publication.desired.local.generation.clone();
        return Ok(PublishOutcome {
            handle,
            generation: generation.clone(),
            publication: PublicationOutcome::NoChange { generation },
        });
    }

    Ok(PublishOutcome {
        handle,
        generation: publication.desired.local.generation,
        publication: PublicationOutcome::Published(report),
    })
}

struct PreparedPublication {
    bundle: ApplicationBundle,
    desired: DesiredPublication,
    ledger: IntegrationLedger,
    ledger_path: std::path::PathBuf,
    plan: IntegrationPlan,
}

fn build_local_publication(handle: &ProjectHandle) -> Result<(), ApplicationError> {
    let document = crate::project::load_project_document(&handle.project_path)?;
    let title = document["gallery"]["title"].as_str().ok_or_else(|| {
        ApplicationError::new(
            ApplicationErrorKind::ProjectInvalid,
            "gallery.title is required",
        )
    })?;
    photo_publisher_pipeline::build_local_gallery(&photo_publisher_pipeline::PipelineOptions {
        source_dir: handle.source_dir.clone(),
        output_dir: handle.output_dir.clone(),
        project_title: title.to_owned(),
    })
    .map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Publication, format!("{error}"), error)
    })
}

pub(crate) fn read_generation_shared(handle: &ProjectHandle) -> Result<String, ApplicationError> {
    photo_publisher_integration::LocalPublication::from_output(handle.output_dir.as_path())
        .map(|publication| publication.generation)
        .map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })
}

fn prepare_v2_publication(
    handle: &ProjectHandle,
    configuration: &ProjectPublicationConfig,
) -> Result<PreparedPublication, ApplicationError> {
    let output_dir = &handle.output_dir;

    let template =
        ApplicationBundle::from_directory(&configuration.bundle_directory).map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::ResourceMissing,
                format!("template bundle: {error}"),
                error,
            )
        })?;
    let manifest = PublicGalleryManifest::derive_from_committed_output(configuration, output_dir)
        .map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Publication, format!("{error}"), error)
    })?;
    let bundle = compose_application_bundle(&template, &manifest).map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Publication, format!("{error}"), error)
    })?;
    let desired = DesiredPublication::from_committed_output(configuration, output_dir, &bundle)
        .map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Publication,
                format!("{error}"),
                error,
            )
        })?;

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

    Ok(PreparedPublication {
        bundle,
        desired,
        ledger,
        ledger_path,
        plan,
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

    fn write_jpeg(path: &Path, value: u8) {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
        image.save(path).unwrap();
    }

    fn sha_of(bytes: &[u8]) -> String {
        let (size, sha256) = copy_with_sha256(&mut &*bytes, &mut std::io::sink()).unwrap();
        let _ = size;
        sha256
    }

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
            if self.staged.is_empty() {
                return Ok(CommitInfo {
                    revision: "base".to_owned(),
                    message: message.to_owned(),
                    changed_paths: Vec::new(),
                });
            }
            self.commits += 1;
            let staged = std::mem::take(&mut self.staged);
            let mut changed: Vec<RepositoryPath> = staged
                .keys()
                .map(|path| RepositoryPath::new(path).unwrap())
                .collect();
            changed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
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
                changed_paths: changed,
            })
        }
    }

    #[derive(Default)]
    struct FakeHosting {
        published: Vec<String>,
    }

    impl photo_publisher_integration::HostingPublisher for FakeHosting {
        fn publish(
            &mut self,
            bundle: &ApplicationBundle,
            configuration: &HostingPublicationConfig,
        ) -> ProviderResult<DeploymentInfo> {
            self.published.push(format!(
                "{}:{}:{}",
                configuration.project_id,
                bundle.fingerprint(),
                configuration.team_id.as_deref().unwrap_or("<none>")
            ));
            Ok(DeploymentInfo {
                id: "dpl-fake".to_owned(),
                url: "example.test".to_owned(),
            })
        }
    }

    #[derive(Default)]
    struct FakeCredentials {
        values: HashMap<String, Vec<u8>>,
    }

    impl CredentialStore for FakeCredentials {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(self.values.get(name).cloned())
        }
        fn set(&mut self, _name: &str, _secret: &[u8]) -> ProviderResult<()> {
            Ok(())
        }
        fn delete(&mut self, _name: &str) -> ProviderResult<()> {
            Ok(())
        }
    }

    fn fake_credentials() -> FakeCredentials {
        let mut store = FakeCredentials::default();
        for name in [
            "github.token",
            "r2.access_key_id",
            "r2.secret_access_key",
            "vercel.token",
        ] {
            store
                .values
                .insert(name.to_owned(), b"dummy-not-a-secret".to_vec());
        }
        store
    }

    struct Fixture {
        #[allow(dead_code)] // keeps the tempdir alive for the whole test
        root: tempfile::TempDir,
        project_path: std::path::PathBuf,
    }

    fn fixture(schema_version: u64) -> Fixture {
        let root = tempdir().unwrap();
        let base = root.path().join("Meu Álbum");
        std::fs::create_dir_all(base.join("source")).unwrap();
        if schema_version == 2 {
            std::fs::create_dir_all(base.join("gallery-app")).unwrap();
            std::fs::write(base.join("gallery-app/index.html"), b"<h1>a</h1>").unwrap();
        }
        write_jpeg(&base.join("source").join("a.jpg"), 1);
        let document = if schema_version == 2 {
            serde_json::json!({
                "schemaVersion": 2,
                "project": {"id": "pub-test", "name": "Pub Test"},
                "gallery": {"template": "editorial-v1", "title": "Pub Test", "bundlePath": "gallery-app"},
                "source": {"type": "folder", "path": "source"},
                "repository": {"provider": "github", "repository": "owner/repo", "branch": "main"},
                "hosting": {"provider": "vercel", "project": "pub-test", "teamId": "team_x"},
                "storage": {
                    "preview": {"provider": "github", "prefix": "previews", "publicBaseUrl": "https://cdn.example.com/previews"},
                    "highResolution": {"provider": "r2", "accountId": "acct", "bucket": "bkt", "prefix": "originals", "publicBaseUrl": "https://downloads.example.com/originals"}
                }
            })
        } else {
            serde_json::json!({
                "schemaVersion": 1,
                "project": {"id": "pub-test", "name": "Pub Test"},
                "gallery": {"template": "local", "title": "Pub Test"},
                "source": {"type": "folder", "path": "source"},
                "repository": {"provider": "local", "repository": "local"},
                "hosting": {"provider": "local"},
                "storage": {
                    "preview": {"provider": "local"},
                    "highResolution": {"provider": "local"}
                }
            })
        };
        let project_path = base.join("project.json");
        std::fs::write(&project_path, serde_json::to_vec(&document).unwrap()).unwrap();
        Fixture { root, project_path }
    }

    fn configuration_from(path: &std::path::Path) -> ProjectPublicationConfig {
        photo_publisher_integration::load_project_v2(path).unwrap()
    }

    #[test]
    fn publish_v1_publishes_locally_with_no_remote_work() {
        let fixture = fixture(1);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
        let outcome = publish_project(
            &fixture.project_path,
            None,
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

        assert_eq!(outcome.generation, "g-000001");
        assert!(outcome.handle.output_dir.join("gallery.json").is_file());
        assert!(matches!(
            outcome.publication,
            PublicationOutcome::NoChange { .. }
        ));
        assert!(storage.0.is_empty());
        assert!(repository.files.is_empty());
        assert!(hosting.published.is_empty());
    }

    #[test]
    fn publish_v2_forwards_granular_events_through_the_single_sink() {
        use photo_publisher_integration::{IntegrationEvent, OperationFailure};
        let _ = OperationFailure::Ambiguous; // ensure the type is observable
        let fixture = fixture(2);
        let configuration = configuration_from(&fixture.project_path);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
        let mut events: Vec<ApplicationEvent> = Vec::new();
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
            &mut |event| events.push(event),
        )
        .unwrap();

        let operations: Vec<&IntegrationEvent> = events
            .iter()
            .filter_map(|event| match event {
                ApplicationEvent::Operation(operation) => Some(operation),
                _ => None,
            })
            .collect();
        let kinds: Vec<&'static str> = operations
            .iter()
            .map(|operation| match operation {
                IntegrationEvent::StoragePutStarted { .. } => "StoragePutStarted",
                IntegrationEvent::StoragePutFinished { .. } => "StoragePutFinished",
                IntegrationEvent::StorageDeleteStarted { .. } => "StorageDeleteStarted",
                IntegrationEvent::StorageDeleteFinished { .. } => "StorageDeleteFinished",
                IntegrationEvent::RepositoryBatchStarted { .. } => "RepositoryBatchStarted",
                IntegrationEvent::RepositoryBatchFinished { .. } => "RepositoryBatchFinished",
                IntegrationEvent::HostingPublishStarted { .. } => "HostingPublishStarted",
                IntegrationEvent::HostingPublishFinished { .. } => "HostingPublishFinished",
                IntegrationEvent::StoragePutFailed { .. } => "StoragePutFailed",
                IntegrationEvent::StorageDeleteFailed { .. } => "StorageDeleteFailed",
                IntegrationEvent::RepositoryBatchFailed { .. } => "RepositoryBatchFailed",
                IntegrationEvent::HostingPublishFailed { .. } => "HostingPublishFailed",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "StoragePutStarted",
                "StoragePutFinished",
                "RepositoryBatchStarted",
                "RepositoryBatchFinished",
                "HostingPublishStarted",
                "HostingPublishFinished",
            ]
        );

        // Identity and progress fields must be domain data only.
        let IntegrationEvent::StoragePutStarted {
            key,
            size_bytes,
            generation,
            index,
            total,
        } = operations[0]
        else {
            panic!("expected StoragePutStarted");
        };
        assert!(key.as_str().starts_with("originals/photos/download/"));
        assert!(*size_bytes > 0);
        assert_eq!(generation, "g-000001");
        assert_eq!(*index, 1);
        assert_eq!(*total, 1);
        let IntegrationEvent::RepositoryBatchStarted {
            writes,
            deletes,
            paths,
            ..
        } = operations[2]
        else {
            panic!("expected RepositoryBatchStarted");
        };
        assert_eq!(*writes, 3); // index.html + gallery.json + preview asset
        assert_eq!(*deletes, 0);
        assert!(paths
            .iter()
            .any(|path| path.as_str().starts_with("previews/photos/preview/")));
        let IntegrationEvent::HostingPublishFinished {
            deployment_id,
            url,
            generation,
        } = operations[5]
        else {
            panic!("expected HostingPublishFinished");
        };
        assert_eq!(deployment_id, "dpl-fake");
        assert_eq!(url, "example.test");
        assert_eq!(generation, "g-000001");

        // Workflow steps stay the enveloping lifecycle; operations sit inside
        // PublishIntegrate.
        let first_operation = events
            .iter()
            .position(|event| matches!(event, ApplicationEvent::Operation(_)))
            .unwrap();
        let entered_integrate = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ApplicationEvent::EnteredStep(WorkflowStep::PublishIntegrate)
                )
            })
            .unwrap();
        assert!(entered_integrate < first_operation);
        assert_eq!(events.last(), Some(&ApplicationEvent::Finished));
    }

    #[test]
    fn second_publish_emits_zero_operation_events() {
        let fixture = fixture(2);
        let configuration = configuration_from(&fixture.project_path);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
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

        // Idempotent second run: plan is empty, so no operation events at all.
        let mut events: Vec<ApplicationEvent> = Vec::new();
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
            &mut |event| events.push(event),
        )
        .unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ApplicationEvent::Operation(_))),
            "idempotent publish must not invent operation events"
        );
        assert_eq!(events.last(), Some(&ApplicationEvent::Finished));
    }

    #[test]
    fn operation_payloads_carry_no_secrets_and_no_local_paths() {
        let fixture = fixture(2);
        let root_display = fixture.root.path().to_string_lossy().into_owned();
        let configuration = configuration_from(&fixture.project_path);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
        let mut events: Vec<ApplicationEvent> = Vec::new();
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
            &mut |event| events.push(event),
        )
        .unwrap();
        let text = format!("{events:?}");
        for needle in [
            "token",
            "secret",
            "authorization",
            "password",
            "PHOTO_PUBLISHER_",
        ] {
            assert!(
                !text.to_lowercase().contains(needle),
                "event payload leaked {needle}"
            );
        }
        assert!(!text.contains(root_display.as_str()));
    }

    #[test]
    fn publish_v2_runs_full_integration_through_fakes() {
        let fixture = fixture(2);
        let configuration = configuration_from(&fixture.project_path);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
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

        assert_eq!(outcome.generation, "g-000001");
        let PublicationOutcome::Published(report) = &outcome.publication else {
            panic!("expected Published");
        };
        assert!(report.storage.is_some());
        assert!(report.repository.is_some());
        assert!(report.hosting.is_some());
        assert!(
            !storage.0.is_empty(),
            "storage must receive the download object"
        );
        assert!(
            repository.files.contains_key("index.html"),
            "template file must be committed"
        );
        assert_eq!(hosting.published.len(), 1);

        // Second identical publish over the same fixture: complete no-op.
        let outcome2 = publish_project(
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
            outcome2.publication,
            PublicationOutcome::NoChange { .. }
        ));
    }

    #[test]
    fn publish_event_sequence_is_deterministic() {
        let collect = || -> Vec<ApplicationEvent> {
            let fixture = fixture(2);
            let configuration = configuration_from(&fixture.project_path);
            let mut storage = FakeStorage::default();
            let mut repository = FakeRepository::default();
            let mut hosting = FakeHosting::default();
            let credentials = fake_credentials();
            let mut events: Vec<ApplicationEvent> = Vec::new();
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
                &mut |event| events.push(event),
            )
            .unwrap();
            events
        };

        let first = collect();
        // Structural shape: workflow steps envelope the operation lifecycle,
        // and every operation opens and closes exactly once.
        let shape = first
            .iter()
            .map(|event| match event {
                ApplicationEvent::EnteredStep(step) => format!("entered:{step:?}"),
                ApplicationEvent::LeftStep { step, ok } => format!("left:{step:?}:{ok}"),
                ApplicationEvent::Finished => "finished".to_owned(),
                ApplicationEvent::Failed => "failed".to_owned(),
                ApplicationEvent::Operation(operation) => {
                    use photo_publisher_integration::IntegrationEvent as I;
                    match operation {
                        I::StoragePutStarted { .. } => "op:StoragePutStarted",
                        I::StoragePutFinished { .. } => "op:StoragePutFinished",
                        I::StorageDeleteStarted { .. } => "op:StorageDeleteStarted",
                        I::StorageDeleteFinished { .. } => "op:StorageDeleteFinished",
                        I::RepositoryBatchStarted { .. } => "op:RepositoryBatchStarted",
                        I::RepositoryBatchFinished { .. } => "op:RepositoryBatchFinished",
                        I::HostingPublishStarted { .. } => "op:HostingPublishStarted",
                        I::HostingPublishFinished { .. } => "op:HostingPublishFinished",
                        I::StoragePutFailed { .. } => "op:StoragePutFailed",
                        I::StorageDeleteFailed { .. } => "op:StorageDeleteFailed",
                        I::RepositoryBatchFailed { .. } => "op:RepositoryBatchFailed",
                        I::HostingPublishFailed { .. } => "op:HostingPublishFailed",
                    }
                    .to_owned()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shape,
            vec![
                "entered:LoadProject",
                "entered:LocalPublication",
                "left:LocalPublication:true",
                "entered:BuildPlan",
                "left:BuildPlan:true",
                "entered:PublishIntegrate",
                "op:StoragePutStarted",
                "op:StoragePutFinished",
                "op:RepositoryBatchStarted",
                "op:RepositoryBatchFinished",
                "op:HostingPublishStarted",
                "op:HostingPublishFinished",
                "left:PublishIntegrate:true",
                "left:LoadProject:true",
                "finished",
            ]
        );
        // Two identical fresh runs produce identical full event streams
        // (including every operation payload).
        assert_eq!(first, collect());
    }

    #[test]
    fn publish_with_unicode_paths() {
        let fixture = fixture(2);
        assert!(fixture.project_path.to_string_lossy().contains("Meu Álbum"));
        let configuration = configuration_from(&fixture.project_path);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
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
        assert!(!hosting.published.is_empty());
    }

    #[test]
    fn publish_blocked_by_pending_ledger_entry_is_a_publication_error() {
        use photo_publisher_integration::OperationState;
        let fixture = fixture(2);
        let configuration = configuration_from(&fixture.project_path);
        let mut storage = FakeStorage::default();
        let mut repository = FakeRepository::default();
        let mut hosting = FakeHosting::default();
        let credentials = fake_credentials();
        let publish = |storage: &mut FakeStorage,
                       repository: &mut FakeRepository,
                       hosting: &mut FakeHosting| {
            publish_project(
                &fixture.project_path,
                Some(&configuration),
                PublishOptions,
                &mut PublicationProviders {
                    storage,
                    repository,
                    hosting,
                    credentials: &credentials,
                },
                &mut |_| {},
            )
        };
        publish(&mut storage, &mut repository, &mut hosting).unwrap();

        // Force the coordinator to refuse: an existing confirmed key becomes
        // Pending, so reconciliation requires human-class recovery.
        let ledger_path = fixture
            .root
            .path()
            .join("Meu Álbum/output/.publisher/integration-state.json");
        let mut ledger = IntegrationLedger::read_from(&ledger_path).unwrap();
        let key_str = ledger.r2_inventory.keys().next().unwrap().clone();
        let entry = ledger.r2_inventory[&key_str].clone();
        let key = ObjectKey::new(key_str).unwrap();
        let object = StorageObject {
            key: key.clone(),
            size_bytes: entry.size_bytes,
            sha256: entry.sha256,
            content_type: entry.content_type,
        };
        ledger
            .record_r2_object(&key, &object, OperationState::Pending)
            .unwrap();
        ledger.write_to(&ledger_path).unwrap();

        let storage_len = storage.0.len();
        let error = publish(&mut storage, &mut repository, &mut hosting).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::Publication);
        // No remote effect happened while blocked.
        assert_eq!(storage.0.len(), storage_len);

        // Errors/events never carry secrets.
        let text = format!("{error}");
        for needle in ["token", "secret", "password", "PHOTO_PUBLISHER_"] {
            assert!(!text.contains(needle), "leaked {needle} in error text");
        }
    }
}
