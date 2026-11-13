//! ToUnicode CMap parser.
//!
//! A ToUnicode CMap maps character codes (1–4 bytes, usually 1–2) to Unicode
//! strings, emitted as UTF-16BE hex. We extract the `bfchar`/`bfrange` blocks
//! and the `codespacerange`s. A code's length comes from the codespace range
//! it falls in; without one, mapped codes are tried longest first and an
//! unmapped code takes the font's code width. The corpus fonts lack
//! ToUnicode (they use Encoding/Differences — see `agl.rs` + `font.rs`), so this
//! module is exercised mainly by other PDFs that do embed it. Input is
//! untrusted: range expansion and total entries are capped.

use std::collections::HashMap;

/// Hard cap on mapped codes per CMap, so a hostile `bfrange` flood cannot
/// allocate without bound. A full 2-byte codespace is 65 536 codes; this leaves
/// room for several fully-populated ranges.
const MAX_ENTRIES: usize = 1 << 18;

/// Hard cap on the codes expanded from a single `bfrange`.
const MAX_RANGE: u32 = 1 << 16;

/// Hard cap on codes expanded across all `bfrange`s of one CMap. Ranges that
/// keep rewriting the same keys never grow the map, so the entry cap alone does
/// not bound the work.
const MAX_RANGE_WORK: usize = 1 << 19;

/// A parsed ToUnicode CMap. `(code length, code) → Unicode string`. Destinations
/// are strings, not scalars: ligature entries like `<FB01> <00660069>` map one
/// code to several characters.
#[derive(Clone)]
pub struct CMap {
    map: HashMap<(u8, u32), String>,
    /// Bitmask of code lengths (bit `n` = n-byte codes) present in `map`.
    lens: u8,
    /// `codespacerange` entries: `(length, low bytes, high bytes)`. They decide
    /// how many bytes the next code takes, mapped or not.
    spaces: Vec<(usize, [u8; 4], [u8; 4])>,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Hex(Vec<u8>),
    Open,
    Close,
}

impl CMap {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(data);
        let mut map: HashMap<(u8, u32), String> = HashMap::new();
        let mut work = 0usize;

