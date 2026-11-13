//! Hostile / malformed input: every fixture here is synthesized from the PDF
//! spec (ISO 32000-2) in-process. The Worker runs this crate on user uploads, so
//! the contract under test is: no panic, no unbounded time or memory, a typed
//! error (not garbage) for what cannot be read, and graceful degradation for what
//! can.

use std::io::Write;
use std::time::{Duration, Instant};

use pdftotext::{extract, Document, Error, Options};

/// Minimal PDF assembler: numbered objects + an automatically computed xref. A
/// later definition of the same object number replaces the earlier one.
#[derive(Default)]
struct Pdf {
    objs: Vec<(u32, Vec<u8>)>,
    trailer_extra: String,
}

impl Pdf {
    fn new() -> Self {
        Self::default()
    }

    fn obj(&mut self, n: u32, body: impl AsRef<[u8]>) -> &mut Self {
        self.objs.retain(|(k, _)| *k != n);
        self.objs.push((n, body.as_ref().to_vec()));
        self
    }

    fn stream(&mut self, n: u32, dict_extra: &str, data: &[u8]) -> &mut Self {
        let mut body = format!("<</Length {} {dict_extra}>>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.obj(n, body)
    }

    fn build(&self) -> Vec<u8> {
        let size = self.objs.iter().map(|(n, _)| *n).max().unwrap_or(0) + 1;
        let mut offsets = vec![None; size as usize];
        let mut buf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        for (n, body) in &self.objs {
            offsets[*n as usize] = Some(buf.len());
            writeln!(buf, "{n} 0 obj").unwrap();
            buf.extend_from_slice(body);
            buf.extend_from_slice(b"\nendobj\n");
        }
        let xref_off = buf.len();
        write!(buf, "xref\n0 {size}\n").unwrap();
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for off in offsets.iter().skip(1) {
            match off {
                Some(o) => writeln!(buf, "{o:010} 00000 n ").unwrap(),
                None => buf.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }
        write!(
            buf,
            "trailer\n<</Size {size}/Root 1 0 R {}>>\nstartxref\n{xref_off}\n%%EOF",
            self.trailer_extra
        )
        .unwrap();
        buf
    }
}

const FONT: &str = "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";

/// 1 page; objects 1 Catalog, 2 Pages, 3 Page, 4 content, 5 font. The page's
/// resources also name forms `Fm1`/`Fm2` (objects 6/7) for tests that add them.
fn one_page(content: &[u8], dict_extra: &str) -> Pdf {
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>/XObject<</Fm1 6 0 R/Fm2 7 0 R>>>>>>",
        )
        .stream(4, dict_extra, content)
        .obj(5, FONT);
    p
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn text_of(pdf: &[u8], layout: bool) -> String {
    let doc = Document::parse(pdf).expect("parse");
    doc.extract_text(0, layout).expect("extract")
}

fn within(limit: Duration, f: impl FnOnce()) {
    let t = Instant::now();
    f();
    assert!(
        t.elapsed() < limit,
        "took {:?} (limit {limit:?})",
        t.elapsed()
    );
}

// ---- Form XObjects: cycles, fan-out, positioning ---------------------------

#[test]
fn self_referencing_form_xobject_terminates() {
    let mut p = one_page(b"BT /F1 12 Tf 10 700 Td (page) Tj ET /Fm1 Do", "");
    p.stream(
        6,
        "/Type/XObject/Subtype/Form/BBox[0 0 100 100]/Resources<</Font<</F1 5 0 R>>/XObject<</Fm1 6 0 R>>>>",
        b"BT /F1 12 Tf (loop) Tj ET /Fm1 Do /Fm1 Do",
    );
    let pdf = p.build();
    within(Duration::from_secs(10), || {
        let t = text_of(&pdf, true);
        assert!(t.contains("page"));
        // The cycle is cut at the first re-entry: "loop" appears exactly once.
        assert_eq!(t.matches("loop").count(), 1, "{t:?}");
    });
}

#[test]
fn mutually_recursive_forms_terminate() {
    let mut p = one_page(b"/Fm1 Do", "");
    p.stream(
        6,
        "/Subtype/Form/Resources<</Font<</F1 5 0 R>>/XObject<</Fm2 7 0 R>>>>",
        b"BT /F1 12 Tf (a) Tj ET /Fm2 Do",
    )
    .stream(
        7,
        "/Subtype/Form/Resources<</Font<</F1 5 0 R>>/XObject<</Fm1 6 0 R>>>>",
        b"BT /F1 12 Tf (b) Tj ET /Fm1 Do",
    );
    within(Duration::from_secs(10), || {
        let t = text_of(&p.build(), false);
        assert!(t.contains('a') && t.contains('b'));
    });
}

#[test]
fn acyclic_form_fanout_is_bounded() {
    // 8 levels, each form invoking the next level's form 50 times (acyclic):
    // 50^8 leaf invocations if unbounded. Depth and cycle checks alone do not
    // stop this; the per-page invocation cap must.
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</XObject<</X 10 0 R>>>>>>",
        )
        .stream(4, "", b"/X Do");
    for level in 0..8u32 {
        let id = 10 + level;
        let next = id + 1;
        let body = "/X Do ".repeat(50);
        p.stream(
            id,
            &format!("/Subtype/Form/Resources<</XObject<</X {next} 0 R>>>>"),
            body.as_bytes(),
        );
    }
    p.stream(18, "/Subtype/Form", b"");
    within(Duration::from_secs(30), || {
        let doc = Document::parse(&p.build()).unwrap();
        // A truncated result or a Limit error are both fine; hanging is not.
        let _ = doc.extract_text(0, true);
    });
}

#[test]
fn form_text_is_placed_by_its_transform_not_appended() {
    // The form draws "Top" at its local origin; the page places that origin at
    // y=700 with `cm`, above the page's own "Bottom" at y=100. Reading order must
    // follow the drawn position (Top first), not "page text, then form block".
    let mut p = one_page(
        b"BT /F1 12 Tf 72 100 Td (Bottom) Tj ET q 1 0 0 1 72 700 cm /Fm1 Do Q",
        "",
    );
    p.stream(
        6,
        "/Subtype/Form/BBox[0 0 200 50]/Resources<</Font<</F1 5 0 R>>>>",
        b"BT /F1 12 Tf 0 0 Td (Top) Tj ET",
    );
    let t = text_of(&p.build(), true);
    let top = t.find("Top").expect("Top");
    let bottom = t.find("Bottom").expect("Bottom");
    assert!(top < bottom, "form text must sort by position: {t:?}");
}

#[test]
fn form_matrix_is_applied() {
    // Same, but the placement comes from the form's own /Matrix (translate to y=700).
    let mut p = one_page(b"BT /F1 12 Tf 72 100 Td (Bottom) Tj ET /Fm1 Do", "");
    p.stream(
        6,
        "/Subtype/Form/BBox[0 0 200 50]/Matrix[1 0 0 1 72 700]/Resources<</Font<</F1 5 0 R>>>>",
        b"BT /F1 12 Tf 0 0 Td (Top) Tj ET",
    );
    let t = text_of(&p.build(), true);
    assert!(t.find("Top").unwrap() < t.find("Bottom").unwrap(), "{t:?}");
}

#[test]
fn raw_mode_keeps_form_text_inline_in_stream_order() {
    let mut p = one_page(
        b"BT /F1 12 Tf (before) Tj ET /Fm1 Do BT /F1 12 Tf (after) Tj ET",
        "",
    );
    p.stream(
        6,
        "/Subtype/Form/Resources<</Font<</F1 5 0 R>>>>",
        b"BT /F1 12 Tf (inside) Tj ET",
    );
    let t = text_of(&p.build(), false);
    let (b, i, a) = (
        t.find("before").unwrap(),
        t.find("inside").unwrap(),
        t.find("after").unwrap(),
    );
    assert!(b < i && i < a, "{t:?}");
}

#[test]
fn flipped_page_ctm_still_reads_top_down() {
    // Exporters like Cairo/Skia flip the page with `1 0 0 -1 0 792 cm` and draw
    // top-down in that space. Without CTM tracking the lines came out reversed.
    let t = text_of(
        &one_page(
            b"1 0 0 -1 0 792 cm BT /F1 12 Tf 72 72 Td (first) Tj 0 100 Td (second) Tj ET",
            "",
        )
        .build(),
        true,
    );
    assert!(
        t.find("first").unwrap() < t.find("second").unwrap(),
        "{t:?}"
    );
}

#[test]
fn tj_kerning_gap_becomes_a_word_space() {
    // [(Hello)-400(World)] — the -400 adjustment is a 0.4 em gap: a word break the
    // content stream encodes only as spacing.
    let t = text_of(
        &one_page(b"BT /F1 12 Tf 72 700 Td [(Hello)-400(World)] TJ ET", "").build(),
        true,
    );
    assert!(t.contains("Hello World"), "{t:?}");
    let t = text_of(
        &one_page(b"BT /F1 12 Tf 72 700 Td [(Hel)-20(lo)] TJ ET", "").build(),
        true,
    );
    assert!(
        t.contains("Hello"),
        "tight kerning must not split a word: {t:?}"
    );
}

// ---- Streams: bombs, truncation, indirection -------------------------------

#[test]
fn decompression_bomb_content_stream_is_capped() {
    // ~200 MiB of zeros deflates to a few hundred KB. The decoded content must be
    // capped (64 MiB), not materialized.
    let zeros = vec![0u8; 200 << 20];
    let bomb = zlib(&zeros);
    assert!(
        bomb.len() < 1 << 20,
        "fixture should be small: {}",
        bomb.len()
    );
    drop(zeros);
    let pdf = one_page(&bomb, "/Filter/FlateDecode").build();
    within(Duration::from_secs(120), || {
        let doc = Document::parse(&pdf).unwrap();
        let content = doc.page_content(0).unwrap();
        assert!(
            content.len() <= 64 << 20,
            "decoded {} bytes — cap not enforced",
            content.len()
        );
        let _ = doc.extract_text(0, true);
    });
}

#[test]
fn decompression_bomb_object_stream_is_rejected_or_capped_at_load() {
    // An /ObjStm that inflates far past the load-time cap must not be inflated
    // unbounded by lopdf: parse either fails with a typed error or succeeds with
    // the stream dropped.
    let bomb = zlib(&vec![b' '; 100 << 20]);
    let mut p = one_page(b"BT (x) Tj ET", "");
    p.stream(8, "/Type/ObjStm/N 1/First 4/Filter/FlateDecode", &bomb);
    let pdf = p.build();
    within(Duration::from_secs(120), || match Document::parse(&pdf) {
        Ok(_) | Err(Error::Limit(_)) | Err(Error::Unsupported(_)) => {}
        Err(e) => panic!("unexpected error class: {e}"),
    });
}

#[test]
fn truncated_flate_stream_keeps_the_good_prefix() {
    let content = b"BT /F1 12 Tf 72 700 Td (survivor) Tj ET\n".repeat(2000);
    let z = zlib(&content);
    let cut = &z[..z.len() * 3 / 4]; // lop off the tail incl. the adler32 trailer
    let pdf = one_page(cut, "/Filter/FlateDecode").build();
    let t = text_of(&pdf, false);
    assert!(t.contains("survivor"), "partial output must be kept: {t:?}");
}

#[test]
fn contents_as_indirect_array_is_read() {
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 8 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        )
        .obj(8, "[4 0 R 9 0 R]")
        .stream(4, "", b"BT /F1 12 Tf 72 700 Td (one) Tj")
        .stream(9, "", b"(two) Tj ET")
        .obj(5, FONT);
    let t = text_of(&p.build(), false);
    assert!(t.contains("one") && t.contains("two"), "{t:?}");
}

