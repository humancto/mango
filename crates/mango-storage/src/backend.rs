//! Storage-backend and Raft-log-store traits frozen in
//! `.planning/adr/0002-storage-engine.md` §6.
//!
//! Two traits: [`Backend`] is the user-data KV; [`RaftLogStore`] is
//! the Raft log. Separate because their hot paths differ (log is
//! append-and-truncate; KV is range-and-point-and-batch-commit) and
//! because they may be backed by different engines in Phase 10+
//! multi-Raft configurations. No engine types (`redb::*`,
//! `raft_engine::*`) leak through these traits — that is ADR 0002
//! §6 design point 1.
//!
//! # Example
//!
//! ```no_run
//! use mango_storage::{Backend, BackendConfig, BackendError, BucketId};
//! # fn example<B: Backend>() -> Result<(), BackendError> {
//! let cfg = BackendConfig::new("/tmp/mango".into(), false);
//! let backend = B::open(cfg)?;
//! backend.register_bucket("kv", BucketId::new(1))?;
//! let _snap = backend.snapshot()?;
//! # Ok(()) }
//! ```

#![deny(rustdoc::broken_intra_doc_links)]

use core::future::Future;

use bytes::Bytes;

/// Identifies a namespaced keyspace inside a [`Backend`]. Maps to a
/// redb `Table`, a bbolt bucket, a heed `Database`, or an LMDB named
/// sub-db depending on the backing engine. Allocation of specific
/// `u16` values is the impl's responsibility and SHOULD be
/// centralized in a single module in `mango-mvcc` when that crate
/// lands; mango-storage is agnostic.
///
/// `#[non_exhaustive]` keeps the door open for a future `(u16, u8)`
/// shape (e.g. generation tag) without a breaking change. Use
/// [`BucketId::new`] to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct BucketId {
    /// The raw `u16` identifier. `pub` for read access (logging,
    /// `Debug`, serialization); `#[non_exhaustive]` on the enclosing
    /// struct blocks literal-construction from outside the crate.
    pub raw: u16,
}

impl BucketId {
    /// Construct a [`BucketId`] from a `u16`. `const` so callers can
    /// declare bucket tables in `const` context.
    #[must_use]
    pub const fn new(id: u16) -> Self {
        Self { raw: id }
    }
}

/// Errors returned by [`Backend`], [`ReadSnapshot`], [`WriteBatch`],
/// and [`RaftLogStore`] methods. `#[non_exhaustive]` per workspace
/// policy (`docs/api-stability.md`); new variants can be added in a
/// minor version without breaking downstream match expressions.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// I/O error from the underlying engine or OS.
    #[error("storage I/O: {0}")]
    Io(#[from] std::io::Error),

    /// The backing store reported data corruption (checksum
    /// mismatch, torn write detected by the engine, etc.).
    #[error("storage corruption: {0}")]
    Corruption(String),

    /// A [`BucketId`] was used that was never registered via
    /// [`Backend::register_bucket`]. Separate from a data-absent
    /// condition because it is a programming error.
    #[error("bucket {0:?} not registered")]
    UnknownBucket(BucketId),

    /// Range bounds supplied to [`ReadSnapshot::range`] or
    /// [`WriteBatch::delete_range`] are invalid (`start > end`, for
    /// example).
    #[error("invalid range: {0}")]
    InvalidRange(&'static str),

    /// The backend was closed (or is being closed) and new work
    /// cannot be accepted.
    #[error("backend closed")]
    Closed,

    /// A [`Backend::register_bucket`] call tried to bind an id that
    /// is already bound to a different name (or bind a name to a
    /// different id). Structured rather than stringly-typed so
    /// operators can route on the fields.
    #[error("bucket id {id:?} already bound to {existing:?}; cannot rebind to {requested:?}")]
    BucketConflict {
        /// The [`BucketId`] being registered.
        id: BucketId,
        /// The name currently bound to `id`.
        existing: String,
        /// The name the caller supplied.
        requested: String,
    },

    /// Any other engine-defined error, wrapped as a string so
    /// engine types do not leak through the trait per ADR 0002 §6
    /// design point 1. Prefer a structured variant where possible.
    #[error("backend: {0}")]
    Other(String),
}

/// Opaque, monotonic durable-commit cursor. Returned by
/// [`Backend::commit_batch`] and [`Backend::commit_group`]. The
/// `seq` field is impl-defined; callers MUST treat it as an opaque
/// comparable handle and SHOULD NOT decode it. `#[non_exhaustive]`
/// so impls can carry additional metadata (e.g. a durability
/// timestamp) without a breaking change.
///
/// `Ord` because commit cursors are monotonically non-decreasing —
/// downstream code needs to compare `stamp_a < stamp_b` to check
/// durability ordering. `#[must_use]` because silently dropping a
/// returned cursor is a Raft correctness bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[must_use]
#[non_exhaustive]
pub struct CommitStamp {
    /// Impl-defined monotonically-non-decreasing commit sequence.
    pub seq: u64,
}

impl CommitStamp {
    /// Construct a [`CommitStamp`] with the given sequence.
    /// Primarily for impl crates — application code receives stamps
    /// from commit methods and does not construct them.
    pub const fn new(seq: u64) -> Self {
        Self { seq }
    }
}

/// Configuration for [`Backend::open`]. Engine-specific knobs belong
/// on the impl type, not here; this struct carries only the
/// portable configuration every backend needs. `#[non_exhaustive]`
/// for forward-compatibility — construct via [`BackendConfig::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BackendConfig {
    /// Filesystem path of the backend's data directory. Impls that
    /// are in-memory ignore this.
    pub data_dir: std::path::PathBuf,
    /// If `true`, open the backend read-only. Write methods MUST
    /// return [`BackendError::Closed`]-equivalent errors. Not yet
    /// fully specified; impls MAY return `Other("read-only not
    /// supported")` until the MVCC layer lands. Kept on the struct
    /// so the API is frozen now.
    pub read_only: bool,
}

