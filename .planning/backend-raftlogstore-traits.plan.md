# PR-2: `Backend` + `RaftLogStore` trait pair (ROADMAP:816)

## Goal

Define the two load-bearing storage traits frozen in
`.planning/adr/0002-storage-engine.md` §6, in a new module
`crates/mango-storage/src/backend.rs`, and re-export the public surface
from `lib.rs`. **Trait definitions only.** No implementations —
ROADMAP:817 (`Backend` against `redb`) and ROADMAP:818 (`RaftLogStore`
against `raft-engine`) are separate PRs.

The ADR sketch is the contract; this PR lands it verbatim with two
desugaring decisions explicitly called out below and a handful of
supporting types the ADR names but does not define.

## Non-goals

- No `redb` or `raft-engine` use. The trait file does NOT import either
  crate. `Cargo.toml` keeps `redb.workspace = true` and
  `raft-engine.workspace = true` (already in the skeleton) so downstream
  impl PRs don't have to re-add them, but `backend.rs` is engine-free.
- No mock / in-memory impl (Phase 2 may land one for tests; not this PR).
- No conversion helpers to/from `raft-proto` or `raft-rs` types.
- No snapshot-diff or compaction-state types beyond what the ADR sketch
  names.
- No `tokio`, `async_trait`, or `trait_variant` deps. AFIT (native async
  fn in trait, stable since Rust 1.75; MSRV is 1.89) is sufficient.

## Scope — exact file list

1. **`crates/mango-storage/src/backend.rs`** (new) — the whole trait
   surface and the supporting types. ~280 lines with rustdoc
   (revised up from ~220 after rust-expert review added five `fn
new` constructors, a `BucketConflict` error variant, and
   `#[diagnostic::on_unimplemented]` attributes).

2. **`crates/mango-storage/src/lib.rs`** (edit) — add
   `pub mod backend;` and re-export the public names at crate root
   (`pub use backend::{Backend, BackendConfig, BackendError, BucketId,
CommitStamp, HardState, RaftEntry, RaftEntryType, RaftLogStore,
RaftSnapshotMetadata, RangeIter, ReadSnapshot, WriteBatch};`).
   The file-level doc comment gets one sentence mentioning the new
   module; the `VERSION` const and the `version_matches_cargo_manifest`
   test stay.

3. **`Cargo.toml`** (workspace root) — add `bytes = "1.11"` and
   `thiserror = "2"` to `[workspace.dependencies]`. Both crates already
   have `[[exemptions.*]]` entries in `supply-chain/config.toml` as
   transitives, so no vet churn. Rationale-comment block for each.

4. **`crates/mango-storage/Cargo.toml`** (edit) — add
   `bytes.workspace = true` and `thiserror.workspace = true` to
   `[dependencies]`.

5. **`unsafe-baseline.json`** (regenerate) — the new module is
   `unsafe`-free; the first-party entry for `mango-storage` stays all
   zeros. Regenerate anyway via `bash scripts/geiger-update-baseline.sh`
   so the timestamp matches the PR date and any incidental format
   diff surfaces in the diff.

No other files. No CI wiring changes. No ADR edits (the ADR is the
contract; if the trait diverges from it, the ADR must change first in
its own PR — out of scope here).

## Trait surface — final shape

This is the precise code that lands, with the ADR §6 sketch as the
source of truth.

````rust
// crates/mango-storage/src/backend.rs
#![allow(clippy::module_name_repetitions)]
#![deny(rustdoc::broken_intra_doc_links)]

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
//! let cfg = BackendConfig {
//!     data_dir: "/tmp/mango".into(),
//!     read_only: false,
//! };
//! let backend = B::open(cfg)?;
//! backend.register_bucket("kv", BucketId::new(1))?;
//! let snap = backend.snapshot()?;
//! # let _ = snap; Ok(()) }
//! ```

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
    /// [`Backend::register_bucket`]. Separate from `NotFound`
    /// because it is a programming error, not a data-absent
    /// condition.
    #[error("bucket {0:?} not registered")]
    UnknownBucket(BucketId),

    /// Range bounds supplied to [`ReadSnapshot::range`] or
    /// [`WriteBatch::delete_range`] are invalid (`start > end`,
    /// for example).
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
    #[error(
        "bucket id {id:?} already bound to {existing:?}; cannot rebind to {requested:?}"
    )]
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
    /// Construct a [`CommitStamp`] with the given sequence. Primarily
    /// for impl crates (`mango-storage` against redb, against
    /// `raft-engine`, etc.) — application code receives stamps from
    /// commit methods and does not construct them.
    #[must_use]
    pub const fn new(seq: u64) -> Self {
        Self { seq }
    }
}