#[test]
fn ascii85_then_flate_chain_is_decoded() {
    // /Filter [/ASCII85Decode /FlateDecode]: previously fell through as raw
    // bytes (no text). Encode with a tiny ASCII85 encoder.
    let z = zlib(b"BT /F1 12 Tf 72 700 Td (chained) Tj ET");
    let mut a85 = Vec::new();
    for chunk in z.chunks(4) {
        let mut v = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            v |= (*b as u32) << (24 - 8 * i);
        }
        let mut digits = [0u8; 5];
        let mut x = v;
        for d in digits.iter_mut().rev() {
            *d = (x % 85) as u8 + b'!';
            x /= 85;
        }
        a85.extend_from_slice(&digits[..chunk.len() + 1]);
    }
    a85.extend_from_slice(b"~>");
    let pdf = one_page(&a85, "/Filter[/ASCII85Decode/FlateDecode]").build();
    assert!(text_of(&pdf, false).contains("chained"));
}

// ---- Structure: cycles, missing parts, page counts -------------------------

#[test]
fn cyclic_page_tree_terminates() {
    // Pages node lists itself as a kid (twice); page's Parent points back at it.
    // lopdf's page walk bounds iterations by the object count, so the self-kids
    // may exhaust that budget before the real page: the result is either the page
    // or a typed "no pages" error — never a hang or a panic.
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(
            2,
            "<</Type/Pages/Kids[2 0 R 3 0 R 2 0 R]/Count 3/Parent 2 0 R>>",
        )
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        )
        .stream(4, "", b"BT /F1 12 Tf (reachable) Tj ET")
        .obj(5, FONT);
    within(Duration::from_secs(10), || {
        match extract(&p.build(), &Options::default()) {
            Ok(pages) => assert!(pages[0].text.contains("reachable")),
            Err(Error::Unsupported(_)) => {}
            Err(e) => panic!("unexpected error class: {e}"),
        }
    });
}

