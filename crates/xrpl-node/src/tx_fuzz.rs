//! Differential transaction fuzzer — libxrpl against our engine, on real
//! pre-states, with unsigned mutants of real transactions.
//!
//! Driven from `differential_probe --fuzz K`: for every transaction of the
//! fixture ledger, before the real one is applied, K mutants
//! (`fuzz_mutants`) are applied twice on the SAME pre-state — once by
//! libxrpl through the shim under `tapDRY_RUN` (0x1000: signature checks
//! skipped for an unsigned transaction, everything else as on the network,
//! Transactor.cpp:719), once by our engine — and the outcomes compared:
//! TER, then the mutation set (key, kind), then bytes. libxrpl runs FIRST
//! through a provider that reads our native state and falls back to the
//! parent ledger over RPC; every fallback read is hydrated into the native
//! state before our leg runs, so both legs see the same objects and a
//! disagreement is never a hydration gap. Each disagreement is written as a
//! `probe_bundle` file (pre = every object libxrpl read, expect = libxrpl's
//! mutations, result = libxrpl's TER) — a ready-made fixture for the drill.
use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use xrpl_core::types::Hash256;
use xrpl_ledger::ledger::sandbox::SandboxEntry;
use xrpl_ledger::ledger::state::LedgerState;
use xrpl_ledger::shamap::hash::{sha512_half_prefixed, HASH_PREFIX_TRANSACTION_ID};
use xrpl_ffi::{MutationKind, SleProvider};

use crate::ffi_engine::{fetch_mainnet_amendments_at, NetParams, RpcProvider, RpcReadOutcome};
use crate::fuzz_mutants::{mutants, Rng};
use crate::native_apply::{build_txfields, canon_for_encode, hexify_addresses, native_apply_one};

/// rippled's `tapDRY_RUN` (ApplyView.h): the `simulate` path.
pub const TAP_DRY_RUN: u32 = 0x1000;

#[derive(Default, Debug, Clone)]
pub struct FuzzTally {
    pub attempted: u64,
    pub agree: u64,
    pub ter_mm: u64,
    pub mut_mm: u64,
    pub byte_mm: u64,
    pub noop: u64,
    pub ffi_null: u64,
    pub skipped: u64,
    pub written: Vec<String>,
}

impl FuzzTally {
    pub fn add(&mut self, o: &FuzzTally) {
        self.attempted += o.attempted;
        self.agree += o.agree;
        self.ter_mm += o.ter_mm;
        self.mut_mm += o.mut_mm;
        self.byte_mm += o.byte_mm;
        self.noop += o.noop;
        self.ffi_null += o.ffi_null;
        self.skipped += o.skipped;
        self.written.extend(o.written.iter().cloned());
    }
}

pub struct FuzzCtx {
    pub seq: u32,
    pub pct: u32,
    pub parent_hash: [u8; 32],
    pub total_drops: u64,
    pub net: NetParams,
    pub amendments: Vec<[u8; 32]>,
    pub rpc: RpcProvider,
    pub out_dir: std::path::PathBuf,
    pub rng: Rng,
    pub seed: u64,
    pub k: usize,
}

impl FuzzCtx {
    pub fn new(rpc_url: &str, seq: u32, hdr: &Value, parent_hash: [u8; 32], out_dir: &str, seed: u64, k: usize) -> Self {
        let dflt = NetParams::default();
        let net = NetParams {
            network_id: hdr["network_id"].as_u64().unwrap_or(dflt.network_id as u64) as u32,
            base_fee_drops: hdr["base_fee_drops"].as_u64().unwrap_or(dflt.base_fee_drops),
            reserve_drops: hdr["reserve_drops"].as_u64().unwrap_or(dflt.reserve_drops),
            increment_drops: hdr["increment_drops"].as_u64().unwrap_or(dflt.increment_drops),
        };
        let amendments = fetch_mainnet_amendments_at(rpc_url, &(seq - 1).to_string());
        std::fs::create_dir_all(out_dir).ok();
        Self {
            seq,
            pct: hdr["parent_close_time"].as_u64().unwrap_or(0) as u32,
            parent_hash,
            total_drops: hdr["total_drops"].as_u64().unwrap_or(100_000_000_000_000_000),
            net,
            amendments,
            rpc: RpcProvider::new(rpc_url.to_string(), seq - 1),
            out_dir: std::path::PathBuf::from(out_dir),
            rng: Rng(seed ^ 0x9E37_79B9_7F4A_7C15),
            seed,
            k,
        }
    }

