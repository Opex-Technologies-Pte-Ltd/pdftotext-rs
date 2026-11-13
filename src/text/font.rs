//! Font resolution: turn a font dictionary into something that can decode
//! show-string bytes to Unicode and report glyph advances.
//!
//! Simple fonts (Type1/TrueType/Type3) get a 1-byte `[u32; 256]` code→codepoint
//! table from `/Encoding` (predefined name OR `/Differences`, glyph names
//! resolved via `agl`) and `/Widths`. Composite (`Type0`) fonts use 2-byte codes,
//! the descendant font's `/DW` + `/W` widths, and decode through `/ToUnicode`
//! (or, for the Unicode-keyed predefined CMaps `*-UCS2-*` / `*-UTF16-*`,
//! directly as UTF-16BE). A `/ToUnicode` CMap takes priority over the encoding
//! for every font kind.
//!
//! A composite font with neither a ToUnicode map nor a Unicode-keyed CMap has
//! glyph IDs we cannot map to text; it decodes to the empty string instead of
//! guessing (printable-ASCII GIDs would be misleading garbage). A page made only
//! of such text then reads as image-heavy, which is what routes it to OCR.

use std::collections::HashMap;

use lopdf::{Dictionary, Document as LoDoc, Object};

use super::agl;
use std::sync::Arc;

use super::cid2uni::{has_standard_ascii_glyph_order, standard_order_ascii, Collection};
use super::cjk_cmap::{predefined as predefined_cmap, EncodingCMap};
use super::cmap::CMap;
use crate::content::Advance;
use crate::reader::object_stream_content;

/// ToUnicode CMaps are small; refuse to inflate more than this.
const MAX_TOUNICODE_BYTES: usize = 4 << 20;

/// Cap on `/Widths` entries and `/W` records read.
const MAX_WIDTH_ENTRIES: usize = 1 << 16;

/// Width of a code the font gives no width for, in 1/1000 em. Used only when the
/// font carries no width data at all (e.g. the standard 14 fonts).
const FALLBACK_WIDTH: f32 = 500.0;

/// Rough advance of a printable-ASCII code in a proportional Latin font, in
/// 1/1000 em, for fonts that carry no widths (the standard 14 fonts). Grouping
/// by letter shape keeps the estimated end of a word close enough to its real
/// end that a word space (~250 units) still shows up as a gap.
fn estimated_width(code: u32) -> f32 {
    match char::from_u32(code).unwrap_or('\0') {
        ' ' => 260.0,
        'i' | 'j' | 'l' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' => 240.0,
        'f' | 't' | 'r' | 'I' | '(' | ')' | '[' | ']' | '-' | '/' | '`' => 330.0,
        'm' | 'w' => 800.0,
        'M' | 'W' | '@' | '%' => 880.0,
        'A'..='Z' => 680.0,
        '0'..='9' => 540.0,
        'a'..='z' => 520.0,
        _ => FALLBACK_WIDTH,
    }
}

/// Glyph advance widths in 1/1000 em.
#[derive(Clone)]
enum Widths {
    /// No width data: estimate [`FALLBACK_WIDTH`] per code.
    Unknown,
    /// `/Widths` indexed by `code - first`; codes outside use `missing`.
    Simple {
        first: u32,
        widths: Vec<f32>,
        missing: f32,
    },
    /// Descendant CIDFont `/DW` + `/W` (CID == code, i.e. Identity encodings).
    Cid {
        default: f32,
        list: HashMap<u32, f32>,
        /// Sorted, non-overlapping-assumed `(first, last, width)` ranges.
        ranges: Vec<(u32, u32, f32)>,
    },
}