#[test]
fn resources_parent_cycle_terminates_and_keeps_page_fonts() {
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1/Parent 3 0 R>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        )
        .stream(4, "", b"BT /F1 12 Tf (still here) Tj ET")
        .obj(5, FONT);
    let t = text_of(&p.build(), false);
    assert!(t.contains("still here"), "{t:?}");
}

#[test]
fn resources_inherited_from_pages_node_inline_dict() {
    // /Resources lives inline on the Pages node (legal inheritance) — not as an
    // indirect object, which is the only shape lopdf's own helper follows.
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(
            2,
            "<</Type/Pages/Kids[3 0 R]/Count 1/Resources<</Font<</F1 5 0 R>>>>>>",
        )
        .obj(3, "<</Type/Page/Parent 2 0 R/Contents 4 0 R>>")
        .stream(4, "", b"BT /F1 12 Tf (inherited \\222) Tj ET")
        .obj(5, FONT);
    let t = text_of(&p.build(), false);
    assert!(t.contains("inherited \u{2019}"), "font not resolved: {t:?}");
}

#[test]
fn zero_page_document_is_unsupported_not_a_panic() {
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[]/Count 0>>");
    let pdf = p.build();
    assert_eq!(Document::parse(&pdf).unwrap().page_count(), 0);
    // Previously `first..=last` over (0, 0) underflowed `i - 1`.
    assert!(matches!(
        extract(&pdf, &Options::default()),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn page_range_past_the_end_is_refused() {
    let pdf = one_page(b"BT /F1 12 Tf (x) Tj ET", "").build();
    // A start page past the end is an error (as in poppler), never another page.
    let opts = Options {
        first_page: 50,
        last_page: Some(99),
        ..Options::default()
    };
    assert!(matches!(extract(&pdf, &opts), Err(Error::Unsupported(_))));
    // A last page past the end still means "to the end".
    let opts = Options {
        first_page: 1,
        last_page: Some(99),
        ..Options::default()
    };
    assert_eq!(extract(&pdf, &opts).unwrap().len(), 1);
}

// ---- Encryption ------------------------------------------------------------

#[test]
fn password_protected_pdf_reports_encrypted() {
    // Standard security handler, R2/V1, with an /O and /U that cannot match the
    // empty user password.
    let mut p = one_page(b"BT /F1 12 Tf (secret) Tj ET", "");
    let hex = "AB".repeat(32);
    p.obj(
        9,
        format!("<</Filter/Standard/V 1/R 2/Length 40/P -4/O <{hex}>/U <{hex}>>>"),
    );
    p.trailer_extra =
        "/Encrypt 9 0 R /ID [<00112233445566778899AABBCCDDEEFF><00112233445566778899AABBCCDDEEFF>]"
            .into();
    match Document::parse(&p.build()) {
        Err(Error::Encrypted(_)) => {}
        other => panic!("expected Encrypted, got ok={}", other.is_ok()),
    }
}

#[test]
fn direct_encrypt_dictionary_is_refused_not_decoded_as_garbage() {
    let mut p = one_page(b"BT /F1 12 Tf (secret) Tj ET", "");
    p.trailer_extra =
        "/Encrypt <</Filter/Standard/V 1/R 2/P -4/O (aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)/U (bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)>>".into();
    assert!(matches!(
        Document::parse(&p.build()),
        Err(Error::Encrypted(_))
    ));
}

// ---- Fonts -----------------------------------------------------------------

#[test]
fn huge_tounicode_bfrange_does_not_blow_up() {
    let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap
1 begincodespacerange <00000000> <FFFFFFFF> endcodespacerange
1 beginbfrange <00000000> <FFFFFFFF> <0000> endbfrange
endcmap";
    let mut p = one_page(b"BT /F9 12 Tf (A) Tj ET", "");
    p.obj(
        3,
        "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F9 10 0 R>>>>>>",
    )
    .obj(
        10,
        "<</Type/Font/Subtype/Type1/BaseFont/X/ToUnicode 11 0 R>>",
    )
    .stream(11, "", cmap);
    within(Duration::from_secs(10), || {
        let _ = Document::parse(&p.build()).map(|d| d.extract_text(0, true));
    });
}

#[test]
fn tounicode_ligature_survives_and_composite_font_without_map_is_silent() {
    let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange
1 beginbfchar <01> <FB01> endbfchar";
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 10 0 R/F2 12 0 R>>>>>>",
        )
        .stream(
            4,
            "",
            b"BT /F1 12 Tf 10 700 Td <01> Tj <6E64> Tj ET BT /F2 12 Tf 10 600 Td <00410042> Tj ET",
        )
        .obj(
            10,
            "<</Type/Font/Subtype/Type1/BaseFont/X/ToUnicode 11 0 R>>",
        )
        .stream(11, "", cmap)
        .obj(
            12,
            "<</Type/Font/Subtype/Type0/BaseFont/Y/Encoding/Identity-H/DescendantFonts[13 0 R]>>",
        )
        .obj(
            13,
            "<</Type/Font/Subtype/CIDFontType2/BaseFont/Y/DW 600/W[0 [500 500] 65 66 700]>>",
        );
    let t = text_of(&p.build(), true);
    assert!(t.contains("fi"), "ligature code lost: {t:?}");
    // F2 is Type0/Identity-H with no ToUnicode: GIDs 0x0041/0x0042 must not be
    // reported as the ASCII text "AB".
    assert!(!t.contains("AB"), "glyph ids leaked as text: {t:?}");
}

// ---- Fuzz-ish smoke --------------------------------------------------------

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn mutated_pdfs_never_panic_or_hang() {
    let mut p = one_page(
        &zlib(b"BT /F1 12 Tf 72 700 Td (Hello) Tj [(a)-300(b)] TJ ET /Fm1 Do"),
        "/Filter/FlateDecode",
    );
    p.stream(
        6,
        "/Subtype/Form/Matrix[1 0 0 1 5 5]/Resources<</Font<</F1 5 0 R>>>>",
        b"BT /F1 9 Tf (form) Tj ET",
    );
    let base = p.build();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    within(Duration::from_secs(120), || {
        for round in 0..400 {
            let mut m = base.clone();
            for _ in 0..(1 + rng.next() % 12) {
                let at = (rng.next() as usize) % m.len();
                match rng.next() % 4 {
                    0 => m[at] = rng.next() as u8,
                    1 => m[at] ^= 1 << (rng.next() % 8),
                    2 => {
                        m.truncate(at.max(8));
                    }
                    _ => {
                        let end = (at + (rng.next() as usize % 32)).min(m.len());
                        m.drain(at..end);
                    }
                }
                if m.is_empty() {
                    break;
                }
            }
            // Result is irrelevant; surviving is the assertion (a panic fails the test).
            let r = std::panic::catch_unwind(|| {
                let _ = extract(&m, &Options::default());
                let _ = extract(
                    &m,
                    &Options {
                        layout: true,
                        ..Options::default()
                    },
                );
            });
            assert!(r.is_ok(), "panic on mutation round {round}");
        }
    });
}

#[test]
fn deeply_nested_syntax_does_not_overflow_the_stack() {
    // 200k nested arrays in a content stream and in an object, 100k nested dicts.
    let deep = "[".repeat(200_000);
    let mut p = one_page(deep.as_bytes(), "");
    p.obj(8, deep.as_bytes());
    let r = std::panic::catch_unwind(|| {
        let _ = extract(&p.build(), &Options::default());
    });
    assert!(r.is_ok());
    let dd = "<<".repeat(100_000);
    let mut q = one_page(b"BT (x) Tj ET", "");
    q.obj(8, dd.as_bytes());
    let r = std::panic::catch_unwind(|| {
        let _ = extract(&q.build(), &Options::default());
    });
    assert!(r.is_ok());
}

#[test]
fn non_pdf_and_empty_inputs_are_typed_errors() {
    assert!(matches!(
        extract(b"", &Options::default()),
        Err(Error::NotPdf)
    ));
    assert!(matches!(
        extract(b"%PDF-1.4\n", &Options::default()),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        extract(&[0u8; 4096], &Options::default()),
        Err(Error::NotPdf)
    ));
}

// ---- Patterns from real equipment manuals ----------------------------------

/// Like `one_page`, with a font whose every glyph is 500/1000 em wide, so the
/// expected positions below are exact.
fn one_page_fixed_width(content: &[u8]) -> Vec<u8> {
    let widths = vec!["500"; 95].join(" ");
    let font = format!(
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding/FirstChar 32/LastChar 126/Widths[{widths}]>>"
    );
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        )
        .stream(4, "", content)
        .obj(5, &font);
    p.build()
}

#[test]
fn letter_spaced_text_keeps_its_word_gaps() {
    // Mitsubishi-style: 2.4 pt Tc pulled back glyph by glyph with TJ kerning;
    // the only word gaps are the uncompensated Tc, some inside one string.
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 2.4 Tc 72 700 Td [(d)240(a)240(m)240(a)240(g)240(e)240(,e)240(t)240(c)240(.)-45(m)240(a)240(y)] TJ ET",
    );
    for layout in [true, false] {
        let t = text_of(&pdf, layout);
        assert_eq!(t.trim(), "damage, etc. may", "layout={layout}");
    }
}

