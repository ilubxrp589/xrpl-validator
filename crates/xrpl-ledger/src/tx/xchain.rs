//! XLS-38 cross-chain bridge transactors — all eight, ported from the single
//! XChainBridge.cpp (3.2.1).
//!
//! ⚠ ZERO mainnet usage: no Bridge object exists on mainnet and no XChain
//! transaction appears in any fixture window, so every path here is a BLIND
//! source port. The reachable behavior on today's mainnet is exactly one
//! branch — "no bridge" (tecNO_ENTRY) — everything deeper can only execute
//! after a XChainCreateBridge lands, which the scout would surface with a
//! fixture to verify against. Attestation SIGNATURES are not re-verified
//! (mainnet-validated); signer-list membership and quorum weights are.

use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};
use crate::tx::offer as ox;
use xrpl_core::types::Hash256;

/// The four corners of a bridge spec, decoded from the `XChainBridge` JSON
/// object carried by every XChain transaction and stored on the objects.
struct BridgeSpec {
    locking_door: [u8; 20],
    locking_cur: [u8; 20],
    locking_iss: [u8; 20],
    issuing_door: [u8; 20],
    issuing_cur: [u8; 20],
    issuing_iss: [u8; 20],
    raw: serde_json::Value,
}

fn issue_of(v: &serde_json::Value) -> Option<([u8; 20], [u8; 20], bool)> {
    let cur = v.get("currency").and_then(|c| c.as_str())?;
    if cur == "XRP" {
        return Some(([0u8; 20], [0u8; 20], true));
    }
    let mut c20 = [0u8; 20];
    if cur.len() == 40 {
        c20.copy_from_slice(&hex::decode(cur).ok()?);
    } else if cur.len() == 3 {
        c20[12..15].copy_from_slice(cur.as_bytes());
    } else {
        return None;
    }
    let iss = v.get("issuer").and_then(|i| i.as_str()).and_then(ox::decode20)?;
    Some((c20, iss, false))
}

fn bridge_spec(tx: &TxFields) -> Option<BridgeSpec> {
    let b = tx.fields.get("XChainBridge")?;
    let (lc, li, _) = issue_of(b.get("LockingChainIssue")?)?;
    let (ic, ii, _) = issue_of(b.get("IssuingChainIssue")?)?;
    Some(BridgeSpec {
        locking_door: b.get("LockingChainDoor").and_then(|v| v.as_str()).and_then(ox::decode20)?,
        locking_cur: lc,
        locking_iss: li,
        issuing_door: b.get("IssuingChainDoor").and_then(|v| v.as_str()).and_then(ox::decode20)?,
        issuing_cur: ic,
        issuing_iss: ii,
        raw: b.clone(),
    })
}

impl BridgeSpec {
    /// The bridge object key from THIS chain's perspective: the tx account
    /// tells which side we are (`STXChainBridge::srcChain(account == door)`),
    /// and mainnet — the locking side of any real bridge that names it — is
    /// resolved the same way rippled does: try the side whose door matches.
    fn bridge_key_for(&self, door: &[u8; 20]) -> Hash256 {
        if *door == self.locking_door {
            keylet::bridge_key(&self.locking_door, &self.locking_cur)
        } else {
            keylet::bridge_key(&self.issuing_door, &self.issuing_cur)
        }
    }

    /// Find the bridge object from either side (rippled readBridge: try
    /// locking, then issuing).
    fn read_bridge(&self, sandbox: &Sandbox) -> Option<(Hash256, serde_json::Value)> {
        for key in [
            keylet::bridge_key(&self.locking_door, &self.locking_cur),
            keylet::bridge_key(&self.issuing_door, &self.issuing_cur),
        ] {
            if let Some(b) = ox::json_at(sandbox, &key) {
                return Some((key, b));
            }
        }
        None
    }

    fn claim_id_key(&self, space: u8, seq: u64) -> Hash256 {
        keylet::xchain_claim_id_key(
            space,
            &self.locking_door,
            (&self.locking_cur, &self.locking_iss),
            &self.issuing_door,
            (&self.issuing_cur, &self.issuing_iss),
            seq,
        )
    }
}

