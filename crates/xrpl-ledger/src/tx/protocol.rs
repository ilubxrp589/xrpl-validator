//! Protocol constants, flags, units and ledger-entry templates of the vault
//! and lending amendments (SingleAssetVault, LendingProtocol,
//! LendingProtocolV1_1), from rippled 3.4.1:
//!
//! - `include/xrpl/protocol/Protocol.h` — basis-point units (:101-141), the
//!   lending limits (:143-240), `kMaxDataPayloadLength` (:302), the vault
//!   constants (:304-318), `VaultVersion` / `VaultKind` / `VaultPhase`
//!   (:320-349), the closed-ended bounds (:351-370), `kMaxAssetCheckDepth`
//!   (:376);
//! - `include/xrpl/protocol/LedgerFormats.h:201-207` and
//!   `include/xrpl/protocol/TxFlags.h:196-226` — the lsf* and tf* flags;
//! - `include/xrpl/protocol/detail/ledger_entries.macro:492-626` — the field
//!   templates of ltVAULT, ltLOAN_BROKER and ltLOAN.
//!
//! Nothing here runs for a transaction the engine dispatches today: the
//! vault and loan transactors are the first users.

use crate::tx::number::{Number, NumberError, Rounding};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Basis points (Protocol.h:101-141, Units.h)
// ---------------------------------------------------------------------------

/// `kBipsPerUnity` (Protocol.h:115): 10,000 basis points make 100%.
pub const BIPS_PER_UNITY: u32 = 100 * 100;
/// `kTenthBipsPerUnity` (Protocol.h:117): 100,000 tenths of a basis point
/// make 100%. Rates on LoanBroker and Loan are in this unit.
pub const TENTH_BIPS_PER_UNITY: u32 = BIPS_PER_UNITY * 10;

/// `percentageToBips` (Protocol.h:120-124): integer arithmetic,
/// `percentage * 10,000 / 100`.
pub const fn percentage_to_bips(percentage: u32) -> u32 {
    percentage * BIPS_PER_UNITY / 100
}

/// `percentageToTenthBips` (Protocol.h:125-129).
pub const fn percentage_to_tenth_bips(percentage: u32) -> u32 {
    percentage * TENTH_BIPS_PER_UNITY / 100
}

/// `bipsOfValue<Number>(value, bips)` (Protocol.h:130-135):
/// `value * bips / kBipsPerUnity`, evaluated left to right as two `Number`
/// operations — the product first, then the quotient — each under `mode`
/// (rippled's thread rounding mode at the call).
pub fn bips_of_value(value: Number, bips: u32, mode: Rounding) -> Result<Number, NumberError> {
    let product = value.mul(Number::from_parts(false, bips as u128, 0, mode)?, mode)?;
    product.div(Number::from_parts(false, BIPS_PER_UNITY as u128, 0, mode)?, mode)
}

/// `tenthBipsOfValue<Number>(value, bips)` (Protocol.h:136-141):
/// `value * tenthBips / kTenthBipsPerUnity`, product first, under `mode`.
pub fn tenth_bips_of_value(value: Number, tenth_bips: u32, mode: Rounding) -> Result<Number, NumberError> {
    let product = value.mul(Number::from_parts(false, tenth_bips as u128, 0, mode)?, mode)?;
    product.div(Number::from_parts(false, TENTH_BIPS_PER_UNITY as u128, 0, mode)?, mode)
}

// ---------------------------------------------------------------------------
// Lending limits (Protocol.h:143-240, namespace `lending`)
// ---------------------------------------------------------------------------

