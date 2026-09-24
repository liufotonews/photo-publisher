use anyhow::{Context, Result};
use jsonschema::{Draft, Validator};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// The versioned contract documents, embedded at compile time from the files
/// versioned in `schemas/`.
///
/// These embedded copies are the only schema source used by production code:
/// a distributed executable never consults the filesystem, the current
/// working directory, its own location, or workspace layout. The versioned
/// files remain the source of truth; changing a contract requires a rebuild,
/// which is intentional: the binary then validates exactly the contract it
/// was built for.
const PROJECT_SCHEMA_JSON: &str = include_str!("../../../schemas/project.schema.json");
const GALLERY_SCHEMA_JSON: &str = include_str!("../../../schemas/gallery.schema.json");

/// A contract document embedded in the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedSchema {
    /// `schemas/project.schema.json`
    Project,
    /// `schemas/gallery.schema.json`
    Gallery,
}

/// Compiles an embedded contract document. Uses exactly the same draft and
/// build logic as the path-based loader, so embedded and file compilation
/// converge to the same validator construction.
pub fn compile_embedded_schema(schema: EmbeddedSchema) -> Result<Validator> {
    let document = match schema {
        EmbeddedSchema::Project => PROJECT_SCHEMA_JSON,
        EmbeddedSchema::Gallery => GALLERY_SCHEMA_JSON,
    };
    let value: Value =
        serde_json::from_str(document).context("embedded schema is not valid JSON")?;
    compile_schema_json(&value)
}

pub fn load_json(path: impl AsRef<Path>) -> Result<Value> {
    let path = path.as_ref();
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

pub fn compile_schema(schema_path: impl AsRef<Path>) -> Result<Validator> {
    let schema = load_json(schema_path)?;
    compile_schema_json(&schema)
}

/// Single shared validator construction: Draft 2020-12, same options for
/// file-loaded and embedded schemas.
fn compile_schema_json(schema: &Value) -> Result<Validator> {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(schema)
        .context("failed to compile JSON Schema")
}

pub fn validate(schema_path: impl AsRef<Path>, document_path: impl AsRef<Path>) -> Result<()> {
    let validator = compile_schema(schema_path)?;
    let document = load_json(document_path)?;
    validate_value(&validator, &document)
}

pub fn validate_value(validator: &Validator, document: &Value) -> Result<()> {
    let errors: Vec<String> = validator
        .iter_errors(document)
        .map(|error| error.to_string())
        .collect();

    if !errors.is_empty() {
        anyhow::bail!("schema validation failed:\n{}", errors.join("\n"));
    }

    validate_gallery_photo_ids(document)
}

fn validate_gallery_photo_ids(document: &Value) -> Result<()> {
    let Some(photos) = document.get("photos").and_then(Value::as_array) else {
        return Ok(());
    };

    let mut ids = HashSet::with_capacity(photos.len());
    for (index, photo) in photos.iter().enumerate() {
        let Some(id) = photo.get("id").and_then(Value::as_str) else {
            continue;
        };

        if !ids.insert(id) {
            anyhow::bail!(
                "gallery validation failed: duplicate photo id '{}' at index {}",
                id,
                index
            );
        }
    }

    Ok(())
}
