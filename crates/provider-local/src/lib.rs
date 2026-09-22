//! Filesystem-backed implementations of the generic provider contracts.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use photo_publisher_provider_contracts::{
    copy_with_sha256, replace_file_preserving_old, CommitInfo, FileMetadata, ObjectKey,
    ProviderError, ProviderResult, RepositoryPath, RepositoryProvider, StorageObject,
    StorageProvider,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

#[derive(Debug, Clone)]
pub struct LocalRepositoryProvider {
    root: PathBuf,
    pending_paths: Vec<RepositoryPath>,
}

impl LocalRepositoryProvider {
    pub fn new(root: impl AsRef<Path>) -> ProviderResult<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(ProviderError::from_io)?;
        let root = fs::canonicalize(root).map_err(ProviderError::from_io)?;
        Ok(Self {
            root,
            pending_paths: Vec::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, path: &RepositoryPath) -> ProviderResult<PathBuf> {
        safe_path(&self.root, path.as_str(), false)
    }

    fn mark_changed(&mut self, path: &RepositoryPath) {
        if !self.pending_paths.contains(path) {
            self.pending_paths.push(path.clone());
        }
    }
}

impl RepositoryProvider for LocalRepositoryProvider {
    fn ensure_repository(&mut self) -> ProviderResult<()> {
        Ok(())
    }

    fn exists(&self, path: &RepositoryPath) -> ProviderResult<bool> {
        Ok(self.path_for(path)?.is_file())
    }

    fn read(&self, path: &RepositoryPath) -> ProviderResult<Vec<u8>> {
        fs::read(self.path_for(path)?).map_err(ProviderError::from_io)
    }

    fn write(
        &mut self,
        path: &RepositoryPath,
        content: &mut dyn Read,
    ) -> ProviderResult<FileMetadata> {
        let destination = self.path_for(path)?;
        let parent = destination
            .parent()
            .ok_or_else(|| ProviderError::InvalidPath("path has no parent".to_owned()))?;
        fs::create_dir_all(parent).map_err(ProviderError::from_io)?;
        ensure_inside(&self.root, parent)?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(ProviderError::from_io)?;
        let (size_bytes, sha256) = copy_with_sha256(content, &mut temporary)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(ProviderError::from_io)?;
        let temporary_path = temporary.into_temp_path();
        replace_file_preserving_old(&temporary_path, &destination)?;
        self.mark_changed(path);
        Ok(FileMetadata {
            path: path.clone(),
            size_bytes,
            sha256,
        })
    }

