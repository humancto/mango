// Package main is the bbolt differential/bench oracle for mango.
//
// This binary is invoked as a subprocess by mango's Rust-side test
// harness. It accepts newline-delimited JSON requests on stdin and
// responds with newline-delimited JSON results on stdout. See
// `README.md` for the full protocol.
//
// Two modes share the binary (plan §G6 of
// `.planning/research/bbolt-oracle-setup.md`):
//
//	--mode=diff   (this file)   — differential correctness oracle
//	                              for ROADMAP:819. One bbolt DB,
//	                              serial request/reply, per-op
//	                              state mutation.
//	--mode=bench  (stubbed)     — ROADMAP:829 bench layer. Returns
//	                              a fatal error in this PR.
//
// Supply-chain posture: the only third-party Go dep is
// `go.etcd.io/bbolt` (etcd-io, Apache-2.0). No other modules are
// permitted; `go.mod` is the enforcement surface. See README.md
// `## Threat model`.
package main

import (
	"bufio"
	"bytes"
	"encoding/base64"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"os"
	"time"

	bolt "go.etcd.io/bbolt"
)

// State tracks the oracle's currently-open database handle and
// related context. There is exactly one `*bolt.DB` at a time; the
// `reopen` op closes and reopens it. `path` and `fsync` are latched
// from the first `open` call and reused by `reopen` so the Rust side
// does not need to resend them.
type state struct {
	db    *bolt.DB
	path  string
	fsync bool
	// tx holds the currently-active writable transaction, if any.
	// Only one writable txn is ever live; bbolt enforces this via
	// its internal mutex so we don't need our own guard beyond
	// nil-checking. Reads (get/range/snapshot) do NOT use this
	// field — they acquire short-lived view txns.
	tx *bolt.Tx
}

// request is the common envelope for every line on stdin. Fields
// beyond `op` and `id` are op-specific; we use `json.RawMessage`
// for the body to allow per-op parsing without a monster struct.
type request struct {
	ID uint64 `json:"id,omitempty"`
	Op string `json:"op"`
	// Op-specific fields — flattened into the top level rather
	// than nested under a `body` key so requests remain easy to
	// hand-write / jq.
	Path   string `json:"path,omitempty"`
	Fsync  bool   `json:"fsync,omitempty"`
	Bucket string `json:"bucket,omitempty"`
	Name   string `json:"name,omitempty"`
	Key    string `json:"key,omitempty"`
	Value  string `json:"value,omitempty"`
	Start  string `json:"start,omitempty"`
	End    string `json:"end,omitempty"`
	Limit  int    `json:"limit,omitempty"`
}

// response is the common envelope for every line on stdout. Only
// one of {Value, Entries, State, Bytes} is populated per response.
// Null values for `value` are encoded as a missing field (not JSON
// null) because Go's `encoding/json` emits `"":null` for empty
// pointers, which confuses downstream parsers.
type response struct {
	ID      uint64                 `json:"id,omitempty"`
	OK      bool                   `json:"ok"`
	Error   string                 `json:"error,omitempty"`
	Value   *string                `json:"value,omitempty"`
	Entries [][2]string            `json:"entries,omitempty"`
	State   map[string][][2]string `json:"state,omitempty"`
	Bytes   int64                  `json:"bytes,omitempty"`
}

func main() {
	mode := flag.String("mode", "diff", "oracle mode: diff | bench")
	flag.Parse()

	switch *mode {
	case "diff":
		runDiff()
	case "bench":
		log.Fatal("mode=bench unimplemented; tracked in ROADMAP:829")
	default:
		log.Fatalf("unknown mode %q; expected diff | bench", *mode)
	}
}

