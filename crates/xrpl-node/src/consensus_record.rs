//! Shadow recorder for consensus rounds (independence task 5.2(a)).
//!
//! **Off unless `XRPL_CONSENSUS_RECORD=1`.** Observe only: it decides nothing, drops nothing and
//! never touches signing or the writer.
//! - **Hot path:** the peer loop's whole cost is one bounded-channel `try_send` per checked
//!   proposal or validation.
//! - **Writer thread:** does everything else.
//! - **Full channel:** the event is counted as `lost`, never waited on.
//!
//! It records the fixture 5.2's agreed-set tracker is built against
//! (docs/CONSENSUS-AUDIT-2026-10.md):
//! - **Proposals:** every signature-checked proposal from a trusted validator. Who sent it, its
//!   prevLedger, propose seq (a bow-out is -1), close time and position (the tx set hash), and
//!   when we first saw it.
//! - **Validations:** every signature-checked validation from a trusted validator. The ledger seq
//!   and hash; its `sfConsensusHash`, the root of the set it accepted (rippled sets it at
//!   RCLConsensus.cpp:822); its signing time and full flag; and when we first saw it.
//! - **Headers:** each validated header ws-sync fetches: seq, hash, parent hash, close times,
//!   resolution, flags and transaction_hash.
//!
//! **Trust** is by master key from the verified UNL. A message's signing key finds its validator
//! through the manifests that list carried. A signing key we can't map is noted, never trusted:
//! - its proposals are kept in full, up to 64 a line;
//! - its validations are counted only;
//! - each such validation key is listed once per file.
//!
//! ## File format
//!
//! The file is `/mnt/xrpl-data/consensus_rounds.jsonl`, or `XRPL_CONSENSUS_RECORD_PATH`. It is
//! JSON lines, read in order. There are two kinds.
//!
//! **The validator table:**
//! `{"k":"unl","ver":1,"t":<unix ms>,"keys":[["<master hex>","<signing hex>"|null],...]}`.
//! It is written at the top of every file, after every reopen, and whenever the list changes.
//! Validators are sorted by master key, and a validator's index is its position here.
//!
//! **A round line:**
//! `{"k":"r","ver":1, header fields, "t0", "tw", "h", "pr", "pu", "va", "uk", "n"}`.
//! There is one per validated header, or one per 8,192 waiting messages if headers stop.
//! - **Header fields:** `seq`, `hash`, `parent`, `ct` (close time), `pct` (parent close time),
//!   `res`, `flags`, `txh` (transaction_hash) and `th` (unix ms ws-sync handed it over). All are
//!   absent on a line without a header.
//! - **`t0`:** the unix ms of the line's first message. Each `dt` below is ms after it.
//! - **`tw`:** the unix ms the line was written.
//! - **`h`:** the line's 32-byte values, in uppercase hex. The rows refer to them by index.
//! - **`pr`:** trusted proposals: `[validator, prevLedger, seq, closeTime, position, dt]`.
//! - **`pu`:** proposals whose signing key we can't map:
//!   `["<signing hex>", prevLedger, seq, closeTime, position, dt]`, at most 64 a line.
//! - **`va`:** trusted validations:
//!   `[validator, ledgerSeq, ledgerHash, consensusHash, signingTime, dt, full]`.
//!   consensusHash is -1 when the validation carries none.
//! - **`uk`:** validation signing keys we can't map, each the first time a file sees it.
//! - **`n`:** counts since the last line:
//!   - `pdup` and `vdup`: copies of a message already held;
//!   - `pu` and `vu`: messages from keys we can't map;
//!   - `vbad`: validations that didn't parse;
//!   - `lost`: events the full channel turned away.
//!
//! Keys are always written in the order above.
//!
//! ## An example
//!
//! A file holding a two-validator list and one round. The test
//! `the_documented_example_is_what_the_encoder_writes` checks these exact bytes.
//!
//! ```text
//! {"k":"unl","ver":1,"t":1760000000000,"keys":[["ED4040404040404040404040404040404040404040404040404040404040404040","028080808080808080808080808080808080808080808080808080808080808080"],["ED4141414141414141414141414141414141414141414141414141414141414141","028181818181818181818181818181818181818181818181818181818181818181"]]}
//! {"k":"r","ver":1,"seq":107600001,"hash":"1111111111111111111111111111111111111111111111111111111111111111","parent":"1010101010101010101010101010101010101010101010101010101010101010","ct":813000001,"pct":813000000,"res":10,"flags":0,"txh":"5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A","th":1760000003600,"t0":1760000000000,"tw":1760000003601,"h":["1010101010101010101010101010101010101010101010101010101010101010","B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0","B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1","1111111111111111111111111111111111111111111111111111111111111111"],"pr":[[0,0,0,813000003,1,0],[1,0,0,813000003,1,40],[0,0,1,813000003,2,850],[1,0,-1,813000003,1,2400]],"pu":[["020101010101010101010101010101010101010101010101010101010101010101",0,0,813000003,1,60]],"va":[[0,107600001,3,2,813000004,3100,1]],"uk":["020202020202020202020202020202020202020202020202020202020202020202"],"n":{"pdup":3,"vdup":5,"pu":1,"vu":1,"vbad":0,"lost":0}}
//! ```
//!
//! Reading the round line:
//! - **`pr`:** on prevLedger `h[0]`, validator 0 proposed set `h[1]` (seq 0, at `t0`). 850 ms
//!   later it moved to `h[2]` (seq 1). Validator 1 proposed `h[1]`, then bowed out (seq -1).
//! - **`pu`:** a key that isn't on the list also proposed `h[1]`.
//! - **`va`:** validator 0 validated ledger 107600001 (`h[3]`), naming `h[2]` as the set it
//!   accepted.
//! - **`uk`:** a validator off the list validated too.
//! - **`n`:** 3 proposal copies and 5 validation copies were dropped.
//! - **The header:** the votes (813000003) round to 813000000 at resolution 10. That isn't later
//!   than the parent's close time, so the ledger closed at the parent's + 1 s (5.3's rule).
//!
//! ## Disk
//!
//! - **Writes:** one buffered write per line.
//! - **Size:** a line comes to 4,710 bytes when 35 validators send 3 proposals each, 35 trusted
//!   validations arrive and 120 come from validators off the list. The test
//!   `a_typical_ledger_line_is_about_5_kb` measures it.
//!   - Mainnet closed the 3,000 ledgers from 107,596,001 at 3.865 s each, so 22,357 a day. That
//!     makes about 105 MB a day.
//!   - At 5 proposals per validator it would be about 150 MB a day.
//!   - Each file also starts with the table (about 5 KB) and lists unknown validation keys once
//!     (about 8 KB).
//! - **Rotation:** at 200 MB the file becomes `.1`, and `.1` becomes `.2`. Two old files are
//!   kept, so the recorder holds 600 MB at most.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use serde_json::{json, Value};

use crate::consensus::sig_verify::{field_value_len, read_field_header};
use crate::unl_fetch::UnlEntry;
use crate::ws_sync::LedgerHeader;

/// File format version, on every line as `ver`.
pub const FORMAT_VERSION: u64 = 1;
/// Where the file goes unless `XRPL_CONSENSUS_RECORD_PATH` says otherwise.
pub const DEFAULT_PATH: &str = "/mnt/xrpl-data/consensus_rounds.jsonl";
/// The file rotates when the next line would take it past this many bytes.
pub const ROTATE_BYTES: u64 = 200_000_000;
/// Rotated files kept: `.1` (newer) and `.2`.
pub const KEEP: u32 = 2;

