//! Corpus readability gate: when `PDFTOTEXT_CORPUS_DIR` points at a folder of
//! real `.pdf` files, extract page 1 of each with our reader and assert the
//! output is non-garbage — the bar unpdf/pdfjs misses on CID/subset-font manuals.
//!
//! "No garbage" = low ratio of private-use (U+E000–U+F8FF), replacement
//! (U+FFFD), or non-whitespace control characters. We do NOT assert byte
//! equality with any other engine's output: this crate is clean-room (see the
//! README), so no third-party engine's output is used as an oracle. This gate is
//! the readability + no-glyph-wall contract.
//!
//! Skipped (not failed) when the env var is unset, so CI without the corpus
//! still passes. Run locally:
//!   scripts/fetch-corpus.sh
//!   PDFTOTEXT_CORPUS_DIR=corpus/pdfs cargo test --test parity -- --nocapture

// Form-XObject text is interpreted inline with its `Do` transform (CTM and form
// /Matrix), so imposed forms land where they are drawn; see tests/robustness.rs
// for the positioning regression tests.
use pdftotext::Document;

/// Maximum tolerated garbage ratio on any extracted page.
const MAX_GARBAGE_RATIO: f64 = 0.05;

fn is_garbage(c: char) -> bool {
    let u = c as u32;
    if u == 0xFFFD {
        return true;
    }
    if (0xE000..=0xF8FF).contains(&u) {
        return true; // private-use area — the CID glyph-code wall signature
    }
    if c.is_control() && c != '\n' && c != '\t' && c != '\r' {
        return true;
    }
    false
}

fn garbage_ratio(text: &str) -> (usize, usize) {
    let mut chars = 0usize;
    let mut garbage = 0usize;
    for c in text.chars() {
        chars += 1;
        if is_garbage(c) {
            garbage += 1;
        }
    }
    (garbage, chars)
}

#[test]
fn corpus_extracts_without_glyph_garbage() {
    let dir = match std::env::var("PDFTOTEXT_CORPUS_DIR") {
        Ok(d) => d,
        Err(_) => {
            eprintln!("skip: set PDFTOTEXT_CORPUS_DIR to a dir of .pdf files (scripts/fetch-corpus.sh fills corpus/pdfs)");
            return;
        }
    };

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let p = entry.expect("dir entry").path();
        if p.extension().is_none_or(|e| e != "pdf") {
            continue;
        }
        let bytes = std::fs::read(&p).expect("read pdf");
        let doc = Document::parse(&bytes).unwrap_or_else(|e| panic!("parse {:?}: {e}", p));
        // Sample up to 5 pages (or all if fewer).
        // Every page: a sample of the first few hides the chapters where
        // fonts change (a Japanese manual's first pages are often Latin).
        let pages = doc.page_count();
        let mut page_garbage_max = 0.0f64;
        let mut any_text = false;
        for i in 0..pages {
            let text = doc.extract_text(i, true).unwrap_or_default();
            if !text.trim().is_empty() {
                any_text = true;
            }
            let (g, n) = garbage_ratio(&text);
            if n > 0 {
                page_garbage_max = page_garbage_max.max(g as f64 / n as f64);
            }
        }
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if any_text && page_garbage_max > MAX_GARBAGE_RATIO {
            failures.push(format!("{name}: {:.1}% garbage", page_garbage_max * 100.0));
        }
        eprintln!(
            "{name}: pages={pages} max_garbage={:.2}%",
            page_garbage_max * 100.0
        );
        checked += 1;
    }
    assert!(checked > 0, "no .pdf files under {dir}");
    assert!(
        failures.is_empty(),
        "{} corpus PDF(s) exceeded {:.0}% garbage:\n  {}",
        failures.len(),
        MAX_GARBAGE_RATIO * 100.0,
        failures.join("\n  ")
    );
}
