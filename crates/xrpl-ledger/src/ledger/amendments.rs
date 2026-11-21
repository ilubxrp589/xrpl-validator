//! Amendment state, read from the ledger's own Amendments singleton
//! (keylet 7DB0788C…, `sfAmendments` = the enabled IDs) — rippled's
//! `Rules::enabled`. A state that carries no Amendments object (a bundle
//! drill's seated pre-images, a synthetic test ledger) answers "not
//! enabled", so every pre-amendment specimen keeps the rules it was pinned
//! under and the live mirror applies the amendment from the flag ledger
//! that enabled it.
//!
//! Finding 259: fixCleanup3_3_0 went live on mainnet at flag ledger
//! 106912768 (2026-09-11 ~12:51 UTC) with deploy117 already running. Until
//! then the engine hard-coded the pre-amendment rules it had specimens for.

use super::keylet;
use super::sandbox::Sandbox;
use crate::tx::number::{MantissaScale, ScaleGuard};

/// fixCleanup3_3_0 — AMM deposit/withdraw freeze scope, AMM precision-loss
/// guard, CheckID zero, pseudo-account signers, domain-book validation.
pub const FIX_CLEANUP_3_3_0: &str =
    "3298D47E1F3A8A24FECAA30F699B8FE1DD234E072834BA099AD8180FFCE0FEC4";

/// Whether the amendment with this 256-bit ID (upper-case hex) is enabled
/// in the ledger the sandbox is built on.
pub fn enabled(sandbox: &Sandbox, id_hex: &str) -> bool {
    let Some(bytes) = sandbox.read(&keylet::amendments_key()) else { return false };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false };
    v.get("Amendments")
        .and_then(|a| a.as_array())
        .is_some_and(|a| a.iter().any(|x| x.as_str().is_some_and(|s| s.eq_ignore_ascii_case(id_hex))))
}

/// fixCleanup3_1_3 — `XRPL_FIX(Cleanup3_1_3, Supported::Yes, DefaultYes)`
/// (features.macro:27); the ID is sha512Half("fixCleanup3_1_3"). Named where
/// the vault helpers branch on it: an MPToken's sfLockedAmount counts as an
/// obligation in `removeEmptyHolding` / `authorizeMPToken`
/// (MPTokenHelpers.cpp:227-229, :349-351). Its mainnet status is not
/// recorded in this tree (inventory Q1), so both arms are carried.
pub const FIX_CLEANUP_3_1_3: &str =
    "303ACB16CF8DBD3B5C34F131A9D19A7DE01AE05F480A8A682B869D1B4AAC8CFC";

/// fixCleanup3_2_0 — enabled on mainnet. Gates LedgerStateFix's
/// BookExchangeRate fix type (among other 3.2.0 cleanups ported earlier).
pub const FIX_CLEANUP_3_2_0: &str =
    "21B8D2F76F68E11E9C077A43BBBC394136E9987E99DDB73966DD68419467E431";

pub fn fix_cleanup_3_2_0(sandbox: &Sandbox) -> bool {
    enabled(sandbox, FIX_CLEANUP_3_2_0)
}

pub fn fix_cleanup_3_3_0(sandbox: &Sandbox) -> bool {
    enabled(sandbox, FIX_CLEANUP_3_3_0)
}

