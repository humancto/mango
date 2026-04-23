// toy-unsafe: deliberately carries a known, minimal `unsafe` surface
// so scripts/geiger-scripts-test.sh can assert that real cargo-geiger
// output round-trips through our per-crate scan loop and check script.
// Hand-counted oracle lives in `../expected-baseline.json`.
//
// We use ONLY `unsafe impl Send` + `unsafe fn`. These map one-to-one
// onto geiger categories (`item_impls`, `functions`). Unsafe
// expression blocks are intentionally avoided here because geiger's
// `exprs` counter walks *sub-expressions* inside the block (derefs,
// references, calls), and that count is awkward to pin down by
// hand — it's not load-bearing for what this fixture proves.
//
// NOT part of mango's workspace (root Cargo.toml excludes this
// path), so the workspace `unsafe_code = "forbid"` lint does not
// reach here.

#[repr(C)]
pub struct Handle;

unsafe impl Send for Handle {}

pub unsafe fn assume_valid(_h: &Handle) {}
