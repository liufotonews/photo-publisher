//! Provisioning contracts (Phase 7-G).
//!
//! This crate holds the *formal contracts* for provisioning the four kinds
//! of infrastructure a project may need:
//!
//! | Boundary                | Resource                                |
//! |-------------------------|-----------------------------------------|
//! | [`RepositoryProvisioner`] | repository (`owner/name`)             |
//! | [`StorageProvisioner`]    | object-storage bucket (`account/bucket`)|
//! | [`HostingProvisioner`]    | hosting project (`project`)             |
//! | [`DomainProvisioner`]     | public hostname (`hostname`)            |
//!
//! ## The frontier
//!
//! Provisioning ≠ Publishing. A *Provider* (`provider-contracts`) operates
//! on infrastructure that already exists; a *Provisioner* creates or
//! reconciles infrastructure. Publishing never creates infrastructure
//! implicitly, and provisioning never writes content.
//!
//! By construction this crate cannot do anything remote or local of
//! consequence: it contains **contracts only** — traits, configuration
//! types, outcome types, and the error classification. No implementation
//! exists here, and the crate imports nothing that could perform I/O. The
//! only external dependency is the `CredentialStore` trait, reused as the
//! pre-established secrets boundary: a provisioner receives it, consults it,
//! and may never copy what it returns into a configuration, an outcome, an
//! error, or a log.
//!
//! ## Idempotency as contract
//!
//! Every provisioning verb documents the same rule: acting on an existing,
//! compatible resource yields [`ProvisioningOutcome::Unchanged`] — a
//! deliberate success, never an error — while irreconcilable divergence is
//! a [`ProvisioningErrorKind::Conflict`] *error*. Creation and
//! reconciliation are distinguishable: `Created`, `Unchanged`,
//! `Configured`, `Changed`.
//!
//! ## Known gaps (documented, not patched)
//!
//! `project.json` intentionally stays as-is in this phase. The following
//! provisioning-relevant data has no declarative home in the schema yet and
//! is left for a later phase's explicit design instead of being improvised
//! here: repository visibility/description, hosting framework/build
//! settings, domain DNS record intent, and the orchestration that would
//! attach a domain to a hosting project.

pub mod domain;
pub mod errors;
pub mod hosting;
pub mod outcome;
pub mod repository;
pub mod storage;

pub use domain::{DomainIdentity, DomainProvisionConfig, DomainProvisioner};
pub use errors::{ProvisioningError, ProvisioningErrorKind, ProvisioningResult};
pub use hosting::{HostingIdentity, HostingProvisionConfig, HostingProvisioner};
pub use outcome::{ProvisioningOutcome, ProvisioningStatus};
pub use repository::{RepositoryIdentity, RepositoryProvisionConfig, RepositoryProvisioner};
pub use storage::{StorageIdentity, StorageProvisionConfig, StorageProvisioner};

#[cfg(test)]
mod tests {
    use super::*;
    use photo_publisher_provider_contracts::{CredentialStore, ProviderResult};
    use std::collections::HashMap;

    /// In-memory credential boundary for the contract tests: no environment,
    /// no filesystem, no network.
    #[derive(Default)]
    struct FakeCredentials {
        values: HashMap<String, Vec<u8>>,
    }

    impl CredentialStore for FakeCredentials {
        fn get(&self, name: &str) -> ProviderResult<Option<Vec<u8>>> {
            Ok(self.values.get(name).cloned())
        }
        fn set(&mut self, _name: &str, _secret: &[u8]) -> ProviderResult<()> {
            unimplemented!("contracts never store credentials in tests")
        }
        fn delete(&mut self, _name: &str) -> ProviderResult<()> {
            unimplemented!("contracts never delete credentials in tests")
        }
    }

    // Four unrelated stub implementations: each boundary is exercised
    // through its own trait object, proving object safety and independence.
    struct StubRepository;
    struct StubStorage;
    struct StubHosting;
    struct StubDomain;

    impl RepositoryProvisioner for StubRepository {
        fn provision_repository(
            &mut self,
            config: &RepositoryProvisionConfig,
            _credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<RepositoryIdentity>> {
            Ok(ProvisioningOutcome::Created(config.identity()))
        }
    }

