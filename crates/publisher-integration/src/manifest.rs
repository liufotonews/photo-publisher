//! Public gallery manifest contract.
//!
//! The pipeline produces a local `gallery.json` whose asset URLs are
//! publication-relative (`photos/...`) and verified against the local output.
//! This module derives the **public** manifest served by the site: the same
//! document with each asset URL rewritten to its absolute public URL. The
//! local manifest is never modified.
//!
//! URL rule (single, deterministic; used for both asset classes):
//!
//! ```text
//! public_url = join(target.public_base_url, asset.url)
//! ```
//!
//! The `prefix` fields locate the object inside its target (R2 key or
//! repository path) and are intentionally **not** part of the public URL:
//! `publicBaseUrl` already denotes the serving root of that target (see
//! `fixtures/valid/project.v2.valid.json`). The join removes duplicate
//! slashes at the boundary, preserves the base as-is (scheme, host, path —
//! validated by [`PublicBaseUrl`]), performs no percent-encoding or Unicode
//! normalization, and rejects anything that is not a safe publication
//! relative path by reusing [`crate::PublicationPath`].

use std::path::PathBuf;

use anyhow::{Context, Result};
use photo_publisher_contract_validator::{compile_schema, validate_value};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ApplicationBundle, ProjectPublicationConfig, PublicBaseUrl, PublicationPath};

/// The final public `gallery.json`: canonical bytes plus their SHA-256.
///
/// The digest is computed over exactly the bytes returned by
/// [`bytes`](Self::bytes), so the same payload can be reused identically as a
/// repository file and as an application bundle member without a second,
/// independent serialization living elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicGalleryManifest {
    bytes: Vec<u8>,
    sha256: String,
}

impl PublicGalleryManifest {
    /// Derives the public manifest from a committed local `gallery.json` and
    /// the publication configuration.
    ///
    /// Pure and deterministic: no filesystem, no network, same input always
    /// produces the same bytes. The input is expected to be a gallery
    /// manifest that is already valid; the derived result should be validated
    /// with [`validate_public_gallery_manifest`] before publication.
    pub fn derive(local_manifest: &[u8], configuration: &ProjectPublicationConfig) -> Result<Self> {
        let mut value: Value = serde_json::from_slice(local_manifest)
            .context("local gallery manifest is not valid JSON")?;
        let photos = value
            .get_mut("photos")
            .and_then(Value::as_array_mut)
            .context("local gallery manifest has no photos array")?;
        for photo in photos {
            let photo = photo
                .as_object_mut()
                .context("gallery photo must be an object")?;
            let preview = photo
                .get_mut("preview")
                .context("gallery photo preview is required")?;
            rewrite_asset_url(preview, &configuration.preview_public_base_url)?;
            if let Some(download) = photo.get_mut("download") {
                rewrite_asset_url(download, &configuration.high_resolution_public_base_url)?;
            }
        }
        let bytes = serde_json::to_vec(&value).expect("derived manifest is serializable");
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        Ok(Self { bytes, sha256 })
    }

    /// Canonical serialized bytes of the public manifest.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// SHA-256 of [`bytes`](Self::bytes), in lowercase hex.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The manifest as text; the bytes are always valid UTF-8 JSON.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("public manifest bytes are UTF-8 JSON")
    }

    /// Derives the public manifest from the journal-committed `gallery.json`
    /// of a local publication.
    ///
    /// The committed journal authenticates the active manifest's SHA-256
    /// before any transformation, so the derivation can never silently read a
    /// tampered or stale `gallery.json`. Prefer this over reading the file
    /// directly.
    pub fn derive_from_committed_output(
        configuration: &ProjectPublicationConfig,
        output_dir: impl AsRef<std::path::Path>,
    ) -> Result<Self> {
        let output_dir = output_dir.as_ref();
        // from_output authenticates the active gallery manifest against the
        // committed journal (phase + hashes) before returning.
        let _local = crate::LocalPublication::from_output(output_dir)?;
        let bytes = std::fs::read(output_dir.join("gallery.json"))
            .context("failed to read the committed local gallery manifest")?;
        Self::derive(&bytes, configuration)
    }
}

