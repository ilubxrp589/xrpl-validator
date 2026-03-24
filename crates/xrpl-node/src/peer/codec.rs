//! Wire protocol framing codec for XRPL peer connections.
//!
//! After the TLS handshake and HTTP upgrade, peers exchange binary-framed
//! protobuf messages:
//!
//! ```text
//! [4 bytes: payload_len (big-endian u32)] [2 bytes: type_code (big-endian u16)] [payload_len bytes: protobuf]
//! ```
//!
//! `payload_len` is the protobuf payload size only — it does NOT include the
//! 2-byte type code. Total frame size = 4 + 2 + payload_len.
//!
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

/// Fixed overhead: 4-byte length + 2-byte type code.
const HEADER_SIZE: usize = 6;

/// Length-prefixed protobuf codec for the XRPL peer protocol.
pub struct MessageCodec;

impl Decoder for MessageCodec {
    type Item = PeerMessage;
    type Error = NodeError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<PeerMessage>, NodeError> {
        let mut skipped = 0;
        loop {
            // Need at least the 6-byte header.
            if src.len() < HEADER_SIZE {
                return Ok(None);
            }

            // Peek at the 4-byte protobuf payload length (does NOT include type code).
            let payload_len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);

            if payload_len > MAX_MESSAGE_SIZE {
                return Err(NodeError::MessageDecode(format!(
                    "message too large: {payload_len} bytes (max {MAX_MESSAGE_SIZE})"
                )));
            }

            // Total frame = 4 (length) + 2 (type code) + payload_len (protobuf)
            let total_frame_len = HEADER_SIZE + payload_len as usize;

            // Wait for the full frame.
            if src.len() < total_frame_len {
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

            tracing::trace!(
                payload_len,
                type_code,
                compressed,
                proto_bytes = frame.len(),
                "decoding frame"
            );

            let proto_data = &frame[..];

            // Decompress if needed.
            let data = if compressed {
                let mut decoder = ZlibDecoder::new(proto_data).take(16 * 1024 * 1024);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed).map_err(|e| {
                    NodeError::MessageDecode(format!("zlib decompression failed: {e}"))
                })?;
                decompressed
            } else {
                proto_data.to_vec()
            };

            match PeerMessage::decode(type_code, &data) {
                Ok(msg) => return Ok(Some(msg)),
                Err(e) => {
                    skipped += 1;
                    tracing::debug!(type_code, skipped, "skipping undecodable message: {e}");
                    if skipped >= 10 {
                        return Err(NodeError::MessageDecode(format!(
                            "too many consecutive undecodable frames ({skipped}), last: {e}"
                        )));
                    }
                }
            }
        }
    }
}

impl Encoder<PeerMessage> for MessageCodec {
    type Error = NodeError;

    fn encode(&mut self, item: PeerMessage, dst: &mut BytesMut) -> Result<(), NodeError> {
        let type_code = item.type_code();
        let proto_bytes = item.encode_to_vec();

        let payload_len = proto_bytes.len();

        if payload_len > MAX_MESSAGE_SIZE as usize {
            return Err(NodeError::MessageDecode(format!(
                "outgoing message too large: {payload_len} bytes"
            )));
        }

        // Frame: 4-byte length + 2-byte type + protobuf payload
        dst.reserve(HEADER_SIZE + payload_len);
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

        codec.encode(ping, &mut buf).unwrap();

        // Verify frame structure: 4-byte len + 2-byte type + protobuf
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let tc = u16::from_be_bytes([buf[4], buf[5]]);
        assert_eq!(tc, 3); // TMPing type code
        assert_eq!(len as usize + HEADER_SIZE, buf.len()); // length is protobuf-only

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

        // 4-byte header saying payload is 10 bytes, but we only have 6 bytes total
        let mut buf = BytesMut::from(&[0u8, 0, 0, 10, 0, 3][..]);
        // Need 6 + 10 = 16 bytes total, only have 6
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

        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn frame_layout_matches_spec() {
        // Verify our frame format: [4-byte len][2-byte type][protobuf...]
        // where len = protobuf size only (NOT including type code)
        let mut codec = MessageCodec;
        let mut buf = BytesMut::new();

        let ping = PeerMessage::Ping(protocol::TmPing {
            r#type: protocol::tm_ping::PingType::PtPing as i32,
            seq: Some(1),
            ping_time: None,
            net_time: None,
        });

        codec.encode(ping, &mut buf).unwrap();

        let payload_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let type_code = u16::from_be_bytes([buf[4], buf[5]]);
        let total = buf.len();

        // Total = 4 (len field) + 2 (type field) + payload_len (protobuf)
        assert_eq!(total, 4 + 2 + payload_len);
        assert_eq!(type_code, 3); // TMPing
        // Protobuf data starts at byte 6
        assert!(payload_len > 0);
    }
}