/// Events the channel holds before `try_send` turns them away: about a minute of traffic.
const CHANNEL_CAPACITY: usize = 16_384;
/// A line is written without a header once this many messages wait (headers stopped).
const MAX_BATCH_ROWS: usize = 8_192;
/// Proposals from keys we can't map, kept in full per line.
const MAX_UNKNOWN_PROPOSALS: usize = 64;
/// Unknown validation keys a file lists before it only counts them.
const MAX_NOTED_KEYS: usize = 4_096;
/// rippled `ConsensusProposal::kSeqLeave` (ConsensusProposal.h:50): a bow-out.
const SEQ_LEAVE: u32 = 0xFFFF_FFFF;
/// rippled `vfFullValidation`: the validator proposed in the round.
const VF_FULL_VALIDATION: u32 = 0x0000_0001;

/// One signature-checked proposal, as it arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// The key it was signed with: the validator's ephemeral key, from its manifest.
    pub signing_key: Vec<u8>,
    pub prev_ledger: [u8; 32],
    /// `0xFFFF_FFFF` (kSeqLeave) marks a bow-out.
    pub propose_seq: u32,
    pub close_time: u32,
    /// The proposed transaction set's hash.
    pub position: [u8; 32],
    /// Unix ms when we first saw it.
    pub seen_ms: u64,
}

/// What the recorder keeps from a signature-checked validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    pub signing_key: Vec<u8>,
    pub ledger_seq: u32,
    pub ledger_hash: [u8; 32],
    /// `sfConsensusHash`: the root of the transaction set the validator accepted.
    pub consensus_hash: Option<[u8; 32]>,
    pub signing_time: u32,
    /// `vfFullValidation`: the validator proposed in the round.
    pub full: bool,
    pub seen_ms: u64,
}

/// A validated ledger header, as ws-sync fetched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub seq: u32,
    pub hash: [u8; 32],
    pub parent_hash: [u8; 32],
    pub close_time: u32,
    pub parent_close_time: u32,
    pub close_resolution: u8,
    pub close_flags: u8,
    pub transaction_hash: [u8; 32],
    /// Unix ms when ws-sync handed it over.
    pub seen_ms: u64,
}

impl Header {
    pub fn from_ws(seq: u32, h: &LedgerHeader, seen_ms: u64) -> Self {
        Self {
            seq,
            hash: h.ledger_hash,
            parent_hash: h.parent_hash,
            close_time: h.close_time,
            parent_close_time: h.parent_close_time,
            close_resolution: h.close_resolution,
            close_flags: h.close_flags,
            transaction_hash: h.transaction_hash,
            seen_ms,
        }
    }
}

/// Pulls the recorded fields out of an STValidation blob. `None` when the blob doesn't parse or
/// lacks its ledger sequence, ledger hash, signing time or signing key.
pub fn parse_validation(blob: &[u8], seen_ms: u64) -> Option<Validation> {
    let mut flags = 0u32;
    let (mut ledger_seq, mut signing_time) = (None, None);
    let (mut ledger_hash, mut consensus_hash, mut signing_key) = (None, None, None);
    let mut i = 0;
    while i < blob.len() {
        let (type_code, field_code, header_len) = read_field_header(blob, i)?;
        let (len, prefix_len) = field_value_len(blob, i + header_len, type_code)?;
        let start = i + header_len + prefix_len;
        let end = start.checked_add(len)?;
        let value = blob.get(start..end)?;
        match (type_code, field_code) {
            (2, 2) => flags = u32::from_be_bytes(value.try_into().ok()?),
            (2, 6) => ledger_seq = Some(u32::from_be_bytes(value.try_into().ok()?)),
            (2, 9) => signing_time = Some(u32::from_be_bytes(value.try_into().ok()?)),
            (5, 1) => ledger_hash = Some(<[u8; 32]>::try_from(value).ok()?),
            (5, 23) => consensus_hash = Some(<[u8; 32]>::try_from(value).ok()?),
            (7, 3) => signing_key = Some(value.to_vec()),
            _ => {}
        }
        i = end;
    }
    Some(Validation {
        signing_key: signing_key?,
        ledger_seq: ledger_seq?,
        ledger_hash: ledger_hash?,
        consensus_hash,
        signing_time: signing_time?,
        full: flags & VF_FULL_VALIDATION != 0,
        seen_ms,
    })
}

/// The verified list as the recorder indexes it.
/// - Validators are sorted by master key, and a validator's index is its position.
/// - A message's signing key (from the manifest the list carried) finds its validator, and so
///   does its master key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustTable {
    /// (master key, signing key) per validator.
    pub keys: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    index: HashMap<Vec<u8>, u32>,
}

impl TrustTable {
    /// From the verified list. Entries whose keys aren't hex are left out.
    pub fn from_entries(entries: &[UnlEntry]) -> Self {
        let mut keys: Vec<(Vec<u8>, Option<Vec<u8>>)> = entries
            .iter()
            .filter_map(|e| {
                let master = hex::decode(&e.public_key).ok()?;
                let signing = e.signing_key.as_deref().and_then(|s| hex::decode(s).ok());
                Some((master, signing))
            })
            .collect();
        keys.sort();
        keys.dedup_by(|a, b| a.0 == b.0);
        Self::from_keys(keys)
    }