#[test]
fn raw_mode_does_not_space_out_per_glyph_strings() {
    // InDesign-style: every glyph is its own string, placed exactly where the
    // previous one ended. Raw mode used to print "T h e".
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 700 Td (T) Tj 5 0 Td (h) Tj 5 0 Td (e) Tj 10 0 Td (end) Tj 0 -14 Td (next) Tj ET",
    );
    assert_eq!(text_of(&pdf, false), "The end\nnext\n");
}

#[test]
fn stray_space_glyph_inside_a_word_is_ignored() {
    // A space glyph drawn over the "ff" ligature (Atlas Copco manual) printed
    // "diff erent".
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 700 Td (di) Tj 10 0 Td (ff) Tj 7 0 Td ( ) Tj 3 0 Td (erent) Tj ET",
    );
    for layout in [true, false] {
        assert_eq!(text_of(&pdf, layout).trim(), "different", "layout={layout}");
    }
}

#[test]
fn overprinted_fake_bold_is_read_once() {
    // The same string drawn twice 0.3 pt apart (fake bold, SKF catalogue) was
    // interleaved letter by letter: "MorMore infe informaormationtion".
    let pdf = one_page_fixed_width(
        b"BT /F1 13 Tf 72 700 Td (More information) Tj 0.3 0 Td (More information) Tj ET",
    );
    for layout in [true, false] {
        assert_eq!(
            text_of(&pdf, layout).trim(),
            "More information",
            "layout={layout}"
        );
    }
}

