//! TrustSet transaction — create or modify trust lines (RippleState).
//!
//! # DEAD CODE WARNING
//!
//! This module is **not called** by the live validator. Production transaction
//! application is delegated to rippled's C++ engine via FFI — see
//! `crates/xrpl-ffi/src/lib.rs` and `crates/xrpl-node/src/ffi_engine.rs`.
//!
//! This code is retained as a reference implementation / learning artifact.
//! Tests in this module prove the code works in isolation; they do NOT prove
//! the validator is correct.
//!
//! If you are adding a new amendment or tx type: add it to the FFI path,
//! not here. See `ffi/ARCHITECTURE.md` for the architectural decision record.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

/// TrustSet transactor.
pub struct TrustSetTransactor;

impl TrustSetTransactor {
    /// Decode an account id from a tx-field string. Mainnet JSON encodes the
    /// LimitAmount issuer as a base58 classic address (`r…`); the engine's own
    /// internal state uses 20-byte hex. Accept both so real transactions parse.
    fn decode_issuer(s: &str) -> Option<[u8; 20]> {
        if let Ok(bytes) = hex::decode(s) {
            if bytes.len() == 20 {
                let mut a = [0u8; 20];
                a.copy_from_slice(&bytes);
                return Some(a);
            }
        }
        xrpl_core::types::AccountId::from_address(s).ok().map(|a| a.0)
    }

    fn extract_limit_amount(tx: &TxFields) -> Option<(&str, [u8; 20])> {
        let limit = tx.fields.get("LimitAmount")?;
        let currency = limit.get("currency")?.as_str()?;
        let issuer = Self::decode_issuer(limit.get("issuer")?.as_str()?)?;
        Some((currency, issuer))
    }

    /// Whether the sender's side of the (post-tx) trust line holds an
    /// *interest* — a non-default flag/quality/limit/balance that obliges the
    /// sender to reserve for the line. Faithful port of rippled SetTrust's
    /// `bLowReserveSet`/`bHighReserveSet` for the sender's side only (the sole
    /// side whose reserve rippled can newly charge, and thus the only side that
    /// drives `bReserveIncrease`). Reserve enforcement keys off THIS, not off a
    /// naive "limit != 0" — e.g. #105765230 clears NoRipple with limit 0 yet
    /// still triggers `tecINSUF_RESERVE_LINE` because a cleared NoRipple differs
    /// from a non-DefaultRipple account's default.
    ///
    /// Returns `(reserve_set, currently_reserved)` for the sender's side.
    fn sender_reserve_state(
        tx: &TxFields,
        sandbox: &Sandbox,
        line: &serde_json::Value,
        issuer: &[u8; 20],
    ) -> (bool, bool) {
        const LSF_LOW_RESERVE: u64 = 0x0001_0000;
        const LSF_HIGH_RESERVE: u64 = 0x0002_0000;
        const LSF_LOW_NO_RIPPLE: u64 = 0x0010_0000;
        const LSF_HIGH_NO_RIPPLE: u64 = 0x0020_0000;
        const LSF_LOW_FREEZE: u64 = 0x0040_0000;
        const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
        const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000; // AccountRoot flag
        const TF_SET_NO_RIPPLE: u64 = 0x0002_0000;
        const TF_CLEAR_NO_RIPPLE: u64 = 0x0004_0000;
        const TF_SET_FREEZE: u64 = 0x0010_0000;
        const TF_CLEAR_FREEZE: u64 = 0x0020_0000;
        const QUALITY_ONE: u64 = 1_000_000_000;

        let sender_low = &tx.account < issuer;
        let (no_ripple_bit, freeze_bit, reserve_bit, q_in_field, q_out_field) = if sender_low {
            (LSF_LOW_NO_RIPPLE, LSF_LOW_FREEZE, LSF_LOW_RESERVE, "LowQualityIn", "LowQualityOut")
        } else {
            (LSF_HIGH_NO_RIPPLE, LSF_HIGH_FREEZE, LSF_HIGH_RESERVE, "HighQualityIn", "HighQualityOut")
        };

        let flags_in = line["Flags"].as_u64().unwrap_or(0);
        let txf = tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0);

        // Low-account balance sign (line Balance is stored low-perspective).
        let bal = line["Balance"]["value"].as_str().unwrap_or("0");
        let bal_zero = bal == "0" || bal == "-0";
        let bal_neg = !bal_zero && bal.starts_with('-');
        let bal_pos = !bal_zero && !bal_neg;
        // Sender's balance is in-favor when the low-side balance leans their way.
        let sender_balance_positive = if sender_low { bal_pos } else { bal_neg };
        // "Cannot set noRipple on a negative balance" gate.
        let sender_balance_nonneg = if sender_low { !bal_neg } else { !bal_pos };

