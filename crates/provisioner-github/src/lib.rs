//! Concrete [`RepositoryProvisioner`] for GitHub (Phase 7-H).
//!
//! Infrastructure only: this provisioner checks whether a repository exists
//! and creates it when it does not. It never writes files, blobs, trees,
//! commits, or refs — all of that is Publishing (`provider-github`), and
//! this crate neither imports nor calls any of it.
//!
//! Endpoint semantics (deterministic, no blind retries):
//!
//! ```text
//! GET /repos/{owner}/{name}
//!   200 → Unchanged (existing and compatible by construction: the desired
//!         state declared today is only the repository's existence)
//!   404 → creation path
//! create:
//!   GET /user                     (who is the token's owner?)
//!     login == owner   → POST /user/repos
//!     login != owner   → GET /orgs/{owner}
//!                          200 → POST /orgs/{owner}/repos
//!                          404 → Conflict (owner is neither the
//!                                authenticated user nor an accessible org)
//!   POST …/repos → 2xx → Created
//!   POST …/repos → 409/422 → Conflict (never re-sent blindly)
//! ```

use std::time::Duration;

use photo_publisher_provider_contracts::CredentialStore;
use publisher_provisioning::{
    ProvisioningError, ProvisioningErrorKind, ProvisioningOutcome, ProvisioningResult,
    RepositoryIdentity, RepositoryProvisionConfig, RepositoryProvisioner,
};
use reqwest::blocking::Client;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

const TOKEN_NAME: &str = "github.token";
const API_VERSION: &str = "2026-03-10";
const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

/// Concrete repository provisioner backed by the GitHub REST API.
///
/// The value holds no credentials and no repository state: the identity to
/// act on and the credential boundary are supplied per call, as the
/// contract requires.
pub struct GitHubRepositoryProvisioner {
    client: Client,
    api_base_url: String,
}

impl GitHubRepositoryProvisioner {
    /// Production endpoint.
    pub fn new() -> ProvisioningResult<Self> {
        Self::with_api_base_url(DEFAULT_API_BASE_URL)
    }

    /// Endpoint override — used by the offline tests (and by future
    /// GitHub-Enterprise surfaces). The URL must be absolute HTTP(S).
    pub fn with_api_base_url(base: impl Into<String>) -> ProvisioningResult<Self> {
        let base = base.into();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::InvalidConfiguration,
                "the GitHub API base URL must be absolute http(s)",
            ));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("photo-publisher-provisioner")
            .build()
            .map_err(|error| {
                ProvisioningError::with_source(
                    ProvisioningErrorKind::Internal,
                    "failed to build the HTTP client",
                    error,
                )
            })?;
        Ok(Self {
            client,
            api_base_url: base.trim_end_matches('/').to_owned(),
        })
    }

    fn token(credentials: &dyn CredentialStore) -> ProvisioningResult<String> {
        let value = credentials.get(TOKEN_NAME).map_err(|error| {
            ProvisioningError::with_source(
                ProvisioningErrorKind::Internal,
                "the credential store failed while reading the GitHub credential",
                error,
            )
        })?;
        match value {
            Some(bytes) if !bytes.is_empty() => String::from_utf8(bytes).map_err(|_| {
                ProvisioningError::new(
                    ProvisioningErrorKind::AuthenticationFailed,
                    "the configured GitHub credential is not valid UTF-8",
                )
            }),
            _ => Err(ProvisioningError::new(
                ProvisioningErrorKind::AuthenticationRequired,
                "the GitHub credential is not configured",
            )),
        }
    }

    fn map_status(status: StatusCode) -> ProvisioningErrorKind {
        match status {
            StatusCode::UNAUTHORIZED => ProvisioningErrorKind::AuthenticationFailed,
            StatusCode::FORBIDDEN => ProvisioningErrorKind::PermissionDenied,
            StatusCode::NOT_FOUND => ProvisioningErrorKind::NotFound,
            StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => {
                ProvisioningErrorKind::Conflict
            }
            StatusCode::TOO_MANY_REQUESTS => ProvisioningErrorKind::Network,
            status if status.is_server_error() => ProvisioningErrorKind::Network,
            _ => ProvisioningErrorKind::Internal,
        }
    }

    /// Performs one HTTP call (never implicitly retried; creation calls are
    /// single-shot by design). Returns the status and, when present, the
    /// decoded JSON body.
    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<(StatusCode, Option<Value>)> {
        let token = Self::token(credentials)?;
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.api_base_url))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|error| {
            ProvisioningError::with_source(
                ProvisioningErrorKind::Network,
                "the GitHub request failed before a response was received",
                error,
            )
        })?;
        let status = response.status();
        let body = response.json::<Value>().ok();
        Ok((status, body))
    }
}

