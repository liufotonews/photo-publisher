//! GitHub RepositoryProvider backed by the Git Database REST API.
//!
//! `write` and `delete` create pending logical changes. Writes may create
//! remote blobs before `commit`; unreferenced blobs are harmless Git objects.
//! Only the final non-forced ref update publishes a commit on the branch.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use photo_publisher_provider_contracts::{
    CommitInfo, CredentialStore, FileMetadata, ProviderError, ProviderResult, RepositoryPath,
    RepositoryProvider,
};
use reqwest::blocking::Client;
use reqwest::{Method, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const API_VERSION: &str = "2026-03-10";
const DEFAULT_BRANCH: &str = "main";
const DEFAULT_TOKEN_NAME: &str = "github.token";
const MAX_SHORT_RETRY_SECONDS: u64 = 2;

#[derive(Debug, Clone)]
pub struct GitHubRepositoryConfig {
    pub owner: String,
    pub repository: String,
    pub branch: String,
    pub token_name: String,
    pub api_base_url: String,
    pub create_if_missing: bool,
    pub organization: Option<String>,
}

impl GitHubRepositoryConfig {
    pub fn new(owner: impl Into<String>, repository: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repository: repository.into(),
            branch: DEFAULT_BRANCH.to_owned(),
            token_name: DEFAULT_TOKEN_NAME.to_owned(),
            api_base_url: "https://api.github.com".to_owned(),
            create_if_missing: false,
            organization: None,
        }
    }
}

#[derive(Debug, Clone)]
enum PendingChange {
    Put { blob_sha: String },
    Delete,
}

#[derive(Debug, Clone)]
pub struct GitHubRepositoryProvider<C> {
    client: Client,
    config: GitHubRepositoryConfig,
    credentials: Arc<C>,
    pending: HashMap<RepositoryPath, PendingChange>,
}

#[derive(Debug, Deserialize)]
struct RefResponse {
    object: GitObject,
}

#[derive(Debug, Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    object_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitResponse {
    tree: GitObject,
}

#[derive(Debug, Deserialize)]
struct TreeResponse {
    tree: Vec<TreeEntry>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    entry_type: String,
    sha: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlobResponse {
    content: String,
    encoding: String,
}

#[derive(Debug, Deserialize)]
struct BlobCreated {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct CommitCreated {
    sha: String,
}

impl<C: CredentialStore> GitHubRepositoryProvider<C> {
    pub fn new(config: GitHubRepositoryConfig, credentials: C) -> ProviderResult<Self> {
        if config.owner.is_empty() || config.repository.is_empty() || config.branch.is_empty() {
            return Err(ProviderError::Other);
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| ProviderError::Network)?;
        Ok(Self {
            client,
            config,
            credentials: Arc::new(credentials),
            pending: HashMap::new(),
        })
    }

    pub fn config(&self) -> &GitHubRepositoryConfig {
        &self.config
    }

    fn token(&self) -> ProviderResult<String> {
        let bytes = self
            .credentials
            .get(&self.config.token_name)?
            .ok_or(ProviderError::AuthenticationRequired)?;
        String::from_utf8(bytes).map_err(|_| ProviderError::AuthenticationFailed)
    }

    fn api_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.config.api_base_url.trim_end_matches('/'),
            path
        )
    }

    fn repository_url(&self, path: &str) -> String {
        format!(
            "{}/repos/{}/{}/{}",
            self.config.api_base_url.trim_end_matches('/'),
            self.config.owner,
            self.config.repository,
            path
        )
    }

