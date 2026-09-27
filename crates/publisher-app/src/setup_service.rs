//! Project Setup application service (Phase 7-B).
//!
//! Thin orchestration over the Phase 7-A contract: the service receives a
//! `ProjectSetup`, writes exactly one `project.json` via the contract itself,
//! and returns a small, stable outcome. It knows no providers, no network,
//! no `CredentialStore`, and no filesystem beyond the target file; it reads
//! no environment, timestamp, UUID, or global state.

use std::path::{Path, PathBuf};

use crate::{ApplicationError, ProjectSetup};

/// The handle produced after writing a project configuration.
///
/// Carries the same identity the caller already had; no secrets, no
/// credentials, no provider clients, no network responses, nothing extra.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSetupOutcome {
    pub project_id: String,
    pub project_name: String,
    /// The path the caller provided and that was written — returned as given.
    pub project_path: PathBuf,
}

/// Creates the project setup: a single `project.json` at `project_path`,
/// validated by the exact contract shared with the rest of the application.
///
/// The function is a pure orchestration of the existing contract: it neither
/// validates by second means nor writes through any other channel.
pub fn create_project_setup(
    project_path: &Path,
    setup: ProjectSetup,
) -> Result<ProjectSetupOutcome, ApplicationError> {
    setup.write(project_path)?;
    Ok(ProjectSetupOutcome {
        project_id: setup.project.id,
        project_name: setup.project.name,
        project_path: project_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApplicationErrorKind;
    use tempfile::tempdir;

    fn valid_v2_setup() -> ProjectSetup {
        // Minimal valid v2 document; the schema is the only authority.
        crate::setup::ProjectSetup {
            schema_version: 2,
            project: crate::setup::ProjectIdentity {
                id: "joao-maria-2026".to_owned(),
                name: "João & Maria".to_owned(),
                client: Some("João & Maria".to_owned()),
                date: Some("2026-09-20".to_owned()),
            },
            gallery: crate::setup::GallerySetup {
                template: "editorial-v1".to_owned(),
                title: "João & Maria".to_owned(),
                description: Some("Galeria de entrega.".to_owned()),
                bundle_path: Some("gallery-app".to_owned()),
            },
            source: Some(crate::setup::SourceSetup {
                kind: "folder".to_owned(),
                path: Some("fotos".to_owned()),
            }),
            repository: crate::setup::RepositorySetup {
                provider: "github".to_owned(),
                repository: "fotografo/joao-maria-2026".to_owned(),
                branch: Some("main".to_owned()),
            },
            hosting: crate::setup::HostingSetup {
                provider: "vercel".to_owned(),
                project: Some("joao-maria-2026".to_owned()),
                team_id: Some("team_example".to_owned()),
            },
            storage: crate::setup::StorageSetup {
                preview: crate::setup::StorageTargetSetup {
                    provider: "github".to_owned(),
                    bucket: None,
                    prefix: Some("previews".to_owned()),
                    account_id: None,
                    public_base_url: Some("https://cdn.example.com/previews".to_owned()),
                },
                high_resolution: crate::setup::StorageTargetSetup {
                    provider: "r2".to_owned(),
                    bucket: Some("fotografia".to_owned()),
                    prefix: Some("originals".to_owned()),
                    account_id: Some("account-example".to_owned()),
                    public_base_url: Some("https://downloads.example.com/originals".to_owned()),
                },
            },
            domain: Some(crate::setup::DomainSetup {
                url: "https://galeria.exemplo.com".to_owned(),
            }),
        }
    }

    #[test]
    fn create_writes_exactly_one_project_json() {
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        let outcome = create_project_setup(&target, valid_v2_setup()).unwrap();
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert_eq!(outcome.project_name, "João & Maria");
        assert_eq!(outcome.project_path, target);
        // Exactly one file — nothing else is written.
        assert!(target.is_file());
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "the service must not produce side files"
        );
    }

    #[test]
    fn created_document_round_trips_against_the_contract() {
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        let expected = valid_v2_setup();
        create_project_setup(&target, expected.clone()).unwrap();
        // The file we wrote must be loadable through the very same contract.
        let reloaded = ProjectSetup::load(&target).unwrap();
        assert_eq!(reloaded, expected);
    }

    #[test]
    fn missing_directory_is_a_resource_missing_error() {
        let root = tempdir().unwrap();
        let target = root.path().join("inexistente").join("project.json");
        let error = create_project_setup(&target, valid_v2_setup()).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ResourceMissing);
    }

    #[test]
    fn generic_io_failure_never_becomes_resource_missing() {
        // Same deterministic case as the Phase 7-A fix: the destination is an
        // existing directory, so the write must fail without misclassification.
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        std::fs::create_dir(&target).unwrap();
        let error = create_project_setup(&target, valid_v2_setup()).unwrap_err();
        assert!(matches!(error.kind, ApplicationErrorKind::Internal));
        let message = error.to_string();
        assert!(!message.contains(target.display().to_string().as_str()));
    }
}
