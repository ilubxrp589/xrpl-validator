//! Independence 4.1 probe: fetch validated ledgers from ONE mainnet peer over the peer protocol, prove each
//! to its ledger hash (header, then every transaction-tree node), and compare its transactions and records
//! with an RPC server's copy, byte for byte. A standalone tool: it opens its own peer connection, with a
//! fresh node identity, and never touches the validator.
//!
//!   peer_fetch_probe [--peer s1.ripple.com:51235] [--rpc https://s2.ripple.com:51234] [--count 5]
//!                    [--ledger SEQ] [--record DIR]
//!
//! --count ledgers counting back from the latest validated one (or from --ledger), one at a time and at
//! most one request every 2 s, so a peer's resource charge (250 a request, warning at 5,000 over its 32 s
//! window) stays near 4,000 at worst. --record writes each reply, as protobuf, for test fixtures.

use std::time::{Duration, Instant};

use bytes::BytesMut;
use prost::Message as _;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::Decoder;
use xrpl_node::peer::codec::MessageCodec;
use xrpl_node::peer::message::PeerMessage;
use xrpl_node::peer_fetch::{compare, LedgerFetch, Step};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

async fn rpc(url: &str, body: Value) -> Result<Value, String> {
    let c = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let v: Value = c.post(url).json(&body).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    Ok(v["result"].clone())
}

/// The RPC server's copy: the ledger hash and every transaction blob with its metadata.
async fn their_copy(url: &str, seq: u32) -> Result<(String, Vec<(Vec<u8>, Vec<u8>)>), String> {
    let r = rpc(url, json!({"method": "ledger", "params": [{"ledger_index": seq, "transactions": true, "expand": true, "binary": true}]})).await?;
    let hash = r["ledger_hash"].as_str().ok_or("no ledger_hash")?.to_string();
    let mut pairs = Vec::new();
    for t in r["ledger"]["transactions"].as_array().ok_or("no transactions")? {
        let tx = hex::decode(t["tx_blob"].as_str().ok_or("no tx_blob")?).map_err(|e| e.to_string())?;
        let meta = hex::decode(t["meta"].as_str().ok_or("no meta")?).map_err(|e| e.to_string())?;
        pairs.push((tx, meta));
    }
    Ok((hash, pairs))
}

