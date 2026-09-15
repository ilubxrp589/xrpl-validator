//! rippled `PaymentSandbox` (`include/xrpl/ledger/PaymentSandbox.h`,
//! `src/libxrpl/ledger/PaymentSandbox.cpp`): the view a strand runs in.
//!
//! Two things make it more than a scratch view:
//!
//! * **Deferred credits** (`detail::DeferredCredits`). Within a payment, an
//!   IOU credit an account receives does NOT fund its later spending in the
//!   same flow: `balanceHookIOU` answers every funds question with
//!   `min(current, original − debits, min original)` over the whole stack of
//!   nested sandboxes. The engine's finding 165 / `deferred_record` is the
//!   hand model of this table; here it is the table.
//! * **Owner counts** (`ownerCountHook`): the reserve an account's deletions
//!   free during the flow never funds its later offers — the hook answers
//!   the LARGER of the current count and every count remembered at an
//!   adjustment (finding 134's `ORIG_OWNER_COUNTS` is the hand model).
//!
//! rippled nests these views (`PaymentSandbox(PaymentSandbox*)`): a child
//! reads through its parent, keeps its own writes and its own credits table,
//! and `apply(parent)` folds both into the parent; a child dropped without
//! `apply` vanishes. Our `Sandbox` is one layer with snapshot/restore, so the
//! nesting is a STACK of layers over one `Sandbox`: pushing a layer takes a
//! snapshot and opens a fresh credits table, `apply` merges the table into
//! the parent's and keeps the writes, `discard` restores the snapshot. Flow
//! only ever nests sequentially (one live child at a time), which is exactly
//! what a stack expresses.
//!
//! `afView` — rippled's "after" view that `flow()` hands each strand beside
//! its sandbox (the ledger as it stands with EARLIER strands applied, i.e.
//! without this trial's tentative writes) — is `af_read`: the entry as of
//! the innermost layer's opening.
use std::collections::{BTreeMap, HashMap};

use crate::ledger::sandbox::{Sandbox, SandboxEntry};
use xrpl_core::types::Hash256;

use super::amounts::IouAmount;

/// rippled `OwnerCounts` (OwnerCounts.h): the three counters an account root
/// carries; `count()` is what the reserve is priced on.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct OwnerCounts {
    pub owner: u32,
    pub sponsored: u32,
    pub sponsoring: u32,
}

impl OwnerCounts {
    /// `owner − sponsored + sponsoring`, floored at zero.
    pub fn count(&self) -> u32 {
        let x = self.owner as i64 - self.sponsored as i64 + self.sponsoring as i64;
        if x < 0 { 0 } else { x as u32 }
    }
}

impl PartialOrd for OwnerCounts {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OwnerCounts {
    /// rippled compares `OwnerCounts` by `count()` (the `std::max` in
    /// `DeferredCredits::ownerCount` and `ownerCountHook`).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.count().cmp(&other.count())
    }
}

/// `DeferredCredits::AdjustmentIOU`: what `main` has been debited and
/// credited against `other` in this table, and its balance before the first
/// credit — all from `main`'s point of view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AdjustmentIou {
    pub debits: IouAmount,
    pub credits: IouAmount,
    pub orig_balance: IouAmount,
}

/// One table's entry for a (low, high, currency) line: each side's debits
/// and the LOW account's balance before the first credit (PaymentSandbox.h
/// `ValueIOU`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct ValueIou {
    low_acct_debits: IouAmount,
    high_acct_debits: IouAmount,
    low_acct_orig_balance: IouAmount,
}

/// `detail::DeferredCredits` — IOU lines and owner counts (the MPT tables
/// arrive with the MPT slice).
#[derive(Clone, Debug, Default)]
pub struct DeferredCredits {
    credits_iou: BTreeMap<([u8; 20], [u8; 20], [u8; 20]), ValueIou>,
    owner_counts: BTreeMap<[u8; 20], OwnerCounts>,
}

impl DeferredCredits {
    /// `makeKeyIOU`: the two accounts in address order.
    fn key(a1: &[u8; 20], a2: &[u8; 20], currency: &[u8; 20]) -> ([u8; 20], [u8; 20], [u8; 20]) {
        if a1 < a2 { (*a1, *a2, *currency) } else { (*a2, *a1, *currency) }
    }

