//! AccountSet and AccountDelete transactions.
//!
//! AccountSet: modify account flags, domain, transfer rate, etc.
//! AccountDelete: remove an account (requires OwnerCount=0, high fee).
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

/// AccountSet transactor.
pub struct AccountSetTransactor;

impl Transactor for AccountSetTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AccountSet" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
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
        let acct_key = keylet::account_root_key(&tx.account);
        let data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let mut acct: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // asf → lsf per rippled SetAccount: the asf NUMBERS are not bit
        // positions. #106455275 56049FD1 (SetFlag 13, DisallowIncomingCheck):
        // mainnet stamps lsf 0x08000000 where the old `1 << 13` wrote 0x2000
        // — invisible to every per-tx leg (mut compare is (key,kind), flags
        // compared nowhere) and dormant for 240 replay ledgers because
        // flag-setting AccountSets are rare. Constants verbatim from
        // LedgerFormats.h:128-142 / TxFlags asf table :408-425. asf 5 and 10
        // manage FIELD PRESENCE, not bits; unmapped values under 32 are
        // rippled's silent no-op, not an error.
        fn asf_lsf(flag: u64) -> Option<u64> {
            Some(match flag {
                1 => 0x0002_0000,  // asfRequireDest → lsfRequireDestTag
                2 => 0x0004_0000,  // asfRequireAuth
                3 => 0x0008_0000,  // asfDisallowXRP
                4 => 0x0010_0000,  // asfDisableMaster
                6 => 0x0020_0000,  // asfNoFreeze
                7 => 0x0040_0000,  // asfGlobalFreeze
                8 => 0x0080_0000,  // asfDefaultRipple
                9 => 0x0100_0000,  // asfDepositAuth
                12 => 0x0400_0000, // asfDisallowIncomingNFTokenOffer
                13 => 0x0800_0000, // asfDisallowIncomingCheck
                14 => 0x1000_0000, // asfDisallowIncomingPayChan
                15 => 0x2000_0000, // asfDisallowIncomingTrustline
                16 => 0x8000_0000, // asfAllowTrustLineClawback
                17 => 0x4000_0000, // asfAllowTrustLineLocking
                _ => return None,  // 5 AccountTxnID, 10 NFTokenMinter, 11 reserved
            })
        }

        // Apply SetFlag
        if let Some(flag) = tx.fields.get("SetFlag").and_then(|f| f.as_u64()) {
            if flag >= 32 {
                return TxResult::Malformed;
            }
            if let Some(bit) = asf_lsf(flag) {
                let current = acct["Flags"].as_u64().unwrap_or(0);
                acct["Flags"] = serde_json::Value::Number((current | bit).into());
            } else if flag == 5 && acct.get("AccountTxnID").is_none() {
                // asfAccountTxnID: make the field present (zero hash);
                // apply_common then rewrites it with each tx hash.
                acct["AccountTxnID"] =
                    serde_json::Value::String("0".repeat(64));
            } else if flag == 10 {
                // asfAuthorizedNFTokenMinter: the minter comes with the tx.
                if let Some(m) = tx.fields.get("NFTokenMinter") {
                    acct["NFTokenMinter"] = m.clone();
                }
            }
        }

        // Apply ClearFlag
        if let Some(flag) = tx.fields.get("ClearFlag").and_then(|f| f.as_u64()) {
            if flag >= 32 {
                return TxResult::Malformed;
            }
            if let Some(bit) = asf_lsf(flag) {
                let current = acct["Flags"].as_u64().unwrap_or(0);
                acct["Flags"] = serde_json::Value::Number((current & !bit).into());
            } else if flag == 5 {
                if let Some(o) = acct.as_object_mut() {
                    o.remove("AccountTxnID");
                }
            } else if flag == 10 {
                if let Some(o) = acct.as_object_mut() {
                    o.remove("NFTokenMinter");
                }
            }
        }

        // Apply optional fields
        // The transaction-level tf* flags are the OLD spelling of the same
        // switches and rippled honours both: `bSetRequireDest = (uTxFlags &
        // tfRequireDestTag) || uSetFlag == asfRequireDest` and so on for
        // RequireAuth and DisallowXRP, with tfOptionalDestTag / tfOptionalAuth
        // / tfAllowXRP as the clears (SetAccount.cpp:326-336, applied
        // :45-84). We only read SetFlag/ClearFlag. #106698282 95AFC9A9
        // (finding 65): `SetFlag: 8, Flags: 0x100000` (tfDisallowXRP) on a
        // fresh issuer lands Flags 0x880000 on mainnet — DefaultRipple AND
        // DisallowXRP — and 0x800000 here.
        {
            const TF_REQUIRE_DEST_TAG: u64 = 0x0001_0000;
            const TF_OPTIONAL_DEST_TAG: u64 = 0x0002_0000;
            const TF_REQUIRE_AUTH: u64 = 0x0004_0000;
            const TF_OPTIONAL_AUTH: u64 = 0x0008_0000;
            const TF_DISALLOW_XRP: u64 = 0x0010_0000;
            const TF_ALLOW_XRP: u64 = 0x0020_0000;
            let tf = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
            let mut flags = acct["Flags"].as_u64().unwrap_or(0);
            // RequireAuth can only be switched ON while the account owns
            // nothing (tecOWNERS, SetAccount.cpp preclaim). `flags_in` is
            // the pre-tx word — the SetFlag path above may already have
            // set the bit, so judge on the transaction's intent.
            let flags_in = serde_json::from_slice::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v["Flags"].as_u64())
                .unwrap_or(0);
            let wants_require_auth = tf & TF_REQUIRE_AUTH != 0
                || tx.fields.get("SetFlag").and_then(|f| f.as_u64()) == Some(2);
            // F86 — asfAllowTrustLineClawback (16) has the same "owns nothing"
            // gate (SetAccount.cpp:278-292): refused tecNO_PERMISSION while
            // lsfNoFreeze is set, tecOWNERS while the owner directory holds
            // anything. #106703565 9709366F: SetFlag 16 by an account with 96
            // objects — mainnet tecOWNERS, we set the bit.
            let wants_clawback = tx.fields.get("SetFlag").and_then(|f| f.as_u64()) == Some(16)
                && flags_in & 0x8000_0000 == 0;
            if wants_clawback && flags_in & 0x0020_0000 != 0 {
                return TxResult::NoPermission;
            }
            if (wants_require_auth && flags_in & 0x0004_0000 == 0) || wants_clawback {
                let dir_key = keylet::owner_dir_key(&tx.account);
                let owns_something = sandbox
                    .read(&dir_key)
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                    .map(|root| {
                        root.get("Indexes").and_then(|v| v.as_array()).is_some_and(|a| !a.is_empty())
                            || root.get("IndexNext").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|x| u64::from_str_radix(x, 16).ok()))).unwrap_or(0) != 0
                    })
                    .unwrap_or(false);
                if owns_something {
                    return TxResult::Owners;
                }
            }
            for (set_bit, clear_bit, lsf) in [
                (TF_REQUIRE_DEST_TAG, TF_OPTIONAL_DEST_TAG, 0x0002_0000u64),
                (TF_REQUIRE_AUTH, TF_OPTIONAL_AUTH, 0x0004_0000u64),
                (TF_DISALLOW_XRP, TF_ALLOW_XRP, 0x0008_0000u64),
            ] {
                if tf & set_bit != 0 {
                    flags |= lsf;
                }
                if tf & clear_bit != 0 {
                    flags &= !lsf;
                }
            }
            acct["Flags"] = serde_json::Value::Number(flags.into());
        }
        // F70 — AN EMPTY OR ZERO VALUE CLEARS THE FIELD (SetAccount.cpp:500-590).
        // rippled never files the sentinel: an empty Domain/MessageKey blob, a
        // zero EmailHash/WalletLocator, a TransferRate of 0 or QUALITY_ONE and
        // a TickSize of 0 or Quality::maxTickSize (15) all `makeFieldAbsent`.
        // We copied the tx value across, so a clear wrote an empty VL field.
        //
        // #106699631 D842A3B1: `Domain: ""` with SetFlag 15 — mainnet's root
        // is 87 bytes, ours carried a `7700` (empty Domain) at 89.
        let mut clear = |acct: &mut serde_json::Value, field: &str| {
            if let Some(o) = acct.as_object_mut() {
                o.remove(field);
            }
        };
        for field in ["Domain", "MessageKey"] {
            if let Some(val) = tx.fields.get(field) {
                if val.as_str().is_none_or(|v| v.is_empty()) {
                    clear(&mut acct, field);
                } else {
                    acct[field] = val.clone();
                }
            }
        }
        for field in ["EmailHash", "WalletLocator"] {
            if let Some(val) = tx.fields.get(field) {
                if val.as_str().is_none_or(|v| v.bytes().all(|b| b == b'0')) {
                    clear(&mut acct, field);
                } else {
                    acct[field] = val.clone();
                }
            }
        }
        if let Some(val) = tx.fields.get("TransferRate") {
            match val.as_u64() {
                Some(r) if r != 0 && r != 1_000_000_000 => acct["TransferRate"] = val.clone(),
                _ => clear(&mut acct, "TransferRate"),
            }
        }
        if let Some(val) = tx.fields.get("TickSize") {
            match val.as_u64() {
                Some(t) if t != 0 && t != 15 => acct["TickSize"] = val.clone(),
                _ => clear(&mut acct, "TickSize"),
            }
        }

        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap());
        TxResult::Success
    }
}

