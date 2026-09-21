use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePhoto {
    pub relative_path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherState {
    pub photos: BTreeMap<String, SourcePhoto>,
}

pub fn state_from(photos: &[SourcePhoto]) -> PublisherState {
    PublisherState {
        photos: photos
            .iter()
            .map(|p| (p.relative_path.clone(), p.clone()))
            .collect(),
    }
}

pub fn load_state(path: impl AsRef<Path>) -> Result<PublisherState> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(PublisherState::default());
    }
    serde_json::from_reader(fs::File::open(path)?)
        .with_context(|| format!("invalid state in {}", path.display()))
}

pub fn save_state(path: impl AsRef<Path>, state: &PublisherState) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state)?;
    fs::write(path, format!("{text}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn state_from_empty_slice() {
        let state = state_from(&[]);
        assert!(state.photos.is_empty());
    }

    #[test]
    fn state_from_preserves_all_photos() {
        let photos = [
            SourcePhoto {
                relative_path: "a.jpg".into(),
                bytes: 10,
                sha256: "abc".into(),
            },
            SourcePhoto {
                relative_path: "b.jpg".into(),
                bytes: 20,
                sha256: "def".into(),
            },
        ];
        let state = state_from(&photos);
        assert_eq!(state.photos.len(), 2);
        assert_eq!(state.photos.get("a.jpg").unwrap().bytes, 10);
        assert_eq!(state.photos.get("b.jpg").unwrap().bytes, 20);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let state = load_state(&path).unwrap();
        assert!(state.photos.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");

        let photos = [SourcePhoto {
            relative_path: "test.jpg".into(),
            bytes: 42,
            sha256: "hash".into(),
        }];
        let original_state = state_from(&photos);

        save_state(&path, &original_state).unwrap();
        let loaded_state = load_state(&path).unwrap();

        assert_eq!(original_state, loaded_state);
    }
}
