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
}
