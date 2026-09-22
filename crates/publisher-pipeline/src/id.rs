/// Generate the logical photo_id from the relative path.
/// We use the first 16 characters of the SHA-256 of the UTF-8 bytes of the path.
pub fn generate_photo_id(relative_path: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(relative_path.as_bytes());
    let result = hasher.finalize();

    // Convert to hex and take first 16 chars
    let hex = result
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    hex.chars().take(16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_photo_id_generation() {
        let id1 = generate_photo_id("folder/photo.jpg");
        let id2 = generate_photo_id("folder/photo.jpg");
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);

        let id3 = generate_photo_id("folder/other.jpg");
        assert_ne!(id1, id3);
    }
}