impl Widths {
    fn width(&self, code: u32) -> f32 {
        match self {
            Widths::Unknown => estimated_width(code),
            Widths::Simple {
                first,
                widths,
                missing,
            } => code
                .checked_sub(*first)
                .and_then(|i| widths.get(i as usize))
                .copied()
                .unwrap_or(*missing),
            Widths::Cid {
                default,
                list,
                ranges,
            } => {
                if let Some(w) = list.get(&code) {
                    return *w;
                }
                // Ranges are sorted by first CID but may overlap: look back
                // (a bounded distance) for the nearest one that covers `code`.
                let at = ranges.partition_point(|r| r.0 <= code);
                ranges[..at]
                    .iter()
                    .rev()
                    .take(64)
                    .find(|r| code <= r.1)
                    .map_or(*default, |r| r.2)
            }
        }
    }
}

/// A resolved font ready to decode show-string bytes.
#[derive(Clone)]
pub struct ResolvedFont {
    /// 1-byte code → Unicode scalar (0 = unmapped). `None` if no usable encoding.
    table: Option<Box<[u32; 256]>>,
    to_unicode: Option<CMap>,
    widths: Widths,
    /// `Type0` composite font.
    composite: bool,
    /// How a composite font's codes become CIDs (and how long they are).
    codes: CodeMap,
    /// How a composite font's CIDs become text when ToUnicode does not say.
    cid_text: CidText,
}

/// A composite font's `/Encoding`.
#[derive(Clone)]
enum CodeMap {
    /// `Identity-H`/`-V` (or an embedded/unknown CMap, read as 2-byte codes):
    /// code = CID.
    Identity,
    /// A Unicode-keyed predefined CMap (`Uni*-UCS2-*`, `Uni*-UTF16-*`): codes
    /// are UTF-16 and are the text; CIDs (for widths) come from the collection.
    Unicode(Option<Collection>),
    /// A legacy predefined CMap (Shift-JIS, EUC, GBK, Big5, UHC, ...).
    Legacy(Arc<EncodingCMap>),
}

/// Fallback decoding for the CIDs of a composite font with an `Identity`
/// encoding (code = CID).
#[derive(Clone, Copy, Debug, PartialEq)]
enum CidText {
    /// Glyph IDs carry no text.
    None,
    /// The font uses an Adobe CJK collection: its published mapping applies.
    Collection(Collection),
    /// `Ordering (Identity)` built from a standard-order TrueType family: ASCII
    /// is glyph ID + 29.
    StandardOrderAscii,
}

impl ResolvedFont {
    /// Decode a show string to Unicode. Priority: ToUnicode → (composite:
    /// Unicode-keyed CMap) → 1-byte table → best-effort ASCII passthrough.
    pub fn decode(&self, bytes: &[u8]) -> String {
        if let (Some(cm), CodeMap::Legacy(_)) = (&self.to_unicode, &self.codes) {
            // Mixed one- and two-byte codes: split by the font's own CMap.
            let mut out = String::new();
            for code in self.split_codes(bytes) {
                match cm.get(code) {
                    Some(s) if !s.chars().any(|c| c.is_control() && c != '\t' && c != '\n') => {
                        out.push_str(s)
                    }
                    _ => out.push_str(&self.decode_without_cmap(code)),
                }
            }
            return expand_ligatures(out);
        }
        if let Some(cm) = &self.to_unicode {
            // Code by code: a ToUnicode that covers only some codes (common in
            // subset fonts) must not erase the rest, which the encoding table
            // or the Unicode-keyed CMap can still decode.
            let unit = self.code_len();
            let mut out = String::new();
            let mut i = 0;
            let mut mapped_any = false;
            while i < bytes.len() {
                let (len, s) = cm.next_code(&bytes[i..], unit);
                let code = &bytes[i..(i + len).min(bytes.len())];
                // An entry mapping to a control character (seen: digits mapped
                // to U+0018) is a broken ToUnicode, not text: use the encoding.
                let usable =
                    s.filter(|s| !s.chars().any(|c| c.is_control() && c != '\t' && c != '\n'));
                match usable {
                    Some(s) => {
                        out.push_str(s);
                        mapped_any = true;
                    }
                    None => out.push_str(&self.decode_without_cmap(code)),
                }
                i += len;
            }
            if mapped_any {
                return expand_ligatures(out);
            }
        }
        self.decode_without_cmap(bytes)
    }

