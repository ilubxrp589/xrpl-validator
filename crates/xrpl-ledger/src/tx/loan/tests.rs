//! The four Loan transactors on hand-built ledgers: the result codes and
//! branches of LoanSet.cpp, LoanPay.cpp, LoanManage.cpp and LoanDelete.cpp
//! (3.4.1), the objects they leave, and the two custom base fees. The cases
//! are written from the transactors' source (each test cites its lines);
//! rippled's own ledger-level tests (src/test/app/lending/Loan*_test.cpp)
//! were not ported. Real devnet transactions are in
//! `crates/xrpl-node/tests/loan_vector.rs`.

use super::*;
use crate::ledger::header::LedgerHeader;
use crate::ledger::state::LedgerState;
use crate::tx::dispatch::apply_on_sandbox;
use crate::tx::number::{MantissaScale, ScaleGuard};
use crate::tx::st_number::number_from_str;
use serde_json::json;

/// Owns the vault and the broker.
const OWNER: [u8; 20] = [0x11; 20];
/// Borrows.
const BORROWER: [u8; 20] = [0x12; 20];
/// A third party.
const CAROL: [u8; 20] = [0x13; 20];
/// Issues USD and the MPT.
const ISSUER: [u8; 20] = [0x21; 20];
const USD_CODE: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x55, 0x53, 0x44, 0, 0, 0, 0, 0];
const USD: HeldAsset = HeldAsset::Iou { currency: USD_CODE, issuer: ISSUER };
const BALANCE: u64 = 1_000_000_000;
const FEE: u64 = 10;
const LSF_DEFAULT_RIPPLE: u64 = 0x0080_0000;
const LSF_GLOBAL_FREEZE: u64 = 0x0040_0000;
/// The parent ledger's close time in every test state; a loan made then
/// starts at NOW + 10 (the close-time resolution).
const NOW: u32 = 1000;
const START: u32 = NOW + 10;
const MONTH: u32 = 30 * 24 * 60 * 60;

/// Mainnet's rules today plus what a loan needs.
const MAINNET_WITH_LENDING: [&str; 10] = [
    am::FIX_CLEANUP_3_1_3, am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::SINGLE_ASSET_VAULT,
    am::LENDING_PROTOCOL, am::MPTOKENS_V1, am::PERMISSIONED_DOMAINS, am::CREDENTIALS, am::BATCH_V1_1,
];

fn rules_with(extra: &[&'static str]) -> Vec<&'static str> {
    let mut all = MAINNET_WITH_LENDING.to_vec();
    all.extend_from_slice(extra);
    all
}

fn rules_without(off: &[&str]) -> Vec<&'static str> {
    MAINNET_WITH_LENDING.iter().copied().filter(|a| !off.contains(a)).collect()
}

fn put(state: &mut LedgerState, key: Hash256, v: &Value) {
    state.state_map.insert(key, serde_json::to_vec(v).unwrap()).unwrap();
}

fn root_in(state: &mut LedgerState, id: [u8; 20], flags: u64) {
    let a = json!({
        "LedgerEntryType": "AccountRoot", "Account": hex::encode(id), "Balance": BALANCE.to_string(),
        "Sequence": 5, "OwnerCount": 0, "Flags": flags,
    });
    put(state, keylet::account_root_key(&id), &a);
}

fn set_rules(state: &mut LedgerState, amendments: &[&str]) {
    put(state, keylet::amendments_key(), &json!({"LedgerEntryType": "Amendments", "Flags": 0, "Amendments": amendments}));
}

fn state_with(amendments: &[&str]) -> LedgerState {
    let mut state = LedgerState::new_unverified(LedgerHeader {
        sequence: 100, total_coins: 100_000_000_000_000_000,
        parent_hash: Hash256([0x77; 32]), transaction_hash: Hash256([0; 32]), account_hash: Hash256([0; 32]),
        parent_close_time: NOW, close_time: NOW, close_time_resolution: 10, close_flags: 0,
    });
    set_rules(&mut state, amendments);
    root_in(&mut state, OWNER, 0);
    root_in(&mut state, BORROWER, 0);
    root_in(&mut state, CAROL, 0);
    root_in(&mut state, ISSUER, LSF_DEFAULT_RIPPLE);
    state
}

fn mptid(seq: u32, issuer: [u8; 20]) -> [u8; 24] {
    let mut id = [0u8; 24];
    id[..4].copy_from_slice(&seq.to_be_bytes());
    id[4..].copy_from_slice(&issuer);
    id
}

fn issuance(state: &mut LedgerState, flags: u64) -> [u8; 24] {
    let id = mptid(7, ISSUER);
    let o = json!({
        "LedgerEntryType": "MPTokenIssuance", "Issuer": hex::encode(ISSUER), "Flags": flags, "Sequence": 7,
        "OutstandingAmount": "0", "OwnerNode": "0",
    });
    put(state, keylet::mpt_issuance_key(&id), &o);
    id
}

fn token(state: &mut LedgerState, id: &[u8; 24], holder: [u8; 20], amount: u64) {
    let ikey = keylet::mpt_issuance_key(id);
    let t = json!({
        "LedgerEntryType": "MPToken", "Account": hex::encode(holder), "MPTokenIssuanceID": hex::encode_upper(id),
        "Flags": 0, "OwnerNode": "0", "MPTAmount": amount.to_string(),
    });
    let mut i: Value = serde_json::from_slice(&state.state_map.lookup(&ikey).unwrap()).unwrap();
    let outstanding = mpt::dec_field(&i, "OutstandingAmount") + amount;
    i["OutstandingAmount"] = json!(outstanding.to_string());
    put(state, ikey, &i);
    put(state, keylet::mptoken_key(&ikey, &holder), &t);
}

/// A USD line between `holder` and ISSUER, the holder holding `balance`
/// with its own limit `limit`.
fn line(state: &mut LedgerState, holder: [u8; 20], flags: u64, balance: &str, limit: &str) -> Hash256 {
    let key = keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE);
    let cur = hex::encode_upper(USD_CODE);
    let holder_low = holder < ISSUER;
    let (low, high) = if holder_low { (holder, ISSUER) } else { (ISSUER, holder) };
    let stored = if holder_low || balance == "0" { balance.to_string() } else { format!("-{balance}") };
    let (low_limit, high_limit) = if holder_low { (limit, "0") } else { ("0", limit) };
    put(state, key, &json!({
        "LedgerEntryType": "RippleState", "Flags": flags,
        "Balance": {"currency": cur, "issuer": "0000000000000000000000000000000000000001", "value": stored},
        "LowLimit": {"currency": cur, "issuer": hex::encode(low), "value": low_limit},
        "HighLimit": {"currency": cur, "issuer": hex::encode(high), "value": high_limit},
        "LowNode": "0", "HighNode": "0",
    }));
    key
}

fn usd_json(value: &str) -> Value {
    json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER), "value": value})
}

fn get(sb: &Sandbox, key: &Hash256) -> Option<Value> {
    json_at(sb, key)
}

fn owners(sb: &Sandbox, id: &[u8; 20]) -> u64 {
    root_of(sb, id).unwrap()["OwnerCount"].as_u64().unwrap()
}

fn xrp_of(sb: &Sandbox, id: &[u8; 20]) -> u64 {
    xrp_balance(&root_of(sb, id).unwrap())
}

fn dir_has(sb: &Sandbox, owner: &[u8; 20], key: &Hash256) -> bool {
    get(sb, &keylet::owner_dir_key(owner))
        .is_some_and(|d| d["Indexes"].as_array().is_some_and(|a| a.iter().any(|k| k == &json!(hex::encode_upper(key.0)))))
}

fn holds(sb: &Sandbox, holder: &[u8; 20], asset: &HeldAsset) -> Number {
    holding::account_holds(sb, holder, asset, false, false).number()
}

fn n(s: &str) -> Number {
    number_from_str(s).unwrap()
}

fn nf(obj: &Value, field: &str) -> Number {
    number_field(obj, field)
}

fn large() -> ScaleGuard {
    ScaleGuard::new(MantissaScale::Large330)
}

fn key_hex(key: &Hash256) -> String {
    hex::encode_upper(key.0)
}

fn tx(tx_type: &str, account: [u8; 20], fields: Value) -> TxFields {
    let mut all = json!({
        "TransactionType": tx_type, "Account": hex::encode(account), "Fee": FEE.to_string(), "Sequence": 5,
        "hash": "AB".repeat(32),
    });
    for (k, v) in fields.as_object().unwrap() {
        all[k] = v.clone();
    }
    TxFields::from_json(&all).unwrap()
}

fn apply<'a>(state: &'a LedgerState, tx: &TxFields) -> (TxResult, Sandbox<'a>) {
    let mut sb = Sandbox::new(state);
    let (result, _) = apply_on_sandbox(tx, &mut sb);
    (result, sb)
}

fn result(state: &LedgerState, tx: &TxFields) -> TxResult {
    apply(state, tx).0
}

/// Apply and keep: the state after a transaction that must succeed. The
/// account's Sequence is put back to 5 for the next one.
fn commit(state: &mut LedgerState, tx: &TxFields) {
    let (result, sb) = apply(state, tx);
    assert_eq!(result, TxResult::Success, "{} must apply", tx.tx_type);
    let mods = sb.into_modifications();
    crate::ledger::sandbox::apply_modifications(state, mods).unwrap();
    let key = keylet::account_root_key(&tx.account);
    let mut root: Value = serde_json::from_slice(&state.state_map.lookup(&key).unwrap()).unwrap();
    root["Sequence"] = json!(5);
    put(state, key, &root);
}

fn edit(state: &mut LedgerState, key: Hash256, change: impl FnOnce(&mut Value)) {
    let mut v: Value = serde_json::from_slice(&state.state_map.lookup(&key).unwrap()).unwrap();
    change(&mut v);
    put(state, key, &v);
}

fn stored(state: &LedgerState, key: &Hash256) -> Value {
    serde_json::from_slice(&state.state_map.lookup(key).unwrap()).unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Xrp,
    Usd,
    Mpt,
}

/// A vault of OWNER holding 10,000 units (XRP: 10,000,000 drops), a broker
/// of OWNER on it with 1,000 units of cover, and a BORROWER holding 1,000
/// units of the asset.
struct Fx {
    state: LedgerState,
    asset: HeldAsset,
    vault: Hash256,
    vault_pseudo: [u8; 20],
    broker: Hash256,
    broker_pseudo: [u8; 20],
    /// One unit of the asset as a transaction value (XRP: 1000 drops).
    unit: u64,
}