/// Configuration for [`Backend::open`]. Engine-specific knobs belong
/// on the impl type, not here; this struct carries only the
/// portable configuration every backend needs. `#[non_exhaustive]`
/// for forward-compatibility.
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

/// Point-in-time read snapshot of a [`Backend`]. Implementations
/// MUST provide snapshot isolation: iterators and `get` calls
/// against the same `Self` observe a consistent view even if
/// commits land concurrently. `Send + Sync` so a snapshot can be
/// moved across tasks and shared between reader threads.
pub trait ReadSnapshot: Send + Sync {
    /// Point lookup. Returns `Ok(None)` if the key is absent from
    /// the bucket at the snapshot's cut.
    fn get(
        &self,
        bucket: BucketId,
        key: &[u8],
    ) -> Result<Option<Bytes>, BackendError>;

    /// Forward range iterator over the half-open interval
    /// `[start, end)`. Lifetime-parameterized trait object so the
    /// iterator borrows from `&self` — no per-item heap allocation.
    ///
    /// The trait-object `Box` is one allocation per range call and
    /// is intentional: it lets engine-specific iterator types stay
    /// hidden behind the trait (ADR 0002 §6 design point 1) without
    /// requiring a GAT on `ReadSnapshot` (which would block trait
    /// objects). See §"Desugaring decisions" in the plan.
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
pub trait RangeIter<'a>:
    Iterator<Item = Result<(Bytes, Bytes), BackendError>> + Send
{
}

/// Builder-style write batch. NOT `Send` by design: a batch is
/// thread-local by construction (single writer per batch; multiple
/// concurrent batches are orchestrated via
/// [`Backend::commit_group`]). Impls are free to make this type
/// `!Send` to enforce the invariant statically.
pub trait WriteBatch {
    /// Insert-or-overwrite.
    fn put(
        &mut self,
        bucket: BucketId,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), BackendError>;

    /// Remove a single key. No-op if the key is absent.
    fn delete(
        &mut self,
        bucket: BucketId,
        key: &[u8],
    ) -> Result<(), BackendError>;

    /// Remove every key in the half-open interval `[start, end)`.
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
    /// called before any read or write targets that bucket. Idempotent
    /// across opens: repeated registration of the same (name, id) is
    /// a no-op; re-binding the same id to a different name MUST
    /// return [`BackendError::BucketConflict`].
    fn register_bucket(
        &self,
        name: &str,
        id: BucketId,
    ) -> Result<(), BackendError>;

    /// Acquire a read snapshot. Cheap — no I/O.
    fn snapshot(&self) -> Result<Self::Snapshot, BackendError>;

    /// Start a new write batch. Cheap — no I/O.
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
    /// flag) to enforce idempotence. `Drop` on the last `Arc` handle
    /// remains a safety net that performs the engine's default
    /// shutdown; prefer explicit `close` for deterministic
    /// last-fsync and error return.
    fn close(&self) -> Result<(), BackendError>;

    /// On-disk size in bytes. Advisory; may lag actual disk usage
    /// by an engine-defined amount.
    fn size_on_disk(&self) -> Result<u64, BackendError>;

