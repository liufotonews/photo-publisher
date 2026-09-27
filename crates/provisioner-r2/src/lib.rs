//! Concrete [`StorageProvisioner`] for Cloudflare R2 (Phase 7-H).
//!
//! Bucket infrastructure only: this provisioner checks whether the bucket
//! exists and creates it when it does not. It never uploads, lists, reads,
//! or deletes objects Ã¢â‚¬â€ buckets are infrastructure; objects are Publishing
//! (`provider-r2`), and this crate neither imports nor calls any of it.
//!
//! Endpoint semantics (deterministic; creation is single-shot):
//!
//! ```text
//! HEAD bucket
//!   200 Ã¢â€ â€™ Unchanged (existing and compatible by construction: the desired
//!         state declared today is only the bucket's existence)
//!   404 Ã¢â€ â€™ creation path
//! PUT bucket
//!   200 Ã¢â€ â€™ Created
//!   BucketAlreadyOwnedByYou Ã¢â€ â€™ Unchanged (the deterministic race outcome Ã¢â‚¬â€
//!   the bucket is ours and compatible, exactly the idempotent contract)
//!   BucketAlreadyExists (someone else's namespace) Ã¢â€ â€™ Conflict
//! ```

use aws_config::{retry::RetryConfig, BehaviorVersion};
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::create_bucket::CreateBucketError;
use aws_sdk_s3::Client;
use photo_publisher_provider_contracts::CredentialStore;
use publisher_provisioning::{
    ProvisioningError, ProvisioningErrorKind, ProvisioningOutcome, ProvisioningResult,
    StorageIdentity, StorageProvisionConfig, StorageProvisioner,
};
use tokio::runtime::Runtime;

const ACCESS_KEY_NAME: &str = "r2.access_key_id";
const SECRET_KEY_NAME: &str = "r2.secret_access_key";

/// Concrete storage provisioner backed by the S3-compatible R2 API.
///
/// Holds no credentials and no bucket state: the identity to act on and the
/// credential boundary are supplied per call, as the contract requires.
pub struct R2StorageProvisioner {
    runtime: Runtime,
    endpoint_override: Option<String>,
}

impl R2StorageProvisioner {
    /// Production endpoint (derived per call from the account id).
    pub fn new() -> ProvisioningResult<Self> {
        Self::build(None)
    }

    /// Endpoint override Ã¢â‚¬â€ used by the offline tests.
    pub fn with_endpoint_url(endpoint: impl Into<String>) -> ProvisioningResult<Self> {
        Self::build(Some(endpoint.into()))
    }

    fn build(endpoint_override: Option<String>) -> ProvisioningResult<Self> {
        let runtime = Runtime::new().map_err(|error| {
            ProvisioningError::with_source(
                ProvisioningErrorKind::Internal,
                "failed to start the async runtime",
                error,
            )
        })?;
        Ok(Self {
            runtime,
            endpoint_override,
        })
    }

    fn credential(
        credentials: &dyn CredentialStore,
        name: &str,
        what: &str,
    ) -> ProvisioningResult<String> {
        let value = credentials.get(name).map_err(|error| {
            ProvisioningError::with_source(
                ProvisioningErrorKind::Internal,
                "the credential store failed while reading an R2 credential",
                error,
            )
        })?;
        match value {
            Some(bytes) if !bytes.is_empty() => String::from_utf8(bytes).map_err(|_| {
                ProvisioningError::new(
                    ProvisioningErrorKind::AuthenticationFailed,
                    format!("the configured {what} is not valid UTF-8"),
                )
            }),
            _ => Err(ProvisioningError::new(
                ProvisioningErrorKind::AuthenticationRequired,
                format!("the {what} credential is not configured"),
            )),
        }
    }

