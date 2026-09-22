use photo_publisher_pipeline::{
    build_local_gallery, build_local_gallery_with_fault,
    journal::{read_journal, update_journal, JournalPhase},
    recover_publication_with_fault, FaultPoint, PipelineOptions,
};
use std::fs;
use tempfile::tempdir;

const TINY_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x01, 0x00, 0x48,
    0x00, 0x48, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x03, 0x02, 0x02, 0x03,
    0x03, 0x03, 0x03, 0x04, 0x03, 0x03, 0x04, 0x05, 0x08, 0x05, 0x05, 0x04, 0x04, 0x05, 0x0a, 0x07,
    0x07, 0x06, 0x08, 0x0c, 0x0a, 0x0c, 0x0c, 0x0b, 0x0a, 0x0b, 0x0b, 0x0d, 0x0e, 0x12, 0x10, 0x0d,
    0x0e, 0x11, 0x0e, 0x0b, 0x0b, 0x10, 0x16, 0x10, 0x11, 0x13, 0x14, 0x15, 0x15, 0x15, 0x0c, 0x0f,
    0x17, 0x18, 0x16, 0x14, 0x18, 0x12, 0x14, 0x15, 0x14, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xff, 0xc4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0xff, 0xc4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x3f, 0xff, 0xd9,
];

#[test]
fn test_build_empty_gallery() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Empty".into(),
    };
    build_local_gallery(&opts).unwrap();
    assert!(out.path().join("gallery.json").exists());
}

#[test]
fn test_build_with_photos() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();

    fs::write(src.path().join("1.jpg"), TINY_JPEG).unwrap();
    fs::create_dir_all(src.path().join("sub")).unwrap();
    fs::write(src.path().join("sub/2.jpg"), TINY_JPEG).unwrap();

    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Photos".into(),
    };
    build_local_gallery(&opts).unwrap();

    assert!(out.path().join("gallery.json").exists());

    let previews = fs::read_dir(out.path().join("photos/preview"))
        .unwrap()
        .count();
    let downloads = fs::read_dir(out.path().join("photos/download"))
        .unwrap()
        .count();

    assert_eq!(previews, 1);
    assert_eq!(downloads, 1);
}

#[test]
fn test_corrupted_jpeg_fails_safely() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();

    // 1st run: Valid photo
    fs::write(src.path().join("valid.jpg"), TINY_JPEG).unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Valid".into(),
    };
    build_local_gallery(&opts).unwrap();

    let original_gallery = fs::read_to_string(out.path().join("gallery.json")).unwrap();
    let original_state = fs::read_to_string(out.path().join(".publisher/state.json")).unwrap();

    // 2nd run: Introduce corrupted JPEG
    let corrupted = b"corrupted data that isn't a jpeg";
    fs::write(src.path().join("corrupt.jpg"), corrupted).unwrap();

    let result = build_local_gallery(&opts);
    assert!(result.is_err(), "Should fail during decode");

    let current_gallery = fs::read_to_string(out.path().join("gallery.json")).unwrap();
    let current_state = fs::read_to_string(out.path().join(".publisher/state.json")).unwrap();

    assert_eq!(original_gallery, current_gallery);
    assert_eq!(original_state, current_state);
}

#[test]
fn test_second_run_is_idempotent() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Idempotent".into(),
    };

    build_local_gallery(&opts).unwrap();
    let gallery = fs::read_to_string(out.path().join("gallery.json")).unwrap();
    let state = fs::read_to_string(out.path().join(".publisher/state.json")).unwrap();
    build_local_gallery(&opts).unwrap();
    assert_eq!(
        gallery,
        fs::read_to_string(out.path().join("gallery.json")).unwrap()
    );
    assert_eq!(
        state,
        fs::read_to_string(out.path().join(".publisher/state.json")).unwrap()
    );
}

