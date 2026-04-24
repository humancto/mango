// Go-side unit tests for the bbolt oracle. These test the
// *handler* logic in isolation (no subprocess spawn, no JSON
// framing): we build a `state` and call `dispatch` directly with
// constructed `request` values. The full stdin/stdout integration
// path is covered by the Rust differential harness at
// `crates/mango-storage/tests/differential_vs_bbolt.rs` — testing
// both paths in Go would be redundant.
//
// Scope for this commit: open / close / reopen plumbing. Data-op
// tests (put / get / delete / delete_range / commit_group /
// rollback / defragment) land in subsequent commits alongside
// their handlers.
package main

import (
	"encoding/base64"
	"path/filepath"
	"testing"
)

// b64 is a test helper — standard encoding, matching the protocol.
func b64(s string) string { return base64.StdEncoding.EncodeToString([]byte(s)) }

// openedState returns a state with a fresh db opened and one
// bucket `b1` pre-registered. The db path is in t.TempDir() so
// test cleanup is automatic.
func openedState(t *testing.T) *state {
	t.Helper()
	st := &state{}
	if r := dispatch(st, &request{Op: "open", Path: freshTmpDB(t)}); !r.OK {
		t.Fatalf("open: %q", r.Error)
	}
	if r := dispatch(st, &request{Op: "bucket", Name: "b1"}); !r.OK {
		t.Fatalf("bucket: %q", r.Error)
	}
	t.Cleanup(func() { _ = dispatch(st, &request{Op: "close"}) })
	return st
}

// freshTmpDB returns a fresh on-disk path for a bbolt database
// rooted in t.TempDir(). `t.TempDir()` is test-scoped so cleanup
// is automatic on test exit.
func freshTmpDB(t *testing.T) string {
	t.Helper()
	return filepath.Join(t.TempDir(), "oracle.db")
}

func TestOpenCloseRoundTrip(t *testing.T) {
	st := &state{}
	path := freshTmpDB(t)

	r := dispatch(st, &request{Op: "open", Path: path, Fsync: false})
	if !r.OK {
		t.Fatalf("open: ok=false error=%q", r.Error)
	}
	if st.db == nil {
		t.Fatal("open: state.db not set")
	}

	r = dispatch(st, &request{Op: "close"})
	if !r.OK {
		t.Fatalf("close: ok=false error=%q", r.Error)
	}
	if st.db != nil {
		t.Fatal("close: state.db not cleared")
	}
}

func TestDoubleOpenRejected(t *testing.T) {
	st := &state{}
	path := freshTmpDB(t)

	if r := dispatch(st, &request{Op: "open", Path: path}); !r.OK {
		t.Fatalf("first open: %q", r.Error)
	}
	// Second open without intervening close must fail.
	r := dispatch(st, &request{Op: "open", Path: freshTmpDB(t)})
	if r.OK {
		t.Fatal("second open succeeded; expected app-level error")
	}
	// Cleanup.
	_ = dispatch(st, &request{Op: "close"})
}

func TestCloseWithoutOpenRejected(t *testing.T) {
	st := &state{}
	r := dispatch(st, &request{Op: "close"})
	if r.OK {
		t.Fatal("close on empty state succeeded; expected app-level error")
	}
}

func TestReopenLatchesPathAndFsync(t *testing.T) {
	st := &state{}
	path := freshTmpDB(t)

	if r := dispatch(st, &request{Op: "open", Path: path, Fsync: true}); !r.OK {
		t.Fatalf("open: %q", r.Error)
	}
	// reopen must close+reopen at the same path without the caller
	// re-supplying it.
	r := dispatch(st, &request{Op: "reopen"})
	if !r.OK {
		t.Fatalf("reopen: %q", r.Error)
	}
	if st.path != path {
		t.Fatalf("reopen: path=%q, want %q", st.path, path)
	}
	if !st.fsync {
		t.Fatal("reopen: fsync bit lost")
	}
	_ = dispatch(st, &request{Op: "close"})
}

func TestUnknownOpProtocolError(t *testing.T) {
	st := &state{}
	r := dispatch(st, &request{Op: "asdf"})
	if r.OK {
		t.Fatal("unknown op accepted; expected protocol error")
	}
	if r.Error == "" {
		t.Fatal("unknown op: empty error message")
	}
}

