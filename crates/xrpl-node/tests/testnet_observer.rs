//! Live test: follow XRPL testnet consensus for 10+ ledgers.
//!
//! Run with: cargo test -p xrpl-node --features live-tests -- testnet_observer --nocapture

#![cfg(feature = "live-tests")]

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tokio_util::codec::Decoder;

use xrpl_node::peer::codec::MessageCodec;
use xrpl_node::peer::handshake;
use xrpl_node::peer::identity::NodeIdentity;
use xrpl_node::peer::message::PeerMessage;

const TESTNET_PEER: &str = "s.altnet.rippletest.net:51235";

#[tokio::test]
async fn follow_consensus_10_ledgers() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let identity = NodeIdentity::generate().expect("identity");
    println!("Connecting to {TESTNET_PEER}...");

    let mut hs = timeout(
        Duration::from_secs(15),
        handshake::outbound_handshake(TESTNET_PEER, &identity, handshake::NETWORK_ID_TESTNET),
    )
    .await
    .expect("timeout")
    .expect("handshake");

    println!("Connected!");

    let mut codec = MessageCodec;
    let mut buf = BytesMut::with_capacity(65536);
    buf.extend_from_slice(&hs.remaining_bytes);

    let mut ledger_count = 0;
    let mut last_ledger: Option<u32> = None;
    let mut validation_count = 0;
    let mut proposal_count = 0;
    let mut tx_count = 0;
    let start = std::time::Instant::now();

    println!("Following consensus for 10 ledgers (up to 60s)...\n");

    while start.elapsed() < Duration::from_secs(60) && ledger_count < 10 {
        // Decode
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(msg)) => {
                    match &msg {
                        PeerMessage::StatusChange(sc) => {
                            if let Some(seq) = sc.ledger_seq {
                                if last_ledger.map_or(true, |l| seq > l) {
                                    ledger_count += 1;
                                    let gap = last_ledger.map(|l| seq - l).unwrap_or(0);
                                    println!(
                                        "  Ledger #{seq} (gap={gap}) | proposals={proposal_count} validations={validation_count} txs={tx_count}"
                                    );
                                    last_ledger = Some(seq);
                                    // Reset per-ledger counters
                                    validation_count = 0;
                                    proposal_count = 0;
                                    tx_count = 0;
                                }
                            }
                        }
                        PeerMessage::Validation(_) => validation_count += 1,
                        PeerMessage::ProposeSet(_) => proposal_count += 1,
                        PeerMessage::Transaction(_) => tx_count += 1,
                        _ => {}
                    }
                    continue;
                }
                Ok(None) => break,
                Err(e) => {
                    println!("  [err] {e}");
                    break;
                }
            }
        }

        // Read more
        let mut chunk = vec![0u8; 32768];
        match timeout(Duration::from_secs(10), hs.stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => { println!("  Read error: {e}"); break; }
            Err(_) => continue,
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!("\nFollowed {ledger_count} ledgers in {elapsed:.1}s");
    if ledger_count > 1 {
        println!("Average close interval: {:.1}s", elapsed / ledger_count as f64);
    }

    assert!(ledger_count >= 5, "should follow at least 5 ledgers in 60s");
    println!("PASSED: followed {ledger_count} consecutive ledgers on testnet");
}