    /// `creditIOU`: `sender` paid `amount` to `receiver` on `currency`;
    /// `pre_credit_sender_balance` is the sender's balance on that line
    /// (from the sender's side) before this credit. The first credit on a
    /// line remembers the LOW account's original balance (negated when the
    /// sender is the high account).
    pub fn credit_iou(
        &mut self,
        sender: &[u8; 20],
        receiver: &[u8; 20],
        currency: &[u8; 20],
        amount: IouAmount,
        pre_credit_sender_balance: IouAmount,
    ) {
        debug_assert!(sender != receiver, "DeferredCredits::creditIOU : sender is not receiver");
        debug_assert!(!amount.negative, "DeferredCredits::creditIOU : positive amount");
        let k = Self::key(sender, receiver, currency);
        let sender_low = sender < receiver;
        match self.credits_iou.get_mut(&k) {
            None => {
                let v = if sender_low {
                    ValueIou {
                        low_acct_debits: amount,
                        high_acct_debits: IouAmount::ZERO,
                        low_acct_orig_balance: pre_credit_sender_balance,
                    }
                } else {
                    ValueIou {
                        low_acct_debits: IouAmount::ZERO,
                        high_acct_debits: amount,
                        low_acct_orig_balance: pre_credit_sender_balance.negated(),
                    }
                };
                self.credits_iou.insert(k, v);
            }
            Some(v) => {
                if sender_low {
                    v.low_acct_debits = v.low_acct_debits.add(amount);
                } else {
                    v.high_acct_debits = v.high_acct_debits.add(amount);
                }
            }
        }
    }

    /// `adjustmentsIOU(main, other, currency)`: the line's record from
    /// `main`'s side, if any.
    pub fn adjustments_iou(&self, main: &[u8; 20], other: &[u8; 20], currency: &[u8; 20]) -> Option<AdjustmentIou> {
        let v = self.credits_iou.get(&Self::key(main, other, currency))?;
        Some(if main < other {
            AdjustmentIou { debits: v.low_acct_debits, credits: v.high_acct_debits, orig_balance: v.low_acct_orig_balance }
        } else {
            AdjustmentIou { debits: v.high_acct_debits, credits: v.low_acct_debits, orig_balance: v.low_acct_orig_balance.negated() }
        })
    }

    /// `ownerCount(id, cur, next)`: remember the larger of the two, and of
    /// anything already remembered.
    pub fn owner_count_adjust(&mut self, id: &[u8; 20], cur: OwnerCounts, next: OwnerCounts) {
        let v = std::cmp::max(cur, next);
        let e = self.owner_counts.entry(*id).or_insert(v);
        *e = std::cmp::max(*e, v);
    }

    pub fn owner_count(&self, id: &[u8; 20]) -> Option<OwnerCounts> {
        self.owner_counts.get(id).copied()
    }

    /// `apply(to)`: fold this table into `to` — debits add, original
    /// balances keep the parent's (first) record, owner counts take the max.
    pub fn apply(&self, to: &mut DeferredCredits) {
        for (k, v) in &self.credits_iou {
            match to.credits_iou.get_mut(k) {
                None => {
                    to.credits_iou.insert(*k, *v);
                }
                Some(t) => {
                    t.low_acct_debits = t.low_acct_debits.add(v.low_acct_debits);
                    t.high_acct_debits = t.high_acct_debits.add(v.high_acct_debits);
                }
            }
        }
        for (id, c) in &self.owner_counts {
            let e = to.owner_counts.entry(*id).or_insert(*c);
            *e = std::cmp::max(*e, *c);
        }
    }
}

/// One nesting level: the sandbox entries as they stood when the layer
/// opened (the `afView` and the rollback), and the layer's own table.
struct Layer {
    snapshot: HashMap<Hash256, SandboxEntry>,
    tab: DeferredCredits,
}

/// rippled `PaymentSandbox`, as a stack of layers over one `Sandbox`.
pub struct PaymentSandbox<'a, 'b> {
    sandbox: &'b mut Sandbox<'a>,
    /// The outermost table (rippled's root PaymentSandbox, `ps_ == nullptr`).
    root_tab: DeferredCredits,
    layers: Vec<Layer>,
}

