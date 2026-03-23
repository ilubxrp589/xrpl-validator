//! NFToken transaction types — Mint, Burn, CreateOffer, AcceptOffer, CancelOffer.
//!
//! Simplified implementations: create/delete NFToken entries and NFT offers,
//! transfer token ownership on accept.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::shamap::hash::sha512_half;

/// Compute a deterministic key for an NFToken object.
/// `key = SHA512Half(0x0050 || account_id || taxon_be32 || sequence_be32)`
///
/// This is a simplified key — real rippled uses NFTokenPage paging.
/// We use the base nftoken_page_key XOR'd with a hash of (taxon, sequence)
/// so each token gets a unique key under the owner's page space.
fn nftoken_object_key(account: &[u8; 20], taxon: u32, sequence: u32) -> xrpl_core::types::Hash256 {
    let mut buf = [0u8; 30];
    // space 'P' for NFTokenPage
    buf[0] = 0x00;
    buf[1] = 0x50;
    buf[2..22].copy_from_slice(account);
    buf[22..26].copy_from_slice(&taxon.to_be_bytes());
    buf[26..30].copy_from_slice(&sequence.to_be_bytes());
    sha512_half(&buf)
}

/// Compute a deterministic key for an NFToken offer.
/// Reuses the offer keylet space but with a different discriminator.
fn nft_offer_key(account: &[u8; 20], sequence: u32) -> xrpl_core::types::Hash256 {
    // Use the standard offer_key — NFT offers in our simplified model
    // share the offer space but that's fine since account+sequence is unique.
    keylet::offer_key(account, sequence)
}

