//! Granular, provider-neutral publication events.
//!
//! These events OBSERVE the existing saga; they never change it. An event is
//! emitted by the executors exactly around the same ledger/persist/provider
//! transitions that already existed, never influencing decisions, rollback,
//! retries, or ordering. Payloads carry only safe domain identifiers
//! (object keys, repository paths, counts, generation, commit revision,
//! deployment identity) — never credentials, HTTP details, file contents,
//! or local filesystem paths.
//!
//! Storage is object-oriented; repository publishes as one commit batch;
//! hosting publishes as one deployment. There are deliberately no per-byte,
//! per-chunk, per-poll, or per-HTTP-call events: consumers see the real,
//! semantic units of publication only.

use photo_publisher_provider_contracts::{ObjectKey, RepositoryPath};

/// Structured, safe failure category attached to `*Failed` events.
///
/// This is a deliberately small, stable vocabulary mirroring the executor
/// error families. The detailed message remains the responsibility of
/// `ApplicationError`; events carry only structured progress context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationFailure {
    /// Local validation failed before any remote effect (invalid or missing
    /// source file, fingerprint mismatch, unresolvable output directory).
    LocalValidation,
    /// The operation was deterministically rejected without a remote effect;
    /// the previous ledger state was restored and persisted.
    Rejected,
    /// The remote outcome cannot be determined; `Unknown` was persisted and
    /// execution stopped without retrying.
    Ambiguous,
    /// The plan or a provider result contradicts the expected state and
    /// execution stopped without committing anything new.
    Inconsistent,
    /// The durable ledger could not be persisted; the durable file remains
    /// authoritative.
    Ledger,
}

/// One granular publication event observed during plan execution.
///
/// `*`Started marks the beginning of an execution attempt for the operation
/// (never a completion). `*`Finished is emitted only after the success
/// transition has been durably persisted. `*`Failed carries the structured
/// failure category produced by the executor. An operation that ends
/// ambiguous is reported as `*Failed` with [`OperationFailure::Ambiguous`],
/// never as Finished.
///
/// Events follow exactly the deterministic order of the plan
/// (BTreeMap-ordered inventories, plan order, single repository batch,
/// sequential execution, synchronous emission).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationEvent {
    /// A storage PUT attempt started for this object.
    StoragePutStarted {
        key: ObjectKey,
        size_bytes: u64,
        generation: String,
        /// 1-based position among this run's storage operations.
        index: usize,
        total: usize,
    },
    /// The storage PUT was confirmed and persisted.
    StoragePutFinished {
        key: ObjectKey,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    /// The storage PUT failed with the given structured category.
    StoragePutFailed {
        key: ObjectKey,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
        failure: OperationFailure,
    },
    /// A storage DELETE attempt started for this object.
    StorageDeleteStarted {
        key: ObjectKey,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    /// The storage DELETE removal was persisted (idempotent success).
    StorageDeleteFinished {
        key: ObjectKey,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
    },
    /// The storage DELETE failed with the given structured category.
    StorageDeleteFailed {
        key: ObjectKey,
        size_bytes: u64,
        generation: String,
        index: usize,
        total: usize,
        failure: OperationFailure,
    },
    /// The repository batch (one commit) attempt started.
    RepositoryBatchStarted {
        generation: String,
        writes: usize,
        deletes: usize,
        /// Every planned path, in plan order.
        paths: Vec<RepositoryPath>,
    },
    /// The commit publishing the whole batch was confirmed and persisted.
    RepositoryBatchFinished {
        generation: String,
        writes: usize,
        deletes: usize,
        /// Real commit revision that published the batch.
        revision: Option<String>,
    },
    /// The batch failed with the given structured category.
    RepositoryBatchFailed {
        generation: String,
        writes: usize,
        deletes: usize,
        failure: OperationFailure,
    },
    /// The hosting publication attempt started.
    HostingPublishStarted {
        generation: String,
        bundle_fingerprint: String,
    },
    /// The deployment was created with a verifiable identity and persisted.
    HostingPublishFinished {
        generation: String,
        deployment_id: String,
        url: String,
    },
    /// The hosting publication failed with the given structured category.
    HostingPublishFailed {
        generation: String,
        failure: OperationFailure,
    },
}

/// Observer of granular integration events: a plain synchronous mutable
/// callback. Consumers without interest simply pass a no-op closure; no bus,
/// channel, singleton, or framework is involved.
pub type EventObserver<'a> = dyn FnMut(IntegrationEvent) + 'a;
