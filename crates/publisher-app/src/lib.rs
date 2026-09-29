//! Application layer for Photo Publisher.
//!
//! This crate is the seam between interfaces (the CLI today, a GUI later) and
//! the validated publication engine. It knows only domain types: it must not
//! know about the CLI arguments parser, any UI toolkit, or any concrete
//! provider. Publication planning, execution, recovery, and integrity rules
//! stay in `publisher-core`, `publisher-pipeline`, and `publisher-integration`.

pub mod config_validation;
pub mod credentials;
pub mod errors;
pub mod events;
pub mod outcome;
pub mod project;
pub mod providers;
pub mod provisioning;
pub mod setup;
pub mod setup_service;
pub mod use_cases;

pub use config_validation::{
    validate_project_configuration, validate_setup, ConfigurationIssue, ConfigurationIssueCode,
    ConfigurationValidationOutcome,
};
pub use credentials::{
    credential_status, delete_credential, set_credential, CredentialStatus, SUPPORTED_CREDENTIALS,
};
pub use errors::{ApplicationError, ApplicationErrorKind};
pub use events::{ApplicationEvent, WorkflowStep};
pub use outcome::PublicationOutcome;
pub use project::{
    load_project_document, resolve_output_dir, resolve_source_dir, ProjectHandle, ProjectKind,
};
pub use providers::PublicationProviders;
pub use provisioning::{
    provision_project, ProvisionProjectReport, ProvisionedResource, ProvisioningFailure,
    ProvisioningProviders,
};
pub use setup::{
    DomainSetup, GallerySetup, HostingSetup, ProjectIdentity, ProjectSetup, RepositorySetup,
    SourceSetup, StorageSetup, StorageTargetSetup,
};
pub use setup_service::{create_project_setup, ProjectSetupOutcome};
pub use use_cases::{
    dry_run_project, inspect_project, preflight_project, prepare_local_publication,
    publish_project, recover_publication, validate_project, DryRunOutcome, InspectOutcome,
    JournalSummary, PreflightIssue, PreflightIssueCode, PreflightOutcome, PrepareLocalOutcome,
    PublishOptions, PublishOutcome, RecoverOutcome, RecoveryState,
};