/// AccountDelete transactor.
pub struct AccountDeleteTransactor;

impl Transactor for AccountDeleteTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "AccountDelete" {
            return TxResult::Malformed;
        }
        if tx.fields.get("Destination").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        // "The fee required for AccountDelete is one owner reserve"
        // (AccountDelete::calculateBaseFee -> calculateOwnerReserveFee). That
        // is read from the ledger's fee settings, not fixed: the 2024 vote cut
        // the increment from 2 XRP to 0.2, so a hardcoded 2_000_000 rejects
        // every present-day AccountDelete (#105764469 2A99D114 pays exactly
        // the 200000-drop increment — mainnet tesSUCCESS, we said temBAD_FEE).
        // Needs the view, so it belongs here rather than in preflight.
        if tx.fee < crate::ledger::fees::reserve_inc(sandbox) {
            return TxResult::BadFee;
        }
        let acct_key = keylet::account_root_key(&tx.account);
        let data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let acct: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        // OwnerCount ZERO IS NOT THE SAME AS OWNING NOTHING — and owning
        // SOMETHING is not the same as an obligation. rippled has NO
        // OwnerCount rule here at all: the directory walk below decides, and
        // an obligation answers tecHAS_OBLIGATIONS whatever the count says.
        // #106066467 248BF6E3: rhokiAcW holds obligations at a nonzero count;
        // mainnet says HAS_OBLIGATIONS, the old count-first gate said
        // NO_PERMISSION — right refusal, wrong code, from a rule rippled
        // never had. The count-gate survives only BELOW the walk, as the
        // stand-in for the doApply deleter we don't model yet (rippled
        // deletes an all-deletable directory along with the account).
        //
        // rippled
        // walks the owner DIRECTORY and refuses on any entry whose type has no
        // `nonObligationDeleter` (AccountDelete.cpp):
        //     if (dirIsEmpty(ctx.view, ownerDirKeylet)) return tesSUCCESS;
        //     do {
        //         auto sleItem = ctx.view.read(keylet::child(dirEntry));
        //         if (nonObligationDeleter(nodeType) == nullptr)
        //             return tecHAS_OBLIGATIONS;
        //     } while (cdirNext(...));
        // Deletable: Offer, SignerList, Ticket, DepositPreauth, NFTokenOffer,
        // DID, Oracle, Credential, Delegate. Everything else is an obligation.
        //
        // #106322004 77C5E61D: the account holds OwnerCount 0 and one ESCROW.
        // An escrow that names this account as DESTINATION is linked into its
        // directory but does NOT raise its OwnerCount — the reserve sits with
        // the escrow's creator. So the count says "owns nothing" while the
        // directory says otherwise, and mainnet refuses. We deleted the account
        // and paid its balance away: 2 mutations against mainnet's 1, and an
        // account destroyed that mainnet keeps.
        const DELETABLE: [&str; 9] = [
            "Offer", "SignerList", "Ticket", "DepositPreauth", "NFTokenOffer",
            "DID", "Oracle", "Credential", "Delegate",
        ];
        let dir_root = keylet::owner_dir_key(&tx.account);
        let mut page_key = dir_root;
        for _ in 0..1000 {
            let Some(page) = sandbox
                .read(&page_key)
                .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
            else { break };
            for idx in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
                let Some(k) = idx.as_str().and_then(|s| {
                    hex::decode(s).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                }) else { continue };
                let kind = sandbox
                    .read(&xrpl_core::types::Hash256(k))
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                    .and_then(|o| o.get("LedgerEntryType").and_then(|t| t.as_str()).map(str::to_string));
                match kind {
                    // Unreadable means unhydrated, not absent — refusing on it
                    // would invent obligations. Anything we CAN read decides.
                    None => continue,
                    Some(t) if DELETABLE.contains(&t.as_str()) => continue,
                    Some(_) => return TxResult::HasObligations,
                }
            }
            let next = page
                .get("IndexNext")
                .and_then(|v| {
                    v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                })
                .unwrap_or(0);
            if next == 0 {
                break;
            }
            page_key = keylet::dir_page_key(&dir_root, next);
        }

        // Finding 41 (#106669431 535CFFF6): the count-gate that stood in for
        // the unmodeled deleter is gone — the walk above is the whole rule.
        // An all-deletable directory falls through to do_apply's cascade,
        // exactly rippled's shape (obligations already refused per entry).

        // Account sequence + 256 must be <= current ledger sequence
        let acct_seq = acct["Sequence"].as_u64().unwrap_or(0) as u32;
        let ledger_seq = sandbox.base().header.sequence;
        if acct_seq.saturating_add(256) > ledger_seq {
            return TxResult::NoPermission;
        }

        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let acct_key = keylet::account_root_key(&tx.account);
        let data = match sandbox.read(&acct_key) {
            Some(d) => d,
            None => return TxResult::NoAccount,
        };
        let acct: serde_json::Value = match serde_json::from_slice(&data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };

        let balance = acct["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // Transfer remaining balance to destination
        let dest_hex = match tx.fields.get("Destination").and_then(|d| d.as_str()) {
            Some(h) => h,
            None => return TxResult::Malformed,
        };
        let dest_bytes = match hex::decode(dest_hex) {
            Ok(b) if b.len() == 20 => b,
            _ => return TxResult::Malformed,
        };
        let mut dest_id = [0u8; 20];
        dest_id.copy_from_slice(&dest_bytes);
        let dest_key = keylet::account_root_key(&dest_id);

        // Destination must exist
        let dest_data = match sandbox.read(&dest_key) {
            Some(d) => d,
            None => return TxResult::NoDst,
        };
        let mut dest: serde_json::Value = match serde_json::from_slice(&dest_data) {
            Ok(v) => v,
            Err(_) => return TxResult::Malformed,
        };
        // `AccountDelete::preclaim` (:229-235): tecNO_DST, then
        // lsfRequireDestTag with no DestinationTag -> tecDST_TAG_NEEDED.
        // Same sweep as pay_channel above; no failing ledger pins this one
        // either, but rippled's condition is unambiguous.
        if dest["Flags"].as_u64().unwrap_or(0) & 0x0002_0000 != 0
            && tx.fields.get("DestinationTag").is_none()
        {
            return TxResult::DstTagNeeded; // lsfRequireDestTag
        }

        let dest_balance = dest["Balance"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        dest["Balance"] =
            serde_json::Value::String(dest_balance.checked_add(balance).unwrap_or(u64::MAX).to_string());
        // F80 — "Re-arm the password change fee": a positive credit clears the
        // destination's lsfPasswordSpent (DeleteAccount.cpp:436-437), exactly
        // as an XRP Payment does (Payment.cpp:715-717, mirrored in payment.rs).
        // #106702154 67271C65: rns1WK7… held 0x20010000 and mainnet leaves it
        // at 0x20000000; we kept the bit.
        if balance > 0 {
            const LSF_PASSWORD_SPENT: u64 = 0x0001_0000;
            let dflags = dest["Flags"].as_u64().unwrap_or(0);
            if dflags & LSF_PASSWORD_SPENT != 0 {
                dest["Flags"] = serde_json::json!(dflags & !LSF_PASSWORD_SPENT);
            }
        }
        sandbox.write(dest_key, serde_json::to_vec(&dest).unwrap());

        // Finding 41: the nonObligationDeleter cascade. preclaim proved every
        // owned object deletable; delete each with its own machinery — plain
        // offers leave their book (delete_maker_offer), NFT offers leave the
        // token-side directory and THREAD their Destination (rippled's meta
        // for the specimen shows the Destination's root as a ModifiedNode
        // with EMPTY PreviousFields — a threading-only touch), the simple
        // types just leave the owner directory. Deletable-in-preclaim types
        // we have no modeled deleter for (DID/Oracle/Credential/Delegate)
        // keep the conservative refusal until a specimen arrives.
        {
            let dir_root = keylet::owner_dir_key(&tx.account);
            let mut cascade: Vec<(xrpl_core::types::Hash256, serde_json::Value)> = Vec::new();
            let mut page_key = dir_root;
            for _ in 0..1000 {
                let Some(page) = sandbox
                    .read(&page_key)
                    .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                else { break };
                for idx in page.get("Indexes").and_then(|v| v.as_array()).into_iter().flatten() {
                    let Some(k) = idx.as_str().and_then(|s| {
                        hex::decode(s).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                    }) else { continue };
                    let kh = xrpl_core::types::Hash256(k);
                    if let Some(obj) = sandbox
                        .read(&kh)
                        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
                    {
                        cascade.push((kh, obj));
                    }
                }
                let next = page
                    .get("IndexNext")
                    .and_then(|v| {
                        v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                    })
                    .unwrap_or(0);
                if next == 0 {
                    break;
                }
                page_key = keylet::dir_page_key(&dir_root, next);
            }
            let hint_of = |o: &serde_json::Value, f: &str| -> Option<u64> {
                o.get(f).and_then(|v| {
                    v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s, 16).ok()))
                })
            };
            for (kh, obj) in &cascade {
                match obj.get("LedgerEntryType").and_then(|t| t.as_str()) {
                    Some("Offer") => {
                        crate::tx::offer::delete_maker_offer(sandbox, kh, obj, &tx.account);
                    }
                    Some("NFTokenOffer") => {
                        crate::ledger::directory::owner_dir_remove(
                            sandbox, &tx.account, kh, hint_of(obj, "OwnerNode"), true,
                        );
                        if let Some(nft_id) = obj.get("NFTokenID").and_then(|v| v.as_str()).and_then(|s| {
                            hex::decode(s).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
                        }) {
                            let nid = xrpl_core::types::Hash256(nft_id);
                            let tok_dir = if obj["Flags"].as_u64().unwrap_or(0) & 1 != 0 {
                                keylet::nft_sell_offers_key(&nid)
                            } else {
                                keylet::nft_buy_offers_key(&nid)
                            };
                            crate::ledger::directory::dir_remove(
                                sandbox, &tok_dir, kh, hint_of(obj, "NFTokenOfferNode"), false,
                            );
                        }
                        sandbox.delete(*kh);
                        if let Some(dh) = obj.get("Destination").and_then(|v| v.as_str()) {
                            if let Ok(db) = hex::decode(dh) {
                                if let Ok(did) = <[u8; 20]>::try_from(db.as_slice()) {
                                    let dk = keylet::account_root_key(&did);
                                    if let Some(dv) = sandbox.read(&dk) {
                                        sandbox.write(dk, dv);
                                    }
                                }
                            }
                        }
                    }
                    Some("Ticket") | Some("DepositPreauth") | Some("SignerList") => {
                        crate::ledger::directory::owner_dir_remove(
                            sandbox, &tx.account, kh, hint_of(obj, "OwnerNode"), true,
                        );
                        sandbox.delete(*kh);
                    }
                    _ => return TxResult::NoPermission,
                }
            }
        }

        // The owner DIRECTORY goes too. rippled walks it deleting each owned
        // object and then removes the directory ITSELF, before erasing the
        // account (AccountDelete.cpp):
        //     Keylet const ownerDirKeylet{keylet::ownerDir(accountID_)};
        //     …
        //     if (view().exists(ownerDirKeylet) && !view().emptyDirDelete(ownerDirKeylet))
        //     view().update(dst);
        //     view().erase(src);
        //
        // An account at OwnerCount 0 still OWNS an empty root page — the count
        // tracks reserved objects, not the directory that held them — and that
        // page is a ledger object mainnet removes. #106295546 4568277964F6 is
        // exactly that and nothing else: 3 nodes to our 2, the missing one
        // being the directory F2EAB202… whose RootIndex is itself.
        //
        // Guarded on emptiness because that is what `emptyDirDelete` means;
        // preclaim already requires OwnerCount == 0, so a non-empty directory
        // here would be a state we do not understand and must not silently
        // discard.
        let dir_key = keylet::owner_dir_key(&tx.account);
        if let Some(d) = sandbox.read(&dir_key) {
            let empty = serde_json::from_slice::<serde_json::Value>(&d)
                .ok()
                .map(|v| {
                    v.get("Indexes")
                        .and_then(|i| i.as_array())
                        .is_none_or(|a| a.is_empty())
                })
                .unwrap_or(false);
            if empty {
                sandbox.delete(dir_key);
            }
        }

        // Delete the account
        sandbox.delete(acct_key);

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
            "Flags": 0,
        });
        let key = keylet::account_root_key(id);
        state.state_map.insert(key, serde_json::to_vec(&acct).unwrap()).unwrap();
        state
    }

    #[test]
    fn account_set_flag() {
        let acct = [0x01u8; 20];
        let state = make_state(&acct, 50_000_000);

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: acct,
            tx_type: "AccountSet".to_string(),
            fee: 12,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"SetFlag": 8}), // asfDefaultRipple
        };

        assert_eq!(AccountSetTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        let key = keylet::account_root_key(&acct);
        let data = sandbox.read(&key).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        // asfDefaultRipple → lsfDefaultRipple 0x00800000, not 1 << 8.
        assert_eq!(v["Flags"].as_u64().unwrap() & 0x0080_0000, 0x0080_0000);
    }

    #[test]
    fn account_delete() {
        let alice = [0x01u8; 20];
        let bob = [0x02u8; 20];
        let mut state = make_state(&alice, 50_000_000);
        // Add bob
        let bob_acct = serde_json::json!({
            "LedgerEntryType": "AccountRoot",
            "Account": hex::encode(bob),
            "Balance": "10000000",
            "Sequence": 1,
            "OwnerCount": 0,
            "Flags": 0,
        });
        state.state_map.insert(keylet::account_root_key(&bob), serde_json::to_vec(&bob_acct).unwrap()).unwrap();

        let mut sandbox = Sandbox::new(&state);
        let tx = TxFields {
            account: alice,
            tx_type: "AccountDelete".to_string(),
            fee: 2_000_000,
            sequence: 1,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({"Destination": hex::encode(bob)}),
        };

        assert_eq!(AccountDeleteTransactor.do_apply(&tx, &mut sandbox), TxResult::Success);

        // Alice should be gone
        assert!(!sandbox.exists(&keylet::account_root_key(&alice)));

        // Bob should have Alice's balance
        let bob_data = sandbox.read(&keylet::account_root_key(&bob)).unwrap();
        let bv: serde_json::Value = serde_json::from_slice(&bob_data).unwrap();
        // Bob had 10M, Alice had 50M (fee already deducted by apply_common before do_apply)
        assert_eq!(bv["Balance"].as_str().unwrap(), "60000000");
    }
}