    /// Decode through the encoding (simple fonts) or a Unicode-keyed CMap
    /// (composite fonts), ignoring ToUnicode.
    fn decode_without_cmap(&self, bytes: &[u8]) -> String {
        if self.composite {
            if matches!(self.codes, CodeMap::Unicode(_)) {
                let units: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes(*c))
                    .collect();
                return expand_ligatures(
                    char::decode_utf16(units)
                        .filter_map(|r| r.ok())
                        .filter(|c| !c.is_control() || *c == '\t' || *c == '\n')
                        .collect(),
                );
            }
            let cids = self
                .split_codes(bytes)
                .into_iter()
                .filter_map(|c| self.cid(c));
            return match self.cid_text {
                CidText::None => String::new(),
                CidText::Collection(col) => {
                    expand_ligatures(cids.filter_map(|cid| col.to_char(cid)).collect())
                }
                CidText::StandardOrderAscii => cids.filter_map(standard_order_ascii).collect(),
            };
        }
        if let Some(t) = &self.table {
            let mut s = String::new();
            for &b in bytes {
                let cp = t[b as usize];
                if cp == 0 {
                    // Not in the encoding: printable ASCII is the best guess.
                    if (0x20..0x7f).contains(&b) {
                        s.push(b as char);
                    }
                } else if let Some(c) = char::from_u32(cp) {
                    // Skip non-printable control chars — a Differences glyph name
                    // like G12/G5 resolves to form-feed/ENQ, but the actual glyph
                    // is a visible char (leader/space); emitting the control would
                    // be garbage. Tabs/newlines are kept.
                    if !c.is_control() || c == '\t' || c == '\n' {
                        s.push(c);
                    }
                } else if (0x20..0x7f).contains(&b) {
                    s.push(b as char); // ASCII fallback for uncovered codes
                }
            }
            return expand_ligatures(s);
        }
        // No table and no ToUnicode: ASCII-only passthrough (skips high bytes
        // rather than emitting glyph garbage).
        bytes
            .iter()
            .filter(|&&b| (0x20..0x7f).contains(&b))
            .map(|&b| b as char)
            .collect()
    }

    /// Advance of a show string: glyph widths in em, glyph-code count, and the
    /// number of single-byte space codes (the only codes `Tw` applies to).
    pub fn advance(&self, bytes: &[u8]) -> Advance {
        if self.composite {
            let mut em = 0f32;
            let mut glyphs = 0u32;
            let mut spaces = 0u32;
            for c in self.split_codes(bytes) {
                em += self.widths.width(self.cid(c).unwrap_or(0));
                glyphs += 1;
                // Word spacing applies to the single-byte code 32 in any font.
                if c == [0x20] {
                    spaces += 1;
                }
            }
            return Advance {
                em: em / 1000.0,
                glyphs,
                spaces,
            };
        }
        let mut em = 0f32;
        let mut spaces = 0u32;
        for &b in bytes {
            em += self.widths.width(b as u32);
            if b == 0x20 {
                spaces += 1;
            }
        }
        Advance {
            em: em / 1000.0,
            glyphs: bytes.len() as u32,
            spaces,
        }
    }

    /// Whether the font carries real width data (otherwise advances are estimates
    /// and the layout pass is more conservative about inserting spaces).
    pub fn widths_known(&self) -> bool {
        !matches!(self.widths, Widths::Unknown)
    }

    /// Bytes per character code when fixed (1 for simple fonts, 2 for
    /// Identity and Unicode composite fonts); 1 for legacy CJK encodings,
    /// whose codes are one or two bytes (see `split_codes`).
    pub fn code_len(&self) -> usize {
        match (&self.codes, self.composite) {
            (_, false) | (CodeMap::Legacy(_), true) => 1,
            _ => 2,
        }
    }

    /// Split a show string into character codes.
    pub fn split_codes<'a>(&self, bytes: &'a [u8]) -> Vec<&'a [u8]> {
        match &self.codes {
            CodeMap::Legacy(m) if self.composite => {
                let mut out = Vec::new();
                let mut i = 0;
                while i < bytes.len() {
                    let n = m.code_len(&bytes[i..]).min(bytes.len() - i);
                    out.push(&bytes[i..i + n]);
                    i += n;
                }
                out
            }
            _ => bytes.chunks(self.code_len()).collect(),
        }
    }

    /// CID of one code of a composite font.
    fn cid(&self, code: &[u8]) -> Option<u32> {
        let value = || code.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b));
        match &self.codes {
            CodeMap::Identity => Some(value()),
            CodeMap::Legacy(m) => m.cid(code),
            CodeMap::Unicode(col) => {
                let c = char::from_u32(value());
                Some(
                    col.zip(c)
                        .and_then(|(col, c)| col.cid_of(c))
                        .unwrap_or(value()),
                )
            }
        }
    }

    /// A font that could not be resolved: ASCII passthrough, estimated widths.
    pub fn unmapped() -> Self {
        ResolvedFont {
            table: None,
            to_unicode: None,
            widths: Widths::Unknown,
            composite: false,
            codes: CodeMap::Identity,
            cid_text: CidText::None,
        }
    }
}

