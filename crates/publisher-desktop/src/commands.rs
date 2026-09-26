//! Desktop application commands (the Tauri-facing application bridge).
//!
//! These functions are pure adapters: they validate input shapes, delegate to
//! `publisher-app` use cases, and convert results into small serializable
//! types. They contain no business rules, never touch providers, credentials,
//! the network, or the ledger, and never write to the filesystem.
//!
//! The functions deliberately carry no `#[tauri::command]` macro: the library
//! stays free of the Tauri runtime (so tests link nothing webview-related),
//! and the desktop binary registers thin wrappers for each of them.

use publisher_app::events::EventSink;
use serde::Serialize;

/// Non-sensitive application identity, derived from the crate itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// Application identity for the desktop shell. No paths, no host info, no
/// environment variables — only the two public product facts.
pub fn get_app_info() -> AppInfo {
    AppInfo {
        name: "Photo Publisher",
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// The error surface of the desktop command layer: the stable application
/// classification plus a human message. Never a stack trace, never secrets,
/// never raw internal error types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandError {
    pub kind: String,
    pub message: String,
}

impl From<publisher_app::ApplicationError> for CommandError {
    fn from(error: publisher_app::ApplicationError) -> Self {
        Self {
            kind: error.kind.as_str().to_owned(),
            message: error.to_string(),
        }
    }
}

/// Successful validation outcome of a `project.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateProjectOutcome {
    pub valid: bool,
    pub project: ProjectSummary,
}

/// Public project identity facts published by the application layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    /// "v1" (local-only) or "v2" (integrated publication).
    pub kind: &'static str,
}

/// Validates a `project.json` through the application layer.
///
/// This command performs no preflight of providers and reads no credentials:
/// validation is intentionally independent of any publication setup, so it
/// works on a machine without GitHub/R2/Vercel configuration. The caller
/// provides the event sink (the desktop binary forwards it to the Tauri
/// event channel); nothing is emitted unless the caller does so.
pub fn validate_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<ValidateProjectOutcome, CommandError> {
    let trimmed = project_path.trim();
    if trimmed.is_empty() {
        return Err(CommandError {
            kind: publisher_app::ApplicationErrorKind::ProjectInvalid
                .as_str()
                .to_owned(),
            message: "project path is required".to_owned(),
        });
    }
    let handle = publisher_app::validate_project(std::path::Path::new(trimmed), events)?;
    Ok(ValidateProjectOutcome {
        valid: true,
        project: ProjectSummary {
            id: handle.project_id,
            name: handle.project_name,
            kind: match handle.kind {
                publisher_app::ProjectKind::Version1 => "v1",
                publisher_app::ProjectKind::Version2 => "v2",
            },
        },
    })
}

fn check_path(project_path: &str) -> Result<std::path::PathBuf, CommandError> {
    let trimmed = project_path.trim();
    if trimmed.is_empty() {
        return Err(CommandError {
            kind: publisher_app::ApplicationErrorKind::ProjectInvalid
                .as_str()
                .to_owned(),
            message: "project path is required".to_owned(),
        });
    }
    Ok(std::path::PathBuf::from(trimmed))
}

/// Publication outcome for the desktop shell: a stable, serializable view of
/// the application outcome. Counts only — never internal types, never
/// secrets, never provider details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishOutcomeDto {
    /// Stable outcome name: "published" | "no_change" | "blocked" |
    /// "needs_recovery".
    pub outcome: &'static str,
    /// Generation of the committed local publication.
    pub generation: String,
    /// Storage summary, present only when remote storage work was confirmed.
    pub storage: Option<OperationCountsDto>,
    /// Repository summary, present only when a commit was confirmed.
    pub repository: Option<RepositoryPublishDto>,
    /// Hosting summary, present only when a deployment was confirmed.
    pub hosting: Option<HostingPublishDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationCountsDto {
    pub written: usize,
    pub deleted: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryPublishDto {
    pub written: usize,
    pub deleted: usize,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostingPublishDto {
    pub deployment_id: String,
    pub url: String,
}

/// Dry-run outcome for the desktop shell: counts of the reconciliation plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DryRunOutcomeDto {
    pub generation: String,
    pub storage_operations: usize,
    pub repository_operations: usize,
    pub hosting_operations: usize,
    pub reconciliation_requirements: usize,
}

/// Publishes the project through the application layer.
///
/// The desktop layer only: (1) reads the validated document, (2) runs the
/// existing publication preflight for v2 projects via the desktop composition
/// root, (3) delegates the orchestration to `publisher_app::publish_project`
/// with the composed providers. No business rule lives here.
pub fn publish_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<PublishOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let document = publisher_app::load_project_document(&path)?;
    let is_v2 = document["schemaVersion"].as_u64() == Some(2);

    // Providers exist only for the duration of this invocation, and only for
    // schema-v2 publications.
    let composed = if is_v2 {
        let configuration = crate::composition::preflight_publication(&document, &path)?;
        let providers = crate::composition::build_providers(&configuration)?;
        Some((configuration, providers))
    } else {
        None
    };

    let outcome = match composed {
        Some((configuration, mut providers)) => {
            let mut publication_providers = providers.as_publication_providers();
            publisher_app::publish_project(
                &path,
                Some(&configuration),
                publisher_app::PublishOptions,
                &mut publication_providers,
                events,
            )?
        }
        None => {
            // Schema v1 is local-only: the application layer never calls the
            // providers at all, so the composition root provides an inert set.
            let mut inert = crate::composition::NoopProviders::new();
            publisher_app::publish_project(
                &path,
                None,
                publisher_app::PublishOptions,
                &mut inert.as_providers(),
                events,
            )?
        }
    };
    Ok(publish_dto(&outcome))
}

