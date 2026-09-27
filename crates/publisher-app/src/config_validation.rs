//! Configuration validation (Phase 7-D).
//!
//! A `project.json` is evaluated at three distinct levels:
//!
//! 1. **Schema validity** — the embedded contract enforced at load time by
//!    `ProjectSetup::load`. A document that fails here is `ProjectInvalid`
//!    and this module never diagnoses it further.
//! 2. **Configuration coherence** — THIS module: whether the *declared*
//!    values are coherent for the publication flow they enable. Example the
//!    schema cannot express: provider fields are free strings in the schema,
//!    but this version of the application only supports a fixed provider
//!    matrix per role.
//! 3. **External availability** — credentials, network reachability, remote
//!    resources: a future preflight. Not here, by design.
//!
//! Level 2 never opens a network connection, never constructs a provider,
//! never reads a credential or any environment variable, and never touches
//! the filesystem beyond the `project.json` load itself. Every rule is a
//! pure function over the decoded [`ProjectSetup`], so the outcome is fully
//! deterministic: same document, same issues, same order.
//!
//! The provider matrix diagnosed here mirrors the one the composition roots
//! enforce at publication time (`enforce_supported_providers` in the CLI and
//! the desktop); this module only *reports* — enforcement before any remote
//! effect stays where it already exists.

use std::path::Path;

use crate::setup::ProjectSetup;
use crate::ApplicationError;

/// Stable machine code of a configuration issue. Codes — never messages —
/// identify the diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationIssueCode {
    InvalidSource,
    InvalidRepository,
    InvalidHosting,
    InvalidPreviewStorage,
    InvalidHighResolutionStorage,
    InvalidDomain,
    UnsupportedConfiguration,
}

impl ConfigurationIssueCode {
    /// Stable machine name, safe for interfaces to match on.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidSource => "invalid_source_configuration",
            Self::InvalidRepository => "invalid_repository_configuration",
            Self::InvalidHosting => "invalid_hosting_configuration",
            Self::InvalidPreviewStorage => "invalid_preview_storage_configuration",
            Self::InvalidHighResolutionStorage => "invalid_high_resolution_storage_configuration",
            Self::InvalidDomain => "invalid_domain_configuration",
            Self::UnsupportedConfiguration => "unsupported_configuration",
        }
    }
}

/// One diagnosed incoherence: stable code, schema-aligned field path, and a
/// human message. No secrets, no absolute paths, no timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationIssue {
    pub code: ConfigurationIssueCode,
    /// camelCase field path of the schema, e.g. `storage.highResolution.bucket`.
    pub field: String,
    pub message: String,
}

/// The deterministic result of a configuration validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationValidationOutcome {
    pub valid: bool,
    /// The document's declared schema version (1 = local-only, 2 = integrated);
    /// the version is reported, never migrated.
    pub schema_version: u8,
    pub project_id: String,
    pub project_name: String,
    /// Every issue found, in a fixed rule order — validation never stops at
    /// the first problem.
    pub issues: Vec<ConfigurationIssue>,
}

/// The provider matrix supported by this version of the publication flow,
/// per role. Mirrored from the composition roots, which remain the
/// enforcement point (this module only diagnoses).
const REPOSITORY_PROVIDER: &str = "github";
const PREVIEW_PROVIDER: &str = "github";
const HIGH_RESOLUTION_PROVIDER: &str = "r2";
const HOSTING_PROVIDER: &str = "vercel";

/// Loads the project (level 1: file + schema, unchanged) and diagnoses the
/// declared configuration (level 2). Level-1 failures keep their existing
/// classification; this function adds no new error category.
pub fn validate_project_configuration(
    project_path: &Path,
) -> Result<ConfigurationValidationOutcome, ApplicationError> {
    let setup = ProjectSetup::load(project_path)?;
    Ok(validate_setup(&setup))
}

