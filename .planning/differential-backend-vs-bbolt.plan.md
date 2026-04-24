# Phase 1 — Differential-test harness vs bbolt (ROADMAP:819)

**Roadmap item:** `ROADMAP.md:819` —

> **Differential-test harness vs bbolt** `tests/differential/backend_vs_bbolt.rs`:
> proptest-generated 10k-operation sequences run against mango's `Backend` and
> against the `benches/oracles/bbolt/` Go binary via JSON IPC; assert identical
> visible state after every commit. Blocker for Phase 1 close.
> **Non-bbolt-quirk divergence that cannot be root-caused to a wrapper bug
> triggers engine swap** per ADR 0002 §5 Tier-1 trigger #2.

**Status:** APPROVE_WITH_NITS from rust-expert on v2; 8 nits handled inline during §9 Rollout.
**Companion research:** `.planning/research/bbolt-oracle-setup.md` (§G6: shared
Go binary with `--mode` dispatch).
**ADR:** `.planning/adr/0002-storage-engine.md` §5 (Tier-1 trigger #2).

## Revision log

v1 → v2 changes driven by rust-expert review:

- **S1** (empty-value / nil-key): no longer in the quirks list. They are first-class ops with a symmetric-error-divergence contract — if one engine rejects and the other accepts, the test fails.
- **S2** (`commit_group`): added to the op language at 5% probability.
- **S3** (drop-guard): concrete `impl Drop` for `GoOracle` in §7, with field-drop order, kill-then-wait timeout, and BufReader sizing.
- **B1** (time budget): re-derived; dominant cost is per-commit fsync (~100 µs × ~1M commits = ~100 s) + per-op JSON encode/decode (~10 µs × 10M ops = ~100 s). Realistic thorough-run budget: 5–15 min, not 20 min.
- **B2** (value distribution): `prop_oneof![0..=16 @ 40%, 64..=256 @ 40%, 1024..=4096 @ 20%]` exercises bbolt's overflow-page path.
- **B3** (Scanner buffer): explicit `scanner.Buffer(make([]byte, 1<<20), 16<<20)` in `main.go` + `BufReader::with_capacity(16 << 20, stdout)` on Rust side.
- **R1** (macOS fsync): `fsync` bool on the `open` op (default true, gated to false via `MANGO_DIFFERENTIAL_FSYNC=0` for dev iteration; CI enforces true).
- **R2** (supply-chain): exact versions enumerated in §6 + `cargo vet` must be clean in the same PR.
- **R3** (error asymmetry): codified in §5 — error-divergence IS a test failure unless both engines return an error for the same op.
- **R4** (defragment): added at 1% probability.
- **M1** (raft-log diff harness): plan §12 proposes a new ROADMAP bullet; not gating this PR.
- **M2** (`--mode`): scaffolded in `main.go`; `bench` returns `unimplemented` for 829 to layer in.
- **M3** (failure reporting): §7 now has serialized op-sequence dumps, artifact preservation, and minimal-diff output.
- **M4** (seeded regressions on PR): explicit — `tests/differential_vs_bbolt/seeds/` runs on every PR before the proptest-sampled 256.
- **N1** (`id` field): kept, documented as advisory-only; no logic depends on it matching.
- **N2** (time budget consistency): §7 and §10 DoD both say ≤ 15 minutes.
- **N3** (byte ordering): bucket names constrained to ASCII `[a-z0-9_]`; documented.
- **N4** (non-goal): harness does NOT test iterator stability under concurrent mutation.

---

## 0. Why this exists

The bbolt oracle is the external ground truth against which redb's
`Backend` impl is validated. This item is about **semantic
agreement**, not performance (performance is ROADMAP:829). A 10k-op
proptest sequence applied to both engines with identical commit
boundaries must yield byte-identical `get` / `range` results after
every commit. Divergence ⇒ bug in the wrapper or an engine-swap
trigger.

The bbolt binary referenced in the roadmap doesn't exist yet — this
PR stands up the diff-mode oracle, scaffolding for 829's bench
mode to layer in without re-plumbing.

## 1. Scope (what this PR ships)

1. **Go oracle binary** at `benches/oracles/bbolt/`:
   - `main.go` — `--mode` flag dispatching to `diff` (this PR) or
     `bench` (stubbed: `log.Fatal("mode=bench unimplemented; tracked in ROADMAP:829")`).
     Diff mode reads line-delimited JSON ops on stdin, applies them
     to an on-disk bbolt db, responds with line-delimited JSON results
     on stdout.
   - `go.mod` / `go.sum` — pinned to `go.etcd.io/bbolt` at the version
     etcd v3.5.29's `go.mod` pins (verified at commit time).
   - `VERSIONS` — shell-sourceable: `BBOLT_VERSION`,
     `BBOLT_GOMOD_SHA`, `BBOLT_GOSUM_SHA`.
   - `build.sh` — reproducible build (`go build -trimpath -ldflags="-s -w"`).
   - `README.md` — usage, protocol spec, pin-bumping procedure,
     TOFU threat model, audit posture (no third-party Go deps).
   - `BBOLT_QUIRKS.md` — enumerates the accepted semantic deltas.
   - `.gitignore` — the built binary.
2. **Rust differential harness** at
   `crates/mango-storage/tests/differential_vs_bbolt.rs`:
   - proptest strategy over the op-language (§3).
   - spawns the Go binary as a subprocess per test case, wired by
     stdin/stdout; one subprocess per case, reused across all ops
     in the case.
   - applies the same sequence to `RedbBackend` in-process and to
     the Go oracle.
   - after every commit, reads the full state from both and asserts
     byte-identical equality (§4).
   - default `PROPTEST_CASES=256` (fast); CI env
     `MANGO_DIFFERENTIAL_THOROUGH=1` bumps it to 10_000.
   - PR-time suite ALSO runs committed regression seeds from
     `tests/differential_vs_bbolt/seeds/`.
3. **CI wiring** (`.github/workflows/`):
   - new job `differential` that installs Go toolchain, builds the
     oracle, runs `cargo test --test differential_vs_bbolt` at
     default case count for PRs.
   - nightly job (`cron`) runs with `MANGO_DIFFERENTIAL_THOROUGH=1`
     at 10k cases.
   - milestone-tag job (triggered on `v*` push) runs the thorough
     suite + archives `target/differential-failures/` if anything
     diverged.

### Explicitly out of scope

- Bench-oriented workload phases (range-scan, hot/cold cache,
  latency histograms) — those are item 829. `main.go`'s `bench`
  mode is stubbed for 829 to layer in.
- Tier 1 Linux procurement — not a code problem, not in this PR.
- Cross-platform Go binary pre-built artifacts — each contributor
  builds locally via `build.sh`.
- RaftLogStore differential harness — §12 proposes a new ROADMAP
  bullet; separate PR.
- Iterator stability under concurrent mutation — this harness
  snapshots only after commit. Concurrent-mutation iterators are
  out of scope per ADR 0002 §6 (single-writer).

## 2. JSON IPC protocol

**Wire format:** newline-delimited JSON on stdin/stdout. One
request per line, one response per line. Request/reply is strictly
serial — one in flight at any time. The `id` field is advisory for
debugging; the harness does NOT match responses by id.

### Framing

- Go side: `bufio.Scanner` with explicit buffer sizing:
  `scanner.Buffer(make([]byte, 1<<20), 16<<20)` (start 1 MiB, grow
  to 16 MiB). The default 64 KiB cap overflows on realistic
  `snapshot` responses.
- Rust side: `BufReader::with_capacity(16 << 20, stdout)` on the
  child stdout; `BufWriter` on child stdin.
- Newlines in embedded string fields: Go's `encoding/json`
  escapes them in strings (`\n` → `\\n`). The Rust side splits
  frames on literal `\n` bytes; this is safe because
  `encoding/json` emits escaped newlines, never raw ones.
- Protocol-level errors (bad JSON, unknown op) return
  `{"ok": false, "error": "protocol: ..."}` and the Go side
  continues processing. Only `close` ends the process.

### Request shapes

```json
{"id": 1, "op": "open",     "path": "/tmp/xxx.db", "fsync": true}
{"id": 2, "op": "bucket",   "name": "default"}
{"id": 3, "op": "begin"}
{"id": 4, "op": "put",      "bucket": "default", "key": "aGVsbG8=", "value": "d29ybGQ="}
{"id": 5, "op": "delete",   "bucket": "default", "key": "aGVsbG8="}
{"id": 6, "op": "delete_range", "bucket": "default", "start": "YQ==", "end": "eg=="}
{"id": 7, "op": "commit",   "fsync": true}
{"id": 8, "op": "commit_group", "batches": [[...ops...], [...ops...]], "fsync": true}
{"id": 9, "op": "rollback"}
{"id": 10, "op": "get",     "bucket": "default", "key": "aGVsbG8="}
{"id": 11, "op": "range",   "bucket": "default", "start": "YQ==", "end": "eg==", "limit": 1000}
{"id": 12, "op": "snapshot"}
{"id": 13, "op": "size"}
{"id": 14, "op": "compact"}
{"id": 15, "op": "close"}
{"id": 16, "op": "reopen"}
```

Keys/values/ranges are base64-encoded because stdin is a text
channel and byte strings are not reliably JSON-safe. Base64 adds
~200ms total overhead across the 10M-op thorough run — negligible
next to the debugging-by-`jq` win.

`snapshot` returns `{bucket: [[k, v], ...]}` keyed by bucket name.

### Response shapes

```json
{"id": 1, "ok": true}
{"id": 10, "ok": true, "value": "d29ybGQ="}   // null on miss
{"id": 11, "ok": true, "entries": [["a","v1"],["b","v2"]]}
{"id": 12, "ok": true, "state": {"default": [["a","v1"]]}}
{"id": 13, "ok": true, "bytes": 16384}
{"id": X,  "ok": false, "error": "app: ..."}    // app-level (e.g. ErrValueNil)
{"id": X,  "ok": false, "error": "protocol: ..."}   // malformed request
```

**Error classes** matter for the differential contract — see §5.

## 3. Op language (Rust side)

```rust
enum DiffOp {
    Put { bucket: BucketId, key: Vec<u8>, value: Vec<u8> },
    Delete { bucket: BucketId, key: Vec<u8> },
    DeleteRange { bucket: BucketId, start: Vec<u8>, end: Vec<u8> },
    Commit { fsync: bool },
    CommitGroup { batches: Vec<Vec<DiffOp>>, fsync: bool }, // NEW
    Rollback,
    CloseReopen,
    Defragment, // NEW
}
```

Read ops (`get`, `range`, `snapshot`) are driven by the harness's
checker, NOT by proptest — after each `Commit` / `CommitGroup` /
`Defragment` / `CloseReopen` the harness snapshots both engines
and diffs them exhaustively.

### proptest strategy

- **BucketId** drawn from a set of 3, named `b1`, `b2`, `b3`
  (ASCII `[a-z0-9_]` only — byte-lex == UTF-8-lex for ASCII, so
  bucket ordering is identical on both engines).
- **Keys**: length drawn from `0..=16` (includes empty), bytes
  drawn from a 16-value alphabet (`[0..=15]`). High collision
  density on purpose.
- **Values**: length drawn from `prop_oneof![
  1 => Just(0_usize),       // empty values (boundary)
  39 => 1..=16_usize,       // small — leaf-page residency
  40 => 64..=256_usize,     // medium — etcd median
  20 => 1024..=4096_usize,  // large — exercises bbolt overflow pages
]`. Content bytes drawn from `[0..=15]` (same compression-unfriendly
  alphabet as keys).
- **Op distribution**: 50% Put, 18% Delete, 5% DeleteRange,
  15% Commit, 5% CommitGroup, 3% Rollback, 2% CloseReopen, 1%
  Defragment, 1% "error-triggering" op (nil key — see §5).
  Final op is always `Commit` to force a terminal-state diff.
- **CommitGroup inner batches**: 2..=8 batches, each 0..=10 ops
  (Put/Delete only — no nested CloseReopen / CommitGroup).
  Proptest generates these flat; the harness emits them as a
  single request on the wire.

### Per-case run shape

```
per case:
  - mkdir two temp dirs, one per engine
  - spawn Go oracle subprocess; send "open" with bbolt path
  - open RedbBackend
  - register 3 buckets on both
  - smoke-check: round-trip a known KV, assert equality
  - apply ops in order
  - after each Commit / CommitGroup / Defragment / CloseReopen:
      - snapshot both
      - assert byte-for-byte equality (ordered by (bucket, key))
      - on failure: dump op sequence + bbolt db + redb db to
        target/differential-failures/<case-id>/
  - at end: drop both (Drop order: GoOracle, then RedbBackend,
    then TempDirs) + emit final close
```

## 4. Wiring — mango's `RedbBackend` end

The harness uses the existing `RedbBackend` impl. Nothing here
requires changes to mango source — the `Backend` trait is the
exact surface we need.

Concerns verified against the trait:

1. `commit_batch(force_fsync: true)` aligns with bbolt's
   `Tx.Commit()` which fsyncs unconditionally.
2. `commit_group(batches, force_fsync)` is the Raft-batching
   primitive. The Go oracle emulates by wrapping all batches in
   one `db.Update()` call; document the lowering in
   `BBOLT_QUIRKS.md`.
3. `snapshot()` returns a `ReadSnapshot` over a consistent point
   in time, aligning with bbolt's read-only `Tx`.
4. `delete_range` lowers to a cursor loop in redb; bbolt has no
   native delete_range → the Go oracle emulates it with a cursor
   loop in a single txn. Semantics: **inclusive start,
   exclusive end, `[low, high)` convention** documented in
   `README.md` and backed by a unit test in `main_test.go`.
5. `defragment` lowers to `db.Close() → bbolt.Compact()` on the
   Go side and `RedbBackend::defragment()` on the Rust side.

## 5. Semantic-divergence contract (replaces v1's quirks list)

"Quirks" in v1 swept real divergences under the rug. v2's shape:

### Hard contracts (failure = bug)

- **Empty values**: both engines accept OR both reject with the
  same error class. Harness emits empty values at ~3% weighted.
  Expected: bbolt rejects (`ErrValueNil`); the `RedbBackend`
  wrapper MUST lift this into the same `BackendError::Other(...)`
  so both sides return `ok: false, error: "app: empty value"`.
  **If the wrapper doesn't, this PR adds the lifting.** No quirks
  allowed.
- **Empty keys**: same contract. bbolt rejects; wrapper lifts.
- **Mid-batch error asymmetry**: if op N returns `ok: false` on
  one engine but `ok: true` on the other, that's divergence →
  test fails, artifact preserved. Symmetric errors (same error
  class on both) are fine.
- **`[low, high)` range convention**: both engines must return the
  same keys for `[low, high)` across all (low, high) pairs.
- **Post-reopen state**: after `CloseReopen`, read of any key that
  was in the last committed state returns the same value on both
  engines.

### Accepted quirks (genuinely engine-specific, documented in

`BBOLT_QUIRKS.md`)