    fn client(
        &self,
        config: &StorageProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<Client> {
        let access_key = Self::credential(credentials, ACCESS_KEY_NAME, "R2 access key")?;
        let secret_key = Self::credential(credentials, SECRET_KEY_NAME, "R2 secret key")?;
        let endpoint = self
            .endpoint_override
            .clone()
            .unwrap_or_else(|| format!("https://{}.r2.cloudflarestorage.com", config.account_id));
        let shared = self.runtime.block_on(async move {
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new("auto"))
                // Creation is single-shot (a PUT misread must never be
                // resent blindly); the head check is a read, so a bounded
                // retry is honest Ã¢â‚¬â€ the SDK already classifies failures.
                .retry_config(RetryConfig::standard().with_max_attempts(2))
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "photo-publisher-r2-provisioner",
                ))
                .endpoint_url(endpoint)
                .load()
                .await
        });
        let config = aws_sdk_s3::config::Builder::from(&shared)
            .force_path_style(true)
            .build();
        Ok(Client::from_conf(config))
    }

    fn kind_from_status(status: u16) -> ProvisioningErrorKind {
        match status {
            401 => ProvisioningErrorKind::AuthenticationFailed,
            403 => ProvisioningErrorKind::PermissionDenied,
            404 => ProvisioningErrorKind::NotFound,
            409 => ProvisioningErrorKind::Conflict,
            429 | 500..=599 => ProvisioningErrorKind::Network,
            _ => ProvisioningErrorKind::Internal,
        }
    }

    fn map_sdk_error<E: std::fmt::Debug>(error: &SdkError<E>) -> ProvisioningError {
        let kind = match error {
            SdkError::ServiceError(service) => {
                Self::kind_from_status(service.raw().status().as_u16())
            }
            SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) => {
                ProvisioningErrorKind::Network
            }
            SdkError::ResponseError(_) => ProvisioningErrorKind::Network,
            _ => ProvisioningErrorKind::Internal,
        };
        // The SDK error embeds response payloads; nothing of it may cross
        // this boundary Ã¢â‚¬â€ only the mapped kind and a fixed public message.
        ProvisioningError::new(kind, "the R2 bucket operation failed")
    }
}

impl StorageProvisioner for R2StorageProvisioner {
    fn provision_storage(
        &mut self,
        config: &StorageProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<StorageIdentity>> {
        let client = self.client(config, credentials)?;
        let bucket = config.bucket.clone();
        let head = self
            .runtime
            .block_on(client.head_bucket().bucket(&bucket).send());
        match head {
            Ok(_) => return Ok(ProvisioningOutcome::Unchanged(config.identity())),
            Err(SdkError::ServiceError(service)) if service.raw().status().as_u16() == 404 => {}
            // 404 Ã¢â€¡â€™ the bucket does not exist: creation path below.
            Err(error) => return Err(Self::map_sdk_error(&error)),
        }

        let create = self
            .runtime
            .block_on(client.create_bucket().bucket(&bucket).send());
        match create {
            Ok(_) => Ok(ProvisioningOutcome::Created(config.identity())),
            Err(SdkError::ServiceError(service)) => {
                let status = service.raw().status().as_u16();
                match service.err() {
                    CreateBucketError::BucketAlreadyOwnedByYou(_) => {
                        // A concurrent creation by ourselves is exactly the
                        // idempotent-compatible case: not an error.
                        Ok(ProvisioningOutcome::Unchanged(config.identity()))
                    }
                    CreateBucketError::BucketAlreadyExists(_) => Err(ProvisioningError::new(
                        ProvisioningErrorKind::Conflict,
                        "the bucket name is taken by another account",
                    )),
                    _ => Err(ProvisioningError::new(
                        Self::kind_from_status(status),
                        "the R2 bucket creation failed",
                    )),
                }
            }
            Err(error) => Err(Self::map_sdk_error(&error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use photo_publisher_provider_contracts::ProviderResult;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryCredentials(Mutex<HashMap<String, Vec<u8>>>);

    impl CredentialStore for MemoryCredentials {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(name).cloned())
        }
        fn set(&mut self, name: &str, secret: &[u8]) -> ProviderResult<()> {
            self.0
                .get_mut()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> ProviderResult<()> {
            self.0.get_mut().unwrap().remove(name);
            Ok(())
        }
    }

    const SENTINEL: &str = "TEST_SECRET_SHOULD_NEVER_ESCAPE";

    fn credentials_full() -> MemoryCredentials {
        let mut credentials = MemoryCredentials::default();
        credentials
            .set(ACCESS_KEY_NAME, SENTINEL.as_bytes())
            .unwrap();
        credentials
            .set(SECRET_KEY_NAME, SENTINEL.as_bytes())
            .unwrap();
        credentials
    }

    fn config() -> StorageProvisionConfig {
        StorageProvisionConfig::new("account-example", "fotografia").unwrap()
    }

    fn provisioner(server: &Server) -> R2StorageProvisioner {
        R2StorageProvisioner::with_endpoint_url(server.url()).unwrap()
    }

    #[test]
    fn existing_bucket_is_unchanged() {
        let mut server = Server::new();
        let head = server.mock("HEAD", Matcher::Any).with_status(200).create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "unchanged");
        assert_eq!(outcome.resource().to_string(), "account-example/fotografia");
        head.assert();
    }

    #[test]
    fn missing_bucket_is_created() {
        let mut server = Server::new();
        server.mock("HEAD", Matcher::Any).with_status(404).create();
        let create = server.mock("PUT", Matcher::Any).with_status(200).create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        create.assert();
    }

    #[test]
    fn a_bucket_we_already_own_is_unchanged_not_an_error() {
        let mut server = Server::new();
        server.mock("HEAD", Matcher::Any).with_status(404).create();
        server
            .mock("PUT", Matcher::Any)
            .with_status(409)
            .with_body(
                "<Error><Code>BucketAlreadyOwnedByYou</Code><Message>owned</Message></Error>",
            )
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "unchanged");
    }

    #[test]
    fn a_bucket_owned_by_someone_else_is_a_conflict() {
        let mut server = Server::new();
        server.mock("HEAD", Matcher::Any).with_status(404).create();
        server
            .mock("PUT", Matcher::Any)
            .with_status(409)
            .with_body("<Error><Code>BucketAlreadyExists</Code><Message>taken</Message></Error>")
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Conflict);
    }

    #[test]
    fn missing_credentials_fail_before_any_http_call() {
        let server = Server::new();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_storage(&config(), &MemoryCredentials::default())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::AuthenticationRequired);
    }

