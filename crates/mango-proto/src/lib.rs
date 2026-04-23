//! Mango — generated protobuf bindings.
//!
//! Phase 0 skeleton (ROADMAP.md:761). Real wire surfaces land in
//! Phase 6+. The `hello::v0` module ships with a single
//! request/reply pair solely to exercise the `tonic-build`
//! pipeline end-to-end under the workspace lint and MSRV regime.
//! It is **not** wire-stable and will be deleted when the first
//! real `mango.*.v1` proto lands.

#![deny(missing_docs)]
// `publish = false` and forward-looking: prost-emitted enums are
// never `#[non_exhaustive]`, and retrofitting post-codegen is
// brittle. Opt the whole crate out of the workspace
// `clippy::exhaustive_enums = "deny"` policy. When `mango-proto`
// becomes publishable in Phase 6+, this allow stays (generated-
// code exception in `docs/api-stability.md`).
#![allow(clippy::exhaustive_enums)]

/// Namespace root for `mango.hello.*` protobuf services. Phase-0
/// smoke surface only; real versioned namespaces (`mango.kv.v1`,
/// `mango.raft.v1`, …) land in Phase 6+.
pub mod hello {
    /// Version 0 of the hello service. `v0` is a pipeline smoke
    /// test, NOT a wire-stable surface — it will be deleted when
    /// the first real `mango.*.v1` service lands.
    pub mod v0 {
        // Generated code is foreign — we do not own the style. Every
        // workspace-denied lint that prost's expansion could plausibly
        // trip (now or in a future point release) is enumerated here.
        // The `clippy::all` / `clippy::pedantic` / `clippy::nursery`
        // group allows are priority-0; the workspace table sets the
        // individual denies at priority-1. Priority-1 deny wins over
        // priority-0 allow, so the individual denies MUST be listed
        // explicitly — the group allows are kept only for
        // future-proofing against new lints in those groups.
        //
        // The `rustdoc::bare_urls` and `rustdoc::broken_intra_doc_links`
        // allows guard against future prost-build versions embedding
        // `.proto` comment URLs or cross-references. The outer
        // `-D warnings` in the doc gate does NOT silence those two
        // from inside this module without the explicit allow — a
        // dep bump would otherwise red the `doc` job with no source
        // change from us.
        #![allow(
            clippy::all,
            clippy::pedantic,
            clippy::nursery,
            clippy::arithmetic_side_effects,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::indexing_slicing,
            clippy::disallowed_types,
            clippy::incompatible_msrv,
            clippy::unwrap_used,
            clippy::expect_used,
            clippy::panic,
            clippy::unimplemented,
            clippy::todo,
            clippy::dbg_macro,
            clippy::print_stdout,
            clippy::print_stderr,
            clippy::await_holding_lock,
            clippy::await_holding_refcell_ref,
            unreachable_pub,
            missing_docs,
            rustdoc::bare_urls,
            rustdoc::broken_intra_doc_links
        )]
        include!(concat!(env!("OUT_DIR"), "/mango.hello.v0.rs"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unnecessary_literal_unwrap,
        clippy::arithmetic_side_effects
    )]

    use super::hello::v0::{HelloReply, HelloRequest};

    // Compile-time assertion that prost-derive emits the expected
    // trait bounds. If a future prost-build config change strips a
    // derive (e.g., omits `Default`), this const fails to typecheck
    // and the build breaks loudly at PR time — cheaper than
    // discovering the regression from a field using the method.
    const _: fn() = || {
        fn assert_bounds<T>()
        where
            T: Clone + Default + std::fmt::Debug + PartialEq + prost::Message,
        {
        }
        assert_bounds::<HelloRequest>();
        assert_bounds::<HelloReply>();
    };

    #[test]
    fn hello_types_roundtrip_via_prost() {
        // Exercise the generated types: construct, serialize, round-
        // trip. Proves (a) codegen ran, (b) the types implement
        // prost::Message, (c) encoding/decoding are wired correctly.
        use prost::Message;

        let req = HelloRequest {
            name: "mango".to_string(),
        };
        let mut buf = Vec::new();
        req.encode(&mut buf).expect("encode HelloRequest");

        let decoded = HelloRequest::decode(buf.as_slice()).expect("decode HelloRequest");
        assert_eq!(decoded.name, "mango");

        let reply = HelloReply {
            message: "hello, mango".to_string(),
        };
        assert_eq!(reply.message, "hello, mango");
    }
}
