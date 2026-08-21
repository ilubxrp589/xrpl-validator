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
/// tfSellNFToken — the only flag a mint-created offer may carry.
const TF_SELL_NFTOKEN: u64 = 0x0000_0001;

/// Current OwnerCount on an account root, 0 when absent.
fn owner_count_of(sandbox: &Sandbox, account: &[u8; 20]) -> u64 {
    sandbox
        .read(&keylet::account_root_key(account))
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
        .and_then(|a| a["OwnerCount"].as_u64())
        .unwrap_or(0)
}

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

/// Create an NFTokenOffer and thread it into both directories.
///
/// rippled factors this out as `nft::tokenOfferCreateApply` and calls it from
/// BOTH NFTokenCreateOffer and NFTokenMint (NFTokenMint.cpp:312-330). A mint
/// that carries `Amount` mints the token AND rests a sell offer in one
/// transaction — the featureNFTokenMintOffer path — and rippled is explicit
/// that the offer is always a SELL: "we pass tfSellNFToken as the transaction
/// flags because a Mint is only allowed to create a sell offer."
///
/// `flags` is the value stored on the offer object; callers pass the
/// transaction's flags for CreateOffer and tfSellNFToken for Mint.
#[allow(clippy::too_many_arguments)]
fn token_offer_create_apply(
    sandbox: &mut Sandbox,
    account: &[u8; 20],
    seq: u32,
    nftoken_id: &serde_json::Value,
    amount: &serde_json::Value,
    destination: Option<&serde_json::Value>,
    expiration: Option<&serde_json::Value>,
    flags: u64,
) -> Hash256 {
    let offer_key = keylet::nft_offer_key(account, seq);
    let is_sell = flags & 0x0000_0001 != 0;

    let mut offer_obj = serde_json::json!({
        "LedgerEntryType": "NFTokenOffer",
        "Owner": hex::encode(account),
        "NFTokenID": nftoken_id.clone(),
        "Amount": amount.clone(),
        "Flags": flags & 0xFFFF,
    });
    if let Some(dest) = destination {
        offer_obj["Destination"] = dest.clone();
    }
    if let Some(exp) = expiration {
        offer_obj["Expiration"] = exp.clone();
    }

    increment_owner_count(account, sandbox);

    let owner_node = owner_dir_insert(sandbox, account, &offer_key);
    offer_obj["OwnerNode"] = serde_json::Value::String(format!("{owner_node:x}"));
    if let Some(nft_id) = hash256_from(nftoken_id) {
        let dir_root = if is_sell {
            keylet::nft_sell_offers_key(&nft_id)
        } else {
            keylet::nft_buy_offers_key(&nft_id)
        };
        // sfNFTokenOfferNode is SoeRequired on the offer — present even at
        // zero (byte census: net carries 3C 0000000000000000 we omitted).
        let tok_node = dir_insert(sandbox, &dir_root, None, &offer_key);
        offer_obj["NFTokenOfferNode"] = serde_json::Value::String(format!("{tok_node:x}"));
    }
    sandbox.write(offer_key, serde_json::to_vec(&offer_obj).unwrap());
    // The Destination-root touch the meta shows is a threadOwners hit (the
    // created offer names sfDestination) — the old fake OwnerCount bump here
    // CORRUPTED the destination's count and is gone.
    offer_key
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
        // MINTING FOR SOMEONE ELSE NEEDS THEIR AUTHORISATION.
        //     if (auto issuer = ctx.tx[~sfIssuer]) {
        //         auto const sle = ctx.view.read(keylet::account(*issuer));
        //         if (!sle) return tecNO_ISSUER;
        //         if (auto const minter = (*sle)[~sfNFTokenMinter]; minter != ctx.tx[sfAccount])
        //             return tecNO_PERMISSION;
        //     }
        //     (NFTokenMint.cpp preclaim)
        // We checked only the SUBMITTER's account here and left the issuer to
        // do_apply, which returned NoAccount — a **tef** — when it could not
        // read the issuer's root. That is the wrong CLASS as well as the wrong
        // code: tef claims no fee, so we produced ZERO mutations against
        // mainnet's one.
        //
        // 26 specimens in the historical sweep share this exact shape, e.g.
        // #106278193: rBFaAJtb mints with Issuer rfaafF5H, and rfaafF5H's
        // `NFTokenMinter` is not rBFaAJtb — mainnet claims the fee with
        // tecNO_PERMISSION.
        //
        // ⚠ Load-bearing hydration, as with NFTokenModify (`39df833`): the
        // issuer's AccountRoot is not the submitter's, so nothing else fetches
        // it. Absent it we must NOT condemn — an unreadable issuer yields
        // tecNO_ISSUER, exactly as rippled does, rather than a silent pass.
        if let Some(issuer) = tx.fields.get("Issuer").and_then(decode_account_id) {
            if issuer != tx.account {
                let Some(sle) = sandbox
                    .read(&keylet::account_root_key(&issuer))
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                else {
                    return TxResult::NoIssuer;
                };
                if sle.get("NFTokenMinter").and_then(decode_account_id) != Some(tx.account) {
                    return TxResult::NoPermission;
                }
            }
        }
        // A MINT CARRYING `Amount` ALSO CREATES A SELL OFFER, and inherits that
        // transaction's validation: `NFTokenMint::preclaim` hands off to
        //     nft::tokenOfferCreatePreclaim(...)
        // which refuses a destination that has switched incoming NFT offers off:
        //     if (sleDst->isFlag(lsfDisallowIncomingNFTokenOffer))
        //         return tecNO_PERMISSION;      (NFTokenHelpers.cpp:885-892)
        //
        // This is the rule behind ALL 26 mint specimens in the sweep, e.g.
        // #106278193: destination r4Wyoz7t carries Flags 0x04000000. The
        // authorised-minter check above passes legitimately there — rfaafF5H's
        // `NFTokenMinter` really is the submitter — so the refusal comes
        // entirely from the offer half of the transaction.
        //
        // ⚠ A transaction that is TWO operations inherits BOTH sets of rules.
        // Reading NFTokenMint's own preclaim in isolation shows nothing.
        if tx.fields.get("Amount").is_some() {
            if let Some(dest) = tx.fields.get("Destination").and_then(decode_account_id) {
                if let Some(d) = sandbox
                    .read(&keylet::account_root_key(&dest))
                    .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
                {
                    if d["Flags"].as_u64().unwrap_or(0) & 0x0400_0000 != 0 {
                        return TxResult::NoPermission; // lsfDisallowIncomingNFTokenOffer
                    }
                }
            }
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
        // rippled snapshots ownerCountBefore at doApply entry
        // (NFTokenMint.cpp:281-282), BEFORE insertToken — so the tail reserve
        // gate covers the new NFTokenPage as well as the optional sell offer.
        // Snapshotting after the page insert (as this used to) makes a
        // page-creating mint with no Amount invisible to the gate:
        // #106058568 CD70A6A6 and #106100514 6250D5ED, the same
        // reserve-starved bot family as the AcceptOffer cluster.
        let owner_count_before = owner_count_of(sandbox, &tx.account);
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

        // featureNFTokenMintOffer: a mint carrying `Amount` also rests a sell
        // offer for the token it just created (NFTokenMint.cpp:312-330), via
        // the same code NFTokenCreateOffer uses. Always a SELL — rippled passes
        // tfSellNFToken because "a Mint is only allowed to create a sell offer"
        // — so the transaction's own flags (which carry mint flags like
        // tfTransferable) must NOT be forwarded onto the offer.
        //
        // We minted the token but never created the offer, losing exactly three
        // mutations every time: the NFTokenOffer, its new sell-offer directory
        // page, and the owner directory it threads into. 11 cases in the fresh
        // batch, all identically our_muts=5 vs net_muts=8 with nothing extra —
        // e.g. #105815415 D44804DCF372, which carries Amount 0, a Destination
        // and an Expiration.
        if let Some(amount) = tx.fields.get("Amount") {
            let seq = if tx.uses_ticket() {
                tx.ticket_seq.unwrap_or(0)
            } else {
                tx.sequence
            };
            token_offer_create_apply(
                sandbox,
                &tx.account,
                seq,
                &serde_json::json!(hex::encode_upper(id)),
                amount,
                tx.fields.get("Destination"),
                tx.fields.get("Expiration"),
                TF_SELL_NFTOKEN,
            );
        }

        // rippled re-checks the reserve only when the owner count actually
        // moved, "so NFTs can be added to the page without requiring the
        // reserve each time", and compares accountReserve(ownerCountAfter) —
        // NOT +1 — against preFeeBalance_. do_apply runs post-fee, so the fee
        // is added back for the same reason as OfferCreate's reserve gate.
        let owner_count_after = owner_count_of(sandbox, &tx.account);
        if owner_count_after > owner_count_before {
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(d) = sandbox.read(&acct_key) {
                if let Ok(a) = serde_json::from_slice::<serde_json::Value>(&d) {
                    let bal: u64 = a["Balance"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0);
                    if bal + tx.fee < crate::ledger::fees::account_reserve(sandbox, owner_count_after) {
                        return TxResult::InsufficientReserve;
                    }
                }
            }
        }

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
        // Each page that disappears — emptied or consolidated away — releases
        // one reserve (rippled adjustOwnerCount by the merge count).
        for _ in 0..(u32::from(removal.page_deleted) + removal.pages_merged) {
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

        // A burn also takes the token's OUTSTANDING OFFERS with it — otherwise
        // they are left pointing at an NFT that no longer exists. `NFTokenBurn::
        // doApply` deletes up to 500 in total, SELL directory first:
        //
        //     std::size_t const deletedSellOffers = nft::removeTokenOffersWithLimit(
        //         view(), keylet::nftSells(id), kMaxDeletableTokenOfferEntries);
        //     if (kMaxDeletableTokenOfferEntries > deletedSellOffers)
        //         nft::removeTokenOffersWithLimit(view(), keylet::nftBuys(id),
        //             kMaxDeletableTokenOfferEntries - deletedSellOffers);
        //
        // rippled's own comment gives the reason for the ordering: sell offers
        // are the smaller set, so clearing them first is what empties the sell
        // directory within the budget.
        //
        // #106322254 674FD021 and #106323904 AB1D285C, the same shape in two
        // ledgers: each leaves one sell offer alive (50000000 drops, flags=1),
        // its now-empty sell directory undeleted, and the owner directory
        // unmodified — 3 mutations against mainnet's 6.
        burn_token_offers(sandbox, &id);

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
        let owner_count_before = owner_count_of(sandbox, &tx.account);
        token_offer_create_apply(
            sandbox,
            &tx.account,
            seq,
            &tx.fields["NFTokenID"],
            &tx.fields["Amount"],
            tx.fields.get("Destination"),
            tx.fields.get("Expiration"),
            tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0),
        );

        // An NFTokenOffer is an owned object, so it costs a reserve unit — and
        // this transactor never charged for it. Same gate NFTokenMint uses
        // above: compare accountReserve(ownerCountAfter) against the PRE-fee
        // balance, do_apply running post-fee.
        //
        // #106154459 A55AD53EEFA2: `rN7JYEJHfj3Rm4ktX5CTGTPwPEs2G8aD9y` sits at
        // OwnerCount 1418 with 284637037 drops. One more object needs
        // 1000000 + 1419*200000 = 284800000 — it is 162963 short, so mainnet
        // takes the fee and stops (tecINSUFFICIENT_RESERVE, ONE node: the
        // AccountRoot). We created the offer and both directory pages and
        // returned tesSUCCESS: 5 mutations against 1, all EXTRA.
        //
        // ⚠ Placed here rather than in preclaim to match the calibrated
        // NFTokenMint form. If a specimen ever turns up that fails BOTH this
        // and a preclaim-stage check, the ORDER will decide the code — that is
        // the same trap 5626357/b569fa9 walked into for destination checks.
        let owner_count_after = owner_count_of(sandbox, &tx.account);
        if owner_count_after > owner_count_before {
            let acct_key = keylet::account_root_key(&tx.account);
            if let Some(d) = sandbox.read(&acct_key) {
                if let Ok(a) = serde_json::from_slice::<serde_json::Value>(&d) {
                    let bal: u64 = a["Balance"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0);
                    if bal + tx.fee < crate::ledger::fees::account_reserve(sandbox, owner_count_after) {
                        return TxResult::InsufficientReserve;
                    }
                }
            }
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

/// `accountFunds(view, payer, needed, ...) < needed` for an XRP-priced NFT
/// offer. For XRP that resolves to `accountHolds`, i.e. the balance less the
/// account's reserve at its current OwnerCount.
///
/// ⚠ IOU-priced offers return FALSE — not "funded", but "not judged here".
/// `do_apply` still does not move value over trust lines for them (see its own
/// note), so inventing a funds verdict would swap one wrong result code for
/// another. When IOU settlement lands, this is the first thing to revisit.
fn nft_funds_short(sandbox: &Sandbox, payer: &[u8; 20], amount: &serde_json::Value) -> bool {
    let Some(needed) = amount.as_str().and_then(|s| s.parse::<u64>().ok()) else {
        return false;
    };
    let Some(d) = sandbox.read(&keylet::account_root_key(payer)) else {
        return false;
    };
    let Ok(a) = serde_json::from_slice::<serde_json::Value>(&d) else {
        return false;
    };
    let bal: u64 = a["Balance"].as_str().and_then(|v| v.parse().ok()).unwrap_or(0);
    let reserve = crate::ledger::fees::account_reserve(sandbox, owner_count_of(sandbox, payer));
    bal.saturating_sub(reserve) < needed
}

/// Delete the burned token's outstanding offers — SELL directory first, then
/// BUY with what is left of the 500 budget (`NFTokenBurn::doApply`).
///
/// Keys are collected per directory BEFORE deleting any of them: `delete_offer`
/// relinks and can drop the very pages the walk is standing on.
fn burn_token_offers(sandbox: &mut Sandbox, id: &Hash256) {
    const MAX_DELETABLE: usize = 500;
    let mut budget = MAX_DELETABLE;
    for root in [keylet::nft_sell_offers_key(id), keylet::nft_buy_offers_key(id)] {
        if budget == 0 {
            return;
        }
        let mut keys: Vec<Hash256> = Vec::new();
        let mut page_key = root;
        for _ in 0..1000 {
            let Some(page) = sandbox
                .read(&page_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else { break };
            for e in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(k) = e
                    .as_str()
                    .and_then(|s| hex::decode(s).ok())
                    .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                {
                    keys.push(Hash256(k));
                }
            }
            let next = page.get("IndexNext").map(crate::tx::offer::dirnum).unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key = keylet::dir_page_key(&root, next);
        }
        for k in keys {
            if budget == 0 {
                return;
            }
            if let Some(off) = read_offer(sandbox, k) {
                delete_offer(sandbox, &off);
                budget -= 1;
            }
        }
    }
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
    for _ in 0..(u32::from(removal.page_deleted) + removal.pages_merged) {
        decrement_owner_count(seller, sandbox);
    }
    if nftpage::page_insert(sandbox, buyer, removal.entry) {
        increment_owner_count(buyer, sandbox);
        // fixNFTokenReserve (NFTokenAcceptOffer.cpp:378-399, transferNFToken):
        // when the insert had to CREATE a page, the buyer must hold the
        // reserve for it, judged on the balance AS IT STANDS — post-fee,
        // post-price — against accountReserve(ownerCountAfter). Deliberately
        // NOT the pre-fee form the Mint/CreateOffer gates use: rippled's own
        // comment rules out `preFeeBalance_` here "because NFT is sold for a
        // price", accepting that "the reserve requirement is a few drops
        // higher".
        //
        // #106055317 9982EA5D calibrates it: buyer rfwYSmo7 holds 1798921
        // drops at OwnerCount 3 and accepts a FREE sell offer (Amount "0",
        // Destination = buyer). The new page lifts the count to 4, reserve
        // 1000000 + 4*200000 = 1800000, balance post-fee 1798911 — 1089
        // drops short — so mainnet takes the fee and the token stays with
        // the seller. We moved it: 6 mutations against 1. Eight specimens,
        // one bot family.
        //
        // An unreadable buyer AccountRoot must NOT condemn (the inverted
        // hydration trap): only a parsed Balance is judged. rippled reads
        // the same SLE it just inserted into, so absence there is
        // tecINTERNAL, unreachable with sound hydration.
        let count_after = owner_count_of(sandbox, buyer);
        let bal = sandbox
            .read(&keylet::account_root_key(buyer))
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            .and_then(|a| a["Balance"].as_str().and_then(|v| v.parse::<u64>().ok()));
        if let Some(bal) = bal {
            if bal < crate::ledger::fees::account_reserve(sandbox, count_after) {
                return TxResult::InsufficientReserve;
            }
        }
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
        // rippled's `checkOffer` (NFTokenAcceptOffer.cpp): a named offer that is
        // the zero hash, or that is not in the ledger, is tecOBJECT_NOT_FOUND —
        // and that verdict is reached BEFORE any of the checks below.
        let read = |field: &str| -> Result<Option<OfferSle>, TxResult> {
            let Some(v) = tx.fields.get(field) else { return Ok(None) };
            match hash256_from(v) {
                Some(k) if k.0 != [0u8; 32] => match read_offer(sandbox, k) {
                    Some(o) => Ok(Some(o)),
                    None => Err(TxResult::ObjectNotFound),
                },
                _ => Err(TxResult::ObjectNotFound),
            }
        };
        let bo = match read("NFTokenBuyOffer") {
            Ok(o) => o,
            Err(e) => return e,
        };
        let so = match read("NFTokenSellOffer") {
            Ok(o) => o,
            Err(e) => return e,
        };
        // "The account offering to buy must have funds":
        //   accountFunds(view, (*bo)[sfOwner], needed, ...) < needed
        //     -> tecINSUFFICIENT_FUNDS
        // The payer is the BUY OFFER'S OWNER, not the submitter — in brokered
        // mode the broker never funds the purchase, which is why checking the
        // submitter there "doesn't make sense, causes an unnecessary tec", in
        // rippled's own words. With only a sell offer the payer IS the
        // submitter, and rippled skips that check entirely when a buy offer is
        // also present (`if (!bo)`).
        //
        // #106295345 A197E2D3 is the specimen: a brokered accept where the buy
        // offer's owner rHoGeyNk holds 6087454 drops against an OwnerCount of 8
        // — a 2600000 reserve — so 3487454 spendable against a 5069291 offer.
        // Mainnet claims the fee and stops; we had no funds check at all.
        // IN BROKERED MODE, EITHER OFFER'S `Destination` MUST BE THE BROKER.
        // rippled tests BOTH, in preclaim, before the broker-fee arithmetic:
        //     if (auto const dest = bo->at(~sfDestination); dest && *dest != ctx.tx[sfAccount])
        //         return tecNO_PERMISSION;              // and the same for so
        // (NFTokenAcceptOffer.cpp:115-126). A Destination names who may ACCEPT
        // the offer, and in brokered mode that is the broker submitting the
        // transaction — not the counterparty.
        //
        // We read only ONE offer to check this — `sell_ref.or(buy_ref)` in
        // do_apply — so with both offers named we tested the SELL side and
        // never looked at the BUY side at all.
        //
        // #106368924 53DA72C2: the sell offer's Destination IS the broker
        // rpZqTPC8 (fine), while the buy offer names rDeizxSRo6JH — someone
        // else entirely. Mainnet claims the fee with tecNO_PERMISSION; we
        // brokered the sale, moved the token and paid out 5074492 drops plus a
        // 508-drop broker fee. 7 mutations against 1.
        if let (Some(b), Some(sl)) = (&bo, &so) {
            for d in [b.destination, sl.destination].into_iter().flatten() {
                if d != tx.account {
                    return TxResult::NoPermission;
                }
            }
        }
        if let Some(b) = &bo {
            if nft_funds_short(sandbox, &b.owner, &b.amount) {
                return TxResult::InsufficientFunds;
            }
        }
        if let Some(s) = &so {
            if bo.is_none() && nft_funds_short(sandbox, &tx.account, &s.amount) {
                return TxResult::InsufficientFunds;
            }
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
            // "The seller must own the token" (NFTokenAcceptOffer.cpp:229):
            // a stale offer whose owner no longer holds the NFToken is
            // tecNO_PERMISSION — not the tecNO_ENTRY our transfer_token
            // surfaced later.
            if nftpage::locate_token(sandbox, &seller, &sell.nft_id).is_none() {
                return TxResult::NoPermission;
            }
            // rippled deletes BOTH offers first (doApply, before any payout
            // or the token move): by the time transferNFToken judges the
            // buyer's reserve, the buyer's own buy offer no longer counts
            // against them. Deleting after the check would judge
            // reserve(count + 1) — one unit (200000 drops) too strict. The
            // final state is order-independent; only the check's inputs care.
            delete_offer(sandbox, &sell);
            delete_offer(sandbox, &buy);
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
            } else {
                // IOU-priced brokered sale: same shape over trust lines.
                let bf = tx.fields.get("NFTokenBrokerFee");
                pay_iou_with_transfer_fee(
                    sandbox, &buyer, &seller, &sell.nft_id, &buy.amount,
                    bf.map(|f| (&tx.account, f)),
                );
            }
            return transfer_token(sandbox, &seller, &buyer, &sell.nft_id);
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
        // Ownership precondition, rippled's order (NFTokenAcceptOffer.cpp:170,
        // :229): accepting a SELL offer requires the OFFER OWNER to still
        // hold the token; accepting a BUY offer requires the ACCEPTOR to.
        // Either miss is tecNO_PERMISSION — #106333939 2FEB03EC accepts a
        // stale sell offer (rpApJk4e no longer holds 00081B58…0100) and
        // mainnet claims the fee with NO_PERMISSION where we said NO_ENTRY.
        if nftpage::locate_token(sandbox, &seller, &offer.nft_id).is_none() {
            return TxResult::NoPermission;
        }
        // Offer deleted FIRST — same order as rippled's doApply (see the
        // brokered arm's note): a buy-offer accept must not count the
        // buyer's own offer toward the reserve judged in transfer_token.
        delete_offer(sandbox, &offer);
        if let Some(drops) = offer.amount.as_str().and_then(|s| s.parse::<u64>().ok()) {
            if drops > 0 {
                pay_xrp_with_transfer_fee(sandbox, &buyer, &seller, &offer.nft_id, drops);
            }
        } else {
            pay_iou_with_transfer_fee(sandbox, &buyer, &seller, &offer.nft_id, &offer.amount, None);
        }
        transfer_token(sandbox, &seller, &buyer, &offer.nft_id)
    }
}

/// IOU-priced settlement: buyer's line down by the full amount, the NFT
/// issuer's line up by the transfer-fee cut (fee_units/100000, floor),
/// the seller's line up by the rest — all against the CURRENCY issuer,
/// exactly the three RippleStates rippled's accountSend chain writes.
/// #106047462 990F1EBD: 950000 HADA at fee 2500 → 23750 to the NFT
/// issuer, 926250 to the seller; we had left all three lines untouched
/// ("value movement over trust lines is not modeled yet", 7v10).
/// `line_adjust` no-ops when a party IS the currency issuer, matching
/// redeem/issue semantics.
fn pay_iou_with_transfer_fee(
    sandbox: &mut Sandbox,
    buyer: &[u8; 20],
    seller: &[u8; 20],
    nft_id: &Hash256,
    amount_json: &serde_json::Value,
    broker: Option<(&[u8; 20], &serde_json::Value)>,
) -> bool {
    use crate::tx::offer as ox;
    let (Some(leg), Some(value)) = (
        ox::leg_of(amount_json),
        crate::ledger::keylet::amount_mant_exp(amount_json),
    ) else {
        return false;
    };
    if leg.xrp || value.0 == 0 {
        return false;
    }
    let broker_cut = broker
        .and_then(|(_, fee)| crate::ledger::keylet::amount_mant_exp(fee))
        .unwrap_or((0, 0));
    // Seller's side is what remains after the broker's cut; the NFT
    // transfer fee is carved from THAT (rippled brokered mode pays the
    // broker first and `pay()`s the remainder, which carves the cut).
    let seller_side = ox::me_sub(value, broker_cut);
    let fee_units = u16::from_be_bytes([nft_id.0[2], nft_id.0[3]]) as u128;
    let nft_issuer = nftpage::issuer_of(nft_id);
    let nft_cut = if fee_units > 0 && nft_issuer != *seller {
        ox::me_muldiv(seller_side, (fee_units, 0), (100_000, 0), false)
    } else {
        (0, 0)
    };
    ox::line_adjust(sandbox, buyer, &leg, value, false);
    if let Some((br, _)) = broker {
        if broker_cut.0 > 0 {
            ox::line_adjust(sandbox, br, &leg, broker_cut, true);
        }
    }
    if nft_cut.0 > 0 {
        ox::line_adjust(sandbox, &nft_issuer, &leg, nft_cut, true);
    }
    ox::line_adjust(sandbox, seller, &leg, ox::me_sub(seller_side, nft_cut), true);
    true
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
        // ONE UNCANCELLABLE OFFER FAILS THE WHOLE TRANSACTION.
        // `NFTokenCancelOffer::preclaim` is a `find_if` over sfNFTokenOffers
        // returning tecNO_PERMISSION on the FIRST id the submitter may not
        // cancel. An id is cancellable when: it is absent (assumed already
        // consumed), or it has EXPIRED (anyone may cancel), or the submitter is
        // the offer's Owner, or the submitter is its Destination. Anything else
        // — including an id that resolves to a non-NFTokenOffer — is refused.
        //
        // The comment in do_apply named the three authorisations ("owner,
        // destination, or expiry all authorize") and nothing ever checked that
        // ONE OF THEM HOLDS; we deleted whatever we could read.
        //
        // #106029108 F48C0E1D: rpZqTPC8 cancels AF7F9FBF, owned by rP4pHjuJ and
        // destined for rpx9JThQ — neither is the submitter — with Expiration
        // 839103987 against a parent close of 839022091, so it is NOT expired.
        // Mainnet claims the fee with tecNO_PERMISSION; we returned tesSUCCESS.
        // Identical mutation sets, 1 v 1 — only the result code was wrong.
        let close = sandbox.base().header.close_time as u64;
        if let Some(offers) = tx.fields.get("NFTokenOffers").and_then(|v| v.as_array()) {
            for oref in offers {
                let Some(key) = hash256_from(oref) else { continue };
                // ⚠ An id we cannot READ is skipped, exactly as rippled skips a
                // missing one — but that makes hydration load-bearing: without
                // the offer object this check passes everything. The probe
                // fetches each NFTokenOffers id for precisely this reason.
                let Some(o) = sandbox
                    .read(&key)
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                else {
                    continue;
                };
                if o.get("LedgerEntryType").and_then(|v| v.as_str()) != Some("NFTokenOffer") {
                    return TxResult::NoPermission;
                }
                let expired = o
                    .get("Expiration")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|e| e != 0 && close >= e);
                if expired
                    || o.get("Owner").and_then(decode_account_id) == Some(tx.account)
                    || o.get("Destination").and_then(decode_account_id) == Some(tx.account)
                {
                    continue;
                }
                return TxResult::NoPermission;
            }
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
        // WHO MAY MODIFY AN NFT. `NFTokenModify::preclaim`, after the
        // existence test (NFTokenModify.cpp):
        //   if ((getFlags(id) & kFlagMutable) == 0)          -> tecNO_PERMISSION
        //   if (issuer != account) { minter = issuer's sfNFTokenMinter;
        //                            if (minter != account) -> tecNO_PERMISSION }
        // Both the mutability AND the authorisation live in the NFTokenID
        // itself — flags in bytes 0..2, ISSUER in bytes 4..24 — so neither is a
        // transaction field, and neither was checked here at all.
        //
        // #106374615 / #106377813 / #106374147 are one bot retrying the same
        // modify. NFTokenID 00182710CE07D0D9…: flags 0x0018, so lsfMutable IS
        // set and that gate passes — but the issuer is
        // rK8PZ2r6dSYRJUv5686wc2aesXe2zaLRXZ while the submitter is
        // rLGHuf125sJV9d6g2hcK2HzKrDH2j45dPQ, and the issuer's `NFTokenMinter`
        // is rKqqb5QZXVAL3VqXJL6obfRGeHou1DtyBV — a THIRD account. Mainnet
        // claims the fee with tecNO_PERMISSION; we rewrote the URI (2 muts v 1).
        let id_hex = tx.fields.get("NFTokenID").and_then(|v| v.as_str()).unwrap_or("");
        if id_hex.len() == 64 {
            if let Ok(flags) = u16::from_str_radix(&id_hex[0..4], 16) {
                if flags & 0x0010 == 0 {
                    return TxResult::NoPermission; // not lsfMutable
                }
            }
            if let Some(issuer) = decode_account_id(&serde_json::json!(&id_hex[8..48])) {
                if issuer != tx.account {
                    // ⚠ Only condemn an issuer we can actually READ. An
                    // unhydrated AccountRoot would make this fire on EVERY
                    // cross-issuer modify — the inverse of the usual trap, where
                    // a missing object makes a check vacuous. The probe hydrates
                    // it from the NFTokenID for exactly this reason.
                    if let Some(sle) = sandbox
                        .read(&keylet::account_root_key(&issuer))
                        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                    {
                        let minter = sle.get("NFTokenMinter").and_then(decode_account_id);
                        if minter != Some(tx.account) {
                            return TxResult::NoPermission;
                        }
                    }
                }
            }
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

    /// A mint carrying `Amount` mints the token AND rests a sell offer, via the
    /// same code NFTokenCreateOffer uses (NFTokenMint.cpp:312-330 —
    /// featureNFTokenMintOffer). The offer is ALWAYS a sell: rippled passes
    /// tfSellNFToken because "a Mint is only allowed to create a sell offer",
    /// so the transaction's own flags must not leak onto it.
    ///
    /// We minted but never created the offer, losing exactly three mutations —
    /// the NFTokenOffer, its sell-offer directory page, and the owner
    /// directory. 11 cases in the fresh batch, every one our_muts=5 vs
    /// net_muts=8 with nothing extra; #105815415 D44804DCF372 is the specimen,
    /// carrying Amount 0, a Destination and an Expiration.
    #[test]
    fn a_mint_carrying_an_amount_also_rests_a_sell_offer() {
        let minter = [0x01u8; 20];
        let dest = [0x03u8; 20];
        let mut state = make_state(&[(minter, 100_000_000), (dest, 100_000_000)]);
        let mut sb = Sandbox::new(&mut state);

        let mut tx = mint_tx(minter, 1);
        tx.fields["Amount"] = serde_json::json!("0");
        tx.fields["Destination"] = serde_json::json!(hex::encode(dest));
        tx.fields["Expiration"] = serde_json::json!(839_410_071u64);

        assert_eq!(NFTokenMintTransactor.do_apply(&tx, &mut sb), TxResult::Success);

        let offer_key = keylet::nft_offer_key(&minter, 7);
        let raw = sb.read(&offer_key).expect("the mint must rest an offer");
        let offer: serde_json::Value = serde_json::from_slice(&raw).unwrap();

        assert_eq!(offer["LedgerEntryType"], "NFTokenOffer");
        assert_eq!(offer["Owner"], hex::encode(minter));
        assert_eq!(offer["Amount"], "0");
        assert_eq!(offer["Destination"], hex::encode(dest));
        assert_eq!(offer["Expiration"], 839_410_071u64);
        assert_eq!(
            offer["Flags"], TF_SELL_NFTOKEN,
            "always a sell offer — the mint's own flags (here tfTransferable=8) must not leak",
        );

        // The token it names must be the one just minted, and the offer must be
        // threaded into the SELL directory for that token, not the buy side.
        let minted = page_tokens(&sb, &minter);
        assert_eq!(minted.len(), 1);
        assert_eq!(offer["NFTokenID"], minted[0]);
        let nft_id = hash256_from(&offer["NFTokenID"]).unwrap();
        assert!(
            sb.exists(&keylet::nft_sell_offers_key(&nft_id)),
            "sell-offer directory page is one of the three missing mutations",
        );
        assert!(!sb.exists(&keylet::nft_buy_offers_key(&nft_id)));
    }

    /// The control: a plain mint with no `Amount` rests nothing. Without this
    /// the test above would pass on an implementation that created an offer
    /// unconditionally, which would break every ordinary mint.
    #[test]
    fn a_mint_without_an_amount_rests_no_offer() {
        let minter = [0x01u8; 20];
        let mut state = make_state(&[(minter, 100_000_000)]);
        let mut sb = Sandbox::new(&mut state);

        assert_eq!(
            NFTokenMintTransactor.do_apply(&mint_tx(minter, 1), &mut sb),
            TxResult::Success,
        );
        assert!(!sb.exists(&keylet::nft_offer_key(&minter, 7)));
    }
}