#[test]
fn test_fault_after_gallery_replacement_recovers_previous_pair() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Recovery".into(),
    };
    build_local_gallery(&opts).unwrap();
    let original_gallery = fs::read_to_string(out.path().join("gallery.json")).unwrap();
    let original_state = fs::read_to_string(out.path().join(".publisher/state.json")).unwrap();

    let mut changed_jpeg = TINY_JPEG.to_vec();
    changed_jpeg.push(0);
    fs::write(src.path().join("photo.jpg"), changed_jpeg).unwrap();
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterGalleryReplacement)).is_err()
    );
    photo_publisher_pipeline::recover_publication(out.path()).unwrap();
    assert_eq!(
        original_gallery,
        fs::read_to_string(out.path().join("gallery.json")).unwrap()
    );
    assert_eq!(
        original_state,
        fs::read_to_string(out.path().join(".publisher/state.json")).unwrap()
    );
}

#[test]
fn test_journal_is_committed_and_staged_records_are_integrity_checked() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Journal".into(),
    };
    build_local_gallery(&opts).unwrap();
    let journal = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    assert_eq!(
        journal.phase,
        photo_publisher_pipeline::journal::JournalPhase::Committed
    );
    assert!(out.path().join(".publisher/journal/history").is_dir());
}

#[test]
fn test_corrupted_journal_fails_without_touching_active_files() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Corrupt journal".into(),
    };
    build_local_gallery(&opts).unwrap();
    let gallery = fs::read(out.path().join("gallery.json")).unwrap();
    let state = fs::read(out.path().join(".publisher/state.json")).unwrap();
    fs::write(out.path().join(".publisher/journal.json"), b"not json").unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    assert_eq!(gallery, fs::read(out.path().join("gallery.json")).unwrap());
    assert_eq!(
        state,
        fs::read(out.path().join(".publisher/state.json")).unwrap()
    );
}

#[test]
fn test_invalid_journal_candidate_is_not_automatically_valid() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Candidate".into(),
    };
    build_local_gallery(&opts).unwrap();
    fs::write(out.path().join(".publisher/journal.json.new"), b"invalid").unwrap();
    let recovered = photo_publisher_pipeline::recover_publication(out.path()).unwrap();
    assert_eq!(
        recovered.unwrap().phase,
        photo_publisher_pipeline::journal::JournalPhase::Committed
    );
}

fn changed_jpeg() -> Vec<u8> {
    let mut bytes = TINY_JPEG.to_vec();
    bytes.push(0);
    bytes
}

fn recovery_options(src: &tempfile::TempDir, out: &tempfile::TempDir) -> PipelineOptions {
    PipelineOptions {
        source_dir: src.path().to_path_buf(),
        output_dir: out.path().to_path_buf(),
        project_title: "Recovery blockers".into(),
    }
}

#[test]
fn recovery_does_not_promote_when_staging_is_missing() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    fs::write(src.path().join("photo.jpg"), changed_jpeg()).unwrap();
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterStateReplacement)).is_err()
    );
    let journal = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    fs::remove_dir_all(out.path().join(&journal.staging_directory)).unwrap();
    let gallery = fs::read(out.path().join("gallery.json")).unwrap();
    let state = fs::read(out.path().join(".publisher/state.json")).unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    assert_eq!(gallery, fs::read(out.path().join("gallery.json")).unwrap());
    assert_eq!(
        state,
        fs::read(out.path().join(".publisher/state.json")).unwrap()
    );
}

#[test]
fn recovery_does_not_promote_when_staged_record_is_corrupted() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    fs::write(src.path().join("photo.jpg"), changed_jpeg()).unwrap();
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterStateReplacement)).is_err()
    );
    let journal = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    fs::write(
        out.path()
            .join(&journal.staging_directory)
            .join("record.json"),
        b"corrupt",
    )
    .unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
}

