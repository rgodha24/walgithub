fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/walgit/v1/wal.proto");
    let mut cfg = prost_build::Config::new();
    cfg.bytes(["."]);
    cfg.compile_protos(&["proto/walgit/v1/wal.proto"], &["proto"])?;
    Ok(())
}
