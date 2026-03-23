//! One-time migration from sled to RocksDB.
use std::path::Path;

fn main() {
    let sled_path = Path::new("/mnt/xrpl-data/sync/state.sled");
    let rocks_path = Path::new("/mnt/xrpl-data/sync/state.rocks");

    match xrpl_node::engine::migrate_sled_to_rocksdb(sled_path, rocks_path) {
        Ok(n) => println!("Migration complete: {n} entries"),
        Err(e) => eprintln!("Migration failed: {e}"),
    }
}
