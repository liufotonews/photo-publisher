pub mod id;
pub mod image_ops;
pub mod journal;
pub mod manifest;

use anyhow::{bail, Context, Result};
use image_ops::{process_image, ProcessedDimensions};
use journal::{
    classify_active, read_journal, replace_file_preserving_old, sha256_file, update_journal,
    ActiveStatus, ArtifactRecord, JournalPhase, JournalRecord,
};
use photo_publisher_contract_validator::{compile_schema, validate_value};
use photo_publisher_core::{load_state, plan_sync, scan_jpegs, state_from, SyncAction};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::{tempdir_in, NamedTempFile};

pub struct PipelineOptions {
    pub source_dir: PathBuf,
    pub output_dir: PathBuf,
    pub project_title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    BeforeGalleryReplacement,
    AfterGalleryReplacement,
    BeforeGalleryJournal,
    AfterGalleryJournal,
    BeforeStateReplacement,
    AfterStateReplacement,
    BeforeStateJournal,
    AfterStateJournal,
    BeforeCommitted,
    AfterCommitted,
    AfterRestoreGallery,
    BeforeRestoreState,
}

pub fn build_local_gallery(options: &PipelineOptions) -> Result<()> {
    build_local_gallery_with_fault(options, None)
}

#[doc(hidden)]
pub fn build_local_gallery_with_fault(
    options: &PipelineOptions,
    fault: Option<FaultPoint>,
) -> Result<()> {
    fs::create_dir_all(&options.output_dir)?;
    let publisher_dir = options.output_dir.join(".publisher");
    fs::create_dir_all(&publisher_dir)?;
    fs::create_dir_all(options.output_dir.join("photos/preview"))?;
    fs::create_dir_all(options.output_dir.join("photos/download"))?;

    let lock = ExecutionLock::acquire(&publisher_dir)?;
    let recovered = recover_publication(&options.output_dir)?;

    let previous_state = load_previous_state(&options.output_dir, recovered.as_ref())?;
    let current_photos =
        scan_jpegs(&options.source_dir).context("Failed to scan source directory")?;
    let plan = plan_sync(&previous_state, &current_photos);

    let mut dimensions_cache = BTreeMap::new();
    let mut staged = None;
    if !plan.is_empty() || !options.output_dir.join("gallery.json").exists() {
        let generation = next_generation(recovered.as_ref());
        let staging_dir = publisher_dir.join("staging").join(&generation);
        fs::create_dir_all(&staging_dir)?;
        staged = Some((generation, staging_dir));
    }

    if let Some((generation, staging_dir)) = staged {
        for action in plan {
            match action {
                SyncAction::Add(photo) | SyncAction::Update(photo) => {
                    let source_path = options.source_dir.join(&photo.relative_path);
                    let preview_dest = options
                        .output_dir
                        .join("photos/preview")
                        .join(format!("{}.jpg", photo.sha256));
                    let download_dest = options
                        .output_dir
                        .join("photos/download")
                        .join(format!("{}.jpg", photo.sha256));
                    let dims = process_image(&source_path, &preview_dest, &download_dest)
                        .with_context(|| {
                            format!("Failed to process image {}", photo.relative_path)
                        })?;
                    dimensions_cache.insert(photo.relative_path.clone(), dims);
                }
                SyncAction::Remove { .. } => {}
            }
        }

        let mut final_metadata = Vec::new();
        for photo in &current_photos {
            let dims = if let Some(dims) = dimensions_cache.remove(&photo.relative_path) {
                dims
            } else {
                let preview_path = options
                    .output_dir
                    .join("photos/preview")
                    .join(format!("{}.jpg", photo.sha256));
                let download_path = options
                    .output_dir
                    .join("photos/download")
                    .join(format!("{}.jpg", photo.sha256));
                let (pw, ph) = image::image_dimensions(&preview_path).with_context(|| {
                    format!(
                        "Missing preview for unchanged photo {}",
                        photo.relative_path
                    )
                })?;
                let (ow, oh) = image::image_dimensions(&download_path).with_context(|| {
                    format!(
                        "Missing download for unchanged photo {}",
                        photo.relative_path
                    )
                })?;
                ProcessedDimensions {
                    preview_width: pw,
                    preview_height: ph,
                    orig_width: ow,
                    orig_height: oh,
                }
            };
            final_metadata.push((photo.clone(), dims));
        }

        let gallery_json = manifest::create_gallery_json(&options.project_title, &final_metadata);
        validate_gallery(&gallery_json)?;
        let gallery_path = staging_dir.join("gallery.json");
        write_json(&gallery_path, &gallery_json)?;

        let new_state = state_from(&current_photos);
        let state_path = staging_dir.join("state.json");
        write_json(&state_path, &new_state)?;
        verify_gallery_assets(&options.output_dir, &gallery_json)?;

        let gallery_hash = sha256_file(&gallery_path)?;
        let state_hash = sha256_file(&state_path)?;
        let record =
            ArtifactRecord::new(generation.clone(), gallery_hash.clone(), state_hash.clone());
        write_json(&staging_dir.join("record.json"), &record)?;
        record.validate()?;

        let previous_generation = valid_generation(recovered.as_ref());
        let backup_dir = publisher_dir
            .join("backups")
            .join(previous_generation.as_deref().unwrap_or("none"));
        let previous_gallery_hash = existing_hash(&options.output_dir.join("gallery.json"))?;
        let previous_state_hash = existing_hash(&options.output_dir.join(".publisher/state.json"))?;
        let mut journal = JournalRecord::new(
            1,
            generation,
            previous_generation,
            JournalPhase::Prepared,
            gallery_hash,
            state_hash,
            relative_to_output(&options.output_dir, &staging_dir)?,
            relative_to_output(&options.output_dir, &backup_dir)?,
            previous_gallery_hash,
            previous_state_hash,
        );
        if let Some(existing) = recovered.as_ref() {
            journal.journal_sequence = existing.journal_sequence + 1;
            journal.record_sha256 = journal.compute_record_sha256()?;
        }
        update_journal(&publisher_dir, &journal)?;

        if journal.previous_generation.is_some() {
            create_and_verify_backup(
                &options.output_dir,
                &backup_dir,
                journal.previous_generation.as_deref().unwrap(),
            )?;
            journal = journal.with_phase(JournalPhase::BackupCreated);
            update_journal(&publisher_dir, &journal)?;
        }

        publish_staged(&options.output_dir, &staging_dir, &mut journal, fault)?;
        cleanup_staging(&staging_dir)?;
    }

    drop(lock);
    Ok(())
}

