//! Project Setup contract (Phase 7-A).
//!
//! The contract represents the configuration of a photo publishing project
//! — nothing else. It produces and consumes the exact `project.json` the
//! existing schema defines; it never creates infrastructure, never calls a
//! network, never touches providers, and never stores credentials.
//!
//! This module only defines the model plus deterministic load/save of the
//! document. Publication, preflight, provisioning, UI and credentials all
//! stay outside this contract (see Phase 7-0).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ApplicationError, ApplicationErrorKind};

fn project_invalid(message: impl Into<String>) -> ApplicationError {
    ApplicationError::new(ApplicationErrorKind::ProjectInvalid, message)
}

fn resource_missing(message: impl Into<String>) -> ApplicationError {
    ApplicationError::new(ApplicationErrorKind::ResourceMissing, message)
}

/// The setup document of a project, exactly mirroring `project.schema.json`.
///
/// The model is deliberately data-only: it does not load files implicitly,
/// does not normalize paths, does not canonicalize filesystem entries, does
/// not read environment variables, and does not know providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSetup {
    pub schema_version: u8,
    pub project: ProjectIdentity,
    pub gallery: GallerySetup,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceSetup>,
    pub repository: RepositorySetup,
    pub hosting: HostingSetup,
    pub storage: StorageSetup,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<DomainSetup>,
}

/// Stable identity of the project — never derived from publications,
/// timestamps, hashes, or filesystem paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectIdentity {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GallerySetup {
    pub template: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
}

