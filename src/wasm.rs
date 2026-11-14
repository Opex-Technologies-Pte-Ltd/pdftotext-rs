//! WebAssembly bridge — the Cloudflare Worker entry into this crate.
//!
//! The same `pdftotext` lib compiled to `wasm32-unknown-unknown`; output is
//! bit-identical to the native binary because the lib path is deterministic (no
//! `std::time`/`thread`/`fs`/`getrandom` in `src/lib.rs`'s reach).
//!
//! `extract_pdf(bytes, layout)` returns `{ pageCount, pages: [{ pageNumber,
//! text, charCount, sectionHeaders }] }` — per-page text via the text layer
//! (Differences/AGL/ToUnicode/CTM-aware XObject recursion) plus font-size-detected
//! section headers, the same shape `unpdf` produced so the Worker chunker is
//! unchanged. Whole file is wasm-only — never compiled into the native
//! `[[bin]]`.

#![cfg(target_arch = "wasm32")]
#![allow(non_snake_case)] // JS-facing field names are camelCase by convention

use crate::{Document, Error};
use serde::Serialize;
use wasm_bindgen::prelude::*;

#[derive(Serialize)]
struct PageOut {
    pageNumber: usize,
    text: String,
    charCount: usize,
    sectionHeaders: Vec<String>,
}

#[derive(Serialize)]
struct ExtractResult {
    pageCount: usize,
    pages: Vec<PageOut>,
}

/// Extract per-page text from a PDF buffer. Returns `{ pageCount, pages: [{
/// pageNumber, text, charCount, sectionHeaders }] }`. `layout` mirrors poppler's
/// `-layout` flag: true = physical layout (what `pdftotext -layout` prints);
/// false = reading order, with columns of running text one after the other —
/// the better input for chunking and search.
///
/// Failures throw a JS string beginning with a stable tag — `not_pdf:`,
/// `unsupported:`, `encrypted:` or `limit:` (see `Error`'s `Display`) — and never
/// return partial results: a document that needs a password, has no pages, or
/// trips a work/memory ceiling is an error, so the caller can fall back.
///
/// Panics: the release profile is `panic = "abort"`, so a panic is a wasm trap
/// (`RuntimeError: unreachable`), not an unwind. The wasm instance must be
/// considered poisoned afterwards (its shadow stack is not unwound); the TS
/// loader discards and re-instantiates it. `console_error_panic_hook` logs the
/// panic message first.
#[wasm_bindgen]
pub fn extract_pdf(bytes: &[u8], layout: bool) -> Result<JsValue, JsValue> {
    init_panic_hook();
    run(bytes, layout).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn run(bytes: &[u8], layout: bool) -> crate::Result<JsValue> {
    if !crate::looks_like_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    let doc = Document::parse(bytes)?;
    let total = doc.page_count();
    if total == 0 {
        return Err(Error::Unsupported("no pages found in page tree".into()));
    }
    let mut pages = Vec::with_capacity(total);
    for idx in 0..total {
        let (text, section_headers) = doc.extract_text_with_headers(idx, layout)?;
        let char_count = text.chars().count();
        pages.push(PageOut {
            pageNumber: idx + 1,
            text,
            charCount: char_count,
            sectionHeaders: section_headers,
        });
    }
    let result = ExtractResult {
        pageCount: total,
        pages,
    };
    serde_wasm_bindgen::to_value(&result).map_err(|e| Error::Unsupported(format!("serialize: {e}")))
}

// ── Memory ceiling ─────────────────────────────────────────────────────────
//
// The module is linked with a hard `--max-memory` (see build.rs). When an
// allocation fails at that ceiling, Rust aborts, which in wasm is the same
// `unreachable` trap as a panic. This allocator wrapper records the failure so
// the loader can tell "the document needs more memory than the Worker has"
// (route it to the extraction container) from a bug (fall back).

struct Tracking;

static ALLOC_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn note(p: *mut u8) -> *mut u8 {
    if p.is_null() {
        ALLOC_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    p
}

// SAFETY: forwards every call unchanged to the system allocator.
unsafe impl std::alloc::GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        note(unsafe { std::alloc::System.alloc(l) })
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        note(unsafe { std::alloc::System.alloc_zeroed(l) })
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        note(unsafe { std::alloc::System.realloc(p, l, n) })
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) }
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

/// True once an allocation has failed in this instance (the memory ceiling was
/// reached). Safe to call on an instance that has just trapped: it only reads
/// a static, no shadow stack or allocation.
#[wasm_bindgen]
pub fn alloc_failed() -> bool {
    ALLOC_FAILED.load(std::sync::atomic::Ordering::Relaxed)
}

fn init_panic_hook() {
    // set_once is internally idempotent; the Once guard is belt-and-suspenders.
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(console_error_panic_hook::set_once);
}

// ── Encoded page images (OCR of scanned pages) ─────────────────────────────