    /// Fuzz one real transaction (`base`, the fixture's `tx_json`, API
    /// form) at position `idx` on the current native `state`. `deleted` is
    /// every key an earlier transaction of this ledger removed — the parent
    /// ledger still has them and the provider must not resurrect them.
    pub fn fuzz_tx(&mut self, state: &mut LedgerState, base: &Value, base_hash: &str, idx: usize, deleted: &HashSet<[u8; 32]>) -> FuzzTally {
        let mut tally = FuzzTally::default();
        let muts = mutants(base, self.pct, &mut self.rng, self.k);
        for (n, (label, mutant)) in muts.into_iter().enumerate() {
            // Encode the mutant (unsigned) and name it by its own id.
            let mut enc = mutant.clone();
            canon_for_encode(&mut enc);
            let Ok(bytes) = xrpl_core::codec::encode::encode_transaction_json(&enc, false) else {
                tally.skipped += 1;
                continue;
            };
            let hash_hex = hex::encode_upper(sha512_half_prefixed(&HASH_PREFIX_TRANSACTION_ID, &bytes).0);
            // The engine reads the transaction id from the JSON's `hash`
            // (stamp_account_txn_id): give the mutant its own id before our
            // leg runs, as the ledger feed would.
            let mut mutant = mutant;
            mutant["hash"] = Value::String(hash_hex.clone());
            let Some(txf) = build_txfields(&mutant) else {
                tally.skipped += 1;
                continue;
            };
            tally.attempted += 1;

            // libxrpl leg on a recording provider over the native state.
            self.rpc.prefetch_offer_books_for_tx(&bytes);
            self.rpc.prefetch_nft_pages_for_tx(&bytes);
            let ledger = xrpl_ffi::LedgerInfo {
                seq: self.seq,
                parent_close_time: self.pct,
                total_drops: self.total_drops,
                parent_hash: self.parent_hash,
                base_fee_drops: self.net.base_fee_drops,
                reserve_drops: self.net.reserve_drops,
                increment_drops: self.net.increment_drops,
            };
            let (outcome, reads, fallback) = {
                let prov = RecProvider::new(state, &self.rpc, deleted);
                let o = xrpl_ffi::apply_with_mutations(&bytes, &self.amendments, &ledger, &prov, TAP_DRY_RUN, self.net.network_id);
                (o, prov.reads.into_inner(), prov.fallback.into_inner())
            };
            let Some(outcome) = outcome else {
                tally.ffi_null += 1;
                continue;
            };
            // Hydrate what libxrpl fetched from the parent ledger, so our leg
            // reads the same objects.
            for (k, b) in &fallback {
                if let Ok(mut jv) = xrpl_core::codec::decode::decode_transaction_binary(b) {
                    hexify_addresses(&mut jv);
                    if let Ok(js) = serde_json::to_vec(&jv) {
                        let _ = state.state_map.insert(Hash256(*k), js);
                    }
                }
            }

            // Our leg — with the sandbox read log armed, so the bundle's
            // `pre` can carry every base object our walk consulted.
            xrpl_ledger::ledger::sandbox::read_log_begin();
            let (our_ter, mut mods) = native_apply_one(state, &txf);
            let native_reads = xrpl_ledger::ledger::sandbox::read_log_take();
            xrpl_ledger::ledger::threading::stamp_threading(
                &mut mods,
                &|k| state.state_map.lookup(k).map(|b| b.to_vec()),
                &hash_hex,
                self.seq,
            );

            // Compare.
            let ffi_map: HashMap<[u8; 32], (u8, Vec<u8>)> = outcome
                .mutations
                .iter()
                .map(|m| {
                    let kind = match m.kind {
                        MutationKind::Created => 0,
                        MutationKind::Modified => 1,
                        MutationKind::Deleted => 2,
                    };
                    (m.key, (kind, if kind == 2 { Vec::new() } else { m.data.clone() }))
                })
                .collect();
            let our_map: HashMap<[u8; 32], (u8, Vec<u8>)> = mods
                .iter()
                .map(|(k, ent)| {
                    let (kind, bytes) = match ent {
                        SandboxEntry::Created(b) => (0u8, encode_obj(b)),
                        SandboxEntry::Modified(b) => (1u8, encode_obj(b)),
                        SandboxEntry::Deleted => (2u8, Vec::new()),
                    };
                    (k.0, (kind, bytes))
                })
                .collect();
            // A Modified entry whose content equals the object's pre-image
            // once PreviousTxnID/PreviousTxnLgrSeq are set aside is a
            // threading-only write: rippled files it as a ModifiedNode when a
            // transactor `update()`s a node it did not change, and the live
            // shadow reports the same asymmetry as `noop_extra`/`noop_missing`
            // rather than a divergence. Set-differences made only of those
            // are classed NOOP so the real MUT cases stand out.
            // The pre-image: what libxrpl read, else the object as our
            // native state holds it (a key only our leg wrote was never read
            // by libxrpl, so its pre-image lives only in the state).
            let pre_of = |k: &[u8; 32]| -> Option<Vec<u8>> {
                reads
                    .iter()
                    .find(|(rk, _)| rk == k)
                    .map(|(_, b)| b.clone())
                    .or_else(|| state.state_map.lookup(&Hash256(*k)).map(|js| encode_obj(js)).filter(|b| !b.is_empty()))
            };
            let threading_only = |k: &[u8; 32], kind: u8, b: &[u8]| -> bool {
                kind == 1 && pre_of(k).is_some_and(|pre| strip_threading(&pre) == strip_threading(b))
            };
            let class = if our_ter != outcome.ter_name {
                "TER"
            } else if ffi_map.len() != our_map.len()
                || ffi_map.iter().any(|(k, (kind, _))| our_map.get(k).map(|(ok, _)| ok) != Some(kind))
            {
                let all_noop = ffi_map
                    .iter()
                    .filter(|(k, _)| !our_map.contains_key(*k))
                    .all(|(k, (kind, b))| threading_only(k, *kind, b))
                    && our_map
                        .iter()
                        .filter(|(k, _)| !ffi_map.contains_key(*k))
                        .all(|(k, (kind, b))| threading_only(k, *kind, b))
                    && ffi_map.iter().all(|(k, (kind, _))| our_map.get(k).is_none_or(|(ok, _)| ok == kind));
                if all_noop { "NOOP" } else { "MUT" }
            } else if ffi_map.iter().any(|(k, (_, b))| our_map.get(k).map(|(_, ob)| ob) != Some(b)) {
                "BYTE"
            } else {
                tally.agree += 1;
                continue;
            };
            match class {
                "TER" => tally.ter_mm += 1,
                "MUT" => tally.mut_mm += 1,
                "NOOP" => tally.noop += 1,
                _ => tally.byte_mm += 1,
            }
            let letype = |b: &[u8]| -> String {
                xrpl_core::codec::decode::decode_transaction_binary(b)
                    .ok()
                    .and_then(|v| v.get("LedgerEntryType").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "?".to_string())
            };
            let mut detail = String::new();
            if class != "TER" {
                let only_ffi: Vec<String> = ffi_map
                    .iter()
                    .filter(|(k, _)| !our_map.contains_key(*k))
                    .map(|(k, (kind, b))| format!("{}:{}:{}", &hex::encode_upper(k)[..8], kind, letype(b)))
                    .collect();
                let only_ours: Vec<String> = our_map
                    .iter()
                    .filter(|(k, _)| !ffi_map.contains_key(*k))
                    .map(|(k, (kind, b))| format!("{}:{}:{}", &hex::encode_upper(k)[..8], kind, letype(b)))
                    .collect();
                let kind_diff: Vec<String> = ffi_map
                    .iter()
                    .filter_map(|(k, (kind, _))| our_map.get(k).filter(|(ok, _)| ok != kind).map(|(ok, _)| format!("{}:{}vs{}", &hex::encode_upper(k)[..8], kind, ok)))
                    .collect();
                let byte_diff: Vec<String> = ffi_map
                    .iter()
                    .filter_map(|(k, (kind, b))| our_map.get(k).filter(|(ok, ob)| ok == kind && ob != b).map(|_| format!("{}:{}", &hex::encode_upper(k)[..8], letype(b))))
                    .collect();
                detail = format!(" only-libxrpl={only_ffi:?} only-ours={only_ours:?} kind={kind_diff:?} bytes={byte_diff:?}");
            }
            eprintln!(
                "  FUZZ-{class} #{idx} {}[{n}] {label}: ours {our_ter} libxrpl {} (ours {} muts, libxrpl {}){detail}",
                &base_hash[..12.min(base_hash.len())],
                outcome.ter_name,
                our_map.len(),
                ffi_map.len()
            );
            // Bundle for the drill.
            let tx = mutant.clone();
            let mut pre: serde_json::Map<String, Value> = reads
                .iter()
                .map(|(k, b)| (hex::encode_upper(k), Value::String(hex::encode_upper(b))))
                .collect();
            // A preflight rejection reads nothing, but the native replay must
            // still find the parties to reach the same decision: seed the
            // bundle with the sender's and destination's roots as they stand.
            for f in ["Account", "Destination", "Owner", "Issuer"] {
                let Some(id) = mutant.get(f).and_then(|v| v.as_str()).and_then(crate::native_apply::decode_address) else { continue };
                let k = xrpl_ledger::ledger::keylet::account_root_key(&id);
                if let Some(js) = state.state_map.lookup(&k) {
                    let b = encode_obj(js);
                    if !b.is_empty() {
                        pre.entry(hex::encode_upper(k.0)).or_insert(Value::String(hex::encode_upper(&b)));
                    }
                }
            }
            // Every base object OUR leg read or enumerated, as it stood
            // before the mutant: the bundle then replays our walk, not a
            // narrower one confined to what libxrpl happened to read.
            for k in &native_reads {
                let kh = hex::encode_upper(k.0);
                if pre.contains_key(&kh) {
                    continue;
                }
                if let Some(js) = state.state_map.lookup(k) {
                    let b = encode_obj(js);
                    if !b.is_empty() {
                        pre.insert(kh, Value::String(hex::encode_upper(&b)));
                    }
                }
            }
            let mut expect: serde_json::Map<String, Value> = ffi_map
                .iter()
                .map(|(k, (_, b))| (hex::encode_upper(k), Value::String(hex::encode_upper(b))))
                .collect();
            // An object only our leg wrote becomes an UNTOUCHED pin (expect ==
            // pre): the bundle probe then reports it as written when the
            // replay repeats the divergence, so the drill sees the object by
            // name instead of a key prefix.
            for (k, _) in our_map.iter().filter(|(k, _)| !ffi_map.contains_key(*k)) {
                let pre_bytes = pre_of(k).or_else(|| state.state_map.lookup(&Hash256(*k)).map(|js| encode_obj(js)));
                if let Some(b) = pre_bytes.filter(|b| !b.is_empty()) {
                    let kh = hex::encode_upper(k);
                    pre.entry(kh.clone()).or_insert(Value::String(hex::encode_upper(&b)));
                    expect.entry(kh).or_insert(Value::String(hex::encode_upper(&b)));
                }
            }
            // Our leg's bytes for every key the legs disagree on, so the
            // drill can diff them against the pre-image without re-running
            // the fuzz state (a set difference need not reproduce from the
            // bundle's pre alone — our route may have touched objects
            // libxrpl never read).
            let ours_hex: serde_json::Map<String, Value> = our_map
                .iter()
                .filter(|(k, (kind, b))| ffi_map.get(*k).map(|(fk, fb)| (fk, fb)) != Some((kind, b)))
                .map(|(k, (kind, b))| (hex::encode_upper(k), json!({"kind": kind, "hex": hex::encode_upper(b)})))
                .collect();
            let bundle = json!({
                "seq": self.seq,
                "parent_hash": hex::encode_upper(self.parent_hash),
                "parent_close_time": self.pct,
                "total_coins": self.total_drops,
                "pre": pre,
                "tx": tx,
                "result": outcome.ter_name,
                "expect": expect,
                "fuzz": {"base_hash": base_hash, "index": idx, "mutant": n, "label": label, "seed": self.seed,
                          "class": class, "our_ter": our_ter, "libxrpl_ter": outcome.ter_name,
                          "libxrpl_fatal": outcome.last_fatal, "ours": ours_hex}
            });
            let safe = label.replace([':', '+', '-'], "_");
            let path = self.out_dir.join(format!("fuzz_{}_{idx:03}_{n}_{safe}.json", self.seq));
            if serde_json::to_writer(std::fs::File::create(&path).expect("fuzz out"), &bundle).is_ok() {
                tally.written.push(path.display().to_string());
            }
        }
        tally
    }
}

