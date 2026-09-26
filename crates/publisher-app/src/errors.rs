//! Application errors for the `publisher-app` layer.
//!
//! Errors keep a stable, interface-level classification (which the CLI maps to
//! its existing exit codes — unchanged) plus the original cause. No credential
//! values, tokens, payloads, or user content may ever be placed in an
//! application error.

use std::fmt;

/// Stable interface-level category of an application failure.
///
/// The names intentionally mirror the current CLI classification so a thin
/// adapter can preserve today's exit codes without reinterpreting errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationErrorKind {
    ProjectInvalid,
    ResourceMissing,
    Validation,
    Publication,
    Recovery,
    Internal,
}

impl ApplicationErrorKind {
    /// Stable machine name for the category.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProjectInvalid => "project_invalid",
            Self::ResourceMissing => "resource_missing",
            Self::Validation => "validation",
            Self::Publication => "publication_failed",
            Self::Recovery => "recovery_failed",
            Self::Internal => "internal",
        }
    }
}

/// An application failure: category plus the preserved original cause.
#[derive(Debug)]
pub struct ApplicationError {
    pub kind: ApplicationErrorKind,
    pub message: String,
    source: Option<anyhow::Error>,
}

impl ApplicationError {
    pub fn new(kind: ApplicationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Wraps an underlying cause, preserving its full context chain (either
    /// directly via `anyhow` or by wrapping a typed error). The stable
    /// category remains the interface contract.
    pub fn with_source(
        kind: ApplicationErrorKind,
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(source.into()),
        }
    }

    pub fn source_error(&self) -> Option<&anyhow::Error> {
        self.source.as_ref()
    }
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApplicationError {}
