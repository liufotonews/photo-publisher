//! ValidateProject: load + contract-validate + resolve paths (read-only).

use std::path::Path;

use crate::errors::{ApplicationError, ApplicationErrorKind};
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};
use crate::project::load_project_document;

pub use crate::project::ProjectHandle as ValidatedProject;

/// Loads a project document, validates it against the production schema, and
/// fully resolves its directories. No side effects; pure local reads only.
pub fn validate_project(
    project_path: &Path,
    events: &mut EventSink<'_>,
) -> Result<ValidatedProject, ApplicationError> {
    events(ApplicationEvent::EnteredStep(WorkflowStep::ValidateProject));
    let result = run(project_path);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::ValidateProject,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::ValidateProject,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(project_path: &Path) -> Result<ValidatedProject, ApplicationError> {
    validate_project_inner(project_path)
}

/// Validation logic without event emission, so `Publish` can run the same
/// checks inside its own EnteredStep lifecycle without duplicate events.
pub(crate) fn validate_project_inner(
    project_path: &Path,
) -> Result<ValidatedProject, ApplicationError> {
    let document = load_project_document(project_path)?;
    let handle = crate::project::resolve_handle_from_document(project_path, &document)?;
    if !handle.source_dir.is_dir() {
        return Err(ApplicationError::new(
            ApplicationErrorKind::ResourceMissing,
            format!(
                "source directory does not exist: {}",
                handle.source_dir.display()
            ),
        ));
    }
    Ok(handle)
}