impl BackendConfig {
    /// Construct a [`BackendConfig`]. Required because
    /// `#[non_exhaustive]` blocks struct-literal construction from
    /// outside the defining crate.
    #[must_use]
    pub fn new(data_dir: std::path::PathBuf, read_only: bool) -> Self {
        Self {
            data_dir,
            read_only,
        }
    }
}

/// Point-in-time read snapshot of a [`Backend`]. Implementations
/// MUST provide snapshot isolation: iterators and `get` calls
/// against the same `Self` observe a consistent view even if
/// commits land concurrently. `Send + Sync` so a snapshot can be
/// moved across tasks and shared between reader threads.
pub trait ReadSnapshot: Send + Sync {
    /// Point lookup. Returns `Ok(None)` if the key is absent from
    /// the bucket at the snapshot's cut.
    ///
    /// # Errors
    /// Returns [`BackendError::UnknownBucket`] if `bucket` was never
    /// registered; [`BackendError::Io`] on engine-level I/O error.
    fn get(&self, bucket: BucketId, key: &[u8]) -> Result<Option<Bytes>, BackendError>;

    /// Forward range iterator over the half-open interval
    /// `[start, end)`. Lifetime-parameterized trait object so the
    /// iterator borrows from `&self` — no per-item heap allocation.
    ///
    /// The trait-object `Box` is one allocation per range call and
    /// is intentional: it lets engine-specific iterator types stay
    /// hidden behind the trait (ADR 0002 §6 design point 1) without
    /// requiring a GAT on `ReadSnapshot` (which would block trait
    /// objects).
    ///
    /// # Errors
    /// Returns [`BackendError::UnknownBucket`] or
    /// [`BackendError::InvalidRange`] on caller errors; other
    /// variants on engine-level faults.
    fn range<'a>(
        &'a self,
        bucket: BucketId,
        start: &'a [u8],
        end: &'a [u8],
    ) -> Result<Box<dyn RangeIter<'a> + 'a>, BackendError>;
}

/// Iterator yielded by [`ReadSnapshot::range`]. Lifetime is the
/// snapshot's; items are `(key, value)` pairs as `Bytes` (cheap to
/// clone). `Send` so an iterator can be handed to a rayon worker.
pub trait RangeIter<'a>: Iterator<Item = Result<(Bytes, Bytes), BackendError>> + Send {}

/// Builder-style write batch. NOT `Send` by design: a batch is
/// thread-local by construction (single writer per batch; multiple
/// concurrent batches are orchestrated via
/// [`Backend::commit_group`]). Impls are free to make this type
/// `!Send` to enforce the invariant statically.
pub trait WriteBatch {
    /// Insert-or-overwrite.
    ///
    /// # Errors
    /// [`BackendError::UnknownBucket`] if `bucket` is not
    /// registered; [`BackendError::Closed`] after the owning
    /// backend is closed.
    fn put(&mut self, bucket: BucketId, key: &[u8], value: &[u8]) -> Result<(), BackendError>;

