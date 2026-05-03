// Package main: bench-mode dispatch for the bbolt oracle.
//
// This file implements `--mode=bench` (ROADMAP:829, plan §"bbolt
// oracle protocol — B1"). The bench mode is a separate request /
// response loop from the diff mode and uses a different op set
// (bench_open, bench_load, bench_get_seq, bench_get_zipfian,
// bench_range, bench_size, bench_close).
//
// Why a separate dispatch rather than extending diff mode:
//
//  1. The diff oracle's per-op statefulness (active txn, bucket
//     dance) is wrong for the bench loop, which wants whole-batch
//     timing inside one short-lived view txn / write txn.
//
//  2. Bench responses carry a base64-encoded HdrHistogram blob
//     (`hist_b64`) that diff responses do not. Sharing the
//     `response` struct would either pollute it with bench-only
//     fields (visible to every diff response) or require a tagged
//     union, neither of which is worth it for a 7-op surface.
//
//  3. The wire-format pin (V2 deflate, 1 µs floor, 60 s ceiling,
//     3 sigfigs) is hard-coded here as Go-side constants that
//     mirror `benches/storage/src/measure.rs`. A skew between
//     the two is a wire break.
//
// All bench ops are stateless WRT cross-op transactions: each
// `bench_*` op opens its own short-lived txn against the latched
// `*bolt.DB`. The single piece of cross-op state is the open
// database handle (`benchState.db`).

package main

import (
	"bufio"
	"bytes"
	"compress/zlib"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"io"
	"log"
	"math"
	"math/rand"
	"os"
	"sort"
	"time"

	hdr "github.com/HdrHistogram/hdrhistogram-go"
	bolt "go.etcd.io/bbolt"
)

// Pinned histogram parameters — MUST match `LOWEST_DISCERNIBLE_NS`,
// `HIGHEST_TRACKABLE_NS`, `SIGNIFICANT_FIGURES` in
// `benches/storage/src/measure.rs`. A drift between the two sides
// breaks cross-library round-trip; the Rust side has a unit test
// asserting the constants and a future xlang round-trip test
// (tests/hdrhist_xlang.rs) compares percentiles end-to-end.
const (
	histLowestDiscernibleNS = int64(1_000)
	histHighestTrackableNS  = int64(60_000_000_000)
	histSignificantFigures  = 3
)

// Default bucket name used by bench mode. The Rust harness
// pre-registers this exact name so the bbolt-side bucket dance
// stays out of the timed path.
const benchBucketName = "bench"

// benchState is the mode-bench analogue of `state` in main.go. We
// keep them separate so the two dispatch surfaces don't share
// fields a future refactor might tangle.
type benchState struct {
	db   *bolt.DB
	path string
}

// benchRequest is the bench-mode wire request. Field set is
// intentionally narrow — every key listed here corresponds to
// exactly one bench op.
type benchRequest struct {
	ID    uint64 `json:"id,omitempty"`
	Op    string `json:"op"`
	Path  string `json:"path,omitempty"`
	Fsync bool   `json:"fsync,omitempty"`
	// Load: pairs of base64-encoded (key, value) strings.
	Pairs [][2]string `json:"pairs,omitempty"`
	// Load: batch_size for `commit_group(N)`-equivalent batching.
	BatchSize int `json:"batch_size,omitempty"`
	// Get_seq / get_zipfian: keys to read, base64-encoded.
	Keys []string `json:"keys,omitempty"`
	// Get_zipfian: theta (skew). Recorded for provenance only;
	// the Rust harness already shaped the key list per the
	// distribution before sending it.
	Theta float64 `json:"theta,omitempty"`
	// Range: half-open [start, end) bounds, base64-encoded.
	Start string `json:"start,omitempty"`
	End   string `json:"end,omitempty"`
	Limit int    `json:"limit,omitempty"`
}

// benchResponse is the bench-mode wire response. `Ok` /
// `Error` mirror the diff side; all numeric fields are
// `omitempty` so a typical response stays compact.
type benchResponse struct {
	ID        uint64 `json:"id,omitempty"`
	OK        bool   `json:"ok"`
	Error     string `json:"error,omitempty"`
	ElapsedNS int64  `json:"elapsed_ns,omitempty"`
	Ops       int64  `json:"ops,omitempty"`
	Rows      int64  `json:"rows,omitempty"`
	// HistB64: HdrHistogram serialized as V2-deflate, base64'd.
	HistB64 string `json:"hist_b64,omitempty"`
	// Checksum: xor-fold of `(uint8(k[0])) | (uint8(v[0]) << 8)`
	// over each row of a `bench_range` scan. Forces Go's escape
	// analysis to keep the per-row copies live (S3 N8). The
	// Rust harness asserts it is non-zero on any non-empty
	// range.
	Checksum uint64 `json:"checksum,omitempty"`
	// Bytes: post-Sync `os.Stat(path).Size()` for `bench_size`.
	Bytes int64 `json:"bytes,omitempty"`
}

