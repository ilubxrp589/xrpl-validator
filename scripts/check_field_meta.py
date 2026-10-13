#!/usr/bin/env python3
"""Check the field metadata table (independence task 2.1) against mainnet's own records.

Every field mainnet put in an AffectedNodes entry's PreviousFields, FinalFields or NewFields is
checked against crates/xrpl-ledger/src/ledger/field_meta/table.rs, by the rules of rippled 3.4.1's
ApplyStateTable::apply (src/libxrpl/ledger/ApplyStateTable.cpp:160-245):
  - PreviousFields of a ModifiedNode or DeletedNode need kSmdChangeOrig;
  - FinalFields of a ModifiedNode need kSmdAlways or kSmdChangeNew;
  - FinalFields of a DeletedNode need kSmdAlways or kSmdDeleteFinal;
  - NewFields of a CreatedNode need kSmdCreate or kSmdAlways.
A field the table marks kSmdNever must appear in no section, and a field the table doesn't know is
reported as unknown. Every (source, node type, section, field) seen is counted, node-level fields too.

Sources, in any mix:
  --txtree DIR      '<tx_blob>,<meta>' hex fixtures (crates/xrpl-ledger/tests/data/txtree): binary meta
  --expected DIR    l<N>_expected.json from scripts/fetch_ledger_fixture.py, searched recursively, mainnet
                    ones only (header network_id 0). They keep only Created NewFields and Modified
                    FinalFields, and LedgerEntryType there is the fixture script's own addition, so it
                    is skipped
  --ledgers DIR     `ledger` answers with transactions + expand as JSON (scripts/fetch_meta_ledgers.py json/)
  --binledgers DIR  the same with binary:true, tx_blob + meta hex (scripts/fetch_meta_ledgers.py bin/)

Usage (on the build box):
  python3 scripts/check_field_meta.py --table crates/xrpl-ledger/src/ledger/field_meta/table.rs \\
      --txtree crates/xrpl-ledger/tests/data/txtree [--expected DIR ...] [--ledgers DIR] [--binledgers DIR] \\
      [--counts OUT.tsv]
Exit 0 when the table explains every field, 1 when it doesn't, 2 when a source can't be read.
"""
import argparse
import collections
import json
import re
import sys
from pathlib import Path

NODE_KINDS = {(14, 3): "CreatedNode", (14, 4): "DeletedNode", (14, 5): "ModifiedNode"}
SECTIONS = {(14, 6): "PreviousFields", (14, 7): "FinalFields", (14, 8): "NewFields"}
NODE_LEVEL = "(node)"
AFFECTED_NODES = (15, 8)
OBJECT_END, ARRAY_END = 0xE1, 0xF1

# Serialized value lengths by type code (STI_*); 6, 7, 8, 14, 15, 18, 19, 24 and 25 are read by shape.
FIXED = {1: 2, 2: 4, 3: 8, 4: 16, 5: 32, 9: 12, 10: 4, 11: 8, 16: 1, 17: 20, 20: 12, 21: 24, 22: 48, 23: 64,
         26: 20}
VL_TYPES = {7, 8, 19}  # BLOB, ACCOUNT, VECTOR256


def load_table(path):
    """code -> (name, meta), name -> code, and the SMD_* flag values, from the generated table."""
    text = Path(path).read_text()
    flags = {n: int(v, 16) for n, v in re.findall(r"pub const (SMD_\w+): u8 = 0x([0-9a-f]+);", text)}
    rows = re.findall(r'FieldMeta \{ code: (0x[0-9a-f_]+), type_code: \d+, nth: \d+, name: "(\w+)", '
                      r'meta: ([A-Z_ |]+) \}', text)
    by_code, by_name = {}, {}
    for code, name, meta in rows:
        code = int(code.replace("_", ""), 16)
        value = 0
        for term in meta.split("|"):
            value |= flags[term.strip()]
        by_code[code] = (name, value)
        by_name[name] = code
    declared = re.search(r"static FIELDS: \[FieldMeta; (\d+)\]", text)
    if not declared or int(declared.group(1)) != len(rows) or len(by_name) != len(rows):
        sys.exit(f"{path}: read {len(rows)} rows, the table declares {declared and declared.group(1)}")
    return by_code, by_name, flags


