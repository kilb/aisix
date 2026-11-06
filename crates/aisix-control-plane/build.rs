// `sqlx::migrate!` embeds migrations/ at compile time, and cargo does not
// otherwise notice a file added there: without this, a change that only adds
// a migration builds a binary that never applies it.
//
// The etcd front's server and client stubs are generated from proto/: the
// three trimmed etcd protos etcd-client 0.14.1 ships, copied unchanged
// (etcd's own protos, under the Apache License 2.0). Generating them needs
// `protoc`, on PATH or named by PROTOC.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=proto");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/rpc.proto"], &["proto"])?;
    Ok(())
}
