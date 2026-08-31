//! Sequence fuzz: SHAMap vs a HashMap oracle over book-shaped key clusters.
//!
//! Motivated by the 2026-08-31 live-shadow RECONCILE-LEAK class: on certain
//! BookDirectory page keys the mirror read back the PREVIOUS write after an
//! insert — three consecutive inserts of the same key bounced while lookup
//! kept returning the older value. Book-page siblings share 50+ nibbles and
//! churn through create/delete (leaf-hoisting collapse) constantly, so this
//! fuzz drives exactly that shape: deep shared prefixes, dense overwrite /
//! delete / re-create cycling, and after EVERY op asserts the touched key
//! reads back its oracle value (full-map sweep every 251 ops).

use std::collections::HashMap;

use xrpl_core::types::Hash256;
use xrpl_ledger::shamap::tree::SHAMap;
use xrpl_ledger::shamap::TreeType;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn clustered_ops_match_oracle() {
    let mut rng = Rng(0x5FB8_DF78_5A09_AE57);
    for trial in 0..32u32 {
        let mut map = SHAMap::new(TreeType::State);
        let mut oracle: HashMap<[u8; 32], Vec<u8>> = HashMap::new();

        // Four "books": 24-byte bases; bytes 24..30 zero (shared); only the
        // last two bytes vary — siblings share 60 nibbles, like clustered
        // quality suffixes of one order book.
        let bases: Vec<[u8; 32]> = (0..4u8)
            .map(|b| {
                let mut k = [0u8; 32];
                k[..24].copy_from_slice(&[b.wrapping_mul(37).wrapping_add(trial as u8); 24]);
                k
            })
            .collect();

        for op in 0..20_000u32 {
            let r = rng.next();
            let mut key = bases[(r as usize >> 8) % bases.len()];
            key[30] = ((r >> 16) & 0x0F) as u8;
            key[31] = ((r >> 24) & 0x3F) as u8;
            let kh = Hash256(key);

            match r % 3 {
                0 | 1 => {
                    let val = format!("t{trial}-o{op}").into_bytes();
                    map.insert(kh, val.clone()).expect("insert never errors on 32-byte keys");
                    oracle.insert(key, val);
                }
                _ => {
                    let _ = map.delete(&kh).expect("delete never errors");
                    oracle.remove(&key);
                }
            }

            let got = map.lookup(&kh).map(|b| b.to_vec());
            assert_eq!(
                got,
                oracle.get(&key).cloned(),
                "trial {trial} op {op}: touched key {:02X}{:02X} reads back wrong value",
                key[30],
                key[31]
            );

            if op % 251 == 0 {
                for (k, v) in &oracle {
                    assert_eq!(
                        map.lookup(&Hash256(*k)).map(|b| b.to_vec()).as_ref(),
                        Some(v),
                        "trial {trial} op {op}: sweep mismatch on key …{:02X}{:02X}",
                        k[30],
                        k[31]
                    );
                }
                assert_eq!(map.len() as usize, oracle.len(), "trial {trial} op {op}: size divergence");
            }
        }
        // Final full sweep.
        for (k, v) in &oracle {
            assert_eq!(map.lookup(&Hash256(*k)).map(|b| b.to_vec()).as_ref(), Some(v));
        }
    }
}

/// Ordered prefix enumeration vs the oracle, under the same churn — the book
/// walk's actual read primitive. The 2026-08-31 depth-64 insert fix made
/// depth-64 leaf pairs exist in live trees for the first time; lookup-only
/// fuzzing cannot see an enumeration defect (the replay world never churns
/// via undo/reconcile, and the probe world enumerates from RPC instead).
#[test]
fn prefix_scan_matches_oracle_under_churn() {
    let mut rng = Rng(0x74E2_46E5_7A64_18FC);
    for trial in 0..24u32 {
        let mut map = SHAMap::new(TreeType::State);
        let mut oracle: std::collections::HashMap<[u8; 32], Vec<u8>> = std::collections::HashMap::new();
        let bases: Vec<[u8; 32]> = (0..3u8)
            .map(|b| {
                let mut k = [0u8; 32];
                k[..24].copy_from_slice(&[b.wrapping_mul(41).wrapping_add(trial as u8); 24]);
                k
            })
            .collect();
        for op in 0..8_000u32 {
            let r = rng.next();
            let mut key = bases[(r as usize >> 8) % bases.len()];
            key[30] = ((r >> 16) & 0x0F) as u8;
            key[31] = ((r >> 24) & 0x3F) as u8;
            let kh = Hash256(key);
            match r % 3 {
                0 | 1 => {
                    map.insert(kh, format!("t{trial}-o{op}").into_bytes()).unwrap();
                    oracle.insert(key, Vec::new());
                }
                _ => {
                    let _ = map.delete(&kh).unwrap();
                    oracle.remove(&key);
                }
            }
            if op % 97 == 0 {
                for base in &bases {
                    for plen in [24usize, 26, 30, 31, 32] {
                        let prefix = &base[..plen];
                        // The 32-byte prefix only matches when key[30..] is 0,
                        // fine — the oracle agrees.
                        let mut want: Vec<[u8; 32]> = oracle
                            .keys()
                            .filter(|k| &k[..plen] == prefix)
                            .copied()
                            .collect();
                        want.sort();
                        let got: Vec<[u8; 32]> = map.keys_with_prefix(prefix).iter().map(|h| h.0).collect();
                        assert_eq!(
                            got, want,
                            "trial {trial} op {op}: keys_with_prefix({plen}B) diverges from oracle \
                             (got {} keys, want {})",
                            got.len(),
                            want.len()
                        );
                    }
                }
            }
        }
    }
}

