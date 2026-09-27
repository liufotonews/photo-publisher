//! Desktop application binary entrypoint.
//!
//! The library (`src/lib.rs`) carries the composition root, the application
//! commands, and the event adapter; this binary adds the minimal Tauri
//! bootstrap and the event bridge on top of it. It contains no business
//! logic, never constructs providers, never reads credentials, and never
//! touches the publication engine — all of that stays behind
//! `publisher_desktop::composition` and is invoked only when a command asks
//! for it.
//!
//! Opening the application is not a publication preflight: the bootstrap
//! succeeds on a machine without any `PHOTO_PUBLISHER_*` variables.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use publisher_desktop::commands::{
    AppInfo, CommandError, CreateProjectSetupOutcomeDto, CredentialStatusDto, DryRunOutcomeDto,
    PreflightOutcomeDto, PublishOutcomeDto, RecoverOutcomeDto, ValidateConfigurationOutcomeDto,
    ValidateProjectOutcome,
};
use tauri::Emitter;

/// Minimal, inert application state registered with Tauri.
///
/// It holds nothing (no credentials, no project, no plan, no results); real
/// state is introduced only when the desktop runtime requires it.
#[derive(Debug, Default)]
struct DesktopState;

#[tauri::command]
fn get_app_info() -> AppInfo {
    publisher_desktop::commands::get_app_info()
}

/// The single event sink shared by every command: application events are
/// translated to the stable desktop representation and emitted over the one
/// channel. Emission failure is observation-only and never changes a result.
fn forward_events(
    app_handle: &tauri::AppHandle,
) -> impl FnMut(publisher_app::ApplicationEvent) + '_ {
    |event| {
        let payload = publisher_desktop::events::DesktopEvent::from(&event);
        let _ = app_handle.emit(publisher_desktop::events::PUBLISHER_EVENT_CHANNEL, payload);
    }
}

#[tauri::command]
fn validate_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<ValidateProjectOutcome, CommandError> {
    publisher_desktop::commands::validate_project(&project_path, &mut forward_events(&app_handle))
}

/// Publication is long-running (scanning, hashing, uploads, commit, deploy):
/// run it off the caller's context so the UI stays responsive. The heavy
/// work stays fully synchronous inside `commands::publish_project`; only the
/// boundary is async. Events are emitted from the background closure through
/// the same single channel; emission failure never changes the outcome.
#[tauri::command]
async fn publish_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<PublishOutcomeDto, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut sink = |event: publisher_app::ApplicationEvent| {
            let _ = app_handle.emit(
                publisher_desktop::events::PUBLISHER_EVENT_CHANNEL,
                publisher_desktop::events::DesktopEvent::from(&event),
            );
        };
        publisher_desktop::commands::publish_project(&project_path, &mut sink)
    })
    .await
    .map_err(|_| CommandError {
        kind: publisher_app::ApplicationErrorKind::Internal
            .as_str()
            .to_owned(),
        message: "publication task failed to join".to_owned(),
    })?
}

#[tauri::command]
fn dry_run_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<DryRunOutcomeDto, CommandError> {
    publisher_desktop::commands::dry_run_project(&project_path, &mut forward_events(&app_handle))
}

#[tauri::command]
fn recover_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<RecoverOutcomeDto, CommandError> {
    publisher_desktop::commands::recover_project(&project_path, &mut forward_events(&app_handle))
}

/// Creates a `project.json` from the wizard's payload — a synchronous, short
/// operation fully delegated to the publisher-app setup service.
#[tauri::command]
fn create_project_setup(
    project_path: String,
    setup: serde_json::Value,
) -> Result<CreateProjectSetupOutcomeDto, CommandError> {
    publisher_desktop::commands::create_project_setup(&project_path, setup)
}

/// Diagnoses the declared configuration of a `project.json` — a synchronous
/// read-and-diagnose operation fully delegated to the publisher-app
/// configuration validation (no providers, no credentials, no network).
#[tauri::command]
fn validate_project_configuration(
    project_path: String,
) -> Result<ValidateConfigurationOutcomeDto, CommandError> {
    publisher_desktop::commands::validate_project_configuration(&project_path)
}

/// Lists the configured bit of every supported credential (Phase 7-E).
/// Synchronous and local: the allowlist and the backend call are the whole
/// operation; values never leave the credential store.
#[tauri::command]
fn get_credential_status() -> Result<Vec<CredentialStatusDto>, CommandError> {
    publisher_desktop::commands::get_credential_status()
}

/// Stores one credential (Phase 7-E). The value crosses the bridge for this
/// call only and is never surfaced in results, errors, events, or logs.
#[tauri::command]
fn set_credential(name: String, value: String) -> Result<(), CommandError> {
    publisher_desktop::commands::set_credential(&name, value)
}

/// Removes one credential (Phase 7-E).
#[tauri::command]
fn delete_credential(name: String) -> Result<(), CommandError> {
    publisher_desktop::commands::delete_credential(&name)
}

/// Runs the standalone preflight (Phase 7-F) — a synchronous, local-only
/// precondition check delegated to the publisher-app use case (the
/// credential store supplies only configured bits; nothing remote runs).
#[tauri::command]
fn preflight_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<PreflightOutcomeDto, CommandError> {
    publisher_desktop::commands::preflight_project(&project_path, &mut forward_events(&app_handle))
}

fn main() {
    tauri::Builder::default()
        .manage(DesktopState)
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            validate_project,
            publish_project,
            dry_run_project,
            recover_project,
            create_project_setup,
            validate_project_configuration,
            get_credential_status,
            set_credential,
            delete_credential,
            preflight_project
        ])
        .run(tauri::generate_context!("tauri.conf.json"))
        .expect("error while running the Photo Publisher desktop application");
}
