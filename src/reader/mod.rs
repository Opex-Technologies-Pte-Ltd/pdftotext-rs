//! PDF structure reader.
//!
//! Thin wrapper over `lopdf` (clean-room, MIT). lopdf owns the hard parts —
//! cross-reference table + cross-reference streams (PDF 1.5+), object streams
//! (`ObjStm`), indirect-object resolution, standard-security-handler decryption
//! with the empty user password, and the page-tree walk (which carries its own
//! cycle/depth guards).
//!
//! Stream decoding is ours, because content streams are untrusted input and
//! every decode must be bounded: FlateDecode is inflated with `flate2` under a
//! hard output cap (zlib first, raw-deflate fallback, partial output kept for
//! truncated/corrupt tails). Multi-filter chains made of Flate/LZW/ASCII85 go
//! through lopdf's bounded decoder. Anything else (RunLength, ASCIIHex, image
//! codecs…) is returned as raw bytes — never present in text-bearing content.
//!
//! What lopdf does NOT give us: glyph→unicode mapping, CID/ToUnicode fallbacks,
//! layout reconstruction. That lives in the `text` module, implemented from the
//! spec — never from poppler.

use std::borrow::Cow;
use std::io::Read;

use flate2::read::{DeflateDecoder, ZlibDecoder};
use lopdf::{Dictionary, Document as LoDoc, LoadOptions, Object, ObjectId, Stream};

use crate::text::Shared;
use crate::{Error, Result};

mod fax;
pub mod images;
mod jbig2;
mod png;

/// Hard cap on one decoded stream (content stream, form, ObjStm, xref stream).
/// A stream that inflates past this is truncated (content) or rejected (lopdf's
/// own object-stream decode at load time).
pub const MAX_STREAM_BYTES: usize = 64 << 20;

/// Hard cap on the concatenated decoded content of one page.
pub const MAX_PAGE_CONTENT_BYTES: usize = 64 << 20;

/// Cap on the number of content streams read for one page.
const MAX_CONTENT_STREAMS_PER_PAGE: usize = 10_000;

