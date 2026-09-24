use photo_publisher_contract_validator::{
    compile_embedded_schema, compile_schema, load_json, validate, validate_value, EmbeddedSchema,
};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn valid_project_is_accepted() {
    let r = root();
    validate(
        r.join("schemas/project.schema.json"),
        r.join("fixtures/valid/project.valid.json"),
    )
    .unwrap();
}

#[test]
fn valid_project_v2_is_accepted_with_or_without_team_id() {
    let r = root();
    for fixture in ["project.v2.valid.json", "project.v2.without-team.json"] {
        validate(
            r.join("schemas/project.schema.json"),
            r.join("fixtures/valid").join(fixture),
        )
        .unwrap();
    }
}

#[test]
fn v2_requires_integrated_publication_fields() {
    let r = root();
    for fixture in [
        "project.v2.missing-bundle-path.json",
        "project.v2.missing-account-id.json",
        "project.v2.missing-bucket.json",
        "project.v2.missing-hosting-project.json",
        "project.v2.extra-property.json",
    ] {
        assert!(validate(
            r.join("schemas/project.schema.json"),
            r.join("fixtures/invalid").join(fixture),
        )
        .is_err());
    }
}

#[test]
fn valid_gallery_is_accepted() {
    let r = root();
    validate(
        r.join("schemas/gallery.schema.json"),
        r.join("fixtures/valid/gallery.valid.json"),
    )
    .unwrap();
}

#[test]
fn bad_project_id_is_rejected() {
    let r = root();
    assert!(validate(
        r.join("schemas/project.schema.json"),
        r.join("fixtures/invalid/project.bad-id.json"),
    )
    .is_err());
}

#[test]
fn project_source_without_type_is_rejected() {
    let r = root();
    assert!(validate(
        r.join("schemas/project.schema.json"),
        r.join("fixtures/invalid/project.source-without-type.json"),
    )
    .is_err());
}

#[test]
fn bad_gallery_width_is_rejected() {
    let r = root();
    assert!(validate(
        r.join("schemas/gallery.schema.json"),
        r.join("fixtures/invalid/gallery.bad-width.json"),
    )
    .is_err());
}

#[test]
fn zero_sequence_is_rejected() {
    let r = root();
    assert!(validate(
        r.join("schemas/gallery.schema.json"),
        r.join("fixtures/invalid/gallery.zero-sequence.json"),
    )
    .is_err());
}

#[test]
fn duplicate_photo_id_is_rejected() {
    let r = root();
    assert!(validate(
        r.join("schemas/gallery.schema.json"),
        r.join("fixtures/invalid/gallery.duplicate-photo-id.json"),
    )
    .is_err());
}

#[test]
fn unexpected_gallery_property_is_rejected() {
    let r = root();
    assert!(validate(
        r.join("schemas/gallery.schema.json"),
        r.join("fixtures/invalid/gallery.extra-property.json"),
    )
    .is_err());
}

/// Every fixture must produce the identical validation outcome under the
/// embedded schema and the versioned file: this proves the embedded copies
/// never drift from the sources and that both compilation paths converge
/// (same draft, same extra rules, same accept/reject decisions).
#[test]
fn embedded_schemas_match_the_versioned_files_on_all_fixtures() {
    let r = root();
    let project_embedded = compile_embedded_schema(EmbeddedSchema::Project).unwrap();
    let project_file = compile_schema(r.join("schemas/project.schema.json")).unwrap();
    let gallery_embedded = compile_embedded_schema(EmbeddedSchema::Gallery).unwrap();
    let gallery_file = compile_schema(r.join("schemas/gallery.schema.json")).unwrap();

    for directory in ["fixtures/valid", "fixtures/invalid"] {
        for entry in std::fs::read_dir(root().join(directory)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let value = load_json(&path).unwrap();
            let (embedded, file) = if name.starts_with("gallery") {
                (&gallery_embedded, &gallery_file)
            } else if name.starts_with("project") {
                (&project_embedded, &project_file)
            } else {
                continue;
            };
            let embedded_result = validate_value(embedded, &value);
            let file_result = validate_value(file, &value);
            assert_eq!(
                embedded_result.is_ok(),
                file_result.is_ok(),
                "drift on fixture {name}: embedded={embedded_result:?} file={file_result:?}"
            );
        }
    }
}

/// The known failure classes are exercised explicitly through both paths:
/// duplicate photo id (custom rule), invalid width, zero sequence, and an
/// extra property rejected by `additionalProperties: false`.
#[test]
fn embedded_validation_keeps_the_custom_and_schema_rules() {
    let r = root();
    let gallery = compile_embedded_schema(EmbeddedSchema::Gallery).unwrap();
    for fixture in [
        "gallery.duplicate-photo-id.json",
        "gallery.bad-width.json",
        "gallery.zero-sequence.json",
        "gallery.extra-property.json",
    ] {
        let value = load_json(r.join("fixtures/invalid").join(fixture)).unwrap();
        assert!(
            validate_value(&gallery, &value).is_err(),
            "embedded gallery accepted {fixture}"
        );
    }
    let valid = load_json(r.join("fixtures/valid/gallery.valid.json")).unwrap();
    validate_value(&gallery, &valid).unwrap();
    let project = compile_embedded_schema(EmbeddedSchema::Project).unwrap();
    let valid = load_json(r.join("fixtures/valid/project.valid.json")).unwrap();
    validate_value(&project, &valid).unwrap();
    let valid_v2 = load_json(r.join("fixtures/valid/project.v2.valid.json")).unwrap();
    validate_value(&project, &valid_v2).unwrap();
}
