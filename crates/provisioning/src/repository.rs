//! Repository provisioning boundary.
//!
//! Creates/configures the *existence and settings* of the repository a
//! project publishes to. Operates on infrastructure, never on content:
//! writing files or commits belongs to the publishing side, not here.
//!
//! Configurations carry only data derivable from `project.json`
//! (`repository.provider`, `repository.repository`, `repository.branch`).
//! Properties without a declarative home yet — e.g. repository visibility
//! or a description — are a documented gap for a later phase, not a reason
//! to extend the schema silently.

use std::fmt;

use photo_publisher_provider_contracts::CredentialStore;

use crate::errors::{ProvisioningError, ProvisioningErrorKind, ProvisioningResult};
use crate::outcome::ProvisioningOutcome;

/// Stable public identity of a repository: `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryIdentity {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for RepositoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

/// What a repository provisioning operation needs: purely declarative
/// identity data. No credentials, no secrets, no remote payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryProvisionConfig {
    pub owner: String,
    pub name: String,
}

impl RepositoryProvisionConfig {
    /// Structural validation only: both parts must be non-blank and free of
    /// separators/whitespace (the same `owner/name` shape the publication
    /// composition already splits on). Provider-side existence or validity
    /// is *not* decided here.
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> ProvisioningResult<Self> {
        let owner = owner.into();
        let name = name.into();
        let valid = [&owner, &name].iter().all(|part| {
            !part.trim().is_empty() && !part.contains('/') && !part.chars().any(char::is_whitespace)
        });
        if !valid {
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::InvalidConfiguration,
                "repository identity must be a non-blank 'owner/name' pair",
            ));
        }
        Ok(Self { owner, name })
    }

    /// The public identity this configuration refers to.
    pub fn identity(&self) -> RepositoryIdentity {
        RepositoryIdentity {
            owner: self.owner.clone(),
            name: self.name.clone(),
        }
    }
}

/// Creates or reconciles a repository. This is the only verb: provisioning
/// does not read content, write content, commit, or delete anything.
pub trait RepositoryProvisioner {
    /// Ensures the repository described by `config` exists as desired.
    ///
    /// Idempotency contract: an existing compatible repository yields
    /// `Unchanged` (a deliberate no-op, not an error); a reconciled one
    /// yields `Configured` or `Changed`; divergence that cannot be
    /// reconciled automatically is a `Conflict` *error*. Credentials, when
    /// needed, come exclusively from `credentials` and are consulted by the
    /// backend — they are never copied into configurations, outcomes,
    /// errors, or logs.
    fn provision_repository(
        &mut self,
        config: &RepositoryProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<RepositoryIdentity>>;
}