        // bfchar: <code> <dst>  (dst is UTF-16BE: 1+ units)
        for block in blocks(&text, "beginbfchar", "endbfchar") {
            let toks = tokens(block);
            let mut i = 0;
            while i + 1 < toks.len() {
                if let (Tok::Hex(code), Tok::Hex(dst)) = (&toks[i], &toks[i + 1]) {
                    if let (Some(key), Some(s)) = (code_key(code), utf16_to_string(dst)) {
                        insert(&mut map, key, s);
                    }
                }
                i += 2;
            }
        }
        // bfrange: <lo> <hi> <dst>   OR   <lo> <hi> [<dst1> <dst2> ...]
        for block in blocks(&text, "beginbfrange", "endbfrange") {
            let toks = tokens(block);
            let mut i = 0;
            while i + 2 < toks.len() {
                let (Tok::Hex(lo), Tok::Hex(hi)) = (&toks[i], &toks[i + 1]) else {
                    i += 1; // resync on junk
                    continue;
                };
                let (Some((len, lo_v)), Some((hi_len, hi_v))) = (code_key(lo), code_key(hi)) else {
                    i += 3;
                    continue;
                };
                match &toks[i + 2] {
                    Tok::Open => {
                        // Array form: explicit per-code destination list.
                        let mut n = 0u32;
                        let mut j = i + 3;
                        while j < toks.len() && toks[j] != Tok::Close {
                            if let Tok::Hex(d) = &toks[j] {
                                if let (Some(s), Some(code)) =
                                    (utf16_to_string(d), lo_v.checked_add(n))
                                {
                                    if len == hi_len && code <= hi_v {
                                        insert(&mut map, (len, code), s);
                                    }
                                }
                                n = n.saturating_add(1);
                            }
                            j += 1;
                        }
                        i = if j < toks.len() { j + 1 } else { j };
                    }
                    Tok::Hex(dst) => {
                        // Increment form: dst's last UTF-16 unit counts up across
                        // lo..=hi.
                        if len == hi_len && hi_v >= lo_v {
                            let span = (hi_v - lo_v).min(MAX_RANGE - 1);
                            if let Some(units) = utf16_units(dst) {
                                for off in 0..=span {
                                    if map.len() >= MAX_ENTRIES || work >= MAX_RANGE_WORK {
                                        break;
                                    }
                                    work += 1;
                                    let mut u = units.clone();
                                    let Some(last) = u.last_mut() else { break };
                                    let Some(v) = last.checked_add(off as u16) else {
                                        break;
                                    };
                                    *last = v;
                                    if let Some(s) = units_to_string(&u) {
                                        insert(&mut map, (len, lo_v + off), s);
                                    }
                                }
                            }
                        }
                        i += 3;
                    }
                    Tok::Close => i += 3,
                }
            }
        }
        if map.is_empty() {
            return None;
        }
        let lens = map.keys().fold(0u8, |m, (l, _)| m | (1 << l));
        let mut spaces = Vec::new();
        for block in blocks(&text, "begincodespacerange", "endcodespacerange") {
            let toks = tokens(block);
            for pair in toks.chunks(2) {
                if let [Tok::Hex(lo), Tok::Hex(hi)] = pair {
                    if lo.len() == hi.len() && (1..=4).contains(&lo.len()) && spaces.len() < 64 {
                        let (mut l, mut h) = ([0u8; 4], [0u8; 4]);
                        l[..lo.len()].copy_from_slice(lo);
                        h[..hi.len()].copy_from_slice(hi);
                        spaces.push((lo.len(), l, h));
                    }
                }
            }
        }
        Some(CMap { map, lens, spaces })
    }

    /// Length of the code starting at `bytes[0]` according to the
    /// `codespacerange`s, if any range matches.
    fn codespace_len(&self, bytes: &[u8]) -> Option<usize> {
        self.spaces
            .iter()
            .filter(|(len, ..)| *len <= bytes.len())
            .find(|(len, lo, hi)| (0..*len).all(|k| lo[k] <= bytes[k] && bytes[k] <= hi[k]))
            .map(|(len, ..)| *len)
    }

    /// The mapping of one complete code, if any.
    pub fn get(&self, code: &[u8]) -> Option<&str> {
        let len = code.len();
        if len == 0 || len > 4 {
            return None;
        }
        let v = code.iter().fold(0u32, |a, &b| (a << 8) | u32::from(b));
        self.map.get(&(len as u8, v)).map(String::as_str)
    }

    /// The code at the start of `bytes`: its length and its mapping, if any.
    /// The length comes from the codespace ranges; without a matching range, a
    /// mapped code of any length is tried (longest first) and an unmapped one
    /// takes `unit` bytes (the font's code width), so one unmapped glyph cannot
    /// shift every following 2-byte code by a byte.
    pub fn next_code(&self, bytes: &[u8], unit: usize) -> (usize, Option<&str>) {
        let key = |len: usize| -> (u8, u32) {
            (
                len as u8,
                bytes[..len].iter().fold(0u32, |a, &b| (a << 8) | b as u32),
            )
        };
        if let Some(len) = self.codespace_len(bytes) {
            return (len, self.map.get(&key(len)).map(String::as_str));
        }
        for len in (1..=4usize).rev() {
            if self.lens & (1 << len) == 0 || len > bytes.len() {
                continue;
            }
            if let Some(s) = self.map.get(&key(len)) {
                return (len, Some(s));
            }
        }
        (unit.clamp(1, bytes.len().max(1)), None)
    }

    /// Decode a show-string to Unicode, skipping unmapped codes (`unit` bytes
    /// each when the codespace does not say). Returns None if nothing mapped.
    #[cfg(test)]
    pub fn decode(&self, bytes: &[u8]) -> Option<String> {
        self.decode_unit(bytes, 1)
    }

    #[cfg(test)]
    fn decode_unit(&self, bytes: &[u8], unit: usize) -> Option<String> {
        let mut out = String::new();
        let mut i = 0;
        let mut mapped_any = false;
        while i < bytes.len() {
            let (len, s) = self.next_code(&bytes[i..], unit);
            if let Some(s) = s {
                out.push_str(s);
                mapped_any = true;
            }
            i += len;
        }
        mapped_any.then_some(out)
    }
}

