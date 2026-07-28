//! NFToken transactors — Mint, Burn, CreateOffer, AcceptOffer, CancelOffer,
//! Modify — on the real NFTokenPage model (`ledger::nftpage`).
//!
//! rippled reference: `NFTokenMint.cpp`, `NFTokenAcceptOffer.cpp`,
//! `NFTokenUtils.cpp`. NFTokenID composition (32 bytes):
//! `flags_be16 ‖ transfer_fee_be16 ‖ issuer_20 ‖ (taxon ^ scramble)_be32 ‖
//! token_seq_be32`, where `scramble = 384160001 * token_seq + 2459`
//! (wrapping) and `token_seq = FirstNFTokenSequence + MintedNFTokens` on the
//! issuer's AccountRoot. Offers are referenced by ledger index (Hash256), and
//! each offer lives in BOTH its owner's directory and the token's buy/sell
//! offer directory.

use crate::ledger::directory::{dir_insert, dir_remove, owner_dir_insert, owner_dir_remove};
use crate::ledger::keylet;
use crate::ledger::nftpage;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use xrpl_core::types::Hash256;

/// Whether an account carries `lsfDisallowIncomingNFTokenOffer` (0x04000000),
/// the opt-out from receiving NFT offers. `None` when the account does not
/// exist, which the caller distinguishes from a plain "no".
fn disallows_incoming_nft_offer(sandbox: &Sandbox, id: &[u8; 20]) -> Option<bool> {
    let data = sandbox.read(&keylet::account_root_key(id))?;
    let acct: serde_json::Value = serde_json::from_slice(&data).ok()?;
    Some(acct["Flags"].as_u64().unwrap_or(0) & 0x0400_0000 != 0)
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

/// Helper: decode a 20-byte account ID from a JSON string field.
///
/// Accepts both 40-char hex (as the probe emits for hex-normalised fields) and
/// a base58 r-address. The `Issuer` field on an authorised-minter mint is NOT
/// in the probe's hex-normalisation set, so it arrives base58 — decoding it
/// hex-only silently fell back to the minter, crediting MintedNFTokens to the
/// wrong account and embedding the wrong issuer in the NFTokenID.
fn decode_account_id(val: &serde_json::Value) -> Option<[u8; 20]> {
    crate::tx::offer::decode20(val.as_str()?)
}

/// Decode a 64-hex Hash256 field (NFTokenID, offer index).
fn hash256_from(val: &serde_json::Value) -> Option<Hash256> {
    let b = hex::decode(val.as_str()?).ok()?;
    (b.len() == 32).then(|| {
        let mut k = [0u8; 32];
        k.copy_from_slice(&b);
        Hash256(k)
    })
}

/// Directory page hint from an SLE field (UInt64 as number or hex string).
fn node_hint(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
}

/// Adjust an account's XRP Balance by `delta` drops (negative = debit).
fn adjust_xrp(sandbox: &mut Sandbox, account: &[u8; 20], delta: i128) {
    if delta == 0 {
        return;
    }
    let key = keylet::account_root_key(account);
    if let Some(d) = sandbox.read(&key) {
        if let Ok(mut a) = serde_json::from_slice::<serde_json::Value>(&d) {
            let bal = a["Balance"]
                .as_str()
                .and_then(|s| s.parse::<i128>().ok())
                .unwrap_or(0);
            a["Balance"] = serde_json::Value::String((bal + delta).max(0).to_string());
            sandbox.write(key, serde_json::to_vec(&a).unwrap_or_default());
        }
    }
}

// ---------------------------------------------------------------------------
// NFTokenMint
// ---------------------------------------------------------------------------

pub struct NFTokenMintTransactor;

impl Transactor for NFTokenMintTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenMint" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
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
        let issuer = tx
            .fields
            .get("Issuer")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);

        // token_seq comes off the ISSUER's account root.
        let issuer_key = keylet::account_root_key(&issuer);
        let Some(idata) = sandbox.read(&issuer_key) else {
            return TxResult::NoAccount;
        };
        let Ok(mut iacct) = serde_json::from_slice::<serde_json::Value>(&idata) else {
            return TxResult::Malformed;
        };
        let minted = iacct["MintedNFTokens"].as_u64().unwrap_or(0) as u32;
        let first = iacct["FirstNFTokenSequence"]
            .as_u64()
            .map(|v| v as u32)
            .unwrap_or_else(|| iacct["Sequence"].as_u64().unwrap_or(0) as u32);
        let token_seq = first.wrapping_add(minted);

        let scramble = 384_160_001u32.wrapping_mul(token_seq).wrapping_add(2459);
        let flags16 = (tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0xFFFF) as u16;
        let fee16 = tx
            .fields
            .get("TransferFee")
            .and_then(|f| f.as_u64())
            .unwrap_or(0) as u16;

        let mut id = [0u8; 32];
        id[0..2].copy_from_slice(&flags16.to_be_bytes());
        id[2..4].copy_from_slice(&fee16.to_be_bytes());
        id[4..24].copy_from_slice(&issuer);
        id[24..28].copy_from_slice(&(taxon ^ scramble).to_be_bytes());
        id[28..32].copy_from_slice(&token_seq.to_be_bytes());

        let mut token = serde_json::json!({ "NFTokenID": hex::encode_upper(id) });
        if let Some(uri) = tx.fields.get("URI") {
            token["URI"] = uri.clone();
        }
        let created = nftpage::page_insert(
            sandbox,
            &tx.account,
            serde_json::json!({ "NFToken": token }),
        );
        if created {
            increment_owner_count(&tx.account, sandbox);
        }

        // Re-read: the OwnerCount bump above may have rewritten the same root.
        if let Some(d) = sandbox.read(&issuer_key) {
            if let Ok(a) = serde_json::from_slice::<serde_json::Value>(&d) {
                iacct = a;
            }
        }
        iacct["MintedNFTokens"] = serde_json::json!(minted as u64 + 1);
        if iacct.get("FirstNFTokenSequence").is_none() {
            iacct["FirstNFTokenSequence"] = serde_json::json!(first);
        }
        sandbox.write(issuer_key, serde_json::to_vec(&iacct).unwrap_or_default());

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenBurn
// ---------------------------------------------------------------------------

