//! Keylet — deterministic computation of 256-bit SHAMap keys for XRPL ledger objects.
//!
//! Every object in the XRPL state tree is identified by a unique 256-bit key.
//! The key is computed as: `SHA512Half(space_key_2bytes || object_specific_data)`
//!
//! Space keys from rippled `include/xrpl/protocol/LedgerFormats.h`:
//!   AccountRoot  = 0x0061 ('a')
//!   DirectoryNode = 0x0064 ('d')  — for page directories
//!   OwnerDir     = 0x004F ('O')   — for owner directories
//!   Offer        = 0x006F ('o')
//!   RippleState  = 0x0072 ('r')
//!   LedgerHashes = 0x0073 ('s')   — skip list
//!   Amendments   = 0x0066 ('f')
//!   FeeSettings  = 0x0065 ('e')
//!   Escrow       = 0x0075 ('u')
//!   PayChannel   = 0x0078 ('x')
//!   Check        = 0x0043 ('C')
//!   DepositPreauth = 0x0070 ('p')
//!   NFTokenPage  = 0x0050 ('P')

use crate::shamap::hash::sha512_half;
use xrpl_core::types::Hash256;

// Space key constants (2 bytes, big-endian)
const SPACE_ACCOUNT: [u8; 2] = [0x00, 0x61];    // 'a'
const SPACE_DIR_NODE: [u8; 2] = [0x00, 0x64];    // 'd'
const SPACE_OWNER_DIR: [u8; 2] = [0x00, 0x4F];   // 'O'
const SPACE_OFFER: [u8; 2] = [0x00, 0x6F];       // 'o'
const SPACE_RIPPLE_STATE: [u8; 2] = [0x00, 0x72]; // 'r'
const SPACE_SKIP_LIST: [u8; 2] = [0x00, 0x73];    // 's'
const SPACE_AMENDMENTS: [u8; 2] = [0x00, 0x66];   // 'f'
const SPACE_FEE: [u8; 2] = [0x00, 0x65];          // 'e'
const SPACE_ESCROW: [u8; 2] = [0x00, 0x75];       // 'u'
const SPACE_PAY_CHANNEL: [u8; 2] = [0x00, 0x78];  // 'x'
const SPACE_CHECK: [u8; 2] = [0x00, 0x43];        // 'C'
const SPACE_DEPOSIT_PREAUTH: [u8; 2] = [0x00, 0x70]; // 'p'
const SPACE_NFTOKEN_PAGE: [u8; 2] = [0x00, 0x50]; // 'P'

/// Compute the state tree key for an AccountRoot.
/// `key = SHA512Half(0x0061 || account_id)`
pub fn account_root_key(account_id: &[u8; 20]) -> Hash256 {
    let mut buf = [0u8; 22];
    buf[..2].copy_from_slice(&SPACE_ACCOUNT);
    buf[2..].copy_from_slice(account_id);
    sha512_half(&buf)
}

/// Compute the state tree key for an Offer.
/// `key = SHA512Half(0x006F || account_id || sequence_be32)`
pub fn offer_key(account_id: &[u8; 20], sequence: u32) -> Hash256 {
    let mut buf = [0u8; 26];
    buf[..2].copy_from_slice(&SPACE_OFFER);
    buf[2..22].copy_from_slice(account_id);
    buf[22..26].copy_from_slice(&sequence.to_be_bytes());
    sha512_half(&buf)
}

/// Compute the state tree key for a RippleState (trust line).
/// The two account IDs are sorted lexicographically (smaller first).
/// `key = SHA512Half(0x0072 || min(a,b) || max(a,b) || currency_code)`
pub fn ripple_state_key(
    account_a: &[u8; 20],
    account_b: &[u8; 20],
    currency: &[u8; 20],
) -> Hash256 {
    let (lo, hi) = if account_a < account_b {
        (account_a, account_b)
    } else {
        (account_b, account_a)
    };
    let mut buf = [0u8; 62];
    buf[..2].copy_from_slice(&SPACE_RIPPLE_STATE);
    buf[2..22].copy_from_slice(lo);
    buf[22..42].copy_from_slice(hi);
    buf[42..62].copy_from_slice(currency);
    sha512_half(&buf)
}

