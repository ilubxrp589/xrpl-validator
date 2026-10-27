//! Independence Step 8's switch: turn a proven `peer_state_sync` run into the store live_viewer resumes from.
//!
//!   leaves_to_rocks --dir DIR
//!
//! DIR is `peer_state_sync --out DIR` after it printed "ROOT PROVEN" (proven.json, header.bin, leaves.bin). The
//! store, the resume ticket and the markers go where live_viewer looks for them (`XRPL_DATA_DIR`, `XRPL_ROCKS_PATH`;
//! [`xrpl_node::paths`]). Nothing is written over an existing store. Then start live_viewer with
//! `XRPL_WARM_RESUME=1 XRPL_LEDGER_SOURCE=peers`: it rebuilds the root, holds it against the peers' account hash for
//! that ledger, and catches up from peers.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = match args.iter().position(|a| a == "--dir").and_then(|i| args.get(i + 1)) {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            eprintln!("usage: leaves_to_rocks --dir DIR   (a peer_state_sync --out DIR that printed ROOT PROVEN)");
            std::process::exit(2);
        }
    };
    let to = xrpl_node::cold_start::Targets::from_env();
    eprintln!("[cold-start] {} -> {}", dir.display(), to.rocks.display());
    let t0 = std::time::Instant::now();
    match xrpl_node::cold_start::convert(&dir, &to) {
        Ok(r) => println!(
            "COLD READY #{} root {} — {} objects in {:.0} s; start live_viewer with XRPL_WARM_RESUME=1 XRPL_LEDGER_SOURCE=peers",
            r.seq,
            hex::encode_upper(r.account_hash),
            r.objects,
            t0.elapsed().as_secs_f64()
        ),
        Err(e) => {
            eprintln!("[cold-start] REFUSED: {e}");
            std::process::exit(1);
        }
    }
}
