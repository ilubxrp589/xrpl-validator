#!/usr/bin/env python3
"""loan_bundle.py — a byte-exact vector bundle for one Loan transaction.

The Loan counterpart of loan_broker_bundle.py: a thin driver over
fetch_tx_bundle.py (same bundle format; harness:
crates/xrpl-node/tests/loan_vector.rs). It works out what a LoanSet, LoanPay,
LoanManage or LoanDelete READS beyond what its metadata names, from the
transaction, the Loan, the LoanBroker and the Vault as they stood in the
parent ledger, and hands those keys to fetch_tx_bundle.py as EXTRA_KEYS:

  - FeeSettings (the reserves);
  - the Loan (sfLoanID), its LoanBroker (the Loan's or the transaction's
    sfLoanBrokerID), the broker's Vault;
  - the roots of the submitter, the borrower, the broker's owner and the two
    pseudo-accounts, and each one's holding of the vault's asset (trust line
    or MPToken);
  - the asset: its MPTokenIssuance, its issuer's root and owner directory;
  - the owner directories of the borrower and the broker's pseudo-account
    (the two a Loan is linked into).

Only the Loan, the LoanBroker and the Vault are read as JSON; every other key
is derived locally (Indexes.cpp) and fetch_tx_bundle.py seats the ones that
exist in the parent ledger.

One field is added to fetch_tx_bundle.py's bundle: `close_time_resolution`,
the resolution of the ledger the transaction is in. A Loan's sfStartDate is
the open ledger's close time, the parent's close time plus that resolution
(Ledger.cpp:262-278), so the harness needs it.

Usage: XRPL_RPC=<url> loan_bundle.py <tx_hash> <ledger_seq> <out.json>
Read-only; two to four dozen sequential requests per transaction.
"""
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

RPC = os.environ.get("XRPL_RPC", "http://127.0.0.1:5005/")
FEE_SETTINGS = "4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651"
ALPHABET = "rpshnaf39wBUDNEGHJKLM4PQRST7VWXYZ2bcdeCg65jkm8oFqi1tuvAxyz"


def rpc(method, params):
    req = urllib.request.Request(
        RPC,
        data=json.dumps({"method": method, "params": [params]}).encode(),
        headers={"Content-Type": "application/json"},
    )
    time.sleep(0.05)
    return json.load(urllib.request.urlopen(req, timeout=60))["result"]


def main():
    tx_hash, seq, out = sys.argv[1].upper(), int(sys.argv[2]), sys.argv[3]
    led = rpc("ledger", {"ledger_index": seq, "transactions": True, "expand": True})
    txs = [t for t in led["ledger"]["transactions"] if t.get("hash", "").startswith(tx_hash)]
    if len(txs) != 1:
        sys.exit(f"{tx_hash} matched {len(txs)} transactions in #{seq}")
    tx = txs[0]
    resolution = int(led["ledger"]["close_time_resolution"])
    parent = seq - 1
    keys = [FEE_SETTINGS]

    def entry(index):
        """The object's JSON in the parent ledger, or None."""
        if not index:
            return None
        r = rpc("ledger_entry", {"index": index, "ledger_index": parent})
        if "node" not in r:
            return None
        keys.append(r["index"])
        return r["node"]

    def h(space, *parts):
        return hashlib.sha512(bytes([0, ord(space)]) + b"".join(parts)).digest()[:32].hex().upper()

    def acct(addr):
        n = 0
        for ch in addr:
            n = n * 58 + ALPHABET.index(ch)
        return n.to_bytes(25, "big")[1:21]

    def currency(code):
        if len(code) == 40:
            return bytes.fromhex(code)
        return bytes(12) + code.encode() + bytes(5)

    def root(addr):
        keys.append(h("a", acct(addr)))

    def owner_dir(addr):
        keys.append(h("O", acct(addr)))

    def holding(account, asset):
        if "mpt_issuance_id" in asset:
            issuance = h("~", bytes.fromhex(asset["mpt_issuance_id"]))
            keys.append(h("t", bytes.fromhex(issuance), acct(account)))
        elif asset.get("currency") != "XRP" and account != asset["issuer"]:
            low, high = sorted([acct(account), acct(asset["issuer"])])
            keys.append(h("r", low, high, currency(asset["currency"])))

    def asset_objects(asset):
        if "mpt_issuance_id" in asset:
            raw = bytes.fromhex(asset["mpt_issuance_id"])
            keys.append(h("~", raw))
            keys.append(h("a", raw[4:]))
        elif asset.get("currency") != "XRP":
            root(asset["issuer"])
            owner_dir(asset["issuer"])

    account = tx["Account"]
    root(account)
    loan = entry(tx.get("LoanID"))
    broker = entry(tx.get("LoanBrokerID") or (loan or {}).get("LoanBrokerID"))
    vault = entry((broker or {}).get("VaultID"))
    parties = [account]
    if "Counterparty" in tx:
        parties.append(tx["Counterparty"])
    if loan:
        parties.append(loan["Borrower"])
    if broker:
        parties += [broker["Owner"], broker["Account"]]
    if vault:
        parties.append(vault["Account"])
    parties = list(dict.fromkeys(parties))
    for party in parties:
        root(party)
    if vault:
        asset = vault["Asset"]
        asset_objects(asset)
        for party in parties:
            holding(party, asset)
    if broker:
        owner_dir(broker["Account"])
        borrower = (loan or {}).get("Borrower")
        if not borrower:
            counterparty = tx.get("Counterparty", broker["Owner"])
            borrower = account if counterparty == broker["Owner"] else counterparty
        owner_dir(borrower)

    keys = list(dict.fromkeys(keys))
    with tempfile.NamedTemporaryFile("w", suffix=".keys", delete=False) as f:
        f.write("\n".join(keys) + "\n")
    env = dict(os.environ, EXTRA_KEYS=f.name, XRPL_RPC=RPC)
    here = os.path.dirname(os.path.abspath(__file__))
    r = subprocess.run([sys.executable, os.path.join(here, "fetch_tx_bundle.py"), tx_hash, str(seq), out], env=env)
    os.unlink(f.name)
    if r.returncode == 0:
        with open(out) as f:
            bundle = json.load(f)
        bundle["close_time_resolution"] = resolution
        with open(out, "w") as f:
            json.dump(bundle, f, indent=1)
    sys.exit(r.returncode)


if __name__ == "__main__":
    main()
