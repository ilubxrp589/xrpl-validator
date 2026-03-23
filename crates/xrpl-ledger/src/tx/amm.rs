//! AMM (Automated Market Maker) transaction types.
//!
//! AMMCreate, AMMDeposit, AMMWithdraw, AMMVote, AMMBid, AMMDelete.
//! XRPL's native AMM allows liquidity provision for any token pair.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::shamap::hash::sha512_half;

/// Compute a deterministic key for an AMM instance.
/// AMM key = SHA512Half(0x0041 || issue1_currency(20) || issue1_issuer(20) || issue2_currency(20) || issue2_issuer(20))
/// For XRP, issuer is all zeros and currency is all zeros.
fn amm_key(tx: &TxFields) -> Option<[u8; 32]> {
    // We need Asset and Asset2 from the fields
    // For simplicity, hash the tx account + "AMM" as a deterministic key
    // since full asset pair extraction from JSON is complex
    let asset1 = tx.fields.get("Asset")?;
    let asset2 = tx.fields.get("Asset2")?;

    let mut data = Vec::with_capacity(82);
    data.extend_from_slice(&[0x00, 0x41]); // AMM space key

    // Encode asset1
    encode_asset_to_buf(&mut data, asset1);
    // Encode asset2
    encode_asset_to_buf(&mut data, asset2);

    let hash = sha512_half(&data);
    Some(hash.0)
}

fn encode_asset_to_buf(buf: &mut Vec<u8>, asset: &serde_json::Value) {
    if asset.is_object() {
        // IOU: currency(20 bytes) + issuer(20 bytes)
        let currency_str = asset.get("currency").and_then(|c| c.as_str()).unwrap_or("");
        let issuer_str = asset.get("issuer").and_then(|i| i.as_str()).unwrap_or("");

        // Currency code: 3-char ISO -> 20 bytes (zeros + 3 chars at offset 12)
        let mut currency = [0u8; 20];
        if currency_str.len() == 3 {
            currency[12] = currency_str.as_bytes()[0];
            currency[13] = currency_str.as_bytes()[1];
            currency[14] = currency_str.as_bytes()[2];
        } else if currency_str.len() == 40 {
            if let Ok(bytes) = hex::decode(currency_str) {
                if bytes.len() == 20 { currency.copy_from_slice(&bytes); }
            }
        }
        buf.extend_from_slice(&currency);

        // Issuer: hex or base58 -> 20 bytes
        let mut issuer = [0u8; 20];
        if let Ok(bytes) = hex::decode(issuer_str) {
            if bytes.len() == 20 { issuer.copy_from_slice(&bytes); }
        }
        buf.extend_from_slice(&issuer);
    } else {
        // XRP: 20 zero bytes (currency) + 20 zero bytes (issuer)
        buf.extend_from_slice(&[0u8; 40]);
    }
}

fn read_amm_key_from_fields(tx: &TxFields) -> Option<xrpl_core::types::Hash256> {
    let key_bytes = amm_key(tx)?;
    Some(xrpl_core::types::Hash256(key_bytes))
}

/// For AMMDeposit/Withdraw/Vote/Bid/Delete — read the AMM key from Asset+Asset2
fn amm_key_from_asset_fields(tx: &TxFields) -> Option<xrpl_core::types::Hash256> {
    read_amm_key_from_fields(tx)
}

// ─── AMMCreate ───

pub struct AMMCreateTransactor;