    fn from_keys(keys: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> Self {
        let mut index = HashMap::new();
        for (i, (master, signing)) in keys.iter().enumerate() {
            index.insert(master.clone(), i as u32);
            if let Some(signing) = signing {
                index.insert(signing.clone(), i as u32);
            }
        }
        Self { keys, index }
    }

    /// The validator a key belongs to, if the list has it.
    pub fn lookup(&self, key: &[u8]) -> Option<u32> {
        self.index.get(key).copied()
    }

    /// The table as a `"k":"unl"` line.
    pub fn line(&self, t_ms: u64) -> String {
        let keys: Vec<Value> = self
            .keys
            .iter()
            .map(|(master, signing)| {
                json!([hex::encode_upper(master), signing.as_ref().map(hex::encode_upper)])
            })
            .collect();
        ordered_object(&[
            ("k", "\"unl\"".into()),
            ("ver", FORMAT_VERSION.to_string()),
            ("t", t_ms.to_string()),
            ("keys", Value::Array(keys).to_string()),
        ])
    }

    /// Reads a `"k":"unl"` line back.
    pub fn parse_line(line: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if v["k"] != "unl" {
            return Err("not a unl line".into());
        }
        let mut keys = Vec::new();
        for k in v["keys"].as_array().ok_or("a unl line without keys")? {
            let master = hex::decode(k[0].as_str().ok_or("master key not a string")?)
                .map_err(|e| e.to_string())?;
            let signing = match k[1].as_str() {
                Some(s) => Some(hex::decode(s).map_err(|e| e.to_string())?),
                None => None,
            };
            keys.push((master, signing));
        }
        Ok(Self::from_keys(keys))
    }
}

/// Messages waiting for the next header.
#[derive(Debug, Default)]
pub struct Batch {
    pub proposals: Vec<Proposal>,
    pub validations: Vec<Validation>,
    /// Copies of a message already held, dropped.
    pub proposal_dups: u64,
    pub validation_dups: u64,
    /// Validations that didn't parse.
    pub validation_bad: u64,
}

impl Batch {
    fn rows(&self) -> usize {
        self.proposals.len() + self.validations.len()
    }
}

/// A line's 32-byte values, each written once and referred to by index.
#[derive(Default)]
struct HashTable {
    list: Vec<[u8; 32]>,
    index: HashMap<[u8; 32], u32>,
}

impl HashTable {
    fn ix(&mut self, h: &[u8; 32]) -> u32 {
        let next = self.list.len() as u32;
        let i = *self.index.entry(*h).or_insert(next);
        if i == next {
            self.list.push(*h);
        }
        i
    }
}

/// Builds one `"k":"r"` line: the batch, the header that closed it (if any) and the counters.
/// `noted` holds the unknown validation keys this file has listed already; keys listed now are
/// added to it.
pub fn encode_round(
    batch: &Batch,
    header: Option<&Header>,
    table: &TrustTable,
    noted: &mut HashSet<Vec<u8>>,
    lost: u64,
    written_ms: u64,
) -> String {
    let t0 = batch
        .proposals
        .iter()
        .map(|p| p.seen_ms)
        .chain(batch.validations.iter().map(|v| v.seen_ms))
        .min()
        .unwrap_or(written_ms);
    let dt = |ms: u64| ms.saturating_sub(t0);
    let seq = |s: u32| if s == SEQ_LEAVE { json!(-1) } else { json!(s) };
    let mut hashes = HashTable::default();

    let (mut trusted, mut unknown, mut unknown_proposals) = (Vec::new(), Vec::new(), 0u64);
    for p in &batch.proposals {
        match table.lookup(&p.signing_key) {
            Some(v) => trusted.push(json!([
                v,
                hashes.ix(&p.prev_ledger),
                seq(p.propose_seq),
                p.close_time,
                hashes.ix(&p.position),
                dt(p.seen_ms)
            ])),
            None => {
                unknown_proposals += 1;
                if unknown.len() < MAX_UNKNOWN_PROPOSALS {
                    unknown.push(json!([
                        hex::encode_upper(&p.signing_key),
                        hashes.ix(&p.prev_ledger),
                        seq(p.propose_seq),
                        p.close_time,
                        hashes.ix(&p.position),
                        dt(p.seen_ms)
                    ]));
                }
            }
        }
    }

    let (mut validations, mut new_keys, mut unknown_validations) = (Vec::new(), Vec::new(), 0u64);
    for v in &batch.validations {
        match table.lookup(&v.signing_key) {
            Some(i) => {
                let ledger = hashes.ix(&v.ledger_hash);
                let consensus = match &v.consensus_hash {
                    Some(c) => json!(hashes.ix(c)),
                    None => json!(-1),
                };
                validations.push(json!([
                    i,
                    v.ledger_seq,
                    ledger,
                    consensus,
                    v.signing_time,
                    dt(v.seen_ms),
                    u8::from(v.full)
                ]));
            }
            None => {
                unknown_validations += 1;
                if noted.len() < MAX_NOTED_KEYS && noted.insert(v.signing_key.clone()) {
                    new_keys.push(json!(hex::encode_upper(&v.signing_key)));
                }
            }
        }
    }

    let quoted_hex = |b: &[u8]| format!("\"{}\"", hex::encode_upper(b));
    let mut fields: Vec<(&str, String)> =
        vec![("k", "\"r\"".into()), ("ver", FORMAT_VERSION.to_string())];
    if let Some(h) = header {
        fields.extend([
            ("seq", h.seq.to_string()),
            ("hash", quoted_hex(&h.hash)),
            ("parent", quoted_hex(&h.parent_hash)),
            ("ct", h.close_time.to_string()),
            ("pct", h.parent_close_time.to_string()),
            ("res", h.close_resolution.to_string()),
            ("flags", h.close_flags.to_string()),
            ("txh", quoted_hex(&h.transaction_hash)),
            ("th", h.seen_ms.to_string()),
        ]);
    }
    let list: Vec<String> = hashes.list.iter().map(hex::encode_upper).collect();
    let counts = ordered_object(&[
        ("pdup", batch.proposal_dups.to_string()),
        ("vdup", batch.validation_dups.to_string()),
        ("pu", unknown_proposals.to_string()),
        ("vu", unknown_validations.to_string()),
        ("vbad", batch.validation_bad.to_string()),
        ("lost", lost.to_string()),
    ]);
    fields.extend([
        ("t0", t0.to_string()),
        ("tw", written_ms.to_string()),
        ("h", json!(list).to_string()),
        ("pr", Value::Array(trusted).to_string()),
        ("pu", Value::Array(unknown).to_string()),
        ("va", Value::Array(validations).to_string()),
        ("uk", Value::Array(new_keys).to_string()),
        ("n", counts),
    ]);
    ordered_object(&fields)
}

/// A JSON object with its keys in the order given. serde_json's own map may sort them, and the
/// documented order is easier to read and to grep.
fn ordered_object(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("{{{}}}", body.join(","))
}

/// A proposal read back from a line. `validator` is `None` for a signing key we couldn't map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadProposal {
    pub validator: Option<u32>,
    pub proposal: Proposal,
}

/// A line's `n` counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineCounts {
    pub proposal_dups: u64,
    pub validation_dups: u64,
    pub unknown_proposals: u64,
    pub unknown_validations: u64,
    pub validation_bad: u64,
    pub lost: u64,
}

/// A `"k":"r"` line read back, its indexes resolved through the file's table.
/// - Trusted proposals come first, then the unknown ones.
/// - A trusted row's key is the validator's signing key from the table (its master key when the
///   list had no manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRound {
    pub header: Option<Header>,
    pub t0: u64,
    pub written_ms: u64,
    pub proposals: Vec<ReadProposal>,
    pub validations: Vec<(u32, Validation)>,
    pub unknown_validation_keys: Vec<Vec<u8>>,
    pub counts: LineCounts,
}

fn hex32(v: &Value) -> Result<[u8; 32], String> {
    let bytes = hex::decode(v.as_str().ok_or("hash not a string")?).map_err(|e| e.to_string())?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| "hash not 32 bytes".to_string())
}

fn num(v: &Value, what: &str) -> Result<u64, String> {
    v.as_u64().ok_or_else(|| format!("{what}: not a number"))
}

fn small<T: TryFrom<u64>>(v: &Value, what: &str) -> Result<T, String> {
    T::try_from(num(v, what)?).map_err(|_| format!("{what}: out of range"))
}

fn read_seq(v: &Value) -> Result<u32, String> {
    match v.as_i64() {
        Some(-1) => Ok(SEQ_LEAVE),
        Some(s) => u32::try_from(s).map_err(|_| format!("propose seq {s} out of range")),
        None => Err("propose seq not a number".into()),
    }
}

fn array<'a>(v: &'a Value, name: &str) -> Result<&'a Vec<Value>, String> {
    v[name].as_array().ok_or_else(|| format!("no {name}"))
}