impl RepositoryProvisioner for GitHubRepositoryProvisioner {
    fn provision_repository(
        &mut self,
        config: &RepositoryProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<RepositoryIdentity>> {
        // 1. Existence check: existing and present ⇒ Unchanged.
        let path = format!("/repos/{}/{}", config.owner, config.name);
        let (status, _) = self.request(Method::GET, &path, None, credentials)?;
        if status.is_success() {
            return Ok(ProvisioningOutcome::Unchanged(config.identity()));
        }
        if status != StatusCode::NOT_FOUND {
            let kind = Self::map_status(status);
            return Err(ProvisioningError::new(
                kind,
                format!("could not check the repository state (HTTP {status})"),
            ));
        }

        // 2. Creation: decide the endpoint from the token owner vs. the
        //    declared owner — the only unambiguous, documented possibilities.
        let (status, user) = self.request(Method::GET, "/user", None, credentials)?;
        if !status.is_success() {
            let kind = Self::map_status(status);
            return Err(ProvisioningError::new(
                kind,
                format!("could not identify the authenticated GitHub account (HTTP {status})"),
            ));
        }
        let login = user
            .as_ref()
            .and_then(|value| value.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let create_path = if login == config.owner {
            "/user/repos".to_owned()
        } else {
            let (status, _) = self.request(
                Method::GET,
                &format!("/orgs/{}", config.owner),
                None,
                credentials,
            )?;
            if status.is_success() {
                format!("/orgs/{}/repos", config.owner)
            } else {
                return Err(ProvisioningError::new(
                    ProvisioningErrorKind::Conflict,
                    "the repository does not exist and its owner is neither the authenticated account nor an accessible organization",
                ));
            }
        };
        let (status, _) = self.request(
            Method::POST,
            &create_path,
            Some(json!({ "name": config.name })),
            credentials,
        )?;
        if status.is_success() {
            return Ok(ProvisioningOutcome::Created(config.identity()));
        }
        let kind = Self::map_status(status);
        Err(ProvisioningError::new(
            // A 409/422 at creation time after a 404 above is a divergence,
            // never something to repeat blindly.
            kind,
            format!("the repository could not be created (HTTP {status})"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
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

    fn credentials_with_token() -> MemoryCredentials {
        let mut credentials = MemoryCredentials::default();
        credentials.set(TOKEN_NAME, SENTINEL.as_bytes()).unwrap();
        credentials
    }

    fn config() -> RepositoryProvisionConfig {
        RepositoryProvisionConfig::new("fotografo", "joao-maria-2026").unwrap()
    }

    fn provisioner(server: &Server) -> GitHubRepositoryProvisioner {
        GitHubRepositoryProvisioner::with_api_base_url(server.url()).unwrap()
    }

    #[test]
    fn existing_repository_is_unchanged_never_created_again() {
        let mut server = Server::new();
        let get = server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(200)
            .with_body("{}")
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "unchanged");
        assert_eq!(outcome.resource().to_string(), "fotografo/joao-maria-2026");
        get.assert();
    }

    #[test]
    fn missing_repository_is_created_under_the_token_owner() {
        let mut server = Server::new();
        let get = server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(404)
            .with_body("{}")
            .create();
        let whoami = server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"fotografo"}"#)
            .create();
        let create = server
            .mock("POST", "/user/repos")
            .with_status(201)
            .with_body("{}")
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        assert_eq!(outcome.resource().to_string(), "fotografo/joao-maria-2026");
        get.assert();
        whoami.assert();
        create.assert();
    }

    #[test]
    fn creation_under_an_organization_uses_the_org_endpoint() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(404)
            .with_body("{}")
            .create();
        server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"someone-else"}"#)
            .create();
        let org = server
            .mock("GET", "/orgs/fotografo")
            .with_status(200)
            .with_body("{}")
            .create();
        let create = server
            .mock("POST", "/orgs/fotografo/repos")
            .with_status(201)
            .with_body("{}")
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        org.assert();
        create.assert();
    }

    #[test]
    fn unreachable_owner_is_a_conflict_not_a_creation_attempt() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(404)
            .with_body("{}")
            .create();
        server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"someone-else"}"#)
            .create();
        server
            .mock("GET", "/orgs/fotografo")
            .with_status(404)
            .with_body("{}")
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Conflict);
    }

    #[test]
    fn a_race_at_creation_is_a_conflict_and_is_never_resent() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(404)
            .with_body("{}")
            .create();
        server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"fotografo"}"#)
            .create();
        let create = server
            .mock("POST", "/user/repos")
            .with_status(422)
            .with_body(r#"{"message":"repository already exists"}"#)
            .expect(1)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Conflict);
        create.assert();
    }

    #[test]
    fn missing_credential_fails_before_any_http_call() {
        let server = Server::new();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &MemoryCredentials::default())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::AuthenticationRequired);
        // Nothing was sent: no mock exists, so any call would have panicked.
    }

    #[test]
    fn rejected_credential_is_authentication_failed() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(401)
            .with_body(r#"{"message":"bad credentials"}"#)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::AuthenticationFailed);
    }

    #[test]
    fn unsupported_owner_lookup_surfaces_permission_denied() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(403)
            .with_body("{}")
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::PermissionDenied);
    }

    #[test]
    fn unreachable_api_is_a_network_error() {
        let provisioner_url = "http://127.0.0.1:1";
        let mut provisioner =
            GitHubRepositoryProvisioner::with_api_base_url(provisioner_url).unwrap();
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Network);
    }

    #[test]
    fn secrets_never_appear_in_outcomes_or_errors() {
        let mut server = Server::new();
        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(401)
            // Provocative body: even if the API leaked the token back, the
            // provisioner must not forward response payloads into errors.
            .with_body(format!(r#"{{"message":"{SENTINEL}"}}"#))
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(SENTINEL));

        server
            .mock("GET", "/repos/fotografo/joao-maria-2026")
            .with_status(200)
            .with_body("{}")
            .create();
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        let rendered = format!("{outcome:?} {outcome}");
        assert!(!rendered.contains(SENTINEL));
    }

    #[test]
    fn the_provisioner_never_touches_publishing_surfaces() {
        // Structural proof: this crate implements only the provisioning
        // contract; publishing traits and content verbs never appear.
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
            ".commit(",
            ".write(",
            ".delete(",
            "ensure_repository",
            "git/trees",
            "git/blobs",
            "git/refs",
            "/contents/",
        ] {
            assert!(
                !source.contains(forbidden),
                "github provisioner must not reference {forbidden}"
            );
        }
    }
}
