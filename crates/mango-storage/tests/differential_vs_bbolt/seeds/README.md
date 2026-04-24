# Committed regression seeds — `differential_vs_bbolt`

**Purpose.** When the proptest-driven harness surfaces a real
divergence between the `RedbBackend` and the bbolt oracle, the
minimized reproducing op sequence is committed here as a JSON
artifact. Subsequent PRs must replay every seed before the
proptest-sampled 256 (or 10 000, under `MANGO_DIFFERENTIAL_THOROUGH=1`)
fresh cases run. This guarantees a known-bad input can never
regress unnoticed.

**State at this commit (plan §9 commit 7).** Directory is empty.
No divergences have been recorded yet. The replay-driver test that
iterates this directory on every PR run lands in plan §9 commit 9
alongside the failure-artifact persistence layer. Until then this
README is the sole non-hidden file; the directory is intentionally
committed so git does not drop it.

**File format (commit 9 onward).** Each seed is
`<utc-timestamp>-<case-hash>.json`, containing a serialized
`Vec<DiffOp>` — the exact op sequence up to and including the
diverging commit. The serialization is `serde_json` with the
`DiffOp` enum's default external tagging; field names match the
Rust source.

**Adding a seed by hand.** Don't. Seeds exist to prevent regression
of bugs we _already found_. A hand-crafted seed that has never
actually failed is noise — put it in a dedicated unit test in the
harness file instead.

**Pruning.** Seeds age out when (a) the underlying bug is fixed and
(b) we are confident the fix is stable across multiple proptest
sweeps. Pruning is done in a dedicated PR, never silently alongside
an unrelated change. The PR description must name the seed file(s)
removed and the commit that fixed the bug.
