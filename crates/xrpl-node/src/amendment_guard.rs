//! The amendment guard (independence plan, Step 1.1): our own "amendment blocked".
//!
//! The vote side (validation.rs) says yes only to rules the engine has built. The guard watches the other side:
//! what mainnet has actually switched on. After every ledger the native shadow applies, it reads the ledger's
//! Amendments object (keylet 7DB0788C…: `Amendments` = the enabled IDs, `Majorities` = those with majority)
//! and compares it with [`ENGINE_KNOWN`]:
//!   - an enabled amendment on neither `ENGINE_KNOWN` nor [`KNOWN_GAP`] is an ALARM: the engine is applying
//!     ledgers under a rule it has not built (the API, the metrics, and the copilot's page);
//!   - one on `KNOWN_GAP` is a known gap (live, not yet ported): shown, not alarmed;
//!   - an amendment with majority that the engine does not know is a warning: two weeks until it switches on,
//!     and that is the porting deadline.
//!
//! From Step 5 on, [`blocks_own_hash`] also stops m3060 signing a hash it computed itself. Today it only
//! reports.

use std::sync::Mutex;

use serde_json::{json, Value};

/// Every amendment enabled on mainnet at ledger 107,606,611 (2026-10-12; xrpld 3.4.1's `feature` names all
/// 97), plus fixCleanup3_4_0: majority 2026-10-09, activation ~2026-10-23, ported (F338, F420; the
/// unported parts are tem-only checks, Sponsor role signatures, Vault/Loan, OfferStream's corrupt-book
/// throw and the invariants, see xrpl-ledger amendments.rs). Add an amendment here only when its rules are
/// ported, with the commit that ported them.
pub const ENGINE_KNOWN: &[&str] = &[
    "AMM", "AMMClawback", "BatchV1_1", "CheckCashMakesTrustLine", "Checks", "Clawback", "Credentials",
    "CryptoConditions", "DID", "DeepFreeze", "DeletableAccounts", "DepositAuth", "DepositPreauth",
    "DisallowIncoming", "DynamicNFT", "EnforceInvariants", "Escrow", "ExpandedSignerList", "FeeEscalation", "Flow",
    "FlowCross", "FlowSortStrands", "HardenedValidations", "ImmediateOfferKilled", "MPTokensV1", "MultiSign",
    "MultiSignReserve", "NFTokenMintOffer", "NegativeUNL", "NonFungibleTokensV1_1", "PayChan",
    "PermissionDelegationV1_1", "PermissionedDEX", "PermissionedDomains", "PriceOracle", "RequireFullyCanonicalSig",
    "SortedDirectories", "TickSize", "TicketBatch", "TokenEscrow", "TrustSetAuth", "XRPFees", "fix1201", "fix1368",
    "fix1373", "fix1512", "fix1513", "fix1515", "fix1523", "fix1528", "fix1543", "fix1571", "fix1578", "fix1623",
    "fix1781", "fixAMMClawbackRounding", "fixAMMOverflowOffer", "fixAMMv1_1", "fixAMMv1_2", "fixAMMv1_3",
    "fixAmendmentMajorityCalc", "fixBatchV1_2", "fixCheckThreading", "fixCleanup3_1_3", "fixCleanup3_2_0",
    "fixCleanup3_3_0", "fixDirectoryLimit", "fixDisallowIncomingV1", "fixEmptyDID", "fixEnforceNFTokenTrustline",
    "fixEnforceNFTokenTrustlineV2", "fixFillOrKill", "fixFrozenLPTokenTransfer", "fixIncludeKeyletFields",
    "fixInnerObjTemplate", "fixInnerObjTemplate2", "fixInvalidTxFlags", "fixMPTDeliveredAmount",
    "fixMasterKeyAsRegularKey", "fixNFTokenPageLinks", "fixNFTokenRemint", "fixNFTokenReserve",
    "fixNonFungibleTokensV1_2", "fixPayChanCancelAfter", "fixPayChanRecipientOwnerDir", "fixPreviousTxnID",
    "fixPriceOracleOrder", "fixQualityUpperBound", "fixReducedOffersV1", "fixReducedOffersV2",
    "fixRemoveNFTokenAutoTrustLine", "fixRmSmallIncreasedQOffers", "fixSTAmountCanonicalize",
    "fixTakerDryOfferRemoval", "fixTokenEscrowV1", "fixTrustLinesToSelf", "fixUniversalNumber",
    // Not yet enabled on mainnet; ported.
    "fixCleanup3_4_0",
];

/// Enabled on mainnet but not yet ported: shown, not alarmed. Empty since fixBatchV1_2 was ported (cycle 168).
pub const KNOWN_GAP: &[&str] = &[];

/// Amendments xrpld 3.4.1 knows that the engine has not built, named only so a page can say which one it is.
const OTHER_NAMES: &[&str] = &[
    "ConfidentialTransfer", "CryptoConditionsSuite", "DynamicMPT", "LendingProtocol", "LendingProtocolV1_1",
    "SingleAssetVault", "Sponsor", "XChainBridge", "fixXChainRewardRounding",
];