pub struct NFTokenBurnTransactor;

impl Transactor for NFTokenBurnTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenBurn" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
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
        let Some(id) = tx.fields.get("NFTokenID").and_then(hash256_from) else {
            return TxResult::Malformed;
        };
        let owner = tx
            .fields
            .get("Owner")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);

        let Some(removal) = nftpage::page_remove(sandbox, &owner, &id) else {
            return TxResult::NoEntry;
        };
        if removal.page_deleted {
            decrement_owner_count(&owner, sandbox);
        }

        let issuer = nftpage::issuer_of(&id);
        let issuer_key = keylet::account_root_key(&issuer);
        if let Some(d) = sandbox.read(&issuer_key) {
            if let Ok(mut a) = serde_json::from_slice::<serde_json::Value>(&d) {
                let burned = a["BurnedNFTokens"].as_u64().unwrap_or(0);
                a["BurnedNFTokens"] = serde_json::json!(burned + 1);
                sandbox.write(issuer_key, serde_json::to_vec(&a).unwrap_or_default());
            }
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenCreateOffer
// ---------------------------------------------------------------------------

pub struct NFTokenCreateOfferTransactor;

impl Transactor for NFTokenCreateOfferTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenCreateOffer" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("NFTokenID").is_none() || tx.fields.get("Amount").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) {
            return TxResult::NoAccount;
        }
        // rippled NFTokenCreateOffer::preclaim — the token must EXIST in the
        // relevant owner's pages, else tecNO_ENTRY. Owner is the offer's
        // seller: the tx account for a sell offer (tfSellNFToken), the sfOwner
        // field for a buy offer. (#105757083 1B7B0C8A: a stale buy offer for
        // a token no longer on its owner.)
        let Some(id) = tx.fields.get("NFTokenID").and_then(hash256_from) else {
            return TxResult::Malformed;
        };
        let is_sell = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0) & 0x0000_0001 != 0;
        let token_owner = if is_sell {
            tx.account
        } else {
            match tx.fields.get("Owner").and_then(decode_account_id) {
                Some(o) => o,
                None => return TxResult::Malformed,
            }
        };
        if nftpage::locate_token(sandbox, &token_owner, &id).is_none() {
            return TxResult::NoEntry;
        }
        // An account can opt out of receiving NFT offers, and rippled's shared
        // `tokenOfferCreatePreclaim` (NFTokenHelpers.cpp:824) honours that for
        // BOTH accounts an offer names: a `Destination` must exist (tecNO_DST)
        // and neither it nor the token's `Owner` may carry
        // `lsfDisallowIncomingNFTokenOffer` — either one is tecNO_PERMISSION.
        //
        // #105846674 5C367CC0 and #105875898 19B473AE/677DB175 are buy offers
        // for tokens whose owners set exactly that flag (rnrLUbYH 0x2d0a0000,
        // rD9Po2Jz 0x04000000). Mainnet claims the fee in one mutation; we
        // created the offer in five. The `Owner`-side check is the one those
        // three pin; the `Destination` side is the same two lines of rippled.
        //
        // rippled's `tecNO_TARGET` for a missing owner is unreachable here:
        // `findToken` above already fails such a transaction with tecNO_ENTRY,
        // exactly as it does in rippled's own ordering.
        if let Some(dest) = tx.fields.get("Destination").and_then(decode_account_id) {
            match disallows_incoming_nft_offer(sandbox, &dest) {
                None => return TxResult::NoDst,
                Some(true) => return TxResult::NoPermission,
                Some(false) => {}
            }
        }
        if let Some(owner) = tx.fields.get("Owner").and_then(decode_account_id) {
            if disallows_incoming_nft_offer(sandbox, &owner) == Some(true) {
                return TxResult::NoPermission;
            }
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let seq = if tx.uses_ticket() {
            tx.ticket_seq.unwrap_or(0)
        } else {
            tx.sequence
        };
        let offer_key = keylet::nft_offer_key(&tx.account, seq);
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        let is_sell = flags & 0x0000_0001 != 0;

        let mut offer_obj = serde_json::json!({
            "LedgerEntryType": "NFTokenOffer",
            "Owner": hex::encode(tx.account),
            "NFTokenID": tx.fields["NFTokenID"].clone(),
            "Amount": tx.fields["Amount"].clone(),
            "Flags": flags & 0xFFFF,
            "OwnerNode": 0,
        });
        if let Some(dest) = tx.fields.get("Destination") {
            offer_obj["Destination"] = dest.clone();
        }
        if let Some(exp) = tx.fields.get("Expiration") {
            offer_obj["Expiration"] = exp.clone();
        }

        sandbox.write(offer_key, serde_json::to_vec(&offer_obj).unwrap());
        increment_owner_count(&tx.account, sandbox);

        owner_dir_insert(sandbox, &tx.account, &offer_key);
        if let Some(nft_id) = tx.fields.get("NFTokenID").and_then(hash256_from) {
            let dir_root = if is_sell {
                keylet::nft_sell_offers_key(&nft_id)
            } else {
                keylet::nft_buy_offers_key(&nft_id)
            };
            dir_insert(sandbox, &dir_root, None, &offer_key);
        }
        // Mainnet meta also touches the Destination's AccountRoot (no-op
        // Modified) when the offer names one. OwnerCount bump = the
        // key-granularity touch convention (value fidelity deferred).
        if let Some(dest) = tx.fields.get("Destination").and_then(decode_account_id) {
            increment_owner_count(&dest, sandbox);
        }

        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenAcceptOffer
// ---------------------------------------------------------------------------

pub struct NFTokenAcceptOfferTransactor;

struct OfferSle {
    key: Hash256,
    owner: [u8; 20],
    nft_id: Hash256,
    is_sell: bool,
    amount: serde_json::Value,
    owner_node: Option<u64>,
    offer_node: Option<u64>,
    destination: Option<[u8; 20]>,
}

fn read_offer(sandbox: &Sandbox, key: Hash256) -> Option<OfferSle> {
    let data = sandbox.read(&key)?;
    let o: serde_json::Value = serde_json::from_slice(&data).ok()?;
    Some(OfferSle {
        key,
        owner: decode_account_id(&o["Owner"])?,
        nft_id: hash256_from(&o["NFTokenID"])?,
        is_sell: o["Flags"].as_u64().unwrap_or(0) & 1 != 0,
        amount: o["Amount"].clone(),
        owner_node: node_hint(o.get("OwnerNode")),
        offer_node: node_hint(o.get("NFTokenOfferNode")),
        destination: o.get("Destination").and_then(decode_account_id),
    })
}

/// Delete an NFT offer: object + owner-dir entry + token buy/sell-dir entry +
/// the owner's reserve unit.
fn delete_offer(sandbox: &mut Sandbox, offer: &OfferSle) {
    sandbox.delete(offer.key);
    owner_dir_remove(sandbox, &offer.owner, &offer.key, offer.owner_node, false);
    let dir_root = if offer.is_sell {
        keylet::nft_sell_offers_key(&offer.nft_id)
    } else {
        keylet::nft_buy_offers_key(&offer.nft_id)
    };
    dir_remove(sandbox, &dir_root, &offer.key, offer.offer_node, false);
    decrement_owner_count(&offer.owner, sandbox);
}

/// Move `drops` from buyer to seller, carving the NFTokenID-embedded transfer
/// fee (1/100000 units) out for the issuer when the seller isn't the issuer.
fn pay_xrp_with_transfer_fee(
    sandbox: &mut Sandbox,
    buyer: &[u8; 20],
    seller: &[u8; 20],
    nft_id: &Hash256,
    drops: u64,
) {
    let fee_units = u16::from_be_bytes([nft_id.0[2], nft_id.0[3]]) as u128;
    let issuer = nftpage::issuer_of(nft_id);
    let mut to_issuer = 0u128;
    if fee_units > 0 && issuer != *seller {
        to_issuer = (drops as u128) * fee_units / 100_000;
    }
    adjust_xrp(sandbox, buyer, -(drops as i128));
    adjust_xrp(sandbox, seller, drops as i128 - to_issuer as i128);
    if to_issuer > 0 {
        adjust_xrp(sandbox, &issuer, to_issuer as i128);
    }
}

/// Move the token from seller's pages to buyer's, carrying URI along, and
/// settle the reserve deltas from page churn.
fn transfer_token(
    sandbox: &mut Sandbox,
    seller: &[u8; 20],
    buyer: &[u8; 20],
    nft_id: &Hash256,
) -> TxResult {
    let Some(removal) = nftpage::page_remove(sandbox, seller, nft_id) else {
        return TxResult::NoEntry;
    };
    if removal.page_deleted {
        decrement_owner_count(seller, sandbox);
    }
    if nftpage::page_insert(sandbox, buyer, removal.entry) {
        increment_owner_count(buyer, sandbox);
    }
    TxResult::Success
}

impl Transactor for NFTokenAcceptOfferTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenAcceptOffer" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
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
        let sell_ref = tx.fields.get("NFTokenSellOffer").and_then(hash256_from);
        let buy_ref = tx.fields.get("NFTokenBuyOffer").and_then(hash256_from);

        // Brokered mode: both offers named; tx.account is the broker.
        if let (Some(sk), Some(bk)) = (sell_ref, buy_ref) {
            let (Some(sell), Some(buy)) = (read_offer(sandbox, sk), read_offer(sandbox, bk))
            else {
                // A named NFT offer that isn't there is tecOBJECT_NOT_FOUND,
                // not tecNO_ENTRY (NFTokenAcceptOffer::preclaim's checkOffer).
                return TxResult::ObjectNotFound;
            };
            let seller = sell.owner;
            let buyer = buy.owner;
            // Broker keeps the buy/sell spread; an explicit BrokerFee is the
            // broker's cut, the rest of the buy amount goes to the seller.
            if let Some(drops) = buy.amount.as_str().and_then(|s| s.parse::<u64>().ok()) {
                let broker_fee = tx
                    .fields
                    .get("NFTokenBrokerFee")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let to_seller = drops.saturating_sub(broker_fee);
                adjust_xrp(sandbox, &buyer, -(drops as i128));
                if broker_fee > 0 {
                    adjust_xrp(sandbox, &tx.account, broker_fee as i128);
                }
                pay_settle_seller(sandbox, &seller, &sell.nft_id, to_seller);
            }
            let moved = transfer_token(sandbox, &seller, &buyer, &sell.nft_id);
            if moved != TxResult::Success {
                return moved;
            }
            delete_offer(sandbox, &sell);
            delete_offer(sandbox, &buy);
            return TxResult::Success;
        }

        // Direct mode.
        let Some(offer) = sell_ref.or(buy_ref).and_then(|k| read_offer(sandbox, k)) else {
            // #105786567 63597143: the offer was already gone, and mainnet
            // reports tecOBJECT_NOT_FOUND for it.
            return TxResult::ObjectNotFound;
        };
        if offer.owner == tx.account {
            return TxResult::NoPermission;
        }
        if let Some(dest) = offer.destination {
            if dest != tx.account {
                return TxResult::NoPermission;
            }
        }
        let (seller, buyer) = if offer.is_sell {
            (offer.owner, tx.account)
        } else {
            (tx.account, offer.owner)
        };
        if let Some(drops) = offer.amount.as_str().and_then(|s| s.parse::<u64>().ok()) {
            if drops > 0 {
                pay_xrp_with_transfer_fee(sandbox, &buyer, &seller, &offer.nft_id, drops);
            }
        }
        // IOU-priced offers: value movement over trust lines is not modeled
        // yet — the token/offer mutations below still land on the right keys.
        let moved = transfer_token(sandbox, &seller, &buyer, &offer.nft_id);
        if moved != TxResult::Success {
            return moved;
        }
        delete_offer(sandbox, &offer);
        TxResult::Success
    }
}

