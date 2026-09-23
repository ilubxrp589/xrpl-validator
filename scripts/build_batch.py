#!/usr/bin/env python3
"""build_batch.py <outer_hash> <ledger> <outdir>  — campaign 23's Batch bundle builder.

Campaign 8's recipe (fetch the OUTER's bundle and each FILED inner's bundle with fetch_tx_bundle.py,
merge them in apply order: pre = first occurrence, expect = last occurrence) plus three repairs the
campaign-23 shapes need:

 1. MID-BATCH CREATIONS leave pre. A key CREATED by the outer or an earlier inner and touched again
    by a later inner reaches that later inner's bundle as a rebuilt mid-batch pre-image; "first
    occurrence wins" then seats it as if it existed BEFORE the batch (MPTokenIssuance created by
    inner 1 and paid by inner 3, the Delegate created by a DelegateSet inner, a Ticket created and
    used). Rule: K created by entry j and absent from the pre of every bundle i <= j did not exist
    before the batch -> dropped from pre.
 2. NON-FILED INNERS get their READ-SET. rippled files an inner only when it was applied (tes or a
    claimed tec); an inner refused per-inner (ter/tef: sequence, ticket, permission, prior) or
    discarded by tfAllOrNothing has no metadata, so no bundle names what it reads — and an engine
    that wrongly APPLIES it would then fail for want of the account root and look right by accident.
    Each such inner is hydrated through fetch_tx_bundle.py's SYNTH_TX mode at the outer's
    TransactionIndex (pre-batch images) and merged with setdefault.
 3. UNTOUCHED PINS. Every hydrated key no bundle's expect names was left alone by the ledger; it is
    pinned expect = pre, so an engine that writes it (e.g. applies a refused inner) is caught.

Extra keys for a batch-aware harness: raw_inner_hashes (every RawTransactions id, in order),
filed_results {id: TransactionResult}, mode. inner_hashes / inner_results are merge_batch_bundle.py's
(filed inners only)."""
import hashlib
import json
import os
import subprocess
import sys
import urllib.request

from xrpl.core.binarycodec import encode

RPC = os.environ.get('XRPL_RPC', 'https://s.devnet.rippletest.net:51234')
FETCH = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'fetch_tx_bundle.py')
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MODE_NAMES = {0x00010000: 'allornothing', 0x00020000: 'onlyone', 0x00040000: 'untilfailure', 0x00080000: 'independent'}


def rpc(m, p):
    r = urllib.request.Request(RPC, data=json.dumps({'method': m, 'params': [p]}).encode(), headers={'Content-Type': 'application/json'})
    return json.load(urllib.request.urlopen(r, timeout=90))['result']


def txid_of(d):
    return hashlib.sha512(bytes.fromhex('54584E00' + encode(d))).digest()[:32].hex().upper()


def fetch(h, seq, out, env_extra=None):
    if os.path.exists(out):
        return True
    env = dict(os.environ, XRPL_RPC=RPC)
    if env_extra:
        env.update(env_extra)
    for attempt in range(2):
        with open(out + '.log', 'w') as lf:
            # the fetcher is stdlib-only; /usr/bin/python3 as the brief prescribes
            subprocess.run(['/usr/bin/python3', FETCH, h[:12] if not env_extra else 'SYNTH', str(seq), out],
                           cwd=REPO, env=env, stdout=lf, stderr=subprocess.STDOUT, timeout=900)
        if os.path.exists(out):
            return True
    return False


