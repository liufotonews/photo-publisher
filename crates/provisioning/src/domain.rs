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

/// The explicit non-implementation of domain provisioning (Phase 7-H).
///
/// The project currently has no DNS-management or domain-attachment
/// abstraction to build on, and inventing one would mean inventing an
/// unbacked API. This provisioner makes the absence *explicit and safe*:
/// every call deterministically returns
/// [`ProvisioningErrorKind::Unsupported`] without any network, credential,
/// or filesystem interaction.
pub struct UnsupportedDomainProvisioner;

impl DomainProvisioner for UnsupportedDomainProvisioner {
    fn provision_domain(
        &mut self,
        _config: &DomainProvisionConfig,
        _credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<DomainIdentity>> {
        Err(ProvisioningError::new(
            ProvisioningErrorKind::Unsupported,
            "domain provisioning is not supported by this application version (no DNS/domain backend exists)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contracts::ProviderResult;

    struct NoCredentials;

    impl CredentialStore for NoCredentials {
        fn get(&self, _name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(None)
        }
        fn set(&mut self, _name: &str, _secret: &[u8]) -> ProviderResult<()> {
            unimplemented!("unsupported provisioning never stores credentials")
        }
        fn delete(&mut self, _name: &str) -> ProviderResult<()> {
            unimplemented!("unsupported provisioning never deletes credentials")
        }
    }

    #[test]
    fn domain_provisioning_is_explicitly_and_deterministically_unsupported() {
        let mut provisioner = UnsupportedDomainProvisioner;
        let config = DomainProvisionConfig::new("galeria.exemplo.com").unwrap();
        let credentials = NoCredentials;
        let error = provisioner
            .provision_domain(&config, &credentials)
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Unsupported);
        // Deterministic, public-safe message: hostname is configuration,
        // never a credential — and the message carries nothing beyond the
        // rule itself.
        assert!(error.to_string().contains("not supported"));
        assert!(!error.to_string().contains("galeria.exemplo.com"));
        let mut again = UnsupportedDomainProvisioner;
        let repeated = again.provision_domain(&config, &credentials).unwrap_err();
        assert_eq!(repeated.kind, error.kind);
    }
}
