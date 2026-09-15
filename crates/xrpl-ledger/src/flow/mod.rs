//! Track 2 — a structural port of rippled 3.3.0's path engine
//! (`src/libxrpl/tx/paths/*`, `include/xrpl/tx/paths/detail/*`,
//! `src/libxrpl/ledger/PaymentSandbox.cpp`), file for file, behind a flag.
//!
//! The engine in `tx::offer` / `tx::payment` MODELS rippled's flow — a
//! hand-derived walk that has needed a calibration per specimen (findings
//! 1–287). This module replaces the model with the algorithm: each file here
//! mirrors one rippled file, cites its line ranges, and takes its arithmetic
//! from `tx::number` (track 1's exact `Number`). Nothing in `tx::*` changes
//! while the port lands; dispatch selects an engine per transaction
//! (`XRPL_FLOW_ENGINE=port`), and the shadow soak compares the two.
//!
//! Slices (2026-09-14, James: "slices please"):
//!   1. `amounts`, `quality_function`, `payment_sandbox`, `steps` — the Step
//!      trait, the amount types, the deferred-credits sandbox.
//!   2. `book_step` + `offer_stream` (BookTip, permRmOffer vs became-unfunded).
//!   3. `amm_liquidity` + `amm_offer` + `amm_context`.
//!   4. `strand_flow` + `pay_steps` (flow(), ActiveStrands, limitOut, the
//!      judge, toStrands) and the Payment dispatch; then OfferCreate's
//!      crossing.
pub mod amounts;
pub mod book_step;
pub mod offer_stream;
pub mod payment_sandbox;
pub mod quality_function;
pub mod st_amount;
pub mod steps;
pub mod view;
