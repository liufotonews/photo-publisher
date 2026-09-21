use photo_publisher_contract_validator::validate;
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
