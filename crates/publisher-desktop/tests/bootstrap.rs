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
fn binary_registers_exactly_the_thirteen_commands_and_the_single_channel() {
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
            "dry_run_project",
            "recover_project",
            "create_project_setup",
            "validate_project_configuration",
            "get_credential_status",
            "set_credential",
            "delete_credential",
            "preflight_project",
            "provision_project",
            "prepare_local_publication"
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
/// in the Tauri wrappers that run long work off the caller's context; the
/// library stays synchronous.
#[test]
fn publication_boundary_is_async_only_at_the_tauri_wrapper() {
    let source = read_source("/src/main.rs").replace("\r\n", "\n");
    assert!(source.contains("async fn publish_project"));
    assert!(source.contains("async fn provision_project"));
    assert!(source.contains("async fn prepare_local_publication"));
    assert!(source.contains("tauri::async_runtime::spawn_blocking"));
    assert!(source.contains("fn validate_project("));
    assert!(source.contains("fn dry_run_project("));
    assert!(source.contains("fn get_app_info("));
    // Exactly three async points exist, each a long-running wrapper: the
    // publish wrapper, the provisioning wrapper, and the local bootstrap.
    // Everything else stays synchronous.
    assert_eq!(source.matches("async fn").count(), 3);
    assert_eq!(source.matches("spawn_blocking").count(), 3);
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
    assert!(commands.contains("publisher_app::provision_project"));
    assert!(commands.contains("publisher_app::prepare_local_publication"));
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
fn the_ux_structure_presents_the_publication_and_recovery_sequence() {
    let html = read_source("/ui/index.html");
    let css = read_source("/ui/style.css");
    // Hierarchy of actions, in visual order: project → validate → dry run →
    // publish; and the recovery section when the publish says so.
    for element in [
        "id=\"step-project\"",
        "id=\"step-validate\"",
        "id=\"step-dryrun\"",
        "id=\"step-publish\"",
        "id=\"project-path\"",
        "id=\"global-status\"",
        "id=\"validate-button\"",
        "id=\"dry-run-button\"",
        "id=\"publish-button\"",
        "id=\"recover-button\"",
        "id=\"recover-section\"",
        "id=\"project-result\"",
        "id=\"dry-run-result\"",
        "id=\"publish-result\"",
        "id=\"activity-list\"",
    ] {
        assert!(html.contains(element), "index.html must contain {element}");
    }
    // Status visuals are distinguishable (info / in-progress / ok / error).
    for class in [
        ".status-chip",
        ".result.ok",
        ".result.error",
        ".result.attention",
        ".activity-list li.failed",
        ".activity-list li.in-progress",
        ".activity-list li.done",
        ".step.current",
    ] {
        assert!(css.contains(class), "style.css must define {class}");
    }
    // The visual sequence is only presentation: the JS drives the same labels already
    // published, no new state machine was introduced.
    let js = read_source("/ui/app.js");
    assert!(js.contains("function setPipelineStep()"));
    assert!(js.contains("refreshButtons();"));
    assert!(js.contains("setBusy(false"));
    // Nothing was added that would qualify as a frontend framework.
    let all = (html + &css + &js).to_lowercase();
    for forbidden in ["react", "vite", "tailwindcss", "bootstrapcdn"] {
        assert!(!all.contains(forbidden), "Ux layer must avoid {forbidden}");
    }
}

#[test]
fn the_status_chip_shows_existing_messages_only() {
    let js = read_source("/ui/app.js");
    // The chip only reflects the messages the handlers already produce.
    assert!(js.contains("chip.textContent = label"));
    // And the tone applied is presentation-only (no new labels/texts invented).
    assert!(js.contains("chip.className = \"status-chip\""));
    // The chip exists and is the single global status surface.
    let html = read_source("/ui/index.html");
    assert_eq!(html.matches("id=\"global-status\"").count(), 1);
}

#[test]
fn recover_project_is_registered_and_remains_a_pure_adapter() {
    // The command set now includes recover_project alongside the four
    // existing commands, with the same single event channel.
    let main = read_source("/src/main.rs");
    let handler = main
        .split("generate_handler![")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let names: Vec<&str> = handler
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    assert_eq!(
        names,
        vec![
            "get_app_info",
            "validate_project",
            "publish_project",
            "dry_run_project",
            "recover_project",
            "create_project_setup",
            "validate_project_configuration",
            "get_credential_status",
            "set_credential",
            "delete_credential",
            "preflight_project",
            "provision_project",
            "prepare_local_publication"
        ]
    );
    // The desktop command layer delegates to the application use case.
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("publisher_app::recover_publication"));
    assert!(commands.contains("pub struct RecoverOutcomeDto"));
    // It writes nothing itself: recovery never calls a publish/delete verb.
    let body = commands
        .split("pub fn recover_project")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [".publish(", ".put(", ".commit(", ".delete("] {
        assert!(
            !body.contains(forbidden),
            "recover_project must not contain {forbidden}"
        );
    }
}

#[test]
fn create_project_setup_is_registered_and_a_pure_adapter() {
    // The command registration includes every existing command plus the new
    // one; the adapter delegates to publisher-app, never writes directly.
    let main = read_source("/src/main.rs");
    let handler = main
        .split("generate_handler![")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let names: Vec<&str> = handler
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    assert!(names.contains(&"create_project_setup"));
    assert_eq!(names.len(), 13);
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("publisher_app::create_project_setup"));
    assert!(commands.contains("pub struct CreateProjectSetupOutcomeDto"));
    // It has no provisioning reach: no provider verbs anywhere in setup.
    let body = commands
        .split("pub fn create_project_setup")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [".publish(", ".put(", ".commit(", ".delete(", ".get("] {
        assert!(
            !body.contains(forbidden),
            "create_project_setup must not contain {forbidden}"
        );
    }
}

#[test]
fn validate_project_configuration_is_registered_and_a_pure_adapter() {
    // Phase 7-D: the command is a thin adapter over the publisher-app
    // configuration validation — never a publish/dry-run/recover/create path,
    // never a provider or credential surface.
    let main = read_source("/src/main.rs");
    let handler = main
        .split("generate_handler![")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    assert!(handler.contains("validate_project_configuration"));
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("publisher_app::validate_project_configuration"));
    assert!(commands.contains("pub struct ValidateConfigurationOutcomeDto"));
    let body = commands
        .split("pub fn validate_project_configuration")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "publish_project(",
        "dry_run_project(",
        "recover_project(",
        "create_project_setup(",
        "CredentialStore",
        "std::env",
        ".publish(",
        ".put(",
        ".commit(",
        ".delete(",
    ] {
        assert!(
            !body.contains(forbidden),
            "validate_project_configuration must not contain {forbidden}"
        );
    }
}

#[test]
fn credential_commands_are_registered_and_pure_adapters() {
    // Phase 7-E: the three credential commands are registered and delegate
    // to the publisher-app service over the composition-root backend. The
    // command layer itself never names the concrete store, never publishes,
    // never provisions, and never echoes a secret.
    let main = read_source("/src/main.rs");
    let handler = main
        .split("generate_handler![")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    for command in [
        "get_credential_status",
        "set_credential",
        "delete_credential",
    ] {
        assert!(
            handler.contains(command),
            "generate_handler must register {command}"
        );
    }
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("publisher_app::credential_status"));
    assert!(commands.contains("publisher_app::set_credential"));
    assert!(commands.contains("publisher_app::delete_credential"));
    assert!(commands.contains("pub struct CredentialStatusDto"));
    // The credential-status surface carries exactly: name, label, bit.
    let dto = commands
        .split("pub struct CredentialStatusDto")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for field in [
        "pub name: String",
        "pub label: String",
        "pub configured: bool",
    ] {
        assert!(dto.contains(field), "CredentialStatusDto needs {field}");
    }
    for leakage in ["secret", "value", "bytes", "Vec<u8>"] {
        assert!(
            !dto.contains(leakage),
            "CredentialStatusDto must never carry the credential {leakage}"
        );
    }
    // No credential command may reach publication, provisioning, providers,
    // or the environment by itself (the store lives in the composition root).
    for function in [
        "pub fn get_credential_status",
        "pub fn set_credential",
        "pub fn delete_credential",
    ] {
        let body = commands
            .split(function)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        for forbidden in [
            "publish_project(",
            "dry_run_project(",
            "recover_project(",
            "create_project_setup(",
            "validate_project_configuration(",
            "EnvironmentCredentialStore",
            "std::env",
            ".publish(",
            ".put(",
            ".commit(",
            ".delete(",
            "reqwest",
        ] {
            assert!(
                !body.contains(forbidden),
                "{function} must not contain {forbidden}"
            );
        }
    }
}

