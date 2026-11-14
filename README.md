# pdftotext-rs

A clean-room PDF text extractor in Rust, built from the PDF specification and
tested on real equipment manuals. It is a drop-in for poppler's `pdftotext`
command line, a Rust library, and a WebAssembly module small enough to run
inside a Cloudflare Worker or a browser tab.

It exists because maintenance software has to read machine manuals, and
manuals are where ordinary extractors fail. It was built by Opex Technologies
for [OpexMX](https://mx.opexcg.com), a maintenance management system whose
assistant answers technicians' questions from their equipment manuals.
Licensed Apache-2.0, with no GPL code and no shared libraries.

**Try it in your browser:** the [Manual Text Checker](https://mx.opexcg.com/tools/manual-text-checker)
runs this extractor (compiled to WebAssembly) next to pdf.js on a PDF you drop
in. The file never leaves your machine.

## Why equipment manuals break ordinary extractors

A manual looks like text on screen, but the PDF underneath is made for
printing, not for reading back. Pull the text out in the order it is stored and
you get a scrambled result; a search index or an AI assistant fed from it
answers from garbage. These are the patterns we found in 22 public manuals from
drive, pump, compressor, gearbox, bearing and chiller makers. Each is now a
regression test in `tests/robustness.rs`:

| What the PDF does | What a plain extractor prints | What this one prints |
|---|---|---|
| Japanese/Chinese fonts embedded without a Unicode table | nothing, or `□□□□` | `第 2 章 基本操作` (from Adobe's published character collections) |
| A font built from Arial with no character table at all | `7KLVZD\XS )UDJLOH` (every letter shifted by 3) | `This way up Fragile` |
| Every glyph is its own positioned string (DTP exports) | `T h e s t r u c t u r e` | `The structure` |
| Word gaps that exist only as kerning or letter spacing | `damage,etc.mayoccur.` | `damage, etc. may occur.` |
| A letter-spaced caption | `f i g u r e 1 : 1 9` | `figure 1:19.` |
| Justified text whose spaces are squeezed | `isdisplayed` | `is displayed` |
| A stray space glyph drawn inside a ligature | `diff erent` | `different` |
| Ligatures named `/f_i`, `/f_f_l` | `eldbus`, `lter` | `fieldbus`, `filter` |
| Text overprinted for a bold effect | `MorMore infe informaormationtion` | `More information` |
| An old label covered by a white box, a new one drawn on top | `BGenerearing lifal bee aaring` | `General bearing knowledge` |
| A label sitting on a rule of underscores | `_in_str_uc_ti_on_s___1_` | the label, and the rule on its own line |
| The printer's slug outside the page | `CAM_edition9.indd 8 28/01/19 18:07` on every page | (dropped) |
| A Symbol-font Ω in a parameter table | `0–50W/9999` | `0–50Ω/9999` |
| A broken Unicode table mapping digits to control codes | `1.^X 9733^X462` | `1.3 97333462` |
| Subscripts on their own line | `t - t` / `E2 E1` | `tE2 - tE1` |
| Arabic in a multilingual manual | reversed | logical order |
| Scanned pages | nothing | nothing: detect them and send them to OCR |

### How it compares

Word recall on the 22 manuals (5,206 pages), measured against poppler's
output and against the consensus of five extractors, on a contributor's own
machine (see the clean-room rules: no other engine is used as a test oracle):

| | this | poppler | pdf.js (unpdf) | pypdf | pdfminer.six |
|---|---|---|---|---|---|
| Typical manual | 98–100% | reference | 96–100% | 81–100% | 83–100% |
| Japanese drive manual (560 pages) | 99% | 100% | 0% | 3% | timed out |
| Glyph garbage, worst manual | 0.24% | 2.2% | 14.6% | 25.6% | 3.9% |
| Time, relative to this one | 1× | 1.3–3.7× | 0.1–2× (varies by file) | median 4×, up to 12× | median 8×, up to 29× |

The remaining gap to poppler is mostly deliberate: print slugs and covered
text that poppler prints and this extractor drops, and doubled hyphens read as
one. The manuals are listed with URLs and checksums in `corpus/manuals.tsv`;
`scripts/fetch-corpus.sh` downloads and verifies them, and
`PDFTOTEXT_CORPUS_DIR=corpus/pdfs cargo test --test parity -- --nocapture`
checks every page for glyph garbage.

## Use it

| | Install | Use |
|---|---|---|
| Command line | `cargo install pdftotext-rs` | `pdftotext manual.pdf -` |
| Rust | `cargo add pdftotext-rs` | `use pdftotext::Document;` |
| JavaScript / TypeScript | `npm i pdftotext-rs` | `import init, { extract_pdf } from 'pdftotext-rs'` |

### Command line

The binary is called `pdftotext`, so it can stand in for poppler's.

```
pdftotext [-layout|-raw] [-enc UTF-8] [-f N] [-l N] [-nopgbrk] [-q] <file.pdf> [out.txt|-]
pdftotext -layout -enc UTF-8 manual.pdf -      # pages separated by \f
```

Three arrangements, as in poppler:

- **default: reading order.** Lines top to bottom; where a page has columns of
  running text, each column is read in turn, with full-width headings and
  captions kept in place. Tables stay in rows (a gutter is only treated as a
  column break when both sides behave like running text, not like table
  cells). The best input for search and for chunking text for an AI model.
- **`-layout`: physical layout.** Lines top to bottom, each piece of text near
  the column matching its position, so tables and side-by-side columns stay
  visibly apart.
- **`-raw`: content-stream order**, with spaces and line breaks placed by
  position. Exit codes: `0` success, `2` failure, `3`
encrypted (needs a password), `4` refused as a resource bomb. A page range
outside the document is an error.

### Rust

The package is `pdftotext-rs`; the library is imported as `pdftotext`.

```rust
let doc = pdftotext::Document::parse(&std::fs::read("manual.pdf")?)?;
for page in 0..doc.page_count() {
    let text = doc.extract_text(page, true)?; // true = layout
}
```

### JavaScript / TypeScript (WebAssembly)

```bash
npm i pdftotext-rs
```

```js
import init, { extract_pdf } from 'pdftotext-rs';
await init(); // loads the .wasm once; a no-op in Node
const { pageCount, pages } = extract_pdf(new Uint8Array(buf), false); // false = reading order, true = layout
// pages: [{ pageNumber, text, charCount, sectionHeaders }]
```

The same import works in Node, Bun, Deno, browsers and bundlers. In a
Cloudflare Worker, pass the module in: `import wasm from
'pdftotext-rs/pdftotext_bg.wasm'; await init({ module_or_path: wasm });`.
No native dependencies and no network calls. To build it yourself:
`wasm-pack build --target web --release`, or `scripts/build-npm.sh` for the
npm package.

For scanned pages, `page_images(bytes, pages, budget)` lists each page's
images with size, placement and page coverage, and returns JPEG and JPEG 2000
images as their stored bytes (no decoding: a vision/OCR model takes the JPEG
as is), and decodes bilevel scans to a PNG: fax-style CCITT (a 300-dpi page is
about 70 KB) and JBIG2, with its `/JBIG2Globals`. Scans up to 4000 px wide stay
1-bit; wider ones (a 600-dpi page) are shrunk by a whole factor to at most
4000 px, averaging pixels into 8-bit gray, so they stay within the image limits
of vision APIs (a 600-dpi Letter page comes out 2582 x 3321, about 230 KB).
`width`/`height` are the stored image's; read the PNG header for its own size.
Flate images are reported with metadata only.

Errors are thrown as strings starting with `not_pdf:`, `read:`,
`unsupported:`, `encrypted:` or `limit:`. A panic traps the instance; create a
new one after a `RuntimeError`. The release module is about 0.5 MB gzipped
(0.25 MB of it is the CJK tables).

## What it handles

- **Positioning:** the full text and graphics state (`q Q cm Tm Td TD T* TL Tc
  Tw Tz Ts`), glyph widths (`/Widths`, `/MissingWidth`, CID `/DW` + `/W`,
  Type 3 widths through `/FontMatrix`) and `TJ` kerning, so every piece of text
  has a real start and end on the page. Word breaks come from gaps measured
  from where the ink ends; a literal space glyph always breaks, except in
  letter-spaced text, where only a clearly wider gap does.
- **Fonts and encodings:** WinAnsi, MacRoman, Standard and Symbol encodings
  per ISO 32000 Annex D; `/Differences` with the full Adobe Glyph List,
  `uniXXXX`, `c<code>` and the Latin ligature names; `ToUnicode` read code by
  code with the encoding as fallback and code lengths from its
  `codespacerange`; composite fonts through `ToUnicode`, Unicode-keyed CMaps,
  the Adobe Japan1, GB1, CNS1 and Korea1 collections (also behind the legacy
  Shift-JIS, EUC, GBK, Big5 and UHC encodings of ISO 32000 Table 118, with
  widths looked up by CID), or (for `Identity`
  fonts built from standard-order Windows fonts, confirmed by their widths)
  ASCII by glyph ID.
- **Page content:** Form XObjects inline with their matrices (cycles cut,
  nesting and fan-out bounded); text outside the page's crop box dropped;
  text hidden under a later opaque fill dropped (transparency respected);
  duplicate overprints read once; overlapping lines on different baselines
  kept apart; sub- and superscripts joined to their line.
- **Right-to-left text:** lines reordered from visual to logical order, with
  brackets mirrored and embedded numbers kept left to right.
- **Section headers** (larger-than-body text, joined across kerned pieces) for
  chunking.
- **Reading order** (the default mode): column gutters found by an XY-cut over
  the page's text segments, accepted only between blocks of running text —
  both sides wide and comparable, not made of cells, and not breaking in step
  like table rows.

## Robustness (untrusted input)

It is built to run on user uploads, where a panic or an unbounded loop is a
denial of service. All bounds live in the crate:

| Concern | Guard |
|---|---|
| Decompression bomb (content, forms) | `FlateDecode` inflated under a 64 MiB per-stream cap; page content capped at 64 MiB |
| Decompression bomb (object/xref streams) | 64 MiB per stream at load → `limit:` |
| Total work per document | 1 GiB of decoded content across all pages and forms → `limit:` |
| Memory held per document | Image and font-file payloads, annotations, link targets and name trees, bookmarks, structure trees, shadings and functions are dropped while parsing (also for encrypted files, via a patched lopdf) |
| Form XObject cycles / fan-out | cycle cut, depth ≤ 16, ≤ 50 000 invocations per page |
| Page memory | ≤ 200 000 runs / 8 MiB of text per page |
| Operand and graphics-state stacks | ≤ 65 536 operands, `q` depth ≤ 256 |
| Numeric hostility | NaN/inf rejected; matrices beyond ±1e9 ignored; sorts use `total_cmp` |
| Lexer recursion | none |
| ToUnicode CMaps | ≤ 65 536 codes per range, ≤ 524 288 range expansions and ≤ 262 144 entries per CMap |
| Repeated font parsing | each font resolved once per document, including fonts written inline in a form |
| Fills checked for hidden text | ≤ 2 000 per page |
| Page tree and resource cycles | bounded walks with visited sets |
| Image listing (`page_images`) | ≤ 64 images per page, form depth ≤ 8, ≤ 512 form invocations and 64 MiB of form content per page, ≤ 4 000 000 operators per page; encoded bytes returned within a caller-set budget; CCITT / JBIG2 decoded up to 120 Mpixels (JBIG2 headers checked first: regions ≤ 2 × that in total, ≤ 2^18 symbols, ≤ 2^22 placed glyphs, ≤ 2^22 halftone cells) |
| WebAssembly memory | the module's linear memory is capped at 80 MiB; an allocation past it fails inside the module (reported by `alloc_failed()`), so a host can recover instead of being killed |
| Encryption | owner-password-only files open; anything needing a password → `encrypted:` (never ciphertext as text) |

A mutation-fuzz test runs 400 corrupted variants per `cargo test`. The release
profile is `panic = "abort"`. CPU time in WebAssembly is bounded by the work
caps above, not by a clock; native callers should add a wall-clock limit.

## Known limitations

- **Scanned pages** have no text layer and yield nothing. Detect them (no or
  very little text) and send their images to OCR (`page_images`). JBIG2
  images that declare more than 120 Mpixels per page, 2^18 symbols or
  2^22 placed glyphs are listed but not decoded.
- **Reading order** is detected from whitespace: columns of running text are
  split, tables are kept in rows. Layouts that defeat both (text wrapped
  around figures, a table whose cells are paragraphs on a shared baseline
  grid) can still come out in the wrong order; `-layout` always shows the
  physical arrangement. Page `/Rotate` is not applied.
- **CID fonts without any Unicode information** (an `Identity` ordering from
  a font not known to use the standard glyph order, with no `ToUnicode`) yield
  no text rather than wrong text. A CMap embedded in the PDF as a stream (not
  one of the predefined names) is read as 2-byte codes, and only `ToUnicode`
  can give its text.
- **Vertical writing** (`Identity-V`, `/W2`) is decoded but laid out
  horizontally.
- **Right-to-left support** is a minimal reordering, not the full Unicode
  bidi algorithm; Arabic presentation forms are kept as drawn.
- **Hidden-text detection** looks at opaque rectangles only; text covered by
  an image or an arbitrary shape is still extracted.
- **Content filters:** Flate, LZW and ASCII85 (and chains of them).

## Clean-room rules (for contributors)

This crate is a clean-room implementation; its license depends on it not being
a derivative of poppler, pdfium, MuPDF, xpdf or any other PDF engine.

**Forbidden** for every contributor (human or agent) writing code under
`src/`:

- Reading the **source code** of poppler, pdfium, MuPDF, xpdf, Ghostscript,
  pdf.js or any PDF engine whose output you might reproduce.
- Using another engine's **expected-text files** as a test oracle. Expected
  output in this repository comes from this extractor, checked by a person.
- Committing their **test PDFs**. Fixtures here are synthesized from the spec;
  real-world manuals are referenced by URL in `corpus/manuals.tsv`, never
  committed.

**Allowed:** ISO 32000-2; Adobe's technical notes (Type 1, CMap and CIDFont
specifications, OpenType); the Adobe Glyph List and Adobe CMap resources
(BSD-3-Clause); `lopdf` and `flate2` for object and stream primitives.

Another engine's output may be looked at on your own machine to judge whether
a page reads well, but never wired into an automated assertion and never
copied as expected output.

## Build

```bash
cargo build --release                                   # native CLI
cargo zigbuild --release --target x86_64-unknown-linux-musl   # static Linux binary
wasm-pack build --target web --release                  # WebAssembly
cargo test                                              # 100+ tests incl. fuzz
python3 scripts/gen-cid2unicode.py <cmap-resources dir> # regenerate CJK CID->Unicode tables
python3 scripts/gen-cjk-cmaps.py <cmap-resources dir>   # regenerate CJK encoding CMaps
```

`vendor/lopdf` is lopdf 0.44.0 with one fix to its encrypted-document loader
(it now applies the load filter and releases each object's raw copy as it is
parsed); it goes away once the fix is released upstream
([lopdf#671](https://github.com/J-F-Liu/lopdf/pull/671)). The crates.io
package builds against the released lopdf (cargo ignores `[patch]` for
dependents): text comes out the same, and only encrypted PDFs use more memory
while loading. The npm package and the release binaries are built with the fix.

## License

Licensed under the [Apache License 2.0](LICENSE). Third-party data and code
are listed in [THIRD_PARTY.md](THIRD_PARTY.md).