impl Fx {
    fn new(kind: Kind, rules: &[&str], broker_fields: Value) -> Fx {
        let mut state = state_with(rules);
        let (asset, asset_json, unit) = match kind {
            Kind::Xrp => (HeldAsset::Xrp, json!({"currency": "XRP"}), 1000),
            Kind::Usd => {
                line(&mut state, OWNER, 0, "100000", "1000000");
                line(&mut state, BORROWER, 0, "1000", "1000000");
                (USD, json!({"currency": hex::encode_upper(USD_CODE), "issuer": hex::encode(ISSUER)}), 1)
            }
            Kind::Mpt => {
                let id = issuance(&mut state, mpt::LSF_MPT_CAN_TRANSFER | mpt::LSF_MPT_CAN_CLAWBACK);
                token(&mut state, &id, OWNER, 100_000);
                token(&mut state, &id, BORROWER, 1000);
                (HeldAsset::Mpt(id), json!({"mpt_issuance_id": hex::encode_upper(id)}), 1)
            }
        };
        let vault = keylet::vault_key(&OWNER, 5);
        commit(&mut state, &tx("VaultCreate", OWNER, json!({"Asset": asset_json})));
        let vault_pseudo = account_of(&stored(&state, &vault), "Account").unwrap();
        let mut fx = Fx { state, asset, vault, vault_pseudo, broker: keylet::loan_broker_key(&OWNER, 5), broker_pseudo: [0; 20], unit };
        let deposit = fx.amount(10_000);
        commit(&mut fx.state, &tx("VaultDeposit", OWNER, json!({"VaultID": key_hex(&vault), "Amount": deposit})));
        let mut set = json!({"VaultID": key_hex(&vault)});
        for (k, v) in broker_fields.as_object().unwrap() {
            set[k] = v.clone();
        }
        commit(&mut fx.state, &tx("LoanBrokerSet", OWNER, set));
        fx.broker_pseudo = account_of(&stored(&fx.state, &fx.broker), "Account").unwrap();
        let cover = fx.amount(1000);
        commit(&mut fx.state, &tx("LoanBrokerCoverDeposit", OWNER, json!({"LoanBrokerID": key_hex(&fx.broker), "Amount": cover})));
        fx
    }

    fn xrp() -> Fx {
        Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({}))
    }

    fn usd() -> Fx {
        Fx::new(Kind::Usd, &MAINNET_WITH_LENDING, json!({}))
    }

    /// `units` of the asset as a transaction amount.
    fn amount(&self, units: u64) -> Value {
        match self.asset {
            HeldAsset::Xrp => json!((units * self.unit).to_string()),
            HeldAsset::Iou { .. } => usd_json(&units.to_string()),
            HeldAsset::Mpt(id) => json!({"mpt_issuance_id": hex::encode_upper(id), "value": units.to_string()}),
        }
    }

    /// `units` of the asset as an STNumber field's value.
    fn value(&self, units: u64) -> String {
        (units * self.unit).to_string()
    }

    /// A LoanSet by BORROWER (the broker's owner countersigning) for 1,000
    /// units over four payments ten minutes apart, plus `fields`.
    fn loan_set(&self, fields: Value) -> TxFields {
        self.loan_set_by(BORROWER, fields)
    }

    fn loan_set_by(&self, account: [u8; 20], fields: Value) -> TxFields {
        let mut all = json!({
            "LoanBrokerID": key_hex(&self.broker), "PrincipalRequested": self.value(1000), "PaymentTotal": 4, "PaymentInterval": 600,
            "CounterpartySignature": counter_signature(),
        });
        for (k, v) in fields.as_object().unwrap() {
            if v.is_null() {
                all.as_object_mut().unwrap().remove(k);
            } else {
                all[k] = v.clone();
            }
        }
        tx("LoanSet", account, all)
    }

    fn loan_key(&self) -> Hash256 {
        keylet::loan_key(&self.broker, 1)
    }

    /// The fixture with the loan of [`Fx::loan_set`] made.
    fn with_loan(mut self, fields: Value) -> Fx {
        let set = self.loan_set(fields);
        commit(&mut self.state, &set);
        self
    }

    fn pay(&self, units: u64, flags: u64) -> TxFields {
        self.pay_by(BORROWER, self.amount(units), flags)
    }

    fn pay_by(&self, account: [u8; 20], amount: Value, flags: u64) -> TxFields {
        tx("LoanPay", account, json!({"LoanID": key_hex(&self.loan_key()), "Amount": amount, "Flags": flags}))
    }

    fn manage(&self, flags: u64) -> TxFields {
        tx("LoanManage", OWNER, json!({"LoanID": key_hex(&self.loan_key()), "Flags": flags}))
    }

    fn delete(&self, account: [u8; 20]) -> TxFields {
        tx("LoanDelete", account, json!({"LoanID": key_hex(&self.loan_key())}))
    }

    /// Apply a transaction that must succeed and keep its changes.
    fn commit(&mut self, t: TxFields) {
        commit(&mut self.state, &t);
    }

    fn at(mut self, now: u32) -> Fx {
        self.state.header.close_time = now;
        self.state.header.parent_close_time = now;
        self
    }

    fn loan(&self, sb: &Sandbox) -> Value {
        get(sb, &self.loan_key()).unwrap()
    }

    fn vault_obj(&self, sb: &Sandbox) -> Value {
        get(sb, &self.vault).unwrap()
    }

    fn broker_obj(&self, sb: &Sandbox) -> Value {
        get(sb, &self.broker).unwrap()
    }
}

fn counter_signature() -> Value {
    json!({"SigningPubKey": format!("02{}", "11".repeat(32)), "TxnSignature": "3044"})
}

/// Only the fee and the sequence were written.
fn only_the_fee(sb: &Sandbox, account: &[u8; 20]) {
    assert_eq!(sb.modification_count(), 1, "only the submitter's root");
    assert_eq!(xrp_of(sb, account), xrp_of(&Sandbox::new(sb.base()), account) - FEE);
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// transactions.macro:963-1024 and each type's `checkExtraFeatures`
/// (LendingHelpers.cpp:69-82; Loan tests' "Disabled" cases): with
/// LendingProtocol, SingleAssetVault or MPTokensV1 off every Loan type is
/// temDISABLED and nothing is written — mainnet today.
#[test]
fn the_four_are_disabled_without_their_amendments() {
    let id = "AB".repeat(32);
    for off in [am::LENDING_PROTOCOL, am::SINGLE_ASSET_VAULT, am::MPTOKENS_V1] {
        let st = state_with(&rules_without(&[off]));
        for t in [
            tx("LoanSet", BORROWER, json!({"LoanBrokerID": id, "PrincipalRequested": "1000", "CounterpartySignature": counter_signature()})),
            tx("LoanDelete", BORROWER, json!({"LoanID": id})),
            tx("LoanManage", OWNER, json!({"LoanID": id})),
            tx("LoanPay", BORROWER, json!({"LoanID": id, "Amount": "1000"})),
            // Disabled wins over a malformed transaction.
            tx("LoanPay", BORROWER, json!({"LoanID": "00".repeat(32), "Amount": "0", "Flags": 1})),
            tx("LoanSet", BORROWER, json!({})),
        ] {
            let (r, sb) = apply(&st, &t);
            assert_eq!(r, TxResult::Disabled, "{} without {off}", t.tx_type);
            assert_eq!(sb.modification_count(), 0, "a tem writes nothing");
        }
    }
    // Mainnet's amendments alone (none of the three).
    let st = state_with(&[am::FIX_CLEANUP_3_1_3, am::FIX_CLEANUP_3_2_0, am::FIX_CLEANUP_3_3_0, am::FIX_CLEANUP_3_4_0, am::MPTOKENS_V1]);
    for kind in ["LoanSet", "LoanDelete", "LoanManage", "LoanPay"] {
        assert!(crate::tx::dispatch::is_supported(kind));
        let (r, sb) = apply(&st, &tx(kind, BORROWER, json!({"LoanID": id, "LoanBrokerID": id})));
        assert_eq!(r, TxResult::Disabled, "{kind}");
        assert_eq!(sb.modification_count(), 0);
    }
}

/// Transactor.cpp:231-250 through `lending_helpers::gate`: none of the four
/// is delegable; each has its own flag mask (TxFlags.h:209-226).
#[test]
fn delegation_and_flags() {
    let fx = Fx::xrp();
    let id = key_hex(&fx.loan_key());
    let delegate = hex::encode(CAROL);
    for (kind, fields) in [
        ("LoanSet", json!({"LoanBrokerID": id, "PrincipalRequested": "1000", "CounterpartySignature": counter_signature()})),
        ("LoanDelete", json!({"LoanID": id})),
        ("LoanManage", json!({"LoanID": id})),
        ("LoanPay", json!({"LoanID": id, "Amount": "1000"})),
    ] {
        let mut delegated = fields.clone();
        delegated["Delegate"] = json!(delegate);
        assert_eq!(result(&fx.state, &tx(kind, BORROWER, delegated)), TxResult::InvalidTx, "{kind} delegated");
        let mut flagged = fields.clone();
        flagged["Flags"] = json!(0x0008_0000u64);
        assert_eq!(result(&fx.state, &tx(kind, BORROWER, flagged)), TxResult::InvalidFlag, "{kind} with a flag of nobody's");
    }
    // tfLoanOverpayment is LoanSet's and LoanPay's; LoanDelete has none.
    assert_eq!(result(&fx.state, &tx("LoanDelete", BORROWER, json!({"LoanID": id, "Flags": TF_LOAN_OVERPAYMENT}))), TxResult::InvalidFlag);
    assert_ne!(result(&fx.state, &fx.loan_set(json!({"Flags": TF_LOAN_OVERPAYMENT}))), TxResult::InvalidFlag);
    assert_eq!(result(&fx.state, &fx.loan_set(json!({"Flags": TF_LOAN_FULL_PAYMENT}))), TxResult::InvalidFlag);
}

// ---------------------------------------------------------------------------
// LoanSet
// ---------------------------------------------------------------------------

/// LoanSet.cpp:61-153.
#[test]
fn loan_set_preflight() {
    let fx = Fx::xrp();
    let refused = |fields: Value, want: TxResult, why: &str| {
        let (r, sb) = apply(&fx.state, &fx.loan_set(fields));
        assert_eq!(r, want, "{why}");
        assert_eq!(sb.modification_count(), 0, "{why}: a tem charges nothing");
    };
    let passes = |fields: Value, why: &str| {
        let r = result(&fx.state, &fx.loan_set(fields));
        assert!(!r.code_str().starts_with("tem"), "{why}: {}", r.code_str());
    };
    // The counterparty's signature.
    refused(json!({"CounterpartySignature": null}), TxResult::BadSigner, "no sfCounterpartySignature");
    refused(json!({"CounterpartySignature": {"SigningPubKey": "0211", "TxnSignature": "00"}}), TxResult::BadSignature, "a short key");
    refused(
        json!({"CounterpartySignature": {"SigningPubKey": format!("05{}", "11".repeat(32)), "TxnSignature": "00"}}),
        TxResult::BadSignature,
        "not a key type",
    );
    passes(json!({"CounterpartySignature": {"SigningPubKey": format!("ED{}", "11".repeat(32)), "TxnSignature": "00"}}), "an ed25519 key");
    passes(json!({"CounterpartySignature": {"SigningPubKey": "", "Signers": []}}), "an empty key (multi-signed)");
    // A Batch inner needs no signature of its own, but under BatchV1_1 it must name the counterparty.
    refused(json!({"Flags": TF_INNER_BATCH_TXN, "CounterpartySignature": null}), TxResult::BadSigner, "an inner without sfCounterparty");
    passes(json!({"Flags": TF_INNER_BATCH_TXN, "CounterpartySignature": null, "Counterparty": hex::encode(OWNER)}), "an inner naming it");
    assert_eq!(
        LoanSetTransactor::preflight_rules(&fx.loan_set(json!({"Flags": TF_INNER_BATCH_TXN, "CounterpartySignature": null})), false),
        TxResult::Success,
        "without BatchV1_1 the inner rule is off"
    );
    // sfData.
    refused(json!({"Data": "AB".repeat(257)}), TxResult::InvalidTx, "sfData over 256 bytes");
    passes(json!({"Data": "AB".repeat(256)}), "sfData of 256 bytes");
    passes(json!({"Data": ""}), "an empty sfData");
    // The fees are not negative.
    for field in ["LoanServiceFee", "LatePaymentFee", "ClosePaymentFee", "LoanOriginationFee"] {
        let mut fields = json!({});
        fields[field] = json!("-1");
        refused(fields, TxResult::InvalidTx, field);
    }
    // The principal is positive; the origination fee at most the principal.
    refused(json!({"PrincipalRequested": "0"}), TxResult::InvalidTx, "a zero principal");
    refused(json!({"PrincipalRequested": "-5"}), TxResult::InvalidTx, "a negative principal");
    refused(json!({"PrincipalRequested": null}), TxResult::InvalidTx, "no principal");
    refused(json!({"LoanOriginationFee": "1000001"}), TxResult::InvalidTx, "an origination fee above the principal");
    passes(json!({"LoanOriginationFee": "1000000"}), "an origination fee of the whole principal");
    // The rates are at most 100%.
    for field in ["InterestRate", "OverpaymentFee", "LateInterestRate", "CloseInterestRate", "OverpaymentInterestRate"] {
        let mut fields = json!({});
        fields[field] = json!(100_001);
        refused(fields.clone(), TxResult::InvalidTx, field);
        fields[field] = json!(100_000);
        passes(fields, field);
    }
    // The schedule.
    refused(json!({"PaymentTotal": 0}), TxResult::InvalidTx, "no payments");
    refused(json!({"PaymentInterval": 59}), TxResult::InvalidTx, "an interval under a minute");
    passes(json!({"PaymentInterval": 60}), "an interval of a minute");
    refused(json!({"GracePeriod": 59}), TxResult::InvalidTx, "a grace period under a minute");
    refused(json!({"GracePeriod": 601}), TxResult::InvalidTx, "a grace period longer than the interval");
    passes(json!({"GracePeriod": 600}), "a grace period of the interval");
    refused(json!({"PaymentInterval": null, "GracePeriod": 61}), TxResult::InvalidTx, "a grace period above the default interval");
    refused(json!({"LoanBrokerID": "00".repeat(32)}), TxResult::InvalidTx, "a zero sfLoanBrokerID");
}

/// LoanSet.cpp:155-173 (checkSign's lookup) and :231-317: the broker, the
/// two parties, the borrower's account, the clock.
#[test]
fn loan_set_preclaim_parties_and_time() {
    let fx = Fx::xrp();
    let nowhere = key_hex(&Hash256([0xAB; 32]));
    // Nobody to countersign: temBAD_SIGNER, nothing charged.
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({"LoanBrokerID": nowhere})));
    assert_eq!(r, TxResult::BadSigner);
    assert_eq!(sb.modification_count(), 0);
    // A named counterparty and no broker: tecNO_ENTRY.
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({"LoanBrokerID": nowhere, "Counterparty": hex::encode(OWNER)})));
    assert_eq!(r, TxResult::NoEntry);
    only_the_fee(&sb, &BORROWER);
    // Neither party owns the broker.
    assert_eq!(result(&fx.state, &fx.loan_set(json!({"Counterparty": hex::encode(CAROL)}))), TxResult::NoPermission);
    // The owner submits and names a borrower without an account.
    assert_eq!(result(&fx.state, &fx.loan_set_by(OWNER, json!({"Counterparty": hex::encode([0x77u8; 20])}))), TxResult::NoAccount);
    // The owner submits and names the borrower; the borrower submits and names the owner; the owner borrows from itself.
    assert_eq!(result(&fx.state, &fx.loan_set_by(OWNER, json!({"Counterparty": hex::encode(BORROWER)}))), TxResult::Success);
    assert_eq!(result(&fx.state, &fx.loan_set(json!({"Counterparty": hex::encode(OWNER)}))), TxResult::Success);
    assert_eq!(result(&fx.state, &fx.loan_set_by(OWNER, json!({}))), TxResult::Success);
    // Due dates past the 32-bit clock: tecKILLED.
    let late = Fx::xrp().at(u32::MAX - 10 - 600);
    assert_eq!(result(&late.state, &late.loan_set(json!({"PaymentTotal": 1}))), TxResult::Killed, "the grace period does not fit");
    let late = Fx::xrp().at(u32::MAX - 10 - 3000);
    assert_eq!(result(&late.state, &late.loan_set(json!({"PaymentTotal": 5}))), TxResult::Killed, "five payments do not fit");
    assert_eq!(result(&late.state, &late.loan_set(json!({"PaymentTotal": 4}))), TxResult::Success, "four do");
    let late = Fx::xrp().at(u32::MAX - 10 - 500);
    assert_eq!(result(&late.state, &late.loan_set(json!({}))), TxResult::Killed, "the interval does not fit");
}