func TestOpenRequiresPath(t *testing.T) {
	st := &state{}
	r := dispatch(st, &request{Op: "open", Path: ""})
	if r.OK {
		t.Fatal("open with empty path succeeded")
	}
}

func TestBucketIdempotent(t *testing.T) {
	st := openedState(t)
	// openedState already registered b1; re-register should succeed.
	if r := dispatch(st, &request{Op: "bucket", Name: "b1"}); !r.OK {
		t.Fatalf("re-register b1: %q", r.Error)
	}
	// Register a second bucket.
	if r := dispatch(st, &request{Op: "bucket", Name: "b2"}); !r.OK {
		t.Fatalf("register b2: %q", r.Error)
	}
}

func TestBucketWhileTxnActive(t *testing.T) {
	st := openedState(t)
	if r := dispatch(st, &request{Op: "begin"}); !r.OK {
		t.Fatalf("begin: %q", r.Error)
	}
	r := dispatch(st, &request{Op: "bucket", Name: "b2"})
	if r.OK {
		t.Fatal("bucket while txn active succeeded; expected rejection")
	}
	_ = dispatch(st, &request{Op: "rollback"})
}

func TestPutGetCommitRoundTrip(t *testing.T) {
	st := openedState(t)
	if r := dispatch(st, &request{Op: "begin"}); !r.OK {
		t.Fatalf("begin: %q", r.Error)
	}
	if r := dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64("k"), Value: b64("v")}); !r.OK {
		t.Fatalf("put: %q", r.Error)
	}
	if r := dispatch(st, &request{Op: "commit", Fsync: false}); !r.OK {
		t.Fatalf("commit: %q", r.Error)
	}
	r := dispatch(st, &request{Op: "get", Bucket: "b1", Key: b64("k")})
	if !r.OK {
		t.Fatalf("get: %q", r.Error)
	}
	if r.Value == nil || *r.Value != b64("v") {
		t.Fatalf("get: value=%v want %q", r.Value, b64("v"))
	}
}

func TestGetMissingKey(t *testing.T) {
	st := openedState(t)
	r := dispatch(st, &request{Op: "get", Bucket: "b1", Key: b64("nope")})
	if !r.OK {
		t.Fatalf("get missing: %q", r.Error)
	}
	if r.Value != nil {
		t.Fatalf("get missing: expected nil value, got %q", *r.Value)
	}
}

func TestGetBucketNotRegistered(t *testing.T) {
	st := openedState(t)
	r := dispatch(st, &request{Op: "get", Bucket: "does_not_exist", Key: b64("k")})
	if r.OK {
		t.Fatal("get on missing bucket succeeded")
	}
}

func TestPutRequiresTxn(t *testing.T) {
	st := openedState(t)
	r := dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64("k"), Value: b64("v")})
	if r.OK {
		t.Fatal("put without begin succeeded")
	}
}

func TestCommitRequiresTxn(t *testing.T) {
	st := openedState(t)
	r := dispatch(st, &request{Op: "commit"})
	if r.OK {
		t.Fatal("commit without begin succeeded")
	}
}

func TestRollbackDiscards(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "begin"})
	_ = dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64("k"), Value: b64("discarded")})
	if r := dispatch(st, &request{Op: "rollback"}); !r.OK {
		t.Fatalf("rollback: %q", r.Error)
	}
	r := dispatch(st, &request{Op: "get", Bucket: "b1", Key: b64("k")})
	if r.Value != nil {
		t.Fatalf("get after rollback: expected nil, got %q", *r.Value)
	}
}

func TestDeleteRoundTrip(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "begin"})
	_ = dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64("k"), Value: b64("v")})
	_ = dispatch(st, &request{Op: "commit"})

	_ = dispatch(st, &request{Op: "begin"})
	if r := dispatch(st, &request{Op: "delete", Bucket: "b1", Key: b64("k")}); !r.OK {
		t.Fatalf("delete: %q", r.Error)
	}
	_ = dispatch(st, &request{Op: "commit"})

	r := dispatch(st, &request{Op: "get", Bucket: "b1", Key: b64("k")})
	if r.Value != nil {
		t.Fatalf("get after delete: expected nil, got %q", *r.Value)
	}
}

