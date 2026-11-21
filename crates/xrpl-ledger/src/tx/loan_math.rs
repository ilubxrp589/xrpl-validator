//! The loan maths of rippled 3.4.1 `LendingHelpers`
//! (src/libxrpl/ledger/helpers/LendingHelpers.cpp:118-2315 and the structs of
//! include/xrpl/ledger/helpers/LendingHelpers.h:99-674), ported function for
//! function for the four Loan transactors (`tx::loan`):
//!
//! | here | rippled (LendingHelpers.cpp) |
//! |---|---|
//! | [`loan_periodic_rate`] | `loanPeriodicRate` :154-159 |
//! | [`is_rounded`] | `isRounded` :165-170 |
//! | [`is_payment_late`] | `isPaymentLate` :172-180 |
//! | [`accrual`], [`cash_basis`], the four dispatchers | :182-301 |
//! | [`compute_power_minus_one`], [`compute_power_minus_one_hybrid`] | :328-388 |
//! | [`compute_payment_factor`] | `computePaymentFactor` :395-423 |
//! | [`loan_periodic_payment`] | `loanPeriodicPayment` :430-445 |
//! | [`loan_principal_from_periodic_payment`] | :452-466 |
//! | [`compute_interest_and_fee_parts`], `round_and_split_interest` | :473-502 |
//! | [`loan_late_payment_interest`] | `loanLatePaymentInterest` :509-535 |
//! | [`loan_accrued_interest`] | `loanAccruedInterest` :542-572 |
//! | `do_payment` | `doPayment` :580-686 |
//! | [`try_overpayment`], `do_overpayment` | :698-1030 |
//! | `compute_late_payment`, `compute_full_payment` | :1045-1245 |
//! | [`compute_payment_components`] | `computePaymentComponents` :1271-1506 |
//! | [`compute_overpayment_components`] | :1526-1582 |
//! | `make_full_payment`, `make_late_payment`, `make_regular_payment` | :1588-1841 |
//! | the `LoanState` operators | :1845-1881 |
//! | [`check_loan_guards`] | `checkLoanGuards` :1883-1959 |
//! | [`compute_full_payment_interest`] | :1967-2001 |
//! | [`compute_theoretical_loan_state`] | :2025-2064 |
//! | [`construct_loan_state`], [`loan_state_of`] | :2086-2110 |
//! | [`compute_management_fee`] | `computeManagementFee` :2118-2127 |
//! | [`compute_loan_properties`], [`compute_loan_properties_at_rate`] | :2137-2244 |
//! | [`loan_make_payment`] | `loanMakePayment` :2252-2314 |
//! | [`adjust_imprecise_number`] | LendingHelpers.h:243-255 |
//!
//! # Rounding
//!
//! rippled's rounding mode is a thread-local; here every `Number` operation
//! names its mode. Outside the guards the helpers install themselves
//! (`computeLoanProperties`' total value, `checkLoanGuards`' payment count)
//! and the modes they pass to `roundToAsset`, everything runs at the
//! thread's default, ToNearest ([`A`]): no loan transactor calls these
//! functions from inside a guard of its own, and LoanPay asserts as much
//! (LoanPay.cpp:751-754). The order of the operations of every expression is
//! C++'s (left to right, one rounding per operation).
//!
//! # Exceptions
//!
//! An `Err` is a C++ exception (a `Number` overflow or division by zero, an
//! amount outside its asset's range): tefEXCEPTION in the transactor.
//!
//! # Asserts
//!
//! rippled's `XRPL_ASSERT`s are compiled out of a release server; none is
//! ported as a check. Where a release build goes on past a failed assert, so
//! does this code.
//!
//! # Amendments
//!
//! [`LoanRules`] carries the three the maths branches on: fixCleanup3_1_3
//! (the overpayment amount truncated at the loan scale), fixCleanup3_2_0
//! (the payment factor's binomial branch, principal up / interest down in a
//! payment's target, the exact principal after an overpayment, the rounded
//! overpayment) and fixCleanup3_4_0 (a payment is late only AFTER its due
//! date). The accounting model (accrual or cash basis) is the vault's
//! `sfLEVersion` alone (LendingHelpers.cpp:255-259).
//!
//! NOT ported: LendingProtocolV1_2's FixedPrecision helpers of 3.5.0-rc1
//! (`Supported::No`) — in rc1's LendingHelpers.cpp everything keyed on
//! `VaultVersion::FixedPrecision`.

use crate::flow::st_amount::StError;
use crate::ledger::amendments as am;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::TxResult;
use crate::tx::mpt::HeldAsset;
use crate::tx::number::{Number, Rounding};
use crate::tx::protocol::{tenth_bips_of_value, Soe, VaultVersion, LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION, LSF_LOAN_OVERPAYMENT};
use crate::tx::st_number::{number_field, round_to_asset, scale, set_number_field, Amount};
use crate::tx::vault_helpers::vault_version;
use serde_json::Value;

/// A value or a C++ exception.
pub type M<T> = Result<T, StError>;

/// The thread's default rounding mode.
pub const A: Rounding = Rounding::ToNearest;

/// `kSecondsInYear` (LendingHelpers.h:99).
pub const SECONDS_IN_YEAR: u32 = 365 * 24 * 60 * 60;

fn arith<T>(r: Result<T, crate::tx::number::NumberError>) -> M<T> {
    r.map_err(|_| StError::Overflow)
}

/// `Number{v}` for an unsigned integer (exact: at most ten digits).
pub fn num(v: u64) -> Number {
    Number::from_parts(false, v as u128, 0, A).unwrap_or(Number::ZERO)
}

fn add(a: Number, b: Number) -> M<Number> {
    arith(a.add(b, A))
}

fn sub(a: Number, b: Number) -> M<Number> {
    arith(a.sub(b, A))
}

fn mul(a: Number, b: Number) -> M<Number> {
    arith(a.mul(b, A))
}

fn div(a: Number, b: Number) -> M<Number> {
    arith(a.div(b, A))
}

fn tenth_bips(value: Number, rate: u32) -> M<Number> {
    arith(tenth_bips_of_value(value, rate, A))
}

fn is_negative(n: &Number) -> bool {
    n.negative && !n.is_zero()
}

fn is_positive(n: &Number) -> bool {
    !n.negative && !n.is_zero()
}

/// `std::min(a, b)`: `a` unless `b < a`.
fn nmin(a: Number, b: Number) -> Number {
    if b < a {
        b
    } else {
        a
    }
}

/// `std::max(a, b)`: `a` unless `a < b`.
fn nmax(a: Number, b: Number) -> Number {
    if a < b {
        b
    } else {
        a
    }
}

/// `std::clamp(v, lo, hi)`.
fn clamp(v: Number, lo: Number, hi: Number) -> Number {
    if v < lo {
        lo
    } else if hi < v {
        hi
    } else {
        v
    }
}

fn uint(obj: &Value, field: &str) -> u64 {
    obj.get(field).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// The amendments the loan maths branches on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoanRules {
    pub fix_cleanup_3_1_3: bool,
    pub fix_cleanup_3_2_0: bool,
    pub fix_cleanup_3_4_0: bool,
}

impl LoanRules {
    pub fn of(sandbox: &Sandbox) -> LoanRules {
        LoanRules {
            fix_cleanup_3_1_3: am::enabled(sandbox, am::FIX_CLEANUP_3_1_3),
            fix_cleanup_3_2_0: am::fix_cleanup_3_2_0(sandbox),
            fix_cleanup_3_4_0: am::fix_cleanup_3_4_0(sandbox),
        }
    }

    /// Every fix on: mainnet's rules.
    pub const ALL: LoanRules = LoanRules { fix_cleanup_3_1_3: true, fix_cleanup_3_2_0: true, fix_cleanup_3_4_0: true };
}