/// LoanSet.cpp:319-344: under LendingProtocolV1_1 a closed-ended vault
/// lends only in its Investment phase, and only loans whose last payment
/// falls due kLoanRedemptionBuffer (60 s) before the redemption date.
#[test]
fn loan_set_vault_phases() {
    let closed = |subscription: u32, redemption: u32, rules: &[&str]| {
        let mut fx = Fx::xrp();
        set_rules(&mut fx.state, rules);
        edit(&mut fx.state, fx.vault, |v| {
            v["VaultKind"] = json!(1);
            v["SubscriptionDate"] = json!(subscription);
            v["RedemptionDate"] = json!(redemption);
        });
        fx
    };
    let v1_1 = rules_with(&[am::LENDING_PROTOCOL_V1_1]);
    let r = |fx: &Fx| result(&fx.state, &fx.loan_set(json!({})));
    assert_eq!(r(&closed(NOW, 100_000, &v1_1)), TxResult::TooSoon, "the subscription date itself still subscribes");
    assert_eq!(r(&closed(NOW - 1, NOW, &v1_1)), TxResult::Expired, "redemption has begun");
    // Four payments of 600 s from START end at START + 2400; plus the buffer.
    let last = START + 2400 + 60;
    assert_eq!(r(&closed(NOW - 1, last, &v1_1)), TxResult::Success, "the last payment exactly a buffer ahead");
    assert_eq!(r(&closed(NOW - 1, last - 1, &v1_1)), TxResult::NoPermission, "one second short");
    // Without the amendment the phases are not looked at.
    assert_eq!(r(&closed(NOW, 100_000, &MAINNET_WITH_LENDING)), TxResult::Success);
    assert_eq!(r(&closed(NOW - 1, NOW, &MAINNET_WITH_LENDING)), TxResult::Success);
}

/// LoanSet.cpp:346-428: the vault's maximum, the value fields in the asset,
/// holdings and freezes.
#[test]
fn loan_set_preclaim_vault_and_asset() {
    // An accrual vault at its maximum; a cash-basis one does not care.
    let mut fx = Fx::xrp();
    edit(&mut fx.state, fx.vault, |v| v["AssetsMaximum"] = json!("10000000"));
    assert_eq!(result(&fx.state, &fx.loan_set(json!({}))), TxResult::LimitExceeded);
    edit(&mut fx.state, fx.vault, |v| v["LEVersion"] = json!(1));
    assert_eq!(result(&fx.state, &fx.loan_set(json!({}))), TxResult::Success);

    // A fraction of a drop.
    let fx = Fx::xrp();
    for field in VALUE_FIELDS {
        let mut fields = json!({});
        fields[field] = json!("100.5");
        assert_eq!(result(&fx.state, &fx.loan_set(fields)), TxResult::PrecisionLoss, "{field}");
    }
    // More XRP than exists: 3.4.1 throws, fixCleanup3_5_0 refuses.
    let huge = json!({"LatePaymentFee": "200000000000000000"});
    let (r, sb) = apply(&fx.state, &fx.loan_set(huge.clone()));
    assert_eq!(r, TxResult::TefException);
    assert_eq!(sb.modification_count(), 0);
    let mut fixed = Fx::xrp();
    set_rules(&mut fixed.state, &rules_with(&[am::FIX_CLEANUP_3_5_0]));
    assert_eq!(result(&fixed.state, &fixed.loan_set(huge)), TxResult::PrecisionLoss);
    // Seventeen digits of an IOU.
    let usd = Fx::usd();
    assert_eq!(result(&usd.state, &usd.loan_set(json!({"PrincipalRequested": "1000.0000000000001"}))), TxResult::PrecisionLoss);

    // A frozen asset: the vault's or the borrower's side tecFROZEN.
    let mut frozen = Fx::usd();
    edit(&mut frozen.state, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE));
    assert_eq!(result(&frozen.state, &frozen.loan_set(json!({}))), TxResult::Frozen);

    // An MPT that cannot be transferred cannot be given to a NEW holder:
    // refused when the borrower has no MPToken (and always before
    // fixCleanup3_4_0), not looked at when everybody already holds it.
    let untransferable = |rules: &[&str], borrower_holds: bool| {
        let mut fx = Fx::new(Kind::Mpt, &MAINNET_WITH_LENDING, json!({}));
        set_rules(&mut fx.state, rules);
        let HeldAsset::Mpt(id) = fx.asset else { unreachable!() };
        edit(&mut fx.state, keylet::mpt_issuance_key(&id), |i| i["Flags"] = json!(mpt::LSF_MPT_CAN_CLAWBACK));
        if !borrower_holds {
            fx.state.state_map.delete(&keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &BORROWER)).unwrap();
        }
        let r = result(&fx.state, &fx.loan_set(json!({})));
        r
    };
    assert_eq!(untransferable(&MAINNET_WITH_LENDING, false), TxResult::NoAuth);
    assert_ne!(untransferable(&MAINNET_WITH_LENDING, true), TxResult::NoAuth);
    assert_eq!(untransferable(&rules_without(&[am::FIX_CLEANUP_3_4_0]), true), TxResult::NoAuth);
}

