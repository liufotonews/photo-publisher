use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const JOURNAL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JournalPhase {
    Prepared,
    BackupCreated,
    GalleryInstalled,
    StateInstalled,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "journalSequence")]
    pub journal_sequence: u64,
    pub generation: String,
    #[serde(rename = "previousGeneration")]
    pub previous_generation: Option<String>,
    pub phase: JournalPhase,
    #[serde(rename = "gallerySha256")]
    pub gallery_sha256: String,
    #[serde(rename = "stateSha256")]
    pub state_sha256: String,
    #[serde(rename = "galleryPath")]
    pub gallery_path: String,
    #[serde(rename = "statePath")]
    pub state_path: String,
    #[serde(rename = "stagingDirectory")]
    pub staging_directory: String,
    #[serde(rename = "backupDirectory")]
    pub backup_directory: String,
    #[serde(rename = "previousGallerySha256")]
    pub previous_gallery_sha256: Option<String>,
    #[serde(rename = "previousStateSha256")]
    pub previous_state_sha256: Option<String>,
    #[serde(rename = "recordSha256")]
    pub record_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub generation: String,
    #[serde(rename = "gallerySha256")]
    pub gallery_sha256: String,
    #[serde(rename = "stateSha256")]
    pub state_sha256: String,
    #[serde(rename = "recordSha256")]
    pub record_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveStatus {
    Old,
    New,
    Missing,
    Corrupted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveClassification {
    pub gallery: ActiveStatus,
    pub state: ActiveStatus,
}

impl JournalRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sequence: u64,
        generation: String,
        previous_generation: Option<String>,
        phase: JournalPhase,
        gallery_sha256: String,
        state_sha256: String,
        staging_directory: String,
        backup_directory: String,
        previous_gallery_sha256: Option<String>,
        previous_state_sha256: Option<String>,
    ) -> Self {
        let mut record = Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            journal_sequence: sequence,
            generation,
            previous_generation,
            phase,
            gallery_sha256,
            state_sha256,
            gallery_path: "gallery.json".into(),
            state_path: ".publisher/state.json".into(),
            staging_directory,
            backup_directory,
            previous_gallery_sha256,
            previous_state_sha256,
            record_sha256: String::new(),
        };
        record.record_sha256 = record
            .compute_record_sha256()
            .expect("journal serialization cannot fail");
        record
    }

    pub fn with_phase(&self, phase: JournalPhase) -> Self {
        let mut next = self.clone();
        next.journal_sequence += 1;
        next.phase = phase;
        next.record_sha256 = next
            .compute_record_sha256()
            .expect("journal serialization cannot fail");
        next
    }

    fn canonical_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("journal serialization cannot fail");
        value
            .as_object_mut()
            .expect("journal must serialize as object")
            .remove("recordSha256");
        value
    }

    pub fn compute_record_sha256(&self) -> Result<String> {
        Ok(hex_digest(&serde_json::to_vec(&self.canonical_value())?))
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != JOURNAL_SCHEMA_VERSION {
            bail!("unsupported journal schema version {}", self.schema_version);
        }
        if self.journal_sequence == 0 || !is_generation(&self.generation) {
            bail!("journal sequence and generation are required");
        }
        if let Some(previous) = &self.previous_generation {
            if !is_generation(previous) || previous == &self.generation {
                bail!("invalid previous generation");
            }
        }
        match (
            self.previous_generation.is_some(),
            self.previous_gallery_sha256.is_some(),
            self.previous_state_sha256.is_some(),
        ) {
            (false, false, false) | (true, true, true) => {}
            _ => bail!("previous generation requires both previous hashes"),
        }
        for path in [
            &self.gallery_path,
            &self.state_path,
            &self.staging_directory,
            &self.backup_directory,
        ] {
            if Path::new(path).is_absolute() || path.contains("..") {
                bail!("journal path is not a safe relative path: {path}");
            }
        }
        if !is_sha256(&self.gallery_sha256) || !is_sha256(&self.state_sha256) {
            bail!("journal contains an invalid candidate hash");
        }
        if let Some(hash) = &self.previous_gallery_sha256 {
            if !is_sha256(hash) {
                bail!("invalid previous gallery hash");
            }
        }
        if let Some(hash) = &self.previous_state_sha256 {
            if !is_sha256(hash) {
                bail!("invalid previous state hash");
            }
        }
        if self.record_sha256 != self.compute_record_sha256()? {
            bail!("journal recordSha256 does not match canonical content");
        }
        Ok(())
    }
}

impl ArtifactRecord {
    pub fn new(generation: String, gallery_sha256: String, state_sha256: String) -> Self {
        let mut record = Self {
            generation,
            gallery_sha256,
            state_sha256,
            record_sha256: String::new(),
        };
        record.record_sha256 = record
            .compute_record_sha256()
            .expect("record serialization cannot fail");
        record
    }

    fn canonical_value(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).expect("record serialization cannot fail");
        value.as_object_mut().unwrap().remove("recordSha256");
        value
    }

    fn compute_record_sha256(&self) -> Result<String> {
        Ok(hex_digest(&serde_json::to_vec(&self.canonical_value())?))
    }

    pub fn validate(&self) -> Result<()> {
        if !is_generation(&self.generation)
            || !is_sha256(&self.gallery_sha256)
            || !is_sha256(&self.state_sha256)
            || self.record_sha256 != self.compute_record_sha256()?
        {
            bail!("invalid artifact record");
        }
        Ok(())
    }
}

