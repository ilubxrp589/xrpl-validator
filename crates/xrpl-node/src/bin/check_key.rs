use rocksdb::{Options, DB};

fn main() {
    let key_hex = std::env::args().nth(1).expect("usage: check_key <hex_key> [db_path]");
    let path = std::env::args().nth(2).unwrap_or_else(|| "/mnt/xrpl-data/sync/state.rocks".to_string());
    let key_bytes = hex::decode(&key_hex).expect("decode key");
    let opts = Options::default();
    let db = DB::open_for_read_only(&opts, &path, false).expect("open db");
    match db.get(&key_bytes) {
        Ok(Some(v)) => println!("FOUND in state.rocks: {} bytes", v.len()),
        Ok(None) => println!("MISSING from state.rocks"),
        Err(e) => println!("ERROR: {e}"),
    }
}
