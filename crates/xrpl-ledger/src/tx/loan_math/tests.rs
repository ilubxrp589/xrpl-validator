//! The loan maths against rippled's own expectations: every case of
//! `src/test/app/lending/LendingHelpers_test.cpp` (3.4.1) that names a
//! number, at the Large330 scale the lending amendments run at, plus the
//! consistency properties rippled's tests rely on (a schedule paid to the
//! end leaves exactly nothing).

use super::*;
use crate::tx::number::{MantissaScale, ScaleGuard};
use serde_json::json;

const ISSUER: [u8; 20] = [0x21; 20];
const IOU: HeldAsset = HeldAsset::Iou { currency: [0x55; 20], issuer: ISSUER };
const MPT: HeldAsset = HeldAsset::Mpt([0x07; 24]);
const MONTH: u32 = 30 * 24 * 60 * 60;
const R: LoanRules = LoanRules::ALL;
const PRE_320: LoanRules = LoanRules { fix_cleanup_3_2_0: false, ..LoanRules::ALL };

fn large() -> ScaleGuard {
    ScaleGuard::new(MantissaScale::Large330)
}

/// `Number{m, e}`.
fn n(m: i64, e: i32) -> Number {
    Number::new(m, e, A).unwrap()
}

fn i(v: i64) -> Number {
    n(v, 0)
}

fn show(x: Number) -> String {
    crate::tx::st_number::number_to_string(&x)
}

/// LendingHelpers_test.cpp:39-92 `testComputePaymentFactor`.
#[test]
fn payment_factor() {
    let _g = large();
    let f = |r: Number, k: u32| compute_payment_factor(&R, r, k).unwrap();
    assert_eq!(f(Number::ZERO, 4), n(25, -2), "zero periodic rate: 1/4");
    assert_eq!(f(n(5, -2), 1), n(105, -2), "one payment remaining");
    assert_eq!(f(n(5, -2), 3), n(3672085646312450436, -19), "three payments at 5%");
    assert_eq!(f(n(5, -2), 0), Number::ZERO, "no payments remaining");
}

/// LendingHelpers_test.cpp:94-153 `testLoanPeriodicPayment`.
#[test]
fn periodic_payment() {
    let _g = large();
    let p = |principal: Number, r: Number, k: u32| loan_periodic_payment(&R, principal, r, k).unwrap();
    assert_eq!(p(Number::ZERO, n(5, -2), 5), Number::ZERO);
    assert_eq!(p(i(1000), n(5, -2), 0), Number::ZERO);
    assert_eq!(p(i(1000), Number::ZERO, 4), i(250));
    let rate = loan_periodic_rate(100_000, MONTH).unwrap();
    assert_eq!(p(i(1000), rate, 3), n(389569066396123265, -15), "1000 at 100% a year, three monthly payments");
}

/// LendingHelpers_test.cpp:155-215 `testLoanPrincipalFromPeriodicPayment`.
#[test]
fn principal_from_periodic_payment() {
    let _g = large();
    let p = |payment: Number, r: Number, k: u32| loan_principal_from_periodic_payment(&R, payment, r, k).unwrap();
    assert_eq!(p(Number::ZERO, n(5, -2), 5), Number::ZERO);
    assert_eq!(p(i(1000), n(5, -2), 0), Number::ZERO);
    assert_eq!(p(i(250), Number::ZERO, 4), i(1000));
    let rate = loan_periodic_rate(100_000, MONTH).unwrap();
    assert_eq!(p(n(389569066396123265, -15), rate, 3), i(1000));
}

/// LendingHelpers_test.cpp:217-285 `testComputePowerMinusOne`.
#[test]
fn power_minus_one_binomial() {
    let _g = large();
    let f = |r: Number, k: u32| compute_power_minus_one(r, k).unwrap();
    assert_eq!(f(Number::ZERO, 5), Number::ZERO);
    assert_eq!(f(n(5, -2), 0), Number::ZERO);
    assert_eq!(f(n(5, -2), 3), n(157625, -6), "(1.05)^3 - 1");
    assert_eq!(f(i(1), 1), i(1));
    // Near-zero rate against the polynomial written out: 2r + r², 3r + 3r² + r³.
    let r = loan_periodic_rate(1, 600).unwrap();
    let m = |a: Number, b: Number| a.mul(b, A).unwrap();
    let s = |a: Number, b: Number| a.add(b, A).unwrap();
    assert_eq!(f(r, 2), s(m(i(2), r), m(r, r)));
    assert_eq!(f(r, 3), s(s(m(i(3), r), m(m(i(3), r), r)), m(m(r, r), r)));
    // Large n, early termination: the hybrid takes this path below 1e-9.
    for r in [n(1, -13), n(1, -15)] {
        assert_eq!(f(r, 1000), compute_power_minus_one_hybrid(r, 1000).unwrap());
        assert!(f(r, 1000) > Number::ZERO);
    }
}

/// LendingHelpers_test.cpp:290-406 `testComputePowerMinusOneHybrid`.
#[test]
fn power_minus_one_hybrid() {
    let _g = large();
    let closed = |r: Number, k: u32| Number::one().add(r, A).unwrap().power(k, A).unwrap().sub(Number::one(), A).unwrap();
    let hybrid = |r: Number, k: u32| compute_power_minus_one_hybrid(r, k).unwrap();
    let binom = |r: Number, k: u32| compute_power_minus_one(r, k).unwrap();
    // At or above r × n = 1e-9: the closed form, bit for bit.
    for (r, k) in [(n(5, -2), 3), (n(1, -3), 1000), (n(1, -7), 100)] {
        assert_eq!(hybrid(r, k), closed(r, k), "{} ^ {k}", show(r));
    }
    // Below: the binomial sum.
    for (r, k) in [(loan_periodic_rate(1, 600).unwrap(), 2), (n(1, -12), 100)] {
        assert_eq!(hybrid(r, k), binom(r, k), "{} ^ {k}", show(r));
    }
    assert_eq!(hybrid(Number::ZERO, 100), Number::ZERO);
    assert_eq!(hybrid(n(5, -2), 0), Number::ZERO);
    assert_eq!(hybrid(Number::ZERO, 0), Number::ZERO);
    // Exactly at the threshold the closed form is taken, and the two agree
    // to 1e-18.
    for (r, k) in [(n(1, -9), 1), (n(1, -12), 1000)] {
        assert_eq!(hybrid(r, k), closed(r, k));
        assert!(closed(r, k).sub(binom(r, k), A).unwrap().abs() < n(1, -18));
    }
}

