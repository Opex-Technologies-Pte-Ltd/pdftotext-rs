#!/usr/bin/env python3
"""Generate src/data/cjk-cmaps.bin.z: Adobe's predefined CJK encoding CMaps
(code -> CID: Shift-JIS, EUC, GBK, Big5, UHC, ...), from Adobe's cmap-resources
(BSD-3-Clause, https://github.com/adobe-type-tools/cmap-resources).

A composite font may name one of these as its /Encoding instead of
Identity-H. Its show strings are then legacy multi-byte codes; this table turns
them into CIDs, and the collection tables (gen-cid2unicode.py) turn CIDs into
text. The Unicode-keyed CMaps (Uni*) are not included: their codes are already
Unicode. Vertical (-V) CMaps are read through their -H twin.

Usage: gen-cjk-cmaps.py <dir holding the CMap files>

Format (zlib-compressed, little-endian):
  u16 count
  count x { u8 name_len, name, u8 collection (0 Japan1, 1 GB1, 2 CNS1,
            3 Korea1), u32 offset, u32 length }       -- offsets into the body
  body, per CMap:
    u8 n_spaces;  n x { u8 len, u8[4] lo, u8[4] hi }
    u32 n_ranges; n x { u8 len, u32 lo, u32 hi, u32 cid }   sorted by (len, lo)
"""
import re
import struct
import sys
import zlib
from pathlib import Path

PREDEFINED = [
    "83pv-RKSJ-H", "90ms-RKSJ-H", "90msp-RKSJ-H", "90pv-RKSJ-H", "Add-RKSJ-H",
    "EUC-H", "Ext-RKSJ-H", "H",
    "GB-EUC-H", "GBpc-EUC-H", "GBK-EUC-H", "GBKp-EUC-H", "GBK2K-H",
    "B5pc-H", "HKscs-B5-H", "ETen-B5-H", "ETenms-B5-H", "CNS-EUC-H",
    "KSC-EUC-H", "KSCms-UHC-H", "KSCms-UHC-HW-H", "KSCpc-EUC-H",
]

COLLECTION = {"Japan1": 0, "GB1": 1, "CNS1": 2, "Korea1": 3}


def tokens(text):
    return re.findall(r"<[0-9a-fA-F]+>|/[^\s/<>\[\]()]+|\d+|[A-Za-z]+", text)


def parse(path: Path, cache: dict):
    if path.name in cache:
        return cache[path.name]
    text = path.read_text(encoding="latin-1")
    text = re.sub(r"%[^\n]*", "", text)
    ordering = re.search(r"/Ordering\s*\((\w+)\)", text).group(1)
    spaces, ranges = [], {}
    used = re.search(r"/([\w-]+)\s+usecmap", text)
    if used:
        base = parse(path.parent / used.group(1), cache)
        spaces = list(base["spaces"])
        ranges = dict(base["ranges"])
    hexb = lambda t: bytes.fromhex(t.strip("<>"))
    for block in re.findall(r"begincodespacerange(.*?)endcodespacerange", text, re.S):
        t = re.findall(r"<[0-9a-fA-F]+>", block)
        for lo, hi in zip(t[::2], t[1::2]):
            s = (len(hexb(lo)), hexb(lo).ljust(4, b"\0"), hexb(hi).ljust(4, b"\0"))
            if s not in spaces:
                spaces.append(s)
    num = lambda b: int.from_bytes(b, "big")
    for block in re.findall(r"begincidrange(.*?)endcidrange", text, re.S):
        for lo, hi, cid in re.findall(r"(<[0-9a-fA-F]+>)\s*(<[0-9a-fA-F]+>)\s*(\d+)", block):
            lo_b, hi_b = hexb(lo), hexb(hi)
            ranges[(len(lo_b), num(lo_b))] = (num(hi_b), int(cid))
    for block in re.findall(r"begincidchar(.*?)endcidchar", text, re.S):
        for code, cid in re.findall(r"(<[0-9a-fA-F]+>)\s*(\d+)", block):
            b = hexb(code)
            ranges[(len(b), num(b))] = (num(b), int(cid))
    # Later single-code entries (from a V CMap) override part of an inherited
    # range: split ranges so lookups stay a simple binary search.
    out = split_overrides(ranges)
    cache[path.name] = {"ordering": ordering, "spaces": spaces, "ranges": out}
    return cache[path.name]


def split_overrides(ranges: dict) -> dict:
    items = sorted(ranges.items())
    singles = {k: v for k, v in items if k[1] == v[0]}
    result = {}
    for (ln, lo), (hi, cid) in items:
        if lo == hi:
            continue
        start = lo
        for code in range(lo, hi + 1):
            if (ln, code) in singles:
                if start < code:
                    result[(ln, start)] = (code - 1, cid + (start - lo))
                start = code + 1
        if start <= hi:
            result[(ln, start)] = (hi, cid + (start - lo))
    result.update(singles)
    return result


def body(cm) -> bytes:
    b = bytearray([len(cm["spaces"])])
    for ln, lo, hi in cm["spaces"]:
        b += bytes([ln]) + lo + hi
    rs = sorted(cm["ranges"].items())
    b += struct.pack("<I", len(rs))
    for (ln, lo), (hi, cid) in rs:
        b += struct.pack("<BIII", ln, lo, hi, cid)
    return bytes(b)


def main():
    src = Path(sys.argv[1])
    cache = {}
    # The predefined CJK CMaps of ISO 32000-2 Table 118 (the only ones a PDF
    # may name without embedding them), horizontal forms only: a -V CMap
    # differs from its -H twin just in the vertical forms of some punctuation,
    # and for text the horizontal mapping reads the same (and searches
    # better). The decoder maps -V names to -H.
    names = sorted(n for n in PREDEFINED if (src / n).is_file())
    missing = [n for n in PREDEFINED if not (src / n).is_file()]
    if missing:
        sys.exit(f"missing CMaps: {missing}")
    entries, blob = [], bytearray()
    for n in names:
        cm = parse(src / n, cache)
        data = body(cm)
        entries.append((n, COLLECTION[cm["ordering"]], len(blob), len(data)))
        blob += data
    head = bytearray(struct.pack("<H", len(entries)))
    for n, col, off, ln in entries:
        nb = n.encode()
        head += bytes([len(nb)]) + nb + struct.pack("<BII", col, off, ln)
    raw = bytes(head) + bytes(blob)
    dst = Path(__file__).resolve().parent.parent / "src" / "data" / "cjk-cmaps.bin.z"
    dst.write_bytes(zlib.compress(raw, 9))
    print(f"{len(entries)} CMaps, {len(raw)} bytes raw, {dst.stat().st_size} compressed")


if __name__ == "__main__":
    main()
