//! Debug: check an account's presence and balance in state.rocks
//! Usage: debug_balance <address>

fn main() {
    let addr = std::env::args().nth(1).expect("need address");
    let id = xrpl_node::engine::decode_address(&addr).expect("decode address");
    let key = xrpl_ledger::ledger::keylet::account_root_key(&id);
    println!("address:     {addr}");
    println!("account_id:  {}", hex::encode(id).to_uppercase());
    println!("keylet:      {}", hex::encode(key.0).to_uppercase());

    let db = rocksdb::DB::open_for_read_only(
        &rocksdb::Options::default(),
        "/mnt/xrpl-data/sync/state.rocks",
        false,
    )
    .expect("open db");

    match db.get(key.0) {
        Ok(Some(data)) => {
            println!("FOUND in DB: {} bytes", data.len());
            println!("  FULL hex ({} bytes): {}", data.len(), hex::encode(&data));
            // Scan for all 0x62 bytes (sfBalance field code)
            let mut found_any = false;
            for i in 7..data.len().saturating_sub(9) {
                if data[i] == 0x62 {
                    let amt_bytes: [u8; 8] = data[i + 1..i + 9].try_into().unwrap();
                    let raw = u64::from_be_bytes(amt_bytes);
                    let is_iou = raw & 0x8000000000000000 != 0;
                    let is_pos = raw & 0x4000000000000000 != 0;
                    let drops = raw & 0x3FFFFFFFFFFFFFFF;
                    println!(
                        "  0x62 at offset {}: raw=0x{:016x} iou={} pos_xrp={} drops={}",
                        i, raw, is_iou, is_pos, drops
                    );
                    found_any = true;
                }
            }
            if !found_any {
                println!("  NO 0x62 byte found!");
            }
            // Try the extraction function
            match xrpl_node::engine::extract_xrp_balance_pub(&data) {
                Some(b) => println!("extract_xrp_balance_pub: {b}"),
                None => println!("extract_xrp_balance_pub: None (BUG)"),
            }
        }
        Ok(None) => println!("NOT FOUND in DB (BUG - account should be there)"),
        Err(e) => println!("DB error: {e}"),
    }
}
