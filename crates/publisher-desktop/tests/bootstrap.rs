//! Structural tests for the Tauri bootstrap.
//!
//! These tests verify the desktop application shape without linking or
//! starting any webview runtime: they read the on-disk configuration and the
//! binary source, and confirm that the composition root stays available from
//! the library.

use serde_json::Value;

fn read_config() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json");
    let text = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn bootstrap_configuration_is_minimal_and_plugin_free() {
    let config = read_config();
    assert_eq!(config["productName"], "Photo Publisher");
    assert_eq!(config["identifier"], "com.photopublisher.desktop");
    let windows = config["app"]["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["title"], "Photo Publisher");
    // No plugins are declared anywhere in the configuration.
    assert!(config.get("plugins").is_none());
    let app = config.get("app").unwrap();
    assert!(app.get("plugins").is_none());
}

#[test]
fn bootstrap_never_requires_credentials_and_never_publishes() {
    // The binary entrypoint must not reference the composition root's build
    // functions or any provider verb: opening the app is not a preflight and
    // never executes a publication.
    let main =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    for forbidden in [
        "preflight_publication(",
        "build_providers(",
        ".publish(",
        ".put(",
        ".delete(",
        ".commit(",
        "EnvironmentCredentialStore",
        "std::env::var",
    ] {
        assert!(
            !main.contains(forbidden),
            "desktop bootstrap must not reference {forbidden}"
        );
    }
}

#[test]
fn composition_root_stays_exported_for_the_desktop() {
    // Structural proof that the bootstrap coexists with — and does not
    // replace — the composition root produced in Phase 6-F.2.
    let _preflight: fn(&serde_json::Value, &std::path::Path) -> _ =
        publisher_desktop::composition::preflight_publication;
    let _build: fn(
        &photo_publisher_integration::ProjectPublicationConfig,
    )
        -> Result<publisher_desktop::DesktopProviders, publisher_app::ApplicationError> =
        publisher_desktop::build_providers;
}

#[test]
fn minimal_frontend_exists_without_framework() {
    let index =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/index.html")).unwrap();
    assert!(index.contains("Photo Publisher"));
    // No bundler/framework wiring in this phase.
    for forbidden in ["react", "vite", "tsx", "jsx"] {
        assert!(
            !index.to_lowercase().contains(forbidden),
            "frontend must not reference {forbidden} yet"
        );
    }
}

#[test]
fn command_layer_contains_no_provider_or_network_wiring() {
    // Structural guarantee: the commands module is a pure adapter, so it must
    // not reference concrete providers, credentials, or remote verbs. The
    // test reads the library source from disk to avoid self-reference.
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands.rs")).unwrap();
    for forbidden in [
        "provider_github",
        "provider_r2",
        "provider_vercel",
        "build_providers",
        "preflight_publication",
        "EnvironmentCredentialStore",
        ".publish(",
        ".put(",
        ".delete(",
        ".commit(",
        "reqwest",
    ] {
        assert!(
            !source.contains(forbidden),
            "command layer must not reference {forbidden}"
        );
    }
}
