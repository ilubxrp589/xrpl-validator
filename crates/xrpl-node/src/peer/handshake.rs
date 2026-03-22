//! XRPL peer handshake — HTTP upgrade from HTTPS to XRPL/2.0.
//!
//! Uses OpenSSL for TLS (required for `SSL_get_finished` compatibility
//! with rippled's session cookie mechanism).

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use sha2::{Digest, Sha512};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::identity::NodeIdentity;
use crate::NodeError;

/// Network ID constants.
pub const NETWORK_ID_MAINNET: u32 = 0;
pub const NETWORK_ID_TESTNET: u32 = 1;
pub const NETWORK_ID_DEVNET: u32 = 2;

/// Result of a successful handshake.
pub struct HandshakeResult {
    /// The TLS stream, ready for binary framing.
    pub stream: tokio_openssl::SslStream<TcpStream>,
    /// The peer's public key (extracted from their Public-Key header).
    pub peer_public_key: Vec<u8>,
    /// The peer's reported ledger sequence (if any).
    pub peer_ledger_seq: Option<u32>,
    /// Any bytes read after the HTTP headers that belong to the binary protocol.
    /// Must be prepended to the framed codec's buffer.
    pub remaining_bytes: Vec<u8>,
}

/// Perform an outbound handshake to an XRPL peer.
pub async fn outbound_handshake(
    addr: &str,
    identity: &NodeIdentity,
    network_id: u32,
) -> Result<HandshakeResult, NodeError> {
    let (host, _port) = parse_addr(addr)?;

    // TCP connect
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| NodeError::Connection(format!("TCP connect to {addr}: {e}")))?;

    // OpenSSL TLS connect (required for SSL_get_finished compatibility)
    let mut ssl_builder = SslConnector::builder(SslMethod::tls())
        .map_err(|e| NodeError::Connection(format!("SSL builder: {e}")))?;

    // Don't verify peer certs (self-signed network)
    ssl_builder.set_verify(SslVerifyMode::NONE);

    let ssl_connector = ssl_builder.build();
    let ssl = ssl_connector
        .configure()
        .map_err(|e| NodeError::Connection(format!("SSL configure: {e}")))?
        .into_ssl(host)
        .map_err(|e| NodeError::Connection(format!("SSL create: {e}")))?;

    let mut tls = tokio_openssl::SslStream::new(ssl, tcp)
        .map_err(|e| NodeError::Connection(format!("SSL stream: {e}")))?;

    std::pin::Pin::new(&mut tls)
        .connect()
        .await
        .map_err(|e| NodeError::Connection(format!("TLS handshake to {addr}: {e}")))?;

    // Compute shared value from TLS finished messages
    let shared_value = compute_shared_value(&tls)?;

    // Sign the shared value with our node key
    let signature = identity.sign(&shared_value)?;
    let sig_b64 = base64_encode(&signature);

    // Encode our public key as base58 with node public key prefix (0x1C)
    let pubkey_b58 = encode_node_public_key(identity.public_key());

    // Build HTTP upgrade request
    let request = format!(
        "GET / HTTP/1.1\r\n\
         Upgrade: XRPL/2.2, XRPL/2.1, XRPL/2.0\r\n\
         Connection: Upgrade\r\n\
         Connect-As: Peer\r\n\
         Public-Key: {pubkey_b58}\r\n\
         Session-Signature: {sig_b64}\r\n\
         Crawl: public\r\n\
         Network-ID: {network_id}\r\n\
         User-Agent: xrpl-node-rs/0.1.0\r\n\
         \r\n"
    );

    tls.write_all(request.as_bytes())
        .await
        .map_err(|e| NodeError::HandshakeFailed(format!("write request: {e}")))?;

    // Read HTTP response into a buffer, then split at the header boundary.
    // We read in chunks and look for \r\n\r\n. Any bytes after the headers
    // belong to the binary protocol and must be preserved.
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(4096);
    let header_end;

    let read_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let mut chunk = [0u8; 1024];
        let n = match tokio::time::timeout_at(read_deadline, tls.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(NodeError::HandshakeFailed(format!("read response: {e}"))),
            Err(_) => return Err(NodeError::HandshakeFailed("header read timed out".to_string())),
        };
        if n == 0 {
            return Err(NodeError::HandshakeFailed("connection closed during headers".to_string()));
        }
        buf.extend_from_slice(&chunk[..n]);

        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }

        if buf.len() > 16384 {
            return Err(NodeError::HandshakeFailed("headers too large".to_string()));
        }
    }
    let header_bytes = &buf[..header_end];
    let remaining = buf[header_end..].to_vec();

    // Parse headers
    let header_str = String::from_utf8_lossy(header_bytes);

    tracing::debug!(
        buf_len = buf.len(),
        header_end,
        remaining = remaining.len(),
        "parsed HTTP response headers"
    );
    let lines: Vec<&str> = header_str.split("\r\n").collect();

    let status_line = lines.first().unwrap_or(&"");
    // Parse status code: "HTTP/1.1 101 Switching Protocols"
    let status_ok = status_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code == "101");
    if !status_ok {
        return Err(NodeError::HandshakeFailed(format!(
            "expected 101, got: {status_line}"
        )));
    }

    let mut peer_public_key = Vec::new();
    let mut peer_session_sig = String::new();
    let peer_ledger_seq = None;

    for line in &lines[1..] {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim();

            match key.as_str() {
                "public-key" => {
                    peer_public_key = hex::decode(value)
                        .unwrap_or_else(|_| value.as_bytes().to_vec());
                }
                "session-signature" => {
                    peer_session_sig = value.to_string();
                }
                _ => {}
            }
        }
    }

    // Verify peer's session signature if we have their public key
    if !peer_public_key.is_empty() && !peer_session_sig.is_empty() {
        if let Ok(sig_bytes) = base64_decode(&peer_session_sig) {
            // Peer signs the same shared value we computed
            let valid = if peer_public_key.len() == 33 {
                if peer_public_key[0] == 0x02 || peer_public_key[0] == 0x03 {
                    xrpl_core::crypto::secp256k1::verify(&peer_public_key, &shared_value, &sig_bytes)
                        .unwrap_or(false)
                } else if peer_public_key[0] == 0xED {
                    let mut prefixed = shared_value.to_vec();
                    xrpl_core::crypto::ed25519::verify(&peer_public_key, &prefixed, &sig_bytes)
                        .unwrap_or(false)
                } else {
                    false
                }
            } else {
                false
            };

            if valid {
                tracing::debug!("peer session signature verified");
            } else {
                tracing::warn!("peer session signature FAILED verification (continuing anyway)");
            }
        }
    }

    Ok(HandshakeResult {
        stream: tls,
        peer_public_key,
        peer_ledger_seq,
        remaining_bytes: remaining,
    })
}