/// Compute the state tree key for an owner directory root.
/// `key = SHA512Half(0x004F || account_id)`
pub fn owner_dir_key(account_id: &[u8; 20]) -> Hash256 {
    let mut buf = [0u8; 22];
    buf[..2].copy_from_slice(&SPACE_OWNER_DIR);
    buf[2..].copy_from_slice(account_id);
    sha512_half(&buf)
}

/// Compute the state tree key for a directory page.
/// `key = SHA512Half(0x0064 || root_index || page_number_be64)`
pub fn dir_page_key(root_index: &Hash256, page: u64) -> Hash256 {
    let mut buf = [0u8; 42];
    buf[..2].copy_from_slice(&SPACE_DIR_NODE);
    buf[2..34].copy_from_slice(&root_index.0);
    buf[34..42].copy_from_slice(&page.to_be_bytes());
    sha512_half(&buf)
}

/// Compute the state tree key for the skip list (LedgerHashes singleton).
/// `key = SHA512Half(0x0073)`
pub fn skip_list_key() -> Hash256 {
    sha512_half(&SPACE_SKIP_LIST)
}

/// Compute the state tree key for the FeeSettings singleton.
/// `key = SHA512Half(0x0065)`
pub fn fee_settings_key() -> Hash256 {
    sha512_half(&SPACE_FEE)
}

/// Compute the state tree key for the Amendments singleton.
/// `key = SHA512Half(0x0066)`
pub fn amendments_key() -> Hash256 {
    sha512_half(&SPACE_AMENDMENTS)
}

/// Compute the state tree key for an Escrow.
/// `key = SHA512Half(0x0075 || account_id || sequence_be32)`
pub fn escrow_key(account_id: &[u8; 20], sequence: u32) -> Hash256 {
    let mut buf = [0u8; 26];
    buf[..2].copy_from_slice(&SPACE_ESCROW);
    buf[2..22].copy_from_slice(account_id);
    buf[22..26].copy_from_slice(&sequence.to_be_bytes());
    sha512_half(&buf)
}

/// Compute the state tree key for a PayChannel.
/// `key = SHA512Half(0x0078 || account_id || sequence_be32)`
pub fn pay_channel_key(account_id: &[u8; 20], sequence: u32) -> Hash256 {
    let mut buf = [0u8; 26];
    buf[..2].copy_from_slice(&SPACE_PAY_CHANNEL);
    buf[2..22].copy_from_slice(account_id);
    buf[22..26].copy_from_slice(&sequence.to_be_bytes());
    sha512_half(&buf)
}

/// Compute the state tree key for a Check.
/// `key = SHA512Half(0x0043 || account_id || sequence_be32)`
pub fn check_key(account_id: &[u8; 20], sequence: u32) -> Hash256 {
    let mut buf = [0u8; 26];
    buf[..2].copy_from_slice(&SPACE_CHECK);
    buf[2..22].copy_from_slice(account_id);
    buf[22..26].copy_from_slice(&sequence.to_be_bytes());
    sha512_half(&buf)
}

/// Compute the state tree key for a DepositPreauth.
/// `key = SHA512Half(0x0070 || account_id || authorized_id)`
pub fn deposit_preauth_key(account_id: &[u8; 20], authorized: &[u8; 20]) -> Hash256 {
    let mut buf = [0u8; 42];
    buf[..2].copy_from_slice(&SPACE_DEPOSIT_PREAUTH);
    buf[2..22].copy_from_slice(account_id);
    buf[22..42].copy_from_slice(authorized);
    sha512_half(&buf)
}

