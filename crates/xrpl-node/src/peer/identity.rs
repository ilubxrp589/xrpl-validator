//! Node cryptographic identity — keypair + TLS certificate.
//!
//! Each XRPL node is identified by a Secp256k1 public key
//! (required by the rippled peer protocol). The key is embedded
//! in a self-signed TLS certificate used for peer connections.

use std::sync::Arc;

use rcgen::{CertificateParams, KeyPair as RcgenKeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use xrpl_core::address::KeyType;
use xrpl_core::crypto::signing::{Keypair, Seed};
use zeroize::Zeroize;

use crate::NodeError;

/// Cryptographic identity of this node on the XRPL peer network.
///
/// Two keypairs derived from the same seed:
/// - **Master key** (Ed25519): permanent validator identity, signs manifests
/// - **Signing key** (Secp256k1): ephemeral key, signs validations and peer handshakes
///
/// The TLS keypair is separate (regenerated each startup). The XRPL peer protocol
/// identifies nodes by the Secp256k1 public key in the TLS certificate CN.
pub struct NodeIdentity {
    /// The XRPL signing keypair (Secp256k1). Signs validations and peer handshakes.
    keypair: Keypair,
    /// The master keypair (Ed25519). Permanent validator identity, signs manifests.
    master_keypair: Keypair,
    /// DER-encoded self-signed TLS certificate.
    tls_cert_der: Vec<u8>,
    /// DER-encoded private key for TLS.
    tls_key_der: Vec<u8>,
    /// Reusable TLS server config.
    server_config: Arc<rustls::ServerConfig>,
    /// Reusable TLS client config.
    client_config: Arc<rustls::ClientConfig>,
}

impl NodeIdentity {
    /// Generate a new random node identity.
    pub fn generate() -> Result<Self, NodeError> {
        let seed = Seed::generate_with_type(KeyType::Secp256k1);
        Self::from_seed(&seed)
    }

    /// Derive node identity from an existing seed.
    /// Generates both Ed25519 (master) and Secp256k1 (signing) keypairs from the same seed bytes.
    pub fn from_seed(seed: &Seed) -> Result<Self, NodeError> {
        // Signing key: Secp256k1 (used for validations + peer handshakes)
        let secp_seed = Seed { bytes: seed.bytes, key_type: KeyType::Secp256k1 };
        let keypair = Keypair::from_seed(&secp_seed)
            .map_err(|e| NodeError::Config(format!("secp256k1 keypair derivation failed: {e}")))?;

        // Master key: Ed25519 (permanent validator identity, signs manifests)
        let ed_seed = Seed { bytes: seed.bytes, key_type: KeyType::Ed25519 };
        let master_keypair = Keypair::from_seed(&ed_seed)
            .map_err(|e| NodeError::Config(format!("ed25519 master keypair derivation failed: {e}")))?;

        let pubkey_hex = hex::encode_upper(&keypair.public_key);
        Self::from_keypair(keypair, master_keypair, &pubkey_hex)
    }

    /// Build identity from already-derived keypairs.
    fn from_keypair(keypair: Keypair, master_keypair: Keypair, cn: &str) -> Result<Self, NodeError> {
        // Generate a separate TLS keypair (Ed25519) via rcgen.
        // The XRPL node public key goes in the certificate Subject CN
        // so peers can extract it during handshake.
        let tls_key_pair = RcgenKeyPair::generate_for(&rcgen::PKCS_ED25519)
            .map_err(|e| NodeError::Config(format!("TLS key generation failed: {e}")))?;

        let mut params = CertificateParams::new(vec![])
            .map_err(|e| NodeError::Config(format!("cert params failed: {e}")))?;

        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String(cn.to_string()),
        );

        let cert = params
            .self_signed(&tls_key_pair)
            .map_err(|e| NodeError::Config(format!("self-signed cert failed: {e}")))?;

        let tls_cert_der = cert.der().to_vec();
        let tls_key_der = tls_key_pair.serialize_der();

        let cert_chain = vec![CertificateDer::from(tls_cert_der.clone())];
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(tls_key_der.clone()));

        // Server config: present our cert to connecting peers.
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain.clone(), private_key.clone_key())
            .map_err(|e| NodeError::Config(format!("TLS server config failed: {e}")))?;

        // Client config: we don't verify peer certs (self-signed network).
        let client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_client_auth_cert(cert_chain, private_key)
            .map_err(|e| NodeError::Config(format!("TLS client config failed: {e}")))?;

        Ok(Self {
            keypair,
            master_keypair,
            tls_cert_der,
            tls_key_der,
            server_config: Arc::new(server_config),
            client_config: Arc::new(client_config),
        })
    }

    /// The 33-byte compressed Secp256k1 public key (0x02 or 0x03 prefix).
    /// This is the signing/ephemeral key used for validations and peer handshakes.
    pub fn public_key(&self) -> &[u8] {
        &self.keypair.public_key
    }

    /// The 33-byte Ed25519 master public key (0xED prefix + 32 bytes).
    /// This is the permanent validator identity used in manifests.
    pub fn master_public_key(&self) -> &[u8] {
        &self.master_keypair.public_key
    }

    /// Sign a message with the master (Ed25519) key.
    pub fn master_sign(&self, message: &[u8]) -> Result<Vec<u8>, NodeError> {
        self.master_keypair
            .sign(message)
            .map_err(|e| NodeError::PeerProtocol(format!("master signing failed: {e}")))
    }

    /// Hex-encoded public key (uppercase).
    pub fn public_key_hex(&self) -> String {
        hex::encode_upper(&self.keypair.public_key)
    }

    /// Sign a message with the node's private key.
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, NodeError> {
        self.keypair
            .sign(message)
            .map_err(|e| NodeError::PeerProtocol(format!("signing failed: {e}")))
    }

    /// Verify a signature against the node's public key.
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> Result<bool, NodeError> {
        self.keypair
            .verify(message, signature)
            .map_err(|e| NodeError::PeerProtocol(format!("verification failed: {e}")))
    }

    /// DER-encoded self-signed TLS certificate.
    pub fn tls_cert_der(&self) -> &[u8] {
        &self.tls_cert_der
    }

    /// DER-encoded TLS private key.
    pub(crate) fn tls_key_der(&self) -> &[u8] {
        &self.tls_key_der
    }

    /// Get the TLS server config (for accepting inbound connections).
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        self.server_config.clone()
    }

    /// Get the TLS client config (for outbound connections).
    pub fn client_config(&self) -> Arc<rustls::ClientConfig> {
        self.client_config.clone()
    }

    /// Reference to the underlying keypair.
    pub(crate) fn keypair(&self) -> &Keypair {
        &self.keypair
    }
}

