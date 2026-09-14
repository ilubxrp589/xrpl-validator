//! Rust FFI bindings to libxrpl (rippled's C++ tx engine) via our C shim.
//!
//! The shim is built as a static library in `ffi/build/libxrpl_shim.a` and
//! linked here via `build.rs`. This crate provides:
//!
//! - Raw `extern "C"` declarations matching `ffi/xrpl_shim.h`
//! - Safe Rust wrappers (`parse_tx`, `preflight`, `apply_with_mutations`)
//! - `ApplyResult` owning handle with mutation iterator
//!
//! The validator uses this to delegate transaction application to rippled's
//! production tx engine rather than reimplementing it in Rust.

use std::ffi::{c_char, c_void};

// =========================================================================
// Raw FFI — matches ffi/xrpl_shim.h exactly
// =========================================================================

#[repr(C)]
pub struct XrplEngine {
    _private: [u8; 0],
}

#[repr(C)]
pub struct XrplApplyResult {
    _private: [u8; 0],
}

/// SLE lookup callback: called by C++ when it needs to read a ledger object.
/// Rust must keep the returned buffer alive for the duration of the apply() call.
pub type SleLookupFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    key: *const u8, // [u8; 32]
    out_data: *mut *const u8,
    out_len: *mut usize,
) -> bool;

/// Succ callback: called by C++ for directory traversal (finding the next key
/// strictly greater than `key`). If `last_key` is non-null, only return keys
/// strictly < last_key — rippled's ReadView::succ is the OPEN interval
/// (key, last). Writes successor into `out_succ_key[32]`, returns true if found.
pub type SuccFn = unsafe extern "C" fn(
    user_data: *mut c_void,
    key: *const u8,       // [u8; 32]
    last_key: *const u8,  // [u8; 32] or null
    out_succ_key: *mut u8, // [u8; 32]
) -> bool;

#[link(name = "xrpl_shim", kind = "static")]
extern "C" {
    pub fn xrpl_shim_version() -> *const c_char;
    pub fn xrpl_rippled_version() -> *const c_char;

    pub fn xrpl_engine_create() -> *mut XrplEngine;
    pub fn xrpl_engine_destroy(engine: *mut XrplEngine);

    pub fn xrpl_tx_parse(
        tx_bytes: *const u8,
        tx_len: usize,
        out_hash: *mut u8,           // [u8; 32]
        out_type_name: *mut c_char,
        type_name_buf_len: usize,
    ) -> bool;

    pub fn xrpl_preflight(
        tx_bytes: *const u8,
        tx_len: usize,
        amendments_bytes: *const u8,
        amendments_len: usize,
        apply_flags: u32,
        network_id: u32,
        out_ter_name: *mut c_char,
        ter_name_buf_len: usize,
    ) -> i32;

    pub fn xrpl_apply_with_mutations(
        tx_bytes: *const u8,
        tx_len: usize,
        amendments_bytes: *const u8,
        amendments_len: usize,
        ledger_seq: u32,
        parent_close_time: u32,
        total_drops: u64,
        parent_hash: *const u8, // [u8; 32]
        base_fee_drops: u64,
        reserve_drops: u64,
        increment_drops: u64,
        apply_flags: u32,
        network_id: u32,
        lookup_fn: SleLookupFn,
        lookup_user_data: *mut c_void,
        succ_fn: SuccFn,
        succ_user_data: *mut c_void,
    ) -> *mut XrplApplyResult;

    pub fn xrpl_result_ter(result: *const XrplApplyResult) -> i32;
    pub fn xrpl_result_applied(result: *const XrplApplyResult) -> bool;
    pub fn xrpl_result_ter_name(result: *const XrplApplyResult) -> *const c_char;
    pub fn xrpl_result_drops_destroyed(result: *const XrplApplyResult) -> i64;
    pub fn xrpl_result_last_fatal(result: *const XrplApplyResult) -> *const c_char;
    pub fn xrpl_result_mutation_count(result: *const XrplApplyResult) -> usize;
    pub fn xrpl_result_mutation_at(
        result: *const XrplApplyResult,
        index: usize,
        out_key: *mut u8, // [u8; 32]
        out_kind: *mut u8,
        out_data: *mut *const u8,
        out_data_len: *mut usize,
    ) -> bool;
    pub fn xrpl_result_destroy(result: *mut XrplApplyResult);

    pub fn xrpl_test_callback_read(
        keylet_type: u16,
        key: *const u8,         // [u8; 32]
        sle_bytes: *const u8,
        sle_len: usize,
    ) -> bool;
}

