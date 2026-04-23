fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Types-only — no service trait generation. The hello proto has
    // no `service` block, but even if someone adds one by mistake
    // the build stays types-only (no runtime tonic dep) until Phase 6.
    tonic_build::configure()
        .build_server(false)
        .build_client(false)
        .compile_protos(&["proto/hello.proto"], &["proto"])?;
    Ok(())
}
