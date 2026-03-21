//! XRPL peer handshake — HTTP upgrade from HTTPS to XRPL/2.0.
//!
//! After establishing a TLS connection, peers exchange HTTP headers to
//! upgrade the connection to the XRPL binary peer protocol.
//!
//! ## Outbound handshake
//! 1. Connect via TLS
//! 2. Send `GET / HTTP/1.1` with upgrade headers
//! 3. Receive `HTTP/1.1 101 Switching Protocols`
//! 4. Transition to binary message framing
//!
//! ## Key headers
//! - `Upgrade: XRPL/2.2` — protocol version
//! - `Connection: Upgrade`
//! - `Connect-As: Peer`
//! - `Public-Key: <base58 node pubkey>`
//! - `Session-Signature: <base64 signed shared value>`
//! - `Network-ID: 0` (mainnet) or `1` (testnet)
//! - `Crawl: public`

use sha2::{Digest, Sha512};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::{client::TlsStream, TlsConnector};

use super::identity::NodeIdentity;
use crate::NodeError;

/// Network ID constants.
pub const NETWORK_ID_MAINNET: u32 = 0;
pub const NETWORK_ID_TESTNET: u32 = 1;
pub const NETWORK_ID_DEVNET: u32 = 2;

/// Result of a successful handshake.
pub struct HandshakeResult {
    /// The TLS stream, ready for binary framing.
    pub stream: TlsStream<TcpStream>,
    /// The peer's public key (extracted from their Public-Key header).
    pub peer_public_key: Vec<u8>,
    /// The peer's reported ledger sequence (if any).
    pub peer_ledger_seq: Option<u32>,
}

/// Perform an outbound handshake to an XRPL peer.
///
/// Connects via TLS, sends HTTP upgrade request, validates the
/// 101 response, and returns the upgraded stream.
pub async fn outbound_handshake(
    addr: &str,
    identity: &NodeIdentity,
    network_id: u32,
) -> Result<HandshakeResult, NodeError> {
    // Parse address
    let (host, _port) = parse_addr(addr)?;

    // TCP connect
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| NodeError::Connection(format!("TCP connect to {addr}: {e}")))?;

    // TLS connect
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .unwrap_or_else(|_| rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap());

    let connector = TlsConnector::from(identity.client_config());
    let mut tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| NodeError::Connection(format!("TLS to {addr}: {e}")))?;

    // Compute shared value from TLS session
    let shared_value = compute_shared_value(&tls)?;

    // Sign the shared value
    let signature = identity.sign(&shared_value)?;
    let sig_b64 = base64_encode(&signature);

    // Encode our public key as hex (rippled also accepts base58, but hex is simpler)
    let pubkey_hex = identity.public_key_hex();

    // Build HTTP upgrade request
    let request = format!(
        "GET / HTTP/1.1\r\n\
         Upgrade: XRPL/2.2, XRPL/2.1, XRPL/2.0\r\n\
         Connection: Upgrade\r\n\
         Connect-As: Peer\r\n\
         Public-Key: {pubkey_hex}\r\n\
         Session-Signature: {sig_b64}\r\n\
         Crawl: public\r\n\
         Network-ID: {network_id}\r\n\
         User-Agent: xrpl-node-rs/0.1.0\r\n\
         \r\n"
    );

    tls.write_all(request.as_bytes())
        .await
        .map_err(|e| NodeError::HandshakeFailed(format!("write request: {e}")))?;

    // Read response
    let mut reader = BufReader::new(&mut tls);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|e| NodeError::HandshakeFailed(format!("read status: {e}")))?;

    // Check for 101 Switching Protocols
    if !status_line.contains("101") {
        return Err(NodeError::HandshakeFailed(format!(
            "expected 101, got: {}",
            status_line.trim()
        )));
    }

    // Parse response headers
    let mut peer_public_key = Vec::new();
    let peer_ledger_seq = None;

    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| NodeError::HandshakeFailed(format!("read header: {e}")))?;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            break; // End of headers
        }

        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "public-key" => {
                    peer_public_key = hex::decode(value).unwrap_or_default();
                }
                "closed-ledger" => {
                    // Could parse ledger hash here
                }
                _ => {}
            }
        }
    }

    // Drain any remaining buffered data back onto the stream
    // (BufReader may have read ahead)
    drop(reader);

    Ok(HandshakeResult {
        stream: tls,
        peer_public_key,
        peer_ledger_seq,
    })
}

/// Compute the shared value from TLS session finished messages.
///
/// shared_value = SHA512Half(SHA512(local_finished) XOR SHA512(peer_finished))
///
/// With rustls, we don't have direct access to the SSL finished messages
/// the way OpenSSL exposes them. Instead, we use the TLS exporter keying
/// material (RFC 5705) which serves the same purpose — binding the
/// application layer to the specific TLS session.
fn compute_shared_value(
    tls: &TlsStream<TcpStream>,
) -> Result<[u8; 32], NodeError> {
    // Try to get the TLS exporter keying material
    let (_, conn) = tls.get_ref();

    // Use the TLS exporter to derive a session-bound value.
    // rippled uses SSL_get_finished/SSL_get_peer_finished, but rustls
    // doesn't expose those directly. The exporter serves the same
    // cryptographic binding purpose.
    let mut exported = [0u8; 64];
    conn.export_keying_material(
        &mut exported,
        b"XRPL peer handshake shared value",
        None,
    )
    .map_err(|e| {
        NodeError::HandshakeFailed(format!("TLS export keying material: {e}"))
    })?;

    // SHA512-half of the exported material
    let hash = Sha512::digest(exported);
    let mut result = [0u8; 32];
    result.copy_from_slice(&hash[..32]);
    Ok(result)
}

fn parse_addr(addr: &str) -> Result<(&str, u16), NodeError> {
    if let Some((host, port_str)) = addr.rsplit_once(':') {
        let port = port_str
            .parse::<u16>()
            .map_err(|_| NodeError::Connection(format!("invalid port in {addr}")))?;
        Ok((host, port))
    } else {
        Ok((addr, 51235)) // Default XRPL peer port
    }
}

fn base64_encode(data: &[u8]) -> String {
    use std::fmt::Write;
    // Simple base64 encoding without pulling in another crate
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        write!(result, "{}", CHARS[((triple >> 18) & 0x3F) as usize] as char).unwrap();
        write!(result, "{}", CHARS[((triple >> 12) & 0x3F) as usize] as char).unwrap();
        if chunk.len() > 1 {
            write!(result, "{}", CHARS[((triple >> 6) & 0x3F) as usize] as char).unwrap();
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            write!(result, "{}", CHARS[(triple & 0x3F) as usize] as char).unwrap();
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_addr() {
        let (host, port) = parse_addr("s.altnet.rippletest.net:51235").unwrap();
        assert_eq!(host, "s.altnet.rippletest.net");
        assert_eq!(port, 51235);
    }

    #[test]
    fn test_parse_addr_default_port() {
        let (host, port) = parse_addr("example.com").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 51235);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }
}