/// The object's fields as JSON with the threading stamps removed, for the
/// no-op test; unparsable bytes compare as themselves (hex).
fn strip_threading(sle: &[u8]) -> String {
    match xrpl_core::codec::decode::decode_transaction_binary(sle) {
        Ok(mut v) => {
            if let Some(o) = v.as_object_mut() {
                o.remove("PreviousTxnID");
                o.remove("PreviousTxnLgrSeq");
            }
            v.to_string()
        }
        Err(_) => hex::encode(sle),
    }
}

fn encode_obj(json_bytes: &[u8]) -> Vec<u8> {
    let Ok(mut jv) = serde_json::from_slice::<Value>(json_bytes) else { return Vec::new() };
    canon_for_encode(&mut jv);
    xrpl_core::codec::encode::encode_transaction_json(&jv, false).unwrap_or_default()
}

/// libxrpl's view for one mutant: our native state (objects as this ledger
/// has them so far, re-encoded to SLE bytes), then the parent ledger over
/// RPC for anything else, minus what this ledger already deleted. Records
/// every object it served (`reads`) and the ones it fetched over RPC
/// (`fallback`).
struct RecProvider<'a> {
    state: &'a LedgerState,
    rpc: &'a RpcProvider,
    deleted: &'a HashSet<[u8; 32]>,
    state_keys: Vec<[u8; 32]>,
    arena: parking_lot::Mutex<Vec<Vec<u8>>>,
    reads: parking_lot::Mutex<Vec<([u8; 32], Vec<u8>)>>,
    fallback: parking_lot::Mutex<Vec<([u8; 32], Vec<u8>)>>,
}