/// `lending::kMaxManagementFeeRate` (Protocol.h:149-151): 10%, in 1/10 bips
/// (a UINT16 field).
pub const MAX_MANAGEMENT_FEE_RATE: u16 = percentage_to_tenth_bips(10) as u16;
/// `lending::kMaxCoverRate` (Protocol.h:158): 100% in 1/10 bips.
pub const MAX_COVER_RATE: u32 = percentage_to_tenth_bips(100);
/// `lending::kMaxOverpaymentFee` (Protocol.h:166).
pub const MAX_OVERPAYMENT_FEE: u32 = percentage_to_tenth_bips(100);
/// `lending::kMaxInterestRate` (Protocol.h:174).
pub const MAX_INTEREST_RATE: u32 = percentage_to_tenth_bips(100);
/// `lending::kMaxLateInterestRate` (Protocol.h:183).
pub const MAX_LATE_INTEREST_RATE: u32 = percentage_to_tenth_bips(100);
/// `lending::kMaxCloseInterestRate` (Protocol.h:192).
pub const MAX_CLOSE_INTEREST_RATE: u32 = percentage_to_tenth_bips(100);
/// `lending::kMaxOverpaymentInterestRate` (Protocol.h:201).
pub const MAX_OVERPAYMENT_INTEREST_RATE: u32 = percentage_to_tenth_bips(100);
/// `lending::kLoanPaymentsPerFeeIncrement` (Protocol.h:214): LoanPay costs
/// one base fee per this many combined payments.
pub const LOAN_PAYMENTS_PER_FEE_INCREMENT: u32 = 5;
/// `lending::kLoanMaximumPaymentsPerTransaction` (Protocol.h:239).
pub const LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION: u32 = 100;

// ---------------------------------------------------------------------------
// Lengths and vault constants (Protocol.h:288-376)
// ---------------------------------------------------------------------------

/// `kMaxMpTokenMetadataLength` (Protocol.h:291).
pub const MAX_MPTOKEN_METADATA_LENGTH: usize = 1024;
/// `kMaxDataPayloadLength` (Protocol.h:302): `sfData` on a Vault, a
/// LoanBroker or a Loan.
pub const MAX_DATA_PAYLOAD_LENGTH: usize = 256;
/// `kVaultStrategyFirstComeFirstServe` (Protocol.h:307): the one
/// `sfWithdrawalPolicy`.
pub const VAULT_STRATEGY_FIRST_COME_FIRST_SERVE: u8 = 1;
/// `kVaultDefaultIouScale` (Protocol.h:312): the `sfScale` of a vault of an
/// IOU when the transaction names none.
pub const VAULT_DEFAULT_IOU_SCALE: u8 = 6;
/// `kVaultMaximumIouScale` (Protocol.h:318): 10^19 > the largest MPT amount
/// > 10^18, so one IOU unit always converts to shares.
pub const VAULT_MAXIMUM_IOU_SCALE: u8 = 18;
/// `kLoanRedemptionBuffer` (Protocol.h:356): seconds a closed-ended loan's
/// last scheduled payment must precede the vault's RedemptionDate.
pub const LOAN_REDEMPTION_BUFFER: u32 = 60;
/// `kMinInvestmentPeriod` (Protocol.h:368): the least
/// `RedemptionDate - SubscriptionDate`, inclusive.
pub const MIN_INVESTMENT_PERIOD: u32 = 180;
/// `kMaxInvestmentPeriod` (Protocol.h:370): `std::chrono::years{30}` in
/// seconds — a chrono year is 365.2425 days = 31,556,952 s — exclusive.
pub const MAX_INVESTMENT_PERIOD: u32 = 30 * 31_556_952;
/// `kMaxAssetCheckDepth` (Protocol.h:376).
pub const MAX_ASSET_CHECK_DEPTH: u8 = crate::tx::mpt::MAX_ASSET_CHECK_DEPTH;

/// `VaultVersion` (Protocol.h:326-329): the vault's ledger-entry schema,
/// `sfLEVersion`. Absent is Legacy (accrual-basis accounting); vaults
/// created under LendingProtocolV1_1 are CashBasis.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum VaultVersion {
    Legacy = 0,
    CashBasis = 1,
}