// =========================================================================
// Safe Rust wrappers
// =========================================================================

/// LedgerEntryType numeric values matching rippled's `LedgerFormats.h`.
/// Used by `test_callback_read` to exercise the type-check in
/// `CallbackReadView::read`.
pub mod ledger_entry_type {
    pub const ANY: u16 = 0x0000;
    pub const ACCOUNT_ROOT: u16 = 0x0061;
    pub const OFFER: u16 = 0x006F;
    pub const CHILD: u16 = 0x1CD2;
}

/// Test helper: invokes `CallbackReadView::read(Keylet{keylet_type, key})`
/// against an in-memory SLE consisting of `sle_bytes`. Returns true if read
/// returned a non-null SLE. Regression-tests the ltCHILD wildcard fix.
pub fn test_callback_read(keylet_type: u16, key: &[u8; 32], sle_bytes: &[u8]) -> bool {
    unsafe {
        xrpl_test_callback_read(
            keylet_type,
            key.as_ptr(),
            sle_bytes.as_ptr(),
            sle_bytes.len(),
        )
    }
}

/// libxrpl version we're linked against.
pub fn libxrpl_version() -> String {
    unsafe {
        let ptr = xrpl_rippled_version();
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    }
}

/// Parsed transaction info returned by `parse_tx`.
#[derive(Debug, Clone)]
pub struct ParsedTx {
    /// 32-byte transaction hash (matches rippled's canonical tx ID).
    pub hash: [u8; 32],
    /// Human-readable tx type, e.g. "Payment", "OfferCreate".
    pub tx_type: String,
}

/// Parse a raw XRPL transaction.
///
/// Returns the tx hash and type name. This calls libxrpl's `STTx` constructor
/// via `SerialIter`. Returns `None` if the tx is malformed.
pub fn parse_tx(tx_bytes: &[u8]) -> Option<ParsedTx> {
    let mut hash = [0u8; 32];
    let mut type_name_buf = [0u8; 64];
    let ok = unsafe {
        xrpl_tx_parse(
            tx_bytes.as_ptr(),
            tx_bytes.len(),
            hash.as_mut_ptr(),
            type_name_buf.as_mut_ptr() as *mut c_char,
            type_name_buf.len(),
        )
    };
    if !ok {
        return None;
    }
    // Null-terminated
    let nul = type_name_buf.iter().position(|&b| b == 0).unwrap_or(type_name_buf.len());
    let tx_type = String::from_utf8_lossy(&type_name_buf[..nul]).into_owned();
    Some(ParsedTx { hash, tx_type })
}

/// Result of `preflight()`.
#[derive(Debug, Clone)]
pub struct PreflightOutcome {
    /// TER code (0 = tesSUCCESS).
    pub ter: i32,
    /// Human-readable TER name, e.g. "tesSUCCESS", "temMALFORMED".
    pub ter_name: String,
}

impl PreflightOutcome {
    pub fn is_success(&self) -> bool {
        self.ter == 0
    }
}

