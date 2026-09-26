//! InspectProject: read-only local publication information.

use std::path::Path;

use photo_publisher_core::load_state;
use photo_publisher_pipeline::journal::read_journal;

use crate::errors::{ApplicationError, ApplicationErrorKind};
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};
use crate::project::{load_project_document, resolve_output_dir, resolve_source_dir};

/// Summary of the active local publication, exactly as a surface needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSummary {
    pub generation: String,
    pub phase: photo_publisher_pipeline::journal::JournalPhase,
    pub recovery_pending: bool,
}

/// The outcome of an `InspectProject` use case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectOutcome {
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub source_declared: Option<String>,
    pub source_dir: std::path::PathBuf,
    pub output_dir: std::path::PathBuf,
    pub journal_present: bool,
    pub journal: Option<JournalSummary>,
    pub photo_count: Option<usize>,
}

impl InspectOutcome {
    pub fn recovery_pending(&self) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|journal| journal.recovery_pending)
    }
}

/// Reads the journal and state of the local publication. Never mutates.
pub fn inspect_project(
    project_path: &Path,
    events: &mut EventSink<'_>,
) -> Result<InspectOutcome, ApplicationError> {
    events(ApplicationEvent::EnteredStep(WorkflowStep::InspectProject));
    let result = run(project_path);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::InspectProject,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::InspectProject,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(project_path: &Path) -> Result<InspectOutcome, ApplicationError> {
    let project = load_project_document(project_path)?;
    let source_dir = resolve_source_dir(project_path, &project)?;
    let output_dir = resolve_output_dir(project_path);

    let journal_path = output_dir.join(".publisher/journal.json");
    let journal = if journal_path.is_file() {
        let record = read_journal(&journal_path).map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Recovery,
                format!("invalid journal: {error}"),
                error,
            )
        })?;
        Some(record)
    } else {
        None
    };

    let state_path = output_dir.join(".publisher/state.json");
    let photo_count = if state_path.is_file() {
        let state = load_state(&state_path).map_err(|error| {
            ApplicationError::with_source(
                ApplicationErrorKind::Validation,
                format!("invalid state: {error}"),
                error,
            )
        })?;
        Some(state.photos.len())
    } else {
        None
    };

    Ok(InspectOutcome {
        project_id: project["project"]["id"].as_str().map(str::to_owned),
        project_name: project["project"]["name"].as_str().map(str::to_owned),
        source_declared: project["source"]["path"].as_str().map(str::to_owned),
        source_dir,
        output_dir,
        journal_present: journal_path.is_file(),
        journal: journal.map(|record| {
            let recovery_pending = !matches!(
                record.phase,
                photo_publisher_pipeline::journal::JournalPhase::Committed
            );
            JournalSummary {
                generation: record.generation,
                phase: record.phase,
                recovery_pending,
            }
        }),
        photo_count,
    })
}
