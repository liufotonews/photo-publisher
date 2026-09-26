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

/// Reads a repository file as LF-normalized text: CI checkouts on Windows may
/// produce CRLF endings, which must not break structural assertions.
fn read_source(relative: &str) -> String {
    let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), relative);
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
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
    let main = read_source("/src/main.rs");
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
    let index = read_source("/ui/index.html");
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
    let index = read_source("/ui/index.html");
    let script = read_source("/ui/app.js");
    let style = read_source("/ui/style.css");
    // The shell references exactly the two local assets (no CDN, no bundle).
    assert!(index.contains("href=\"style.css\""));
    assert!(index.contains("src=\"app.js\""));
    assert!(!index.contains("https://"));
    assert!(!style.contains("https://"));
    assert!(!style.contains("@import"));
    // The bridge used is the only global one exposed by the desktop config.
    let config_text = read_source("/tauri.conf.json");
    assert!(config_text.contains("\"withGlobalTauri\": true"));
    // Frontend calls only these commands; publication is confined to its own
    // handler (see publish_is_called_only_through_the_publish_handler...).
    assert!(script.contains("validate_project"));
    assert!(script.contains("dry_run_project"));
    assert!(script.contains("get_app_info"));
    assert!(script.contains("async function onPublish()"));
    let outside_publish = script.split("async function onPublish()").next().unwrap();
    assert!(
        !outside_publish.contains("tauri.invoke(\"publish_project\""),
        "publication must stay inside onPublish"
    );
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
    let script = read_source("/ui/app.js");
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
        // The superseded operation must release the busy state exactly here,
        // never through a stale result: this is what prevents UI deadlock.
        "setBusy(false",
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

    // Order: generation advance first, state clearing next, busy release last.
    let begin = handler.find("beginOperation()").unwrap();
    let cleared = handler.find("state.projectPath = \"\"").unwrap();
    let release = handler.find("setBusy(false").unwrap();
    assert!(
        begin < cleared && cleared < release,
        "operation invalidation must precede the busy release"
    );
}

#[test]
fn stale_async_validation_results_are_rejected() {
    // Structural proof of the async race fix: onValidate must capture the
    // submitted path up front, re-read the field after the await, and ignore
    // the outcome whenever the field no longer matches.
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onValidate()")
        .nth(1)
        .expect("onValidate must exist")
        .split("\n}\n")
        .next()
        .unwrap();

    // 1. The submitted path is captured before the invoke.
    let capture = "const projectPath = currentPath();";
    let invoke = "await tauri.invoke(\"validate_project\"";
    let staleness_check = "currentPath() !== projectPath";
    assert!(
        handler.contains(capture),
        "the submitted path must be captured"
    );
    assert!(
        handler.contains(invoke),
        "the validate invocation must exist"
    );
    assert!(
        handler.contains(staleness_check),
        "the current path must be re-read after the await"
    );
    // Ordering: capture → invoke → staleness check.
    let order_ok = handler.find(capture).unwrap() < handler.find(invoke).unwrap()
        && handler.find(invoke).unwrap() < handler.find(staleness_check).unwrap();
    assert!(order_ok, "capture → invoke → check must be the order");

    // 2. Stale branch must ignore the outcome entirely.
    let accepted = "state.validated = Boolean(outcome.valid)";
    assert!(handler.contains(accepted), "normal path must still accept");
    let stale_branch_position = handler.find(staleness_check).unwrap();
    let accept_position = handler.find(accepted).unwrap();
    assert!(
        stale_branch_position < accept_position,
        "the staleness guard must precede accepting the outcome"
    );
    // The stale branch itself (the first guard after the await) must not
    // write state, must not re-enable Dry Run, and must never touch the busy
    // state of a newer operation.
    let first_guard = handler[stale_branch_position..]
        .split("return;")
        .next()
        .unwrap();
    assert!(
        !first_guard.contains("state.validated"),
        "a stale result must not write state.validated"
    );
    assert!(
        !first_guard.contains("dry-run-button\").disabled = false"),
        "a stale result must not re-enable Dry Run"
    );
    assert!(
        !first_guard.contains("setBusy(false"),
        "a stale result must not touch the current busy state"
    );

    // 3. The existing invalidation-on-input behavior is intact.
    assert!(script.contains("function onProjectPathChanged()"));
}