    #[test]
    fn a_forbidden_bucket_state_is_permission_denied() {
        let mut server = Server::new();
        server.mock("HEAD", Matcher::Any).with_status(403).create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::PermissionDenied);
    }

    #[test]
    fn unreachable_api_is_a_network_error() {
        let mut provisioner =
            R2StorageProvisioner::with_endpoint_url("http://127.0.0.1:1").unwrap();
        let error = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Network);
    }

    #[test]
    fn secrets_never_appear_in_outcomes_or_errors() {
        let mut server = Server::new();
        server.mock("HEAD", Matcher::Any).with_status(403).create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(SENTINEL));

        server.mock("HEAD", Matcher::Any).with_status(200).create();
        let outcome = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap();
        let rendered = format!("{outcome:?} {outcome}");
        assert!(!rendered.contains(SENTINEL));
    }

    #[test]
    fn sdk_timeouts_are_network_errors_not_conflicts() {
        // A 5xx from the endpoint maps to Network (transient), never to
        // Conflict (a misleading "divergence") Ã¢â‚¬â€ ambiguity stays honest.
        let mut server = Server::new();
        server
            .mock("HEAD", Matcher::Any)
            .with_status(500)
            .expect_at_least(1)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_storage(&config(), &credentials_full())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Network);
    }

    #[test]
    fn the_provisioner_never_touches_publishing_surfaces() {
        // Structural proof: bucket infrastructure only Ã¢â‚¬â€ no object verbs,
        // no publishing traits, no application layers.
        let source =
            std::fs::read_to_string(format!("{}/src/lib.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let source = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "RepositoryProvider",
            "StorageProvider",
            "HostingProvider",
            "HostingPublisher",
            "publisher_app",
            "photo_publisher_core",
            "photo_publisher_integration",
            "photo_publisher_pipeline",
            "put_object",
            "delete_object",
            "list_objects",
            "get_object",
            ".put(",
            ".delete(",
        ] {
            assert!(
                !source.contains(forbidden),
                "r2 provisioner must not reference {forbidden}"
            );
        }
    }
}
