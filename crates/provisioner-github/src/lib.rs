//! Concrete [`RepositoryProvisioner`] for GitHub (Phase 7-H; branch
//! guarantee added in Phase 7-K.4).
//!
//! Infrastructure only: this provisioner checks whether a repository exists,
//! creates it when it does not, and guarantees the configured branch exists.
//! It never writes files, blobs, or content refs — the single admitted Git
//! Database use is the *contentless* branch bootstrap (empty tree, root
//! commit without parents or files, branch ref creation). Everything
//! content-bearing is Publishing (`provider-github`), and this crate neither
//! imports nor calls any of it. Repository creation never uses `auto_init`:
//! the branch is bootstrapped through the Git Database instead, so the
//! infrastructure stays free of any project content.
//!
//! Endpoint semantics (deterministic, no blind retries):
//!
//! ```text
//! GET /repos/{owner}/{name}
//!   200 → branch check
//!   404 → creation path
//! branch check:
//!   GET /repos/{owner}/{name}/git/ref/heads/{branch}
//!     200 → branch present
//!     404 → bootstrap path
//! bootstrap (contentless):
//!   POST /repos/{owner}/{name}/git/trees    {"tree": []}
//!   POST /repos/{owner}/{name}/git/commits  {tree, parents: []}
//!   POST /repos/{owner}/{name}/git/refs     {ref: refs/heads/<branch>}
//!     422 "already exists" → created concurrently: one verification read
//!     settles it (present ⇒ achieved; still absent ⇒ Conflict), and the
//!     creation is never re-sent, never forced
//! create:
//!   GET /user                     (who is the token's owner?)
//!     login == owner   → POST /user/repos
//!     login != owner   → GET /orgs/{owner}
//!                          200 → POST /orgs/{owner}/repos
//!                          404 → Conflict (owner is neither the
//!                                authenticated user nor an accessible org)
//!   POST …/repos → 2xx → bootstrap path (a fresh repository has no refs)
//!   POST …/repos → 409/422 → Conflict (never re-sent blindly)
//! outcome:
//!   Created    — repository created and branch bootstrapped
//!   Configured — repository existed; the branch was missing and got created
//!   Unchanged  — repository and branch both already existed
//!   error      — a created repository whose branch could not be
//!                bootstrapped is NOT a "Created" success
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

/// Message of the single contentless infrastructure commit that gives a
/// missing branch its identity. Deterministic and fixed: nothing generated,
/// nothing copied from the project, no timestamps, no secrets.
const BOOTSTRAP_COMMIT_MESSAGE: &str = "photo-publisher: initialize branch";

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

impl GitHubRepositoryProvisioner {
    /// The branch the configuration requires, qualified as a `heads/` ref
    /// path suffix (the same shape the publishing side addresses).
    fn branch_ref(config: &RepositoryProvisionConfig) -> String {
        format!("heads/{}", config.branch())
    }

    /// Creates the repository through the existing endpoint-selection rule.
    /// `auto_init` is never requested: a fresh repository starts with no
    /// refs and the branch bootstrap below supplies exactly one.
    fn create_repository(
        &self,
        config: &RepositoryProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<()> {
        // Decide the endpoint from the token owner vs. the declared owner —
        // the only unambiguous, documented possibilities.
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
            return Ok(());
        }
        let kind = Self::map_status(status);
        Err(ProvisioningError::new(
            // A 409/422 at creation time after a 404 above is a divergence,
            // never something to repeat blindly.
            kind,
            format!("the repository could not be created (HTTP {status})"),
        ))
    }