/// One page with a custom font dictionary (object 5) and extra objects.
fn page_with_font(content: &[u8], font: &str, extra: &[(u32, &str)]) -> Vec<u8> {
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>",
        )
        .stream(4, "", content)
        .obj(5, font);
    for (n, body) in extra {
        p.obj(*n, *body);
    }
    p.build()
}

#[test]
fn adobe_japan1_cid_font_without_tounicode_is_read() {
    // Japanese manuals embed full Kozuka/Ryumin fonts with Identity-H and no
    // ToUnicode: the CIDs are Adobe-Japan1's, and its published mapping
    // decodes them (CID 34 = A, 843 = あ, 1125 = 亜).
    let pdf = page_with_font(
        b"BT /F1 12 Tf 72 700 Td <0022034B0465> Tj ET",
        "<</Type/Font/Subtype/Type0/BaseFont/KozGoPro-Medium-Identity-H/Encoding/Identity-H/DescendantFonts[6 0 R]>>",
        &[(
            6,
            "<</Type/Font/Subtype/CIDFontType0/BaseFont/KozGoPro-Medium/CIDSystemInfo<</Registry(Adobe)/Ordering(Japan1)/Supplement 4>>/DW 1000>>",
        )],
    );
    assert_eq!(text_of(&pdf, true).trim(), "Aあ亜");
}