        // Post-tx NoRipple state for the sender's side.
        let mut no_ripple_set = flags_in & no_ripple_bit != 0;
        if txf & TF_SET_NO_RIPPLE != 0 && txf & TF_CLEAR_NO_RIPPLE == 0 {
            if sender_balance_nonneg {
                no_ripple_set = true;
            }
        } else if txf & TF_CLEAR_NO_RIPPLE != 0 && txf & TF_SET_NO_RIPPLE == 0 {
            no_ripple_set = false;
        }

        // Post-tx freeze state (simple set/clear, matching do_apply).
        let mut freeze_set = flags_in & freeze_bit != 0;
        if txf & TF_SET_FREEZE != 0 && txf & TF_CLEAR_FREEZE == 0 {
            freeze_set = true;
        } else if txf & TF_CLEAR_FREEZE != 0 && txf & TF_SET_FREEZE == 0 {
            freeze_set = false;
        }

        // Post-tx quality on the sender's side (tx value if setting, else line).
        let norm_quality = |q: u64| if q == QUALITY_ONE { 0 } else { q };
        let q_in = match tx.fields.get("QualityIn").and_then(|v| v.as_u64()) {
            Some(q) => norm_quality(q),
            None => norm_quality(line.get(q_in_field).and_then(|v| v.as_u64()).unwrap_or(0)),
        };
        let q_out = match tx.fields.get("QualityOut").and_then(|v| v.as_u64()) {
            Some(q) => norm_quality(q),
            None => norm_quality(line.get(q_out_field).and_then(|v| v.as_u64()).unwrap_or(0)),
        };

        // Sender's new limit (the tx sets the sender's side to LimitAmount).
        let new_limit = tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0");
        let limit_nonzero = new_limit != "0";

        // Sender account's DefaultRipple flag.
        let sender_default_ripple = sandbox
            .read(&keylet::account_root_key(&tx.account))
            .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            .map(|a| a["Flags"].as_u64().unwrap_or(0) & LSF_DEFAULT_RIPPLE != 0)
            .unwrap_or(false);

        let reserve_set = q_in != 0
            || q_out != 0
            || ((!no_ripple_set) != sender_default_ripple)
            || freeze_set
            || limit_nonzero
            || sender_balance_positive;
        let currently_reserved = flags_in & reserve_bit != 0;
        (reserve_set, currently_reserved)
    }

    /// Build the 20-byte currency code from a 3-char ISO code.
    fn currency_code(iso: &str) -> [u8; 20] {
        let mut code = [0u8; 20];
        if iso.len() == 3 {
            // Standard currency: 12 zero bytes + 3 ASCII bytes + 5 zero bytes
            code[12] = iso.as_bytes()[0];
            code[13] = iso.as_bytes()[1];
            code[14] = iso.as_bytes()[2];
        } else if iso.len() == 40 {
            // Hex currency code
            if let Ok(bytes) = hex::decode(iso) {
                if bytes.len() == 20 {
                    code.copy_from_slice(&bytes);
                }
            }
        }
        code
    }
}

