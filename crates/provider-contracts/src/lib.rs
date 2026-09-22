//! Generic provider contracts and small provider-neutral filesystem utilities.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("resource was not found")]
    NotFound,
    #[error("resource already exists")]
    AlreadyExists,
    #[error("permission denied")]
    PermissionDenied,
    #[error("authentication is required")]
    AuthenticationRequired,
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("operation conflicts with the current state")]
    Conflict,
    #[error("invalid repository path: {0}")]
    InvalidPath(String),
    #[error("invalid object key: {0}")]
    InvalidKey(String),
    #[error("network operation failed")]
    Network,
    #[error("content integrity check failed")]
    Integrity,
    #[error("I/O operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("operation is unsupported")]
    Unsupported,
    #[error("internal provider error")]
    Other,
}

impl ProviderError {
    pub fn from_io(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            _ => Self::Io(error),
        }
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepositoryPath(String);

impl RepositoryPath {
    pub fn new(value: impl AsRef<str>) -> ProviderResult<Self> {
        let value = value.as_ref();
        validate_logical_path(value, false).map(|_| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for RepositoryPath {
    type Error = ProviderError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectKey(String);

impl ObjectKey {
    pub fn new(value: impl AsRef<str>) -> ProviderResult<Self> {
        let value = value.as_ref();
        validate_logical_path(value, true).map(|_| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = ProviderError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

fn validate_logical_path(value: &str, key: bool) -> ProviderResult<()> {
    let invalid = |message: &str| {
        if key {
            ProviderError::InvalidKey(message.to_owned())
        } else {
            ProviderError::InvalidPath(message.to_owned())
        }
    };

    if value.is_empty() || value.starts_with('/') || value.starts_with('\\') {
        return Err(invalid("must be a non-empty relative path"));
    }
    if value.contains('\0') || value.contains(':') || value.starts_with("//") {
        return Err(invalid("absolute, drive, and UNC paths are not allowed"));
    }
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(invalid(
                "empty, dot, and traversal components are not allowed",
            ));
        }
        if component == ".provider-metadata" {
            return Err(invalid("reserved provider metadata namespace"));
        }
        if component.contains('\\') {
            return Err(invalid("backslashes are not allowed"));
        }
        if component.ends_with('.') || component.ends_with(' ') {
            return Err(invalid("components may not end with a dot or space"));
        }
        if is_reserved_windows_name(component) {
            return Err(invalid("reserved Windows names are not allowed"));
        }
    }
    Ok(())
}

fn is_reserved_windows_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or_default();
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetadata {
    pub path: RepositoryPath,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageObject {
    pub key: ObjectKey,
    pub size_bytes: u64,
    pub sha256: String,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub revision: String,
    pub message: String,
    pub changed_paths: Vec<RepositoryPath>,
}

pub fn copy_with_sha256(
    reader: &mut dyn Read,
    writer: &mut dyn io::Write,
) -> ProviderResult<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    writer.flush()?;
    Ok((size, format!("{:x}", hasher.finalize())))
}

/// Replaces `destination` with a completed file in the same directory.
/// On Windows it uses the same ReplaceFileW/MoveFileExW strategy as Phase 3.
pub fn replace_file_preserving_old(source: &Path, destination: &Path) -> ProviderResult<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(ProviderError::from_io)?;
    }
    #[cfg(windows)]
    {
        replace_windows(source, destination)
    }
    #[cfg(not(windows))]
    {
        fs::rename(source, destination).map_err(ProviderError::from_io)
    }
}

#[cfg(windows)]
fn replace_windows(source: &Path, destination: &Path) -> ProviderResult<()> {
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
        return Err(ProviderError::from_io(io::Error::last_os_error()));
    }
    Ok(())
}

/// Repository operations form a working-change set until `commit` is called.
/// Implementations must group writes and deletes since the previous commit
/// into one logical revision; the local implementation exposes its working
/// tree immediately, while remote implementations may stage changes remotely.
pub trait RepositoryProvider {
    fn ensure_repository(&mut self) -> ProviderResult<()>;
    fn exists(&self, path: &RepositoryPath) -> ProviderResult<bool>;
    fn read(&self, path: &RepositoryPath) -> ProviderResult<Vec<u8>>;
    fn write(
        &mut self,
        path: &RepositoryPath,
        content: &mut dyn Read,
    ) -> ProviderResult<FileMetadata>;
    fn delete(&mut self, path: &RepositoryPath) -> ProviderResult<()>;
    fn commit(&mut self, message: &str) -> ProviderResult<CommitInfo>;
}

pub trait StorageProvider {
    fn put(
        &mut self,
        key: &ObjectKey,
        content: &mut dyn Read,
        content_type: Option<&str>,
    ) -> ProviderResult<StorageObject>;
    fn head(&self, key: &ObjectKey) -> ProviderResult<Option<StorageObject>>;
    fn delete(&mut self, key: &ObjectKey) -> ProviderResult<()>;
}

pub trait HostingProvider {
    fn publish(&self) -> ProviderResult<()>;
}

pub trait CredentialStore {
    fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>>;
    fn set(&mut self, name: &str, secret: &[u8]) -> ProviderResult<()>;
    fn delete(&mut self, name: &str) -> ProviderResult<()>;
}
