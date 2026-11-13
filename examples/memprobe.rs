//! Dev tool: heap held after parsing a PDF and the worst per-page transient
//! while extracting it. Counts bytes through a wrapping global allocator, so it
//! is exact and portable (unlike RSS, which native rayon threads and allocator
//! arenas inflate; the Worker runs single-threaded wasm).
//!
//! Usage: `cargo run --release --example memprobe -- file.pdf`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let n = LIVE.fetch_add(l.size(), Relaxed) + l.size();
        PEAK.fetch_max(n, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn mb(n: usize) -> String {
    format!("{:.1}MB", n as f64 / 1e6)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: memprobe <file.pdf>");
    let bytes = std::fs::read(&path).expect("read");
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let doc = pdftotext::Document::parse(&bytes).expect("parse");
    println!(
        "parse: held={} peak={}",
        mb(LIVE.load(Relaxed) - base),
        mb(PEAK.load(Relaxed) - base)
    );
    let mut worst = (0, 0usize);
    for i in 0..doc.page_count() {
        let before = LIVE.load(Relaxed);
        PEAK.store(before, Relaxed);
        let _ = doc.extract_text(i, true);
        let jump = PEAK.load(Relaxed) - before;
        if jump > worst.1 {
            worst = (i + 1, jump);
        }
    }
    println!(
        "pages: held={} worst page {} +{}",
        mb(LIVE.load(Relaxed) - base),
        worst.0,
        mb(worst.1)
    );
}