#[test]
fn identity_cid_font_from_standard_order_family_reads_ascii() {
    // Ordering (Identity), no ToUnicode, built from Arial: glyph ID = ASCII
    // - 29. Others print it shifted ("Uhdg" for "Read"); the widths confirm
    // the Latin layout (space and period narrow, capitals wide).
    let pdf = page_with_font(
        b"BT /F1 12 Tf 72 700 Td <0035004800440047> Tj ET",
        "<</Type/Font/Subtype/Type0/BaseFont/ABCDEF+ArialUnicodeMS-Identity-H/Encoding/Identity-H/DescendantFonts[6 0 R]>>",
        &[(
            6,
            "<</Type/Font/Subtype/CIDFontType0/BaseFont/ABCDEF+ArialUnicodeMS/CIDSystemInfo<</Registry(Adobe)/Ordering(Identity)/Supplement 0>>/DW 556/W[3[278]17[278]36[667 667 722 722 667 611 778 722 278 500 667 556 833 722 778 667 778 722 667 611 722 667 944 667 667 611]]>>",
        )],
    );
    assert_eq!(text_of(&pdf, true).trim(), "Read");
}

#[test]
fn symbol_font_glyph_by_code_name_is_greek() {
    // `/c87` = code 87 of the Symbol font's own encoding: Ω, not "W".
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F2 5 0 R>>>>>>",
        )
        .stream(4, "", b"BT /F2 10 Tf 72 700 Td <0203> Tj ET")
        .obj(
            5,
            "<</Type/Font/Subtype/Type1/BaseFont/ABCDEF+Symbol/FirstChar 2/LastChar 3/Widths[768 549]/Encoding<</Type/Encoding/Differences[2/c87/c177]>>>>",
        );
    assert_eq!(text_of(&p.build(), true).trim(), "Ω±");
}