// runDiff is the main loop for --mode=diff. Reads one JSON request
// per line from stdin, dispatches to handler, writes one JSON
// response per line to stdout. Exits when stdin closes or an
// unrecoverable error occurs. The `close` op is the explicit
// shutdown signal.
func runDiff() {
	// Scanner-buffer sizing: the default 64 KiB cap overflows on
	// realistic `snapshot` responses (the harness may ship back all
	// keys in one shot). 1 MiB initial / 16 MiB max matches the
	// Rust side's BufReader::with_capacity(16 << 20). See plan §2.
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 1<<20), 16<<20)

	writer := bufio.NewWriterSize(os.Stdout, 1<<20)
	defer writer.Flush()

	encoder := json.NewEncoder(writer)
	// Default behavior: do NOT HTML-escape, since we are not
	// emitting HTML and the escaping obscures binary payloads.
	encoder.SetEscapeHTML(false)

	st := &state{}
	for scanner.Scan() {
		line := scanner.Bytes()
		var req request
		if err := json.Unmarshal(line, &req); err != nil {
			writeResp(encoder, writer, response{
				OK:    false,
				Error: fmt.Sprintf("protocol: bad JSON: %s", err.Error()),
			})
			continue
		}
		resp := dispatch(st, &req)
		resp.ID = req.ID
		writeResp(encoder, writer, resp)
		if req.Op == "close" && resp.OK {
			// Explicit shutdown — flush and exit.
			_ = writer.Flush()
			return
		}
	}
	if err := scanner.Err(); err != nil {
		log.Printf("bbolt oracle: scanner error: %s", err.Error())
		os.Exit(1)
	}
	// EOF on stdin with no preceding close — close the DB
	// best-effort so the file handle is released before we exit.
	if st.db != nil {
		_ = st.db.Close()
	}
}

// writeResp encodes one response and flushes immediately so the
// Rust side can read without waiting for a buffer fill.
func writeResp(enc *json.Encoder, w *bufio.Writer, resp response) {
	if err := enc.Encode(&resp); err != nil {
		// If encoding itself fails we have no way to signal
		// through the protocol — fall back to stderr + exit.
		fmt.Fprintf(os.Stderr, "bbolt oracle: encode failure: %s\n", err.Error())
		os.Exit(2)
	}
	if err := w.Flush(); err != nil {
		fmt.Fprintf(os.Stderr, "bbolt oracle: flush failure: %s\n", err.Error())
		os.Exit(2)
	}
}

// dispatch routes one parsed request to the appropriate handler.
// Data ops (put/delete/get/range/etc.) are added in a subsequent
// commit — this skeleton implements only `open` / `close` /
// `reopen` to establish the protocol plumbing.
func dispatch(st *state, req *request) response {
	switch req.Op {
	case "open":
		return opOpen(st, req)
	case "close":
		return opClose(st, req)
	case "reopen":
		return opReopen(st, req)
	case "bucket":
		return opBucket(st, req)
	case "begin":
		return opBegin(st, req)
	case "put":
		return opPut(st, req)
	case "delete":
		return opDelete(st, req)
	case "commit":
		return opCommit(st, req)
	case "rollback":
		return opRollback(st, req)
	case "get":
		return opGet(st, req)
	case "range":
		return opRange(st, req)
	case "snapshot":
		return opSnapshot(st, req)
	case "size":
		return opSize(st, req)
	default:
		return response{
			OK:    false,
			Error: fmt.Sprintf("protocol: unknown op %q", req.Op),
		}
	}
}

// opOpen opens the bbolt database at req.Path. Fsync-on-commit is
// set per req.Fsync — plan §7 documents the
// MANGO_DIFFERENTIAL_FSYNC=0 dev-mode knob the harness uses to
// thread this value.
func opOpen(st *state, req *request) response {
	if st.db != nil {
		return response{
			OK:    false,
			Error: "app: database already open; call close first",
		}
	}
	if req.Path == "" {
		return response{
			OK:    false,
			Error: "protocol: open requires non-empty path",
		}
	}
	opts := &bolt.Options{
		Timeout:      5 * time.Second,
		NoSync:       !req.Fsync,
		FreelistType: bolt.FreelistMapType,
	}
	db, err := bolt.Open(req.Path, 0600, opts)
	if err != nil {
		return response{
			OK:    false,
			Error: fmt.Sprintf("app: bolt.Open: %s", err.Error()),
		}
	}
	st.db = db
	st.path = req.Path
	st.fsync = req.Fsync
	return response{OK: true}
}

