//! Project loading and path resolution for the application layer.
//!
//! This module is the single place that resolves a `project.json` into a
//! workspace handle. The rules mirror exactly the published CLI behavior:
//! `source.path` is resolved relative to the project file (absolute paths are
//! preserved), and the local output is always `<project parent>/output`.

use std::path::{Path, PathBuf};

use photo_publisher_contract_validator::{
    compile_embedded_schema, load_json, validate_value, EmbeddedSchema,
};
use serde_json::Value;

use crate::errors::{ApplicationError, ApplicationErrorKind};

/// Whether the document is the current v2 integrated publication contract or
/// the v1 local-only contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// `schemaVersion: 2` — integrated publication.
    Version2,
    /// `schemaVersion: 1` — the local-only contract.
    Version1,
}

/// A resolved project in the application layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectHandle {
    pub project_path: PathBuf,
    pub kind: ProjectKind,
    pub project_id: String,
    pub project_name: String,
    /// `source.path` exactly as declared in the document (before resolution).
    pub source_declared: String,
    pub source_dir: PathBuf,
    pub output_dir: PathBuf,
}

fn invalid(message: impl Into<String>) -> ApplicationError {
    ApplicationError::new(ApplicationErrorKind::ProjectInvalid, message)
}

fn missing(message: impl Into<String>) -> ApplicationError {
    ApplicationError::new(ApplicationErrorKind::ResourceMissing, message)
}

/// Loads a project.json and validates it against the production schema that
/// is embedded in the binary. Path resolution never runs against CWD.
pub fn load_project_document(path: &Path) -> Result<Value, ApplicationError> {
    if !path.is_file() {
        return Err(missing(format!(
            "project file does not exist: {}",
            path.display()
        )));
    }
    let project = load_json(path).map_err(|error| {
        ApplicationError::with_source(
            ApplicationErrorKind::ProjectInvalid,
            format!("{error}"),
            error,
        )
    })?;
    let validator = compile_embedded_schema(EmbeddedSchema::Project).map_err(|error| {
        ApplicationError::with_source(ApplicationErrorKind::Internal, format!("{error}"), error)
    })?;
    validate_value(&validator, &project).map_err(|error| {
        ApplicationError::with_source(
            ApplicationErrorKind::ProjectInvalid,
            format!("project.json contract validation failed: {error}"),
            error,
        )
    })?;
    Ok(project)
}

/// Resolves `source.path` relative to the project file, exactly as the CLI
/// does today. Relative paths join the project parent; absolute paths are
/// preserved. Requires `source.type == "folder"`.
pub fn resolve_source_dir(
    project_path: &Path,
    project: &Value,
) -> Result<PathBuf, ApplicationError> {
    if project["source"]["type"].as_str() != Some("folder") {
        return Err(invalid(
            "project.source.type must be folder for local CLI operations",
        ));
    }
    let raw = project["source"]["path"]
        .as_str()
        .ok_or_else(|| invalid("project.source.path is required"))?;
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        project_path.parent().unwrap_or(Path::new(".")).join(path)
    })
}

/// Local output directory of a project: `<project parent>/output`.
pub fn resolve_output_dir(project_path: &Path) -> PathBuf {
    project_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("output")
}

/// Loads and fully resolves a project handle. Errors carry the same
/// classification the CLI has always produced, so interfaces remain stable.
pub fn project_handle(path: &Path) -> Result<ProjectHandle, ApplicationError> {
    let document = load_project_document(path)?;
    let handle = resolve_handle_from_document(path, &document)?;
    if !handle.source_dir.is_dir() {
        return Err(missing(format!(
            "source directory does not exist: {}",
            handle.source_dir.display()
        )));
    }
    Ok(handle)
}

/// Shared assembly of a handle from an already loaded document. Used by
/// `project_handle` and by `ValidateProject` (which returns it directly).
pub(crate) fn resolve_handle_from_document(
    path: &Path,
    project: &Value,
) -> Result<ProjectHandle, ApplicationError> {
    let kind = match project["schemaVersion"].as_u64() {
        Some(2) => ProjectKind::Version2,
        _ => ProjectKind::Version1,
    };
    Ok(ProjectHandle {
        project_path: path.to_path_buf(),
        kind,
        project_id: project["project"]["id"]
            .as_str()
            .expect("schema requires project.id")
            .to_owned(),
        project_name: project["project"]["name"]
            .as_str()
            .expect("schema requires project.name")
            .to_owned(),
        source_declared: project["source"]["path"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        source_dir: resolve_source_dir(path, project)?,
        output_dir: resolve_output_dir(path),
    })
}
