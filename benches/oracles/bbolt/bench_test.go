// Go-side unit tests for the bbolt oracle's bench mode (B1 in the
// L829 plan). These test the *handler* logic in isolation by
// constructing a `benchState` and calling `dispatchBench` directly
// — no subprocess spawn, no JSON framing. The full stdin/stdout
// integration path is covered by the Rust harness at
// `benches/storage/src/bbolt_runner.rs` (lands in a later commit).
package main

import (
	"bytes"
	"encoding/base64"
	"path/filepath"
	"testing"
)

// freshTmpBenchDB returns a fresh on-disk path for a bbolt bench
// database rooted in t.TempDir().
func freshTmpBenchDB(t *testing.T) string {
	t.Helper()
	return filepath.Join(t.TempDir(), "bench.db")
}

// openedBenchState returns a state with a fresh DB opened and the
// bench bucket pre-created. The DB path is t.TempDir() rooted so
// test cleanup is automatic.
func openedBenchState(t *testing.T) *benchState {
	t.Helper()
	st := &benchState{}
	r := dispatchBench(st, &benchRequest{Op: "bench_open", Path: freshTmpBenchDB(t), Fsync: false})
	if !r.OK {
		t.Fatalf("bench_open: %q", r.Error)
	}
	t.Cleanup(func() { _ = dispatchBench(st, &benchRequest{Op: "bench_close"}) })
	return st
}

func TestBenchOpenCloseRoundTrip(t *testing.T) {
	st := &benchState{}
	path := freshTmpBenchDB(t)

	r := dispatchBench(st, &benchRequest{Op: "bench_open", Path: path})
	if !r.OK {
		t.Fatalf("bench_open: %q", r.Error)
	}
	if st.db == nil {
		t.Fatal("bench_open: state.db nil")
	}
	r = dispatchBench(st, &benchRequest{Op: "bench_close"})
	if !r.OK {
		t.Fatalf("bench_close: %q", r.Error)
	}
	if st.db != nil {
		t.Fatal("bench_close: state.db not cleared")
	}
}

func TestBenchOpenRequiresPath(t *testing.T) {
	st := &benchState{}
	r := dispatchBench(st, &benchRequest{Op: "bench_open"})
	if r.OK {
		t.Fatal("bench_open with empty path should have failed")
	}
}

func TestBenchDoubleOpenRejected(t *testing.T) {
	st := &benchState{}
	path := freshTmpBenchDB(t)
	if r := dispatchBench(st, &benchRequest{Op: "bench_open", Path: path}); !r.OK {
		t.Fatalf("first open: %q", r.Error)
	}
	t.Cleanup(func() { _ = dispatchBench(st, &benchRequest{Op: "bench_close"}) })
	r := dispatchBench(st, &benchRequest{Op: "bench_open", Path: path})
	if r.OK {
		t.Fatal("second bench_open should reject")
	}
}

func TestBenchUnknownOpRejected(t *testing.T) {
	st := &benchState{}
	r := dispatchBench(st, &benchRequest{Op: "bench_nope"})
	if r.OK {
		t.Fatal("unknown bench op should fail")
	}
}

// TestBenchLoadAndGetSeq writes 100 keys, reads them back in
// order. Asserts: ok, ops counter, hist_b64 non-empty,
// elapsed_ns positive.
func TestBenchLoadAndGetSeq(t *testing.T) {
	st := openedBenchState(t)

	// 100 keys, value = "v" + key.
	pairs := make([][2]string, 0, 100)
	keys := make([]string, 0, 100)
	for i := 0; i < 100; i++ {
		k := []byte{byte(i / 10), byte(i % 10)}
		v := append([]byte("v"), k...)
		pairs = append(pairs, [2]string{
			base64.StdEncoding.EncodeToString(k),
			base64.StdEncoding.EncodeToString(v),
		})
		keys = append(keys, base64.StdEncoding.EncodeToString(k))
	}
	r := dispatchBench(st, &benchRequest{Op: "bench_load", Pairs: pairs, BatchSize: 16})
	if !r.OK {
		t.Fatalf("bench_load: %q", r.Error)
	}
	if r.Ops != 100 {
		t.Fatalf("bench_load ops = %d, want 100", r.Ops)
	}
	if r.ElapsedNS <= 0 {
		t.Fatalf("bench_load elapsed_ns = %d, want > 0", r.ElapsedNS)
	}

	r = dispatchBench(st, &benchRequest{Op: "bench_get_seq", Keys: keys})
	if !r.OK {
		t.Fatalf("bench_get_seq: %q", r.Error)
	}
	if r.Ops != 100 {
		t.Fatalf("bench_get_seq ops = %d, want 100", r.Ops)
	}
	if r.HistB64 == "" {
		t.Fatal("bench_get_seq returned empty hist_b64")
	}
	if r.ElapsedNS <= 0 {
		t.Fatal("bench_get_seq elapsed_ns not positive")
	}
}

// TestBenchGetZipfianMirrorsGetSeq: the wire op for zipfian is the
// same as get_seq (the harness shapes the keys before sending), so
// round-tripping the same key list should produce comparable
// shapes.
func TestBenchGetZipfianMirrorsGetSeq(t *testing.T) {
	st := openedBenchState(t)

	pairs := [][2]string{{base64.StdEncoding.EncodeToString([]byte("k")), base64.StdEncoding.EncodeToString([]byte("v"))}}
	if r := dispatchBench(st, &benchRequest{Op: "bench_load", Pairs: pairs, BatchSize: 1}); !r.OK {
		t.Fatalf("load: %q", r.Error)
	}
	keys := []string{base64.StdEncoding.EncodeToString([]byte("k"))}
	r := dispatchBench(st, &benchRequest{Op: "bench_get_zipfian", Keys: keys, Theta: 0.99})
	if !r.OK {
		t.Fatalf("bench_get_zipfian: %q", r.Error)
	}
	if r.Ops != 1 {
		t.Fatalf("ops = %d, want 1", r.Ops)
	}
}