/// fixCleanup3_4_0 — in the 3.4.0 release, unchanged in 3.4.1 (the vendored
/// libxrpl snapshot predates it; mainnet majority 2026-10-09 09:34 UTC, so
/// activation ~2026-10-23, and until then every gated site rests on unit
/// tests). Ported sites: MPT escrow fee floor (F338), escrow reserve
/// recycling on Finish/Cancel, domain validDomain/verifyValidDomain with
/// expired-credential deletion (OfferCreate, Payment), pseudo-account
/// requireAuth — in `require_auth_ter`, the model's `require_auth_known` and
/// the port's `flow::view::is_authorized` (F420: the book walk asks by the
/// last two) — OfferCreate's DisallowIncomingTrustline, the NFT issuer's
/// own-freeze exemption, MPTokenAuthorize's locked-token refusal, AMMBid's
/// zero-fee floor, AMMClawback's three tweaks (amm.rs), a delegate's
/// PaymentBurn cap (delegate.rs). AMMDeposit/Withdraw's overflow →
/// tecAMM_FAILED is the engine's behaviour ungated (amm.rs `nerr`).
/// Deliberately unported: tem-only checks (zero CredentialIDs, badAsset),
/// role signatures (Sponsor), Vault/Loan (both OFF on mainnet), OfferStream's
/// corrupt-domain-book throw, the invariant changes (no invariant layer; the
/// loosened ones already read post-fix).
pub const FIX_CLEANUP_3_4_0: &str =
    "98433DD001A5737F773D74F8CA2A25A065089C73B2E611C760BAF369E4FECA76";

pub fn fix_cleanup_3_4_0(sandbox: &Sandbox) -> bool {
    enabled(sandbox, FIX_CLEANUP_3_4_0)
}

/// fixCleanup3_5_0 — rippled 3.5.0-rc1 (`XRPL_FIX(Cleanup3_5_0, Supported::Yes, DefaultNo)`), unknown to
/// 3.4.1: OFF on mainnet, ON on devnet (3.5.0-rc5, read 2026-11-21). The ID is
/// sha512Half("fixCleanup3_5_0"). Ported sites, in the vault transactors (`tx::vault`):
/// VaultCreate's AssetsMaximum range check (temMALFORMED), VaultSet's (tecPRECISION_LOSS),
/// VaultClawback's missing holder (terNO_ACCOUNT), VaultWithdraw's transfer fee between two
/// holders; and in the loan-broker transactors (`tx::loan_broker`): LoanBrokerSet's DebtMaximum
/// range check (tecPRECISION_LOSS), LoanBrokerDelete's transfer / authorization checks on the
/// cover payout, its owner count released before the payout and the owner's holding created
/// for it; and in the loan transactors (`tx::loan`): LoanSet's out-of-range value fields refused
/// tecPRECISION_LOSS instead of thrown. It also stands in — our choice, not a gate of rippled's —
/// for "this ledger is made by 3.5.x servers" in LoanPay's fund-conservation balances
/// (`tx::loan::ConservationModel`), which 3.5.0 changed without an amendment. Nothing else of
/// 3.5.0 is ported.
pub const FIX_CLEANUP_3_5_0: &str =
    "7300E10109D19BF1E87ACE63D3A79CD4ED6B9851C37D93ED94DE9BD41CF56835";

pub fn fix_cleanup_3_5_0(sandbox: &Sandbox) -> bool {
    enabled(sandbox, FIX_CLEANUP_3_5_0)
}

/// MPTokensV1 — on mainnet. VaultCreate needs it (`VaultCreate::checkExtraFeatures`,
/// VaultCreate.cpp:41-42): a vault's shares are an MPT.
pub const MPTOKENS_V1: &str =
    "950AE2EA4654E47F04AA8739C0B214E242097E802FD372D24047A89AB1F5EC38";

/// Credentials — on mainnet. VaultWithdraw's sfCredentialIDs needs it and fixCleanup3_4_0
/// (`VaultWithdraw::checkExtraFeatures`, VaultWithdraw.cpp:33-38).
pub const CREDENTIALS: &str =
    "1CB67D082CF7D9102412D34258CEDB400E659352D3B207348889297A6D90F5EF";

/// fixBatchV1_2 — rippled 3.4.1 (`features.macro`: `XRPL_FIX(BatchV1_2, …)`,
/// so the name hashed is "fixBatchV1_2"). ON on mainnet by #107540484
/// (2026-10-09), ahead of BatchV1_1 (#107540996), so no mainnet ledger ever
/// ran Batch without it; devnet and testnet did. Gates one rule:
/// `Batch::preflight`'s RawTransaction wrapper check (tx/batch.rs).
pub const FIX_BATCH_V1_2: &str =
    "14A2B45E48A4A124D1BBA657AC7B0DC3D5EA8C256C89E8F0D8142D32960A7944";