#[test]
fn print_slug_outside_the_page_is_dropped() {
    // InDesign/FrameMaker exports carry the file name and print date in the
    // pasteboard below the page.
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 700 Td (Body) Tj ET BT /F1 6 Tf 10 -23 Td (manual.indd 8 28/01/19) Tj ET",
    );
    assert_eq!(text_of(&pdf, true).trim(), "Body");
}

#[test]
fn text_painted_over_by_an_opaque_box_is_hidden() {
    // A white box drawn after a label hides it on the page; the label drawn
    // on top afterwards is what a reader sees.
    let pdf = one_page_fixed_width(
        b"BT /F1 9 Tf 45 521 Td (Old label) Tj ET 1 1 1 rg 40 515 200 20 re f 0 0 0 rg BT /F1 9 Tf 45 521 Td (New label) Tj ET",
    );
    assert_eq!(text_of(&pdf, true).trim(), "New label");
    // A see-through box (ca < 1) hides nothing.
    let mut p = Pdf::new();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>/ExtGState<</H<</ca 0.3>>>>>>>>",
        )
        .stream(4, "", b"BT /F1 9 Tf 45 521 Td (Highlighted) Tj ET /H gs 1 1 0 rg 40 515 200 20 re f")
        .obj(5, FONT);
    assert_eq!(text_of(&p.build(), true).trim(), "Highlighted");
}

#[test]
fn label_over_a_rule_of_underscores_is_not_interleaved() {
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 300 700 Td (Scope) Tj ET BT /F1 10 Tf 300 697 Td (______) Tj ET",
    );
    let t = text_of(&pdf, true);
    assert!(t.contains("Scope"), "{t:?}");
    assert!(t.contains("______"), "{t:?}");
}

#[test]
fn justified_space_narrowed_by_negative_word_spacing_still_breaks() {
    // A literal space glyph is a word break even when Tw squeezes it below
    // the gap threshold (justified text in a 1995 manual: "isdisplayed").
    let pdf = one_page_fixed_width(b"BT /F1 10 Tf -4.5 Tw 72 700 Td (is displayed) Tj ET");
    for layout in [true, false] {
        assert_eq!(
            text_of(&pdf, layout).trim(),
            "is displayed",
            "layout={layout}"
        );
    }
}

#[test]
fn glyph_by_glyph_text_with_short_words_keeps_spaces() {
    // Each glyph its own string, words separated only by position: "Refer to
    // F". Short words must not be mistaken for letter-spaced text.
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 700 Td (R) Tj 5 0 Td (e) Tj 5 0 Td (f) Tj 5 0 Td (e) Tj 5 0 Td (r) Tj 7 0 Td (t) Tj 5 0 Td (o) Tj 7 0 Td (F) Tj ET",
    );
    assert_eq!(text_of(&pdf, true).trim(), "Refer to F");
}

#[test]
fn letter_spaced_caption_with_ligature_space_reads_as_one_word() {
    // Caption tracked 0.32 em, its ligature followed by a space glyph used as
    // the letter gap ("fi" + " " then "gure"; the test font's space is 0.5 em,
    // pulled back to ~0.3 em like the real one): reads "figure 1".
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 700 Td [(fi )200(g)-320(u)-320(r)-320(e)-820(1)] TJ ET",
    );
    assert_eq!(text_of(&pdf, true).trim(), "figure 1");
}

#[test]
fn two_columns_stay_apart_in_layout_mode() {
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 700 Td (Left column) Tj ET BT /F1 10 Tf 320 700 Td (Right column) Tj ET",
    );
    let t = text_of(&pdf, true);
    assert!(
        t.contains("Left column   "),
        "columns must not run together: {t:?}"
    );
}

#[test]
fn shift_jis_encoded_cjk_font_without_tounicode_is_read() {
    // /Encoding /90ms-RKSJ-H: mixed one- and two-byte codes. "A あ 亜" in
    // Shift-JIS is 41, 82A0, 889F; CIDs via the CMap, text via Adobe-Japan1,
    // widths via /W keyed by CID (half-width A, full-width kana and kanji).
    let pdf = page_with_font(
        b"BT /F1 12 Tf 72 700 Td <4182A0889F> Tj ET",
        "<</Type/Font/Subtype/Type0/BaseFont/Ryumin-Light-90ms-RKSJ-H/Encoding/90ms-RKSJ-H/DescendantFonts[6 0 R]>>",
        &[(
            6,
            "<</Type/Font/Subtype/CIDFontType0/BaseFont/Ryumin-Light/CIDSystemInfo<</Registry(Adobe)/Ordering(Japan1)/Supplement 2>>/DW 1000/W[1 95 500]>>",
        )],
    );
    assert_eq!(text_of(&pdf, true).trim(), "Aあ亜");
}

