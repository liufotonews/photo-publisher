use crate::journal::{replace_file_preserving_old, sha256_file};
use anyhow::{Context, Result};
use image::{
    imageops::FilterType, metadata::Orientation, GenericImageView, ImageDecoder, ImageFormat,
    ImageReader,
};
use std::fs;
use std::path::Path;
use tempfile::NamedTempFile;

pub struct ProcessedDimensions {
    pub preview_width: u32,
    pub preview_height: u32,
    pub orig_width: u32,
    pub orig_height: u32,
}

pub fn process_image(
    source_path: &Path,
    preview_dest: &Path,
    download_dest: &Path,
) -> Result<ProcessedDimensions> {
    let orientation = ImageReader::open(source_path)
        .ok()
        .and_then(|reader| reader.into_decoder().ok())
        .and_then(|mut decoder| decoder.orientation().ok())
        .unwrap_or(Orientation::NoTransforms);
    let mut image = image::open(source_path)
        .with_context(|| format!("Failed to decode image: {}", source_path.display()))?;
    image.apply_orientation(orientation);
    let (orig_width, orig_height) = image.dimensions();

    safe_copy(source_path, download_dest)?;

    if preview_dest.exists() {
        if let Ok((preview_width, preview_height)) = image::image_dimensions(preview_dest) {
            if preview_dest
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg"))
            {
                return Ok(ProcessedDimensions {
                    preview_width,
                    preview_height,
                    orig_width,
                    orig_height,
                });
            }
        }
    }

    let max_edge = 2048u32;
    let longest_edge = orig_width.max(orig_height);
    let (new_width, new_height) = if longest_edge > max_edge {
        let ratio = max_edge as f64 / longest_edge as f64;
        (
            ((orig_width as f64 * ratio).round() as u32).max(1),
            ((orig_height as f64 * ratio).round() as u32).max(1),
        )
    } else {
        (orig_width.max(1), orig_height.max(1))
    };

    let preview = image.resize(new_width, new_height, FilterType::Lanczos3);
    if let Some(parent) = preview_dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = preview_dest.parent().unwrap_or(Path::new("."));
    let temp = NamedTempFile::new_in(parent)?;
    preview
        .save_with_format(temp.path(), ImageFormat::Jpeg)
        .context("Failed to save JPEG preview")?;
    temp.persist(preview_dest)
        .map_err(|error| anyhow::anyhow!(error.error))
        .context("Failed to persist preview file")?;

    Ok(ProcessedDimensions {
        preview_width: preview.width(),
        preview_height: preview.height(),
        orig_width,
        orig_height,
    })
}

fn safe_copy(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() && sha256_file(src)? == sha256_file(dest)? {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = dest.parent().unwrap_or(Path::new("."));
    let temp = NamedTempFile::new_in(parent)?;
    fs::copy(src, temp.path())?;
    let temporary = temp.into_temp_path();
    replace_file_preserving_old(temporary.as_ref(), dest)?;
    Ok(())
}