// ---------------------------------------------------------------------------
// The structs (LendingHelpers.h:128-238, :335-339, :408-538)
// ---------------------------------------------------------------------------

/// `LoanPaymentParts` (LendingHelpers.h:128-168): what a payment moves.
/// `principal_paid + interest_paid` goes to the vault, `fee_paid` to the
/// broker; `value_change` is how the loan's total value moved beyond normal
/// amortization.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoanPaymentParts {
    pub principal_paid: Number,
    pub interest_paid: Number,
    pub value_change: Number,
    pub fee_paid: Number,
}

impl LoanPaymentParts {
    pub const ZERO: LoanPaymentParts =
        LoanPaymentParts { principal_paid: Number::ZERO, interest_paid: Number::ZERO, value_change: Number::ZERO, fee_paid: Number::ZERO };

    /// `operator+=` (LendingHelpers.cpp:118-140).
    pub fn accumulate(&mut self, other: &LoanPaymentParts) -> M<()> {
        self.principal_paid = add(self.principal_paid, other.principal_paid)?;
        self.interest_paid = add(self.interest_paid, other.interest_paid)?;
        self.value_change = add(self.value_change, other.value_change)?;
        self.fee_paid = add(self.fee_paid, other.fee_paid)?;
        Ok(())
    }
}

/// `LoanState` (LendingHelpers.h:182-205).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoanState {
    /// Everything the borrower still owes.
    pub value_outstanding: Number,
    pub principal_outstanding: Number,
    /// The interest still due to the vault.
    pub interest_due: Number,
    /// The management fee still due to the broker.
    pub management_fee_due: Number,
}

impl LoanState {
    /// `interestOutstanding()` (LendingHelpers.h:196-204):
    /// `interestDue + managementFeeDue`.
    pub fn interest_outstanding(&self) -> M<Number> {
        add(self.interest_due, self.management_fee_due)
    }
}

/// `LoanProperties` (LendingHelpers.h:216-238).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoanProperties {
    /// Unrounded.
    pub periodic_payment: Number,
    /// Rounded to `loan_scale`.
    pub loan_state: LoanState,
    pub loan_scale: i32,
    /// The principal part of the first payment, unrounded.
    pub first_payment_principal: Number,
}

/// `AccountingDeltas` (LendingHelpers.h:335-339): what one accounting touch
/// point adds to Vault.AssetsTotal and adds to (origination) or takes from
/// (payment) LoanBroker.DebtTotal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AccountingDeltas {
    pub assets_total_delta: Number,
    pub debt_total_delta: Number,
}

/// `detail::PaymentSpecialCase` (LendingHelpers.h:408).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaymentSpecialCase {
    None,
    /// The payment that closes the loan.
    Final,
    /// An overpayment.
    Extra,
}

/// `detail::PaymentComponents` (LendingHelpers.h:419-456): what one payment
/// takes off the Loan's tracked fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PaymentComponents {
    pub tracked_value_delta: Number,
    pub tracked_principal_delta: Number,
    pub tracked_management_fee_delta: Number,
    pub special_case: PaymentSpecialCase,
}

impl PaymentComponents {
    /// `trackedInterestPart()` (LendingHelpers.cpp:1247-1251):
    /// `value - (principal + fee)`.
    pub fn tracked_interest_part(&self) -> M<Number> {
        sub(self.tracked_value_delta, add(self.tracked_principal_delta, self.tracked_management_fee_delta)?)
    }
}

/// `detail::ExtendedPaymentComponents` (LendingHelpers.h:474-505).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExtendedPaymentComponents {
    pub tracked: PaymentComponents,
    /// Fees straight to the broker (service, late, close, overpayment).
    pub untracked_management_fee: Number,
    /// Interest straight to the vault (late or prepayment penalty).
    pub untracked_interest: Number,
    /// `trackedValueDelta + untrackedInterest + untrackedManagementFee`.
    pub total_due: Number,
}

impl ExtendedPaymentComponents {
    /// The constructor (LendingHelpers.h:498-504).
    pub fn new(tracked: PaymentComponents, fee: Number, interest: Number) -> M<ExtendedPaymentComponents> {
        let total_due = add(add(tracked.tracked_value_delta, interest)?, fee)?;
        Ok(ExtendedPaymentComponents { tracked, untracked_management_fee: fee, untracked_interest: interest, total_due })
    }
}

/// `detail::LoanStateDeltas` (LendingHelpers.h:514-538).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoanStateDeltas {
    pub principal: Number,
    pub interest: Number,
    pub management_fee: Number,
}

impl LoanStateDeltas {
    /// `total()`: `principal + interest + managementFee`.
    pub fn total(&self) -> M<Number> {
        add(add(self.principal, self.interest)?, self.management_fee)
    }

    /// `nonNegative()` (LendingHelpers.cpp:305-314).
    pub fn non_negative(&mut self) {
        for part in [&mut self.principal, &mut self.interest, &mut self.management_fee] {
            if is_negative(part) {
                *part = Number::ZERO;
            }
        }
    }
}

/// `operator-(LoanState, LoanState)` (LendingHelpers.cpp:1845-1855).
pub fn state_difference(lhs: &LoanState, rhs: &LoanState) -> M<LoanStateDeltas> {
    Ok(LoanStateDeltas {
        principal: sub(lhs.principal_outstanding, rhs.principal_outstanding)?,
        interest: sub(lhs.interest_due, rhs.interest_due)?,
        management_fee: sub(lhs.management_fee_due, rhs.management_fee_due)?,
    })
}

/// `operator+(LoanState, LoanStateDeltas)` (LendingHelpers.cpp:1870-1881).
pub fn state_plus(lhs: &LoanState, rhs: &LoanStateDeltas) -> M<LoanState> {
    Ok(LoanState {
        value_outstanding: add(lhs.value_outstanding, rhs.total()?)?,
        principal_outstanding: add(lhs.principal_outstanding, rhs.principal)?,
        interest_due: add(lhs.interest_due, rhs.interest)?,
        management_fee_due: add(lhs.management_fee_due, rhs.management_fee)?,
    })
}

/// `operator-(LoanState, LoanStateDeltas)` (LendingHelpers.cpp:1857-1868).
pub fn state_minus(lhs: &LoanState, rhs: &LoanStateDeltas) -> M<LoanState> {
    Ok(LoanState {
        value_outstanding: sub(lhs.value_outstanding, rhs.total()?)?,
        principal_outstanding: sub(lhs.principal_outstanding, rhs.principal)?,
        interest_due: sub(lhs.interest_due, rhs.interest)?,
        management_fee_due: sub(lhs.management_fee_due, rhs.management_fee)?,
    })
}

// ---------------------------------------------------------------------------
// Rates, rounding, lateness
// ---------------------------------------------------------------------------

/// `loanPeriodicRate(interestRate, paymentInterval)` (LendingHelpers.cpp:
/// 154-159): the yearly rate (in 1/10 bips) prorated to one payment
/// interval — `interval × rate / 100000 / 31536000`, in that order.
pub fn loan_periodic_rate(interest_rate: u32, payment_interval: u32) -> M<Number> {
    div(tenth_bips(num(payment_interval as u64), interest_rate)?, num(SECONDS_IN_YEAR as u64))
}

/// `roundPeriodicPayment(asset, periodicPayment, scale)` (LendingHelpers.h:
/// 107-111): always up.
pub fn round_periodic_payment(asset: HeldAsset, periodic_payment: Number, loan_scale: i32) -> M<Number> {
    round_to_asset(asset, periodic_payment, loan_scale, Rounding::Upward)
}

/// `isRounded(asset, value, scale)` (LendingHelpers.cpp:165-170): rounding
/// down and rounding up give the same value.
pub fn is_rounded(asset: HeldAsset, value: Number, loan_scale: i32) -> M<bool> {
    Ok(round_to_asset(asset, value, loan_scale, Rounding::Downward)? == round_to_asset(asset, value, loan_scale, Rounding::Upward)?)
}

