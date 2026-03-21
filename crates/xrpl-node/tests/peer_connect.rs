//! Live integration test: connect to XRPL testnet peer.
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

    // Manually decode messages using the codec on a BytesMut buffer.
    // This avoids any Framed<SslStream> compatibility issues.
    let mut codec = MessageCodec;
    let mut buf = BytesMut::with_capacity(65536);

    // Seed with any remaining bytes from handshake
    buf.extend_from_slice(&hs.remaining_bytes);

    // Read first chunk and find the start of binary framing.
    // rippled may send a few bytes between the HTTP headers and binary protocol.
    {
        let mut initial = vec![0u8; 8192];
        let n = hs.stream.read(&mut initial).await.expect("initial read");
        initial.truncate(n);
        eprintln!("[debug] initial read: {} bytes, first 20: {:02x?}", n, &initial[..std::cmp::min(20, n)]);

        // Find the first valid frame: look for a 4-byte big-endian length
        // where the value is reasonable (< 16MB) and the following 2-byte
        // type code matches a known message type.
        let known_types: &[u16] = &[2, 3, 5, 15, 30, 31, 32, 33, 34, 35, 41, 42, 54, 55, 56, 57, 58, 59, 60, 63, 64];
        let mut offset = 0;
        while offset + 6 <= n {
            let len = u32::from_be_bytes([initial[offset], initial[offset+1], initial[offset+2], initial[offset+3]]);
            let tc = u16::from_be_bytes([initial[offset+4], initial[offset+5]]);
            let actual_tc = tc & 0x7FFF;
            if len < 16_000_000 && len >= 2 && known_types.contains(&actual_tc) {
                eprintln!("[debug] found valid frame at offset {offset}: len={len} type={actual_tc}");
                buf.extend_from_slice(&initial[offset..]);
                break;
            }
            offset += 1;
        }
        if offset + 6 > n {
            eprintln!("[debug] no valid frame found in initial {} bytes!", n);
            buf.extend_from_slice(&initial);
        }
    }

    let mut msg_count = 0;
    let start = std::time::Instant::now();

    println!("Reading messages for up to 30 seconds...");

    while start.elapsed() < Duration::from_secs(30) && msg_count < 30 {
        // Try to decode from existing buffer first
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
                    continue; // Try to decode more from buffer
                }
                Ok(None) => break,    // Need more data
                Err(e) => {
                    println!("  [err] {e}");
                    break;
                }
            }
        }

        if msg_count >= 30 {
            break;
        }

        if msg_count == 0 && !buf.is_empty() {
            println!("  Buffer first 16 bytes: {:02x?}", &buf[..std::cmp::min(16, buf.len())]);
        }

        // Read more data from the stream
        let mut chunk = vec![0u8; 32768];
        match timeout(Duration::from_secs(10), hs.stream.read(&mut chunk)).await {
            Ok(Ok(0)) => {
                println!("  Stream ended");
                break;
            }
            Ok(Ok(n)) => {
                buf.extend_from_slice(&chunk[..n]);
            }
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