fn insert(map: &mut HashMap<(u8, u32), String>, key: (u8, u32), s: String) {
    if map.len() < MAX_ENTRIES || map.contains_key(&key) {
        map.insert(key, s);
    }
}

/// Extract the text between each `begin_tag`/`end_tag` pair.
fn blocks<'a>(text: &'a str, begin_tag: &str, end_tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(bs) = text[search..].find(begin_tag) {
        let start = search + bs + begin_tag.len();
        if let Some(es) = text[start..].find(end_tag) {
            out.push(&text[start..start + es]);
            search = start + es + end_tag.len();
        } else {
            break;
        }
    }
    out
}

/// Tokenize a CMap block into hex strings and `[` / `]` markers (kept distinct
/// from hex strings, so a one-byte `<5B>` is not mistaken for an array opener).
fn tokens(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let Some(p) = bytes[i + 1..].iter().position(|&b| b == b'>') else {
                    break;
                };
                let j = i + 1 + p;
                out.push(Tok::Hex(hex_to_bytes(&bytes[i + 1..j])));
                i = j + 1;
            }
            b'[' => {
                out.push(Tok::Open);
                i += 1;
            }
            b']' => {
                out.push(Tok::Close);
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

fn hex_to_bytes(hex: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut hi: Option<u8> = None;
    for &c in hex {
        if let Some(v) = (c as char).to_digit(16) {
            match hi {
                None => hi = Some(v as u8),
                Some(h) => {
                    out.push((h << 4) | v as u8);
                    hi = None;
                }
            }
        }
    }
    if let Some(h) = hi {
        out.push(h << 4);
    }
    out
}

/// `(length, big-endian value)` of a 1–4 byte code; `None` otherwise.
fn code_key(b: &[u8]) -> Option<(u8, u32)> {
    if b.is_empty() || b.len() > 4 {
        return None;
    }
    Some((
        b.len() as u8,
        b.iter().fold(0u32, |a, &x| (a << 8) | x as u32),
    ))
}

fn utf16_units(b: &[u8]) -> Option<Vec<u16>> {
    // A single-byte destination is treated as one low-byte unit.
    if b.len() == 1 {
        return Some(vec![b[0] as u16]);
    }
    let units: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_be_bytes(*c))
        .collect();
    (!units.is_empty()).then_some(units)
}

fn units_to_string(units: &[u16]) -> Option<String> {
    let s: String = char::decode_utf16(units.iter().copied())
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect();
    // Drop destinations that are only replacement chars / empty.
    (!s.is_empty() && s.chars().any(|c| c != '\u{FFFD}')).then_some(s)
}