pub fn recover_publication(output_dir: &Path) -> Result<Option<JournalRecord>> {
    recover_publication_with_fault(output_dir, None)
}

#[doc(hidden)]
pub fn recover_publication_with_fault(
    output_dir: &Path,
    fault: Option<FaultPoint>,
) -> Result<Option<JournalRecord>> {
    let publisher_dir = output_dir.join(".publisher");
    fs::create_dir_all(&publisher_dir)?;
    let journal_path = publisher_dir.join("journal.json");
    let candidate_path = publisher_dir.join("journal.json.new");
    let mut candidates = Vec::new();
    for path in [&journal_path, &candidate_path] {
        if path.exists() {
            if let Ok(record) = read_journal(path) {
                candidates.push(record);
            }
        }
    }
    let history_dir = publisher_dir.join("journal/history");
    if history_dir.is_dir() {
        for entry in fs::read_dir(history_dir)? {
            let path = entry?.path();
            if path.is_file() {
                if let Ok(record) = read_journal(&path) {
                    candidates.push(record);
                }
            }
        }
    }

    if candidates.is_empty() {
        let gallery_exists = output_dir.join("gallery.json").exists();
        let state_exists = output_dir.join(".publisher/state.json").exists();
        match (gallery_exists, state_exists) {
            (false, false) => return Ok(None),
            (true, false) | (false, true) => {
                bail!("incomplete initial publication without journal")
            }
            (true, true) => bail!("active files exist without trusted journal metadata"),
        }
    }

    for left in 0..candidates.len() {
        for right in (left + 1)..candidates.len() {
            if candidates[left].journal_sequence == candidates[right].journal_sequence
                && candidates[left] != candidates[right]
            {
                bail!(
                    "ambiguous journalSequence {}",
                    candidates[left].journal_sequence
                );
            }
        }
    }
    candidates.sort_by_key(|record| std::cmp::Reverse(record.journal_sequence));
    let evidence = candidates.clone();
    let mut record = candidates
        .into_iter()
        .next()
        .context("no coherent journal could prove a recoverable publication")?;
    Ok(Some(recover_record(
        output_dir,
        &mut record,
        &evidence,
        fault,
    )?))
}