/// Expand the Latin ligature presentation forms (U+FB00–U+FB06) to their letter
/// sequences, so search/tokenization sees `find`, not `ﬁnd`.
fn expand_ligatures(s: String) -> String {
    if !s.chars().any(|c| ('\u{FB00}'..='\u{FB06}').contains(&c)) {
        return s;
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            c => out.push(c),
        }
    }
    out
}

/// Resolve a font object (already fetched, typically the value of a /Font entry).
pub fn resolve(doc: &LoDoc, font_obj: &Object) -> ResolvedFont {
    let dict = match font_obj.as_dict() {
        Ok(d) => d,
        Err(_) => return ResolvedFont::unmapped(),
    };
    let composite = name(dict, b"Subtype").as_deref() == Some("Type0");
    let to_unicode = dict
        .get(b"ToUnicode")
        .ok()
        .and_then(|o| object_stream_content(doc, o, MAX_TOUNICODE_BYTES))
        .and_then(|b| CMap::parse(&b));

    if composite {
        let enc = match dict.get(b"Encoding").ok().map(|o| deref(doc, o)) {
            Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
            _ => String::new(),
        };
        let widths = build_cid_widths(doc, dict);
        let collection = font_collection(doc, dict);
        let (codes, cid_text) = if enc.contains("UCS2") || enc.contains("UTF16") {
            (CodeMap::Unicode(collection), CidText::None)
        } else if let Some(m) = (!enc.starts_with("Identity"))
            .then(|| predefined_cmap(&enc))
            .flatten()
        {
            let col = collection.unwrap_or(m.collection);
            (CodeMap::Legacy(m), CidText::Collection(col))
        } else if enc.starts_with("Identity") {
            (CodeMap::Identity, cid_text(doc, dict, &widths))
        } else {
            // An embedded CMap stream or an unknown name: 2-byte codes, and
            // only ToUnicode can give text.
            (CodeMap::Identity, CidText::None)
        };
        return ResolvedFont {
            table: None,
            to_unicode,
            widths,
            composite: true,
            codes,
            cid_text,
        };
    }

    let subtype = name(dict, b"Subtype");
    let encoding = dict.get(b"Encoding").ok().map(|o| deref(doc, o));
    // The Symbol font's built-in encoding is its own (Greek, math), not
    // StandardEncoding: it is the base wherever the font's built-in applies.
    let symbol = name(dict, b"BaseFont").is_some_and(|b| {
        let b = b.split_once('+').map_or(b.as_str(), |(_, r)| r);
        b == "Symbol" || b.starts_with("Symbol,") || b == "SymbolMT"
    });
    let builtin = if symbol {
        "SymbolEncoding"
    } else {
        "StandardEncoding"
    };
    let table = match encoding {
        Some(enc) => build_table(doc, enc, builtin),
        // A Type 1 font with no /Encoding uses its built-in encoding, which for
        // text fonts is almost always StandardEncoding.
        None if symbol || matches!(subtype.as_deref(), Some("Type1" | "MMType1")) => {
            Some(Box::new(predefined(builtin)))
        }
        None => None,
    };
    ResolvedFont {
        table,
        to_unicode,
        widths: build_simple_widths(doc, dict, subtype.as_deref() == Some("Type3")),
        composite: false,
        codes: CodeMap::Identity,
        cid_text: CidText::None,
    }
}