/// `isPaymentLate(view, loanSle)` (LendingHelpers.cpp:172-180):
/// `hasExpired(view, sfNextPaymentDueDate, …)` — with fixCleanup3_4_0 the
/// parent close time must have PASSED the due date, before it reaching the
/// due date is late. `now` is the parent ledger's close time.
pub fn is_payment_late(rules: &LoanRules, now: u32, loan: &Value) -> bool {
    let due = uint(loan, "NextPaymentDueDate");
    if rules.fix_cleanup_3_4_0 {
        now as u64 > due
    } else {
        now as u64 >= due
    }
}

/// `adjustImpreciseNumber(value, adjustment, asset, vaultScale)`
/// (LendingHelpers.h:243-255): `value + adjustment` re-rounded to the
/// vault's scale (`roundToAsset` at the thread's mode) and never below zero.
pub fn adjust_imprecise_number(value: Number, adjustment: Number, asset: HeldAsset, vault_scale: i32) -> M<Number> {
    let adjusted = round_to_asset(asset, add(value, adjustment)?, vault_scale, A)?;
    Ok(if is_negative(&adjusted) { Number::ZERO } else { adjusted })
}

// ---------------------------------------------------------------------------
// The two accounting models (LendingHelpers.cpp:182-301)
// ---------------------------------------------------------------------------

/// Whole-life recognition (a Legacy vault): interest enters AssetsTotal and
/// DebtTotal at origination. 3.5.0-rc1 renames the namespace
/// `instant_recognition`.
pub mod accrual {
    use super::*;

    /// `accrual::loanOriginationDeltas` (:184-188).
    pub fn loan_origination_deltas(principal_requested: Number, interest_due: Number) -> M<AccountingDeltas> {
        Ok(AccountingDeltas { assets_total_delta: interest_due, debt_total_delta: add(principal_requested, interest_due)? })
    }

    /// `accrual::loanOriginationExceedsVaultMaximum` (:190-197).
    pub fn loan_origination_exceeds_vault_maximum(vault_maximum: Number, vault_total: Number, interest_due: Number) -> M<bool> {
        Ok(!vault_maximum.is_zero() && sub(vault_maximum, vault_total)? < interest_due)
    }

    /// `accrual::loanVaultExposure` (:206-210): principal plus the interest
    /// due to the vault, `TotalValueOutstanding - ManagementFeeOutstanding`.
    pub fn loan_vault_exposure(loan: &Value) -> M<Number> {
        sub(number_field(loan, "TotalValueOutstanding"), number_field(loan, "ManagementFeeOutstanding"))
    }

    /// `accrual::loanPaymentDeltas` (:212-218).
    pub fn loan_payment_deltas(parts: &LoanPaymentParts) -> M<AccountingDeltas> {
        Ok(AccountingDeltas {
            assets_total_delta: parts.value_change,
            debt_total_delta: sub(add(parts.principal_paid, parts.interest_paid)?, parts.value_change)?,
        })
    }
}

/// Cash-basis recognition (a vault created under LendingProtocolV1_1,
/// `sfLEVersion` 1): the totals carry principal only, interest is
/// recognized as it is paid.
pub mod cash_basis {
    use super::*;

    /// `cash_basis::loanOriginationDeltas` (:224-228).
    pub fn loan_origination_deltas(principal_requested: Number) -> AccountingDeltas {
        AccountingDeltas { assets_total_delta: Number::ZERO, debt_total_delta: principal_requested }
    }

    /// `cash_basis::loanVaultExposure` (:235-239).
    pub fn loan_vault_exposure(loan: &Value) -> Number {
        number_field(loan, "PrincipalOutstanding")
    }

    /// `cash_basis::loanPaymentDeltas` (:241-245).
    pub fn loan_payment_deltas(parts: &LoanPaymentParts) -> AccountingDeltas {
        AccountingDeltas { assets_total_delta: parts.interest_paid, debt_total_delta: parts.principal_paid }
    }
}

/// `cashBasisEnabled(vaultSle)` (:255-259): the vault's own version decides —
/// a vault created before LendingProtocolV1_1 keeps accrual accounting for
/// ever. (3.5.0-rc1: `>= CashBasis`, the same for the two versions that
/// exist without LendingProtocolV1_2.)
pub fn cash_basis_enabled(vault: &Value) -> bool {
    vault_version(vault) == VaultVersion::CashBasis
}

/// `loanOriginationDeltas(vaultSle, principalRequested, interestDue)` (:263-272).
pub fn loan_origination_deltas(vault: &Value, principal_requested: Number, interest_due: Number) -> M<AccountingDeltas> {
    if cash_basis_enabled(vault) {
        Ok(cash_basis::loan_origination_deltas(principal_requested))
    } else {
        accrual::loan_origination_deltas(principal_requested, interest_due)
    }
}

/// `loanOriginationExceedsVaultMaximum(vaultSle, vaultTotal, interestDue)`
/// (:274-287).
pub fn loan_origination_exceeds_vault_maximum(vault: &Value, vault_total: Number, interest_due: Number) -> M<bool> {
    if cash_basis_enabled(vault) {
        return Ok(false);
    }
    accrual::loan_origination_exceeds_vault_maximum(number_field(vault, "AssetsMaximum"), vault_total, interest_due)
}

/// `loanVaultExposure(vaultSle, loanSle)` (:289-294).
pub fn loan_vault_exposure(vault: &Value, loan: &Value) -> M<Number> {
    if cash_basis_enabled(vault) {
        Ok(cash_basis::loan_vault_exposure(loan))
    } else {
        accrual::loan_vault_exposure(loan)
    }
}

/// `loanPaymentDeltas(vaultSle, parts)` (:296-301).
pub fn loan_payment_deltas(vault: &Value, parts: &LoanPaymentParts) -> M<AccountingDeltas> {
    if cash_basis_enabled(vault) {
        Ok(cash_basis::loan_payment_deltas(parts))
    } else {
        accrual::loan_payment_deltas(parts)
    }
}

// ---------------------------------------------------------------------------
// The amortization formulas
// ---------------------------------------------------------------------------

/// `computePowerMinusOne(periodicRate, paymentsRemaining)` (:328-353):
/// `(1 + r)^n − 1` as the binomial sum `nr + C(n,2)r² + …`, each term from
/// the one before (`term × r × (n − k) / (k + 1)`, left to right), stopped
/// when a term no longer changes the sum.
pub fn compute_power_minus_one(periodic_rate: Number, payments_remaining: u32) -> M<Number> {
    if payments_remaining == 0 || periodic_rate.is_zero() {
        return Ok(Number::ZERO);
    }
    let mut term = mul(num(payments_remaining as u64), periodic_rate)?;
    let mut sum = term;
    for k in 1..payments_remaining {
        term = div(mul(mul(term, periodic_rate)?, num((payments_remaining - k) as u64))?, num(k as u64 + 1))?;
        let next = add(sum, term)?;
        if next == sum {
            break;
        }
        sum = next;
    }
    Ok(sum)
}

/// `computePowerMinusOneHybrid` (:367-388): the closed form
/// `power(1 + r, n) − 1` when `n × r >= 1e-9`, the binomial sum below it.
pub fn compute_power_minus_one_hybrid(periodic_rate: Number, payments_remaining: u32) -> M<Number> {
    if payments_remaining == 0 || periodic_rate.is_zero() {
        return Ok(Number::ZERO);
    }
    let threshold = arith(Number::from_parts(false, 1, -9, A))?;
    if mul(num(payments_remaining as u64), periodic_rate)? >= threshold {
        return sub(arith(add(Number::one(), periodic_rate)?.power(payments_remaining, A))?, Number::one());
    }
    compute_power_minus_one(periodic_rate, payments_remaining)
}