fn u64_field(v: &serde_json::Value, f: &str) -> u64 {
    v.get(f)
        .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| u64::from_str_radix(s, 16).ok())))
        .unwrap_or(0)
}

/// Move an amount (XRP drops string or IOU object) between accounts through
/// the ordinary leg machinery — the lean stand-in for transferHelper.
fn transfer(sandbox: &mut Sandbox, from: &[u8; 20], to: &[u8; 20], amount: &serde_json::Value) -> bool {
    if let Some(d) = amount.as_str().and_then(|s| s.parse::<u64>().ok()) {
        ox::move_leg(sandbox, from, to, &ox::Leg { xrp: true, cur: [0u8; 20], issuer: [0u8; 20] }, (d as u128, 0));
        return true;
    }
    let (Some(leg), Some(val)) = (ox::leg_of(amount), keylet::amount_mant_exp(amount)) else {
        return false;
    };
    ox::move_leg(sandbox, from, to, &leg, val);
    true
}

/// The door's signer list: (weights by account, quorum).
fn door_signers(
    sandbox: &Sandbox,
    door: &[u8; 20],
) -> Option<(std::collections::HashMap<[u8; 20], u64>, u64)> {
    let sl = ox::json_at(sandbox, &keylet::signers_key(door))?;
    let quorum = sl.get("SignerQuorum").and_then(|v| v.as_u64())?;
    let mut weights = std::collections::HashMap::new();
    for e in sl.get("SignerEntries").and_then(|v| v.as_array()).into_iter().flatten() {
        let inner = e.get("SignerEntry").unwrap_or(e);
        if let (Some(a), Some(w)) = (
            inner.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20),
            inner.get("SignerWeight").and_then(|v| v.as_u64()),
        ) {
            weights.insert(a, w);
        }
    }
    Some((weights, quorum))
}

fn account_exists(sandbox: &Sandbox, tx: &TxFields) -> TxResult {
    if !sandbox.exists(&keylet::account_root_key(&tx.account)) {
        return TxResult::NoAccount;
    }
    TxResult::Success
}

macro_rules! xchain_preflight {
    ($tx:expr, $name:literal) => {
        if $tx.tx_type != $name {
            return TxResult::Malformed;
        }
        if $tx.fee == 0 {
            return TxResult::BadFee;
        }
        if $tx.fields.get("XChainBridge").is_none() {
            return TxResult::Malformed;
        }
    };
}

// ---------------------------------------------------------------------------

pub struct XChainCreateBridgeTransactor;

impl Transactor for XChainCreateBridgeTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainCreateBridge");
        if tx.fields.get("SignatureReward").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        // The creator must be one of the doors (preclaim tecXCHAIN_BRIDGE_
        // NONDOOR_OWNER territory — Malformed stands in, no specimen).
        if tx.account != spec.locking_door && tx.account != spec.issuing_door {
            return TxResult::Malformed;
        }
        let key = spec.bridge_key_for(&tx.account);
        if sandbox.exists(&key) {
            return TxResult::Duplicate;
        }
        let node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &key);
        let mut obj = serde_json::json!({
            "LedgerEntryType": "Bridge",
            "Account": hex::encode(tx.account),
            "XChainBridge": spec.raw,
            "SignatureReward": tx.fields.get("SignatureReward").cloned().unwrap_or_default(),
            "XChainClaimID": "0",
            "XChainAccountCreateCount": "0",
            "XChainAccountClaimCount": "0",
            "OwnerNode": format!("{node:x}"),
        });
        if let Some(m) = tx.fields.get("MinAccountCreateAmount") {
            obj["MinAccountCreateAmount"] = m.clone();
        }
        sandbox.write(key, serde_json::to_vec(&obj).unwrap_or_default());
        ox::owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

pub struct XChainModifyBridgeTransactor;

