//! Gap-exercising fixtures (Phase 7, gap-coverage for the parity critique).
//! Synthesized, spec-valid PDFs built in-process that hit the four general
//! correctness gaps the corpus alone wouldn't catch:
//!   #1 a font with a named Encoding and NO /Differences array (WinAnsi)
//!   #4 inline image (BI … ID <binary with '('> EI) followed by real text
//! (Gap #2 — array-form ToUnicode — is a unit test in `cmap.rs`; #3 AGL scope is
//! documented in `agl.rs`.) These run unconditionally (no corpus env needed).

use pdftotext::Document;

/// Build a 1-page PDF: obj1 Catalog, obj2 Pages, obj3 Page (Resources inline
/// with /F1 → obj5), obj4 content stream, obj5 the given font dict. xref offsets
/// are computed as bytes are emitted.
fn build_pdf(font_obj_body: &str, content: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut offsets = [0u64; 6]; // index by obj number; [0] unused
    let mut buf: Vec<u8> = Vec::new();

    writeln!(buf, "%PDF-1.4").unwrap();
    buf.extend_from_slice(b"%\xe2\xe3\xcf\xd3\n");

    offsets[1] = buf.len() as u64;
    writeln!(buf, "1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj").unwrap();
    offsets[2] = buf.len() as u64;
    writeln!(buf, "2 0 obj\n<</Type/Pages/Kids[3 0 R]/Count 1>>\nendobj").unwrap();
    offsets[4] = buf.len() as u64;
    writeln!(buf, "4 0 obj\n<</Length {}>>\nstream", content.len()).unwrap();
    buf.extend_from_slice(content);
    buf.extend_from_slice(b"\nendstream\nendobj\n");
    offsets[5] = buf.len() as u64;
    writeln!(buf, "5 0 obj\n{font_obj_body}\nendobj").unwrap();
    offsets[3] = buf.len() as u64;
    writeln!(
        buf,
        "3 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>\nendobj"
    )
    .unwrap();

    let xref_off = buf.len();
    writeln!(buf, "xref\n0 6").unwrap();
    buf.extend_from_slice(b"0000000000 65535 f \n");
    for &off in offsets.iter().skip(1) {
        writeln!(buf, "{off:010} 00000 n ").unwrap();
    }
    writeln!(
        buf,
        "trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF"
    )
    .unwrap();
    buf
}

/// Gap #1: a font with `/Encoding /WinAnsiEncoding` and no `/Differences`. Byte
/// 0x92 (octal \222) must decode to U+2019 (right single quote), not the
/// control char 0x0092 the old generic table produced.
#[test]
fn winansi_without_differences_decodes_smart_quote() {
    let font = "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
    let content = b"BT /F1 12 Tf (it\\222s) Tj ET"; // \222 = 0x92 = U+2019
    let pdf = build_pdf(font, content);
    let doc = Document::parse(&pdf).expect("parse");
    let text = doc.extract_text(0, false).expect("extract");
    assert!(
        text.contains("it\u{2019}s"),
        "WinAnsi 0x92 should be U+2019, got: {text:?}"
    );
}

/// Gap #4: an inline image whose binary payload contains an unbalanced `(` must
/// NOT start a literal string that swallows the text operators after `EI`.
#[test]
fn inline_image_does_not_swallow_following_text() {
    let font = "<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
    // Image data between ID and EI contains a raw `(` with no matching `)`.
    let mut content: Vec<u8> = Vec::new();
    content.extend_from_slice(b"BT /F1 12 Tf (before) Tj ET ");
    content.extend_from_slice(b"BI /W 1 /H 1 /BPC 8 /CS /G ID ");
    content.extend_from_slice(&[0xff, b'(', 0xff]); // image bytes incl. an unbalanced '('
    content.extend_from_slice(b"\nEI BT /F1 12 Tf (after) Tj ET");
    let pdf = build_pdf(font, &content);
    let doc = Document::parse(&pdf).expect("parse");
    let text = doc.extract_text(0, false).expect("extract");
    assert!(text.contains("before"), "pre-image text lost: {text:?}");
    assert!(
        text.contains("after"),
        "post-image text swallowed by inline image: {text:?}"
    );
}
