//! Domain events emitted by application use cases.
//!
//! Events are plain data: no UI strings, no timestamps, no credentials, no
//! environment lookups. They are safe to observe from any interface (CLI /
//! future UI) and deterministic for a given execution.

use std::fmt;

use photo_publisher_integration::IntegrationEvent;

/// A named application workflow step, shared by every use case in this phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStep {
    LoadProject,
    ValidateProject,
    InspectProject,
    RecoverPublication,
    Preflight,
    Provisioning,
    LocalPublication,
    BuildPlan,
    PublishIntegrate,
    DryRun,
}

/// One infrastructure resource of a provisioning run, in the fixed
/// contractual order. Plain taxonomy only — identities, statuses, and
/// dispositions belong to the run report, never to an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningResource {
    Repository,
    Storage,
    Hosting,
    Domain,
}

/// An application event emitted while a use case runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationEvent {
    /// The use case entered a workflow step.
    EnteredStep(WorkflowStep),
    /// The use case left a workflow step, with an explicit success flag.
    LeftStep { step: WorkflowStep, ok: bool },
    /// The use case finished with a result; the result itself carries the data.
    Finished,
    /// The use case failed; the returned error carries the data.
    Failed,
    /// A granular, provider-neutral publication operation observed during
    /// `PublishIntegrate`. This forwards the integration-layer event exactly
    /// as produced: it never invents operations (a no-op publish emits none,
    /// and dry-run never produces any).
    Operation(IntegrationEvent),
    /// A granular provisioning operation started (Phase 7-K.5). Content-free
    /// infrastructure work: never files, never payloads, never identities.
    ProvisioningResourceStarted(ProvisioningResource),
    /// A granular provisioning operation finished, with an explicit success
    /// flag. A failed or ambiguous step is marked `ok: false` — never
    /// reported as started-only.
    ProvisioningResourceFinished {
        resource: ProvisioningResource,
        ok: bool,
    },
}

impl fmt::Display for ApplicationEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::EnteredStep(step) => format!("entered-step:{step:?}"),
            Self::LeftStep { step, ok } => format!("left-step:{step:?}:ok={ok}"),
            Self::Finished => "finished".to_owned(),
            Self::Failed => "failed".to_owned(),
            Self::Operation(event) => format!("operation:{event:?}"),
            Self::ProvisioningResourceStarted(resource) => {
                format!("provisioning-resource-started:{resource:?}")
            }
            Self::ProvisioningResourceFinished { resource, ok } => {
                format!("provisioning-resource-finished:{resource:?}:ok={ok}")
            }
        };
        formatter.write_str(&text)
    }
}

/// Sink for application events. Each use case calls it synchronously in order.
pub type EventSink<'a> = dyn FnMut(ApplicationEvent) + 'a;