impl Transactor for XChainModifyBridgeTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainModifyBridge");
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let key = spec.bridge_key_for(&tx.account);
        let Some(mut b) = ox::json_at(sandbox, &key) else { return TxResult::NoEntry };
        if let Some(r) = tx.fields.get("SignatureReward") {
            b["SignatureReward"] = r.clone();
        }
        if let Some(m) = tx.fields.get("MinAccountCreateAmount") {
            b["MinAccountCreateAmount"] = m.clone();
        }
        // tfClearAccountCreateAmount
        let flags = tx.fields.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
        if flags & 0x0001_0000 != 0 {
            b.as_object_mut().map(|o| o.remove("MinAccountCreateAmount"));
        }
        sandbox.write(key, serde_json::to_vec(&b).unwrap_or_default());
        TxResult::Success
    }
}

pub struct XChainCreateClaimIDTransactor;

impl Transactor for XChainCreateClaimIDTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainCreateClaimID");
        if tx.fields.get("SignatureReward").is_none() || tx.fields.get("OtherChainSource").is_none()
        {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let Some((bkey, mut bridge)) = spec.read_bridge(sandbox) else {
            return TxResult::NoEntry;
        };
        if tx.fields.get("SignatureReward") != bridge.get("SignatureReward") {
            return TxResult::XChainRewardMismatch;
        }
        // Reserve for the new claim id (post-fee balance, addSLE-style).
        let acct = ox::json_at(sandbox, &keylet::account_root_key(&tx.account));
        let bal = acct
            .as_ref()
            .and_then(|a| a["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()))
            .unwrap_or(0);
        let oc = acct.as_ref().and_then(|a| a["OwnerCount"].as_u64()).unwrap_or(0);
        if bal < crate::ledger::fees::account_reserve(sandbox, oc + 1) {
            return TxResult::InsufficientReserve;
        }
        let claim_id = u64_field(&bridge, "XChainClaimID") + 1;
        bridge["XChainClaimID"] = serde_json::Value::String(format!("{claim_id:x}"));
        sandbox.write(bkey, serde_json::to_vec(&bridge).unwrap_or_default());
        let ckey = spec.claim_id_key(0x51, claim_id);
        let node = crate::ledger::directory::owner_dir_insert(sandbox, &tx.account, &ckey);
        let obj = serde_json::json!({
            "LedgerEntryType": "XChainOwnedClaimID",
            "Account": hex::encode(tx.account),
            "XChainBridge": spec.raw,
            "XChainClaimID": format!("{claim_id:x}"),
            "OtherChainSource": tx.fields.get("OtherChainSource").cloned().unwrap_or_default(),
            "SignatureReward": tx.fields.get("SignatureReward").cloned().unwrap_or_default(),
            "XChainClaimAttestations": [],
            "OwnerNode": format!("{node:x}"),
        });
        sandbox.write(ckey, serde_json::to_vec(&obj).unwrap_or_default());
        ox::owner_count_add(sandbox, &tx.account, 1);
        TxResult::Success
    }
}

pub struct XChainCommitTransactor;

impl Transactor for XChainCommitTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainCommit");
        if tx.fields.get("Amount").is_none() || tx.fields.get("XChainClaimID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let Some((_bkey, bridge)) = spec.read_bridge(sandbox) else {
            return TxResult::NoEntry;
        };
        let Some(door) = bridge.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        if door == tx.account {
            return TxResult::XChainSelfCommit;
        }
        // The committed asset must be this side's bridge issue.
        let is_locking = door == spec.locking_door;
        let want = if is_locking {
            (spec.locking_cur, spec.locking_iss)
        } else {
            (spec.issuing_cur, spec.issuing_iss)
        };
        let amt = tx.fields.get("Amount").cloned().unwrap_or_default();
        let got = if amt.is_string() {
            ([0u8; 20], [0u8; 20])
        } else {
            match issue_of(&amt) {
                Some((c, i, _)) => (c, i),
                None => return TxResult::Malformed,
            }
        };
        if got != want {
            return TxResult::XChainBadTransferIssue;
        }
        if !transfer(sandbox, &tx.account, &door, &amt) {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
}

pub struct XChainAccountCreateCommitTransactor;

impl Transactor for XChainAccountCreateCommitTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainAccountCreateCommit");
        if tx.fields.get("Amount").is_none()
            || tx.fields.get("SignatureReward").is_none()
            || tx.fields.get("Destination").is_none()
        {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let Some((bkey, mut bridge)) = spec.read_bridge(sandbox) else {
            return TxResult::NoEntry;
        };
        if tx.fields.get("SignatureReward") != bridge.get("SignatureReward") {
            return TxResult::XChainRewardMismatch;
        }
        let Some(door) = bridge.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        let amt = tx.fields.get("Amount").cloned().unwrap_or_default();
        let reward = tx.fields.get("SignatureReward").cloned().unwrap_or_default();
        if !transfer(sandbox, &tx.account, &door, &amt)
            || !transfer(sandbox, &tx.account, &door, &reward)
        {
            return TxResult::Malformed;
        }
        let count = u64_field(&bridge, "XChainAccountCreateCount") + 1;
        bridge["XChainAccountCreateCount"] = serde_json::Value::String(format!("{count:x}"));
        sandbox.write(bkey, serde_json::to_vec(&bridge).unwrap_or_default());
        TxResult::Success
    }
}

