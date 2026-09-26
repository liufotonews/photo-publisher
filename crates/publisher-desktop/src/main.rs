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
    AppInfo, CommandError, DryRunOutcomeDto, PublishOutcomeDto, ValidateProjectOutcome,
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

#[tauri::command]
fn publish_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<PublishOutcomeDto, CommandError> {
    publisher_desktop::commands::publish_project(&project_path, &mut forward_events(&app_handle))
}

#[tauri::command]
fn dry_run_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<DryRunOutcomeDto, CommandError> {
    publisher_desktop::commands::dry_run_project(&project_path, &mut forward_events(&app_handle))
}

fn main() {
    tauri::Builder::default()
        .manage(DesktopState)
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            validate_project,
            publish_project,
            dry_run_project
        ])
        .run(tauri::generate_context!("tauri.conf.json"))
        .expect("error while running the Photo Publisher desktop application");
}