/// Credit the seller with `drops`, carving the transfer fee for the issuer.
fn pay_settle_seller(sandbox: &mut Sandbox, seller: &[u8; 20], nft_id: &Hash256, drops: u64) {
    let fee_units = u16::from_be_bytes([nft_id.0[2], nft_id.0[3]]) as u128;
    let issuer = nftpage::issuer_of(nft_id);
    let mut to_issuer = 0u128;
    if fee_units > 0 && issuer != *seller {
        to_issuer = (drops as u128) * fee_units / 100_000;
    }
    adjust_xrp(sandbox, seller, drops as i128 - to_issuer as i128);
    if to_issuer > 0 {
        adjust_xrp(sandbox, &issuer, to_issuer as i128);
    }
}

// ---------------------------------------------------------------------------
// NFTokenCancelOffer
// ---------------------------------------------------------------------------

pub struct NFTokenCancelOfferTransactor;

impl Transactor for NFTokenCancelOfferTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenCancelOffer" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
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
        // NFTokenOffers entries are ledger indexes (Hash256), and the canceler
        // need not own them (owner, destination, or expiry all authorize) —
        // the reserve refund lands on each OFFER's owner.
        for offer_ref in &offers {
            let Some(key) = hash256_from(offer_ref) else { continue };
            let Some(offer) = read_offer(sandbox, key) else { continue };
            delete_offer(sandbox, &offer);
        }
        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// NFTokenModify (XLS-46, tt 61)