    fn request(
        &self,
        method: Method,
        url: String,
        body: Option<Value>,
        retry_safe: bool,
    ) -> ProviderResult<Value> {
        let mut attempt = 0;
        loop {
            let token = self.token()?;
            let mut request = self
                .client
                .request(method.clone(), &url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION)
                .bearer_auth(token);
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().map_err(|_| ProviderError::Network)?;
            if response.status().is_success() {
                return response.json().map_err(|_| ProviderError::Integrity);
            }
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let reset_wait = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .and_then(|reset| reset.checked_sub(unix_now()));
            let status = response.status();
            let message = response.json::<Value>().ok().and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::FORBIDDEN
                    && message
                        .as_deref()
                        .is_some_and(|value| value.to_ascii_lowercase().contains("rate limit")));
            let wait = retry_after.or(reset_wait);
            if retry_safe && rate_limited && attempt == 0 {
                if let Some(seconds) = wait.filter(|seconds| *seconds <= MAX_SHORT_RETRY_SECONDS) {
                    std::thread::sleep(Duration::from_secs(seconds));
                    attempt += 1;
                    continue;
                }
            }
            return Err(map_http_error(status, rate_limited));
        }
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> ProviderResult<T> {
        let value = self.request(Method::GET, self.repository_url(path), None, true)?;
        serde_json::from_value(value).map_err(|_| ProviderError::Integrity)
    }

    fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
        retry_safe: bool,
    ) -> ProviderResult<T> {
        let value = self.request(
            Method::POST,
            self.repository_url(path),
            Some(body),
            retry_safe,
        )?;
        serde_json::from_value(value).map_err(|_| ProviderError::Integrity)
    }

    fn patch<T: DeserializeOwned>(&self, path: &str, body: Value) -> ProviderResult<T> {
        let value = self.request(Method::PATCH, self.repository_url(path), Some(body), false)?;
        serde_json::from_value(value).map_err(|_| ProviderError::Integrity)
    }

    fn repository_path(&self, path: &RepositoryPath) -> String {
        path.as_str().to_owned()
    }

    fn branch_ref(&self) -> String {
        format!("heads/{}", self.config.branch)
    }

    fn current_base(&self) -> ProviderResult<(String, String)> {
        let reference: RefResponse = self.get(&format!("git/ref/{}", self.branch_ref()))?;
        if reference.object.object_type.as_deref() != Some("commit") {
            return Err(ProviderError::Integrity);
        }
        let commit: CommitResponse = self.get(&format!("git/commits/{}", reference.object.sha))?;
        if commit.tree.object_type.as_deref() != Some("tree") {
            return Err(ProviderError::Integrity);
        }
        Ok((reference.object.sha, commit.tree.sha))
    }

    fn tree_entry(&self, tree_sha: &str, component: &str) -> ProviderResult<Option<TreeEntry>> {
        let tree: TreeResponse = self.get(&format!("git/trees/{}", tree_sha))?;
        if tree.truncated {
            return Err(ProviderError::Integrity);
        }
        Ok(tree.tree.into_iter().find(|entry| entry.path == component))
    }

    fn resolve_path(&self, path: &RepositoryPath) -> ProviderResult<Option<TreeEntry>> {
        let (_, mut tree_sha) = self.current_base()?;
        let components: Vec<&str> = path.as_str().split('/').collect();
        for (index, component) in components.iter().enumerate() {
            let entry = match self.tree_entry(&tree_sha, component)? {
                Some(entry) => entry,
                None => return Ok(None),
            };
            if index + 1 == components.len() {
                return Ok(Some(entry));
            }
            if entry.entry_type != "tree" {
                return Ok(None);
            }
            tree_sha = entry.sha.ok_or(ProviderError::Integrity)?;
        }
        Ok(None)
    }

    fn mark_put(
        &mut self,
        path: RepositoryPath,
        content: &mut dyn Read,
    ) -> ProviderResult<FileMetadata> {
        let mut bytes = Vec::new();
        content
            .read_to_end(&mut bytes)
            .map_err(ProviderError::from_io)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let size_bytes = bytes.len() as u64;
        let blob: BlobCreated = self.post(
            "git/blobs",
            json!({"content": STANDARD.encode(&bytes), "encoding": "base64"}),
            true,
        )?;
        if blob.sha.is_empty() {
            return Err(ProviderError::Integrity);
        }
        self.pending
            .insert(path.clone(), PendingChange::Put { blob_sha: blob.sha });
        Ok(FileMetadata {
            path,
            size_bytes,
            sha256,
        })
    }
}