/// LendingHelpers_test.cpp:412-505: the near-zero-rate regressions
/// fixCleanup3_2_0 is for.
#[test]
fn near_zero_rates() {
    let _g = large();
    let rate = loan_periodic_rate(1, 600).unwrap();
    let payment = loan_periodic_payment(&R, i(100), rate, 3).unwrap();
    // principal <= payment × n.
    for k in [3u32, 2, 1] {
        let principal = loan_principal_from_periodic_payment(&R, payment, rate, k).unwrap();
        assert!(principal <= payment.mul(num(k as u64), A).unwrap(), "n = {k}");
    }
    // A non-negative interestDue.
    let state = compute_theoretical_loan_state(&R, payment, rate, 2, 0).unwrap();
    assert!(state.principal_outstanding <= state.value_outstanding);
    assert!(!(state.interest_due < Number::ZERO));
    assert_eq!(state.management_fee_due, Number::ZERO);
    // The factor with and without the fix, against (1 + 3r + 3r² + r³) / (3 + 3r + r²).
    let m = |a: Number, b: Number| a.mul(b, A).unwrap();
    let s = |a: Number, b: Number| a.add(b, A).unwrap();
    let r = rate;
    let numerator = s(s(s(i(1), m(i(3), r)), m(m(i(3), r), r)), m(m(r, r), r));
    let denominator = s(s(i(3), m(i(3), r)), m(r, r));
    let reference = numerator.div(denominator, A).unwrap();
    let fixed = compute_payment_factor(&R, r, 3).unwrap();
    let buggy = compute_payment_factor(&PRE_320, r, 3).unwrap();
    assert_ne!(fixed, buggy);
    assert!(fixed.sub(reference, A).unwrap().abs() < n(1, -15));
    assert!(buggy.sub(reference, A).unwrap().abs() > n(1, -12));
}

/// LendingHelpers_test.cpp:507-554 `testComputeOverpaymentComponents`.
#[test]
fn overpayment_components() {
    let _g = large();
    // 1000 over, 10% interest, 50% fee, 10% management fee, loan scale 1.
    let c = compute_overpayment_components(IOU, 1, i(1000), 10_000, 50_000, 10_000).unwrap();
    assert_eq!(c.untracked_management_fee, i(500));
    assert_eq!(c.untracked_interest, i(90));
    assert_eq!(c.tracked.tracked_interest_part().unwrap(), i(90));
    assert_eq!(c.tracked.tracked_management_fee_delta, i(10));
    assert_eq!(c.tracked.tracked_principal_delta, i(400));
    assert_eq!(c.tracked.special_case, PaymentSpecialCase::Extra);
    let s = |a: Number, b: Number| a.add(b, A).unwrap();
    assert_eq!(s(c.tracked.tracked_management_fee_delta, c.untracked_interest), i(100), "the gross interest");
    assert_eq!(
        s(s(s(c.tracked.tracked_management_fee_delta, c.untracked_interest), c.tracked.tracked_principal_delta), c.untracked_management_fee),
        i(1000),
        "the parts are the overpayment"
    );
}

/// LendingHelpers_test.cpp:556-608 `testComputeInterestAndFeeParts`.
#[test]
fn interest_and_fee_parts() {
    let _g = large();
    let f = |interest: i64, rate: u32| compute_interest_and_fee_parts(IOU, i(interest), rate, 1).unwrap();
    assert_eq!(f(0, 10_000), (Number::ZERO, Number::ZERO));
    assert_eq!(f(1000, 0), (i(1000), Number::ZERO));
    assert_eq!(f(1000, 10_000), (i(900), i(100)));
}

/// LendingHelpers_test.cpp:610-682 `testLoanLatePaymentInterest`.
#[test]
fn late_payment_interest() {
    let _g = large();
    let f = |principal: i64, rate: u32, now: u32, due: u32| loan_late_payment_interest(i(principal), rate, now, due).unwrap();
    assert_eq!(f(1000, 10_000, 3000, 3000), Number::ZERO, "on time");
    assert_eq!(f(1000, 10_000, 3000, 4000), Number::ZERO, "early");
    assert_eq!(f(0, 10_000, 3000, 2000), Number::ZERO, "no principal");
    assert_eq!(f(1000, 0, 3000, 2000), Number::ZERO, "no late rate");
    assert_eq!(f(1000, 100_000, 3000, 2000), n(317097919837645865, -19), "1000 s late at 100%");
}

/// LendingHelpers_test.cpp:684-770 `testLoanAccruedInterest`.
#[test]
fn accrued_interest() {
    let _g = large();
    let f = |principal: i64, rate: Number, now: u32, start: u32, prev: u32, interval: u32| {
        loan_accrued_interest(i(principal), rate, now, start, prev, interval).unwrap()
    };
    assert_eq!(f(0, n(5, -2), 3000, 2000, 2500, MONTH), Number::ZERO, "no principal");
    assert_eq!(f(1000, n(5, -2), 1000, 2000, 1500, MONTH), Number::ZERO, "before the start date");
    assert_eq!(f(1000, Number::ZERO, 3000, 2000, 2500, MONTH), Number::ZERO, "no rate");
    assert_eq!(f(1000, n(5, -2), 3000, 2000, 2500, 0), Number::ZERO, "no interval");
    assert_eq!(f(1000, n(5, -2), 3000, 1000, 2000, MONTH), n(1929012345679012346, -20));
}