/// Bytes serialised as a JS `Uint8Array` (serde's default for `Vec<u8>` is an
/// array of numbers).
struct Bytes(Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_bytes(&self.0)
    }
}

#[derive(Serialize)]
struct RectOut {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl From<crate::reader::images::Rect> for RectOut {
    fn from(r: crate::reader::images::Rect) -> Self {
        RectOut {
            x0: r.x0,
            y0: r.y0,
            x1: r.x1,
            y1: r.y1,
        }
    }
}

#[derive(Serialize)]
struct ImageOut {
    width: u32,
    height: u32,
    bitsPerComponent: Option<u32>,
    colorSpace: Option<String>,
    filters: Vec<String>,
    /// `jpeg` | `jpx` | `flate` | `jbig2` | `ccitt` | `raw` | `other`
    codec: &'static str,
    /// Colour components in the JPEG frame header (jpeg only).
    jpegComponents: Option<u8>,
    imageMask: bool,
    hasSMask: bool,
    decodeInverted: bool,
    placed: RectOut,
    coverage: f64,
    encodedLen: usize,
    /// `"jpeg"`, `"jpx"` or `"png"` (a CCITT fax or JBIG2 image decoded to
    /// PNG; one wider than 4000 px is shrunk, so read the PNG header for its
    /// size).
    format: Option<&'static str>,
    data: Option<Bytes>,
    /// Why `data` is absent: `not_encoded` | `not_loaded` | `budget` |
    /// `too_large` | `bad_signature` | `filter_chain`
    omitted: Option<&'static str>,
}

#[derive(Serialize)]
struct PageImagesOut {
    pageNumber: usize,
    pageBox: RectOut,
    truncated: bool,
    images: Vec<ImageOut>,
}

#[derive(Serialize)]
struct PageImagesResult {
    pageCount: usize,
    pages: Vec<PageImagesOut>,
}

/// Encoded image XObjects of the given 1-based `pages`, in order: JPEG
/// (`DCTDecode`) and JPEG 2000 (`JPXDecode`) payloads as stored, CCITT fax
/// and JBIG2 images decoded to PNG (`format`), other codecs
/// as metadata only (see `reader::images`). Returns `{ pageCount, pages: [{
/// pageNumber, pageBox, truncated, images: [...] }] }`.
///
/// `budget` caps the image bytes returned by one call. Pages are processed in
/// order and the call stops BEFORE the first page whose images no longer fit,
/// so `pages` may be a prefix of the request: call again with the rest. The
/// first page is always returned (its oversized images then carry `omitted:
/// "budget"`), so every call makes progress. Out-of-range page numbers are
/// skipped. Errors and traps as for `extract_pdf`.
#[wasm_bindgen]
pub fn page_images(bytes: &[u8], pages: &[u32], budget: usize) -> Result<JsValue, JsValue> {
    init_panic_hook();
    run_page_images(bytes, pages, budget).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn run_page_images(bytes: &[u8], pages: &[u32], budget: usize) -> crate::Result<JsValue> {
    use crate::reader::images::{jpeg_components, Omitted};
    if !crate::looks_like_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    let doc = Document::parse_keeping_images(bytes)?;
    let total = doc.page_count();
    let mut out = Vec::new();
    let mut left = budget;
    for &n in pages {
        let Some(idx) = (n as usize).checked_sub(1).filter(|&i| i < total) else {
            continue;
        };
        let mut try_left = left;
        let p = doc.page_images(idx, &mut try_left)?;
        let over = p.images.iter().any(|i| i.omitted == Some(Omitted::Budget));
        if over && !out.is_empty() {
            break;
        }
        left = try_left;
        out.push(PageImagesOut {
            pageNumber: p.page_number,
            pageBox: p.page_box.into(),
            truncated: p.truncated,
            images: p
                .images
                .into_iter()
                .map(|i| ImageOut {
                    width: i.width,
                    height: i.height,
                    bitsPerComponent: i.bits_per_component,
                    colorSpace: i.color_space,
                    filters: i.filters,
                    codec: i.codec.as_str(),
                    jpegComponents: match (&i.data, i.codec) {
                        (Some(d), crate::reader::images::Codec::Jpeg) => jpeg_components(d),
                        _ => None,
                    },
                    imageMask: i.image_mask,
                    hasSMask: i.has_smask,
                    decodeInverted: i.decode_inverted,
                    placed: i.placed.into(),
                    coverage: i.coverage,
                    encodedLen: i.encoded_len,
                    format: i.format,
                    data: i.data.map(Bytes),
                    omitted: i.omitted.map(Omitted::as_str),
                })
                .collect(),
        });
    }
    let result = PageImagesResult {
        pageCount: total,
        pages: out,
    };
    serde_wasm_bindgen::to_value(&result).map_err(|e| Error::Unsupported(format!("serialize: {e}")))
}