// runBench is the bench-mode main loop. Mirrors `runDiff` (one
// JSON request per line, one JSON response per line, `bench_close`
// is the explicit shutdown signal) but uses the `benchRequest` /
// `benchResponse` envelopes.
func runBench() {
	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 1<<20), 16<<20)

	writer := bufio.NewWriterSize(os.Stdout, 1<<20)
	defer writer.Flush()

	encoder := json.NewEncoder(writer)
	encoder.SetEscapeHTML(false)

	st := &benchState{}
	for scanner.Scan() {
		line := scanner.Bytes()
		var req benchRequest
		if err := json.Unmarshal(line, &req); err != nil {
			writeBenchResp(encoder, writer, benchResponse{
				OK:    false,
				Error: fmt.Sprintf("protocol: bad JSON: %s", err.Error()),
			})
			continue
		}
		resp := dispatchBench(st, &req)
		resp.ID = req.ID
		writeBenchResp(encoder, writer, resp)
		if req.Op == "bench_close" && resp.OK {
			_ = writer.Flush()
			return
		}
	}
	if err := scanner.Err(); err != nil {
		log.Printf("bbolt oracle: bench scanner error: %s", err.Error())
		os.Exit(1)
	}
	if st.db != nil {
		_ = st.db.Close()
	}
}

func writeBenchResp(enc *json.Encoder, w *bufio.Writer, resp benchResponse) {
	if err := enc.Encode(&resp); err != nil {
		fmt.Fprintf(os.Stderr, "bbolt oracle: bench encode failure: %s\n", err.Error())
		os.Exit(2)
	}
	if err := w.Flush(); err != nil {
		fmt.Fprintf(os.Stderr, "bbolt oracle: bench flush failure: %s\n", err.Error())
		os.Exit(2)
	}
}

// dispatchBench routes one parsed bench request to its handler.
// `bench_*` is the op-name namespace so a stray diff request
// (op="open" etc.) is rejected with a clear error rather than
// silently misrouted.
func dispatchBench(st *benchState, req *benchRequest) benchResponse {
	switch req.Op {
	case "bench_open":
		return benchOpOpen(st, req)
	case "bench_close":
		return benchOpClose(st, req)
	case "bench_load":
		return benchOpLoad(st, req)
	case "bench_get_seq":
		return benchOpGetSeq(st, req)
	case "bench_get_zipfian":
		return benchOpGetZipfian(st, req)
	case "bench_range":
		return benchOpRange(st, req)
	case "bench_size":
		return benchOpSize(st, req)
	default:
		return benchResponse{
			OK:    false,
			Error: fmt.Sprintf("protocol: unknown bench op %q", req.Op),
		}
	}
}

// benchOpOpen opens the bbolt database for the bench session and
// pre-creates the bench bucket. `Fsync` is honoured (NoSync =
// !Fsync) so the harness can match `Durability::Immediate` on the
// mango side.
func benchOpOpen(st *benchState, req *benchRequest) benchResponse {
	if st.db != nil {
		return benchResponse{OK: false, Error: "app: database already open"}
	}
	if req.Path == "" {
		return benchResponse{OK: false, Error: "protocol: bench_open requires non-empty path"}
	}
	opts := &bolt.Options{
		Timeout:      5 * time.Second,
		NoSync:       !req.Fsync,
		FreelistType: bolt.FreelistMapType,
	}
	db, err := bolt.Open(req.Path, 0600, opts)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: bolt.Open: %s", err.Error())}
	}
	// Pre-create the bench bucket so the timed path doesn't include
	// bucket creation. Idempotent.
	err = db.Update(func(tx *bolt.Tx) error {
		_, e := tx.CreateBucketIfNotExists([]byte(benchBucketName))
		return e
	})
	if err != nil {
		_ = db.Close()
		return benchResponse{OK: false, Error: fmt.Sprintf("app: create bucket: %s", err.Error())}
	}
	st.db = db
	st.path = req.Path
	return benchResponse{OK: true}
}

