//! Desktop adapter crate for Photo Publisher (Phase 6-F foundation).
//!
//! This crate will later host the Tauri adapter on top of `publisher-app`:
//! commands, an event bridge, and the desktop composition root. It must never
//! contain business rules — planning, ledger, executors, providers,
//! validation, hashing, recovery, and idempotency stay in the existing
//! crates.
//!
//! Phase 6-F.1 intentionally contains only this skeleton: no Tauri, no UI,
//! no providers, and no public API yet.
