//! Provisioning outcomes.
//!
//! A completed provisioning operation reports *what happened* to the target
//! resource — never just "ok". The four dispositions below are the whole
//! vocabulary: an already-existing and compatible resource is explicitly a
//! success (`Unchanged`), never an error, which is what makes provisioning
//! safe to re-run (idempotency as a contract, not as a convention).

use std::fmt;

/// The disposition of a completed provisioning operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningStatus {
    /// The resource did not exist and was created.
    Created,
    /// The resource already existed with a compatible configuration; the
    /// operation was a deliberate no-op (second runs are safe).
    Unchanged,
    /// The resource existed and its configuration was brought to the
    /// desired state.
    Configured,
    /// The resource existed and a mutable aspect changed toward the desired
    /// state.
    Changed,
}

impl ProvisioningStatus {
    /// Stable machine name for the disposition.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Unchanged => "unchanged",
            Self::Configured => "configured",
            Self::Changed => "changed",
        }
    }
}

/// The outcome of one provisioning operation: disposition plus the stable
/// public identity of the affected resource.
///
/// Outcomes carry identity only. They never carry credentials, secrets,
/// provider payloads, or any mutable backend state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisioningOutcome<R> {
    Created(R),
    Unchanged(R),
    Configured(R),
    Changed(R),
}

impl<R> ProvisioningOutcome<R> {
    /// The disposition, independent of the resource payload.
    pub fn status(&self) -> ProvisioningStatus {
        match self {
            Self::Created(_) => ProvisioningStatus::Created,
            Self::Unchanged(_) => ProvisioningStatus::Unchanged,
            Self::Configured(_) => ProvisioningStatus::Configured,
            Self::Changed(_) => ProvisioningStatus::Changed,
        }
    }

    /// The stable identity of the resource the operation acted on.
    pub fn resource(&self) -> &R {
        match self {
            Self::Created(resource)
            | Self::Unchanged(resource)
            | Self::Configured(resource)
            | Self::Changed(resource) => resource,
        }
    }

    /// Consumes the outcome, yielding the resource identity.
    pub fn into_resource(self) -> R {
        match self {
            Self::Created(resource)
            | Self::Unchanged(resource)
            | Self::Configured(resource)
            | Self::Changed(resource) => resource,
        }
    }
}

impl<R: fmt::Display> fmt::Display for ProvisioningOutcome<R> {
    /// Identity-only rendering: `"{status}:{resource}"`. No values beyond the
    /// public identity are printable, by construction.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.status().as_str(), self.resource())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions_are_distinct_and_stable() {
        let outcome = ProvisioningOutcome::Created("owner/name".to_owned());
        assert_eq!(outcome.status(), ProvisioningStatus::Created);
        assert_eq!(outcome.status().as_str(), "created");
        assert_eq!(outcome.resource(), "owner/name");

        let rerun = ProvisioningOutcome::Unchanged("owner/name".to_owned());
        assert_eq!(rerun.status(), ProvisioningStatus::Unchanged);
        assert_eq!(rerun.status().as_str(), "unchanged");

        // The core idempotency rule: re-running over an existing compatible
        // resource is a distinguishable success, never an error.
        assert_ne!(outcome.status(), rerun.status());
        assert_eq!(outcome.into_resource(), rerun.into_resource());

        let configured = ProvisioningOutcome::Configured("owner/name".to_owned());
        let changed = ProvisioningOutcome::Changed("owner/name".to_owned());
        assert_ne!(configured.status(), changed.status());
        assert_eq!(configured.status().as_str(), "configured");
        assert_eq!(changed.status().as_str(), "changed");
    }

    #[test]
    fn display_renders_only_status_and_public_identity() {
        let outcome = ProvisioningOutcome::Unchanged("owner/name".to_owned());
        assert_eq!(outcome.to_string(), "unchanged:owner/name");
    }
}
