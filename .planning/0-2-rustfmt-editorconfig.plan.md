# Plan: add `rustfmt.toml`, `.editorconfig`, `.gitattributes`

Roadmap item: Phase 0 — "Add `rustfmt.toml` and `.editorconfig` so
formatting is unambiguous" (`ROADMAP.md:749`).

Status: **revised after rust-expert review** (verdict: APPROVE_WITH_NITS;
all recommended and strongly-recommended points folded in, optional/
taste points noted below).

## Goal

Make formatting deterministic and editor-agnostic from day one, so every
contributor — and every automated tool — produces byte-identical output
for the same source. This closes the "but it looked fine on my machine"
loophole before Phase 1 lands real code. CI's existing
`cargo fmt --all -- --check` job (from PR #1) becomes the enforcer; this
PR gives it an opinionated config to enforce, plus a `.gitattributes` so
the line-ending contract also covers non-Rust files (YAML, TOML, JSON,
Markdown) that `rustfmt` does not see.

## Approach

Three new files at the workspace root. No code changes.

### `rustfmt.toml`

Stable-channel-only options. CI runs stable `rustfmt`; nightly-only
options (`imports_granularity`, `group_imports`, `wrap_comments`,
`format_code_in_doc_comments`, `normalize_comments`, `style_edition =
"2024"`) are unsupported on stable and are intentionally deferred to a
dedicated roadmap follow-up that decides whether the workspace should
pin a nightly toolchain purely for formatting.

Settings:

```toml
# rustfmt — stable channel only.
#
# Nightly-only options (imports_granularity, group_imports,
# wrap_comments, format_code_in_doc_comments, normalize_comments,
# style_edition = "2024") are unsupported on stable rustfmt and are
# intentionally omitted. A nightly-fmt CI job is a separate roadmap
# follow-up that will be evaluated once real crates (mango-proto,
# mango-storage) exist, so the decision is made on a real corpus.

edition = "2021"
newline_style = "Unix"
use_field_init_shorthand = true
```

Rationale per setting:

- `edition = "2021"` — sets the _parser_ edition rustfmt uses when
  invoked without a surrounding Cargo invocation. An editor that runs
  `rustfmt` directly (without `cargo fmt`) defaults the parser edition
  to 2015, which fails on `?`, `async`/`.await`, and other post-2015
  syntax. Setting it explicitly here fixes that failure mode. (The
  _style_ edition is a separate `style_edition` key; `"2021"` style is
  what stable currently ships as the default, and `"2024"` is nightly-
  only, so we inherit the correct 2021 style implicitly.)
- `newline_style = "Unix"` — LF line endings, not `"Native"`. Default
  is `Auto` (preserves whatever the file already has). `"Native"` picks
  the host OS's convention, which means a Windows contributor can
  legitimately commit CRLF and CI on Linux accepts it — defeating the
  contract. `"Unix"` is the right answer for a project that wants one
  line-ending everywhere and matches `.editorconfig` + `.gitattributes`.
- `use_field_init_shorthand = true` — `Foo { x }` over `Foo { x: x }`.
  Reduces visual noise and is standard Rust style.

Intentionally **not** set:

- `max_width` — default 100 is fine; changing it causes wholesale
  reflow churn for no gain.
- `tab_spaces` / `hard_tabs` — defaults (4 / false) are the Rust norm.
- `reorder_imports` — already `true` by default.
- `use_try_shorthand` — `try!()` has been soft-deprecated since the
  2018 edition and no one writes it; setting this option is cargo-cult
  (tokio, hyper, sled, rust-analyzer, ripgrep, and rustc itself all
  ship without it). Keep the config tight.
- `use_small_heuristics = "Max"` — a real aesthetic choice (denser
  single-line match/struct-literal/if that ripgrep and rust-analyzer
  ship). Not adopted here because it's a style commitment that affects
  every future PR's diff, and mango has not yet committed to a
  "dense" vs "vertical" aesthetic. Revisit when there is real code to
  format.
- `unstable_features = true` — unlocks the nightly-only options _but
  only on nightly rustfmt_; on stable its behavior is under-documented
  and has drifted between releases. Not worth the risk until a
  nightly-fmt CI job is adopted deliberately.

### `.editorconfig`

```ini
# EditorConfig — https://editorconfig.org
# Keeps whitespace and line endings consistent across editors and OSes.

root = true

[*]
charset = utf-8
end_of_line = lf
indent_style = space
indent_size = 4
insert_final_newline = true
trim_trailing_whitespace = true

[*.{yml,yaml,toml,json}]
indent_size = 2

[*.md]
indent_size = 2
# Trailing two-space is a Markdown hard line break — don't strip it.
trim_trailing_whitespace = false

[Makefile]
indent_style = tab
```

Rationale:

- `root = true` — stops editor config lookup at the repo root.
- `[*]` block matches every file, setting LF, UTF-8, trailing-newline,
  trim-trailing-whitespace — the universal defaults.
- YAML / TOML / JSON get 2-space indent (ecosystem convention).
- Markdown gets 2-space indent (GFM nested-list convention: `- ` is
  2 chars, so list nesting is 2-space by default in prettier / dprint /
  markdownlint). Preemptive to avoid a future reformat of `ROADMAP.md`.
- Markdown opt-out on trailing-whitespace trim, because two trailing
  spaces is how CommonMark spells a hard line break; stripping them
  silently reflows docs.
- `[Makefile]` uses tabs because `make` actually requires it. Repo has
  no Makefile today; present so whoever adds one later doesn't have
  to relearn this.

### `.gitattributes`

```gitattributes
# Normalize text files to LF at checkout and at commit, regardless of
# contributor platform. Paired with `.editorconfig` (enforces at edit
# time) and `rustfmt.toml` (enforces for .rs via `cargo fmt --check`).
# Without this file, `.editorconfig` is advisory for non-Rust files —
# a Windows contributor whose editor ignores EditorConfig can still
# commit CRLF in .yml / .toml / .md, and CI wouldn't catch it because
# the YAML / TOML / Markdown parsers in use don't care about line
# endings.

* text=auto eol=lf

# Source and config files — explicit LF.
*.rs   text eol=lf
*.toml text eol=lf
*.yml  text eol=lf
*.yaml text eol=lf
*.json text eol=lf
*.md   text eol=lf

# Binary assets — never normalized, never diffed.
*.png  binary
*.jpg  binary
*.jpeg binary
*.ico  binary
*.webp binary
*.pdf  binary
```

Rationale:

- `* text=auto eol=lf` is the belt-and-suspenders default: git decides
  whether a file is text, and if so, normalizes to LF on checkin and
  checkout.
- Explicit `text eol=lf` for known source/config extensions makes the
  intent unambiguous and protects against `text=auto`'s heuristic
  mislabelling an edge-case file.
- `binary` rules prevent git from attempting text normalization on
  images and PDFs (no such files today, but the project will
  eventually have at least a logo / architecture diagram).

## Files to touch

- `rustfmt.toml` — new file, workspace root.
- `.editorconfig` — new file, workspace root.
- `.gitattributes` — new file, workspace root.

That's it. No code changes, no workflow changes, no README changes.

## Edge cases

- **Existing code reflow** — the only Rust file is
  `crates/mango/src/lib.rs` (17 lines). `cargo fmt --all` will be run
  locally before commit; any reformat from the new config goes in the
  same commit as the config. Expected diff: none — the placeholder
  crate uses no field-init patterns.
- **Line-ending rewrites via `.gitattributes`** — git normalizes text
  files on the next commit that touches them. Repo is LF-only today
  (macOS/Linux author), so adding `.gitattributes` is a no-op for
  existing files. To confirm, run `git add --renormalize .` in the
  implementation step; it should report no changes.
- **Editor support asymmetry** — EditorConfig is supported natively by
  IntelliJ/RustRover, VS Code (with extension), Vim (with plugin),
  Emacs (with package), Zed. Contributors without EditorConfig support
  still get enforced correctness via `cargo fmt --check` (for `.rs`)
  and `.gitattributes` (for everything else). EditorConfig is a
  convenience layer, not the gate.
- **Interaction with `cargo fmt --check`** — the existing CI job
  already runs `cargo fmt --all -- --check` against the workspace.
  With this PR, it now checks against the explicit config, not
  rustfmt defaults. If the explicit config disagrees with defaults
  for any existing file, the check would fail; handled by running
  `cargo fmt` locally in the implementation step.
- **Future generated code (e.g., `tonic-build`)** — when
  `crates/mango-proto/` lands, `tonic-build` usually emits generated
  files under `OUT_DIR` (outside the workspace tree, untouched by
  `cargo fmt`). If future plans opt into `src/generated/` style, that
  PR will need a `rustfmt.toml` `ignore = [...]` entry or a file-level
  `#[rustfmt::skip]`. Not this PR's problem; noted on the proto plan
  so that author doesn't get a surprise diff.
- **Stable-rustfmt output drift** — `cargo fmt` is only byte-stable
  for a given rustfmt version. CI floats `stable`, so a future rustfmt
  release can (rarely) reformat unchanged code and fail the `fmt`
  check. Accepted: the remediation is a trivial `cargo fmt` commit
  when it happens. This is a known tradeoff in every Rust project that
  doesn't pin its toolchain.

## Test strategy

- Locally: `cargo fmt --all` — confirm diff is empty (or commit any
  reformat in the same atomic commit).
- Locally: `cargo fmt --all -- --check` — confirm exit 0.
- Locally: `git add --renormalize .` after adding `.gitattributes` —
  confirm no unexpected line-ending rewrites on existing files.
- Locally: `cargo clippy --workspace --all-targets --locked -- -D warnings`
  and `cargo test --workspace --all-targets --locked` — confirm
  unrelated CI jobs still pass (sanity check, no logical reason they
  would break).
- CI: open PR, observe `fmt` / `clippy` / `test` green.

## Rollback

Additive, pure config. Revert the single commit; CI behavior returns
to rustfmt defaults and git's default text-handling.

## Out of scope (explicit, do not do in this PR)

- Nightly-only rustfmt options (`imports_granularity`, `group_imports`,
  `wrap_comments`, `style_edition = "2024"`, ...). Filed as a roadmap
  follow-up that will evaluate on a real corpus once `mango-proto` /
  `mango-storage` exist; that decision includes whether to pin a
  nightly toolchain at all.
- `use_small_heuristics = "Max"` — taste-level aesthetic commit; defer
  until there is code that would demonstrate the trade-off.
- Pre-commit hook that runs `cargo fmt --check`. Useful but out of
  scope for Phase 0; belongs in a dev-ergonomics item.
- Formatting policy doc in `CONTRIBUTING.md` — `CONTRIBUTING.md`
  itself is a later Phase 0 item (`ROADMAP.md:761`). The formatting
  rules will land there when that item is picked.

## Disagreements with reviewer

- Reviewer's nit #6 suggested considering `use_small_heuristics =
"Max"`. **Not adopted.** Reason: the plan explicitly avoids style
  commitments with codebase-wide consequences while the codebase is
  17 lines. Revisit when real code exists.
- Reviewer's nit #2 suggested either dropping `use_try_shorthand` or
  keeping it with a tighter rationale. **Dropped.** Agreed with the
  "dead option" framing.

All other reviewer points (Bugs commentary, Risk #1 rewording, Missing
#4 `.gitattributes`, Nit #9 `[*.md] indent_size`, Nit #7 Unix vs
Native) folded into the plan above.