// ---------------------------------------------------------------------------
// Attestations + claims — the quorum machinery. One attestation per tx (the
// post-XLS-38 single-attestation format).
// ---------------------------------------------------------------------------

/// Tally the stored attestations that match (amount, source, chain[, dst])
/// against the door's signer weights; Some(reward accounts) at quorum.
fn quorum_rewards(
    atts: &[serde_json::Value],
    weights: &std::collections::HashMap<[u8; 20], u64>,
    quorum: u64,
    amount: &serde_json::Value,
    source: &str,
    was_locking: u64,
    dst: Option<&str>,
) -> Option<Vec<[u8; 20]>> {
    let mut weight = 0u64;
    let mut rewards = Vec::new();
    for a in atts {
        let inner = a.get("XChainClaimProofSig").unwrap_or(a);
        let signer = inner
            .get("AttestationSignerAccount")
            .and_then(|v| v.as_str())
            .and_then(ox::decode20);
        let Some(signer) = signer else { continue };
        let Some(w) = weights.get(&signer) else { continue };
        if inner.get("Amount") != Some(amount) {
            continue;
        }
        if inner.get("WasLockingChainSend").and_then(|v| v.as_u64()).unwrap_or(0) != was_locking {
            continue;
        }
        let _ = source;
        if let Some(d) = dst {
            if inner.get("Destination").and_then(|v| v.as_str()) != Some(d) {
                continue;
            }
        }
        weight += w;
        if let Some(r) = inner
            .get("AttestationRewardAccount")
            .and_then(|v| v.as_str())
            .and_then(ox::decode20)
        {
            rewards.push(r);
        }
    }
    (weight >= quorum).then_some(rewards)
}

/// finalizeClaimHelper: pay the claimed amount door→destination, split the
/// reward pool owner→attesters (divide, round down), delete the claim id.
fn finalize_claim(
    sandbox: &mut Sandbox,
    door: &[u8; 20],
    dst: &[u8; 20],
    owner: &[u8; 20],
    amount: &serde_json::Value,
    reward_pool: &serde_json::Value,
    rewards: &[[u8; 20]],
    ckey: Hash256,
    claim: &serde_json::Value,
) -> TxResult {
    if !transfer(sandbox, door, dst, amount) {
        return TxResult::Malformed;
    }
    if !rewards.is_empty() {
        if let Some(pool) = reward_pool.as_str().and_then(|s| s.parse::<u64>().ok()) {
            let share = pool / rewards.len() as u64;
            if share > 0 {
                for r in rewards {
                    let sh = serde_json::Value::String(share.to_string());
                    let _ = transfer(sandbox, owner, r, &sh);
                }
            }
        }
    }
    let hint = claim
        .get("OwnerNode")
        .and_then(|v| v.as_str())
        .and_then(|h| u64::from_str_radix(h, 16).ok());
    sandbox.delete(ckey);
    crate::ledger::directory::owner_dir_remove(sandbox, owner, &ckey, hint, false);
    ox::owner_count_add(sandbox, owner, -1);
    TxResult::Success
}

