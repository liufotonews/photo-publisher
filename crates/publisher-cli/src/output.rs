//! Human-readable CLI presentation of application events.
//!
//! This module is deliberately presentation-only. Application and integration
//! events remain provider-neutral domain data; this formatter is the CLI's
//! human-facing view of those events.

use photo_publisher_integration::{IntegrationEvent, OperationFailure};
use publisher_app::{ApplicationEvent, WorkflowStep};

pub fn format_event(event: &ApplicationEvent) -> String {
    match event {
        ApplicationEvent::EnteredStep(step) => match step {
            WorkflowStep::LoadProject => "[LOAD] Iniciando projeto".to_owned(),
            WorkflowStep::ValidateProject => "[VALIDATE] Validando projeto".to_owned(),
            WorkflowStep::InspectProject => "[INSPECT] Inspecionando publicação".to_owned(),
            WorkflowStep::RecoverPublication => "[RECOVER] Verificando recuperação".to_owned(),
            WorkflowStep::Preflight => "[PREFLIGHT] Verificando configuração".to_owned(),
            WorkflowStep::LocalPublication => "[LOCAL] Construindo publicação local".to_owned(),
            WorkflowStep::BuildPlan => "[PLAN] Construindo plano".to_owned(),
            WorkflowStep::PublishIntegrate => "[PUBLISH] Executando publicação".to_owned(),
            WorkflowStep::DryRun => "[DRY-RUN] Calculando plano".to_owned(),
        },
        ApplicationEvent::LeftStep { step, ok } => {
            let label = match step {
                WorkflowStep::LoadProject => "LOAD",
                WorkflowStep::ValidateProject => "VALIDATE",
                WorkflowStep::InspectProject => "INSPECT",
                WorkflowStep::RecoverPublication => "RECOVER",
                WorkflowStep::Preflight => "PREFLIGHT",
                WorkflowStep::LocalPublication => "LOCAL",
                WorkflowStep::BuildPlan => "PLAN",
                WorkflowStep::PublishIntegrate => "PUBLISH",
                WorkflowStep::DryRun => "DRY-RUN",
            };
            if *ok {
                format!("[OK] {label} concluído")
            } else {
                format!("[ERROR] {label} falhou")
            }
        }
        ApplicationEvent::Finished => "[OK] Operação concluída".to_owned(),
        ApplicationEvent::Failed => "[ERROR] Operação falhou".to_owned(),
        ApplicationEvent::Operation(operation) => format_operation(operation),
    }
}

/// Stable human presentation of an operation failure.
///
/// The Rust variant names are never part of the CLI text: this explicit
/// mapping is the only place where the machine classification meets the
/// human message. An ambiguous outcome is always presented as "could not be
/// confirmed", never as concluded.
fn format_failure(failure: &OperationFailure) -> &'static str {
    match failure {
        OperationFailure::LocalValidation => "operação falhou na validação local",
        OperationFailure::Rejected => "operação foi rejeitada",
        OperationFailure::Ambiguous => "operação não pôde ser confirmada",
        OperationFailure::Inconsistent => "operação encontrou estado inconsistente",
        OperationFailure::Ledger => "operação foi impedida pelo estado consolidado",
    }
}