/// Run libxrpl's preflight on a transaction (no ledger state needed).
///
/// Validates format + signature. The amendment list determines active Rules.
pub fn preflight(
    tx_bytes: &[u8],
    amendments: &[[u8; 32]],
    apply_flags: u32,
    network_id: u32,
) -> PreflightOutcome {
    let amendments_flat: Vec<u8> = amendments.iter().flatten().copied().collect();
    let mut name_buf = [0u8; 128];
    let ter = unsafe {
        xrpl_preflight(
            tx_bytes.as_ptr(),
            tx_bytes.len(),
            if amendments_flat.is_empty() { std::ptr::null() } else { amendments_flat.as_ptr() },
            amendments_flat.len(),
            apply_flags,
            network_id,
            name_buf.as_mut_ptr() as *mut c_char,
            name_buf.len(),
        )
    };
    let nul = name_buf.iter().position(|&b| b == 0).unwrap_or(name_buf.len());
    let ter_name = String::from_utf8_lossy(&name_buf[..nul]).into_owned();
    PreflightOutcome { ter, ter_name }
}

// =========================================================================
// Apply with mutations — the big one
// =========================================================================

/// How an SLE was changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Created,
    Modified,
    Deleted,
}

impl MutationKind {
    fn from_raw(v: u8) -> Option<Self> {
        match v {
            0 => Some(MutationKind::Created),
            1 => Some(MutationKind::Modified),
            2 => Some(MutationKind::Deleted),
            _ => None,
        }
    }
}

/// A single SLE state change from applying a tx.
#[derive(Debug, Clone)]
pub struct SleMutation {
    pub key: [u8; 32],
    pub kind: MutationKind,
    /// Serialized SLE bytes (empty for Deleted).
    pub data: Vec<u8>,
}

/// Ledger info for the apply() call.
#[derive(Debug, Clone)]
pub struct LedgerInfo {
    pub seq: u32,
    pub parent_close_time: u32,
    pub total_drops: u64,
    pub parent_hash: [u8; 32],
    pub base_fee_drops: u64,
    pub reserve_drops: u64,
    pub increment_drops: u64,
}

/// Result of `apply_with_mutations`.
#[derive(Debug)]
pub struct ApplyOutcome {
    pub ter: i32,
    pub ter_name: String,
    pub applied: bool,
    pub drops_destroyed: i64,
    pub mutations: Vec<SleMutation>,
    /// Captured fatal-log text from libxrpl (non-empty for tefEXCEPTION).
    pub last_fatal: String,
}

impl ApplyOutcome {
    pub fn is_success(&self) -> bool {
        self.ter == 0
    }
}

/// Trait for providing SLE state to libxrpl.
///
/// libxrpl will call `read(key)` when it needs to look up a ledger object.
/// Return `Some(bytes)` if found, `None` if not. The returned bytes must remain
/// valid for the duration of the apply() call.
pub trait SleProvider {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]>;
    /// Find the next key strictly greater than `key`. If `last` is Some,
    /// only return keys strictly < last (open interval (key, last), matching
    /// rippled's ReadView::succ). Used for directory traversal.
    fn succ(&self, _key: &[u8; 32], _last: Option<&[u8; 32]>) -> Option<[u8; 32]> {
        None // default: no traversal support
    }
}