#[test]
fn publisher_event_listener_is_installed_once_and_uses_the_single_channel() {
    let script = read_source("/ui/app.js");
    assert!(
        script.contains("\"publisher://event\""),
        "the listener must target the only published channel"
    );
    assert!(
        script.contains("__TAURI__.event"),
        "events must come through the Tauri event API"
    );
    assert!(
        script.contains(".listen(\"publisher://event\""),
        "the event API must be used with listen()"
    );
    assert!(
        script.contains("function installPublisherEventListener()"),
        "the installer must exist"
    );
    let main = script
        .split("async function main()")
        .nth(1)
        .expect("main must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        main.contains("installPublisherEventListener()"),
        "the listener must be installed during startup"
    );
}

#[test]
fn publisher_event_mapping_covers_every_workflow_and_operation_event() {
    let script = read_source("/ui/app.js");
    // Workflow lifecycle.
    for lifecycle in ["entered", "left_ok", "left_failed", "finished", "failed"] {
        assert!(
            script.contains(lifecycle),
            "lifecycle {lifecycle} must be presented"
        );
    }
    // Workflow steps currently producible by the application layer.
    for step in [
        "load_project",
        "validate_project",
        "inspect_project",
        "recover_publication",
        "preflight",
        "local_publication",
        "build_plan",
        "publish_integrate",
        "dry_run",
    ] {
        assert!(script.contains(step), "workflow step {step} must be mapped");
    }
    // Every granular operation event, exactly as serialized (snake_case).
    for operation in [
        "storage_put_started",
        "storage_put_finished",
        "storage_put_failed",
        "storage_delete_started",
        "storage_delete_finished",
        "storage_delete_failed",
        "repository_batch_started",
        "repository_batch_finished",
        "repository_batch_failed",
        "hosting_publish_started",
        "hosting_publish_finished",
        "hosting_publish_failed",
    ] {
        assert!(
            script.contains(operation),
            "operation {operation} must be mapped"
        );
    }
}

#[test]
fn publish_is_called_only_through_the_publish_handler_and_frameworks_stay_out() {
    let script = read_source("/ui/app.js");
    // Publication is reachable, but only from onPublish.
    assert!(script.contains("async function onPublish()"));
    let handler = script
        .split("async function onPublish()")
        .nth(1)
        .expect("onPublish must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("tauri.invoke(\"publish_project\""));
    // validate/dry-run handlers never publish.
    for other in ["async function onValidate()", "async function onDryRun()"] {
        let body = script
            .split(other)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        assert!(
            !body.contains("publish_project"),
            "handler {other} must not publish"
        );
    }
    // No frontend framework.
    let lowered = script.to_lowercase();
    for forbidden in [
        "react.js",
        "react-dom",
        "from \"react",
        "vite.config",
        ".tsx",
        ".jsx",
        "import(",
        "require(",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "frontend must avoid {forbidden}"
        );
    }
}