fn recover_record(
    output_dir: &Path,
    record: &mut JournalRecord,
    candidates: &[JournalRecord],
    fault: Option<FaultPoint>,
) -> Result<JournalRecord> {
    let classification = classify_active(output_dir, record)?;
    validate_generation_chain(record, candidates)?;
    match record.phase {
        JournalPhase::Committed => {
            if classification.gallery == ActiveStatus::New
                && classification.state == ActiveStatus::New
            {
                let gallery: Value =
                    serde_json::from_slice(&fs::read(output_dir.join(&record.gallery_path))?)?;
                validate_gallery(&gallery)?;
                verify_gallery_assets(output_dir, &gallery)?;
                return Ok(record.clone());
            }
            bail!("committed journal does not match active hashes")
        }
        JournalPhase::Prepared => {
            if record.previous_generation.is_none()
                && classification.gallery == ActiveStatus::New
                && classification.state == ActiveStatus::Missing
            {
                validate_staging(output_dir, record)?;
                let state = output_dir
                    .join(&record.staging_directory)
                    .join("state.json");
                replace_staged_file(&state, &output_dir.join(".publisher/state.json"))?;
                if sha256_file(&output_dir.join(".publisher/state.json"))? != record.state_sha256 {
                    bail!("initial recovery installed state hash mismatch");
                }
                *record = record.with_phase(JournalPhase::StateInstalled);
                update_journal(&output_dir.join(".publisher"), record)?;
                *record = record.with_phase(JournalPhase::Committed);
                update_journal(&output_dir.join(".publisher"), record)?;
                return Ok(record.clone());
            }
            if record.previous_generation.is_none()
                && classification.gallery == ActiveStatus::Missing
                && classification.state == ActiveStatus::Missing
            {
                *record = record.with_phase(JournalPhase::RolledBack);
                update_journal(&output_dir.join(".publisher"), record)?;
                return Ok(record.clone());
            }
            rollback_record(output_dir, record, candidates, fault)
        }
        JournalPhase::BackupCreated
        | JournalPhase::GalleryInstalled
        | JournalPhase::StateInstalled => {
            if classification.gallery == ActiveStatus::New
                && classification.state == ActiveStatus::New
            {
                validate_staging(output_dir, record)?;
                verify_backup_for_promotion(output_dir, record)?;
                if record.phase != JournalPhase::StateInstalled {
                    *record = record.with_phase(JournalPhase::StateInstalled);
                    update_journal(&output_dir.join(".publisher"), record)?;
                }
                *record = record.with_phase(JournalPhase::Committed);
                update_journal(&output_dir.join(".publisher"), record)?;
                Ok(record.clone())
            } else {
                rollback_record(output_dir, record, candidates, fault)
            }
        }
        JournalPhase::RolledBack => {
            if record.previous_generation.is_none() {
                if output_dir.join("gallery.json").exists()
                    || output_dir.join(".publisher/state.json").exists()
                {
                    bail!("ROLLED_BACK initial publication has active files");
                }
                return Ok(record.clone());
            }
            verify_backup(output_dir, record)?;
            verify_active_against_previous(output_dir, record)?;
            Ok(record.clone())
        }
    }
}