/// LoanSet.cpp:430-725, an interest-free loan of XRP: every object it
/// writes.
#[test]
fn loan_set_creates_the_loan() {
    let fx = Fx::xrp();
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({})));
    assert_eq!(r, TxResult::Success);
    let key = fx.loan_key();
    let loan = fx.loan(&sb);
    assert_eq!(
        loan,
        json!({
            "LedgerEntryType": "Loan", "Flags": 0, "LoanBrokerID": key_hex(&fx.broker), "LoanSequence": 1,
            "Borrower": hex::encode(BORROWER), "OwnerNode": "0", "LoanBrokerNode": "0",
            "StartDate": START, "PaymentInterval": 600, "GracePeriod": 60, "NextPaymentDueDate": START + 600, "PaymentRemaining": 4,
            "PeriodicPayment": "250000", "PrincipalOutstanding": "1000000", "TotalValueOutstanding": "1000000",
        }),
        "defaults (rates, fees, a zero scale, no previous due date) are absent"
    );
    // In both directories; the borrower pays the reserve; the broker counts it.
    assert!(dir_has(&sb, &fx.broker_pseudo, &key) && dir_has(&sb, &BORROWER, &key));
    assert_eq!(owners(&sb, &BORROWER), 1);
    assert_eq!(owners(&sb, &fx.broker_pseudo), 0, "the pseudo-account's own count does not move");
    let broker = fx.broker_obj(&sb);
    assert_eq!((broker["OwnerCount"].as_u64(), broker["LoanSequence"].as_u64()), (Some(1), Some(2)));
    assert_eq!(nf(&broker, "DebtTotal"), n("1000000"));
    assert_eq!(nf(&broker, "CoverAvailable"), n("1000000"));
    // The vault lent a tenth of what it has.
    let vault = fx.vault_obj(&sb);
    assert_eq!(nf(&vault, "AssetsAvailable"), n("9000000"));
    assert_eq!(nf(&vault, "AssetsTotal"), n("10000000"));
    assert_eq!(xrp_of(&sb, &fx.vault_pseudo), 9_000_000);
    assert_eq!(xrp_of(&sb, &BORROWER), BALANCE - FEE + 1_000_000);
    // The borrower's root, the vault pseudo-account's root, the Vault, the
    // LoanBroker, the Loan and the two directories (the borrower's is new).
    assert_eq!(sb.modification_count(), 7);
}

/// LoanSet.cpp:546-548, :640-648, :660-691: fees, rates, flags and the
/// origination fee, which the broker's owner receives out of the principal.
#[test]
fn loan_set_terms_and_origination_fee() {
    let fx = Fx::usd();
    let terms = json!({
        "Flags": TF_LOAN_OVERPAYMENT, "LoanOriginationFee": "10", "LoanServiceFee": "1", "LatePaymentFee": "2", "ClosePaymentFee": "3",
        "OverpaymentFee": 100, "InterestRate": 10_000, "LateInterestRate": 20_000, "CloseInterestRate": 30_000,
        "OverpaymentInterestRate": 40_000, "GracePeriod": 120, "PaymentInterval": MONTH, "PaymentTotal": 12,
    });
    let (r, sb) = apply(&fx.state, &fx.loan_set(terms));
    assert_eq!(r, TxResult::Success);
    let loan = fx.loan(&sb);
    assert_eq!(loan["Flags"], json!(LSF_LOAN_OVERPAYMENT));
    for (field, want) in [("LoanOriginationFee", "10"), ("LoanServiceFee", "1"), ("LatePaymentFee", "2"), ("ClosePaymentFee", "3")] {
        assert_eq!(loan[field], json!(want), "{field}");
    }
    for (field, want) in [
        ("OverpaymentFee", 100), ("InterestRate", 10_000), ("LateInterestRate", 20_000), ("CloseInterestRate", 30_000),
        ("OverpaymentInterestRate", 40_000), ("GracePeriod", 120), ("PaymentInterval", MONTH), ("PaymentRemaining", 12),
    ] {
        assert_eq!(loan[field], json!(want), "{field}");
    }
    // The vault's total is 10000 (sixteen digits: exponent -11), the loan's value about 1054 (-12): the loan scale is the vault's.
    assert_eq!(loan["LoanScale"], json!(-11));
    let _g = large();
    let value = nf(&loan, "TotalValueOutstanding");
    assert!(value > n("1054") && value < n("1056"), "about 5.5% of interest over a year of monthly payments");
    assert_eq!(nf(&loan, "PrincipalOutstanding"), n("1000"));
    assert!(loan.get("ManagementFeeOutstanding").is_none(), "the broker charges no management fee");
    // Accrual: the interest enters the vault's total and the broker's debt at origination.
    let interest = value.sub(n("1000"), AMBIENT).unwrap();
    let (vault, broker) = (fx.vault_obj(&sb), fx.broker_obj(&sb));
    assert_eq!(nf(&vault, "AssetsAvailable"), n("9000"));
    assert_eq!(nf(&vault, "AssetsTotal"), n("10000").add(interest, AMBIENT).unwrap());
    assert_eq!(nf(&broker, "DebtTotal"), value);
    // 990 to the borrower, 10 to the owner.
    assert_eq!(holds(&sb, &BORROWER, &USD), n("1990"));
    assert_eq!(holds(&sb, &OWNER, &USD), n("89010"), "100000 less the deposit and the cover, plus the fee");
    assert_eq!(holds(&sb, &fx.vault_pseudo, &USD), n("9000"));
}

/// LendingHelpers.cpp:222-247 through LoanSet.cpp:550-551, :695-696: a
/// cash-basis vault books the principal only.
#[test]
fn loan_set_on_a_cash_basis_vault() {
    let mut fx = Fx::usd();
    edit(&mut fx.state, fx.vault, |v| v["LEVersion"] = json!(1));
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({"InterestRate": 10_000, "PaymentInterval": MONTH, "PaymentTotal": 12})));
    assert_eq!(r, TxResult::Success);
    let _g = large();
    assert!(nf(&fx.loan(&sb), "TotalValueOutstanding") > n("1054"));
    assert_eq!(nf(&fx.vault_obj(&sb), "AssetsTotal"), n("10000"));
    assert_eq!(nf(&fx.vault_obj(&sb), "AssetsAvailable"), n("9000"));
    assert_eq!(nf(&fx.broker_obj(&sb), "DebtTotal"), n("1000"));
}

/// LoanSet.cpp:471-475, :503-508, :511-519, :524-531, :550-578: what the
/// vault, the broker and the loan's own arithmetic refuse.
#[test]
fn loan_set_limits() {
    let fx = Fx::xrp();
    // More than the vault has on hand.
    assert_eq!(result(&fx.state, &fx.loan_set(json!({"PrincipalRequested": "10000001"}))), TxResult::InsufficientFunds);
    // A value field finer than the loan's scale. (A USD vault of 10000 has scale -11.)
    let usd = Fx::usd();
    assert_eq!(result(&usd.state, &usd.loan_set(json!({"LoanServiceFee": "0.000000000001"}))), TxResult::PrecisionLoss);
    assert_eq!(result(&usd.state, &usd.loan_set(json!({"LoanServiceFee": "0.00000000001"}))), TxResult::Success);
    // checkLoanGuards: ten drops over six payments cannot be scheduled (the
    // payment rounds up to 2, which pays ten off in five).
    assert_eq!(result(&fx.state, &fx.loan_set(json!({"PrincipalRequested": "10", "PaymentTotal": 6}))), TxResult::PrecisionLoss);
    assert_eq!(result(&fx.state, &fx.loan_set(json!({"PrincipalRequested": "10", "PaymentTotal": 5}))), TxResult::Success);
    // Interest past an accrual vault's maximum.
    let mut capped = Fx::usd();
    edit(&mut capped.state, capped.vault, |v| v["AssetsMaximum"] = json!("10050"));
    let with_interest = json!({"InterestRate": 10_000, "PaymentInterval": MONTH, "PaymentTotal": 12});
    assert_eq!(result(&capped.state, &capped.loan_set(with_interest.clone())), TxResult::LimitExceeded);
    edit(&mut capped.state, capped.vault, |v| v["AssetsMaximum"] = json!("10060"));
    assert_eq!(result(&capped.state, &capped.loan_set(with_interest)), TxResult::Success);
    // The broker's debt maximum.
    let limited = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({"DebtMaximum": "999999"}));
    assert_eq!(result(&limited.state, &limited.loan_set(json!({}))), TxResult::LimitExceeded);
    let limited = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({"DebtMaximum": "1000000"}));
    assert_eq!(result(&limited.state, &limited.loan_set(json!({}))), TxResult::Success);
    // The cover: 10% of the new debt must be there. 1000 units of cover carry 10000 of debt.
    let covered = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({"CoverRateMinimum": 10_000, "CoverRateLiquidation": 10_000}));
    assert_eq!(result(&covered.state, &covered.loan_set(json!({"PrincipalRequested": "10000000"}))), TxResult::Success);
    let thin = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({"CoverRateMinimum": 100_000, "CoverRateLiquidation": 10_000}));
    assert_eq!(result(&thin.state, &thin.loan_set(json!({"PrincipalRequested": "1000001"}))), TxResult::InsufficientFunds);
    assert_eq!(result(&thin.state, &thin.loan_set(json!({"PrincipalRequested": "1000000"}))), TxResult::Success);
}

/// LoanSet.cpp:580-587: the borrower's reserve for the Loan — against the
/// balance before the fee when the borrower submits, the current balance
/// when the owner does.
#[test]
fn loan_set_reserve() {
    // One owned object: the reserve is 1,200,000 drops.
    let poor = |balance: u64| {
        let mut fx = Fx::xrp();
        edit(&mut fx.state, keylet::account_root_key(&BORROWER), |a| a["Balance"] = json!(balance.to_string()));
        fx
    };
    let fx = poor(1_199_999);
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({})));
    assert_eq!(r, TxResult::InsufficientReserve);
    assert_eq!(sb.modification_count(), 1, "a tec keeps only the fee");
    assert_eq!(owners(&sb, &BORROWER), 0);
    let fx = poor(1_200_000);
    assert_eq!(result(&fx.state, &fx.loan_set(json!({}))), TxResult::Success, "the fee is not counted against the submitter");
    assert_eq!(result(&fx.state, &fx.loan_set_by(OWNER, json!({"Counterparty": hex::encode(BORROWER)}))), TxResult::Success);
    let fx = poor(1_199_999);
    assert_eq!(result(&fx.state, &fx.loan_set_by(OWNER, json!({"Counterparty": hex::encode(BORROWER)}))), TxResult::InsufficientReserve);
}

/// LoanSet.cpp:600-637: the borrower's holding is created (an MPToken; a
/// trust line), and an unauthorized borrower is refused.
#[test]
fn loan_set_creates_the_borrowers_holding() {
    let mut fx = Fx::new(Kind::Mpt, &MAINNET_WITH_LENDING, json!({}));
    let HeldAsset::Mpt(id) = fx.asset else { unreachable!() };
    let token_key = keylet::mptoken_key(&keylet::mpt_issuance_key(&id), &BORROWER);
    fx.state.state_map.delete(&token_key).unwrap();
    edit(&mut fx.state, keylet::mpt_issuance_key(&id), |i| i["OutstandingAmount"] = json!("100000"));
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({})));
    assert_eq!(r, TxResult::Success);
    assert_eq!(get(&sb, &token_key).unwrap()["MPTAmount"], json!("1000"));
    assert_eq!(owners(&sb, &BORROWER), 2, "the Loan and the MPToken");
    // A trust line; without the reserve for it tecNO_LINE_INSUF_RESERVE
    // (devnet DAA7FA860800 @ 7129495).
    let mut usd = Fx::usd();
    let line_key = keylet::ripple_state_key(&BORROWER, &ISSUER, &USD_CODE);
    usd.state.state_map.delete(&line_key).unwrap();
    let (r, sb) = apply(&usd.state, &usd.loan_set(json!({})));
    assert_eq!(r, TxResult::Success);
    assert!(get(&sb, &line_key).is_some());
    assert_eq!(holds(&sb, &BORROWER, &USD), n("1000"));
    assert_eq!(owners(&sb, &BORROWER), 2);
    // With two objects already, the Loan needs the reserve for three (1,600,000) and the line for four (1,800,000).
    edit(&mut usd.state, keylet::account_root_key(&BORROWER), |a| {
        a["Balance"] = json!("1700000");
        a["OwnerCount"] = json!(2);
    });
    let (r, sb) = apply(&usd.state, &usd.loan_set(json!({})));
    assert_eq!(r, TxResult::NoLineInsufReserve);
    only_the_fee(&sb, &BORROWER);
}