pub fn fix_batch_v1_2(sandbox: &Sandbox) -> bool {
    enabled(sandbox, FIX_BATCH_V1_2)
}

/// BatchV1_1 — the Batch amendment (`features.macro:21`, `XRPL_FEATURE(BatchV1_1, …)`). ON on
/// mainnet since #107540996 (2026-10-09). Named here for the one place a lending transactor reads
/// it: LoanSet's inner-batch rule (LoanSet.cpp:75-82).
pub const BATCH_V1_1: &str =
    "9F287AED3CDB50A7BD1ACEC24296A30C9B5230CCD136219317AC790E3B884377";

/// DynamicMPT — not enabled on mainnet or testnet (2026-09-23). Widens what
/// an MPTokenIssuanceSet may do on an issuance without lsfMPTCanLock.
pub const DYNAMIC_MPT: &str =
    "58E92F338758479C06084E1B6BA366BAD8F75E5329A7F0EEAFFFDA51E5106B7F";

/// SingleAssetVault / LendingProtocol — enabled on devnet, NOT on mainnet as
/// of 2026-11-21. SingleAssetVault gates the six vault transactors
/// (`tx::vault`); LendingProtocol — with SingleAssetVault and MPTokensV1 —
/// the five loan-broker transactors (`tx::loan_broker`) and the four loan
/// transactors (`tx::loan`: LoanSet, LoanDelete, LoanManage, LoanPay). Either one also
/// makes a pseudo-account (AMM, Vault, LoanBroker) start at Sequence 0
/// instead of the ledger sequence (AccountRootHelpers.cpp:581-589).
pub const SINGLE_ASSET_VAULT: &str =
    "81BD2619B6B3C8625AC5D0BC01DE17F06C3F0AB95C7C87C93715B87A4FD240D8";
pub const LENDING_PROTOCOL: &str =
    "565B90CA1AB2B9D42208ED10884188C64F9E19083DECB9634AAF06EB03299509";

/// LendingProtocolV1_1 — rippled 3.4.1 (`features.macro:22`,
/// `XRPL_FEATURE(LendingProtocolV1_1, …)`, so the name hashed is
/// "LendingProtocolV1_1"). NOT on mainnet. Vault kinds and phases, cash-basis
/// accounting for new vaults, and the Loan / Vault immutability invariants
/// (InvariantCheck.cpp:1189-1233). Read by the vault transactors (kinds,
/// phases, sfLEVersion, VaultDelete's sfMemoData), by LoanBrokerSet (a
/// broker only on a closed-ended vault, LoanBrokerSet.cpp:149-161) and by
/// LoanSet (a closed-ended vault lends in its Investment phase only, and
/// not past its redemption date, LoanSet.cpp:319-344). The loan maths'
/// accounting model follows the vault's sfLEVersion, not this amendment.
pub const LENDING_PROTOCOL_V1_1: &str =
    "A360E2BFD775A5B0DCE1C36C16DF31B72735A57584FD163655D2F9564F8E7AC8";

/// PermissionedDomains — on mainnet. With SingleAssetVault it makes
/// `sfDomainID` legal on MPTokenIssuanceCreate / MPTokenIssuanceSet
/// (MPTokenIssuanceCreate.cpp:33-36, MPTokenIssuanceSet.cpp:29-32).
pub const PERMISSIONED_DOMAINS: &str =
    "A730EB18A9D4BB52502C898589558B4CCEB4BE10044500EE5581137A2E80E849";