fn rollback_record(
    output_dir: &Path,
    record: &mut JournalRecord,
    candidates: &[JournalRecord],
    fault: Option<FaultPoint>,
) -> Result<JournalRecord> {
    validate_generation_chain(record, candidates)?;
    if record.previous_generation.is_some() {
        verify_backup(output_dir, record)?;
        restore_backup(output_dir, record, fault)?;
    } else if output_dir.join("gallery.json").exists()
        || output_dir.join(".publisher/state.json").exists()
    {
        bail!("cannot rollback initial publication with active files");
    }
    *record = record.with_phase(JournalPhase::RolledBack);
    update_journal(&output_dir.join(".publisher"), record)?;
    Ok(record.clone())
}

fn publish_staged(
    output_dir: &Path,
    staging_dir: &Path,
    journal: &mut JournalRecord,
    fault: Option<FaultPoint>,
) -> Result<()> {
    let gallery = staging_dir.join("gallery.json");
    let state = staging_dir.join("state.json");
    if fault == Some(FaultPoint::BeforeGalleryReplacement) {
        bail!("fault injection before gallery replacement");
    }
    replace_staged_file(&gallery, &output_dir.join("gallery.json"))?;
    if sha256_file(&output_dir.join("gallery.json"))? != journal.gallery_sha256 {
        bail!("installed gallery hash mismatch");
    }
    if fault == Some(FaultPoint::AfterGalleryReplacement) {
        bail!("fault injection after gallery replacement");
    }
    if fault == Some(FaultPoint::BeforeGalleryJournal) {
        bail!("fault injection before gallery journal");
    }
    *journal = journal.with_phase(JournalPhase::GalleryInstalled);
    update_journal(&output_dir.join(".publisher"), journal)?;
    if fault == Some(FaultPoint::AfterGalleryJournal) {
        bail!("fault injection after gallery journal");
    }
    if fault == Some(FaultPoint::BeforeStateReplacement) {
        bail!("fault injection before state replacement");
    }
    replace_staged_file(&state, &output_dir.join(".publisher/state.json"))?;
    if sha256_file(&output_dir.join(".publisher/state.json"))? != journal.state_sha256 {
        bail!("installed state hash mismatch");
    }
    if fault == Some(FaultPoint::AfterStateReplacement) {
        bail!("fault injection after state replacement");
    }
    if fault == Some(FaultPoint::BeforeStateJournal) {
        bail!("fault injection before state journal");
    }
    *journal = journal.with_phase(JournalPhase::StateInstalled);
    update_journal(&output_dir.join(".publisher"), journal)?;
    if fault == Some(FaultPoint::AfterStateJournal) {
        bail!("fault injection after state journal");
    }
    if fault == Some(FaultPoint::BeforeCommitted) {
        bail!("fault injection before committed");
    }
    if sha256_file(&output_dir.join("gallery.json"))? != journal.gallery_sha256
        || sha256_file(&output_dir.join(".publisher/state.json"))? != journal.state_sha256
    {
        bail!("final active hashes do not match journal");
    }
    *journal = journal.with_phase(JournalPhase::Committed);
    update_journal(&output_dir.join(".publisher"), journal)?;
    if fault == Some(FaultPoint::AfterCommitted) {
        bail!("fault injection after committed");
    }
    Ok(())
}

fn validate_gallery(value: &Value) -> Result<()> {
    let schema_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/gallery.schema.json");
    let validator = compile_schema(schema_path)?;
    validate_value(&validator, value)
}