/// The Adobe collection a composite font's descendant declares.
fn font_collection(doc: &LoDoc, type0: &Dictionary) -> Option<Collection> {
    let info = descendant(doc, type0)?
        .get(b"CIDSystemInfo")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())?;
    let text = |key: &[u8]| -> String {
        info.get(key)
            .ok()
            .map(|o| deref(doc, o))
            .and_then(|o| o.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    };
    Collection::from_ros(&text(b"Registry"), &text(b"Ordering"))
}

/// Decide how an Identity-encoded composite font's CIDs become text when its
/// ToUnicode is missing or partial: through the Adobe collection named in the
/// descendant's `/CIDSystemInfo`, or, for an `Identity` ordering built from a
/// standard-order TrueType family, ASCII by glyph ID. The second is checked
/// against the font's own widths (space and period narrower than capitals) so
/// a font that merely shares the name prefix is not misread.
fn cid_text(doc: &LoDoc, type0: &Dictionary, widths: &Widths) -> CidText {
    let Some(desc) = descendant(doc, type0) else {
        return CidText::None;
    };
    let info = desc
        .get(b"CIDSystemInfo")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());
    let text = |key: &[u8]| -> String {
        info.and_then(|d| d.get(key).ok())
            .map(|o| deref(doc, o))
            .and_then(|o| o.as_str().ok())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default()
    };
    let (registry, ordering) = (text(b"Registry"), text(b"Ordering"));
    if let Some(col) = Collection::from_ros(&registry, &ordering) {
        return CidText::Collection(col);
    }
    let base = name(desc, b"BaseFont").or_else(|| name(type0, b"BaseFont"));
    let standard_name = base.as_deref().is_some_and(has_standard_ascii_glyph_order);
    if ordering == "Identity" && standard_name && widths_look_latin(widths) {
        return CidText::StandardOrderAscii;
    }
    CidText::None
}

/// Width sanity check for the glyph-order heuristic: in a standard-order font
/// glyph 3 is the space and 17 the period, both narrow; 36..=61 are capitals.
fn widths_look_latin(widths: &Widths) -> bool {
    let Widths::Cid { list, ranges, .. } = widths else {
        return false;
    };
    let explicit = |cid: u32| {
        list.get(&cid).copied().or_else(|| {
            ranges
                .iter()
                .find(|r| r.0 <= cid && cid <= r.1)
                .map(|r| r.2)
        })
    };
    let narrow = [3, 17]
        .iter()
        .filter_map(|&c| explicit(c))
        .collect::<Vec<_>>();
    let caps = (36..=61).filter_map(explicit).collect::<Vec<_>>();
    if narrow.is_empty() || caps.is_empty() {
        return false;
    }
    let avg_caps = caps.iter().sum::<f32>() / caps.len() as f32;
    narrow.iter().all(|&w| w > 0.0 && w < 0.6 * avg_caps)
}

fn descendant<'a>(doc: &'a LoDoc, type0: &'a Dictionary) -> Option<&'a Dictionary> {
    type0
        .get(b"DescendantFonts")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
        .and_then(|a| a.first())
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
}

fn deref<'a>(doc: &'a LoDoc, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(r) => doc.get_object(*r).unwrap_or(obj),
        _ => obj,
    }
}

