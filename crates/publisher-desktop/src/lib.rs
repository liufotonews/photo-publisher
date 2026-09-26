//! Desktop adapter crate for Photo Publisher (Phase 6-F foundation).
//!
//! This crate hosts the desktop-specific assembly on top of
//! `publisher-app`: the composition root that constructs the concrete
//! providers and hands trait objects to the application layer. It must never
//! contain business rules — planning, ledger, executors, providers
//! validation, hashing, recovery, and idempotency stay in the existing
//! crates.
//!
//! Contains no Tauri, no UI framework, no threads, and no async runtime.

pub mod commands;
pub mod composition;
pub mod events;

pub use composition::{build_providers, preflight_publication, DesktopProviders};