/// `VaultKind` (Protocol.h:335-338): `sfVaultKind`; absent is OpenEnded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum VaultKind {
    OpenEnded = 0,
    ClosedEnded = 1,
}

/// `VaultPhase` (Protocol.h:344-349): an open-ended vault is always NoPhase;
/// the other three are a closed-ended vault's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum VaultPhase {
    NoPhase = 0,
    Subscription = 1,
    Investment = 2,
    Redemption = 3,
}

// ---------------------------------------------------------------------------
// Flags (LedgerFormats.h:184-207, TxFlags.h:45-46, :196-226, :264)
// ---------------------------------------------------------------------------

/// `tfUniversal` (TxFlags.h:45): `tfFullyCanonicalSig | tfInnerBatchTxn`.
pub const TF_UNIVERSAL: u64 = 0x8000_0000 | 0x4000_0000;

/// `lsfVaultPrivate` (LedgerFormats.h:202): deposits need the share
/// issuance's domain.
pub const LSF_VAULT_PRIVATE: u64 = 0x0001_0000;
/// `lsfLoanDefault` (LedgerFormats.h:205).
pub const LSF_LOAN_DEFAULT: u64 = 0x0001_0000;
/// `lsfLoanImpaired` (LedgerFormats.h:206).
pub const LSF_LOAN_IMPAIRED: u64 = 0x0002_0000;
/// `lsfLoanOverpayment` (LedgerFormats.h:207): the loan accepts overpayments.
pub const LSF_LOAN_OVERPAYMENT: u64 = 0x0004_0000;
/// `lsfMPTCanTrade` (LedgerFormats.h:188). A vault's share issuance carries
/// it with lsfMPTCanEscrow and lsfMPTCanTransfer unless
/// tfVaultShareNonTransferable was given.
pub const LSF_MPT_CAN_TRADE: u64 = 0x0000_0010;

/// `tfVaultPrivate` (TxFlags.h:197) — the same bit as `lsfVaultPrivate`.
pub const TF_VAULT_PRIVATE: u64 = LSF_VAULT_PRIVATE;
/// `tfVaultShareNonTransferable` (TxFlags.h:198).
pub const TF_VAULT_SHARE_NON_TRANSFERABLE: u64 = 0x0002_0000;
/// `tfVaultCreateMask` (TxFlags.h:264): every bit a VaultCreate may not set.
pub const TF_VAULT_CREATE_MASK: u64 = 0xFFFF_FFFF & !(TF_UNIVERSAL | TF_VAULT_PRIVATE | TF_VAULT_SHARE_NON_TRANSFERABLE);
/// `tfLoanOverpayment` (TxFlags.h:209, :214): on LoanSet the loan supports
/// overpayments; on LoanPay the excess may be taken as one.
pub const TF_LOAN_OVERPAYMENT: u64 = 0x0001_0000;
/// `tfLoanFullPayment` (TxFlags.h:215): LoanPay, an early full payment.
pub const TF_LOAN_FULL_PAYMENT: u64 = 0x0002_0000;
/// `tfLoanLatePayment` (TxFlags.h:218): LoanPay, a late payment.
pub const TF_LOAN_LATE_PAYMENT: u64 = 0x0004_0000;
/// `tfLoanDefault` (TxFlags.h:224): LoanManage.
pub const TF_LOAN_DEFAULT: u64 = 0x0001_0000;
/// `tfLoanImpair` (TxFlags.h:225): LoanManage.
pub const TF_LOAN_IMPAIR: u64 = 0x0002_0000;
/// `tfLoanUnimpair` (TxFlags.h:226): LoanManage.
pub const TF_LOAN_UNIMPAIR: u64 = 0x0004_0000;
/// `tfLoanSetMask`.
pub const TF_LOAN_SET_MASK: u64 = 0xFFFF_FFFF & !(TF_UNIVERSAL | TF_LOAN_OVERPAYMENT);
/// `tfLoanPayMask`.
pub const TF_LOAN_PAY_MASK: u64 =
    0xFFFF_FFFF & !(TF_UNIVERSAL | TF_LOAN_OVERPAYMENT | TF_LOAN_FULL_PAYMENT | TF_LOAN_LATE_PAYMENT);