/// Apply a transaction via libxrpl's full tx engine.
///
/// This runs preflight → preclaim → doApply. State lookups go through `provider`.
/// Returns the outcome + list of SLE mutations to commit.
pub fn apply_with_mutations<P: SleProvider>(
    tx_bytes: &[u8],
    amendments: &[[u8; 32]],
    ledger: &LedgerInfo,
    provider: &P,
    apply_flags: u32,
    network_id: u32,
) -> Option<ApplyOutcome> {
    unsafe extern "C" fn trampoline<P: SleProvider>(
        user_data: *mut c_void,
        key_ptr: *const u8,
        out_data: *mut *const u8,
        out_len: *mut usize,
    ) -> bool {
        let provider = unsafe { &*(user_data as *const P) };
        let mut key = [0u8; 32];
        unsafe { std::ptr::copy_nonoverlapping(key_ptr, key.as_mut_ptr(), 32) };
        match provider.read(&key) {
            Some(bytes) => {
                unsafe {
                    *out_data = bytes.as_ptr();
                    *out_len = bytes.len();
                }
                true
            }
            None => false,
        }
    }

    unsafe extern "C" fn succ_trampoline<P: SleProvider>(
        user_data: *mut c_void,
        key_ptr: *const u8,
        last_ptr: *const u8,
        out_succ: *mut u8,
    ) -> bool {
        let provider = unsafe { &*(user_data as *const P) };
        let mut key = [0u8; 32];
        unsafe { std::ptr::copy_nonoverlapping(key_ptr, key.as_mut_ptr(), 32) };
        let last = if last_ptr.is_null() {
            None
        } else {
            let mut l = [0u8; 32];
            unsafe { std::ptr::copy_nonoverlapping(last_ptr, l.as_mut_ptr(), 32) };
            Some(l)
        };
        match provider.succ(&key, last.as_ref()) {
            Some(found) => {
                unsafe { std::ptr::copy_nonoverlapping(found.as_ptr(), out_succ, 32) };
                true
            }
            None => false,
        }
    }

    let amendments_flat: Vec<u8> = amendments.iter().flatten().copied().collect();
    let amendments_ptr = if amendments_flat.is_empty() {
        std::ptr::null()
    } else {
        amendments_flat.as_ptr()
    };

    let result_ptr = unsafe {
        xrpl_apply_with_mutations(
            tx_bytes.as_ptr(),
            tx_bytes.len(),
            amendments_ptr,
            amendments_flat.len(),
            ledger.seq,
            ledger.parent_close_time,
            ledger.total_drops,
            ledger.parent_hash.as_ptr(),
            ledger.base_fee_drops,
            ledger.reserve_drops,
            ledger.increment_drops,
            apply_flags,
            network_id,
            trampoline::<P>,
            provider as *const P as *mut c_void,
            succ_trampoline::<P>,
            provider as *const P as *mut c_void,
        )
    };

    if result_ptr.is_null() {
        return None;
    }

    let ter = unsafe { xrpl_result_ter(result_ptr) };
    let applied = unsafe { xrpl_result_applied(result_ptr) };
    let drops_destroyed = unsafe { xrpl_result_drops_destroyed(result_ptr) };
    let ter_name = unsafe {
        let ptr = xrpl_result_ter_name(result_ptr);
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    };
    let last_fatal = unsafe {
        let ptr = xrpl_result_last_fatal(result_ptr);
        std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
    };

    let n = unsafe { xrpl_result_mutation_count(result_ptr) };
    let mut mutations = Vec::with_capacity(n);
    for i in 0..n {
        let mut key = [0u8; 32];
        let mut kind_raw: u8 = 0;
        let mut data_ptr: *const u8 = std::ptr::null();
        let mut data_len: usize = 0;
        let ok = unsafe {
            xrpl_result_mutation_at(
                result_ptr,
                i,
                key.as_mut_ptr(),
                &mut kind_raw,
                &mut data_ptr,
                &mut data_len,
            )
        };
        if ok {
            let kind = MutationKind::from_raw(kind_raw).unwrap_or(MutationKind::Modified);
            let data = if data_ptr.is_null() || data_len == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(data_ptr, data_len) }.to_vec()
            };
            mutations.push(SleMutation { key, kind, data });
        }
    }

    unsafe { xrpl_result_destroy(result_ptr) };

    Some(ApplyOutcome {
        ter,
        ter_name,
        applied,
        drops_destroyed,
        mutations,
        last_fatal,
    })
}

/// Simple in-memory SleProvider backed by a HashMap, mostly for tests.
pub struct MemoryProvider {
    map: std::collections::HashMap<[u8; 32], Vec<u8>>,
}

impl MemoryProvider {
    pub fn new() -> Self {
        Self { map: std::collections::HashMap::new() }
    }

    pub fn insert(&mut self, key: [u8; 32], data: Vec<u8>) {
        self.map.insert(key, data);
    }
}

impl Default for MemoryProvider {
    fn default() -> Self { Self::new() }
}

impl SleProvider for MemoryProvider {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        self.map.get(key).map(|v| v.as_slice())
    }
}

// ---- Track 1: arithmetic oracle (ffi/xrpl_arith.cpp) ------------------------