class Reader:
    """A cursor over one binary STObject."""

    def __init__(self, data):
        self.b, self.i = data, 0

    def take(self, n):
        if self.i + n > len(self.b):
            raise ValueError(f"truncated at byte {self.i} (wanted {n})")
        out = self.b[self.i:self.i + n]
        self.i += n
        return out

    def byte(self):
        return self.take(1)[0]

    def peek(self):
        if self.i >= len(self.b):
            raise ValueError(f"truncated at byte {self.i}")
        return self.b[self.i]

    def header(self):
        """A field header: (type code, field number)."""
        b = self.byte()
        t, n = b >> 4, b & 0x0F
        if t == 0:
            t = self.byte()
        if n == 0:
            n = self.byte()
        return t, n

    def vl(self):
        b0 = self.byte()
        if b0 <= 192:
            return b0
        if b0 <= 240:
            return 193 + (b0 - 193) * 256 + self.byte()
        if b0 <= 254:
            b1, b2 = self.byte(), self.byte()
            return 12481 + (b0 - 241) * 65536 + b1 * 256 + b2
        raise ValueError(f"bad length prefix {b0:#x} at byte {self.i - 1}")

    def skip_value(self, t):
        if t in FIXED:
            self.take(FIXED[t])
        elif t in VL_TYPES:
            self.take(self.vl())
        elif t == 6:  # AMOUNT: IOU 48 bytes, MPT 33, XRP 8
            b0 = self.peek()
            self.take(48 if b0 & 0x80 else 33 if b0 & 0x20 else 8)
        elif t == 14:
            self.object_fields()
        elif t == 15:
            self.skip_array()
        elif t == 18:
            self.skip_pathset()
        elif t == 24:
            self.skip_issue()
        elif t == 25:  # XCHAIN_BRIDGE: door account, issue, door account, issue
            self.take(self.vl())
            self.skip_issue()
            self.take(self.vl())
            self.skip_issue()
        else:
            raise ValueError(f"type {t} has no known length (byte {self.i})")

    def object_fields(self):
        """The (type, field) of each field of an inner object, read through its end marker."""
        out = []
        while self.peek() != OBJECT_END:
            t, n = self.header()
            out.append((t, n))
            self.skip_value(t)
        self.i += 1
        return out

    def skip_array(self):
        while self.peek() != ARRAY_END:
            t, n = self.header()
            if t != 14:
                raise ValueError(f"an array element of type {t} (byte {self.i})")
            self.object_fields()
        self.i += 1

    def skip_pathset(self):
        while True:
            b = self.byte()
            if b == 0x00:
                return
            if b == 0xFF:
                continue
            if b & ~0x31:
                raise ValueError(f"path step type {b:#x} (byte {self.i - 1})")
            for bit in (0x01, 0x10, 0x20):  # account, currency, issuer
                if b & bit:
                    self.take(20)

    def skip_issue(self):
        if self.take(20) == bytes(20):
            return  # XRP
        if self.take(20) == bytes(19) + b"\x01":
            self.take(4)  # an MPT: issuer, noAccount, sequence


def binary_nodes(meta):
    """(node type, {section or NODE_LEVEL: [(type, field)]}) for each AffectedNodes entry."""
    r = Reader(meta)
    while r.i < len(r.b):
        t, n = r.header()
        if (t, n) != AFFECTED_NODES:
            r.skip_value(t)
            continue
        while r.peek() != ARRAY_END:
            key = r.header()
            kind = NODE_KINDS.get(key)
            if kind is None:
                raise ValueError(f"AffectedNodes holds field {key}")
            parts = {NODE_LEVEL: []}
            while r.peek() != OBJECT_END:
                ft, fn = r.header()
                section = SECTIONS.get((ft, fn))
                if section:
                    parts[section] = r.object_fields()
                else:
                    parts[NODE_LEVEL].append((ft, fn))
                    r.skip_value(ft)
            r.i += 1
            yield kind, parts
        r.i += 1


