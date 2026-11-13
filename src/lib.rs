//! pdftotext — clean-room PDF text extraction (ISO 32000-2).
//!
//! Library entry points used by the `pdftotext` binary, the wasm bridge and the
//! tests: the reader (`reader`), content interpreter (`content`) and text layer
//! (`text`) turn a PDF into per-page Unicode text.
//!
//! See `README.md` for the clean-room contribution rules.

pub mod content;
pub mod errors;
pub mod reader;
pub mod text;

/// WebAssembly bridge for the Cloudflare Worker — the same lib compiled to
/// `wasm32-unknown-unknown`. wasm-only; never compiled into the native `[[bin]]`.
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use content::{collect_runs, TextRun};
pub use errors::{Error, Result};
pub use reader::Document;

/// Extraction options. Mirrors poppler's `pdftotext` CLI flags.
#[derive(Debug, Clone)]
pub struct Options {
    /// `-layout` — preserve visual layout (best-effort).
    pub layout: bool,
    /// `-raw` — emit text in content-stream order, no layout.
    pub raw: bool,
    /// `-f` first page (1-based, inclusive).
    pub first_page: u32,
    /// `-l` last page (1-based, inclusive); None = to end.
    pub last_page: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            layout: false,
            raw: false,
            first_page: 1,
            last_page: None,
        }
    }
}

/// One page of extracted text.
#[derive(Debug, Clone, Default)]
pub struct PageText {
    pub text: String,
}

/// Quick PDF sanity check: a `%PDF-` header within the first 1024 bytes
/// (spec permits leading junk before the header).
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    let scan = bytes.len().min(1024);
    bytes[..scan].windows(5).any(|w| w == b"%PDF-")
}

#[allow(dead_code)] // retained as a documented fast-fallback; the reader is authoritative
/// Cheap page-count heuristic: counts `/Type /Page` (not `/Pages`) tokens in
/// the raw byte stream. Undercounts when pages live inside compressed object
/// streams (ObjStm, PDF 1.5+). Kept as a fast precheck/fallback; the
/// authoritative count is `page_count` via the reader's page-tree walk.
pub(crate) fn page_count_heuristic(bytes: &[u8]) -> usize {
    let mut count = 0usize;
    let n = bytes.len();
    let mut i = 0usize;
    while i + 5 <= n {
        if &bytes[i..i + 5] == b"/Type" {
            let mut j = i + 5;
            while j < n && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j + 5 <= n && &bytes[j..j + 5] == b"/Page" {
                let next = bytes.get(j + 5).copied();
                if next != Some(b's') && next != Some(b'S') {
                    count += 1;
                }
            }
            i = j + 5;
        } else {
            i += 1;
        }
    }
    count.max(1)
}

/// Authoritative page count via the reader's page-tree walk (handles `/Count`
/// inheritance and compressed object streams). `Error::NotPdf` if the header is
/// absent; `Error::Unsupported` if lopdf cannot parse the structure.
pub fn page_count(bytes: &[u8]) -> Result<usize> {
    if !looks_like_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    Ok(Document::parse(bytes)?.page_count())
}

/// `READY` gates `extract`: `false` makes it return `Unsupported`, so a caller
/// with a fallback extractor switches to it (a kill switch for deployments).
/// Kept as a kill-switch — flip to `false` to disable the Rust extractor without
/// touching the server.
pub const READY: bool = true;

/// Pure 1-based inclusive page-range slice. `last` is clamped to the page
/// count (as `-l 999` means "to the end"); a range that starts after it, or
/// after `last`, is refused rather than silently moved to another page.
fn slice_range(total: u32, opts: &Options) -> Option<(u32, u32)> {
    let last = opts.last_page.unwrap_or(total).min(total);
    let first = opts.first_page.max(1);
    (first <= last).then_some((first, last))
}

/// Extract per-page text. When `READY` is false this returns `Unsupported` and
/// the caller degrades to its fallback; otherwise each page's text is decoded
/// via the text layer (Differences/AGL, ToUnicode, CTM-aware XObject recursion,
/// line-break reconstruction). A document with no pages is `Unsupported`.
pub fn extract(bytes: &[u8], opts: &Options) -> Result<Vec<PageText>> {
    if !READY {
        return Err(Error::Unsupported(
            "text extraction disabled (READY=false); falling back".into(),
        ));
    }
    if !looks_like_pdf(bytes) {
        return Err(Error::NotPdf);
    }
    let doc = Document::parse(bytes)?;
    let total = u32::try_from(doc.page_count()).unwrap_or(u32::MAX);
    if total == 0 {
        // lopdf found no pages (broken page tree); let the caller fall back to a
        // more forgiving extractor rather than report an empty document.
        return Err(Error::Unsupported("no pages found in page tree".into()));
    }
    let Some((first, last)) = slice_range(total, opts) else {
        return Err(Error::Unsupported(format!(
            "page range {}..{} is outside the document's {total} pages",
            opts.first_page,
            opts.last_page.map_or(total.to_string(), |l| l.to_string()),
        )));
    };
    let mut pages = Vec::new();
    for i in first..=last {
        let mode = if opts.layout {
            text::Mode::Layout
        } else if opts.raw {
            text::Mode::Raw
        } else {
            text::Mode::Reading
        };
        let text = doc.extract_text_mode((i - 1) as usize, mode)?;
        pages.push(PageText { text });
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_pdf() {
        assert!(matches!(page_count(b"hello world"), Err(Error::NotPdf)));
        assert!(!looks_like_pdf(b"plain text"));
    }

    #[test]
    fn accepts_header_after_leading_junk() {
        // Spec permits up to 1024 bytes before %PDF-.
        let mut buf = vec![b'%'; 512];
        buf.extend_from_slice(b"%PDF-1.7\nstuff");
        assert!(looks_like_pdf(&buf));
    }

    #[test]
    fn counts_type_page_tokens_ignores_pages() {
        let pdf = b"%PDF-1.7\n/Type /Page\n/Type /Pages\n/Type/Page\n/Type /Pages\n";
        // /Type /Page (count) + /Type /Pages (skip) + /Type/Page (count) + /Type /Pages (skip) = 2.
        assert_eq!(page_count_heuristic(pdf), 2);
    }

    #[test]
    fn extract_rejects_non_pdf() {
        // A non-PDF is rejected at parse.
        assert!(matches!(
            extract(b"plain text", &Options::default()),
            Err(Error::NotPdf)
        ));
    }

    #[test]
    fn slice_range_clamps_to_total() {
        let opts = Options {
            first_page: 2,
            last_page: Some(3),
            ..Options::default()
        };
        // 4-page doc, first=2 last=3 → pages 2..3.
        assert_eq!(slice_range(4, &opts), Some((2, 3)));
    }

    #[test]
    fn slice_range_refuses_start_past_end() {
        let opts = Options {
            first_page: 5,
            last_page: None,
            ..Options::default()
        };
        // total=2: page 5 does not exist, so nothing (not page 2).
        assert_eq!(slice_range(2, &opts), None);
    }

    #[test]
    fn slice_range_refuses_inverted_range() {
        let opts = Options {
            first_page: 10,
            last_page: Some(3),
            ..Options::default()
        };
        // first=10 > last=3: an empty range is refused, not moved to page 3.
        assert_eq!(slice_range(5, &opts), None);
        // first_page 0 (library callers) means page 1, not an underflow.
        let zero = Options {
            first_page: 0,
            ..Options::default()
        };
        assert_eq!(slice_range(5, &zero), Some((1, 5)));
    }
}
