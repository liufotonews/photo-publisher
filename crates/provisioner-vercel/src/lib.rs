//! Concrete [`HostingProvisioner`] for Vercel (Phase 7-H).
//!
//! Project infrastructure only: this provisioner checks whether the hosting
//! project exists and creates it when it does not. It never deploys sites,
//! uploads content, or attaches domains — all of that is Publishing
//! (`provider-vercel`), and this crate neither imports nor calls any of it.
//!
//! Endpoint semantics (deterministic, no blind retries):
//!
//! ```text
//! GET /v9/projects/{project} (+?teamId=… when configured)
//!   200 → Unchanged (existing and compatible by construction: the desired
//!         state declared today is only the project's existence)
//!   404 → creation path
//! POST /v10/projects {"name": project} (+?teamId=…; single-shot)
//!   200/201 → Created
//!   400/409 → Conflict
//! ```

use std::time::Duration;

use photo_publisher_provider_contracts::CredentialStore;
use publisher_provisioning::{
    HostingIdentity, HostingProvisionConfig, HostingProvisioner, ProvisioningError,
    ProvisioningErrorKind, ProvisioningOutcome, ProvisioningResult,
};
use reqwest::blocking::Client;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

const TOKEN_NAME: &str = "vercel.token";
const DEFAULT_API_BASE_URL: &str = "https://api.vercel.com";

/// Concrete hosting provisioner backed by the Vercel REST API.
///
/// Holds no credentials and no project state: the identity to act on and
/// the credential boundary are supplied per call, as the contract requires.
pub struct VercelHostingProvisioner {
    client: Client,
    api_base_url: String,
}

impl VercelHostingProvisioner {
    /// Production endpoint.
    pub fn new() -> ProvisioningResult<Self> {
        Self::with_api_base_url(DEFAULT_API_BASE_URL)
    }

    /// Endpoint override — used by the offline tests.
    pub fn with_api_base_url(base: impl Into<String>) -> ProvisioningResult<Self> {
        let base = base.into();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::InvalidConfiguration,
                "the Vercel API base URL must be absolute http(s)",
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
                "the credential store failed while reading the Vercel credential",
                error,
            )
        })?;
        match value {
            Some(bytes) if !bytes.is_empty() => String::from_utf8(bytes).map_err(|_| {
                ProvisioningError::new(
                    ProvisioningErrorKind::AuthenticationFailed,
                    "the configured Vercel credential is not valid UTF-8",
                )
            }),
            _ => Err(ProvisioningError::new(
                ProvisioningErrorKind::AuthenticationRequired,
                "the Vercel credential is not configured",
            )),
        }
    }

    fn map_status(status: StatusCode) -> ProvisioningErrorKind {
        match status {
            StatusCode::UNAUTHORIZED => ProvisioningErrorKind::AuthenticationFailed,
            StatusCode::FORBIDDEN => ProvisioningErrorKind::PermissionDenied,
            StatusCode::NOT_FOUND => ProvisioningErrorKind::NotFound,
            StatusCode::CONFLICT | StatusCode::BAD_REQUEST => ProvisioningErrorKind::Conflict,
            StatusCode::TOO_MANY_REQUESTS => ProvisioningErrorKind::Network,
            status if status.is_server_error() => ProvisioningErrorKind::Network,
            _ => ProvisioningErrorKind::Internal,
        }
    }

    /// One HTTP call (never implicitly retried; creation is single-shot).
    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        team_id: Option<&str>,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<StatusCode> {
        let token = Self::token(credentials)?;
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.api_base_url))
            .bearer_auth(token);
        if let Some(team_id) = team_id {
            request = request.query(&[("teamId", team_id)]);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|error| {
            ProvisioningError::with_source(
                ProvisioningErrorKind::Network,
                "the Vercel request failed before a response was received",
                error,
            )
        })?;
        Ok(response.status())
    }
}