pub struct XChainAddClaimAttestationTransactor;

impl Transactor for XChainAddClaimAttestationTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainAddClaimAttestation");
        if tx.fields.get("XChainClaimID").is_none() {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let Some((_bkey, bridge)) = spec.read_bridge(sandbox) else {
            return TxResult::NoEntry;
        };
        let Some(door) = bridge.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        let Some((weights, quorum)) = door_signers(sandbox, &door) else {
            return TxResult::XChainNoSignersList;
        };
        let claim_id = tx
            .fields
            .get("XChainClaimID")
            .and_then(|v| v.as_str())
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        let ckey = spec.claim_id_key(0x51, claim_id);
        let Some(mut claim) = ox::json_at(sandbox, &ckey) else {
            return TxResult::XChainNoClaimId;
        };
        let signer = tx
            .fields
            .get("AttestationSignerAccount")
            .and_then(|v| v.as_str())
            .and_then(ox::decode20);
        let Some(signer) = signer else { return TxResult::Malformed };
        if !weights.contains_key(&signer) {
            return TxResult::XChainProofUnknownKey;
        }
        if tx.fields.get("OtherChainSource") != claim.get("OtherChainSource") {
            return TxResult::XChainSendingAccountMismatch;
        }
        // Upsert this signer's attestation.
        let entry = serde_json::json!({"XChainClaimProofSig": {
            "Amount": tx.fields.get("Amount").cloned().unwrap_or_default(),
            "AttestationRewardAccount": tx.fields.get("AttestationRewardAccount").cloned().unwrap_or_default(),
            "AttestationSignerAccount": tx.fields.get("AttestationSignerAccount").cloned().unwrap_or_default(),
            "PublicKey": tx.fields.get("PublicKey").cloned().unwrap_or_default(),
            "WasLockingChainSend": tx.fields.get("WasLockingChainSend").cloned().unwrap_or(serde_json::json!(0)),
            "Destination": tx.fields.get("Destination").cloned().unwrap_or_default(),
        }});
        let mut atts = claim
            .get("XChainClaimAttestations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let same_signer = |a: &serde_json::Value| {
            a.get("XChainClaimProofSig")
                .unwrap_or(a)
                .get("AttestationSignerAccount")
                .and_then(|v| v.as_str())
                .and_then(ox::decode20)
                == Some(signer)
        };
        if let Some(slot) = atts.iter_mut().find(|a| same_signer(a)) {
            *slot = entry;
        } else {
            atts.push(entry);
        }
        claim["XChainClaimAttestations"] = serde_json::Value::Array(atts.clone());
        sandbox.write(ckey, serde_json::to_vec(&claim).unwrap_or_default());

        // Quorum check with the tx's own attested data as the match target.
        let amount = tx.fields.get("Amount").cloned().unwrap_or_default();
        let was_locking =
            tx.fields.get("WasLockingChainSend").and_then(|v| v.as_u64()).unwrap_or(0);
        let dst = tx.fields.get("Destination").and_then(|v| v.as_str());
        let source =
            tx.fields.get("OtherChainSource").and_then(|v| v.as_str()).unwrap_or_default();
        if let (Some(rewards), Some(d)) = (
            quorum_rewards(&atts, &weights, quorum, &amount, source, was_locking, dst),
            dst.and_then(ox::decode20),
        ) {
            let Some(owner) =
                claim.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20)
            else {
                return TxResult::Malformed;
            };
            let pool = claim.get("SignatureReward").cloned().unwrap_or_default();
            return finalize_claim(
                sandbox, &door, &d, &owner, &amount, &pool, &rewards, ckey, &claim,
            );
        }
        TxResult::Success
    }
}