#[test]
fn recovery_promotes_when_staging_and_active_hashes_are_coherent() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    fs::write(src.path().join("photo.jpg"), changed_jpeg()).unwrap();
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterStateReplacement)).is_err()
    );
    assert_eq!(
        photo_publisher_pipeline::recover_publication(out.path())
            .unwrap()
            .unwrap()
            .phase,
        photo_publisher_pipeline::journal::JournalPhase::Committed
    );
}

#[test]
fn committed_external_gallery_change_is_an_error_without_rollback() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    fs::write(out.path().join("gallery.json"), b"external gallery").unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    assert_eq!(
        fs::read(out.path().join("gallery.json")).unwrap(),
        b"external gallery"
    );
}

#[test]
fn committed_external_state_change_is_an_error_without_rollback() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    fs::write(out.path().join(".publisher/state.json"), b"external state").unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    assert_eq!(
        fs::read(out.path().join(".publisher/state.json")).unwrap(),
        b"external state"
    );
}

#[test]
fn committed_both_external_changes_are_not_rolled_back() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    fs::write(out.path().join("gallery.json"), b"external gallery").unwrap();
    fs::write(out.path().join(".publisher/state.json"), b"external state").unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    assert_eq!(
        fs::read(out.path().join("gallery.json")).unwrap(),
        b"external gallery"
    );
    assert_eq!(
        fs::read(out.path().join(".publisher/state.json")).unwrap(),
        b"external state"
    );
}

#[test]
fn partial_restore_is_detected_and_completed_on_next_recovery() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let old_gallery = fs::read(out.path().join("gallery.json")).unwrap();
    let old_state = fs::read(out.path().join(".publisher/state.json")).unwrap();
    fs::write(src.path().join("photo.jpg"), changed_jpeg()).unwrap();
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterStateReplacement)).is_err()
    );
    fs::write(out.path().join("gallery.json"), &old_gallery).unwrap();
    assert!(
        recover_publication_with_fault(out.path(), Some(FaultPoint::BeforeRestoreState)).is_err()
    );
    assert_eq!(
        fs::read(out.path().join("gallery.json")).unwrap(),
        old_gallery
    );
    assert_ne!(
        fs::read(out.path().join(".publisher/state.json")).unwrap(),
        old_state
    );
    assert_eq!(
        photo_publisher_pipeline::recover_publication(out.path())
            .unwrap()
            .unwrap()
            .phase,
        photo_publisher_pipeline::journal::JournalPhase::RolledBack
    );
    assert_eq!(
        fs::read(out.path().join("gallery.json")).unwrap(),
        old_gallery
    );
    assert_eq!(
        fs::read(out.path().join(".publisher/state.json")).unwrap(),
        old_state
    );
}

#[test]
fn valid_new_journal_with_incoherent_staging_is_rejected() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let mut candidate = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    candidate.journal_sequence += 1;
    candidate.phase = photo_publisher_pipeline::journal::JournalPhase::StateInstalled;
    candidate.staging_directory = ".publisher/staging/missing-generation".into();
    candidate.record_sha256 = candidate.compute_record_sha256().unwrap();
    fs::write(
        out.path().join(".publisher/journal.json.new"),
        serde_json::to_vec_pretty(&candidate).unwrap(),
    )
    .unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
}

fn prepare_rolled_back_publication() -> (tempfile::TempDir, Vec<u8>, Vec<u8>) {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let old_gallery = fs::read(out.path().join("gallery.json")).unwrap();
    let old_state = fs::read(out.path().join(".publisher/state.json")).unwrap();
    fs::write(src.path().join("photo.jpg"), changed_jpeg()).unwrap();
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterGalleryReplacement)).is_err()
    );
    assert_eq!(
        photo_publisher_pipeline::recover_publication(out.path())
            .unwrap()
            .unwrap()
            .phase,
        JournalPhase::RolledBack
    );
    (out, old_gallery, old_state)
}