impl<C: CredentialStore> RepositoryProvider for GitHubRepositoryProvider<C> {
    fn ensure_repository(&mut self) -> ProviderResult<()> {
        let repository = self.request(
            Method::GET,
            self.api_url(&format!(
                "repos/{}/{}",
                self.config.owner, self.config.repository
            )),
            None,
            true,
        );
        match repository {
            Ok(_) => {
                let _: RefResponse = self.get(&format!("git/ref/{}", self.branch_ref()))?;
                Ok(())
            }
            Err(ProviderError::NotFound) if self.config.create_if_missing => {
                let path = if let Some(organization) = &self.config.organization {
                    format!("orgs/{organization}/repos")
                } else {
                    "user/repos".to_owned()
                };
                let _: Value = self.request(
                    Method::POST,
                    self.api_url(&path),
                    Some(json!({"name": self.config.repository, "auto_init": true})),
                    false,
                )?;
                let _: RefResponse = self.get(&format!("git/ref/{}", self.branch_ref()))?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn exists(&self, path: &RepositoryPath) -> ProviderResult<bool> {
        Ok(self
            .resolve_path(path)?
            .is_some_and(|entry| entry.entry_type == "blob"))
    }

    fn read(&self, path: &RepositoryPath) -> ProviderResult<Vec<u8>> {
        let entry = self.resolve_path(path)?.ok_or(ProviderError::NotFound)?;
        if entry.entry_type != "blob" {
            return Err(ProviderError::InvalidPath("path is not a file".to_owned()));
        }
        let sha = entry.sha.ok_or(ProviderError::Integrity)?;
        let blob: BlobResponse = self.get(&format!("git/blobs/{sha}"))?;
        if blob.encoding != "base64" {
            return Err(ProviderError::Integrity);
        }
        let content = blob.content.replace('\n', "");
        STANDARD
            .decode(content)
            .map_err(|_| ProviderError::Integrity)
    }

    fn write(
        &mut self,
        path: &RepositoryPath,
        content: &mut dyn Read,
    ) -> ProviderResult<FileMetadata> {
        self.mark_put(path.clone(), content)
    }

    fn delete(&mut self, path: &RepositoryPath) -> ProviderResult<()> {
        self.pending.insert(path.clone(), PendingChange::Delete);
        Ok(())
    }

    fn commit(&mut self, message: &str) -> ProviderResult<CommitInfo> {
        let changed_paths = {
            let mut paths: Vec<_> = self.pending.keys().cloned().collect();
            paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
            paths
        };
        let (base_commit, base_tree) = self.current_base()?;
        if changed_paths.is_empty() {
            return Ok(CommitInfo {
                revision: base_commit,
                message: message.to_owned(),
                changed_paths,
            });
        }
        let entries: Vec<Value> = changed_paths
            .iter()
            .map(
                |path| match self.pending.get(path).expect("path came from pending") {
                    PendingChange::Put { blob_sha, .. } => json!({
                        "path": self.repository_path(path),
                        "mode": "100644",
                        "type": "blob",
                        "sha": blob_sha,
                    }),
                    PendingChange::Delete => json!({
                        "path": self.repository_path(path),
                        "mode": "100644",
                        "type": "blob",
                        "sha": Value::Null,
                    }),
                },
            )
            .collect();
        let tree: GitObject = self.post(
            "git/trees",
            json!({"base_tree": base_tree, "tree": entries}),
            true,
        )?;
        let created: CommitCreated = self.post(
            "git/commits",
            json!({"message": message, "tree": tree.sha, "parents": [base_commit]}),
            false,
        )?;
        let _: RefResponse = self.patch(
            &format!("git/refs/{}", self.branch_ref()),
            json!({"sha": created.sha, "force": false}),
        )?;
        self.pending.clear();
        Ok(CommitInfo {
            revision: created.sha,
            message: message.to_owned(),
            changed_paths,
        })
    }
}

fn map_http_error(status: StatusCode, rate_limited: bool) -> ProviderError {
    if rate_limited {
        return ProviderError::Network;
    }
    match status {
        StatusCode::UNAUTHORIZED => ProviderError::AuthenticationFailed,
        StatusCode::FORBIDDEN => ProviderError::PermissionDenied,
        StatusCode::NOT_FOUND => ProviderError::NotFound,
        StatusCode::CONFLICT => ProviderError::Conflict,
        StatusCode::UNPROCESSABLE_ENTITY => ProviderError::Other,
        status if status.is_server_error() => ProviderError::Network,
        _ => ProviderError::Other,
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryCredentials(Mutex<Option<Vec<u8>>>);

    impl CredentialStore for MemoryCredentials {
        fn get(&self, _name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn set(&mut self, _name: &str, secret: &[u8]) -> ProviderResult<()> {
            *self.0.get_mut().unwrap() = Some(secret.to_vec());
            Ok(())
        }
        fn delete(&mut self, _name: &str) -> ProviderResult<()> {
            *self.0.get_mut().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn defaults_are_non_provisioning_and_main_branch() {
        let config = GitHubRepositoryConfig::new("owner", "repo");
        assert_eq!(config.branch, "main");
        assert!(!config.create_if_missing);
    }

    #[test]
    fn http_mapping_never_contains_response_or_token() {
        assert!(matches!(
            map_http_error(StatusCode::UNAUTHORIZED, false),
            ProviderError::AuthenticationFailed
        ));
        assert!(matches!(
            map_http_error(StatusCode::CONFLICT, false),
            ProviderError::Conflict
        ));
        assert!(matches!(
            map_http_error(StatusCode::TOO_MANY_REQUESTS, true),
            ProviderError::Network
        ));
    }

    #[test]
    fn credential_store_is_the_only_token_source() {
        let credentials = MemoryCredentials(Mutex::new(Some(b"secret".to_vec())));
        let provider = GitHubRepositoryProvider::new(
            GitHubRepositoryConfig::new("owner", "repo"),
            credentials,
        )
        .unwrap();
        assert_eq!(provider.token().unwrap(), "secret");
        assert_eq!(provider.config().token_name, "github.token");
    }

    #[test]
    fn ensure_repository_checks_existing_configured_branch_without_provisioning() {
        let mut server = Server::new();
        let repository = server
            .mock("GET", "/repos/owner/repo")
            .with_status(200)
            .with_body(r#"{"default_branch":"main"}"#)
            .create();
        let branch = server
            .mock("GET", "/repos/owner/repo/git/ref/heads/main")
            .with_status(200)
            .with_body(r#"{"object":{"sha":"abc","type":"commit"}}"#)
            .create();
        let mut config = GitHubRepositoryConfig::new("owner", "repo");
        config.api_base_url = server.url();
        let credentials = MemoryCredentials(Mutex::new(Some(b"secret".to_vec())));
        let mut provider = GitHubRepositoryProvider::new(config, credentials).unwrap();
        provider.ensure_repository().unwrap();
        repository.assert();
        branch.assert();
    }

    #[test]
    fn missing_configured_branch_is_not_created_automatically() {
        let mut server = Server::new();
        let repository = server
            .mock("GET", "/repos/owner/repo")
            .with_status(200)
            .with_body(r#"{"default_branch":"main"}"#)
            .create();
        let branch = server
            .mock("GET", "/repos/owner/repo/git/ref/heads/main")
            .with_status(404)
            .create();
        let mut config = GitHubRepositoryConfig::new("owner", "repo");
        config.api_base_url = server.url();
        let credentials = MemoryCredentials(Mutex::new(Some(b"secret".to_vec())));
        let mut provider = GitHubRepositoryProvider::new(config, credentials).unwrap();
        assert!(matches!(
            provider.ensure_repository(),
            Err(ProviderError::NotFound)
        ));
        repository.assert();
        branch.assert();
    }
}