func TestDeleteMissingKeyNoop(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "begin"})
	if r := dispatch(st, &request{Op: "delete", Bucket: "b1", Key: b64("nope")}); !r.OK {
		t.Fatalf("delete missing: %q", r.Error)
	}
	_ = dispatch(st, &request{Op: "commit"})
}

// TestRangeHalfOpen exercises the `[start, end)` convention: given
// keys {a, b, c}, range(a, c) must return {a, b}.
func TestRangeHalfOpen(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "begin"})
	for _, k := range []string{"a", "b", "c"} {
		_ = dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64(k), Value: b64("v-" + k)})
	}
	_ = dispatch(st, &request{Op: "commit"})

	r := dispatch(st, &request{Op: "range", Bucket: "b1", Start: b64("a"), End: b64("c"), Limit: 0})
	if !r.OK {
		t.Fatalf("range: %q", r.Error)
	}
	if len(r.Entries) != 2 {
		t.Fatalf("range len=%d, want 2; entries=%v", len(r.Entries), r.Entries)
	}
	if r.Entries[0][0] != b64("a") || r.Entries[1][0] != b64("b") {
		t.Fatalf("range entries=%v, want [a,b]", r.Entries)
	}
}

func TestRangeLimit(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "begin"})
	for _, k := range []string{"a", "b", "c", "d", "e"} {
		_ = dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64(k), Value: b64("v-" + k)})
	}
	_ = dispatch(st, &request{Op: "commit"})

	r := dispatch(st, &request{Op: "range", Bucket: "b1", Start: b64("a"), End: b64("z"), Limit: 2})
	if !r.OK {
		t.Fatalf("range: %q", r.Error)
	}
	if len(r.Entries) != 2 {
		t.Fatalf("range limit: len=%d, want 2", len(r.Entries))
	}
}

func TestSnapshotCapturesAllBuckets(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "bucket", Name: "b2"})

	_ = dispatch(st, &request{Op: "begin"})
	_ = dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64("x"), Value: b64("1")})
	_ = dispatch(st, &request{Op: "put", Bucket: "b2", Key: b64("y"), Value: b64("2")})
	_ = dispatch(st, &request{Op: "commit"})

	r := dispatch(st, &request{Op: "snapshot"})
	if !r.OK {
		t.Fatalf("snapshot: %q", r.Error)
	}
	if len(r.State) != 2 {
		t.Fatalf("snapshot: state has %d buckets, want 2; state=%v", len(r.State), r.State)
	}
	if len(r.State["b1"]) != 1 || r.State["b1"][0][0] != b64("x") {
		t.Fatalf("snapshot b1: %v", r.State["b1"])
	}
	if len(r.State["b2"]) != 1 || r.State["b2"][0][0] != b64("y") {
		t.Fatalf("snapshot b2: %v", r.State["b2"])
	}
}

func TestSizeReportsPositive(t *testing.T) {
	st := openedState(t)
	r := dispatch(st, &request{Op: "size"})
	if !r.OK {
		t.Fatalf("size: %q", r.Error)
	}
	if r.Bytes <= 0 {
		t.Fatalf("size: %d, expected positive", r.Bytes)
	}
}

// TestPostReopenPersistence asserts that committed state survives
// a close + reopen cycle — the hard contract from plan §5.
func TestPostReopenPersistence(t *testing.T) {
	st := openedState(t)
	_ = dispatch(st, &request{Op: "begin"})
	_ = dispatch(st, &request{Op: "put", Bucket: "b1", Key: b64("persist"), Value: b64("yes")})
	_ = dispatch(st, &request{Op: "commit", Fsync: true})

	if r := dispatch(st, &request{Op: "reopen"}); !r.OK {
		t.Fatalf("reopen: %q", r.Error)
	}
	r := dispatch(st, &request{Op: "get", Bucket: "b1", Key: b64("persist")})
	if r.Value == nil || *r.Value != b64("yes") {
		t.Fatalf("post-reopen get: value=%v want %q", r.Value, b64("yes"))
	}
}
