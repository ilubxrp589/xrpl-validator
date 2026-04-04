//! Import a clean JSON snapshot into RocksDB.
//! Usage: import_snapshot <snapshot_dir> <rocks_db_path>

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let snapshot_dir = args.get(1).map(|s| s.as_str()).unwrap_or("xrpl_clean_snapshot");
    let db_path = args.get(2).map(|s| s.as_str()).unwrap_or("/mnt/xrpl-data/sync/state.rocks");

    eprintln!("Opening RocksDB at {db_path}...");
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(true);
    opts.set_max_open_files(5000);
    opts.set_write_buffer_size(64 * 1024 * 1024);
    let db = rocksdb::DB::open(&opts, db_path).expect("Failed to open RocksDB");

    // Find all batch files
    let mut files: Vec<_> = std::fs::read_dir(snapshot_dir)
        .expect("Failed to read snapshot dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .map(|e| e.path())
        .collect();
    files.sort();

    eprintln!("Found {} batch files in {snapshot_dir}", files.len());

    let mut total: u64 = 0;
    let start = std::time::Instant::now();

    for (i, path) in files.iter().enumerate() {
        let data = std::fs::read_to_string(path).expect("Failed to read batch file");
        let state: Vec<serde_json::Value> = serde_json::from_str(&data).expect("Failed to parse JSON");

        let mut batch = rocksdb::WriteBatch::default();
        for obj in &state {
            let index = match obj["index"].as_str() {
                Some(s) if s.len() == 64 => s,
                _ => continue,
            };
            let data_hex = match obj["data"].as_str() {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            if let (Ok(key), Ok(val)) = (hex::decode(index), hex::decode(data_hex)) {
                if key.len() == 32 {
                    batch.put(&key, &val);
                    total += 1;
                }
            }
        }
        let _ = db.write(batch);

        if (i + 1) % 100 == 0 || i < 5 || i == files.len() - 1 {
            let elapsed = start.elapsed().as_secs_f64();
            let rate = total as f64 / elapsed.max(0.1);
            eprintln!(
                "  {}/{} files — {} objects — {:.0}/s — {:.0}s",
                i + 1, files.len(), total, rate, elapsed
            );
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "\nDONE! Imported {} objects in {:.0}s ({:.0}/s)",
        total, elapsed, total as f64 / elapsed
    );

    // Write sync marker
    let marker = "/mnt/xrpl-data/sync/sync_complete.marker";
    std::fs::write(marker, format!("{total}")).expect("Failed to write marker");
    eprintln!("Wrote sync marker: {marker}");
}