class Check:
    def __init__(self, by_code, by_name, flags):
        self.by_code, self.by_name, self.flags = by_code, by_name, flags
        self.rules = {
            ("ModifiedNode", "PreviousFields"): flags["SMD_CHANGE_ORIG"],
            ("DeletedNode", "PreviousFields"): flags["SMD_CHANGE_ORIG"],
            ("ModifiedNode", "FinalFields"): flags["SMD_ALWAYS"] | flags["SMD_CHANGE_NEW"],
            ("DeletedNode", "FinalFields"): flags["SMD_ALWAYS"] | flags["SMD_DELETE_FINAL"],
            ("CreatedNode", "NewFields"): flags["SMD_CREATE"] | flags["SMD_ALWAYS"],
        }
        self.counts = collections.Counter()  # (source, node, section, field) -> occurrences
        self.bad = collections.defaultdict(list)  # (node, section, field) -> [where]
        self.unknown = collections.defaultdict(list)  # (node, section, field) -> [where]
        self.totals = collections.Counter()

    def field(self, source, node, section, name, where):
        self.counts[(source, node, section, name)] += 1
        if section == NODE_LEVEL:
            return
        code = self.by_name.get(name)
        if code is None:
            self.unknown[(node, section, name)].append(where)
        elif not self.by_code[code][1] & self.rules.get((node, section), 0):
            self.bad[(node, section, name)].append(where)

    def binary(self, source, meta, where):
        for kind, parts in binary_nodes(meta):
            self.totals[f"{source} {kind}"] += 1
            for section, fields in parts.items():
                for t, n in fields:
                    entry = self.by_code.get((t << 16) | n)
                    self.field(source, kind, section, entry[0] if entry else f"?{t},{n}", where)

    def json_node(self, source, wrapped, where):
        (kind, inner), = wrapped.items()
        self.totals[f"{source} {kind}"] += 1
        for key, value in inner.items():
            if key in ("PreviousFields", "FinalFields", "NewFields"):
                for name in value:
                    self.field(source, kind, key, name, where)
            else:
                self.field(source, kind, NODE_LEVEL, key, where)


def read_txtree(check, folder):
    for path in sorted(Path(folder).glob("*.txt")):
        lines = path.read_text().split()
        check.totals["txtree ledgers"] += 1
        for k, line in enumerate(lines[1:]):
            check.totals["txtree txs"] += 1
            check.binary("txtree", bytes.fromhex(line.split(",")[1]), f"txtree {path.stem} tx {k}")


def read_expected(check, folder):
    for path in sorted(Path(folder).rglob("l*_expected.json")):
        data = json.loads(path.read_text())
        if data.get("header", {}).get("network_id", 0) != 0:
            check.totals["fixture ledgers not mainnet, skipped"] += 1
            continue
        check.totals["fixture ledgers"] += 1
        for h, tx in data.get("txs", {}).items():
            check.totals["fixture txs"] += 1
            for _key, kind, post in tx.get("nodes", []):
                if kind == 2 or post is None:
                    continue
                node, section = ("CreatedNode", "NewFields") if kind == 0 else ("ModifiedNode", "FinalFields")
                check.totals[f"fixture {node}"] += 1
                for name in post:
                    if name != "LedgerEntryType":
                        check.field("fixture", node, section, name, f"{path.name} {h[:12]}")


def ledger_of(path):
    data = json.loads(path.read_text())
    return data.get("result", data).get("ledger", {})


def read_ledgers(check, folder):
    for path in sorted(Path(folder).glob("*.json")):
        check.totals["s2-json ledgers"] += 1
        for tx in ledger_of(path).get("transactions", []):
            meta = tx.get("metaData") or tx.get("meta")
            if not isinstance(meta, dict):
                raise ValueError(f"{path.name}: a transaction without JSON metadata")
            check.totals["s2-json txs"] += 1
            where = f"{path.stem} {tx.get('hash', '?')[:12]}"
            for wrapped in meta.get("AffectedNodes", []):
                check.json_node("s2-json", wrapped, where)


