//! Application-layer result of a publication run.
//!
//! This enum enumerates facts the current architecture can actually produce.
//! It deliberately reuses the published types of `publisher-integration`
//! instead of parallel data structures, and it contains nothing speculative
//! for a future interface.

use photo_publisher_integration::{PublicationReport, ReconciliationRequirement};

/// The outcome of an application-level publication operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationOutcome {
    /// The publication completed and durably persisted the target state.
    Published(PublicationReport),
    /// Nothing had to change: the reconciliation plan was empty (idempotent).
    NoChange {
        /// Generation name read from the committed local publication.
        generation: String,
    },
    /// A reconciliation requirement blocks the operation; nothing remote was
    /// executed. Manual reconciliation is the sanctioned next step.
    Blocked {
        requirements: Vec<ReconciliationRequirement>,
    },
    /// The local publication needs recovery before it can be used.
    NeedsRecovery { reason: String },
}