/// `computePaymentFactor(rules, periodicRate, paymentsRemaining)` (:395-423):
/// `r(1 + r)^n / ((1 + r)^n − 1)`; `1 / n` without interest. Before
/// fixCleanup3_2_0 the denominator is the direct subtraction.
pub fn compute_payment_factor(rules: &LoanRules, periodic_rate: Number, payments_remaining: u32) -> M<Number> {
    if payments_remaining == 0 {
        return Ok(Number::ZERO);
    }
    if periodic_rate.is_zero() {
        return div(Number::one(), num(payments_remaining as u64));
    }
    if rules.fix_cleanup_3_2_0 {
        let raised_minus_one = compute_power_minus_one_hybrid(periodic_rate, payments_remaining)?;
        let raised = add(Number::one(), raised_minus_one)?;
        return div(mul(periodic_rate, raised)?, raised_minus_one);
    }
    let raised = arith(add(Number::one(), periodic_rate)?.power(payments_remaining, A))?;
    div(mul(periodic_rate, raised)?, sub(raised, Number::one())?)
}

/// `loanPeriodicPayment` (:430-445).
pub fn loan_periodic_payment(rules: &LoanRules, principal_outstanding: Number, periodic_rate: Number, payments_remaining: u32) -> M<Number> {
    if principal_outstanding.is_zero() || payments_remaining == 0 {
        return Ok(Number::ZERO);
    }
    if periodic_rate.is_zero() {
        return div(principal_outstanding, num(payments_remaining as u64));
    }
    mul(principal_outstanding, compute_payment_factor(rules, periodic_rate, payments_remaining)?)
}

/// `loanPrincipalFromPeriodicPayment` (:452-466).
pub fn loan_principal_from_periodic_payment(
    rules: &LoanRules,
    periodic_payment: Number,
    periodic_rate: Number,
    payments_remaining: u32,
) -> M<Number> {
    if payments_remaining == 0 {
        return Ok(Number::ZERO);
    }
    if periodic_rate.is_zero() {
        return mul(periodic_payment, num(payments_remaining as u64));
    }
    div(periodic_payment, compute_payment_factor(rules, periodic_rate, payments_remaining)?)
}

/// `computeManagementFee(asset, value, managementFeeRate, scale)`
/// (:2118-2127): the rate of the value, rounded DOWN at the loan scale.
pub fn compute_management_fee(asset: HeldAsset, value: Number, management_fee_rate: u32, loan_scale: i32) -> M<Number> {
    round_to_asset(asset, tenth_bips(value, management_fee_rate)?, loan_scale, Rounding::Downward)
}

/// `computeInterestAndFeeParts` (:473-483): `(interest − fee, fee)`.
pub fn compute_interest_and_fee_parts(asset: HeldAsset, interest: Number, management_fee_rate: u32, loan_scale: i32) -> M<(Number, Number)> {
    let fee = compute_management_fee(asset, interest, management_fee_rate, loan_scale)?;
    Ok((sub(interest, fee)?, fee))
}

/// `roundAndSplitInterest` (:492-502): the raw interest rounded at the loan
/// scale under `mode`, then split.
fn round_and_split_interest(asset: HeldAsset, raw_interest: Number, management_fee_rate: u32, loan_scale: i32, mode: Rounding) -> M<(Number, Number)> {
    let interest = round_to_asset(asset, raw_interest, loan_scale, mode)?;
    compute_interest_and_fee_parts(asset, interest, management_fee_rate, loan_scale)
}

/// `loanLatePaymentInterest` (:509-535): the late rate prorated over the
/// seconds overdue, of the principal. `now` is the parent close time.
pub fn loan_late_payment_interest(principal_outstanding: Number, late_interest_rate: u32, now: u32, next_payment_due_date: u32) -> M<Number> {
    if principal_outstanding.is_zero() || late_interest_rate == 0 || now <= next_payment_due_date {
        return Ok(Number::ZERO);
    }
    let seconds_overdue = now - next_payment_due_date;
    mul(principal_outstanding, loan_periodic_rate(late_interest_rate, seconds_overdue)?)
}

/// `loanAccruedInterest` (:542-572): `principal × rate × seconds / interval`
/// since the later of the previous due date and the start date.
pub fn loan_accrued_interest(
    principal_outstanding: Number,
    periodic_rate: Number,
    now: u32,
    start_date: u32,
    prev_payment_date: u32,
    payment_interval: u32,
) -> M<Number> {
    if periodic_rate.is_zero() || payment_interval == 0 {
        return Ok(Number::ZERO);
    }
    let last_payment_date = prev_payment_date.max(start_date);
    if now <= last_payment_date {
        return Ok(Number::ZERO);
    }
    let seconds = now - last_payment_date;
    div(mul(mul(principal_outstanding, periodic_rate)?, num(seconds as u64))?, num(payment_interval as u64))
}

/// `computeFullPaymentInterest` (:1967-2001): accrued interest plus the
/// prepayment penalty (the close rate of the theoretical principal).
#[allow(clippy::too_many_arguments)]
pub fn compute_full_payment_interest(
    theoretical_principal_outstanding: Number,
    periodic_rate: Number,
    now: u32,
    payment_interval: u32,
    prev_payment_date: u32,
    start_date: u32,
    close_interest_rate: u32,
) -> M<Number> {
    let accrued = loan_accrued_interest(theoretical_principal_outstanding, periodic_rate, now, start_date, prev_payment_date, payment_interval)?;
    let penalty = if close_interest_rate == 0 { Number::ZERO } else { tenth_bips(theoretical_principal_outstanding, close_interest_rate)? };
    add(accrued, penalty)
}

/// `computeTheoreticalLoanState` (:2025-2064): the unrounded state with
/// `paymentRemaining` payments to go.
pub fn compute_theoretical_loan_state(
    rules: &LoanRules,
    periodic_payment: Number,
    periodic_rate: Number,
    payment_remaining: u32,
    management_fee_rate: u32,
) -> M<LoanState> {
    if payment_remaining == 0 {
        return Ok(LoanState {
            value_outstanding: Number::ZERO,
            principal_outstanding: Number::ZERO,
            interest_due: Number::ZERO,
            management_fee_due: Number::ZERO,
        });
    }
    let value_outstanding = mul(periodic_payment, num(payment_remaining as u64))?;
    let principal_outstanding = loan_principal_from_periodic_payment(rules, periodic_payment, periodic_rate, payment_remaining)?;
    let interest_gross = sub(value_outstanding, principal_outstanding)?;
    let management_fee_due = tenth_bips(interest_gross, management_fee_rate)?;
    let interest_due = sub(interest_gross, management_fee_due)?;
    Ok(LoanState { value_outstanding, principal_outstanding, interest_due, management_fee_due })
}

/// `constructLoanState(total, principal, managementFee)` (:2086-2099).
pub fn construct_loan_state(total_value_outstanding: Number, principal_outstanding: Number, management_fee_outstanding: Number) -> M<LoanState> {
    Ok(LoanState {
        value_outstanding: total_value_outstanding,
        principal_outstanding,
        interest_due: sub(sub(total_value_outstanding, principal_outstanding)?, management_fee_outstanding)?,
        management_fee_due: management_fee_outstanding,
    })
}

/// `constructLoanState(loan)` (:2101-2110).
pub fn loan_state_of(loan: &Value) -> M<LoanState> {
    construct_loan_state(
        number_field(loan, "TotalValueOutstanding"),
        number_field(loan, "PrincipalOutstanding"),
        number_field(loan, "ManagementFeeOutstanding"),
    )
}