// opClose closes the bbolt database. Idempotent — calling on an
// already-closed DB returns an error so the Rust side can detect
// protocol mis-sequencing.
func opClose(st *state, _ *request) response {
	if st.db == nil {
		return response{
			OK:    false,
			Error: "app: no database open",
		}
	}
	if err := st.db.Close(); err != nil {
		return response{
			OK:    false,
			Error: fmt.Sprintf("app: bolt.Close: %s", err.Error()),
		}
	}
	st.db = nil
	return response{OK: true}
}

// opReopen closes and reopens the DB at the same path with the
// same fsync setting. Exercises the post-reopen persistence
// contract: committed state must survive a close + reopen.
func opReopen(st *state, _ *request) response {
	if st.db == nil {
		return response{
			OK:    false,
			Error: "app: no database open; call open first",
		}
	}
	if err := st.db.Close(); err != nil {
		return response{
			OK:    false,
			Error: fmt.Sprintf("app: bolt.Close on reopen: %s", err.Error()),
		}
	}
	st.db = nil
	st.tx = nil // any active txn is orphaned by the close above
	// Rebuild the open request from latched state.
	reopenReq := &request{Path: st.path, Fsync: st.fsync}
	return opOpen(st, reopenReq)
}

// opBucket creates (idempotently) a named bucket. The harness
// pre-registers all buckets on both engines before any data op
// runs so the "bbolt auto-creates vs redb requires explicit"
// delta is eliminated at fixture level (see BBOLT_QUIRKS.md).
// Idempotent: re-registering an existing bucket returns OK.
func opBucket(st *state, req *request) response {
	if st.db == nil {
		return response{OK: false, Error: "app: no database open"}
	}
	if req.Name == "" {
		return response{OK: false, Error: "protocol: bucket requires non-empty name"}
	}
	if st.tx != nil {
		// Creating a bucket via a new writable txn while one is
		// already active would deadlock on bbolt's writer mutex.
		// Force the harness to commit/rollback first.
		return response{OK: false, Error: "app: cannot register bucket while txn active"}
	}
	err := st.db.Update(func(tx *bolt.Tx) error {
		_, e := tx.CreateBucketIfNotExists([]byte(req.Name))
		return e
	})
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("app: CreateBucketIfNotExists: %s", err.Error())}
	}
	return response{OK: true}
}

// opBegin starts a writable transaction. `put`, `delete`, and
// `delete_range` operate inside this txn; `commit` or `rollback`
// ends it. Only one writable txn is active at a time.
func opBegin(st *state, _ *request) response {
	if st.db == nil {
		return response{OK: false, Error: "app: no database open"}
	}
	if st.tx != nil {
		return response{OK: false, Error: "app: txn already active; commit or rollback first"}
	}
	tx, err := st.db.Begin(true)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("app: Begin(true): %s", err.Error())}
	}
	st.tx = tx
	return response{OK: true}
}

// txBucket fetches the named bucket from the active writable txn,
// or returns a protocol error if the bucket was never registered.
// Returning (nil, response) lets the caller short-circuit without
// repeating boilerplate.
func txBucket(st *state, name string) (*bolt.Bucket, *response) {
	if st.tx == nil {
		return nil, &response{OK: false, Error: "app: no active txn; call begin first"}
	}
	b := st.tx.Bucket([]byte(name))
	if b == nil {
		return nil, &response{OK: false, Error: fmt.Sprintf("app: bucket %q not registered", name)}
	}
	return b, nil
}

