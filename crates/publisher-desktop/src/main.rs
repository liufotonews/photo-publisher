//! Desktop application binary entrypoint.
//!
//! The library (`src/lib.rs`) carries the composition root and the command
//! logic; this binary adds the minimal Tauri bootstrap on top of them and
//! registers the thin `#[tauri::command]` wrappers. It contains no business
//! logic, never constructs providers, never reads credentials, and never
//! touches the publication engine beyond what application commands delegate.
//!
//! Opening the application is not a publication preflight: the bootstrap
//! succeeds on a machine without any `PHOTO_PUBLISHER_*` variables.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use publisher_desktop::commands::{AppInfo, CommandError, ValidateProjectOutcome};

/// Minimal, inert application state registered with Tauri.
///
/// It holds nothing (no credentials, no project, no plan, no results); real
/// state is introduced only when desktop commands need it.
#[derive(Debug, Default)]
struct DesktopState;

#[tauri::command]
fn get_app_info() -> AppInfo {
    publisher_desktop::commands::get_app_info()
}

#[tauri::command]
fn validate_project(project_path: &str) -> Result<ValidateProjectOutcome, CommandError> {
    publisher_desktop::commands::validate_project(project_path)
}

fn main() {
    tauri::Builder::default()
        .manage(DesktopState)
        .invoke_handler(tauri::generate_handler![get_app_info, validate_project])
        .run(tauri::generate_context!("tauri.conf.json"))
        .expect("error while running the Photo Publisher desktop application");
}