/// LoanSet.cpp:706-710: the broker's loan sequence must not wrap.
#[test]
fn loan_set_at_the_last_loan_sequence() {
    let mut fx = Fx::xrp();
    edit(&mut fx.state, fx.broker, |b| b["LoanSequence"] = json!(u32::MAX));
    let (r, sb) = apply(&fx.state, &fx.loan_set(json!({})));
    assert_eq!(r, TxResult::MaxSequenceReached);
    only_the_fee(&sb, &BORROWER);
}

// ---------------------------------------------------------------------------
// LoanPay
// ---------------------------------------------------------------------------

/// LoanPay.cpp:87-110.
#[test]
fn loan_pay_preflight() {
    let fx = Fx::xrp().with_loan(json!({}));
    let id = key_hex(&fx.loan_key());
    let r = |fields: Value| result(&fx.state, &tx("LoanPay", BORROWER, fields));
    assert_eq!(r(json!({"LoanID": "00".repeat(32), "Amount": "1"})), TxResult::InvalidTx);
    assert_eq!(r(json!({"LoanID": id, "Amount": "0"})), TxResult::BadAmount);
    assert_eq!(r(json!({"LoanID": id, "Amount": "-1"})), TxResult::BadAmount);
    assert_eq!(r(json!({"LoanID": id})), TxResult::BadAmount);
    for two in [TF_LOAN_LATE_PAYMENT | TF_LOAN_FULL_PAYMENT, TF_LOAN_OVERPAYMENT | TF_LOAN_FULL_PAYMENT, TF_LOAN_OVERPAYMENT | TF_LOAN_LATE_PAYMENT] {
        assert_eq!(r(json!({"LoanID": id, "Amount": "1", "Flags": two})), TxResult::InvalidFlag);
    }
}

/// LoanPay.cpp:228-329 (devnet has tecWRONG_ASSET, tecFROZEN, tecEXPIRED
/// and tecINSUFFICIENT_PAYMENT examples; the last is a vector).
#[test]
fn loan_pay_preclaim() {
    let fx = Fx::xrp().with_loan(json!({}));
    let nowhere = tx("LoanPay", BORROWER, json!({"LoanID": "AB".repeat(32), "Amount": "1"}));
    let (r, sb) = apply(&fx.state, &nowhere);
    assert_eq!(r, TxResult::NoEntry);
    only_the_fee(&sb, &BORROWER);
    assert_eq!(result(&fx.state, &fx.pay_by(CAROL, json!("250000"), 0)), TxResult::NoPermission, "only the borrower pays");
    // An overpayment on a loan that takes none: tecNO_PERMISSION, temINVALID_FLAG before fixCleanup3_1_3.
    assert_eq!(result(&fx.state, &fx.pay(250, TF_LOAN_OVERPAYMENT)), TxResult::NoPermission);
    let mut old = Fx::xrp().with_loan(json!({}));
    set_rules(&mut old.state, &rules_without(&[am::FIX_CLEANUP_3_1_3]));
    let (r, sb) = apply(&old.state, &old.pay(250, TF_LOAN_OVERPAYMENT));
    assert_eq!(r, TxResult::InvalidFlag);
    assert_eq!(sb.modification_count(), 0);
    // Another asset.
    assert_eq!(result(&fx.state, &fx.pay_by(BORROWER, usd_json("250"), 0)), TxResult::WrongAsset);
    // More than the borrower can spend (XRP: above the reserve).
    assert_eq!(result(&fx.state, &fx.pay_by(BORROWER, json!((BALANCE + 2_000_000).to_string()), 0)), TxResult::InsufficientFunds);
    // A paid-off loan.
    let mut done = Fx::xrp().with_loan(json!({}));
    done.commit(done.pay(1000, 0));
    assert_eq!(result(&done.state, &done.pay(250, 0)), TxResult::Killed);
    // A frozen borrower; a borrower without the asset.
    let mut frozen = Fx::usd().with_loan(json!({}));
    edit(&mut frozen.state, keylet::account_root_key(&ISSUER), |a| a["Flags"] = json!(LSF_DEFAULT_RIPPLE | LSF_GLOBAL_FREEZE));
    assert_eq!(result(&frozen.state, &frozen.pay(250, 0)), TxResult::Frozen);
    let mut broke = Fx::usd().with_loan(json!({}));
    edit(&mut broke.state, keylet::ripple_state_key(&BORROWER, &ISSUER, &USD_CODE), |l| {
        l["Balance"]["value"] = json!(if BORROWER < ISSUER { "100" } else { "-100" })
    });
    assert_eq!(result(&broke.state, &broke.pay(250, 0)), TxResult::InsufficientFunds);
}

/// LoanPay.cpp:331-848 on an interest-free XRP loan with a service fee:
/// one scheduled payment, then the rest, then nothing left
/// (devnet: the three vectors on loan 5FEE8B895C).
#[test]
fn loan_pay_regular_payments_to_payoff() {
    let mut fx = Fx::xrp().with_loan(json!({"LoanServiceFee": "100"}));
    // One period: 250,000 of principal and the 100-drop service fee.
    let short = fx.pay_by(BORROWER, json!("250099"), 0);
    let (r, sb) = apply(&fx.state, &short);
    assert_eq!(r, TxResult::InsufficientPayment);
    only_the_fee(&sb, &BORROWER);
    // 250,150: one payment; the 50 extra are not taken.
    let (r, sb) = apply(&fx.state, &fx.pay_by(BORROWER, json!("250150"), 0));
    assert_eq!(r, TxResult::Success);
    let loan = fx.loan(&sb);
    assert_eq!((loan["PaymentRemaining"].as_u64(), loan["PreviousPaymentDueDate"].as_u64(), loan["NextPaymentDueDate"].as_u64()),
        (Some(3), Some(START as u64 + 600), Some(START as u64 + 1200)));
    assert_eq!(nf(&loan, "PrincipalOutstanding"), n("750000"));
    assert_eq!(nf(&loan, "TotalValueOutstanding"), n("750000"));
    let (vault, broker) = (fx.vault_obj(&sb), fx.broker_obj(&sb));
    assert_eq!(nf(&vault, "AssetsAvailable"), n("9250000"));
    assert_eq!(nf(&vault, "AssetsTotal"), n("10000000"));
    assert_eq!(nf(&broker, "DebtTotal"), n("750000"));
    assert_eq!(nf(&broker, "CoverAvailable"), n("1000000"), "the fee went to the owner");
    assert_eq!(xrp_of(&sb, &fx.vault_pseudo), 9_250_000);
    assert_eq!(xrp_of(&sb, &OWNER), xrp_of(&Sandbox::new(&fx.state), &OWNER) + 100);
    assert_eq!(xrp_of(&sb, &BORROWER), xrp_of(&Sandbox::new(&fx.state), &BORROWER) - FEE - 250_100);
    fx.commit(fx.pay_by(BORROWER, json!("250150"), 0));
    // The other three at once.
    let (r, sb) = apply(&fx.state, &fx.pay(1000, 0));
    assert_eq!(r, TxResult::Success);
    let loan = fx.loan(&sb);
    for gone in ["PaymentRemaining", "NextPaymentDueDate", "PrincipalOutstanding", "TotalValueOutstanding", "ManagementFeeOutstanding"] {
        assert!(loan.get(gone).is_none(), "{gone} is cleared");
    }
    assert_eq!(loan["PreviousPaymentDueDate"], json!(START + 2400));
    assert_eq!(nf(&fx.vault_obj(&sb), "AssetsAvailable"), n("10000000"));
    assert!(fx.broker_obj(&sb).get("DebtTotal").is_none());
    assert_eq!(fx.broker_obj(&sb)["OwnerCount"], json!(1), "the loan is still there until LoanDelete");
    assert_eq!(xrp_of(&sb, &BORROWER), xrp_of(&Sandbox::new(&fx.state), &BORROWER) - FEE - 750_300);
}

/// LoanPay.cpp:373-406, :561-564: while the cover is below its minimum (or
/// the owner cannot receive the asset) the fee tops the cover up instead.
#[test]
fn loan_pay_fee_goes_to_the_cover_when_it_is_short() {
    let mut fx = Fx::xrp().with_loan(json!({"LoanServiceFee": "100"}));
    // 100% of 1,000,000 of debt is needed; exactly that is there: the owner is paid.
    edit(&mut fx.state, fx.broker, |b| {
        b["CoverRateMinimum"] = json!(100_000);
        b["CoverRateLiquidation"] = json!(100_000);
    });
    let owner_before = xrp_of(&Sandbox::new(&fx.state), &OWNER);
    let (_, sb) = apply(&fx.state, &fx.pay_by(BORROWER, json!("250100"), 0));
    assert_eq!(xrp_of(&sb, &OWNER), owner_before + 100);
    // One drop short: the pseudo-account and sfCoverAvailable take it.
    edit(&mut fx.state, fx.broker, |b| b["CoverAvailable"] = json!("999999"));
    let pseudo_before = xrp_of(&Sandbox::new(&fx.state), &fx.broker_pseudo);
    let (r, sb) = apply(&fx.state, &fx.pay_by(BORROWER, json!("250100"), 0));
    assert_eq!(r, TxResult::Success);
    assert_eq!(xrp_of(&sb, &OWNER), owner_before);
    assert_eq!(xrp_of(&sb, &fx.broker_pseudo), pseudo_before + 100);
    assert_eq!(nf(&fx.broker_obj(&sb), "CoverAvailable"), n("1000099"));
    // An owner without the asset's holding cannot be paid either.
    let mut usd = Fx::usd().with_loan(json!({"LoanServiceFee": "1"}));
    usd.state.state_map.delete(&keylet::ripple_state_key(&OWNER, &ISSUER, &USD_CODE)).unwrap();
    let (r, sb) = apply(&usd.state, &usd.pay(251, 0));
    assert_eq!(r, TxResult::Success);
    assert_eq!(nf(&usd.broker_obj(&sb), "CoverAvailable"), n("1001"));
    assert_eq!(holds(&sb, &usd.broker_pseudo, &USD), n("1001"));
}

