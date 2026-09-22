use photo_publisher_pipeline::{build_local_gallery_with_fault, FaultPoint, PipelineOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn write_jpeg(path: &Path, value: u8) {
    let image = image::RgbImage::from_pixel(1, 1, image::Rgb([value, 0, 0]));
    image.save(path).unwrap();
}

fn cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_photo-publisher"))
}

fn project(root: &Path, source: &Path) -> PathBuf {
    let path = root.join("project.json");
    let value = serde_json::json!({
        "schemaVersion": 1,
        "project": {"id": "cli-test", "name": "CLI Test"},
        "gallery": {"template": "local", "title": "CLI Test"},
        "source": {"type": "folder", "path": source.to_string_lossy()},
        "repository": {"provider": "local", "repository": "local"},
        "hosting": {"provider": "local"},
        "storage": {"preview": {"provider": "local"}, "highResolution": {"provider": "local"}}
    });
    fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    path
}

#[test]
fn help_version_and_invalid_command_have_stable_behavior() {
    let help = Command::new(cli()).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("USAGE"));

    let version = Command::new(cli()).arg("version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("photo-publisher"));

    let invalid = Command::new(cli()).args(["wat"]).output().unwrap();
    assert_eq!(invalid.status.code(), Some(2));

    let invalid_json = Command::new(cli())
        .args(["wat", "--json"])
        .output()
        .unwrap();
    assert_eq!(invalid_json.status.code(), Some(2));
    let body: serde_json::Value = serde_json::from_slice(&invalid_json.stdout).unwrap();
    assert_eq!(body["error"]["code"], 2);
}

#[test]
fn validate_json_and_missing_source_use_json_protocol() {
    let root = tempdir().unwrap();
    let source = root.path().join("Fotos Unicode");
    fs::create_dir_all(&source).unwrap();
    let project_path = project(root.path(), &source);
    let valid = Command::new(cli())
        .args(["validate", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(valid.status.success());
    let body: serde_json::Value = serde_json::from_slice(&valid.stdout).unwrap();
    assert_eq!(body["ok"], true);

    let missing = root.path().join("missing");
    let missing_project = project(root.path(), &missing);
    let result = Command::new(cli())
        .args(["validate", missing_project.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(4));
    let body: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(body["error"]["code"], 4);

    let bad_json = root.path().join("bad.json");
    fs::write(&bad_json, b"{bad").unwrap();
    let result = Command::new(cli())
        .args(["validate", bad_json.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    let body: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(body["error"]["category"], "project_invalid");
}

#[test]
fn publish_second_run_inspect_and_dry_run_are_supported() {
    let root = tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(&source).unwrap();
    let project_path = project(root.path(), &source);

    let dry = Command::new(cli())
        .args([
            "publish",
            project_path.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(dry.status.success());
    assert!(!root.path().join("output/gallery.json").exists());

    let publish = Command::new(cli())
        .args(["publish", project_path.to_str().unwrap(), "--silent"])
        .output()
        .unwrap();
    assert!(
        publish.status.success(),
        "{}",
        String::from_utf8_lossy(&publish.stderr)
    );
    assert!(root.path().join("output/gallery.json").exists());

    let second = Command::new(cli())
        .args(["publish", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(second.status.success());
    let inspect = Command::new(cli())
        .args(["inspect", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(inspect.status.success());
    let body: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["journal_present"], true);

    let recover = Command::new(cli())
        .args(["recover", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(recover.status.success());
}

#[test]
fn publish_handles_new_and_modified_photos() {
    let root = tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(&source).unwrap();
    let project_path = project(root.path(), &source);

    let initial = Command::new(cli())
        .args(["publish", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(initial.status.success());

    write_jpeg(&source.join("a.jpg"), 10);
    let added = Command::new(cli())
        .args(["publish", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    write_jpeg(&source.join("a.jpg"), 200);
    let modified = Command::new(cli())
        .args(["publish", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        modified.status.success(),
        "{}",
        String::from_utf8_lossy(&modified.stderr)
    );
}

#[test]
fn recover_handles_an_interrupted_publication() {
    let root = tempdir().unwrap();
    let source = root.path().join("source");
    let output = root.path().join("output");
    fs::create_dir_all(&source).unwrap();
    write_jpeg(&source.join("a.jpg"), 10);
    let project_path = project(root.path(), &source);
    let options = PipelineOptions {
        source_dir: source,
        output_dir: output.clone(),
        project_title: "CLI Test".into(),
    };
    assert!(
        build_local_gallery_with_fault(&options, Some(FaultPoint::AfterGalleryReplacement))
            .is_err()
    );
    let recovered = Command::new(cli())
        .args(["recover", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(output.join("gallery.json").is_file());
    assert!(output.join(".publisher/state.json").is_file());
}

#[test]
fn inspect_rejects_corrupt_journal_and_state() {
    let root = tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(source).unwrap();
    let project_path = project(root.path(), &root.path().join("source"));
    let publisher = root.path().join("output/.publisher");
    fs::create_dir_all(&publisher).unwrap();
    fs::write(publisher.join("journal.json"), b"{bad").unwrap();
    let journal_error = Command::new(cli())
        .args(["inspect", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(journal_error.status.code(), Some(7));
    let body: serde_json::Value = serde_json::from_slice(&journal_error.stdout).unwrap();
    assert_eq!(body["error"]["category"], "recovery_failed");

    fs::remove_file(publisher.join("journal.json")).unwrap();
    fs::write(publisher.join("state.json"), b"{bad").unwrap();
    let state_error = Command::new(cli())
        .args(["inspect", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(state_error.status.code(), Some(5));
    let body: serde_json::Value = serde_json::from_slice(&state_error.stdout).unwrap();
    assert_eq!(body["error"]["category"], "validation");
}

#[test]
fn distributed_executable_uses_adjacent_schema_and_reports_missing_schema() {
    let root = tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir_all(&source).unwrap();
    let project_path = project(root.path(), &source);
    let dist = root.path().join("dist");
    fs::create_dir_all(dist.join("schemas")).unwrap();
    let exe = dist.join("photo-publisher.exe");
    fs::copy(cli(), &exe).unwrap();
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/project.schema.json"),
        dist.join("schemas/project.schema.json"),
    )
    .unwrap();
    let with_schema = Command::new(&exe)
        .current_dir(root.path())
        .args(["validate", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(with_schema.status.success());

    fs::remove_file(dist.join("schemas/project.schema.json")).unwrap();
    let without_schema = Command::new(&exe)
        .current_dir(root.path())
        .args(["validate", project_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert_eq!(without_schema.status.code(), Some(4));
    let body: serde_json::Value = serde_json::from_slice(&without_schema.stdout).unwrap();
    assert_eq!(body["error"]["category"], "resource_missing");
}