pub struct XChainClaimTransactor;

impl Transactor for XChainClaimTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainClaim");
        if tx.fields.get("XChainClaimID").is_none()
            || tx.fields.get("Destination").is_none()
            || tx.fields.get("Amount").is_none()
        {
            return TxResult::Malformed;
        }
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let Some((_bkey, bridge)) = spec.read_bridge(sandbox) else {
            return TxResult::NoEntry;
        };
        let Some(door) = bridge.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        let claim_id = tx
            .fields
            .get("XChainClaimID")
            .and_then(|v| v.as_str())
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .unwrap_or(0);
        let ckey = spec.claim_id_key(0x51, claim_id);
        let Some(claim) = ox::json_at(sandbox, &ckey) else {
            return TxResult::XChainNoClaimId;
        };
        // Only the claim's owner may claim explicitly.
        let owner_ok = claim
            .get("Account")
            .and_then(|v| v.as_str())
            .and_then(ox::decode20)
            .map(|o| o == tx.account)
            .unwrap_or(false);
        if !owner_ok {
            return TxResult::NoPermission;
        }
        let Some((weights, quorum)) = door_signers(sandbox, &door) else {
            return TxResult::XChainNoSignersList;
        };
        let atts = claim
            .get("XChainClaimAttestations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let amount = tx.fields.get("Amount").cloned().unwrap_or_default();
        let source = claim
            .get("OtherChainSource")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Explicit claim: dst comes from THIS tx, so attested dst need not
        // match (CheckDst::Ignore) — try both chain directions like rippled's
        // match against the stored attestation set.
        let rewards = quorum_rewards(&atts, &weights, quorum, &amount, &source, 1, None)
            .or_else(|| quorum_rewards(&atts, &weights, quorum, &amount, &source, 0, None));
        let Some(rewards) = rewards else { return TxResult::XChainClaimNoQuorum };
        let Some(dst) =
            tx.fields.get("Destination").and_then(|v| v.as_str()).and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        let pool = claim.get("SignatureReward").cloned().unwrap_or_default();
        finalize_claim(sandbox, &door, &dst, &tx.account, &amount, &pool, &rewards, ckey, &claim)
    }
}

pub struct XChainAddAccountCreateAttestationTransactor;

impl Transactor for XChainAddAccountCreateAttestationTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        xchain_preflight!(tx, "XChainAddAccountCreateAttestation");
        TxResult::Success
    }
    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        account_exists(sandbox, tx)
    }
    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        // Account-create attestations accumulate on an
        // XChainOwnedCreateAccountClaimID (space 'K', keyed by the bridge's
        // XChainAccountCreateCount order) and at quorum CREATE the destination
        // account funded from the door. The full ordered-claim machinery
        // (create counts, out-of-order buffering) is ported only to the
        // refusal edges — the success path needs a bridge, a signer list, and
        // a matching create count, none of which can exist on today's mainnet.
        // The first real specimen decides the rest.
        let Some(spec) = bridge_spec(tx) else { return TxResult::Malformed };
        let Some((_bkey, bridge)) = spec.read_bridge(sandbox) else {
            return TxResult::NoEntry;
        };
        let Some(door) = bridge.get("Account").and_then(|v| v.as_str()).and_then(ox::decode20)
        else {
            return TxResult::Malformed;
        };
        if door_signers(sandbox, &door).is_none() {
            return TxResult::XChainNoSignersList;
        }
        let signer = tx
            .fields
            .get("AttestationSignerAccount")
            .and_then(|v| v.as_str())
            .and_then(ox::decode20);
        let Some(signer) = signer else { return TxResult::Malformed };
        let (weights, _q) = door_signers(sandbox, &door).unwrap_or_default();
        if !weights.contains_key(&signer) {
            return TxResult::XChainProofUnknownKey;
        }
        // Beyond this point the ordered create-count machinery applies; no
        // reachable state exists to verify it against. Refuse honestly rather
        // than invent: the probe reports the divergence the day one lands.
        TxResult::XChainNoClaimId
    }
}