/// MPTokensV2 — `Supported::No` in 3.4.1 (features.macro:26), on no network.
/// Named only where rippled ORs it into a SingleAssetVault gate (MPT
/// `requireAuth`'s pseudo-account exemption, MPTokenHelpers.cpp:388-389);
/// nothing else of it is ported.
pub const MPTOKENS_V2: &str =
    "BE2D87DF21B690ED1497B593FDC013CC04276302380B1BD50A033DCF8DEFB2EB";

/// ConfidentialTransfer — not on mainnet. Named only where rippled ORs it
/// into a SingleAssetVault gate (MPT `accountHolds`, TokenHelpers.cpp:452-455;
/// MPTokenIssuanceSet's no-op refusal, MPTokenIssuanceSet.cpp:82-89) and
/// where its fields are refused without it (MPTokenIssuanceSet.cpp:61-66).
pub const CONFIDENTIAL_TRANSFER: &str =
    "2110E4A19966E2EF517C0A8C56A5F35099D7665B0BB89D7B126B30D50B86AAD5";

/// XRPFees — on mainnet: fees and reserves in drops (BaseFeeDrops, ReserveBaseDrops, ReserveIncrementDrops)
/// replace the legacy fee units. Gates SetFee's fields (Change::preclaim, Change::applyFee).
pub const XRP_FEES: &str =
    "93E516234E35E08CA689FA33A6D38E103881F8DCB53023F728C307AA89D515A7";

/// rippled `createPseudoAccount`'s sequence rule: 0 once SingleAssetVault or
/// LendingProtocol is enabled, else the sequence of the ledger being built.
pub fn pseudo_account_sequence_is_zero(sandbox: &Sandbox) -> bool {
    enabled(sandbox, SINGLE_ASSET_VAULT) || enabled(sandbox, LENDING_PROTOCOL)
}

/// The mantissa scale `Number` runs at in the ledger the sandbox is built
/// on — rippled `setCurrentTransactionRules` (Rules.cpp:35-74): the 16-digit
/// Small scale while neither SingleAssetVault nor LendingProtocol is
/// enabled, the 19-digit Large330 scale once either is. (rippled's two older
/// Large scales need fixCleanup3_3_0 or fixCleanup3_2_0 OFF with a vault
/// amendment ON; both fixes are on at mainnet and those scales are not
/// ported — see `tx::number`.) The day either amendment activates, every
/// transaction's IOU and AMM arithmetic changes, vault transaction or not.
pub fn number_scale(sandbox: &Sandbox) -> MantissaScale {
    MantissaScale::for_rules(enabled(sandbox, SINGLE_ASSET_VAULT), enabled(sandbox, LENDING_PROTOCOL))
}

