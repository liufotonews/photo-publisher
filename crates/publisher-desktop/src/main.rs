//! Desktop application binary entrypoint.
//!
//! The library (`src/lib.rs`) carries the composition root; this binary adds
//! the minimal Tauri bootstrap on top of it. It contains no business logic,
//! never constructs providers, never reads credentials, and never touches
//! the publication engine — all of that stays behind
//! `publisher_desktop::composition` and is invoked only when a future
//! application command asks for it.
//!
//! Opening the application is not a publication preflight: the bootstrap
//! succeeds on a machine without any `PHOTO_PUBLISHER_*` variables.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Minimal, inert application state registered with Tauri.
///
/// It holds nothing (no credentials, no project, no plan, no results); real
/// state is introduced only when desktop commands exist.
#[derive(Debug, Default)]
struct DesktopState;

fn main() {
    tauri::Builder::default()
        .manage(DesktopState)
        .run(tauri::generate_context!("tauri.conf.json"))
        .expect("error while running the Photo Publisher desktop application");
}