/// LendingHelpers.cpp:2286-2297, :1045-1120 through LoanPay: a late payment
/// needs tfLoanLatePayment and pays the penalties; the late interest is
/// new value for the vault.
#[test]
fn loan_pay_late() {
    let fx = Fx::xrp().with_loan(json!({"LatePaymentFee": "500", "LateInterestRate": 100_000})).at(START + 600 + 31_536);
    // Without the flag: tecEXPIRED, only the fee.
    let (r, sb) = apply(&fx.state, &fx.pay(250, 0));
    assert_eq!(r, TxResult::Expired);
    only_the_fee(&sb, &BORROWER);
    // 31,536 s late at 100% a year on 1,000,000: a thousandth, 1000 drops.
    let (r, _) = apply(&fx.state, &fx.pay_by(BORROWER, json!("251499"), TF_LOAN_LATE_PAYMENT));
    assert_eq!(r, TxResult::InsufficientPayment);
    let (r, sb) = apply(&fx.state, &fx.pay_by(BORROWER, json!("251500"), TF_LOAN_LATE_PAYMENT));
    assert_eq!(r, TxResult::Success);
    let vault = fx.vault_obj(&sb);
    assert_eq!(nf(&vault, "AssetsAvailable"), n("9251000"));
    assert_eq!(nf(&vault, "AssetsTotal"), n("10001000"), "the penalty interest is value the vault gained");
    assert_eq!(nf(&fx.broker_obj(&sb), "DebtTotal"), n("750000"));
    assert_eq!(fx.loan(&sb)["PaymentRemaining"], json!(3));
    // On time, the late flag is tecTOO_SOON; the due date itself is not late under fixCleanup3_4_0.
    let on_time = Fx::xrp().with_loan(json!({})).at(START + 600);
    assert_eq!(result(&on_time.state, &on_time.pay(250, TF_LOAN_LATE_PAYMENT)), TxResult::TooSoon);
    assert_eq!(result(&on_time.state, &on_time.pay(250, 0)), TxResult::Success);
    let mut pre = Fx::xrp().with_loan(json!({})).at(START + 600);
    set_rules(&mut pre.state, &rules_without(&[am::FIX_CLEANUP_3_4_0]));
    assert_eq!(result(&pre.state, &pre.pay(250, 0)), TxResult::Expired);
    assert_eq!(result(&pre.state, &pre.pay(250, TF_LOAN_LATE_PAYMENT)), TxResult::Success);
}

/// LendingHelpers.cpp:1141-1245 through LoanPay: an early full payment —
/// the principal, the close fee and the prepayment interest; tecKILLED on
/// the last payment.
#[test]
fn loan_pay_full() {
    let fx = Fx::xrp().with_loan(json!({"ClosePaymentFee": "700", "CloseInterestRate": 1_000}));
    // 1% of 1,000,000 is the prepayment penalty.
    assert_eq!(result(&fx.state, &fx.pay_by(BORROWER, json!("1010699"), TF_LOAN_FULL_PAYMENT)), TxResult::InsufficientPayment);
    let (r, sb) = apply(&fx.state, &fx.pay_by(BORROWER, json!("1010700"), TF_LOAN_FULL_PAYMENT));
    assert_eq!(r, TxResult::Success);
    let loan = fx.loan(&sb);
    assert!(loan.get("PaymentRemaining").is_none() && loan.get("TotalValueOutstanding").is_none());
    assert_eq!(nf(&fx.vault_obj(&sb), "AssetsAvailable"), n("10010000"));
    assert_eq!(nf(&fx.vault_obj(&sb), "AssetsTotal"), n("10010000"));
    assert!(fx.broker_obj(&sb).get("DebtTotal").is_none());
    assert_eq!(xrp_of(&sb, &OWNER), xrp_of(&Sandbox::new(&fx.state), &OWNER) + 700);
    let last = Fx::xrp().with_loan(json!({"PaymentTotal": 1}));
    let (r, sb) = apply(&last.state, &last.pay(2000, TF_LOAN_FULL_PAYMENT));
    assert_eq!(r, TxResult::Killed);
    only_the_fee(&sb, &BORROWER);
}

/// LendingHelpers.cpp:1742-1822 through LoanPay: an overpayment pays down
/// principal and re-amortizes; an interest-bearing loan loses future
/// interest, which leaves the accrual vault's total.
#[test]
fn loan_pay_overpayment() {
    let fx = Fx::usd().with_loan(json!({"Flags": TF_LOAN_OVERPAYMENT, "InterestRate": 100_000, "PaymentInterval": MONTH, "PaymentTotal": 4}));
    let _g = large();
    let before = (fx.loan(&Sandbox::new(&fx.state)), fx.vault_obj(&Sandbox::new(&fx.state)), fx.broker_obj(&Sandbox::new(&fx.state)));
    let (r, sb) = apply(&fx.state, &fx.pay(500, TF_LOAN_OVERPAYMENT));
    assert_eq!(r, TxResult::Success);
    let (loan, vault, broker) = (fx.loan(&sb), fx.vault_obj(&sb), fx.broker_obj(&sb));
    assert_eq!(loan["PaymentRemaining"], json!(3), "one scheduled payment and an overpayment");
    assert!(nf(&loan, "PeriodicPayment") < nf(&before.0, "PeriodicPayment"));
    // Everything offered was taken and went to the vault (no fees).
    assert_eq!(holds(&sb, &BORROWER, &USD), holds(&Sandbox::new(&fx.state), &BORROWER, &USD).sub(n("500"), AMBIENT).unwrap());
    assert_eq!(nf(&vault, "AssetsAvailable"), nf(&before.1, "AssetsAvailable").add(n("500"), AMBIENT).unwrap());
    // The loan's value fell by more than 500: interest saved, which the vault's total and the debt lose.
    let value_fall = nf(&before.0, "TotalValueOutstanding").sub(nf(&loan, "TotalValueOutstanding"), AMBIENT).unwrap();
    assert!(value_fall > n("500"));
    let saved = value_fall.sub(n("500"), AMBIENT).unwrap();
    assert_eq!(nf(&vault, "AssetsTotal"), nf(&before.1, "AssetsTotal").sub(saved, AMBIENT).unwrap());
    assert_eq!(nf(&broker, "DebtTotal"), nf(&before.2, "DebtTotal").sub(value_fall, AMBIENT).unwrap());
    assert_eq!(nf(&broker, "DebtTotal"), nf(&loan, "TotalValueOutstanding"));
}

/// LoanPay.cpp:413-419: paying an impaired loan unimpairs it.
#[test]
fn loan_pay_unimpairs() {
    let mut fx = Fx::xrp().with_loan(json!({})).at(START + 601);
    fx.commit(fx.manage(TF_LOAN_IMPAIR));
    assert_eq!(nf(&stored(&fx.state, &fx.vault), "LossUnrealized"), n("1000000"));
    let (r, sb) = apply(&fx.state, &fx.pay_by(BORROWER, json!("250000"), TF_LOAN_LATE_PAYMENT));
    assert_eq!(r, TxResult::Success);
    assert_eq!(fx.loan(&sb)["Flags"], json!(0));
    assert!(fx.vault_obj(&sb).get("LossUnrealized").is_none());
}

/// LoanPay.cpp:54-72, :634-640, :761 against 3.5.0-rc1 :53-90 — item B1 of
/// the 3.4.1 / 3.5.0 diff: with issuer-side limits of 9e95 on the
/// borrower's and the fee payee's lines, 3.4.1's conservation balances
/// overflow (tefEXCEPTION, nothing applied, no fee); with fixCleanup3_5_0
/// on — our proxy for a 3.5.x ledger — the payment applies.
#[test]
fn loan_pay_conservation_balances_3_4_1_against_3_5_0() {
    let mut fx = Fx::usd().with_loan(json!({}));
    for holder in [BORROWER, OWNER] {
        let issuer_side = if holder < ISSUER { "HighLimit" } else { "LowLimit" };
        edit(&mut fx.state, keylet::ripple_state_key(&holder, &ISSUER, &USD_CODE), |l| l[issuer_side]["value"] = json!("9e95"));
    }
    let (r, sb) = apply(&fx.state, &fx.pay(250, 0));
    assert_eq!(r, TxResult::TefException, "3.4.1: FullBalance adds the issuer's limit and the sum leaves the IOU range");
    assert_eq!(sb.modification_count(), 0);
    assert_eq!(ConservationModel::of(&Sandbox::new(&fx.state)), ConservationModel::V341);
    set_rules(&mut fx.state, &rules_with(&[am::FIX_CLEANUP_3_5_0]));
    assert_eq!(ConservationModel::of(&Sandbox::new(&fx.state)), ConservationModel::V350);
    assert_eq!(result(&fx.state, &fx.pay(250, 0)), TxResult::Success, "3.5.0: stored balances");
    // One such line alone does not overflow in either.
    let mut one = Fx::usd().with_loan(json!({}));
    let issuer_side = if BORROWER < ISSUER { "HighLimit" } else { "LowLimit" };
    edit(&mut one.state, keylet::ripple_state_key(&BORROWER, &ISSUER, &USD_CODE), |l| l[issuer_side]["value"] = json!("9e95"));
    assert_eq!(result(&one.state, &one.pay(250, 0)), TxResult::Success);
}

// ---------------------------------------------------------------------------
// LoanManage
// ---------------------------------------------------------------------------

/// LoanManage.cpp:43-132.
#[test]
fn loan_manage_preflight_and_preclaim() {
    let fx = Fx::xrp().with_loan(json!({}));
    let id = key_hex(&fx.loan_key());
    assert_eq!(result(&fx.state, &tx("LoanManage", OWNER, json!({"LoanID": "00".repeat(32)}))), TxResult::InvalidTx);
    for two in [TF_LOAN_DEFAULT | TF_LOAN_IMPAIR, TF_LOAN_IMPAIR | TF_LOAN_UNIMPAIR, TF_LOAN_DEFAULT | TF_LOAN_UNIMPAIR] {
        assert_eq!(result(&fx.state, &fx.manage(two)), TxResult::InvalidFlag);
    }
    assert_eq!(result(&fx.state, &tx("LoanManage", OWNER, json!({"LoanID": "AB".repeat(32)}))), TxResult::NoEntry);
    // Only the broker's owner.
    assert_eq!(result(&fx.state, &tx("LoanManage", BORROWER, json!({"LoanID": id}))), TxResult::NoPermission);
    // Unimpairing a loan that is not impaired.
    assert_eq!(result(&fx.state, &fx.manage(TF_LOAN_UNIMPAIR)), TxResult::NoPermission);
    // A default before the grace period is over (due START + 600, grace 60):
    // strictly after it under fixCleanup3_4_0 (devnet E2C28921D130 @ 7129512 is a tecTOO_SOON).
    assert_eq!(result(&fx.state, &fx.manage(TF_LOAN_DEFAULT)), TxResult::TooSoon);
    let at_deadline = Fx::xrp().with_loan(json!({})).at(START + 660);
    assert_eq!(result(&at_deadline.state, &at_deadline.manage(TF_LOAN_DEFAULT)), TxResult::TooSoon);
    let mut pre = Fx::xrp().with_loan(json!({})).at(START + 660);
    set_rules(&mut pre.state, &rules_without(&[am::FIX_CLEANUP_3_4_0]));
    assert_eq!(result(&pre.state, &pre.manage(TF_LOAN_DEFAULT)), TxResult::Success);
    // Impairing twice; anything on a defaulted or paid-off loan.
    let mut impaired = Fx::xrp().with_loan(json!({})).at(START + 661);
    impaired.commit(impaired.manage(TF_LOAN_IMPAIR));
    assert_eq!(result(&impaired.state, &impaired.manage(TF_LOAN_IMPAIR)), TxResult::NoPermission);
    impaired.commit(impaired.manage(TF_LOAN_DEFAULT));
    for flags in [0, TF_LOAN_IMPAIR, TF_LOAN_UNIMPAIR, TF_LOAN_DEFAULT] {
        assert_eq!(result(&impaired.state, &impaired.manage(flags)), TxResult::NoPermission, "defaulted, flags {flags:#x}");
    }
    let mut paid = Fx::xrp().with_loan(json!({}));
    paid.commit(paid.pay(1000, 0));
    assert_eq!(result(&paid.state, &paid.manage(0)), TxResult::NoPermission);
}

