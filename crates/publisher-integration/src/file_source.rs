//! Shared physical validation of publication-sourced files.
//!
//! A publication-sourced repository file (`RepositoryFileSource::PublicationFile`)
//! declares bytes that live in the committed publication output, not in the
//! application bundle. Before any remote effect, the executor revalidates the
//! physical file and keeps an open, rewound handle ready to stream to the
//! provider. The checks mirror the storage side: the file must exist, be a
//! regular file, stay inside the publication root, and match the planned size
//! and SHA-256 exactly. This is intentionally a small `pub(crate)` helper, not
//! a provider trait or a new subsystem.

use std::fs;
use std::io::{self, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};
use photo_publisher_provider_contracts::copy_with_sha256;

use crate::PublicationPath;

/// Validates a publication-sourced file against its planned identity and
/// returns an open handle rewound to the start, ready to stream.
///
/// Pure local reads only: no provider, no network, no ledger, no writes.
pub(crate) fn open_validated_source(
    output_dir: &Path,
    canonical_root: &Path,
    source_path: &PublicationPath,
    expected_sha256: &str,
    expected_size_bytes: u64,
) -> Result<fs::File> {
    let source = source_path.as_str();
    let physical = output_dir.join(source);
    let metadata = fs::metadata(&physical)
        .with_context(|| format!("publication file does not exist: {source}"))?;
    if !metadata.is_file() {
        bail!("publication file is not a regular file: {source}");
    }
    let canonical = physical
        .canonicalize()
        .with_context(|| format!("failed to resolve publication file: {source}"))?;
    if canonical.strip_prefix(canonical_root).is_err() {
        bail!("publication file escapes the publication root: {source}");
    }
    if metadata.len() != expected_size_bytes {
        bail!(
            "publication file size {} does not match the desired {} bytes: {source}",
            metadata.len(),
            expected_size_bytes
        );
    }
    let mut file = fs::File::open(&physical)
        .with_context(|| format!("failed to open publication file: {source}"))?;
    let (bytes, sha256) = copy_with_sha256(&mut file, &mut io::sink())
        .with_context(|| format!("failed to hash publication file: {source}"))?;
    if bytes != metadata.len() {
        bail!("publication file changed while it was hashed: {source}");
    }
    if sha256 != expected_sha256 {
        bail!(
            "publication file SHA-256 {sha256} does not match the desired {expected_sha256}: {source}"
        );
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind publication file: {source}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use tempfile::tempdir;

    use crate::PublicationPath;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempdir().unwrap();
        let output = root.path().join("output");
        fs::create_dir_all(&output).unwrap();
        let canonical_root = output.canonicalize().unwrap();
        (root, output, canonical_root)
    }

    fn write(output: &Path, relative: &str, bytes: &[u8]) -> crate::PublicationPath {
        let path = output.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        crate::PublicationPath::new(relative).unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", sha2::Sha256::digest(bytes))
    }

    #[test]
    fn valid_source_opens_rewound_and_verified() {
        let (_root, output, canonical_root) = fixture();
        let source = write(&output, "photos/preview/a.jpg", b"preview-bytes");

        let mut file = open_validated_source(
            &output,
            &canonical_root,
            &source,
            &sha(b"preview-bytes"),
            13,
        )
        .unwrap();

        use std::io::Read;
        let mut content = Vec::new();
        file.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"preview-bytes");
    }

    #[test]
    fn missing_non_regular_and_wrong_size_or_hash_are_rejected() {
        let (_root, output, canonical_root) = fixture();
        let source = write(&output, "photos/preview/a.jpg", b"abc");

        let missing = PublicationPath::new("photos/preview/missing.jpg").unwrap();
        assert!(open_validated_source(&output, &canonical_root, &missing, &sha(b""), 0).is_err());

        assert!(open_validated_source(&output, &canonical_root, &source, &sha(b"abc"), 4).is_err());
        assert!(
            open_validated_source(&output, &canonical_root, &source, &sha(b"other"), 3).is_err()
        );

        let directory = output.join("photos/preview/dir.jpg");
        fs::remove_file(output.join("photos/preview/a.jpg")).unwrap();
        fs::create_dir_all(&directory).unwrap();
        let dir_path = PublicationPath::new("photos/preview/dir.jpg").unwrap();
        assert!(open_validated_source(&output, &canonical_root, &dir_path, &sha(b""), 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let (_root, output, canonical_root) = fixture();
        let outside = _root.path().join("outside.jpg");
        fs::write(&outside, b"outside").unwrap();
        let link = output.join("photos/preview");
        fs::create_dir_all(&link).unwrap();
        std::os::unix::fs::symlink(&outside, link.join("a.jpg")).unwrap();
        let source = PublicationPath::new("photos/preview/a.jpg").unwrap();

        assert!(
            open_validated_source(&output, &canonical_root, &source, &sha(b"outside"), 7).is_err()
        );
    }
}
