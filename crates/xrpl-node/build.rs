fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("proto");

    prost_build::Config::new()
        .out_dir("src/peer/")
        .compile_protos(&[proto_dir.join("xrpl.proto")], &[&proto_dir])?;

    println!("cargo:rerun-if-changed=../../proto/xrpl.proto");
    Ok(())
}