    /// Compact and reclaim space. Engine-specific operation; no
    /// timing or blast-radius guarantees at the trait level.
    fn defragment(
        &self,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
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
    /// for ProposalContext tracking. Empty for most entries.
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
    /// protocol-level ConfChangeV2 bytes; `mango-raft` decodes it.
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
    /// otherwise returns `BackendError::InvalidRange`.
    fn entries(
        &self,
        low: u64,
        high: u64,
    ) -> Result<Vec<RaftEntry>, BackendError>;

    /// Highest log index present in the store. After a fresh open
    /// with no snapshot, returns `0`.
    fn last_index(&self) -> Result<u64, BackendError>;

    /// Lowest log index present in the store. After
    /// [`Self::compact`] at index `N`, returns `N + 1`.
    fn first_index(&self) -> Result<u64, BackendError>;

    /// Discard all entries strictly before `idx`. Post-condition:
    /// `first_index() == idx` (if `idx <= last_index()`) or no
    /// change (if `idx > last_index()`). Impls MAY truncate lazily.
    fn compact(
        &self,
        idx: u64,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;

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
    fn hard_state(&self) -> Result<HardState, BackendError>;
}
````

## Desugaring decisions (flagged for rust-expert review)

These are the places where the ADR §6 sketch is a shape sketch rather
than literal code. Decisions and rationale:

1. **`async fn` → `impl Future<Output = ...> + Send`** on every
   async method. ADR §6 sketch uses `async fn`. Native AFIT (stable
   1.75) does not propagate `Send` on the returned future, which
   breaks all real multi-threaded tokio usage. Options and the pick:
   - `async_trait` macro — boxes every future. ADR 0002 §6 design
     point 5 forbids accidental allocation on the hot read path; boxing
     all read futures too is the kind of tax that's easy to defend
     against at the trait level. **Rejected.**
   - `trait_variant::make(Send)` — clean but adds a dep and a macro-
     expansion debug burden. **Rejected as trait-level sugar; may adopt
     later if the explicit-Future shape becomes painful at call sites.**
   - Explicit `fn … -> impl Future<…> + Send` — zero deps, zero
     allocation, matches the design point. **Picked.**
     The method-level contract (name, args, return type after
     desugaring) is identical to the ADR's `async fn` shape. If the
     reviewer prefers `async fn` + explicit `Send` callsite bounds, the
     swap is mechanical.

2. **`ReadSnapshot::range` returns `Box<dyn RangeIter<'a> + 'a>`
   verbatim** from the ADR. The alternative — a GAT
   `type RangeIter<'a>: …` on the `ReadSnapshot` trait — blocks
   `dyn ReadSnapshot` usage (GATs and dyn-trait don't compose). The
   `Box` is one allocation per range call, not per item; acceptable.
   ADR 0002 §6 explicitly chose this shape.

3. **`BackendError` as a `#[non_exhaustive]` enum with `thiserror`**.
   `thiserror 2` is in the transitive graph already (through madsim
   and indirectly through several others); the workspace already
   carries an exemption at `supply-chain/config.toml:610`. Alternative
   (hand-roll `Display` + `std::error::Error`) is ~30 lines of
   boilerplate per error enum and adds no engineering value.

4. **`CommitStamp.seq: u64` kept `pub`** per the ADR sketch, inside
   a `#[non_exhaustive]` struct. The `pub` means callers can READ
   `seq`, but `#[non_exhaustive]` means they CANNOT construct the
   struct outside the crate — exactly the shape we want (opaque for
   construction, readable for logging/comparison).

5. **Supporting types (`RaftEntry`, `RaftEntryType`, `HardState`,
   `RaftSnapshotMetadata`) mirror Raft protocol shape but do NOT
   import `raft-proto`**. Importing `raft-proto` would drag a
   `protobuf` dep into the storage trait crate, which is both
   cross-cutting (trait crate pulls a serialization format) and
   reintroduces the protobuf-2.x supply-chain scar we just exempted
   (RUSTSEC-2024-0437). Impl PRs convert at their boundary. The
   trait crate stays protocol-agnostic.

6. **`#[non_exhaustive]` on every pub enum and pub struct** per
   `docs/api-stability.md`. Workspace lint `exhaustive_enums = deny`
   catches enum cases; struct-level is a house-convention audit item.
   Every `#[non_exhaustive]` struct with at least one `pub` field
   ships a `pub [const] fn new(...)` constructor so the impl crate
   (`mango-storage` → redb in :817; `mango-raft` → raft-engine in
   :818) can synthesize values from foreign types. This applies to
   `BucketId::new`, `CommitStamp::new`, `RaftEntry::new`,
   `HardState::new`, and `RaftSnapshotMetadata::new`.

7. **`Backend::close` takes `&self`, not `self`.** ADR §6 sketch used
   `fn close(self)`, which is incompatible with `Arc<B>` sharing —
   the only way to move `self` out of an `Arc` is `Arc::into_inner`,
   which requires a refcount of 1. Impls use interior state (an
   atomic `closed` flag; subsequent methods return
   `BackendError::Closed`) to enforce idempotence. The "deterministic
   last-fsync" semantic the ADR reached for is preserved — callers
   still call `close` explicitly to commit the final fsync and
   receive the error code; `Drop` is the safety-net fallback.

8. **`Self::Batch: 'static` (no GAT).** Stated explicitly in the
   `Backend` trait rustdoc. Impls that need to hold a borrow of the
   database (e.g. `redb::WriteTransaction<'db>`) MUST wrap the
   database in `Arc<Database>` and take an `Arc` handle into the
   batch rather than a reference. A GAT on the associated type would
   tie the batch to a lifetime, but would also block adapter types
   that need `dyn Backend`-compatible wrappers. Trading one
   allocation at batch-begin time for trait-simplicity is the right
   call at this layer.

9. **`#[must_use]` on cursor / state types**
   (`CommitStamp`, `HardState`, `RaftEntry`, `RaftSnapshotMetadata`).
   Discarding any of these on the floor is a Raft or storage
   correctness bug; the attribute forces callers to bind or
   consciously `let _ = ...` them. `Result<T, _>` already carries
   `#[must_use]` at the outer level, but the inner attribute catches
   the destructured case (`let Ok(stamp) = …; drop(stamp);`).

10. **`#[diagnostic::on_unimplemented]` on both traits.** Stabilized
    in 1.78 (within MSRV 1.89). Points downstream implementors at
    the ADR file in the compile error rather than a bare
    "the trait bound is not satisfied." Zero runtime cost.

11. **`BackendError::BucketConflict { id, existing, requested }` as
    a structured variant** instead of stringly-typed
    `Other("bucket 1 already bound to foo...")`. Operators will want
    to route / alarm on bucket-conflict events; structured fields
    give them stable names. `#[non_exhaustive]` on the variant lets
    us add fields later without a breaking change.

## Edge cases and risks

1. **AFIT + `Send` desugaring in lint output.** The explicit
   `impl Future<…> + Send` form has a known clippy interaction in
   1.89 where `future_not_send` fires with a surprising note; we don't
   use that lint, so this is a no-op here, but a note for reviewers
   peeking at rustdoc output is that the generated trait docs will
   show the desugared `-> impl Future<…> + Send` signature, not the
   `async fn` one. Rustdoc renders both correctly — just different
   prose. Acceptable.

2. **`Bytes` in error paths.** `BackendError` does not carry `Bytes`
   today. If a future variant does (e.g., `KeyCollision { key:
Bytes }`), `thiserror`'s `#[from]` / `#[source]` integration still
   works — no current cost, noted for posterity.

3. **`Backend::open` as a trait method with `where Self: Sized`.**
   Makes `Backend` non-object-safe. That is fine: consumers want a
   concrete impl for the monomorphization benefits (zero virtual
   dispatch on commit_batch). The trait is object-safe for all OTHER
   methods, so a future need for `dyn Backend` can be served via an
   adapter that doesn't expose `open`.

4. **Symmetry with `RaftLogStore` open**. `RaftLogStore` intentionally
   does NOT have `open`/`close`/`config`. The Phase 1 impl in
   ROADMAP:818 will define a raft-engine-specific constructor on the
   impl type, not on the trait. Callers that want both a Backend and a
   RaftLogStore open one of each via their own `open`. This is
   consistent with ADR 0002 §6's emphasis that the trait surface is
   minimal — construction and config are engine-specific.

5. **`RaftEntry.data` as `Bytes` (vs `Vec<u8>`).** `Bytes` is
   ref-counted and cheap-to-clone; Raft log entries are passed across
   components (log → apply loop → state machine). `Vec<u8>` would
   force a clone at every handoff. `Bytes` is the right shape and
   already exempted in cargo-vet.

6. **`Bytes::new()` as the empty sentinel for `conf_state` / `context`.**
   `Bytes` zero-length is cheap and allocation-free. Tests will
   verify that `RaftEntry { …, context: Bytes::new() }` compiles and
   that comparing two empty `Bytes` is cheap. No runtime cost.

7. **cargo-public-api snapshotting.** Adding a new public module will
   register as ~30 new public API items. Since `cargo-public-api` is
   advisory pre-Phase-6 (ROADMAP note), the job will warn but not
   block. Reviewer should expect the diff to include an updated
   `public-api.txt` (if the workflow writes one) or accept the warn.
   Verified: the workflow is advisory (does not fail CI).

8. **`unsafe_code = "forbid"` + `#![deny(missing_docs)]` compliance.**
   Module has zero unsafe; every `pub` item has rustdoc. Unit tests
   in `mod tests` stay in `lib.rs`; `backend.rs` has no `#[cfg(test)]`
   block (keeps the surface clean — trait-only crate module).

9. **Clippy `pedantic` warnings.** `module_name_repetitions` is
   allowed workspace-wide; other pedantic lints (e.g.,
   `needless_lifetimes`) should not fire on the shape above but I
   will run clippy with `--all-targets -- -D warnings` before pushing.

10. **MSRV 1.89 check.** AFIT is stable since 1.75;
    `impl Future<…> + Send` return types are stable since 1.75 as
    well. No 1.90+ features in use. MSRV job should pass.

## Test strategy

Trait-only PR; tests verify the traits actually compile and are
object-/impl-compatible in the expected shapes. Tests live in
`crates/mango-storage/src/lib.rs` under `mod tests` (where the
version smoke already lives):

1. **`trait_shape_compiles`** — static-assertion functions plus a
   `const _: () = ...` block:

   ```rust
   fn _assert_read_snapshot_object_safe(_: &dyn ReadSnapshot) {}
   fn _assert_range_iter_send<'a>(_: Box<dyn RangeIter<'a> + 'a>)
   where
       Box<dyn RangeIter<'a> + 'a>: Send,
   {}
   fn _assert_backend_send_sync_static<T: Backend>() {
       fn needs<T: Send + Sync + 'static>() {}
       needs::<T>();
   }
   fn _assert_raft_log_store_send_sync_static<T: RaftLogStore>() {
       fn needs<T: Send + Sync + 'static>() {}
       needs::<T>();
   }
   ```

   Locks in: `ReadSnapshot` is dyn-compatible; `Box<dyn RangeIter>`
   carries the `Send` bound through the supertrait; `Backend` and
   `RaftLogStore` generic bounds match the declared super-traits.

2. **`error_display_covers_every_variant`** — constructs one of each
   `BackendError` variant and asserts `format!("{e}")` is non-empty
   and contains the expected substring. Catches silent `#[error]`
   drops on variant rename.

3. **`backend_error_io_source_chain`** — builds a
   `BackendError::Io(io::Error::other("x"))` and asserts that
   `std::error::Error::source()` returns `Some(_)` whose Display
   mentions `"x"`. Guards against a future refactor that drops
   `#[from]` / `#[source]` and silently breaks error chains.

4. **`hard_state_default_is_zero`** — `HardState::default()` returns
   `{ term: 0, vote: 0, commit: 0 }`. Contract with raft-rs.

5. **`commit_stamp_is_copy_eq_ord`** —

   ```rust
   let a = CommitStamp::new(1);
   let b = CommitStamp::new(2);
   assert_eq!(a, CommitStamp::new(1));
   assert!(a < b);
   let _ = a; // `Copy`; original still usable.
   assert_eq!(a.seq, 1);
   ```

   Catches accidental removal of `Copy`, `PartialOrd`, or `Ord`.

6. **`raft_entry_type_is_non_exhaustive`** — compile-fenced assertion
   that `match RaftEntryType::Normal { Normal => …, ConfChange => …, _
=> … }` uses the wildcard arm. This is the `exhaustive_enums` lint's
   contract; test form:

   ```rust
   #[allow(clippy::wildcard_enum_match_arm, unreachable_patterns)]
   fn _non_exhaustive_match(t: RaftEntryType) -> &'static str {
       match t {
           RaftEntryType::Normal => "n",
           RaftEntryType::ConfChange => "c",
           _ => "future",
       }
   }
   ```

   `#[allow(unreachable_patterns)]` is belt-and-suspenders: inside
   the defining crate, the `_` arm is technically unreachable at
   compile time, and `rustc_lint::unreachable_patterns` is warn-by-
   default — the allow keeps the file clean without
   `#[allow(clippy::wildcard_enum_match_arm)]` drift.

7. **`bucket_id_constructor_and_non_exhaustive`** — asserts
   `BucketId::new(7).raw == 7` and that `const` construction
   compiles:
   ```rust
   const _BUCKET_KV: BucketId = BucketId::new(1);
   ```
   The `const` context is load-bearing — downstream crates will want
   to declare bucket ids as `const`s.

Not tested in this PR (scoped out):

- Actual I/O behavior (no impl exists yet).
- `commit_group` atomicity (no impl).
- Trait-object usage at runtime (`dyn ReadSnapshot` — the type-level
  assertion covers object-safety; an integration test needs an impl).

Local gates rerun before PR:

- `cargo check --workspace --all-features` — compiles.
- `cargo nextest run -p mango-storage` — 8 tests pass (1 existing +
  7 new).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --check` — clean.
- `cargo doc --no-deps --document-private-items` — clean; both
  `missing_docs` and `rustdoc::broken_intra_doc_links` gates pass.
- `cargo deny check` — unchanged (no new crates at the bans/sources
  level).
- `cargo vet check` — unchanged.
- `cargo run -q -p xtask-vet-ttl` — unchanged.
- `bash scripts/geiger-check.sh` — unchanged.
- `bash scripts/non-exhaustive-check.sh` — passes (all new pub enums
  carry `#[non_exhaustive]`).
- `cargo test --doc -p mango-storage` — one module-level doctest
  compiles and runs under `no_run` (shape-only; confirms the public
  API is usable as advertised).
- `cargo public-api --package mango-storage > public-api.txt` —
  commit the initial baseline so :817's redb impl can diff against
  it.

## Commit plan

**Single atomic commit.** Each part of the change is useless without
the others: adding `bytes`/`thiserror` without using them is warning-
noise; adding `backend.rs` without the `Cargo.toml` deps fails to
compile. Three commits would only aid bisect at the cost of non-
compiling intermediate states. Prior art: PR #49 shipped as one
commit + two follow-ups for rust-expert REVISE items — pattern to
match.

Commit message shape:

```
feat(storage): define Backend + RaftLogStore trait pair (ADR 0002 §6)

Lands ROADMAP:816: the two storage traits frozen in ADR 0002 §6.
Trait definitions only — impls follow in ROADMAP:817 (Backend
against redb) and ROADMAP:818 (RaftLogStore against raft-engine).

- crates/mango-storage/src/backend.rs: new module. Defines
  Backend + ReadSnapshot + RangeIter + WriteBatch + RaftLogStore
  and the supporting types (BucketId, BackendConfig, BackendError,
  CommitStamp, RaftEntry, RaftEntryType, HardState,
  RaftSnapshotMetadata). ~220 LoC with full rustdoc.
- crates/mango-storage/src/lib.rs: pub mod + pub use re-exports;
  adds 7 compile-time shape tests to the existing tests module.
- Cargo.toml (workspace): adds bytes = "1.11" and thiserror = "2"
  to [workspace.dependencies]. Both are already in the transitive
  graph with cargo-vet exemptions; no new supply-chain churn.
- crates/mango-storage/Cargo.toml: declares .workspace = true on
  both.
- unsafe-baseline.json: regenerated; mango-storage stays all-zero.

Desugaring decisions (all flagged in the plan and reviewed by
rust-expert pre-implementation):
- async fn in trait → impl Future<..> + Send (ADR 0002 §6 design
  point 5: no accidental allocation; async_trait boxes all futures
  and is banned).
- ReadSnapshot::range keeps Box<dyn RangeIter<'a> + 'a> from the
  ADR sketch — a GAT would block dyn ReadSnapshot.
- Raft protocol types (RaftEntry, HardState, etc.) are mango-
  native and do NOT import raft-proto — avoids dragging protobuf
  into the trait crate.
- Backend::close takes &self (idempotent) rather than self-by-
  value, so the backend can be wrapped in Arc<B> without blocking
  the close path. ADR sketch used `fn close(self)`; desugared here.
- Self::Batch: 'static (no GAT on the associated type). Impls that
  need to hold a borrow to the database wrap it in Arc.
- Every #[non_exhaustive] pub struct ships a pub [const] fn new(..)
  constructor (BucketId, CommitStamp, RaftEntry, HardState,
  RaftSnapshotMetadata) so downstream impl crates can synthesize
  values from foreign types.
- CommitStamp derives PartialOrd/Ord and carries #[must_use] —
  cursor comparison is semantically load-bearing; discarding one
  is a correctness bug.
- BackendError::BucketConflict is a structured variant rather than
  stringly-typed `Other("..")`.
- #[diagnostic::on_unimplemented] on both traits points downstream
  implementors at ADR 0002 §6 in the compile error.

Closes ROADMAP:816. Next: ROADMAP:817 (Backend impl against redb).
```

## Rollback plan

`git revert` the squash-commit. Nothing external depends on the
traits yet (mango-raft, mango-mvcc are future phases). The existing
`mango-storage` skeleton stays; the revert just removes `backend.rs`
and the re-exports.

If the trait shape turns out to be wrong at impl time (ROADMAP:817 /
:818), the fix is an ADR 0002 amendment PR followed by a trait PR
that updates `backend.rs` — the trait file is the single place to
edit, by design.

## PR description — must include

For reviewer-of-record:

- Text of the final `backend.rs` (link to the file; ~220 LoC).
- Diff note: `bytes`/`thiserror` are new workspace deps but already
  had cargo-vet exemptions; no new crates enter the dep graph.
- Output of `cargo doc -p mango-storage --no-deps
--document-private-items` landing successfully (proves no rustdoc
  regressions from `missing_docs`).
- Output of `cargo run -q -p xtask-vet-ttl` showing unchanged
  exemption count (149/149).
- Pointer to ADR 0002 §6 and to the "Desugaring decisions" section
  of this plan for reviewer context.

## Out of scope, explicitly

- Trait impls (ROADMAP:817 redb Backend, ROADMAP:818 raft-engine
  RaftLogStore).
- An in-memory reference impl for testing (can land in Phase 2 or in
  817; not this PR).
- Differential oracle harness (ROADMAP:819).
- MVCC `KeyIndex` and Revision types (Phase 2).
- Any Raft-rs `Storage` trait adapter (Phase 5+; that's
  `mango-raft`, not `mango-storage`).
- Conversion helpers between `RaftEntry` and `raft-proto::Entry`
  (land in ROADMAP:818 at the impl boundary).

## Plan revisions from rust-expert review

The plan went through one adversarial-review round before
implementation. rust-expert returned **REVISE** with 1 showstopper
and 5 bugs plus strongly-encouraged follow-ups; every finding was
applied above. For the record, the must-fix list and its
resolutions:

1. **S1 — `BucketId` missing `#[non_exhaustive]` + constructor.**
   Addressed: `BucketId` now carries `#[non_exhaustive]`, the inner
   `u16` is exposed as `pub raw: u16` (readable but not
   literal-constructable from outside), and a `pub const fn new(id:
u16) -> Self` constructor lets impl crates and consumers build the
   type. Test `bucket_id_constructor_and_non_exhaustive` locks in
   both the constructor and `const` usability.

2. **B5 — no `pub fn new` on `RaftEntry` / `HardState` /
   `RaftSnapshotMetadata`, blocking ROADMAP:818's conversion from
   `raft::prelude::Entry`.** Addressed: all three types ship
   constructors. Same pattern applied to `CommitStamp` for
   consistency (though the impl crate is the primary consumer
   there).

3. **B1 — `install_snapshot` rustdoc did not pin
   `first_index`/`last_index` post-conditions.** Addressed:
   `install_snapshot` rustdoc now spells out
   `first_index() == snapshot.index + 1`, `last_index() >=
snapshot.index`, and "entries before `snapshot.index + 1` MAY
   be discarded" — matching raft-rs's `Storage::snapshot` contract.

4. **B4 — `Backend::close(self)` incompatible with `Arc<B>`
   sharing.** Addressed: signature flipped to `fn close(&self)`
   with an idempotent-call contract (atomic `closed` flag internal
   to impl; subsequent read/write methods return
   `BackendError::Closed`). The "deterministic last-fsync" semantic
   the ADR wanted is preserved via explicit `close` call; `Drop` is
   the safety-net fallback.

5. **B3 — `Self::Batch: 'static` design constraint undocumented.**
   Addressed: bound made explicit on the associated type, and the
   `Backend` trait rustdoc carries an "Associated-type design
   notes" section pointing out that GATs would block future
   adapter types and that borrows must go through `Arc<Database>`.

6. **R4 + M1 — no `Ord` on `CommitStamp`; no `#[must_use]` on
   cursor / state types.** Addressed: `CommitStamp` derives
   `PartialOrd, Ord` (cursor comparison is load-bearing for
   durability ordering), and `#[must_use]` is now on `CommitStamp`,
   `HardState`, `RaftEntry`, `RaftSnapshotMetadata`.

Strongly-encouraged follow-ups also applied (so the PR lands
complete rather than bouncing back for nits):

- **M2** — `#[diagnostic::on_unimplemented]` on both `Backend` and
  `RaftLogStore`, pointing at ADR 0002 §6.
- **M3** — `#![deny(rustdoc::broken_intra_doc_links)]` at the
  module root. Guards the intradoc-link network from silent rot.
- **M4** — one module-level `no_run` doctest showing the expected
  shape (`open` → `register_bucket` → `snapshot`).
- **B2** — static assertion that `Box<dyn RangeIter<'a> + 'a>` is
  `Send` (the supertrait bound propagates, but pinning it in a
  test locks it in for future refactors).
- **M7** — `backend_error_io_source_chain` test verifies
  `Error::source()` chains through `BackendError::Io` (guards
  against a future refactor dropping `#[from]` / `#[source]`).
- **BucketConflict** — replaces `BackendError::Other(..)` for the
  register-bucket-collision case with a structured variant
  (`id: BucketId`, `existing: String`, `requested: String`).
  Operators can route on the field names rather than string
  substrings.

Resulting delta versus the pre-review plan:

- `backend.rs` LoC estimate: ~220 → ~280.
- Test count in `lib.rs`'s `mod tests`: 5 → 7 new.
- One new error variant (`BucketConflict`).
- Five `fn new` constructors added.
- Two `#[diagnostic::on_unimplemented]` attributes.
- One `#![deny(rustdoc::broken_intra_doc_links)]` directive.
- `Backend::close` receiver: `self` → `&self`.
- `CommitStamp` derives: `PartialOrd, Ord` added.
- `#[must_use]` added to four types.

Unaccepted (with justification): none. Every item in the
rust-expert review either applied cleanly or was already addressed
in the draft (nit items 9, 10 about clippy and MSRV were already
covered in "Edge cases and risks"). The LoC growth is acceptable
for the quality lift.