/// `tfLoanManageMask`.
pub const TF_LOAN_MANAGE_MASK: u64 = 0xFFFF_FFFF & !(TF_UNIVERSAL | TF_LOAN_DEFAULT | TF_LOAN_IMPAIR | TF_LOAN_UNIMPAIR);
/// `tfUniversalMask` (TxFlags.h:46): the mask of a transaction type with no
/// flags of its own — VaultSet, VaultDelete, VaultDeposit, VaultWithdraw,
/// VaultClawback and the LoanBroker family.
pub const TF_UNIVERSAL_MASK: u64 = 0xFFFF_FFFF & !TF_UNIVERSAL;

// ---------------------------------------------------------------------------
// Ledger-entry templates (ledger_entries.macro:492-626)
// ---------------------------------------------------------------------------

/// `SOEStyle`: how a ledger-entry template holds a field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Soe {
    /// `SoeRequired`: always present, a zero included.
    Required,
    /// `SoeOptional`: present or absent; a present zero stays.
    Optional,
    /// `SoeDefault`: absent means the type's default (zero / empty), and the
    /// default is never written — `STObject::Proxy::assign` makes the field
    /// absent when it is given the default (STObject.h:766-772), and a
    /// serialized object carrying an explicit default does not parse.
    Default,
}

/// One ledger-entry type's template.
#[derive(Debug)]
pub struct LedgerFormat {
    /// The `LedgerEntryType` name in JSON.
    pub name: &'static str,
    /// The `LedgerEntryType` code.
    pub code: u16,
    /// The type's own fields in macro order. Every type also has the common
    /// fields `sfLedgerEntryType` and `sfFlags` (required) and
    /// `sfLedgerIndex` (optional) — LedgerFormats.cpp:15-17.
    pub fields: &'static [(&'static str, Soe)],
}

use Soe::{Default as D, Optional as O, Required as R};

/// ltVAULT 0x0084 (ledger_entries.macro:492-514). There is no SharesTotal
/// (the share issuance's OutstandingAmount) and no domain (the share
/// issuance's DomainID) — the macro's closing comments.
pub static VAULT: LedgerFormat = LedgerFormat {
    name: "Vault",
    code: 0x0084,
    fields: &[
        ("PreviousTxnID", R),
        ("PreviousTxnLgrSeq", R),
        ("Sequence", R),
        ("OwnerNode", R),
        ("Owner", R),
        ("Account", R),
        ("Data", O),
        ("Asset", R),
        ("AssetsTotal", D),
        ("AssetsAvailable", D),
        ("AssetsMaximum", D),
        ("LossUnrealized", D),
        ("ShareMPTID", R),
        ("WithdrawalPolicy", R),
        ("Scale", D),
        ("LEVersion", D),
        ("VaultKind", D),
        ("SubscriptionDate", O),
        ("RedemptionDate", O),
    ],
};

/// ltLOAN_BROKER 0x0088 (ledger_entries.macro:522-540). `sfOwnerCount` is
/// the number of the broker's live Loans; `sfData` is SoeDefault here
/// (SoeOptional on a Vault).
pub static LOAN_BROKER: LedgerFormat = LedgerFormat {
    name: "LoanBroker",
    code: 0x0088,
    fields: &[
        ("PreviousTxnID", R),
        ("PreviousTxnLgrSeq", R),
        ("Sequence", R),
        ("OwnerNode", R),
        ("VaultNode", R),
        ("VaultID", R),
        ("Account", R),
        ("Owner", R),
        ("LoanSequence", R),
        ("Data", D),
        ("ManagementFeeRate", D),
        ("OwnerCount", D),
        ("DebtTotal", D),
        ("DebtMaximum", D),
        ("CoverAvailable", D),
        ("CoverRateMinimum", D),
        ("CoverRateLiquidation", D),
    ],
};