/// An amendment's 256-bit ID: SHA-512Half of its name (rippled `Feature.cpp`).
pub fn amendment_id(name: &str) -> String {
    hex::encode_upper(xrpl_core::crypto::signing::sha512_half(name.as_bytes()))
}

fn name_of(id: &str) -> Option<&'static str> {
    ENGINE_KNOWN.iter().chain(KNOWN_GAP).chain(OTHER_NAMES).copied().find(|n| amendment_id(n).eq_ignore_ascii_case(id))
}

/// What one ledger's Amendments object says against a known list and a gap list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardReport {
    pub seq: u32,
    pub enabled: usize,
    /// Enabled, on neither list: the alarm. Names where known, else the ID.
    pub unknown_enabled: Vec<String>,
    /// Enabled and on the gap list.
    pub gap_enabled: Vec<String>,
    /// With majority, not known: (name or ID, the majority's close time).
    pub majority_unknown: Vec<(String, u64)>,
}

impl GuardReport {
    pub fn status(&self) -> &'static str {
        if !self.unknown_enabled.is_empty() {
            "alarm"
        } else if !self.gap_enabled.is_empty() {
            "gap"
        } else if !self.majority_unknown.is_empty() {
            "majority"
        } else {
            "ok"
        }
    }
}

/// Judge an Amendments object (engine JSON) against `known` and `gap` (names).
pub fn assess_with(seq: u32, sle: &Value, known: &[&str], gap: &[&str]) -> GuardReport {
    let ids = |names: &[&str]| -> Vec<String> { names.iter().map(|n| amendment_id(n)).collect() };
    let (known, gap) = (ids(known), ids(gap));
    let has = |set: &[String], id: &str| set.iter().any(|k| k.eq_ignore_ascii_case(id));
    let label = |id: &str| name_of(id).map(str::to_string).unwrap_or_else(|| id.to_uppercase());
    let enabled: Vec<&str> = sle["Amendments"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    let mut r = GuardReport { seq, enabled: enabled.len(), ..Default::default() };
    for id in &enabled {
        if has(&gap, id) {
            r.gap_enabled.push(label(id));
        } else if !has(&known, id) {
            r.unknown_enabled.push(label(id));
        }
    }
    for m in sle["Majorities"].as_array().into_iter().flatten() {
        let m = &m["Majority"];
        let Some(id) = m["Amendment"].as_str() else { continue };
        if !has(&known, id) && !has(&gap, id) {
            r.majority_unknown.push((label(id), m["CloseTime"].as_u64().unwrap_or(0)));
        }
    }
    r
}

pub fn assess(seq: u32, sle: &Value) -> GuardReport {
    assess_with(seq, sle, ENGINE_KNOWN, KNOWN_GAP)
}

static LAST: Mutex<Option<GuardReport>> = Mutex::new(None);

/// Record ledger `seq`'s Amendments object (its engine-JSON bytes, None when the state has none). Logs only
/// when the verdict changes, so a reading every ledger costs one small parse.
pub fn observe(seq: u32, sle_json: Option<&[u8]>) {
    let Some(sle) = sle_json.and_then(|b| serde_json::from_slice::<Value>(b).ok()) else { return };
    let r = assess(seq, &sle);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let changed = last.as_ref().is_none_or(|l| (&l.unknown_enabled, &l.gap_enabled, &l.majority_unknown) != (&r.unknown_enabled, &r.gap_enabled, &r.majority_unknown));
    if changed {
        match r.status() {
            "alarm" => eprintln!("[amendment-guard] ⚠ ALARM #{seq}: enabled on mainnet, not built by the engine: {}", r.unknown_enabled.join(", ")),
            s => eprintln!("[amendment-guard] #{seq}: {s} ({} enabled; gap {:?}; majority, not built {:?})", r.enabled, r.gap_enabled, r.majority_unknown),
        }
    }
    *last = Some(r);
}

/// From Step 5 on: an amendment the engine has not built is switched on, so m3060 must not sign a hash it
/// computed itself. No reading yet also blocks.
pub fn blocks_own_hash() -> bool {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|r| !r.unknown_enabled.is_empty() || !r.gap_enabled.is_empty())
}

/// The guard's part of `/api/amendments`.
pub fn json() -> Value {
    match LAST.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        None => json!({"status": "unknown", "detail": "no ledger read yet"}),
        Some(r) => json!({
            "status": r.status(),
            "seq": r.seq,
            "enabled": r.enabled,
            "known": ENGINE_KNOWN.len(),
            "unknown_enabled": r.unknown_enabled,
            "gap_enabled": r.gap_enabled,
            "majority_unknown": r.majority_unknown.iter().map(|(n, t)| json!({"name": n, "majority_close_time": t})).collect::<Vec<_>>(),
        }),
    }
}