/// LendingHelpers_test.cpp:774-847 `testComputeFullPaymentInterest`.
#[test]
fn full_payment_interest() {
    let _g = large();
    let f = |principal: i64, close_rate: u32| compute_full_payment_interest(i(principal), n(5, -2), 3000, MONTH, 2000, 1000, close_rate).unwrap();
    assert_eq!(f(0, 10_000), Number::ZERO);
    assert_eq!(f(1000, 0), n(1929012345679012346, -20));
    assert_eq!(f(1000, 10_000), n(1000192901234567901, -16));
}

struct Overpaid {
    parts: LoanPaymentParts,
    old: LoanState,
    new: LoanState,
}

/// The common body of the six `testTryOverpayment…` cases
/// (LendingHelpers_test.cpp:849-1482): a 1000 loan over ten monthly
/// payments at loan scale -5, overpaid by 50.
fn overpay(rules: &LoanRules, loan_rate: u32, management_fee_rate: u32, over_interest: u32, over_fee: u32) -> Overpaid {
    let scale = -5;
    let rate = loan_periodic_rate(loan_rate, MONTH).unwrap();
    let components = compute_overpayment_components(IOU, scale, i(50), over_interest, over_fee, management_fee_rate).unwrap();
    let properties = compute_loan_properties(rules, IOU, i(1000), loan_rate, MONTH, 10, management_fee_rate, scale).unwrap();
    let (parts, new) =
        try_overpayment(rules, IOU, scale, &components, &properties.loan_state, properties.periodic_payment, rate, 10, management_fee_rate)
            .unwrap()
            .expect("the overpayment is taken");
    Overpaid { parts, old: properties.loan_state, new: new.loan_state }
}

fn diff(a: Number, b: Number) -> Number {
    a.sub(b, A).unwrap()
}

/// LendingHelpers_test.cpp:849-943.
#[test]
fn try_overpayment_no_interest_no_fee() {
    let _g = large();
    let o = overpay(&R, 0, 0, 0, 0);
    assert_eq!(o.parts.value_change, Number::ZERO);
    assert_eq!(o.parts.fee_paid, Number::ZERO);
    assert_eq!(o.parts.interest_paid, Number::ZERO);
    assert_eq!(o.parts.principal_paid, i(50));
    assert_eq!(diff(o.old.interest_due, o.new.interest_due), Number::ZERO);
    assert_eq!(diff(o.old.management_fee_due, o.new.management_fee_due), Number::ZERO);
    assert_eq!(o.parts.principal_paid, diff(o.old.principal_outstanding, o.new.principal_outstanding));
}

/// LendingHelpers_test.cpp:945-1039.
#[test]
fn try_overpayment_no_interest_overpayment_fee() {
    let _g = large();
    let o = overpay(&R, 0, 0, 0, 10_000);
    assert_eq!(o.parts.value_change, Number::ZERO);
    assert_eq!(o.parts.fee_paid, i(5));
    assert_eq!(o.parts.principal_paid, i(45));
    assert_eq!(o.parts.interest_paid, Number::ZERO);
    assert_eq!(diff(o.old.interest_due, o.new.interest_due), Number::ZERO);
    assert_eq!(diff(o.old.management_fee_due, o.new.management_fee_due), Number::ZERO);
    assert_eq!(o.parts.principal_paid, diff(o.old.principal_outstanding, o.new.principal_outstanding));
}

/// LendingHelpers_test.cpp:1041-1141.
#[test]
fn try_overpayment_loan_interest_no_overpayment_fees() {
    let _g = large();
    let o = overpay(&R, 10_000, 0, 0, 0);
    assert_eq!(show(o.parts.value_change), show(n(-228802, -5)));
    assert_eq!(o.parts.fee_paid, Number::ZERO);
    assert_eq!(o.parts.principal_paid, i(50));
    assert_eq!(o.parts.interest_paid, Number::ZERO);
    assert_eq!(o.parts.principal_paid, diff(o.old.principal_outstanding, o.new.principal_outstanding));
    assert_eq!(o.parts.value_change, diff(o.new.interest_due, o.old.interest_due));
    assert_eq!(diff(o.old.management_fee_due, o.new.management_fee_due), Number::ZERO);
}

/// LendingHelpers_test.cpp:1143-1249.
#[test]
fn try_overpayment_loan_interest_overpayment_interest() {
    let _g = large();
    let o = overpay(&R, 10_000, 0, 10_000, 0);
    assert_eq!(o.parts.interest_paid, i(5));
    assert_eq!(show(o.parts.value_change), show(n(-205922, -5).add(o.parts.interest_paid, A).unwrap()));
    assert_eq!(o.parts.fee_paid, Number::ZERO);
    assert_eq!(o.parts.principal_paid, i(45));
    assert_eq!(o.parts.principal_paid, diff(o.old.principal_outstanding, o.new.principal_outstanding));
    assert_eq!(diff(o.parts.value_change, o.parts.interest_paid), diff(o.new.interest_due, o.old.interest_due));
    assert_eq!(diff(o.old.management_fee_due, o.new.management_fee_due), Number::ZERO);
}

/// LendingHelpers_test.cpp:1251-1361.
#[test]
fn try_overpayment_loan_interest_and_fee_overpayment_interest() {
    let _g = large();
    let o = overpay(&R, 10_000, 10_000, 10_000, 0);
    assert_eq!(o.parts.interest_paid, n(45, -1));
    assert_eq!(show(o.parts.value_change), show(n(-18533, -4).add(o.parts.interest_paid, A).unwrap()));
    assert_eq!(o.parts.fee_paid, n(5, -1));
    assert_eq!(o.parts.principal_paid, i(45));
    assert_eq!(o.parts.principal_paid, diff(o.old.principal_outstanding, o.new.principal_outstanding));
    assert_eq!(show(diff(o.new.management_fee_due, o.old.management_fee_due)), show(n(-20592, -5)));
    assert_eq!(diff(o.parts.value_change, o.parts.interest_paid), diff(o.new.interest_due, o.old.interest_due));
}