impl Drop for NodeIdentity {
    fn drop(&mut self) {
        // SECURITY(1.6): Zeroize TLS private key material on drop.
        // The XRPL Keypair's private_key field is automatically zeroed via
        // ZeroizeOnDrop derived on the Keypair struct in xrpl-core.
        self.tls_key_der.zeroize();
    }
}

/// TLS certificate verifier that accepts any certificate.
/// XRPL peers use self-signed certs — identity is verified
/// at the protocol level via the handshake, not via a CA.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_crypto() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn generate_identity() {
        init_crypto();
        let identity = NodeIdentity::generate().unwrap();

        // Secp256k1 compressed public key is 33 bytes: 0x02 or 0x03 prefix
        assert_eq!(identity.public_key().len(), 33);
        assert!(
            identity.public_key()[0] == 0x02 || identity.public_key()[0] == 0x03,
            "expected compressed secp256k1 prefix, got 0x{:02x}",
            identity.public_key()[0]
        );

        // Hex is 66 chars
        assert_eq!(identity.public_key_hex().len(), 66);

        // TLS cert and key are non-empty
        assert!(!identity.tls_cert_der().is_empty());
        assert!(!identity.tls_key_der().is_empty());
    }

    #[test]
    fn sign_and_verify() {
        init_crypto();
        let identity = NodeIdentity::generate().unwrap();
        let message = b"hello xrpl network";

        let sig = identity.sign(message).unwrap();
        assert!(identity.verify(message, &sig).unwrap());

        // Wrong message should fail
        assert!(!identity.verify(b"wrong message", &sig).unwrap());
    }

    #[test]
    fn deterministic_from_seed() {
        init_crypto();
        let seed = Seed::generate();
        let id1 = NodeIdentity::from_seed(&seed).unwrap();
        let id2 = NodeIdentity::from_seed(&seed).unwrap();

        // Same seed → same XRPL public key
        assert_eq!(id1.public_key(), id2.public_key());
    }

    #[test]
    fn public_key_in_cert_cn() {
        init_crypto();
        let identity = NodeIdentity::generate().unwrap();
        let pubkey_hex = identity.public_key_hex();

        // Verify the DER certificate contains the hex pubkey string
        // (it's embedded in the Subject CN field)
        let cert_bytes = identity.tls_cert_der();
        let pubkey_bytes = pubkey_hex.as_bytes();
        assert!(
            cert_bytes
                .windows(pubkey_bytes.len())
                .any(|w| w == pubkey_bytes),
            "cert should contain pubkey hex in CN"
        );
    }
}
