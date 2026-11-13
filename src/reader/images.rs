//! Encoded image XObjects of a page, for OCR of scanned pages.
//!
//! A scanned manual page is one (or a few) raster images and no text. The OCR
//! caller wants the image bytes as they are stored, not decoded pixels: a
//! `DCTDecode` image is a complete JPEG file that a vision model accepts
//! directly, and decoding a 300-dpi page to RGBA costs 25-35 MB per page.
//!
//! [`Document::page_images`] walks a page's content stream (and the Form
//! XObjects it invokes) tracking only the CTM, and reports every image XObject
//! painted with `Do`: its pixel size, filters, colour space, where it lands on
//! the page, and, for JPEG (`DCTDecode`) and JPEG 2000 (`JPXDecode`), the
//! encoded bytes. CCITT fax and JBIG2 images are decoded to a 1-bit PNG
//! (`fax.rs`, `jbig2.rs`). Other codecs (Flate, uncompressed) are reported
//! with metadata only; the caller decides whether to rasterise them elsewhere.
//!
//! Needs a document from [`Document::parse_keeping_images`]: the default parse
//! drops image payloads, so its images report [`Omitted::NotLoaded`].
//!
//! Bounded like the rest of the crate: content and form streams decode under
//! the per-stream cap, a page reads at most [`MAX_FORM_BYTES_PER_PAGE`] of form
//! content and [`MAX_FORMS_PER_PAGE`] form invocations, forms nest at most
//! [`MAX_FORM_DEPTH`] deep, a page reports at most [`MAX_IMAGES_PER_PAGE`]
//! images, the lexer is iterative and every matrix is checked for finite values.

use std::collections::HashSet;

use lopdf::{Dictionary, Document as LoDoc, Object, ObjectId, Stream};

use super::{
    decode_stream, filter_names, flate_decode, is_flate, page_content_raw, MAX_STREAM_BYTES,
};
use crate::{Document, Result};

/// Images reported per page; the rest are ignored (`PageImages::truncated`).
pub const MAX_IMAGES_PER_PAGE: usize = 64;
/// Form XObject nesting depth followed from a page.
pub const MAX_FORM_DEPTH: usize = 8;
/// Form XObject invocations followed per page (fan-out guard).
pub const MAX_FORMS_PER_PAGE: usize = 512;
/// Decoded Form XObject content read per page.
pub const MAX_FORM_BYTES_PER_PAGE: usize = 64 << 20;
/// Operators interpreted per page, across the page and its forms.
pub const MAX_OPS_PER_PAGE: usize = 4_000_000;
/// `q` nesting tracked; deeper saves are counted but not stored.
const MAX_Q_DEPTH: usize = 256;
/// Coordinates beyond this are treated as hostile and the matrix is ignored.
const MAX_COORD: f64 = 1e9;

/// How an image's samples are encoded (from the last filter of its chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// `DCTDecode`: the payload is a JPEG file.
    Jpeg,
    /// `JPXDecode`: the payload is a JPEG 2000 file or codestream.
    Jpeg2000,
    /// `FlateDecode`, `LZWDecode` or `RunLengthDecode` over raw samples.
    Deflate,
    /// `JBIG2Decode` (bilevel, with an optional `/JBIG2Globals` stream).
    Jbig2,
    /// `CCITTFaxDecode` (bilevel fax).
    Ccitt,
    /// No compression filter: raw samples.
    Raw,
    /// Anything else (unknown or malformed filter chain).
    Other,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Jpeg => "jpeg",
            Codec::Jpeg2000 => "jpx",
            Codec::Deflate => "flate",
            Codec::Jbig2 => "jbig2",
            Codec::Ccitt => "ccitt",
            Codec::Raw => "raw",
            Codec::Other => "other",
        }
    }
}

/// Why an image has no `data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Omitted {
    /// Not a JPEG / JPEG 2000 / CCITT / JBIG2 image (or a CCITT / JBIG2
    /// stream that did not decode): the caller must rasterise it.
    NotEncoded,
    /// The document was parsed without image payloads.
    NotLoaded,
    /// The caller's byte budget ran out.
    Budget,
    /// The payload is larger than the per-stream cap.
    TooLarge,
    /// The payload does not start with a JPEG / JPEG 2000 signature.
    BadSignature,
    /// The filter chain in front of the JPEG cannot be undone here.
    FilterChain,
}

