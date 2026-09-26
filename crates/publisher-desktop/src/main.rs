//! Desktop application binary entrypoint.
//!
//! The library (`src/lib.rs`) carries the composition root and the application
//! commands; this binary adds the minimal Tauri bootstrap and the event
//! bridge on top of it. It contains no business logic, never constructs
//! providers, never reads credentials, and never touches the publication
//! engine — all of that stays behind `publisher_desktop::composition` and is
//! invoked only when a future application command asks for it.
//!
//! Opening the application is not a publication preflight: the bootstrap
//! succeeds on a machine without any `PHOTO_PUBLISHER_*` variables.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use publisher_desktop::commands::{AppInfo, CommandError, ValidateProjectOutcome};
use tauri::Emitter;

/// Minimal, inert application state registered with Tauri.
///
/// It holds nothing (no credentials, no project, no plan, no results); real
/// state is introduced only when desktop commands exist.
#[derive(Debug, Default)]
struct DesktopState;

#[tauri::command]
fn get_app_info() -> AppInfo {
    publisher_desktop::commands::get_app_info()
}

#[tauri::command]
fn validate_project(
    app_handle: tauri::AppHandle,
    project_path: String,
) -> Result<ValidateProjectOutcome, CommandError> {
    // The event bridge is observation-only: translation happens in the
    // library; emission failure can never turn a valid outcome into a
    // validation failure.
    let mut sink = |event: publisher_app::ApplicationEvent| {
        let payload = publisher_desktop::events::DesktopEvent::from(&event);
        let _ = app_handle.emit(publisher_desktop::events::PUBLISHER_EVENT_CHANNEL, payload);
    };
    publisher_desktop::commands::validate_project(&project_path, &mut sink)
}

fn main() {
    tauri::Builder::default()
        .manage(DesktopState)
        .invoke_handler(tauri::generate_handler![get_app_info, validate_project])
        .run(tauri::generate_context!("tauri.conf.json"))
        .expect("error while running the Photo Publisher desktop application");
}
