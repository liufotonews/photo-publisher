//! Errors of the provisioning boundary.
//!
//! This classification family is deliberately separate from the publishing
//! side (`ProviderError` in `provider-contracts`): provisioning answers
//! "can this infrastructure be created/kept as desired?", publishing answers
//! "can content be put on existing infrastructure?". Sharing a type would
//! blur that frontier, so this crate defines its own error.

use std::error::Error;
use std::fmt;

/// Stable machine classification of a provisioning failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningErrorKind {
    /// The resource the operation addresses does not exist.
    NotFound,
    /// The provisioner does not support this operation at all.
    Unsupported,
    /// No usable credential is available: authentication is required first.
    AuthenticationRequired,
    /// The credential was presented and rejected.
    AuthenticationFailed,
    /// The credential is valid but lacks the required permission.
    PermissionDenied,
    /// The existing resource diverges from the desired configuration in a
    /// way the provisioner refuses to change without a human decision.
    Conflict,
    /// A network failure occurred. Reservable headroom for real
    /// implementations; contracts themselves never perform I/O.
    Network,
    /// Any internal failure of the provisioner not covered above.
    Internal,
    /// The provided configuration is structurally invalid (caught before any
    /// backend could ever run).
    InvalidConfiguration,
}

impl ProvisioningErrorKind {
    /// Stable machine name for the classification.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Unsupported => "unsupported",
            Self::AuthenticationRequired => "authentication_required",
            Self::AuthenticationFailed => "authentication_failed",
            Self::PermissionDenied => "permission_denied",
            Self::Conflict => "conflict",
            Self::Network => "network",
            Self::Internal => "internal",
            Self::InvalidConfiguration => "invalid_configuration",
        }
    }
}

/// A provisioning failure: a stable kind plus a public-safe message.
///
/// The message must never carry secret material, credential values,
/// environment variables, or provider response bodies; the optional source
/// preserves the original cause for diagnostics under the same rule.
#[derive(Debug)]
pub struct ProvisioningError {
    pub kind: ProvisioningErrorKind,
    pub message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProvisioningError {
    pub fn new(kind: ProvisioningErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Wraps the underlying cause, preserving the classification.
    pub fn with_source<E>(
        kind: ProvisioningErrorKind,
        message: impl Into<String>,
        source: E,
    ) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub fn source_error(&self) -> Option<&(dyn Error + Send + Sync + 'static)> {
        self.source.as_deref()
    }
}

impl fmt::Display for ProvisioningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProvisioningError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn Error + 'static))
    }
}

/// The result of every provisioning operation.
pub type ProvisioningResult<T> = Result<T, ProvisioningError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kinds_have_stable_distinct_names() {
        let names: Vec<&'static str> = [
            ProvisioningErrorKind::NotFound,
            ProvisioningErrorKind::Unsupported,
            ProvisioningErrorKind::AuthenticationRequired,
            ProvisioningErrorKind::AuthenticationFailed,
            ProvisioningErrorKind::PermissionDenied,
            ProvisioningErrorKind::Conflict,
            ProvisioningErrorKind::Network,
            ProvisioningErrorKind::Internal,
            ProvisioningErrorKind::InvalidConfiguration,
        ]
        .iter()
        .map(|kind| kind.as_str())
        .collect();
        for window in names.windows(2) {
            assert_ne!(window[0], window[1]);
        }
        assert_eq!(names.len(), 9);
        assert!(names.contains(&"conflict"));
        assert!(names.contains(&"network"));
        assert!(names.contains(&"authentication_required"));
    }

    #[test]
    fn messages_display_without_debug_guts_and_keep_the_cause() {
        #[derive(Debug)]
        struct Cause;
        impl fmt::Display for Cause {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("root cause")
            }
        }
        impl Error for Cause {}

        let error = ProvisioningError::with_source(
            ProvisioningErrorKind::Internal,
            "the provisioner failed",
            Cause,
        );
        assert_eq!(error.to_string(), "the provisioner failed");
        assert!(error.source_error().is_some());
        assert!(std::error::Error::source(&error).is_some());
    }
}