impl Omitted {
    pub fn as_str(self) -> &'static str {
        match self {
            Omitted::NotEncoded => "not_encoded",
            Omitted::NotLoaded => "not_loaded",
            Omitted::Budget => "budget",
            Omitted::TooLarge => "too_large",
            Omitted::BadSignature => "bad_signature",
            Omitted::FilterChain => "filter_chain",
        }
    }
}

/// Axis-aligned box in default user space (PDF points, origin bottom-left).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Rect {
    pub fn area(&self) -> f64 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    fn intersect(&self, o: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(o.x0),
            y0: self.y0.max(o.y0),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        }
    }
}

/// One image XObject painted on a page.
#[derive(Debug, Clone)]
pub struct PageImage {
    pub object_id: Option<ObjectId>,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: Option<u32>,
    /// Colour space name, or the family of an array colour space
    /// (`ICCBased`, `Indexed`, …). `None` for image masks.
    pub color_space: Option<String>,
    /// Filter chain, normalised to full names.
    pub filters: Vec<String>,
    pub codec: Codec,
    /// `/ImageMask true`: a 1-bit stencil painted in the fill colour.
    pub image_mask: bool,
    pub has_smask: bool,
    /// `/Decode [1 0 …]`: sample values are inverted when painted.
    pub decode_inverted: bool,
    /// Where the image lands on the page (bounding box of the transformed unit
    /// square). The largest placement wins when one image is painted twice.
    pub placed: Rect,
    /// Fraction of the page box the image covers, 0..=1.
    pub coverage: f64,
    /// Stored (still encoded) payload size.
    pub encoded_len: usize,
    /// The image file, when available: the stored JPEG / JPEG 2000, or a 1-bit
    /// PNG decoded from a CCITT fax or JBIG2 image. See `format`.
    pub data: Option<Vec<u8>>,
    /// What `data` is: `"jpeg"`, `"jpx"` or `"png"`.
    pub format: Option<&'static str>,
    pub omitted: Option<Omitted>,
}

/// Images of one page.
#[derive(Debug, Clone, Default)]
pub struct PageImages {
    /// 1-based page number.
    pub page_number: usize,
    /// The page's CropBox (else MediaBox), in user space.
    pub page_box: Rect,
    pub images: Vec<PageImage>,
    /// More than [`MAX_IMAGES_PER_PAGE`] images, or a work cap was reached.
    pub truncated: bool,
}