/// Helper: read an account, increment OwnerCount, write back.
fn increment_owner_count(account: &[u8; 20], sandbox: &mut Sandbox) {
    let acct_key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&acct_key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let count = acct["OwnerCount"].as_u64().unwrap_or(0);
            acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
            sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

/// Helper: read an account, decrement OwnerCount (clamped to 0), write back.
fn decrement_owner_count(account: &[u8; 20], sandbox: &mut Sandbox) {
    let acct_key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&acct_key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let count = acct["OwnerCount"].as_u64().unwrap_or(0);
            if count > 0 {
                acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
            }
            sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

/// Helper: decode a 20-byte hex account ID from a JSON string field.
fn decode_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    let hex_str = val.as_str()?;
    let bytes = hex::decode(hex_str).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Some(arr)
}

// ---------------------------------------------------------------------------
// NFTokenMint
// ---------------------------------------------------------------------------

/// NFTokenMint transactor — create an NFToken entry in state.
pub struct NFTokenMintTransactor;

impl Transactor for NFTokenMintTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenMint" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // NFTokenTaxon is required
        if tx.fields.get("NFTokenTaxon").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let taxon = match tx.fields.get("NFTokenTaxon").and_then(|v| v.as_u64()) {
            Some(t) => t as u32,
            None => return TxResult::Malformed,
        };

        let seq = if tx.uses_ticket() {
            tx.ticket_seq.unwrap_or(0)
        } else {
            tx.sequence
        };

        let token_key = nftoken_object_key(&tx.account, taxon, seq);

        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let uri = tx.fields.get("URI").cloned().unwrap_or(serde_json::Value::Null);
        let transfer_fee = tx.fields.get("TransferFee").and_then(|f| f.as_u64()).unwrap_or(0);

        // Determine the issuer — may be the sender or an explicit Issuer field
        let issuer_hex = if let Some(issuer_val) = tx.fields.get("Issuer") {
            issuer_val.as_str().unwrap_or(&hex::encode(tx.account)).to_string()
        } else {
            hex::encode(tx.account)
        };

        let token_obj = serde_json::json!({
            "LedgerEntryType": "NFToken",
            "Owner": hex::encode(tx.account),
            "Issuer": issuer_hex,
            "NFTokenTaxon": taxon,
            "Sequence": seq,
            "Flags": flags,
            "URI": uri,
            "TransferFee": transfer_fee,
        });

        sandbox.write(token_key, serde_json::to_vec(&token_obj).unwrap());
        increment_owner_count(&tx.account, sandbox);

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenBurn
// ---------------------------------------------------------------------------

/// NFTokenBurn transactor — delete an NFToken from state.
pub struct NFTokenBurnTransactor;

impl Transactor for NFTokenBurnTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenBurn" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // NFTokenID is required
        if tx.fields.get("NFTokenID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // In our simplified model, the NFTokenID encodes taxon + sequence
        // so we can reconstruct the key. We expect NFTokenID to be a JSON
        // object with {taxon, sequence} or alternatively {owner, taxon, sequence}.
        let nft_id = &tx.fields["NFTokenID"];

        let owner = if let Some(owner_val) = nft_id.get("Owner") {
            match decode_account_id(owner_val) {
                Some(id) => id,
                None => return TxResult::Malformed,
            }
        } else {
            tx.account
        };

        let taxon = match nft_id.get("Taxon").and_then(|v| v.as_u64()) {
            Some(t) => t as u32,
            None => return TxResult::Malformed,
        };
        let sequence = match nft_id.get("Sequence").and_then(|v| v.as_u64()) {
            Some(s) => s as u32,
            None => return TxResult::Malformed,
        };

        let token_key = nftoken_object_key(&owner, taxon, sequence);

        if !sandbox.exists(&token_key) {
            return TxResult::NoEntry;
        }

        // Verify the sender has the right to burn (must be owner or issuer with lsfBurnable)
        if let Some(data) = sandbox.read(&token_key) {
            if let Ok(token) = serde_json::from_slice::<serde_json::Value>(&data) {
                let token_owner_hex = token["Owner"].as_str().unwrap_or("");
                let sender_hex = hex::encode(tx.account);
                if token_owner_hex != sender_hex {
                    // Check if sender is the issuer with burn flag (0x0001 = lsfBurnable)
                    let issuer_hex = token["Issuer"].as_str().unwrap_or("");
                    let flags = token["Flags"].as_u64().unwrap_or(0);
                    if issuer_hex != sender_hex || flags & 0x0001 == 0 {
                        return TxResult::NoPermission;
                    }
                }
            }
        }

        sandbox.delete(token_key);
        decrement_owner_count(&owner, sandbox);

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenCreateOffer
// ---------------------------------------------------------------------------

/// NFTokenCreateOffer transactor — create an offer to buy or sell an NFToken.
pub struct NFTokenCreateOfferTransactor;

impl Transactor for NFTokenCreateOfferTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenCreateOffer" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // NFTokenID and Amount are required
        if tx.fields.get("NFTokenID").is_none() {
            return TxResult::Malformed;
        }
        if tx.fields.get("Amount").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let seq = if tx.uses_ticket() {
            tx.ticket_seq.unwrap_or(0)
        } else {
            tx.sequence
        };

        let offer_key = nft_offer_key(&tx.account, seq);

        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        // tfSellNFToken = 0x00000001
        let is_sell = flags & 0x00000001 != 0;

        let mut offer_obj = serde_json::json!({
            "LedgerEntryType": "NFTokenOffer",
            "Owner": hex::encode(tx.account),
            "NFTokenID": tx.fields["NFTokenID"].clone(),
            "Amount": tx.fields["Amount"].clone(),
            "Flags": flags,
            "Sequence": seq,
            "IsSell": is_sell,
        });

        // Optional: Destination (restrict who can accept the offer)
        if let Some(dest) = tx.fields.get("Destination") {
            offer_obj["Destination"] = dest.clone();
        }

        // Optional: Expiration
        if let Some(exp) = tx.fields.get("Expiration") {
            offer_obj["Expiration"] = exp.clone();
        }

        sandbox.write(offer_key, serde_json::to_vec(&offer_obj).unwrap());
        increment_owner_count(&tx.account, sandbox);

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenAcceptOffer
// ---------------------------------------------------------------------------

/// NFTokenAcceptOffer transactor — accept an NFT offer, transferring the token.
pub struct NFTokenAcceptOfferTransactor;

impl Transactor for NFTokenAcceptOfferTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenAcceptOffer" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // Must have at least one of NFTokenSellOffer or NFTokenBuyOffer
        if tx.fields.get("NFTokenSellOffer").is_none()
            && tx.fields.get("NFTokenBuyOffer").is_none()
        {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Determine which offer to accept (sell offer has priority in brokered mode)
        let offer_field = if tx.fields.get("NFTokenSellOffer").is_some() {
            "NFTokenSellOffer"
        } else {
            "NFTokenBuyOffer"
        };

        let offer_ref = &tx.fields[offer_field];

        // The offer reference contains {Account, Sequence} to locate the offer object
        let offer_owner = match offer_ref.get("Account").and_then(|v| decode_account_id(v)) {
            Some(id) => id,
            None => return TxResult::Malformed,
        };
        let offer_seq = match offer_ref.get("Sequence").and_then(|v| v.as_u64()) {
            Some(s) => s as u32,
            None => return TxResult::Malformed,
        };

        let offer_key = nft_offer_key(&offer_owner, offer_seq);

        // Read the offer
        let offer_data = match sandbox.read(&offer_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let offer: serde_json::Value = match serde_json::from_slice(&offer_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Bug 2: Self-accept check — you cannot accept your own offer.
        // For sell offers, the offer owner is the seller; tx.account must not be the seller.
        // For buy offers, the offer owner is the buyer; tx.account must not be the buyer.
        let is_sell = offer["IsSell"].as_bool().unwrap_or(false);
        if is_sell && tx.account == offer_owner {
            // Seller trying to accept their own sell offer
            return TxResult::NoPermission;
        }
        if !is_sell && tx.account == offer_owner {
            // Buyer trying to accept their own buy offer
            return TxResult::NoPermission;
        }

        // Get the NFTokenID from the offer to locate the token
        let nft_id = &offer["NFTokenID"];
        let token_owner_id = if let Some(owner_val) = nft_id.get("Owner") {
            match decode_account_id(owner_val) {
                Some(id) => id,
                None => return TxResult::Malformed,
            }
        } else {
            // If no Owner in NFTokenID, the offer owner is the token owner (sell)
            // or the tx sender is buying (buy offer)
            if is_sell { offer_owner } else { tx.account }
        };

        let taxon = match nft_id.get("Taxon").and_then(|v| v.as_u64()) {
            Some(t) => t as u32,
            None => return TxResult::Malformed,
        };
        let token_seq = match nft_id.get("Sequence").and_then(|v| v.as_u64()) {
            Some(s) => s as u32,
            None => return TxResult::Malformed,
        };

        let old_token_key = nftoken_object_key(&token_owner_id, taxon, token_seq);

        // Read the token
        let token_data = match sandbox.read(&old_token_key) {
            Some(d) => d,
            None => return TxResult::NoEntry,
        };
        let mut token: serde_json::Value = match serde_json::from_slice(&token_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // Determine new owner: if sell offer, buyer is tx.account; if buy offer, seller accepts
        let new_owner = if is_sell { tx.account } else { offer_owner };

        // Transfer: update the Owner field
        let old_owner_hex = token["Owner"].as_str().unwrap_or("").to_string();
        token["Owner"] = serde_json::Value::String(hex::encode(new_owner));

        // Bug 7: Recompute key for new owner, delete old key, write at new key.
        // The nftoken_object_key is derived from the owner, so transferring
        // ownership requires moving the entry to the new owner's key space.
        let new_token_key = nftoken_object_key(&new_owner, taxon, token_seq);
        sandbox.delete(old_token_key);
        sandbox.write(new_token_key, serde_json::to_vec(&token).unwrap());

        // Adjust OwnerCount: decrement old owner, increment new owner
        if let Ok(old_bytes) = hex::decode(&old_owner_hex) {
            if old_bytes.len() == 20 {
                let mut old_id = [0u8; 20];
                old_id.copy_from_slice(&old_bytes);
                decrement_owner_count(&old_id, sandbox);
            }
        }
        increment_owner_count(&new_owner, sandbox);

        // Delete the offer
        sandbox.delete(offer_key);
        decrement_owner_count(&offer_owner, sandbox);

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenCancelOffer
// ---------------------------------------------------------------------------

/// NFTokenCancelOffer transactor — cancel (delete) one or more NFT offers.
pub struct NFTokenCancelOfferTransactor;

impl Transactor for NFTokenCancelOfferTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenCancelOffer" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // NFTokenOffers array is required
        if tx.fields.get("NFTokenOffers").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let offers = match tx.fields.get("NFTokenOffers").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => return TxResult::Malformed,
        };

        for offer_ref in &offers {
            let offer_owner = match offer_ref.get("Account").and_then(|v| decode_account_id(v)) {
                Some(id) => id,
                None => continue, // skip malformed entries
            };
            let offer_seq = match offer_ref.get("Sequence").and_then(|v| v.as_u64()) {
                Some(s) => s as u32,
                None => continue,
            };

            let offer_key = nft_offer_key(&offer_owner, offer_seq);

            if sandbox.exists(&offer_key) {
                // Verify the sender has permission to cancel:
                // - Owner of the offer, OR
                // - The offer has expired, OR
                // - The NFToken's owner (for sell offers)
                // Simplified: only the offer owner can cancel.
                if let Some(data) = sandbox.read(&offer_key) {
                    if let Ok(offer) = serde_json::from_slice::<serde_json::Value>(&data) {
                        let owner_hex = offer["Owner"].as_str().unwrap_or("");
                        let sender_hex = hex::encode(tx.account);
                        if owner_hex != sender_hex {
                            // In full rippled, other parties can cancel expired offers.
                            // Simplified: skip offers we don't own.
                            continue;
                        }
                    }
                }

                sandbox.delete(offer_key);
                decrement_owner_count(&offer_owner, sandbox);
            }
        }

        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn make_state(accounts: &[([u8; 20], u64)]) -> LedgerState {
        let header = LedgerHeader {
            sequence: 100,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        let mut state = LedgerState::new_unverified(header);
        for (id, balance) in accounts {
            let acct = serde_json::json!({
                "LedgerEntryType": "AccountRoot",
                "Account": hex::encode(id),
                "Balance": balance.to_string(),
                "Sequence": 1,
                "OwnerCount": 0,
            });
            let key = keylet::account_root_key(id);
            state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        }
        state
    }

    fn read_owner_count(sandbox: &Sandbox, id: &[u8; 20]) -> u64 {
        let key = keylet::account_root_key(id);
        let data = sandbox.read(&key).expect("account not found");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        v["OwnerCount"].as_u64().unwrap_or(0)
    }

    #[test]
    fn mint_creates_nftoken() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "NFTokenMint".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenTaxon": 0,
                "URI": "ipfs://abc123",
            }),
        };

        assert_eq!(NFTokenMintTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(NFTokenMintTransactor.preclaim(&tx, &sandbox), TxResult::Success);
        assert_eq!(NFTokenMintTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Token should exist
        let token_key = nftoken_object_key(&alice, 0, 1);
        assert!(sandbox.exists(&token_key));
        assert_eq!(read_owner_count(&sandbox, &alice), 1);
    }

    #[test]
    fn mint_preflight_rejects_missing_taxon() {
        let alice = [0x01u8; 20];
        let tx = TxFields {
            account: alice,
            tx_type: "NFTokenMint".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({}),
        };
        assert_eq!(NFTokenMintTransactor.preflight(&tx), TxResult::Malformed);
    }

    #[test]
    fn burn_deletes_nftoken() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // First mint
        let mint_tx = TxFields {
            account: alice,
            tx_type: "NFTokenMint".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"NFTokenTaxon": 42}),
        };
        NFTokenMintTransactor.do_apply(&mint_tx, &mut sandbox);
        assert_eq!(read_owner_count(&sandbox, &alice), 1);

        // Then burn
        let burn_tx = TxFields {
            account: alice,
            tx_type: "NFTokenBurn".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenID": {"Taxon": 42, "Sequence": 1},
            }),
        };

        assert_eq!(NFTokenBurnTransactor.preflight(&burn_tx), TxResult::Success);
        assert_eq!(NFTokenBurnTransactor.do_apply(&burn_tx, &mut sandbox), TxResult::Success);

        let token_key = nftoken_object_key(&alice, 42, 1);
        assert!(!sandbox.exists(&token_key));
        assert_eq!(read_owner_count(&sandbox, &alice), 0);
    }

    #[test]
    fn burn_nonexistent_token_fails() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "NFTokenBurn".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenID": {"Taxon": 99, "Sequence": 1},
            }),
        };
        assert_eq!(NFTokenBurnTransactor.do_apply(&tx, &mut sandbox), TxResult::NoEntry);
    }

    #[test]
    fn create_offer_and_cancel() {
        let alice = [0x01u8; 20];
        let state = make_state(&[(alice, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Create offer
        let create_tx = TxFields {
            account: alice,
            tx_type: "NFTokenCreateOffer".to_string(),
            fee: 12,
            sequence: 5,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenID": {"Taxon": 0, "Sequence": 1},
                "Amount": "10000000",
                "Flags": 1, // tfSellNFToken
            }),
        };

        assert_eq!(NFTokenCreateOfferTransactor.preflight(&create_tx), TxResult::Success);
        assert_eq!(NFTokenCreateOfferTransactor.do_apply(&create_tx, &mut sandbox), TxResult::Success);

        let offer_key = nft_offer_key(&alice, 5);
        assert!(sandbox.exists(&offer_key));
        assert_eq!(read_owner_count(&sandbox, &alice), 1);

        // Cancel offer
        let cancel_tx = TxFields {
            account: alice,
            tx_type: "NFTokenCancelOffer".to_string(),
            fee: 12,
            sequence: 6,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenOffers": [
                    {"Account": hex::encode(alice), "Sequence": 5}
                ]
            }),
        };

        assert_eq!(NFTokenCancelOfferTransactor.preflight(&cancel_tx), TxResult::Success);
        assert_eq!(NFTokenCancelOfferTransactor.do_apply(&cancel_tx, &mut sandbox), TxResult::Success);

        assert!(!sandbox.exists(&offer_key));
        assert_eq!(read_owner_count(&sandbox, &alice), 0);
    }

    #[test]
    fn accept_sell_offer_transfers_token() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let state = make_state(&[(alice, 50_000_000), (bob, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);

        // Alice mints a token
        let mint_tx = TxFields {
            account: alice,
            tx_type: "NFTokenMint".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"NFTokenTaxon": 10}),
        };
        NFTokenMintTransactor.do_apply(&mint_tx, &mut sandbox);

        // Alice creates a sell offer
        let offer_tx = TxFields {
            account: alice,
            tx_type: "NFTokenCreateOffer".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenID": {"Owner": hex::encode(alice), "Taxon": 10, "Sequence": 1},
                "Amount": "5000000",
                "Flags": 1, // tfSellNFToken
            }),
        };
        NFTokenCreateOfferTransactor.do_apply(&offer_tx, &mut sandbox);

        // Bob accepts the sell offer
        let accept_tx = TxFields {
            account: bob,
            tx_type: "NFTokenAcceptOffer".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenSellOffer": {
                    "Account": hex::encode(alice),
                    "Sequence": 2
                }
            }),
        };

        assert_eq!(NFTokenAcceptOfferTransactor.preflight(&accept_tx), TxResult::Success);
        assert_eq!(NFTokenAcceptOfferTransactor.preclaim(&accept_tx, &sandbox), TxResult::Success);
        assert_eq!(NFTokenAcceptOfferTransactor.do_apply(&accept_tx, &mut sandbox), TxResult::Success);

        // Token should now be at Bob's key (recomputed for new owner)
        let old_token_key = nftoken_object_key(&alice, 10, 1);
        assert!(!sandbox.exists(&old_token_key), "old key should be deleted after transfer");

        let new_token_key = nftoken_object_key(&bob, 10, 1);
        let token_data = sandbox.read(&new_token_key).unwrap();
        let token: serde_json::Value = serde_json::from_slice(&token_data).unwrap();
        assert_eq!(token["Owner"].as_str().unwrap(), hex::encode(bob));

        // Offer should be deleted
        let offer_key = nft_offer_key(&alice, 2);
        assert!(!sandbox.exists(&offer_key));
    }

    #[test]
    fn accept_nonexistent_offer_fails() {
        let bob = [0x02u8; 20];
        let state = make_state(&[(bob, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: bob,
            tx_type: "NFTokenAcceptOffer".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenSellOffer": {
                    "Account": hex::encode([0xFFu8; 20]),
                    "Sequence": 99
                }
            }),
        };
        assert_eq!(NFTokenAcceptOfferTransactor.do_apply(&tx, &mut sandbox), TxResult::NoEntry);
    }
}