    impl StorageProvisioner for StubStorage {
        fn provision_storage(
            &mut self,
            config: &StorageProvisionConfig,
            _credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<StorageIdentity>> {
            Ok(ProvisioningOutcome::Unchanged(config.identity()))
        }
    }

    impl HostingProvisioner for StubHosting {
        fn provision_hosting(
            &mut self,
            config: &HostingProvisionConfig,
            _credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<HostingIdentity>> {
            Ok(ProvisioningOutcome::Configured(config.identity()))
        }
    }

    impl DomainProvisioner for StubDomain {
        fn provision_domain(
            &mut self,
            config: &DomainProvisionConfig,
            _credentials: &dyn CredentialStore,
        ) -> ProvisioningResult<ProvisioningOutcome<DomainIdentity>> {
            Ok(ProvisioningOutcome::Unchanged(config.identity()))
        }
    }

    #[test]
    fn each_boundary_is_independent_and_object_safe() {
        let credentials = FakeCredentials::default();

        // Each stub is used exclusively through its own trait object; no
        // boundary requires (or provides) another one.
        let repository: &mut dyn RepositoryProvisioner = &mut StubRepository;
        let storage: &mut dyn StorageProvisioner = &mut StubStorage;
        let hosting: &mut dyn HostingProvisioner = &mut StubHosting;
        let domain: &mut dyn DomainProvisioner = &mut StubDomain;

        let repo_config = RepositoryProvisionConfig::new("fotografo", "joao-maria-2026").unwrap();
        let outcome = repository
            .provision_repository(&repo_config, &credentials)
            .unwrap();
        assert_eq!(outcome.status(), ProvisioningStatus::Created);
        assert_eq!(outcome.resource().to_string(), "fotografo/joao-maria-2026");

        let storage_config = StorageProvisionConfig::new("account-example", "fotografia").unwrap();
        let outcome = storage
            .provision_storage(&storage_config, &credentials)
            .unwrap();
        assert_eq!(outcome.status(), ProvisioningStatus::Unchanged);
        assert_eq!(outcome.resource().to_string(), "account-example/fotografia");

        let hosting_config =
            HostingProvisionConfig::new("joao-maria-2026", Some("team_example".to_owned()))
                .unwrap();
        let outcome = hosting
            .provision_hosting(&hosting_config, &credentials)
            .unwrap();
        assert_eq!(outcome.status(), ProvisioningStatus::Configured);
        assert_eq!(outcome.resource().to_string(), "joao-maria-2026");

        let domain_config = DomainProvisionConfig::new("galeria.exemplo.com").unwrap();
        let outcome = domain
            .provision_domain(&domain_config, &credentials)
            .unwrap();
        assert_eq!(outcome.status(), ProvisioningStatus::Unchanged);
        assert_eq!(outcome.resource().to_string(), "galeria.exemplo.com");
    }

    #[test]
    fn conflicts_are_errors_and_reconciliation_states_are_outcomes() {
        // A divergent existing resource is representable as an *error*...
        let error = ProvisioningError::new(
            ProvisioningErrorKind::Conflict,
            "the existing repository diverges from the desired configuration",
        );
        assert_eq!(error.kind.as_str(), "conflict");
        // ...while "already exists but compatible" is a success outcome.
        let unchanged = ProvisioningOutcome::<RepositoryIdentity>::Unchanged(
            RepositoryProvisionConfig::new("fotografo", "joao-maria-2026")
                .unwrap()
                .identity(),
        );
        assert_eq!(unchanged.status().as_str(), "unchanged");
        assert!(unchanged.status() != ProvisioningStatus::Created);
    }

    #[test]
    fn configurations_validate_structure_before_any_backend_runs() {
        assert!(RepositoryProvisionConfig::new("", "name").is_err());
        assert!(RepositoryProvisionConfig::new("owner", "has/slash").is_err());
        assert!(RepositoryProvisionConfig::new("has space", "name").is_err());
        assert!(StorageProvisionConfig::new("account", " ").is_err());
        assert!(HostingProvisionConfig::new("project", Some(" ".to_owned())).is_err());
        assert!(DomainProvisionConfig::new("https://galeria.exemplo.com").is_err());
        assert!(DomainProvisionConfig::new("galeria.exemplo.com/path").is_err());
        for error in [
            RepositoryProvisionConfig::new("", "name").unwrap_err(),
            DomainProvisionConfig::new(" ").unwrap_err(),
        ] {
            assert_eq!(error.kind, ProvisioningErrorKind::InvalidConfiguration);
        }
    }

