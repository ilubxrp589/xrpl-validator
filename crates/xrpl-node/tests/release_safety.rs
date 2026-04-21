//! CI guard — fails the build if any shipped config/template commits values
//! that should be operator-specific (seeds, non-local IPs, tokens, etc.).
//!
//! Adapted from xLedgRS/tests/release_safety.rs, scoped to this repo's layout.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // crate dir is crates/xrpl-node; walk up twice to the workspace root.
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

/// All checked-in config/template files. Extend as needed.
fn checked_templates() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let root = repo_root();

    // Look in a future `cfg/` directory if you add one (modeled on xLedgRS/cfg).
    if let Ok(rd) = fs::read_dir(root.join("cfg")) {
        for entry in rd.flatten() {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "cfg" | "toml" | "conf") {
                files.push(p);
            }
        }
    }

    // Catch any `config.example.*` at the root too.
    for name in &["config.example.toml", "config.toml.example", "example.cfg"] {
        let p = root.join(name);
        if p.exists() {
            files.push(p);
        }
    }

    files.sort();
    files
}

fn uncommented_lines_in_section(contents: &str, section: &str) -> Vec<(usize, String)> {
    let header = format!("[{section}]");
    let mut in_block = false;
    let mut offending = Vec::new();
    for (idx, line) in contents.lines().enumerate() {
        let t = line.trim();
        if t == header {
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') {
            in_block = false;
            continue;
        }
        offending.push((idx + 1, t.to_string()));
    }
    offending
}

fn non_local_ipv4s(contents: &str) -> Vec<String> {
    const ALLOWED: &[&str] = &["0.0.0.0", "127.0.0.1", "255.255.255.255"];
    let mut found = Vec::new();
    for line in contents.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        for tok in t.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
            if tok.is_empty() || ALLOWED.contains(&tok) {
                continue;
            }
            let parts: Vec<&str> = tok.split('.').collect();
            if parts.len() != 4 {
                continue;
            }
            if parts.iter().all(|p| !p.is_empty() && p.parse::<u8>().is_ok()) {
                found.push(tok.to_string());
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn no_uncommented_validation_seed() {
    for path in checked_templates() {
        let s = read_text(&path);
        let bad = uncommented_lines_in_section(&s, "validation_seed");
        assert!(
            bad.is_empty(),
            "{} ships uncommented [validation_seed]: {:?}",
            path.display(),
            bad
        );
    }
}

#[test]
fn no_uncommented_validator_token() {
    for path in checked_templates() {
        let s = read_text(&path);
        let bad = uncommented_lines_in_section(&s, "validator_token");
        assert!(
            bad.is_empty(),
            "{} ships uncommented [validator_token]: {:?}",
            path.display(),
            bad
        );
    }
}

#[test]
fn no_hardcoded_non_local_ipv4() {
    for path in checked_templates() {
        let s = read_text(&path);
        let ips = non_local_ipv4s(&s);
        assert!(
            ips.is_empty(),
            "{} contains hardcoded non-local IPs: {:?}",
            path.display(),
            ips
        );
    }
}

/// No raw .hex / .seed / validator_seed files should be tracked by git at all.
#[test]
fn no_seed_files_committed() {
    let forbidden_names = [
        "validator_seed.hex",
        "validator_seed",
        "keys.json",
        "validator_keys.json",
        "validator_token.txt",
    ];
    let root = repo_root();
    for name in &forbidden_names {
        let p = root.join(name);
        assert!(
            !p.exists(),
            "Forbidden file present in repo: {}",
            p.display()
        );
    }
}