type Matrix = [f64; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `m × n` (apply `m`, then `n`), the PDF row-vector convention.
fn mul(m: &Matrix, n: &Matrix) -> Matrix {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn sane(m: &Matrix) -> bool {
    m.iter().all(|v| v.is_finite() && v.abs() <= MAX_COORD)
}

fn unit_square_bbox(m: &Matrix) -> Rect {
    let pts = [
        (m[4], m[5]),
        (m[0] + m[4], m[1] + m[5]),
        (m[2] + m[4], m[3] + m[5]),
        (m[0] + m[2] + m[4], m[1] + m[3] + m[5]),
    ];
    let mut r = Rect {
        x0: f64::INFINITY,
        y0: f64::INFINITY,
        x1: f64::NEG_INFINITY,
        y1: f64::NEG_INFINITY,
    };
    for (x, y) in pts {
        r.x0 = r.x0.min(x);
        r.y0 = r.y0.min(y);
        r.x1 = r.x1.max(x);
        r.y1 = r.y1.max(y);
    }
    r
}

fn resolve<'a>(doc: &'a LoDoc, obj: &'a Object) -> Option<&'a Object> {
    doc.dereference(obj).ok().map(|(_, o)| o)
}

fn dict_of(o: &Object) -> Option<&Dictionary> {
    match o {
        Object::Dictionary(d) => Some(d),
        Object::Stream(s) => Some(&s.dict),
        _ => None,
    }
}

fn number(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

fn read_rect(doc: &LoDoc, o: &Object) -> Option<Rect> {
    let Object::Array(a) = resolve(doc, o)? else {
        return None;
    };
    if a.len() != 4 {
        return None;
    }
    let mut v = [0.0; 4];
    for (slot, item) in v.iter_mut().zip(a) {
        *slot = number(resolve(doc, item)?)?;
        if !slot.is_finite() || slot.abs() > MAX_COORD {
            return None;
        }
    }
    Some(Rect {
        x0: v[0].min(v[2]),
        y0: v[1].min(v[3]),
        x1: v[0].max(v[2]),
        y1: v[1].max(v[3]),
    })
}

fn read_matrix(doc: &LoDoc, o: &Object) -> Option<Matrix> {
    let Object::Array(a) = resolve(doc, o)? else {
        return None;
    };
    if a.len() != 6 {
        return None;
    }
    let mut m = [0.0; 6];
    for (slot, item) in m.iter_mut().zip(a) {
        *slot = number(resolve(doc, item)?)?;
    }
    sane(&m).then_some(m)
}

/// Inherited page attribute (`/Resources`, `/CropBox`, `/MediaBox`): walks
/// `/Parent` with a visited set, at most 64 ancestors.
fn inherited<'a>(doc: &'a LoDoc, page_id: ObjectId, key: &[u8]) -> Option<&'a Object> {
    let mut seen = HashSet::new();
    let mut id = page_id;
    for _ in 0..64 {
        if !seen.insert(id) {
            return None;
        }
        let d = doc.get_dictionary(id).ok()?;
        if let Ok(v) = d.get(key) {
            return Some(v);
        }
        id = d.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

fn full_filter_name(n: &[u8]) -> String {
    let full: &[u8] = match n {
        b"DCT" => b"DCTDecode",
        b"Fl" => b"FlateDecode",
        b"LZW" => b"LZWDecode",
        b"RL" => b"RunLengthDecode",
        b"CCF" => b"CCITTFaxDecode",
        b"AHx" => b"ASCIIHexDecode",
        b"A85" => b"ASCII85Decode",
        other => other,
    };
    String::from_utf8_lossy(full).into_owned()
}

fn codec_of(filters: &[String]) -> Codec {
    match filters.last().map(String::as_str) {
        None => Codec::Raw,
        Some("DCTDecode") => Codec::Jpeg,
        Some("JPXDecode") => Codec::Jpeg2000,
        Some("JBIG2Decode") => Codec::Jbig2,
        Some("CCITTFaxDecode") => Codec::Ccitt,
        Some("FlateDecode" | "LZWDecode" | "RunLengthDecode") => Codec::Deflate,
        Some("ASCIIHexDecode" | "ASCII85Decode") => Codec::Raw,
        Some(_) => Codec::Other,
    }
}

fn has_signature(codec: Codec, data: &[u8]) -> bool {
    match codec {
        Codec::Jpeg => data.starts_with(&[0xFF, 0xD8]),
        // JP2 file (signature box) or a raw J2K codestream (SOC + SIZ).
        Codec::Jpeg2000 => {
            data.starts_with(b"\x00\x00\x00\x0cjP  \r\n\x87\n")
                || data.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
        }
        _ => false,
    }
}

/// Number of colour components a JPEG declares in its frame header (SOFn), or
/// `None` if no frame header is found in the first 1 MiB. 4 means CMYK/YCCK,
/// which vision models do not decode reliably.
pub fn jpeg_components(data: &[u8]) -> Option<u8> {
    let scan = &data[..data.len().min(1 << 20)];
    let mut i = 2; // after SOI
    while i + 4 <= scan.len() {
        if scan[i] != 0xFF {
            return None;
        }
        let marker = scan[i + 1];
        if marker == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if matches!(marker, 0x01 | 0xD0..=0xD7) {
            i += 2; // standalone markers
            continue;
        }
        let len = usize::from(u16::from_be_bytes([scan[i + 2], scan[i + 3]]));
        if len < 2 {
            return None;
        }
        // SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            return scan.get(i + 9).copied();
        }
        if marker == 0xDA {
            return None; // start of scan before any frame header
        }
        i += 2 + len;
    }
    None
}

fn color_space_name(doc: &LoDoc, d: &Dictionary) -> Option<String> {
    let cs = resolve(doc, d.get(b"ColorSpace").ok()?)?;
    let name = match cs {
        Object::Name(n) => n.as_slice(),
        Object::Array(a) => match a.first().and_then(|o| resolve(doc, o)) {
            Some(Object::Name(n)) => n.as_slice(),
            _ => return None,
        },
        _ => return None,
    };
    Some(String::from_utf8_lossy(name).into_owned())
}

fn decode_inverted(doc: &LoDoc, d: &Dictionary) -> bool {
    let Some(Object::Array(a)) = d.get(b"Decode").ok().and_then(|o| resolve(doc, o)) else {
        return false;
    };
    match (a.first().and_then(number), a.get(1).and_then(number)) {
        (Some(lo), Some(hi)) => lo > hi,
        _ => false,
    }
}

fn as_u32(doc: &LoDoc, d: &Dictionary, key: &[u8]) -> Option<u32> {
    match resolve(doc, d.get(key).ok()?)? {
        Object::Integer(i) => u32::try_from(*i).ok(),
        Object::Real(r) if r.is_finite() && *r >= 0.0 => Some(*r as u32),
        _ => None,
    }
}

/// The JPEG / JPEG 2000 bytes of an image stream, or why there are none.
/// Leading `FlateDecode` filters in front of the JPEG are undone (bounded).
fn encoded_payload(
    s: &Stream,
    codec: Codec,
    budget: usize,
) -> std::result::Result<Vec<u8>, Omitted> {
    if !matches!(codec, Codec::Jpeg | Codec::Jpeg2000) {
        return Err(Omitted::NotEncoded);
    }
    let names = filter_names(&s.dict);
    let prefix = &names[..names.len().saturating_sub(1)];
    let data: Vec<u8> = if prefix.is_empty() {
        if s.content.len() > MAX_STREAM_BYTES {
            return Err(Omitted::TooLarge);
        }
        if s.content.len() > budget {
            return Err(Omitted::Budget);
        }
        s.content.clone()
    } else if prefix.len() == 1 && is_flate(prefix[0]) {
        let out = flate_decode(&s.content, MAX_STREAM_BYTES.min(budget.saturating_add(1)));
        if out.len() > budget {
            return Err(if budget >= MAX_STREAM_BYTES {
                Omitted::TooLarge
            } else {
                Omitted::Budget
            });
        }
        out.into_owned()
    } else {
        return Err(Omitted::FilterChain);
    };
    if data.is_empty() {
        return Err(Omitted::NotLoaded);
    }
    if !has_signature(codec, &data) {
        return Err(Omitted::BadSignature);
    }
    Ok(data)
}

/// The stored bytes of a bilevel (CCITT / JBIG2) image with a leading Flate
/// undone, if any: only a single codec filter, or one behind a Flate.
fn bilevel_input(s: &Stream) -> std::result::Result<std::borrow::Cow<'_, [u8]>, Omitted> {
    let names = filter_names(&s.dict);
    let prefix = &names[..names.len().saturating_sub(1)];
    if s.content.is_empty() {
        return Err(Omitted::NotLoaded);
    }
    if s.content.len() > MAX_STREAM_BYTES {
        return Err(Omitted::TooLarge);
    }
    if prefix.is_empty() {
        Ok(std::borrow::Cow::Borrowed(s.content.as_slice()))
    } else if prefix.len() == 1 && is_flate(prefix[0]) {
        Ok(flate_decode(&s.content, MAX_STREAM_BYTES))
    } else {
        Err(Omitted::FilterChain)
    }
}

/// Why a bilevel decode produced no PNG.
fn bilevel_failure(e: super::png::PngError, budget: usize) -> Omitted {
    match e {
        super::png::PngError::TooBig if budget < MAX_STREAM_BYTES => Omitted::Budget,
        super::png::PngError::TooBig => Omitted::TooLarge,
        super::png::PngError::Undecodable => Omitted::NotEncoded,
    }
}

/// A CCITT fax image as a 1-bit PNG (bounded by the per-stream cap and the
/// caller's budget), or why not.
fn ccitt_payload(
    doc: &LoDoc,
    s: &Stream,
    width: u32,
    height: u32,
    inverted: bool,
    budget: usize,
) -> std::result::Result<Vec<u8>, Omitted> {
    let raw = bilevel_input(s)?;
    let params = super::fax::FaxParams::read(doc, &s.dict, width, height);
    super::fax::ccitt_to_png(&raw, &params, inverted, budget)
        .map_err(|e| bilevel_failure(e, budget))
}

/// The `/JBIG2Globals` stream of a `JBIG2Decode` image, decoded (bounded).
/// `Err` if the entry is present but is not a readable stream: the image's
/// symbols then cannot be resolved.
fn jbig2_globals<'a>(
    doc: &'a LoDoc,
    d: &'a Dictionary,
) -> std::result::Result<Option<std::borrow::Cow<'a, [u8]>>, Omitted> {
    let parms = match d.get(b"DecodeParms").ok().and_then(|o| resolve(doc, o)) {
        Some(Object::Dictionary(p)) => Some(p),
        // One entry per filter; the JBIG2 one is the last.
        Some(Object::Array(a)) => a
            .last()
            .and_then(|o| resolve(doc, o))
            .and_then(|o| o.as_dict().ok()),
        _ => None,
    };
    let Some(entry) = parms.and_then(|p| p.get(b"JBIG2Globals").ok()) else {
        return Ok(None);
    };
    match resolve(doc, entry) {
        Some(Object::Stream(g)) => Ok(Some(decode_stream(g, MAX_STREAM_BYTES))),
        Some(Object::Null) | None => Ok(None),
        _ => Err(Omitted::NotEncoded),
    }
}