// TestBenchRangeChecksumNonZero proves the per-row copy + xor-fold
// (N8) keeps the per-row work alive — a non-empty range must
// produce a non-zero checksum, otherwise Go's escape analysis
// elided the copy and we are silently asymmetric on bbolt's mmap
// (S3 fairness).
func TestBenchRangeChecksumNonZero(t *testing.T) {
	st := openedBenchState(t)

	// Load 50 keys whose first byte is non-zero and whose value
	// also starts with a non-zero byte so the xor-fold is
	// guaranteed non-zero.
	pairs := make([][2]string, 0, 50)
	for i := 1; i <= 50; i++ {
		k := []byte{byte(i)}
		v := []byte{byte(0xff - i), 'v'}
		pairs = append(pairs, [2]string{
			base64.StdEncoding.EncodeToString(k),
			base64.StdEncoding.EncodeToString(v),
		})
	}
	if r := dispatchBench(st, &benchRequest{Op: "bench_load", Pairs: pairs, BatchSize: 16}); !r.OK {
		t.Fatalf("load: %q", r.Error)
	}

	r := dispatchBench(st, &benchRequest{
		Op:    "bench_range",
		Start: base64.StdEncoding.EncodeToString([]byte{1}),
		End:   base64.StdEncoding.EncodeToString([]byte{60}),
		Limit: 0,
	})
	if !r.OK {
		t.Fatalf("bench_range: %q", r.Error)
	}
	if r.Rows != 50 {
		t.Fatalf("rows = %d, want 50", r.Rows)
	}
	if r.Checksum == 0 {
		t.Fatal("bench_range returned zero checksum on non-empty range — fairness invariant broken (Option A in plan §S3)")
	}
}

// TestBenchRangeRespectsLimit caps at the request's limit.
func TestBenchRangeRespectsLimit(t *testing.T) {
	st := openedBenchState(t)
	pairs := make([][2]string, 0, 10)
	for i := 1; i <= 10; i++ {
		k := []byte{byte(i)}
		v := []byte("v")
		pairs = append(pairs, [2]string{
			base64.StdEncoding.EncodeToString(k),
			base64.StdEncoding.EncodeToString(v),
		})
	}
	if r := dispatchBench(st, &benchRequest{Op: "bench_load", Pairs: pairs, BatchSize: 5}); !r.OK {
		t.Fatalf("load: %q", r.Error)
	}
	r := dispatchBench(st, &benchRequest{
		Op:    "bench_range",
		Start: base64.StdEncoding.EncodeToString([]byte{1}),
		End:   base64.StdEncoding.EncodeToString([]byte{20}),
		Limit: 3,
	})
	if !r.OK {
		t.Fatalf("bench_range: %q", r.Error)
	}
	if r.Rows != 3 {
		t.Fatalf("rows = %d, want 3 (limit)", r.Rows)
	}
}

// TestBenchSizeReportsPositive: size after a load is non-zero.
func TestBenchSizeReportsPositive(t *testing.T) {
	st := openedBenchState(t)
	pairs := [][2]string{
		{base64.StdEncoding.EncodeToString([]byte("k1")), base64.StdEncoding.EncodeToString(bytes.Repeat([]byte{'v'}, 1024))},
	}
	if r := dispatchBench(st, &benchRequest{Op: "bench_load", Pairs: pairs, BatchSize: 1}); !r.OK {
		t.Fatalf("load: %q", r.Error)
	}
	r := dispatchBench(st, &benchRequest{Op: "bench_size"})
	if !r.OK {
		t.Fatalf("bench_size: %q", r.Error)
	}
	if r.Bytes <= 0 {
		t.Fatalf("bench_size bytes = %d, want > 0", r.Bytes)
	}
}

// TestBenchHistB64DecodesAsValidBase64 — the V2-deflate output
// must be valid base64 (the Rust side calls `base64.decode`).
func TestBenchHistB64DecodesAsValidBase64(t *testing.T) {
	st := openedBenchState(t)
	pairs := [][2]string{
		{base64.StdEncoding.EncodeToString([]byte("k")), base64.StdEncoding.EncodeToString([]byte("v"))},
	}
	if r := dispatchBench(st, &benchRequest{Op: "bench_load", Pairs: pairs, BatchSize: 1}); !r.OK {
		t.Fatalf("load: %q", r.Error)
	}
	keys := []string{base64.StdEncoding.EncodeToString([]byte("k"))}
	r := dispatchBench(st, &benchRequest{Op: "bench_get_seq", Keys: keys})
	if !r.OK {
		t.Fatalf("bench_get_seq: %q", r.Error)
	}
	if _, err := base64.StdEncoding.DecodeString(r.HistB64); err != nil {
		t.Fatalf("hist_b64 not valid base64: %s", err.Error())
	}
}

// TestBenchOpsRejectedWithoutOpen ensures every data op fails
// cleanly when no DB is open. Belt-and-braces: the ops all check
// `st.db == nil` themselves, but a single test enforces the
// invariant for every op without us having to remember to add a
// per-op test on every new bench op.
func TestBenchOpsRejectedWithoutOpen(t *testing.T) {
	for _, op := range []string{
		"bench_close",
		"bench_load",
		"bench_get_seq",
		"bench_get_zipfian",
		"bench_range",
		"bench_size",
	} {
		t.Run(op, func(t *testing.T) {
			st := &benchState{}
			r := dispatchBench(st, &benchRequest{Op: op})
			if r.OK {
				t.Fatalf("op %q without open should have failed", op)
			}
		})
	}
}