/// LendingHelpers_test.cpp:1363-1482: with and without fixCleanup3_2_0 the
/// re-amortized loan differs by one unit of the loan scale.
#[test]
fn try_overpayment_loan_interest_and_fee_overpayment_interest_and_fee_both_arms() {
    let _g = large();
    let fixed = overpay(&R, 10_000, 10_000, 10_000, 10_000);
    let legacy = overpay(&PRE_320, 10_000, 10_000, 10_000, 10_000);
    for (o, tag) in [(&fixed, "fixed"), (&legacy, "legacy")] {
        assert_eq!(o.parts.interest_paid, n(45, -1), "{tag}");
        assert_eq!(o.parts.fee_paid, n(55, -1), "{tag}");
        assert_eq!(o.parts.principal_paid, i(40), "{tag}");
        assert_eq!(o.parts.principal_paid, diff(o.old.principal_outstanding, o.new.principal_outstanding), "{tag}");
        assert_eq!(diff(o.parts.value_change, o.parts.interest_paid), diff(o.new.interest_due, o.old.interest_due), "{tag}");
    }
    assert_eq!(show(fixed.parts.value_change), show(n(-164738, -5).add(fixed.parts.interest_paid, A).unwrap()));
    assert_eq!(show(diff(fixed.new.management_fee_due, fixed.old.management_fee_due)), show(n(-18303, -5)));
    assert_eq!(show(legacy.parts.value_change), show(n(-164737, -5).add(legacy.parts.interest_paid, A).unwrap()));
    assert_eq!(show(diff(legacy.new.management_fee_due, legacy.old.management_fee_due)), show(n(-18304, -5)));
}

/// LendingHelpers_test.cpp:1484-1667: the two accounting models.
#[test]
fn accounting_models() {
    let _g = large();
    // accrual::loanOriginationDeltas (:1485-1520).
    for (principal, interest) in [(1000, 50), (1000, 0), (0, 0)] {
        let d = accrual::loan_origination_deltas(i(principal), i(interest)).unwrap();
        assert_eq!((d.assets_total_delta, d.debt_total_delta), (i(interest), i(principal + interest)));
    }
    // cash_basis: interest is ignored (:1522-1540).
    let d = cash_basis::loan_origination_deltas(i(1000));
    assert_eq!((d.assets_total_delta, d.debt_total_delta), (Number::ZERO, i(1000)));
    // accrual::loanOriginationExceedsVaultMaximum (:1542-1620): no maximum
    // never exceeds; else interest > maximum - total.
    let exceeds = |max: i64, total: i64, interest: i64| accrual::loan_origination_exceeds_vault_maximum(i(max), i(total), i(interest)).unwrap();
    assert!(!exceeds(0, 1000, 500));
    assert!(!exceeds(1000, 900, 100));
    assert!(exceeds(1000, 900, 101));
    assert!(!exceeds(1000, 900, 0));
    assert!(exceeds(1000, 1000, 1));
    // Exposure (:1622-1638).
    let loan = json!({"TotalValueOutstanding": "1100", "PrincipalOutstanding": "1000", "ManagementFeeOutstanding": "10"});
    assert_eq!(accrual::loan_vault_exposure(&loan).unwrap(), i(1090));
    assert_eq!(cash_basis::loan_vault_exposure(&loan), i(1000));
    // Payment deltas (:1640-1667).
    let parts = LoanPaymentParts { principal_paid: i(100), interest_paid: i(10), value_change: i(3), fee_paid: i(1) };
    let d = accrual::loan_payment_deltas(&parts).unwrap();
    assert_eq!((d.assets_total_delta, d.debt_total_delta), (i(3), i(107)));
    let d = cash_basis::loan_payment_deltas(&parts);
    assert_eq!((d.assets_total_delta, d.debt_total_delta), (i(10), i(100)));
}

/// LendingHelpers_test.cpp:1669-1803: the dispatchers follow the vault's
/// LEVersion alone.
#[test]
fn the_accounting_model_is_the_vaults_version() {
    let _g = large();
    let legacy = json!({"LedgerEntryType": "Vault", "AssetsMaximum": "1000"});
    let cash = json!({"LedgerEntryType": "Vault", "AssetsMaximum": "1000", "LEVersion": 1});
    assert!(!cash_basis_enabled(&legacy));
    assert!(cash_basis_enabled(&cash));
    let d = loan_origination_deltas(&legacy, i(1000), i(50)).unwrap();
    assert_eq!((d.assets_total_delta, d.debt_total_delta), (i(50), i(1050)));
    let d = loan_origination_deltas(&cash, i(1000), i(50)).unwrap();
    assert_eq!((d.assets_total_delta, d.debt_total_delta), (Number::ZERO, i(1000)));
    assert!(loan_origination_exceeds_vault_maximum(&legacy, i(990), i(50)).unwrap());
    assert!(!loan_origination_exceeds_vault_maximum(&cash, i(990), i(50)).unwrap());
    let loan = json!({"TotalValueOutstanding": "1100", "PrincipalOutstanding": "1000", "ManagementFeeOutstanding": "10"});
    assert_eq!(loan_vault_exposure(&legacy, &loan).unwrap(), i(1090));
    assert_eq!(loan_vault_exposure(&cash, &loan).unwrap(), i(1000));
    let parts = LoanPaymentParts { principal_paid: i(100), interest_paid: i(10), value_change: i(3), fee_paid: i(1) };
    assert_eq!(loan_payment_deltas(&legacy, &parts).unwrap().debt_total_delta, i(107));
    assert_eq!(loan_payment_deltas(&cash, &parts).unwrap().debt_total_delta, i(100));
}