/// `computeLoanProperties(rules, asset, principal, interestRate,
/// paymentInterval, …)` (:2137-2158).
#[allow(clippy::too_many_arguments)]
pub fn compute_loan_properties(
    rules: &LoanRules,
    asset: HeldAsset,
    principal_outstanding: Number,
    interest_rate: u32,
    payment_interval: u32,
    payments_remaining: u32,
    management_fee_rate: u32,
    minimum_scale: i32,
) -> M<LoanProperties> {
    let periodic_rate = loan_periodic_rate(interest_rate, payment_interval)?;
    compute_loan_properties_at_rate(rules, asset, principal_outstanding, periodic_rate, payments_remaining, management_fee_rate, minimum_scale)
}

/// `computeLoanProperties(rules, asset, principal, periodicRate, …)`
/// (:2168-2244).
///
/// The total value — `periodicPayment × paymentsRemaining` as an STAmount —
/// is computed under one guard, Upward when there is interest and ToNearest
/// when there is none: the product, the conversion to the asset, and the
/// rounding to the loan scale. The loan scale is the larger of
/// `minimumScale` (the vault's) and that amount's exponent.
pub fn compute_loan_properties_at_rate(
    rules: &LoanRules,
    asset: HeldAsset,
    principal_outstanding: Number,
    periodic_rate: Number,
    payments_remaining: u32,
    management_fee_rate: u32,
    minimum_scale: i32,
) -> M<LoanProperties> {
    let periodic_payment = loan_periodic_payment(rules, principal_outstanding, periodic_rate, payments_remaining)?;

    let guard = if periodic_rate.is_zero() { Rounding::ToNearest } else { Rounding::Upward };
    let raw_total = arith(periodic_payment.mul(num(payments_remaining as u64), guard))?;
    let amount = Amount::from_number(asset, raw_total, guard)?;
    let loan_scale = minimum_scale.max(scale(raw_total, asset, guard)?);
    let total_value_outstanding = round_to_asset(asset, amount.number(), loan_scale, guard)?;

    let rounded_principal = round_to_asset(asset, principal_outstanding, loan_scale, Rounding::ToNearest)?;
    let total_interest = sub(total_value_outstanding, rounded_principal)?;
    let fee_owed_to_broker = compute_management_fee(asset, total_interest, management_fee_rate, loan_scale)?;

    // The unrounded principal part of the first payment.
    let starting = compute_theoretical_loan_state(rules, periodic_payment, periodic_rate, payments_remaining, management_fee_rate)?;
    let after_first =
        compute_theoretical_loan_state(rules, periodic_payment, periodic_rate, payments_remaining.wrapping_sub(1), management_fee_rate)?;
    let first_payment_principal = sub(starting.principal_outstanding, after_first.principal_outstanding)?;

    Ok(LoanProperties {
        periodic_payment,
        loan_state: construct_loan_state(total_value_outstanding, rounded_principal, fee_owed_to_broker)?,
        loan_scale,
        first_payment_principal,
    })
}

/// `checkLoanGuards` (:1883-1959): whether a loan with these properties can
/// be amortized at all.
///
/// 1. interest expected but none computed: tecPRECISION_LOSS; 1a. none
///    expected but some computed: tecINTERNAL;
/// 2. the first payment pays no principal: tecPRECISION_LOSS;
/// 3. the periodic payment rounds (up) to zero: tecPRECISION_LOSS;
/// 4. `valueOutstanding / roundedPayment`, taken as an integer under an
///    Upward guard, is not the number of payments: tecPRECISION_LOSS.
pub fn check_loan_guards(
    asset: HeldAsset,
    principal_requested: Number,
    expect_interest: bool,
    payment_total: u32,
    properties: &LoanProperties,
) -> M<TxResult> {
    let total_interest = sub(properties.loan_state.value_outstanding, principal_requested)?;
    if expect_interest && !is_positive(&total_interest) {
        return Ok(TxResult::PrecisionLoss);
    }
    if !expect_interest && is_positive(&total_interest) {
        return Ok(TxResult::TecInternal);
    }
    if !is_positive(&properties.first_payment_principal) {
        return Ok(TxResult::PrecisionLoss);
    }
    let rounded_payment = round_periodic_payment(asset, properties.periodic_payment, properties.loan_scale)?;
    if rounded_payment.is_zero() {
        return Ok(TxResult::PrecisionLoss);
    }
    let up = Rounding::Upward;
    let computed_payments = arith(arith(properties.loan_state.value_outstanding.div(rounded_payment, up))?.to_drops(up))?;
    if computed_payments != payment_total as i64 {
        return Ok(TxResult::PrecisionLoss);
    }
    Ok(TxResult::Success)
}

// ---------------------------------------------------------------------------
// One payment's components
// ---------------------------------------------------------------------------

/// `computePaymentComponents` (:1271-1478): how one scheduled payment splits
/// into principal, interest and management fee.
///
/// The last payment (one remaining, or the whole value no more than one
/// rounded payment) takes everything. Otherwise the target is the
/// theoretical state one payment on, rounded to the loan scale — with
/// fixCleanup3_2_0 principal UP and interest DOWN, before it both at the
/// thread's mode — and the parts are the current state less the target,
/// floored at zero, capped by what is outstanding and by the rounded
/// periodic payment, any excess taken from interest, then fee, then
/// principal.
#[allow(clippy::too_many_arguments)]
pub fn compute_payment_components(
    rules: &LoanRules,
    asset: HeldAsset,
    loan_scale: i32,
    total_value_outstanding: Number,
    principal_outstanding: Number,
    management_fee_outstanding: Number,
    periodic_payment: Number,
    periodic_rate: Number,
    payment_remaining: u32,
    management_fee_rate: u32,
) -> M<PaymentComponents> {
    let rounded_periodic_payment = round_periodic_payment(asset, periodic_payment, loan_scale)?;
    if payment_remaining == 1 || total_value_outstanding <= rounded_periodic_payment {
        return Ok(PaymentComponents {
            tracked_value_delta: total_value_outstanding,
            tracked_principal_delta: principal_outstanding,
            tracked_management_fee_delta: management_fee_outstanding,
            special_case: PaymentSpecialCase::Final,
        });
    }

    let true_target =
        compute_theoretical_loan_state(rules, periodic_payment, periodic_rate, payment_remaining.wrapping_sub(1), management_fee_rate)?;
    let fix320 = rules.fix_cleanup_3_2_0;
    let principal_rounding = if fix320 { Rounding::Upward } else { A };
    let interest_rounding = if fix320 { Rounding::Downward } else { A };
    let rounded_target = LoanState {
        value_outstanding: round_to_asset(asset, true_target.value_outstanding, loan_scale, A)?,
        principal_outstanding: round_to_asset(asset, true_target.principal_outstanding, loan_scale, principal_rounding)?,
        interest_due: round_to_asset(asset, true_target.interest_due, loan_scale, interest_rounding)?,
        management_fee_due: round_to_asset(asset, true_target.management_fee_due, loan_scale, A)?,
    };
    let current = construct_loan_state(total_value_outstanding, principal_outstanding, management_fee_outstanding)?;

    let mut deltas = state_difference(&current, &rounded_target)?;
    deltas.non_negative();

    deltas.principal = nmin(deltas.principal, current.principal_outstanding);
    // std::min({interest, max(0, payment − principal), interestDue}).
    let left_after_principal = nmax(Number::ZERO, sub(rounded_periodic_payment, deltas.principal)?);
    deltas.interest = nmin(nmin(deltas.interest, left_after_principal), current.interest_due);
    // std::min({fee, payment − (principal + interest), feeDue}).
    let left_after_interest = sub(rounded_periodic_payment, add(deltas.principal, deltas.interest)?)?;
    deltas.management_fee = nmin(nmin(deltas.management_fee, left_after_interest), current.management_fee_due);

    // `takeFrom` / `addressExcess` (:1383-1403).
    fn take_from(component: &mut Number, excess: &mut Number) -> M<()> {
        if is_positive(excess) {
            let part = nmin(*component, *excess);
            *component = sub(*component, part)?;
            *excess = sub(*excess, part)?;
        }
        Ok(())
    }
    fn address_excess(deltas: &mut LoanStateDeltas, excess: &mut Number) -> M<()> {
        take_from(&mut deltas.interest, excess)?;
        take_from(&mut deltas.management_fee, excess)?;
        take_from(&mut deltas.principal, excess)
    }

    let mut total_overpayment = sub(deltas.total()?, current.value_outstanding)?;
    if is_positive(&total_overpayment) {
        // UNREACHABLE in rippled; a release build goes on.
        address_excess(&mut deltas, &mut total_overpayment)?;
    }

    let shortage = sub(rounded_periodic_payment, deltas.total()?)?;
    if is_negative(&shortage) {
        let mut excess = shortage.negated();
        address_excess(&mut deltas, &mut excess)?;
    }

    Ok(PaymentComponents {
        tracked_value_delta: clamp(deltas.total()?, Number::ZERO, current.value_outstanding),
        tracked_principal_delta: clamp(deltas.principal, Number::ZERO, current.principal_outstanding),
        tracked_management_fee_delta: clamp(deltas.management_fee, Number::ZERO, current.management_fee_due),
        special_case: PaymentSpecialCase::None,
    })
}

