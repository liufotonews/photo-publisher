//! Credential UX service (Phase 7-E).
//!
//! This module is the provider-neutral application boundary for credential
//! management. It speaks only to the existing `CredentialStore` contract —
//! never to a concrete backend, an environment variable, a file, or the
//! network — and only for the four credential names this application
//! version supports.
//!
//! The prime rule: a secret may pass *through* an operation, but it never
//! becomes application state. No credential value is ever placed in a
//! result, an error message, a status entry, an event, or a log. The status
//! surface carries exactly one bit per credential: configured or not.

use photo_publisher_provider_contracts::CredentialStore;

use crate::errors::{ApplicationError, ApplicationErrorKind};

/// The credential names this application version supports, in a stable
/// presentation order. The allowlist is closed: an unknown name is a
/// deterministic validation error, never a store write.
pub const SUPPORTED_CREDENTIALS: [&str; 4] = [
    "github.token",
    "r2.access_key_id",
    "r2.secret_access_key",
    "vercel.token",
];

/// A credential's public state: identity, a safe display label, and the
/// configured bit. The name is configuration data (like the environment
/// variable names), never a secret; the value never appears here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    pub name: &'static str,
    pub label: &'static str,
    pub configured: bool,
}

/// Safe public label per credential name. Returns `None` outside the
/// allowlist.
fn label_of(name: &str) -> Option<&'static str> {
    Some(match name {
        "github.token" => "GitHub token",
        "r2.access_key_id" => "R2 access key",
        "r2.secret_access_key" => "R2 secret key",
        "vercel.token" => "Vercel token",
        _ => return None,
    })
}

fn unknown_credential(name: &str) -> ApplicationError {
    // Names are configuration, not secrets: echoing the rejected name is
    // safe and keeps the diagnosis actionable.
    ApplicationError::new(
        ApplicationErrorKind::Validation,
        format!("unknown credential name: {name}"),
    )
}

fn store_failure(
    operation: &str,
    error: photo_publisher_provider_contracts::ProviderError,
) -> ApplicationError {
    // The public message is generic; the provider contract guarantees its
    // errors never carry secret material, so the cause chain is preserved.
    ApplicationError::with_source(
        ApplicationErrorKind::Internal,
        format!("the credential store could not {operation}"),
        error,
    )
}

/// Reads the configured bit of every supported credential, in the stable
/// allowlist order. A store failure aborts the whole read: a partial or
/// guessed status would be worse than none.
pub fn credential_status(
    credentials: &dyn CredentialStore,
) -> Result<Vec<CredentialStatus>, ApplicationError> {
    SUPPORTED_CREDENTIALS
        .iter()
        .map(|name| {
            let value = credentials
                .get(name)
                .map_err(|error| store_failure("read the credential state", error))?;
            Ok(CredentialStatus {
                name,
                label: label_of(name).expect("allowlist names always have labels"),
                configured: matches!(value, Some(secret) if !secret.is_empty()),
            })
        })
        .collect()
}

/// Stores one credential through the backend. The secret is used for this
/// operation only: it is not copied into any result, error, event, or log.
/// Blank secrets and unknown names are rejected before the store is touched.
pub fn set_credential(
    credentials: &mut dyn CredentialStore,
    name: &str,
    secret: &[u8],
) -> Result<(), ApplicationError> {
    if label_of(name).is_none() {
        return Err(unknown_credential(name));
    }
    if secret.is_empty() || secret.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(ApplicationError::new(
            ApplicationErrorKind::Validation,
            "the credential value must not be empty",
        ));
    }
    credentials
        .set(name, secret)
        .map_err(|error| store_failure("store the credential", error))
}