fn number(doc: &LoDoc, obj: &Object) -> Option<f32> {
    match deref(doc, obj) {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

/// Read `/Widths` (array, indexed by `code - /FirstChar`) + `/FirstChar` +
/// `/FontDescriptor/MissingWidth` for advance estimation.
fn build_simple_widths(doc: &LoDoc, dict: &Dictionary, type3: bool) -> Widths {
    let Some(arr) = dict
        .get(b"Widths")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    else {
        return Widths::Unknown;
    };
    let first = dict
        .get(b"FirstChar")
        .ok()
        .and_then(|o| number(doc, o))
        .map(|v| v.max(0.0) as u32)
        .unwrap_or(0);
    // Type 3 widths are in glyph space; /FontMatrix maps them to text space
    // (1000 glyph units per em only for the conventional 0.001 matrix).
    let scale = if type3 {
        dict.get(b"FontMatrix")
            .ok()
            .map(|o| deref(doc, o))
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| number(doc, o))
            .map(|a| (a * 1000.0).abs())
            .filter(|s| *s > 0.0 && s.is_finite())
            .unwrap_or(1.0)
    } else {
        1.0
    };
    let widths: Vec<f32> = arr
        .iter()
        .take(MAX_WIDTH_ENTRIES)
        .map(|o| number(doc, o).unwrap_or(0.0) * scale)
        .collect();
    if widths.is_empty() {
        return Widths::Unknown;
    }
    let missing = dict
        .get(b"FontDescriptor")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| d.get(b"MissingWidth").ok())
        .and_then(|o| number(doc, o))
        .unwrap_or(0.0)
        * scale;
    Widths::Simple {
        first,
        widths,
        missing,
    }
}

/// Read the descendant CIDFont's `/DW` and `/W`:
/// `c [w1 w2 …]` (consecutive CIDs from `c`) and `c_first c_last w` (a range).
fn build_cid_widths(doc: &LoDoc, dict: &Dictionary) -> Widths {
    let desc = dict
        .get(b"DescendantFonts")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
        .and_then(|a| a.first())
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());
    let Some(desc) = desc else {
        return Widths::Unknown;
    };
    let default = desc
        .get(b"DW")
        .ok()
        .and_then(|o| number(doc, o))
        .unwrap_or(1000.0);
    let mut list: HashMap<u32, f32> = HashMap::new();
    let mut ranges: Vec<(u32, u32, f32)> = Vec::new();
    if let Some(w) = desc
        .get(b"W")
        .ok()
        .map(|o| deref(doc, o))
        .and_then(|o| o.as_array().ok())
    {
        let mut i = 0;
        let mut records = 0usize;
        while i < w.len() && records < MAX_WIDTH_ENTRIES {
            let Some(c) = number(doc, &w[i]).map(|v| v.max(0.0) as u32) else {
                i += 1;
                continue;
            };
            match w.get(i + 1).map(|o| deref(doc, o)) {
                Some(Object::Array(ws)) => {
                    for (k, wo) in ws.iter().enumerate() {
                        if let (Some(width), Some(cid)) = (number(doc, wo), c.checked_add(k as u32))
                        {
                            if list.len() < MAX_WIDTH_ENTRIES {
                                list.insert(cid, width);
                            }
                        }
                    }
                    i += 2;
                }
                Some(_) => {
                    let last = w.get(i + 1).and_then(|o| number(doc, o));
                    let width = w.get(i + 2).and_then(|o| number(doc, o));
                    if let (Some(last), Some(width)) = (last, width) {
                        ranges.push((c, (last.max(0.0) as u32).max(c), width));
                    }
                    i += 3;
                }
                None => break,
            }
            records += 1;
        }
    }
    ranges.sort_by_key(|r| r.0);
    Widths::Cid {
        default,
        list,
        ranges,
    }
}