fn validate_staging(output_dir: &Path, journal: &JournalRecord) -> Result<()> {
    let staging_dir = output_dir.join(&journal.staging_directory);
    if !staging_dir.is_dir() {
        bail!("staging directory is missing: {}", staging_dir.display());
    }
    let gallery_path = staging_dir.join("gallery.json");
    let state_path = staging_dir.join("state.json");
    let record_path = staging_dir.join("record.json");
    if !gallery_path.is_file() || !state_path.is_file() || !record_path.is_file() {
        bail!(
            "staging generation is incomplete: {}",
            staging_dir.display()
        );
    }

    let gallery_bytes = fs::read(&gallery_path)?;
    let gallery: Value =
        serde_json::from_slice(&gallery_bytes).context("staged gallery is not valid JSON")?;
    validate_gallery(&gallery)?;
    verify_gallery_assets(output_dir, &gallery)?;

    let _: photo_publisher_core::PublisherState =
        serde_json::from_slice(&fs::read(&state_path)?).context("staged state is invalid")?;
    let staged_record: ArtifactRecord = serde_json::from_slice(&fs::read(&record_path)?)?;
    staged_record.validate()?;
    if staged_record.generation != journal.generation
        || staged_record.gallery_sha256 != journal.gallery_sha256
        || staged_record.state_sha256 != journal.state_sha256
        || staged_record.gallery_sha256 != sha256_file(&gallery_path)?
        || staged_record.state_sha256 != sha256_file(&state_path)?
    {
        bail!("staging record and journal hashes are inconsistent");
    }
    Ok(())
}

fn verify_gallery_assets(output_dir: &Path, gallery: &Value) -> Result<()> {
    let photos = gallery
        .get("photos")
        .and_then(Value::as_array)
        .context("gallery photos must be an array")?;
    for photo in photos {
        for key in ["preview", "download"] {
            let url = photo
                .get(key)
                .and_then(|a| a.get("url"))
                .and_then(Value::as_str)
                .context("asset URL missing")?;
            let relative = url
                .strip_prefix("photos/")
                .map(|rest| format!("photos/{rest}"))
                .context("asset URL must be relative")?;
            let path = output_dir.join(relative);
            if !path.is_file() {
                bail!("gallery asset does not exist: {url}");
            }
            let (width, height) = image::image_dimensions(&path)
                .with_context(|| format!("gallery asset is not a readable image: {url}"))?;
            let expected_width = photo
                .get(key)
                .and_then(|asset| asset.get("width"))
                .and_then(Value::as_u64)
                .context("asset width missing")? as u32;
            let expected_height = photo
                .get(key)
                .and_then(|asset| asset.get("height"))
                .and_then(Value::as_u64)
                .context("asset height missing")? as u32;
            if (width, height) != (expected_width, expected_height) {
                bail!("gallery asset dimensions mismatch: {url}");
            }
            if key == "download" {
                let expected_hash = Path::new(url)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .context("download asset has no hash filename")?;
                if sha256_file(&path)? != expected_hash {
                    bail!("download asset hash mismatch: {url}");
                }
            }
        }
    }
    Ok(())
}

fn create_and_verify_backup(output_dir: &Path, backup_dir: &Path, generation: &str) -> Result<()> {
    if backup_dir.exists() {
        verify_backup_directory(backup_dir, generation)?;
        return Ok(());
    }
    fs::create_dir_all(backup_dir)?;
    let gallery = output_dir.join("gallery.json");
    let state = output_dir.join(".publisher/state.json");
    if !gallery.is_file() || !state.is_file() {
        bail!("cannot create backup without active pair");
    }
    fs::copy(&gallery, backup_dir.join("gallery.json"))?;
    fs::copy(&state, backup_dir.join("state.json"))?;
    let record = ArtifactRecord::new(
        generation.into(),
        sha256_file(&gallery)?,
        sha256_file(&state)?,
    );
    write_json(&backup_dir.join("record.json"), &record)?;
    verify_backup_directory(backup_dir, generation)
}

fn verify_backup(output_dir: &Path, journal: &JournalRecord) -> Result<()> {
    let backup = output_dir.join(&journal.backup_directory);
    let (previous_generation, previous_gallery, previous_state) = match (
        journal.previous_generation.as_deref(),
        journal.previous_gallery_sha256.as_deref(),
        journal.previous_state_sha256.as_deref(),
    ) {
        (Some(generation), Some(gallery), Some(state)) => (generation, gallery, state),
        (None, None, None) => return Ok(()),
        _ => bail!("incomplete previous-generation backup evidence"),
    };
    if journal.previous_generation.is_none() {
        return Ok(());
    }
    verify_backup_directory(&backup, previous_generation)?;
    if sha256_file(&backup.join("gallery.json"))? != previous_gallery {
        bail!("backup gallery hash mismatch");
    }
    if sha256_file(&backup.join("state.json"))? != previous_state {
        bail!("backup state hash mismatch");
    }
    Ok(())
}

