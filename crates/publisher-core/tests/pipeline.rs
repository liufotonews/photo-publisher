use photo_publisher_core::{load_state, plan_sync, save_state, scan_jpegs, state_from, SyncAction};
use std::fs;
use tempfile::tempdir;

#[test]
fn scan_empty_directory() {
    let dir = tempdir().unwrap();
    let photos = scan_jpegs(dir.path()).unwrap();
    assert!(photos.is_empty());
}

#[test]
fn scan_single_jpeg() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("test.jpg");
    fs::write(&file_path, b"test content").unwrap();

    let photos = scan_jpegs(dir.path()).unwrap();
    assert_eq!(photos.len(), 1);
    assert_eq!(photos[0].relative_path, "test.jpg");
    assert_eq!(photos[0].bytes, 12);
    // sha256 of "test content" is 6ae8a75555209fd6c44157c0aed8016e763ff435a19cf186f76863140143ff72
    assert_eq!(
        photos[0].sha256,
        "6ae8a75555209fd6c44157c0aed8016e763ff435a19cf186f76863140143ff72"
    );
}

#[test]
fn scan_nested_directories() {
    let dir = tempdir().unwrap();
    let nested = dir.path().join("sub").join("folder");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("photo.jpg"), b"data").unwrap();

    let photos = scan_jpegs(dir.path()).unwrap();
    assert_eq!(photos.len(), 1);
    assert_eq!(photos[0].relative_path, "sub/folder/photo.jpg");
}

#[test]
fn scan_ignores_non_jpeg() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("image.png"), b"png").unwrap();
    fs::write(dir.path().join("doc.txt"), b"txt").unwrap();
    fs::write(dir.path().join("data.raw"), b"raw").unwrap();
    fs::write(dir.path().join("actual.jpg"), b"jpg").unwrap();

    let photos = scan_jpegs(dir.path()).unwrap();
    assert_eq!(photos.len(), 1);
    assert_eq!(photos[0].relative_path, "actual.jpg");
}

#[test]
fn scan_nonexistent_directory_fails() {
    let dir = tempdir().unwrap();
    let bad_path = dir.path().join("does_not_exist");

    let result = scan_jpegs(&bad_path);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("source directory does not exist"));
}

#[test]
fn full_pipeline_scan_state_plan() {
    let source_dir = tempdir().unwrap();
    let state_dir = tempdir().unwrap();
    let state_path = state_dir.path().join("state.json");

    // 1. Initial state
    fs::write(source_dir.path().join("1.jpg"), b"1").unwrap();
    fs::write(source_dir.path().join("2.jpg"), b"2").unwrap();

    let initial_photos = scan_jpegs(source_dir.path()).unwrap();
    let initial_state = state_from(&initial_photos);
    save_state(&state_path, &initial_state).unwrap();

    // 2. Modify files
    fs::remove_file(source_dir.path().join("1.jpg")).unwrap(); // Remove 1.jpg
    fs::write(source_dir.path().join("2.jpg"), b"2_modified").unwrap(); // Update 2.jpg
    fs::write(source_dir.path().join("3.jpg"), b"3").unwrap(); // Add 3.jpg

    // 3. Scan again and plan sync
    let current_photos = scan_jpegs(source_dir.path()).unwrap();
    let loaded_state = load_state(&state_path).unwrap();

    let plan = plan_sync(&loaded_state, &current_photos);

    assert_eq!(plan.len(), 3);

    let mut adds = 0;
    let mut updates = 0;
    let mut removes = 0;

    for action in plan {
        match action {
            SyncAction::Add(p) => {
                assert_eq!(p.relative_path, "3.jpg");
                adds += 1;
            }
            SyncAction::Update(p) => {
                assert_eq!(p.relative_path, "2.jpg");
                updates += 1;
            }
            SyncAction::Remove { relative_path } => {
                assert_eq!(relative_path, "1.jpg");
                removes += 1;
            }
        }
    }

    assert_eq!(adds, 1);
    assert_eq!(updates, 1);
    assert_eq!(removes, 1);
}