/// LendingHelpers.cpp:172-180: the due date itself is late only before
/// fixCleanup3_4_0.
#[test]
fn a_payment_is_late_after_its_due_date() {
    let loan = json!({"NextPaymentDueDate": 1000});
    let pre = LoanRules { fix_cleanup_3_4_0: false, ..LoanRules::ALL };
    assert!(!is_payment_late(&R, 999, &loan));
    assert!(!is_payment_late(&R, 1000, &loan));
    assert!(is_payment_late(&R, 1001, &loan));
    assert!(!is_payment_late(&pre, 999, &loan));
    assert!(is_payment_late(&pre, 1000, &loan));
    // No due date (a finished loan): `hasExpired` of zero.
    assert!(is_payment_late(&R, 1, &json!({})));
}

/// LendingHelpers.h:243-255.
#[test]
fn adjust_imprecise_number_rounds_at_the_vault_scale_and_floors_at_zero() {
    let _g = large();
    // At scale -10, 100 + 0.00000000004 is 100 and + 0.00000000006 is 100.0000000001.
    assert_eq!(adjust_imprecise_number(i(100), n(4, -11), IOU, -10).unwrap(), i(100));
    assert_eq!(show(adjust_imprecise_number(i(100), n(6, -11), IOU, -10).unwrap()), "100.0000000001");
    assert_eq!(adjust_imprecise_number(i(100), i(-101), IOU, -10).unwrap(), Number::ZERO);
    assert_eq!(adjust_imprecise_number(i(100), i(-40), MPT, 0).unwrap(), i(60));
}

/// LendingHelpers.cpp:1883-1959, each guard.
#[test]
fn loan_guards() {
    let _g = large();
    let props = |principal: i64, rate: u32, payments: u32, scale: i32, asset: HeldAsset| {
        compute_loan_properties(&R, asset, i(principal), rate, MONTH, payments, 0, scale).unwrap()
    };
    // A sound loan.
    let p = props(1000, 10_000, 10, -5, IOU);
    assert_eq!(check_loan_guards(IOU, i(1000), true, 10, &p).unwrap(), TxResult::Success);
    // Guard 1: interest expected, none computed.
    let none = props(1000, 0, 10, -5, IOU);
    assert_eq!(check_loan_guards(IOU, i(1000), true, 10, &none).unwrap(), TxResult::PrecisionLoss);
    // Guard 1a: none expected, some computed.
    assert_eq!(check_loan_guards(IOU, i(1000), false, 10, &p).unwrap(), TxResult::TecInternal);
    // Guard 2: no principal in the first payment.
    let mut flat = none;
    flat.first_payment_principal = Number::ZERO;
    assert_eq!(check_loan_guards(IOU, i(1000), false, 10, &flat).unwrap(), TxResult::PrecisionLoss);
    // Guard 3: a periodic payment of zero.
    let mut zero = none;
    zero.periodic_payment = Number::ZERO;
    assert_eq!(check_loan_guards(IOU, i(1000), false, 10, &zero).unwrap(), TxResult::PrecisionLoss);
    // Guard 4: 10 units of an MPT over 3 payments — the payment rounds up to
    // 4, and ceil(10 / 4) = 3 payments: fine. Over 4 payments it rounds to
    // 3 and ceil(10 / 3) = 4: fine. 10 over 6: payment 2, ceil(10 / 2) = 5 ≠ 6.
    for (payments, want) in [(3, TxResult::Success), (4, TxResult::Success), (6, TxResult::PrecisionLoss)] {
        let p = props(10, 0, payments, 0, MPT);
        assert_eq!(check_loan_guards(MPT, i(10), false, payments, &p).unwrap(), want, "{payments} payments");
    }
}

/// A Loan as LoanSet would write it from [`compute_loan_properties`].
fn loan_of(asset: HeldAsset, principal: i64, rate: u32, payments: u32, management_fee_rate: u32, minimum_scale: i32, start: u32, interval: u32) -> Value {
    let p = compute_loan_properties(&R, asset, i(principal), rate, interval, payments, management_fee_rate, minimum_scale).unwrap();
    assert_eq!(check_loan_guards(asset, i(principal), rate != 0, payments, &p).unwrap(), TxResult::Success);
    let mut loan = json!({
        "LedgerEntryType": "Loan", "Flags": 0, "StartDate": start, "PaymentInterval": interval, "InterestRate": rate,
        "NextPaymentDueDate": start + interval, "PaymentRemaining": payments, "LoanScale": p.loan_scale,
    });
    set_number_field(&mut loan, "PeriodicPayment", p.periodic_payment, Soe::Required);
    set_number_field(&mut loan, "PrincipalOutstanding", i(principal), Soe::Default);
    set_number_field(&mut loan, "TotalValueOutstanding", p.loan_state.value_outstanding, Soe::Default);
    set_number_field(&mut loan, "ManagementFeeOutstanding", p.loan_state.management_fee_due, Soe::Default);
    loan
}

fn amount(asset: HeldAsset, value: Number) -> Amount {
    Amount::from_number(asset, value, Rounding::Upward).unwrap()
}

