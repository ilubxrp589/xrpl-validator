//! Round-trip audit for one ledger entry: canonical binary -> our decoder ->
//! JSON -> canon_for_encode -> our encoder -> compare to the original bytes.
//! The shadow mirror lives on exactly this pipeline; a non-round-tripping
//! object diverges in the live shadow while the RPC-JSON-hydrated probe world
//! stays clean (finding-38 signature, 2026-08-31).
//!
//! Usage: cargo run --release -p xrpl-node --example roundtrip_entry -- <hexfile>

fn main() {
    let path = std::env::args().nth(1).expect("usage: roundtrip_entry <hexfile>");
    let hex_str = std::fs::read_to_string(&path).expect("read hexfile");
    let bytes = hex::decode(hex_str.trim()).expect("hex decode");
    println!("input: {} bytes", bytes.len());

    let mut jv = match xrpl_core::codec::decode::decode_transaction_binary(&bytes) {
        Ok(v) => v,
        Err(e) => {
            println!("DECODE ERROR: {e:?}");
            std::process::exit(2);
        }
    };
    xrpl_node::native_apply::hexify_addresses(&mut jv);
    println!("decoded JSON: {}", serde_json::to_string_pretty(&jv).unwrap_or_default());

    let mut canon = jv.clone();
    xrpl_node::native_apply::canon_for_encode(&mut canon);
    match xrpl_core::codec::encode::encode_transaction_json(&canon, false) {
        Ok(out) => {
            if out == bytes {
                println!("ROUNDTRIP: EXACT ({} bytes)", out.len());
            } else {
                let off = out
                    .iter()
                    .zip(bytes.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or(out.len().min(bytes.len()));
                println!(
                    "ROUNDTRIP: DIFF @ {off} — ours-len {} orig-len {}",
                    out.len(),
                    bytes.len()
                );
                let s = off.saturating_sub(8);
                println!("orig[{}..{}]: {}", s, (off + 12).min(bytes.len()), hex::encode_upper(&bytes[s..(off + 12).min(bytes.len())]));
                println!("ours[{}..{}]: {}", s, (off + 12).min(out.len()), hex::encode_upper(&out[s..(off + 12).min(out.len())]));
                std::process::exit(1);
            }
        }
        Err(e) => {
            println!("ENCODE ERROR: {e:?}");
            std::process::exit(2);
        }
    }
}
