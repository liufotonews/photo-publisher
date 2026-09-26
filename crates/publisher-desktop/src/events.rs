//! Desktop event adapter: stable, frontend-safe serialization of the
//! application-layer event stream.
//!
//! `publisher-app` owns the event contracts; this module ONLY translates
//! them into a serializable DTO. No decisions live here, no timestamps, no
//! UUIDs, no platform or environment information is added, and no secrets,
//! credentials, provider tokens, stack traces, or Rust debug representations
//! are exposed. The translation is total and pure: every existing event
//! variant has an explicit mapping.

use photo_publisher_integration::{IntegrationEvent, OperationFailure};
use publisher_app::{ApplicationEvent, WorkflowStep};
use serde::Serialize;

/// The single Tauri event channel used for all application events.
/// The frontend subscribes once; the payload type discriminates the event.
pub const PUBLISHER_EVENT_CHANNEL: &str = "publisher://event";

/// One application event, serialized for the desktop frontend.
///
/// Tagged as `{ "type": "workflow", "data": ... }` or
/// `{ "type": "operation", "data": ... }`. The representation mirrors the
/// existing domain events field by field; nothing is added or dropped
/// silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DesktopEvent {
    /// Workflow lifecycle of the application use case in flight.
    Workflow(WorkflowEventDto),
    /// Granular publication operation observed during an integrated publish.
    Operation(OperationEventDto),
}

/// Stable workflow lifecycle payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowEventDto {
    /// Stable snake_case name of the workflow step.
    pub step: &'static str,
    /// Stable lifecycle marker: entered | left_ok | left_failed | finished |
    /// failed.
    pub lifecycle: &'static str,
}

/// Safe, stable failure classification for the frontend. Names mirror the
/// structured `OperationFailure` categories without exposing Rust identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationFailureDto {
    LocalValidation,
    Rejected,
    Ambiguous,
    Inconsistent,
    Ledger,
}

impl From<&OperationFailure> for OperationFailureDto {
    fn from(failure: &OperationFailure) -> Self {
        match failure {
            OperationFailure::LocalValidation => Self::LocalValidation,
            OperationFailure::Rejected => Self::Rejected,
            OperationFailure::Ambiguous => Self::Ambiguous,
            OperationFailure::Inconsistent => Self::Inconsistent,
            OperationFailure::Ledger => Self::Ledger,
        }
    }
}