/// The source the Publisher consumes: a folder of final JPEG files. Raw,
/// TIFF, PSD/PSB, Lightroom catalogs, or Photoshop sources never enter this
/// contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSetup {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySetup {
    pub provider: String,
    pub repository: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostingSetup {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSetup {
    pub preview: StorageTargetSetup,
    pub high_resolution: StorageTargetSetup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageTargetSetup {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainSetup {
    pub url: String,
}

impl ProjectSetup {
    /// Loads a `project.json` into the contract: identical validation as the
    /// application layer's existing loader, then a semantic decode.
    pub fn load(path: &Path) -> Result<Self, ApplicationError> {
        let document = crate::project::load_project_document(path)?;
        let model: Self = serde_json::from_value(document.clone()).map_err(|error| {
            project_invalid(format!(
                "project.json does not match the setup contract: {error}"
            ))
        })?;
        Ok(model)
    }

    /// Serializes the model to a JSON string (stable content for a given
    /// value — no timestamps, no UUIDs, nothing generated implicitly).
    pub fn to_document(&self) -> Result<String, ApplicationError> {
        // The written document must pass the same embedded schema the reader
        // enforces; no alternate format, no shortcuts.
        let value = serde_json::to_value(self)
            .map_err(|error| project_invalid(format!("setup serialization failed: {error}")))?;
        validate_document(&value)?;
        serde_json::to_string_pretty(&value)
            .map_err(|error| project_invalid(format!("setup serialization failed: {error}")))
    }

    /// Writes the document to `path` (the file is the single output of this
    /// contract; no other filesystem entry is touched).
    pub fn write(&self, path: &Path) -> Result<(), ApplicationError> {
        let document = self.to_document()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.is_dir() {
                return Err(resource_missing(format!(
                    "project directory does not exist: {}",
                    parent.display()
                )));
            }
        }
        std::fs::write(path, document).map_err(|error| {
            // Filesystem/I/O failure: classified as the existing application
            // `Internal` category; the public message is generic and never
            // carries the user-provided path; the cause is preserved.
            ApplicationError::with_source(
                ApplicationErrorKind::Internal,
                "failed to write project configuration",
                error,
            )
        })
    }
}

/// Validation against the embedded schema, the same authority the rest of
/// the application already uses.
fn validate_document(value: &serde_json::Value) -> Result<(), ApplicationError> {
    let validator = photo_publisher_contract_validator::compile_embedded_schema(
        photo_publisher_contract_validator::EmbeddedSchema::Project,
    )
    .map_err(|error| {
        ApplicationError::with_source(
            ApplicationErrorKind::Internal,
            format!("embedded schema unavailable: {error}"),
            error,
        )
    })?;
    photo_publisher_contract_validator::validate_value(&validator, value).map_err(|error| {
        project_invalid(format!("project.json contract validation failed: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A valid v2 project the current schema already accepts.
    fn valid_v2() -> ProjectSetup {
        ProjectSetup {
            schema_version: 2,
            project: ProjectIdentity {
                id: "joao-maria-2026".to_owned(),
                name: "João & Maria".to_owned(),
                client: Some("João & Maria".to_owned()),
                date: Some("2026-09-20".to_owned()),
            },
            gallery: GallerySetup {
                template: "editorial-v1".to_owned(),
                title: "João & Maria".to_owned(),
                description: Some("Galeria de entrega.".to_owned()),
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
                team_id: Some("team_example".to_owned()),
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
            domain: Some(DomainSetup {
                url: "https://galeria.exemplo.com".to_owned(),
            }),
        }
    }

    #[test]
    fn valid_v2_contract_serializes_to_a_schema_valid_document() {
        let model = valid_v2();
        let document = model.to_document().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&document).unwrap();
        assert_eq!(parsed["schemaVersion"], 2);
        assert_eq!(parsed["project"]["id"], "joao-maria-2026");
        assert_eq!(parsed["project"]["client"], "João & Maria");
        assert_eq!(parsed["gallery"]["bundlePath"], "gallery-app");
        assert_eq!(
            parsed["storage"]["highResolution"]["accountId"],
            "account-example"
        );
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let model = valid_v2();
        let document = model.to_document().unwrap();
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        std::fs::write(&path, &document).unwrap();
        let reloaded = ProjectSetup::load(&path).unwrap();
        assert_eq!(reloaded, model);
    }

    #[test]
    fn existing_repository_fixture_loads_through_the_contract() {
        // The canonical fixture published by the contract validator continues
        // to load without migration.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/valid/project.v2.valid.json");
        let model = ProjectSetup::load(&path).unwrap();
        assert_eq!(model.project.id, "joao-maria-2026");
        assert_eq!(model.schema_version, 2);
    }

    #[test]
    fn existing_v1_fixture_loads_without_migration() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/valid/project.valid.json");
        let model = ProjectSetup::load(&path).unwrap();
        assert_eq!(model.schema_version, 1);
        // v1 never invented new required fields.
        assert!(model.gallery.bundle_path.is_none());
    }

    #[test]
    fn missing_required_fields_are_rejected_at_write_time() {
        let mut model = valid_v2();
        model.project.id = String::new(); // schema requires project.id
        let error = model.to_document().unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::ProjectInvalid);
    }

    #[test]
    fn write_produces_no_side_effects_beyond_the_document() {
        let root = tempdir().unwrap();
        let path = root.path().join("project.json");
        let model = valid_v2();
        model.write(&path).unwrap();
        let entries: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only project.json must be written");
        // Writing twice is byte-stable.
        model.write(&path).unwrap();
        let first = std::fs::read(&path).unwrap();
        model.write(&path).unwrap();
        assert_eq!(first, std::fs::read(&path).unwrap());
    }

    #[test]
    fn no_secrets_can_appear_in_the_serialized_document() {
        let model = valid_v2();
        let text = model.to_document().unwrap().to_lowercase();
        for needle in ["token", "secret", "password", "credential", "api_key"] {
            assert!(!text.contains(needle), "setup document leaked {needle}");
        }
    }

    #[test]
    fn unicode_and_spaces_are_preserved_verbatim() {
        let mut model = valid_v2();
        model.project.name = "Casamento João & Maria — ẞàrão".to_owned();
        model.source = Some(SourceSetup {
            kind: "folder".to_owned(),
            path: Some("Fotos com espaços\\Finais".to_owned()),
        });
        let document = model.to_document().unwrap();
        assert!(document.contains("ẞàrão"));
        assert!(document.contains("Fotos com espaços"));
    }

    #[test]
    fn schema_version_is_preserved_as_data_not_execution_state() {
        // Project identity must be data, never derived from execution.
        let model = valid_v2();
        assert_eq!(model.project.id, "joao-maria-2026");
        let again = valid_v2();
        assert_eq!(model, again);
    }

    #[test]
    fn write_never_classifies_generic_io_errors_as_resource_missing() {
        // Deterministic failure: the target project path IS an existing
        // directory, so any write into it must fail (PermissionDenied on
        // Windows, EISDIR on Linux).
        let root = tempdir().unwrap();
        let target = root.path().join("project.json");
        std::fs::create_dir(&target).unwrap();
        let error = valid_v2().write(&target).unwrap_err();

        // 1. Not a ResourceMissing: the destination exists.
        assert_ne!(error.kind, ApplicationErrorKind::ResourceMissing);
        assert_eq!(error.kind, ApplicationErrorKind::Internal);

        // 2. The public message never contains the user-provided path.
        let message = error.to_string();
        let path_text = target.display().to_string();
        assert!(
            !message.contains(path_text.as_str()),
            "error message must not expose the user path: {message}"
        );

        // 3. The underlying I/O cause is preserved for diagnostics.
        assert!(error.source_error().is_some());
    }
}