pub fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    Ok(hex_digest(&fs::read(path).with_context(|| {
        format!("failed to read {}", path.display())
    })?))
}

pub fn read_journal(path: &Path) -> Result<JournalRecord> {
    let record: JournalRecord = serde_json::from_slice(&fs::read(path)?)?;
    record.validate()?;
    Ok(record)
}

pub fn update_journal(publisher_dir: &Path, record: &JournalRecord) -> Result<()> {
    record.validate()?;
    let journal_path = publisher_dir.join("journal.json");
    let candidate_path = publisher_dir.join("journal.json.new");
    let history_dir = publisher_dir.join("journal").join("history");
    fs::create_dir_all(&history_dir)?;

    if journal_path.exists() {
        if let Ok(old) = read_journal(&journal_path) {
            let history_path =
                history_dir.join(format!("journal-{:020}.json", old.journal_sequence));
            if !history_path.exists() {
                fs::copy(&journal_path, &history_path)?;
                if read_journal(&history_path)?.journal_sequence != old.journal_sequence {
                    bail!("journal history verification failed");
                }
            }
        }
    }

    if candidate_path.exists() {
        match read_journal(&candidate_path) {
            Ok(existing) if existing == *record => {}
            Ok(_) => bail!("journal.json.new contains a different candidate"),
            Err(error) => return Err(error).context("journal.json.new exists but is invalid"),
        }
    } else {
        fs::write(&candidate_path, serde_json::to_vec_pretty(record)?)?;
    }
    if read_journal(&candidate_path)? != *record {
        bail!("journal candidate verification failed");
    }
    replace_file_preserving_old(&candidate_path, &journal_path)
}

pub fn replace_file_preserving_old(source: &Path, destination: &Path) -> Result<()> {
    // Atomic replacement prevents an intentional remove-then-rename gap, but
    // neither this operation nor NamedTempFile::persist is an fsync protocol.
    // A power loss may therefore still lose recently written directory/data
    // blocks; recovery protects process crashes, not storage durability.
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if cfg!(windows) {
        replace_windows(source, destination)
    } else {
        fs::rename(source, destination)
            .with_context(|| format!("failed to replace {}", destination.display()))
    }
}

#[cfg(windows)]
fn replace_windows(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH,
    };
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let src = wide(source);
    let dst = wide(destination);
    let ok = if destination.exists() {
        unsafe {
            ReplaceFileW(
                dst.as_ptr(),
                src.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        }
    } else {
        unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), MOVEFILE_WRITE_THROUGH) }
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to replace {}", destination.display()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_windows(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).map_err(Into::into)
}

pub fn classify_active(output_dir: &Path, record: &JournalRecord) -> Result<ActiveClassification> {
    let gallery = classify_one(
        &output_dir.join(&record.gallery_path),
        &record.gallery_sha256,
        record.previous_gallery_sha256.as_deref(),
    )?;
    let state = classify_one(
        &output_dir.join(&record.state_path),
        &record.state_sha256,
        record.previous_state_sha256.as_deref(),
    )?;
    Ok(ActiveClassification { gallery, state })
}

fn classify_one(path: &Path, new_hash: &str, old_hash: Option<&str>) -> Result<ActiveStatus> {
    if !path.exists() {
        return Ok(ActiveStatus::Missing);
    }
    let hash = sha256_file(path)?;
    if hash == new_hash {
        return Ok(ActiveStatus::New);
    }
    if old_hash.is_some_and(|old| hash == old) {
        return Ok(ActiveStatus::Old);
    }
    Ok(ActiveStatus::Corrupted)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_generation(value: &str) -> bool {
    value.len() == 8
        && value.starts_with("g-")
        && value[2..].bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn journal_hash_is_deterministic_without_self_reference() {
        let record = JournalRecord::new(
            1,
            "g-000001".into(),
            None,
            JournalPhase::Prepared,
            "a".repeat(64),
            "b".repeat(64),
            ".publisher/staging/g-000001".into(),
            ".publisher/backups/none".into(),
            None,
            None,
        );
        assert_eq!(
            record.record_sha256,
            record.compute_record_sha256().unwrap()
        );
    }

    #[test]
    fn previous_generation_requires_both_previous_hashes() {
        for (gallery, state) in [
            (None, None),
            (Some("a".repeat(64)), None),
            (None, Some("b".repeat(64))),
        ] {
            let mut record = JournalRecord::new(
                2,
                "g-000002".into(),
                Some("g-000001".into()),
                JournalPhase::StateInstalled,
                "c".repeat(64),
                "d".repeat(64),
                ".publisher/staging/g-000002".into(),
                ".publisher/backups/g-000001".into(),
                gallery,
                state,
            );
            record.record_sha256 = record.compute_record_sha256().unwrap();
            assert!(record.validate().is_err());
        }
    }

    #[cfg(windows)]
    #[test]
    fn replacement_preserves_locked_destination_and_succeeds_after_unlock() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempdir().unwrap();
        let source = dir.path().join("new.json");
        let destination = dir.path().join("old.json");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&destination, b"old").unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&destination)
            .unwrap();
        assert!(replace_file_preserving_old(&source, &destination).is_err());
        drop(locked);
        assert_eq!(std::fs::read(&destination).unwrap(), b"old");
        replace_file_preserving_old(&source, &destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    }
}