fn verify_backup_for_promotion(output_dir: &Path, journal: &JournalRecord) -> Result<()> {
    if journal.previous_generation.is_some() {
        verify_backup(output_dir, journal)?;
    }
    Ok(())
}

fn verify_active_against_previous(output_dir: &Path, journal: &JournalRecord) -> Result<()> {
    match (
        journal.previous_generation.as_deref(),
        journal.previous_gallery_sha256.as_deref(),
        journal.previous_state_sha256.as_deref(),
    ) {
        (Some(_), Some(gallery_hash), Some(state_hash)) => {
            if sha256_file(&output_dir.join("gallery.json"))? != gallery_hash
                || sha256_file(&output_dir.join(".publisher/state.json"))? != state_hash
            {
                bail!("ROLLED_BACK active files do not match previous generation");
            }
        }
        (None, None, None) => {
            if output_dir.join("gallery.json").exists()
                || output_dir.join(".publisher/state.json").exists()
            {
                bail!("initial ROLLED_BACK state has active files");
            }
        }
        _ => bail!("incomplete previous-generation evidence"),
    }
    Ok(())
}

fn validate_generation_chain(record: &JournalRecord, candidates: &[JournalRecord]) -> Result<()> {
    let Some(previous) = record.previous_generation.as_deref() else {
        if candidates.iter().any(|candidate| {
            candidate.journal_sequence < record.journal_sequence
                && candidate.phase == JournalPhase::Committed
        }) {
            bail!("candidate is orphaned after an existing committed generation");
        }
        return Ok(());
    };
    let predecessor = candidates
        .iter()
        .filter(|candidate| {
            candidate.journal_sequence < record.journal_sequence
                && candidate.phase == JournalPhase::Committed
        })
        .max_by_key(|candidate| candidate.journal_sequence)
        .context("previous generation has no committed evidence")?;
    if predecessor.generation != previous {
        bail!("previous generation is not the latest committed predecessor");
    }
    let previous_gallery = record
        .previous_gallery_sha256
        .as_deref()
        .context("missing previous gallery hash")?;
    let previous_state = record
        .previous_state_sha256
        .as_deref()
        .context("missing previous state hash")?;
    if predecessor.gallery_sha256 != previous_gallery || predecessor.state_sha256 != previous_state
    {
        bail!("previous generation hashes are inconsistent");
    }
    Ok(())
}

fn verify_backup_directory(backup: &Path, generation: &str) -> Result<()> {
    let record: ArtifactRecord = serde_json::from_slice(&fs::read(backup.join("record.json"))?)?;
    record.validate()?;
    if record.generation != generation
        || sha256_file(&backup.join("gallery.json"))? != record.gallery_sha256
        || sha256_file(&backup.join("state.json"))? != record.state_sha256
    {
        bail!("backup verification failed");
    }
    Ok(())
}

fn restore_backup(
    output_dir: &Path,
    journal: &JournalRecord,
    fault: Option<FaultPoint>,
) -> Result<()> {
    let backup = output_dir.join(&journal.backup_directory);
    let temp = tempdir_in(output_dir.join(".publisher"))?;
    let gallery = temp.path().join("gallery.json");
    let state = temp.path().join("state.json");
    fs::copy(backup.join("gallery.json"), &gallery)?;
    fs::copy(backup.join("state.json"), &state)?;
    replace_file_preserving_old(&gallery, &output_dir.join("gallery.json"))?;
    if let Some(hash) = &journal.previous_gallery_sha256 {
        if sha256_file(&output_dir.join("gallery.json"))? != *hash {
            bail!("restored gallery hash mismatch");
        }
    }
    if fault == Some(FaultPoint::AfterRestoreGallery)
        || fault == Some(FaultPoint::BeforeRestoreState)
    {
        bail!("fault injection during backup restore");
    }
    replace_file_preserving_old(&state, &output_dir.join(".publisher/state.json"))?;
    if let Some(hash) = &journal.previous_state_sha256 {
        if sha256_file(&output_dir.join(".publisher/state.json"))? != *hash {
            bail!("restored state hash mismatch");
        }
    }
    Ok(())
}