/// Composes the final application bundle: the template files plus the public
/// `gallery.json` at the canonical root path.
///
/// The manifest is validated **before** composition, so an invalid manifest
/// can never enter a bundle. A template file named `gallery.json` is
/// explicitly replaced by the public manifest: exactly one `gallery.json`
/// exists in the result, with exactly [`PublicGalleryManifest::bytes`]. The
/// composition is pure in-memory: it never writes files, never touches the
/// ledger, and never calls a provider. The resulting
/// [`ApplicationBundle::fingerprint`] covers the manifest bytes.
pub fn compose_application_bundle(
    template: &ApplicationBundle,
    manifest: &PublicGalleryManifest,
) -> Result<ApplicationBundle> {
    validate_public_gallery_manifest(manifest)?;
    let mut files: Vec<(String, Vec<u8>)> = template
        .files()
        .iter()
        .filter(|file| file.path().as_str() != "gallery.json")
        .map(|file| (file.path().as_str().to_owned(), file.content().to_vec()))
        .collect();
    files.push(("gallery.json".to_owned(), manifest.bytes().to_vec()));
    Ok(ApplicationBundle::from_files(files)?)
}

/// Validates a derived public manifest against `gallery.schema.json`.
///
/// Not part of the pure derivation: this helper reads the schema file.
pub fn validate_public_gallery_manifest(manifest: &PublicGalleryManifest) -> Result<()> {
    let schema_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/gallery.schema.json");
    let validator = compile_schema(schema_path)?;
    let value: Value = serde_json::from_slice(manifest.bytes())
        .expect("derived manifest was serialized from a JSON value");
    validate_value(&validator, &value)
}

/// Rewrites one asset's `url` field from publication-relative to its absolute
/// public URL, preserving every other field of the asset object.
fn rewrite_asset_url(asset: &mut Value, base: &PublicBaseUrl) -> Result<()> {
    let object = asset
        .as_object_mut()
        .context("gallery asset must be an object")?;
    let relative = object
        .get("url")
        .and_then(Value::as_str)
        .context("gallery asset url must be a string")?;
    object.insert("url".to_owned(), Value::String(public_url(base, relative)?));
    Ok(())
}

