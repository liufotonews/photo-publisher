//! Preflight: a standalone, provider-aware *precondition* check (Phase 7-F).
//!
//! Preflight answers exactly one question: “does this project have
//! everything a publication attempt needs?” It never computes a plan (that
//! is the Dry Run's job), never writes a ledger or any remote resource
//! (Publish), and never contacts a provider or the network. Everything it
//! inspects is local: the project document (level 1, reused), the declared
//! configuration (level 2, reuse of Phase 7-D), the source folder's
//! availability (the existing path-resolution rule), and — for the
//! integrated flow only — the *configured bits* of the allowlisted
//! credentials (reuse of the Phase 7-E service).
//!
//! A credential is only ever read as "configured / not configured"; its
//! value never appears in an issue, an error, or the outcome.

use std::path::Path;

use photo_publisher_provider_contracts::CredentialStore;

use crate::config_validation::{validate_setup, ConfigurationIssueCode};
use crate::credentials::credential_status;
use crate::errors::ApplicationError;
use crate::events::{ApplicationEvent, EventSink, WorkflowStep};
use crate::project::resolve_source_dir;
use crate::setup::ProjectSetup;

/// Stable machine code of a preflight issue. Codes — never messages —
/// identify the diagnosis; the Phase 7-D codes are reused unchanged for
/// configuration problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightIssueCode {
    /// A configuration-rule diagnosis (Phase 7-D), carried verbatim.
    Configuration(ConfigurationIssueCode),
    /// The declared source folder does not exist on this machine.
    SourceUnavailable,
    /// A credential the integrated publication needs is not configured.
    CredentialMissing,
}

impl PreflightIssueCode {
    /// Stable machine name, safe for interfaces to match on.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Configuration(code) => code.as_str(),
            Self::SourceUnavailable => "source_unavailable",
            Self::CredentialMissing => "credential_missing",
        }
    }
}

/// One diagnosed precondition failure: stable code, schema-aligned field
/// path (a credential name is configuration, never a value), human message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightIssue {
    pub code: PreflightIssueCode,
    pub field: String,
    pub message: String,
}

/// The deterministic result of a preflight check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightOutcome {
    /// True only when every precondition holds.
    pub ready: bool,
    /// The document's declared schema version (1 = local-only, 2 =
    /// integrated); reported, never interpreted away.
    pub schema_version: u8,
    pub project_id: String,
    pub project_name: String,
    /// Every failure found, in a fixed rule order — preflight never stops at
    /// the first problem.
    pub issues: Vec<PreflightIssue>,
}

/// Runs the standalone preflight with the existing event lifecycle
/// (`WorkflowStep::Preflight`, already part of the shared vocabulary).
pub fn preflight_project(
    project_path: &Path,
    credentials: &dyn CredentialStore,
    events: &mut EventSink<'_>,
) -> Result<PreflightOutcome, ApplicationError> {
    events(ApplicationEvent::EnteredStep(WorkflowStep::Preflight));
    let result = run(project_path, credentials);
    match &result {
        Ok(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::Preflight,
                ok: true,
            });
            events(ApplicationEvent::Finished);
        }
        Err(_) => {
            events(ApplicationEvent::LeftStep {
                step: WorkflowStep::Preflight,
                ok: false,
            });
            events(ApplicationEvent::Failed);
        }
    }
    result
}

