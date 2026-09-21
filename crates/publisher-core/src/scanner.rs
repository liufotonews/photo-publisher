use crate::hash::sha256_file;
use crate::state::SourcePhoto;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

pub fn scan_jpegs(root: impl AsRef<Path>) -> Result<Vec<SourcePhoto>> {
    let root = root.as_ref();
    if !root.is_dir() {
        bail!("source directory does not exist: {}", root.display());
    }
    let mut files = Vec::new();
    scan_dir(root, root, &mut files)?;
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

fn scan_dir(root: &Path, directory: &Path, files: &mut Vec<SourcePhoto>) -> Result<()> {
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            scan_dir(root, &path, files)?;
            continue;
        }
        if !is_jpeg(&path) {
            continue;
        }
        let relative_path = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = entry.metadata()?;
        files.push(SourcePhoto {
            relative_path,
            bytes: metadata.len(),
            sha256: sha256_file(&path)?,
        });
    }
    Ok(())
}

fn is_jpeg(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jpg") | Some("jpeg")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_jpeg_accepts_jpg() {
        assert!(is_jpeg(Path::new("photo.jpg")));
    }

    #[test]
    fn is_jpeg_accepts_jpeg_uppercase() {
        assert!(is_jpeg(Path::new("photo.JPEG")));
    }

    #[test]
    fn is_jpeg_rejects_png() {
        assert!(!is_jpeg(Path::new("photo.png")));
    }
}