fn load_previous_state(
    output_dir: &Path,
    journal: Option<&JournalRecord>,
) -> Result<photo_publisher_core::PublisherState> {
    let path = output_dir.join(".publisher/state.json");
    if journal.is_none() {
        return Ok(photo_publisher_core::PublisherState::default());
    }
    load_state(&path).with_context(|| format!("failed to load valid state {}", path.display()))
}

fn valid_generation(journal: Option<&JournalRecord>) -> Option<String> {
    journal.and_then(|record| match record.phase {
        JournalPhase::RolledBack => record.previous_generation.clone(),
        _ => Some(record.generation.clone()),
    })
}

fn next_generation(journal: Option<&JournalRecord>) -> String {
    let number = journal
        .and_then(|record| record.generation.strip_prefix("g-")?.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    format!("g-{number:06}")
}

fn existing_hash(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        Ok(Some(sha256_file(path)?))
    } else {
        Ok(None)
    }
}

fn relative_to_output(output_dir: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(output_dir)?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut temp = NamedTempFile::new_in(path.parent().context("JSON path has no parent")?)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.persist(path)
        .map_err(|error| anyhow::anyhow!(error.error))?;
    Ok(())
}

fn replace_staged_file(source: &Path, destination: &Path) -> Result<()> {
    let parent = source.parent().context("staged file has no parent")?;
    let temporary = NamedTempFile::new_in(parent)?;
    fs::copy(source, temporary.path())?;
    let temporary_path = temporary.into_temp_path();
    replace_file_preserving_old(temporary_path.as_ref(), destination)
}

fn cleanup_staging(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

struct ExecutionLock {
    path: PathBuf,
}

impl ExecutionLock {
    fn acquire(publisher_dir: &Path) -> Result<Self> {
        let path = publisher_dir.join("run.lock");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Self { path }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("another publisher execution is active")
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for ExecutionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_monotonic() {
        assert_eq!(next_generation(None), "g-000001");
    }

    #[test]
    fn generation_chain_requires_latest_committed_predecessor() {
        let g1 = JournalRecord::new(
            10,
            "g-000001".into(),
            None,
            JournalPhase::Committed,
            "a".repeat(64),
            "b".repeat(64),
            ".publisher/staging/g-000001".into(),
            ".publisher/backups/none".into(),
            None,
            None,
        );
        let g2 = JournalRecord::new(
            20,
            "g-000002".into(),
            Some("g-000001".into()),
            JournalPhase::Committed,
            "c".repeat(64),
            "d".repeat(64),
            ".publisher/staging/g-000002".into(),
            ".publisher/backups/g-000001".into(),
            Some("a".repeat(64)),
            Some("b".repeat(64)),
        );
        let g3_from_g1 = JournalRecord::new(
            30,
            "g-000003".into(),
            Some("g-000001".into()),
            JournalPhase::StateInstalled,
            "e".repeat(64),
            "f".repeat(64),
            ".publisher/staging/g-000003".into(),
            ".publisher/backups/g-000001".into(),
            Some("a".repeat(64)),
            Some("b".repeat(64)),
        );
        assert!(validate_generation_chain(&g3_from_g1, &[g1.clone(), g2.clone()]).is_err());
        let g3_from_g2 = JournalRecord::new(
            30,
            "g-000003".into(),
            Some("g-000002".into()),
            JournalPhase::StateInstalled,
            "e".repeat(64),
            "f".repeat(64),
            ".publisher/staging/g-000003".into(),
            ".publisher/backups/g-000002".into(),
            Some("c".repeat(64)),
            Some("d".repeat(64)),
        );
        assert!(validate_generation_chain(&g3_from_g2, &[g1, g2]).is_ok());
    }
}