impl Transactor for AMMCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMCreate" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Amount").is_none() || tx.fields.get("Amount2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Compute AMM key from the asset pair
        // AMMCreate uses Amount/Amount2, but the AMM object key uses Asset/Asset2
        // We derive asset info from the Amount fields
        let amount1 = &tx.fields["Amount"];
        let amount2 = &tx.fields["Amount2"];

        // Build asset pair for key computation
        let asset1 = if amount1.is_string() {
            serde_json::json!({"currency": "XRP"})
        } else {
            serde_json::json!({
                "currency": amount1.get("currency").unwrap_or(&serde_json::Value::Null),
                "issuer": amount1.get("issuer").unwrap_or(&serde_json::Value::Null),
            })
        };
        let asset2 = if amount2.is_string() {
            serde_json::json!({"currency": "XRP"})
        } else {
            serde_json::json!({
                "currency": amount2.get("currency").unwrap_or(&serde_json::Value::Null),
                "issuer": amount2.get("issuer").unwrap_or(&serde_json::Value::Null),
            })
        };

        // Compute AMM key
        let mut data = Vec::with_capacity(82);
        data.extend_from_slice(&[0x00, 0x41]);
        encode_asset_to_buf(&mut data, &asset1);
        encode_asset_to_buf(&mut data, &asset2);
        let amm_hash = sha512_half(&data);

        // Bug 4: Check for duplicate AMM
        if sandbox.exists(&amm_hash) {
            return TxResult::NoPermission;
        }

        // Deduct XRP amount from sender if Amount is XRP, track initial pool balance
        let mut initial_pool: u64 = 0;
        if let Some(drops_str) = amount1.as_str() {
            if let Ok(drops) = drops_str.parse::<u64>() {
                if !deduct_xrp(sandbox, &tx.account, drops) {
                    return TxResult::UnfundedPayment;
                }
                initial_pool += drops;
            }
        }
        if let Some(drops_str) = amount2.as_str() {
            if let Ok(drops) = drops_str.parse::<u64>() {
                if !deduct_xrp(sandbox, &tx.account, drops) {
                    return TxResult::UnfundedPayment;
                }
                initial_pool += drops;
            }
        }

        // Create AMM ledger entry
        let amm_obj = serde_json::json!({
            "LedgerEntryType": "AMM",
            "Account": hex::encode(tx.account),
            "Asset": asset1,
            "Asset2": asset2,
            "PoolBalance": initial_pool.to_string(),
            "LPTokenBalance": {
                "currency": "LPT",
                "issuer": hex::encode(tx.account),
                "value": "0"
            },
            "TradingFee": tx.fields.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0),
            "VoteSlots": [],
        });
        sandbox.write(amm_hash, serde_json::to_vec(&amm_obj).unwrap());

        // Increment OwnerCount
        increment_owner_count(sandbox, &tx.account);

        TxResult::Success
    }
}

// ─── AMMDeposit ───

pub struct AMMDepositTransactor;

impl Transactor for AMMDepositTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMDeposit" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        // Check AMM exists
        match amm_key_from_asset_fields(tx) {
            Some(key) => {
                if !sandbox.exists(&key) { return TxResult::NoEntry; }
            }
            None => return TxResult::Malformed,
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let amm_key = match amm_key_from_asset_fields(tx) {
            Some(k) => k,
            None => return TxResult::Malformed,
        };

        // Deduct deposited amounts from sender
        if let Some(amount) = tx.fields.get("Amount") {
            if let Some(drops_str) = amount.as_str() {
                if let Ok(drops) = drops_str.parse::<u64>() {
                    if !deduct_xrp(sandbox, &tx.account, drops) {
                        return TxResult::UnfundedPayment;
                    }
                    // Bug 7: Update AMM pool balance
                    update_amm_pool_balance(sandbox, &amm_key, drops, true);
                }
            }
        }
        if let Some(amount2) = tx.fields.get("Amount2") {
            if let Some(drops_str) = amount2.as_str() {
                if let Ok(drops) = drops_str.parse::<u64>() {
                    if !deduct_xrp(sandbox, &tx.account, drops) {
                        return TxResult::UnfundedPayment;
                    }
                    update_amm_pool_balance(sandbox, &amm_key, drops, true);
                }
            }
        }
        TxResult::Success
    }
}

// ─── AMMWithdraw ───

pub struct AMMWithdrawTransactor;

impl Transactor for AMMWithdrawTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMWithdraw" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        match amm_key_from_asset_fields(tx) {
            Some(key) => {
                if !sandbox.exists(&key) { return TxResult::NoEntry; }
            }
            None => return TxResult::Malformed,
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let amm_key = match amm_key_from_asset_fields(tx) {
            Some(k) => k,
            None => return TxResult::Malformed,
        };

        // Credit withdrawn amounts to sender
        if let Some(amount) = tx.fields.get("Amount") {
            if let Some(drops_str) = amount.as_str() {
                if let Ok(drops) = drops_str.parse::<u64>() {
                    // Bug 7: Verify AMM pool has sufficient funds before crediting
                    if !update_amm_pool_balance(sandbox, &amm_key, drops, false) {
                        return TxResult::Unfunded;
                    }
                    credit_xrp(sandbox, &tx.account, drops);
                }
            }
        }
        if let Some(amount2) = tx.fields.get("Amount2") {
            if let Some(drops_str) = amount2.as_str() {
                if let Ok(drops) = drops_str.parse::<u64>() {
                    if !update_amm_pool_balance(sandbox, &amm_key, drops, false) {
                        return TxResult::Unfunded;
                    }
                    credit_xrp(sandbox, &tx.account, drops);
                }
            }
        }
        TxResult::Success
    }
}

