//! Preflight: a standalone, provider-aware *precondition* check (Phase 7-F).
//!
//! Preflight answers exactly one question: “does this project have
//! everything a publication attempt needs?” It never computes a plan (that
//! is the Dry Run's job), never writes a ledger or any remote resource
//! (Publish), and never contacts a provider or the network. Everything it
//! inspects is local: the project document (level 1, reused), the declared
//! configuration (level 2, reuse of Phase 7-D), the source folder's
//! availability (the existing path-resolution rule), and — for the
//! integrated flow only — the gallery template's availability (the existing
//! bundle-resolution rule, Phase 7-K.3) and the *configured bits* of the
//! allowlisted credentials (reuse of the Phase 7-E service).
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
    /// The gallery template the integrated publication composes from (the
    /// local bundle declared in `gallery.bundlePath`) is not available on
    /// this machine.
    TemplateUnavailable,
    /// A credential the integrated publication needs is not configured.
    CredentialMissing,
}

impl PreflightIssueCode {
    /// Stable machine name, safe for interfaces to match on.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Configuration(code) => code.as_str(),
            Self::SourceUnavailable => "source_unavailable",
            Self::TemplateUnavailable => "template_unavailable",
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

    // Template availability (Phase 7-K.3): the integrated (v2) publication
    // composes the publishable application from the local bundle the
    // document declares in `gallery.bundlePath`, so that resource must exist
    // before the flow continues. The check reuses the exact resolution and
    // loading rule Dry Run and Publish apply — resolve relative to the
    // project file, reject absolute/escaping paths, load as an application
    // bundle — and is strictly read-only: nothing is fetched, downloaded,
    // created, or provisioned. The v1 local publication is produced by the
    // local pipeline alone and never consumes a template directory, so no
    // template resource is required (or looked at) there.
    if setup.schema_version == 2 {
        if let Some(bundle_path) = setup
            .gallery
            .bundle_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
        {
            if photo_publisher_integration::ApplicationBundle::from_project_bundle(
                project_path,
                bundle_path,
            )
            .is_err()
            {
                issues.push(PreflightIssue {
                    code: PreflightIssueCode::TemplateUnavailable,
                    field: "gallery.bundlePath".to_owned(),
                    // The declared path is the user's own configuration —
                    // never a resolved absolute machine path.
                    message: format!(
                        "O template da galeria ('{bundle_path}') não está disponível neste computador."
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
        // A declared bundle path materializes as a minimal on-disk template:
        // preflight then sees exactly the resource the publication would
        // consume. Tests for its absence remove the directory explicitly.
        if let Some(bundle_path) = document["gallery"]["bundlePath"].as_str() {
            let directory = root.join(bundle_path);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("index.html"), b"<h1>template</h1>").unwrap();
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
    fn v2_with_a_missing_template_is_not_ready_and_reports_template_unavailable() {
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), true);
        // The declared bundle is not (or no longer) on this machine.
        std::fs::remove_dir_all(root.path().join("gallery-app")).unwrap();
        let store = fully_configured_store();
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        assert_eq!(codes(&outcome), ["template_unavailable"]);
        assert_eq!(outcome.issues[0].field, "gallery.bundlePath");
        // The message carries the declared path (the user's own
        // configuration) — never the resolved absolute machine path.
        assert!(outcome.issues[0].message.contains("'gallery-app'"));
        assert!(
            !outcome.issues[0]
                .message
                .contains(root.path().display().to_string().as_str()),
            "the resolved absolute path must never leak into the message"
        );
    }

    #[test]
    fn template_availability_joins_the_fixed_issue_order_of_the_preflight() {
        // No source folder, no bundle on disk, no credentials: every rule
        // fires exactly once — configuration issues first (none here), then
        // source availability, then template availability, then the
        // credential bits. The new rule inserts into the established order;
        // it never starts a parallel one.
        let root = tempdir().unwrap();
        let path = write_project(root.path(), &v2_document(), false);
        std::fs::remove_dir_all(root.path().join("gallery-app")).unwrap();
        let outcome = preflight_project(&path, &FakeStore::default(), &mut |_| {}).unwrap();
        assert!(!outcome.ready);
        let all = codes(&outcome);
        assert_eq!(all.first(), Some(&"source_unavailable"));
        assert_eq!(all.get(1), Some(&"template_unavailable"));
        assert_eq!(
            all.iter().skip(2).copied().collect::<Vec<_>>(),
            vec!["credential_missing"; 4]
        );
    }

    #[test]
    fn v1_never_requires_a_template_bundle_on_disk() {
        // The v1 local publication is produced by the local pipeline alone:
        // no template directory is resolved or read for it — even when the
        // document happens to declare a (never consumed) bundle path.
        let root = tempdir().unwrap();
        let mut document = v1_document();
        document["gallery"]["bundlePath"] = serde_json::Value::String("missing-bundle".to_owned());
        let path = write_project(root.path(), &document, true);
        std::fs::remove_dir_all(root.path().join("missing-bundle")).unwrap();
        let store = FakeStore::default();
        let outcome = preflight_project(&path, &store, &mut |_| {}).unwrap();
        assert!(outcome.ready, "issues: {:?}", outcome.issues);
        assert!(outcome.issues.is_empty());
    }

    /// Deterministic recursive snapshot of a project tree: every relative
    /// path plus, for files, its byte length.
    fn tree_snapshot(root: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, entries: &mut Vec<String>) {
            let mut listing: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
            listing.sort_by_key(|entry| entry.as_ref().unwrap().path());
            for entry in listing {
                let entry = entry.unwrap();
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let file_type = entry.file_type().unwrap();
                if file_type.is_dir() {
                    entries.push(format!("{relative}/"));
                    walk(root, &entry.path(), entries);
                } else {
                    let size = std::fs::read(entry.path()).unwrap().len();
                    entries.push(format!("{relative}:{size}"));
                }
            }
        }
        let mut entries = Vec::new();
        walk(root, root, &mut entries);
        entries
    }

    #[test]
    fn preflight_writes_nothing_whatever_the_template_state() {
        for template_present in [true, false] {
            let root = tempdir().unwrap();
            let path = write_project(root.path(), &v2_document(), true);
            if !template_present {
                std::fs::remove_dir_all(root.path().join("gallery-app")).unwrap();
            }
            let before = tree_snapshot(root.path());
            let outcome = preflight_project(&path, &fully_configured_store(), &mut |_| {}).unwrap();
            assert_eq!(outcome.ready, template_present);
            assert_eq!(
                before,
                tree_snapshot(root.path()),
                "preflight must be strictly read-only"
            );
            // No gallery, state, journal, ledger, staging, or backup is
            // ever produced: the local output directory is not even created.
            assert!(!root.path().join("output").exists());
        }
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
