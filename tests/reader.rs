//! Phase 1 integration: reader page-tree walk + decoded content-stream access.
//!
//! Fixtures are synthesized in-process (`build_minimal_pdf`) — deterministic,
//! no external PDF, no license exposure. A corpus-gated test exercises real
//! real manuals when `PDFTOTEXT_CORPUS_DIR` is set (run locally, not in CI).

use pdftotext::Document;

/// Build a minimal but spec-valid PDF with `num_pages` pages, each carrying a
/// one-text-show content stream `BT (Page k) Tj ET`. Cross-reference offsets are
/// computed as bytes are emitted, so the xref table is always correct.
fn build_minimal_pdf(num_pages: usize) -> Vec<u8> {
    use std::io::Write;
    let n = num_pages;
    // Object ids: 1=Catalog, 2=Pages, 3..2+n=pages, 3+n..2+2n=content streams.
    let total_objs = 2 + 2 * n;
    let mut offsets = vec![0u64; total_objs + 1]; // index by object number; [0] unused
    let mut buf: Vec<u8> = Vec::new();

    writeln!(buf, "%PDF-1.4").unwrap();
    // High-byte comment marks the file as binary (spec recommendation).
    buf.extend_from_slice(b"%\xe2\xe3\xcf\xd3\n");

    offsets[1] = buf.len() as u64;
    writeln!(buf, "1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj").unwrap();

    let kids: String = (3..3 + n).map(|i| format!("{i} 0 R ")).collect();
    offsets[2] = buf.len() as u64;
    writeln!(
        buf,
        "2 0 obj\n<</Type/Pages/Kids[{}]/Count {n}>>\nendobj",
        kids.trim_end()
    )
    .unwrap();

    // Content stream objects first (order is irrelevant — xref points to each).
    for k in 0..n {
        let id = 3 + n + k;
        let body = format!("BT (Page {}) Tj ET", k + 1);
        offsets[id] = buf.len() as u64;
        writeln!(
            buf,
            "{} 0 obj\n<</Length {}>>\nstream\n{}\nendstream\nendobj",
            id,
            body.len(),
            body
        )
        .unwrap();
    }
    // Page objects referencing their content stream.
    for k in 0..n {
        let page_id = 3 + k;
        let content_id = 3 + n + k;
        offsets[page_id] = buf.len() as u64;
        writeln!(
            buf,
            "{} 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents {} 0 R>>\nendobj",
            page_id, content_id
        )
        .unwrap();
    }

    let xref_off = buf.len();
    writeln!(buf, "xref").unwrap();
    writeln!(buf, "0 {}", total_objs + 1).unwrap();
    buf.extend_from_slice(b"0000000000 65535 f \n");
    for &off in offsets.iter().skip(1) {
        writeln!(buf, "{off:010} 00000 n ").unwrap();
    }
    writeln!(buf, "trailer").unwrap();
    writeln!(buf, "<</Size {}/Root 1 0 R>>", total_objs + 1).unwrap();
    writeln!(buf, "startxref").unwrap();
    writeln!(buf, "{xref_off}").unwrap();
    buf.extend_from_slice(b"%%EOF");
    buf
}

#[test]
fn reader_counts_pages_and_reads_decoded_content() {
    let bytes = build_minimal_pdf(3);
    let doc = Document::parse(&bytes).expect("parse minimal pdf");
    assert_eq!(doc.page_count(), 3, "page-tree walk should report 3 pages");

    let b0 = doc.page_content(0).expect("content page 0");
    let c0 = String::from_utf8_lossy(&b0);
    assert!(c0.contains("Page 1"), "decoded content page 0: {c0:?}");
    let b2 = doc.page_content(2).expect("content page 2");
    let c2 = String::from_utf8_lossy(&b2);
    assert!(c2.contains("Page 3"), "decoded content page 2: {c2:?}");
}

#[test]
fn reader_page_content_out_of_range_errors() {
    let doc = Document::parse(&build_minimal_pdf(1)).expect("parse");
    assert!(
        doc.page_content(5).is_err(),
        "out-of-range page should error"
    );
}

/// Real-corpus smoke: page counts must be ≥1 on every parseable PDF in
/// `PDFTOTEXT_CORPUS_DIR`. Skipped (not failed) when the env var is unset.
#[test]
fn corpus_page_counts_are_plausible() {
    let dir = match std::env::var("PDFTOTEXT_CORPUS_DIR") {
        Ok(d) => d,
        Err(_) => {
            eprintln!("skip: set PDFTOTEXT_CORPUS_DIR to a dir of .pdf files");
            return;
        }
    };
    let mut count = 0;
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let p = entry.expect("dir entry").path();
        if p.extension().is_none_or(|e| e != "pdf") {
            continue;
        }
        let bytes = std::fs::read(&p).expect("read pdf");
        let doc = Document::parse(&bytes).unwrap_or_else(|e| panic!("parse {:?}: {e}", p));
        let n = doc.page_count();
        assert!(n >= 1, "{:?} page_count={n}", p);
        eprintln!("{:?}: {n} pages", p.file_name().unwrap());
        count += 1;
    }
    assert!(count > 0, "no .pdf files under {dir}");
}