impl Transactor for TrustSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "TrustSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        if tx.fields.get("LimitAmount").is_none() {
            return TxResult::Malformed;
        }
        // Reject self-trust-lines: account cannot trust itself
        if let Some((_, issuer)) = Self::extract_limit_amount(tx) {
            if tx.account == issuer {
                return TxResult::Malformed;
            }
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        let Some(data) = sandbox.read(&acct_key) else {
            return TxResult::NoAccount;
        };
        let Ok(acct) = serde_json::from_slice::<serde_json::Value>(&data) else {
            return TxResult::Malformed;
        };

        let Some((currency_str, issuer)) = Self::extract_limit_amount(tx) else {
            return TxResult::Malformed;
        };

        // Owner reserve required to gain a trust line. rippled SetTrust:
        // reserve is NOT enforced until the account owns >= 2 objects — the
        // gateway-funding carve-out — otherwise accountReserve(ownerCount + 1).
        let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
        let reserve_create = if oc < 2 {
            0
        } else {
            crate::ledger::fees::account_reserve(sandbox, oc + 1)
        };
        // preclaim runs before apply_common deducts the fee → this IS
        // rippled's preFeeBalance_.
        let balance = acct["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);

        let currency = Self::currency_code(currency_str);
        let line_key = keylet::ripple_state_key(&tx.account, &issuer, &currency);

        if let Some(line_data) = sandbox.read(&line_key) {
            // Existing line: charge the sender only when this tx flips their
            // side from unreserved to reserved (bReserveIncrease) and they
            // cannot cover the incremental reserve → tecINSUF_RESERVE_LINE
            // (#105765230, #105765676, #105779597, #105795073, #105796649).
            if let Ok(line) = serde_json::from_slice::<serde_json::Value>(&line_data) {
                let (reserve_set, currently_reserved) =
                    Self::sender_reserve_state(tx, sandbox, &line, &issuer);
                let reserve_increase = reserve_set && !currently_reserved;
                if reserve_increase && balance < reserve_create {
                    return TxResult::InsufReserveLine;
                }
            }
        } else {
            // New line. rippled orders these two checks: the redundant guard
            // fires BEFORE the reserve guard, so replicate that order.
            const QUALITY_ONE: u64 = 1_000_000_000;
            const TF_SETF_AUTH: u64 = 0x0001_0000;
            let txf = tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0);
            let limit_zero = tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0") == "0";
            let q_in = tx.fields.get("QualityIn").and_then(|v| v.as_u64());
            let q_out = tx.fields.get("QualityOut").and_then(|v| v.as_u64());
            let setting_q_in = matches!(q_in, Some(q) if q != 0 && q != QUALITY_ONE);
            let setting_q_out = matches!(q_out, Some(q) if q != 0 && q != QUALITY_ONE);
            let set_auth = txf & TF_SETF_AUTH != 0;
            if limit_zero && !setting_q_in && !setting_q_out && !set_auth {
                // Setting a non-existent line to defaults changes nothing
                // (#105765676 DD72D291).
                return TxResult::NoLineRedundant;
            }
            if balance < reserve_create {
                return TxResult::NoLineInsufReserve;
            }
        }

        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let (currency_str, issuer) = match Self::extract_limit_amount(tx) {
            Some(v) => v,
            None => return TxResult::Malformed,
        };

        let currency = Self::currency_code(currency_str);
        let line_key = keylet::ripple_state_key(&tx.account, &issuer, &currency);

        if let Some(data) = sandbox.read(&line_key) {
            // Trust line exists — update the correct side (LowLimit or HighLimit).
            // RippleState has LowLimit (lower account) and HighLimit (higher account).
            // Determine which side the sender is on by lexicographic comparison.
            if let Ok(mut line) = serde_json::from_slice::<serde_json::Value>(&data) {
                let new_value = tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0");
                if tx.account < issuer {
                    // Sender is the low side
                    line["LowLimit"]["value"] = serde_json::Value::String(new_value.to_string());
                } else {
                    // Sender is the high side
                    line["HighLimit"]["value"] = serde_json::Value::String(new_value.to_string());
                }
                if let Some(qin) = tx.fields.get("QualityIn") {
                    line["QualityIn"] = qin.clone();
                }
                if let Some(qout) = tx.fields.get("QualityOut") {
                    line["QualityOut"] = qout.clone();
                }

                // Apply the tx's tf-flags to the SENDER's side of the line
                // (tfSetNoRipple/tfClearNoRipple/tfSetFreeze/tfClearFreeze)
                // BEFORE the default-state test below — rippled evaluates
                // deletability on the post-flag state.
                {
                    const TF_SET_NO_RIPPLE: u64 = 0x0002_0000;
                    const TF_CLEAR_NO_RIPPLE: u64 = 0x0004_0000;
                    const TF_SET_FREEZE: u64 = 0x0010_0000;
                    const TF_CLEAR_FREEZE: u64 = 0x0020_0000;
                    let txf = tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0);
                    let sender_low = tx.account < issuer;
                    let (no_ripple_bit, freeze_bit) = if sender_low {
                        (0x0010_0000u64, 0x0040_0000u64) // lsfLowNoRipple, lsfLowFreeze
                    } else {
                        (0x0020_0000u64, 0x0080_0000u64) // lsfHighNoRipple, lsfHighFreeze
                    };
                    let mut lf = line["Flags"].as_u64().unwrap_or(0);
                    if txf & TF_SET_NO_RIPPLE != 0 {
                        lf |= no_ripple_bit;
                    } else if txf & TF_CLEAR_NO_RIPPLE != 0 {
                        lf &= !no_ripple_bit;
                    }
                    if txf & TF_SET_FREEZE != 0 {
                        lf |= freeze_bit;
                    } else if txf & TF_CLEAR_FREEZE != 0 {
                        lf &= !freeze_bit;
                    }
                    line["Flags"] = serde_json::Value::Number(lf.into());
                }

                // Default-line deletion (mirrors rippled SetTrust): after the
                // limit update, each side keeps the line only if it still has
                // an *interest* — a limit, a balance in its favor, quality
                // settings, freeze, auth, or a NoRipple state differing from
                // its account default. A side's default NoRipple is SET iff
                // its account lacks lsfDefaultRipple, so "non-default" is the
                // elegant `noRipple == defaultRipple`. No interest on either
                // side + zero balance → delete the RippleState, unlink it from
                // both owners' directories (via the line's LowNode/HighNode
                // page hints, rippled's no-walk design), and decrement
                // OwnerCount for each side whose reserve flag was set.
                const LSF_LOW_RESERVE: u64 = 0x0001_0000;
                const LSF_HIGH_RESERVE: u64 = 0x0002_0000;
                const LSF_LOW_AUTH: u64 = 0x0004_0000;
                const LSF_HIGH_AUTH: u64 = 0x0008_0000;
                const LSF_LOW_NO_RIPPLE: u64 = 0x0010_0000;
                const LSF_HIGH_NO_RIPPLE: u64 = 0x0020_0000;
                const LSF_LOW_FREEZE: u64 = 0x0040_0000;
                const LSF_HIGH_FREEZE: u64 = 0x0080_0000;
                const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000; // AccountRoot flag

                let flags = line["Flags"].as_u64().unwrap_or(0);
                let bal = line["Balance"]["value"].as_str().unwrap_or("0");
                let bal_zero = bal == "0" || bal == "-0";
                let bal_pos = !bal_zero && !bal.starts_with('-');
                let bal_neg = !bal_zero && bal.starts_with('-');
                let limit_zero = |side: &str| line[side]["value"].as_str().unwrap_or("0") == "0";
                let quality = |f: &str| line.get(f).and_then(|v| v.as_u64()).unwrap_or(0) != 0;
                let (low_id, high_id) = if tx.account < issuer {
                    (tx.account, issuer)
                } else {
                    (issuer, tx.account)
                };
                let default_ripple = |data: Option<&[u8]>| {
                    data.and_then(|d| serde_json::from_slice::<serde_json::Value>(d).ok())
                        .map(|a| a["Flags"].as_u64().unwrap_or(0) & LSF_DEFAULT_RIPPLE != 0)
                        .unwrap_or(false)
                };
                let low_root = sandbox.read(&keylet::account_root_key(&low_id));
                let high_root = sandbox.read(&keylet::account_root_key(&high_id));
                let low_dr = default_ripple(low_root.as_deref());
                let high_dr = default_ripple(high_root.as_deref());
                let low_interest = !limit_zero("LowLimit")
                    || bal_pos
                    || quality("LowQualityIn")
                    || quality("LowQualityOut")
                    || ((flags & LSF_LOW_NO_RIPPLE != 0) == low_dr)
                    || flags & (LSF_LOW_FREEZE | LSF_LOW_AUTH) != 0;
                let high_interest = !limit_zero("HighLimit")
                    || bal_neg
                    || quality("HighQualityIn")
                    || quality("HighQualityOut")
                    || ((flags & LSF_HIGH_NO_RIPPLE != 0) == high_dr)
                    || flags & (LSF_HIGH_FREEZE | LSF_HIGH_AUTH) != 0;

                if bal_zero && !low_interest && !high_interest {
                    let hint = |v: &serde_json::Value| -> Option<u64> {
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                    };
                    let low_hint = hint(&line["LowNode"]);
                    let high_hint = hint(&line["HighNode"]);
                    sandbox.delete(line_key);
                    crate::ledger::directory::owner_dir_remove(sandbox, &low_id, &line_key, low_hint);
                    crate::ledger::directory::owner_dir_remove(sandbox, &high_id, &line_key, high_hint);
                    // rippled touches BOTH AccountRoots at trustDelete (the
                    // reserve payer's OwnerCount decrements; the other side is
                    // a no-op Modified that rippled still records in the meta).
                    // Mirror the create path's convention (which bumps both):
                    // decrement both, keeping the pair self-consistent and the
                    // key-set identical to mainnet's.
                    for id in [low_id, high_id] {
                        let acct_key = keylet::account_root_key(&id);
                        if let Some(data) = sandbox.read(&acct_key) {
                            if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                                let c = acct["OwnerCount"].as_u64().unwrap_or(0);
                                acct["OwnerCount"] = serde_json::Value::Number(c.saturating_sub(1).into());
                                sandbox.write(acct_key, serde_json::to_vec(&acct).expect("valid JSON"));
                            }
                        }
                    }
                } else {
                    sandbox.write(line_key, serde_json::to_vec(&line).expect("serializing valid JSON Value"));
                }
            }
        } else {
            // Create new trust line. Creation flags per rippled trustCreate:
            // the creator's side gets its reserve bit; its NoRipple bit follows
            // the tx's tfSetNoRipple/tfClearNoRipple, defaulting to SET when
            // the creator's account lacks lsfDefaultRipple.
            let creation_flags = {
                let sender_low = tx.account < issuer;
                let reserve_bit: u64 = if sender_low { 0x0001_0000 } else { 0x0002_0000 };
                let no_ripple_bit: u64 = if sender_low { 0x0010_0000 } else { 0x0020_0000 };
                let txf = tx.fields.get("Flags").and_then(|v| v.as_u64()).unwrap_or(0);
                let creator_default_ripple = sandbox
                    .read(&keylet::account_root_key(&tx.account))
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                    .map(|a| a["Flags"].as_u64().unwrap_or(0) & 0x0080_0000 != 0)
                    .unwrap_or(false);
                let mut f = reserve_bit;
                if txf & 0x0002_0000 != 0 {
                    f |= no_ripple_bit; // tfSetNoRipple
                } else if txf & 0x0004_0000 == 0 && !creator_default_ripple {
                    f |= no_ripple_bit; // default NoRipple (no tfClearNoRipple)
                }
                f
            };
            let line_obj = serde_json::json!({
                "LedgerEntryType": "RippleState",
                "Balance": {
                    "currency": currency_str,
                    "issuer": "0000000000000000000000000000000000000000",
                    "value": "0"
                },
                "LowLimit": {
                    "currency": currency_str,
                    "issuer": hex::encode(if tx.account < issuer { tx.account } else { issuer }),
                    "value": if tx.account < issuer {
                        tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0").to_string()
                    } else {
                        "0".to_string()
                    }
                },
                "HighLimit": {
                    "currency": currency_str,
                    "issuer": hex::encode(if tx.account < issuer { issuer } else { tx.account }),
                    "value": if tx.account >= issuer {
                        tx.fields["LimitAmount"]["value"].as_str().unwrap_or("0").to_string()
                    } else {
                        "0".to_string()
                    }
                },
                "Flags": creation_flags,
            });
            sandbox.write(line_key, serde_json::to_vec(&line_obj).expect("serializing valid JSON Value"));

            // A new RippleState is inserted into BOTH parties' owner directories.
            crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &line_key);
            crate::ledger::directory::owner_dir_insert(sandbox, &issuer, &line_key);

            // Increment OwnerCount for both accounts
            for id in [&tx.account, &issuer] {
                let acct_key = keylet::account_root_key(id);
                if let Some(data) = sandbox.read(&acct_key) {
                    if let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) {
                        let count = acct["OwnerCount"].as_u64().unwrap_or(0);
                        acct["OwnerCount"] = serde_json::Value::Number((count + 1).into());
                        sandbox.write(acct_key, serde_json::to_vec(&acct).expect("serializing valid JSON Value"));
                    }
                }
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

    #[test]
    fn decode_issuer_accepts_hex_and_base58() {
        let id = [0x0Au8; 20];
        // internal-state convention: 20-byte hex
        assert_eq!(TrustSetTransactor::decode_issuer(&hex::encode(id)), Some(id));
        // mainnet JSON convention: base58 classic address
        let addr = xrpl_core::types::AccountId(id).to_address();
        assert_eq!(TrustSetTransactor::decode_issuer(&addr), Some(id));
        // junk decodes to nothing (not a panic, not a wrong id)
        assert_eq!(TrustSetTransactor::decode_issuer("not-an-address"), None);
    }

    #[test]
    fn create_trust_line() {
        let alice = [0x01u8; 20];
        let issuer = [0x02u8; 20];
        let state = make_state(&[(alice, 50_000_000), (issuer, 50_000_000)]);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "TrustSet".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({
                "LimitAmount": {
                    "currency": "USD",
                    "issuer": hex::encode(issuer),
                    "value": "1000"
                }
            }),
        };

        assert_eq!(TrustSetTransactor.preflight(&tx), TxResult::Success);
        assert_eq!(TrustSetTransactor.preclaim(&tx, &sandbox), TxResult::Success);
        assert_eq!(TrustSetTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Trust line should exist
        let currency = TrustSetTransactor::currency_code("USD");
        let line_key = keylet::ripple_state_key(&alice, &issuer, &currency);
        assert!(sandbox.exists(&line_key));
    }
}
