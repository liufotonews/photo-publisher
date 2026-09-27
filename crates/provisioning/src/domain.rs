//! Domain provisioning boundary.
//!
//! Configures the public hostname of a published gallery (the domain side
//! only: DNS records / domain registration state). Attaching a hostname to
//! a hosting project intentionally stays out of this trait — it would
//! couple this boundary to the hosting one; that orchestration, when/if
//! needed, is a later phase's explicit decision.

use std::fmt;

use photo_publisher_provider_contracts::CredentialStore;

use crate::errors::{ProvisioningError, ProvisioningErrorKind, ProvisioningResult};
use crate::outcome::ProvisioningOutcome;

/// Stable public identity of a domain: its hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainIdentity {
    pub hostname: String,
}

impl fmt::Display for DomainIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hostname)
    }
}

/// What domain provisioning needs: the bare hostname — already declared by
/// `project.json` (`domain.url`), from which the host part is derived by
/// the caller. Schemes, paths and ports are not part of a domain identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainProvisionConfig {
    pub hostname: String,
}

impl DomainProvisionConfig {
    /// Structural validation only: a bare, non-blank hostname — no scheme,
    /// no path, no whitespace.
    pub fn new(hostname: impl Into<String>) -> ProvisioningResult<Self> {
        let hostname = hostname.into();
        let valid = !hostname.trim().is_empty()
            && !hostname.contains("://")
            && !hostname.contains('/')
            && !hostname.chars().any(char::is_whitespace);
        if !valid {
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::InvalidConfiguration,
                "domain identity must be a bare hostname, without scheme, path, or spaces",
            ));
        }
        Ok(Self { hostname })
    }

    /// The public identity this configuration refers to.
    pub fn identity(&self) -> DomainIdentity {
        DomainIdentity {
            hostname: self.hostname.clone(),
        }
    }
}

/// Creates or reconciles the domain configuration. This is the only verb:
/// provisioning never performs DNS lookups or validations over the wire.
pub trait DomainProvisioner {
    /// Ensures the domain described by `config` is configured as desired.
    ///
    /// Idempotency contract: an already-configured domain yields
    /// `Unchanged`; a reconciled one yields `Configured` or `Changed`;
    /// divergence that cannot be reconciled automatically is a `Conflict`
    /// error. Credentials come exclusively from `credentials` and are never
    /// copied into configurations, outcomes, errors, or logs.
    fn provision_domain(
        &mut self,
        config: &DomainProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<DomainIdentity>>;
}