/// The pure core of configuration validation: a function of the decoded
/// contract alone. No I/O of any kind happens here, which is what makes the
/// outcome deterministic.
pub fn validate_setup(setup: &ProjectSetup) -> ConfigurationValidationOutcome {
    let mut issues = Vec::new();

    // --- Rules for every schema version -----------------------------------
    //
    // Source: checked only when declared (the schema keeps the section
    // optional). A declared source is consumed by the local pipeline, which
    // requires a folder with a path — mirroring `resolve_source_dir`.
    if let Some(source) = &setup.source {
        if source.kind != "folder" {
            issues.push(issue(
                ConfigurationIssueCode::InvalidSource,
                "source.type",
                "A fonte do projeto deve ser uma pasta local ('folder').",
            ));
        }
        match source.path.as_deref() {
            Some(path) if !path.trim().is_empty() => {}
            _ => issues.push(issue(
                ConfigurationIssueCode::InvalidSource,
                "source.path",
                "A fonte declarada precisa do caminho da pasta de fotos finais.",
            )),
        }
    }

    // --- Rules of the integrated publication (schema v2) -------------------
    //
    // v1 is the local-only contract: its repository/hosting/storage values
    // are inert declarations that no publication step consumes, so no rule
    // here applies to them. Nothing is migrated or upgraded silently.
    if setup.schema_version == 2 {
        validate_v2(setup, &mut issues);
    }

    // Domain: when declared, its URL is a public address of the published
    // gallery — the same structural rule the publication layer applies to
    // every public base URL. No DNS, no network: structure only.
    if let Some(domain) = &setup.domain {
        if photo_publisher_integration::validate_public_base_url(domain.url.trim()).is_err() {
            issues.push(issue(
                ConfigurationIssueCode::InvalidDomain,
                "domain.url",
                "O domínio deve ser uma URL pública absoluta (http/https), sem credenciais, query ou fragmento.",
            ));
        }
    }

    ConfigurationValidationOutcome {
        valid: issues.is_empty(),
        schema_version: setup.schema_version,
        project_id: setup.project.id.clone(),
        project_name: setup.project.name.clone(),
        issues,
    }
}

/// Level-2 rules of the integrated publication. Deeper checks of a role run
/// only when the role's provider is the supported one: an unsupported
/// provider is diagnosed once, without cascading noise.
fn validate_v2(setup: &ProjectSetup, issues: &mut Vec<ConfigurationIssue>) {
    // Repository.
    if setup.repository.provider != REPOSITORY_PROVIDER {
        issues.push(unsupported(
            "repository.provider",
            &setup.repository.provider,
            "repositório",
            REPOSITORY_PROVIDER,
        ));
    } else if !is_owner_slash_name(setup.repository.repository.trim()) {
        issues.push(issue(
            ConfigurationIssueCode::InvalidRepository,
            "repository.repository",
            "O repositório deve estar no formato 'dono/nome' (ex.: estudio/joao-maria-2026).",
        ));
    }

    // Hosting.
    if setup.hosting.provider != HOSTING_PROVIDER {
        issues.push(unsupported(
            "hosting.provider",
            &setup.hosting.provider,
            "hospedagem",
            HOSTING_PROVIDER,
        ));
    } else {
        if setup
            .hosting
            .project
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            issues.push(issue(
                ConfigurationIssueCode::InvalidHosting,
                "hosting.project",
                "O projeto de hospedagem é obrigatório para a publicação integrada.",
            ));
        }
        // Mirrors `HostingPublicationConfig::new`: a present team id must not
        // be blank.
        if setup
            .hosting
            .team_id
            .as_deref()
            .is_some_and(|team| team.trim().is_empty())
        {
            issues.push(issue(
                ConfigurationIssueCode::InvalidHosting,
                "hosting.teamId",
                "O identificador da equipe de hospedagem não pode estar em branco.",
            ));
        }
    }

    // Preview storage.
    if setup.storage.preview.provider != PREVIEW_PROVIDER {
        issues.push(unsupported(
            "storage.preview.provider",
            &setup.storage.preview.provider,
            "armazenamento de previews",
            PREVIEW_PROVIDER,
        ));
    } else {
        match setup.storage.preview.public_base_url.as_deref() {
            Some(url) if photo_publisher_integration::validate_public_base_url(url.trim()).is_ok() => {}
            Some(_) => issues.push(issue(
                ConfigurationIssueCode::InvalidPreviewStorage,
                "storage.preview.publicBaseUrl",
                "A URL pública de previews deve ser absoluta (http/https) e não pode ser um endereço local ou interno.",
            )),
            None => issues.push(issue(
                ConfigurationIssueCode::InvalidPreviewStorage,
                "storage.preview.publicBaseUrl",
                "A URL pública de previews é obrigatória para a publicação integrada.",
            )),
        }
    }

    // High-resolution storage.
    let high = &setup.storage.high_resolution;
    if high.provider != HIGH_RESOLUTION_PROVIDER {
        issues.push(unsupported(
            "storage.highResolution.provider",
            &high.provider,
            "armazenamento de alta resolução",
            HIGH_RESOLUTION_PROVIDER,
        ));
    } else {
        if high
            .account_id
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            issues.push(issue(
                ConfigurationIssueCode::InvalidHighResolutionStorage,
                "storage.highResolution.accountId",
                "A conta do armazenamento de alta resolução é obrigatória para a publicação integrada.",
            ));
        }
        if high
            .bucket
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
        {
            issues.push(issue(
                ConfigurationIssueCode::InvalidHighResolutionStorage,
                "storage.highResolution.bucket",
                "O bucket do armazenamento de alta resolução é obrigatório para a publicação integrada.",
            ));
        }
        match high.public_base_url.as_deref() {
            Some(url) if photo_publisher_integration::validate_public_base_url(url.trim()).is_ok() => {}
            Some(_) => issues.push(issue(
                ConfigurationIssueCode::InvalidHighResolutionStorage,
                "storage.highResolution.publicBaseUrl",
                "A URL pública de alta resolução deve ser absoluta (http/https) e não pode ser um endereço local ou interno.",
            )),
            None => issues.push(issue(
                ConfigurationIssueCode::InvalidHighResolutionStorage,
                "storage.highResolution.publicBaseUrl",
                "A URL pública de alta resolução é obrigatória para a publicação integrada.",
            )),
        }
    }
}