// ---------------------------------------------------------------------------

pub struct NFTokenModifyTransactor;

impl Transactor for NFTokenModifyTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "NFTokenModify" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
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
        let Some(id) = tx.fields.get("NFTokenID").and_then(hash256_from) else {
            return TxResult::Malformed;
        };
        let owner = tx
            .fields
            .get("Owner")
            .and_then(decode_account_id)
            .unwrap_or(tx.account);

        let Some((page_key, mut page)) = nftpage::locate_token(sandbox, &owner, &id) else {
            return TxResult::NoEntry;
        };
        let id_hex = hex::encode_upper(id.0);
        if let Some(arr) = page.get_mut("NFTokens").and_then(|v| v.as_array_mut()) {
            for e in arr.iter_mut() {
                if e["NFToken"]["NFTokenID"].as_str().unwrap_or("").eq_ignore_ascii_case(&id_hex) {
                    match tx.fields.get("URI") {
                        Some(uri) => e["NFToken"]["URI"] = uri.clone(),
                        None => {
                            e["NFToken"].as_object_mut().map(|o| o.remove("URI"));
                        }
                    }
                    break;
                }
            }
        }
        sandbox.write(page_key, serde_json::to_vec(&page).unwrap_or_default());
        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;

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
                "Sequence": 7,
                "OwnerCount": 0,
                "Flags": 0,
            });
            state.state_map.insert(
                keylet::account_root_key(id),
                serde_json::to_vec(&acct).unwrap(),
            );
        }
        state
    }

    fn mint_tx(account: [u8; 20], taxon: u32) -> TxFields {
        TxFields {
            account,
            tx_type: "NFTokenMint".into(),
            fee: 10,
            sequence: 7,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({ "NFTokenTaxon": taxon, "Flags": 8 }),
        }
    }

    fn page_tokens(sb: &Sandbox, owner: &[u8; 20]) -> Vec<String> {
        sb.read(&nftpage::max_page_key(owner))
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            .and_then(|p| {
                p["NFTokens"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|e| e["NFToken"]["NFTokenID"].as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// An account can opt out of receiving NFT offers with
    /// `lsfDisallowIncomingNFTokenOffer`, and rippled's shared
    /// `tokenOfferCreatePreclaim` (NFTokenHelpers.cpp:824) refuses a buy offer
    /// naming such an owner with tecNO_PERMISSION. #105846674 5C367CC0 and
    /// #105875898 19B473AE/677DB175 are exactly that: mainnet claims the fee
    /// in one mutation, we created the offer in five.
    #[test]
    fn a_buy_offer_is_refused_when_the_owner_disallows_incoming_nft_offers() {
        let owner = [0x01u8; 20];
        let buyer = [0x02u8; 20];
        let state = make_state(&[(owner, 500_000_000), (buyer, 500_000_000)]);
        let mut sb = Sandbox::new(&state);
        assert_eq!(NFTokenMintTransactor.do_apply(&mint_tx(owner, 0), &mut sb), TxResult::Success);
        let id = page_tokens(&sb, &owner).first().expect("token minted").clone();

        let tx = TxFields {
            account: buyer,
            tx_type: "NFTokenCreateOffer".into(),
            fee: 12,
            sequence: 3,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenID": id, "Amount": "1000000",
                "Owner": hex::encode(owner), "Flags": 0,
            }),
        };
        // Nothing else about the offer changes — only the owner's opt-out.
        assert_eq!(
            NFTokenCreateOfferTransactor.preclaim(&tx, &sb),
            TxResult::Success,
            "a buy offer against a willing owner stands",
        );

        let akey = keylet::account_root_key(&owner);
        let mut acct: serde_json::Value =
            serde_json::from_slice(&sb.read(&akey).unwrap()).unwrap();
        acct["Flags"] = serde_json::json!(0x0400_0000u64);
        sb.write(akey, serde_json::to_vec(&acct).unwrap());

        assert_eq!(
            NFTokenCreateOfferTransactor.preclaim(&tx, &sb),
            TxResult::NoPermission,
            "but not once that owner disallows incoming NFT offers",
        );
    }

    #[test]
    fn decode20_resolves_base58_issuer() {
        // The probe does NOT hex-normalise the Issuer field, so the engine sees
        // a raw base58 r-address. decode20 must resolve it (else it silently
        // falls back to the minter).
        let got = crate::tx::offer::decode20("rNR6vtb85KWJyTs86mcHB2UqNVwgnaBGRF");
        assert!(got.is_some(), "base58 Issuer must decode, got None");
    }

    #[test]
    fn mint_with_issuer_field_credits_issuer_not_minter() {
        // Authorised-minter mint (fixtures 105803446 / 105762114): Account is the
        // minter, Issuer is the account on whose behalf the token is minted. The
        // NFTokenID must embed the ISSUER, and MintedNFTokens must bump on the
        // ISSUER — while the token still lands in the MINTER's pages.
        let minter = [0x11u8; 20];
        let issuer = crate::tx::offer::decode20("rNR6vtb85KWJyTs86mcHB2UqNVwgnaBGRF")
            .expect("issuer base58 must decode");
        let state = make_state(&[(minter, 1_000_000_000), (issuer, 1_000_000_000)]);
        let mut sb = Sandbox::new(&state);
        let mut tx = mint_tx(minter, 5);
        tx.fields["Issuer"] = serde_json::json!("rNR6vtb85KWJyTs86mcHB2UqNVwgnaBGRF");
        assert_eq!(
            NFTokenMintTransactor.do_apply(&tx, &mut sb),
            TxResult::Success
        );
        // MintedNFTokens lands on the ISSUER.
        let iacct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&issuer)).unwrap()).unwrap();
        assert_eq!(iacct["MintedNFTokens"], 1, "issuer must be credited the mint");
        // The minter must NOT be credited a MintedNFTokens count.
        let macct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&minter)).unwrap()).unwrap();
        assert!(
            macct.get("MintedNFTokens").is_none(),
            "minter must not be credited MintedNFTokens"
        );
        // The token lands in the MINTER's pages, issued-by the ISSUER.
        let toks = page_tokens(&sb, &minter);
        assert_eq!(toks.len(), 1, "token lives in the minter's page");
        assert_eq!(
            &toks[0][8..48],
            hex::encode_upper(issuer).as_str(),
            "NFTokenID issuer bytes must be the Issuer field, not the minter"
        );
    }

    #[test]
    fn mint_places_token_on_max_page() {
        let minter = [0x11u8; 20];
        let state = make_state(&[(minter, 1_000_000_000)]);
        let mut sb = Sandbox::new(&state);
        let tr = NFTokenMintTransactor;
        assert_eq!(tr.do_apply(&mint_tx(minter, 5), &mut sb), TxResult::Success);
        let toks = page_tokens(&sb, &minter);
        assert_eq!(toks.len(), 1);
        // Issuer bytes 4..24 of the id are the minter.
        assert_eq!(&toks[0][8..48], hex::encode_upper(minter).as_str());
        let acct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&minter)).unwrap()).unwrap();
        assert_eq!(acct["MintedNFTokens"], 1);
        assert_eq!(acct["OwnerCount"], 1); // the new page
    }

    #[test]
    fn accept_sell_offer_moves_token_and_funds() {
        let seller = [0x21u8; 20];
        let buyer = [0x22u8; 20];
        let state = make_state(&[(seller, 1_000_000_000), (buyer, 1_000_000_000)]);
        let mut sb = Sandbox::new(&state);

        // Mint to the seller, then hand-build a sell offer SLE for 500 drops.
        NFTokenMintTransactor.do_apply(&mint_tx(seller, 1), &mut sb);
        let id_hex = page_tokens(&sb, &seller)[0].clone();
        let offer_key = keylet::nft_offer_key(&seller, 99);
        let offer = serde_json::json!({
            "LedgerEntryType": "NFTokenOffer",
            "Owner": hex::encode(seller),
            "NFTokenID": id_hex,
            "Amount": "500",
            "Flags": 1,
        });
        sb.write(offer_key, serde_json::to_vec(&offer).unwrap());

        let accept = TxFields {
            account: buyer,
            tx_type: "NFTokenAcceptOffer".into(),
            fee: 12,
            sequence: 7,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenSellOffer": hex::encode_upper(offer_key.0),
            }),
        };
        assert_eq!(
            NFTokenAcceptOfferTransactor.do_apply(&accept, &mut sb),
            TxResult::Success
        );
        assert!(page_tokens(&sb, &seller).is_empty());
        assert_eq!(page_tokens(&sb, &buyer).len(), 1);
        assert!(sb.read(&offer_key).is_none());
        let sacct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&seller)).unwrap()).unwrap();
        assert_eq!(sacct["Balance"], "1000000500");
    }

    #[test]
    fn cancel_offer_by_index_refunds_owner_reserve() {
        let owner = [0x31u8; 20];
        let canceler = [0x32u8; 20];
        let state = make_state(&[(owner, 1_000_000_000), (canceler, 1_000_000_000)]);
        let mut sb = Sandbox::new(&state);
        let offer_key = keylet::nft_offer_key(&owner, 5);
        let offer = serde_json::json!({
            "LedgerEntryType": "NFTokenOffer",
            "Owner": hex::encode(owner),
            "NFTokenID": hex::encode_upper([0xABu8; 32]),
            "Amount": "1",
            "Flags": 1,
        });
        sb.write(offer_key, serde_json::to_vec(&offer).unwrap());
        increment_owner_count(&owner, &mut sb);

        let cancel = TxFields {
            account: canceler,
            tx_type: "NFTokenCancelOffer".into(),
            fee: 30,
            sequence: 7,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "NFTokenOffers": [hex::encode_upper(offer_key.0)],
            }),
        };
        assert_eq!(
            NFTokenCancelOfferTransactor.do_apply(&cancel, &mut sb),
            TxResult::Success
        );
        assert!(sb.read(&offer_key).is_none());
        let oacct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&owner)).unwrap()).unwrap();
        assert_eq!(oacct["OwnerCount"], 0);
    }

    #[test]
    fn modify_updates_uri_in_place() {
        let owner = [0x41u8; 20];
        let state = make_state(&[(owner, 1_000_000_000)]);
        let mut sb = Sandbox::new(&state);
        NFTokenMintTransactor.do_apply(&mint_tx(owner, 2), &mut sb);
        let id_hex = page_tokens(&sb, &owner)[0].clone();

        let modify = TxFields {
            account: owner,
            tx_type: "NFTokenModify".into(),
            fee: 15,
            sequence: 8,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({ "NFTokenID": id_hex, "URI": "697066733A2F2F78" }),
        };
        assert_eq!(
            NFTokenModifyTransactor.do_apply(&modify, &mut sb),
            TxResult::Success
        );
        let page: serde_json::Value =
            serde_json::from_slice(&sb.read(&nftpage::max_page_key(&owner)).unwrap()).unwrap();
        assert_eq!(page["NFTokens"][0]["NFToken"]["URI"], "697066733A2F2F78");
    }
}
