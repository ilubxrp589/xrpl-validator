//! build.rs — tells cargo where to find libxrpl_shim.a and all transitive libs.
//!
//! Assumes: `ffi/build/` has been built via `cd ffi && cmake --build build`.
//! That produces libxrpl_shim.a + links against libxrpl.a and conan-provided deps.

use std::path::PathBuf;

fn main() {
    // Locate the FFI build directory relative to this crate.
    // xrpl-ffi is at crates/xrpl-ffi/, ffi/build is at ../../ffi/build/
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ffi_dir = manifest.parent().unwrap().parent().unwrap().join("ffi");
    let build_dir = ffi_dir.join("build");
    let rippled_build = ffi_dir.join("vendor/rippled/.build");

    if !build_dir.join("libxrpl_shim.a").exists() {
        panic!(
            "libxrpl_shim.a not found at {}. Run: cd ffi && cmake --build build",
            build_dir.display()
        );
    }

    // Static libs from our build
    println!("cargo:rustc-link-search=native={}", build_dir.display());
    println!("cargo:rustc-link-search=native={}", rippled_build.display());

    // libxrpl_shim wraps libxrpl + libxrpl.libpb
    println!("cargo:rustc-link-lib=static=xrpl_shim");
    println!("cargo:rustc-link-lib=static=xrpl");
    println!("cargo:rustc-link-lib=static=xrpl.libpb");

    // ed25519 and secp256k1 are built from rippled's external/ subdirs
    let ed25519_dir = rippled_build.join("external/ed25519-donna");
    let secp256k1_dir = rippled_build.join("external/secp256k1/lib");
    println!("cargo:rustc-link-search=native={}", ed25519_dir.display());
    println!("cargo:rustc-link-search=native={}", secp256k1_dir.display());
    println!("cargo:rustc-link-lib=static=ed25519");
    println!("cargo:rustc-link-lib=static=secp256k1");

    // Conan-provided deps. Paths are inside the conan cache. We extract them
    // from the conan-generated cmake data file at build time.
    let conan_gen = rippled_build.join("build/generators");
    collect_conan_libs(&conan_gen);

    // System libs
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rustc-link-lib=m");

    // Rerun if shim changes
    println!("cargo:rerun-if-changed={}", build_dir.join("libxrpl_shim.a").display());
}

/// Parse conan-generated cmake files to extract library names + paths.
/// Scans ALL `*-release-x86_64-data.cmake` files (except `module-*` which
/// are aliases for the main ones).
fn collect_conan_libs(conan_gen: &std::path::Path) {
    use std::collections::HashSet;
    use std::fs;

    let mut lib_dirs: HashSet<String> = HashSet::new();
    let mut libs: Vec<String> = Vec::new();

    let entries = match fs::read_dir(conan_gen) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with("-release-x86_64-data.cmake") {
            continue;
        }
        if name.starts_with("module-") {
            continue; // These are alias files for find_package compatibility
        }
        let content = fs::read_to_string(entry.path()).unwrap_or_default();
        for line in content.lines() {
            let line = line.trim();
            // Parse: set(<pkg>_PACKAGE_FOLDER_RELEASE "<path>")
            if line.contains("_PACKAGE_FOLDER_RELEASE") {
                if let Some(start) = line.find('"') {
                    if let Some(end) = line.rfind('"') {
                        if end > start {
                            let path = &line[start + 1..end];
                            lib_dirs.insert(format!("{path}/lib"));
                        }
                    }
                }
            }
            // Parse: set(<pkg>_LIBS_RELEASE lib1 lib2 ...)
            // Skip SYSTEM_LIBS / DEPS_LIBS / OBJECTS / FRAMEWORKS
            if line.contains("_LIBS_RELEASE")
                && !line.contains("_SYSTEM_LIBS")
                && !line.contains("_DEPS_LIBS")
                && !line.contains("_FRAMEWORKS")
                && !line.contains("_OBJECTS")
            {
                if let Some(start) = line.find("_LIBS_RELEASE") {
                    let rest = &line[start + "_LIBS_RELEASE".len()..];
                    let rest = rest.trim_end_matches(')').trim();
                    for lib in rest.split_whitespace() {
                        if !lib.is_empty() {
                            libs.push(lib.to_string());
                        }
                    }
                }
            }
            // Emit system libs as dynamic
            if line.contains("_SYSTEM_LIBS_RELEASE") {
                if let Some(start) = line.find("_SYSTEM_LIBS_RELEASE") {
                    let rest = &line[start + "_SYSTEM_LIBS_RELEASE".len()..];
                    let rest = rest.trim_end_matches(')').trim();
                    for lib in rest.split_whitespace() {
                        if !lib.is_empty() && !["pthread", "dl", "m"].contains(&lib) {
                            println!("cargo:rustc-link-lib={lib}");
                        }
                    }
                }
            }
        }
    }

    for dir in &lib_dirs {
        println!("cargo:rustc-link-search=native={dir}");
    }

    // Exclusions:
    // - boost_test_exec_monitor / boost_prg_exec_monitor / boost_unit_test_framework
    //   define main() → skip.
    // - rocksdb/snappy/lz4/zlib: xrpl-node's rocksdb crate ships its own C++
    //   rocksdb; linking conan's copy causes duplicate symbols.
    let exclude_prefixes = [
        "boost_test_exec_monitor",
        "boost_prg_exec_monitor",
        "boost_unit_test_framework",
        "rocksdb",
        "ssl",
        "crypto",
    ];
    let mut emitted: HashSet<String> = HashSet::new();
    for lib in &libs {
        if exclude_prefixes.iter().any(|p| lib.starts_with(p)) {
            continue;
        }
        if emitted.insert(lib.clone()) {
            println!("cargo:rustc-link-lib=static={lib}");
        }
    }
}
