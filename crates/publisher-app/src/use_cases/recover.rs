//! RecoverPublication: run the publication journal recovery flow.

use std::path::Path;

use photo_publisher_pipeline::journal::JournalPhase;

use crate::errors::{ApplicationError, ApplicationErrorKind};
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};

/// The state produced by a recovery run, or the absence of prior work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryState {
    pub generation: String,
    pub phase: JournalPhase,
    /// True when the recovered journal is not yet committed.
    pub recovery_needed: bool,
}

/// Outcome of `RecoverPublication`: there may be no publication at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverOutcome {
    pub attempted: bool,
    pub state: Option<RecoveryState>,
    /// Project identity and resolved output directory — the same facts the
    /// CLI published before this migration; no UI semantics are carried.
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub source_declared: Option<String>,
    pub output_dir: std::path::PathBuf,
}

/// Executes the publication recovery flow for the project's output directory.
/// All journal mechanics stay in `publisher-pipeline`; this layer only
/// resolves the project and converts the outcome.
pub fn recover_publication(
    project_path: &Path,
    events: &mut EventSink<'_>,
) -> Result<RecoverOutcome, ApplicationError> {
    events(ApplicationEvent::EnteredStep(
        WorkflowStep::RecoverPublication,
    ));
    let result = run(project_path);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::RecoverPublication,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::RecoverPublication,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(project_path: &Path) -> Result<RecoverOutcome, ApplicationError> {
    // Resolve paths without doing pathological recovery itself.
    let project = crate::project::load_project_document(project_path)?;
    let output_dir = crate::project::resolve_output_dir(project_path);

    let recovered =
        photo_publisher_pipeline::recover_publication(&output_dir).map_err(|error| {
            ApplicationError::with_source(ApplicationErrorKind::Recovery, format!("{error}"), error)
        })?;

    Ok(RecoverOutcome {
        attempted: true,
        state: recovered.map(|record| {
            let recovery_needed = !matches!(record.phase, JournalPhase::Committed);
            RecoveryState {
                generation: record.generation,
                phase: record.phase,
                recovery_needed,
            }
        }),
        project_id: project["project"]["id"].as_str().map(str::to_owned),
        project_name: project["project"]["name"].as_str().map(str::to_owned),
        source_declared: project["source"]["path"].as_str().map(str::to_owned),
        output_dir,
    })
}
