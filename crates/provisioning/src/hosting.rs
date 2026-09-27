//! Hosting provisioning boundary.
//!
//! Creates/configures the hosting *project* a site is deployed to. The
//! project is the resource; deployments belong to the publishing side,
//! never to this contract.

use std::fmt;

use photo_publisher_provider_contracts::CredentialStore;

use crate::errors::{ProvisioningError, ProvisioningErrorKind, ProvisioningResult};
use crate::outcome::ProvisioningOutcome;

/// Stable public identity of a hosting project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostingIdentity {
    pub project: String,
}

impl fmt::Display for HostingIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.project)
    }
}

/// What hosting provisioning needs: the project name and the optional team
/// scope — both already declared by `project.json` (`hosting.project`,
/// `hosting.teamId`). Frameworks, build settings and root directories have
/// no declarative home yet: a documented gap for a later phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostingProvisionConfig {
    pub project: String,
    pub team_id: Option<String>,
}

impl HostingProvisionConfig {
    /// Structural validation only: the project name must be non-blank and
    /// free of separators/whitespace; a present team id must not be blank.
    pub fn new(project: impl Into<String>, team_id: Option<String>) -> ProvisioningResult<Self> {
        let project = project.into();
        let project_ok = !project.trim().is_empty()
            && !project.contains('/')
            && !project.chars().any(char::is_whitespace);
        let team_ok = !team_id
            .as_deref()
            .is_some_and(|team| team.trim().is_empty());
        if !project_ok || !team_ok {
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::InvalidConfiguration,
                "hosting identity must be a non-blank project name (and a non-blank team id when present)",
            ));
        }
        Ok(Self { project, team_id })
    }

    /// The public identity this configuration refers to.
    pub fn identity(&self) -> HostingIdentity {
        HostingIdentity {
            project: self.project.clone(),
        }
    }
}

/// Creates or reconciles a hosting project. This is the only verb:
/// provisioning never deploys sites or uploads files.
pub trait HostingProvisioner {
    /// Ensures the hosting project described by `config` exists as desired.
    ///
    /// Idempotency contract: an existing compatible project yields
    /// `Unchanged`; a reconciled one yields `Configured` or `Changed`;
    /// divergence that cannot be reconciled automatically is a `Conflict`
    /// error. Credentials come exclusively from `credentials` and are never
    /// copied into configurations, outcomes, errors, or logs.
    fn provision_hosting(
        &mut self,
        config: &HostingProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<HostingIdentity>>;
}
