//! Dev tool: dump a PDF's page count and the decoded content-stream head of its
//! first page. Useful while building the Phase 2 content interpreter.
//!
//! Usage: `cargo run --example dump -- path/to/file.pdf [page_index]`

use pdftotext::Document;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump <file.pdf> [page_index]");
    let page_idx: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(2);
    });
    let doc = Document::parse(&bytes).unwrap_or_else(|e| {
        eprintln!("parse failed: {e}");
        std::process::exit(2);
    });

    println!("pages: {}", doc.page_count());
    println!("extracted text (page {page_idx}):\n----- text -----");
    match doc.extract_text(page_idx, true) {
        Ok(t) => println!("{}", &t[..t.len().min(800)]),
        Err(e) => eprintln!("extract_text: {e}"),
    }
    println!("----- end text -----");
    match doc.page_content(page_idx) {
        Ok(c) => {
            let s = String::from_utf8_lossy(&c);
            let head = 600.min(s.len());
            println!("page {page_idx} decoded content ({} bytes):", c.len());
            println!("----- head -----");
            println!("{}", &s[..head]);
            println!("----- end ------");
        }
        Err(e) => eprintln!("page {page_idx} content: {e}"),
    }
}