/// LoanManage.cpp:392-436: no flag — a success that changes nothing but
/// the fee (devnet 2C47CF7E6248 @ 7129530).
#[test]
fn loan_manage_without_a_flag_changes_nothing() {
    let fx = Fx::xrp().with_loan(json!({}));
    let (r, sb) = apply(&fx.state, &fx.manage(0));
    assert_eq!(r, TxResult::Success);
    only_the_fee(&sb, &OWNER);
}

/// LoanManage.cpp:286-390. (No devnet example of either.)
#[test]
fn loan_manage_impair_and_unimpair() {
    // fixCleanup3_4_0: only a late loan.
    let fx = Fx::xrp().with_loan(json!({}));
    assert_eq!(result(&fx.state, &fx.manage(TF_LOAN_IMPAIR)), TxResult::TooSoon);
    let mut fx = fx.at(START + 601);
    let (r, sb) = apply(&fx.state, &fx.manage(TF_LOAN_IMPAIR));
    assert_eq!(r, TxResult::Success);
    assert_eq!(fx.loan(&sb)["Flags"], json!(LSF_LOAN_IMPAIRED));
    assert_eq!(fx.loan(&sb)["NextPaymentDueDate"], json!(START + 600), "the due date stays");
    assert_eq!(nf(&fx.vault_obj(&sb), "LossUnrealized"), n("1000000"));
    assert_eq!(sb.modification_count(), 3, "the owner's root, the Vault and the Loan; the broker is only read");
    fx.commit(fx.manage(TF_LOAN_IMPAIR));
    let (r, sb) = apply(&fx.state, &fx.manage(TF_LOAN_UNIMPAIR));
    assert_eq!(r, TxResult::Success);
    assert_eq!(fx.loan(&sb)["Flags"], json!(0));
    assert!(fx.vault_obj(&sb).get("LossUnrealized").is_none());
    assert_eq!(fx.loan(&sb)["NextPaymentDueDate"], json!(START + 600));

    // More loss than the vault has lent out: tecLIMIT_EXCEEDED.
    let mut over = Fx::xrp().with_loan(json!({})).at(START + 601);
    edit(&mut over.state, over.vault, |v| v["LossUnrealized"] = json!("1"));
    assert_eq!(result(&over.state, &over.manage(TF_LOAN_IMPAIR)), TxResult::LimitExceeded);

    // Before fixCleanup3_4_0: a loan not yet late is impaired by pulling its
    // due date to now, and unimpairing puts the schedule back.
    let mut pre = Fx::xrp().with_loan(json!({}));
    set_rules(&mut pre.state, &rules_without(&[am::FIX_CLEANUP_3_4_0]));
    let (r, sb) = apply(&pre.state, &pre.manage(TF_LOAN_IMPAIR));
    assert_eq!(r, TxResult::Success);
    assert_eq!(pre.loan(&sb)["NextPaymentDueDate"], json!(NOW));
    pre.commit(pre.manage(TF_LOAN_IMPAIR));
    let (_, sb) = apply(&pre.state, &pre.manage(TF_LOAN_UNIMPAIR));
    assert_eq!(pre.loan(&sb)["NextPaymentDueDate"], json!(START + 600), "the normal due date has not passed");
    let pre = pre.at(START + 600);
    let (_, sb) = apply(&pre.state, &pre.manage(TF_LOAN_UNIMPAIR));
    assert_eq!(pre.loan(&sb)["NextPaymentDueDate"], json!(START + 1200), "it has: one interval from now");
}

/// LoanManage.cpp:134-284 (devnet
/// 3945B76FB7B0 @ 7129649): the first-loss capital covers part, the vault
/// loses the rest.
#[test]
fn loan_manage_default() {
    // Cover 1,000,000; minimum 10% of the 1,000,000 debt; half of that is liquidated: 50,000.
    let fx = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({"CoverRateMinimum": 10_000, "CoverRateLiquidation": 50_000}))
        .with_loan(json!({}))
        .at(START + 661);
    let (r, sb) = apply(&fx.state, &fx.manage(TF_LOAN_DEFAULT));
    assert_eq!(r, TxResult::Success);
    let (loan, vault, broker) = (fx.loan(&sb), fx.vault_obj(&sb), fx.broker_obj(&sb));
    assert_eq!(loan["Flags"], json!(LSF_LOAN_DEFAULT));
    for gone in ["PaymentRemaining", "NextPaymentDueDate", "PrincipalOutstanding", "TotalValueOutstanding", "ManagementFeeOutstanding"] {
        assert!(loan.get(gone).is_none(), "{gone}");
    }
    assert_eq!(nf(&vault, "AssetsTotal"), n("9050000"));
    assert_eq!(nf(&vault, "AssetsAvailable"), n("9050000"));
    assert!(broker.get("DebtTotal").is_none());
    assert_eq!(nf(&broker, "CoverAvailable"), n("950000"));
    assert_eq!(broker["OwnerCount"], json!(1));
    assert_eq!(xrp_of(&sb, &fx.vault_pseudo), 9_050_000);
    assert_eq!(xrp_of(&sb, &fx.broker_pseudo), 950_000);

    // An impaired loan's unrealized loss is released by the default.
    let mut impaired = Fx::xrp().with_loan(json!({})).at(START + 661);
    impaired.commit(impaired.manage(TF_LOAN_IMPAIR));
    let (r, sb) = apply(&impaired.state, &impaired.manage(TF_LOAN_DEFAULT));
    assert_eq!(r, TxResult::Success);
    let vault = impaired.vault_obj(&sb);
    assert!(vault.get("LossUnrealized").is_none());
    // No cover rates: nothing is covered, the vault loses it all.
    assert_eq!(nf(&vault, "AssetsTotal"), n("9000000"));
    assert_eq!(nf(&vault, "AssetsAvailable"), n("9000000"));
    assert_eq!(nf(&impaired.broker_obj(&sb), "CoverAvailable"), n("1000000"));

    // Cash basis: the exposure is the principal alone.
    let mut cash = Fx::usd();
    edit(&mut cash.state, cash.vault, |v| v["LEVersion"] = json!(1));
    let cash = cash.with_loan(json!({"InterestRate": 100_000, "PaymentInterval": MONTH, "GracePeriod": 60})).at(START + MONTH + 61);
    let (r, sb) = apply(&cash.state, &cash.manage(TF_LOAN_DEFAULT));
    assert_eq!(r, TxResult::Success);
    assert_eq!(nf(&cash.vault_obj(&sb), "AssetsTotal"), n("9000"));
    assert!(cash.broker_obj(&sb).get("DebtTotal").is_none());
}

// ---------------------------------------------------------------------------
// LoanDelete
// ---------------------------------------------------------------------------

/// LoanDelete.cpp:27-136 (devnet 05FD3130167F @ 7129497, 32F27BD10A22 @
/// 7129650).
#[test]
fn loan_delete() {
    let mut fx = Fx::xrp().with_loan(json!({}));
    assert_eq!(result(&fx.state, &tx("LoanDelete", BORROWER, json!({"LoanID": "00".repeat(32)}))), TxResult::InvalidTx);
    assert_eq!(result(&fx.state, &tx("LoanDelete", BORROWER, json!({"LoanID": "AB".repeat(32)}))), TxResult::NoEntry);
    // Payments remain.
    let (r, sb) = apply(&fx.state, &fx.delete(BORROWER));
    assert_eq!(r, TxResult::HasObligations);
    only_the_fee(&sb, &BORROWER);
    fx.commit(fx.pay(1000, 0));
    // Somebody else.
    assert_eq!(result(&fx.state, &fx.delete(CAROL)), TxResult::NoPermission);
    // Dust left in the broker's debt goes with its last loan.
    edit(&mut fx.state, fx.broker, |b| b["DebtTotal"] = json!("0.5"));
    for who in [BORROWER, OWNER] {
        let (r, sb) = apply(&fx.state, &fx.delete(who));
        assert_eq!(r, TxResult::Success);
        let key = fx.loan_key();
        assert!(get(&sb, &key).is_none());
        assert!(!dir_has(&sb, &BORROWER, &key) && !dir_has(&sb, &fx.broker_pseudo, &key));
        assert_eq!(owners(&sb, &BORROWER), 0);
        let broker = fx.broker_obj(&sb);
        assert!(broker.get("OwnerCount").is_none() && broker.get("DebtTotal").is_none());
        assert_eq!(broker["LoanSequence"], json!(2));
        assert_eq!(get(&sb, &fx.vault), Some(stored(&fx.state, &fx.vault)), "the vault is not written");
    }
    // With another loan alive the debt stays.
    let mut two = Fx::xrp().with_loan(json!({}));
    two.commit(two.loan_set(json!({})));
    two.commit(two.pay(1000, 0));
    let (r, sb) = apply(&two.state, &two.delete(OWNER));
    assert_eq!(r, TxResult::Success);
    assert_eq!(two.broker_obj(&sb)["OwnerCount"], json!(1));
    assert_eq!(nf(&two.broker_obj(&sb), "DebtTotal"), n("1000000"));
    assert!(dir_has(&sb, &BORROWER, &keylet::loan_key(&two.broker, 2)));
}

/// A whole life on a USD loan with interest and a management fee: made,
/// paid period by period to the end, deleted — the vault ends with its
/// principal back plus the interest, the broker with no debt
/// (LoanSet.cpp, LoanPay.cpp, LoanDelete.cpp end to end).
#[test]
fn a_whole_loan_life() {
    let mut fx = Fx::new(Kind::Usd, &MAINNET_WITH_LENDING, json!({"ManagementFeeRate": 10_000}))
        .with_loan(json!({"InterestRate": 100_000, "PaymentInterval": MONTH, "PaymentTotal": 6}));
    let _g = large();
    let loan = stored(&fx.state, &fx.loan_key());
    let value = nf(&loan, "TotalValueOutstanding");
    let fee = nf(&loan, "ManagementFeeOutstanding");
    assert!(fee > Number::ZERO);
    let interest = value.sub(n("1000"), AMBIENT).unwrap().sub(fee, AMBIENT).unwrap();
    assert_eq!(nf(&stored(&fx.state, &fx.broker), "DebtTotal"), value.sub(fee, AMBIENT).unwrap(), "the debt is what the vault is owed");
    let owner_before = holds(&Sandbox::new(&fx.state), &OWNER, &USD);
    let mut payments = 0;
    while stored(&fx.state, &fx.loan_key()).get("PaymentRemaining").is_some() {
        // 300 covers one period (about 218) but not two.
        fx.commit(fx.pay(300, 0));
        payments += 1;
        assert!(payments <= 6);
    }
    assert_eq!(payments, 6);
    let (vault, broker) = (stored(&fx.state, &fx.vault), stored(&fx.state, &fx.broker));
    assert_eq!(nf(&vault, "AssetsAvailable"), n("10000").add(interest, AMBIENT).unwrap());
    assert_eq!(nf(&vault, "AssetsTotal"), nf(&vault, "AssetsAvailable"));
    assert!(broker.get("DebtTotal").is_none());
    let sb = Sandbox::new(&fx.state);
    assert_eq!(holds(&sb, &fx.vault_pseudo, &USD), nf(&vault, "AssetsAvailable"));
    assert_eq!(holds(&sb, &OWNER, &USD), owner_before.add(fee, AMBIENT).unwrap(), "the management fee went to the owner");
    assert_eq!(holds(&sb, &BORROWER, &USD), n("2000").sub(value, AMBIENT).unwrap());
    fx.commit(fx.delete(BORROWER));
    assert!(fx.state.state_map.lookup(&fx.loan_key()).is_none());
    assert!(stored(&fx.state, &fx.broker).get("OwnerCount").is_none());
}