func benchOpClose(st *benchState, _ *benchRequest) benchResponse {
	if st.db == nil {
		return benchResponse{OK: false, Error: "app: no database open"}
	}
	if err := st.db.Close(); err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: bolt.Close: %s", err.Error())}
	}
	st.db = nil
	return benchResponse{OK: true}
}

// benchOpLoad inserts `req.Pairs` in batches of `req.BatchSize`
// each. Each batch is one bbolt write transaction (commit-once-
// per-batch semantics, the bench protocol's "batched_size"
// equivalent). Timing is whole-batch wall-clock; per-op timing on
// the write path is not requested by the protocol.
func benchOpLoad(st *benchState, req *benchRequest) benchResponse {
	if st.db == nil {
		return benchResponse{OK: false, Error: "app: no database open"}
	}
	batch := req.BatchSize
	if batch <= 0 {
		batch = 1
	}
	pairs, err := decodePairs(req.Pairs)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("protocol: %s", err.Error())}
	}
	start := time.Now()
	for i := 0; i < len(pairs); i += batch {
		end := i + batch
		if end > len(pairs) {
			end = len(pairs)
		}
		chunk := pairs[i:end]
		err := st.db.Update(func(tx *bolt.Tx) error {
			b := tx.Bucket([]byte(benchBucketName))
			if b == nil {
				return fmt.Errorf("bench bucket missing")
			}
			for _, p := range chunk {
				if e := b.Put(p[0], p[1]); e != nil {
					return e
				}
			}
			return nil
		})
		if err != nil {
			return benchResponse{OK: false, Error: fmt.Sprintf("app: load batch: %s", err.Error())}
		}
	}
	elapsed := time.Since(start).Nanoseconds()
	return benchResponse{
		OK:        true,
		ElapsedNS: elapsed,
		Ops:       int64(len(pairs)),
	}
}

// benchOpGetSeq reads `req.Keys` in the order given. The histogram
// captures per-op latency (one sample per `Get`); aggregate elapsed
// is the whole-batch wall-clock for throughput math.
func benchOpGetSeq(st *benchState, req *benchRequest) benchResponse {
	return benchGet(st, req.Keys)
}

// benchOpGetZipfian is currently a relabel of `benchOpGetSeq`: the
// Rust harness shapes the keys per the zipfian distribution before
// sending, so on the wire the two ops are isomorphic. The op name
// is kept distinct so the result-JSON labels match the workload's
// distribution name and so a future divergence (e.g., per-engine
// pre-warming) has a place to land.
func benchOpGetZipfian(st *benchState, req *benchRequest) benchResponse {
	return benchGet(st, req.Keys)
}

func benchGet(st *benchState, keys []string) benchResponse {
	if st.db == nil {
		return benchResponse{OK: false, Error: "app: no database open"}
	}
	decoded, err := decodeKeys(keys)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("protocol: %s", err.Error())}
	}
	histogram := hdr.New(histLowestDiscernibleNS, histHighestTrackableNS, histSignificantFigures)

	// Single read transaction wraps the whole batch — bbolt's
	// View txns are very cheap to acquire but per-op acquisition
	// would dominate the timed path. The plan calls this out:
	// batch-scope txn, per-op timer.
	overall := time.Now()
	err = st.db.View(func(tx *bolt.Tx) error {
		b := tx.Bucket([]byte(benchBucketName))
		if b == nil {
			return fmt.Errorf("bench bucket missing")
		}
		for _, k := range decoded {
			t0 := time.Now()
			_ = b.Get(k)
			ns := time.Since(t0).Nanoseconds()
			// SaturatingRecord-equivalent: clamp a stall above 60 s
			// into the top bucket rather than failing the whole run.
			if ns > histHighestTrackableNS {
				ns = histHighestTrackableNS
			}
			if ns < 0 {
				ns = 0
			}
			if rerr := histogram.RecordValue(ns); rerr != nil {
				return rerr
			}
		}
		return nil
	})
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: get batch: %s", err.Error())}
	}
	elapsed := time.Since(overall).Nanoseconds()
	b64, err := encodeHistB64(histogram)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: hist encode: %s", err.Error())}
	}
	return benchResponse{
		OK:        true,
		ElapsedNS: elapsed,
		Ops:       int64(len(decoded)),
		HistB64:   b64,
	}
}