/// The property rippled's loan tests check after every schedule
/// (LoanTestBase.h `makeLoanPayments` and its callers): paying the rounded
/// periodic payment each period ends with nothing outstanding after exactly
/// PaymentTotal payments, the parts of every payment add up to what was
/// paid, the principal paid in total is the principal lent, and interest
/// plus fee is the total value less the principal.
#[test]
fn a_schedule_paid_to_the_end_leaves_exactly_zero() {
    let _g = large();
    let cases: [(HeldAsset, i64, u32, u32, u32, i32); 7] = [
        (IOU, 1000, 10_000, 10, 0, -5),
        (IOU, 1000, 10_000, 10, 10_000, -5),
        (IOU, 1000, 100_000, 3, 1_000, -11),
        (IOU, 12_345, 50_000, 24, 5_000, -10),
        (IOU, 1000, 0, 7, 0, -5),
        (MPT, 1_000_000, 12_000, 12, 10_000, 0),
        (HeldAsset::Xrp, 100_000_000, 7_500, 36, 2_500, 0),
    ];
    for (asset, principal, rate, payments, fee_rate, min_scale) in cases {
        let what = format!("{principal} at {rate} over {payments}, fee {fee_rate}");
        let start = 1000;
        let mut loan = loan_of(asset, principal, rate, payments, fee_rate, min_scale, start, MONTH);
        let broker = json!({"ManagementFeeRate": fee_rate});
        let total_value = number_field(&loan, "TotalValueOutstanding");
        let total_fee = number_field(&loan, "ManagementFeeOutstanding");
        let mut paid = LoanPaymentParts::ZERO;
        let mut made = 0;
        while uint(&loan, "PaymentRemaining") > 0 {
            let before = loan_state_of(&loan).unwrap();
            // The borrower offers the rounded periodic payment, or what is left.
            let due = round_periodic_payment(asset, number_field(&loan, "PeriodicPayment"), loan_scale_of(&loan)).unwrap();
            let offer = if uint(&loan, "PaymentRemaining") == 1 { before.value_outstanding } else { nmin(due, before.value_outstanding) };
            let now = uint(&loan, "NextPaymentDueDate") as u32 - 1;
            let parts = loan_make_payment(&R, now, asset, &mut loan, &broker, &amount(asset, offer), LoanPaymentType::Regular)
                .unwrap()
                .unwrap_or_else(|refused| panic!("{what}: payment {made} refused {refused:?}"));
            made += 1;
            let after = loan_state_of(&loan).unwrap();
            // The parts are what the Loan lost, no value change, and no more than was offered.
            assert_eq!(parts.principal_paid, diff(before.principal_outstanding, after.principal_outstanding), "{what}");
            assert_eq!(parts.fee_paid, diff(before.management_fee_due, after.management_fee_due), "{what}");
            assert_eq!(parts.interest_paid, diff(before.interest_due, after.interest_due), "{what}");
            assert_eq!(parts.value_change, Number::ZERO, "{what}");
            let total = parts.principal_paid.add(parts.interest_paid, A).unwrap().add(parts.fee_paid, A).unwrap();
            assert!(total <= offer, "{what}: payment {made} took {} of {}", show(total), show(offer));
            for part in [parts.principal_paid, parts.interest_paid, parts.fee_paid] {
                assert!(is_rounded(asset, part, loan_scale_of(&loan)).unwrap() && !is_negative(&part), "{what}");
            }
            paid.accumulate(&parts).unwrap();
            assert!(made <= payments, "{what}: more payments than scheduled");
        }
        assert_eq!(made, payments, "{what}: the schedule's length");
        for field in ["PrincipalOutstanding", "TotalValueOutstanding", "ManagementFeeOutstanding", "NextPaymentDueDate", "PaymentRemaining"] {
            let left = loan.get(field).cloned().unwrap_or(json!(0));
            assert!(left == json!(0), "{what}: {field} left at {left}");
        }
        assert_eq!(uint(&loan, "PreviousPaymentDueDate"), start as u64 + payments as u64 * MONTH as u64, "{what}");
        assert_eq!(paid.principal_paid, i(principal), "{what}: principal repaid");
        assert_eq!(paid.fee_paid, total_fee, "{what}: management fee paid");
        assert_eq!(paid.principal_paid.add(paid.interest_paid, A).unwrap().add(paid.fee_paid, A).unwrap(), total_value, "{what}: value repaid");
    }
}

/// LendingHelpers.cpp:1661-1733: several scheduled payments in one LoanPay,
/// the service fee on each, the leftover ignored without tfLoanOverpayment.
#[test]
fn one_payment_can_cover_several_periods() {
    let _g = large();
    let mut loan = loan_of(MPT, 1200, 0, 12, 0, 0, 1000, MONTH);
    loan["LoanServiceFee"] = json!("5");
    let broker = json!({});
    // Each period is 100 + 5. 330 covers three and leaves 15.
    let pay = |loan: &mut Value, value: i64, kind| loan_make_payment(&R, 1001, MPT, loan, &broker, &amount(MPT, i(value)), kind).unwrap();
    assert_eq!(pay(&mut loan.clone(), 104, LoanPaymentType::Regular), Err(TxResult::InsufficientPayment));
    let parts = pay(&mut loan, 330, LoanPaymentType::Regular).unwrap();
    assert_eq!(parts, LoanPaymentParts { principal_paid: i(300), interest_paid: Number::ZERO, value_change: Number::ZERO, fee_paid: i(15) });
    assert_eq!(uint(&loan, "PaymentRemaining"), 9);
    assert_eq!(uint(&loan, "NextPaymentDueDate"), 1000 + 4 * MONTH as u64);
    assert_eq!(uint(&loan, "PreviousPaymentDueDate"), 1000 + 3 * MONTH as u64);
    assert_eq!(number_field(&loan, "PrincipalOutstanding"), i(900));
    // An overpayment flag on a loan that does not take overpayments changes nothing.
    let parts = pay(&mut loan, 110, LoanPaymentType::Overpayment).unwrap();
    assert_eq!(parts.principal_paid, i(100));
    // More than everything: the eight left, the last one Final.
    let parts = pay(&mut loan, 1_000_000, LoanPaymentType::Regular).unwrap();
    assert_eq!(parts, LoanPaymentParts { principal_paid: i(800), interest_paid: Number::ZERO, value_change: Number::ZERO, fee_paid: i(40) });
    assert_eq!(uint(&loan, "PaymentRemaining"), 0);
    assert!(loan.get("PrincipalOutstanding").is_none());
    // A finished loan: tecKILLED.
    assert_eq!(pay(&mut loan, 105, LoanPaymentType::Regular), Err(TxResult::Killed));
}