fn run(
    project_path: &Path,
    credentials: &dyn CredentialStore,
) -> Result<PreflightOutcome, ApplicationError> {
    // Level 1 (document + schema), loaded once through the existing path.
    let (setup, document) = ProjectSetup::load_with_document(project_path)?;

    // Level 2 (declared configuration), reused from Phase 7-D in its fixed
    // rule order — never duplicated here.
    let mut issues: Vec<PreflightIssue> = validate_setup(&setup)
        .issues
        .into_iter()
        .map(|issue| PreflightIssue {
            code: PreflightIssueCode::Configuration(issue.code),
            field: issue.field,
            message: issue.message,
        })
        .collect();

    // Source availability: only a *declared and coherent* folder source is
    // resolved (an incoherent or absent one is already a level-2 issue).
    // Resolution is the existing rule, never a second implementation.
    if document["source"]["type"].as_str() == Some("folder") {
        if let Some(declared) = document["source"]["path"]
            .as_str()
            .filter(|path| !path.trim().is_empty())
        {
            let source_dir = resolve_source_dir(project_path, &document)?;
            if !source_dir.is_dir() {
                issues.push(PreflightIssue {
                    code: PreflightIssueCode::SourceUnavailable,
                    field: "source.path".to_owned(),
                    // The declared path is the user's own configuration —
                    // never a resolved absolute machine path.
                    message: format!(
                        "A pasta de origem declarada ('{declared}') não existe neste computador."
                    ),
                });
            }
        }
    }

    // Credential bits: required only by the integrated (v2) publication. A
    // v1 project publishes locally and never needs a credential. Only the
    // configured bit is consulted — the backend is read through the shared
    // Phase 7-E service, never for its values.
    if setup.schema_version == 2 {
        for status in credential_status(credentials)? {
            if !status.configured {
                issues.push(PreflightIssue {
                    code: PreflightIssueCode::CredentialMissing,
                    field: status.name.to_owned(),
                    message: format!("A credencial '{}' não está configurada.", status.label),
                });
            }
        }
    }

    Ok(PreflightOutcome {
        ready: issues.is_empty(),
        schema_version: setup.schema_version,
        project_id: setup.project.id,
        project_name: setup.project.name,
        issues,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contracts::{ProviderError, ProviderResult};
    use std::collections::HashMap;
    use tempfile::tempdir;

    /// In-memory credential backend: no environment, no filesystem, no
    /// network — the only surface this use case may talk to.
    #[derive(Default)]
    struct FakeStore {
        values: HashMap<String, Vec<u8>>,
        failing: bool,
    }

    impl CredentialStore for FakeStore {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            if self.failing {
                return Err(ProviderError::Other);
            }
            Ok(self.values.get(name).cloned())
        }
        fn set(&mut self, _name: &str, _secret: &[u8]) -> ProviderResult<()> {
            Err(ProviderError::Unsupported)
        }
        fn delete(&mut self, _name: &str) -> ProviderResult<()> {
            Err(ProviderError::Unsupported)
        }
    }

    fn fully_configured_store() -> FakeStore {
        let mut store = FakeStore::default();
        for name in crate::credentials::SUPPORTED_CREDENTIALS {
            store.values.insert(name.to_owned(), b"x".to_vec());
        }
        store
    }

    fn v1_document() -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 1,
            "project": {"id": "joao-maria-2026", "name": "João & Maria"},
            "gallery": {"template": "editorial-v1", "title": "João & Maria"},
            "source": {"type": "folder", "path": "fotos"},
            "repository": {"provider": "local", "repository": "local"},
            "hosting": {"provider": "local"},
            "storage": {
                "preview": {"provider": "local"},
                "highResolution": {"provider": "local"}
            }
        })
    }

    fn v2_document() -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 2,
            "project": {"id": "joao-maria-2026", "name": "João & Maria"},
            "gallery": {"template": "editorial-v1", "title": "João & Maria", "bundlePath": "gallery-app"},
            "source": {"type": "folder", "path": "fotos"},
            "repository": {"provider": "github", "repository": "fotografo/joao-maria-2026", "branch": "main"},
            "hosting": {"provider": "vercel", "project": "joao-maria-2026", "teamId": "team_example"},
            "storage": {
                "preview": {"provider": "github", "prefix": "previews", "publicBaseUrl": "https://cdn.example.com/previews"},
                "highResolution": {"provider": "r2", "accountId": "account-example", "bucket": "fotografia", "publicBaseUrl": "https://downloads.example.com/originals"}
            }
        })
    }

    fn write_project(
        root: &Path,
        document: &serde_json::Value,
        with_source: bool,
    ) -> std::path::PathBuf {
        if with_source {
            std::fs::create_dir_all(root.join("fotos")).unwrap();
        }
        let path = root.join("project.json");
        std::fs::write(&path, serde_json::to_vec(document).unwrap()).unwrap();
        path
    }

    fn codes(outcome: &PreflightOutcome) -> Vec<&'static str> {
        outcome
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect()
    }

    #[test]
    fn v1_local_project_is_ready_without_any_credentials() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v1_document(), true);
        // An EMPTY credential backend must not matter for the local-only flow.
        let store = FakeStore::default();
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert!(outcome.ready, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 1);
        assert!(outcome.issues.is_empty());
    }

    #[test]
    fn v2_project_is_ready_when_everything_is_configured() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), true);
        let store = fully_configured_store();
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert!(outcome.ready, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 2);
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert!(outcome.issues.is_empty());
    }

    #[test]
    fn v2_reports_each_missing_credential_by_name_never_by_value() {
        for (position, missing) in [
            "github.token",
            "r2.access_key_id",
            "r2.secret_access_key",
            "vercel.token",
        ]
        .iter()
        .enumerate()
        {
            let mut store = fully_configured_store();
            store.values.remove(*missing);
            let root = tempdir().unwrap();
            let path = write_project(root.path(), &v2_document(), true);
            let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
            assert!(!outcome.ready);
            assert_eq!(codes(&outcome), ["credential_missing"]);
            assert_eq!(outcome.issues[0].field, *missing);
            // Sanitity of the stable credential order overall:
            let outcome_empty =
                preflight_project(&path, &FakeStore::default(), &mut |_| {}).unwrap();
            let empty_fields: Vec<&str> = outcome_empty
                .issues
                .iter()
                .map(|issue| issue.field.as_str())
                .collect();
            assert_eq!(empty_fields[position], *missing);
        }
    }

    #[test]
    fn v2_reports_all_missing_credentials_in_stable_order() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), true);
        let store = FakeStore::default();
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        assert_eq!(
            outcome
                .issues
                .iter()
                .map(|issue| issue.field.as_str())
                .collect::<Vec<_>>(),
            crate::credentials::SUPPORTED_CREDENTIALS
        );
        assert!(codes(&outcome)
            .iter()
            .all(|code| *code == "credential_missing"));
    }

    #[test]
    fn configuration_problems_and_missing_credentials_are_all_reported() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), true);
        let mut document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["hosting"]["provider"] = serde_json::Value::String("other-host".to_owned());
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        // No short circuit: the configuration issue AND every missing
        // credential are reported in one deterministic outcome.
        let outcome = preflight_project(&path, &FakeStore::default(), &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        let all = codes(&outcome);
        assert_eq!(all.first(), Some(&"unsupported_configuration"));
        assert_eq!(
            all.iter().filter(|c| **c == "credential_missing").count(),
            4
        );
    }

    #[test]
    fn a_declared_but_absent_source_is_unavailable_not_invalid() {
        let root = tempdir().unwrap();
        // The schema-valid document points at a folder that does not exist.
        let path = write_project(root.path(), &v1_document(), false);
        let store = FakeStore::default();
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        assert_eq!(codes(&outcome), ["source_unavailable"]);
        assert_eq!(outcome.issues[0].field, "source.path");
        assert!(outcome.issues[0].message.contains("'fotos'"));
        assert!(
            !outcome.issues[0]
                .message
                .contains(root.path().display().to_string().as_str()),
            "the resolved absolute path must never leak into the message"
        );
    }

    #[test]
    fn level_1_failures_keep_their_existing_classification() {
        let root = tempdir().unwrap();
        // Missing file.
        let missing = root.path().join("project.json");
        let store = FakeStore::default();
        let error = preflight_project(&missing, &store, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, crate::ApplicationErrorKind::ResourceMissing);
        // Schema-invalid document.
        let invalid = root.path().join("invalid.json");
        let mut document = v1_document();
        document["source"]["type"] = serde_json::Value::String("s3".to_owned());
        std::fs::write(&invalid, serde_json::to_vec(&document).unwrap()).unwrap();
        let error = preflight_project(&invalid, &store, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, crate::ApplicationErrorKind::ProjectInvalid);
    }

    #[test]
    fn preflight_is_deterministic() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), false);
        let store = FakeStore::default();
        let first = preflight_project(&path, &store, &mut |_| {}).unwrap();
        let second = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn preflight_never_ratchets_a_secret_into_any_surface() {
        let sentinel = b"TEST_SECRET_SHOULD_NEVER_ESCAPE";
        let mut store = FakeStore::default();
        store
            .values
            .insert("github.token".to_owned(), sentinel.to_vec());
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), true);
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        let rendered = format!("{outcome:?}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));

        // A failing credential backend surfaces a classified application
        // error — not the secret, not the provider payload.
        let failing = FakeStore {
            failing: true,
            ..FakeStore::default()
        };
        let error = preflight_project(&path, &failing, &mut |_| {}).unwrap_err();
        assert_eq!(error.kind, crate::ApplicationErrorKind::Internal);
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));
    }

    #[test]
    fn preflight_emits_the_existing_workflow_events() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v1_document(), true);
        let store = FakeStore::default();
        let mut captured: Vec<ApplicationEvent> = Vec::new();
        preflight_project(&path, &store, &mut |event| captured.push(event)).unwrap();
        assert_eq!(
            captured,
            vec![
                ApplicationEvent::EnteredStep(WorkflowStep::Preflight),
                ApplicationEvent::LeftStep {
                    step: WorkflowStep::Preflight,
                    ok: true
                },
                ApplicationEvent::Finished,
            ]
        );
    }

    #[test]
    fn preflight_never_builds_providers_or_touches_the_network() {
        // Structural proof: the use case knows only the CredentialStore
        // trait and local rules. Only the non-test part is scanned (the
        // token list itself lives in this test).
        let source = include_str!("preflight.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "preflight_publication",
            "build_providers",
            "photo_publisher_provider_github",
            "photo_publisher_provider_r2",
            "photo_publisher_provider_vercel",
            "std::env",
            "reqwest",
            "TcpStream",
            "HttpClient",
            ".publish(",
            ".put(",
            ".commit(",
            ".delete(",
            ".head(",
            ".exists(",
            "compute_plan",
            "dry_run",
        ] {
            assert!(
                !source.contains(forbidden),
                "preflight must not reference {forbidden}"
            );
        }
    }
}