fn unsupported(field: &str, actual: &str, role: &str, expected: &str) -> ConfigurationIssue {
    issue(
        ConfigurationIssueCode::UnsupportedConfiguration,
        field,
        // The declared provider value is configuration data (never a secret).
        &format!(
            "Provider de {role} não suportado: '{actual}'. Suportado nesta versão: '{expected}'."
        ),
    )
}

fn issue(code: ConfigurationIssueCode, field: &str, message: &str) -> ConfigurationIssue {
    ConfigurationIssue {
        code,
        field: field.to_owned(),
        message: message.to_owned(),
    }
}

/// The `owner/name` shape the repository adapter expects — the same rule the
/// composition root applies when building the provider.
fn is_owner_slash_name(value: &str) -> bool {
    match value.split_once('/') {
        Some((owner, name)) => !owner.is_empty() && !name.is_empty() && !name.contains('/'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::{
        DomainSetup, GallerySetup, HostingSetup, ProjectIdentity, RepositorySetup, SourceSetup,
        StorageSetup, StorageTargetSetup,
    };
    use crate::ApplicationErrorKind;
    use tempfile::tempdir;

    /// A schema-valid, fully coherent v2 setup.
    fn coherent_v2() -> ProjectSetup {
        ProjectSetup {
            schema_version: 2,
            project: ProjectIdentity {
                id: "joao-maria-2026".to_owned(),
                name: "João & Maria".to_owned(),
                client: None,
                date: None,
            },
            gallery: GallerySetup {
                template: "editorial-v1".to_owned(),
                title: "João & Maria".to_owned(),
                description: None,
                bundle_path: Some("gallery-app".to_owned()),
            },
            source: Some(SourceSetup {
                kind: "folder".to_owned(),
                path: Some("fotos".to_owned()),
            }),
            repository: RepositorySetup {
                provider: "github".to_owned(),
                repository: "fotografo/joao-maria-2026".to_owned(),
                branch: Some("main".to_owned()),
            },
            hosting: HostingSetup {
                provider: "vercel".to_owned(),
                project: Some("joao-maria-2026".to_owned()),
                team_id: None,
            },
            storage: StorageSetup {
                preview: StorageTargetSetup {
                    provider: "github".to_owned(),
                    bucket: None,
                    prefix: Some("previews".to_owned()),
                    account_id: None,
                    public_base_url: Some("https://cdn.example.com/previews".to_owned()),
                },
                high_resolution: StorageTargetSetup {
                    provider: "r2".to_owned(),
                    bucket: Some("fotografia".to_owned()),
                    prefix: Some("originals".to_owned()),
                    account_id: Some("account-example".to_owned()),
                    public_base_url: Some("https://downloads.example.com/originals".to_owned()),
                },
            },
            domain: None,
        }
    }

    fn codes(outcome: &ConfigurationValidationOutcome) -> Vec<&'static str> {
        outcome
            .issues
            .iter()
            .map(|issue| issue.code.as_str())
            .collect()
    }

    #[test]
    fn coherent_v2_configuration_is_valid_without_issues() {
        let outcome = validate_setup(&coherent_v2());
        assert!(outcome.valid);
        assert!(outcome.issues.is_empty());
        assert_eq!(outcome.schema_version, 2);
        assert_eq!(outcome.project_id, "joao-maria-2026");
        assert_eq!(outcome.project_name, "João & Maria");
    }

    #[test]
    fn the_canonical_v2_fixture_is_a_valid_configuration() {
        // The contract's own published v2 example must stay coherent. It has
        // no `source` section: an undeclared source is not a level-2 issue.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/valid/project.v2.valid.json");
        let outcome = validate_project_configuration(&path).unwrap();
        assert!(outcome.valid, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 2);
    }

    #[test]
    fn the_v1_local_only_fixture_remains_valid_without_migration() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/valid/project.valid.json");
        let outcome = validate_project_configuration(&path).unwrap();
        assert!(outcome.valid, "issues: {:?}", outcome.issues);
        assert_eq!(outcome.schema_version, 1);
        assert_eq!(outcome.project_id, "joao-maria-2026");
    }

    #[test]
    fn v1_with_arbitrary_provider_names_still_validates() {
        // v1 is local-only: provider names are inert declarations. Publishing
        // a v1 project never builds a provider, so no matrix rule may fire.
        let mut setup = coherent_v2();
        setup.schema_version = 1;
        setup.gallery.bundle_path = None;
        setup.hosting.project = None;
        setup.storage.preview.public_base_url = None;
        setup.storage.high_resolution.account_id = None;
        setup.storage.high_resolution.bucket = None;
        setup.storage.high_resolution.public_base_url = None;
        setup.repository.provider = "local".to_owned();
        setup.hosting.provider = "local".to_owned();
        setup.storage.preview.provider = "local".to_owned();
        setup.storage.high_resolution.provider = "local".to_owned();
        let outcome = validate_setup(&setup);
        assert!(outcome.valid, "issues: {:?}", outcome.issues);
    }

    #[test]
    fn unknown_v2_provider_is_diagnosed_never_panics() {
        let mut setup = coherent_v2();
        setup.repository.provider = "gitlab".to_owned();
        let outcome = validate_setup(&setup);
        assert!(!outcome.valid);
        // Exactly one issue: deeper repository checks do not cascade onto an
        // unknown provider.
        assert_eq!(codes(&outcome), ["unsupported_configuration"]);
        assert_eq!(outcome.issues[0].field, "repository.provider");
        assert!(outcome.issues[0].message.contains("'gitlab'"));
        assert!(outcome.issues[0].message.contains("'github'"));
    }

    #[test]
    fn every_role_of_the_v2_matrix_is_checked() {
        let mut setup = coherent_v2();
        setup.hosting.provider = "other-host".to_owned();
        setup.storage.preview.provider = "s3".to_owned();
        setup.storage.high_resolution.provider = "s3".to_owned();
        let outcome = validate_setup(&setup);
        assert_eq!(
            codes(&outcome),
            [
                "unsupported_configuration", // hosting.provider
                "unsupported_configuration", // storage.preview.provider
                "unsupported_configuration", // storage.highResolution.provider
            ]
        );
        assert_eq!(outcome.issues[0].field, "hosting.provider");
        assert_eq!(outcome.issues[1].field, "storage.preview.provider");
        assert_eq!(outcome.issues[2].field, "storage.highResolution.provider");
    }

    #[test]
    fn repository_must_be_owner_slash_name() {
        for bad in ["no-slash", "/apenas-nome", "dono/", "dono/a/b", "   "] {
            let mut setup = coherent_v2();
            setup.repository.repository = bad.to_owned();
            let outcome = validate_setup(&setup);
            assert_eq!(
                codes(&outcome),
                ["invalid_repository_configuration"],
                "value: {bad:?}"
            );
            assert_eq!(outcome.issues[0].field, "repository.repository");
        }
    }

    #[test]
    fn hosting_project_and_team_must_not_be_blank() {
        let mut setup = coherent_v2();
        setup.hosting.project = Some("   ".to_owned());
        setup.hosting.team_id = Some(" ".to_owned());
        let outcome = validate_setup(&setup);
        assert_eq!(
            codes(&outcome),
            [
                "invalid_hosting_configuration",
                "invalid_hosting_configuration"
            ]
        );
        assert_eq!(outcome.issues[0].field, "hosting.project");
        assert_eq!(outcome.issues[1].field, "hosting.teamId");
    }

    #[test]
    fn preview_public_base_url_must_be_public_and_absolute() {
        // Schema shape is fine for these (level 1 passes, e.g. the `uri`
        // format); level 2 applies the publication layer's public-URL rule.
        for bad in [
            "http://localhost/previews",
            "http://192.168.1.10/cdn",
            "https://internal.internal/x",
            "ftp://cdn.example.com/previews",
            "https://user:pw@cdn.example.com/x",
        ] {
            let mut setup = coherent_v2();
            setup.storage.preview.public_base_url = Some(bad.to_owned());
            let outcome = validate_setup(&setup);
            assert_eq!(
                codes(&outcome),
                ["invalid_preview_storage_configuration"],
                "value: {bad}"
            );
            assert_eq!(outcome.issues[0].field, "storage.preview.publicBaseUrl");
        }
        let mut setup = coherent_v2();
        setup.storage.preview.public_base_url = None;
        let outcome = validate_setup(&setup);
        assert_eq!(codes(&outcome), ["invalid_preview_storage_configuration"]);
    }

    #[test]
    fn high_resolution_storage_needs_account_bucket_and_public_url() {
        let mut setup = coherent_v2();
        setup.storage.high_resolution.account_id = Some(String::new());
        setup.storage.high_resolution.bucket = Some("  ".to_owned());
        setup.storage.high_resolution.public_base_url = None;
        let outcome = validate_setup(&setup);
        assert_eq!(
            codes(&outcome),
            [
                "invalid_high_resolution_storage_configuration",
                "invalid_high_resolution_storage_configuration",
                "invalid_high_resolution_storage_configuration",
            ]
        );
        assert_eq!(outcome.issues[0].field, "storage.highResolution.accountId");
        assert_eq!(outcome.issues[1].field, "storage.highResolution.bucket");
        assert_eq!(
            outcome.issues[2].field,
            "storage.highResolution.publicBaseUrl"
        );
    }

    #[test]
    fn a_declared_source_must_be_a_folder_with_a_path() {
        // Declared with a blank path.
        let mut setup = coherent_v2();
        setup.source = Some(SourceSetup {
            kind: "folder".to_owned(),
            path: Some("   ".to_owned()),
        });
        let outcome = validate_setup(&setup);
        assert_eq!(codes(&outcome), ["invalid_source_configuration"]);
        assert_eq!(outcome.issues[0].field, "source.path");

        // Declared without a path at all.
        let mut setup = coherent_v2();
        setup.source = Some(SourceSetup {
            kind: "folder".to_owned(),
            path: None,
        });
        let outcome = validate_setup(&setup);
        assert_eq!(codes(&outcome), ["invalid_source_configuration"]);

        // Defensive: a non-folder kind cannot survive the schema, but the
        // rule exists at level 2 independently.
        let mut setup = coherent_v2();
        setup.source = Some(SourceSetup {
            kind: "s3".to_owned(),
            path: Some("fotos".to_owned()),
        });
        let outcome = validate_setup(&setup);
        assert_eq!(codes(&outcome), ["invalid_source_configuration"]);
        assert_eq!(outcome.issues[0].field, "source.type");

        // Undeclared source: not a level-2 issue (the schema allows it).
        let mut setup = coherent_v2();
        setup.source = None;
        let outcome = validate_setup(&setup);
        assert!(outcome.valid);
    }

    #[test]
    fn a_declared_domain_must_be_structurally_public() {
        for bad in ["http://app.local", "ftp://exemplo.com", "http://127.0.0.1/"] {
            let mut setup = coherent_v2();
            setup.domain = Some(DomainSetup {
                url: bad.to_owned(),
            });
            let outcome = validate_setup(&setup);
            assert_eq!(
                codes(&outcome),
                ["invalid_domain_configuration"],
                "value: {bad}"
            );
            assert_eq!(outcome.issues[0].field, "domain.url");
        }
        // A valid domain keeps the configuration valid.
        let mut setup = coherent_v2();
        setup.domain = Some(DomainSetup {
            url: "https://galeria.exemplo.com".to_owned(),
        });
        assert!(validate_setup(&setup).valid);
        // Domain structure is version-independent: same rule on v1.
        let mut setup = coherent_v2();
        setup.schema_version = 1;
        setup.domain = Some(DomainSetup {
            url: "http://app.local".to_owned(),
        });
        assert_eq!(
            codes(&validate_setup(&setup)),
            ["invalid_domain_configuration"]
        );
    }

    #[test]
    fn multiple_problems_are_all_reported_in_a_stable_order() {
        let mut setup = coherent_v2();
        setup.source = Some(SourceSetup {
            kind: "folder".to_owned(),
            path: Some(String::new()),
        });
        setup.repository.provider = "gitlab".to_owned();
        setup.hosting.project = Some(" ".to_owned());
        setup.storage.preview.public_base_url = Some("http://localhost/x".to_owned());
        setup.storage.high_resolution.bucket = Some(" ".to_owned());
        setup.domain = Some(DomainSetup {
            url: "http://10.0.0.4".to_owned(),
        });
        let outcome = validate_setup(&setup);
        // Fixed order: source → repository → hosting → preview →
        // high-resolution → domain. The first problem never stops the rest.
        assert_eq!(
            codes(&outcome),
            [
                "invalid_source_configuration",
                "unsupported_configuration",
                "invalid_hosting_configuration",
                "invalid_preview_storage_configuration",
                "invalid_high_resolution_storage_configuration",
                "invalid_domain_configuration",
            ]
        );
        assert!(!outcome.valid);
    }

    #[test]
    fn validation_is_deterministic_and_carries_no_secrets() {
        let mut setup = coherent_v2();
        setup.repository.provider = "gitlab".to_owned();
        setup.storage.high_resolution.bucket = Some(String::new());
        let first = validate_setup(&setup);
        let second = validate_setup(&setup);
        assert_eq!(first, second);
        // No issue content may carry secrets, environment names, or paths.
        let text = format!("{first:?}").to_lowercase();
        for needle in [
            "photo_publisher_",
            "token",
            "secret",
            "password",
            "credential",
            "c:\\",
        ] {
            assert!(!text.contains(needle), "outcome leaked {needle}");
        }
    }

    #[test]
    fn missing_project_file_keeps_the_existing_classification() {
        let root = tempdir().unwrap();
        let error = validate_project_configuration(&root.path().join("project.json")).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ResourceMissing);
    }

    #[test]
    fn schema_invalid_documents_stay_level_1_and_never_become_issues() {
        // `source.type` has a schema enum: a non-folder source fails level 1
        // (project_invalid), proving levels are not conflated.
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "project": {"id": "ab-cd", "name": "N"},
                "gallery": {"template": "editorial-v1", "title": "N", "bundlePath": "gallery-app"},
                "source": {"type": "s3", "path": "fotos"},
                "repository": {"provider": "github", "repository": "o/r"},
                "hosting": {"provider": "vercel", "project": "p"},
                "storage": {
                    "preview": {"provider": "github", "publicBaseUrl": "https://cdn.example.com/p"},
                    "highResolution": {"provider": "r2", "accountId": "a", "bucket": "b", "publicBaseUrl": "https://d.example.com/o"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let error = validate_project_configuration(&path).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ProjectInvalid);
    }

    #[test]
    fn the_module_never_touches_credentials_providers_or_the_network() {
        // Structural proof of the phase boundary: this module performs no
        // I/O beyond reading the project document and builds nothing remote.
        // Only the non-test part is scanned (the token list itself lives in
        // this test).
        let source = include_str!("config_validation.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "CredentialStore",
            "std::env",
            "photo_publisher_provider_",
            "preflight_publication",
            "build_providers",
            "reqwest",
            "TcpStream",
            "HttpClient",
            ".publish(",
            ".put(",
            ".commit(",
            ".delete(",
        ] {
            assert!(
                !source.contains(forbidden),
                "config_validation must not reference {forbidden}"
            );
        }
    }
}