/// join(base, path): exactly one slash at the boundary; the base is preserved
/// verbatim except for trailing slashes; the path must be a safe
/// publication-relative path (no absolutes, drives, URLs, backslashes, or
/// `..`), enforced by [`PublicationPath`].
fn public_url(base: &PublicBaseUrl, relative: &str) -> Result<String> {
    let relative = PublicationPath::new(relative)
        .context("gallery asset url must be a safe publication-relative path")?;
    Ok(format!(
        "{}/{}",
        base.as_str().trim_end_matches('/'),
        relative.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostingPublicationConfig;
    use serde_json::json;

    fn configuration() -> ProjectPublicationConfig {
        ProjectPublicationConfig {
            project_id: "project".to_owned(),
            bundle_directory: "site".into(),
            repository_provider: "repository".to_owned(),
            repository: "owner/project".to_owned(),
            branch: Some("main".to_owned()),
            preview_prefix: Some("public/photos".to_owned()),
            preview_public_base_url: PublicBaseUrl::parse("https://cdn.example.com/previews/")
                .unwrap(),
            high_resolution_account_id: "account".to_owned(),
            high_resolution_bucket: Some("bucket".to_owned()),
            high_resolution_prefix: Some("joao-maria-2026".to_owned()),
            high_resolution_public_base_url: PublicBaseUrl::parse(
                "https://downloads.example.com/joao-maria-2026/",
            )
            .unwrap(),
            hosting: HostingPublicationConfig::new("hosting-project", None).unwrap(),
        }
    }

    fn local_manifest() -> Value {
        json!({
            "schemaVersion": 1,
            "gallery": {
                "id": "gallery-local",
                "title": "Título",
                "date": "2026-09-01",
                "description": "descrição álbum"
            },
            "photos": [
                {
                    "id": "photo-1",
                    "filename": "a.jpg",
                    "sequence": 1,
                    "preview": {"url": "photos/preview/aa.jpg", "width": 800, "height": 600},
                    "download": {
                        "url": "photos/download/cc.jpg",
                        "width": 4000,
                        "height": 3000,
                        "sizeBytes": 123,
                        "mimeType": "image/jpeg"
                    },
                    "alt": "foto a",
                    "caption": "legenda a"
                },
                {
                    "id": "photo-2",
                    "filename": "b.jpg",
                    "sequence": 2,
                    "preview": {"url": "photos/preview/bb.jpg", "width": 900, "height": 700}
                }
            ]
        })
    }

    fn local_bytes() -> Vec<u8> {
        serde_json::to_vec_pretty(&local_manifest()).unwrap()
    }

    #[test]
    fn public_manifest_rewrites_urls_and_preserves_content() {
        let manifest = PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap();
        let public: Value = serde_json::from_slice(manifest.bytes()).unwrap();
        let photos = public["photos"].as_array().unwrap();

        assert_eq!(
            photos[0]["preview"]["url"],
            "https://cdn.example.com/previews/photos/preview/aa.jpg"
        );
        assert_eq!(
            photos[0]["download"]["url"],
            "https://downloads.example.com/joao-maria-2026/photos/download/cc.jpg"
        );
        // Semantics preserved verbatim: metadata, dimensions, optionals.
        assert_eq!(public["gallery"]["title"], "Título");
        assert_eq!(public["gallery"]["date"], "2026-09-01");
        assert_eq!(public["gallery"]["description"], "descrição álbum");
        assert_eq!(photos[0]["filename"], "a.jpg");
        assert_eq!(photos[1]["sequence"], 2);
        assert_eq!(photos[0]["preview"]["width"], 800);
        assert_eq!(photos[0]["preview"]["height"], 600);
        assert_eq!(photos[0]["download"]["sizeBytes"], 123);
        assert_eq!(photos[0]["download"]["mimeType"], "image/jpeg");
        assert_eq!(photos[0]["alt"], "foto a");
        assert_eq!(photos[0]["caption"], "legenda a");
        // A photo without a download asset simply keeps having none.
        assert!(photos[1].get("download").is_none());
        assert_eq!(
            photos[1]["preview"]["url"],
            "https://cdn.example.com/previews/photos/preview/bb.jpg"
        );
    }

    #[test]
    fn preview_and_download_urls_use_their_own_base_urls() {
        let mut configuration = configuration();
        configuration.preview_public_base_url =
            PublicBaseUrl::parse("https://cdn.example.com/previews").unwrap();
        configuration.high_resolution_public_base_url =
            PublicBaseUrl::parse("https://downloads.example.com/joao-maria-2026").unwrap();
        let manifest = PublicGalleryManifest::derive(&local_bytes(), &configuration).unwrap();
        let public: Value = serde_json::from_slice(manifest.bytes()).unwrap();
        assert_eq!(
            public["photos"][0]["preview"]["url"],
            "https://cdn.example.com/previews/photos/preview/aa.jpg"
        );
        assert_eq!(
            public["photos"][0]["download"]["url"],
            "https://downloads.example.com/joao-maria-2026/photos/download/cc.jpg"
        );
    }

    #[test]
    fn prefixes_do_not_enter_public_urls() {
        let mut without_prefixes = configuration();
        without_prefixes.preview_prefix = None;
        without_prefixes.high_resolution_prefix = None;
        let with_prefixes =
            PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap();
        let without_prefixes =
            PublicGalleryManifest::derive(&local_bytes(), &without_prefixes).unwrap();
        // The prefix locates the object inside its target (R2 key, repository
        // path); the public URL is anchored only at the serving root.
        assert_eq!(with_prefixes, without_prefixes);
    }

    #[test]
    fn join_removes_duplicate_slashes_at_the_boundary() {
        let mut trailing = configuration();
        trailing.preview_public_base_url =
            PublicBaseUrl::parse("https://cdn.example.com/previews/").unwrap();
        let mut plain = configuration();
        plain.preview_public_base_url =
            PublicBaseUrl::parse("https://cdn.example.com/previews").unwrap();
        let one = PublicGalleryManifest::derive(&local_bytes(), &trailing).unwrap();
        let two = PublicGalleryManifest::derive(&local_bytes(), &plain).unwrap();
        assert_eq!(one, two);
        assert!(!one.as_str().contains("previews//photos"));
        assert!(one
            .as_str()
            .contains("https://cdn.example.com/previews/photos/"));
        // The scheme separator is never collapsed.
        assert!(one.as_str().contains("https://"));
    }

    #[test]
    fn unicode_paths_are_preserved_verbatim() {
        let mut local = local_manifest();
        local["photos"][0]["preview"]["url"] = json!("photos/preview/álbum-é.jpg");
        let bytes = serde_json::to_vec(&local).unwrap();
        let manifest = PublicGalleryManifest::derive(&bytes, &configuration()).unwrap();
        assert!(manifest
            .as_str()
            .contains("https://cdn.example.com/previews/photos/preview/álbum-é.jpg"));
        assert!(!manifest.as_str().contains("%C3%A1")); // no percent-encoding
    }

    #[test]
    fn invalid_public_base_urls_are_rejected_by_the_shared_validator() {
        // PublicBaseUrl is only constructible through `parse`, so an invalid
        // base can never reach the join rule.
        for invalid in [
            "not a url",
            "https://localhost/photos",
            "https://127.0.0.1/x",
            "ftp://example.com/x",
            "https://example.com/x?y=1",
        ] {
            assert!(PublicBaseUrl::parse(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn unsafe_or_absolute_asset_paths_are_rejected() {
        for url in [
            "https://evil.example.com/x.jpg",
            "http://evil.example.com/x.jpg",
            "/photos/download/a.jpg",
            "photos/../x.jpg",
            "photos\\download\\a.jpg",
            "C:/photos/x.jpg",
            "photos//download/a.jpg",
        ] {
            let mut local = local_manifest();
            local["photos"][0]["preview"]["url"] = json!(url);
            let bytes = serde_json::to_vec(&local).unwrap();
            assert!(
                PublicGalleryManifest::derive(&bytes, &configuration()).is_err(),
                "accepted {url}"
            );
        }
    }

    #[test]
    fn derivation_is_deterministic_byte_for_byte() {
        let one = PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap();
        let two = PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap();
        assert_eq!(one, two);
        assert_eq!(one.sha256(), two.sha256());
        // Pretty or compact input produces the same canonical public bytes.
        let compact = serde_json::to_vec(&local_manifest()).unwrap();
        let three = PublicGalleryManifest::derive(&compact, &configuration()).unwrap();
        assert_eq!(one, three);
    }

    #[test]
    fn sha256_is_computed_over_the_returned_bytes() {
        let manifest = PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap();
        assert_eq!(
            manifest.sha256(),
            format!("{:x}", Sha256::digest(manifest.bytes()))
        );
        assert_eq!(
            manifest.sha256(),
            format!("{:x}", Sha256::digest(manifest.as_str().as_bytes()))
        );
    }

    #[test]
    fn local_manifest_is_never_modified() {
        let input = local_bytes();
        let before = input.clone();
        let manifest = PublicGalleryManifest::derive(&input, &configuration()).unwrap();
        assert_eq!(input, before);
        let local: Value = serde_json::from_slice(&input).unwrap();
        assert_eq!(
            local["photos"][0]["preview"]["url"],
            "photos/preview/aa.jpg"
        );
        assert_ne!(manifest.bytes(), input);
        assert!(manifest.as_str().contains("https://"));
    }

    #[test]
    fn derived_manifest_validates_against_the_gallery_schema() {
        let manifest = PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap();
        validate_public_gallery_manifest(&manifest).unwrap();

        // A structurally valid derivation over semantically invalid content
        // (duplicate photo ids) must fail schema validation.
        let mut local = local_manifest();
        local["photos"][1]["id"] = json!("photo-1");
        let bytes = serde_json::to_vec(&local).unwrap();
        let manifest = PublicGalleryManifest::derive(&bytes, &configuration()).unwrap();
        assert!(validate_public_gallery_manifest(&manifest).is_err());
    }

    #[test]
    fn invalid_local_manifest_is_rejected() {
        assert!(PublicGalleryManifest::derive(b"not json", &configuration()).is_err());

        let without_photos = json!({"schemaVersion": 1, "gallery": {"id": "g", "title": "t"}});
        assert!(PublicGalleryManifest::derive(
            &serde_json::to_vec(&without_photos).unwrap(),
            &configuration()
        )
        .is_err());

        let mut missing_preview = local_manifest();
        missing_preview["photos"][0]
            .as_object_mut()
            .unwrap()
            .remove("preview");
        assert!(PublicGalleryManifest::derive(
            &serde_json::to_vec(&missing_preview).unwrap(),
            &configuration()
        )
        .is_err());

        let mut non_string_url = local_manifest();
        non_string_url["photos"][0]["preview"]["url"] = json!(42);
        assert!(PublicGalleryManifest::derive(
            &serde_json::to_vec(&non_string_url).unwrap(),
            &configuration()
        )
        .is_err());

        let mut non_object_photo = local_manifest();
        non_object_photo["photos"][0] = json!("oops");
        assert!(PublicGalleryManifest::derive(
            &serde_json::to_vec(&non_object_photo).unwrap(),
            &configuration()
        )
        .is_err());
    }

    fn template_bundle() -> ApplicationBundle {
        ApplicationBundle::from_files(vec![
            ("assets/app.js".to_owned(), b"app".to_vec()),
            ("index.html".to_owned(), b"index".to_vec()),
        ])
        .unwrap()
    }

    fn derived_manifest() -> PublicGalleryManifest {
        PublicGalleryManifest::derive(&local_bytes(), &configuration()).unwrap()
    }

    #[test]
    fn composition_inserts_gallery_json_at_the_bundle_root() {
        let manifest = derived_manifest();
        let bundle = compose_application_bundle(&template_bundle(), &manifest).unwrap();

        let names: Vec<_> = bundle
            .files()
            .iter()
            .map(|file| file.path().as_str())
            .collect();
        assert_eq!(names, vec!["assets/app.js", "gallery.json", "index.html"]);
    }

    #[test]
    fn composition_replaces_a_template_gallery_json() {
        let template = ApplicationBundle::from_files(vec![
            ("gallery.json".to_owned(), b"template placeholder".to_vec()),
            ("index.html".to_owned(), b"index".to_vec()),
        ])
        .unwrap();
        let manifest = derived_manifest();
        let bundle = compose_application_bundle(&template, &manifest).unwrap();

        // Exactly one gallery.json survives: the public manifest.
        let members: Vec<_> = bundle
            .files()
            .iter()
            .filter(|file| file.path().as_str() == "gallery.json")
            .collect();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].content(), manifest.bytes());
    }

    #[test]
    fn composition_validates_before_composing() {
        // A manifesto inválido (ids duplicados) não entra no bundle e não
        // altera o template.
        let mut local = local_manifest();
        local["photos"][1]["id"] = json!("photo-1");
        let manifest =
            PublicGalleryManifest::derive(&serde_json::to_vec(&local).unwrap(), &configuration())
                .unwrap();
        let template = template_bundle();
        let before = template.clone();
        assert!(compose_application_bundle(&template, &manifest).is_err());
        assert_eq!(template, before);
    }

    #[test]
    fn bundle_fingerprint_covers_the_public_manifest() {
        let template = template_bundle();
        let manifest = derived_manifest();
        let composed = compose_application_bundle(&template, &manifest).unwrap();
        assert_ne!(template.fingerprint(), composed.fingerprint());

        // A G2-style change in the manifest changes the fingerprint.
        let mut g2_local = local_manifest();
        g2_local["photos"].as_array_mut().unwrap().remove(1);
        let manifest2 = PublicGalleryManifest::derive(
            &serde_json::to_vec(&g2_local).unwrap(),
            &configuration(),
        )
        .unwrap();
        let composed2 = compose_application_bundle(&template, &manifest2).unwrap();
        assert_ne!(manifest.sha256(), manifest2.sha256());
        assert_ne!(composed.fingerprint(), composed2.fingerprint());

        // Same inputs: exactly the same fingerprint (determinism).
        let recomposed = compose_application_bundle(&template, &derived_manifest()).unwrap();
        assert_eq!(composed, recomposed);
    }

    #[test]
    fn github_and_bundle_share_the_same_gallery_json_bytes_and_sha() {
        let manifest = derived_manifest();
        let bundle = compose_application_bundle(&template_bundle(), &manifest).unwrap();
        let member = bundle
            .files()
            .iter()
            .find(|file| file.path().as_str() == "gallery.json")
            .unwrap();

        // The bundle member IS the manifest: the same bytes, and therefore
        // the same sha256 that a repository desired entry records.
        assert_eq!(member.content(), manifest.bytes());
        assert_eq!(
            format!("{:x}", Sha256::digest(member.content())),
            manifest.sha256()
        );
    }
}