def main():
    outer_hash, seq, outdir = sys.argv[1].upper(), int(sys.argv[2]), os.path.abspath(sys.argv[3])
    os.makedirs(outdir, exist_ok=True)
    led = rpc('ledger', {'ledger_index': seq, 'transactions': True, 'expand': True})['ledger']
    txs = led['transactions']
    outer = next(t for t in txs if t.get('hash', '').upper() == outer_hash)
    om = outer.get('metaData') or outer.get('meta')
    outer_index = om['TransactionIndex']
    filed = []
    for t in txs:
        m = t.get('metaData') or t.get('meta') or {}
        if (m.get('ParentBatchID') or '').upper() == outer_hash:
            filed.append((m['TransactionIndex'], t['hash'].upper(), m['TransactionResult'], m))
    filed.sort()
    raws = [e['RawTransaction'] for e in outer.get('RawTransactions', [])]
    raw_ids = [txid_of(r) for r in raws]
    filed_ids = {h for _, h, _, _ in filed}
    order = [h for _, h, _, _ in filed]
    # RawTransactions order must agree with the ledger's filing order
    pos = [raw_ids.index(h) for h in order if h in raw_ids]
    assert pos == sorted(pos), f'filed order {pos} is not RawTransactions order'
    assert all(h in raw_ids for h in order), 'a filed inner is not among the RawTransactions ids'

    # 1. bundles for the outer and each filed inner, in apply order
    paths = [os.path.join(outdir, f'{outer_hash[:12]}.json')]
    ok = fetch(outer_hash, seq, paths[0])
    for h in order:
        p = os.path.join(outdir, f'{h[:12]}.json')
        ok = fetch(h, seq, p) and ok
        paths.append(p)
    if not ok:
        missing = [p for p in paths if not os.path.exists(p)]
        print(f'BUNDLE-FAIL {outer_hash[:12]}: {len(missing)} of {len(paths)} bundles missing: {[os.path.basename(m) for m in missing]}')
        sys.exit(2)
    bundles = [json.load(open(p)) for p in paths]
    metas = [om] + [m for _, _, _, m in filed]

    # merge (merge_batch_bundle.py's rules)
    ob = bundles[0]
    merged = {k: ob[k] for k in ('seq', 'parent_close_time', 'total_coins', 'parent_hash', 'tx', 'result')}
    pre, expect = dict(ob.get('pre', {})), dict(ob.get('expect', {}))
    for b in bundles[1:]:
        assert b['seq'] == ob['seq']
        for k, v in b.get('pre', {}).items():
            pre.setdefault(k, v)
        for k, v in b.get('expect', {}).items():
            expect[k] = v

    # repair 1: keys created inside the batch before any bundle saw them pre-batch
    dropped = []
    created_at = {}
    for j, m in enumerate(metas):
        for n in m.get('AffectedNodes', []):
            if 'CreatedNode' in n:
                created_at.setdefault(n['CreatedNode']['LedgerIndex'].upper(), j)
    for k, j in created_at.items():
        if k in pre and not any(k in bundles[i].get('pre', {}) for i in range(0, j + 1)):
            del pre[k]
            dropped.append(k)

    # repair 1b: a key that did not exist before the batch and whose LAST toucher deleted it is no
    # entry at all (created by one inner and deleted by a later one: a Ticket made and then used) —
    # native_apply::fold_batch_mutset's rule ("a key Created and then Deleted inside the batch is no
    # entry at all"). Left in, the probe demands a deletion of a key the batch never had to write.
    net_none = [k for k, v in expect.items() if v == '' and k not in pre]
    for k in net_none:
        del expect[k]

    # repair 2: read-sets of the inners the ledger did not file
    synth_n = 0
    nonfiled = [(i, r) for i, r in enumerate(raws) if raw_ids[i] not in filed_ids]
    for i, r in nonfiled:
        sp = os.path.join(outdir, f'synth_{i}_{raw_ids[i][:12]}.json')
        sj = os.path.join(outdir, f'synth_{i}_{raw_ids[i][:12]}.tx.json')
        rr = dict(r)
        rr['hash'] = raw_ids[i]
        json.dump(rr, open(sj, 'w'))
        if fetch(raw_ids[i], seq, sp, {'SYNTH_TX': sj, 'SYNTH_INDEX': str(outer_index)}):
            sb = json.load(open(sp))
            for k, v in sb.get('pre', {}).items():
                if k in created_at:
                    continue  # does not exist before the batch
                if k not in pre:
                    pre[k] = v
                    synth_n += 1
        else:
            print(f'WARN synth fetch failed for inner {i} {raw_ids[i][:12]}')

    # repair 3: untouched pins
    pins = 0
    for k, v in pre.items():
        if k not in expect:
            expect[k] = v
            pins += 1

    merged['pre'], merged['expect'] = pre, expect
    merged['inner_hashes'] = [b['tx']['hash'] for b in bundles[1:]]
    merged['inner_results'] = [b['result'] for b in bundles[1:]]
    merged['raw_inner_hashes'] = raw_ids
    merged['filed_results'] = {h: res for _, h, res, _ in filed}
    merged['mode'] = MODE_NAMES.get(int(outer.get('Flags', 0)) & 0x000F0000, '?')
    merged['c23_notes'] = {'dropped_mid_batch_creations': dropped, 'created_then_deleted': net_none, 'synth_keys_added': synth_n, 'untouched_pins': pins,
                           'nonfiled_inners': [i for i, _ in nonfiled]}
    out = os.path.join(outdir, 'bundle.json')
    json.dump(merged, open(out, 'w'))
    print(f'{outer_hash[:12]}@{seq} {merged["mode"]}: raw={len(raws)} filed={len(filed)} nonfiled={len(nonfiled)} '
          f'pre={len(pre)} expect={len(expect)} dropped={len(dropped)} synth+={synth_n} pins={pins}')


if __name__ == '__main__':
    main()
