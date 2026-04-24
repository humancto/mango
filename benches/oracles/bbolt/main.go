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
	// Rebuild the open request from latched state.
	reopenReq := &request{Path: st.path, Fsync: st.fsync}
	return opOpen(st, reopenReq)
}
