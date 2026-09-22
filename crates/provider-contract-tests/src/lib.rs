//! Reusable behavioral contract tests for provider implementations.

use std::io::{self, Cursor, Read};

use photo_publisher_provider_contracts::{
    HostingProvider, ObjectKey, ProviderError, RepositoryPath, RepositoryProvider, StorageProvider,
};

struct FailingReader;

impl Read for FailingReader {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("injected read failure"))
    }
}

pub fn repository_contract(provider: &mut impl RepositoryProvider) {
    provider.ensure_repository().unwrap();
    let path = RepositoryPath::new("álbum/IMG 001 espaço.bin").unwrap();
    let bytes = [0_u8, 1, 2, 127, 255];

    assert!(!provider.exists(&path).unwrap());
    let metadata = provider.write(&path, &mut Cursor::new(bytes)).unwrap();
    assert_eq!(metadata.size_bytes, bytes.len() as u64);
    assert_eq!(metadata.sha256.len(), 64);
    assert!(provider.exists(&path).unwrap());
    assert_eq!(provider.read(&path).unwrap(), bytes);

    let replacement = b"replacement";
    let replaced = provider
        .write(&path, &mut Cursor::new(replacement))
        .unwrap();
    assert_eq!(replaced.size_bytes, replacement.len() as u64);
    assert_eq!(provider.read(&path).unwrap(), replacement);

    let empty = RepositoryPath::new("empty").unwrap();
    let empty_metadata = provider.write(&empty, &mut Cursor::new([])).unwrap();
    assert_eq!(empty_metadata.size_bytes, 0);
    assert_eq!(provider.read(&empty).unwrap(), b"");

    let missing = RepositoryPath::new("missing").unwrap();
    provider.delete(&missing).unwrap();
    assert!(matches!(
        provider.read(&missing),
        Err(ProviderError::NotFound)
    ));
    provider.delete(&path).unwrap();
    provider.delete(&path).unwrap();
    assert!(!provider.exists(&path).unwrap());

    let failed = RepositoryPath::new("failed").unwrap();
    assert!(provider.write(&failed, &mut FailingReader).is_err());
    assert!(!provider.exists(&failed).unwrap());

    for invalid in [
        "../outside",
        r"..\outside",
        r"album\..\outside",
        r"C:\outside",
        "C:/outside",
        r"\outside",
        "/outside",
        r"\server\share",
        "album//file",
        "CON",
        "PRN.txt",
        "AUX",
        "NUL",
        "COM1",
        "LPT1",
        "file.",
        "file ",
    ] {
        assert!(
            RepositoryPath::new(invalid).is_err(),
            "accepted path: {invalid}"
        );
    }

    let commit_path = RepositoryPath::new("commit.txt").unwrap();
    provider
        .write(&commit_path, &mut Cursor::new(b"commit"))
        .unwrap();
    let commit = provider.commit("contract commit").unwrap();
    assert_eq!(commit.message, "contract commit");
    assert!(commit.changed_paths.contains(&commit_path));
    assert!(!commit.revision.is_empty());
}

pub fn storage_contract(provider: &mut impl StorageProvider) {
    let key = ObjectKey::new("álbum/IMG 001 espaço.bin").unwrap();
    let bytes = [0_u8, 1, 2, 127, 255];

    assert!(provider.head(&key).unwrap().is_none());
    let object = provider
        .put(
            &key,
            &mut Cursor::new(bytes),
            Some("application/octet-stream"),
        )
        .unwrap();
    assert_eq!(object.size_bytes, bytes.len() as u64);
    assert_eq!(object.sha256.len(), 64);
    assert_eq!(
        object.content_type.as_deref(),
        Some("application/octet-stream")
    );

    let head = provider.head(&key).unwrap().unwrap();
    assert_eq!(head.size_bytes, bytes.len() as u64);
    assert_eq!(head.sha256, object.sha256);
    assert_eq!(
        head.content_type.as_deref(),
        Some("application/octet-stream")
    );

    let replacement = b"replacement";
    provider
        .put(&key, &mut Cursor::new(replacement), None)
        .unwrap();
    let head = provider.head(&key).unwrap().unwrap();
    assert_eq!(head.size_bytes, replacement.len() as u64);
    assert_eq!(head.content_type, None);

    let empty = ObjectKey::new("empty").unwrap();
    let empty_object = provider.put(&empty, &mut Cursor::new([]), None).unwrap();
    assert_eq!(empty_object.size_bytes, 0);

    let missing = ObjectKey::new("missing").unwrap();
    provider.delete(&missing).unwrap();
    provider.delete(&key).unwrap();
    provider.delete(&key).unwrap();
    assert!(provider.head(&key).unwrap().is_none());

    let failed = ObjectKey::new("failed").unwrap();
    assert!(provider.put(&failed, &mut FailingReader, None).is_err());
    assert!(provider.head(&failed).unwrap().is_none());

    for invalid in [
        "../outside",
        r"..\outside",
        r"album\..\outside",
        r"C:\outside",
        "C:/outside",
        r"\outside",
        "/outside",
        r"\server\share",
        "album//file",
        "CON",
        "PRN.txt",
        "AUX",
        "NUL",
        "COM1",
        "LPT1",
        "file.",
        "file ",
    ] {
        assert!(ObjectKey::new(invalid).is_err(), "accepted key: {invalid}");
    }
}

/// Minimal behavioral contract for hosting providers.
pub fn hosting_contract(provider: &impl HostingProvider) {
    let deployment = provider.publish().unwrap();
    assert!(!deployment.id.is_empty());
    assert!(!deployment.url.is_empty());
}

#[cfg(test)]
mod hosting_tests {
    use super::*;
    use photo_publisher_provider_contracts::{DeploymentInfo, ProviderResult};

    struct FakeHosting {
        fails: bool,
    }

    impl HostingProvider for FakeHosting {
        fn publish(&self) -> ProviderResult<DeploymentInfo> {
            if self.fails {
                Err(ProviderError::Network)
            } else {
                Ok(DeploymentInfo {
                    id: "deployment-1".to_owned(),
                    url: "https://example.test".to_owned(),
                })
            }
        }
    }

    #[test]
    fn hosting_contract_requires_deployment_id_and_url() {
        hosting_contract(&FakeHosting { fails: false });
    }

    #[test]
    fn hosting_provider_propagates_provider_error() {
        let provider = FakeHosting { fails: true };
        assert!(matches!(provider.publish(), Err(ProviderError::Network)));
    }
}