fn publish_dto(outcome: &publisher_app::PublishOutcome) -> PublishOutcomeDto {
    let publish_outcome_name;
    let mut storage = None;
    let mut repository = None;
    let mut hosting = None;
    match &outcome.publication {
        publisher_app::PublicationOutcome::Published(report) => {
            publish_outcome_name = "published";
            storage = report.storage.as_ref().map(|report| OperationCountsDto {
                written: report.uploaded.len(),
                deleted: report.deleted.len(),
            });
            repository = report
                .repository
                .as_ref()
                .map(|report| RepositoryPublishDto {
                    written: report.written.len(),
                    deleted: report.deleted.len(),
                    revision: report.revision.clone(),
                });
            hosting = report.hosting.as_ref().map(|report| HostingPublishDto {
                deployment_id: report
                    .deployment
                    .as_ref()
                    .map(|deployment| deployment.id.clone())
                    .unwrap_or_default(),
                url: report
                    .deployment
                    .as_ref()
                    .map(|deployment| deployment.url.clone())
                    .unwrap_or_default(),
            });
        }
        publisher_app::PublicationOutcome::NoChange { .. } => {
            publish_outcome_name = "no_change";
        }
        publisher_app::PublicationOutcome::Blocked { .. } => {
            publish_outcome_name = "blocked";
        }
        publisher_app::PublicationOutcome::NeedsRecovery { .. } => {
            publish_outcome_name = "needs_recovery";
        }
    }
    PublishOutcomeDto {
        outcome: publish_outcome_name,
        generation: outcome.generation.clone(),
        storage,
        repository,
        hosting,
    }
}