    /// Remove a single key. No-op if the key is absent.
    ///
    /// # Errors
    /// Same as [`Self::put`].
    fn delete(&mut self, bucket: BucketId, key: &[u8]) -> Result<(), BackendError>;

    /// Remove every key in the half-open interval `[start, end)`.
    ///
    /// # Errors
    /// [`BackendError::InvalidRange`] if `start > end`; otherwise
    /// same as [`Self::put`].
    fn delete_range(
        &mut self,
        bucket: BucketId,
        start: &[u8],
        end: &[u8],
    ) -> Result<(), BackendError>;
}

/// The user-data storage backend. Single-writer / multi-reader MVCC,
/// matching etcd's batch-tx-over-bbolt semantics.
///
/// All write methods return `impl Future<..> + Send` rather than
/// being `async fn`. This is a deliberate desugaring: under native
/// async-fn-in-trait (stable since 1.75), `async fn` does NOT
/// advertise `Send` on the returned future, which breaks usage from
/// multi-threaded tokio runtimes without either `#[async_trait]`
/// (boxes every future — banned by ADR 0002 §6 design point 5) or
/// `#[trait_variant::make(Send)]` (adds a dep). Explicit
/// `impl Future<..> + Send` in the signature is the zero-dep,
/// zero-allocation form.
///
/// # Object safety
///
/// `Backend` is intentionally NOT object-safe: [`Self::open`] has
/// `where Self: Sized`, the associated types flow through the
/// signatures, and the `-> impl Future<..> + Send` returns require
/// generic monomorphization. Use `impl Backend` or a generic
/// `T: Backend` bound; do not try `dyn Backend`.
///
/// # Associated-type design notes
///
/// [`Self::Batch`] is `'static` by default — the trait does not
/// parameterize it with a lifetime (no GAT). Impls that need to
/// hold a reference to the backend (e.g. `redb::WriteTransaction<'db>`)
/// MUST wrap it via `Arc<Database>` (or equivalent) rather than a
/// borrow. This is the explicit cost of keeping `Backend` simple for
/// future adapter types.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a mango storage `Backend`",
    note = "see `.planning/adr/0002-storage-engine.md` §6 for the contract"
)]
pub trait Backend: Send + Sync + 'static {
    /// Point-in-time read snapshot type. See [`ReadSnapshot`].
    type Snapshot: ReadSnapshot + 'static;

    /// Write-batch type. See [`WriteBatch`]. `!Send` is permitted;
    /// `'static` is required (no lifetime parameter on the
    /// associated type — see the "Associated-type design notes"
    /// section).
    type Batch: WriteBatch + 'static;

    /// Register a named bucket with the given [`BucketId`]. MUST be
    /// called before any read or write targets that bucket.
    /// Idempotent across opens: repeated registration of the same
    /// (name, id) is a no-op; re-binding the same id to a different
    /// name MUST return [`BackendError::BucketConflict`].
    ///
    /// # Errors
    /// Returns [`BackendError::BucketConflict`] on a name/id rebind
    /// collision; [`BackendError::Closed`] if the backend is
    /// closed.
    fn register_bucket(&self, name: &str, id: BucketId) -> Result<(), BackendError>;

    /// Acquire a read snapshot. Cheap — no I/O.
    ///
    /// # Errors
    /// [`BackendError::Closed`] after [`Self::close`].
    fn snapshot(&self) -> Result<Self::Snapshot, BackendError>;

    /// Start a new write batch. Cheap — no I/O.
    ///
    /// # Errors
    /// [`BackendError::Closed`] after [`Self::close`].
    fn begin_batch(&self) -> Result<Self::Batch, BackendError>;

    /// Commit a single batch. If `force_fsync` is `true`, the impl
    /// MUST call `fsync` before returning; otherwise, the impl MAY
    /// coalesce the fsync with other in-flight batches (see
    /// [`Self::commit_group`]).
    fn commit_batch(
        &self,
        batch: Self::Batch,
        force_fsync: bool,
    ) -> impl Future<Output = Result<CommitStamp, BackendError>> + Send;

    /// Atomically commit multiple batches with a single fsync. The
    /// fsync-batching primitive for Raft (ADR 0002 §6 design point
    /// 4; verification H4). All batches succeed or none do; the
    /// returned [`CommitStamp`] is the cursor after the group.
    fn commit_group(
        &self,
        batches: Vec<Self::Batch>,
    ) -> impl Future<Output = Result<CommitStamp, BackendError>> + Send;

    /// Open the backend against `config`. Synchronous by design —
    /// open runs once per process and is not on any hot path.
    ///
    /// # Errors
    /// Returns [`BackendError::Io`] if the data directory cannot be
    /// opened; [`BackendError::Corruption`] if the engine detects
    /// on-disk damage at open time.
    fn open(config: BackendConfig) -> Result<Self, BackendError>
    where
        Self: Sized;

    /// Close the backend. After the first successful call, further
    /// read/write methods MUST return [`BackendError::Closed`]. This
    /// is idempotent: repeated calls after the first return `Ok(())`.
    ///
    /// Takes `&self` (not `self`) so the backend can be wrapped in
    /// `Arc<B>` and shared across tasks — the closer does not need
    /// unique ownership. Impls use interior state (an atomic `closed`
    /// flag) to enforce idempotence. `Drop` on the last handle
    /// remains a safety net that performs the engine's default
    /// shutdown; prefer explicit `close` for deterministic
    /// last-fsync and error return.
    fn close(&self) -> Result<(), BackendError>;

    /// On-disk size in bytes. Advisory; may lag actual disk usage
    /// by an engine-defined amount.
    fn size_on_disk(&self) -> Result<u64, BackendError>;

    /// Compact and reclaim space. Engine-specific operation; no
    /// timing or blast-radius guarantees at the trait level.
    fn defragment(&self) -> impl Future<Output = Result<(), BackendError>> + Send;
}