/// Removes one credential through the backend. Deletion is idempotent at
/// this layer: removing an absent credential succeeds (the observed end
/// state — not configured — is the one requested).
pub fn delete_credential(
    credentials: &mut dyn CredentialStore,
    name: &str,
) -> Result<(), ApplicationError> {
    if label_of(name).is_none() {
        return Err(unknown_credential(name));
    }
    credentials
        .delete(name)
        .map_err(|error| store_failure("remove the credential", error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contracts::{ProviderError, ProviderResult};
    use std::collections::HashMap;

    /// In-memory backend: no environment, no filesystem, no network — the
    /// only way these tests can interact with credentials at all.
    #[derive(Default)]
    struct FakeStore {
        values: HashMap<String, Vec<u8>>,
        failing: bool,
    }

    impl CredentialStore for FakeStore {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            if self.failing {
                return Err(ProviderError::Other);
            }
            Ok(self.values.get(name).cloned())
        }
        fn set(&mut self, name: &str, secret: &[u8]) -> ProviderResult<()> {
            if self.failing {
                return Err(ProviderError::Other);
            }
            self.values.insert(name.to_owned(), secret.to_vec());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> ProviderResult<()> {
            if self.failing {
                return Err(ProviderError::Other);
            }
            self.values.remove(name);
            Ok(())
        }
    }

    #[test]
    fn empty_store_reports_every_credential_as_not_configured() {
        let store = FakeStore::default();
        let status = credential_status(&store).unwrap();
        assert_eq!(status.len(), 4);
        assert_eq!(
            status.iter().map(|entry| entry.name).collect::<Vec<_>>(),
            SUPPORTED_CREDENTIALS
        );
        assert!(status.iter().all(|entry| !entry.configured));
        assert!(status.iter().all(|entry| !entry.label.is_empty()));
    }

    #[test]
    fn configured_store_reports_true_without_any_secret_material() {
        let mut store = FakeStore::default();
        for name in SUPPORTED_CREDENTIALS {
            store.set(name, b"some-value").unwrap();
        }
        let status = credential_status(&store).unwrap();
        assert!(status.iter().all(|entry| entry.configured));
        let rendered = format!("{status:?}");
        assert!(!rendered.contains("some-value"));
    }

    #[test]
    fn set_and_delete_roundtrip_through_the_backend() {
        let mut store = FakeStore::default();
        set_credential(&mut store, "github.token", b"abc123").unwrap();
        assert!(credential_status(&store).unwrap()[0].configured);
        delete_credential(&mut store, "github.token").unwrap();
        assert!(!credential_status(&store).unwrap()[0].configured);
        // Delete is idempotent at this layer.
        delete_credential(&mut store, "github.token").unwrap();
    }

    #[test]
    fn every_supported_name_is_accepted() {
        let mut store = FakeStore::default();
        for name in SUPPORTED_CREDENTIALS {
            set_credential(&mut store, name, b"x").unwrap();
        }
        assert!(credential_status(&store)
            .unwrap()
            .iter()
            .all(|entry| entry.configured));
        for name in SUPPORTED_CREDENTIALS {
            delete_credential(&mut store, name).unwrap();
        }
        assert!(credential_status(&store)
            .unwrap()
            .iter()
            .all(|entry| !entry.configured));
    }

    #[test]
    fn unknown_credential_names_are_rejected_before_the_store_is_touched() {
        let mut store = FakeStore::default();
        for name in ["foo.secret", "GITHUB.TOKEN", "github.token "] {
            let error = set_credential(&mut store, name, b"x").unwrap_err();
            assert_eq!(error.kind, ApplicationErrorKind::Validation);
            let error = delete_credential(&mut store, name).unwrap_err();
            assert_eq!(error.kind, ApplicationErrorKind::Validation);
        }
        assert!(store.values.is_empty(), "the store must stay untouched");
    }

    #[test]
    fn blank_secrets_are_rejected_before_the_store_is_touched() {
        let mut store = FakeStore::default();
        for blank in [&b""[..], &b"   "[..], &b"\n\t"[..]] {
            let error = set_credential(&mut store, "github.token", blank).unwrap_err();
            assert_eq!(error.kind, ApplicationErrorKind::Validation);
        }
        assert!(store.values.is_empty());
    }

    #[test]
    fn a_sentinel_secret_never_escapes_results_or_errors() {
        let sentinel = b"TEST_SECRET_SHOULD_NEVER_ESCAPE";
        let mut store = FakeStore::default();
        set_credential(&mut store, "vercel.token", sentinel).unwrap();
        let status = credential_status(&store).unwrap();
        let rendered = format!("{status:?}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));

        // Failing backend: the error must not echo the secret either.
        let mut failing = FakeStore {
            failing: true,
            ..FakeStore::default()
        };
        let error = set_credential(&mut failing, "vercel.token", sentinel).unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));
        let mut failing = FakeStore {
            failing: true,
            ..FakeStore::default()
        };
        let error = delete_credential(&mut failing, "vercel.token").unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));
        let failing = FakeStore {
            failing: true,
            ..FakeStore::default()
        };
        let error = credential_status(&failing).unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));
    }

    #[test]
    fn backend_failures_are_classified_safely() {
        let mut failing = FakeStore {
            failing: true,
            ..FakeStore::default()
        };
        let error = set_credential(&mut failing, "github.token", b"x").unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::Internal);
        assert_eq!(
            error.to_string(),
            "the credential store could not store the credential"
        );
        let error = credential_status(&failing).unwrap_err();
        assert_eq!(error.kind, ApplicationErrorKind::Internal);
        // The operation vocabulary never names a provider or an environment
        // variable.
        let rendered = format!("{error}");
        for needle in ["PHOTO_PUBLISHER_", "github", "r2.", "vercel"] {
            assert!(!rendered.contains(needle), "error leaked {needle}");
        }
    }

    #[test]
    fn the_service_never_reaches_a_concrete_backend_environment_or_network() {
        // Structural proof: the application layer speaks only the
        // `CredentialStore` contract. Only the non-test part is scanned (the
        // token list itself lives in this test).
        let source = include_str!("credentials.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            "EnvironmentCredentialStore",
            "std::env",
            "std::fs",
            "reqwest",
            "TcpStream",
            "HttpClient",
            "photo_publisher_provider_github",
            "photo_publisher_provider_r2",
            "photo_publisher_provider_vercel",
            "println!",
            "eprintln!",
            "dbg!",
        ] {
            assert!(
                !source.contains(forbidden),
                "credentials service must not reference {forbidden}"
            );
        }
    }
}
