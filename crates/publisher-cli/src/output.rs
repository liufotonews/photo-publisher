//! Human-readable CLI presentation of application events.
//!
//! This module is deliberately presentation-only. Application and integration
//! events remain provider-neutral domain data; this formatter is the CLI's
//! human-facing view of those events.

use publisher_app::{ApplicationEvent, WorkflowStep};
use photo_publisher_integration::IntegrationEvent;

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

fn format_operation(event: &IntegrationEvent) -> String {
    match event {
        IntegrationEvent::StoragePutStarted {
            key, index, total, ..
        } => format!("[STORAGE] Upload {index}/{total}: {key_str}"),
        IntegrationEvent::StoragePutFinished { key, .. } => {
            format!("[STORAGE] Concluído: {}", key.as_str())
        }
        IntegrationEvent::StoragePutFailed {
            key, failure, ..
        } => format!(
            "[ERROR] STORAGE: upload não confirmado: {} ({failure:?})",
            key.as_str()
        ),
        IntegrationEvent::StorageDeleteStarted {
            key, index, total, ..
        } => format!("[STORAGE] Removendo {index}/{total}: {}", key.as_str()),
        IntegrationEvent::StorageDeleteFinished { key, .. } => {
            format!("[STORAGE] Removido: {}", key.as_str())
        }
        IntegrationEvent::StorageDeleteFailed {
            key, failure, ..
        } => format!(
            "[ERROR] STORAGE: remoção não confirmada: {} ({failure:?})",
            key.as_str()
        ),
        IntegrationEvent::RepositoryBatchStarted {
            writes, deletes, ..
        } => format!(
            "[REPOSITORY] Preparando lote: {writes} arquivos escritos, {deletes} removidos"
        ),
        IntegrationEvent::RepositoryBatchFinished { revision, .. } => match revision {
            Some(revision) => format!("[REPOSITORY] Commit concluído: {revision}"),
            None => "[REPOSITORY] Commit concluído".to_owned(),
        },
        IntegrationEvent::RepositoryBatchFailed { failure, .. } => {
            format!("[ERROR] REPOSITORY: lote não concluído ({failure:?})")
        }
        IntegrationEvent::HostingPublishStarted { .. } => {
            "[HOSTING] Publicando galeria".to_owned()
        }
        IntegrationEvent::HostingPublishFinished {
            deployment_id, url, ..
        } => format!("[HOSTING] Deployment concluído: {deployment_id} ({url})"),
        IntegrationEvent::HostingPublishFailed { failure, .. } => {
            format!("[ERROR] HOSTING: deployment não concluído ({failure:?})")
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
        assert_eq!(
            format_event(&event),
            "[STORAGE] Upload 2/7: photos/a.jpg"
        );
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
        assert!(text.contains("não confirmado"));
        assert!(text.contains("Ambiguous"));
        assert!(!text.contains("Concluído"));
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
