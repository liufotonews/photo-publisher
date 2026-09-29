//! PrepareLocalPublication: build the local gallery explicitly (Phase 7-K.2).
//!
//! Dry Run and Publish both plan against a *committed* local publication,
//! and until now only Publish created one — which left the first integrated
//! publish unreachable (Dry Run had nothing to plan from). This use case
//! closes that cycle without inventing a second publication: it resolves
//! the project exactly like Validate/Publish and then calls the very same
//! local-publication step Publish runs. The pipeline keeps owning staging,
//! journal, backup, recovery, and the execution lock; this layer only
//! orchestrates and reports.
//!
//! Provider-neutral and credential-free by construction: the operation is
//! exclusively local — no repository, storage, hosting, or credential
//! boundary is ever involved.

use std::path::Path;

use crate::errors::ApplicationError;
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};

/// Outcome of the local bootstrap: the project identity plus the committed
/// generation — the proof Dry Run plans against. Nothing presentational
/// beyond these public facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareLocalOutcome {
    pub project_id: String,
    pub project_name: String,
    /// Generation of the committed local publication (e.g. `g-000001`). A
    /// repeated run without source changes reports the same generation:
    /// the pipeline is a no-op when the committed state already matches.
    pub generation: String,
}

/// Builds the local publication through the exact step Publish uses.
///
/// Idempotent by inheritance: the pipeline scans and diffs before staging,
/// so a second run without source changes commits nothing new and reports
/// the existing generation. An interrupted previous run is healed by the
/// pipeline's own recovery — recovery stays where it already lives, and no
/// second execution lock is introduced (the pipeline's is the real one).
pub fn prepare_local_publication(
    project_path: &Path,
    events: &mut EventSink<'_>,
) -> Result<PrepareLocalOutcome, ApplicationError> {
    events(ApplicationEvent::EnteredStep(
        WorkflowStep::LocalPublication,
    ));
    let result = run(project_path);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::LocalPublication,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::LocalPublication,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(project_path: &Path) -> Result<PrepareLocalOutcome, ApplicationError> {
    // Resolve exactly like Validate/Publish: level-1 document, schema, and
    // the source directory — the preconditions the local pipeline needs.
    let handle = super::validate::validate_project_inner(project_path)?;
    // The same local-publication step Publish runs — never a second
    // implementation of it.
    super::publish::build_local_publication(&handle)?;
    let generation = super::publish::read_generation_shared(&handle)?;
    Ok(PrepareLocalOutcome {
        project_id: handle.project_id,
        project_name: handle.project_name,
        generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_jpeg(path: &Path, value: u8) {
        let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
        image.save(path).unwrap();
    }

    fn v1_document() -> serde_json::Value {
        serde_json::json!({
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
        })
    }

    fn write_v1_project(root: &Path) -> std::path::PathBuf {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_jpeg(&source.join("a.jpg"), 1);
        let path = root.join("project.json");
        std::fs::write(&path, serde_json::to_vec(&v1_document()).unwrap()).unwrap();
        path
    }

    #[test]
    fn first_prepare_creates_a_committed_publication_with_the_expected_events() {
        let root = tempdir().unwrap();
        let path = write_v1_project(root.path());
        let mut captured: Vec<ApplicationEvent> = Vec::new();
        let outcome = prepare_local_publication(&path, &mut |event| captured.push(event)).unwrap();
        assert_eq!(outcome.project_id, "app-test");
        assert_eq!(outcome.project_name, "App Test");
        assert_eq!(outcome.generation, "g-000001");
        assert!(root.path().join("output/gallery.json").is_file());
        assert_eq!(
            captured,
            vec![
                ApplicationEvent::EnteredStep(WorkflowStep::LocalPublication),
                ApplicationEvent::LeftStep {
                    step: WorkflowStep::LocalPublication,
                    ok: true
                },
                ApplicationEvent::Finished,
            ]
        );
    }

    #[test]
    fn a_second_prepare_without_source_changes_is_a_noop() {
        let root = tempdir().unwrap();
        let path = write_v1_project(root.path());
        let first = prepare_local_publication(&path, &mut |_| {}).unwrap();
        let gallery_before = std::fs::read(root.path().join("output/gallery.json")).unwrap();
        let second = prepare_local_publication(&path, &mut |_| {}).unwrap();
        // Same committed generation: the pipeline creates no new one when
        // the committed state already matches the source.
        assert_eq!(first.generation, second.generation);
        assert_eq!(second.generation, "g-000001");
        let gallery_after = std::fs::read(root.path().join("output/gallery.json")).unwrap();
        assert_eq!(gallery_before, gallery_after);
    }

    #[test]
    fn prepare_recovers_and_completes_an_interrupted_previous_run() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        write_jpeg(&source.join("a.jpg"), 1);
        let path = root.path().join("project.json");
        std::fs::write(&path, serde_json::to_vec(&v1_document()).unwrap()).unwrap();

        // First bootstrap commits the initial generation.
        prepare_local_publication(&path, &mut |_| {}).unwrap();

        // A second, interrupted run dies mid-replacement — the pipeline's
        // own fault-injection seam, the same one the recovery tests use.
        write_jpeg(&source.join("b.jpg"), 2);
        let interrupted = photo_publisher_pipeline::build_local_gallery_with_fault(
            &photo_publisher_pipeline::PipelineOptions {
                source_dir: source.clone(),
                output_dir: crate::project::resolve_output_dir(&path),
                project_title: "App Test".to_owned(),
            },
            Some(photo_publisher_pipeline::FaultPoint::AfterGalleryReplacement),
        );
        assert!(interrupted.is_err());

        // The bootstrap succeeds by reusing the pipeline recovery: the
        // committed publication now carries both photos, and the reported
        // generation is exactly the committed one.
        let outcome = prepare_local_publication(&path, &mut |_| {}).unwrap();
        assert!(!outcome.generation.is_empty());
        let gallery: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("output/gallery.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(gallery["photos"].as_array().map(Vec::len), Some(2));
        let committed =
            photo_publisher_integration::LocalPublication::from_output(root.path().join("output"))
                .unwrap();
        assert_eq!(outcome.generation, committed.generation);
    }

    #[test]
    fn prepare_failures_keep_the_classification_and_emit_failed() {
        let root = tempdir().unwrap();
        let missing = root.path().join("project.json");
        let mut captured: Vec<ApplicationEvent> = Vec::new();
        let error =
            prepare_local_publication(&missing, &mut |event| captured.push(event)).unwrap_err();
        assert_eq!(error.kind, crate::ApplicationErrorKind::ResourceMissing);
        assert_eq!(
            captured,
            vec![
                ApplicationEvent::EnteredStep(WorkflowStep::LocalPublication),
                ApplicationEvent::LeftStep {
                    step: WorkflowStep::LocalPublication,
                    ok: false
                },
                ApplicationEvent::Failed,
            ]
        );
        assert!(!captured.contains(&ApplicationEvent::Finished));
    }

    #[test]
    fn a_concurrent_execution_is_refused_by_the_pipeline_lock() {
        // The domain protection is the pipeline's own execution lock: with
        // the lock held, a second bootstrap is refused immediately — the
        // guard is not UI state.
        let root = tempdir().unwrap();
        let path = write_v1_project(root.path());
        let publisher_dir = root.path().join("output").join(".publisher");
        std::fs::create_dir_all(&publisher_dir).unwrap();
        std::fs::write(publisher_dir.join("run.lock"), b"").unwrap();
        let error = prepare_local_publication(&path, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, crate::ApplicationErrorKind::Publication);
        assert!(error
            .to_string()
            .contains("another publisher execution is active"));
        assert!(!root.path().join("output").join("gallery.json").exists());
    }

    #[test]
    fn prepare_works_the_same_way_for_v2_projects() {
        // The local bootstrap is the same operation for every schema
        // version; it is never artificially limited to v2.
        let root = tempdir().unwrap();
        let source = root.path().join("fotos");
        std::fs::create_dir_all(&source).unwrap();
        write_jpeg(&source.join("a.jpg"), 1);
        let path = root.path().join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "project": {"id": "joao-maria-2026", "name": "João & Maria"},
                "gallery": {"template": "editorial-v1", "title": "João & Maria", "bundlePath": "gallery-app"},
                "source": {"type": "folder", "path": "fotos"},
                "repository": {"provider": "github", "repository": "fotografo/joao-maria-2026", "branch": "main"},
                "hosting": {"provider": "vercel", "project": "joao-maria-2026"},
                "storage": {
                    "preview": {"provider": "github", "publicBaseUrl": "https://cdn.example.com"},
                    "highResolution": {"provider": "r2", "accountId": "a", "bucket": "b", "publicBaseUrl": "https://d.example.com"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let outcome = prepare_local_publication(&path, &mut |_| {}).unwrap();
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert_eq!(outcome.generation, "g-000001");
    }

    #[test]
    fn prepare_reuses_the_publishing_step_and_never_reaches_providers() {
        // Structural proof: the bootstrap delegates to the SAME local
        // publication step Publish runs, and never talks to providers,
        // credentials, the environment, or a second pipeline entry point.
        let source = include_str!("prepare_local.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "build_local_gallery",
            "build_local_gallery_with_fault",
            "reqwest",
            "CredentialStore",
            "photo_publisher_provider",
            "EnvironmentCredentialStore",
            "std::env",
            "TcpStream",
            "spawn_blocking",
            "async fn",
        ] {
            assert!(
                !source.contains(forbidden),
                "the local bootstrap must not reference {forbidden}"
            );
        }
        for required in [
            "build_local_publication(&handle)",
            "validate_project_inner(project_path)",
            "WorkflowStep::LocalPublication",
        ] {
            assert!(
                source.contains(required),
                "the local bootstrap must reuse {required}"
            );
        }
    }
}