#[test]
fn preflight_project_is_registered_and_a_pure_adapter() {
    // Phase 7-F: the command is registered and delegates to the publisher-app
    // preflight use case. It never plans (dry-run), never publishes, never
    // recovers, never builds a provider, never reaches the network — its only
    // credential surface is the configured bits via the composition root.
    let main = read_source("/src/main.rs");
    let handler = main
        .split("generate_handler![")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    assert!(handler.contains("preflight_project"));
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("publisher_app::preflight_project"));
    assert!(commands.contains("pub struct PreflightOutcomeDto"));
    let body = commands
        .split("pub fn preflight_project")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "publish_project(",
        "dry_run_project(",
        "recover_project(",
        "create_project_setup(",
        "preflight_publication(",
        "build_providers(",
        "EnvironmentCredentialStore",
        "std::env",
        ".publish(",
        ".put(",
        ".commit(",
        ".delete(",
        "reqwest",
    ] {
        assert!(
            !body.contains(forbidden),
            "preflight_project command must not contain {forbidden}"
        );
    }
    // The publication boundary stays exclusively on the publish wrapper.
    let main_normalized = read_source("/src/main.rs");
    let preflight_wrapper = main_normalized
        .split("fn preflight_project(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        !preflight_wrapper.contains("spawn_blocking"),
        "preflight stays synchronous: it performs no remote work"
    );
}

#[test]
fn desktop_provisioning_lives_only_in_the_adapter_and_composition() {
    // Phase 7-J wired provisioning into the desktop: the composition root
    // builds the four concrete provisioners, the command layer delegates to
    // the application service, and nothing else touches them. The publish
    // path stays free of provisioning.
    let main = read_source("/src/main.rs");
    assert!(main.contains("provision_project"));
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("crate::composition::build_provisioners"));
    assert!(commands.contains("publisher_app::provision_project"));
    let body = commands
        .split("pub fn provision_project")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "publish_project(",
        "dry_run_project(",
        "recover_project(",
        "create_project_setup(",
        "validate_project_configuration(",
        "preflight_project(",
        "GitHubRepositoryProvisioner",
        "R2StorageProvisioner",
        "VercelHostingProvisioner",
        "EnvironmentCredentialStore",
        "std::env",
        "reqwest",
        ".publish(",
        ".put(",
        ".commit(",
        ".delete(",
    ] {
        assert!(
            !body.contains(forbidden),
            "provision_project command must not contain {forbidden}"
        );
    }
    // The composition root is the single place naming the concrete backends.
    let composition = read_source("/src/composition.rs");
    for required in [
        "GitHubRepositoryProvisioner",
        "R2StorageProvisioner",
        "VercelHostingProvisioner",
        "UnsupportedDomainProvisioner",
        "as_provisioning_providers",
    ] {
        assert!(
            composition.contains(required),
            "composition must wire {required}"
        );
    }
    // Publish never provisions: the publish command body keeps its boundary.
    let publish = commands
        .split("pub fn publish_project")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        !publish.contains("provision_project"),
        "publish must never call provisioning"
    );
}

#[test]
fn needs_recovery_unlocks_recovery_but_blocked_never_does() {
    let script = read_source("/ui/app.js");
    let html = read_source("/ui/index.html");
    assert!(
        html.contains("recover-button"),
        "UI exposes the recover action"
    );
    assert!(
        html.contains("recover-section"),
        "UI exposes the recovery result area"
    );
    // canRecover is gated on the explicit publish outcome.
    assert!(script.contains("canRecover()"));
    let guard = script
        .split("function canRecover()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "!state.busy",
        "state.needsRecovery",
        "currentPath() === state.projectPath",
    ] {
        assert!(
            guard.contains(required),
            "canRecover must require {required}"
        );
    }
    // showPublish maps ONLY the "needs_recovery" outcome to the recovery state;
    // "blocked" (and all others) cannot activate it by accident.
    let publish_handler = script
        .split("function showPublish(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(publish_handler.contains("outcome.outcome === \"needs_recovery\""));
    // After recovery the publish gate stays closed (a new Dry Run must run).
    let recover_handler = script
        .split("async function onRecover()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(recover_handler.contains("state.dryRunReady = false"));
    // The recovery success path never re-enables Publish.
    assert!(
        !recover_handler.contains("el(\"publish-button\").disabled = false"),
        "recovery must not re-enable Publish"
    );
    // Stale recovery branches are pure returns (no setBusy) — like every other
    // stale handler in this generation-based UI.
    let stale_pos = recover_handler.find("!isCurrentOperation(").unwrap();
    let stale_block = recover_handler[stale_pos..]
        .split("return;")
        .next()
        .unwrap();
    assert!(
        !stale_block.contains("setBusy"),
        "stale recovery must never touch the busy state"
    );
    assert!(recover_handler.contains("tauri.invoke(\"recover_project\""));
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
        "async function onPrepareLocal()",
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
        "async function onPrepareLocal()",
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

// ---------------------------------------------------------------------------
// Phase 7-C — Desktop Project Setup Wizard
// ---------------------------------------------------------------------------

#[test]
fn wizard_entry_point_and_fields_exist_in_the_ui() {
    let html = read_source("/ui/index.html");
    // The "Novo projeto" entry point and the wizard container.
    assert!(
        html.contains("id=\"tab-create\""),
        "Novo projeto tab exists"
    );
    assert!(html.contains("id=\"tab-publish\""), "Publicar tab exists");
    assert!(html.contains("id=\"create-flow\""), "wizard section exists");
    // Required fields per group (Projeto / Galeria / destino).
    for id in [
        "wizard-project-id",
        "wizard-project-name",
        "wizard-project-client",
        "wizard-project-date",
        "wizard-gallery-template",
        "wizard-gallery-title",
        "wizard-gallery-description",
        "wizard-gallery-bundle",
        "wizard-source-path",
        "wizard-repository-provider",
        "wizard-repository-name",
        "wizard-repository-branch",
        "wizard-hosting-provider",
        "wizard-hosting-project",
        "wizard-hosting-team",
        "wizard-preview-provider",
        "wizard-preview-prefix",
        "wizard-preview-public-url",
        "wizard-hires-provider",
        "wizard-hires-bucket",
        "wizard-hires-prefix",
        "wizard-hires-account",
        "wizard-hires-public-url",
        "wizard-domain-url",
        "wizard-project-path",
        "create-submit",
        "create-cancel",
        "create-result",
        "create-status",
        "create-result-title",
    ] {
        assert!(html.contains(id), "wizard field {id} must exist");
    }
    // Source stays folder-only in this phase.
    assert!(
        !html.contains("wizard-source-type"),
        "no extra source types in 7-C"
    );
    // The wizard never collects credentials.
    let wizard_section = html.split("id=\"create-flow\"").nth(1).unwrap();
    for forbidden in [
        "password",
        "token",
        "secret",
        "api-key",
        "apikey",
        "credential",
    ] {
        assert!(
            !wizard_section.to_lowercase().contains(forbidden),
            "wizard must never collect {forbidden}"
        );
    }
}

#[test]
fn wizard_invokes_only_create_project_setup() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onCreateProject()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    // Exactly one invoke, targeting the new command by name.
    assert!(handler.contains("tauri.invoke(\"create_project_setup\""));
    for forbidden in [
        "publish_project",
        "dry_run_project",
        "recover_project",
        "validate_project",
    ] {
        assert!(
            !handler.contains(forbidden),
            "wizard must never invoke {forbidden}"
        );
    }
}

