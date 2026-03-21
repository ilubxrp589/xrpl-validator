//! Live integration test: connect to XRPL testnet peer.
//!
//! Run with: cargo test -p xrpl-node --features live-tests -- peer_connect --nocapture

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
async fn connect_to_testnet_peer() {
    let identity = NodeIdentity::generate().expect("generate identity");
    println!("Node public key: {}", identity.public_key_hex());
    println!("Connecting to {TESTNET_PEER}...");

    let mut hs = timeout(
        Duration::from_secs(15),
        handshake::outbound_handshake(TESTNET_PEER, &identity, handshake::NETWORK_ID_TESTNET),
    )
    .await
    .expect("handshake timed out")
    .expect("handshake failed");

    println!("Handshake successful!");
    println!("  Remaining bytes: {}", hs.remaining_bytes.len());

    let mut codec = MessageCodec;
    let mut buf = BytesMut::with_capacity(65536);
    buf.extend_from_slice(&hs.remaining_bytes);

    let mut msg_count = 0;
    let start = std::time::Instant::now();

    println!("Reading messages for up to 30 seconds...");

    while start.elapsed() < Duration::from_secs(30) && msg_count < 30 {
        // Decode from buffer
        loop {
            match codec.decode(&mut buf) {
                Ok(Some(msg)) => {
                    msg_count += 1;
                    let detail = match &msg {
                        PeerMessage::StatusChange(sc) => {
                            format!("ledger_seq={:?} status={:?}", sc.ledger_seq, sc.new_status)
                        }
                        PeerMessage::Validation(_) => "(validation)".to_string(),
                        PeerMessage::Manifests(m) => format!("{} manifests", m.list.len()),
                        PeerMessage::ValidatorListCollection(_) => "(validator list)".to_string(),
                        PeerMessage::Endpoints(e) => format!("{} endpoints", e.endpoints_v2.len()),
                        PeerMessage::ProposeSet(p) => format!("seq={}", p.propose_seq),
                        PeerMessage::Transaction(_) => "(transaction)".to_string(),
                        _ => String::new(),
                    };
                    println!("  [{msg_count:>3}] {} {detail}", msg.name());
                    continue;
                }
                Ok(None) => break,
                Err(e) => {
                    println!("  [err] {e}");
                    break;
                }
            }
        }

        if msg_count >= 30 {
            break;
        }

        // Read more from stream
        let mut chunk = vec![0u8; 32768];
        match timeout(Duration::from_secs(10), hs.stream.read(&mut chunk)).await {
            Ok(Ok(0)) => {
                println!("  Stream ended");
                break;
            }
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => {
                println!("  Read error: {e}");
                break;
            }
            Err(_) => continue,
        }
    }

    assert!(msg_count > 0, "should have received at least one message");
    println!(
        "\nReceived {msg_count} messages from XRPL testnet in {:.1}s",
        start.elapsed().as_secs_f64()
    );
}