fn frame(msg: &PeerMessage) -> Vec<u8> {
    let p = msg.encode_to_vec();
    let mut f = Vec::with_capacity(6 + p.len());
    f.extend_from_slice(&(p.len() as u32).to_be_bytes());
    f.extend_from_slice(&msg.type_code().to_be_bytes());
    f.extend_from_slice(&p);
    f
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let peer = arg("--peer").unwrap_or_else(|| "s1.ripple.com:51235".into());
    let url = arg("--rpc").unwrap_or_else(|| "https://s2.ripple.com:51234".into());
    let count: u32 = arg("--count").and_then(|c| c.parse().ok()).unwrap_or(5);
    let record = arg("--record").map(std::path::PathBuf::from);
    let start = match arg("--ledger").and_then(|s| s.parse::<u32>().ok()) {
        Some(s) => s,
        None => match rpc(&url, json!({"method": "ledger", "params": [{"ledger_index": "validated"}]})).await {
            Ok(r) => r["ledger_index"].as_u64().or_else(|| r["ledger"]["ledger_index"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0) as u32,
            Err(e) => {
                eprintln!("RPC: {e}");
                std::process::exit(2);
            }
        },
    };
    if let Some(d) = &record {
        let _ = std::fs::create_dir_all(d);
    }
    let identity = xrpl_node::peer::identity::NodeIdentity::generate().expect("identity");
    let hs = match tokio::time::timeout(
        Duration::from_secs(20),
        xrpl_node::peer::handshake::outbound_handshake(&peer, &identity, xrpl_node::peer::handshake::NETWORK_ID_MAINNET),
    )
    .await
    {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            eprintln!("handshake with {peer}: {e}");
            std::process::exit(3);
        }
        Err(_) => {
            eprintln!("handshake with {peer}: timeout");
            std::process::exit(3);
        }
    };
    eprintln!("connected to {peer}; ledgers #{start} back {count}");
    let (mut rd, mut wr) = tokio::io::split(hs.stream);
    let mut buf = BytesMut::from(&hs.remaining_bytes[..]);
    let mut codec = MessageCodec;
    let (mut ok, mut bad) = (0u32, 0u32);
    let mut last_send = Instant::now() - Duration::from_secs(10);
    for seq in (start + 1 - count.min(start)..=start).rev() {
        let (hash_hex, theirs) = match their_copy(&url, seq).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("#{seq}: RPC copy: {e}");
                bad += 1;
                continue;
            }
        };
        let hash: [u8; 32] = match hex::decode(&hash_hex).ok().and_then(|b| b.try_into().ok()) {
            Some(h) => h,
            None => continue,
        };
        let t0 = Instant::now();
        let mut f = LedgerFetch::new(hash);
        let mut next = Some(f.base_request());
        let mut replies = 0u32;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut outcome = String::from("timeout");
        'fetch: while let Some(req) = next.take() {
            // At most one request every 2 s on this one connection.
            let wait = Duration::from_secs(2).saturating_sub(last_send.elapsed());
            tokio::time::sleep(wait).await;
            if wr.write_all(&frame(&PeerMessage::GetLedger(req))).await.is_err() {
                outcome = "write failed".into();
                break;
            }
            last_send = Instant::now();
            // Read until a reply for this ledger arrives (pings answered, everything else skipped).
            loop {
                if Instant::now() > deadline {
                    break 'fetch;
                }
                match codec.decode(&mut buf) {
                    Ok(Some(PeerMessage::LedgerData(d))) => {
                        if let Some(dir) = &record {
                            let _ = std::fs::write(dir.join(format!("{seq}-{replies}.ledger_data.pb")), d.encode_to_vec());
                        }
                        replies += 1;
                        match f.on_data(&d) {
                            Step::Ignored => continue,
                            Step::Done => {
                                outcome = "done".into();
                                break 'fetch;
                            }
                            Step::Progress(_) => {
                                next = f.next_request();
                                continue 'fetch;
                            }
                            Step::BadHeader(e) => {
                                outcome = format!("bad header: {e}");
                                break 'fetch;
                            }
                            Step::PeerError(e) => {
                                outcome = format!("peer error {e}");
                                break 'fetch;
                            }
                        }
                    }
                    Ok(Some(PeerMessage::Ping(p))) if p.r#type == 0 => {
                        let mut pong = p;
                        pong.r#type = 1;
                        let _ = wr.write_all(&frame(&PeerMessage::Ping(pong))).await;
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        match tokio::time::timeout(left, rd.read_buf(&mut buf)).await {
                            Ok(Ok(0)) => {
                                outcome = "peer closed".into();
                                break 'fetch;
                            }
                            Ok(Ok(_)) => {}
                            Ok(Err(e)) => {
                                outcome = format!("read: {e}");
                                break 'fetch;
                            }
                            Err(_) => break 'fetch,
                        }
                    }
                    Err(e) => {
                        outcome = format!("decode: {e}");
                        break 'fetch;
                    }
                }
            }
        }
        let (requests, rejected) = (f.requests, f.rejected);
        let ms = t0.elapsed().as_millis();
        match (outcome.as_str(), f.into_result()) {
            ("done", Some((header, ours))) => {
                let verdict = compare(&ours, &theirs);
                eprintln!(
                    "#{seq} {}: header proven, {} transactions, {requests} requests, {replies} replies, {rejected} nodes rejected, {ms} ms; vs the RPC copy: {}",
                    &hash_hex[..12],
                    ours.len(),
                    verdict.clone().unwrap_or_else(|| "SAME, byte for byte".into())
                );
                debug_assert_eq!(header.sequence, seq);
                if verdict.is_none() && header.sequence == seq {
                    ok += 1;
                } else {
                    bad += 1;
                }
            }
            (o, _) => {
                eprintln!("#{seq} {}: NOT fetched ({o}), {requests} requests, {replies} replies, {ms} ms", &hash_hex[..12]);
                bad += 1;
            }
        }
    }
    eprintln!("PEER FETCH SUMMARY: {ok} ledgers fetched from {peer} and identical to the RPC copy, {bad} not");
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