def read_binledgers(check, folder):
    for path in sorted(Path(folder).glob("*.json")):
        check.totals["s2-bin ledgers"] += 1
        for k, tx in enumerate(ledger_of(path).get("transactions", [])):
            meta = tx.get("meta") or tx.get("metaData")
            if not isinstance(meta, str):
                raise ValueError(f"{path.name}: a transaction without binary metadata")
            check.totals["s2-bin txs"] += 1
            check.binary("s2-bin", bytes.fromhex(meta), f"{path.stem} tx {k}")


def report(check, counts_path):
    print("read: " + ", ".join(f"{k} {v}" for k, v in sorted(check.totals.items())))
    occurrences = collections.Counter()
    distinct = collections.defaultdict(set)
    for (source, node, section, name), n in check.counts.items():
        occurrences[(source, node, section)] += n
        distinct[(source, node, section)].add(name)
    for key in sorted(occurrences):
        print(f"  {key[0]:8} {key[1]:13} {key[2]:15} {len(distinct[key]):4} distinct fields, "
              f"{occurrences[key]:9} occurrences")
    never = check.flags["SMD_NEVER"]
    never_seen = [k for k in check.bad if check.by_code[check.by_name[k[2]]][1] == never]
    risky = [k for k in check.bad if (k[0], k[1]) in (("DeletedNode", "FinalFields"), ("CreatedNode", "NewFields"))]
    print(f"not allowed by the table: {len(check.bad)} (node, section, field) kinds, "
          f"{sum(len(v) for v in check.bad.values())} occurrences")
    for key, where in sorted(check.bad.items()):
        print(f"  {key[0]} {key[1]} {key[2]}: {len(where)}x, first {where[0]}")
    print(f"(a) kSmdNever fields seen in a section: {len(never_seen)} {sorted(never_seen)}")
    print(f"(b) Deleted FinalFields / Created NewFields the table doesn't allow: {len(risky)} {sorted(risky)}")
    print(f"unknown to the table: {len(check.unknown)}")
    for key, where in sorted(check.unknown.items()):
        print(f"  {key[0]} {key[1]} {key[2]}: {len(where)}x, first {where[0]}")
    if counts_path:
        with open(counts_path, "w") as f:
            f.write("source\tnode\tsection\tfield\tcode\tmeta\tallowed\tcount\n")
            for (source, node, section, name), n in sorted(check.counts.items()):
                code = check.by_name.get(name)
                meta = check.by_code[code][1] if code is not None else None
                if section == NODE_LEVEL:
                    allowed = "-"
                elif code is None:
                    allowed = "unknown"
                else:
                    allowed = "yes" if meta & check.rules.get((node, section), 0) else "NO"
                f.write(f"{source}\t{node}\t{section}\t{name}\t{'' if code is None else f'{code:#010x}'}\t"
                        f"{'' if meta is None else f'{meta:#04x}'}\t{allowed}\t{n}\n")
        print(f"counts: {counts_path}")
    return 0 if not check.bad and not check.unknown else 1


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--table", required=True)
    p.add_argument("--txtree", action="append", default=[])
    p.add_argument("--expected", action="append", default=[])
    p.add_argument("--ledgers", action="append", default=[])
    p.add_argument("--binledgers", action="append", default=[])
    p.add_argument("--counts")
    a = p.parse_args()
    check = Check(*load_table(a.table))
    try:
        for folder in a.txtree:
            read_txtree(check, folder)
        for folder in a.expected:
            read_expected(check, folder)
        for folder in a.ledgers:
            read_ledgers(check, folder)
        for folder in a.binledgers:
            read_binledgers(check, folder)
    except (ValueError, KeyError, json.JSONDecodeError) as e:
        print(f"cannot read a source: {e}", file=sys.stderr)
        return 2
    return report(check, a.counts)


if __name__ == "__main__":
    sys.exit(main())