/// Mirror of the C `XrplAmt`: an STAmount as (mantissa, exponent, sign, native).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XrplAmt {
    pub mantissa: u64,
    pub exponent: i32,
    pub negative: u8,
    pub native: u8,
}

pub const ARITH_MULROUND: i32 = 0;
pub const ARITH_MULROUND_STRICT: i32 = 1;
pub const ARITH_DIVROUND: i32 = 2;
pub const ARITH_DIVROUND_STRICT: i32 = 3;
pub const ARITH_MULTIPLY: i32 = 4;
pub const ARITH_DIVIDE: i32 = 5;
pub const ARITH_ADD: i32 = 6;
pub const ARITH_SUB: i32 = 7;
pub const ARITH_CANONICALIZE: i32 = 8;
pub const NUM_ADD: i32 = 0;
pub const NUM_SUB: i32 = 1;
pub const NUM_MUL: i32 = 2;
pub const NUM_DIV: i32 = 3;
pub const NUM_ROOT2: i32 = 4;
/// Number rounding modes: 0 ToNearest, 1 TowardsZero, 2 Downward, 3 Upward.
pub const NUM_ROUND_NEAREST: i32 = 0;
pub const NUM_ROUND_TOWARDS_ZERO: i32 = 1;
pub const NUM_ROUND_DOWN: i32 = 2;
pub const NUM_ROUND_UP: i32 = 3;

#[link(name = "xrpl_shim", kind = "static")]
extern "C" {
    pub fn xrpl_arith_last_error() -> *const c_char;
    pub fn xrpl_arith_stamount_op(op: i32, a: *const XrplAmt, b: *const XrplAmt, round_up: u8, result_native: u8, out: *mut XrplAmt) -> i32;
    pub fn xrpl_arith_get_rate(offer_out: *const XrplAmt, offer_in: *const XrplAmt) -> u64;
    pub fn xrpl_arith_number_op(op: i32, m1: i64, e1: i32, m2: i64, e2: i32, rounding_mode: i32, out_m: *mut i64, out_e: *mut i32) -> i32;
}

/// Safe wrappers over the oracle. `Err(msg)` carries rippled's exception text.
pub mod arith {
    use super::*;

    pub fn iou(mantissa: u64, exponent: i32) -> XrplAmt {
        XrplAmt { mantissa, exponent, negative: 0, native: 0 }
    }
    pub fn xrp(drops: u64) -> XrplAmt {
        XrplAmt { mantissa: drops, exponent: 0, negative: 0, native: 1 }
    }
    fn last_error() -> String {
        unsafe { std::ffi::CStr::from_ptr(xrpl_arith_last_error()).to_string_lossy().into_owned() }
    }
    /// libxrpl's STAmount operation `op` on `a` and `b`; the result's asset is
    /// XRP when `result_native`, else an IOU of no issue.
    pub fn stamount_op(op: i32, a: XrplAmt, b: XrplAmt, round_up: bool, result_native: bool) -> Result<XrplAmt, String> {
        let mut out = XrplAmt { mantissa: 0, exponent: 0, negative: 0, native: 0 };
        let rc = unsafe { xrpl_arith_stamount_op(op, &a, &b, round_up as u8, result_native as u8, &mut out) };
        if rc == 0 { Ok(out) } else { Err(last_error()) }
    }
    /// `getRate(offerOut, offerIn)`: the 64-bit book quality, 0 on overflow.
    pub fn get_rate(offer_out: XrplAmt, offer_in: XrplAmt) -> u64 {
        unsafe { xrpl_arith_get_rate(&offer_out, &offer_in) }
    }
    /// `Number` operation `op` under `rounding_mode`; returns (mantissa, exponent).
    pub fn number_op(op: i32, a: (i64, i32), b: (i64, i32), rounding_mode: i32) -> Result<(i64, i32), String> {
        let (mut m, mut e) = (0i64, 0i32);
        let rc = unsafe { xrpl_arith_number_op(op, a.0, a.1, b.0, b.1, rounding_mode, &mut m, &mut e) };
        if rc == 0 { Ok((m, e)) } else { Err(last_error()) }
    }
}