/// Reads a `"k":"r"` line back. `table` is the last `"k":"unl"` line before it in its file.
pub fn decode_round(line: &str, table: &TrustTable) -> Result<ReadRound, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if v["k"] != "r" {
        return Err("not a round line".into());
    }
    let hashes: Vec<[u8; 32]> = array(&v, "h")?.iter().map(hex32).collect::<Result<_, _>>()?;
    let hash_at = |x: &Value| -> Result<[u8; 32], String> {
        let i: usize = small(x, "hash index")?;
        hashes.get(i).copied().ok_or_else(|| format!("hash index {i} out of range"))
    };
    let key_of = |i: u32| -> Result<Vec<u8>, String> {
        let (master, signing) =
            table.keys.get(i as usize).ok_or_else(|| format!("validator {i} not in the table"))?;
        Ok(signing.clone().unwrap_or_else(|| master.clone()))
    };
    let t0 = num(&v["t0"], "t0")?;
    let header = match v.get("seq") {
        None => None,
        Some(seq) => Some(Header {
            seq: small(seq, "seq")?,
            hash: hex32(&v["hash"])?,
            parent_hash: hex32(&v["parent"])?,
            close_time: small(&v["ct"], "ct")?,
            parent_close_time: small(&v["pct"], "pct")?,
            close_resolution: small(&v["res"], "res")?,
            close_flags: small(&v["flags"], "flags")?,
            transaction_hash: hex32(&v["txh"])?,
            seen_ms: num(&v["th"], "th")?,
        }),
    };

    let mut proposals = Vec::new();
    for r in array(&v, "pr")? {
        let validator: u32 = small(&r[0], "validator")?;
        proposals.push(ReadProposal {
            validator: Some(validator),
            proposal: Proposal {
                signing_key: key_of(validator)?,
                prev_ledger: hash_at(&r[1])?,
                propose_seq: read_seq(&r[2])?,
                close_time: small(&r[3], "close time")?,
                position: hash_at(&r[4])?,
                seen_ms: t0 + num(&r[5], "dt")?,
            },
        });
    }
    for r in array(&v, "pu")? {
        proposals.push(ReadProposal {
            validator: None,
            proposal: Proposal {
                signing_key: hex::decode(r[0].as_str().ok_or("signing key not a string")?)
                    .map_err(|e| e.to_string())?,
                prev_ledger: hash_at(&r[1])?,
                propose_seq: read_seq(&r[2])?,
                close_time: small(&r[3], "close time")?,
                position: hash_at(&r[4])?,
                seen_ms: t0 + num(&r[5], "dt")?,
            },
        });
    }

    let mut validations = Vec::new();
    for r in array(&v, "va")? {
        let validator: u32 = small(&r[0], "validator")?;
        validations.push((
            validator,
            Validation {
                signing_key: key_of(validator)?,
                ledger_seq: small(&r[1], "ledger seq")?,
                ledger_hash: hash_at(&r[2])?,
                consensus_hash: if r[3].as_i64() == Some(-1) { None } else { Some(hash_at(&r[3])?) },
                signing_time: small(&r[4], "signing time")?,
                full: num(&r[6], "full")? == 1,
                seen_ms: t0 + num(&r[5], "dt")?,
            },
        ));
    }

    let unknown_validation_keys = array(&v, "uk")?
        .iter()
        .map(|k| -> Result<Vec<u8>, String> {
            hex::decode(k.as_str().ok_or("unknown key not a string")?).map_err(|e| e.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let n = &v["n"];
    let counts = LineCounts {
        proposal_dups: num(&n["pdup"], "pdup")?,
        validation_dups: num(&n["vdup"], "vdup")?,
        unknown_proposals: num(&n["pu"], "pu")?,
        unknown_validations: num(&n["vu"], "vu")?,
        validation_bad: num(&n["vbad"], "vbad")?,
        lost: num(&n["lost"], "lost")?,
    };

    Ok(ReadRound {
        header,
        t0,
        written_ms: num(&v["tw"], "tw")?,
        proposals,
        validations,
        unknown_validation_keys,
        counts,
    })
}

/// `path` with `.i` appended: the i-th newest rotated file.
pub fn rotated(path: &Path, i: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{i}"));
    PathBuf::from(name)
}

enum Event {
    Unl(Vec<UnlEntry>),
    Proposal(Proposal),
    Validation { blob: Vec<u8>, seen_ms: u64 },
    Header(Header),
}

fn proposal_key(p: &Proposal) -> u64 {
    let mut h = DefaultHasher::new();
    0u8.hash(&mut h);
    (&p.signing_key, &p.prev_ledger, p.propose_seq, p.close_time, &p.position).hash(&mut h);
    h.finish()
}

fn blob_key(blob: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    1u8.hash(&mut h);
    blob.hash(&mut h);
    h.finish()
}

/// The writer thread's state: the open file, the table, the batch and the copies already seen.
struct Writer {
    path: PathBuf,
    rotate_bytes: u64,
    keep: u32,
    out: Option<BufWriter<File>>,
    size: u64,
    table: TrustTable,
    table_due: bool,
    batch: Batch,
    /// Messages seen since the last line, and in the line before it: a late copy is still a copy.
    seen: HashSet<u64>,
    seen_before: HashSet<u64>,
    /// Unknown validation keys this file has listed.
    noted: HashSet<Vec<u8>>,
    last_header: Option<(u32, [u8; 32])>,
    lost: Arc<AtomicU64>,
    failures: u64,
}

impl Writer {
    fn new(path: PathBuf, rotate_bytes: u64, keep: u32, lost: Arc<AtomicU64>) -> Self {
        Self {
            path,
            rotate_bytes,
            keep,
            out: None,
            size: 0,
            table: TrustTable::default(),
            table_due: true,
            batch: Batch::default(),
            seen: HashSet::new(),
            seen_before: HashSet::new(),
            noted: HashSet::new(),
            last_header: None,
            lost,
            failures: 0,
        }
    }

    fn run(mut self, rx: Receiver<Event>) {
        while let Ok(event) = rx.recv() {
            self.handle(event);
        }
        // The channel closed (a test, or shutdown): write what's left.
        if self.batch.rows() > 0 {
            self.flush(None);
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Unl(entries) => {
                let table = TrustTable::from_entries(&entries);
                if table != self.table {
                    self.table = table;
                    self.table_due = true;
                }
            }
            Event::Proposal(p) => {
                if self.first_copy(proposal_key(&p)) {
                    self.batch.proposals.push(p);
                } else {
                    self.batch.proposal_dups += 1;
                }
            }
            Event::Validation { blob, seen_ms } => {
                if !self.first_copy(blob_key(&blob)) {
                    self.batch.validation_dups += 1;
                } else if let Some(v) = parse_validation(&blob, seen_ms) {
                    self.batch.validations.push(v);
                } else {
                    self.batch.validation_bad += 1;
                }
            }
            Event::Header(h) => {
                if self.last_header != Some((h.seq, h.hash)) {
                    self.last_header = Some((h.seq, h.hash));
                    self.flush(Some(&h));
                }
            }
        }
        if self.batch.rows() >= MAX_BATCH_ROWS {
            self.flush(None);
        }
    }

    fn first_copy(&mut self, key: u64) -> bool {
        !self.seen_before.contains(&key) && self.seen.insert(key)
    }

    fn flush(&mut self, header: Option<&Header>) {
        let lost = self.lost.swap(0, Ordering::Relaxed);
        let now = now_ms();
        let batch = std::mem::take(&mut self.batch);
        self.seen_before = std::mem::take(&mut self.seen);
        if let Err(e) = self.write_round(&batch, header, lost, now) {
            self.out = None;
            self.failures += 1;
            if self.failures <= 3 || self.failures.is_multiple_of(1_000) {
                eprintln!(
                    "[consensus-record] writing {} failed (#{}): {e}",
                    self.path.display(),
                    self.failures
                );
            }
        }
    }

    fn write_round(
        &mut self,
        batch: &Batch,
        header: Option<&Header>,
        lost: u64,
        now: u64,
    ) -> io::Result<()> {
        self.open()?;
        let mut line = encode_round(batch, header, &self.table, &mut self.noted, lost, now);
        if self.size > 0 && self.size + line.len() as u64 + 1 > self.rotate_bytes {
            self.rotate()?;
            // A new file lists its own unknown keys.
            line = encode_round(batch, header, &self.table, &mut self.noted, lost, now);
        }
        if self.table_due {
            let table_line = self.table.line(now);
            self.write_line(&table_line)?;
            self.table_due = false;
        }
        self.write_line(&line)?;
        if let Some(out) = self.out.as_mut() {
            out.flush()?;
        }
        Ok(())
    }

    fn open(&mut self) -> io::Result<()> {
        if self.out.is_none() {
            let file = OpenOptions::new().create(true).append(true).open(&self.path)?;
            self.size = file.metadata()?.len();
            self.out = Some(BufWriter::with_capacity(64 * 1024, file));
            // Every file, and every reopen after a restart or an error, restates the table its
            // indexes refer to.
            self.table_due = true;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut out) = self.out.take() {
            out.flush()?;
        }
        for i in (1..self.keep).rev() {
            let from = rotated(&self.path, i);
            if from.exists() {
                std::fs::rename(&from, rotated(&self.path, i + 1))?;
            }
        }
        if self.keep == 0 {
            std::fs::remove_file(&self.path)?;
        } else {
            std::fs::rename(&self.path, rotated(&self.path, 1))?;
        }
        self.noted.clear();
        self.open()
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let out = self.out.as_mut().ok_or_else(|| io::Error::other("file not open"))?;
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")?;
        self.size += line.len() as u64 + 1;
        Ok(())
    }
}

/// A running recorder: the channel the peer loop sends into, and the writer thread behind it.
pub struct Recorder {
    tx: SyncSender<Event>,
    lost: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts the writer thread. The file opens when the first line is written.
    pub fn start(path: PathBuf, rotate_bytes: u64, keep: u32) -> io::Result<Self> {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let lost = Arc::new(AtomicU64::new(0));
        let writer = Writer::new(path, rotate_bytes, keep, Arc::clone(&lost));
        let thread = std::thread::Builder::new()
            .name("consensus-record".into())
            .spawn(move || writer.run(rx))?;
        Ok(Self { tx, lost, thread: Some(thread) })
    }

    /// Hands an event to the writer without waiting. If the channel is full, the event is
    /// counted as lost.
    fn send(&self, event: Event) {
        if self.tx.try_send(event).is_err() {
            self.lost.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn proposal(
        &self,
        signing_key: &[u8],
        prev_ledger: &[u8],
        propose_seq: u32,
        close_time: u32,
        position: &[u8],
    ) {
        let (Ok(prev_ledger), Ok(position)) =
            (<[u8; 32]>::try_from(prev_ledger), <[u8; 32]>::try_from(position))
        else {
            return;
        };
        self.send(Event::Proposal(Proposal {
            signing_key: signing_key.to_vec(),
            prev_ledger,
            propose_seq,
            close_time,
            position,
            seen_ms: now_ms(),
        }));
    }

    pub fn validation(&self, blob: &[u8]) {
        self.send(Event::Validation { blob: blob.to_vec(), seen_ms: now_ms() });
    }

    pub fn header(&self, seq: u32, header: &LedgerHeader) {
        self.send(Event::Header(Header::from_ws(seq, header, now_ms())));
    }

    pub fn unl(&self, entries: &[UnlEntry]) {
        self.send(Event::Unl(entries.to_vec()));
    }

    /// Closes the channel, then waits for the writer to write what it holds. Used by tests and
    /// at shutdown; the process-wide recorder lives as long as the process.
    pub fn finish(mut self) {
        let thread = self.thread.take();
        drop(self);
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();

fn recorder() -> Option<&'static Recorder> {
    RECORDER
        .get_or_init(|| {
            if !std::env::var("XRPL_CONSENSUS_RECORD").map(|v| v == "1").unwrap_or(false) {
                return None;
            }
            let path = std::env::var("XRPL_CONSENSUS_RECORD_PATH")
                .unwrap_or_else(|_| DEFAULT_PATH.to_string());
            match Recorder::start(PathBuf::from(&path), ROTATE_BYTES, KEEP) {
                Ok(r) => {
                    eprintln!(
                        "[consensus-record] ON: {path} (rotates at {} MB, keeps {KEEP} old files)",
                        ROTATE_BYTES / 1_000_000
                    );
                    Some(r)
                }
                Err(e) => {
                    eprintln!("[consensus-record] could not start the writer: {e}; recording stays off");
                    None
                }
            }
        })
        .as_ref()
}

/// Reads `XRPL_CONSENSUS_RECORD` (once) and starts the writer if it is `1`. Returns whether
/// recording is on.
/// - Calling it at startup puts the ON line in the log early.
/// - Each `record_*` call does the same on its first use.
pub fn init() -> bool {
    recorder().is_some()
}

/// A proposal whose signature checked out (`SigOutcome::Valid`).
pub fn record_proposal(
    signing_key: &[u8],
    prev_ledger: &[u8],
    propose_seq: u32,
    close_time: u32,
    position: &[u8],
) {
    if let Some(r) = recorder() {
        r.proposal(signing_key, prev_ledger, propose_seq, close_time, position);
    }
}

/// A validation whose signature checked out (`SigOutcome::Valid`).
pub fn record_validation(blob: &[u8]) {
    if let Some(r) = recorder() {
        r.validation(blob);
    }
}

/// A validated header ws-sync fetched.
pub fn record_header(seq: u32, header: &LedgerHeader) {
    if let Some(r) = recorder() {
        r.header(seq, header);
    }
}

/// The verified list, each time it loads.
pub fn record_unl(entries: &[UnlEntry]) {
    if let Some(r) = recorder() {
        r.unl(entries);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signing(b: u8) -> Vec<u8> {
        let mut k = vec![0x02];
        k.extend_from_slice(&[b; 32]);
        k
    }

    fn master(b: u8) -> Vec<u8> {
        let mut k = vec![0xED];
        k.extend_from_slice(&[b; 32]);
        k
    }

    fn entry(m: u8, s: Option<u8>) -> UnlEntry {
        UnlEntry {
            public_key: hex::encode_upper(master(m)),
            signing_key: s.map(|s| hex::encode_upper(signing(s))),
        }
    }

    /// A list of `n` validators. Validator i has master key 0x40+i and signing key 0x80+i, so
    /// its index in the table is i.
    fn unl(n: u8) -> Vec<UnlEntry> {
        (0..n).map(|i| entry(0x40 + i, Some(0x80 + i))).collect()
    }

    /// An STValidation in mainnet's shape: the fields rippled 3.4.1 sets in
    /// `RCLConsensus::Adaptor::validate`, in canonical order (type, then field).
    /// `voting` adds what a voting ledger's validation also carries: the load fee, the server
    /// version, the XRPFees votes and the amendment votes.
    fn validation_blob(
        key: &[u8],
        seq: u32,
        ledger: [u8; 32],
        consensus: Option<[u8; 32]>,
        signing_time: u32,
        full: bool,
        voting: bool,
    ) -> Vec<u8> {
        let mut b = vec![0x22]; // Flags (UInt32 2): vfFullyCanonicalSig, and vfFullValidation
        b.extend_from_slice(&(0x8000_0000_u32 | u32::from(full)).to_be_bytes());
        b.push(0x26); // LedgerSequence (UInt32 6)
        b.extend_from_slice(&seq.to_be_bytes());
        b.push(0x29); // SigningTime (UInt32 9)
        b.extend_from_slice(&signing_time.to_be_bytes());
        if voting {
            b.extend_from_slice(&[0x20, 0x18]); // LoadFee (UInt32 24)
            b.extend_from_slice(&256_u32.to_be_bytes());
        }
        b.push(0x3A); // Cookie (UInt64 10)
        b.extend_from_slice(&0x1234_5678_9ABC_DEF0_u64.to_be_bytes());
        if voting {
            b.push(0x3B); // ServerVersion (UInt64 11)
            b.extend_from_slice(&0x0103_0401_0000_0000_u64.to_be_bytes());
        }
        b.push(0x51); // LedgerHash (Hash256 1)
        b.extend_from_slice(&ledger);
        if let Some(c) = consensus {
            b.extend_from_slice(&[0x50, 0x17]); // ConsensusHash (Hash256 23)
            b.extend_from_slice(&c);
        }
        b.extend_from_slice(&[0x50, 0x19]); // ValidatedHash (Hash256 25)
        b.extend_from_slice(&[0x77; 32]);
        if voting {
            // BaseFeeDrops, ReserveBaseDrops, ReserveIncrementDrops (Amount 22-24), as XRP
            for field in [0x16_u8, 0x17, 0x18] {
                b.extend_from_slice(&[0x60, field]);
                b.extend_from_slice(&(0x4000_0000_0000_0000_u64 | 200_000).to_be_bytes());
            }
        }
        b.push(0x73); // SigningPubKey (VL 3)
        b.push(key.len() as u8);
        b.extend_from_slice(key);
        b.extend_from_slice(&[0x76, 71]); // Signature (VL 6), DER-sized
        b.extend_from_slice(&[0x30; 71]);
        if voting {
            b.extend_from_slice(&[0x03, 0x13, 64]); // Amendments (Vector256 3)
            b.extend_from_slice(&[0xA1; 32]);
            b.extend_from_slice(&[0xA2; 32]);
        }
        b
    }

    fn proposal(key: Vec<u8>, prev: u8, seq: u32, position: u8, seen_ms: u64) -> Proposal {
        Proposal {
            signing_key: key,
            prev_ledger: [prev; 32],
            propose_seq: seq,
            close_time: 813_000_000,
            position: [position; 32],
            seen_ms,
        }
    }

    fn header(seq: u32) -> Header {
        Header {
            seq,
            hash: [seq as u8; 32],
            parent_hash: [seq.wrapping_sub(1) as u8; 32],
            close_time: 813_000_002,
            parent_close_time: 813_000_000,
            close_resolution: 10,
            close_flags: 0,
            transaction_hash: [0x5A; 32],
            seen_ms: 1_760_000_009_000,
        }
    }

    fn ws_header(hash: u8) -> LedgerHeader {
        LedgerHeader { ledger_hash: [hash; 32], ..LedgerHeader::default() }
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path).expect("reads").lines().map(str::to_owned).collect()
    }

    #[test]
    fn parse_validation_reads_the_recorded_fields() {
        let key = signing(0x81);
        for voting in [false, true] {
            let blob =
                validation_blob(&key, 107_600_001, [0x11; 32], Some([0x22; 32]), 813_000_001, true, voting);
            assert_eq!(
                parse_validation(&blob, 5),
                Some(Validation {
                    signing_key: key.clone(),
                    ledger_seq: 107_600_001,
                    ledger_hash: [0x11; 32],
                    consensus_hash: Some([0x22; 32]),
                    signing_time: 813_000_001,
                    full: true,
                    seen_ms: 5,
                }),
                "voting={voting}"
            );
        }
        // A partial validation that carries no ConsensusHash.
        let v = parse_validation(&validation_blob(&key, 9, [0x11; 32], None, 7, false, false), 0)
            .expect("parses");
        assert_eq!((v.consensus_hash, v.full), (None, false));
    }

    #[test]
    fn parse_validation_refuses_what_it_cannot_record() {
        let key = signing(0x81);
        let blob = validation_blob(&key, 9, [0x11; 32], None, 7, true, false);
        assert!(parse_validation(&blob[..blob.len() - 1], 0).is_none(), "truncated");
        // Cut out the LedgerHash field (0x51 + 32 bytes), which follows Flags, LedgerSequence,
        // SigningTime (5 bytes each) and Cookie (9 bytes).
        let at = 5 + 5 + 5 + 9;
        let mut no_hash = blob[..at].to_vec();
        no_hash.extend_from_slice(&blob[at + 33..]);
        assert!(parse_validation(&no_hash, 0).is_none(), "no ledger hash");
        // An STObject field (type 14) has no place in a validation: the walk stops there.
        let mut odd = blob.clone();
        odd.push(0xE1);
        assert!(parse_validation(&odd, 0).is_none(), "unparseable");
        assert!(parse_validation(&[], 0).is_none(), "empty");
    }

    #[test]
    fn the_trust_table_maps_signing_and_master_keys() {
        let mut entries = vec![entry(3, Some(13)), entry(1, Some(11)), entry(2, None)];
        entries.push(UnlEntry { public_key: "not hex".into(), signing_key: None });
        let table = TrustTable::from_entries(&entries);
        assert_eq!(table.keys.len(), 3, "the bad entry is left out");
        assert_eq!(table.lookup(&signing(11)), Some(0));
        assert_eq!(table.lookup(&master(1)), Some(0));
        assert_eq!(table.lookup(&master(2)), Some(1), "no manifest: found by its master key");
        assert_eq!(table.lookup(&signing(13)), Some(2));
        assert_eq!(table.lookup(&signing(99)), None);
        assert_eq!(TrustTable::parse_line(&table.line(1)).expect("reads back"), table);
    }

    #[test]
    fn a_round_line_reads_back_exactly() {
        let table = TrustTable::from_entries(&unl(35));
        let t = 1_760_000_000_000;
        let mut batch = Batch::default();
        batch.proposals.push(proposal(signing(0x80), 0xA0, 0, 0xB0, t + 10));
        batch.proposals.push(proposal(signing(0x81), 0xA0, 1, 0xB1, t + 20));
        batch.proposals.push(proposal(signing(0x82), 0xA0, SEQ_LEAVE, 0xB1, t + 30)); // a bow-out
        batch.proposals.push(proposal(signing(0x01), 0xA0, 2, 0xB2, t + 40)); // a key we can't map
        let checked = |key: Vec<u8>, consensus: Option<[u8; 32]>, full: bool| {
            let blob = validation_blob(&key, 107_600_001, [0xC0; 32], consensus, 813_000_003, full, false);
            parse_validation(&blob, t + 50).expect("parses")
        };
        batch.validations.push(checked(signing(0x80), Some([0xD0; 32]), true));
        batch.validations.push(checked(signing(0x81), None, false));
        batch.validations.push(checked(signing(0x02), Some([0xD0; 32]), true)); // can't map
        batch.proposal_dups = 4;
        batch.validation_dups = 5;
        batch.validation_bad = 1;
        let mut noted = HashSet::new();
        let h = header(107_600_001);

        let line = encode_round(&batch, Some(&h), &table, &mut noted, 2, t + 99);
        let read = decode_round(&line, &table).expect("decodes");
        assert_eq!(read.header, Some(h.clone()));
        assert_eq!((read.t0, read.written_ms), (t + 10, t + 99));
        let want: Vec<ReadProposal> = batch
            .proposals
            .iter()
            .enumerate()
            .map(|(i, p)| ReadProposal { validator: (i < 3).then_some(i as u32), proposal: p.clone() })
            .collect();
        assert_eq!(read.proposals, want);
        assert_eq!(
            read.validations,
            vec![(0, batch.validations[0].clone()), (1, batch.validations[1].clone())]
        );
        assert_eq!(read.unknown_validation_keys, vec![signing(0x02)]);
        assert_eq!(
            read.counts,
            LineCounts {
                proposal_dups: 4,
                validation_dups: 5,
                unknown_proposals: 1,
                unknown_validations: 1,
                validation_bad: 1,
                lost: 2,
            }
        );

        // A file lists an unknown key once.
        let again = encode_round(&batch, Some(&h), &table, &mut noted, 0, t + 199);
        let read = decode_round(&again, &table).expect("decodes");
        assert!(read.unknown_validation_keys.is_empty());
        assert_eq!(read.counts.unknown_validations, 1);
    }

    /// The module doc's example, byte for byte: change the two together.
    #[test]
    fn the_documented_example_is_what_the_encoder_writes() {
        let unl_line = r#"{"k":"unl","ver":1,"t":1760000000000,"keys":[["ED4040404040404040404040404040404040404040404040404040404040404040","028080808080808080808080808080808080808080808080808080808080808080"],["ED4141414141414141414141414141414141414141414141414141414141414141","028181818181818181818181818181818181818181818181818181818181818181"]]}"#;
        let round_line = r#"{"k":"r","ver":1,"seq":107600001,"hash":"1111111111111111111111111111111111111111111111111111111111111111","parent":"1010101010101010101010101010101010101010101010101010101010101010","ct":813000001,"pct":813000000,"res":10,"flags":0,"txh":"5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A5A","th":1760000003600,"t0":1760000000000,"tw":1760000003601,"h":["1010101010101010101010101010101010101010101010101010101010101010","B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0B0","B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1B1","1111111111111111111111111111111111111111111111111111111111111111"],"pr":[[0,0,0,813000003,1,0],[1,0,0,813000003,1,40],[0,0,1,813000003,2,850],[1,0,-1,813000003,1,2400]],"pu":[["020101010101010101010101010101010101010101010101010101010101010101",0,0,813000003,1,60]],"va":[[0,107600001,3,2,813000004,3100,1]],"uk":["020202020202020202020202020202020202020202020202020202020202020202"],"n":{"pdup":3,"vdup":5,"pu":1,"vu":1,"vbad":0,"lost":0}}"#;
        let t = 1_760_000_000_000;
        let table = TrustTable::from_entries(&unl(2));
        assert_eq!(table.line(t), unl_line);

        let proposed = |key: Vec<u8>, seq: u32, position: u8, after: u64| Proposal {
            signing_key: key,
            prev_ledger: [0x10; 32],
            propose_seq: seq,
            close_time: 813_000_003,
            position: [position; 32],
            seen_ms: t + after,
        };
        let validated = |key: Vec<u8>, after: u64| Validation {
            signing_key: key,
            ledger_seq: 107_600_001,
            ledger_hash: [0x11; 32],
            consensus_hash: Some([0xB1; 32]),
            signing_time: 813_000_004,
            full: true,
            seen_ms: t + after,
        };
        let batch = Batch {
            proposals: vec![
                proposed(signing(0x80), 0, 0xB0, 0),
                proposed(signing(0x81), 0, 0xB0, 40),
                proposed(signing(0x01), 0, 0xB0, 60), // a key that isn't on the list
                proposed(signing(0x80), 1, 0xB1, 850),
                proposed(signing(0x81), SEQ_LEAVE, 0xB0, 2_400), // the bow-out
            ],
            validations: vec![validated(signing(0x80), 3_100), validated(signing(0x02), 3_150)],
            proposal_dups: 3,
            validation_dups: 5,
            validation_bad: 0,
        };
        let header = Header {
            seq: 107_600_001,
            hash: [0x11; 32],
            parent_hash: [0x10; 32],
            close_time: 813_000_001,
            parent_close_time: 813_000_000,
            close_resolution: 10,
            close_flags: 0,
            transaction_hash: [0x5A; 32],
            seen_ms: t + 3_600,
        };
        let line = encode_round(&batch, Some(&header), &table, &mut HashSet::new(), 0, t + 3_601);
        assert_eq!(line, round_line);
        let read = decode_round(&line, &table).expect("decodes");
        assert_eq!(read.header, Some(header));
        assert_eq!(read.proposals.len(), 5);
    }

    #[test]
    fn a_line_without_a_header_reads_back() {
        let table = TrustTable::from_entries(&unl(35));
        let mut batch = Batch::default();
        batch.proposals.push(proposal(signing(0x80), 0xA0, 0, 0xB0, 7));
        let line = encode_round(&batch, None, &table, &mut HashSet::new(), 0, 9);
        let read = decode_round(&line, &table).expect("decodes");
        assert_eq!(read.header, None);
        assert_eq!(read.proposals.len(), 1);
    }

    #[test]
    fn unknown_proposals_are_kept_up_to_the_cap_and_all_counted() {
        let table = TrustTable::from_entries(&unl(35));
        let mut batch = Batch::default();
        for i in 0..100u32 {
            batch.proposals.push(proposal(signing(0x01), 0xA0, i, 0xB0, u64::from(i)));
        }
        let line = encode_round(&batch, None, &table, &mut HashSet::new(), 0, 0);
        let read = decode_round(&line, &table).expect("decodes");
        assert_eq!(read.proposals.len(), MAX_UNKNOWN_PROPOSALS);
        assert_eq!(read.counts.unknown_proposals, 100);
    }

    /// The disk figure in the module docs. A typical ledger: 35 validators send 3 proposals each,
    /// 35 trusted validations arrive, and 120 more come from validators off the list.
    #[test]
    fn a_typical_ledger_line_is_about_5_kb() {
        let table = TrustTable::from_entries(&unl(35));
        let t = 1_760_000_000_000_u64;
        let mut batch = Batch::default();
        for i in 0..35u8 {
            for seq in 0..3u32 {
                batch.proposals.push(Proposal {
                    signing_key: signing(0x80 + i),
                    prev_ledger: [0xA0; 32],
                    propose_seq: seq,
                    close_time: 813_000_000 + seq,
                    position: [0xB0 + seq as u8; 32],
                    seen_ms: t + u64::from(i) * 37 + u64::from(seq) * 1_100,
                });
            }
            batch.validations.push(Validation {
                signing_key: signing(0x80 + i),
                ledger_seq: 107_600_001,
                ledger_hash: [0xC0; 32],
                consensus_hash: Some([0xB2; 32]),
                signing_time: 813_000_004,
                full: true,
                seen_ms: t + 3_900 + u64::from(i) * 11,
            });
        }
        for i in 0..120u8 {
            batch.validations.push(Validation {
                signing_key: signing(i),
                ledger_seq: 107_600_001,
                ledger_hash: [0xC0; 32],
                consensus_hash: Some([0xB2; 32]),
                signing_time: 813_000_004,
                full: true,
                seen_ms: t + 3_950,
            });
        }
        let mut noted = HashSet::new();
        let first = encode_round(&batch, Some(&header(107_600_001)), &table, &mut noted, 0, t + 4_000);
        let steady = encode_round(&batch, Some(&header(107_600_002)), &table, &mut noted, 0, t + 8_000);
        let per_day_mb = steady.len() as f64 * 86_400.0 / 3.865 / 1e6;
        println!(
            "consensus_record sizes: table line {} B, a file's first round line {} B, \
             a steady round line {} B, {per_day_mb:.0} MB a day at 3.865 s a ledger",
            table.line(t).len(),
            first.len(),
            steady.len()
        );
        assert!(steady.len() < 6_000, "{} bytes", steady.len());
    }

    #[test]
    fn the_writer_records_a_round_and_drops_copies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rounds.jsonl");
        let r = Recorder::start(path.clone(), ROTATE_BYTES, KEEP).expect("starts");
        r.unl(&unl(35));
        let blob =
            validation_blob(&signing(0x81), 107_600_001, [0xC0; 32], Some([0xD0; 32]), 813_000_003, true, true);
        for _ in 0..3 {
            r.proposal(&signing(0x80), &[0xA0; 32], 1, 813_000_000, &[0xB0; 32]);
            r.validation(&blob);
        }
        r.proposal(&signing(0x80), &[0xA0; 32], 1, 813_000_000, &[0xB0; 31]); // malformed: ignored
        let ws = LedgerHeader {
            parent_hash: [0x10; 32],
            parent_close_time: 813_000_000,
            close_time: 813_000_002,
            close_resolution: 10,
            ..ws_header(0x11)
        };
        r.header(107_600_001, &ws);
        r.header(107_600_001, &ws); // the same header again: no second line
        r.finish();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2, "{lines:#?}");
        let table = TrustTable::parse_line(&lines[0]).expect("the table comes first");
        assert_eq!(table, TrustTable::from_entries(&unl(35)));
        let round = decode_round(&lines[1], &table).expect("decodes");
        let h = round.header.expect("a header");
        assert_eq!(
            (h.seq, h.hash, h.parent_hash, h.close_time, h.close_resolution),
            (107_600_001, [0x11; 32], [0x10; 32], 813_000_002, 10)
        );
        assert_eq!(round.proposals.len(), 1);
        assert_eq!(round.proposals[0].validator, Some(0));
        assert_eq!(round.validations.len(), 1);
        assert_eq!(round.validations[0].0, 1);
        assert_eq!(round.validations[0].1.consensus_hash, Some([0xD0; 32]));
        assert_eq!((round.counts.proposal_dups, round.counts.validation_dups), (2, 2));
    }

    #[test]
    fn a_copy_arriving_after_the_next_header_is_still_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rounds.jsonl");
        let r = Recorder::start(path.clone(), ROTATE_BYTES, KEEP).expect("starts");
        r.unl(&unl(35));
        r.proposal(&signing(0x80), &[0xA0; 32], 0, 1, &[0xB0; 32]);
        r.header(1, &ws_header(1));
        r.proposal(&signing(0x80), &[0xA0; 32], 0, 1, &[0xB0; 32]); // a late copy
        r.header(2, &ws_header(2));
        r.finish();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3, "{lines:#?}");
        let table = TrustTable::parse_line(&lines[0]).expect("table");
        let second = decode_round(&lines[2], &table).expect("decodes");
        assert!(second.proposals.is_empty());
        assert_eq!(second.counts.proposal_dups, 1);
    }

    #[test]
    fn the_file_rotates_and_keeps_two_old_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rounds.jsonl");
        let limit = 3_000;
        let r = Recorder::start(path.clone(), limit, 2).expect("starts");
        r.unl(&unl(5));
        for seq in 1..=40u32 {
            for i in 0..5u8 {
                r.proposal(&signing(0x80 + i), &[seq as u8; 32], 0, 813_000_000, &[0xB0; 32]);
            }
            r.header(seq, &ws_header(seq as u8));
        }
        r.finish();

        assert!(rotated(&path, 1).exists() && rotated(&path, 2).exists());
        assert!(!rotated(&path, 3).exists());
        let mut seqs = Vec::new();
        for file in [rotated(&path, 2), rotated(&path, 1), path.clone()] {
            assert!(std::fs::metadata(&file).expect("exists").len() <= limit, "{}", file.display());
            let lines = read_lines(&file);
            let table = TrustTable::parse_line(&lines[0]).expect("each file starts with the table");
            for line in &lines[1..] {
                let round = decode_round(line, &table).expect("decodes");
                assert_eq!(round.proposals.len(), 5);
                seqs.push(round.header.expect("a header").seq);
            }
        }
        assert_eq!(seqs.last(), Some(&40));
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    }

    #[test]
    fn a_restart_appends_and_restates_the_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rounds.jsonl");
        for seq in 1..=2u32 {
            let r = Recorder::start(path.clone(), ROTATE_BYTES, KEEP).expect("starts");
            r.unl(&unl(35));
            r.proposal(&signing(0x80), &[seq as u8; 32], 0, 1, &[0xB0; 32]);
            r.header(seq, &ws_header(seq as u8));
            r.finish();
        }
        let lines = read_lines(&path);
        let kinds: Vec<String> = lines
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).expect("json")["k"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(kinds, ["unl", "r", "unl", "r"]);
    }

    #[test]
    fn a_full_channel_counts_what_it_turned_away_and_the_next_line_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rounds.jsonl");
        let (tx, rx) = sync_channel(2);
        let lost = Arc::new(AtomicU64::new(0));
        let r = Recorder { tx, lost: Arc::clone(&lost), thread: None };
        for _ in 0..5 {
            r.proposal(&signing(0x80), &[0xA0; 32], 0, 1, &[0xB0; 32]);
        }
        assert_eq!(lost.load(Ordering::Relaxed), 3, "two fit, three were turned away");
        r.header(1, &ws_header(1)); // turned away too: the channel is still full
        drop(r);
        Writer::new(path.clone(), ROTATE_BYTES, KEEP, lost).run(rx);

        let lines = read_lines(&path);
        let table = TrustTable::parse_line(&lines[0]).expect("table");
        let round = decode_round(&lines[1], &table).expect("decodes");
        assert_eq!(round.header, None, "the header never arrived");
        assert_eq!(round.counts.lost, 4);
        assert_eq!(round.counts.proposal_dups, 1, "the two that fit are one proposal");
        assert_eq!(round.counts.unknown_proposals, 1, "no list loaded yet");
    }

    #[test]
    fn recording_is_off_unless_the_switch_is_set() {
        if std::env::var("XRPL_CONSENSUS_RECORD").is_ok() {
            return; // someone ran the tests with the switch on
        }
        assert!(!init());
        // With recording off, every entry point returns at once.
        record_unl(&unl(35));
        record_proposal(&signing(0x80), &[0; 32], 0, 0, &[0; 32]);
        record_validation(&[]);
        record_header(1, &ws_header(1));
    }
}