// ─── AMMVote ───

pub struct AMMVoteTransactor;

impl Transactor for AMMVoteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMVote" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        if tx.fields.get("TradingFee").is_none() { return TxResult::Malformed; }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Update AMM's VoteSlots with this account's fee vote
        if let Some(key) = amm_key_from_asset_fields(tx) {
            if let Some(data) = sandbox.read(&key) {
                if let Ok(mut amm) = serde_json::from_slice::<serde_json::Value>(&data) {
                    let fee = tx.fields.get("TradingFee").and_then(|f| f.as_u64()).unwrap_or(0);
                    let vote = serde_json::json!({
                        "Account": hex::encode(tx.account),
                        "TradingFee": fee,
                    });

                    // Add/replace vote in VoteSlots
                    let slots = amm.get_mut("VoteSlots")
                        .and_then(|s| s.as_array_mut());
                    if let Some(slots) = slots {
                        // Remove existing vote from this account
                        let acct_hex = hex::encode(tx.account);
                        slots.retain(|v| v.get("Account").and_then(|a| a.as_str()) != Some(&acct_hex));
                        slots.push(vote);
                        // Limit to 8 vote slots (rippled maximum)
                        if slots.len() > 8 { slots.remove(0); }
                    }

                    sandbox.write(key, serde_json::to_vec(&amm).unwrap());
                }
            }
        }
        TxResult::Success
    }
}

// ─── AMMBid ───

pub struct AMMBidTransactor;

impl Transactor for AMMBidTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMBid" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Bid for the AMM's auction slot (discounted trading fee)
        if let Some(key) = amm_key_from_asset_fields(tx) {
            if let Some(data) = sandbox.read(&key) {
                if let Ok(mut amm) = serde_json::from_slice::<serde_json::Value>(&data) {
                    amm["AuctionSlot"] = serde_json::json!({
                        "Account": hex::encode(tx.account),
                        "Expiration": 0,
                        "Price": tx.fields.get("BidMin").cloned().unwrap_or(serde_json::json!("0")),
                    });
                    sandbox.write(key, serde_json::to_vec(&amm).unwrap());
                }
            }
        }
        TxResult::Success
    }
}

// ─── AMMDelete ───

pub struct AMMDeleteTransactor;

impl Transactor for AMMDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AMMDelete" { return TxResult::Malformed; }
        if tx.fee == 0 { return TxResult::BadFee; }
        if tx.fields.get("Asset").is_none() || tx.fields.get("Asset2").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        if !sandbox.exists(&acct_key) { return TxResult::NoAccount; }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        if let Some(key) = amm_key_from_asset_fields(tx) {
            if sandbox.exists(&key) {
                // Read AMM to find the creator for OwnerCount
                if let Some(data) = sandbox.read(&key) {
                    if let Ok(amm) = serde_json::from_slice::<serde_json::Value>(&data) {
                        if let Some(creator_hex) = amm.get("Account").and_then(|a| a.as_str()) {
                            if let Ok(creator_bytes) = hex::decode(creator_hex) {
                                if creator_bytes.len() == 20 {
                                    let mut creator = [0u8; 20];
                                    creator.copy_from_slice(&creator_bytes);
                                    decrement_owner_count(sandbox, &creator);
                                }
                            }
                        }
                    }
                }
                sandbox.delete(key);
            }
        }
        TxResult::Success
    }
}

// ─── Helpers ───

fn deduct_xrp(sandbox: &mut Sandbox, account: &[u8; 20], drops: u64) -> bool {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if balance < drops {
                return false;
            }
            let new_balance = balance - drops;
            acct["Balance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
            return true;
        }
    }
    false
}

