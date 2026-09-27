//! Storage provisioning boundary.
//!
//! Creates/configures the object-storage bucket a project publishes to.
//! The bucket is the resource; key prefixes inside it are a *publishing*
//! namespace concern (`project.json`'s `prefix`) and are intentionally not
//! part of this contract.

use std::fmt;

use photo_publisher_provider_contracts::CredentialStore;

use crate::errors::{ProvisioningError, ProvisioningErrorKind, ProvisioningResult};
use crate::outcome::ProvisioningOutcome;

/// Stable public identity of a storage bucket: `account_id/bucket`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageIdentity {
    pub account_id: String,
    pub bucket: String,
}

impl fmt::Display for StorageIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.account_id, self.bucket)
    }
}

/// What storage provisioning needs: the account scope and the bucket name.
/// Credentials stay outside this structure, always.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageProvisionConfig {
    pub account_id: String,
    pub bucket: String,
}

impl StorageProvisionConfig {
    /// Structural validation only: both parts must be non-blank and free of
    /// separators/whitespace. Provider-side rules (length, DNS-safe names,
    /// etc.) belong to the future concrete provisioner.
    pub fn new(
        account_id: impl Into<String>,
        bucket: impl Into<String>,
    ) -> ProvisioningResult<Self> {
        let account_id = account_id.into();
        let bucket = bucket.into();
        let valid = [&account_id, &bucket].iter().all(|part| {
            !part.trim().is_empty() && !part.contains('/') && !part.chars().any(char::is_whitespace)
        });
        if !valid {
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::InvalidConfiguration,
                "storage identity must be a non-blank 'account/bucket' pair",
            ));
        }
        Ok(Self { account_id, bucket })
    }

    /// The public identity this configuration refers to.
    pub fn identity(&self) -> StorageIdentity {
        StorageIdentity {
            account_id: self.account_id.clone(),
            bucket: self.bucket.clone(),
        }
    }
}

/// Creates or reconciles a storage bucket. This is the only verb:
/// provisioning never uploads, reads, or deletes objects.
pub trait StorageProvisioner {
    /// Ensures the bucket described by `config` exists as desired.
    ///
    /// Idempotency contract: an existing compatible bucket yields
    /// `Unchanged`; a reconciled one yields `Configured` or `Changed`;
    /// divergence that cannot be reconciled automatically is a `Conflict`
    /// error. Credentials come exclusively from `credentials` and are never
    /// copied into configurations, outcomes, errors, or logs.
    fn provision_storage(
        &mut self,
        config: &StorageProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<StorageIdentity>>;
}
