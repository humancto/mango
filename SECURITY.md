# Security policy

## Status

Mango is **pre-alpha**. It is not recommended for production use. The
threat model formalized in `ROADMAP.md` north-star bar #6 (Security —
Defense-in-depth) is a **target**, not a current commitment.

We do not currently offer a bug bounty.

## Reporting a vulnerability

Email **archith.rapaka@gmail.com** with a subject line beginning
`[mango-security]`. Do not file a public GitHub issue for security
matters until the fix has shipped.

If you prefer encrypted disclosure, request a PGP key in your initial
mail and we will exchange one out-of-band.

## Scope

In scope:

- Code in this repository (`crates/`, `tests/`, `benches/`, `scripts/`).
- Build- and supply-chain artifacts produced by this repository's CI.
- Documentation that, if followed, would lead a user to an insecure
  configuration.

Out of scope:

- Vulnerabilities in third-party dependencies. Track those via
  [`cargo-audit`](https://crates.io/crates/cargo-audit); we ingest
  advisories via the supply-chain workflow described in
  [`docs/supply-chain-policy.md`](./docs/supply-chain-policy.md).
- Vulnerabilities in upstream etcd. Mango is a port, not a fork — etcd
  is a separate project with its own security process.
- Findings from automated scanners without a working reproducer.

## Response timeline (best-effort, pre-1.0)

- **Acknowledgement**: within 7 days of receipt.
- **Triage and impact assessment**: within 14 days.
- **Fix or documented workaround**: within 90 days for high/critical
  severity; lower severities are batched into the next minor release.
- **Public disclosure**: jointly with the reporter, after the fix has
  landed and a release is cut. Reporters are credited unless they
  request anonymity.

We do not guarantee these timelines pre-1.0. The project ships when
correctness, not calendars, says it ships.

## Known limitations

- No `unsafe` blocks except in modules listed in
  [`docs/unsafe-policy.md`](./docs/unsafe-policy.md). Memory-safety
  bugs in safe Rust are reportable.
- Cryptographic primitives are deferred to vetted crates listed in
  `Cargo.toml`. Cryptographic agility is a Phase 8 concern.
- The Threat Model document (Phase 12 milestone) does not yet exist.
  Until it does, the security posture is "what Rust gives us by
  construction, plus the lints in `clippy.toml` and the deny rules in
  `deny.toml`."

## License

This security policy is part of the Mango project and licensed under
the Apache License, Version 2.0.