/// Extra /metrics lines.
pub fn metrics_text() -> String {
    let g = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (seen, alarm, gap, maj, seq) = match &g {
        None => (0, 0, 0, 0, 0),
        Some(r) => (1, r.unknown_enabled.len(), r.gap_enabled.len(), r.majority_unknown.len(), r.seq),
    };
    let mut out = String::new();
    for (name, help, v) in [
        ("xrpl_amendment_guard_read", "1 once the guard has read a ledger's Amendments object.", seen as u64),
        ("xrpl_amendment_guard_unknown_enabled", "Amendments enabled on mainnet that the engine has not built (the alarm).", alarm as u64),
        ("xrpl_amendment_guard_gap_enabled", "Enabled amendments on the known-gap list.", gap as u64),
        ("xrpl_amendment_guard_majority_unknown", "Amendments with majority that the engine has not built.", maj as u64),
        ("xrpl_amendment_guard_seq", "The ledger the guard last read.", u64::from(seq)),
    ] {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = include_str!("../tests/vectors/amendments_107606611.json");

    fn live() -> Value {
        serde_json::from_str::<Value>(LIVE).unwrap()["node"].clone()
    }

    #[test]
    fn ids_are_the_sha512half_of_the_name() {
        assert_eq!(amendment_id("fixCleanup3_4_0"), "98433DD001A5737F773D74F8CA2A25A065089C73B2E611C760BAF369E4FECA76");
        assert_eq!(amendment_id("fixCleanup3_3_0"), xrpl_ledger::ledger::amendments::FIX_CLEANUP_3_3_0);
        assert_eq!(amendment_id("fixCleanup3_2_0"), xrpl_ledger::ledger::amendments::FIX_CLEANUP_3_2_0);
        assert_eq!(amendment_id("fixCleanup3_4_0"), xrpl_ledger::ledger::amendments::FIX_CLEANUP_3_4_0);
    }

    #[test]
    fn mainnet_today_is_all_known_and_the_coming_one_is_built() {
        let r = assess(107_606_611, &live());
        assert_eq!(r.enabled, 97);
        assert_eq!(r.status(), "ok", "{r:?}");
        assert!(r.majority_unknown.is_empty(), "fixCleanup3_4_0 has majority and is ported: {r:?}");
    }

    #[test]
    fn an_enabled_amendment_the_engine_has_not_built_raises_the_alarm() {
        // 9 October: fixBatchV1_2 switched on before its source was out. Off the known list, it must alarm.
        let known: Vec<&str> = ENGINE_KNOWN.iter().copied().filter(|n| *n != "fixBatchV1_2").collect();
        let r = assess_with(107_606_611, &live(), &known, &[]);
        assert_eq!(r.unknown_enabled, vec!["fixBatchV1_2".to_string()]);
        assert_eq!(r.status(), "alarm");
        // On the gap list it is shown, not alarmed.
        let r = assess_with(107_606_611, &live(), &known, &["fixBatchV1_2"]);
        assert!(r.unknown_enabled.is_empty());
        assert_eq!((r.gap_enabled.clone(), r.status()), (vec!["fixBatchV1_2".to_string()], "gap"));
    }

    #[test]
    fn a_majority_the_engine_has_not_built_is_a_warning_named_by_its_amendment() {
        let known: Vec<&str> = ENGINE_KNOWN.iter().copied().filter(|n| *n != "fixCleanup3_4_0").collect();
        let r = assess_with(107_606_611, &live(), &known, &[]);
        assert_eq!(r.majority_unknown, vec![("fixCleanup3_4_0".to_string(), 844_853_670)]);
        assert_eq!(r.status(), "majority");
        // An ID no list names is shown as its ID.
        let mut sle = live();
        sle["Amendments"].as_array_mut().unwrap().push(json!("00".repeat(32)));
        let r = assess(1, &sle);
        assert_eq!(r.unknown_enabled, vec!["00".repeat(32)]);
    }

    #[test]
    fn every_rule_we_vote_for_or_gate_on_is_known() {
        for n in crate::validation::SUPPORTED_AMENDMENT_NAMES {
            assert!(ENGINE_KNOWN.contains(n), "we vote yes for {n} but the guard does not know it");
        }
        for (n, _) in crate::validation::READY_PENDING {
            assert!(ENGINE_KNOWN.contains(n), "{n} is ready-pending but not known");
        }
        assert!(KNOWN_GAP.iter().all(|g| !ENGINE_KNOWN.contains(g)), "a gap is by definition not built");
        assert!(OTHER_NAMES.iter().all(|o| !ENGINE_KNOWN.contains(o)));
    }

    #[test]
    fn every_metrics_line_starts_at_the_margin() {
        // 2026-10-13: the first build's lines carried the source's indentation (a shell heredoc ate the
        // string continuations), so every line after the first began with spaces.
        let m = metrics_text();
        assert!(m.lines().all(|l| l.starts_with("# ") || l.starts_with("xrpl_amendment_guard_")), "{m}");
        assert_eq!(m.lines().count(), 15);
    }
}
