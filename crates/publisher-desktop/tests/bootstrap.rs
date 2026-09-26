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
    // No bundler/framework wiring.
    for forbidden in ["react", "vite", "tsx", "jsx"] {
        assert!(
            !index.to_lowercase().contains(forbidden),
            "frontend must not reference {forbidden} yet"
        );
    }
}

#[test]
fn ui_assets_are_plain_html_js_css_with_no_framework_or_credentials() {
    let index =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/index.html")).unwrap();
    let script =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.js")).unwrap();
    let style =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/style.css")).unwrap();
    // The shell references exactly the two local assets (no CDN, no bundle).
    assert!(index.contains("href=\"style.css\""));
    assert!(index.contains("src=\"app.js\""));
    assert!(!index.contains("https://"));
    assert!(!style.contains("https://"));
    assert!(!style.contains("@import"));
    // The bridge used is the only global one exposed by the desktop config.
    let config_text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json")).unwrap();
    assert!(config_text.contains("\"withGlobalTauri\": true"));
    // Frontend calls only these commands — publish stays out of this phase.
    assert!(script.contains("validate_project"));
    assert!(script.contains("dry_run_project"));
    assert!(script.contains("get_app_info"));
    assert!(!script.contains("publish_project"));
    // No credentials, no tokens, no persistent storage, no filesystem access.
    let script_lower = script.to_lowercase();
    for forbidden in [
        "photo_publisher_",
        "token",
        "secret",
        "password",
        "localstorage",
        "indexeddb",
        "sessionstorage",
        "require(",
        "import(",
    ] {
        assert!(
            !script_lower.contains(forbidden),
            "frontend must not reference {forbidden}"
        );
    }
}

#[test]
fn changing_the_project_path_invalidates_the_previous_validation() {
    // Strutural proof of the stale-validation fix: the field must have an
    // `input` listener; the handler must clear the validated state, disable
    // the Dry Run button and hide previous results, and must never call Rust.
    let script =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app.js")).unwrap();
    assert!(
        script.contains("addEventListener(\"input\", onProjectPathChanged)"),
        "project-path must invalidate state on input"
    );
    let handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .expect("onProjectPathChanged must exist")
        .split("\n}")
        .next()
        .unwrap();
    for required in [
        "state.validated = false",
        "state.projectPath = \"\"",
        "dry-run-button\").disabled = true",
        "hide(\"project-result\")",
        "hide(\"dry-run-result\")",
    ] {
        assert!(
            handler.contains(required),
            "handler must contain {required}"
        );
    }
    assert!(
        !handler.contains("invoke"),
        "invalidation must not call Rust: {handler}"
    );
}

#[test]
fn command_layer_contains_no_provider_or_network_wiring() {
    // Structural guarantee: the commands module is a pure adapter, so it must
    // not reference concrete providers, credentials directly, or remote verbs.
    // It DOES reuse the existing composition root functions — that is the
    // intended delegation, not duplication. The test reads the library source
    // from disk to avoid self-reference.
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands.rs")).unwrap();
    for forbidden in [
        "provider_github",
        "provider_r2",
        "provider_vercel",
        "EnvironmentCredentialStore",
        ".publish(",
        ".put(",
        ".delete(",
        ".commit(",
        "reqwest",
        "ProviderError",
    ] {
        assert!(
            !source.contains(forbidden),
            "command layer must not reference {forbidden}"
        );
    }
    // The composition root is reused, never replaced.
    assert!(source.contains("crate::composition::preflight_publication"));
    assert!(source.contains("crate::composition::build_providers"));
}

#[test]
fn binary_registers_exactly_the_four_commands_and_the_single_channel() {
    // The Tauri binary is the only place that may touch `tauri`; the command
    // set and the event channel feed must remain exactly what this phase
    // specifies. Line endings are normalized (CI checkouts on Windows use
    // CRLF) before any string comparison.
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
        .unwrap()
        .replace("\r\n", "\n");
    let handler = source
        .split("generate_handler![")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let names: Vec<&str> = handler
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(
        names,
        vec![
            "get_app_info",
            "validate_project",
            "publish_project",
            "dry_run_project"
        ]
    );
    assert!(source.contains("publisher_desktop::events::PUBLISHER_EVENT_CHANNEL"));
    assert!(source.contains("publisher_desktop::events::DesktopEvent::from"));
    // Emission failure is swallowed: the bridge can never fail the command.
    assert!(source.contains("let _ = app_handle.emit("));
}

#[test]
fn event_channel_is_the_single_stable_contract_name() {
    assert_eq!(
        publisher_desktop::events::PUBLISHER_EVENT_CHANNEL,
        "publisher://event"
    );
}

