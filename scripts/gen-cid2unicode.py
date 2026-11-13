#!/usr/bin/env python3
"""Generate src/data/cid2uni/*.bin: CID -> Unicode for the Adobe CJK character
collections, from Adobe's published Unicode -> CID CMaps (cmap-resources,
BSD-3-Clause, https://github.com/adobe-type-tools/cmap-resources).

Used for composite fonts that name an Adobe collection (Adobe-Japan1, GB1,
CNS1, Korea1) in /CIDSystemInfo but carry no /ToUnicode: their CIDs are the
collection's, so the collection's own mapping decodes them.

Usage: gen-cid2unicode.py <dir with the CMaps named in SOURCES>

Output format (little-endian): repeated records of
  u32 first CID, u32 first code point, u16 run length
where CID first+i maps to code point first+i.
"""
import re
import struct
import sys
from pathlib import Path

# Per collection: the main Unicode CMap, then supplements that only fill CIDs
# the main one leaves unmapped (half-width forms reached from Shift-JIS
# single-byte codes, vertical forms, JIS 2004 / X 0213 glyphs).
SOURCES = {
    "Japan1": ["UniJIS-UTF32-H", "UniJIS-UCS2-HW-H", "UniJIS-UTF32-V",
               "UniJIS2004-UTF32-H", "UniJISX0213-UTF32-H"],
    "GB1": ["UniGB-UTF32-H", "UniGB-UTF32-V"],
    "CNS1": ["UniCNS-UTF32-H", "UniCNS-UTF32-V"],
    "Korea1": ["UniKS-UTF32-H", "UniKS-UTF32-V"],
}


def compat(cp: int) -> bool:
    """Compatibility/presentation forms: used only when nothing else maps."""
    return (
        0xF900 <= cp <= 0xFAFF  # CJK compatibility ideographs
        or 0x2F800 <= cp <= 0x2FA1F  # supplement
        or 0x2F00 <= cp <= 0x2FDF  # Kangxi radicals
        or 0x3300 <= cp <= 0x33FF  # CJK compatibility (squared words)
        or 0xFE30 <= cp <= 0xFE4F  # vertical forms
        or 0xE000 <= cp <= 0xF8FF  # private use
    )


def better(new: int, old: int) -> bool:
    return (compat(old), old) > (compat(new), new)


def parse(text: str) -> dict:
    best: dict[int, int] = {}

    def put(cid: int, cp: int) -> None:
        if cid not in best or better(cp, best[cid]):
            best[cid] = cp

    hexnum = lambda s: int(s.strip("<>"), 16)
    for block in re.findall(r"begincidrange(.*?)endcidrange", text, re.S):
        for lo, hi, cid in re.findall(r"(<[0-9a-fA-F]+>)\s*(<[0-9a-fA-F]+>)\s*(\d+)", block):
            lo, hi, cid = hexnum(lo), hexnum(hi), int(cid)
            for k in range(hi - lo + 1):
                put(cid + k, lo + k)
    for block in re.findall(r"begincidchar(.*?)endcidchar", text, re.S):
        for cp, cid in re.findall(r"(<[0-9a-fA-F]+>)\s*(\d+)", block):
            put(int(cid), hexnum(cp))
    return best


def runs(best: dict) -> bytes:
    out = bytearray()
    items = sorted(best.items())
    i = 0
    while i < len(items):
        cid0, cp0 = items[i]
        n = 1
        while (
            i + n < len(items)
            and n < 0xFFFF
            and items[i + n] == (cid0 + n, cp0 + n)
        ):
            n += 1
        out += struct.pack("<IIH", cid0, cp0, n)
        i += n
    return bytes(out)


def main() -> None:
    src = Path(sys.argv[1])
    dst = Path(__file__).resolve().parent.parent / "src" / "data" / "cid2uni"
    dst.mkdir(parents=True, exist_ok=True)
    for ordering, names in SOURCES.items():
        best = {}
        for name in names:
            extra = parse((src / name).read_text(encoding="latin-1"))
            for cid, cp in extra.items():
                best.setdefault(cid, cp)
        data = runs(best)
        (dst / f"{ordering}.bin").write_bytes(data)
        print(f"{ordering}: {len(best)} CIDs, {len(data) // 10} runs, {len(data)} bytes")


if __name__ == "__main__":
    main()