/// The overload reading the tracked fields off the Loan (:1487-1506).
fn compute_payment_components_of(rules: &LoanRules, asset: HeldAsset, loan: &Value, periodic_rate: Number, management_fee_rate: u32) -> M<PaymentComponents> {
    compute_payment_components(
        rules,
        asset,
        loan_scale_of(loan),
        number_field(loan, "TotalValueOutstanding"),
        number_field(loan, "PrincipalOutstanding"),
        number_field(loan, "ManagementFeeOutstanding"),
        number_field(loan, "PeriodicPayment"),
        periodic_rate,
        uint(loan, "PaymentRemaining") as u32,
        management_fee_rate,
    )
}

/// `loan->at(sfLoanScale)` (an INT32, absent at zero).
pub fn loan_scale_of(loan: &Value) -> i32 {
    loan.get("LoanScale").and_then(|v| v.as_i64()).unwrap_or(0) as i32
}

/// `computeOverpaymentComponents` (:1526-1582): of an overpayment, the fee
/// (`overpaymentFeeRate`, to the broker), the penalty interest
/// (`overpaymentInterestRate`, split into the vault's part and the
/// management fee) and the rest, which pays down principal.
pub fn compute_overpayment_components(
    asset: HeldAsset,
    loan_scale: i32,
    overpayment: Number,
    overpayment_interest_rate: u32,
    overpayment_fee_rate: u32,
    management_fee_rate: u32,
) -> M<ExtendedPaymentComponents> {
    let overpayment_fee = round_to_asset(asset, tenth_bips(overpayment, overpayment_fee_rate)?, loan_scale, A)?;
    let (rounded_interest, rounded_management_fee) =
        round_and_split_interest(asset, tenth_bips(overpayment, overpayment_interest_rate)?, management_fee_rate, loan_scale, A)?;
    ExtendedPaymentComponents::new(
        PaymentComponents {
            tracked_value_delta: sub(overpayment, overpayment_fee)?,
            tracked_principal_delta: sub(sub(sub(overpayment, rounded_interest)?, rounded_management_fee)?, overpayment_fee)?,
            tracked_management_fee_delta: rounded_management_fee,
            special_case: PaymentSpecialCase::Extra,
        },
        overpayment_fee,
        rounded_interest,
    )
}

// ---------------------------------------------------------------------------
// Applying payments to the Loan
// ---------------------------------------------------------------------------

/// `doPayment(payment, loan)` (:580-686): the Loan's tracked fields after
/// one payment, and what was paid.
///
/// The final payment zeroes PaymentRemaining, the three balances and
/// NextPaymentDueDate (all SoeDefault: the fields go) and records the due
/// date it met as PreviousPaymentDueDate. Any other scheduled payment takes
/// one from PaymentRemaining and moves the two due dates on by one interval;
/// an overpayment (Extra) leaves the schedule alone. The balances fall by
/// the tracked deltas.
fn do_payment(payment: &ExtendedPaymentComponents, loan: &mut Value) -> M<LoanPaymentParts> {
    let tracked = &payment.tracked;
    if tracked.special_case == PaymentSpecialCase::Final {
        loan["PaymentRemaining"] = Value::from(0u32);
        loan["PreviousPaymentDueDate"] = Value::from(uint(loan, "NextPaymentDueDate"));
        loan["NextPaymentDueDate"] = Value::from(0u32);
        for field in ["PrincipalOutstanding", "TotalValueOutstanding", "ManagementFeeOutstanding"] {
            set_number_field(loan, field, Number::ZERO, Soe::Default);
        }
    } else {
        if tracked.special_case != PaymentSpecialCase::Extra {
            let next_due = uint(loan, "NextPaymentDueDate") as u32;
            loan["PaymentRemaining"] = Value::from((uint(loan, "PaymentRemaining") as u32).wrapping_sub(1));
            loan["PreviousPaymentDueDate"] = Value::from(next_due);
            loan["NextPaymentDueDate"] = Value::from(next_due.wrapping_add(uint(loan, "PaymentInterval") as u32));
        }
        let principal = sub(number_field(loan, "PrincipalOutstanding"), tracked.tracked_principal_delta)?;
        let value = sub(number_field(loan, "TotalValueOutstanding"), tracked.tracked_value_delta)?;
        let fee = sub(number_field(loan, "ManagementFeeOutstanding"), tracked.tracked_management_fee_delta)?;
        set_number_field(loan, "PrincipalOutstanding", principal, Soe::Default);
        set_number_field(loan, "TotalValueOutstanding", value, Soe::Default);
        set_number_field(loan, "ManagementFeeOutstanding", fee, Soe::Default);
    }
    Ok(LoanPaymentParts {
        principal_paid: tracked.tracked_principal_delta,
        interest_paid: add(tracked.tracked_interest_part()?, payment.untracked_interest)?,
        value_change: payment.untracked_interest,
        fee_paid: add(tracked.tracked_management_fee_delta, payment.untracked_management_fee)?,
    })
}

