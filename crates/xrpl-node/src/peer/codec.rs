//! Wire protocol framing codec for XRPL peer connections.
//!
//! After the TLS handshake and HTTP upgrade, peers exchange binary-framed
//! protobuf messages:
//!
//! ```text
//! [4 bytes: payload_len (big-endian u32)] [2 bytes: type_code (big-endian u16)] [N bytes: protobuf]
//! ```
//!
//! `payload_len` includes the 2-byte type code, so protobuf data length = payload_len - 2.
//! If bit 15 of the type code is set, the protobuf payload is zlib-compressed.

use bytes::{Buf, BufMut, BytesMut};
use flate2::read::ZlibDecoder;
use std::io::Read;
use tokio_util::codec::{Decoder, Encoder};

use super::message::PeerMessage;
use crate::NodeError;

/// Maximum allowed message size (16 MB, same as rippled).
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// Bit mask for compression flag in the type code.
const COMPRESSION_FLAG: u16 = 1 << 15;

/// Length-prefixed protobuf codec for the XRPL peer protocol.
pub struct MessageCodec;

impl Decoder for MessageCodec {
    type Item = PeerMessage;
    type Error = NodeError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<PeerMessage>, NodeError> {
        // Need at least 4 bytes for the length prefix.
        if src.len() < 4 {
            return Ok(None);
        }

        // Peek at the length without consuming.
        let payload_len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);

        if payload_len > MAX_MESSAGE_SIZE {
            return Err(NodeError::MessageDecode(format!(
                "message too large: {payload_len} bytes (max {MAX_MESSAGE_SIZE})"
            )));
        }

        if payload_len < 2 {
            return Err(NodeError::MessageDecode(format!(
                "payload too short: {payload_len} bytes (need at least 2 for type code)"
            )));
        }

        let total_frame_len = 4 + payload_len as usize;

        // Wait for the full frame.
        if src.len() < total_frame_len {
            // Reserve space to avoid repeated allocations.
            src.reserve(total_frame_len - src.len());
            return Ok(None);
        }

        // Consume the frame.
        let mut frame = src.split_to(total_frame_len);
        frame.advance(4); // skip length prefix

        // Read 2-byte type code.
        let raw_type_code = frame.get_u16();
        let compressed = raw_type_code & COMPRESSION_FLAG != 0;
        let type_code = raw_type_code & !COMPRESSION_FLAG;

        let proto_data = &frame[..];

        // Decompress if needed.
        let data = if compressed {
            let mut decoder = ZlibDecoder::new(proto_data);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed).map_err(|e| {
                NodeError::MessageDecode(format!("zlib decompression failed: {e}"))
            })?;
            decompressed
        } else {
            proto_data.to_vec()
        };

        let msg = PeerMessage::decode(type_code, &data)?;
        Ok(Some(msg))
    }
}

impl Encoder<PeerMessage> for MessageCodec {
    type Error = NodeError;

    fn encode(&mut self, item: PeerMessage, dst: &mut BytesMut) -> Result<(), NodeError> {
        let type_code = item.type_code();
        let proto_bytes = item.encode_to_vec();

        // payload = 2-byte type code + protobuf bytes
        let payload_len = 2 + proto_bytes.len();

        if payload_len > MAX_MESSAGE_SIZE as usize {
            return Err(NodeError::MessageDecode(format!(
                "outgoing message too large: {payload_len} bytes"
            )));
        }

        dst.reserve(4 + payload_len);
        dst.put_u32(payload_len as u32);
        dst.put_u16(type_code);
        dst.put_slice(&proto_bytes);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::protocol;

    #[test]
    fn encode_decode_ping() {
        let ping = PeerMessage::Ping(protocol::TmPing {
            r#type: protocol::tm_ping::PingType::PtPing as i32,
            seq: Some(42),
            ping_time: Some(1234567890),
            net_time: None,
        });

        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();

        // Encode
        codec.encode(ping, &mut buf).unwrap();

        // Decode
        let decoded = codec.decode(&mut buf).unwrap().unwrap();

        match decoded {
            PeerMessage::Ping(p) => {
                assert_eq!(p.r#type, protocol::tm_ping::PingType::PtPing as i32);
                assert_eq!(p.seq, Some(42));
                assert_eq!(p.ping_time, Some(1234567890));
            }
            other => panic!("expected Ping, got {}", other.name()),
        }
    }

    #[test]
    fn encode_decode_status_change() {
        let status = PeerMessage::StatusChange(protocol::TmStatusChange {
            new_status: Some(protocol::NodeStatus::NsValidating as i32),
            new_event: Some(protocol::NodeEvent::NeAcceptedLedger as i32),
            ledger_seq: Some(12345678),
            ledger_hash: Some(vec![0xAB; 32]),
            ledger_hash_previous: Some(vec![0xCD; 32]),
            network_time: Some(7890),
            first_seq: Some(1),
            last_seq: Some(12345678),
        });

        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();

        codec.encode(status, &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().unwrap();

        match decoded {
            PeerMessage::StatusChange(s) => {
                assert_eq!(s.ledger_seq, Some(12345678));
                assert_eq!(s.new_status, Some(protocol::NodeStatus::NsValidating as i32));
            }
            other => panic!("expected StatusChange, got {}", other.name()),
        }
    }

    #[test]
    fn partial_frame_waits() {
        let mut codec = MessageCodec;

        // Only 2 bytes — not enough for length prefix
        let mut buf = BytesMut::from(&[0u8, 0, 0, 10][..]);
        // We said payload is 10 bytes but only have the 4-byte header
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn reject_oversized_message() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();

        // Claim payload is 20 MB
        buf.put_u32(20 * 1024 * 1024);
        buf.put_u16(3); // type: Ping
        buf.put_slice(&[0u8; 100]);

        let result = codec.decode(&mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_frames_in_buffer() {
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();

        // Encode two pings
        let ping1 = PeerMessage::Ping(protocol::TmPing {
            r#type: protocol::tm_ping::PingType::PtPing as i32,
            seq: Some(1),
            ping_time: None,
            net_time: None,
        });
        let ping2 = PeerMessage::Ping(protocol::TmPing {
            r#type: protocol::tm_ping::PingType::PtPong as i32,
            seq: Some(2),
            ping_time: None,
            net_time: None,
        });

        codec.encode(ping1, &mut buf).unwrap();
        codec.encode(ping2, &mut buf).unwrap();

        // Decode both
        let msg1 = codec.decode(&mut buf).unwrap().unwrap();
        let msg2 = codec.decode(&mut buf).unwrap().unwrap();

        match msg1 {
            PeerMessage::Ping(p) => assert_eq!(p.seq, Some(1)),
            _ => panic!("expected Ping"),
        }
        match msg2 {
            PeerMessage::Ping(p) => assert_eq!(p.seq, Some(2)),
            _ => panic!("expected Ping"),
        }

        // Buffer should be empty
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }
}