/// ltLOAN 0x0089 (ledger_entries.macro:546-626). No sfSequence and no
/// sfAccount: a Loan is keyed by its broker and `sfLoanSequence`.
pub static LOAN: LedgerFormat = LedgerFormat {
    name: "Loan",
    code: 0x0089,
    fields: &[
        ("PreviousTxnID", R),
        ("PreviousTxnLgrSeq", R),
        ("OwnerNode", R),
        ("LoanBrokerNode", R),
        ("LoanBrokerID", R),
        ("LoanSequence", R),
        ("Borrower", R),
        ("LoanOriginationFee", D),
        ("LoanServiceFee", D),
        ("LatePaymentFee", D),
        ("ClosePaymentFee", D),
        ("OverpaymentFee", D),
        ("InterestRate", D),
        ("LateInterestRate", D),
        ("CloseInterestRate", D),
        ("OverpaymentInterestRate", D),
        ("StartDate", R),
        ("PaymentInterval", R),
        ("GracePeriod", D),
        ("PreviousPaymentDueDate", D),
        ("NextPaymentDueDate", D),
        ("PaymentRemaining", D),
        ("PeriodicPayment", R),
        ("PrincipalOutstanding", D),
        ("TotalValueOutstanding", D),
        ("ManagementFeeOutstanding", D),
        ("LoanScale", D),
    ],
};

/// The template of a ledger-entry type by its JSON name, for the three types
/// of this family.
pub fn ledger_format(name: &str) -> Option<&'static LedgerFormat> {
    [&VAULT, &LOAN_BROKER, &LOAN].into_iter().find(|f| f.name == name)
}

/// The fields the engine's threading stamps after the transactor has run
/// (`PreviousTxnID`, `PreviousTxnLgrSeq`): required by every template, absent
/// from an object a transactor has just built.
const STAMPED: [&str; 2] = ["PreviousTxnID", "PreviousTxnLgrSeq"];

impl LedgerFormat {
    /// The template style of `field`, the common fields included; None for a
    /// field the type does not have.
    pub fn style(&self, field: &str) -> Option<Soe> {
        match field {
            "LedgerEntryType" | "Flags" => Some(Soe::Required),
            "index" => Some(Soe::Optional),
            _ => self.fields.iter().find(|(name, _)| *name == field).map(|(_, style)| *style),
        }
    }

    /// Bring an object a transactor built or changed to the form rippled
    /// serializes: every SoeDefault field holding its type's default is
    /// removed (rippled's proxies never leave one — STObject.h:766-772).
    /// Then the template is checked as `STObject::applyTemplate` checks a
    /// parsed object: a field outside the template or a missing required
    /// field is an error naming it. `PreviousTxnID` / `PreviousTxnLgrSeq`
    /// may be missing — threading stamps them after the transactor.
    pub fn finish(&self, obj: &mut Value) -> Result<(), String> {
        let Some(map) = obj.as_object_mut() else { return Err(format!("{}: not an object", self.name)) };
        for (field, style) in self.fields {
            if *style != Soe::Default {
                continue;
            }
            let default = match (map.get(*field), crate::ledger::field_meta::by_name(field)) {
                (Some(value), Some(meta)) => crate::ledger::tx_meta::is_default(meta.type_code, value),
                _ => false,
            };
            if default {
                map.remove(*field);
            }
        }
        if let Some(stray) = map.keys().find(|k| self.style(k).is_none()) {
            return Err(format!("{}: field {stray} is not in the template", self.name));
        }
        if map.get("LedgerEntryType").and_then(|t| t.as_str()) != Some(self.name) {
            return Err(format!("{}: LedgerEntryType is missing or different", self.name));
        }
        if !map.contains_key("Flags") {
            return Err(format!("{}: required field Flags is missing", self.name));
        }
        for (field, style) in self.fields {
            if *style == Soe::Required && !map.contains_key(*field) && !STAMPED.contains(field) {
                return Err(format!("{}: required field {field} is missing", self.name));
            }
        }
        Ok(())
    }
}