fn filter_names(dict: &Dictionary) -> Vec<&[u8]> {
    match dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![n.as_slice()],
        Ok(Object::Array(a)) => a
            .iter()
            .filter_map(|o| match o {
                Object::Name(n) => Some(n.as_slice()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn is_flate(name: &[u8]) -> bool {
    name == b"FlateDecode" || name == b"Fl"
}

fn clamp(bytes: &[u8], limit: usize) -> &[u8] {
    &bytes[..bytes.len().min(limit)]
}

fn flate_decode(content: &[u8], limit: usize) -> Cow<'_, [u8]> {
    let cap = limit as u64;
    let mut out = Vec::new();
    // Keep whatever inflated before an error: truncated or corrupt tails are
    // common in the wild and the leading content is still good text.
    let _ = ZlibDecoder::new(content).take(cap).read_to_end(&mut out);
    if !out.is_empty() {
        return Cow::Owned(out);
    }
    let _ = DeflateDecoder::new(content).take(cap).read_to_end(&mut out);
    if out.is_empty() {
        Cow::Borrowed(clamp(content, limit))
    } else {
        Cow::Owned(out)
    }
}

/// Decode a stream payload, bounded by `limit` output bytes. Uncompressed
/// streams are returned borrowed (no copy).
pub fn decode_stream(s: &Stream, limit: usize) -> Cow<'_, [u8]> {
    let names = filter_names(&s.dict);
    if names.is_empty() {
        return Cow::Borrowed(clamp(&s.content, limit));
    }
    if names.len() == 1 && is_flate(names[0]) {
        return flate_decode(&s.content, limit);
    }
    let supported = names
        .iter()
        .all(|n| matches!(*n, b"FlateDecode" | b"LZWDecode" | b"ASCII85Decode"));
    if supported {
        // lopdf's bounded multi-filter decoder; an oversized or corrupt chain
        // yields nothing rather than raw compressed bytes.
        return match s.decompressed_content_with_limit(limit) {
            Ok(v) => Cow::Owned(v),
            Err(_) => Cow::Owned(Vec::new()),
        };
    }
    Cow::Borrowed(clamp(&s.content, limit))
}

/// Decode a stream under the default [`MAX_STREAM_BYTES`] cap.
pub fn inflate_stream(s: &Stream) -> Cow<'_, [u8]> {
    decode_stream(s, MAX_STREAM_BYTES)
}

/// Resolve an object (following an indirect reference) to its decoded stream
/// content, bounded by `limit` output bytes.
pub fn object_stream_content(doc: &LoDoc, obj: &Object, limit: usize) -> Option<Vec<u8>> {
    let o = match obj {
        Object::Reference(r) => doc.get_object(*r).ok()?,
        _ => obj,
    };
    match o {
        Object::Stream(s) => Some(decode_stream(s, limit).into_owned()),
        _ => None,
    }
}

/// Concatenate a page's content stream(s). `/Contents` may be a stream, an
/// array of streams, or an indirect reference to either; each is decoded under
/// the per-stream cap and the whole page under [`MAX_PAGE_CONTENT_BYTES`].
pub fn page_content_raw(doc: &LoDoc, page_id: ObjectId) -> Vec<u8> {
    let mut out = Vec::new();
    let Ok(dict) = doc.get_dictionary(page_id) else {
        return out;
    };
    let Ok(contents) = dict.get(b"Contents") else {
        return out;
    };
    let mut streams = 0usize;
    append_contents(doc, &mut out, contents, 0, &mut streams);
    out
}

fn append_contents(
    doc: &LoDoc,
    out: &mut Vec<u8>,
    obj: &Object,
    depth: usize,
    streams: &mut usize,
) {
    if out.len() >= MAX_PAGE_CONTENT_BYTES || *streams >= MAX_CONTENT_STREAMS_PER_PAGE || depth > 2
    {
        return;
    }
    let resolved = match obj {
        Object::Reference(r) => match doc.get_object(*r) {
            Ok(o) => o,
            Err(_) => return,
        },
        o => o,
    };
    match resolved {
        Object::Array(items) => {
            for item in items {
                append_contents(doc, out, item, depth + 1, streams);
            }
        }
        Object::Stream(s) => {
            *streams += 1;
            let remaining = MAX_PAGE_CONTENT_BYTES - out.len();
            let bytes = decode_stream(s, remaining.min(MAX_STREAM_BYTES));
            if bytes.is_empty() {
                return;
            }
            if !out.is_empty() && !out.ends_with(b"\n") {
                out.push(b'\n'); // keep operators from separate streams distinct
            }
            out.extend_from_slice(&bytes);
        }
        _ => {}
    }
}

/// Parsed PDF. Owns the lopdf document, the flattened page list, and the
/// per-document font cache + decode budget shared by every page extraction.
pub struct Document {
    doc: LoDoc,
    pages: Vec<ObjectId>,
    shared: Shared,
}

/// Load-time filter: drop what text extraction never reads, so it does not stay
/// in memory. lopdf keeps every number and name of a parsed document as a
/// 120-byte node, so a 23 MB catalogue parsed eagerly costs ~100 MB; in a
/// 128 MB Worker that decides whether a manual can be read at all.
///
/// - Payloads of image XObjects, embedded font programs (`/Length1`, or a
///   `/Subtype` of `Type1C`/`CIDFontType0C`/`OpenType`), XMP metadata and
///   embedded files. Their dictionaries stay, so resource lookups still
///   resolve and an image `Do` is still recognised and skipped.
/// - Whole objects that hold no text: annotations and link actions, named
///   destinations and the name trees indexing them, bookmarks, tagged-PDF
///   structure elements, shadings, functions and shading patterns. They
///   become `null`.
fn drop_unread_payloads(id: ObjectId, obj: &mut Object) -> Option<(ObjectId, Object)> {
    drop_unread(id, obj, false)
}

/// [`drop_unread_payloads`] for [`Document::parse_keeping_images`]: the same
/// filter, except image XObject payloads stay (the OCR path reads them).
fn drop_unread_keep_images(id: ObjectId, obj: &mut Object) -> Option<(ObjectId, Object)> {
    drop_unread(id, obj, true)
}

fn drop_unread(id: ObjectId, obj: &mut Object, keep_images: bool) -> Option<(ObjectId, Object)> {
    let dict = match &*obj {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    };
    if let Some(d) = dict {
        let name = |key: &[u8]| match d.get(key) {
            Ok(Object::Name(n)) => Some(n.as_slice()),
            _ => None,
        };
        let no_text = matches!(
            name(b"Type"),
            Some(b"Annot" | b"StructElem" | b"MCR" | b"OBJR" | b"Action")
        )
            // Named destinations (`<< /D [page /XYZ …] >>`, one per link
            // target: a 560-page manual can carry 140 000 of them), link
            // actions, and the name / number tree nodes that index them.
            || (d.has(b"D") && d.len() <= 2 && !d.has(b"Type"))
            || (d.has(b"S") && (d.has(b"D") || d.has(b"URI")) && d.len() <= 3)
            || (d.has(b"Limits") && (d.has(b"Names") || d.has(b"Nums") || d.has(b"Kids")))
            // Bookmarks (outline items).
            || (d.has(b"Title") && d.has(b"Parent"))
            || d.has(b"ShadingType")
            || d.has(b"FunctionType")
            || matches!(d.get(b"PatternType"), Ok(Object::Integer(2)));
        let unread_payload = match name(b"Subtype") {
            Some(b"Image") => !keep_images,
            Some(b"Type1C" | b"CIDFontType0C" | b"OpenType") => true,
            _ => false,
        } || matches!(name(b"Type"), Some(b"Metadata" | b"EmbeddedFile"))
            || d.has(b"Length1");
        if no_text {
            *obj = Object::Null;
        } else if let (true, Object::Stream(s)) = (unread_payload, &mut *obj) {
            // Keep only the dictionary. Emptying `content` is not enough:
            // lopdf re-reads every empty stream from the file after the
            // filter runs. A bare dictionary is never re-read.
            let dict = std::mem::take(&mut s.dict);
            *obj = Object::Dictionary(dict);
        }
    }
    // lopdf keeps the mutated `obj` for top-level objects and only checks that
    // the result is `Some`; for objects inside object streams it keeps the
    // returned copy instead. Those are never streams, so the clone is small.
    match obj {
        Object::Stream(_) => Some((id, Object::Null)),
        other => Some((id, other.clone())),
    }
}

impl Document {
    /// Parse. Errors:
    /// - `Error::Encrypted` — needs a password, or an encryption scheme lopdf
    ///   cannot open. (Owner-password-only files open with the empty user
    ///   password transparently.)
    /// - `Error::Limit` — an object/xref stream inflates past
    ///   [`MAX_STREAM_BYTES`].
    /// - `Error::Unsupported` — anything else lopdf cannot read.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_filter(bytes, drop_unread_payloads)
    }

    /// [`Document::parse`] that keeps image XObject payloads, for
    /// [`Document::page_images`]. Text extraction works on it too, but it holds
    /// every image in memory: use [`Document::parse`] unless you need the bytes.
    pub fn parse_keeping_images(bytes: &[u8]) -> Result<Self> {
        Self::parse_with_filter(bytes, drop_unread_keep_images)
    }

    fn parse_with_filter(bytes: &[u8], filter: lopdf::FilterFunc) -> Result<Self> {
        let opts = LoadOptions {
            max_decompressed_size: Some(MAX_STREAM_BYTES),
            filter: Some(filter),
            ..LoadOptions::default()
        };
        let doc = LoDoc::load_mem_with_options(bytes, opts).map_err(classify_load_error)?;
        // An /Encrypt entry that lopdf did not act on (e.g. a direct dictionary
        // instead of a reference) leaves ciphertext streams in place; extracting
        // from them yields garbage, so refuse up front.
        if doc.trailer.get(b"Encrypt").is_ok() && !doc.was_encrypted() {
            return Err(Error::Encrypted(
                "encryption dictionary could not be applied".into(),
            ));
        }
        let pages: Vec<ObjectId> = doc.page_iter().collect();
        Ok(Self {
            doc,
            pages,
            shared: Shared::new(),
        })
    }

    /// Page count via the real page-tree walk (handles `/Count` inheritance and
    /// compressed object streams).
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    fn page_id(&self, idx: usize) -> Result<ObjectId> {
        self.pages.get(idx).copied().ok_or_else(|| {
            Error::Unsupported(format!(
                "page {} out of {} total",
                idx + 1,
                self.pages.len()
            ))
        })
    }

    /// Decoded content-stream bytes for page `idx` (0-based). Returns an empty
    /// `Vec` for a page that has no content stream. `Error::Unsupported` if the
    /// page index is out of range.
    pub fn page_content(&self, idx: usize) -> Result<Vec<u8>> {
        Ok(page_content_raw(&self.doc, self.page_id(idx)?))
    }

    /// Text runs for page `idx` (0-based), in content-stream order. The run
    /// bytes are raw show-string bytes; glyph→unicode mapping happens in the
    /// text layer. Form XObjects are not followed.
    pub fn page_runs(&self, idx: usize) -> Result<Vec<crate::TextRun>> {
        let content = self.page_content(idx)?;
        Ok(crate::content::collect_runs(&content))
    }

    /// Readable Unicode text for page `idx` (0-based), including text imposed
    /// inside Form XObjects. `layout`: physical layout; otherwise reading order.
    pub fn extract_text(&self, idx: usize, layout: bool) -> Result<String> {
        self.extract_text_mode(idx, crate::text::Mode::from_layout(layout))
    }

    /// Text of page `idx` in the given arrangement.
    pub fn extract_text_mode(&self, idx: usize, mode: crate::text::Mode) -> Result<String> {
        let page_id = self.page_id(idx)?;
        Ok(crate::text::page_text_with_headers(&self.doc, &self.shared, page_id, mode)?.0)
    }

    /// Readable Unicode text for page `idx` PLUS its section headers (detected
    /// by the font-size heuristic in `text`). The wasm Worker path uses this so
    /// it can surface the same `sectionHeaders` unpdf produced (the chunker
    /// titles chunks off them).
    pub fn extract_text_with_headers(
        &self,
        idx: usize,
        layout: bool,
    ) -> Result<(String, Vec<String>)> {
        let page_id = self.page_id(idx)?;
        crate::text::page_text_with_headers(
            &self.doc,
            &self.shared,
            page_id,
            crate::text::Mode::from_layout(layout),
        )
    }
}

fn classify_load_error(e: lopdf::Error) -> Error {
    use lopdf::{DecompressError, Error as L};
    match e {
        L::InvalidPassword | L::Decryption(_) | L::UnsupportedSecurityHandler(_) => {
            Error::Encrypted(e.to_string())
        }
        L::Decompress(DecompressError::MemoryLimitExceeded { .. }) => {
            Error::Limit(format!("parse: {e}"))
        }
        other => Error::Unsupported(format!("parse: {other}")),
    }
}
