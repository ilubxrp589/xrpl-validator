//! TicketCreate — reserve a batch of sequence numbers as Ticket objects.
//!
//! rippled reference: `CreateTicket.cpp`. A TicketCreate consumes its own
//! sequence number, then reserves the NEXT `TicketCount` numbers as Ticket
//! ledger objects; the account's Sequence lands one past the last ticket.
//! Each ticket is a separate owned object: it goes into the owner directory
//! and counts against both `OwnerCount` and `TicketCount`.
//!
//! Mutation shape (mainnet-verified against #105091578 / #105091579 /
//! #105667108): N × Ticket Created + AccountRoot Modified + the owner-dir
//! last page Modified, spilling Created pages via the shared `dir_insert`
//! mechanics when a page fills.

use crate::ledger::directory::owner_dir_insert;
use crate::ledger::keylet;
use crate::ledger::sandbox::Sandbox;
use crate::ledger::transactor::{Transactor, TxFields, TxResult};

fn ticket_count(tx: &TxFields) -> Option<u64> {
    tx.fields.get("TicketCount").and_then(|v| v.as_u64())
}

pub struct TicketCreateTransactor;

impl Transactor for TicketCreateTransactor {
    fn preflight(&self, tx: &TxFields) -> TxResult {
        if tx.tx_type != "TicketCreate" {
            return TxResult::Malformed;
        }
        if tx.fee == 0 {
            return TxResult::BadFee;
        }
        // temINVALID_COUNT: 1..=250 tickets per transaction.
        match ticket_count(tx) {
            Some(n) if (1..=250).contains(&n) => TxResult::Success,
            _ => TxResult::Malformed,
        }
    }

    fn preclaim(&self, tx: &TxFields, sandbox: &Sandbox) -> TxResult {
        let k = keylet::account_root_key(&tx.account);
        let Some(data) = sandbox.read(&k) else {
            return TxResult::NoAccount;
        };
        let Ok(acct) = serde_json::from_slice::<serde_json::Value>(&data) else {
            return TxResult::Malformed;
        };
        // An account may hold at most 250 outstanding tickets (tecDIR_FULL).
        let have = acct["TicketCount"].as_u64().unwrap_or(0);
        let count = ticket_count(tx).unwrap_or(0);
        if have + count > 250 {
            return TxResult::DirFull;
        }
        // Each ticket is an owned object and costs an incremental owner reserve.
        // rippled CreateTicket::doApply: preFeeBalance_ <
        // accountReserve(ownerCount + ticketCount) → tecINSUFFICIENT_RESERVE.
        // preclaim runs before apply_common deducts the fee, so the sandbox
        // balance here IS preFeeBalance_ (#105762093 B03E3974, #105779059).
        let balance = acct["Balance"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
        let reserve = crate::ledger::fees::account_reserve(sandbox, oc + count);
        if balance < reserve {
            return TxResult::InsufficientReserve;
        }
        TxResult::Success
    }

    fn do_apply(&self, tx: &TxFields, sandbox: &mut Sandbox) -> TxResult {
        let count = ticket_count(tx).unwrap_or(0) as u32;
        let acct_key = keylet::account_root_key(&tx.account);
        let Some(data) = sandbox.read(&acct_key) else {
            return TxResult::NoAccount;
        };
        let Ok(mut acct) = serde_json::from_slice::<serde_json::Value>(&data) else {
            return TxResult::Malformed;
        };

        // apply_common already advanced Sequence past the tx itself. Tickets
        // claim the NEXT `count` numbers; for a ticket-funded TicketCreate the
        // account sequence was untouched, so the batch starts right at it.
        let first = if tx.uses_ticket() {
            acct["Sequence"].as_u64().unwrap_or(0) as u32
        } else {
            tx.sequence.saturating_add(1)
        };

        for n in first..first.saturating_add(count) {
            let tk = keylet::ticket_key(&tx.account, n);
            let ticket = serde_json::json!({
                "LedgerEntryType": "Ticket",
                "Account": hex::encode(tx.account),
                "TicketSequence": n,
                "OwnerNode": 0,
            });
            sandbox.write(tk, serde_json::to_vec(&ticket).unwrap_or_default());
            owner_dir_insert(sandbox, &tx.account, &tk);
        }

        acct["Sequence"] = serde_json::json!(first.saturating_add(count));
        let tc = acct["TicketCount"].as_u64().unwrap_or(0);
        acct["TicketCount"] = serde_json::json!(tc + count as u64);
        let oc = acct["OwnerCount"].as_u64().unwrap_or(0);
        acct["OwnerCount"] = serde_json::json!(oc + count as u64);
        sandbox.write(acct_key, serde_json::to_vec(&acct).unwrap_or_default());

        TxResult::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::header::LedgerHeader;
    use crate::ledger::state::LedgerState;
    use xrpl_core::types::Hash256;

    fn state_with_account(id: &[u8; 20], seq: u32) -> LedgerState {
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
            "Balance": "1000000000",
            "Sequence": seq,
            "OwnerCount": 0,
            "Flags": 0,
        });
        state.state_map.insert(
            keylet::account_root_key(id),
            serde_json::to_vec(&acct).unwrap(),
        );
        state
    }

    fn tx(account: [u8; 20], seq: u32, count: u64) -> TxFields {
        TxFields {
            account,
            tx_type: "TicketCreate".into(),
            fee: 10,
            sequence: seq,
            ticket_seq: None,
            last_ledger_seq: None,
            fields: serde_json::json!({ "TicketCount": count }),
        }
    }

    #[test]
    fn creates_tickets_and_advances_sequence() {
        let id = [0x42u8; 20];
        let state = state_with_account(&id, 700);
        let t = tx(id, 700, 3);
        let tr = TicketCreateTransactor;
        assert_eq!(tr.preflight(&t), TxResult::Success);
        let mut sb = Sandbox::new(&state);
        crate::ledger::transactor::apply_common(&t, &mut sb);
        assert_eq!(tr.do_apply(&t, &mut sb), TxResult::Success);
        // Tickets 701, 702, 703 exist; sequence lands at 704.
        for n in 701..=703u32 {
            assert!(sb.read(&keylet::ticket_key(&id, n)).is_some(), "ticket {n}");
        }
        let acct: serde_json::Value =
            serde_json::from_slice(&sb.read(&keylet::account_root_key(&id)).unwrap()).unwrap();
        assert_eq!(acct["Sequence"], 704);
        assert_eq!(acct["TicketCount"], 3);
        assert_eq!(acct["OwnerCount"], 3);
    }

    #[test]
    fn preflight_rejects_zero_and_oversize_counts() {
        let id = [0x42u8; 20];
        let tr = TicketCreateTransactor;
        assert_eq!(tr.preflight(&tx(id, 1, 0)), TxResult::Malformed);
        assert_eq!(tr.preflight(&tx(id, 1, 251)), TxResult::Malformed);
        assert_eq!(tr.preflight(&tx(id, 1, 250)), TxResult::Success);
    }
}
