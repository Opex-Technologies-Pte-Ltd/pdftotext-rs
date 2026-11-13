# Third-party material

This crate is licensed under Apache-2.0 (see `LICENSE`). It includes or depends on the following, each under its own
terms:

| What | Where | License |
|---|---|---|
| Adobe Glyph List | `src/data/glyphlist.txt` | BSD-3-Clause (header kept in the file) |
| Adobe CMap resources: Japan1, GB1, CNS1, Korea1 Unicode mappings converted to CID → Unicode tables by `scripts/gen-cid2unicode.py`, and the ISO 32000 predefined CJK encoding CMaps converted by `scripts/gen-cjk-cmaps.py` | `src/data/cid2uni/*.bin`, `src/data/cjk-cmaps.bin.z` | BSD-3-Clause, `src/data/cid2uni/LICENSE-adobe-cmap-resources.md` |
| lopdf 0.44.0, with a patch to its encrypted-document loader | `vendor/lopdf/` | MIT, `vendor/lopdf/LICENSE` |
| fax (CCITT G3/G4 decoder) | crates.io | MIT |
| hayro-jbig2 (JBIG2 decoder) and its hayro-ccitt dependency (MMR) | crates.io | Apache-2.0 OR MIT |
| flate2, crc32fast, wasm-bindgen, serde and their dependencies | crates.io | MIT and/or Apache-2.0 |

The test corpus (`corpus/manuals.tsv`) lists public manuals by URL and
checksum; the PDFs belong to their publishers and are not included.