/// LendingHelpers.cpp:1694-1702 with Protocol.h:239: at most 100 scheduled
/// payments per transaction, and no overpayment after the hundredth.
#[test]
fn at_most_a_hundred_payments_per_transaction() {
    let _g = large();
    let mut loan = loan_of(MPT, 15_000, 0, 150, 0, 0, 1000, 600);
    loan["Flags"] = json!(LSF_LOAN_OVERPAYMENT);
    let parts = loan_make_payment(&R, 1001, MPT, &mut loan, &json!({}), &amount(MPT, i(14_950)), LoanPaymentType::Overpayment).unwrap().unwrap();
    assert_eq!(parts.principal_paid, i(10_000));
    assert_eq!(uint(&loan, "PaymentRemaining"), 50);
    assert_eq!(number_field(&loan, "PeriodicPayment"), i(100), "no re-amortization");
}

/// LendingHelpers.cpp:1742-1822: what is left after the scheduled payments
/// pays down principal and the loan is re-amortized.
#[test]
fn an_overpayment_reamortizes_the_loan() {
    let _g = large();
    let mut loan = loan_of(IOU, 1000, 10_000, 10, 0, -5, 1000, MONTH);
    loan["Flags"] = json!(LSF_LOAN_OVERPAYMENT);
    let broker = json!({});
    let periodic = number_field(&loan, "PeriodicPayment");
    // What the first scheduled payment takes (no more than the rounded periodic payment).
    let due = compute_payment_components_of(&R, IOU, &loan, loan_periodic_rate(10_000, MONTH).unwrap(), 0).unwrap().tracked_value_delta;
    assert!(due <= round_periodic_payment(IOU, periodic, -5).unwrap());
    let before = loan_state_of(&loan).unwrap();
    let offer = due.add(i(50), A).unwrap();
    let parts = loan_make_payment(&R, 1001, IOU, &mut loan, &broker, &amount(IOU, offer), LoanPaymentType::Overpayment).unwrap().unwrap();
    let after = loan_state_of(&loan).unwrap();
    assert_eq!(uint(&loan, "PaymentRemaining"), 9);
    // The whole offer was taken: one period and 50 of principal.
    assert_eq!(parts.principal_paid.add(parts.interest_paid, A).unwrap().add(parts.fee_paid, A).unwrap(), offer);
    assert_eq!(parts.principal_paid, diff(before.principal_outstanding, after.principal_outstanding));
    // The value fell by more than was paid: future interest saved.
    assert!(is_negative(&parts.value_change));
    assert_eq!(diff(before.value_outstanding, after.value_outstanding), offer.sub(parts.value_change, A).unwrap());
    assert!(number_field(&loan, "PeriodicPayment") < periodic, "a smaller periodic payment");
    // Without the flag on the payment the 50 stays with the borrower.
    let mut plain = loan_of(IOU, 1000, 10_000, 10, 0, -5, 1000, MONTH);
    plain["Flags"] = json!(LSF_LOAN_OVERPAYMENT);
    let parts = loan_make_payment(&R, 1001, IOU, &mut plain, &broker, &amount(IOU, offer), LoanPaymentType::Regular).unwrap().unwrap();
    assert_eq!(parts.principal_paid.add(parts.interest_paid, A).unwrap(), due);
    assert_eq!(number_field(&plain, "PeriodicPayment"), periodic);
    // The re-amortized loan still pays out to exactly zero.
    let mut made = 0;
    while uint(&loan, "PaymentRemaining") > 0 {
        let due = round_periodic_payment(IOU, number_field(&loan, "PeriodicPayment"), -5).unwrap();
        let offer = nmin(due, number_field(&loan, "TotalValueOutstanding"));
        loan_make_payment(&R, 1001, IOU, &mut loan, &broker, &amount(IOU, offer), LoanPaymentType::Regular).unwrap().unwrap();
        made += 1;
        assert!(made <= 9);
    }
    assert_eq!(made, 9);
    assert!(loan.get("TotalValueOutstanding").is_none() && loan.get("PrincipalOutstanding").is_none());
}

/// LendingHelpers.cpp:1045-1120, :2286-2297: a late payment needs the flag
/// (tecEXPIRED without it), is tecTOO_SOON when not late, and costs the
/// late fee and the late interest on top of the scheduled payment; the late
/// interest raises the loan's value.
#[test]
fn a_late_payment_pays_the_penalties() {
    let _g = large();
    let mut loan = loan_of(IOU, 1000, 10_000, 10, 10_000, -5, 1000, MONTH);
    loan["LateInterestRate"] = json!(100_000);
    loan["LatePaymentFee"] = json!("2");
    let broker = json!({"ManagementFeeRate": 10_000});
    let due_date = 1000 + MONTH;
    let late_at = due_date + 1000;
    let due = compute_payment_components_of(&R, IOU, &loan, loan_periodic_rate(10_000, MONTH).unwrap(), 10_000).unwrap().tracked_value_delta;
    let pay = |loan: &mut Value, now: u32, value: Number, kind| loan_make_payment(&R, now, IOU, loan, &broker, &amount(IOU, value), kind).unwrap();
    assert_eq!(pay(&mut loan.clone(), late_at, i(500), LoanPaymentType::Regular), Err(TxResult::Expired));
    assert_eq!(pay(&mut loan.clone(), late_at, i(500), LoanPaymentType::Full), Err(TxResult::Expired));
    assert_eq!(pay(&mut loan.clone(), due_date, i(500), LoanPaymentType::Late), Err(TxResult::TooSoon));
    // 1000 s late at 100% a year on 1000: 0.0317097919837645865, rounded to
    // 0.03171 at scale -5 (nearest); a tenth of it, rounded down, is the
    // broker's: 0.00317; the vault's 0.02854.
    let penalty_interest = n(2854, -5);
    let penalty_fee = n(317, -5);
    let total = due.add(penalty_interest, A).unwrap().add(i(2), A).unwrap().add(penalty_fee, A).unwrap();
    assert_eq!(pay(&mut loan.clone(), late_at, total.sub(n(1, -5), A).unwrap(), LoanPaymentType::Late), Err(TxResult::InsufficientPayment));
    let before = loan_state_of(&loan).unwrap();
    let parts = pay(&mut loan, late_at, total, LoanPaymentType::Late).unwrap();
    let after = loan_state_of(&loan).unwrap();
    assert_eq!(show(parts.value_change), show(penalty_interest));
    assert_eq!(parts.principal_paid.add(parts.interest_paid, A).unwrap().add(parts.fee_paid, A).unwrap(), total);
    assert_eq!(parts.fee_paid, diff(before.management_fee_due, after.management_fee_due).add(i(2), A).unwrap().add(penalty_fee, A).unwrap());
    assert_eq!(uint(&loan, "PaymentRemaining"), 9);
    assert_eq!(uint(&loan, "NextPaymentDueDate"), (due_date + MONTH) as u64);
}