    fn delete(&mut self, path: &RepositoryPath) -> ProviderResult<()> {
        match fs::remove_file(self.path_for(path)?) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ProviderError::from_io(error)),
        }
        self.mark_changed(path);
        Ok(())
    }

    fn commit(&mut self, message: &str) -> ProviderResult<CommitInfo> {
        let mut changed_paths = std::mem::take(&mut self.pending_paths);
        changed_paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let mut hasher = Sha256::new();
        hasher.update(message.as_bytes());
        for path in &changed_paths {
            hasher.update([0]);
            hasher.update(path.as_str().as_bytes());
        }
        Ok(CommitInfo {
            revision: format!("local-{:x}", hasher.finalize()),
            message: message.to_owned(),
            changed_paths,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LocalStorageProvider {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutStage {
    BeforeGeneration,
    AfterBlobWrite,
    AfterBlobValidation,
    AfterManifestPreparation,
    BeforeManifestSwap,
    AfterManifestSwap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    key: String,
    generation: String,
    state: ManifestState,
    blob_path: Option<String>,
    size_bytes: Option<u64>,
    sha256: Option<String>,
    content_type: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ManifestState {
    Present,
    Deleted,
}

impl LocalStorageProvider {
    pub fn new(root: impl AsRef<Path>) -> ProviderResult<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(ProviderError::from_io)?;
        let root = fs::canonicalize(root).map_err(ProviderError::from_io)?;
        fs::create_dir_all(root.join(".provider-metadata/manifests"))
            .map_err(ProviderError::from_io)?;
        fs::create_dir_all(root.join(".provider-metadata/blobs"))
            .map_err(ProviderError::from_io)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &ObjectKey) -> ProviderResult<PathBuf> {
        safe_path(&self.root, key.as_str(), true)
    }

    fn manifest_path(&self, key: &ObjectKey) -> PathBuf {
        self.root
            .join(".provider-metadata/manifests")
            .join(format!("{}.json", sha256_bytes(key.as_str().as_bytes())))
    }

    fn blob_path(&self, generation: &str) -> PathBuf {
        self.root
            .join(".provider-metadata/blobs")
            .join(generation)
            .join("content")
    }

    fn put_internal(
        &mut self,
        key: &ObjectKey,
        content: &mut dyn Read,
        content_type: Option<&str>,
        failure: Option<PutStage>,
    ) -> ProviderResult<StorageObject> {
        fail_at(failure, PutStage::BeforeGeneration)?;
        let generation = format!(
            "g-{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ProviderError::Other)?
                .as_nanos(),
            sha256_bytes(key.as_str().as_bytes())
        );
        let blob = self.blob_path(&generation);
        let parent = blob.parent().ok_or(ProviderError::Other)?;
        fs::create_dir_all(parent).map_err(ProviderError::from_io)?;
        let mut temporary = NamedTempFile::new_in(parent).map_err(ProviderError::from_io)?;
        let (size_bytes, sha256) = copy_with_sha256(content, &mut temporary)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(ProviderError::from_io)?;
        let temporary_path = temporary.into_temp_path();
        fail_at(failure, PutStage::AfterBlobWrite)?;
        replace_file_preserving_old(&temporary_path, &blob)?;
        validate_blob(&blob, size_bytes, &sha256)?;
        fail_at(failure, PutStage::AfterBlobValidation)?;

        let manifest = Manifest {
            version: 1,
            key: key.as_str().to_owned(),
            generation,
            state: ManifestState::Present,
            blob_path: Some(relative_path(&self.root, &blob)?),
            size_bytes: Some(size_bytes),
            sha256: Some(sha256.clone()),
            content_type: content_type.map(str::to_owned),
        };
        validate_manifest(&self.root, key, &manifest)?;
        let manifest_bytes =
            serde_json::to_vec_pretty(&manifest).map_err(|_| ProviderError::Integrity)?;
        let mut manifest_temp = NamedTempFile::new_in(self.manifest_path(key).parent().unwrap())
            .map_err(ProviderError::from_io)?;
        std::io::Write::write_all(&mut manifest_temp, &manifest_bytes)
            .map_err(ProviderError::from_io)?;
        manifest_temp
            .as_file()
            .sync_all()
            .map_err(ProviderError::from_io)?;
        let manifest_temp_path = manifest_temp.into_temp_path();
        fail_at(failure, PutStage::AfterManifestPreparation)?;

        let public_path = self.path_for(key)?;
        materialize(&blob, &public_path)?;
        fail_at(failure, PutStage::BeforeManifestSwap)?;
        replace_file_preserving_old(&manifest_temp_path, &self.manifest_path(key))?;
        fail_at(failure, PutStage::AfterManifestSwap)?;
        Ok(StorageObject {
            key: key.clone(),
            size_bytes,
            sha256,
            content_type: content_type.map(str::to_owned),
        })
    }
}

impl StorageProvider for LocalStorageProvider {
    fn put(
        &mut self,
        key: &ObjectKey,
        content: &mut dyn Read,
        content_type: Option<&str>,
    ) -> ProviderResult<StorageObject> {
        self.put_internal(key, content, content_type, None)
    }

    fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>> {
        let manifest_path = self.manifest_path(key);
        let manifest: Manifest = match fs::read(&manifest_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| ProviderError::Integrity)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ProviderError::from_io(error)),
        };
        validate_manifest(&self.root, key, &manifest)?;
        if manifest.state == ManifestState::Deleted {
            return Ok(None);
        }
        let blob_path = self.root.join(manifest.blob_path.as_deref().unwrap());
        let size_bytes = manifest.size_bytes.unwrap();
        let sha256 = manifest.sha256.as_deref().unwrap();
        validate_blob(&blob_path, size_bytes, sha256)?;
        Ok(Some(StorageObject {
            key: key.clone(),
            size_bytes,
            sha256: sha256.to_owned(),
            content_type: manifest.content_type,
        }))
    }

    fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()> {
        let manifest = Manifest {
            version: 1,
            key: key.as_str().to_owned(),
            generation: format!("d-{}", unique_suffix()),
            state: ManifestState::Deleted,
            blob_path: None,
            size_bytes: None,
            sha256: None,
            content_type: None,
        };
        validate_manifest(&self.root, key, &manifest)?;
        let manifest_path = self.manifest_path(key);
        let mut temporary = NamedTempFile::new_in(manifest_path.parent().unwrap())
            .map_err(ProviderError::from_io)?;
        serde_json::to_writer_pretty(&mut temporary, &manifest)
            .map_err(|_| ProviderError::Integrity)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(ProviderError::from_io)?;
        let temporary_path = temporary.into_temp_path();
        replace_file_preserving_old(&temporary_path, &manifest_path)?;
        let public_path = self.path_for(key)?;
        let _ = fs::remove_file(public_path);
        Ok(())
    }
}

fn fail_at(actual: Option<PutStage>, expected: PutStage) -> ProviderResult<()> {
    if actual == Some(expected) {
        Err(ProviderError::Other)
    } else {
        Ok(())
    }
}

fn unique_suffix() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

fn validate_manifest(root: &Path, key: &ObjectKey, manifest: &Manifest) -> ProviderResult<()> {
    if manifest.version != 1 || manifest.key != key.as_str() || manifest.generation.is_empty() {
        return Err(ProviderError::Integrity);
    }
    match manifest.state {
        ManifestState::Deleted => {
            if manifest.blob_path.is_some()
                || manifest.size_bytes.is_some()
                || manifest.sha256.is_some()
                || manifest.content_type.is_some()
            {
                return Err(ProviderError::Integrity);
            }
        }
        ManifestState::Present => {
            let blob_path = manifest
                .blob_path
                .as_deref()
                .ok_or(ProviderError::Integrity)?;
            manifest.size_bytes.ok_or(ProviderError::Integrity)?;
            let hash = manifest.sha256.as_deref().ok_or(ProviderError::Integrity)?;
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(ProviderError::Integrity);
            }
            let expected_blob_path =
                format!(".provider-metadata/blobs/{}/content", manifest.generation);
            if blob_path != expected_blob_path
                || Path::new(blob_path).components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir
                            | Component::RootDir
                            | Component::Prefix(_)
                            | Component::CurDir
                    )
                })
                || manifest
                    .content_type
                    .as_deref()
                    .is_some_and(|value| value.is_empty() || value.chars().any(char::is_control))
            {
                return Err(ProviderError::Integrity);
            }
            let resolved = root.join(blob_path);
            if !resolved.starts_with(root.join(".provider-metadata/blobs")) {
                return Err(ProviderError::Integrity);
            }
            if !blob_path.starts_with(".provider-metadata/blobs/") {
                return Err(ProviderError::Integrity);
            }
        }
    }
    Ok(())
}