impl HostingProvisioner for VercelHostingProvisioner {
    fn provision_hosting(
        &mut self,
        config: &HostingProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<HostingIdentity>> {
        let team_id = config.team_id.as_deref();
        let status = self.request(
            Method::GET,
            &format!("/v9/projects/{}", config.project),
            None,
            team_id,
            credentials,
        )?;
        if status.is_success() {
            return Ok(ProvisioningOutcome::Unchanged(config.identity()));
        }
        if status != StatusCode::NOT_FOUND {
            let kind = Self::map_status(status);
            return Err(ProvisioningError::new(
                kind,
                format!("could not check the hosting project state (HTTP {status})"),
            ));
        }

        // Creation is a single deliberate call; an ambiguous answer becomes
        // a Conflict/Network error, never a blind repeat.
        let status = self.request(
            Method::POST,
            "/v10/projects",
            Some(json!({ "name": config.project })),
            team_id,
            credentials,
        )?;
        if status.is_success() {
            return Ok(ProvisioningOutcome::Created(config.identity()));
        }
        let kind = Self::map_status(status);
        Err(ProvisioningError::new(
            kind,
            format!("the hosting project could not be created (HTTP {status})"),
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

    fn config() -> HostingProvisionConfig {
        HostingProvisionConfig::new("joao-maria-2026", None).unwrap()
    }

    fn config_with_team() -> HostingProvisionConfig {
        HostingProvisionConfig::new("joao-maria-2026", Some("team_example".to_owned())).unwrap()
    }

    fn provisioner(server: &Server) -> VercelHostingProvisioner {
        VercelHostingProvisioner::with_api_base_url(server.url()).unwrap()
    }

    #[test]
    fn existing_project_is_unchanged_never_created_again() {
        let mut server = Server::new();
        let get = server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(200)
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "unchanged");
        assert_eq!(outcome.resource().to_string(), "joao-maria-2026");
        get.assert();
    }

    #[test]
    fn missing_project_is_created() {
        let mut server = Server::new();
        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(404)
            .create();
        let create = server
            .mock("POST", "/v10/projects")
            .match_header("content-type", "application/json")
            .with_status(200)
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        create.assert();
    }

    #[test]
    fn team_id_is_a_query_parameter_never_payload() {
        let mut server = Server::new();
        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .match_query("teamId=team_example")
            .with_status(404)
            .create();
        let create = server
            .mock("POST", "/v10/projects")
            .match_query("teamId=team_example")
            .with_status(200)
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_hosting(&config_with_team(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        create.assert();
    }

    #[test]
    fn a_name_taken_is_a_conflict_never_resent() {
        let mut server = Server::new();
        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(404)
            .create();
        let create = server
            .mock("POST", "/v10/projects")
            .with_status(409)
            .expect(1)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Conflict);
        create.assert();
    }

    #[test]
    fn missing_credential_fails_before_any_http_call() {
        let server = Server::new();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_hosting(&config(), &MemoryCredentials::default())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::AuthenticationRequired);
    }

    #[test]
    fn rejected_credential_is_authentication_failed() {
        let mut server = Server::new();
        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(401)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::AuthenticationFailed);
    }

    #[test]
    fn forbidden_project_access_is_permission_denied() {
        let mut server = Server::new();
        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(403)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::PermissionDenied);
    }

    #[test]
    fn unreachable_api_is_a_network_error() {
        let mut provisioner =
            VercelHostingProvisioner::with_api_base_url("http://127.0.0.1:1").unwrap();
        let error = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Network);
    }

    #[test]
    fn secrets_never_appear_in_outcomes_or_errors() {
        let mut server = Server::new();
        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(401)
            .with_body(format!(r#"{{"error":{{"message":"{SENTINEL}"}}}}"#))
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap_err();
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(SENTINEL));

        server
            .mock("GET", "/v9/projects/joao-maria-2026")
            .with_status(200)
            .create();
        let outcome = provisioner
            .provision_hosting(&config(), &credentials_with_token())
            .unwrap();
        let rendered = format!("{outcome:?} {outcome}");
        assert!(!rendered.contains(SENTINEL));
    }

    #[test]
    fn the_provisioner_never_touches_publishing_surfaces() {
        // Structural proof: project infrastructure only — no deployments,
        // no files, no publishing traits, no application layers.
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
            "deployments",
            "files",
        ] {
            assert!(
                !source.contains(forbidden),
                "vercel provisioner must not reference {forbidden}"
            );
        }
    }
}