impl<'a> RecProvider<'a> {
    fn new(state: &'a LedgerState, rpc: &'a RpcProvider, deleted: &'a HashSet<[u8; 32]>) -> Self {
        let mut state_keys: Vec<[u8; 32]> = state.state_map.keys_with_prefix(&[]).into_iter().map(|h| h.0).collect();
        state_keys.sort_unstable();
        Self {
            state,
            rpc,
            deleted,
            state_keys,
            arena: parking_lot::Mutex::new(Vec::new()),
            reads: parking_lot::Mutex::new(Vec::new()),
            fallback: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

impl<'a> SleProvider for RecProvider<'a> {
    fn read(&self, key: &[u8; 32]) -> Option<&[u8]> {
        if self.deleted.contains(key) {
            return None;
        }
        if let Some(js) = self.state.state_map.lookup(&Hash256(*key)) {
            let bytes = encode_obj(js);
            if bytes.is_empty() {
                return None;
            }
            self.reads.lock().push((*key, bytes.clone()));
            let mut arena = self.arena.lock();
            arena.push(bytes);
            let last = arena.last().unwrap();
            // The arena only grows for the provider's lifetime: the slice
            // stays valid as long as `self` (the LayeredProvider pattern).
            return Some(unsafe { std::slice::from_raw_parts(last.as_ptr(), last.len()) });
        }
        match self.rpc.read_with_outcome(key) {
            RpcReadOutcome::Hit(b) => {
                self.reads.lock().push((*key, b.to_vec()));
                self.fallback.lock().push((*key, b.to_vec()));
                Some(b)
            }
            _ => None,
        }
    }

    fn succ(&self, key: &[u8; 32], last: Option<&[u8; 32]>) -> Option<[u8; 32]> {
        let mut from_rpc = self.rpc.succ(key, last);
        while let Some(k) = from_rpc {
            if self.deleted.contains(&k) {
                from_rpc = self.rpc.succ(&k, last);
            } else {
                break;
            }
        }
        let pos = self.state_keys.partition_point(|k| k <= key);
        let from_state = self.state_keys.get(pos).copied().filter(|k| last.is_none_or(|l| k < l));
        match (from_rpc, from_state) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}
