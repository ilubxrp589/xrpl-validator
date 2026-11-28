//! The MPT transfer fee `accountSend` and `accountSendMulti` charge between
//! two holders (directSendNoLimitMPT, directSendNoLimitMultiMPT): 3.4.1's
//! `multiply(amount, rate)`, to nearest (TokenHelpers.cpp:1345-1347,
//! :1458-1460), and under fixCleanup3_5_0 `addTransferFee(amount, rate)`,
//! exact and rounded up (3.5.0-rc1 TokenHelpers.cpp:1363-1373, :1485-1500,
//! :555-561). A cost past int64 throws in both: tefEXCEPTION.
use super::*;

/// ALICE holds 40 of a 0.001% issuance (TransferFee 1), PSEUDO and BOB hold
/// nothing; fixCleanup3_5_0 on when `fix`.
fn fee_state(fix: bool, alice: u64) -> (LedgerState, [u8; 24]) {
    let mut rules = ON.to_vec();
    if fix {
        rules.push(am::FIX_CLEANUP_3_5_0);
    }
    let mut st = state_with(&rules);
    let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({"OutstandingAmount": alice.to_string(), "TransferFee": 1}));
    token(&mut st, &id, ALICE, 0, alice);
    token(&mut st, &id, PSEUDO, 0, 0);
    token(&mut st, &id, BOB, 0, 0);
    (st, id)
}

fn balance(sb: &Sandbox, id: &[u8; 24], who: &[u8; 20]) -> u64 {
    let ikey = keylet::mpt_issuance_key(id);
    get(sb, &keylet::mptoken_key(&ikey, who)).map(|t| mpt::dec_field(&t, "MPTAmount")).unwrap_or(0)
}

fn outstanding(sb: &Sandbox, id: &[u8; 24]) -> u64 {
    mpt::dec_field(&get(sb, &keylet::mpt_issuance_key(id)).unwrap(), "OutstandingAmount")
}

/// 10 units at 1.00001 cost 10.0001: 10 to nearest before the fix, 11
/// rounded up under it — the extra unit redeemed to the issuer.
#[test]
fn account_send_rounds_the_mpt_fee_up_under_fixcleanup350() {
    let (st, id) = fee_state(false, 40);
    let mut sb = Sandbox::new(&st);
    assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &units(&id, 10), false), TxResult::Success);
    assert_eq!((balance(&sb, &id, &ALICE), balance(&sb, &id, &PSEUDO), outstanding(&sb, &id)), (30, 10, 40));

    let (st, id) = fee_state(true, 40);
    let mut sb = Sandbox::new(&st);
    assert_eq!(account_send(&mut sb, &ALICE, &PSEUDO, &units(&id, 10), false), TxResult::Success);
    assert_eq!((balance(&sb, &id, &ALICE), balance(&sb, &id, &PSEUDO), outstanding(&sb, &id)), (29, 10, 39));
}

/// 7e18 at a 50% fee costs 1.05e19, past int64: both rules throw
/// (tefEXCEPTION), where the engine answered tecINTERNAL.
#[test]
fn account_send_with_a_cost_past_int64_throws() {
    for fix in [false, true] {
        let mut rules = ON.to_vec();
        if fix {
            rules.push(am::FIX_CLEANUP_3_5_0);
        }
        let mut st = state_with(&rules);
        let id = issuance(&mut st, mpt::LSF_MPT_CAN_TRANSFER, json!({"OutstandingAmount": "7000000000000000000", "TransferFee": 50_000}));
        token(&mut st, &id, ALICE, 0, 7_000_000_000_000_000_000);
        token(&mut st, &id, PSEUDO, 0, 0);
        let sent = account_send(&mut Sandbox::new(&st), &ALICE, &PSEUDO, &units(&id, 7_000_000_000_000_000_000), false);
        assert_eq!(sent, TxResult::TefException, "fix {fix}");
    }
}

/// The multi-receiver twin: 10 to each of PSEUDO and BOB at 1.00001 costs
/// 10 + 10 before the fix and 11 + 11 under it, settled with the issuer in
/// one debit.
#[test]
fn account_send_multi_rounds_the_mpt_fee_up_under_fixcleanup350() {
    use crate::tx::number::{Number, Rounding};
    let near = Rounding::ToNearest;
    let pay = [(PSEUDO, Number::new(10, 0, near).unwrap()), (BOB, Number::new(10, 0, near).unwrap())];
    for (fix, alice_after) in [(false, 20), (true, 18)] {
        let (st, id) = fee_state(fix, 40);
        let mut sb = Sandbox::new(&st);
        assert_eq!(account_send_multi(&mut sb, &ALICE, &HeldAsset::Mpt(id), &pay, false, near), Ok(TxResult::Success));
        let got = (balance(&sb, &id, &ALICE), balance(&sb, &id, &PSEUDO), balance(&sb, &id, &BOB), outstanding(&sb, &id));
        assert_eq!(got, (alice_after, 10, 10, 20 + alice_after), "fix {fix}");
    }
}
