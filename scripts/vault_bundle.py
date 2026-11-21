#!/usr/bin/env python3
"""vault_bundle.py — a byte-exact vector bundle for one vault transaction.

A thin driver over fetch_tx_bundle.py (same bundle format, same harness: see
crates/xrpl-node/tests/vault_vector.rs). fetch_tx_bundle.py seats what the
transaction's metadata names; a vault transaction also READS objects its
metadata never shows, and a failed one (tec) shows nothing but the fee. This
driver works that read-set out from the transaction and the vault as it stood
in the parent ledger, asks the server for each object's index (ledger_entry by
its natural key, so no keylet is re-derived here), and hands the list to
fetch_tx_bundle.py as EXTRA_KEYS:

  - FeeSettings (the reserves; devnet's are not mainnet's);
  - the Vault, its pseudo-account's root, the share MPTokenIssuance;
  - the asset: its MPTokenIssuance, or its issuer's root;
  - for each party (the submitter, the Destination, the Holder, the vault's
    owner and pseudo-account): its root, its holding of the asset (trust line
    or MPToken) and its MPToken of the share;
  - the share's permissioned domain and each party's credential for every
    credential the domain accepts; the transaction's own CredentialIDs; the
    Destination's DepositPreauth of the submitter;
  - VaultCreate: the asset's objects, the owner's holding and the DomainID.

Usage: XRPL_RPC=<url> vault_bundle.py <tx_hash> <ledger_seq> <out.json>
Read-only; a few dozen sequential requests per transaction.
"""
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

RPC = os.environ.get("XRPL_RPC", "http://127.0.0.1:5005/")
FEE_SETTINGS = "4BC50C9B0D8515D3EAAE1E74B29A95804346C491EE1A95BF25E4AAB854A6A651"


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

    def holding(account, asset):
        if "mpt_issuance_id" in asset:
            entry({"mptoken": {"mpt_issuance_id": asset["mpt_issuance_id"], "account": account}})
        elif asset.get("currency") != "XRP" and account != asset["issuer"]:
            entry({"ripple_state": {"accounts": [account, asset["issuer"]], "currency": asset["currency"]}})

    def asset_objects(asset):
        if "mpt_issuance_id" in asset:
            entry({"mpt_issuance": asset["mpt_issuance_id"]})
            # The MPT's issuer: the account half of the id, which the issuance names.
            _, issuance = entry({"mpt_issuance": asset["mpt_issuance_id"]})
            if issuance:
                entry({"account_root": issuance["Issuer"]})
        elif asset.get("currency") != "XRP":
            entry({"account_root": asset["issuer"]})

    def domain_objects(domain_id, parties):
        _, domain = entry({"index": domain_id})
        if not domain:
            return
        for accepted in domain.get("AcceptedCredentials", []):
            c = accepted["Credential"]
            for party in parties:
                entry({"credential": {"subject": party, "issuer": c["Issuer"], "credential_type": c["CredentialType"]}})

    account = tx["Account"]
    entry({"account_root": account})
    for cred in tx.get("CredentialIDs", []):
        entry({"index": cred})

    if tx["TransactionType"] == "VaultCreate":
        asset_objects(tx["Asset"])
        holding(account, tx["Asset"])
        if "DomainID" in tx:
            entry({"index": tx["DomainID"]})
    else:
        _, vault = entry({"index": tx["VaultID"]})
        if vault:
            asset, share = vault["Asset"], {"mpt_issuance_id": vault["ShareMPTID"]}
            parties = [account, vault["Owner"], vault["Account"]]
            for field in ("Destination", "Holder"):
                if field in tx:
                    parties.append(tx[field])
            parties = list(dict.fromkeys(parties))
            asset_objects(asset)
            _, share_issuance = entry({"mpt_issuance": vault["ShareMPTID"]})
            for party in parties:
                entry({"account_root": party})
                holding(party, asset)
                holding(party, share)
            if share_issuance and "DomainID" in share_issuance:
                domain_objects(share_issuance["DomainID"], [p for p in parties if p != vault["Account"]])
            if "DomainID" in tx and tx["DomainID"].strip("0"):
                entry({"index": tx["DomainID"]})
            if "Destination" in tx and tx["Destination"] != account:
                entry({"deposit_preauth": {"owner": tx["Destination"], "authorized": account}})

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