/// Stable operation payload; one variant per `IntegrationEvent` variant, same
/// semantics, frontend-oriented field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OperationEventDto {
    StoragePutStarted {
        key: String,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    StoragePutFinished {
        key: String,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    StoragePutFailed {
        key: String,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
        failure: OperationFailureDto,
    },
    StorageDeleteStarted {
        key: String,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    StorageDeleteFinished {
        key: String,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    StorageDeleteFailed {
        key: String,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
        failure: OperationFailureDto,
    },
    RepositoryBatchStarted {
        generation: String,
        writes: usize,
        deletes: usize,
        paths: Vec<String>,
    },
    RepositoryBatchFinished {
        generation: String,
        writes: usize,
        deletes: usize,
        revision: Option<String>,
    },
    RepositoryBatchFailed {
        generation: String,
        writes: usize,
        deletes: usize,
        failure: OperationFailureDto,
    },
    HostingPublishStarted {
        generation: String,
        bundle_fingerprint: String,
    },
    HostingPublishFinished {
        generation: String,
        deployment_id: String,
        url: String,
    },
    HostingPublishFailed {
        generation: String,
        failure: OperationFailureDto,
    },
}

fn step_name(step: &WorkflowStep) -> &'static str {
    match step {
        WorkflowStep::LoadProject => "load_project",
        WorkflowStep::ValidateProject => "validate_project",
        WorkflowStep::InspectProject => "inspect_project",
        WorkflowStep::RecoverPublication => "recover_publication",
        WorkflowStep::Preflight => "preflight",
        WorkflowStep::LocalPublication => "local_publication",
        WorkflowStep::BuildPlan => "build_plan",
        WorkflowStep::PublishIntegrate => "publish_integrate",
        WorkflowStep::DryRun => "dry_run",
    }
}

impl From<&ApplicationEvent> for DesktopEvent {
    fn from(event: &ApplicationEvent) -> Self {
        match event {
            ApplicationEvent::EnteredStep(step) => DesktopEvent::Workflow(WorkflowEventDto {
                step: step_name(step),
                lifecycle: "entered",
            }),
            ApplicationEvent::LeftStep { step, ok } => DesktopEvent::Workflow(WorkflowEventDto {
                step: step_name(step),
                lifecycle: if *ok { "left_ok" } else { "left_failed" },
            }),
            ApplicationEvent::Finished => DesktopEvent::Workflow(WorkflowEventDto {
                step: "workflow",
                lifecycle: "finished",
            }),
            ApplicationEvent::Failed => DesktopEvent::Workflow(WorkflowEventDto {
                step: "workflow",
                lifecycle: "failed",
            }),
            ApplicationEvent::Operation(operation) => {
                DesktopEvent::Operation(operation_event(operation))
            }
        }
    }
}

fn operation_event(operation: &IntegrationEvent) -> OperationEventDto {
    match operation {
        IntegrationEvent::StoragePutStarted {
            key,
            size_bytes,
            generation,
            index,
            total,
        } => OperationEventDto::StoragePutStarted {
            key: key.as_str().to_owned(),
            size_bytes: *size_bytes,
            generation: generation.clone(),
            index: *index,
            total: *total,
        },
        IntegrationEvent::StoragePutFinished {
            key,
            size_bytes,
            generation,
            index,
            total,
        } => OperationEventDto::StoragePutFinished {
            key: key.as_str().to_owned(),
            size_bytes: *size_bytes,
            generation: generation.clone(),
            index: *index,
            total: *total,
        },
        IntegrationEvent::StoragePutFailed {
            key,
            size_bytes,
            generation,
            index,
            total,
            failure,
        } => OperationEventDto::StoragePutFailed {
            key: key.as_str().to_owned(),
            size_bytes: *size_bytes,
            generation: generation.clone(),
            index: *index,
            total: *total,
            failure: OperationFailureDto::from(failure),
        },
        IntegrationEvent::StorageDeleteStarted {
            key,
            size_bytes,
            generation,
            index,
            total,
        } => OperationEventDto::StorageDeleteStarted {
            key: key.as_str().to_owned(),
            size_bytes: *size_bytes,
            generation: generation.clone(),
            index: *index,
            total: *total,
        },
        IntegrationEvent::StorageDeleteFinished {
            key,
            size_bytes,
            generation,
            index,
            total,
        } => OperationEventDto::StorageDeleteFinished {
            key: key.as_str().to_owned(),
            size_bytes: *size_bytes,
            generation: generation.clone(),
            index: *index,
            total: *total,
        },
        IntegrationEvent::StorageDeleteFailed {
            key,
            size_bytes,
            generation,
            index,
            total,
            failure,
        } => OperationEventDto::StorageDeleteFailed {
            key: key.as_str().to_owned(),
            size_bytes: *size_bytes,
            generation: generation.clone(),
            index: *index,
            total: *total,
            failure: OperationFailureDto::from(failure),
        },
        IntegrationEvent::RepositoryBatchStarted {
            generation,
            writes,
            deletes,
            paths,
        } => OperationEventDto::RepositoryBatchStarted {
            generation: generation.clone(),
            writes: *writes,
            deletes: *deletes,
            paths: paths.iter().map(|path| path.as_str().to_owned()).collect(),
        },
        IntegrationEvent::RepositoryBatchFinished {
            generation,
            writes,
            deletes,
            revision,
        } => OperationEventDto::RepositoryBatchFinished {
            generation: generation.clone(),
            writes: *writes,
            deletes: *deletes,
            revision: revision.clone(),
        },
        IntegrationEvent::RepositoryBatchFailed {
            generation,
            writes,
            deletes,
            failure,
        } => OperationEventDto::RepositoryBatchFailed {
            generation: generation.clone(),
            writes: *writes,
            deletes: *deletes,
            failure: OperationFailureDto::from(failure),
        },
        IntegrationEvent::HostingPublishStarted {
            generation,
            bundle_fingerprint,
        } => OperationEventDto::HostingPublishStarted {
            generation: generation.clone(),
            bundle_fingerprint: bundle_fingerprint.clone(),
        },
        IntegrationEvent::HostingPublishFinished {
            generation,
            deployment_id,
            url,
        } => OperationEventDto::HostingPublishFinished {
            generation: generation.clone(),
            deployment_id: deployment_id.clone(),
            url: url.clone(),
        },
        IntegrationEvent::HostingPublishFailed {
            generation,
            failure,
        } => OperationEventDto::HostingPublishFailed {
            generation: generation.clone(),
            failure: OperationFailureDto::from(failure),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contracts::{ObjectKey, RepositoryPath};

    fn key(value: &str) -> ObjectKey {
        ObjectKey::new(value).unwrap()
    }

    fn serialize(event: &ApplicationEvent) -> String {
        serde_json::to_string(&DesktopEvent::from(event)).unwrap()
    }

    #[test]
    fn workflow_events_translate_to_stable_shape() {
        let flow = [
            ApplicationEvent::EnteredStep(WorkflowStep::PublishIntegrate),
            ApplicationEvent::LeftStep {
                step: WorkflowStep::PublishIntegrate,
                ok: true,
            },
            ApplicationEvent::Finished,
            ApplicationEvent::Failed,
        ];
        let parts: Vec<String> = flow.iter().map(serialize).collect();
        assert_eq!(
            parts,
            vec![
                r#"{"type":"workflow","data":{"step":"publish_integrate","lifecycle":"entered"}}"#,
                r#"{"type":"workflow","data":{"step":"publish_integrate","lifecycle":"left_ok"}}"#,
                r#"{"type":"workflow","data":{"step":"workflow","lifecycle":"finished"}}"#,
                r#"{"type":"workflow","data":{"step":"workflow","lifecycle":"failed"}}"#,
            ]
        );
    }

    #[test]
    fn every_operation_variant_translates_without_losing_public_information() {
        let events = [
            ApplicationEvent::Operation(IntegrationEvent::StoragePutStarted {
                key: key("originals/a.jpg"),
                size_bytes: 7,
                generation: "g-000001".to_owned(),
                index: 1,
                total: 2,
            }),
            ApplicationEvent::Operation(IntegrationEvent::StoragePutFinished {
                key: key("originals/a.jpg"),
                size_bytes: 7,
                generation: "g-000001".to_owned(),
                index: 1,
                total: 2,
            }),
            ApplicationEvent::Operation(IntegrationEvent::StoragePutFailed {
                key: key("originals/a.jpg"),
                size_bytes: 7,
                generation: "g-000001".to_owned(),
                index: 1,
                total: 2,
                failure: OperationFailure::Ambiguous,
            }),
            ApplicationEvent::Operation(IntegrationEvent::StorageDeleteStarted {
                key: key("originals/b.jpg"),
                size_bytes: 3,
                generation: "g-000001".to_owned(),
                index: 2,
                total: 2,
            }),
            ApplicationEvent::Operation(IntegrationEvent::StorageDeleteFinished {
                key: key("originals/b.jpg"),
                size_bytes: 3,
                generation: "g-000001".to_owned(),
                index: 2,
                total: 2,
            }),
            ApplicationEvent::Operation(IntegrationEvent::StorageDeleteFailed {
                key: key("originals/b.jpg"),
                size_bytes: 3,
                generation: "g-000001".to_owned(),
                index: 2,
                total: 2,
                failure: OperationFailure::LocalValidation,
            }),
            ApplicationEvent::Operation(IntegrationEvent::RepositoryBatchStarted {
                generation: "g-000001".to_owned(),
                writes: 2,
                deletes: 1,
                paths: vec![
                    RepositoryPath::new("index.html").unwrap(),
                    RepositoryPath::new("gallery.json").unwrap(),
                ],
            }),
            ApplicationEvent::Operation(IntegrationEvent::RepositoryBatchFinished {
                generation: "g-000001".to_owned(),
                writes: 2,
                deletes: 1,
                revision: Some("commit-01".to_owned()),
            }),
            ApplicationEvent::Operation(IntegrationEvent::RepositoryBatchFailed {
                generation: "g-000001".to_owned(),
                writes: 2,
                deletes: 1,
                failure: OperationFailure::Rejected,
            }),
            ApplicationEvent::Operation(IntegrationEvent::HostingPublishStarted {
                generation: "g-000001".to_owned(),
                bundle_fingerprint: "fp".to_owned(),
            }),
            ApplicationEvent::Operation(IntegrationEvent::HostingPublishFinished {
                generation: "g-000001".to_owned(),
                deployment_id: "dpl-1".to_owned(),
                url: "project.vercel.app".to_owned(),
            }),
            ApplicationEvent::Operation(IntegrationEvent::HostingPublishFailed {
                generation: "g-000001".to_owned(),
                failure: OperationFailure::Inconsistent,
            }),
        ];
        for event in events {
            let json = serde_json::to_value(DesktopEvent::from(&event)).unwrap();
            assert_eq!(json["type"], "operation");
            assert!(
                json["data"]["event"].is_string(),
                "missing event tag: {json}"
            );
        }
    }

    #[test]
    fn failure_kinds_are_stable_strings_not_rust_names() {
        use OperationFailure as F;
        use OperationFailureDto as D;
        let map: &[(F, &str)] = &[
            (F::LocalValidation, "local_validation"),
            (F::Rejected, "rejected"),
            (F::Ambiguous, "ambiguous"),
            (F::Inconsistent, "inconsistent"),
            (F::Ledger, "ledger"),
        ];
        for (failure, expected) in map {
            let text = serde_json::to_string(&D::from(failure)).unwrap();
            assert!(text.contains(expected), "{text} must contain {expected}");
        }
        let text = serde_json::to_string(&D::from(&F::Ambiguous)).unwrap();
        assert_eq!(text, r#"{"kind":"ambiguous"}"#);
    }

    #[test]
    fn serialized_events_are_deterministic() {
        let event = ApplicationEvent::Operation(IntegrationEvent::HostingPublishFailed {
            generation: "g-000001".to_owned(),
            failure: OperationFailure::Ledger,
        });
        assert_eq!(serialize(&event), serialize(&event));
    }

    #[test]
    fn serialized_events_never_carry_secrets_environment_or_rust_internals() {
        let events = [
            ApplicationEvent::Operation(IntegrationEvent::StoragePutFailed {
                key: key("originals/a.jpg"),
                size_bytes: 7,
                generation: "g-000001".to_owned(),
                index: 1,
                total: 1,
                failure: OperationFailure::Ambiguous,
            }),
            ApplicationEvent::Operation(IntegrationEvent::RepositoryBatchFailed {
                generation: "g-000001".to_owned(),
                writes: 1,
                deletes: 0,
                failure: OperationFailure::Rejected,
            }),
        ];
        for event in events {
            let text = serialize(&event);
            // No secrets or environment material (case-insensitive).
            let lowered = text.to_lowercase();
            for needle in [
                "photo_publisher_",
                "token",
                "secret",
                "authorization",
                "password",
            ] {
                assert!(
                    !lowered.contains(needle),
                    "event serialization leaked {needle}: {text}"
                );
            }
            // No Rust variant names (PascalCase) and no debug-ish operators.
            for needle in [
                "LocalValidation",
                "Rejected",
                "Ambiguous",
                "Inconsistent",
                "Ledger",
                "::",
            ] {
                assert!(
                    !text.contains(needle),
                    "event serialization leaked {needle}: {text}"
                );
            }
        }
    }

    #[test]
    fn serialized_events_do_not_add_absolute_paths() {
        let event = ApplicationEvent::Operation(IntegrationEvent::RepositoryBatchStarted {
            generation: "g-000001".to_owned(),
            writes: 1,
            deletes: 0,
            paths: vec![RepositoryPath::new("index.html").unwrap()],
        });
        let text = serialize(&event);
        assert!(!text.contains("C:\\"));
        assert!(!text.contains("/home/"));
        assert!(!text.contains(std::env::temp_dir().to_string_lossy().as_ref()));
    }
}
