#!/usr/bin/env python3
"""loan_broker_bundle.py — a byte-exact vector bundle for one loan-broker transaction.

The LoanBroker counterpart of vault_bundle.py: a thin driver over
fetch_tx_bundle.py (same bundle format; harness:
crates/xrpl-node/tests/loan_broker_vector.rs). It works out what the
transaction READS beyond what its metadata names, from the transaction, the
LoanBroker and the Vault as they stood in the parent ledger, asks the server
for each object's index (ledger_entry by its natural key) and hands the list
to fetch_tx_bundle.py as EXTRA_KEYS:

  - FeeSettings (the reserves);
  - the LoanBroker (sfLoanBrokerID, or — LoanBrokerCoverClawback without one —
    the sfLoanBrokerID of the account sfAmount names as issuer), its
    pseudo-account's root;
  - the Vault (sfVaultID or the broker's), its pseudo-account's root;
  - the asset: its MPTokenIssuance and issuer's root, or its issuer's root;
  - for each party (the submitter, the Destination, the broker's owner, the
    broker's and the vault's pseudo-accounts): its root and its holding of the
    asset (trust line or MPToken);
  - the owner directories of the submitter, the vault's pseudo-account and the
    broker's pseudo-account;
  - the transaction's CredentialIDs; the Destination's DepositPreauth of the
    submitter.

Only the LoanBroker and the Vault are read as JSON; every other key is derived
locally (Indexes.cpp) and fetch_tx_bundle.py seats the ones that exist.

Usage: XRPL_RPC=<url> loan_broker_bundle.py <tx_hash> <ledger_seq> <out.json>
Read-only; a few dozen sequential requests per transaction.
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
    time.sleep(0.1)
    return json.load(urllib.request.urlopen(req, timeout=30))["result"]


def main():
    tx_hash, seq, out = sys.argv[1].upper(), int(sys.argv[2]), sys.argv[3]
    led = rpc("ledger", {"ledger_index": seq, "transactions": True, "expand": True})
    txs = [t for t in led["ledger"]["transactions"] if t.get("hash", "").startswith(tx_hash)]
    if len(txs) != 1:
        sys.exit(f"{tx_hash} matched {len(txs)} transactions in #{seq}")
    tx = txs[0]
    parent = seq - 1
    keys = [FEE_SETTINGS]

    def entry(params):
        """The object's index and JSON in the parent ledger, or (None, None)."""
        r = rpc("ledger_entry", dict(params, ledger_index=parent))
        if "node" not in r:
            return None, None
        keys.append(r["index"])
        return r["index"], r["node"]

    # The keys of everything else are derived here (Indexes.cpp) rather than
    # asked for one by one: fetch_tx_bundle.py seats each key that exists in
    # the parent ledger and drops the rest, at one request per key.
    def h(space, *parts):
        return hashlib.sha512(bytes([0, ord(space)]) + b"".join(parts)).digest()[:32].hex().upper()

    def acct(addr):
        n = 0
        for ch in addr:
            n = n * 58 + ALPHABET.index(ch)
        raw = n.to_bytes(25, "big")
        return raw[1:21]

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

    def asset_issuer(asset):
        if "mpt_issuance_id" in asset:
            return bytes.fromhex(asset["mpt_issuance_id"])[4:]
        if asset.get("currency") != "XRP":
            return acct(asset["issuer"])
        return None

    def asset_objects(asset):
        if "mpt_issuance_id" in asset:
            keys.append(h("~", bytes.fromhex(asset["mpt_issuance_id"])))
        issuer = asset_issuer(asset)
        if issuer:
            keys.append(h("a", issuer))
            keys.append(h("O", issuer))

    account = tx["Account"]
    root(account)
    owner_dir(account)
    keys.extend(tx.get("CredentialIDs", []))

    broker = None
    broker_id = tx.get("LoanBrokerID")
    if not broker_id and tx["TransactionType"] == "LoanBrokerCoverClawback" and isinstance(tx.get("Amount"), dict):
        # The amount's issuer is the broker's pseudo-account.
        _, maybe_pseudo = entry({"account_root": tx["Amount"].get("issuer", "")})
        broker_id = (maybe_pseudo or {}).get("LoanBrokerID")
    if broker_id:
        _, broker = entry({"index": broker_id})
    vault_id = tx.get("VaultID") or (broker or {}).get("VaultID")
    vault = entry({"index": vault_id})[1] if vault_id else None

    if vault:
        asset = vault["Asset"]
        parties = [account, vault["Account"]]
        if broker:
            parties += [broker["Owner"], broker["Account"]]
        if "Destination" in tx:
            parties.append(tx["Destination"])
        parties = list(dict.fromkeys(parties))
        asset_objects(asset)
        for party in parties:
            root(party)
            owner_dir(party)
            holding(party, asset)
        if "Destination" in tx and tx["Destination"] != account:
            keys.append(h("p", acct(tx["Destination"]), acct(account)))

    keys = list(dict.fromkeys(keys))
    with tempfile.NamedTemporaryFile("w", suffix=".keys", delete=False) as f:
        f.write("\n".join(keys) + "\n")
    env = dict(os.environ, EXTRA_KEYS=f.name, XRPL_RPC=RPC)
    here = os.path.dirname(os.path.abspath(__file__))
    r = subprocess.run([sys.executable, os.path.join(here, "fetch_tx_bundle.py"), tx_hash, str(seq), out], env=env)
    os.unlink(f.name)
    sys.exit(r.returncode)


if __name__ == "__main__":
    main()