#[test]
fn wizard_presents_success_with_identity_and_path() {
    let script = read_source("/ui/app.js");
    let show = script
        .split("function showCreated(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "outcome.project_name",
        "outcome.project_id",
        "outcome.project_path",
        "create-result",
    ] {
        assert!(
            show.contains(required),
            "success rendering must include {required}"
        );
    }
    // Errors surface a friendly message through the same result area.
    let show_error = script
        .split("function showCreateError(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(show_error.contains("create-status"));
    let handler = script
        .split("async function onCreateProject()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("showCreateError("));
}

#[test]
fn wizard_state_never_touches_the_publication_workflow() {
    let script = read_source("/ui/app.js");
    // The wizard keeps an independent state container...
    assert!(script.contains("const wizardState = {"));
    // ...and its handlers never mutate the publication state machine.
    for chunk_name in [
        "async function onCreateProject()",
        "function showCreated(",
        "function showCreateError(",
        "function setCreateMode(",
    ] {
        let body = script
            .split(chunk_name)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        for forbidden in [
            "state.validated",
            "state.dryRunReady",
            "state.needsRecovery",
            "state.projectPath",
            "operationGeneration",
        ] {
            assert!(
                !body.contains(forbidden),
                "{chunk_name} must not touch {forbidden}"
            );
        }
    }
    // The wizard uses its own generation guard (same stale discipline).
    let handler = script
        .split("async function onCreateProject()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("wizardState.generation"));
    assert!(handler.contains("generation !== wizardState.generation"));
}

#[test]
fn wizard_stays_framework_free_like_the_rest_of_the_ui() {
    let html = read_source("/ui/index.html");
    let script = read_source("/ui/app.js");
    for forbidden in [
        "react",
        "React",
        "npm",
        "node_modules",
        "@tauri-apps/api",
        "import ",
        "require(",
    ] {
        assert!(
            !script.contains(forbidden),
            "frontend must not reference {forbidden}"
        );
    }
    assert!(
        !html.contains("cdn") && !html.contains("https://"),
        "no external frontend asset may be loaded"
    );
}

// ---------------------------------------------------------------------------
// Phase 7-D — Configuration validation UI
// ---------------------------------------------------------------------------

#[test]
fn configuration_check_button_and_result_area_exist() {
    let html = read_source("/ui/index.html");
    for id in [
        "id=\"config-validate-button\"",
        "id=\"configuration-result\"",
        "id=\"configuration-result-title\"",
        "id=\"config-status\"",
        "id=\"config-issues\"",
    ] {
        assert!(html.contains(id), "index.html must contain {id}");
    }
    let script = read_source("/ui/app.js");
    assert!(
        script.contains(
            "el(\"config-validate-button\").addEventListener(\"click\", onValidateConfiguration)"
        ),
        "the button must be wired to the configuration check handler"
    );
}

#[test]
fn configuration_check_invokes_only_validate_project_configuration() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onValidateConfiguration()")
        .nth(1)
        .expect("onValidateConfiguration must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // Exactly one backend call: the new command, by name.
    assert!(handler.contains("tauri.invoke(\"validate_project_configuration\""));
    for forbidden in [
        "invoke(\"validate_project\"",
        "publish_project",
        "dry_run_project",
        "recover_project",
        "create_project_setup",
    ] {
        assert!(
            !handler.contains(forbidden),
            "configuration check must never invoke {forbidden}"
        );
    }
    // It never mutates the publication workflow state — it is a diagnosis,
    // not a step of the publish gate.
    for forbidden in [
        "state.validated",
        "state.dryRunReady",
        "state.needsRecovery",
        "state.projectPath",
    ] {
        assert!(
            !handler.contains(forbidden),
            "configuration check must not mutate {forbidden}"
        );
    }
    // Same generation/stale discipline as every other async handler.
    assert!(handler.contains("beginOperation()"));
    let await_pos = handler.find("await tauri.invoke").unwrap();
    let guard_pos = handler.find("isCurrentOperation(").unwrap();
    assert!(
        await_pos < guard_pos,
        "the generation check must follow the await"
    );
    // Stale branches are pure returns: no busy-state mutation.
    let staleness = "if (currentPath() !== projectPath) {\n      return;\n    }";
    assert!(
        handler.contains(staleness),
        "stale configuration results must be ignored"
    );
    let stale_block = handler[handler.find("isCurrentOperation(").unwrap()..]
        .split("return;")
        .next()
        .unwrap();
    assert!(
        !stale_block.contains("setBusy"),
        "stale configuration results never touch the busy state"
    );
}

#[test]
fn configuration_check_renders_issues_safely() {
    let script = read_source("/ui/app.js");
    let show = script
        .split("function showConfiguration(")
        .nth(1)
        .expect("showConfiguration must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // Both outcomes are presented distinctly (valid / requires fixes).
    assert!(show.contains("coerente para publicação"));
    assert!(show.contains("requer correções"));
    // Issues are rendered from the backend payload as plain text — never as
    // HTML (the payload contains user-editable configuration strings).
    assert!(show.contains("outcome.issues"));
    assert!(show.contains("issue.field"));
    assert!(show.contains("issue.message"));
    assert!(show.contains("textContent"));
    assert!(show.contains("document.createElement(\"li\")"));
    assert!(show.contains("appendChild"));
    assert!(
        !show.contains("innerHTML"),
        "issue rendering must never use innerHTML"
    );
}

#[test]
fn configuration_check_result_is_forgotten_on_path_change_or_mode_switch() {
    let script = read_source("/ui/app.js");
    let path_handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        path_handler.contains("hide(\"configuration-result\")"),
        "a path change must discard the configuration diagnosis"
    );
    let mode = script
        .split("function setCreateMode(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(
        mode.contains("el(\"configuration-result\")"),
        "the wizard mode must hide the configuration diagnosis"
    );
}

// ---------------------------------------------------------------------------
// Phase 7-E — Credential UX
// ---------------------------------------------------------------------------

#[test]
fn credentials_section_exists_with_secret_safe_inputs() {
    let html = read_source("/ui/index.html");
    assert!(html.contains("id=\"tab-credentials\""), "Credenciais tab");
    assert!(
        html.contains("id=\"credentials-section\""),
        "credentials section exists"
    );
    assert!(html.contains("id=\"credential-note\""));
    // Exactly the four supported credential names, carried in markup (never
    // assembled by the frontend logic).
    for name in [
        "data-credential-name=\"github.token\"",
        "data-credential-name=\"r2.access_key_id\"",
        "data-credential-name=\"r2.secret_access_key\"",
        "data-credential-name=\"vercel.token\"",
    ] {
        assert_eq!(
            html.matches(name).count(),
            1,
            "exactly one field must carry {name}"
        );
    }
    // Every credential input is a masked field; no reveal control exists.
    let section = html.split("id=\"credentials-section\"").nth(1).unwrap();
    let section = section.split("</section>").next().unwrap();
    assert_eq!(section.matches("type=\"password\"").count(), 4);
    assert_eq!(
        section.matches("data-credential-action=\"save\"").count(),
        4
    );
    assert_eq!(
        section.matches("data-credential-action=\"remove\"").count(),
        4
    );
    let lowered = section.to_lowercase();
    for forbidden in [
        "localstorage",
        "sessionstorage",
        "clipboard",
        "type=\"text\" name=",
        "show-token",
        "reveal",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "credentials section must not contain {forbidden}"
        );
    }
}

#[test]
fn credential_actions_use_only_credential_commands_and_clear_inputs() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onCredentialAction(")
        .nth(1)
        .expect("onCredentialAction must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // Exactly the two mutating commands, never anything else.
    assert!(handler.contains("invoke(\"set_credential\""));
    assert!(handler.contains("invoke(\"delete_credential\""));
    for forbidden in [
        "publish_project",
        "dry_run_project",
        "recover_project",
        "create_project_setup",
        "invoke(\"validate_project",
    ] {
        assert!(
            !handler.contains(forbidden),
            "credential actions must never invoke {forbidden}"
        );
    }
    // The typed value is cleared from the field immediately at click time —
    // before any await resolves — and never written anywhere else.
    assert!(handler.contains("input.value = \"\";"));
    assert!(
        !handler.contains("textContent = value"),
        "the value must never be rendered"
    );
    assert!(
        !handler.contains("innerHTML"),
        "credential UI must never build HTML from values"
    );
    let refresh = script
        .split("async function refreshCredentialStatus(")
        .nth(1)
        .expect("refreshCredentialStatus must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(refresh.contains("invoke(\"get_credential_status\""));
    // Removal requires an explicit confirmation.
    assert!(handler.contains("window.confirm("));
    // Status changes are presentation text only (fixed strings written via
    // textContent in markCredentialRow; render delegates to it).
    let render = script
        .split("function renderCredentialStatus(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(render.contains("markCredentialRow("));
    assert!(!render.contains("innerHTML"));
    let mark = script
        .split("function markCredentialRow(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(mark.contains("textContent"));
    assert!(!mark.contains("innerHTML"));
    // The rendered marks are fixed strings, never the stored value.
    assert!(mark.contains("Configurada"));
    assert!(mark.contains("configurada"));
}

#[test]
fn credential_state_is_independent_of_the_publication_flow() {
    let script = read_source("/ui/app.js");
    // Independent state container with the same stale-answer discipline.
    assert!(script.contains("const credentialsState = {"));
    for chunk_name in [
        "async function onCredentialAction(",
        "async function refreshCredentialStatus(",
        "function setCredentialsMode(",
    ] {
        let body = script
            .split(chunk_name)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        for forbidden in [
            "state.validated",
            "state.dryRunReady",
            "state.needsRecovery",
            "state.projectPath",
            "operationGeneration",
            "setBusy(",
        ] {
            assert!(
                !body.contains(forbidden),
                "{chunk_name} must not touch {forbidden}"
            );
        }
    }
    // Both async handlers apply a generation guard after each await.
    for chunk_name in [
        "async function onCredentialAction(",
        "async function refreshCredentialStatus(",
    ] {
        let body = script
            .split(chunk_name)
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        let await_pos = body.find("await tauri.invoke").unwrap();
        let guard_pos = body
            .find("generation !== credentialsState.generation")
            .unwrap();
        assert!(
            await_pos < guard_pos,
            "{chunk_name}: the generation guard must follow the await"
        );
    }
    // Entering the tab refreshes the status; entering/leaving never leaks
    // into the publish tab.
    assert!(script.contains("setCredentialsMode(true)"));
    let mode = script
        .split("function setCredentialsMode(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(mode.contains("refreshCredentialStatus()"));
}

// ---------------------------------------------------------------------------
// Phase 7-F — Preflight independente do Dry Run
// ---------------------------------------------------------------------------

#[test]
fn preflight_button_step_and_result_area_exist_in_order() {
    let html = read_source("/ui/index.html");
    for id in [
        "id=\"step-preflight\"",
        "id=\"preflight-button\"",
        "id=\"preflight-result\"",
        "id=\"preflight-result-title\"",
        "id=\"preflight-status\"",
        "id=\"preflight-issues\"",
    ] {
        assert!(html.contains(id), "index.html must contain {id}");
    }
    // The visual sequence is Projeto → Validar → Preflight → Dry Run →
    // Publicar, in the markup itself.
    let validate_pos = html.find("id=\"step-validate\"").unwrap();
    let preflight_pos = html.find("id=\"step-preflight\"").unwrap();
    let dryrun_pos = html.find("id=\"step-dryrun\"").unwrap();
    let publish_pos = html.find("id=\"step-publish\"").unwrap();
    assert!(validate_pos < preflight_pos && preflight_pos < dryrun_pos);
    assert!(dryrun_pos < publish_pos);
    // The button is wired to its handler.
    let script = read_source("/ui/app.js");
    assert!(script.contains("el(\"preflight-button\").addEventListener(\"click\", onPreflight)"));
}

#[test]
fn preflight_requires_validation_and_unlocks_dry_run() {
    let script = read_source("/ui/app.js");
    // The publish flow gained its own preflight state.
    assert!(script.contains("preflightReady"));
    // Gate 1: Preflight requires a current validation.
    let can_preflight = script
        .split("function canPreflight()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "!state.busy",
        "state.validated",
        "currentPath() === state.projectPath",
    ] {
        assert!(
            can_preflight.contains(required),
            "canPreflight needs {required}"
        );
    }
    let handler = script
        .split("async function onPreflight()")
        .nth(1)
        .expect("onPreflight must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("if (state.busy || !state.validated) return;"));
    // Gate 2: a successful Preflight is what unlocks the Dry Run; a failed
    // one keeps it locked. Publish still requires both preflight and dry-run.
    let can_dry_run = script
        .split("function canDryRun()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(can_dry_run.contains("state.preflightReady"));
    assert!(can_dry_run.contains("state.validated"));
    assert!(handler.contains("state.preflightReady = Boolean(outcome.ready)"));
    let catch_block = handler.split("} catch (error) {").nth(1).unwrap();
    assert!(catch_block.contains("state.preflightReady = false"));
    assert!(
        !catch_block.contains("state.dryRunReady = true"),
        "a failed preflight must never unlock the Dry Run"
    );
    let can_publish = script
        .split("function canPublish()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "state.validated",
        "state.preflightReady",
        "state.dryRunReady",
    ] {
        assert!(
            can_publish.contains(required),
            "canPublish needs {required}"
        );
    }
    // The button recalculations route through the guards.
    let refresh = script
        .split("function refreshButtons()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(refresh.contains("el(\"preflight-button\").disabled = !canPreflight()"));
    assert!(refresh.contains("el(\"dry-run-button\").disabled = !canDryRun()"));
}

#[test]
fn preflight_invokes_only_preflight_and_keeps_the_flows_independent() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onPreflight()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("tauri.invoke(\"preflight_project\""));
    for forbidden in [
        "invoke(\"validate_project\"",
        "publish_project",
        "dry_run_project",
        "recover_project",
        "create_project_setup",
        "get_credential_status",
        "set_credential",
        "delete_credential",
        "credentialsState",
    ] {
        assert!(
            !handler.contains(forbidden),
            "preflight must never reference {forbidden}"
        );
    }
    // Preflight never mutates the dry-run/recovery state itself.
    assert!(!handler.contains("state.dryRunReady = true"));
    assert!(!handler.contains("state.needsRecovery"));
    // Same generation/stale discipline as every other pipeline handler.
    assert!(handler.contains("beginOperation()"));
    let await_pos = handler.find("await tauri.invoke").unwrap();
    let guard_pos = handler.find("isCurrentOperation(").unwrap();
    assert!(
        await_pos < guard_pos,
        "the generation check must follow the await"
    );
    let stale = "if (currentPath() !== projectPath) {\n      return;\n    }";
    assert!(
        handler.contains(stale),
        "stale preflight results are ignored"
    );
    let stale_block = handler[handler.find("isCurrentOperation(").unwrap()..]
        .split("return;")
        .next()
        .unwrap();
    assert!(
        !stale_block.contains("setBusy"),
        "stale preflight never touches the busy state"
    );
    // Rendering is text-only.
    let show = script
        .split("function showPreflight(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(show.contains("outcome.issues"));
    assert!(show.contains("textContent"));
    assert!(!show.contains("innerHTML"));
}

#[test]
fn preflight_is_invalidated_by_path_change_and_mode_switch() {
    let script = read_source("/ui/app.js");
    let path_handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "state.preflightReady = false",
        "el(\"preflight-button\").disabled = true",
        "hide(\"preflight-result\")",
    ] {
        assert!(
            path_handler.contains(required),
            "path change must contain {required}"
        );
    }
    // A revalidation also restarts the preflight step for the same path.
    let validate_handler = script
        .split("async function onValidate()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(validate_handler.contains("state.preflightReady = false"));
    assert!(validate_handler.contains("hide(\"preflight-result\")"));
    // The wizard/credentials modes hide the preflight result with the rest
    // of the publication flow.
    let mode = script
        .split("function setCreateMode(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(mode.contains("el(\"preflight-result\")"));
    assert!(mode.contains("el(\"provision-section\")"));
}

// ---------------------------------------------------------------------------
// Phase 7-J — Desktop Provisioning UX
// ---------------------------------------------------------------------------

#[test]
fn provisioning_section_exists_as_an_explicit_separate_action() {
    let html = read_source("/ui/index.html");
    for id in [
        "id=\"provision-section\"",
        "id=\"provision-title\"",
        "id=\"provision-button\"",
        "id=\"provision-result\"",
        "id=\"provision-status\"",
        "id=\"provision-resources\"",
    ] {
        assert!(html.contains(id), "index.html must contain {id}");
    }
    // Explicitly labeled as infrastructure — never confused with publishing.
    assert!(html.contains("Provisionar infraestrutura"));
    let script = read_source("/ui/app.js");
    assert!(script.contains("el(\"provision-button\").addEventListener(\"click\", onProvision)"));
}

#[test]
fn provision_requires_a_successful_preflight_and_never_runs_automatically() {
    let script = read_source("/ui/app.js");
    // Phase 7-K.5 pipeline order: Validar → Preflight → Provisionar →
    // Preparar → ... — the provisioning gate is the successful Preflight
    // (which already carries the configuration coherence rules), the idle
    // UI, no pending recovery, and the same path. It never requires the
    // local bootstrap, dry run, or publish.
    let guard = script
        .split("function canProvision()")
        .nth(1)
        .expect("canProvision must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "!state.busy",
        "state.validated",
        "state.preflightReady",
        "currentPath() === state.projectPath",
        "!state.needsRecovery",
    ] {
        assert!(guard.contains(required), "canProvision needs {required}");
    }
    for forbidden in ["state.configReady", "state.localReady", "state.dryRunReady"] {
        assert!(
            !guard.contains(forbidden),
            "provisioning must not require {forbidden}"
        );
    }
    let handler = script
        .split("async function onProvision()")
        .nth(1)
        .expect("onProvision must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("if (!canProvision()) return;"));
    // The configuration check feeds configReady (and resets it on failure).
    let config_handler = script
        .split("async function onValidateConfiguration()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(config_handler.contains("state.configReady = Boolean(outcome.valid)"));
    assert!(config_handler.contains("state.configReady = false"));
    // Provision is invoked EXACTLY once across the whole UI: nothing else
    // (validate, preflight, dry-run, publish, wizard, credentials) triggers it.
    assert_eq!(script.matches("invoke(\"provision_project\"").count(), 1);
}

#[test]
fn provision_keeps_preflight_valid_but_stales_dry_run_and_never_enables_publish() {
    // Phase 7-K.5 pipeline order: provisioning changes REMOTE infrastructure
    // only. A successful (or failed) run must NOT invalidate the Preflight
    // (local/declared preconditions unchanged) so the flow can continue to
    // Preparar galeria local — but any earlier Dry Run plan is stale and
    // Publish still requires a fresh one.
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onProvision()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    let success = handler
        .split("showProvision(outcome);")
        .nth(1)
        .unwrap()
        .split("} catch (error) {")
        .next()
        .unwrap();
    assert!(
        !success.contains("state.preflightReady = false"),
        "provisioning must keep the preflight verdict (7-K.5 order)"
    );
    assert!(success.contains("state.provisioned = true"));
    assert!(success.contains("state.dryRunReady = false"));
    assert!(success.contains("hide(\"dry-run-result\")"));
    assert!(success.contains("hide(\"publish-result\")"));
    assert!(
        !success.contains("publish-button\").disabled = false"),
        "a successful provision must never re-enable Publish"
    );
    let failure = handler.split("} catch (error) {").nth(1).unwrap();
    assert!(
        !failure.contains("state.preflightReady = false"),
        "a failed provision keeps the preflight verdict too"
    );
    assert!(failure.contains("state.provisioned = false"));
    assert!(failure.contains("state.dryRunReady = false"));
    // Publish continues to require validated + preflight + dry-run.
    let can_publish = script
        .split("function canPublish()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(can_publish.contains("state.preflightReady"));
    assert!(can_publish.contains("state.dryRunReady"));
}

#[test]
fn provision_rejects_stale_results_and_never_invokes_other_commands() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onProvision()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("tauri.invoke(\"provision_project\""));
    for forbidden in [
        "publish_project",
        "dry_run_project",
        "recover_project",
        "create_project_setup",
        "validate_project",
        "preflight_project",
        "get_credential_status",
        "set_credential",
        "delete_credential",
        "credentialsState",
    ] {
        assert!(
            !handler.contains(forbidden),
            "provisioning must never reference {forbidden}"
        );
    }
    // Generation/stale discipline identical to the other pipeline handlers.
    assert!(handler.contains("beginOperation()"));
    let await_pos = handler.find("await tauri.invoke").unwrap();
    let guard_pos = handler.find("isCurrentOperation(").unwrap();
    assert!(
        await_pos < guard_pos,
        "the generation check must follow the await"
    );
    let stale_block = handler[handler.find("isCurrentOperation(").unwrap()..]
        .split("return;")
        .next()
        .unwrap();
    assert!(
        !stale_block.contains("setBusy"),
        "stale provisioning never touches the busy state"
    );
    // Busy guard: generation is captured before the async boundary.
    let busy_pos = handler.find("setBusy(true").unwrap();
    assert!(handler.find("beginOperation()").unwrap() < busy_pos);
}

#[test]
fn provision_renders_per_resource_results_without_secrets_or_html() {
    let script = read_source("/ui/app.js");
    let render = script
        .split("function renderProvisionResources(")
        .nth(1)
        .expect("renderProvisionResources must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(render.contains("textContent"));
    assert!(render.contains("appendChild"));
    assert!(!render.contains("innerHTML"));
    // All four resource keys are rendered when present; statuses are mapped
    // from the contract vocabulary.
    for key in ["repository", "storage", "hosting", "domain"] {
        assert!(render.contains(key), "render must cover {key}");
    }
    for status in ["created", "unchanged", "configured", "changed"] {
        assert!(script.contains(status), "status mapping must know {status}");
    }
    // Partial failure: the error rendering consumes the embedded report.
    let error = script
        .split("function showProvisionError(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(error.contains("error.report"));
}

#[test]
fn path_change_invalidates_provisioning_too() {
    let script = read_source("/ui/app.js");
    let path_handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "state.configReady = false",
        "state.provisioned = false",
        "el(\"provision-button\").disabled = true",
        "hide(\"provision-result\")",
    ] {
        assert!(
            path_handler.contains(required),
            "path change must contain {required}"
        );
    }
}

#[test]
fn preflight_rerun_invalidates_the_previous_dry_run() {
    // Audit fix (Phase 7-F microfix): starting a new Preflight is a new
    // precondition check, so the previous Dry Run result no longer
    // represents the current state. Both invalidations must happen before
    // the backend call — never after, never conditionally on success.
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onPreflight()")
        .nth(1)
        .expect("onPreflight must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "state.preflightReady = false",
        "state.dryRunReady = false",
        "hide(\"preflight-result\")",
        "hide(\"dry-run-result\")",
    ] {
        assert!(
            handler.contains(required),
            "onPreflight must contain {required}"
        );
    }
    let await_pos = handler
        .find("await tauri.invoke(\"preflight_project\"")
        .unwrap();
    for invalidation in [
        "state.preflightReady = false",
        "state.dryRunReady = false",
        "hide(\"preflight-result\")",
        "hide(\"dry-run-result\")",
    ] {
        let pos = handler.find(invalidation).unwrap();
        assert!(
            pos < await_pos,
            "{invalidation} must happen before the preflight await"
        );
    }
    // Discipline order: generation first, invalidations next, busy last.
    let generation_pos = handler.find("beginOperation()").unwrap();
    let busy_pos = handler.find("setBusy(true").unwrap();
    let invalidate_pos = handler.find("state.dryRunReady = false").unwrap();
    assert!(generation_pos < invalidate_pos && invalidate_pos < busy_pos);
    // A successful Preflight re-enables the Dry Run but never the Publish:
    // the publish gate continues to require a fresh dryRunReady.
    let success_branch = handler
        .split("state.preflightReady = Boolean(outcome.ready)")
        .nth(1)
        .unwrap();
    let success_stop = success_branch.find("} catch (error) {").unwrap();
    let success = &success_branch[..success_stop];
    assert!(!success.contains("state.dryRunReady = true"));
    let can_publish = script
        .split("function canPublish()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(can_publish.contains("state.dryRunReady"));
}

// ---------------------------------------------------------------------------
// Phase 7-K.2 — Explicit local publication bootstrap
//
// The committed local publication that Dry Run plans against — until now
// created only inside Publish — becomes an explicit UI step between Preflight
// and Dry Run. These tests pin the UI contract, the stale/ABA discipline of
// the new handler, and the backend boundary of the new command.
// ---------------------------------------------------------------------------

#[test]
fn local_bootstrap_step_button_and_result_exist_between_preflight_and_dry_run() {
    let html = read_source("/ui/index.html");
    for id in [
        "id=\"step-prepare\"",
        "id=\"prepare-local-button\"",
        "id=\"prepare-local-result\"",
        "id=\"prepare-local-generation\"",
    ] {
        assert!(html.contains(id), "index.html must contain {id}");
    }
    // The visual sequence gains the explicit step:
    // Preflight < Preparar local < Dry Run < Publicar.
    let preflight = html.find("id=\"step-preflight\"").unwrap();
    let prepare = html.find("id=\"step-prepare\"").unwrap();
    let dryrun = html.find("id=\"step-dryrun\"").unwrap();
    let publish = html.find("id=\"step-publish\"").unwrap();
    assert!(preflight < prepare && prepare < dryrun && dryrun < publish);
    let script = read_source("/ui/app.js");
    assert!(
        script.contains("el(\"prepare-local-button\").addEventListener(\"click\", onPrepareLocal)")
    );
    // The new step participates in the visual progress highlighting.
    let pipeline = script
        .split("function setPipelineStep()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(pipeline.contains("\"step-prepare\""));
}

#[test]
fn local_bootstrap_gates_dry_run_and_keeps_publish_on_dry_run() {
    let script = read_source("/ui/app.js");
    // The bootstrap itself requires validation and a fresh preflight.
    let guard = script
        .split("function canPrepareLocal()")
        .nth(1)
        .expect("canPrepareLocal must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "!state.busy",
        "state.validated",
        "state.preflightReady",
        "currentPath() === state.projectPath",
    ] {
        assert!(
            guard.contains(required),
            "canPrepareLocal must require {required}"
        );
    }
    let handler = script
        .split("async function onPrepareLocal()")
        .nth(1)
        .expect("onPrepareLocal must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(handler.contains("if (!canPrepareLocal()) return;"));
    // Dry Run now additionally requires the committed local publication…
    let can_dry_run = script
        .split("function canDryRun()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(can_dry_run.contains("state.localReady"));
    // …at the button gate AND inside the handler (defense in depth).
    let on_dry_run = script
        .split("async function onDryRun()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(on_dry_run.contains("if (!state.localReady) return;"));
    // Publish keeps its existing gate: dryRunReady only, never localReady.
    let can_publish = script
        .split("function canPublish()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(can_publish.contains("state.dryRunReady"));
    assert!(
        !can_publish.contains("state.localReady"),
        "publish keeps its existing gate"
    );
}

#[test]
fn local_bootstrap_success_unlocks_dry_run_and_invalidates_previous_plans() {
    let script = read_source("/ui/app.js");
    let handler = script
        .split("async function onPrepareLocal()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    // The run invalidates the previous local/dry-run state BEFORE awaiting.
    let await_pos = handler.find("await tauri.invoke").unwrap();
    assert!(handler.find("state.localReady = false").unwrap() < await_pos);
    assert!(handler.find("state.dryRunReady = false").unwrap() < await_pos);
    // Success: local ready, previous dry-run plan invalidated. The try
    // block carries the state assignments before rendering the outcome.
    let success = handler
        .split("try {")
        .nth(1)
        .expect("the success branch must exist")
        .split("} catch (error) {")
        .next()
        .unwrap();
    assert!(success.contains("state.localReady = true"));
    assert!(success.contains("state.dryRunReady = false"));
    assert!(success.contains("showPrepareLocal(outcome);"));
    assert!(success.contains("el(\"dry-run-button\").disabled = !canDryRun()"));
    // Failure: stays not ready.
    let failure = handler.split("} catch (error) {").nth(1).unwrap();
    assert!(failure.contains("state.localReady = false"));
}

#[test]
fn local_bootstrap_is_invalidated_by_path_change_validation_and_preflight() {
    let script = read_source("/ui/app.js");
    // A path change clears the step completely.
    let path_handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "state.localReady = false",
        "el(\"prepare-local-button\").disabled = true",
        "hide(\"prepare-local-result\")",
    ] {
        assert!(
            path_handler.contains(required),
            "path change must contain {required}"
        );
    }
    // Revalidation restarts the flow for the same path.
    let validate_handler = script
        .split("async function onValidate()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(validate_handler.contains("state.localReady = false"));
    // A new Preflight invalidates downstream state before its await.
    let preflight_handler = script
        .split("async function onPreflight()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    let await_pos = preflight_handler.find("await tauri.invoke").unwrap();
    let invalidation = preflight_handler.find("state.localReady = false").unwrap();
    assert!(
        invalidation < await_pos,
        "the preflight invalidation must precede the await"
    );
}

#[test]
fn prepare_local_command_is_registered_and_stays_a_pure_local_adapter() {
    // Library side: a synchronous command delegating to the application use
    // case — with no provider, credential, or environment surface.
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("pub fn prepare_local_publication"));
    assert!(commands.contains("publisher_app::prepare_local_publication"));
    let body = commands
        .split("pub fn prepare_local_publication")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "build_providers(",
        "build_provisioners(",
        "credential_store(",
        "EnvironmentCredentialStore",
        "std::env",
        "reqwest",
    ] {
        assert!(
            !body.contains(forbidden),
            "the local bootstrap must not reference {forbidden}"
        );
    }
    // Binary side: registered exactly once in the handler list, behind the
    // same async boundary as the other long operations. The name appears
    // exactly four times: doc comment, definition, call, registration.
    let main = read_source("/src/main.rs");
    assert_eq!(main.matches("prepare_local_publication").count(), 4);
    assert!(main.contains("async fn prepare_local_publication"));
}

// ---------------------------------------------------------------------------
// Phase 7-J concurrency fix — backend-exclusive provisioning slot
//
// The UI can free its busy flag while a provisioning worker is still
// running (a path change invalidates the current operation), so JavaScript
// can never be the real exclusion. These structural tests pin the binary
// wiring: Tauri-managed gate, atomic acquisition BEFORE any blocking work,
// immediate refusal that runs nothing, and a permit that only the worker's
// completion can release.
// ---------------------------------------------------------------------------

#[test]
fn provisioning_exclusion_is_managed_state_acquired_before_the_blocking_work() {
    let main = read_source("/src/main.rs");
    // The gate is registered as Tauri-managed state at bootstrap.
    assert!(
        main.contains(".manage(ProvisioningGate::new())"),
        "the gate must be registered as managed state"
    );
    let wrapper = main
        .split("async fn provision_project")
        .nth(1)
        .expect("the provisioning wrapper must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // Acquisition precedes the blocking task...
    let acquire = wrapper
        .find("try_acquire()")
        .expect("the wrapper must acquire the gate");
    let spawn = wrapper
        .find("spawn_blocking")
        .expect("the wrapper must run the work in a blocking task");
    assert!(
        acquire < spawn,
        "the slot must be acquired before any blocking work is queued"
    );
    // ...and the refusal is immediate: it returns before anything is queued.
    let refusal = wrapper
        .find("provisioning_busy_failure()")
        .expect("the refusal must be the dedicated busy failure");
    assert!(refusal < spawn, "a refused attempt must not queue any work");
    // The refusal branch is a pure return: no command, no runner, no spawn.
    let refusal_block = wrapper[wrapper.find("else {").unwrap()..]
        .split('}')
        .next()
        .unwrap();
    for forbidden in [
        "provision_project",
        "provision_exclusive",
        "spawn_blocking",
        "build_provisioners",
    ] {
        assert!(
            !refusal_block.contains(forbidden),
            "the refusal must never reference {forbidden}"
        );
    }
    assert!(refusal_block.contains("return Err"));
    assert!(refusal_block.contains("provisioning_busy_failure()"));
}

#[test]
fn the_permit_covers_the_whole_blocking_execution_and_only_the_worker_releases_it() {
    let main = read_source("/src/main.rs");
    let wrapper = main
        .split("async fn provision_project")
        .nth(1)
        .expect("the provisioning wrapper must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // The acquired permit moves INTO the blocking worker and is handed to
    // the exclusive runner, so the slot stays held for the entire
    // execution and is released exactly when the worker finishes.
    let exclusive_call = wrapper
        .split("provision_exclusive(")
        .nth(1)
        .expect("the wrapper must run the work through the exclusive runner");
    assert!(
        exclusive_call.contains("permit,"),
        "the permit must move into the exclusive runner"
    );
    assert!(
        exclusive_call.contains("publisher_desktop::commands::provision_project"),
        "the work must be the provisioning command"
    );
    // The wrapper itself never releases the slot: exactly one acquisition
    // point, and no explicit drop of the permit outside the worker.
    assert_eq!(
        wrapper.matches("try_acquire()").count(),
        1,
        "exactly one acquisition point exists"
    );
    assert!(
        !wrapper.contains("drop(permit"),
        "the wrapper must never release the slot itself"
    );
}

#[test]
fn the_exclusive_slot_is_a_permit_released_only_by_the_worker_completion() {
    // Library side of the contract: the gate is managed-state friendly, the
    // permit is a data-free RAII token whose drop is the single release
    // point, the exclusive runner holds it for its whole body, and the
    // refusal is pure fixed data.
    let commands = read_source("/src/commands.rs");
    for required in [
        "pub struct ProvisioningGate",
        "pub struct ProvisioningPermit",
        "impl Drop for ProvisioningPermit",
        "pub fn try_acquire",
        "pub fn provision_exclusive",
        "pub fn provisioning_busy_failure",
        "pub const PROVISIONING_BUSY_KIND",
    ] {
        assert!(
            commands.contains(required),
            "the gate must define {required}"
        );
    }
    // The runner holds the permit for its whole body and only then drops it.
    let runner = commands
        .split("pub fn provision_exclusive")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(runner.contains("let _held = permit;"));
    assert!(runner.contains("work(project_path, events)"));
    // The refusal constructs pure data only: no provisioner, no application
    // service, no environment, no path.
    let refusal = commands
        .split("pub fn provisioning_busy_failure")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "provision_project(",
        "build_provisioners",
        "std::env",
        "reqwest",
        "project_path",
    ] {
        assert!(
            !refusal.contains(forbidden),
            "the refusal must not reference {forbidden}"
        );
    }
    assert!(refusal.contains("PROVISIONING_BUSY_KIND"));
}

// ---------------------------------------------------------------------------
// Phase 7-K.5 — Desktop Wiring: provisioning in the visual flow
//
// The provisioning backend existed since 7-J; this phase connects it to the
// pipeline order (Validar → Preflight → Provisionar → Preparar → Dry Run →
// Publicar) and to the single event channel. These tests pin: the new
// pipeline step, the preflight gate, the ephemeral `provisioned` state, the
// event bridge, and the per-resource activity rendering.
// ---------------------------------------------------------------------------

#[test]
fn pipeline_shows_provisionar_between_preflight_and_prepare() {
    let html = read_source("/ui/index.html");
    assert!(html.contains("id=\"step-provision\""));
    let preflight = html.find("id=\"step-preflight\"").unwrap();
    let provision = html.find("id=\"step-provision\"").unwrap();
    let prepare = html.find("id=\"step-prepare\"").unwrap();
    let dryrun = html.find("id=\"step-dryrun\"").unwrap();
    assert!(
        preflight < provision && provision < prepare && prepare < dryrun,
        "pipeline order must be preflight → provision → prepare → dry-run"
    );
    // The provisioning section sits in the same visual order, after the
    // preflight result and before the local bootstrap result.
    let preflight_result = html.find("id=\"preflight-result\"").unwrap();
    let provision_section = html.find("id=\"provision-section\"").unwrap();
    let prepare_result = html.find("id=\"prepare-local-result\"").unwrap();
    assert!(preflight_result < provision_section && provision_section < prepare_result);
    // The step participates in the visual progress highlighting.
    let script = read_source("/ui/app.js");
    let pipeline = script
        .split("function setPipelineStep()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(pipeline.contains("\"step-provision\""));
}

#[test]
fn provisioned_is_ephemeral_state_invalidated_by_path_change_and_revalidation() {
    let script = read_source("/ui/app.js");
    // The session-only flag exists and starts false; nothing persists it.
    assert!(script.contains("provisioned: false"));
    let path_handler = script
        .split("function onProjectPathChanged()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(path_handler.contains("state.provisioned = false"));
    let validate_handler = script
        .split("async function onValidate()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(validate_handler.contains("state.provisioned = false"));
    // A new provisioning run resets the verdict before the await boundary.
    let handler = script
        .split("async function onProvision()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    let await_pos = handler.find("await tauri.invoke").unwrap();
    let reset = handler.find("state.provisioned = false").unwrap();
    assert!(reset < await_pos, "the verdict resets before the async run");
}

#[test]
fn provisioning_events_reach_the_single_channel_and_the_activity_feed() {
    // Binary: the provisioning wrapper forwards events exactly like publish
    // and prepare-local do — same channel, same adapter, same boundary.
    let main = read_source("/src/main.rs");
    let wrapper = main
        .split("async fn provision_project")
        .nth(1)
        .expect("the provisioning wrapper must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    for required in [
        "tauri::AppHandle",
        "app_handle.emit(",
        "PUBLISHER_EVENT_CHANNEL",
        "DesktopEvent::from(&event)",
        "&mut sink",
    ] {
        assert!(
            wrapper.contains(required),
            "the provisioning wrapper must forward events ({required})"
        );
    }
    assert_eq!(wrapper.matches("app_handle.emit(").count(), 1);
    // Command: the adapter takes the sink and hands it to the app service.
    let commands = read_source("/src/commands.rs");
    let command = commands
        .split("pub fn provision_project")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(command.contains("events: &mut EventSink"));
    assert!(command.contains("publisher_app::provision_project("));
    assert!(command.contains("events,"));
    // Adapter: a dedicated, stable provisioning payload exists.
    let events = read_source("/src/events.rs");
    for required in [
        "Provisioning(ProvisioningEventDto)",
        "\"provisioning\"",
        "\"finished_ok\"",
        "\"finished_failed\"",
        "\"repository\"",
        "\"storage\"",
        "\"hosting\"",
        "\"domain\"",
    ] {
        assert!(
            events.contains(required),
            "the desktop event adapter must define {required}"
        );
    }
    // UI: the activity feed renders the granular lifecycle.
    let script = read_source("/ui/app.js");
    assert!(script.contains("event.type === \"provisioning\""));
    let describe = script
        .split("function describePublisherEvent(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(describe.contains("finished_failed"));
    let workflow = script
        .split("const WORKFLOW_STEP_MESSAGES =")
        .nth(1)
        .unwrap()
        .split("};")
        .next()
        .unwrap();
    assert!(workflow.contains("provisioning:"));
    let resources = script
        .split("const PROVISION_STEP_MESSAGES =")
        .nth(1)
        .unwrap()
        .split("};")
        .next()
        .unwrap();
    for key in ["repository", "storage", "hosting", "domain"] {
        assert!(
            resources.contains(key),
            "activity messages must cover {key}"
        );
    }
    // A finished marker carries its outcome: a failed resource never
    // renders as success.
    assert!(resources.contains("finished_failed"));
}

#[test]
fn a_partial_provisioning_is_never_presented_as_a_full_success() {
    // The error branch renders the embedded partial report and names it a
    // partial completion; success styling stays exclusive to the Ok path.
    let script = read_source("/ui/app.js");
    let error = script
        .split("function showProvisionError(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(error.contains("error.report"));
    assert!(error.contains("parcial"));
    assert!(error.contains("attention"));
    assert!(!error.contains("\"ok\""));
    let success = script
        .split("function showProvision(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(success.contains("classList.add(\"ok\")"));
    assert!(!success.contains("attention"));
}

#[test]
fn provisioning_commands_keep_their_shape_and_stay_within_their_scope() {
    // The command set is unchanged (thirteen commands, one channel), and the
    // provisioning command remains a pure adapter: gate → composition root →
    // publisher_app::provision_project. No provisioning rule lives here.
    let main = read_source("/src/main.rs");
    assert!(main.contains("provision_project"));
    let commands = read_source("/src/commands.rs");
    assert!(commands.contains("pub fn provision_project"));
    assert!(commands.contains("publisher_app::provision_project"));
    let body = commands
        .split("pub fn provision_project")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "publish_project(",
        "dry_run_project(",
        "recover_project(",
        "create_project_setup(",
        "prepare_local_publication(",
        "GitHubRepositoryProvisioner",
        "R2StorageProvisioner",
        "VercelHostingProvisioner",
        "EnvironmentCredentialStore",
        "std::env",
        "reqwest",
        ".publish(",
        ".put(",
        ".commit(",
        ".delete(",
    ] {
        assert!(
            !body.contains(forbidden),
            "provision_project command must not contain {forbidden}"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 7-K.6 — PublicationGate: one publication at a time
//
// The JavaScript busy flag is presentation only; the real exclusion is a
// Tauri-managed backend gate, acquired atomically before any blocking work
// is queued and released only by the worker finishing. These structural
// tests pin the binary/library wiring and the UI presentation of a refused
// concurrent publication.
// ---------------------------------------------------------------------------

#[test]
fn publication_exclusion_is_managed_state_acquired_before_the_blocking_work() {
    let main = read_source("/src/main.rs");
    // The publication gate is registered as Tauri-managed state, as an
    // INDEPENDENT slot next to the provisioning one.
    assert!(
        main.contains(".manage(PublicationGate::new())"),
        "the publication gate must be registered as managed state"
    );
    assert!(main.contains(".manage(ProvisioningGate::new())"));
    let wrapper = main
        .split("async fn publish_project")
        .nth(1)
        .expect("the publication wrapper must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // Acquisition precedes the blocking task...
    let acquire = wrapper
        .find("try_acquire()")
        .expect("the wrapper must acquire the gate");
    let spawn = wrapper
        .find("spawn_blocking")
        .expect("the wrapper must run the work in a blocking task");
    assert!(
        acquire < spawn,
        "the slot must be acquired before any blocking work is queued"
    );
    // ...and the refusal is immediate: it returns before anything is queued.
    let refusal = wrapper
        .find("publication_busy_error()")
        .expect("the refusal must be the dedicated busy error");
    assert!(refusal < spawn, "a refused attempt must not queue any work");
    // The refusal branch is a pure return: no command, no runner, no spawn.
    let refusal_block = wrapper[wrapper.find("else {").unwrap()..]
        .split('}')
        .next()
        .unwrap();
    for forbidden in [
        "publish_project(",
        "publish_exclusive",
        "spawn_blocking",
        "build_providers",
    ] {
        assert!(
            !refusal_block.contains(forbidden),
            "the refusal must never reference {forbidden}"
        );
    }
    assert!(refusal_block.contains("return Err"));
    assert!(refusal_block.contains("publication_busy_error()"));
    // A refused publication emits no events: nothing started, so the
    // activity feed must not observe a phantom run. The refusal has no sink.
    assert!(!refusal_block.contains("emit("));
}

#[test]
fn the_publication_permit_covers_the_whole_blocking_execution() {
    let main = read_source("/src/main.rs");
    let wrapper = main
        .split("async fn publish_project")
        .nth(1)
        .expect("the publication wrapper must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    // The acquired permit moves INTO the blocking worker and is handed to
    // the exclusive runner, so the slot stays held for the entire
    // publication — load, local publication, plan, providers — and is
    // released exactly when the worker completes.
    let exclusive_call = wrapper
        .split("publish_exclusive(")
        .nth(1)
        .expect("the wrapper must run the work through the exclusive runner");
    assert!(
        exclusive_call.contains("permit,"),
        "the permit must move into the exclusive runner"
    );
    assert!(
        exclusive_call.contains("publisher_desktop::commands::publish_project"),
        "the work must be the publication command"
    );
    assert!(
        exclusive_call.contains("&mut sink"),
        "the event sink must keep flowing through the runner"
    );
    // The wrapper itself never releases the slot: exactly one acquisition
    // point, and no explicit drop of the permit outside the worker.
    assert_eq!(
        wrapper.matches("try_acquire()").count(),
        1,
        "exactly one acquisition point exists"
    );
    assert!(
        !wrapper.contains("drop(permit"),
        "the wrapper must never release the slot itself"
    );
}

#[test]
fn the_publication_slot_primitives_live_in_the_command_layer() {
    // Library side of the contract: the gate is managed-state friendly, the
    // permit is a data-free RAII token whose drop is the single release
    // point, the exclusive runner holds it for its whole body, and the
    // refusal is pure fixed data with a distinct kind.
    let commands = read_source("/src/commands.rs");
    for required in [
        "pub struct PublicationGate",
        "pub struct PublicationPermit",
        "impl Drop for PublicationPermit",
        "pub fn try_acquire",
        "pub fn publish_exclusive",
        "pub fn publication_busy_error",
        "pub const PUBLICATION_BUSY_KIND",
        "\"publication_busy\"",
    ] {
        assert!(
            commands.contains(required),
            "the publication gate must define {required}"
        );
    }
    let runner = commands
        .split("pub fn publish_exclusive")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(runner.contains("let _held = permit;"));
    assert!(runner.contains("work(project_path, events)"));
    // The refusal constructs pure data only: no provider, no application
    // service, no environment, no path.
    let refusal = commands
        .split("pub fn publication_busy_error")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    for forbidden in [
        "publish_project(",
        "build_providers",
        "std::env",
        "reqwest",
        "project_path",
    ] {
        assert!(
            !refusal.contains(forbidden),
            "the publication refusal must not reference {forbidden}"
        );
    }
    assert!(refusal.contains("PUBLICATION_BUSY_KIND"));
}

#[test]
fn only_publish_acquires_the_publication_gate() {
    // The gate belongs exclusively to Publish: no other command wrapper may
    // reference it (dry-run stays free of providers/network/credentials and
    // never holds the publication slot; provisioning keeps its own gate).
    let main = read_source("/src/main.rs");
    for wrapper_name in [
        "async fn publish_project",
        "async fn provision_project",
        "async fn prepare_local_publication",
        "fn dry_run_project",
        "fn recover_project",
        "fn validate_project",
        "fn preflight_project",
    ] {
        let wrapper = main
            .split(wrapper_name)
            .nth(1)
            .unwrap_or_else(|| panic!("{wrapper_name} must exist"))
            .split("\n}\n")
            .next()
            .unwrap();
        if wrapper_name == "async fn publish_project" {
            assert!(
                wrapper.contains("PublicationGate"),
                "publish must acquire the publication gate"
            );
        } else {
            assert!(
                !wrapper.contains("PublicationGate"),
                "{wrapper_name} must not touch the publication gate"
            );
        }
    }
    assert!(main.contains("pub struct PublicationGate") || main.contains("PublicationGate,"));
}

#[test]
fn a_refused_concurrent_publish_is_presented_distinctly_and_invalidates_nothing() {
    // UI side of 7-K.6: the rejection is shown as "another publication is
    // running" — never as a failed publication — and it must leave every
    // piece of valid state untouched (no dryRunReady/localReady/provisioned
    // invalidation, no activity pollution, no result cleanup of the run in
    // flight).
    let script = read_source("/ui/app.js");
    assert!(script.contains("function showPublishBusy("));
    let busy = script
        .split("function showPublishBusy(")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(busy.contains("textContent"));
    assert!(!busy.contains("innerHTML"));
    let handler = script
        .split("async function onPublish()")
        .nth(1)
        .expect("onPublish must exist")
        .split("\n}\n")
        .next()
        .unwrap();
    let catch = handler.split("} catch (error) {").nth(1).unwrap();
    assert!(catch.contains("error.kind === \"publication_busy\""));
    // The busy branch and its surroundings invalidate nothing.
    let busy_branch = catch
        .split("error.kind === \"publication_busy\"")
        .nth(1)
        .unwrap();
    for forbidden in [
        "state.dryRunReady = false",
        "state.preflightReady = false",
        "state.localReady = false",
        "state.provisioned = false",
        "state.validated = false",
        "hide(\"dry-run-result\")",
        "hide(\"preflight-result\")",
        "addActivity(",
    ] {
        assert!(
            !busy_branch.contains(forbidden),
            "a refused publication must not contain {forbidden}"
        );
    }
    // The failure presentation stays distinct: the generic failure path
    // still exists for real failures.
    assert!(catch.contains("showPublishError(error)"));
    assert!(catch.contains("Falha na publicação."));
}