/// A JBIG2 image as a 1-bit PNG (bounded by `jbig2.rs`'s work limits, the
/// per-stream cap and the caller's budget), or why not.
fn jbig2_payload(
    doc: &LoDoc,
    s: &Stream,
    inverted: bool,
    budget: usize,
) -> std::result::Result<Vec<u8>, Omitted> {
    let raw = bilevel_input(s)?;
    let globals = jbig2_globals(doc, &s.dict)?;
    super::jbig2::jbig2_to_png(&raw, globals.as_deref(), inverted, budget)
        .map_err(|e| bilevel_failure(e, budget))
}

struct Walk<'a> {
    doc: &'a LoDoc,
    page_box: Rect,
    images: Vec<PageImage>,
    truncated: bool,
    forms: usize,
    form_bytes: usize,
    ops: usize,
    /// Byte budget left for image data.
    budget: usize,
}

impl<'a> Walk<'a> {
    fn record_image(&mut self, id: Option<ObjectId>, obj: &'a Object, ctm: &Matrix) {
        let placed = unit_square_bbox(ctm);
        let page_area = self.page_box.area();
        let coverage = if page_area > 0.0 {
            (placed.intersect(&self.page_box).area() / page_area).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if let Some(prev) = self
            .images
            .iter_mut()
            .find(|i| id.is_some() && i.object_id == id)
        {
            if coverage > prev.coverage {
                prev.placed = placed;
                prev.coverage = coverage;
            }
            return;
        }
        if self.images.len() >= MAX_IMAGES_PER_PAGE {
            self.truncated = true;
            return;
        }
        let Some(d) = dict_of(obj) else { return };
        let doc = self.doc;
        let filters: Vec<String> = filter_names(d).into_iter().map(full_filter_name).collect();
        let codec = codec_of(&filters);
        let image_mask = matches!(
            d.get(b"ImageMask").ok().and_then(|o| resolve(doc, o)),
            Some(Object::Boolean(true))
        );
        let width = as_u32(doc, d, b"Width").unwrap_or(0);
        let height = as_u32(doc, d, b"Height").unwrap_or(0);
        let inverted = decode_inverted(doc, d);
        let payload = |s: &Stream, budget: usize| {
            if codec == Codec::Ccitt {
                ccitt_payload(doc, s, width, height, inverted, budget).map(|v| (v, "png"))
            } else if codec == Codec::Jbig2 {
                jbig2_payload(doc, s, inverted, budget).map(|v| (v, "png"))
            } else {
                let format = if codec == Codec::Jpeg { "jpeg" } else { "jpx" };
                encoded_payload(s, codec, budget).map(|v| (v, format))
            }
        };
        let (data, format, omitted, encoded_len) = match obj {
            Object::Stream(s) => match payload(s, self.budget) {
                Ok((v, f)) => {
                    self.budget -= v.len();
                    (Some(v), Some(f), None, s.content.len())
                }
                Err(why) => (None, None, Some(why), s.content.len()),
            },
            // The default parse keeps only the dictionary of an image.
            _ => (None, None, Some(Omitted::NotLoaded), 0),
        };
        self.images.push(PageImage {
            object_id: id,
            width,
            height,
            bits_per_component: as_u32(doc, d, b"BitsPerComponent"),
            color_space: if image_mask {
                None
            } else {
                color_space_name(doc, d)
            },
            filters,
            codec,
            image_mask,
            has_smask: d.has(b"SMask"),
            decode_inverted: inverted,
            placed,
            coverage,
            encoded_len,
            data,
            format,
            omitted,
        });
    }

    /// Interpret `content` with `resources`, starting from `base` CTM.
    fn run(
        &mut self,
        content: &[u8],
        resources: Option<&'a Dictionary>,
        base: Matrix,
        depth: usize,
        active: &mut Vec<ObjectId>,
    ) {
        let mut ctm = base;
        let mut stack: Vec<Matrix> = Vec::new();
        let mut overflow_q = 0usize;
        let mut nums: Vec<f64> = Vec::new();
        let mut last_name: Option<&[u8]> = None;
        let mut lx = Lexer { s: content, i: 0 };
        while let Some(tok) = lx.next() {
            match tok {
                Tok::Num(v) => {
                    if nums.len() >= 16 {
                        nums.remove(0);
                    }
                    nums.push(v);
                }
                Tok::Name(n) => last_name = Some(n),
                Tok::Other => {}
                Tok::Op(op) => {
                    self.ops += 1;
                    if self.ops > MAX_OPS_PER_PAGE {
                        self.truncated = true;
                        return;
                    }
                    match op {
                        b"q" => {
                            if stack.len() < MAX_Q_DEPTH {
                                stack.push(ctm);
                            } else {
                                overflow_q += 1;
                            }
                        }
                        b"Q" => {
                            if overflow_q > 0 {
                                overflow_q -= 1;
                            } else if let Some(m) = stack.pop() {
                                ctm = m;
                            }
                        }
                        b"cm" if nums.len() >= 6 => {
                            let k = nums.len() - 6;
                            let m = [
                                nums[k],
                                nums[k + 1],
                                nums[k + 2],
                                nums[k + 3],
                                nums[k + 4],
                                nums[k + 5],
                            ];
                            let next = mul(&m, &ctm);
                            if sane(&next) {
                                ctm = next;
                            }
                        }
                        b"Do" => {
                            if let Some(name) = last_name {
                                self.invoke(name, resources, &ctm, depth, active);
                            }
                        }
                        b"ID" => lx.skip_inline_image(),
                        _ => {}
                    }
                    nums.clear();
                    last_name = None;
                }
            }
        }
    }

    fn invoke(
        &mut self,
        name: &[u8],
        resources: Option<&'a Dictionary>,
        ctm: &Matrix,
        depth: usize,
        active: &mut Vec<ObjectId>,
    ) {
        let doc = self.doc;
        let Some(xobjects) = resources
            .and_then(|r| r.get(b"XObject").ok())
            .and_then(|o| resolve(doc, o))
            .and_then(dict_of)
        else {
            return;
        };
        let Ok(entry) = xobjects.get(name) else {
            return;
        };
        let id = entry.as_reference().ok();
        let Some(obj) = resolve(doc, entry) else {
            return;
        };
        let Some(d) = dict_of(obj) else { return };
        let subtype = match d.get(b"Subtype") {
            Ok(Object::Name(n)) => n.as_slice(),
            _ => return,
        };
        match subtype {
            b"Image" => self.record_image(id, obj, ctm),
            b"Form" => {
                let Object::Stream(s) = obj else { return };
                if depth >= MAX_FORM_DEPTH || self.forms >= MAX_FORMS_PER_PAGE {
                    self.truncated = true;
                    return;
                }
                if let Some(fid) = id {
                    if active.contains(&fid) {
                        return; // cycle
                    }
                }
                let remaining = MAX_FORM_BYTES_PER_PAGE.saturating_sub(self.form_bytes);
                if remaining == 0 {
                    self.truncated = true;
                    return;
                }
                self.forms += 1;
                let content = decode_stream(s, remaining.min(MAX_STREAM_BYTES));
                self.form_bytes += content.len();
                let m = d
                    .get(b"Matrix")
                    .ok()
                    .and_then(|o| read_matrix(doc, o))
                    .unwrap_or(IDENTITY);
                let form_ctm = mul(&m, ctm);
                if !sane(&form_ctm) {
                    return;
                }
                let form_res = d
                    .get(b"Resources")
                    .ok()
                    .and_then(|o| resolve(doc, o))
                    .and_then(dict_of)
                    .or(resources);
                if let Some(fid) = id {
                    active.push(fid);
                }
                self.run(&content, form_res, form_ctm, depth + 1, active);
                if id.is_some() {
                    active.pop();
                }
            }
            _ => {}
        }
    }
}

enum Tok<'s> {
    Num(f64),
    Name(&'s [u8]),
    Op(&'s [u8]),
    /// Strings, arrays, dictionaries, booleans: operands this walk ignores.
    Other,
}

fn is_ws(c: u8) -> bool {
    matches!(c, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn is_delim(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

/// Minimal content-stream lexer (ISO 32000-2 §7.2): iterative, never fails,
/// always advances.
struct Lexer<'s> {
    s: &'s [u8],
    i: usize,
}

impl<'s> Lexer<'s> {
    fn regular_run(&mut self) -> &'s [u8] {
        let start = self.i;
        while self.i < self.s.len() && !is_ws(self.s[self.i]) && !is_delim(self.s[self.i]) {
            self.i += 1;
        }
        &self.s[start..self.i]
    }

    fn next(&mut self) -> Option<Tok<'s>> {
        let s = self.s;
        loop {
            while self.i < s.len() && is_ws(s[self.i]) {
                self.i += 1;
            }
            let &c = s.get(self.i)?;
            match c {
                b'%' => {
                    while self.i < s.len() && s[self.i] != b'\n' && s[self.i] != b'\r' {
                        self.i += 1;
                    }
                }
                b'(' => {
                    self.skip_string();
                    return Some(Tok::Other);
                }
                b'<' if s.get(self.i + 1) == Some(&b'<') => {
                    self.i += 2;
                    return Some(Tok::Other);
                }
                b'>' if s.get(self.i + 1) == Some(&b'>') => {
                    self.i += 2;
                    return Some(Tok::Other);
                }
                b'<' => {
                    while self.i < s.len() && s[self.i] != b'>' {
                        self.i += 1;
                    }
                    self.i = (self.i + 1).min(s.len());
                    return Some(Tok::Other);
                }
                b'/' => {
                    self.i += 1;
                    return Some(Tok::Name(self.regular_run()));
                }
                b'[' | b']' | b'{' | b'}' | b')' | b'>' => {
                    self.i += 1;
                    return Some(Tok::Other);
                }
                _ => {
                    let run = self.regular_run();
                    if run.is_empty() {
                        self.i += 1; // unreachable in practice; guarantees progress
                        continue;
                    }
                    if matches!(run[0], b'0'..=b'9' | b'+' | b'-' | b'.') {
                        return Some(
                            match std::str::from_utf8(run)
                                .ok()
                                .and_then(|t| t.parse::<f64>().ok())
                            {
                                Some(v) if v.is_finite() => Tok::Num(v),
                                _ => Tok::Other,
                            },
                        );
                    }
                    return Some(match run {
                        b"true" | b"false" | b"null" => Tok::Other,
                        op => Tok::Op(op),
                    });
                }
            }
        }
    }

    fn skip_string(&mut self) {
        let s = self.s;
        self.i += 1;
        let mut depth = 1usize;
        while self.i < s.len() {
            match s[self.i] {
                b'\\' => self.i += 1,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        self.i = s.len();
    }

    /// After `ID`: skip the inline image's binary data up to an `EI` that
    /// stands alone between whitespace (or the end of the stream).
    fn skip_inline_image(&mut self) {
        let s = self.s;
        self.i = (self.i + 1).min(s.len()); // the single whitespace after ID
        while self.i + 1 < s.len() {
            if s[self.i] == b'E'
                && s[self.i + 1] == b'I'
                && self.i > 0
                && is_ws(s[self.i - 1])
                && s.get(self.i + 2).is_none_or(|&c| is_ws(c))
            {
                self.i += 2;
                return;
            }
            self.i += 1;
        }
        self.i = s.len();
    }
}

impl Document {
    /// Image XObjects painted on page `idx` (0-based), with JPEG / JPEG 2000
    /// payloads up to `budget` bytes in total; `budget` is decreased by what
    /// was returned. Use a document from [`Document::parse_keeping_images`].
    /// `Error::Unsupported` if the page index is out of range.
    pub fn page_images(&self, idx: usize, budget: &mut usize) -> Result<PageImages> {
        let page_id = self.page_id(idx)?;
        let doc = &self.doc;
        let page_box = inherited(doc, page_id, b"CropBox")
            .and_then(|o| read_rect(doc, o))
            .or_else(|| inherited(doc, page_id, b"MediaBox").and_then(|o| read_rect(doc, o)))
            .unwrap_or(Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 612.0,
                y1: 792.0,
            });
        let resources = inherited(doc, page_id, b"Resources")
            .and_then(|o| resolve(doc, o))
            .and_then(dict_of);
        let content = page_content_raw(doc, page_id);
        let mut walk = Walk {
            doc,
            page_box,
            images: Vec::new(),
            truncated: false,
            forms: 0,
            form_bytes: 0,
            ops: 0,
            budget: *budget,
        };
        walk.run(&content, resources, IDENTITY, 0, &mut Vec::new());
        *budget = walk.budget;
        Ok(PageImages {
            page_number: idx + 1,
            page_box,
            images: walk.images,
            truncated: walk.truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_order_matches_pdf_convention() {
        // Scale then translate: [2 0 0 3 0 0] cm, then [1 0 0 1 10 20] cm applied first.
        let m = mul(
            &[1.0, 0.0, 0.0, 1.0, 10.0, 20.0],
            &[2.0, 0.0, 0.0, 3.0, 0.0, 0.0],
        );
        assert_eq!(m, [2.0, 0.0, 0.0, 3.0, 20.0, 60.0]);
    }

    #[test]
    fn lexer_skips_strings_dicts_and_inline_images() {
        let src: &[u8] = b"(a \\) Do (nested (x)) ) <</K 1>> <414243> BI /W 1 ID \x00EI\xffEI EI 1 0 0 1 5 5 cm /Im0 Do";
        let mut lx = Lexer { s: src, i: 0 };
        let mut ops = Vec::new();
        while let Some(t) = lx.next() {
            if let Tok::Op(op) = t {
                ops.push(op.to_vec());
                if op == b"ID" {
                    lx.skip_inline_image();
                }
            }
        }
        assert_eq!(
            ops,
            vec![
                b"BI".to_vec(),
                b"ID".to_vec(),
                b"cm".to_vec(),
                b"Do".to_vec()
            ]
        );
    }

    #[test]
    fn lexer_terminates_on_garbage() {
        let junk: Vec<u8> = (0..20_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let mut lx = Lexer { s: &junk, i: 0 };
        let mut n = 0usize;
        while lx.next().is_some() {
            n += 1;
            assert!(n <= junk.len() + 1);
        }
    }

    #[test]
    fn jpeg_components_read_from_frame_header() {
        // SOI, APP0 (len 4), SOF0 (len 11: P=8, H=1, W=1, Nf=3, …).
        let jpg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00,
            0x01, 0x00, 0x01, 0x03,
        ];
        assert_eq!(jpeg_components(&jpg), Some(3));
        assert_eq!(jpeg_components(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]), None);
        assert_eq!(jpeg_components(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF]), None);
        assert_eq!(jpeg_components(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn jpeg_signature_checked() {
        assert!(has_signature(Codec::Jpeg, &[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!has_signature(Codec::Jpeg, b"GIF89a"));
        assert!(has_signature(Codec::Jpeg2000, &[0xFF, 0x4F, 0xFF, 0x51]));
    }
}