#[test]
fn gbk_and_big5_encoded_fonts_are_read() {
    let gbk = page_with_font(
        b"BT /F1 12 Tf 72 700 Td <D6D0CEC4> Tj ET",
        "<</Type/Font/Subtype/Type0/BaseFont/STSong-Light-GBK-EUC-H/Encoding/GBK-EUC-H/DescendantFonts[6 0 R]>>",
        &[(6, "<</Type/Font/Subtype/CIDFontType0/BaseFont/STSong-Light/CIDSystemInfo<</Registry(Adobe)/Ordering(GB1)/Supplement 4>>/DW 1000>>")],
    );
    assert_eq!(text_of(&gbk, true).trim(), "中文");
    let big5 = page_with_font(
        b"BT /F1 12 Tf 72 700 Td <A4A4A4E5> Tj ET",
        "<</Type/Font/Subtype/Type0/BaseFont/MingStd-Light-B5pc-H/Encoding/B5pc-H/DescendantFonts[6 0 R]>>",
        &[(6, "<</Type/Font/Subtype/CIDFontType0/BaseFont/MingStd-Light/CIDSystemInfo<</Registry(Adobe)/Ordering(CNS1)/Supplement 4>>/DW 1000>>")],
    );
    assert_eq!(text_of(&big5, true).trim(), "中文");
}

/// A page of two text columns (left and right), each `n` lines, with
/// paragraph breaks at different heights, plus a full-width heading.
fn two_column_page(n: usize) -> Vec<u8> {
    let mut c = String::from("BT /F1 10 Tf 72 760 Td (Heading across both columns) Tj ET\n");
    for i in 0..n {
        let yl = 730.0 - 12.0 * i as f32 - if i >= 4 { 12.0 } else { 0.0 };
        let yr = 730.0 - 12.0 * i as f32 - if i >= 7 { 12.0 } else { 0.0 };
        c.push_str(&format!(
            "BT /F1 10 Tf 72 {yl} Td (left line {i} of running text here) Tj ET\n\
             BT /F1 10 Tf 320 {yr} Td (right line {i} of running text here) Tj ET\n"
        ));
    }
    one_page_fixed_width(c.as_bytes())
}

#[test]
fn reading_order_reads_text_columns_one_after_the_other() {
    let t = text_of(&two_column_page(10), false);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "Heading across both columns");
    assert_eq!(lines[1], "left line 0 of running text here");
    assert_eq!(lines[10], "left line 9 of running text here");
    assert_eq!(lines[11], "right line 0 of running text here", "{t}");
    // Layout mode keeps the physical arrangement.
    let l = text_of(&two_column_page(10), true);
    assert!(l.lines().nth(1).unwrap().contains("left line 0"));
    assert!(l.lines().nth(1).unwrap().contains("right line 0"));
}

#[test]
fn reading_order_keeps_table_rows_together() {
    // A two-column troubleshooting table: every row ends on both sides at once,
    // so the gutter between the columns is not a text gutter.
    let mut c = String::new();
    for i in 0..10 {
        let y = 720.0 - 30.0 * i as f32;
        c.push_str(&format!(
            "BT /F1 10 Tf 72 {y} Td (Fault number {i} described here) Tj ET\n\
             BT /F1 10 Tf 320 {y} Td (Remedy number {i} described here) Tj ET\n\
             BT /F1 10 Tf 320 {} Td (and continued on a second line) Tj ET\n",
            y - 12.0
        ));
    }
    let t = text_of(&one_page_fixed_width(c.as_bytes()), false);
    assert!(
        t.lines()
            .any(|l| l == "Fault number 3 described here Remedy number 3 described here"),
        "{t}"
    );
}

#[test]
fn raw_mode_still_follows_the_content_stream() {
    // Drawn bottom line first: raw keeps stream order, reading order does not.
    let pdf = one_page_fixed_width(
        b"BT /F1 10 Tf 72 600 Td (second) Tj ET BT /F1 10 Tf 72 700 Td (first) Tj ET",
    );
    let doc = Document::parse(&pdf).unwrap();
    assert_eq!(
        doc.extract_text_mode(0, pdftotext::text::Mode::Raw)
            .unwrap(),
        "second\nfirst\n"
    );
    assert_eq!(doc.extract_text(0, false).unwrap(), "first\nsecond\n");
}
