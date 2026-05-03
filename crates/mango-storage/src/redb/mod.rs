//! `redb`-backed [`crate::Backend`] impl (ROADMAP:817).
//!
//! # Design
//!
//! The backend is a thin adapter on top of `::redb::Database`:
//!
//! - **State** lives in [`Inner`] behind `Arc<Inner>`; [`RedbBackend`]
//!   is a handle that clones this `Arc`. This gives us `Send + Sync +
//!   'static` for the whole handle, which is what the `Backend` trait
//!   requires and what the `impl Future<..> + Send` commit methods
//!   need.
//! - **Durability** is always `Durability::Immediate` for now. The
//!   `force_fsync` parameter on [`Backend::commit_batch`] is currently
//!   unused; wiring `Durability::None` (or coalesced fsync) for the
//!   `false` case is a ROADMAP:817 follow-up — the correctness bar is
//!   "at least as strong as etcd" and always-fsync is strictly
//!   stronger than the batch-tx model.
//! - **Registry** lives in-memory as [`registry::Registry`] with a
//!   mirror in the on-disk `__mango_bucket_registry` table. At open
//!   time, we hydrate the in-memory view from disk; at
//!   `register_bucket` time, we write through to disk.
//! - **Write-path ordering** inside a single `commit_batch` /
//!   `commit_group`: staged ops are grouped by `BucketId` (via a
//!   `BTreeMap` — deterministic ordering is helpful for testing and
//!   never hurts) so each redb `Table` is opened exactly once per
//!   commit. redb 4.x's `WriteTransaction::open_table` returns
//!   `TableError::TableAlreadyOpen` (not a panic) when the same
//!   table is opened twice in one txn; grouping is
//!   correctness-load-bearing because the recovery path from that
//!   error is uglier than pre-sorting the ops.
//! - **Close** flips an `AtomicBool` via `compare_exchange`
//!   (idempotent) and drops the `Database` handle by replacing the
//!   `Option<Database>` with `None`. Subsequent methods see
//!   `closed = true` and return [`BackendError::Closed`] before
//!   touching the option.
//!
//! # Send-ness of the write path
//!
//! [`RedbBatch`] is `!Send`; the trait contract in
//! [`crate::WriteBatch`] permits this. The commit methods extract the
//! staged `Vec<StagedOp>` synchronously (via `batch.into_staged()`)
//! BEFORE constructing the `async` block, so the `!Send` marker never
//! flows into the returned `Future`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
// Traits in scope for `Database::begin_read` and
// `ReadOnlyTable::iter`/`get`/`range` — redb 4.1 hangs these on
// `ReadableDatabase` / `ReadableTable` so the read-only and
// read-write database types can share them.
use ::redb::{ReadableDatabase, ReadableTable};

use crate::backend::{Backend, BackendConfig, BackendError, BucketId, CommitStamp};

pub(crate) mod batch;
pub(crate) mod registry;
pub(crate) mod snapshot;
pub(crate) mod value_compression;

use batch::{RedbBatch, StagedOp};
use registry::{physical_table_name, RegisterOutcome, Registry, REGISTRY_TABLE_NAME};
use snapshot::RedbSnapshot;

/// Map a `tokio::task::JoinError` into [`BackendError`]. Used by
/// every `spawn_blocking`-based commit path; factored so the
/// rendered message is identical across them.
fn map_join_error(e: &tokio::task::JoinError) -> BackendError {
    BackendError::Other(format!("spawn_blocking join: {e}"))
}

/// Name of the single redb file inside the user-supplied data
/// directory. Kept as a constant rather than a config knob — the
/// file layout is our implementation detail, not part of the public
/// surface.
const DB_FILENAME: &str = "mango.redb";

/// Typed handle for the bucket-name ⇄ id table persisted on disk.
/// `&str` keys are stable under redb's default UTF-8 encoding;
/// `u16` values match [`BucketId::raw`].
const REGISTRY_TABLE: ::redb::TableDefinition<'_, &str, u16> =
    ::redb::TableDefinition::new(REGISTRY_TABLE_NAME);

