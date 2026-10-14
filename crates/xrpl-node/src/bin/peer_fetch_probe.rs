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
//!
//!   peer_fetch_probe --driver MINUTES [--peers a:51235,b:51235] [--rpc URL] [--log FILE]
//!
//! The live driver itself (peer_fetch::start, register, on_ledger_data, offer_network: its pacing, retries
//! and peer rotation) over sessions to each of --peers, fed every validated ledger the RPC server reports,
//! for MINUTES. Prints the driver's counters every minute.
//!
//!   peer_fetch_probe --validated MINUTES [--peers ...] [--rpc URL] [--log FILE]
//!
//! Independence 4.5: own_validated's tally over the validations the --peers sessions relay (each signature
//! checked, as the validator does), against the RPC server's validated ledgers, under the verified list and the
//! RPC server's negative UNL. Prints the verdicts every minute.

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

/// One peer session for --validated: every signature-checked validation into own_validated.
async fn validation_session(addr: String) {
    let identity = match xrpl_node::peer::identity::NodeIdentity::generate() {
        Ok(i) => i,
        Err(_) => return,
    };
    let Ok(Ok(hs)) = tokio::time::timeout(
        Duration::from_secs(20),
        xrpl_node::peer::handshake::outbound_handshake(&addr, &identity, xrpl_node::peer::handshake::NETWORK_ID_MAINNET),
    )
    .await
    else {
        eprintln!("{addr}: no session");
        return;
    };
    eprintln!("{addr}: connected");
    let (mut rd, mut wr) = tokio::io::split(hs.stream);
    let mut buf = BytesMut::from(&hs.remaining_bytes[..]);
    let mut codec = MessageCodec;
    loop {
        match codec.decode(&mut buf) {
            Ok(Some(PeerMessage::Validation(v))) => {
                if xrpl_node::consensus::sig_verify::verify_validation_blob(&v.validation) == xrpl_node::consensus::sig_verify::SigOutcome::Valid {
                    xrpl_node::own_validated::on_validation(&v.validation);
                }
            }
            Ok(Some(PeerMessage::Ping(p))) if p.r#type == 0 => {
                let mut pong = p;
                pong.r#type = 1;
                let _ = wr.write_all(&frame(&PeerMessage::Ping(pong))).await;
            }
            Ok(Some(_)) => {}
            Ok(None) => match rd.read_buf(&mut buf).await {
                Ok(n) if n > 0 => {}
                _ => break,
            },
            Err(_) => break,
        }
    }
    eprintln!("{addr}: session ended");
}

async fn run_validated(url: &str, minutes: u64) -> i32 {
    match xrpl_node::unl_fetch::fetch_default_unl_verified().await {
        Ok(entries) => xrpl_node::own_validated::set_unl(&entries),
        Err(e) => {
            eprintln!("validator list: {e}");
            return 2;
        }
    }
    let peers = arg("--peers").unwrap_or_else(|| "s1.ripple.com:51235,s2.ripple.com:51235".into());
    for p in peers.split(',').filter(|p| !p.is_empty()) {
        tokio::spawn(validation_session(p.to_string()));
    }
    let end = Instant::now() + Duration::from_secs(60 * minutes);
    let (mut last, mut said) = (0u32, Instant::now());
    while Instant::now() < end {
        // The negative UNL as the RPC server's validated ledger holds it.
        let nunl = rpc(url, json!({"method": "ledger_entry", "params": [{"index": "2E8A59AA9D3B5B186B0B9E0F62E6C02587CA74A4D778938E957B6357D364B244", "ledger_index": "validated", "binary": true}]}))
            .await
            .ok()
            .and_then(|r| r["node_binary"].as_str().and_then(|h| hex::decode(h).ok()));
        xrpl_node::own_validated::set_negative_unl(nunl.as_deref());
        let r = rpc(url, json!({"method": "ledger", "params": [{"ledger_index": "validated"}]})).await.ok();
        let v = r.as_ref().and_then(|r| r["ledger_index"].as_u64()).unwrap_or(0) as u32;
        if v > last {
            let from = if last == 0 { v } else { last + 1 };
            for seq in from..=v {
                let hash = if seq == v {
                    r.as_ref().and_then(|r| r["ledger_hash"].as_str().map(str::to_string))
                } else {
                    rpc(url, json!({"method": "ledger", "params": [{"ledger_index": seq}]})).await.ok().and_then(|x| x["ledger_hash"].as_str().map(str::to_string))
                };
                if let Some(h) = hash.and_then(|h| hex::decode(h).ok()).and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                    xrpl_node::own_validated::network_validated(seq, h);
                }
            }
            last = v;
        }
        if said.elapsed() >= Duration::from_secs(60) {
            eprintln!("VALIDATED {}", xrpl_node::own_validated::summary());
            said = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!("VALIDATED SUMMARY: {}", xrpl_node::own_validated::summary());
    let st = xrpl_node::own_validated::stats();
    let l = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    i32::from(l(&st.differ) + l(&st.short) > 0 || l(&st.agree) == 0)
}

/// One peer session for the driver: register it, write the driver's frames, hand it every mtLEDGER_DATA.
async fn driver_session(addr: String) {
    let identity = match xrpl_node::peer::identity::NodeIdentity::generate() {
        Ok(i) => i,
        Err(_) => return,
    };
    let hs = match tokio::time::timeout(
        Duration::from_secs(20),
        xrpl_node::peer::handshake::outbound_handshake(&addr, &identity, xrpl_node::peer::handshake::NETWORK_ID_MAINNET),
    )
    .await
    {
        Ok(Ok(h)) => h,
        other => {
            eprintln!("{addr}: no session ({})", match other { Ok(Err(e)) => e.to_string(), _ => "timeout".into() });
            return;
        }
    };
    eprintln!("{addr}: connected");
    let (mut rd, wr) = tokio::io::split(hs.stream);
    let wr = std::sync::Arc::new(tokio::sync::Mutex::new(wr));
    if let Some(mut frames) = xrpl_node::peer_fetch::register(&addr) {
        let w = wr.clone();
        tokio::spawn(async move {
            while let Some(f) = frames.recv().await {
                if w.lock().await.write_all(&f).await.is_err() {
                    break;
                }
            }
        });
    }
    let mut buf = BytesMut::from(&hs.remaining_bytes[..]);
    let mut codec = MessageCodec;
    loop {
        match codec.decode(&mut buf) {
            Ok(Some(PeerMessage::LedgerData(d))) => xrpl_node::peer_fetch::on_ledger_data(&addr, &d),
            Ok(Some(PeerMessage::Ping(p))) if p.r#type == 0 => {
                let mut pong = p;
                pong.r#type = 1;
                let _ = wr.lock().await.write_all(&frame(&PeerMessage::Ping(pong))).await;
            }
            Ok(Some(_)) => {}
            Ok(None) => match rd.read_buf(&mut buf).await {
                Ok(n) if n > 0 => {}
                _ => break,
            },
            Err(_) => break,
        }
    }
    eprintln!("{addr}: session ended");
    xrpl_node::peer_fetch::unregister(&addr);
}