- **On-disk size**: bbolt 4 KiB pages + COW allocator; redb 4 KiB
  pages + COW. Sizes within 2× are accepted. The diff harness
  reports sizes but does not assert equality (that's item 829).
- **Bucket auto-create**: bbolt creates buckets on first write;
  Rust side calls `register_bucket` explicitly. Harness
  pre-registers all 3 buckets on both sides before any op, so
  this is eliminated at the fixture level.
- **`defragment` semantics**: bbolt `Compact` produces a new file;
  redb `defragment` operates in-place. Harness measures
  pre/post-state, not file identity.
- **Iterator stability under concurrent mutation**: not tested.
  Non-goal per ADR 0002 §6 (single-writer).

If a new divergence is found outside the accepted list, the ADR
0002 §5 Tier-1 engine-swap trigger fires. That's the point.

## 6. Files to touch

### New

| Path                                                      | Purpose                                                        |
| --------------------------------------------------------- | -------------------------------------------------------------- |
| `benches/oracles/bbolt/main.go`                           | Go oracle, `--mode=diff` (bench stubbed)                       |
| `benches/oracles/bbolt/main_test.go`                      | Go-side unit tests for delete_range + quirks                   |
| `benches/oracles/bbolt/go.mod`                            | Pinned bbolt dep                                               |
| `benches/oracles/bbolt/go.sum`                            | Integrity checksums                                            |
| `benches/oracles/bbolt/VERSIONS`                          | Shell-sourceable pins                                          |
| `benches/oracles/bbolt/build.sh`                          | Reproducible build                                             |
| `benches/oracles/bbolt/README.md`                         | Usage + protocol spec                                          |
| `benches/oracles/bbolt/BBOLT_QUIRKS.md`                   | Accepted-delta list                                            |
| `benches/oracles/bbolt/.gitignore`                        | Ignore built binary + `*.db` test artifacts                    |
| `crates/mango-storage/tests/differential_vs_bbolt.rs`     | Rust harness                                                   |
| `crates/mango-storage/tests/differential_vs_bbolt/seeds/` | Committed regression-seed directory (initially empty + README) |

### Modified

| Path                                           | Change                                                                                |
| ---------------------------------------------- | ------------------------------------------------------------------------------------- |
| `.github/workflows/ci.yml`                     | Add `differential` job (setup-go@v5, build oracle, run harness at default case count) |
| `.github/workflows/differential-nightly.yml`   | New file — cron, 10k cases, artifact upload on failure                                |
| `.github/workflows/differential-milestone.yml` | New file — runs on `v*` tag push, thorough suite + artifact archive                   |
| `crates/mango-storage/Cargo.toml`              | Add dev-deps (exact versions below)                                                   |
| `supply-chain/audits.toml` / `config.toml`     | `cargo vet` entries for any new transitive deps not already audited                   |

**Exact dev-dep versions:**

- `proptest = "1.5"` (workspace already uses it transitively via some crates; confirm at implementation time; if not present, add)
- `base64 = "0.22"`
- `serde_json = "1"` (likely already in workspace)

**Supply-chain step** (commit 0 of the feature branch):
`cargo vet` runs clean with ALL new dev-deps; any new transitive
audit entries land in this PR, not a follow-up. If `cargo vet`
cannot close, adding the dep is reverted and a workaround (e.g.
hand-rolled base64 in ~30 lines) is used instead.

## 7. Subprocess lifecycle & drop-guard

Concrete `GoOracle` struct and Drop impl:

```rust
struct GoOracle {
    child: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    next_id: u64, // advisory
}

impl GoOracle {
    fn spawn(binary: &Path, db_path: &Path, fsync: bool) -> io::Result<Self> {
        let mut child = Command::new(binary)
            .args(["--mode=diff"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = BufWriter::new(child.stdin.take().expect("stdin piped"));
        let stdout = BufReader::with_capacity(16 << 20, child.stdout.take().expect("stdout piped"));
        let mut oracle = Self { child, stdin, stdout, next_id: 0 };
        oracle.call(&json!({"op":"open","path":db_path,"fsync":fsync}))?;
        Ok(oracle)
    }
    fn call(&mut self, req: &serde_json::Value) -> io::Result<serde_json::Value> {
        // write newline-terminated, flush, read one line, parse.
    }
}

impl Drop for GoOracle {
    fn drop(&mut self) {
        // Best-effort graceful close; ignore errors — the child may already be dead.
        let _ = writeln!(self.stdin, r#"{{"op":"close"}}"#);
        let _ = self.stdin.flush();
        // Poll with short timeout, then hard-kill.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() >= deadline => break,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
```

**Field-drop order** in the test fixture struct:

```rust
struct Case {
    oracle: GoOracle,        // drops first — closes pipe, reaps child
    redb: RedbBackend,       // drops second — closes db
    _bbolt_dir: TempDir,     // drops third — removes bbolt db file
    _redb_dir: TempDir,      // drops fourth — removes redb db file
}
```

Rust drops struct fields in declaration order. Getting this
wrong (e.g. `TempDir` before `GoOracle`) would let `db.Close()`
run on a deleted directory → EIO on fsync → panic in Drop.

### Failure-reporting

On assert failure (commit-point diff mismatch), the harness:

1. Writes `target/differential-failures/<utc-timestamp>-<case-hash>/` with:
   - `ops.json` — the full op sequence up to and including the
     diverging commit (serialized `Vec<DiffOp>`, not proptest seed).
   - `bbolt.db` — the bbolt file at the moment of divergence.
   - `redb.db` — the redb file at the moment of divergence.
   - `diff.txt` — human-readable minimal diff: for each (bucket,
     key) that differs, one line `bucket/key: bbolt=..., redb=...`,
     up to 20 lines then `...and N more`.
2. Prints `diff.txt` to stdout so CI logs have the smoking gun.
3. Aborts the test with `panic!` carrying the artifact path.

**The `TempDir`s are NOT auto-removed on divergence** — the Drop
impl for the `Case` fixture detects the failure flag and `mem::forget`s
both TempDirs so the files are preserved for post-mortem.

### Subprocess health

One subprocess per proptest case. Between cases, send `close`
and reap. The `GoOracle` Drop impl handles panic unwinding.

### `fsync` env gating

- Default: `fsync=true` on `open` → bbolt does real `Tx.Commit()`
  fsyncs; redb uses `Durability::Immediate`.
- `MANGO_DIFFERENTIAL_FSYNC=0` flips to `fsync=false` for local
  dev iteration on macOS (where `F_FULLFSYNC` is painfully slow).
- CI asserts `MANGO_DIFFERENTIAL_FSYNC` is unset or `=1` (gate
  check in the workflow; fails loud if someone tries to ship
  fsync-off numbers).

## 8. Risks & mitigations (re-derived)

| Risk                                                        | Mitigation                                                                                                                                                                                                                          |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Thorough run exceeds CI timeout.                            | Realistic budget: 10k cases × ~1k ops. Dominant costs: (a) ~1M commits × ~100 µs fsync on SSD ≈ 100s; (b) 10M ops × ~10 µs JSON encode/decode ≈ 100s; (c) 10k fork-execs ≈ 10s. Total ~5–15 min. Nightly CI has 6 h — 10× headroom. |
| Binary key/value escaping bugs.                             | Harness startup: round-trip a known KV `[0x00, 0xff, 0x0a, 0x0d]` — asserts base64 + newlines survive the wire.                                                                                                                     |
| `bufio.Scanner` 64 KiB cap blows up on `snapshot`.          | Explicit `scanner.Buffer(make([]byte, 1<<20), 16<<20)` in `main.go`. BufReader 16 MiB on Rust side.                                                                                                                                 |
| Go toolchain install flaky on CI.                           | `actions/setup-go@v5` with `go-version-file: benches/oracles/bbolt/go.mod`. Cache `~/go/pkg/mod`.                                                                                                                                   |
| macOS dev iteration stalled on `F_FULLFSYNC`.               | `MANGO_DIFFERENTIAL_FSYNC=0` flag; CI asserts fsync=1.                                                                                                                                                                              |
| Rollback semantics.                                         | Unit tests on both sides: begin → put → rollback → reopen → assert empty. `main_test.go` + a Rust unit test in the harness fixture.                                                                                                 |
| `delete_range` semantics mismatch.                          | `main_test.go` covers `[a,c)` on `{a,b,c}` returning `{c}`. Harness proptest emits delete_range and diffs post-state.                                                                                                               |
| Supply-chain: new Go dep creep.                             | Zero third-party Go deps beyond `go.etcd.io/bbolt` + stdlib. Enforced by README contract + `go.mod`-level pin. Any future dep bump requires ADR revision.                                                                           |
| Supply-chain: new Rust dev-deps unaudited.                  | `cargo vet` runs clean in the same PR. Any missing entries added in the same PR or the dep is hand-rolled.                                                                                                                          |
| CloseReopen race (Go mid-write).                            | Strict request/reply — one in flight. Single goroutine on Go side.                                                                                                                                                                  |
| Empty-key / empty-value lifting in wrapper not yet shipped. | This PR lifts them if absent. Unit test coverage (§11) ensures the lifting is observable.                                                                                                                                           |
| Drop-guard panic during panic unwind.                       | Drop impl is panic-safe (everything is `let _ = ...`). No `?` or `unwrap` in drop.                                                                                                                                                  |
| TempDir cleanup races with Go child fsync.                  | Field-drop order: `GoOracle` first, `TempDir` last. Documented in §7.                                                                                                                                                               |

## 9. Rollout

Small atomic commits:

1. **Supply-chain**: `cargo vet` closure for `proptest`, `base64`,
   `serde_json` (if not already audited). If this can't close,
   fall back to hand-rolled helpers before writing any feature
   code. Tests: `cargo vet` clean.
2. **Go oracle skeleton**: `main.go` with `--mode` dispatch, `open`
   - `close` ops, protocol plumbing, 16 MiB scanner buffer. Plus
     `go.mod` / `go.sum` / `VERSIONS` / `build.sh` / `README.md` /
     `.gitignore`. Tests: `go test ./benches/oracles/bbolt/`
     (trivial test covering open/close).
3. **Go oracle data ops**: `bucket` / `put` / `delete` / `get` /
   `range` / `snapshot` / `size` / `commit` / `rollback` / `reopen`.
   Plus `main_test.go` covering delete_range + `[low,high)` semantics.
4. **Go oracle advanced ops**: `delete_range` / `commit_group` /
   `compact`. `main_test.go` extends.
5. **Empty-value / empty-key lifting** in `RedbBackend` (if not
   yet present). Unit test in `crates/mango-storage/tests/redb_backend.rs`
   asserting both error shapes match expected.
6. **Rust harness: subprocess helper** (`GoOracle`, drop-guard,
   protocol round-trip). Hardcoded 10-op smoke test.
7. **Rust harness: proptest strategy** (without CommitGroup,
   Defragment). 256-case default, seeds dir (empty).
8. **Rust harness: advanced ops** (CommitGroup, Defragment,
   CloseReopen, error-triggering).
9. **Failure-reporting** (`target/differential-failures/`, diff.txt
   minimal output, TempDir preservation on failure).
10. **CI wiring**: `differential` job on PR. Integration test.
11. **CI wiring**: nightly + milestone workflows.
12. **BBOLT_QUIRKS.md** final pass.

Tests pass (`cargo test -p mango-storage`, `cargo clippy -p mango-storage --all-targets -- -D warnings`,
`go test ./benches/oracles/bbolt/`) at every commit boundary.
Each commit is reviewable in isolation.

## 10. Definition of done

- [ ] Go oracle compiles, `go test ./benches/oracles/bbolt/` green.
- [ ] Rust harness round-trips the startup smoke check.
- [ ] Rust harness runs 256 cases locally in < 60s, 0 divergences.
- [ ] Rust harness runs 10k cases under `MANGO_DIFFERENTIAL_THOROUGH=1`
      in < 15 min on a Linux dev box, 0 divergences.
- [ ] CI `differential` job green on the PR.
- [ ] `seeds/` directory exercised on every PR run.
- [ ] BBOLT_QUIRKS.md enumerates the accepted-delta list.
- [ ] `cargo vet` clean with all new dev-deps.
- [ ] `cargo clippy -p mango-storage --all-targets -- -D warnings` clean.
- [ ] rust-expert APPROVE on the final diff.
- [ ] ROADMAP.md line 819 flipped to `- [x]` on main.

## 11. Test ladder

Tests gated at each commit boundary; every claim has a matching test:

- `main_test.go`: open/close round-trip; delete_range `[low,high)`
  correctness; commit_group lowering; rollback discards uncommitted.
- `crates/mango-storage/tests/redb_backend.rs` (extends existing):
  empty-value rejection, empty-key rejection, lifted error shapes.
- `crates/mango-storage/tests/differential_vs_bbolt.rs`:
  - `smoke_10_ops_no_divergence` — hardcoded sequence covering
    put/delete/delete_range/commit/rollback/get/range, asserts
    equality.
  - `seeds/*` — committed regression replay.
  - `proptest_256_cases_no_divergence` — default PR run.
  - `proptest_10k_cases_no_divergence` — `MANGO_DIFFERENTIAL_THOROUGH=1` gated.

**Regression verification**: before final push, deliberately
mutate one op handler in `main.go` (e.g. `Delete` becomes a no-op)
to confirm the harness catches it. Undo, push. This step is in
the PR description's test plan.

## 12. Follow-up ROADMAP bullet (proposed)

Rust-expert flagged: a RaftLogStore differential harness is
needed but out of scope here. Propose adding to ROADMAP Phase 3
(Raft):

```
- [ ] **Differential-test harness vs etcd WAL** `tests/differential/raft_log_store_vs_etcd_wal.rs`:
      proptest-generated RaftLogStore op sequences run against mango's
      RaftEngineLogStore and against an etcd `raft/wal` Go oracle; assert
      identical append/compact/snapshot-install behavior. Blocker for
      Phase 3 close.
```

Add to the same PR as a separate commit (roadmap edit only;
implementation is the future PR's problem).

## 12b. Nits from rust-expert re-review on v2 (handle inline)

These are cleanup items to address during §9 Rollout; none block the
verdict.

1. **CommitGroup atomicity verification (commit 4).** Before writing
   the Go lowering, read `crates/mango-storage/src/backend.rs` (or
   wherever `Backend::commit_group` is defined) and confirm whether
   the contract is "atomic across all batches" vs "per-batch with
   one terminal fsync". If the latter, revise the Go lowering and
   document in `BBOLT_QUIRKS.md`.
2. **README cleanup note (commit 2).** Add one sentence to
   `benches/oracles/bbolt/README.md`: "On divergence,
   `target/differential-failures/*` is preserved for post-mortem and
   must be GC'd manually."
3. **Explicit base64 encoder (commit 3).** Use
   `base64.StdEncoding.EncodeToString` / `.DecodeString` with a
   `// no newlines — StdEncoding, not MIME` comment to pin the
   encoder choice against future regressions.
4. **Final-op-Commit implies final snapshot diff (commit 7).**
   Document this in the per-case run shape + test-ladder entry; the
   final op is always `Commit` and the harness asserts equality
   after it.
5. **Pipe child stderr, log on divergence (commit 9).**
   `Stdio::piped()` on stderr; only dump to
   `target/differential-failures/<case>/stderr.log` on divergence.
   Prevents 10k-case runs from interleaving Go panics with cargo
   output.
6. **`verify_harness_catches.sh` (commit 9 or 11).** Codify the
   "deliberately mutate one op handler" meta-test as a CI-runnable
   script instead of a manual checklist. Runs as a separate
   non-gating CI job (or locally).
7. **Hosted-runner fsync caveat (commit 10).** Add a comment in the
   nightly workflow that fsync latency on GitHub-hosted runners can
   10–50× the local SSD estimate; if the 10k run times out in
   practice, halve to 5k cases.
8. **`cargo vet` hard-fail (commit 0).** Add to §6: "If a new vet
   audit cannot be obtained in-session, the PR is deferred to the
   next review cycle rather than shipped with a suppression."

## 13. Rollback

If the harness surfaces a real bbolt-vs-redb divergence outside
the accepted list (§5), the loop stops — we file an ADR 0002
revision and the roadmap's Tier-1 engine-swap trigger fires. That
is not a rollback of THIS PR (the harness is doing its job); it's
the next-level process.

If this PR's CI wiring is unstable (flaky Go install, slow
nightly), we can narrow the PR job to the default 256 cases and
move the nightly to a separate follow-up. Go oracle and Rust
harness revert as independent commits.

If `cargo vet` can't close in this PR, fall back to hand-rolled
helpers (base64: ~30 LOC; proptest: NOT replaceable — delay the
item and file the vet entry upstream instead).