#[test]
fn rolled_back_with_correct_active_pair_is_accepted() {
    let (out, old_gallery, old_state) = prepare_rolled_back_publication();
    assert_eq!(
        fs::read(out.path().join("gallery.json")).unwrap(),
        old_gallery
    );
    assert_eq!(
        fs::read(out.path().join(".publisher/state.json")).unwrap(),
        old_state
    );
    assert_eq!(
        photo_publisher_pipeline::recover_publication(out.path())
            .unwrap()
            .unwrap()
            .phase,
        JournalPhase::RolledBack
    );
}

#[test]
fn rolled_back_with_incorrect_active_files_is_rejected() {
    for corrupt_gallery in [true, false] {
        let (out, old_gallery, old_state) = prepare_rolled_back_publication();
        if corrupt_gallery {
            fs::write(out.path().join("gallery.json"), b"wrong gallery").unwrap();
        } else {
            fs::write(out.path().join(".publisher/state.json"), b"wrong state").unwrap();
        }
        assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
        if corrupt_gallery {
            assert_eq!(
                fs::read(out.path().join("gallery.json")).unwrap(),
                b"wrong gallery"
            );
            assert_eq!(
                fs::read(out.path().join(".publisher/state.json")).unwrap(),
                old_state
            );
        } else {
            assert_eq!(
                fs::read(out.path().join("gallery.json")).unwrap(),
                old_gallery
            );
            assert_eq!(
                fs::read(out.path().join(".publisher/state.json")).unwrap(),
                b"wrong state"
            );
        }
    }
}

#[test]
fn intermediate_generation_requires_intact_backup_for_promotion() {
    for corrupt in [false, true] {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
        let opts = recovery_options(&src, &out);
        build_local_gallery(&opts).unwrap();
        fs::write(src.path().join("photo.jpg"), changed_jpeg()).unwrap();
        assert!(
            build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterStateReplacement)).is_err()
        );
        let journal = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
        let backup = out.path().join(&journal.backup_directory);
        if corrupt {
            fs::write(backup.join("gallery.json"), b"corrupted backup").unwrap();
        } else {
            fs::remove_dir_all(backup).unwrap();
        }
        assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    }
}

#[test]
fn duplicate_journal_sequence_is_an_ambiguity_error() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let mut duplicate = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    duplicate.generation = "g-999999".into();
    duplicate.record_sha256 = duplicate.compute_record_sha256().unwrap();
    let history = out.path().join(".publisher/journal/history");
    fs::write(
        history.join("ambiguous.json"),
        serde_json::to_vec_pretty(&duplicate).unwrap(),
    )
    .unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
}

#[test]
fn different_existing_journal_candidate_is_not_overwritten() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let journal_path = out.path().join(".publisher/journal.json");
    let candidate_path = out.path().join(".publisher/journal.json.new");
    let mut candidate = read_journal(&journal_path).unwrap();
    candidate.journal_sequence += 1;
    candidate.generation = "g-999999".into();
    candidate.record_sha256 = candidate.compute_record_sha256().unwrap();
    let original = serde_json::to_vec_pretty(&candidate).unwrap();
    fs::write(&candidate_path, &original).unwrap();
    let current = read_journal(&journal_path).unwrap();
    assert!(update_journal(&out.path().join(".publisher"), &current).is_err());
    assert_eq!(fs::read(candidate_path).unwrap(), original);
}

#[test]
fn equivalent_existing_journal_candidate_is_reused_deterministically() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let publisher_dir = out.path().join(".publisher");
    let journal = read_journal(&publisher_dir.join("journal.json")).unwrap();
    fs::write(
        publisher_dir.join("journal.json.new"),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();
    update_journal(&publisher_dir, &journal).unwrap();
    assert_eq!(
        read_journal(&publisher_dir.join("journal.json")).unwrap(),
        journal
    );
}