    #[test]
    fn secrets_never_enter_configurations_outcomes_or_errors() {
        let sentinel = b"TEST_SECRET_SHOULD_NEVER_ESCAPE";
        let mut credentials = FakeCredentials::default();
        credentials
            .values
            .insert("github.token".to_owned(), sentinel.to_vec());
        let config = RepositoryProvisionConfig::new("fotografo", "projeto").unwrap();
        let mut repository = StubRepository;
        let outcome = repository
            .provision_repository(&config, &credentials)
            .unwrap();
        let rendered = format!("{config:?} {outcome:?} {outcome}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));

        let error = ProvisioningError::new(
            ProvisioningErrorKind::AuthenticationFailed,
            "the credential was rejected",
        );
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("TEST_SECRET_SHOULD_NEVER_ESCAPE"));
    }

    /// Reads one of this crate's source files (test-only I/O).
    fn module_source(file: &str) -> String {
        std::fs::read_to_string(format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR")))
            .unwrap()
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn the_crate_can_never_perform_io_or_touch_secrets_letters() {
        // Non-test code of the whole crate: no filesystem, no environment,
        // no process, no network, no logging sinks, no raw secret buffers.
        for file in [
            "lib.rs",
            "errors.rs",
            "outcome.rs",
            "repository.rs",
            "storage.rs",
            "hosting.rs",
            "domain.rs",
        ] {
            let source = module_source(file);
            for forbidden in [
                "std::net",
                "TcpStream",
                "reqwest",
                "HttpClient",
                "std::fs",
                "std::env",
                "std::process",
                "println!",
                "eprintln!",
                "dbg!",
                "Vec<u8>",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{file} must not reference {forbidden}"
                );
            }
        }
    }

    #[test]
    fn provisioning_errors_are_a_family_apart_from_publishing() {
        // The only tolerated external import is the CredentialStore secrets
        // boundary; nothing from the publishing engine may appear.
        for file in ["lib.rs", "errors.rs", "outcome.rs"] {
            let source = module_source(file);
            assert!(
                !source.contains("photo_publisher_provider_contracts"),
                "{file} needs no contract import"
            );
        }
        for file in ["repository.rs", "storage.rs", "hosting.rs", "domain.rs"] {
            let source = module_source(file);
            let imports: Vec<&str> = source
                .lines()
                .filter(|line| line.contains("photo_publisher_provider_contracts"))
                .collect();
            assert_eq!(
                imports.len(),
                1,
                "{file} may import exactly one contract item"
            );
            assert!(
                imports[0].contains("CredentialStore"),
                "{file} may import only CredentialStore"
            );
            for forbidden in [
                "ProviderError",
                "RepositoryProvider",
                "StorageProvider",
                "HostingPublisher",
                "photo_publisher_integration",
                "publisher_app",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "{file} must not reference {forbidden}"
                );
            }
        }
    }

    #[test]
    fn publishing_layers_do_not_depend_on_the_provisioning_crate() {
        // No existing crate may depend on (or even name) the provisioning
        // contracts in this phase: Setup ≠ Provisioning ≠ Publishing.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for member in [
            "crates/contract-validator",
            "crates/publisher-app",
            "crates/publisher-cli",
            "crates/publisher-core",
            "crates/publisher-desktop",
            "crates/publisher-integration",
            "crates/publisher-pipeline",
            "crates/provider-contracts",
            "crates/provider-contract-tests",
            "crates/provider-github",
            "crates/provider-local",
            "crates/provider-r2",
            "crates/provider-vercel",
        ] {
            let manifest = std::fs::read_to_string(root.join(member).join("Cargo.toml")).unwrap();
            assert!(
                !manifest.contains("publisher-provisioning"),
                "{member} must not depend on publisher-provisioning"
            );
            let src = root.join(member).join("src");
            let mut stack = vec![src];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|ext| ext == "rs") {
                        let source = std::fs::read_to_string(&path).unwrap();
                        assert!(
                            !source.contains("publisher_provisioning"),
                            "{member} source must not reference publisher_provisioning"
                        );
                        assert!(
                            !source.contains("Provisioner"),
                            "{member} source must not reference a Provisioner trait"
                        );
                    }
                }
            }
        }
    }
}
