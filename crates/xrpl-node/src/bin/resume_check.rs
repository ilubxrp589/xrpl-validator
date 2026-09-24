//! resume_check — rehearse the warm-restart check on a kept `state.rocks`, read-only.
//!
//! Usage: `resume_check <path/to/state.rocks>`; the RPC source is `XRPL_RPC_URL`, as for
//! live_viewer. Reads the bookmark, rebuilds the ws-sync hasher's root and compares it with the
//! network's account hash for that ledger. Exit 0 = the state is that ledger; 1 = it is not;
//! 2 = usage or open error.
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: resume_check <path/to/state.rocks>");
        std::process::exit(2);
    };
    let t0 = std::time::Instant::now();
    let db = match rocksdb::DB::open_for_read_only(&rocksdb::Options::default(), &path, false) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(2);
        }
    };
    let open_secs = t0.elapsed().as_secs_f64();
    let hash_comp = Arc::new(xrpl_node::state_hash::StateHashComputer::new());
    let net = xrpl_node::resume::RpcNetwork::new();
    match xrpl_node::resume::verify_state_at_bookmark(&net, &db, &hash_comp).await {
        Ok(v) => println!(
            "RESUME-CHECK OK: #{} root {} equals the network account_hash; {} entries; open {open_secs:.1}s, rebuild {:.1}s; {} ledgers behind the network now",
            v.seq, v.root, v.entries, v.build_secs, v.gap
        ),
        Err(e) => {
            println!("RESUME-CHECK FAIL: {e} (open {open_secs:.1}s)");
            std::process::exit(1);
        }
    }
}