/// rippled's `CurrentTransactionRulesGuard` as far as `Number` goes
/// (Transactor.cpp:1562 for doApply; applySteps.cpp:60-75 for preflight,
/// preclaim and the fee): hold the returned guard for the whole of one
/// transaction's application and its arithmetic runs at the ledger's scale.
/// Taken at each outer entry (`ledger::apply`, `tx::dispatch`, the node's
/// `native_apply`); a Batch inner taking it again inside is harmless.
pub fn number_scale_guard(sandbox: &Sandbox) -> ScaleGuard {
    ScaleGuard::new(number_scale(sandbox))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn test_state() -> LedgerState {
        let header = LedgerHeader {
            sequence: 1,
            total_coins: 100_000_000_000_000_000,
            parent_hash: Hash256([0; 32]),
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 10,
            close_time_resolution: 10,
            close_flags: 0,
        };
        LedgerState::new_unverified(header)
    }

    #[test]
    fn reads_the_amendments_singleton_and_defaults_to_off() {
        let mut state = test_state();
        assert!(!fix_cleanup_3_3_0(&Sandbox::new(&state)), "no singleton → off");
        let obj = serde_json::json!({
            "LedgerEntryType": "Amendments",
            "Flags": 0,
            "Amendments": ["00000000000000000000000000000000000000000000000000000000000000AA", FIX_CLEANUP_3_3_0.to_lowercase()],
        });
        state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&obj).unwrap()).unwrap();
        assert!(fix_cleanup_3_3_0(&Sandbox::new(&state)));
        assert!(!enabled(&Sandbox::new(&state), "00000000000000000000000000000000000000000000000000000000000000BB"));
    }

    /// Rules.cpp:43-71: Small with neither vault amendment, Large330 with
    /// either — and the guard holds that scale for its scope only.
    #[test]
    fn number_scale_follows_the_two_vault_amendments() {
        use crate::tx::number::mantissa_scale;
        let with = |ids: &[&str]| {
            let mut state = test_state();
            let obj = serde_json::json!({ "LedgerEntryType": "Amendments", "Flags": 0, "Amendments": ids });
            state.state_map.insert(keylet::amendments_key(), serde_json::to_vec(&obj).unwrap()).unwrap();
            state
        };
        assert_eq!(number_scale(&Sandbox::new(&test_state())), MantissaScale::Small, "no singleton");
        // Mainnet today: the cleanups are on, the vault amendments are not.
        let mainnet = with(&[FIX_CLEANUP_3_2_0, FIX_CLEANUP_3_3_0, FIX_CLEANUP_3_4_0]);
        assert_eq!(number_scale(&Sandbox::new(&mainnet)), MantissaScale::Small);
        for ids in [vec![SINGLE_ASSET_VAULT], vec![LENDING_PROTOCOL], vec![SINGLE_ASSET_VAULT, LENDING_PROTOCOL]] {
            let mut all = vec![FIX_CLEANUP_3_2_0, FIX_CLEANUP_3_3_0];
            all.extend(ids);
            let state = with(&all);
            assert_eq!(number_scale(&Sandbox::new(&state)), MantissaScale::Large330, "{all:?}");
            assert_eq!(mantissa_scale(), MantissaScale::Small);
            {
                let _scale = number_scale_guard(&Sandbox::new(&state));
                assert_eq!(mantissa_scale(), MantissaScale::Large330);
            }
            assert_eq!(mantissa_scale(), MantissaScale::Small);
        }
    }

    /// The IDs named for the vault port are the hashes of their registered
    /// names (features.macro: `XRPL_FEATURE(Name, …)` hashes "Name").
    #[test]
    fn the_vault_port_amendment_ids_are_the_hashes_of_their_names() {
        for (name, id) in [
            ("SingleAssetVault", SINGLE_ASSET_VAULT),
            ("LendingProtocol", LENDING_PROTOCOL),
            ("LendingProtocolV1_1", LENDING_PROTOCOL_V1_1),
            ("PermissionedDomains", PERMISSIONED_DOMAINS),
            ("MPTokensV2", MPTOKENS_V2),
            ("ConfidentialTransfer", CONFIDENTIAL_TRANSFER),
            ("DynamicMPT", DYNAMIC_MPT),
            ("fixCleanup3_5_0", FIX_CLEANUP_3_5_0),
            ("fixCleanup3_1_3", FIX_CLEANUP_3_1_3),
            ("fixCleanup3_2_0", FIX_CLEANUP_3_2_0),
            ("fixCleanup3_3_0", FIX_CLEANUP_3_3_0),
            ("fixCleanup3_4_0", FIX_CLEANUP_3_4_0),
            ("MPTokensV1", MPTOKENS_V1),
            ("Credentials", CREDENTIALS),
            ("BatchV1_1", BATCH_V1_1),
        ] {
            let h = crate::shamap::hash::sha512_half(name.as_bytes());
            assert_eq!(hex::encode_upper(h.0), id, "{name}");
        }
    }

    /// An amendment's ID is SHA-512Half of its registered name; fixBatchV1_2's
    /// is the one mainnet's `feature` RPC reported when it activated.
    #[test]
    fn fix_batch_v1_2_is_the_hash_of_its_name() {
        let id = crate::shamap::hash::sha512_half(b"fixBatchV1_2");
        assert_eq!(hex::encode_upper(id.0), FIX_BATCH_V1_2);
    }
}