/// `tryOverpayment` (:698-888): re-amortize the loan with the overpayment's
/// principal part taken off, on copies. `None` is rippled's
/// `std::unexpected(tesSUCCESS)`: the overpayment is ignored (the loan would
/// fail [`check_loan_guards`], its properties would not compute, or its
/// value would rise).
///
/// The rounding errors the loan has accumulated (`rounded − theoretical`)
/// are carried into the new state. With fixCleanup3_2_0 the new principal is
/// pinned to `old − trackedPrincipalDelta` and the management fee re-derived
/// from the interest that leaves.
#[allow(clippy::too_many_arguments)]
pub fn try_overpayment(
    rules: &LoanRules,
    asset: HeldAsset,
    loan_scale: i32,
    overpayment: &ExtendedPaymentComponents,
    rounded_old_state: &LoanState,
    periodic_payment: Number,
    periodic_rate: Number,
    payment_remaining: u32,
    management_fee_rate: u32,
) -> M<Option<(LoanPaymentParts, LoanProperties)>> {
    let theoretical = compute_theoretical_loan_state(rules, periodic_payment, periodic_rate, payment_remaining, management_fee_rate)?;
    let errors = state_difference(rounded_old_state, &theoretical)?;
    let new_theoretical_principal =
        nmax(sub(theoretical.principal_outstanding, overpayment.tracked.tracked_principal_delta)?, Number::ZERO);

    let mut new_properties = compute_loan_properties_at_rate(
        rules,
        asset,
        new_theoretical_principal,
        periodic_rate,
        payment_remaining,
        management_fee_rate,
        loan_scale,
    )?;

    let new_theoretical_state = {
        let state = state_plus(
            &compute_theoretical_loan_state(rules, new_properties.periodic_payment, periodic_rate, payment_remaining, management_fee_rate)?,
            &errors,
        )?;
        if !rules.fix_cleanup_3_2_0 {
            state
        } else {
            let principal = sub(rounded_old_state.principal_outstanding, overpayment.tracked.tracked_principal_delta)?;
            let management_fee = tenth_bips(sub(state.value_outstanding, principal)?, management_fee_rate)?;
            construct_loan_state(state.value_outstanding, principal, management_fee)?
        }
    };

    let up = Rounding::Upward;
    let principal_outstanding = clamp(
        round_to_asset(asset, new_theoretical_state.principal_outstanding, loan_scale, up)?,
        Number::ZERO,
        rounded_old_state.principal_outstanding,
    );
    let total_value_outstanding = clamp(
        round_to_asset(asset, add(principal_outstanding, new_theoretical_state.interest_outstanding()?)?, loan_scale, up)?,
        Number::ZERO,
        rounded_old_state.value_outstanding,
    );
    let management_fee_outstanding = clamp(
        round_to_asset(asset, new_theoretical_state.management_fee_due, loan_scale, A)?,
        Number::ZERO,
        rounded_old_state.management_fee_due,
    );
    let rounded_new_state = construct_loan_state(total_value_outstanding, principal_outstanding, management_fee_outstanding)?;
    new_properties.loan_state = rounded_new_state;

    // The loan may have been created with interest that is already paid
    // off: expect interest only if some is still outstanding.
    let expect_interest = !rounded_new_state.interest_outstanding()?.is_zero();
    if check_loan_guards(asset, principal_outstanding, expect_interest, payment_remaining, &new_properties)? != TxResult::Success {
        return Ok(None);
    }
    if !is_positive(&new_properties.periodic_payment)
        || !is_positive(&new_properties.loan_state.value_outstanding)
        || is_negative(&new_properties.loan_state.management_fee_due)
    {
        return Ok(None);
    }

    let deltas = state_difference(rounded_old_state, &rounded_new_state)?;
    // The value moves by the interest the lower principal saves.
    let value_change = deltas.interest.negated();
    if is_positive(&value_change) {
        return Ok(None);
    }
    Ok(Some((
        LoanPaymentParts {
            principal_paid: deltas.principal,
            interest_paid: overpayment.untracked_interest,
            value_change: add(value_change, overpayment.untracked_interest)?,
            fee_paid: add(overpayment.untracked_management_fee, overpayment.tracked.tracked_management_fee_delta)?,
        },
        new_properties,
    )))
}

/// `doOverpayment` (:901-1030): [`try_overpayment`] on the Loan's values,
/// committed when the principal really fell: the three balances and the
/// (unrounded) periodic payment of the re-amortized loan.
fn do_overpayment(
    rules: &LoanRules,
    asset: HeldAsset,
    loan_scale: i32,
    overpayment: &ExtendedPaymentComponents,
    loan: &mut Value,
    periodic_rate: Number,
    management_fee_rate: u32,
) -> M<Option<LoanPaymentParts>> {
    let principal_before = number_field(loan, "PrincipalOutstanding");
    let state = construct_loan_state(number_field(loan, "TotalValueOutstanding"), principal_before, number_field(loan, "ManagementFeeOutstanding"))?;
    let Some((parts, new_properties)) = try_overpayment(
        rules,
        asset,
        loan_scale,
        overpayment,
        &state,
        number_field(loan, "PeriodicPayment"),
        periodic_rate,
        uint(loan, "PaymentRemaining") as u32,
        management_fee_rate,
    )?
    else {
        return Ok(None);
    };
    let new_state = new_properties.loan_state;
    if principal_before <= new_state.principal_outstanding {
        return Ok(None);
    }
    set_number_field(loan, "TotalValueOutstanding", new_state.value_outstanding, Soe::Default);
    set_number_field(loan, "PrincipalOutstanding", new_state.principal_outstanding, Soe::Default);
    set_number_field(loan, "ManagementFeeOutstanding", new_state.management_fee_due, Soe::Default);
    set_number_field(loan, "PeriodicPayment", new_properties.periodic_payment, Soe::Required);
    Ok(Some(parts))
}

/// `computeLatePayment` (:1045-1120): the scheduled payment plus the late
/// penalties — sfLatePaymentFee, and the late interest
/// ([`loan_late_payment_interest`]) rounded at the loan scale and split.
/// tecTOO_SOON when the payment is not late; tecINSUFFICIENT_PAYMENT when
/// `amount` does not cover it.
fn compute_late_payment(
    rules: &LoanRules,
    now: u32,
    asset: HeldAsset,
    loan: &Value,
    periodic: &ExtendedPaymentComponents,
    amount: Number,
    management_fee_rate: u32,
) -> M<Result<ExtendedPaymentComponents, TxResult>> {
    if !is_payment_late(rules, now, loan) {
        return Ok(Err(TxResult::TooSoon));
    }
    let late_interest = loan_late_payment_interest(
        number_field(loan, "PrincipalOutstanding"),
        uint(loan, "LateInterestRate") as u32,
        now,
        uint(loan, "NextPaymentDueDate") as u32,
    )?;
    let (rounded_late_interest, rounded_late_management_fee) =
        round_and_split_interest(asset, late_interest, management_fee_rate, loan_scale_of(loan), A)?;
    let late = ExtendedPaymentComponents::new(
        periodic.tracked,
        add(add(periodic.untracked_management_fee, number_field(loan, "LatePaymentFee"))?, rounded_late_management_fee)?,
        add(periodic.untracked_interest, rounded_late_interest)?,
    )?;
    if amount < late.total_due {
        return Ok(Err(TxResult::InsufficientPayment));
    }
    Ok(Ok(late))
}

/// `computeFullPayment` (:1141-1245): paying the loan off early — every
/// tracked balance, plus sfClosePaymentFee and the full-payment interest
/// ([`compute_full_payment_interest`] of the THEORETICAL principal, rounded
/// DOWN at the loan scale and split) in place of the interest still
/// scheduled. tecKILLED with one payment (or none) remaining;
/// tecINSUFFICIENT_PAYMENT when `amount` does not cover it.
fn compute_full_payment(
    rules: &LoanRules,
    now: u32,
    asset: HeldAsset,
    loan: &Value,
    periodic_rate: Number,
    amount: Number,
    management_fee_rate: u32,
) -> M<Result<ExtendedPaymentComponents, TxResult>> {
    let payment_remaining = uint(loan, "PaymentRemaining") as u32;
    let loan_scale = loan_scale_of(loan);
    if payment_remaining <= 1 {
        return Ok(Err(TxResult::Killed));
    }
    let theoretical_principal =
        loan_principal_from_periodic_payment(rules, number_field(loan, "PeriodicPayment"), periodic_rate, payment_remaining)?;
    let full_payment_interest = compute_full_payment_interest(
        theoretical_principal,
        periodic_rate,
        now,
        uint(loan, "PaymentInterval") as u32,
        uint(loan, "PreviousPaymentDueDate") as u32,
        uint(loan, "StartDate") as u32,
        uint(loan, "CloseInterestRate") as u32,
    )?;
    let (rounded_full_interest, rounded_full_management_fee) =
        round_and_split_interest(asset, full_payment_interest, management_fee_rate, loan_scale, Rounding::Downward)?;

    let state = loan_state_of(loan)?;
    let close_payment_fee = round_to_asset(asset, number_field(loan, "ClosePaymentFee"), loan_scale, A)?;
    let full = ExtendedPaymentComponents::new(
        PaymentComponents {
            tracked_value_delta: add(add(state.principal_outstanding, state.interest_due)?, state.management_fee_due)?,
            tracked_principal_delta: state.principal_outstanding,
            tracked_management_fee_delta: state.management_fee_due,
            special_case: PaymentSpecialCase::Final,
        },
        // The close fee and the fee on the full-payment interest, less the
        // tracked fee paid above.
        sub(add(close_payment_fee, rounded_full_management_fee)?, state.management_fee_due)?,
        // What the loan earns now against what it was scheduled to earn.
        sub(rounded_full_interest, state.interest_due)?,
    )?;
    if amount < full.total_due {
        return Ok(Err(TxResult::InsufficientPayment));
    }
    Ok(Ok(full))
}