/// ToUnicode destinations are UTF-16BE (one or more units: surrogate pairs and
/// multi-character ligature expansions both appear).
fn utf16_to_string(b: &[u8]) -> Option<String> {
    units_to_string(&utf16_units(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bfchar_one_byte() {
        let cmap = b"/CIDInit /ProcSet findresource begin
12 dict begin begincmap
/CMapType 2 def
1 begincodespacerange <00> <FF> endcodespacerange
2 beginbfchar <41> <0041> <42> <0042> endbfchar
endcmap";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(b"AB"), Some("AB".to_string()));
    }

    #[test]
    fn parses_bfrange_increment() {
        let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange
1 beginbfrange <20> <7E> <0020> endbfrange";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(b" !\""), Some(" !\"".to_string()));
    }

    #[test]
    fn bfchar_ligature_destination_keeps_all_characters() {
        let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange
1 beginbfchar <01> <00660069> endbfchar";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(&[0x01]), Some("fi".to_string()));
    }

    #[test]
    fn surrogate_pair_destination() {
        let cmap = b"1 begincodespacerange <0000> <FFFF> endcodespacerange
1 beginbfchar <0001> <D83DDE00> endbfchar";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(&[0x00, 0x01]), Some("\u{1F600}".to_string()));
    }

    #[test]
    fn one_byte_destination_is_not_an_array_marker() {
        // <5B> is '[' as a byte value; it must stay a destination, not open an array.
        let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange
1 beginbfrange <01> <02> <5B> endbfrange
1 beginbfchar <03> <0041> endbfchar";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(&[0x01, 0x02, 0x03]), Some("[\\A".to_string()));
    }

    #[test]
    fn huge_bfrange_is_capped_not_expanded() {
        // 4 billion codes must not be materialized (DoS), nor overflow.
        let cmap = b"1 begincodespacerange <00000000> <FFFFFFFF> endcodespacerange
1 beginbfrange <00000000> <FFFFFFFF> <0000> endbfrange";
        let c = CMap::parse(cmap).unwrap();
        assert!(c.map.len() <= MAX_RANGE as usize);
        assert_eq!(c.decode(&[0, 0, 0, 0x41]), Some("A".to_string()));
    }

    #[test]
    fn bfrange_destination_overflow_does_not_panic() {
        let cmap = b"1 beginbfrange <0000> <0005> <FFFE> endbfrange";
        let c = CMap::parse(cmap).unwrap();
        // Only codes whose destination stays in the u16 range are mapped.
        assert!(c.map.len() <= 2);
    }

    #[test]
    fn lone_low_surrogate_in_pair_does_not_underflow() {
        let cmap = b"1 beginbfchar <01> <D800 0041> endbfchar";
        // High surrogate followed by a non-low-surrogate: decoded lossily, no panic.
        let _ = CMap::parse(cmap);
    }

    #[test]
    fn unmapped_two_byte_code_keeps_alignment() {
        // 0003 is unmapped; stepping one byte would read 0300 (X) next.
        let cmap = b"1 begincodespacerange <0000> <FFFF> endcodespacerange
3 beginbfchar <0041> <0041> <0042> <0042> <0300> <0058> endbfchar";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(&[0, 0x41, 0, 3, 0, 0x42]), Some("AB".to_string()));
        // Same without a codespacerange: the font's code width decides.
        let bare =
            CMap::parse(b"3 beginbfchar <0041> <0041> <0042> <0042> <0300> <0058> endbfchar")
                .unwrap();
        assert_eq!(
            bare.decode_unit(&[0, 0x41, 0, 3, 0, 0x42], 2),
            Some("AB".to_string())
        );
    }

    #[test]
    fn repeated_ranges_are_bounded() {
        // Every line rewrites the same 65 536 keys: the map never grows, the
        // work must still stop.
        let mut cmap = b"1 begincodespacerange <0000> <FFFF> endcodespacerange\n".to_vec();
        cmap.extend_from_slice(b"2000 beginbfrange\n");
        for _ in 0..2000 {
            cmap.extend_from_slice(b"<0000> <FFFF> <0000>\n");
        }
        cmap.extend_from_slice(b"endbfrange");
        let t = std::time::Instant::now();
        let c = CMap::parse(&cmap).unwrap();
        assert!(t.elapsed().as_secs() < 5);
        assert!(c.map.len() <= MAX_ENTRIES);
    }

    #[test]
    fn parses_bfrange_array_form() {
        // Gap #2: non-linear destination list <lo> <hi> [<d0> <d1> ...].
        let cmap = b"1 begincodespacerange <00> <FF> endcodespacerange
1 beginbfrange <01> <03> [<0041> <0042> <0043>] endbfrange";
        let c = CMap::parse(cmap).unwrap();
        assert_eq!(c.decode(&[0x01, 0x02, 0x03]), Some("ABC".to_string()));
    }
}