/// A single Raft log entry, engine-neutral. Fields mirror the Raft
/// protocol's `Entry` without pulling a `raft-proto` dep — impls
/// convert at the boundary (`raft-engine` impl in ROADMAP:818 does
/// the conversion locally).
///
/// `#[must_use]` — silently discarding a log entry is a Raft
/// correctness bug.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub struct RaftEntry {
    /// Log index (1-origin per the Raft protocol; 0 is reserved).
    pub index: u64,
    /// Raft term at which this entry was proposed.
    pub term: u64,
    /// Entry category. See [`RaftEntryType`].
    pub entry_type: RaftEntryType,
    /// Opaque payload. For normal entries this is the
    /// application-level proposal bytes.
    pub data: Bytes,
    /// Optional opaque context the raft-rs layer threads through
    /// for `ProposalContext` tracking. Empty for most entries.
    pub context: Bytes,
}

impl RaftEntry {
    /// Construct a [`RaftEntry`]. Required because `#[non_exhaustive]`
    /// blocks struct-literal construction from outside the defining
    /// crate, and ROADMAP:818 (`mango-raft`) needs to convert
    /// `raft::prelude::Entry` → `RaftEntry` at the trait boundary.
    pub fn new(
        index: u64,
        term: u64,
        entry_type: RaftEntryType,
        data: Bytes,
        context: Bytes,
    ) -> Self {
        Self {
            index,
            term,
            entry_type,
            data,
            context,
        }
    }
}

/// Raft entry classification. Mirrors `raft-proto`'s `EntryType`.
/// Kept minimal — mango does not use `EntryConfChangeV1` at the
/// storage layer; config changes ride as normal entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RaftEntryType {
    /// Normal user-data entry. The `data` field is the application
    /// proposal; `mango-mvcc` decodes it.
    Normal,
    /// Raft conf-change entry (joint consensus). Payload is
    /// protocol-level `ConfChangeV2` bytes; `mango-raft` decodes it.
    ConfChange,
}

/// Durable Raft hard state. Frozen across restarts; every update
/// goes through [`RaftLogStore::save_hard_state`]. `#[must_use]`
/// because discarding a freshly-read hard state is a Raft
/// correctness bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
#[non_exhaustive]
pub struct HardState {
    /// Latest term the server has seen.
    pub term: u64,
    /// Candidate that received this server's vote in the current
    /// term, or 0 if none.
    pub vote: u64,
    /// Highest log entry known to be committed.
    pub commit: u64,
}