fn build_table(doc: &LoDoc, enc: &Object, builtin: &str) -> Option<Box<[u32; 256]>> {
    let enc_dict = match enc {
        Object::Dictionary(d) => d,
        Object::Name(n) => return Some(Box::new(predefined(&String::from_utf8_lossy(n)))),
        _ => return None,
    };
    // No /BaseEncoding: the font's built-in encoding, which we cannot read from
    // the font program; StandardEncoding is what text fonts use.
    let mut table = match enc_dict.get(b"BaseEncoding").ok().map(|o| deref(doc, o)) {
        Some(Object::Name(n)) => predefined(&String::from_utf8_lossy(n)),
        _ => predefined(builtin),
    };
    // `c<decimal>` names (some producers' subsets): "the glyph at this code in
    // the font's built-in encoding". Resolve through that encoding, so a
    // Symbol `/c87` reads as Ω, not the ASCII `W` of code 87.
    let builtin_table = predefined(builtin);
    let differences = enc_dict.get(b"Differences").ok().map(|o| deref(doc, o));
    if let Some(Object::Array(arr)) = differences {
        let mut code: u32 = 0;
        for item in arr.iter() {
            match item {
                Object::Integer(i) => code = (*i).clamp(0, 255) as u32,
                Object::Name(nm) => {
                    let nm = String::from_utf8_lossy(nm);
                    let by_code = nm
                        .strip_prefix('c')
                        .filter(|d| {
                            (1..=3).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_digit())
                        })
                        .and_then(|d| d.parse::<usize>().ok())
                        .filter(|&n| n < 256)
                        .map(|n| builtin_table[n])
                        .filter(|&cp| cp != 0);
                    // Unknown name: the base encoding's character is not what
                    // this glyph shows. 0 = unmapped (ASCII fallback in decode).
                    table[code as usize] = by_code
                        .or_else(|| agl::glyph_to_char(&nm).map(|c| c as u32))
                        .unwrap_or_default();
                    if code < 255 {
                        code += 1;
                    }
                }
                _ => {}
            }
        }
    }
    Some(Box::new(table))
}

/// Delegate to the real standard encoding tables (WinAnsi/MacRoman exact,
/// others approximated) — see `encoding.rs`.
fn predefined(name: &str) -> [u32; 256] {
    super::encoding::predefined(name)
}

fn name(d: &Dictionary, key: &[u8]) -> Option<String> {
    match d.get(key).ok()? {
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmapped_font_decodes_ascii_only() {
        let f = ResolvedFont::unmapped();
        // High bytes dropped (no glyph garbage), ASCII kept.
        assert_eq!(f.decode(&[b'H', 0xff, b'i']), "Hi");
    }

    #[test]
    fn ligature_presentation_forms_are_expanded() {
        assert_eq!(
            expand_ligatures("\u{FB01}nd \u{FB03}ce".into()),
            "find ffice"
        );
        assert_eq!(expand_ligatures("plain".into()), "plain");
    }

    #[test]
    fn composite_without_tounicode_decodes_to_nothing() {
        let f = ResolvedFont {
            composite: true,
            ..ResolvedFont::unmapped()
        };
        // GIDs that happen to look like ASCII must not leak out as text.
        assert_eq!(f.decode(&[0x00, 0x41, 0x00, 0x42]), "");
    }

    #[test]
    fn composite_utf16_cmap_decodes_directly() {
        let f = ResolvedFont {
            composite: true,
            codes: CodeMap::Unicode(None),
            ..ResolvedFont::unmapped()
        };
        assert_eq!(f.decode(&[0x00, 0x48, 0x00, 0x69]), "Hi");
    }

    #[test]
    fn cid_widths_lookup_list_range_and_default() {
        let mut list = HashMap::new();
        list.insert(5u32, 300.0);
        let w = Widths::Cid {
            default: 1000.0,
            list,
            ranges: vec![(10, 20, 600.0)],
        };
        assert_eq!(w.width(5), 300.0);
        assert_eq!(w.width(15), 600.0);
        assert_eq!(w.width(21), 1000.0);
        assert_eq!(w.width(2), 1000.0);
    }

    #[test]
    fn simple_widths_and_spaces_in_advance() {
        let f = ResolvedFont {
            widths: Widths::Simple {
                first: 32,
                widths: vec![250.0, 500.0],
                missing: 0.0,
            },
            ..ResolvedFont::unmapped()
        };
        let a = f.advance(b" !~");
        assert_eq!(a.glyphs, 3);
        assert_eq!(a.spaces, 1);
        assert!((a.em - 0.75).abs() < 1e-6); // 250 + 500 + missing(0)
    }
}