fn format_operation(event: &IntegrationEvent) -> String {
    match event {
        IntegrationEvent::StoragePutStarted {
            key, index, total, ..
        } => format!("[STORAGE] Upload {index}/{total}: {}", key.as_str()),
        IntegrationEvent::StoragePutFinished { key, .. } => {
            format!("[STORAGE] Concluído: {}", key.as_str())
        }
        IntegrationEvent::StoragePutFailed { key, failure, .. } => {
            format!(
                "[ERROR] STORAGE (upload): {}: {}",
                format_failure(failure),
                key.as_str()
            )
        }
        IntegrationEvent::StorageDeleteStarted {
            key, index, total, ..
        } => format!("[STORAGE] Removendo {index}/{total}: {}", key.as_str()),
        IntegrationEvent::StorageDeleteFinished { key, .. } => {
            format!("[STORAGE] Removido: {}", key.as_str())
        }
        IntegrationEvent::StorageDeleteFailed { key, failure, .. } => {
            format!(
                "[ERROR] STORAGE (remoção): {}: {}",
                format_failure(failure),
                key.as_str()
            )
        }
        IntegrationEvent::RepositoryBatchStarted {
            writes, deletes, ..
        } => {
            format!("[REPOSITORY] Preparando lote: {writes} arquivos escritos, {deletes} removidos")
        }
        IntegrationEvent::RepositoryBatchFinished { revision, .. } => match revision {
            Some(revision) => format!("[REPOSITORY] Commit concluído: {revision}"),
            None => "[REPOSITORY] Commit concluído".to_owned(),
        },
        IntegrationEvent::RepositoryBatchFailed { failure, .. } => {
            format!("[ERROR] REPOSITORY (lote): {}", format_failure(failure))
        }
        IntegrationEvent::HostingPublishStarted { .. } => "[HOSTING] Publicando galeria".to_owned(),
        IntegrationEvent::HostingPublishFinished {
            deployment_id, url, ..
        } => format!("[HOSTING] Deployment concluído: {deployment_id} ({url})"),
        IntegrationEvent::HostingPublishFailed { failure, .. } => {
            format!("[ERROR] HOSTING (deployment): {}", format_failure(failure))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_workflow_events_without_debug_contract() {
        assert_eq!(
            format_event(&ApplicationEvent::EnteredStep(WorkflowStep::LoadProject)),
            "[LOAD] Iniciando projeto"
        );
        assert_eq!(
            format_event(&ApplicationEvent::LeftStep {
                step: WorkflowStep::BuildPlan,
                ok: true,
            }),
            "[OK] PLAN concluído"
        );
    }

    #[test]
    fn formats_storage_progress() {
        let event = ApplicationEvent::Operation(IntegrationEvent::StoragePutStarted {
            key: photo_publisher_provider_contracts::ObjectKey::new("photos/a.jpg").unwrap(),
            size_bytes: 12,
            generation: "g-000001".to_owned(),
            index: 2,
            total: 7,
        });
        assert_eq!(format_event(&event), "[STORAGE] Upload 2/7: photos/a.jpg");
    }

    #[test]
    fn formats_ambiguous_failure_as_unconfirmed() {
        let event = ApplicationEvent::Operation(IntegrationEvent::StoragePutFailed {
            key: photo_publisher_provider_contracts::ObjectKey::new("photos/a.jpg").unwrap(),
            size_bytes: 12,
            generation: "g-000001".to_owned(),
            index: 1,
            total: 1,
            failure: photo_publisher_integration::OperationFailure::Ambiguous,
        });
        let text = format_event(&event);
        assert!(text.contains("não pôde ser confirmada"));
        // The Rust variant name and any success wording are never presented.
        assert!(!text.contains("Ambiguous"));
        assert!(!text.contains("Concluído"));
        assert!(!text.contains("concluído"));
    }

    #[test]
    fn every_operation_failure_has_a_stable_human_message() {
        use photo_publisher_integration::OperationFailure as F;
        let cases: &[(F, &str)] = &[
            (F::LocalValidation, "operação falhou na validação local"),
            (F::Rejected, "operação foi rejeitada"),
            (F::Ambiguous, "operação não pôde ser confirmada"),
            (F::Inconsistent, "operação encontrou estado inconsistente"),
            (F::Ledger, "operação foi impedida pelo estado consolidado"),
        ];
        for (failure, expected) in cases {
            assert_eq!(format_failure(failure), *expected);
        }
    }

    #[test]
    fn failure_lines_never_expose_rust_variant_names() {
        use photo_publisher_integration::OperationFailure as F;
        let key = photo_publisher_provider_contracts::ObjectKey::new("photos/a.jpg").unwrap();
        // Every failure position in the formatter, every variant.
        for failure in [
            F::LocalValidation,
            F::Rejected,
            F::Ambiguous,
            F::Inconsistent,
            F::Ledger,
        ] {
            let events = [
                IntegrationEvent::StoragePutFailed {
                    key: key.clone(),
                    size_bytes: 12,
                    generation: "g-000001".to_owned(),
                    index: 1,
                    total: 1,
                    failure,
                },
                IntegrationEvent::StorageDeleteFailed {
                    key: key.clone(),
                    size_bytes: 12,
                    generation: "g-000001".to_owned(),
                    index: 1,
                    total: 1,
                    failure,
                },
                IntegrationEvent::RepositoryBatchFailed {
                    generation: "g-000001".to_owned(),
                    writes: 2,
                    deletes: 0,
                    failure,
                },
                IntegrationEvent::HostingPublishFailed {
                    generation: "g-000001".to_owned(),
                    failure,
                },
            ];
            for event in events {
                let text = format_event(&ApplicationEvent::Operation(event));
                for rust_name in [
                    "LocalValidation",
                    "Rejected",
                    "Ambiguous",
                    "Inconsistent",
                    "Ledger",
                ] {
                    assert!(
                        !text.contains(rust_name),
                        "CLI output leaked the Rust variant name {rust_name}: {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn ambiguous_is_never_presented_as_concluded_in_any_failure_line() {
        use photo_publisher_integration::OperationFailure as F;
        let key = photo_publisher_provider_contracts::ObjectKey::new("photos/a.jpg").unwrap();
        let events = [
            IntegrationEvent::StoragePutFailed {
                key: key.clone(),
                size_bytes: 12,
                generation: "g-000001".to_owned(),
                index: 1,
                total: 1,
                failure: F::Ambiguous,
            },
            IntegrationEvent::StorageDeleteFailed {
                key,
                size_bytes: 12,
                generation: "g-000001".to_owned(),
                index: 1,
                total: 1,
                failure: F::Ambiguous,
            },
            IntegrationEvent::RepositoryBatchFailed {
                generation: "g-000001".to_owned(),
                writes: 1,
                deletes: 1,
                failure: F::Ambiguous,
            },
            IntegrationEvent::HostingPublishFailed {
                generation: "g-000001".to_owned(),
                failure: F::Ambiguous,
            },
        ];
        for event in events {
            let text = format_event(&ApplicationEvent::Operation(event));
            assert!(text.contains("não pôde ser confirmada"), "{text}");
            assert!(!text.contains("Concluído"), "{text}");
            assert!(!text.contains("concluído"), "{text}");
        }
    }

    #[test]
    fn failure_lines_carry_no_secrets_and_no_local_paths() {
        use photo_publisher_integration::OperationFailure as F;
        let key = photo_publisher_provider_contracts::ObjectKey::new("photos/a.jpg").unwrap();
        let event = IntegrationEvent::StoragePutFailed {
            key,
            size_bytes: 12,
            generation: "g-000001".to_owned(),
            index: 1,
            total: 1,
            failure: F::Ambiguous,
        };
        let text = format_event(&ApplicationEvent::Operation(event));
        for needle in ["token", "secret", "authorization", "password", "C:\\"] {
            assert!(
                !text.to_lowercase().contains(needle),
                "failure line leaked {needle}: {text}"
            );
        }
    }

    #[test]
    fn formatter_does_not_include_local_paths_or_secrets_from_event_data() {
        let event = ApplicationEvent::Operation(IntegrationEvent::HostingPublishStarted {
            generation: "g-000001".to_owned(),
            bundle_fingerprint: "abcdef".to_owned(),
        });
        let text = format_event(&event);
        assert!(!text.contains("C:\\"));
        assert!(!text.contains("token"));
        assert!(!text.contains("secret"));
    }
}