/// Shared state. Everything in the backend sits behind `Arc<Inner>`
/// so clones of [`RedbBackend`], read snapshots, and `spawn_blocking`
/// closures can all share the same state cheaply.
#[derive(Debug)]
pub(crate) struct Inner {
    /// The redb `Database`. Wrapped in `Option` so `close` can
    /// deterministically drop it (and release the file lock) without
    /// needing unique ownership of the backend. `RwLock` because
    /// [`::redb::Database::compact`] takes `&mut self`; every other
    /// path takes a read guard.
    db: RwLock<Option<::redb::Database>>,
    /// Bucket-name ⇄ id registry. Mutated on `register_bucket`;
    /// read-only on every other hot path.
    pub(super) registry: RwLock<Registry>,
    /// `true` once `close` has returned `Ok(())`. Checked via
    /// `load(Acquire)` at the top of every public method so
    /// in-flight operations see a single consistent "closed" cut.
    closed: AtomicBool,
    /// Monotonically non-decreasing commit sequence. `fetch_add(1,
    /// Release)` on every successful commit — including empty
    /// `commit_group` calls, so the returned [`CommitStamp`] is
    /// *strictly* monotonic. `Release` (not `SeqCst`) is sufficient:
    /// readers of a stamp only need to observe the effects of the
    /// commit that produced it, and `Acquire` on the read side
    /// completes the pair.
    commit_seq: AtomicU64,
    /// Filesystem path of the single redb file. Captured at open
    /// time — redb 4.x does not expose its backing path through the
    /// `Database` handle, so `size_on_disk` has to stat the file we
    /// recorded ourselves.
    db_path: PathBuf,
}

impl Inner {
    /// Fail fast if the backend is closed. Centralized so every
    /// public method gets the same ordering.
    fn check_open(&self) -> Result<(), BackendError> {
        if self.closed.load(Ordering::Acquire) {
            Err(BackendError::Closed)
        } else {
            Ok(())
        }
    }
}

/// Translate [`::redb::StorageError`] into [`BackendError`]. The
/// trait contract forbids leaking engine types (ADR 0002 §6 design
/// point 1), so every redb error flows through a translator.
pub(crate) fn map_storage_error(e: ::redb::StorageError) -> BackendError {
    match e {
        ::redb::StorageError::Io(err) => BackendError::Io(err),
        ::redb::StorageError::Corrupted(msg) => BackendError::Corruption(msg),
        ::redb::StorageError::ValueTooLarge(n) => {
            BackendError::Other(format!("value too large ({n} bytes)"))
        }
        ::redb::StorageError::DatabaseClosed => BackendError::Closed,
        ::redb::StorageError::PreviousIo => BackendError::Corruption(
            "a prior I/O error left the database in an inconsistent state".to_owned(),
        ),
        ::redb::StorageError::LockPoisoned(loc) => {
            BackendError::Other(format!("lock poisoned at {loc}"))
        }
        // `StorageError` is `#[non_exhaustive]`; render future
        // variants as `Other` so a redb minor bump does not silently
        // mis-classify a new failure mode.
        other => BackendError::Other(format!("storage error: {other}")),
    }
}

/// Translate [`::redb::TableError`]. Callers that handle
/// `TableDoesNotExist` specifically (snapshot read path) peel it off
/// before calling this; anything reaching here is propagated.
pub(crate) fn map_table_error(e: ::redb::TableError) -> BackendError {
    match e {
        ::redb::TableError::Storage(se) => map_storage_error(se),
        other => BackendError::Other(format!("table error: {other}")),
    }
}

fn map_txn_error(e: ::redb::TransactionError) -> BackendError {
    match e {
        ::redb::TransactionError::Storage(se) => map_storage_error(se),
        other => BackendError::Other(format!("transaction error: {other}")),
    }
}

fn map_commit_error(e: ::redb::CommitError) -> BackendError {
    match e {
        ::redb::CommitError::Storage(se) => map_storage_error(se),
        other => BackendError::Other(format!("commit error: {other}")),
    }
}