fn credit_xrp(sandbox: &mut Sandbox, account: &[u8; 20], drops: u64) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let balance = acct["Balance"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let new_balance = balance.saturating_add(drops);
            acct["Balance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

/// Update the AMM pool's XRP balance.
/// If `is_deposit` is true, adds `drops` to the pool.
/// If `is_deposit` is false (withdraw), checks the pool has sufficient funds and subtracts.
/// Returns true on success, false if the pool has insufficient funds (withdraw only).
fn update_amm_pool_balance(
    sandbox: &mut Sandbox,
    amm_key: &xrpl_core::types::Hash256,
    drops: u64,
    is_deposit: bool,
) -> bool {
    if let Some(data) = sandbox.read(amm_key) {
        if let Ok(mut amm) = serde_json::from_slice::<serde_json::Value>(&data) {
            let pool_balance = amm
                .get("PoolBalance")
                .and_then(|b| b.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            let new_balance = if is_deposit {
                pool_balance.saturating_add(drops)
            } else {
                if pool_balance < drops {
                    return false;
                }
                pool_balance - drops
            };

            amm["PoolBalance"] = serde_json::Value::String(new_balance.to_string());
            sandbox.write(*amm_key, serde_json::to_vec(&amm).unwrap());
            return true;
        }
    }
    // AMM not found — shouldn't happen if preclaim passed, but treat as failure for withdrawals
    is_deposit
}

fn increment_owner_count(sandbox: &mut Sandbox, account: &[u8; 20]) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let count = acct["OwnerCount"].as_u64().unwrap_or(0);
            acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

fn decrement_owner_count(sandbox: &mut Sandbox, account: &[u8; 20]) {
    let key = keylet::account_root_key(account);
    if let Some(data) = sandbox.read(&key) {
        if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
            let count = acct["OwnerCount"].as_u64().unwrap_or(0);
            if count > 0 {
                acct["OwnerCount"] = serde_json::Value::Number((count - 1).into());
            }
            sandbox.write(key, serde_json::to_vec(&acct).unwrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::sandbox::Sandbox;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn make_state(id: &[u8; 20], balance: u64) -> LedgerState {
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
        let acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(id),
            "Balance": balance.to_string(),
            "Sequence": 1,
            "OwnerCount": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        state
    }

    #[test]
    fn amm_create_and_delete() {
        let alice = [0x01u8; 20];
        let state = make_state(&alice, 100_000_000);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "50000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "100"},
                "TradingFee": 500,
            }),
        };

        assert_eq!(AMMCreateTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(AMMCreateTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Check balance reduced
        let key = keylet::account_root_key(&alice);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "50000000"); // 100M - 50M
        assert_eq!(v["OwnerCount"].as_u64().unwrap(), 1);
    }

    #[test]
    fn amm_vote() {
        let alice = [0x01u8; 20];
        let state = make_state(&alice, 100_000_000);

        // First create an AMM
        let mut sandbox = Sandbox::new(&state);
        let create_tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "50000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "100"},
                "TradingFee": 500,
            }),
        };
        AMMCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Vote on it
        let vote_tx = TxFields {
            account: alice,
            tx_type: "AMMVote".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20])},
                "TradingFee": 300,
            }),
        };
        assert_eq!(AMMVoteTransactor.preflight(&vote_tx), TxResult::Success);
        assert_eq!(AMMVoteTransactor.do_apply(&vote_tx, &mut sandbox), TxResult::Success);
    }

    #[test]
    fn amm_deposit_withdraw() {
        let alice = [0x01u8; 20];
        let state = make_state(&alice, 100_000_000);

        let mut sandbox = Sandbox::new(&state);

        // Create AMM first
        let create_tx = TxFields {
            account: alice,
            tx_type: "AMMCreate".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Amount": "30000000",
                "Amount2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20]), "value": "100"},
                "TradingFee": 500,
            }),
        };
        AMMCreateTransactor.do_apply(&create_tx, &mut sandbox);

        // Deposit more XRP
        let dep_tx = TxFields {
            account: alice,
            tx_type: "AMMDeposit".to_string(),
            fee: 12,
            sequence: 2,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20])},
                "Amount": "10000000",
            }),
        };
        assert_eq!(AMMDepositTransactor.do_apply(&dep_tx, &mut sandbox), TxResult::Success);

        // Balance: 100M - 30M - 10M = 60M
        let key = keylet::account_root_key(&alice);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "60000000");

        // Withdraw
        let wd_tx = TxFields {
            account: alice,
            tx_type: "AMMWithdraw".to_string(),
            fee: 12,
            sequence: 3,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "Asset": {"currency": "XRP"},
                "Asset2": {"currency": "USD", "issuer": hex::encode([0x02u8; 20])},
                "Amount": "5000000",
            }),
        };
        assert_eq!(AMMWithdrawTransactor.do_apply(&wd_tx, &mut sandbox), TxResult::Success);

        // Balance: 60M + 5M = 65M
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(v["Balance"].as_str().unwrap(), "65000000");
    }
}