#[test]
fn command_layer_contains_no_provider_or_network_wiring() {
    // Structural guarantee: the commands module is a pure adapter, so it must
    // not reference concrete providers, credentials directly, or remote verbs.
    // It DOES reuse the existing composition root functions — that is the
    // intended delegation, not duplication. The test reads the library source
    // from disk to avoid self-reference.
    let source = read_source("/src/commands.rs");
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
    let source = read_source("/src/main.rs").replace("\r\n", "\n");
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
    let source = read_source("/src/main.rs").replace("\r\n", "\n");
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
    let lib = read_source("/src/lib.rs");
    assert!(!lib.contains("tokio"));
    assert!(!lib.contains("spawn_blocking"));
    let commands = read_source("/src/commands.rs");
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
    let source = read_source("/src/events.rs");
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

#[test]
fn publish_gate_requires_validation_and_dry_run() {
    let script = read_source("/ui/app.js");
    // A publish guard must exist and require validated + dryRunReady + the
    // current field still matching the captured path, plus not busy.
    assert!(
        script.contains("function canPublish()"),
        "canPublish must exist"
    );
    let publish_guard = script
        .split("function canPublish()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "!state.busy",
        "state.validated",
        "state.dryRunReady",
        "currentPath() === state.projectPath",
    ] {
        assert!(
            publish_guard.contains(required),
            "canPublish must require {required}"
        );
    }
    // Validation alone no longer unlocks publish: dry-run must run again.
    assert!(script.contains("state.dryRunReady = true"), "dry-run gate");
    let validate_handler = script
        .split("async function onValidate()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(validate_handler.contains("state.dryRunReady = false"));
    assert!(validate_handler.contains("el(\"publish-button\").disabled = true"));
}

#[test]
fn publish_rejects_stale_results_and_busy_runs() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onPublish()")
        .nth(1)
        .expect("onPublish must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // Busy guard plus captured-path comparison.
    assert!(handler.contains("if (!canPublish()) return;"));
    assert!(handler.contains("const projectPath = state.projectPath;"));
    assert!(handler.contains("tauri.invoke(\"publish_project\""));
    let staleness = "if (currentPath() !== projectPath) {\n      return;\n    }";
    assert!(
        handler.contains(staleness),
        "stale publish results must be ignored"
    );
    // Inside the stale guard body: exactly a return; — no setBusy at all.
    let check = "if (currentPath() !== projectPath)";
    let guard_inner = handler[handler.find(check).unwrap()..]
        .split('{')
        .nth(1)
        .unwrap()
        .split('}')
        .next()
        .unwrap()
        .to_owned();
    assert!(
        !guard_inner.contains("setBusy"),
        "stale publish results never touch the busy state"
    );
    assert!(guard_inner.contains("return;"));
    // A stale result must not re-enable Publish.
    assert!(
        !handler.contains("el(\"publish-button\").disabled = false"),
        "stale publish results must not re-enable publish"
    );
    // After publishing, the gate resets: a new Dry Run is required again.
    assert!(handler.contains("state.dryRunReady = false"));
}

#[test]
fn dry_run_rejects_stale_results_too() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onDryRun()")
        .nth(1)
        .expect("onDryRun must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("const projectPath = state.projectPath;"));
    assert!(handler.contains("tauri.invoke(\"dry_run_project\""));
    assert!(
        handler.contains("currentPath() !== projectPath"),
        "stale check"
    );
    assert!(handler.contains("state.dryRunReady = true"));
    let stale = handler
        .split("if (currentPath() !== projectPath)")
        .nth(1)
        .unwrap()
        .split("state.dryRunReady = true")
        .next()
        .unwrap();
    assert!(
        stale.contains("return;"),
        "stale dry-run result must return without marking dryRunReady"
    );
    assert!(
        !stale.contains("setBusy(false"),
        "stale dry-run result must never touch the busy state"
    );
}

#[test]
fn exactly_one_event_listener_is_installed() {
    let script = read_source("/ui/app.js");
    let count = script.matches(".listen(\"publisher://event\"").count();
    assert_eq!(count, 1, "exactly one listener registration expected");
}

#[test]
fn aba_protection_uses_operation_generation_in_all_async_workflows() {
    let script = read_source("/ui/app.js");
    // The helpers define the generation-based guard against A→B→A races.
    assert!(script.contains("let operationGeneration = 0;"));
    assert!(script.contains("function beginOperation()"));
    assert!(script.contains("function isCurrentOperation("));

    for handler_name in [
        "async function onValidate()",
        "async function onDryRun()",
        "async function onPublish()",
    ] {
        let handler = script
            .split(handler_name)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        // Each async handler captures the generation with its path.
        assert!(
            handler.contains("beginOperation()"),
            "{handler_name} must capture the operation generation"
        );
        // And rejects stale results through the combined test.
        let await_pos = handler.find("await tauri.invoke").unwrap();
        let guard_pos = handler.find("isCurrentOperation(").unwrap();
        assert!(
            await_pos < guard_pos,
            "{handler_name}: the generation check must follow the await"
        );
    }

    // A path change starts a new generation (so A→B→A can never validate the
    // superseded operation again).
    let path_handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        path_handler.contains("beginOperation()"),
        "path changes must advance the operation generation"
    );
    // The stale guard never re-enables Dry Run or Publish.
    for handler_name in [
        "async function onValidate()",
        "async function onDryRun()",
        "async function onPublish()",
    ] {
        let handler = script
            .split(handler_name)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        let guard = handler
            .split("isCurrentOperation(")
            .nth(1)
            .unwrap()
            .split("return;")
            .next()
            .unwrap();
        assert!(
            !guard.contains(".disabled = false"),
            "{handler_name}: stale must not re-enable anything"
        );
        assert!(
            !guard.contains("setBusy"),
            "{handler_name}: stale never touches the busy state"
        );
    }
}