fn map_database_error(e: ::redb::DatabaseError) -> BackendError {
    match e {
        ::redb::DatabaseError::Storage(se) => map_storage_error(se),
        ::redb::DatabaseError::DatabaseAlreadyOpen => {
            BackendError::Other("database file already locked by another process".to_owned())
        }
        // The file's on-disk format is unreadable by this binary —
        // either it was written by a newer redb, or its header is
        // damaged. Callers routing on `Corruption` (operator alert,
        // restore from snapshot, etc.) want to see this, not a
        // generic `Other`.
        ::redb::DatabaseError::UpgradeRequired(v) => {
            BackendError::Corruption(format!("redb upgrade required (file format v{v})"))
        }
        ::redb::DatabaseError::RepairAborted => {
            BackendError::Corruption("redb repair aborted by callback".to_owned())
        }
        other => BackendError::Other(format!("database error: {other}")),
    }
}

fn map_compaction_error(e: ::redb::CompactionError) -> BackendError {
    match e {
        ::redb::CompactionError::Storage(se) => map_storage_error(se),
        other => BackendError::Other(format!("compaction error: {other}")),
    }
}

/// Read the on-disk registry table into `registry`. If the table
/// does not exist, the registry is left empty (fresh database case).
/// Duplicate names on disk are flagged as corruption — the writer
/// path never produces them, so their presence indicates on-disk
/// damage or a bug in an earlier version.
fn hydrate_registry(db: &::redb::Database, registry: &mut Registry) -> Result<(), BackendError> {
    let txn = db.begin_read().map_err(map_txn_error)?;
    let table = match txn.open_table(REGISTRY_TABLE) {
        Ok(t) => t,
        Err(::redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(map_table_error(e)),
    };
    let iter = table.iter().map_err(map_storage_error)?;
    for row in iter {
        let (k, v) = row.map_err(map_storage_error)?;
        let name = k.value().to_owned();
        let id = BucketId::new(v.value());
        if let Some(prior) = registry.force_insert(name.clone(), id) {
            return Err(BackendError::Corruption(format!(
                "registry table has duplicate entries for {name:?}: prior={prior:?}, current={id:?}"
            )));
        }
    }
    Ok(())
}

/// Extract the [`BucketId`] from any [`StagedOp`] variant. Keeps the
/// bucket-validation loop readable.
fn op_bucket(op: &StagedOp) -> BucketId {
    match *op {
        StagedOp::Put { bucket, .. }
        | StagedOp::Delete { bucket, .. }
        | StagedOp::DeleteRange { bucket, .. } => bucket,
    }
}

/// Validate every op's bucket against the in-memory registry and
/// pre-validate every `DeleteRange` op's bounds. Runs synchronously
/// in the commit prologue so user errors surface *before* we pay
/// for a write transaction.
fn validate_ops(registry: &Registry, ops: &[StagedOp]) -> Result<(), BackendError> {
    for op in ops {
        let b = op_bucket(op);
        if !registry.contains_id(b) {
            return Err(BackendError::UnknownBucket(b));
        }
        if let StagedOp::DeleteRange { start, end, .. } = op {
            // Empty `end` means "unbounded upper" per the engine-neutral
            // DeleteRange contract (matches bbolt's `len(end) == 0`
            // semantics). When end is unbounded, any start is legal —
            // including one that would otherwise exceed a non-empty end.
            if !end.is_empty() && start > end {
                return Err(BackendError::InvalidRange("start > end"));
            }
        }
    }
    Ok(())
}

/// Replay staged ops into an open [`::redb::WriteTransaction`].
/// Groups by [`BucketId`] via `BTreeMap` so each redb `Table` is
/// opened exactly once per commit — see the module doc for why this
/// is correctness-load-bearing, not just a perf trick.
fn apply_staged(txn: &::redb::WriteTransaction, ops: Vec<StagedOp>) -> Result<(), BackendError> {
    use std::collections::BTreeMap;

    let mut by_bucket: BTreeMap<u16, Vec<StagedOp>> = BTreeMap::new();
    for op in ops {
        by_bucket.entry(op_bucket(&op).raw).or_default().push(op);
    }

    for (raw, group) in by_bucket {
        let name = physical_table_name(BucketId::new(raw));
        let td: ::redb::TableDefinition<&[u8], &[u8]> = ::redb::TableDefinition::new(&name);
        let mut table = txn.open_table(td).map_err(map_table_error)?;
        for op in group {
            match op {
                StagedOp::Put { key, value, .. } => {
                    table
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(map_storage_error)?;
                }
                StagedOp::Delete { key, .. } => {
                    table.remove(key.as_slice()).map_err(map_storage_error)?;
                }
                StagedOp::DeleteRange { start, end, .. } => {
                    // `retain_in` keeps items for which the predicate
                    // returns `true`; a constant-`false` predicate
                    // deletes the whole half-open range in one pass.
                    //
                    // Empty `end` means "unbounded upper" per the
                    // engine-neutral DeleteRange contract (matches
                    // bbolt's `len(end) == 0` semantics). Splitting
                    // the branches here because a `[]..[]` half-open
                    // range is the empty range on redb, while we want
                    // "delete everything from `start` onward".
                    if end.is_empty() {
                        table
                            .retain_in::<&[u8], _>(start.as_slice().., |_, _| false)
                            .map_err(map_storage_error)?;
                    } else {
                        table
                            .retain_in::<&[u8], _>(start.as_slice()..end.as_slice(), |_, _| false)
                            .map_err(map_storage_error)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Flatten multiple batches' staged ops into one vector, consuming
/// each batch. Order is preserved: `batches[0]`'s ops come first.
/// The `Send`-ness of the returned `Vec` is what lets the `async`
/// block in `commit_group` stay `Send`.
fn flatten_batches(batches: Vec<RedbBatch>) -> Vec<StagedOp> {
    let mut out = Vec::new();
    for b in batches {
        out.extend(b.into_staged());
    }
    out
}

/// The public handle. Cheap to clone (just an `Arc` bump).
#[derive(Debug, Clone)]
pub struct RedbBackend {
    inner: Arc<Inner>,
}

impl Backend for RedbBackend {
    type Snapshot = RedbSnapshot;
    type Batch = RedbBatch;

    fn open(config: BackendConfig) -> Result<Self, BackendError> {
        if config.read_only {
            return Err(BackendError::Other(
                "read-only not yet supported; see ROADMAP:817 follow-up".to_owned(),
            ));
        }
        std::fs::create_dir_all(&config.data_dir).map_err(BackendError::Io)?;
        let db_path: PathBuf = config.data_dir.join(DB_FILENAME);
        let db = ::redb::Database::create(&db_path).map_err(map_database_error)?;

        let mut registry = Registry::default();
        hydrate_registry(&db, &mut registry)?;

        Ok(Self {
            inner: Arc::new(Inner {
                db: RwLock::new(Some(db)),
                registry: RwLock::new(registry),
                closed: AtomicBool::new(false),
                commit_seq: AtomicU64::new(0),
                db_path,
            }),
        })
    }

    fn close(&self) -> Result<(), BackendError> {
        // `compare_exchange` rather than `swap`: we only want to run
        // the drop once, even under concurrent `close` calls. The
        // subsequent `Ok(())` return from repeat callers is the
        // trait's idempotence contract.
        if self
            .inner
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Drop the database handle, releasing the file lock.
            // Any in-flight write-txn references will already have
            // been dropped — `Database::drop` blocks otherwise.
            let mut guard = self.inner.db.write();
            *guard = None;
        }
        Ok(())
    }

    fn register_bucket(&self, name: &str, id: BucketId) -> Result<(), BackendError> {
        self.inner.check_open()?;

        // Two-phase: (1) `check_only` decides the outcome WITHOUT
        // mutating the in-memory registry; (2) on `Inserted` we
        // persist to disk; (3) only after the commit succeeds do we
        // apply the in-memory insert. A failed commit therefore
        // leaves the registry identical to the on-disk mirror — the
        // backend remains usable after an `Err(Io)`. The write-lock
        // is held across all three phases so a concurrent caller
        // cannot observe the half-applied state.
        let mut reg = self.inner.registry.write();
        match reg.check_only(name, id)? {
            RegisterOutcome::AlreadyRegistered => Ok(()),
            RegisterOutcome::Inserted => {
                let db_guard = self.inner.db.read();
                let db = db_guard.as_ref().ok_or(BackendError::Closed)?;
                let txn = db.begin_write().map_err(map_txn_error)?;
                {
                    let mut table = txn.open_table(REGISTRY_TABLE).map_err(map_table_error)?;
                    table.insert(name, id.raw).map_err(map_storage_error)?;
                }
                txn.commit().map_err(map_commit_error)?;
                reg.force_insert(name.to_owned(), id);
                Ok(())
            }
        }
    }

    fn snapshot(&self) -> Result<Self::Snapshot, BackendError> {
        self.inner.check_open()?;
        let db_guard = self.inner.db.read();
        let db = db_guard.as_ref().ok_or(BackendError::Closed)?;
        let txn = db.begin_read().map_err(map_txn_error)?;
        Ok(RedbSnapshot::new(txn, Arc::clone(&self.inner)))
    }

    fn begin_batch(&self) -> Result<Self::Batch, BackendError> {
        self.inner.check_open()?;
        Ok(RedbBatch::new())
    }

    fn commit_batch(
        &self,
        batch: Self::Batch,
        force_fsync: bool,
    ) -> impl core::future::Future<Output = Result<CommitStamp, BackendError>> + Send {
        // Sync prologue: extract staged ops, validate bucket ids and
        // ranges. Doing this BEFORE the async block keeps the `!Send`
        // `RedbBatch` off the future's capture set.
        let prologue: Result<Vec<StagedOp>, BackendError> = (|| {
            self.inner.check_open()?;
            let staged = batch.into_staged();
            let reg = self.inner.registry.read();
            validate_ops(&reg, &staged)?;
            Ok(staged)
        })();
        let inner = Arc::clone(&self.inner);
        let _ = force_fsync; // reserved — see module doc

        async move {
            let staged = prologue?;
            tokio::task::spawn_blocking(move || -> Result<CommitStamp, BackendError> {
                commit_staged(&inner, staged)
            })
            .await
            .map_err(|e| map_join_error(&e))?
        }
    }

    fn commit_group(
        &self,
        batches: Vec<Self::Batch>,
    ) -> impl core::future::Future<Output = Result<CommitStamp, BackendError>> + Send {
        let prologue: Result<Vec<StagedOp>, BackendError> = (|| {
            self.inner.check_open()?;
            let merged = flatten_batches(batches);
            let reg = self.inner.registry.read();
            validate_ops(&reg, &merged)?;
            Ok(merged)
        })();
        let inner = Arc::clone(&self.inner);

        async move {
            let staged = prologue?;
            tokio::task::spawn_blocking(move || -> Result<CommitStamp, BackendError> {
                commit_staged(&inner, staged)
            })
            .await
            .map_err(|e| map_join_error(&e))?
        }
    }

    fn size_on_disk(&self) -> Result<u64, BackendError> {
        self.inner.check_open()?;
        // redb 4.x does not expose a size accessor on the Database
        // handle, so we stat the file we recorded at open time.
        // Advisory only per the trait contract — the value may lag
        // an uncommitted in-flight write-txn.
        let meta = std::fs::metadata(&self.inner.db_path).map_err(BackendError::Io)?;
        Ok(meta.len())
    }

    // NOTE: `defragment` holds `Inner::db.write()` across the
    // blocking `db.compact()` call. Every concurrent commit,
    // snapshot, and register_bucket will block on that guard for
    // the compact's duration (seconds → minutes on multi-GiB
    // files). Acceptable for Phase 1; the Phase 6 operability
    // bars will want online compaction that yields.
    fn defragment(&self) -> impl core::future::Future<Output = Result<(), BackendError>> + Send {
        let inner = Arc::clone(&self.inner);
        async move {
            tokio::task::spawn_blocking(move || -> Result<(), BackendError> {
                inner.check_open()?;
                let mut guard = inner.db.write();
                let db = guard.as_mut().ok_or(BackendError::Closed)?;
                db.compact().map_err(map_compaction_error)?;
                Ok(())
            })
            .await
            .map_err(|e| map_join_error(&e))?
        }
    }
}

/// Test-only fault-injection constructor. Gated on the
/// `_fault_test_internal` feature (leading underscore: cargo
/// convention for "private"). Used by integration tests under
/// `crates/mango-storage/tests/` that wrap a real
/// `redb::backends::FileBackend` in an arm-gated fault-injection
/// shim — ROADMAP:826's `crash_recovery_eio.rs` (EIO from
/// `sync_data`) and ROADMAP:827's `disk_full.rs` (ENOSPC from
/// `set_len`/`write`) at the time of writing, plus future fault
/// axes.
#[cfg(any(test, feature = "_fault_test_internal"))]
impl RedbBackend {
    /// Wrap a caller-supplied `redb::StorageBackend` in
    /// `RedbBackend` so integration tests can drive the production
    /// commit path against a fault-injected backend.
    ///
    /// `db_path` is used **only** for `size_on_disk` reporting and
    /// MUST be the on-disk path of the file the supplied `backend`
    /// reads from / writes to.
    ///
    /// # MIRROR-WITH `Backend::open`
    ///
    /// This constructor's `Builder` setup MUST stay in lock-step
    /// with `Backend::open` above. Any new `redb::Builder` knob
    /// added there (cache size, repair callback, etc.) MUST be
    /// replicated here or the test exercises a differently-
    /// configured engine than production — silently. (Filed as
    /// issue #64 during PR #62 rust-expert review; addressed here.)
    ///
    /// # Drop-time fsync caveat
    ///
    /// `redb::Database::Drop` calls `sync_data` up to four times via
    /// its trim-and-close path. If your wrapper is armed to fail at
    /// drop time it will silently swallow errors from those calls.
    /// Disarm before dropping.
    #[doc(hidden)]
    pub fn with_backend(
        backend: impl ::redb::StorageBackend,
        db_path: PathBuf,
    ) -> Result<Self, BackendError> {
        let db = ::redb::Builder::new()
            .create_with_backend(backend)
            .map_err(map_database_error)?;
        let mut registry = Registry::default();
        hydrate_registry(&db, &mut registry)?;
        Ok(Self {
            inner: Arc::new(Inner {
                db: RwLock::new(Some(db)),
                registry: RwLock::new(registry),
                closed: AtomicBool::new(false),
                commit_seq: AtomicU64::new(0),
                db_path,
            }),
        })
    }
}

/// The work performed inside `spawn_blocking` for both
/// `commit_batch` and `commit_group`. Opens a write transaction,
/// replays staged ops, sets `Immediate` durability, commits, and
/// bumps the commit sequence. Runs on a blocking thread because
/// `WriteTransaction::commit` calls `fsync` synchronously and we
/// MUST NOT block a tokio worker.
fn commit_staged(inner: &Inner, staged: Vec<StagedOp>) -> Result<CommitStamp, BackendError> {
    inner.check_open()?;
    let guard = inner.db.read();
    let db = guard.as_ref().ok_or(BackendError::Closed)?;
    let mut txn = db.begin_write().map_err(map_txn_error)?;
    txn.set_durability(::redb::Durability::Immediate)
        .map_err(|e| BackendError::Other(format!("set_durability: {e}")))?;
    apply_staged(&txn, staged)?;
    txn.commit().map_err(map_commit_error)?;
    drop(guard);
    // Strictly monotonic. `Release` pairs with callers that observe
    // the stamp and then `Acquire`-read the committed state.
    let prev = inner.commit_seq.fetch_add(1, Ordering::Release);
    Ok(CommitStamp::new(prev.checked_add(1).ok_or_else(|| {
        BackendError::Other("commit_seq overflow".to_owned())
    })?))
}