// opPut writes a key/value inside the active writable txn. Empty
// key or empty value is rejected at the bbolt layer (errors
// `ErrKeyRequired` / `ErrValueNil`); the redb-side wrapper lifts
// these into the same error class so symmetry holds. See plan §5
// "Semantic-divergence contract — Hard contracts".
func opPut(st *state, req *request) response {
	b, errResp := txBucket(st, req.Bucket)
	if errResp != nil {
		return *errResp
	}
	key, err := decode64(req.Key)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("protocol: bad base64 key: %s", err.Error())}
	}
	val, err := decode64(req.Value)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("protocol: bad base64 value: %s", err.Error())}
	}
	if putErr := b.Put(key, val); putErr != nil {
		return response{OK: false, Error: fmt.Sprintf("app: Put: %s", putErr.Error())}
	}
	return response{OK: true}
}

// opDelete removes a key from the active writable txn. Deleting a
// non-existent key is a no-op in bbolt (returns nil); the
// harness's redb side matches this behavior. This is NOT a quirk
// — it's the documented contract of both engines.
func opDelete(st *state, req *request) response {
	b, errResp := txBucket(st, req.Bucket)
	if errResp != nil {
		return *errResp
	}
	key, err := decode64(req.Key)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("protocol: bad base64 key: %s", err.Error())}
	}
	if delErr := b.Delete(key); delErr != nil {
		return response{OK: false, Error: fmt.Sprintf("app: Delete: %s", delErr.Error())}
	}
	return response{OK: true}
}

// opCommit commits the active writable txn, optionally honoring
// the per-commit fsync flag. bbolt's per-txn fsync is controlled
// globally by `db.NoSync`; we toggle it before commit so the
// protocol's `{"op":"commit","fsync":true/false}` translates to
// an actual durability guarantee. After the commit the field is
// restored to the open-time default so subsequent commits inherit
// the original behavior unless told otherwise.
func opCommit(st *state, req *request) response {
	if st.tx == nil {
		return response{OK: false, Error: "app: no active txn to commit"}
	}
	// bbolt commits always flush the page cache; the fsync bit
	// governs whether the OS is asked to persist. Per-txn
	// override: flip NoSync around the commit, restore after.
	previousNoSync := st.db.NoSync
	st.db.NoSync = !req.Fsync
	err := st.tx.Commit()
	st.db.NoSync = previousNoSync
	st.tx = nil
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("app: Commit: %s", err.Error())}
	}
	return response{OK: true}
}

// opRollback discards the active writable txn.
func opRollback(st *state, _ *request) response {
	if st.tx == nil {
		return response{OK: false, Error: "app: no active txn to rollback"}
	}
	err := st.tx.Rollback()
	st.tx = nil
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("app: Rollback: %s", err.Error())}
	}
	return response{OK: true}
}

// opGet reads one key. Runs inside a short-lived view txn so the
// read sees the last-committed state, NOT any uncommitted data in
// an active writable txn. This mirrors mango's `Backend::get`
// which operates against a snapshot, not against a writer's
// staging area. A missing key returns `ok: true` with a nil
// value; the response omits the `value` field entirely (see
// response type's `*string` + `omitempty`).
func opGet(st *state, req *request) response {
	if st.db == nil {
		return response{OK: false, Error: "app: no database open"}
	}
	key, err := decode64(req.Key)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("protocol: bad base64 key: %s", err.Error())}
	}
	var out *string
	viewErr := st.db.View(func(tx *bolt.Tx) error {
		b := tx.Bucket([]byte(req.Bucket))
		if b == nil {
			// Bucket not registered — treat as bucket-level error,
			// not as a missing key. Mirrors the wrapper behavior.
			return fmt.Errorf("bucket %q not registered", req.Bucket)
		}
		v := b.Get(key)
		if v != nil {
			// Copy — bbolt's value slice is only valid inside the
			// txn. We need to capture before View returns.
			s := encode64(v)
			out = &s
		}
		return nil
	})
	if viewErr != nil {
		return response{OK: false, Error: fmt.Sprintf("app: get: %s", viewErr.Error())}
	}
	return response{OK: true, Value: out}
}