/// Plans the integrated publication without performing it.
///
/// Delegates to `publisher_app::dry_run_project`, which by construction
/// cannot construct providers, write the ledger, or touch the network.
pub fn dry_run_project(
    project_path: &str,
    events: &mut EventSink<'_>,
) -> Result<DryRunOutcomeDto, CommandError> {
    let path = check_path(project_path)?;
    let outcome = publisher_app::dry_run_project(&path, events)?;
    Ok(DryRunOutcomeDto {
        generation: outcome.generation,
        storage_operations: outcome.storage_operations,
        repository_operations: outcome.repository_operations,
        hosting_operations: outcome.hosting_operations,
        reconciliation_requirements: outcome.reconciliation_requirements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_project(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root.join("source")).unwrap();
        let path = root.join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "project": {"id": "app-test", "name": "App Test"},
                "gallery": {"template": "local", "title": "App Test"},
                "source": {"type": "folder", "path": "source"},
                "repository": {"provider": "local", "repository": "local"},
                "hosting": {"provider": "local"},
                "storage": {
                    "preview": {"provider": "local"},
                    "highResolution": {"provider": "local"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn get_app_info_exposes_only_public_product_facts() {
        let info = get_app_info();
        assert_eq!(info.name, "Photo Publisher");
        assert_eq!(info.version, "0.1.0");
        let json = serde_json::to_value(&info).unwrap();
        let text = json.to_string();
        for forbidden in [
            std::env::temp_dir().to_string_lossy().as_ref(),
            "PHOTO_PUBLISHER_",
            "token",
            "secret",
        ] {
            assert!(!text.contains(forbidden), "get_app_info leaked {forbidden}");
        }
    }

    #[test]
    fn validate_project_accepts_a_valid_fixture() {
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        let outcome = validate_project(project.to_str().unwrap(), &mut |_| {}).unwrap();
        assert!(outcome.valid);
        assert_eq!(outcome.project.id, "app-test");
        assert_eq!(outcome.project.name, "App Test");
        assert_eq!(outcome.project.kind, "v1");
    }

    #[test]
    fn validate_project_reports_invalid_projects_without_internals() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(&path, b"{not json").unwrap();
        let error = validate_project(path.to_str().unwrap(), &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert!(!error.message.contains("panicked"));
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["kind"], "project_invalid");
    }

    #[test]
    fn validate_project_rejects_empty_paths_deterministically() {
        let error = validate_project("   ", &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, "project_invalid");
        assert_eq!(error.message, "project path is required");
    }

    #[test]
    fn validate_project_never_needs_credentials() {
        // Validation never consults the environment: the command runs
        // identically regardless of any PHOTO_PUBLISHER_* configuration.
        // (No mutation here — environment changes race with the composition
        // tests in the same process; see tests/bootstrap.rs for the structural
        // guarantee that no credential lookup happens at all.)
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        validate_project(project.to_str().unwrap(), &mut |_| {}).unwrap();
    }

    #[test]
    fn validate_project_emits_events_without_changing_the_outcome() {
        let root = tempdir().unwrap();
        let project = write_project(root.path());
        let mut captured: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let outcome =
            validate_project(project.to_str().unwrap(), &mut |event| captured.push(event)).unwrap();
        // Same result regardless of whether events are observed.
        assert!(outcome.valid);
        assert_eq!(outcome.project.id, "app-test");
        assert_eq!(outcome.project.kind, "v1");
        // The use case emitted its workflow lifecycle; the adapter observed it.
        assert_eq!(
            captured.first(),
            Some(&publisher_app::ApplicationEvent::EnteredStep(
                publisher_app::WorkflowStep::ValidateProject
            ))
        );
        assert_eq!(
            captured.last(),
            Some(&publisher_app::ApplicationEvent::Finished)
        );
    }

    fn write_jpeg(path: &Path, value: u8) {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
        image.save(path).unwrap();
    }

    fn write_v1_project_with_photo(root: &Path) -> std::path::PathBuf {
        let project = write_project(root);
        write_jpeg(&root.join("source").join("a.jpg"), 1);
        project
    }

    #[test]
    fn publish_v1_publishes_locally_without_providers_or_credentials() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let outcome = publish_project(project.to_str().unwrap(), &mut |_| {}).unwrap();
        assert_eq!(outcome.outcome, "no_change");
        assert_eq!(outcome.generation, "g-000001");
        assert!(outcome.storage.is_none());
        assert!(outcome.repository.is_none());
        assert!(outcome.hosting.is_none());
    }

    #[test]
    fn dry_run_v1_returns_a_stable_application_error() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let error = dry_run_project(project.to_str().unwrap(), &mut |_| {}).unwrap_err();
        // v1 projects have no integrated publication configuration.
        assert_eq!(error.kind, "project_invalid");
        assert!(!error.message.contains("PHOTO_PUBLISHER_"));
        assert!(!error.message.contains("panicked"));
    }

    #[test]
    fn dry_run_emits_zero_operation_events() {
        let root = tempdir().unwrap();
        let project = write_v1_project_with_photo(root.path());
        let mut captured: Vec<publisher_app::ApplicationEvent> = Vec::new();
        let _ = dry_run_project(project.to_str().unwrap(), &mut |event| captured.push(event));
        assert!(
            !captured
                .iter()
                .any(|event| matches!(event, publisher_app::ApplicationEvent::Operation(_))),
            "dry-run must never emit operation events"
        );
    }

    #[test]
    fn publish_dto_serializes_deterministically() {
        let dto = PublishOutcomeDto {
            outcome: "no_change",
            generation: "g-000001".to_owned(),
            storage: None,
            repository: None,
            hosting: None,
        };
        assert_eq!(
            serde_json::to_string(&dto).unwrap(),
            serde_json::to_string(&dto).unwrap()
        );
        let json = serde_json::to_string(&dto).unwrap();
        for needle in ["PHOTO_PUBLISHER_", "token", "secret", "password", "C:\\"] {
            assert!(!json.contains(needle), "DTO leaked {needle}: {json}");
        }
    }
}
