//! Synchronous Vercel hosting provider backed by the official REST API.
//!
//! Deployment files are materialized in memory and sent inline as base64 JSON.
//! This intentionally avoids GitHub, the publisher pipeline, provisioning, and
//! multipart uploads in this phase.

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use photo_publisher_provider_contracts::{
    CredentialStore, DeploymentInfo, HostingProvider, ProviderError, ProviderResult,
};
use reqwest::blocking::{Client, Response};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const TOKEN_KEY: &str = "vercel.token";
const DEFAULT_API_BASE_URL: &str = "https://api.vercel.com";
const MAX_GET_RETRIES: usize = 2;
const MAX_POLL_ATTEMPTS: usize = 10;
const RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct VercelConfig {
    pub project_id: String,
    pub team_id: Option<String>,
    pub api_base_url: String,
    files: Vec<DeploymentFile>,
}

impl VercelConfig {
    pub fn new(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            team_id: None,
            api_base_url: DEFAULT_API_BASE_URL.to_owned(),
            files: Vec::new(),
        }
    }

    pub fn with_files(mut self, files: Vec<(String, Vec<u8>)>) -> ProviderResult<Self> {
        self.files = files
            .into_iter()
            .map(|(path, content)| DeploymentFile::new(path, content))
            .collect::<ProviderResult<Vec<_>>>()?;
        Ok(self)
    }

    fn validate(&self) -> ProviderResult<()> {
        if self.project_id.trim().is_empty() || self.api_base_url.trim().is_empty() {
            return Err(ProviderError::Other);
        }
        if !self.api_base_url.starts_with("http://") && !self.api_base_url.starts_with("https://") {
            return Err(ProviderError::Other);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct DeploymentFile {
    path: String,
    content: Vec<u8>,
}

impl DeploymentFile {
    fn new(path: String, content: Vec<u8>) -> ProviderResult<Self> {
        let path = path.replace('\\', "/");
        validate_file_path(&path)?;
        Ok(Self { path, content })
    }
}

fn validate_file_path(path: &str) -> ProviderResult<()> {
    if path.is_empty() || path.starts_with('/') || path.contains(':') {
        return Err(ProviderError::InvalidPath(path.to_owned()));
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(ProviderError::InvalidPath(path.to_owned()));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct VercelHostingProvider<C> {
    config: VercelConfig,
    credentials: C,
    client: Client,
}

impl<C: CredentialStore> VercelHostingProvider<C> {
    pub fn new(config: VercelConfig, credentials: C) -> ProviderResult<Self> {
        config.validate()?;
        let client = Client::builder()
            .build()
            .map_err(|_| ProviderError::Other)?;
        Ok(Self {
            config,
            credentials,
            client,
        })
    }

    pub fn config(&self) -> &VercelConfig {
        &self.config
    }

    fn token(&self) -> ProviderResult<String> {
        let bytes = self
            .credentials
            .get(TOKEN_KEY)?
            .ok_or(ProviderError::AuthenticationRequired)?;
        String::from_utf8(bytes).map_err(|_| ProviderError::AuthenticationFailed)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.config.api_base_url.trim_end_matches('/'), path)
    }

    fn query(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(team_id) = self.config.team_id.as_deref() {
            request.query(&[("teamId", team_id)])
        } else {
            request
        }
    }

    fn create_request(&self) -> DeploymentRequest {
        let files = self
            .config
            .files
            .iter()
            .map(|file| DeploymentRequestFile {
                file: file.path.clone(),
                data: BASE64.encode(&file.content),
                encoding: "base64",
            })
            .collect();
        DeploymentRequest {
            project: self.config.project_id.clone(),
            files,
        }
    }

    fn create_deployment(&self, token: &str) -> ProviderResult<DeploymentResponse> {
        let payload = self.create_request();
        let request = self
            .query(self.client.post(self.endpoint("/v13/deployments")))
            .bearer_auth(token)
            .json(&payload);
        let response = request.send().map_err(|_| ProviderError::Network)?;
        self.parse_response(response)
    }

    fn get_deployment(&self, token: &str, id: &str) -> ProviderResult<DeploymentResponse> {
        let mut attempts = 0;
        loop {
            let response = self
                .query(
                    self.client
                        .get(self.endpoint(&format!("/v13/deployments/{id}"))),
                )
                .bearer_auth(token)
                .send();
            match response {
                Ok(response)
                    if is_retryable_status(response.status()) && attempts < MAX_GET_RETRIES =>
                {
                    attempts += 1;
                    std::thread::sleep(RETRY_DELAY);
                }
                Ok(response) => return self.parse_response(response),
                Err(_) if attempts < MAX_GET_RETRIES => {
                    attempts += 1;
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(_) => return Err(ProviderError::Network),
            }
        }
    }

    fn parse_response(&self, response: Response) -> ProviderResult<DeploymentResponse> {
        let status = response.status();
        if !status.is_success() {
            return Err(map_status(status));
        }
        response.json().map_err(|_| ProviderError::Other)
    }

    fn complete(response: DeploymentResponse) -> ProviderResult<DeploymentInfo> {
        let id = non_empty(response.id).ok_or(ProviderError::Integrity)?;
        let url = non_empty(response.url).ok_or(ProviderError::Integrity)?;
        if !is_valid_deployment_url(&url) {
            return Err(ProviderError::Integrity);
        }
        Ok(DeploymentInfo { id, url })
    }
}

impl<C: CredentialStore> HostingProvider for VercelHostingProvider<C> {
    fn publish(&self) -> ProviderResult<DeploymentInfo> {
        self.config.validate()?;
        let token = self.token()?;
        let mut deployment = self.create_deployment(&token)?;
        let id = non_empty(deployment.id.clone()).ok_or(ProviderError::Integrity)?;

        for _ in 0..MAX_POLL_ATTEMPTS {
            match deployment.state()? {
                DeploymentState::Ready => return Self::complete(deployment),
                DeploymentState::Error | DeploymentState::Canceled | DeploymentState::Blocked => {
                    return Err(ProviderError::Other)
                }
                DeploymentState::Queued
                | DeploymentState::Initializing
                | DeploymentState::Building => {
                    std::thread::sleep(RETRY_DELAY);
                    deployment = self.get_deployment(&token, &id)?;
                }
            }
        }
        Err(ProviderError::Network)
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn is_valid_deployment_url(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) || value.starts_with('/') {
        return false;
    }

    if value.starts_with("http://") || value.starts_with("https://") {
        return reqwest::Url::parse(value)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| host.contains('.'));
    }

    if value.contains('/') || value.contains('?') || value.contains('#') {
        return false;
    }

    let mut labels = value.split('.');
    let Some(first_label) = labels.next() else {
        return false;
    };
    if !is_valid_host_label(first_label) {
        return false;
    }
    labels.next().is_some_and(is_valid_host_label)
        && labels.all(is_valid_host_label)
        && value.len() <= 253
}

fn is_valid_host_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

fn map_status(status: StatusCode) -> ProviderError {
    match status.as_u16() {
        401 => ProviderError::AuthenticationFailed,
        403 => ProviderError::PermissionDenied,
        404 => ProviderError::NotFound,
        409 => ProviderError::Conflict,
        429 | 500..=599 => ProviderError::Network,
        _ => ProviderError::Other,
    }
}

#[derive(Debug, Serialize)]
struct DeploymentRequest {
    project: String,
    files: Vec<DeploymentRequestFile>,
}

#[derive(Debug, Serialize)]
struct DeploymentRequestFile {
    file: String,
    data: String,
    encoding: &'static str,
}

#[derive(Debug, Deserialize)]
struct DeploymentResponse {
    id: Option<String>,
    url: Option<String>,
    #[serde(rename = "readyState")]
    ready_state: Option<String>,
    status: Option<String>,
}

impl DeploymentResponse {
    fn state(&self) -> ProviderResult<DeploymentState> {
        let state = self
            .ready_state
            .as_deref()
            .or(self.status.as_deref())
            .ok_or(ProviderError::Integrity)?;
        match state {
            "READY" => Ok(DeploymentState::Ready),
            "ERROR" => Ok(DeploymentState::Error),
            "CANCELED" => Ok(DeploymentState::Canceled),
            "BLOCKED" => Ok(DeploymentState::Blocked),
            "QUEUED" => Ok(DeploymentState::Queued),
            "INITIALIZING" => Ok(DeploymentState::Initializing),
            "BUILDING" => Ok(DeploymentState::Building),
            _ => Err(ProviderError::Integrity),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DeploymentState {
    Ready,
    Error,
    Canceled,
    Blocked,
    Queued,
    Initializing,
    Building,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use photo_publisher_provider_contract_tests::hosting_contract;
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

    fn provider(server: &Server, token: Option<&[u8]>) -> VercelHostingProvider<MemoryCredentials> {
        let mut config = VercelConfig::new("project-id");
        config.api_base_url = server.url();
        config.files =
            vec![DeploymentFile::new("index.html".to_owned(), b"<h1>x</h1>".to_vec()).unwrap()];
        let mut credentials = MemoryCredentials::default();
        if let Some(token) = token {
            credentials.set(TOKEN_KEY, token).unwrap();
        }
        VercelHostingProvider::new(config, credentials).unwrap()
    }

    fn provider_with_team(
        server: &Server,
        token: Option<&[u8]>,
        team_id: &str,
    ) -> VercelHostingProvider<MemoryCredentials> {
        let mut provider = provider(server, token);
        provider.config.team_id = Some(team_id.to_owned());
        provider
    }

    #[test]
    fn path_validation_rejects_absolute_and_traversal_paths() {
        for path in [
            "/index.html",
            "../index.html",
            "a/../b",
            "C:/index.html",
            "",
        ] {
            assert!(validate_file_path(path).is_err());
        }
        let normalized = DeploymentFile::new("a\\b".to_owned(), vec![0, 255]).unwrap();
        assert_eq!(normalized.path, "a/b");
        assert_eq!(normalized.content, vec![0, 255]);
        assert!(validate_file_path("álbum/index.html").is_ok());
    }

    #[test]
    fn missing_token_is_rejected_without_network_access() {
        let server = Server::new();
        let provider = provider(&server, None);
        assert!(matches!(
            provider.publish(),
            Err(ProviderError::AuthenticationRequired)
        ));
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        assert!(VercelConfig::new("").validate().is_err());
        let mut config = VercelConfig::new("project");
        config.api_base_url = "not-a-url".to_owned();
        assert!(config.validate().is_err());
    }

    #[test]
    fn post_uses_project_id_and_polls_to_ready_url_from_api() {
        let mut server = Server::new();
        let create = server
            .mock("POST", "/v13/deployments")
            .match_header("authorization", "Bearer opaque-test-credential")
            .match_header("content-type", "application/json")
            .match_body(Matcher::Json(serde_json::json!({
                "project": "project-id",
                "files": [{"file": "index.html", "data": BASE64.encode(b"<h1>x</h1>"), "encoding": "base64"}]
            })))
            .with_status(200)
            .with_body(r#"{"id":"dpl_1","readyState":"BUILDING"}"#)
            .create();
        let poll = server
            .mock("GET", "/v13/deployments/dpl_1")
            .with_status(200)
            .with_body(r#"{"id":"dpl_1","url":"project.vercel.app","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        let deployment = provider.publish().unwrap();
        assert_eq!(deployment.id, "dpl_1");
        assert_eq!(deployment.url, "project.vercel.app");
        create.assert();
        poll.assert();
    }

    #[test]
    fn missing_final_id_is_integrity_error() {
        let mut server = Server::new();
        let create = server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"readyState":"READY","url":"project.vercel.app"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert!(matches!(provider.publish(), Err(ProviderError::Integrity)));
        create.assert();
    }

    #[test]
    fn missing_final_url_is_integrity_error() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"id":"dpl_1","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert!(matches!(provider.publish(), Err(ProviderError::Integrity)));
    }

    #[test]
    fn empty_final_url_is_integrity_error() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"id":"dpl_1","url":"","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert!(matches!(provider.publish(), Err(ProviderError::Integrity)));
    }

    #[test]
    fn invalid_final_url_is_integrity_error() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"id":"dpl_1","url":"not-a-url","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert!(matches!(provider.publish(), Err(ProviderError::Integrity)));
    }

    #[test]
    fn team_id_is_url_encoded_on_create_and_not_in_json() {
        let mut server = Server::new();
        let create = server
            .mock("POST", "/v13/deployments")
            .match_query(Matcher::UrlEncoded("teamId".into(), "team x/1".into()))
            .match_body(Matcher::Json(serde_json::json!({
                "project": "project-id",
                "files": [{"file": "index.html", "data": BASE64.encode(b"<h1>x</h1>"), "encoding": "base64"}]
            })))
            .with_status(200)
            .with_body(r#"{"id":"dpl-team","url":"team.vercel.app","readyState":"READY"}"#)
            .create();
        let provider = provider_with_team(&server, Some(b"opaque-test-credential"), "team x/1");
        assert_eq!(provider.publish().unwrap().id, "dpl-team");
        create.assert();
    }

    #[test]
    fn create_without_team_id_omits_query_parameter() {
        let mut server = Server::new();
        let create = server
            .mock("POST", "/v13/deployments")
            .match_query(Matcher::Missing)
            .with_status(200)
            .with_body(r#"{"id":"dpl-no-team","url":"no-team.vercel.app","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert_eq!(provider.publish().unwrap().id, "dpl-no-team");
        create.assert();
    }

    #[test]
    fn polling_preserves_url_encoded_team_id() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .match_query(Matcher::UrlEncoded("teamId".into(), "team x/1".into()))
            .with_status(200)
            .with_body(r#"{"id":"dpl-team","readyState":"BUILDING"}"#)
            .create();
        let poll = server
            .mock("GET", "/v13/deployments/dpl-team")
            .match_query(Matcher::UrlEncoded("teamId".into(), "team x/1".into()))
            .with_status(200)
            .with_body(r#"{"id":"dpl-team","url":"team.vercel.app","readyState":"READY"}"#)
            .create();
        let provider = provider_with_team(&server, Some(b"opaque-test-credential"), "team x/1");
        assert_eq!(provider.publish().unwrap().id, "dpl-team");
        poll.assert();
    }

    #[test]
    fn deployment_error_and_canceled_states_fail() {
        for state in ["ERROR", "CANCELED", "BLOCKED"] {
            let mut server = Server::new();
            server
                .mock("POST", "/v13/deployments")
                .with_status(200)
                .with_body(format!(
                    r#"{{"id":"dpl-{}","readyState":"{}"}}"#,
                    state.to_lowercase(),
                    state
                ))
                .create();
            let provider = provider(&server, Some(b"opaque-test-credential"));
            assert!(matches!(provider.publish(), Err(ProviderError::Other)));
        }
    }

    #[test]
    fn invalid_json_maps_to_other_and_token_is_not_in_error() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body("not-json")
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        let error = provider.publish().unwrap_err();
        assert!(matches!(error, ProviderError::Other));
        assert!(!error.to_string().contains("opaque-test-credential"));
    }

    #[test]
    fn http_errors_are_mapped_without_response_body() {
        for (status, expected) in [
            (401, ProviderError::AuthenticationFailed),
            (403, ProviderError::PermissionDenied),
            (404, ProviderError::NotFound),
            (409, ProviderError::Conflict),
            (429, ProviderError::Network),
            (500, ProviderError::Network),
            (502, ProviderError::Network),
            (504, ProviderError::Network),
        ] {
            let mut server = Server::new();
            server
                .mock("POST", "/v13/deployments")
                .with_status(status)
                .with_body("secret response body")
                .create();
            let provider = provider(&server, Some(b"opaque-test-credential"));
            let actual = provider.publish().unwrap_err();
            assert!(matches!(
                (&actual, &expected),
                (
                    ProviderError::AuthenticationFailed,
                    ProviderError::AuthenticationFailed
                ) | (
                    ProviderError::PermissionDenied,
                    ProviderError::PermissionDenied
                ) | (ProviderError::NotFound, ProviderError::NotFound)
                    | (ProviderError::Conflict, ProviderError::Conflict)
                    | (ProviderError::Network, ProviderError::Network)
            ));
            assert!(!actual.to_string().contains("secret response body"));
        }
    }

    #[test]
    fn get_retries_transient_error_then_succeeds() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"id":"dpl-retry","readyState":"BUILDING"}"#)
            .create();
        server
            .mock("GET", "/v13/deployments/dpl-retry")
            .with_status(503)
            .expect(1)
            .create();
        server
            .mock("GET", "/v13/deployments/dpl-retry")
            .with_status(200)
            .with_body(r#"{"id":"dpl-retry","url":"retry.vercel.app","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert_eq!(provider.publish().unwrap().id, "dpl-retry");
    }

    #[test]
    fn get_retries_each_supported_transient_status() {
        for status in [429, 500, 502, 503, 504] {
            let mut server = Server::new();
            server
                .mock("POST", "/v13/deployments")
                .with_status(200)
                .with_body(r#"{"id":"dpl-transient","readyState":"BUILDING"}"#)
                .create();
            server
                .mock("GET", "/v13/deployments/dpl-transient")
                .with_status(status)
                .expect(1)
                .create();
            server
                .mock("GET", "/v13/deployments/dpl-transient")
                .with_status(200)
                .with_body(
                    r#"{"id":"dpl-transient","url":"transient.vercel.app","readyState":"READY"}"#,
                )
                .create();
            let provider = provider(&server, Some(b"opaque-test-credential"));
            assert_eq!(provider.publish().unwrap().id, "dpl-transient");
        }
    }

    #[test]
    fn polling_timeout_is_bounded() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"id":"dpl-timeout","readyState":"BUILDING"}"#)
            .create();
        server
            .mock("GET", "/v13/deployments/dpl-timeout")
            .with_status(200)
            .with_body(r#"{"id":"dpl-timeout","readyState":"BUILDING"}"#)
            .expect(MAX_POLL_ATTEMPTS)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert!(matches!(provider.publish(), Err(ProviderError::Network)));
    }

    #[test]
    fn post_is_not_retried_after_ambiguous_server_error() {
        let mut server = Server::new();
        let create = server
            .mock("POST", "/v13/deployments")
            .with_status(503)
            .expect(1)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        assert!(matches!(provider.publish(), Err(ProviderError::Network)));
        create.assert();
    }

    #[test]
    fn reusable_hosting_contract_passes_offline() {
        let mut server = Server::new();
        server
            .mock("POST", "/v13/deployments")
            .with_status(200)
            .with_body(r#"{"id":"dpl-contract","url":"contract.vercel.app","readyState":"READY"}"#)
            .create();
        let provider = provider(&server, Some(b"opaque-test-credential"));
        hosting_contract(&provider);
    }
}