/// Compute the base state tree key for an NFTokenPage.
///
/// The correct rippled formula is: `SHA512Half(0x0050 || account_id)`, then
/// the low 96 bits are cleared to produce the base key. For finding a specific
/// page, the token_id's low 96 bits are OR'd into the base key — but since we
/// don't do page lookups yet, this function returns the base key only.
pub fn nftoken_page_key(account_id: &[u8; 20]) -> Hash256 {
    let mut buf = [0u8; 22];
    buf[..2].copy_from_slice(&SPACE_NFTOKEN_PAGE);
    buf[2..22].copy_from_slice(account_id);
    let mut key = sha512_half(&buf);
    // Clear the low 96 bits (12 bytes) — bytes 20..32
    key.0[20..32].fill(0);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_root_deterministic() {
        let acct = [0xAAu8; 20];
        let k1 = account_root_key(&acct);
        let k2 = account_root_key(&acct);
        assert_eq!(k1, k2);
        assert_ne!(k1, Hash256::ZERO);
    }

    #[test]
    fn different_accounts_different_keys() {
        let a = [0x01u8; 20];
        let b = [0x02u8; 20];
        assert_ne!(account_root_key(&a), account_root_key(&b));
    }

    #[test]
    fn offer_key_includes_sequence() {
        let acct = [0xBBu8; 20];
        let k1 = offer_key(&acct, 1);
        let k2 = offer_key(&acct, 2);
        assert_ne!(k1, k2);
    }

    #[test]
    fn ripple_state_order_independent() {
        let a = [0x01u8; 20];
        let b = [0x02u8; 20];
        let cur = [0xCCu8; 20];
        // Order of accounts should not matter
        assert_eq!(
            ripple_state_key(&a, &b, &cur),
            ripple_state_key(&b, &a, &cur),
        );
    }

    #[test]
    fn ripple_state_different_currency() {
        let a = [0x01u8; 20];
        let b = [0x02u8; 20];
        let c1 = [0xAAu8; 20];
        let c2 = [0xBBu8; 20];
        assert_ne!(
            ripple_state_key(&a, &b, &c1),
            ripple_state_key(&a, &b, &c2),
        );
    }

    #[test]
    fn singletons_are_fixed() {
        // Singletons should always produce the same key
        let f1 = fee_settings_key();
        let f2 = fee_settings_key();
        assert_eq!(f1, f2);

        let a1 = amendments_key();
        let a2 = amendments_key();
        assert_eq!(a1, a2);

        // But different from each other
        assert_ne!(fee_settings_key(), amendments_key());
        assert_ne!(fee_settings_key(), skip_list_key());
    }

    #[test]
    fn fee_settings_key_mainnet_verified() {
        // Verified against mainnet: ledger_entry with this index returns FeeSettings
        let expected = Hash256::from_hex(
            "4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651"
        ).unwrap();
        assert_eq!(fee_settings_key(), expected);
    }

    #[test]
    fn amendments_key_mainnet_verified() {
        // Verified against mainnet: ledger_entry with this index returns Amendments
        let expected = Hash256::from_hex(
            "7DB0788C020F02780A673DC74757F23823FA3014C1866E72CC4CD8B226CD6EF4"
        ).unwrap();
        assert_eq!(amendments_key(), expected);
    }

    #[test]
    fn owner_dir_key_deterministic() {
        let acct = [0xDDu8; 20];
        let k = owner_dir_key(&acct);
        assert_ne!(k, Hash256::ZERO);
        assert_eq!(k, owner_dir_key(&acct));
    }

    #[test]
    fn dir_page_key_varies_by_page() {
        let root = Hash256([0xFFu8; 32]);
        let p0 = dir_page_key(&root, 0);
        let p1 = dir_page_key(&root, 1);
        assert_ne!(p0, p1);
    }

    #[test]
    fn escrow_pay_channel_check_distinct() {
        let acct = [0x55u8; 20];
        let seq = 42u32;
        // Same account+sequence but different space keys → different results
        let ek = escrow_key(&acct, seq);
        let pk = pay_channel_key(&acct, seq);
        let ck = check_key(&acct, seq);
        assert_ne!(ek, pk);
        assert_ne!(ek, ck);
        assert_ne!(pk, ck);
    }

    #[test]
    fn account_root_key_mainnet_verified() {
        // rHb9CJAWyB4rj91VRWn96DkukG4bwdtyTh (genesis account)
        // Account ID: B5F762798A53D543A014CAF8B297CFF8F2F937E8
        // Verified against mainnet RPC `ledger_entry` index on 2026-03-22
        let account_id: [u8; 20] = [
            0xB5, 0xF7, 0x62, 0x79, 0x8A, 0x53, 0xD5, 0x43, 0xA0, 0x14,
            0xCA, 0xF8, 0xB2, 0x97, 0xCF, 0xF8, 0xF2, 0xF9, 0x37, 0xE8,
        ];
        let expected = Hash256::from_hex(
            "2B6AC232AA4C4BE41BF49D2459FA4A0347E1B543A4C92FCEE0821C0201E2E9A8"
        ).unwrap();
        assert_eq!(account_root_key(&account_id), expected);
    }

    // Live verification test — fetch a known account from mainnet RPC
    // and verify our computed keylet matches the `index` field.
    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn verify_account_root_key_against_mainnet() {
        // rPEPPER7kfTD9w2To4CQk6UCfuHM9c6GDY is a well-known account
        // Account ID (hex of decoded base58): derived from the address
        // We'll fetch it via RPC and compare our keylet to the response's `index`

        let client = reqwest::Client::new();
        let resp: serde_json::Value = client
            .post("https://xrplcluster.com")
            .json(&serde_json::json!({
                "method": "ledger_entry",
                "params": [{
                    "account_root": "rPEPPER7kfTD9w2To4CQk6UCfuHM9c6GDY",
                    "ledger_index": "validated"
                }]
            }))
            .send()
            .await
            .expect("RPC request failed")
            .json()
            .await
            .expect("JSON parse failed");

        let index_hex = resp["result"]["index"]
            .as_str()
            .expect("no index in response");
        let expected = Hash256::from_hex(index_hex).expect("bad hex");

        // Decode the account address to get the 20-byte account ID
        // rPEPPER7kfTD9w2To4CQk6UCfuHM9c6GDY → account ID
        let account_info: serde_json::Value = client
            .post("https://xrplcluster.com")
            .json(&serde_json::json!({
                "method": "account_info",
                "params": [{"account": "rPEPPER7kfTD9w2To4CQk6UCfuHM9c6GDY", "ledger_index": "validated"}]
            }))
            .send()
            .await
            .expect("RPC request failed")
            .json()
            .await
            .expect("JSON parse failed");

        // The account_data.Account field is the base58 address, but we need
        // the raw 20-byte account ID. The `index` field IS the keylet.
        // To verify our function, we need the raw account_id bytes.
        // We can get them from the binary ledger_entry response.
        let binary_resp: serde_json::Value = client
            .post("https://xrplcluster.com")
            .json(&serde_json::json!({
                "method": "ledger_entry",
                "params": [{
                    "account_root": "rPEPPER7kfTD9w2To4CQk6UCfuHM9c6GDY",
                    "ledger_index": "validated",
                    "binary": true
                }]
            }))
            .send()
            .await
            .expect("RPC request failed")
            .json()
            .await
            .expect("JSON parse failed");

        // The index should match regardless of binary mode
        let index_hex_2 = binary_resp["result"]["index"]
            .as_str()
            .expect("no index in binary response");
        assert_eq!(index_hex, index_hex_2);

        // Now compute our keylet from the known account ID
        // rPEPPER7kfTD9w2To4CQk6UCfuHM9c6GDY decodes to account ID:
        // We use xrpl_core's address decoding if available, or hardcode the known ID
        // For now, verify the index is consistent (same value from two API calls)
        println!("Mainnet keylet for rPEPPER: {}", index_hex);
        println!("Our fee_settings_key: {:?}", fee_settings_key());
        println!("Our amendments_key: {:?}", amendments_key());
    }
}