fn validate_blob(path: &Path, expected_size: u64, expected_hash: &str) -> ProviderResult<()> {
    let metadata = fs::metadata(path).map_err(|_| ProviderError::Integrity)?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(ProviderError::Integrity);
    }
    if sha256_file(path)? != expected_hash {
        return Err(ProviderError::Integrity);
    }
    Ok(())
}

fn materialize(source: &Path, destination: &Path) -> ProviderResult<()> {
    let parent = destination.parent().ok_or(ProviderError::Other)?;
    fs::create_dir_all(parent).map_err(ProviderError::from_io)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(ProviderError::from_io)?;
    let mut source_file = File::open(source).map_err(ProviderError::from_io)?;
    std::io::copy(&mut source_file, &mut temporary).map_err(ProviderError::from_io)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(ProviderError::from_io)?;
    let temporary_path = temporary.into_temp_path();
    replace_file_preserving_old(&temporary_path, destination)
}

fn relative_path(root: &Path, path: &Path) -> ProviderResult<String> {
    Ok(path
        .strip_prefix(root)
        .map_err(|_| ProviderError::Integrity)?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn safe_path(root: &Path, logical: &str, key: bool) -> ProviderResult<PathBuf> {
    let mut path = root.to_path_buf();
    for component in logical.split('/') {
        path.push(component);
    }
    if path.exists() {
        ensure_inside(root, &path)?;
    } else if let Some(parent) = path.parent() {
        if parent.exists() {
            ensure_inside(root, parent)?;
        }
    }
    if !path.starts_with(root) {
        return Err(if key {
            ProviderError::InvalidKey("path escapes provider root".to_owned())
        } else {
            ProviderError::InvalidPath("path escapes provider root".to_owned())
        });
    }
    Ok(path)
}

fn ensure_inside(root: &Path, path: &Path) -> ProviderResult<()> {
    let canonical = fs::canonicalize(path).map_err(ProviderError::from_io)?;
    if canonical.starts_with(root) {
        Ok(())
    } else {
        Err(ProviderError::Conflict)
    }
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sha256_file(path: &Path) -> ProviderResult<String> {
    let mut file = File::open(path).map_err(|_| ProviderError::Integrity)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| ProviderError::Integrity)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contract_tests::{repository_contract, storage_contract};
    use std::io::{self, Cursor, Read};

    struct FailingReader;
    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected read failure"))
        }
    }

    fn key() -> ObjectKey {
        ObjectKey::new("album/photo.jpg").unwrap()
    }

    #[test]
    fn reusable_contracts_pass_for_local_providers() {
        let repository_temp = tempfile::tempdir().unwrap();
        let mut repository = LocalRepositoryProvider::new(repository_temp.path()).unwrap();
        repository_contract(&mut repository);
        let storage_temp = tempfile::tempdir().unwrap();
        let mut storage = LocalStorageProvider::new(storage_temp.path()).unwrap();
        storage_contract(&mut storage);
    }

    #[test]
    fn public_cache_is_ignored_by_head() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider
            .put(&key, &mut Cursor::new(b"logical"), Some("image/jpeg"))
            .unwrap();
        let public = provider.path_for(&key).unwrap();
        assert_eq!(provider.head(&key).unwrap().unwrap().size_bytes, 7);
        fs::remove_file(&public).unwrap();
        assert_eq!(provider.head(&key).unwrap().unwrap().size_bytes, 7);
        fs::write(&public, b"different").unwrap();
        assert_eq!(provider.head(&key).unwrap().unwrap().size_bytes, 7);
        fs::write(&public, b"corrupt").unwrap();
        assert_eq!(provider.head(&key).unwrap().unwrap().size_bytes, 7);
    }

    #[test]
    fn deleted_manifest_wins_over_existing_public_cache() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider
            .put(&key, &mut Cursor::new(b"content"), Some("image/jpeg"))
            .unwrap();
        let public = provider.path_for(&key).unwrap();
        provider.delete(&key).unwrap();
        fs::write(public, b"stale public cache").unwrap();
        assert!(provider.head(&key).unwrap().is_none());
    }

    #[test]
    fn overwrite_updates_manifest_content_type_and_none_removes_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider
            .put(&key, &mut Cursor::new(b"jpeg"), Some("image/jpeg"))
            .unwrap();
        assert_eq!(
            provider
                .head(&key)
                .unwrap()
                .unwrap()
                .content_type
                .as_deref(),
            Some("image/jpeg")
        );
        provider
            .put(&key, &mut Cursor::new(b"png"), Some("image/png"))
            .unwrap();
        assert_eq!(
            provider
                .head(&key)
                .unwrap()
                .unwrap()
                .content_type
                .as_deref(),
            Some("image/png")
        );
        provider.put(&key, &mut Cursor::new(b"none"), None).unwrap();
        assert_eq!(provider.head(&key).unwrap().unwrap().content_type, None);
    }

    #[test]
    fn failure_after_public_materialization_keeps_previous_manifest_active() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider
            .put(&key, &mut Cursor::new(b"old"), Some("image/jpeg"))
            .unwrap();
        assert!(provider
            .put_internal(
                &key,
                &mut Cursor::new(b"new"),
                Some("image/png"),
                Some(PutStage::BeforeManifestSwap)
            )
            .is_err());
        let object = provider.head(&key).unwrap().unwrap();
        assert_eq!(object.size_bytes, 3);
        assert_eq!(object.content_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn all_publication_fault_points_have_a_verifiable_head_state() {
        for stage in [
            PutStage::BeforeGeneration,
            PutStage::AfterBlobWrite,
            PutStage::AfterBlobValidation,
            PutStage::AfterManifestPreparation,
            PutStage::BeforeManifestSwap,
            PutStage::AfterManifestSwap,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
            let key = key();
            provider
                .put(
                    &key,
                    &mut Cursor::new(b"old generation"),
                    Some("image/jpeg"),
                )
                .unwrap();
            assert!(provider
                .put_internal(
                    &key,
                    &mut Cursor::new(b"new generation"),
                    Some("image/png"),
                    Some(stage),
                )
                .is_err());
            let object = provider.head(&key).unwrap().unwrap();
            if stage == PutStage::AfterManifestSwap {
                assert_eq!(object.content_type.as_deref(), Some("image/png"));
            } else {
                assert_eq!(object.content_type.as_deref(), Some("image/jpeg"));
            }
            assert!(
                object.size_bytes == b"old generation".len() as u64
                    || object.size_bytes == b"new generation".len() as u64
            );
        }
    }

    #[test]
    fn failing_reader_before_generation_preserves_previous_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider
            .put(&key, &mut Cursor::new(b"old"), Some("image/jpeg"))
            .unwrap();
        assert!(provider
            .put(&key, &mut FailingReader, Some("image/png"))
            .is_err());
        assert_eq!(provider.head(&key).unwrap().unwrap().size_bytes, 3);
    }

    #[test]
    fn corrupt_manifest_is_integrity_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider.put(&key, &mut Cursor::new(b"ok"), None).unwrap();
        fs::write(provider.manifest_path(&key), b"not json").unwrap();
        assert!(matches!(provider.head(&key), Err(ProviderError::Integrity)));
    }

    #[test]
    fn invalid_content_type_in_manifest_is_integrity_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider.put(&key, &mut Cursor::new(b"ok"), None).unwrap();
        let manifest_path = provider.manifest_path(&key);
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        value["content_type"] = serde_json::Value::String("bad\ncontent".to_owned());
        fs::write(manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(provider.head(&key), Err(ProviderError::Integrity)));
    }

    #[test]
    fn delete_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let mut provider = LocalStorageProvider::new(temp.path()).unwrap();
        let key = key();
        provider.delete(&key).unwrap();
        provider.delete(&key).unwrap();
        assert!(provider.head(&key).unwrap().is_none());
    }
}
