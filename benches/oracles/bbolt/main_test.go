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
	"path/filepath"
	"testing"
)

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
