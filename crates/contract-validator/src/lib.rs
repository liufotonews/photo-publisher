use anyhow::{Context, Result};
use jsonschema::{Draft, Validator};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub fn load_json(path: impl AsRef<Path>) -> Result<Value> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("invalid JSON in {}", path.display()))
}

pub fn compile_schema(schema_path: impl AsRef<Path>) -> Result<Validator> {
    let schema = load_json(schema_path)?;
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .context("failed to compile JSON Schema")?;
    Ok(validator)
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