/// Compute the shared value from TLS finished messages.
///
/// Algorithm (matching rippled exactly):
/// 1. cookie1 = SHA512(SSL_get_finished())
/// 2. cookie2 = SHA512(SSL_get_peer_finished())
/// 3. result = cookie1 XOR cookie2
/// 4. shared_value = SHA512Half(result)
fn compute_shared_value(
    tls: &tokio_openssl::SslStream<TcpStream>,
) -> Result<[u8; 32], NodeError> {
    let ssl = tls.ssl();

    // Get local finished message
    let mut local_finished = vec![0u8; 64];
    let local_len = ssl.finished(&mut local_finished);
    if local_len == 0 {
        return Err(NodeError::HandshakeFailed(
            "SSL_get_finished returned empty".to_string(),
        ));
    }
    local_finished.truncate(local_len);

    // Get peer finished message
    let mut peer_finished = vec![0u8; 64];
    let peer_len = ssl.peer_finished(&mut peer_finished);
    if peer_len == 0 {
        return Err(NodeError::HandshakeFailed(
            "SSL_get_peer_finished returned empty".to_string(),
        ));
    }
    peer_finished.truncate(peer_len);

    // Hash both with SHA-512
    let cookie1: [u8; 64] = Sha512::digest(&local_finished).into();
    let cookie2: [u8; 64] = Sha512::digest(&peer_finished).into();

    // XOR the two hashes
    let mut xored = [0u8; 64];
    for i in 0..64 {
        xored[i] = cookie1[i] ^ cookie2[i];
    }

    // SHA512-half of the XOR result
    let hash = Sha512::digest(xored);
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
        Ok((addr, 51235))
    }
}

/// Encode a 33-byte public key as a base58check node public key.
/// Uses prefix byte `0x1C` (28), producing strings starting with 'n'.
fn encode_node_public_key(pubkey: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    const XRPL_ALPHABET: &[u8; 58] =
        b"rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz";

    let mut payload = Vec::with_capacity(1 + pubkey.len() + 4);
    payload.push(0x1C);
    payload.extend_from_slice(pubkey);

    let hash1 = Sha256::digest(&payload);
    let hash2 = Sha256::digest(hash1);
    payload.extend_from_slice(&hash2[..4]);

    let leading_zeros = payload.iter().take_while(|&&b| b == 0).count();
    let mut digits: Vec<u8> = Vec::new();
    for &byte in &payload {
        let mut carry = byte as u32;
        for digit in digits.iter_mut() {
            carry += (*digit as u32) * 256;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }

    let mut result = String::with_capacity(leading_zeros + digits.len());
    for _ in 0..leading_zeros {
        result.push(XRPL_ALPHABET[0] as char);
    }
    for &d in digits.iter().rev() {
        result.push(XRPL_ALPHABET[d as usize] as char);
    }
    result
}

fn base64_decode(input: &str) -> Result<Vec<u8>, ()> {
    const DECODE: [u8; 128] = {
        let mut t = [255u8; 128];
        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 { t[chars[i] as usize] = i as u8; i += 1; }
        t
    };
    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let bytes = input.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let (a, b, c, d) = (
            DECODE.get(bytes[i] as usize).copied().unwrap_or(255),
            DECODE.get(bytes[i+1] as usize).copied().unwrap_or(255),
            DECODE.get(bytes[i+2] as usize).copied().unwrap_or(255),
            DECODE.get(bytes[i+3] as usize).copied().unwrap_or(255),
        );
        if a == 255 || b == 255 { return Err(()); }
        out.push((a << 2) | (b >> 4));
        if c != 255 { out.push((b << 4) | (c >> 2)); }
        if d != 255 { out.push((c << 6) | d); }
        i += 4;
    }
    // Handle remaining 2-3 chars
    if i + 1 < bytes.len() {
        let a = DECODE.get(bytes[i] as usize).copied().unwrap_or(255);
        let b = DECODE.get(bytes[i+1] as usize).copied().unwrap_or(255);
        if a != 255 && b != 255 { out.push((a << 2) | (b >> 4)); }
        if i + 2 < bytes.len() {
            let c = DECODE.get(bytes[i+2] as usize).copied().unwrap_or(255);
            if c != 255 { out.push((b << 4) | (c >> 2)); }
        }
    }
    Ok(out)
}

fn base64_encode(data: &[u8]) -> String {
    use std::fmt::Write;
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

    #[test]
    fn test_encode_node_public_key() {
        // A node public key should start with 'n'
        let fake_key = [0x02; 33]; // compressed secp256k1 point
        let encoded = encode_node_public_key(&fake_key);
        assert!(encoded.starts_with('n'), "expected 'n' prefix, got: {encoded}");
    }
}