impl<'a, 'b> PaymentSandbox<'a, 'b> {
    /// `PaymentSandbox(ApplyView const* base)`: the root view of a flow.
    pub fn new(sandbox: &'b mut Sandbox<'a>) -> Self {
        PaymentSandbox { sandbox, root_tab: DeferredCredits::default(), layers: Vec::new() }
    }

    /// `PaymentSandbox(PaymentSandbox* base)`: open a nested view.
    pub fn push(&mut self) {
        let snapshot = self.sandbox.snapshot();
        self.layers.push(Layer { snapshot, tab: DeferredCredits::default() });
    }

    /// `child.apply(parent)`: keep the writes, fold the table into the
    /// parent's. Returns false when no layer is open.
    pub fn apply_to_parent(&mut self) -> bool {
        let Some(layer) = self.layers.pop() else { return false };
        match self.layers.last_mut() {
            Some(parent) => layer.tab.apply(&mut parent.tab),
            None => layer.tab.apply(&mut self.root_tab),
        }
        true
    }

    /// Drop the innermost view without applying: writes roll back, the
    /// table is forgotten.
    pub fn discard(&mut self) -> bool {
        let Some(layer) = self.layers.pop() else { return false };
        self.sandbox.restore_snapshot(layer.snapshot);
        true
    }

    pub fn depth(&self) -> usize {
        self.layers.len()
    }