// benchOpRange walks the bucket from `req.Start` to `req.End`
// (half-open), capping at `req.Limit` rows if > 0. **Per N8 in the
// plan**: each row's key+value are copied into freshly-allocated
// Go byte slices (`append([]byte{}, k...)`) and the first byte of
// each is xor-folded into the `Checksum` field. Reading the
// checksum on the Rust side forces Go's escape analysis to keep
// the copies live — without this the compiler could elide the
// copy under SSA and the bench would silently re-create the mmap
// asymmetry (S3).
func benchOpRange(st *benchState, req *benchRequest) benchResponse {
	if st.db == nil {
		return benchResponse{OK: false, Error: "app: no database open"}
	}
	startKey, err := decodeBase64(req.Start)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("protocol: start: %s", err.Error())}
	}
	endKey, err := decodeBase64(req.End)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("protocol: end: %s", err.Error())}
	}
	limit := req.Limit
	var rows int64
	var checksum uint64
	t0 := time.Now()
	err = st.db.View(func(tx *bolt.Tx) error {
		b := tx.Bucket([]byte(benchBucketName))
		if b == nil {
			return fmt.Errorf("bench bucket missing")
		}
		c := b.Cursor()
		for k, v := c.Seek(startKey); k != nil; k, v = c.Next() {
			if len(endKey) > 0 && bytes.Compare(k, endKey) >= 0 {
				break
			}
			if limit > 0 && rows >= int64(limit) {
				break
			}
			// Force-copy: append to a fresh slice so escape
			// analysis cannot elide the allocation. The xor-fold
			// reads from the copies, not from the mmap pointers.
			kCopy := append([]byte{}, k...)
			vCopy := append([]byte{}, v...)
			if len(kCopy) > 0 {
				checksum ^= uint64(kCopy[0])
			}
			if len(vCopy) > 0 {
				checksum ^= uint64(vCopy[0]) << 8
			}
			rows++
		}
		return nil
	})
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: range: %s", err.Error())}
	}
	elapsed := time.Since(t0).Nanoseconds()
	return benchResponse{
		OK:        true,
		ElapsedNS: elapsed,
		Rows:      rows,
		Checksum:  checksum,
	}
}

// benchOpSize fsyncs the database (so cache pages are flushed and
// the file size on disk matches the post-write state) and returns
// `os.Stat(path).Size()`. Plan §"on-disk-size" is explicit that we
// must use `os.Stat`, not `db.Stats().Size()` — the latter reports
// the bbolt file's allocated page count rather than physical disk
// usage.
func benchOpSize(st *benchState, _ *benchRequest) benchResponse {
	if st.db == nil {
		return benchResponse{OK: false, Error: "app: no database open"}
	}
	if err := st.db.Sync(); err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: db.Sync: %s", err.Error())}
	}
	info, err := os.Stat(st.path)
	if err != nil {
		return benchResponse{OK: false, Error: fmt.Sprintf("app: os.Stat: %s", err.Error())}
	}
	return benchResponse{OK: true, Bytes: info.Size()}
}

