//! Application use cases.

pub mod dry_run;
pub mod inspect;
pub mod publish;
pub mod recover;
pub mod validate;

pub use dry_run::{dry_run_project, DryRunOutcome};
pub use inspect::{inspect_project, InspectOutcome, JournalSummary};
pub use publish::{publish_project, PublishOptions, PublishOutcome};
pub use recover::{recover_publication, RecoverOutcome, RecoveryState};
pub use validate::validate_project;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    use crate::events::ApplicationEvent;
    use crate::{ApplicationErrorKind, ProjectKind, PublicationOutcome, WorkflowStep};

    fn write_project(root: &Path, schema_version: u64) -> std::path::PathBuf {
        write_project_named(root, schema_version, "project.json")
    }

    fn write_project_named(root: &Path, schema_version: u64, filename: &str) -> std::path::PathBuf {
        let path = root.join(filename);
        let project = serde_json::json!({
            "schemaVersion": schema_version,
            "project": {"id": "app-test", "name": "App Test"},
            "gallery": {"template": "local", "title": "App Test"},
            "source": {"type": "folder", "path": "source"},
            "repository": {"provider": "local", "repository": "local"},
            "hosting": {"provider": "local"},
            "storage": {
                "preview": {"provider": "local"},
                "highResolution": {"provider": "local"}
            }
        });
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(&path, serde_json::to_vec(&project).unwrap()).unwrap();
        path
    }

    fn valid_project_with_source(root: &Path) -> std::path::PathBuf {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_project(root, 1)
    }

    #[derive(Default)]
    struct Capture(pub RefCell<Vec<ApplicationEvent>>);

    impl Capture {
        fn fire(&self) -> Vec<ApplicationEvent> {
            let mut sink = self.0.borrow_mut();
            std::mem::take(&mut *sink)
        }
    }

    #[test]
    fn validate_project_ok_v1() {
        let root = tempdir().unwrap();
        let project = valid_project_with_source(root.path());
        let capture = Capture::default();
        let mut sink = |event: ApplicationEvent| capture.0.borrow_mut().push(event);
        let result = validate_project(&project, &mut sink).unwrap();
        assert_eq!(result.kind, ProjectKind::Version1);
        assert!(result.source_dir.ends_with("source"));
        assert!(result.output_dir.ends_with("output"));
        assert_eq!(result.project_id, "app-test");
        assert_eq!(result.project_name, "App Test");
        assert_eq!(result.source_declared, "source");
        let events = capture.fire();
        assert_eq!(
            events[0],
            ApplicationEvent::EnteredStep(WorkflowStep::ValidateProject)
        );
        assert_eq!(
            events[1],
            ApplicationEvent::LeftStep {
                step: WorkflowStep::ValidateProject,
                ok: true
            }
        );
        assert_eq!(events[2], ApplicationEvent::Finished);
    }

    #[test]
    fn validate_project_ok_v2() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let project = {
            let path = root.path().join("project.json");
            let document = serde_json::json!({
                "schemaVersion": 2,
                "project": {"id": "app-test", "name": "App Test"},
                "gallery": {"template": "local", "title": "App Test", "bundlePath": "bundle"},
                "source": {"type": "folder", "path": "Títulos João"},
                "repository": {"provider": "github", "repository": "owner/repo"},
                "hosting": {"provider": "vercel", "project": "p"},
                "storage": {
                    "preview": {"provider": "github", "publicBaseUrl": "https://cdn.example.com"},
                    "highResolution": {"provider": "r2", "accountId": "a", "bucket": "b", "publicBaseUrl": "https://d.example.com"}
                }
            });
            std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            path
        };
        let source = root.path().join("Títulos João");
        std::fs::create_dir_all(&source).unwrap();
        let result = validate_project(&project, &mut |_| {}).unwrap();
        assert_eq!(result.kind, ProjectKind::Version2);
        assert_eq!(
            result.source_dir.file_name().unwrap().to_str().unwrap(),
            "Títulos João"
        );
    }

    #[test]
    fn validate_project_missing_file() {
        let root = tempdir().unwrap();
        let missing = root.path().join("missing.json");
        let error = validate_project(&missing, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ResourceMissing);
        assert!(error.message.contains("project file does not exist"));
    }

    #[test]
    fn validate_project_invalid_json() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(&path, b"{not json").unwrap();
        let error = validate_project(&path, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ProjectInvalid);
    }

    #[test]
    fn validate_project_schema_invalid() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("source")).unwrap();
        let path = root.path().join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 1,
                "project": {"id": "app-test", "name": "App Test"},
                "gallery": {"template": "local", "title": "App Test"},
                "source": {"type": "folder", "path": "source"},
                "unexpected": true
            }))
            .unwrap(),
        )
        .unwrap();
        let error = validate_project(&path, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ProjectInvalid);
    }

    #[test]
    fn validate_project_missing_source_dir() {
        let root = tempdir().unwrap();
        let project = write_project(root.path(), 1);
        let error = validate_project(&project, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ResourceMissing);
        assert!(error.message.contains("source directory does not exist"));
    }

    #[test]
    fn validate_project_unicode_and_spaces_path() {
        let root = tempdir().unwrap();
        let nested = root.path().join("Coleção João & Maria 2026");
        let source = nested.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let project = write_project(&nested, 1);
        let result = validate_project(&project, &mut |_| {}).unwrap();
        assert!(result
            .project_path
            .to_string_lossy()
            .contains("Coleção João & Maria 2026"));
    }

    #[test]
    fn validate_project_events_failed_on_schema_error() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(&path, b"not json at all").unwrap();
        let capture = Capture::default();
        let mut sink = |event: ApplicationEvent| capture.0.borrow_mut().push(event);
        let _ = validate_project(&path, &mut sink).unwrap_err();
        let events = capture.fire();
        assert_eq!(events.last(), Some(&ApplicationEvent::Failed));
    }

    fn write_jpeg(path: &std::path::Path, value: u8) {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
        image.save(path).unwrap();
    }

    fn publish_once(root: &Path) -> std::path::PathBuf {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_jpeg(&source.join("a.jpg"), 1);
        let project = write_project(root, 1);
        photo_publisher_pipeline::build_local_gallery(&photo_publisher_pipeline::PipelineOptions {
            source_dir: source,
            output_dir: crate::project::resolve_output_dir(&project),
            project_title: "App Test".to_owned(),
        })
        .unwrap();
        project
    }

    #[test]
    fn inspect_project_without_publication() {
        let root = tempdir().unwrap();
        let project = valid_project_with_source(root.path());
        let capture = Capture::default();
        let mut sink = |event: ApplicationEvent| capture.0.borrow_mut().push(event);
        let result = inspect_project(&project, &mut sink).unwrap();
        assert!(!result.journal_present);
        assert!(result.journal.is_none());
        assert_eq!(result.photo_count, None);
        assert!(!result.recovery_pending());
        let events = capture.fire();
        assert_eq!(
            events[0],
            ApplicationEvent::EnteredStep(WorkflowStep::InspectProject)
        );
    }

    #[test]
    fn inspect_project_with_committed_publication() {
        let root = tempdir().unwrap();
        let project = publish_once(root.path());
        let result = inspect_project(&project, &mut |_| {}).unwrap();
        assert!(result.journal_present);
        let journal = result.journal.unwrap();
        assert_eq!(journal.generation, "g-000001");
        assert_eq!(
            journal.phase,
            photo_publisher_pipeline::journal::JournalPhase::Committed
        );
        assert!(!journal.recovery_pending);
        assert_eq!(result.photo_count, Some(1));
    }

    #[test]
    fn inspect_project_with_corrupt_journal() {
        let root = tempdir().unwrap();
        let project = publish_once(root.path());
        let journal = crate::project::resolve_output_dir(&project).join(".publisher/journal.json");
        std::fs::write(&journal, b"{corrupt").unwrap();
        let error = inspect_project(&project, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::Recovery);
    }

    #[test]
    fn inspect_project_with_corrupt_state() {
        let root = tempdir().unwrap();
        let project = publish_once(root.path());
        let state = crate::project::resolve_output_dir(&project).join(".publisher/state.json");
        std::fs::write(&state, b"{corrupt").unwrap();
        let error = inspect_project(&project, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::Validation);
    }

    #[test]
    fn recover_publication_without_prior_publication() {
        let root = tempdir().unwrap();
        let project = valid_project_with_source(root.path());
        let result = recover_publication(&project, &mut |_| {}).unwrap();
        assert!(result.attempted);
        assert!(result.state.is_none());
    }

    #[test]
    fn recover_publication_after_committed_publication_is_not_needed() {
        let root = tempdir().unwrap();
        let project = publish_once(root.path());
        let result = recover_publication(&project, &mut |_| {}).unwrap();
        let state = result.state.unwrap();
        assert_eq!(state.generation, "g-000001");
        assert_eq!(
            state.phase,
            photo_publisher_pipeline::journal::JournalPhase::Committed
        );
        assert!(!state.recovery_needed);
    }

    #[test]
    fn recover_publication_handles_interrupted_publication() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_jpeg(&source.join("a.jpg"), 1);
        let project = write_project(root.path(), 1);
        // First publication completes.
        photo_publisher_pipeline::build_local_gallery(&photo_publisher_pipeline::PipelineOptions {
            source_dir: source.clone(),
            output_dir: crate::project::resolve_output_dir(&project),
            project_title: "App Test".to_owned(),
        })
        .unwrap();
        // Second publication is interrupted while replacing the gallery:
        // recovery must roll back to the committed previous generation.
        write_jpeg(&source.join("b.jpg"), 2);
        let interrupted = photo_publisher_pipeline::build_local_gallery_with_fault(
            &photo_publisher_pipeline::PipelineOptions {
                source_dir: source,
                output_dir: crate::project::resolve_output_dir(&project),
                project_title: "App Test".to_owned(),
            },
            Some(photo_publisher_pipeline::FaultPoint::AfterGalleryReplacement),
        );
        assert!(interrupted.is_err());

        let result = recover_publication(&project, &mut |_| {}).unwrap();
        let state = result.state.unwrap();
        assert_eq!(state.generation, "g-000002");
        assert_eq!(
            state.phase,
            photo_publisher_pipeline::journal::JournalPhase::RolledBack
        );
        assert!(state.recovery_needed);
    }

    #[test]
    fn recover_publication_emits_events() {
        let root = tempdir().unwrap();
        let project = publish_once(root.path());
        let capture = Capture::default();
        let mut sink = |event: ApplicationEvent| capture.0.borrow_mut().push(event);
        let _ = recover_publication(&project, &mut sink).unwrap();
        let events = capture.fire();
        assert_eq!(
            events[0],
            ApplicationEvent::EnteredStep(WorkflowStep::RecoverPublication)
        );
        assert_eq!(
            events[1],
            ApplicationEvent::LeftStep {
                step: WorkflowStep::RecoverPublication,
                ok: true
            }
        );
        assert_eq!(events[2], ApplicationEvent::Finished);
    }

    #[test]
    fn publication_outcome_variants_exist_without_invented_state() {
        let empty: Vec<photo_publisher_integration::ReconciliationRequirement> = Vec::new();
        let _blocked = PublicationOutcome::Blocked {
            requirements: empty,
        };
        let _nochange = PublicationOutcome::NoChange {
            generation: "g-000001".to_owned(),
        };
        let _recovery = PublicationOutcome::NeedsRecovery {
            reason: "journal not committed".to_owned(),
        };
    }

    #[test]
    fn error_message_preserves_cause_and_never_contains_secrets() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(&path, b"garbage").unwrap();
        let error = validate_project(&path, &mut |_| {}).unwrap_err();
        assert!(error.source_error().is_some());
        let display = format!("{error}");
        let debug = format!("{error:?}");
        for text in [display, debug] {
            assert!(!text.to_lowercase().contains("token"));
            assert!(!text.to_lowercase().contains("secret"));
            assert!(!text.to_lowercase().contains("password"));
        }
    }

    #[test]
    fn events_are_deterministic_for_repeated_run() {
        let root = tempdir().unwrap();
        let project = valid_project_with_source(root.path());
        let collect = || {
            let capture = Capture::default();
            let mut sink = |event: ApplicationEvent| capture.0.borrow_mut().push(event);
            let _ = validate_project(&project, &mut sink).unwrap();
            capture.fire()
        };
        assert_eq!(collect(), collect());
    }
}