/// `getFlagsMask`-style test (Transactor.cpp preflight1): whether `flags`
/// sets a bit of `mask` — temINVALID_FLAG.
pub const fn invalid_flags(flags: u64, mask: u64) -> bool {
    flags & mask != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::keylet::vault_devnet_fixture::object;
    use crate::tx::number::{MantissaScale, ScaleGuard};
    use serde_json::json;

    /// Protocol.h's own static_asserts (:116, :118, :151, :159) and the
    /// derived constants.
    #[test]
    fn the_units_and_limits_are_rippleds() {
        assert_eq!(BIPS_PER_UNITY, 10_000);
        assert_eq!(TENTH_BIPS_PER_UNITY, 100_000);
        assert_eq!(MAX_MANAGEMENT_FEE_RATE, 10_000);
        assert_eq!(MAX_COVER_RATE, 100_000);
        assert_eq!(percentage_to_bips(50), 5_000, "Protocol.h:113: 50% is 5,000 bps");
        // "This is 946708560 seconds" (Protocol.h:369).
        assert_eq!(MAX_INVESTMENT_PERIOD, 946_708_560);
        assert_eq!(MIN_INVESTMENT_PERIOD, 180);
        assert_eq!(MAX_ASSET_CHECK_DEPTH, 5);
        assert_eq!(TF_VAULT_CREATE_MASK, 0x3FFC_FFFF);
        assert_eq!(TF_UNIVERSAL_MASK, 0x3FFF_FFFF);
        assert_eq!(TF_LOAN_SET_MASK, 0x3FFE_FFFF);
        assert_eq!(TF_LOAN_PAY_MASK, 0x3FF8_FFFF);
        assert_eq!(TF_LOAN_MANAGE_MASK, 0x3FF8_FFFF);
        assert!(invalid_flags(0x0004_0000, TF_VAULT_CREATE_MASK));
        assert!(!invalid_flags(0x8000_0000 | TF_VAULT_PRIVATE | TF_VAULT_SHARE_NON_TRANSFERABLE, TF_VAULT_CREATE_MASK));
    }

    /// Protocol.h:106-108: "If a loan broker has 999 XRP of debt, and must
    /// maintain 1,000 bps of that debt as cover (10%), then the minimum
    /// cover amount is 999,000,000 drops * 1000 / bipsPerUnity = 99,900,000
    /// drops". The same in tenths of a basis point, and a rate that does
    /// not divide evenly.
    #[test]
    fn bips_of_a_value_is_the_product_over_unity() {
        let _scale = ScaleGuard::new(MantissaScale::Large330);
        let n = |m: i64, e: i32| Number::new(m, e, Rounding::ToNearest).unwrap();
        let debt = n(999_000_000, 0);
        assert_eq!(bips_of_value(debt, 1_000, Rounding::ToNearest).unwrap(), n(99_900_000, 0));
        assert_eq!(tenth_bips_of_value(debt, 10_000, Rounding::ToNearest).unwrap(), n(99_900_000, 0));
        // 1 × 33,333 / 100,000 = 0.33333 exactly.
        assert_eq!(tenth_bips_of_value(n(1, 0), 33_333, Rounding::ToNearest).unwrap(), n(33_333, -5));
        assert_eq!(tenth_bips_of_value(Number::ZERO, 33_333, Rounding::ToNearest).unwrap(), Number::ZERO);
    }

    /// Every field of every real devnet Vault, LoanBroker and Loan is in its
    /// template, every required field is there, and no SoeDefault field
    /// holds a default: `finish` accepts each as it stands and changes
    /// nothing.
    #[test]
    fn the_templates_accept_the_real_devnet_objects_unchanged() {
        for name in ["vault_xrp", "vault_usd_closed_ended", "loan_broker_xrp", "loan_broker_usd", "loan_usd"] {
            let (node, _) = object(name);
            let format = ledger_format(node["LedgerEntryType"].as_str().unwrap()).expect("a template");
            let mut finished = node.clone();
            assert_eq!(format.finish(&mut finished), Ok(()), "{name}");
            assert_eq!(finished, node, "{name}");
        }
        assert_eq!((VAULT.code, LOAN_BROKER.code, LOAN.code), (0x0084, 0x0088, 0x0089));
        assert!(ledger_format("AccountRoot").is_none());
    }

    /// SoeDefault fields at their default leave the object; SoeRequired and
    /// SoeOptional zeros stay; a stray field and a missing required field
    /// are refused. The devnet XRP vault shows the result: no AssetsMaximum,
    /// LossUnrealized, Scale, LEVersion or VaultKind.
    #[test]
    fn finish_drops_defaults_and_checks_the_template() {
        let mut vault = json!({
            "LedgerEntryType": "Vault", "Flags": 0, "Sequence": 0, "OwnerNode": "0",
            "Owner": "11".repeat(20), "Account": "31".repeat(20), "Asset": {"currency": "XRP"},
            "AssetsTotal": "0", "AssetsAvailable": "12", "AssetsMaximum": "0", "LossUnrealized": "0",
            "ShareMPTID": "00".repeat(24), "WithdrawalPolicy": 1, "Scale": 0, "LEVersion": 0, "VaultKind": 0,
            "SubscriptionDate": 0, "Data": "",
        });
        assert_eq!(VAULT.finish(&mut vault), Ok(()));
        let kept: Vec<&str> = vault.as_object().unwrap().keys().map(String::as_str).collect();
        for gone in ["AssetsTotal", "AssetsMaximum", "LossUnrealized", "Scale", "LEVersion", "VaultKind"] {
            assert!(!kept.contains(&gone), "{gone} is SoeDefault and zero");
        }
        // Sequence 0 is required; SubscriptionDate 0 and an empty Data are optional and present.
        for stays in ["Sequence", "AssetsAvailable", "SubscriptionDate", "Data", "WithdrawalPolicy", "Flags"] {
            assert!(kept.contains(&stays), "{stays}");
        }

        // On a LoanBroker sfData is SoeDefault: an empty one goes.
        let mut broker = json!({
            "LedgerEntryType": "LoanBroker", "Flags": 0, "Sequence": 5, "OwnerNode": "0", "VaultNode": "0",
            "VaultID": "84".repeat(32), "Account": "32".repeat(20), "Owner": "11".repeat(20), "LoanSequence": 1,
            "Data": "", "ManagementFeeRate": 0, "OwnerCount": 0, "DebtTotal": "0", "CoverRateMinimum": 0,
        });
        assert_eq!(LOAN_BROKER.finish(&mut broker), Ok(()));
        assert_eq!(broker.as_object().unwrap().len(), 9, "{broker}");

        let mut stray = vault.clone();
        stray["DebtTotal"] = json!("1");
        assert_eq!(VAULT.finish(&mut stray), Err("Vault: field DebtTotal is not in the template".to_string()));
        let mut short = vault.clone();
        short.as_object_mut().unwrap().remove("ShareMPTID");
        assert_eq!(VAULT.finish(&mut short), Err("Vault: required field ShareMPTID is missing".to_string()));
        let mut wrong = vault.clone();
        wrong["LedgerEntryType"] = json!("Loan");
        assert!(VAULT.finish(&mut wrong).is_err());
        assert_eq!(VAULT.style("Scale"), Some(Soe::Default));
        assert_eq!(VAULT.style("Data"), Some(Soe::Optional));
        assert_eq!(LOAN_BROKER.style("Data"), Some(Soe::Default));
        assert_eq!(LOAN.style("PeriodicPayment"), Some(Soe::Required));
        assert_eq!(LOAN.style("Sequence"), None);
    }
}