/// `loanRatesFor(loan, brokerSle)` (:1588-1596): the broker's management
/// fee rate and the loan's periodic rate.
fn loan_rates_for(loan: &Value, broker: &Value) -> M<(u32, Number)> {
    let management_fee_rate = uint(broker, "ManagementFeeRate") as u32;
    let periodic_rate = loan_periodic_rate(uint(loan, "InterestRate") as u32, uint(loan, "PaymentInterval") as u32)?;
    Ok((management_fee_rate, periodic_rate))
}

/// `LoanPaymentType` (LendingHelpers.h:664).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoanPaymentType {
    Regular,
    Late,
    Full,
    Overpayment,
}

/// `makeRegularPayment` (:1661-1841): as many whole scheduled payments
/// (each `tracked + sfLoanServiceFee`) as `amount` covers, at most
/// kLoanMaximumPaymentsPerTransaction (100) and no more than remain; none
/// is tecINSUFFICIENT_PAYMENT. Then, for an Overpayment on a loan flagged
/// lsfLoanOverpayment with payments still remaining and the count below
/// the maximum, what is left of the amount — truncated at the loan scale
/// under fixCleanup3_1_3, capped at the value outstanding, rounded DOWN at
/// the loan scale under fixCleanup3_2_0 — is an overpayment, taken when
/// some of it reaches principal and [`try_overpayment`] accepts it.
fn make_regular_payment(
    rules: &LoanRules,
    asset: HeldAsset,
    loan: &mut Value,
    broker: &Value,
    amount: Number,
    payment_type: LoanPaymentType,
) -> M<Result<LoanPaymentParts, TxResult>> {
    let (management_fee_rate, periodic_rate) = loan_rates_for(loan, broker)?;
    let loan_scale = loan_scale_of(loan);
    let service_fee = number_field(loan, "LoanServiceFee");
    let next = |loan: &Value| -> M<ExtendedPaymentComponents> {
        ExtendedPaymentComponents::new(compute_payment_components_of(rules, asset, loan, periodic_rate, management_fee_rate)?, service_fee, Number::ZERO)
    };

    let mut periodic = next(loan)?;
    let mut total_parts = LoanPaymentParts::ZERO;
    let mut total_paid = Number::ZERO;
    let mut num_payments: u32 = 0;
    while amount >= add(total_paid, periodic.total_due)?
        && uint(loan, "PaymentRemaining") > 0
        && num_payments < LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION
    {
        total_paid = add(total_paid, periodic.total_due)?;
        total_parts.accumulate(&do_payment(&periodic, loan)?)?;
        num_payments += 1;
        if periodic.tracked.special_case == PaymentSpecialCase::Final {
            break;
        }
        periodic = next(loan)?;
    }
    if num_payments == 0 {
        return Ok(Err(TxResult::InsufficientPayment));
    }

    let rounded_amount = if rules.fix_cleanup_3_1_3 { round_to_asset(asset, amount, loan_scale, Rounding::TowardsZero)? } else { amount };
    let flags = loan.get("Flags").and_then(|f| f.as_u64()).unwrap_or(0);
    let overpayment_supported = payment_type == LoanPaymentType::Overpayment && flags & LSF_LOAN_OVERPAYMENT != 0;
    let overpayment_allowed =
        uint(loan, "PaymentRemaining") > 0 && total_paid < rounded_amount && num_payments < LOAN_MAXIMUM_PAYMENTS_PER_TRANSACTION;
    if overpayment_supported && overpayment_allowed {
        let overpayment_raw = nmin(sub(rounded_amount, total_paid)?, number_field(loan, "TotalValueOutstanding"));
        let fix320 = rules.fix_cleanup_3_2_0;
        let overpayment = if fix320 { round_to_asset(asset, overpayment_raw, loan_scale, Rounding::Downward)? } else { overpayment_raw };
        if !fix320 || is_positive(&overpayment) {
            let components = compute_overpayment_components(
                asset,
                loan_scale,
                overpayment,
                uint(loan, "OverpaymentInterestRate") as u32,
                uint(loan, "OverpaymentFee") as u32,
                management_fee_rate,
            )?;
            // Nothing to do when fees and interest eat the whole amount.
            if is_positive(&components.tracked.tracked_principal_delta) {
                if let Some(parts) = do_overpayment(rules, asset, loan_scale, &components, loan, periodic_rate, management_fee_rate)? {
                    total_parts.accumulate(&parts)?;
                }
            }
        }
    }
    Ok(Ok(total_parts))
}

/// `loanMakePayment` (:2252-2314): one LoanPay's effect on the Loan.
///
/// - a loan already paid off: tecKILLED; no NextPaymentDueDate: tecINTERNAL;
/// - a late payment not flagged as one: tecEXPIRED;
/// - Full: [`compute_full_payment`] then `doPayment`; Late: the scheduled
///   payment plus the penalties ([`compute_late_payment`]); Regular and
///   Overpayment: [`make_regular_payment`].
///
/// `now` is the parent ledger's close time. On a refusal the Loan may hold
/// partial changes; the caller discards it.
pub fn loan_make_payment(
    rules: &LoanRules,
    now: u32,
    asset: HeldAsset,
    loan: &mut Value,
    broker: &Value,
    amount: &Amount,
    payment_type: LoanPaymentType,
) -> M<Result<LoanPaymentParts, TxResult>> {
    if uint(loan, "PaymentRemaining") == 0 || number_field(loan, "PrincipalOutstanding").is_zero() {
        return Ok(Err(TxResult::Killed));
    }
    if uint(loan, "NextPaymentDueDate") == 0 {
        return Ok(Err(TxResult::TecInternal));
    }
    if payment_type != LoanPaymentType::Late && is_payment_late(rules, now, loan) {
        return Ok(Err(TxResult::Expired));
    }
    let amount = amount.number();
    match payment_type {
        LoanPaymentType::Full => {
            let (management_fee_rate, periodic_rate) = loan_rates_for(loan, broker)?;
            match compute_full_payment(rules, now, asset, loan, periodic_rate, amount, management_fee_rate)? {
                Ok(full) => Ok(Ok(do_payment(&full, loan)?)),
                Err(refused) => Ok(Err(refused)),
            }
        }
        LoanPaymentType::Late => {
            let (management_fee_rate, periodic_rate) = loan_rates_for(loan, broker)?;
            let periodic = ExtendedPaymentComponents::new(
                compute_payment_components_of(rules, asset, loan, periodic_rate, management_fee_rate)?,
                number_field(loan, "LoanServiceFee"),
                Number::ZERO,
            )?;
            match compute_late_payment(rules, now, asset, loan, &periodic, amount, management_fee_rate)? {
                Ok(late) => Ok(Ok(do_payment(&late, loan)?)),
                Err(refused) => Ok(Err(refused)),
            }
        }
        LoanPaymentType::Regular | LoanPaymentType::Overpayment => make_regular_payment(rules, asset, loan, broker, amount, payment_type),
    }
}

#[cfg(test)]
mod tests;