impl HardState {
    /// Construct a [`HardState`]. Required because `#[non_exhaustive]`
    /// blocks struct-literal construction from outside the defining
    /// crate, and ROADMAP:818 needs to convert `raft::HardState`
    /// → `HardState`.
    pub const fn new(term: u64, vote: u64, commit: u64) -> Self {
        Self { term, vote, commit }
    }
}

/// Metadata for a snapshot installed on a follower. Payload is
/// managed out-of-band by the snapshot-transfer layer; this struct
/// is only the cursor the log store keeps. `#[must_use]` because
/// dropping a snapshot cursor on the floor is a correctness bug.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub struct RaftSnapshotMetadata {
    /// Log index the snapshot covers.
    pub index: u64,
    /// Raft term at `index`.
    pub term: u64,
    /// Encoded `ConfState` bytes at `index`. Empty if no
    /// configuration change has been applied yet.
    pub conf_state: Bytes,
}

impl RaftSnapshotMetadata {
    /// Construct a [`RaftSnapshotMetadata`]. Required because
    /// `#[non_exhaustive]` blocks struct-literal construction from
    /// outside the defining crate, and ROADMAP:818 needs to convert
    /// `raft::SnapshotMetadata` → `RaftSnapshotMetadata`.
    pub fn new(index: u64, term: u64, conf_state: Bytes) -> Self {
        Self {
            index,
            term,
            conf_state,
        }
    }
}

/// The Raft log store. Append-only on the write path; engine-
/// specific truncation and compaction. Separate from [`Backend`]
/// per ADR 0002 §6 (different hot paths, potentially different
/// engines).
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a mango `RaftLogStore`",
    note = "see `.planning/adr/0002-storage-engine.md` §6 for the contract"
)]
pub trait RaftLogStore: Send + Sync + 'static {
    /// Append `entries` to the log. `entries[0].index` MUST be
    /// exactly `last_index() + 1`; otherwise the impl MUST return
    /// `BackendError::Other` (strict consecutivity is a Raft
    /// invariant the storage layer enforces).
    fn append(
        &self,
        entries: &[RaftEntry],
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Read a half-open range `[low, high)`. `low` MUST be
    /// `>= first_index()` and `high` MUST be `<= last_index() + 1`;
    /// otherwise returns [`BackendError::InvalidRange`].
    ///
    /// # Errors
    /// [`BackendError::InvalidRange`] on out-of-bounds access.
    fn entries(&self, low: u64, high: u64) -> Result<Vec<RaftEntry>, BackendError>;

    /// Highest log index present in the store. After a fresh open
    /// with no snapshot, returns `0`.
    ///
    /// # Errors
    /// [`BackendError::Io`] on engine-level I/O error.
    fn last_index(&self) -> Result<u64, BackendError>;

    /// Lowest log index present in the store. After
    /// [`Self::compact`] at index `N`, returns `N + 1`.
    ///
    /// # Errors
    /// [`BackendError::Io`] on engine-level I/O error.
    fn first_index(&self) -> Result<u64, BackendError>;

    /// Discard all entries strictly before `idx`. Post-condition:
    /// `first_index() == idx` (if `idx <= last_index()`) or no
    /// change (if `idx > last_index()`). Impls MAY truncate lazily.
    fn compact(&self, idx: u64) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Record that a snapshot covering `snapshot.index` has been
    /// installed. Callers handle snapshot payload transfer; this
    /// method only persists the cursor.
    ///
    /// Post-conditions after a successful return:
    /// - [`Self::first_index`] returns `snapshot.index + 1`.
    /// - [`Self::last_index`] returns at least `snapshot.index`.
    /// - Entries strictly before `snapshot.index + 1` MAY be
    ///   discarded by the impl. Downstream callers MUST NOT assume
    ///   their presence.
    ///
    /// This matches raft-rs's `Storage::snapshot` contract
    /// (`MemStorage::apply_snapshot` implements the same invariant).
    fn install_snapshot(
        &self,
        snapshot: &RaftSnapshotMetadata,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Persist a new [`HardState`]. Durably committed before
    /// return.
    fn save_hard_state(
        &self,
        hs: &HardState,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Read the persisted hard state. Returns `HardState::default()`
    /// on a fresh store.
    ///
    /// # Errors
    /// [`BackendError::Io`] on engine-level I/O error;
    /// [`BackendError::Corruption`] on decode failure.
    fn hard_state(&self) -> Result<HardState, BackendError>;
}
