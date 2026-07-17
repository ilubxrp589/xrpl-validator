#!/usr/bin/env python3
"""Fetch a mainnet ledger into offline test fixtures (rebuilds the capability the
test docs called `fetch_cluster_txs.sh`, which was never committed).

For ledger N, writes into crates/xrpl-node/tests/data/:
  l<N>_blobs.txt      one `<TransactionIndex>\t<tx_blob_hex>` per real tx
                      (pseudo-txs skipped), same format ticket_cluster_reapply uses
  l<N>_expected.json  {"header": {ledger_seq, parent_hash, parent_close_time,
                                  total_drops},
                       "txs": {"<HASH>": {"ter": "...",
                                          "nodes": [["<LedgerIndex>", kind], ...]}}}
                      kind: 0=Created 1=Modified 2=Deleted (ws_sync encoding)

parent_close_time comes from ledger N-1's close_time. Source must be full-history
(default s2.ripple.com). Usage:
  python3 scripts/fetch_ledger_fixture.py <seq> [<seq>...] [--rpc URL] [--outdir DIR]
"""
import json
import sys
import urllib.request

RPC = "https://s2.ripple.com:51234"
OUTDIR = "crates/xrpl-node/tests/data"
PSEUDO_TX_TYPES = {100, 101, 102}  # EnableAmendment, SetFee, UNLModify


def rpc(url, method, params):
    req = urllib.request.Request(
        url, data=json.dumps({"method": method, "params": [params]}).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        body = json.loads(r.read())
    res = body.get("result", {})
    if res.get("status") != "success":
        raise RuntimeError(f"{method} {params.get('ledger_index')}: {res.get('error', body)}")
    return res


def tx_index_from_meta(meta_hex):
    try:
        b = bytes.fromhex(meta_hex[:12])
        if len(b) >= 6 and b[0] == 0x20 and b[1] == 0x1C:
            return int.from_bytes(b[2:6], "big")
    except ValueError:
        pass
    return None


def is_pseudo(blob_hex):
    try:
        b = bytes.fromhex(blob_hex[:6])
        return len(b) >= 3 and b[0] == 0x12 and int.from_bytes(b[1:3], "big") in PSEUDO_TX_TYPES
    except ValueError:
        return False


def net_params(url):
    """Network-specific apply parameters (drops) from server_state; mainnet
    defaults on any failure. Lets fixtures from testnet/devnet replay with the
    right network_id, fees and reserves."""
    try:
        req = urllib.request.Request(
            url, data=json.dumps({"method": "server_state", "params": [{}]}).encode(),
            headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=30) as r:
            st = json.loads(r.read())["result"]["state"]
        vl = st.get("validated_ledger", {})
        return {
            "network_id": int(st.get("network_id", 0)),
            "base_fee_drops": int(vl.get("base_fee", 10)),
            "reserve_drops": int(vl.get("reserve_base", 1_000_000)),
            "increment_drops": int(vl.get("reserve_inc", 200_000)),
        }
    except Exception as e:
        print(f"  (net_params fallback to mainnet defaults: {e})")
        return {"network_id": 0, "base_fee_drops": 10,
                "reserve_drops": 1_000_000, "increment_drops": 200_000}


def fetch_one(url, seq, outdir):
    binres = rpc(url, "ledger", {"ledger_index": seq, "transactions": True,
                                 "expand": True, "binary": True})
    jres = rpc(url, "ledger", {"ledger_index": seq, "transactions": True,
                               "expand": True, "binary": False})
    parent = rpc(url, "ledger", {"ledger_index": seq - 1, "transactions": False})

    lg = jres["ledger"]
    header = {
        "ledger_seq": seq,
        "parent_hash": lg["parent_hash"].upper(),
        "parent_close_time": int(parent["ledger"]["close_time"]),
        "total_drops": int(lg["total_coins"]),
    }
    header.update(net_params(url))

    rows = []
    for tx in binres["ledger"]["transactions"]:
        blob = tx.get("tx_blob") if isinstance(tx, dict) else tx
        meta = tx.get("meta", "") if isinstance(tx, dict) else ""
        if not blob or is_pseudo(blob):
            continue
        idx = tx_index_from_meta(meta)
        if idx is None:
            raise RuntimeError(f"tx without parseable TransactionIndex in #{seq}")
        rows.append((idx, blob.upper()))
    rows.sort()

    txs = {}
    tx_json = {}       # hash -> tx fields (metaData stripped) for the native leg
    ordered_hashes = []  # (TransactionIndex, hash) for canonical native-apply order
    for tx in jres["ledger"]["transactions"]:
        h = tx.get("hash", "").upper()
        md = tx.get("metaData") or tx.get("meta") or {}
        ter = md.get("TransactionResult")
        if not h or not ter:
            continue
        nodes = []
        for node in md.get("AffectedNodes", []):
            if "CreatedNode" in node:
                kind, inner = 0, node["CreatedNode"]
            elif "ModifiedNode" in node:
                kind, inner = 1, node["ModifiedNode"]
            elif "DeletedNode" in node:
                kind, inner = 2, node["DeletedNode"]
            else:
                continue
            li = inner.get("LedgerIndex")
            if li:
                nodes.append([li.upper(), kind])
        txs[h] = {"ter": ter, "nodes": nodes}
        fields = {k: v for k, v in tx.items() if k not in ("metaData", "meta", "hash")}
        tx_json[h] = fields
        ordered_hashes.append((md.get("TransactionIndex", 0), h))

    ordered_hashes.sort(key=lambda x: x[0])
    tx_order = [h for _, h in ordered_hashes]

    blobs_path = f"{outdir}/l{seq}_blobs.txt"
    with open(blobs_path, "w") as f:
        for idx, blob in rows:
            f.write(f"{idx}\t{blob}\n")
    exp_path = f"{outdir}/l{seq}_expected.json"
    with open(exp_path, "w") as f:
        json.dump({"header": header, "txs": txs, "tx_json": tx_json, "tx_order": tx_order},
                  f, indent=0, sort_keys=True)
    print(f"#{seq}: {len(rows)} txs -> {blobs_path}, {len(txs)} expected + {len(tx_json)} tx_json -> {exp_path} "
          f"(parent {header['parent_hash'][:12]}…, pct {header['parent_close_time']})")


def main():
    args = [a for a in sys.argv[1:]]
    url, outdir, seqs = RPC, OUTDIR, []
    i = 0
    while i < len(args):
        if args[i] == "--rpc":
            url = args[i + 1]; i += 2
        elif args[i] == "--outdir":
            outdir = args[i + 1]; i += 2
        else:
            seqs.append(int(args[i])); i += 1
    if not seqs:
        print(__doc__)
        return 1
    for seq in seqs:
        fetch_one(url, seq, outdir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