#[test]
fn publish_v2_without_credentials_fails_before_any_remote_effect() {
    // Integration-test process: mutating the process environment here cannot
    // race the composition tests in the library binary.
    for key in [
        "PHOTO_PUBLISHER_GITHUB_TOKEN",
        "PHOTO_PUBLISHER_R2_ACCESS_KEY_ID",
        "PHOTO_PUBLISHER_R2_SECRET_ACCESS_KEY",
        "PHOTO_PUBLISHER_VERCEL_TOKEN",
    ] {
        std::env::remove_var(key);
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("fotos")).unwrap();
    let project_path = root.path().join("project.json");
    std::fs::write(
        &project_path,
        r#"{
            "schemaVersion": 2,
            "project": {"id": "pub-sixf", "name": "Pub SixF"},
            "gallery": {"template": "editorial-v1", "title": "Pub SixF", "bundlePath": "gallery-app"},
            "source": {"type": "folder", "path": "fotos"},
            "repository": {"provider": "github", "repository": "owner/repo"},
            "hosting": {"provider": "vercel", "project": "pub-sixf"},
            "storage": {
                "preview": {"provider": "github", "publicBaseUrl": "https://cdn.example.com"},
                "highResolution": {"provider": "r2", "accountId": "a", "bucket": "b", "publicBaseUrl": "https://d.example.com"}
            }
        }"#,
    )
    .unwrap();
    let mut events: Vec<publisher_app::ApplicationEvent> = Vec::new();
    let error = publisher_desktop::commands::publish_project(
        project_path.to_str().unwrap(),
        &mut |event| events.push(event),
    )
    .unwrap_err();
    assert_eq!(error.kind, "resource_missing");
    assert!(error.message.contains("PHOTO_PUBLISHER_GITHUB_TOKEN"));
    assert!(!error.message.contains("token value"));
    // Nothing was published: no local output and no ledger were created.
    assert!(!root.path().join("output").exists());
    // Zero operation events: no executor ever ran.
    assert!(!events
        .iter()
        .any(|event| matches!(event, publisher_app::ApplicationEvent::Operation(_))));
}

/// The desktop-wide invariant of phase 6-F.7: the async boundary exists only
/// in the Tauri wrapper of publish; the library stays synchronous.
#[test]
fn publication_boundary_is_async_only_at_the_tauri_wrapper() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
        .unwrap()
        .replace("\r\n", "\n");
    assert!(source.contains("async fn publish_project"));
    assert!(source.contains("tauri::async_runtime::spawn_blocking"));
    assert!(source.contains("fn validate_project("));
    assert!(source.contains("fn dry_run_project("));
    assert!(source.contains("fn get_app_info("));
    // Exactly one async point exists: the publish wrapper.
    assert_eq!(source.matches("async fn").count(), 1);
    assert_eq!(source.matches("spawn_blocking").count(), 1);
}

#[test]
fn the_library_stays_synchronous() {
    let lib = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs")).unwrap();
    assert!(!lib.contains("tokio"));
    assert!(!lib.contains("spawn_blocking"));
    let commands =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands.rs")).unwrap();
    assert!(!commands.contains("async fn"));
    assert!(!commands.contains("spawn_blocking"));
    // The synchronous application use cases are still the delegations.
    assert!(commands.contains("publisher_app::publish_project"));
    assert!(commands.contains("publisher_app::dry_run_project"));
}

#[test]
fn no_global_state_or_manual_leaks() {
    let mut source = String::new();
    for file in [
        "lib.rs",
        "commands.rs",
        "composition.rs",
        "events.rs",
        "main.rs",
    ] {
        source.push_str(
            &std::fs::read_to_string(format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap(),
        );
    }
    for forbidden in ["static mut", "OnceLock", "Lazy", "Box::leak", "unsafe "] {
        assert!(
            !source.contains(forbidden),
            "desktop must not introduce {forbidden}"
        );
    }
}

#[test]
fn event_adapter_has_no_business_or_runtime_surface() {
    // Structural guarantee: the adapter module translates and nothing else —
    // no provider, credential, process, or time sources; no Tauri runtime (the
    // emission lives in the binary). Reads the source from disk to avoid
    // self-reference.
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/events.rs")).unwrap();
    for forbidden in [
        "std::time",
        "SystemTime",
        "std::process",
        "provider_github",
        "provider_r2",
        "provider_vercel",
        "EnvironmentCredentialStore",
        "app_handle",
        "emit(",
        "tauri::",
    ] {
        assert!(
            !source.contains(forbidden),
            "event adapter must not reference {forbidden}"
        );
    }
}