// encodeHistB64 serializes a Histogram via the V2-deflate codec and
// base64-encodes the result.
//
// Cross-language compatibility note: `hdrhistogram-go` v1.x
// hardcodes `getNormalizingIndexOffset() = 1` (see hdr.go:169
// in v1.2.0). The Rust `hdrhistogram` crate's deserializer rejects
// any non-zero normalizing offset with `DeserializeError::
// UnsupportedFeature` (deserializer.rs:146-148 in 7.5.x). The two
// libraries' V2-compressed payloads are therefore NOT directly
// interoperable out of the box.
//
// We work around this on the encoder side: take the standard Go
// output, inflate the inner payload, patch bytes [8..12] of the
// inflated stream (the normalizingIndexOffset slot) to zero,
// re-deflate, and emit. The resulting payload decodes cleanly on
// both sides and changes nothing about the histogram semantics —
// the offset slot is unused when no shifted recording happens, and
// our harness never shifts.
//
// The fixup is documented in BBOLT_QUIRKS.md and exercised by the
// Rust-side `bbolt_runner` integration tests
// (`load_then_get_seq_round_trip`, `range_checksum_is_non_zero`...).
func encodeHistB64(h *hdr.Histogram) (string, error) {
	encoded, err := h.Encode(hdr.V2CompressedEncodingCookieBase)
	if err != nil {
		return "", err
	}
	// `Encode` returns base64-encoded bytes; the wire wrapper is:
	//   bytes [0:4]  outer cookie (V2_COMPRESSED, big-endian)
	//   bytes [4:8]  compressed payload length (big-endian int32)
	//   bytes [8:8+L] zlib-compressed inner V2 payload
	raw, err := base64.StdEncoding.DecodeString(string(encoded))
	if err != nil {
		return "", fmt.Errorf("encodeHistB64: base64 decode: %w", err)
	}
	if len(raw) < 8 {
		return "", fmt.Errorf("encodeHistB64: encoded payload < 8 bytes (got %d)", len(raw))
	}
	var compLen int32
	if err := binary.Read(bytes.NewReader(raw[4:8]), binary.BigEndian, &compLen); err != nil {
		return "", fmt.Errorf("encodeHistB64: read compLen: %w", err)
	}
	if compLen < 0 || int(compLen) > len(raw)-8 {
		return "", fmt.Errorf("encodeHistB64: bogus compLen=%d (raw len=%d)", compLen, len(raw))
	}
	zr, err := zlib.NewReader(bytes.NewReader(raw[8 : 8+int(compLen)]))
	if err != nil {
		return "", fmt.Errorf("encodeHistB64: zlib reader: %w", err)
	}
	inflated, err := io.ReadAll(zr)
	_ = zr.Close()
	if err != nil {
		return "", fmt.Errorf("encodeHistB64: inflate: %w", err)
	}
	// Inflated layout (V2): cookie[0:4] | payloadLen[4:8] |
	// normalizingOffset[8:12] | sigFigures[12:16] | low[16:24] |
	// high[24:32] | int2double[32:40] | counts...
	if len(inflated) < 12 {
		return "", fmt.Errorf("encodeHistB64: inflated payload < 12 bytes (got %d)", len(inflated))
	}
	inflated[8] = 0
	inflated[9] = 0
	inflated[10] = 0
	inflated[11] = 0
	var compBuf bytes.Buffer
	zw, err := zlib.NewWriterLevel(&compBuf, zlib.BestCompression)
	if err != nil {
		return "", fmt.Errorf("encodeHistB64: zlib writer: %w", err)
	}
	if _, err := zw.Write(inflated); err != nil {
		_ = zw.Close()
		return "", fmt.Errorf("encodeHistB64: deflate write: %w", err)
	}
	if err := zw.Close(); err != nil {
		return "", fmt.Errorf("encodeHistB64: deflate close: %w", err)
	}
	var out bytes.Buffer
	out.Write(raw[0:4])
	if err := binary.Write(&out, binary.BigEndian, int32(compBuf.Len())); err != nil {
		return "", fmt.Errorf("encodeHistB64: write outer length: %w", err)
	}
	out.Write(compBuf.Bytes())
	return base64.StdEncoding.EncodeToString(out.Bytes()), nil
}

// decodePairs base64-decodes a slice of (key, value) string pairs
// into byte slices.
func decodePairs(pairs [][2]string) ([][2][]byte, error) {
	out := make([][2][]byte, 0, len(pairs))
	for i, p := range pairs {
		k, err := decodeBase64(p[0])
		if err != nil {
			return nil, fmt.Errorf("pair %d key: %s", i, err.Error())
		}
		v, err := decodeBase64(p[1])
		if err != nil {
			return nil, fmt.Errorf("pair %d value: %s", i, err.Error())
		}
		out = append(out, [2][]byte{k, v})
	}
	return out, nil
}

func decodeKeys(keys []string) ([][]byte, error) {
	out := make([][]byte, 0, len(keys))
	for i, k := range keys {
		b, err := decodeBase64(k)
		if err != nil {
			return nil, fmt.Errorf("key %d: %s", i, err.Error())
		}
		out = append(out, b)
	}
	return out, nil
}

func decodeBase64(s string) ([]byte, error) {
	if s == "" {
		return nil, nil
	}
	return base64.StdEncoding.DecodeString(s)
}

// Unused helpers retained so the build verifies they compile —
// they document the constants the Rust side will eventually
// exercise via the xlang round-trip test (tests/hdrhist_xlang.rs).
// `math.MaxInt64` and `sort.Sort` are imported via standard
// packages elsewhere; their presence here is documentation, not
// API surface.
var (
	_ = math.MaxInt64
	_ = sort.Search
	_ = rand.New
	_ = fnv.New64a
)