// opRange returns all (k, v) pairs in `[start, end)` up to
// `limit`. The half-open interval and the `limit=0 means no cap`
// convention are enforced identically on the redb side; see plan
// §5 for the contract and `main_test.go` for the boundary tests.
func opRange(st *state, req *request) response {
	if st.db == nil {
		return response{OK: false, Error: "app: no database open"}
	}
	start, err := decode64(req.Start)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("protocol: bad base64 start: %s", err.Error())}
	}
	end, err := decode64(req.End)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("protocol: bad base64 end: %s", err.Error())}
	}
	var entries [][2]string
	viewErr := st.db.View(func(tx *bolt.Tx) error {
		b := tx.Bucket([]byte(req.Bucket))
		if b == nil {
			return fmt.Errorf("bucket %q not registered", req.Bucket)
		}
		c := b.Cursor()
		// `Seek` returns the first key >= start, or nil if the
		// bucket has no such key. From there we walk forward
		// until k >= end or we've hit the limit.
		for k, v := c.Seek(start); k != nil; k, v = c.Next() {
			if len(end) != 0 && bytes.Compare(k, end) >= 0 {
				break
			}
			entries = append(entries, [2]string{encode64(k), encode64(v)})
			if req.Limit > 0 && len(entries) >= req.Limit {
				break
			}
		}
		return nil
	})
	if viewErr != nil {
		return response{OK: false, Error: fmt.Sprintf("app: range: %s", viewErr.Error())}
	}
	return response{OK: true, Entries: entries}
}

// opSnapshot returns the full (bucket, key, value) state across
// every registered bucket. This is the workhorse the Rust harness
// compares against redb's equivalent after each commit boundary.
// Ordering: within each bucket, keys are returned in byte-lex
// order (bbolt's natural cursor order); buckets are returned in
// the order `tx.ForEach` surfaces them (which is also byte-lex).
// The Rust side sorts independently before comparing so even if
// this contract changes, equality still holds.
func opSnapshot(st *state, _ *request) response {
	if st.db == nil {
		return response{OK: false, Error: "app: no database open"}
	}
	out := make(map[string][][2]string)
	err := st.db.View(func(tx *bolt.Tx) error {
		return tx.ForEach(func(name []byte, b *bolt.Bucket) error {
			var bucketEntries [][2]string
			if cerr := b.ForEach(func(k, v []byte) error {
				bucketEntries = append(bucketEntries, [2]string{encode64(k), encode64(v)})
				return nil
			}); cerr != nil {
				return cerr
			}
			out[string(name)] = bucketEntries
			return nil
		})
	})
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("app: snapshot: %s", err.Error())}
	}
	return response{OK: true, State: out}
}

// opSize returns the on-disk size of the db file. Reported for
// debugging; the harness does NOT assert size equality (bbolt and
// redb use different page / freelist accounting — documented in
// BBOLT_QUIRKS.md "On-disk size"). Taken from `os.Stat` rather
// than `db.Stats()` so we report the actual filesystem footprint,
// including any trailing unused space.
func opSize(st *state, _ *request) response {
	if st.db == nil {
		return response{OK: false, Error: "app: no database open"}
	}
	info, err := os.Stat(st.path)
	if err != nil {
		return response{OK: false, Error: fmt.Sprintf("app: Stat: %s", err.Error())}
	}
	return response{OK: true, Bytes: info.Size()}
}

// decode64 wraps base64.StdEncoding.DecodeString so the dispatch
// layer can surface decode errors as protocol responses instead
// of panicking. Explicit StdEncoding (not URL-safe, not MIME)
// guarantees no line-break bytes appear in our wire format — the
// harness's newline-delimited framing depends on this.
func decode64(s string) ([]byte, error) {
	if s == "" {
		return nil, nil
	}
	return base64.StdEncoding.DecodeString(s)
}

// encode64 is the inverse of decode64. StdEncoding emits no line
// breaks regardless of payload length; this is the property the
// protocol depends on.
func encode64(b []byte) string {
	return base64.StdEncoding.EncodeToString(b)
}
