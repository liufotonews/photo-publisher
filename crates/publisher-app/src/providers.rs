//! Provider-neutral wiring types for the application layer.
//!
//! This module contains NO concrete provider imports. Everything is a trait
//! object reference, so a GUI, a CLI, or a test can inject fakes without any
//! provider crate being linked into the application layer.

use photo_publisher_integration::HostingPublisher;
use photo_publisher_provider_contracts::{CredentialStore, RepositoryProvider, StorageProvider};

/// Concrete provider implementations, injected by the composition root.
pub struct PublicationProviders<'a> {
    pub storage: &'a mut dyn StorageProvider,
    pub repository: &'a mut dyn RepositoryProvider,
    pub hosting: &'a mut dyn HostingPublisher,
    pub credentials: &'a dyn CredentialStore,
}