    /// Whether the configured branch already exists: one read, one answer.
    /// Any status other than 200/404 maps through the existing
    /// classification — existence is never guessed from an ambiguous reply.
    fn branch_exists(
        &self,
        config: &RepositoryProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<bool> {
        let (status, _) = self.request(
            Method::GET,
            &format!(
                "/repos/{}/{}/git/ref/{}",
                config.owner,
                config.name,
                Self::branch_ref(config)
            ),
            None,
            credentials,
        )?;
        if status.is_success() {
            return Ok(true);
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        Err(ProvisioningError::new(
            Self::map_status(status),
            format!("could not check the repository branch state (HTTP {status})"),
        ))
    }

    /// Creates the configured branch as a *contentless* Git root through the
    /// Git Database API (Phase 7-K.4): an empty tree, a single
    /// infrastructure commit (no parents, no files), and the branch ref.
    /// The branch gives infrastructure identity only — no Publisher content
    /// is ever written here, and an existing branch is never moved or
    /// re-created (no force, no blind retry).
    fn bootstrap_branch(
        &self,
        config: &RepositoryProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<()> {
        let repository = format!("/repos/{}/{}", config.owner, config.name);

        // 1. The empty tree — no blobs, no files, no Publisher content.
        let (status, body) = self.request(
            Method::POST,
            &format!("{repository}/git/trees"),
            Some(json!({ "tree": [] })),
            credentials,
        )?;
        let tree_sha = Self::created_sha(status, body, "the empty infrastructure tree")?;

        // 2. The single contentless infrastructure commit.
        let (status, body) = self.request(
            Method::POST,
            &format!("{repository}/git/commits"),
            Some(json!({
                "message": BOOTSTRAP_COMMIT_MESSAGE,
                "tree": tree_sha,
                "parents": [],
            })),
            credentials,
        )?;
        let commit_sha = Self::created_sha(status, body, "the infrastructure commit")?;

        // 3. The branch ref itself.
        let (status, body) = self.request(
            Method::POST,
            &format!("{repository}/git/refs"),
            Some(json!({
                "ref": format!("refs/{}", Self::branch_ref(config)),
                "sha": commit_sha,
            })),
            credentials,
        )?;
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::UNPROCESSABLE_ENTITY && Self::references_existing_ref(&body) {
            // Another process created the branch between the existence
            // check and this creation. Resolve the race with one
            // verification read — never a blind retry, never a force: if
            // the branch is there now, the desired state holds.
            if self.branch_exists(config, credentials)? {
                return Ok(());
            }
            return Err(ProvisioningError::new(
                ProvisioningErrorKind::Conflict,
                "the branch creation raced and the branch is still absent",
            ));
        }
        Err(ProvisioningError::new(
            Self::map_status(status),
            format!("could not create the repository branch (HTTP {status})"),
        ))
    }

    /// The object id a successful Git Database creation returned. A success
    /// status without a sha cannot be acted upon and is surfaced as an
    /// internal provisioner failure — success is never assumed.
    fn created_sha(
        status: StatusCode,
        body: Option<Value>,
        what: &str,
    ) -> ProvisioningResult<String> {
        if !status.is_success() {
            return Err(ProvisioningError::new(
                Self::map_status(status),
                format!("could not create {what} (HTTP {status})"),
            ));
        }
        match body
            .as_ref()
            .and_then(|value| value.get("sha"))
            .and_then(Value::as_str)
        {
            Some(sha) if !sha.is_empty() => Ok(sha.to_owned()),
            _ => Err(ProvisioningError::new(
                ProvisioningErrorKind::Internal,
                format!("{what} response carried no object id"),
            )),
        }
    }

    /// True only when the server *explicitly* reported the ref as already
    /// existing — the single 422 case that is safe to resolve by a
    /// verification read. Any other failure text stays a plain error.
    fn references_existing_ref(body: &Option<Value>) -> bool {
        body.as_ref()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .is_some_and(|message| message.to_ascii_lowercase().contains("already exists"))
    }
}

impl RepositoryProvisioner for GitHubRepositoryProvisioner {
    fn provision_repository(
        &mut self,
        config: &RepositoryProvisionConfig,
        credentials: &dyn CredentialStore,
    ) -> ProvisioningResult<ProvisioningOutcome<RepositoryIdentity>> {
        // 1. Existence check. An existing repository is reconciled by its
        //    branch: present ⇒ Unchanged, absent ⇒ bootstrapped (Configured).
        let path = format!("/repos/{}/{}", config.owner, config.name);
        let (status, _) = self.request(Method::GET, &path, None, credentials)?;
        if status.is_success() {
            return if self.branch_exists(config, credentials)? {
                Ok(ProvisioningOutcome::Unchanged(config.identity()))
            } else {
                self.bootstrap_branch(config, credentials)?;
                Ok(ProvisioningOutcome::Configured(config.identity()))
            };
        }
        if status != StatusCode::NOT_FOUND {
            let kind = Self::map_status(status);
            return Err(ProvisioningError::new(
                kind,
                format!("could not check the repository state (HTTP {status})"),
            ));
        }

        // 2. Creation: existing endpoint-selection rule, no `auto_init`.
        self.create_repository(config, credentials)?;

        // 3. A freshly created repository has no refs: bootstrap the
        //    configured branch unconditionally. A bootstrap failure is an
        //    ERROR, never a bare "Created" that would leave the Publisher
        //    facing a missing branch.
        self.bootstrap_branch(config, credentials)?;
        Ok(ProvisioningOutcome::Created(config.identity()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Mock, Server};
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
    const TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const COMMIT_SHA: &str = "0000000000000000000000000000000000000001";

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

    /// Exact-body matcher built with the same serializer the client uses:
    /// any extra or missing key (e.g. an `auto_init`) fails the match.
    fn exact_body(body: Value) -> Matcher {
        Matcher::Exact(serde_json::to_string(&body).unwrap())
    }

    fn repository_path() -> String {
        "/repos/fotografo/joao-maria-2026".to_owned()
    }

    fn branch_ref_path(branch: &str) -> String {
        format!("{}/git/ref/heads/{branch}", repository_path())
    }

    /// The branch-existence read, answered as present.
    fn branch_present(server: &mut Server, branch: &str) -> Mock {
        server
            .mock("GET", branch_ref_path(branch).as_str())
            .with_status(200)
            .with_body(format!(
                r#"{{"ref":"refs/heads/{branch}","object":{{"sha":"{COMMIT_SHA}","type":"commit"}}}}"#
            ))
            .create()
    }

    /// The contentless bootstrap, asserting the exact payloads: the tree is
    /// empty (no files), the commit has no parents, the ref targets the
    /// configured branch and is created — never patched.
    fn bootstrap_mocks(server: &mut Server, branch: &str) -> (Mock, Mock, Mock) {
        let tree = server
            .mock("POST", format!("{}/git/trees", repository_path()).as_str())
            .match_body(exact_body(json!({"tree": []})))
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{TREE_SHA}"}}"#))
            .create();
        let commit = server
            .mock(
                "POST",
                format!("{}/git/commits", repository_path()).as_str(),
            )
            .match_body(exact_body(
                json!({"message": BOOTSTRAP_COMMIT_MESSAGE, "tree": TREE_SHA, "parents": []}),
            ))
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{COMMIT_SHA}"}}"#))
            .create();
        let reference = server
            .mock("POST", format!("{}/git/refs", repository_path()).as_str())
            .match_body(exact_body(
                json!({"ref": format!("refs/heads/{branch}"), "sha": COMMIT_SHA}),
            ))
            .with_status(201)
            .with_body(format!(r#"{{"ref":"refs/heads/{branch}"}}"#))
            .create();
        (tree, commit, reference)
    }

    #[test]
    fn existing_repository_is_unchanged_never_created_again() {
        let mut server = Server::new();
        let get = server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .create();
        let branch = branch_present(&mut server, "main");
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "unchanged");
        assert_eq!(outcome.resource().to_string(), "fotografo/joao-maria-2026");
        get.assert();
        branch.assert();
        // No mutation mock exists: any POST would have failed the request.
    }

    #[test]
    fn missing_repository_is_created_under_the_token_owner() {
        let mut server = Server::new();
        let get = server
            .mock("GET", repository_path().as_str())
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
            // Exact body: `auto_init` (or any other key) never appears.
            .match_body(exact_body(json!({"name": "joao-maria-2026"})))
            .with_status(201)
            .with_body("{}")
            .create();
        let (tree, commit, reference) = bootstrap_mocks(&mut server, "main");
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        assert_eq!(outcome.resource().to_string(), "fotografo/joao-maria-2026");
        get.assert();
        whoami.assert();
        create.assert();
        tree.assert();
        commit.assert();
        reference.assert();
    }

    #[test]
    fn creation_under_an_organization_uses_the_org_endpoint() {
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
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
            .match_body(exact_body(json!({"name": "joao-maria-2026"})))
            .with_status(201)
            .with_body("{}")
            .create();
        let (tree, commit, reference) = bootstrap_mocks(&mut server, "main");
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        org.assert();
        create.assert();
        tree.assert();
        commit.assert();
        reference.assert();
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
        branch_present(&mut server, "main");
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        let rendered = format!("{outcome:?} {outcome}");
        assert!(!rendered.contains(SENTINEL));
    }

    #[test]
    fn bootstrap_failures_never_leak_the_token_or_succeed_silently() {
        // The branch bootstrap is part of the provisioning promise: a
        // failure there is an ERROR (never a hollow "Created"/"Configured"),
        // and provider payloads — even ones echoing the credential — never
        // reach the error surface.
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .create();
        server
            .mock("GET", branch_ref_path("main").as_str())
            .with_status(404)
            .with_body("{}")
            .create();
        server
            .mock("POST", format!("{}/git/trees", repository_path()).as_str())
            .with_status(401)
            .with_body(format!(r#"{{"message":"{SENTINEL}"}}"#))
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::AuthenticationFailed);
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(SENTINEL));
    }

    #[test]
    fn existing_repository_without_the_branch_is_configured_not_created() {
        // Case C: the repository exists but the configured branch does not —
        // the contentless bootstrap supplies exactly that branch.
        let mut server = Server::new();
        let get = server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .create();
        let missing = server
            .mock("GET", branch_ref_path("main").as_str())
            .with_status(404)
            .with_body("{}")
            .create();
        let (tree, commit, reference) = bootstrap_mocks(&mut server, "main");
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "configured");
        assert_eq!(outcome.resource().to_string(), "fotografo/joao-maria-2026");
        get.assert();
        missing.assert();
        tree.assert();
        commit.assert();
        reference.assert();
    }

    #[test]
    fn a_custom_configured_branch_is_checked_and_bootstrapped_at_its_own_ref() {
        // `branch = "production"` must guarantee refs/heads/production —
        // never the default `main`.
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .create();
        let missing = server
            .mock("GET", branch_ref_path("production").as_str())
            .with_status(404)
            .with_body("{}")
            .create();
        let (tree, commit, reference) = bootstrap_mocks(&mut server, "production");
        let mut provisioner = provisioner(&server);
        let config = config().with_branch(Some("production".to_owned())).unwrap();
        let outcome = provisioner
            .provision_repository(&config, &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "configured");
        missing.assert();
        tree.assert();
        commit.assert();
        reference.assert();
    }

    #[test]
    fn a_second_run_never_recreates_the_repository_or_the_branch() {
        // First run: everything absent → repository created, branch
        // bootstrapped, `Created`.
        let mut server = Server::new();
        {
            let missing = server
                .mock("GET", repository_path().as_str())
                .with_status(404)
                .with_body("{}")
                .expect(1)
                .create();
            let whoami = server
                .mock("GET", "/user")
                .with_status(200)
                .with_body(r#"{"login":"fotografo"}"#)
                .expect(1)
                .create();
            let create = server
                .mock("POST", "/user/repos")
                .with_status(201)
                .with_body("{}")
                .expect(1)
                .create();
            let (tree, commit, reference) = bootstrap_mocks(&mut server, "main");
            let mut provisioner = provisioner(&server);
            let outcome = provisioner
                .provision_repository(&config(), &credentials_with_token())
                .unwrap();
            assert_eq!(outcome.status().as_str(), "created");
            missing.assert();
            whoami.assert();
            create.assert();
            tree.assert();
            commit.assert();
            reference.assert();
            drop(missing);
            drop(whoami);
            drop(create);
            drop(tree);
            drop(commit);
            drop(reference);
        }
        // Second run: everything present → `Unchanged`, and the only
        // tolerated calls are the two reads — any mutation POST reaches no
        // mock and fails the run.
        let existing = server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create();
        let branch = server
            .mock("GET", branch_ref_path("main").as_str())
            .with_status(200)
            .with_body(format!(
                r#"{{"ref":"refs/heads/main","object":{{"sha":"{COMMIT_SHA}","type":"commit"}}}}"#
            ))
            .expect(1)
            .create();
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "unchanged");
        existing.assert();
        branch.assert();
    }

    #[test]
    fn a_branch_creation_race_is_resolved_by_verification_not_retry() {
        // Creation path raced: the repository is brand new (no prior ref
        // read), the ref creation answers "already exists" because another
        // process created the branch concurrently, and one verification
        // read confirms the desired state. Created stays honest; nothing is
        // retried blindly or forced.
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
            .with_status(404)
            .with_body("{}")
            .create();
        server
            .mock("GET", "/user")
            .with_status(200)
            .with_body(r#"{"login":"fotografo"}"#)
            .create();
        server
            .mock("POST", "/user/repos")
            .with_status(201)
            .with_body("{}")
            .create();
        server
            .mock("POST", format!("{}/git/trees", repository_path()).as_str())
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{TREE_SHA}"}}"#))
            .create();
        server
            .mock(
                "POST",
                format!("{}/git/commits", repository_path()).as_str(),
            )
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{COMMIT_SHA}"}}"#))
            .create();
        let raced = server
            .mock("POST", format!("{}/git/refs", repository_path()).as_str())
            .with_status(422)
            .with_body(r#"{"message":"Reference already exists"}"#)
            .expect(1)
            .create();
        let verified = branch_present(&mut server, "main");
        let mut provisioner = provisioner(&server);
        let outcome = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap();
        assert_eq!(outcome.status().as_str(), "created");
        raced.assert();
        verified.assert();
    }

    #[test]
    fn an_unconfirmed_branch_creation_race_is_a_conflict_never_a_success() {
        // Existing repository, branch missing, creation answers "already
        // exists", but the verification read still finds nothing: the race
        // cannot be resolved unambiguously, so the run is a Conflict — the
        // desired state is never pretended.
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .create();
        let missing = server
            .mock("GET", branch_ref_path("main").as_str())
            .with_status(404)
            .with_body("{}")
            .expect(2) // initial check + the single verification read
            .create();
        server
            .mock("POST", format!("{}/git/trees", repository_path()).as_str())
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{TREE_SHA}"}}"#))
            .create();
        server
            .mock(
                "POST",
                format!("{}/git/commits", repository_path()).as_str(),
            )
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{COMMIT_SHA}"}}"#))
            .create();
        let raced = server
            .mock("POST", format!("{}/git/refs", repository_path()).as_str())
            .with_status(422)
            .with_body(r#"{"message":"Reference already exists"}"#)
            .expect(1)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Conflict);
        missing.assert();
        raced.assert();
    }

    #[test]
    fn an_ambiguous_branch_creation_failure_is_a_conflict_never_a_success() {
        // A 422 that does NOT report an existing ref is not resolvable: it
        // maps to the existing Conflict classification and no verification
        // pretends success.
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
            .with_status(200)
            .with_body("{}")
            .create();
        server
            .mock("GET", branch_ref_path("main").as_str())
            .with_status(404)
            .with_body("{}")
            .create();
        server
            .mock("POST", format!("{}/git/trees", repository_path()).as_str())
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{TREE_SHA}"}}"#))
            .create();
        server
            .mock(
                "POST",
                format!("{}/git/commits", repository_path()).as_str(),
            )
            .with_status(201)
            .with_body(format!(r#"{{"sha":"{COMMIT_SHA}"}}"#))
            .create();
        let failed = server
            .mock("POST", format!("{}/git/refs", repository_path()).as_str())
            .with_status(422)
            .with_body(r#"{"message":"Validation Failed"}"#)
            .expect(1)
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Conflict);
        failed.assert();
    }

    #[test]
    fn a_failed_bootstrap_after_creation_is_an_error_never_created() {
        // Case D: the repository was created but the branch bootstrap
        // failed — the outcome must NOT be the hollow "Created" that would
        // leave the Publisher facing a missing branch.
        let mut server = Server::new();
        server
            .mock("GET", repository_path().as_str())
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
            .with_status(201)
            .with_body("{}")
            .create();
        let tree = server
            .mock("POST", format!("{}/git/trees", repository_path()).as_str())
            .with_status(500)
            .with_body("{}")
            .create();
        let mut provisioner = provisioner(&server);
        let error = provisioner
            .provision_repository(&config(), &credentials_with_token())
            .unwrap_err();
        assert_eq!(error.kind, ProvisioningErrorKind::Network);
        create.assert();
        tree.assert();
    }

    #[test]
    fn the_branch_bootstrap_contains_no_files_and_no_publisher_content() {
        // Structural proof, at the source level: the only Git Database
        // bodies this provisioner can build are the empty tree, the
        // contentless commit, and the ref creation — `bootstrap_mocks`
        // pins the exact bytes in the behavioral tests above.
        let source =
            std::fs::read_to_string(format!("{}/src/lib.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let source = source.split("#[cfg(test)]").next().unwrap();
        assert!(source.contains("json!({ \"tree\": [] })"));
        assert!(source.contains("\"parents\": []"));
        for forbidden in [
            "README",
            ".gitignore",
            "LICENSE",
            "gallery.json",
            "state.json",
        ] {
            assert!(
                !source.contains(forbidden),
                "the branch bootstrap must not reference {forbidden}"
            );
        }
        // The injected configuration itself never carries file content.
        let config = config();
        let rendered = format!("{config:?}");
        for forbidden in [
            "README",
            ".gitignore",
            "LICENSE",
            "gallery.json",
            "state.json",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "config must not carry {forbidden}"
            );
        }
        // The commit message is a fixed infrastructure marker, free of any
        // project data.
        assert_eq!(
            BOOTSTRAP_COMMIT_MESSAGE,
            "photo-publisher: initialize branch"
        );
    }

    #[test]
    fn the_provisioner_never_touches_publishing_surfaces() {
        // Structural proof: this crate implements only the provisioning
        // contract; publishing traits and content verbs never appear.
        // Phase 7-K.4 adds exactly ONE sanctioned Git Database use — the
        // contentless branch bootstrap (empty tree, parentless commit, ref
        // creation) — everything content-bearing stays forbidden.
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
            "git/blobs",
            "/contents/",
            "\"auto_init\"",
        ] {
            assert!(
                !source.contains(forbidden),
                "github provisioner must not reference {forbidden}"
            );
        }
        // The sanctioned bootstrap is exactly these three Git Database
        // calls — and no PATCH of any ref (a branch is never moved here).
        for required in ["git/trees", "git/commits", "git/refs"] {
            assert!(
                source.contains(required),
                "github provisioner must use {required} for the branch bootstrap"
            );
        }
        assert!(
            !source.contains("Method::PATCH"),
            "the provisioner must never patch a ref"
        );
    }
}