async fn run_driver(url: &str, minutes: u64) -> i32 {
    if !xrpl_node::peer_fetch::start() {
        eprintln!("the driver did not start (XRPL_PEER_FETCH)");
        return 2;
    }
    let peers = arg("--peers").unwrap_or_else(|| "s1.ripple.com:51235,s2.ripple.com:51235".into());
    for p in peers.split(',').filter(|p| !p.is_empty()) {
        tokio::spawn(driver_session(p.to_string()));
    }
    let end = Instant::now() + Duration::from_secs(60 * minutes);
    let (mut last, mut said) = (0u32, Instant::now());
    while Instant::now() < end {
        let v = rpc(url, json!({"method": "ledger", "params": [{"ledger_index": "validated"}]}))
            .await
            .ok()
            .and_then(|r| r["ledger_index"].as_u64())
            .unwrap_or(0) as u32;
        if v > last {
            let from = if last == 0 { v } else { last + 1 };
            for seq in from..=v {
                match their_copy(url, seq).await {
                    Ok((h, pairs)) => {
                        if let Some(hash) = hex::decode(&h).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()) {
                            xrpl_node::peer_fetch::offer_network(seq, hash, &pairs);
                        }
                    }
                    Err(e) => eprintln!("#{seq}: RPC copy: {e}"),
                }
            }
            last = v;
        }
        if said.elapsed() >= Duration::from_secs(60) {
            eprintln!("DRIVER {}", xrpl_node::peer_fetch::summary());
            said = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    tokio::time::sleep(Duration::from_secs(15)).await; // the last fetch finishes
    let st = xrpl_node::peer_fetch::stats();
    let l = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("DRIVER SUMMARY: {}", xrpl_node::peer_fetch::summary());
    i32::from(l(&st.differ) > 0 || l(&st.fetched) == 0)
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Some(minutes) = arg("--validated").and_then(|m| m.parse::<u64>().ok()) {
        std::env::set_var("XRPL_OWN_VALIDATED", "1");
        std::env::set_var("XRPL_OWN_VALIDATED_LOG", arg("--log").unwrap_or_else(|| "/tmp/own_validated_probe.jsonl".into()));
        let url = arg("--rpc").unwrap_or_else(|| "https://s2.ripple.com:51234".into());
        std::process::exit(run_validated(&url, minutes).await);
    }
    if let Some(minutes) = arg("--driver").and_then(|m| m.parse::<u64>().ok()) {
        std::env::set_var("XRPL_PEER_FETCH", "1");
        std::env::set_var("XRPL_PEER_FETCH_LOG", arg("--log").unwrap_or_else(|| "/tmp/peer_fetch_probe.jsonl".into()));
        let url = arg("--rpc").unwrap_or_else(|| "https://s2.ripple.com:51234".into());
        std::process::exit(run_driver(&url, minutes).await);
    }
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
