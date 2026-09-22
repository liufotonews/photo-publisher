use crate::id::generate_photo_id;
use crate::image_ops::ProcessedDimensions;
use photo_publisher_core::state::SourcePhoto;
use serde_json::json;

pub fn create_gallery_json(
    title: &str,
    photos_metadata: &[(SourcePhoto, ProcessedDimensions)],
) -> serde_json::Value {
    let mut photos_array = Vec::new();

    for (idx, (photo, dims)) in photos_metadata.iter().enumerate() {
        let photo_id = generate_photo_id(&photo.relative_path);
        let filename = photo
            .relative_path
            .split('/')
            .next_back()
            .unwrap_or(&photo.relative_path);

        photos_array.push(json!({
            "id": photo_id,
            "filename": filename,
            "sequence": idx + 1,
            "preview": {
                "url": format!("photos/preview/{}.jpg", photo.sha256),
                "width": dims.preview_width,
                "height": dims.preview_height
            },
            "download": {
                "url": format!("photos/download/{}.jpg", photo.sha256),
                "width": dims.orig_width,
                "height": dims.orig_height
            }
        }));
    }

    json!({
        "schemaVersion": 1,
        "gallery": {
            "id": "gallery-local",
            "title": title
        },
        "photos": photos_array
    })
}