/// LendingHelpers.cpp:1141-1245: an early full payment closes the loan for
/// the principal, the tracked fee, the interest accrued since the last due
/// date plus the close rate of the theoretical principal, and the close fee.
#[test]
fn a_full_payment_closes_the_loan_early() {
    let _g = large();
    let mut loan = loan_of(IOU, 1000, 10_000, 10, 0, -5, 1000, MONTH);
    loan["CloseInterestRate"] = json!(10_000);
    loan["ClosePaymentFee"] = json!("3");
    let broker = json!({});
    let now = 1000 + MONTH / 2;
    let pay = |loan: &mut Value, value: Number| loan_make_payment(&R, now, IOU, loan, &broker, &amount(IOU, value), LoanPaymentType::Full).unwrap();
    // Half a period of interest on 1000 at 10% a year over a 30-day period,
    // plus 10% of 1000, rounded down at scale -5.
    let rate = loan_periodic_rate(10_000, MONTH).unwrap();
    let theoretical = loan_principal_from_periodic_payment(&R, number_field(&loan, "PeriodicPayment"), rate, 10).unwrap();
    let interest = round_to_asset(IOU, compute_full_payment_interest(theoretical, rate, now, MONTH, 0, 1000, 10_000).unwrap(), -5, Rounding::Downward).unwrap();
    let total = i(1000).add(interest, A).unwrap().add(i(3), A).unwrap();
    assert_eq!(pay(&mut loan.clone(), total.sub(n(1, -5), A).unwrap()), Err(TxResult::InsufficientPayment));
    let before = loan_state_of(&loan).unwrap();
    let parts = pay(&mut loan, total).unwrap();
    assert_eq!(parts.principal_paid, i(1000));
    assert_eq!(parts.interest_paid, interest);
    assert_eq!(parts.fee_paid, i(3));
    assert_eq!(parts.value_change, diff(interest, before.interest_due));
    assert_eq!(uint(&loan, "PaymentRemaining"), 0);
    assert!(loan.get("TotalValueOutstanding").is_none());
    assert_eq!(uint(&loan, "PreviousPaymentDueDate"), (1000 + MONTH) as u64);
    // The last payment cannot be a full payment.
    let mut last = loan_of(IOU, 1000, 10_000, 1, 0, -5, 1000, MONTH);
    assert_eq!(pay(&mut last, i(2000)), Err(TxResult::Killed));
}

/// LendingHelpers.cpp:2168-2244: the loan scale is the larger of the
/// minimum (the vault's) and the total value's exponent; the total value is
/// rounded UP when there is interest; the management fee DOWN.
#[test]
fn loan_properties() {
    let _g = large();
    // 1000 at 100% a year, three monthly payments: 389.569066396123265 each.
    let p = compute_loan_properties(&R, IOU, i(1000), 100_000, MONTH, 3, 10_000, -20).unwrap();
    assert_eq!(p.periodic_payment, n(389569066396123265, -15));
    // 1168.707199188369795 as sixteen digits rounded up: 1168.707199188370, exponent -12.
    assert_eq!(p.loan_scale, -12);
    assert_eq!(show(p.loan_state.value_outstanding), "1168.70719918837");
    assert_eq!(p.loan_state.principal_outstanding, i(1000));
    // 10% of 168.70719918837 rounded down at -12.
    assert_eq!(show(p.loan_state.management_fee_due), "16.870719918837");
    assert_eq!(show(p.loan_state.interest_due), "151.836479269533");
    // A coarser minimum scale wins and the value is rounded up to it.
    let p = compute_loan_properties(&R, IOU, i(1000), 100_000, MONTH, 3, 10_000, -5).unwrap();
    assert_eq!(p.loan_scale, -5);
    assert_eq!(show(p.loan_state.value_outstanding), "1168.7072");
    assert_eq!(show(p.loan_state.management_fee_due), "16.87072");
    // An integral asset: scale 0, whole units, the value rounded up.
    let p = compute_loan_properties(&R, MPT, i(1000), 100_000, MONTH, 3, 10_000, 0).unwrap();
    assert_eq!(p.loan_scale, 0);
    assert_eq!(p.loan_state.value_outstanding, i(1169));
    assert_eq!(p.loan_state.management_fee_due, i(16));
    // No interest: to nearest, and the value is the principal.
    let p = compute_loan_properties(&R, IOU, i(1000), 0, MONTH, 3, 10_000, -5).unwrap();
    assert_eq!(p.loan_state.value_outstanding, i(1000));
    assert_eq!(p.loan_state.management_fee_due, Number::ZERO);
    assert!(is_positive(&p.first_payment_principal));
}