    /// The live view (reads and writes go to the sandbox itself).
    pub fn view(&mut self) -> &mut Sandbox<'a> {
        self.sandbox
    }

    pub fn read(&self, key: &Hash256) -> Option<Vec<u8>> {
        self.sandbox.read(key)
    }

    /// The live view, read-only (`view.rs` answers `accountHolds` and the
    /// directory walk over it).
    pub fn sandbox(&self) -> &Sandbox<'a> {
        self.sandbox
    }

    /// The view the flow opened with — the `cancelView` an OfferStream
    /// reads "original funds" from: the base ledger beneath every layer.
    pub fn base_view(&self) -> &Sandbox<'a> {
        self.sandbox
    }

    /// `afView` read: the entry as it stood when the innermost layer opened
    /// (or the live view outside any layer).
    pub fn af_read(&self, key: &Hash256) -> Option<Vec<u8>> {
        match self.layers.last() {
            Some(layer) => match layer.snapshot.get(key) {
                Some(SandboxEntry::Created(b)) | Some(SandboxEntry::Modified(b)) => Some(b.clone()),
                Some(SandboxEntry::Deleted) => None,
                None => self.sandbox.base().state_map.lookup(key).map(|b| b.to_vec()),
            },
            None => self.sandbox.read(key),
        }
    }

    fn tab_mut(&mut self) -> &mut DeferredCredits {
        match self.layers.last_mut() {
            Some(layer) => &mut layer.tab,
            None => &mut self.root_tab,
        }
    }

    /// Tables from the innermost layer outward, then the root — the order
    /// `for (curSB = this; curSB; curSB = curSB->ps_)` walks them.
    fn tabs(&self) -> impl Iterator<Item = &DeferredCredits> {
        self.layers.iter().rev().map(|l| &l.tab).chain(std::iter::once(&self.root_tab))
    }

    /// `balanceHookIOU(account, issuer, amount)`: `amount` is the account's
    /// current balance on the line with `issuer` (from the account's side);
    /// the answer is what it may SPEND — `min(amount, lastOrig − Σdebits,
    /// min orig)` across the stack; a negative XRP-issuer result clears.
    pub fn balance_hook_iou(&self, account: &[u8; 20], issuer: &[u8; 20], currency: &[u8; 20], amount: IouAmount) -> IouAmount {
        let mut delta = IouAmount::ZERO;
        let mut last_bal = amount;
        let mut min_bal = amount;
        for tab in self.tabs() {
            if let Some(adj) = tab.adjustments_iou(account, issuer, currency) {
                delta = delta.add(adj.debits);
                last_bal = adj.orig_balance;
                if last_bal < min_bal {
                    min_bal = last_bal;
                }
            }
        }
        let candidates = [amount, last_bal.sub(delta), min_bal];
        let adjusted = candidates.iter().copied().min().unwrap_or(amount);
        if issuer == &[0u8; 20] && adjusted < IouAmount::ZERO {
            return IouAmount::ZERO;
        }
        adjusted
    }

    /// `creditHookIOU(from, to, amount, preCreditBalance)`.
    pub fn credit_hook_iou(&mut self, from: &[u8; 20], to: &[u8; 20], currency: &[u8; 20], amount: IouAmount, pre_credit_balance: IouAmount) {
        self.tab_mut().credit_iou(from, to, currency, amount, pre_credit_balance);
    }

    /// `adjustOwnerCountHook(account, cur, next)`.
    pub fn adjust_owner_count_hook(&mut self, account: &[u8; 20], cur: OwnerCounts, next: OwnerCounts) {
        self.tab_mut().owner_count_adjust(account, cur, next);
    }

    /// `ownerCountHook(account, count)`: the largest count on record.
    pub fn owner_count_hook(&self, account: &[u8; 20], count: OwnerCounts) -> OwnerCounts {
        let mut result = count;
        for tab in self.tabs() {
            if let Some(c) = tab.owner_count(account) {
                result = std::cmp::max(result, c);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{LedgerHeader, LedgerState};
    use crate::tx::number::{Number, Rounding};

    fn iou(v: i64) -> IouAmount {
        IouAmount::from_number(Number::new(v, 0, Rounding::ToNearest).unwrap())
    }
    const A: [u8; 20] = [1; 20];
    const B: [u8; 20] = [2; 20];
    const USD: [u8; 20] = [9; 20];

    fn header() -> LedgerHeader {
        LedgerHeader {
            sequence: 1,
            parent_hash: Hash256([0; 32]),
            total_coins: 0,
            transaction_hash: Hash256([0; 32]),
            account_hash: Hash256([0; 32]),
            parent_close_time: 0,
            close_time: 0,
            close_time_resolution: 10,
            close_flags: 0,
        }
    }

    #[test]
    fn credits_never_fund_spending_in_the_same_flow() {
        let state = LedgerState::new_unverified(header());
        let mut sb = Sandbox::new(&state);
        let mut ps = PaymentSandbox::new(&mut sb);
        // A holds 100 USD (issuer B); A pays 30 to B: the table remembers
        // A's original 100 and its 30 debit.
        ps.credit_hook_iou(&A, &B, &USD, iou(30), iou(100));
        // A's line now reads 70; the hook says 70 (100 − 30).
        assert_eq!(ps.balance_hook_iou(&A, &B, &USD, iou(70)), iou(70));
        // B credits A back 50 (line reads 120): still only 70 may be spent.
        assert_eq!(ps.balance_hook_iou(&A, &B, &USD, iou(120)), iou(70));
        // A nested trial debits another 20 and is applied: 50.
        ps.push();
        ps.credit_hook_iou(&A, &B, &USD, iou(20), iou(120));
        assert_eq!(ps.balance_hook_iou(&A, &B, &USD, iou(100)), iou(50));
        assert!(ps.apply_to_parent());
        assert_eq!(ps.balance_hook_iou(&A, &B, &USD, iou(100)), iou(50));
        // A discarded trial leaves no trace.
        ps.push();
        ps.credit_hook_iou(&A, &B, &USD, iou(40), iou(100));
        assert_eq!(ps.balance_hook_iou(&A, &B, &USD, iou(60)), iou(10));
        assert!(ps.discard());
        assert_eq!(ps.balance_hook_iou(&A, &B, &USD, iou(100)), iou(50));
    }

    #[test]
    fn owner_counts_only_grow_within_a_flow() {
        let state = LedgerState::new_unverified(header());
        let mut sb = Sandbox::new(&state);
        let mut ps = PaymentSandbox::new(&mut sb);
        let c = |n: u32| OwnerCounts { owner: n, ..Default::default() };
        assert_eq!(ps.owner_count_hook(&A, c(5)), c(5));
        ps.adjust_owner_count_hook(&A, c(5), c(4)); // a deletion
        assert_eq!(ps.owner_count_hook(&A, c(4)), c(5));
        ps.push();
        ps.adjust_owner_count_hook(&A, c(4), c(7));
        assert_eq!(ps.owner_count_hook(&A, c(7)), c(7));
        ps.discard();
        assert_eq!(ps.owner_count_hook(&A, c(4)), c(5));
    }
}