/// The minimal reproducer of the depth-64 freeze (2026-08-31): two keys
/// differing only in their LAST nibble legally place both leaves at depth
/// 64. The old top-of-insert `depth >= 64` guard let the pair be BUILT but
/// errored every later overwrite of either key — and `let _ = insert(...)`
/// callers silently froze the key at its old value (lookup and delete,
/// unguarded, kept working). Adjacent book-page qualities collide like this
/// in the wild constantly.
#[test]
fn last_nibble_siblings_stay_writable() {
    let mut map = SHAMap::new(TreeType::State);
    let mut a = [0x5F; 32];
    a[31] = 0x04;
    let mut b = a;
    b[31] = 0x05; // shares the first 63 nibbles with `a`
    let (ka, kb) = (Hash256(a), Hash256(b));
    map.insert(ka, b"a0".to_vec()).unwrap();
    map.insert(kb, b"b0".to_vec()).unwrap(); // builds the depth-64 leaf pair
    map.insert(ka, b"a1".to_vec()).expect("overwrite of a depth-64 leaf must land");
    map.insert(kb, b"b1".to_vec()).expect("overwrite of a depth-64 leaf must land");
    assert_eq!(map.lookup(&ka), Some(&b"a1"[..]));
    assert_eq!(map.lookup(&kb), Some(&b"b1"[..]));
    assert!(map.delete(&ka).unwrap());
    assert_eq!(map.lookup(&ka), None);
    assert_eq!(map.lookup(&kb), Some(&b"b1"[..]));
    map.insert(ka, b"a2".to_vec()).unwrap();
    assert_eq!(map.lookup(&ka), Some(&b"a2"[..]));
    assert!(map.insert_hash_only(Hash256({ let mut c = a; c[31] = 0x06; c }), Hash256([9; 32])).is_ok());
}

/// The live failure's exact op shape on one key: insert (a prior ledger's
/// reconcile), overwrite (native apply), overwrite back (undo restore),
/// overwrite forward (reconcile), read — with sibling create/delete churn
/// between rounds to exercise hoisting around the key.
#[test]
fn overwrite_after_sibling_churn_always_lands() {
    let mut rng = Rng(0xC83D_47E7_0CDF_44D0);
    let mut map = SHAMap::new(TreeType::State);
    let mut base = [0u8; 32];
    base[..24].copy_from_slice(&[0x5F; 24]);

    let key = {
        let mut k = base;
        k[31] = 0x04;
        Hash256(k)
    };
    map.insert(key, b"v0".to_vec()).unwrap();

    for round in 0..5_000u32 {
        // Sibling churn: create then delete a handful of deep-prefix siblings.
        let mut sibs = Vec::new();
        for _ in 0..(rng.next() % 4) {
            let mut s = base;
            s[30] = (rng.next() & 0x0F) as u8;
            s[31] = (rng.next() & 0xFF) as u8;
            if s == key.0 {
                continue;
            }
            let _ = map.insert(Hash256(s), b"sib".to_vec()).unwrap();
            sibs.push(s);
        }
        for s in &sibs {
            if rng.next() % 2 == 0 {
                let _ = map.delete(&Hash256(*s)).unwrap();
            }
        }

        // The reconcile shape: native write, undo restore, canonical write.
        let native = format!("native-{round}").into_bytes();
        let undo = format!("undo-{round}").into_bytes();
        let canon = format!("canon-{round}").into_bytes();
        map.insert(key, native).unwrap();
        map.insert(key, undo).unwrap();
        map.insert(key, canon.clone()).unwrap();
        assert_eq!(
            map.lookup(&key).map(|b| b.to_vec()),
            Some(canon),
            "round {round}: the canonical overwrite must be what reads back"
        );
        // Clean up remaining siblings so the shape re-randomizes.
        for s in sibs {
            let _ = map.delete(&Hash256(s));
        }
    }
}
