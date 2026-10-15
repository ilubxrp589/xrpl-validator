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

    // The commit in version::PEER_VERSION: BUILD_COMMIT as `git archive` wrote it (the deploy build), else the
    // checkout's HEAD (gate and dev builds), else "dev".
    println!("cargo:rerun-if-changed=BUILD_COMMIT");
    let stamped = std::fs::read_to_string("BUILD_COMMIT").unwrap_or_default();
    let stamped = stamped.trim();
    let commit = if !stamped.is_empty() && !stamped.contains('$') {
        stamped.to_string()
    } else {
        std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "dev".to_string())
    };
    println!("cargo:rustc-env=XRPL_BUILD_COMMIT={commit}");
    Ok(())
}
