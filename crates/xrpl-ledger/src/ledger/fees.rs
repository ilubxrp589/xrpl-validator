//! Fee/reserve schedule — read the network's owner-reserve parameters from the
//! `FeeSettings` singleton and compute `accountReserve(ownerCount)`.
//!
//! rippled reference: `Fees::accountReserve()` = `reserve_base + reserve_inc *
//! ownerCount`. Both the base and the per-owned-object increment are voted
//! network parameters carried in the `FeeSettings` ledger object; the harness
//! replays each ledger under its own captured values (see the payment reserve
//! guard, #105035381 D21350B6, which pinned the modern 1-XRP base).
//!
//! This is the single source used by every transactor that enforces an owner
//! reserve (Payment funding guard, TicketCreate, TrustSet, …) so the schedule
//! never drifts between call sites.

use super::keylet;
use super::sandbox::Sandbox;

/// Base reserve (drops) from `FeeSettings`, defaulting to the modern 1 XRP.
pub fn reserve_base(sandbox: &Sandbox) -> u64 {
    let fee_key = keylet::fee_settings_key();
    if let Some(data) = sandbox.read(&fee_key) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(r) = v.get("ReserveBase").and_then(|r| r.as_u64()) {
                return r;
            }
            // Newer format uses ReserveBaseDrops.
            if let Some(r) = v
                .get("ReserveBaseDrops")
                .and_then(|r| r.as_str())
                .and_then(|s| s.parse::<u64>().ok())
            {
                return r;
            }
        }
    }
    // Mainnet base reserve since the 2024 vote (was 10 XRP).
    1_000_000
}

/// Per-owned-object reserve increment (drops) from `FeeSettings`, defaulting to
/// the modern 0.2 XRP.
pub fn reserve_inc(sandbox: &Sandbox) -> u64 {
    let fee_key = keylet::fee_settings_key();
    if let Some(data) = sandbox.read(&fee_key) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            if let Some(r) = v.get("ReserveIncrement").and_then(|r| r.as_u64()) {
                return r;
            }
            if let Some(r) = v
                .get("ReserveIncrementDrops")
                .and_then(|r| r.as_str())
                .and_then(|s| s.parse::<u64>().ok())
            {
                return r;
            }
        }
    }
    // Mainnet owner increment since the 2024 vote (was 2 XRP).
    200_000
}

/// The XRP (drops) an account must hold to own `owner_count` objects:
/// `reserve_base + reserve_inc * owner_count`. Mirrors rippled
/// `Fees::accountReserve(std::size_t ownerCount)`.
pub fn account_reserve(sandbox: &Sandbox, owner_count: u64) -> u64 {
    reserve_base(sandbox).saturating_add(reserve_inc(sandbox).saturating_mul(owner_count))
}