/// What the vault and loan-broker transactors (ported earlier) read of the
/// state a loan leaves: VaultWithdraw against sfAssetsAvailable
/// (VaultWithdraw.cpp:440-499), VaultDelete's and LoanBrokerDelete's
/// obligations (VaultDelete.cpp:56-66, LoanBrokerDelete.cpp:59-63),
/// LoanBrokerCoverWithdraw's minimum cover on sfDebtTotal
/// (LoanBrokerCoverWithdraw.cpp:171-195).
#[test]
fn the_vault_and_the_broker_see_the_loan() {
    let mut fx = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({"CoverRateMinimum": 10_000, "CoverRateLiquidation": 10_000})).with_loan(json!({}));
    let (vault, broker) = (key_hex(&fx.vault), key_hex(&fx.broker));
    // 9,000,000 drops are on hand; 1,000,000 are out on loan.
    let withdraw = |drops: u64| tx("VaultWithdraw", OWNER, json!({"VaultID": vault, "Amount": drops.to_string()}));
    assert_eq!(result(&fx.state, &withdraw(9_000_001)), TxResult::InsufficientFunds);
    assert_eq!(result(&fx.state, &withdraw(9_000_000)), TxResult::Success);
    assert_eq!(result(&fx.state, &tx("VaultDelete", OWNER, json!({"VaultID": vault}))), TxResult::HasObligations);
    assert_eq!(result(&fx.state, &tx("LoanBrokerDelete", OWNER, json!({"LoanBrokerID": broker}))), TxResult::HasObligations);
    // 10% of the 1,000,000 debt must stay as cover.
    let cover_out = |drops: u64| tx("LoanBrokerCoverWithdraw", OWNER, json!({"LoanBrokerID": broker, "Amount": drops.to_string()}));
    assert_eq!(result(&fx.state, &cover_out(900_001)), TxResult::InsufficientFunds);
    assert_eq!(result(&fx.state, &cover_out(900_000)), TxResult::Success);
    // An impaired loan's exposure is an unrealized loss: the same shares are worth less.
    let late = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({})).with_loan(json!({})).at(START + 601);
    let mut impaired = Fx::new(Kind::Xrp, &MAINNET_WITH_LENDING, json!({})).with_loan(json!({})).at(START + 601);
    impaired.commit(impaired.manage(TF_LOAN_IMPAIR));
    let share = stored(&late.state, &late.vault)["ShareMPTID"].as_str().unwrap().to_string();
    let redeem = |fx: &Fx| {
        let before = xrp_of(&Sandbox::new(&fx.state), &OWNER);
        let t = tx("VaultWithdraw", OWNER, json!({"VaultID": key_hex(&fx.vault), "Amount": {"mpt_issuance_id": share, "value": "1000000"}}));
        let (r, sb) = apply(&fx.state, &t);
        assert_eq!(r, TxResult::Success);
        xrp_of(&sb, &OWNER) + FEE - before
    };
    // fixCleanup3_2_0 waives the discount for the vault's only shareholder
    // (VaultWithdraw.cpp:40-50 `shouldWaiveWithdrawal`); before it the loss
    // lowers the rate.
    assert_eq!(redeem(&impaired), redeem(&late), "a sole shareholder is not discounted");
    let (mut late, mut impaired) = (late, impaired);
    set_rules(&mut late.state, &rules_without(&[am::FIX_CLEANUP_3_2_0]));
    set_rules(&mut impaired.state, &rules_without(&[am::FIX_CLEANUP_3_2_0]));
    assert!(redeem(&impaired) < redeem(&late), "the loss discounts a withdrawal");
    // Paid off and deleted, the broker can go.
    fx.commit(fx.pay(1000, 0));
    assert_eq!(result(&fx.state, &tx("LoanBrokerDelete", OWNER, json!({"LoanBrokerID": broker}))), TxResult::HasObligations, "the Loan object is still there");
    fx.commit(fx.delete(OWNER));
    let (r, sb) = apply(&fx.state, &tx("LoanBrokerDelete", OWNER, json!({"LoanBrokerID": broker})));
    assert_eq!(r, TxResult::Success);
    assert!(get(&sb, &fx.broker).is_none() && root_of(&sb, &fx.broker_pseudo).is_none());
}

// ---------------------------------------------------------------------------
// Base fees
// ---------------------------------------------------------------------------

/// Transactor.cpp:449-474, LoanSet.cpp:183-208.
#[test]
fn loan_set_base_fee_counts_the_counterpartys_signers() {
    let signer = json!({"Signer": {"Account": hex::encode(CAROL), "SigningPubKey": "", "TxnSignature": ""}});
    assert_eq!(loan_set_base_fee(&json!({}), 10), 10, "no counterparty signature at all");
    assert_eq!(loan_set_base_fee(&json!({"CounterpartySignature": {"SigningPubKey": ""}}), 10), 10, "neither a signature nor signers");
    assert_eq!(loan_set_base_fee(&json!({"CounterpartySignature": counter_signature()}), 10), 20, "one signature");
    assert_eq!(loan_set_base_fee(&json!({"CounterpartySignature": {"Signers": [signer, signer, signer]}}), 10), 40, "three signers");
    // The signers array wins over a signature beside it; the primary's own multi-signers add theirs.
    assert_eq!(
        loan_set_base_fee(&json!({"Signers": [signer, signer], "CounterpartySignature": {"TxnSignature": "00", "Signers": [signer]}}), 10),
        40
    );
    assert_eq!(transactor_base_fee(&json!({"SponsorSignature": {"Signers": [signer]}, "Signers": [signer]}), 10), 30);
}

/// LoanPay.cpp:112-226.
#[test]
fn loan_pay_base_fee_is_one_increment_per_five_payments() {
    // 100 payments of 1000 drops.
    let fx = Fx::xrp().with_loan(json!({"PrincipalRequested": "100000", "PaymentTotal": 100, "PaymentInterval": 60}));
    let fee = |fx: &Fx, t: &TxFields| loan_pay_base_fee(&Sandbox::new(&fx.state), t, 10).unwrap();
    let pay = |drops: u64, flags: u64| fx.pay_by(BORROWER, json!(drops.to_string()), flags);
    // floor(amount / 1000) payments, a fifth of that rounded up, at least one.
    for (drops, want) in [(1, 10), (1000, 10), (5000, 10), (5999, 10), (6000, 20), (9999, 20), (10_999, 20), (11_000, 30), (50_000, 100), (99_999, 200)] {
        assert_eq!(fee(&fx, &pay(drops, 0)), want, "{drops} drops");
    }
    // An overpayment counts a started payment as one: ceil(5001 / 1000) = 6.
    assert_eq!(fee(&fx, &pay(5000, TF_LOAN_OVERPAYMENT)), 10);
    assert_eq!(fee(&fx, &pay(5001, TF_LOAN_OVERPAYMENT)), 20);
    // fixCleanup3_1_3: a hundred payments or more is the maximum, twenty increments.
    assert_eq!(fee(&fx, &pay(100_000, 0)), 200);
    assert_eq!(fee(&fx, &pay(900_000_000, 0)), 200);
    // Each increment costs the transaction's normal cost: one more base fee per multi-signer.
    let mut signed = pay(6000, 0);
    signed.fields["Signers"] = json!([{"Signer": {}}, {"Signer": {}}]);
    assert_eq!(fee(&fx, &signed), 60);
    // The normal cost: a full or late payment, another asset, no such loan.
    assert_eq!(fee(&fx, &pay(50_000, TF_LOAN_FULL_PAYMENT)), 10);
    assert_eq!(fee(&fx, &pay(50_000, TF_LOAN_LATE_PAYMENT)), 10);
    assert_eq!(fee(&fx, &fx.pay_by(BORROWER, usd_json("50000"), 0)), 10);
    assert_eq!(fee(&fx, &tx("LoanPay", BORROWER, json!({"LoanID": "AB".repeat(32), "Amount": "50000"}))), 10);
    // A late loan; five or fewer payments remaining.
    let late = Fx::xrp().with_loan(json!({"PrincipalRequested": "100000", "PaymentTotal": 100, "PaymentInterval": 60})).at(START + 61);
    assert_eq!(fee(&late, &late.pay_by(BORROWER, json!("50000"), 0)), 10);
    let short = Fx::xrp().with_loan(json!({"PrincipalRequested": "5000", "PaymentTotal": 5, "PaymentInterval": 60}));
    assert_eq!(fee(&short, &short.pay_by(BORROWER, json!("5000"), 0)), 10);
    let six = Fx::xrp().with_loan(json!({"PrincipalRequested": "6000", "PaymentTotal": 6, "PaymentInterval": 60}));
    assert_eq!(fee(&six, &six.pay_by(BORROWER, json!("6000"), 0)), 20);
    // A non-positive amount: the normal cost under fixCleanup3_4_0 (and one increment before it).
    assert_eq!(fee(&fx, &pay(0, 0)), 10);

    // Before fixCleanup3_1_3 nothing caps the estimate: a large amount costs
    // its count, and one whose count does not fit 64 bits throws — the
    // transaction is tefEXCEPTION in preclaim, before LoanPay's own checks.
    let mut old = Fx::usd().with_loan(json!({"PaymentTotal": 100, "PaymentInterval": 60}));
    set_rules(&mut old.state, &rules_without(&[am::FIX_CLEANUP_3_1_3]));
    assert_eq!(fee(&old, &old.pay(100_000, 0)), 20_000, "10000 payments of 10 USD: 2000 increments");
    let enormous = old.pay_by(BORROWER, usd_json("9e40"), 0);
    assert!(loan_pay_base_fee(&Sandbox::new(&old.state), &enormous, 10).is_err());
    let (r, sb) = apply(&old.state, &enormous);
    assert_eq!(r, TxResult::TefException);
    assert_eq!(sb.modification_count(), 0);
    set_rules(&mut old.state, &MAINNET_WITH_LENDING);
    assert_eq!(fee(&old, &enormous), 200);
    assert_eq!(result(&old.state, &enormous), TxResult::InsufficientFunds, "with the fix it reaches preclaim's balance check");
}
