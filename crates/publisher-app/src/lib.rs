//! Application layer for Photo Publisher.
//!
//! This crate is the seam between interfaces (the CLI today, a GUI later) and
//! the validated publication engine. It knows only domain types: it must not
//! know about the CLI arguments parser, any UI toolkit, or any concrete
//! provider. Publication planning, execution, recovery, and integrity rules
//! stay in `publisher-core`, `publisher-pipeline`, and `publisher-integration`.

pub mod errors;
pub mod events;
pub mod outcome;
pub mod project;
pub mod providers;
pub mod use_cases;

pub use errors::{ApplicationError, ApplicationErrorKind};
pub use events::{ApplicationEvent, WorkflowStep};
pub use outcome::PublicationOutcome;
pub use project::{
    load_project_document, resolve_output_dir, resolve_source_dir, ProjectHandle, ProjectKind,
};
pub use providers::PublicationProviders;
pub use use_cases::{
    dry_run_project, inspect_project, publish_project, recover_publication, validate_project,
    DryRunOutcome, InspectOutcome, JournalSummary, PublishOptions, PublishOutcome, RecoverOutcome,
    RecoveryState,
};