#[test]
fn initial_publication_after_gallery_replacement_is_completed() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    assert!(
        build_local_gallery_with_fault(&opts, Some(FaultPoint::AfterGalleryReplacement)).is_err()
    );
    assert_eq!(
        read_journal(&out.path().join(".publisher/journal.json"))
            .unwrap()
            .phase,
        JournalPhase::Prepared
    );
    assert_eq!(
        photo_publisher_pipeline::recover_publication(out.path())
            .unwrap()
            .unwrap()
            .phase,
        JournalPhase::Committed
    );
    assert!(out.path().join("gallery.json").is_file());
    assert!(out.path().join(".publisher/state.json").is_file());
}

#[test]
fn later_orphan_candidate_cannot_supplant_committed_generation() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let current = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    let staging = out.path().join(".publisher/staging/g-999999");
    fs::create_dir_all(&staging).unwrap();
    fs::copy(
        out.path().join("gallery.json"),
        staging.join("gallery.json"),
    )
    .unwrap();
    fs::copy(
        out.path().join(".publisher/state.json"),
        staging.join("state.json"),
    )
    .unwrap();
    let staged_record = photo_publisher_pipeline::journal::ArtifactRecord::new(
        "g-999999".into(),
        current.gallery_sha256.clone(),
        current.state_sha256.clone(),
    );
    fs::write(
        staging.join("record.json"),
        serde_json::to_vec_pretty(&staged_record).unwrap(),
    )
    .unwrap();
    let mut orphan = current.clone();
    orphan.journal_sequence += 1;
    orphan.generation = "g-999999".into();
    orphan.previous_generation = None;
    orphan.phase = JournalPhase::StateInstalled;
    orphan.staging_directory = ".publisher/staging/g-999999".into();
    orphan.backup_directory = ".publisher/backups/none".into();
    orphan.previous_gallery_sha256 = None;
    orphan.previous_state_sha256 = None;
    orphan.record_sha256 = orphan.compute_record_sha256().unwrap();
    fs::write(
        out.path().join(".publisher/journal.json.new"),
        serde_json::to_vec_pretty(&orphan).unwrap(),
    )
    .unwrap();
    let gallery = fs::read(out.path().join("gallery.json")).unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
    assert_eq!(fs::read(out.path().join("gallery.json")).unwrap(), gallery);
}

#[test]
fn corrupted_published_download_is_rejected_by_recovery() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let gallery: serde_json::Value =
        serde_json::from_slice(&fs::read(out.path().join("gallery.json")).unwrap()).unwrap();
    let url = gallery["photos"][0]["download"]["url"].as_str().unwrap();
    fs::write(out.path().join(url), b"corrupt asset").unwrap();
    assert!(photo_publisher_pipeline::recover_publication(out.path()).is_err());
}

#[test]
fn malformed_previous_hashes_return_error_without_panic() {
    let src = tempdir().unwrap();
    let out = tempdir().unwrap();
    fs::write(src.path().join("photo.jpg"), TINY_JPEG).unwrap();
    let opts = recovery_options(&src, &out);
    build_local_gallery(&opts).unwrap();
    let mut malformed = read_journal(&out.path().join(".publisher/journal.json")).unwrap();
    malformed.journal_sequence += 1;
    malformed.generation = "g-000002".into();
    malformed.previous_generation = Some("g-000001".into());
    malformed.previous_gallery_sha256 = None;
    malformed.previous_state_sha256 = None;
    malformed.phase = JournalPhase::StateInstalled;
    malformed.record_sha256 = malformed.compute_record_sha256().unwrap();
    fs::write(
        out.path().join(".publisher/journal.json"),
        serde_json::to_vec_pretty(&malformed).unwrap(),
    )
    .unwrap();
    let gallery = fs::read(out.path().join("gallery.json")).unwrap();
    let result =
        std::panic::catch_unwind(|| photo_publisher_pipeline::recover_publication(out.path()));
    assert!(result.is_ok(), "malformed journal must not panic");
    assert!(result.unwrap().is_err());
    assert_eq!(fs::read(out.path().join("gallery.json")).unwrap(), gallery);
}
