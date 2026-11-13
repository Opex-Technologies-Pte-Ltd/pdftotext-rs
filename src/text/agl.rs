//! Glyph-name → Unicode resolver.
//!
//! PDF Type1/TrueType fonts map character codes to *glyph names* via the
//! Encoding/Differences array; the glyph name must then be turned into a Unicode
//! scalar. We handle, in priority order:
//!
//! 1. `G<decimal>` — generator-specific subset convention observed in the
//!    equipment manuals we tested (e.g. `G73` = 'I', `G32` = space). The decimal
//!    is the Unicode codepoint directly. NOT an Adobe-standard convention, but
//!    it is what the QIMP/QITE-imposed manuals in the corpus emit, and decoding
//!    it is what makes their text readable.
//! 2. `uni<H4..6>` — Adobe AGL convention, hex codepoint (4–6 digits, last group
//!    may encode a variation sequence; we take the first scalar).
//! 3. `u<H5..6>` — Adobe AGL convention, hex codepoint.
//! 4. The full Adobe Glyph List (~4300 names, embedded) for standard names
//!    like `quoteleft`, `Aacute`, `Ogonek`, …
//! 5. A single-character name (e.g. the name is literally `A`) → that character.
//!
//! Names with a `.`-suffix (variant selectors, e.g. `A.alt`) are resolved by
//! their base. Unknown names return `None` so the caller can fall back.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Resolve a glyph name to a Unicode scalar, or `None` if unknown.
pub fn glyph_to_char(name: &str) -> Option<char> {
    if name.is_empty() {
        return None;
    }
    // 1. G<decimal> — corpus subset convention.
    if let Some(rest) = name.strip_prefix('G') {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(cp) = rest.parse::<u32>() {
                if let Some(c) = char::from_u32(cp) {
                    return Some(c);
                }
            }
        }
    }
    // 2/3. uniXXXX / uXXXXX (strip a trailing variant-selector group after ';').
    let base = name.split(';').next().unwrap_or(name);
    if let Some(hex) = base
        .strip_prefix("uni")
        .filter(|s| (4..=6).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        if let Ok(cp) = u32::from_str_radix(hex, 16) {
            if let Some(c) = char::from_u32(cp) {
                return Some(c);
            }
        }
    }
    if let Some(hex) = base
        .strip_prefix('u')
        .filter(|s| (4..=6).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        if let Ok(cp) = u32::from_str_radix(hex, 16) {
            if let Some(c) = char::from_u32(cp) {
                return Some(c);
            }
        }
    }
    // 4. Compact Adobe Glyph List.
    if let Some(c) = agl_lookup(base) {
        return Some(c);
    }
    // 4a. AGL ligature composition (`f_i`, `f_f_l`, also with a `.variant`):
    // the Latin ones map to their presentation forms, which the decoder
    // expands back to letters. Other compositions would need several
    // characters per code and stay unresolved.
    let stem = base.split('.').next().unwrap_or(base);
    let ligature = match stem {
        "f_f" => Some('\u{FB00}'),
        "f_i" => Some('\u{FB01}'),
        "f_l" => Some('\u{FB02}'),
        "f_f_i" => Some('\u{FB03}'),
        "f_f_l" => Some('\u{FB04}'),
        "s_t" => Some('\u{FB06}'),
        _ => None,
    };
    if ligature.is_some() {
        return ligature;
    }
    // 4b. Strip a `.variant` suffix and retry the table.
    if let Some((stem, _)) = base.split_once('.') {
        if let Some(c) = agl_lookup(stem) {
            return Some(c);
        }
        // 5. single-character stem
        if let Some(c) = stem.chars().next() {
            if stem.chars().count() == 1 && (c.is_ascii_alphanumeric() || c.is_ascii_punctuation())
            {
                return Some(c);
            }
        }
    }
    // 5. single-character name.
    let mut it = base.chars();
    match (it.next(), it.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() || c.is_ascii_punctuation() => Some(c),
        _ => None,
    }
}

/// Look up `name` in the full Adobe Glyph List (`src/data/glyphlist.txt`, the
/// public `glyphlist.txt` v2.0 distributed under Adobe's BSD license; header
/// retained in the data file). Entries that map to a sequence of scalars (a few
/// Hebrew/Indic composites) resolve to their first scalar. Parsed once on first
/// use.
fn agl_lookup(name: &str) -> Option<char> {
    static TABLE: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let mut m = HashMap::with_capacity(4400);
            for line in include_str!("../data/glyphlist.txt").lines() {
                if line.starts_with('#') {
                    continue;
                }
                let Some((glyph, hex)) = line.split_once(';') else {
                    continue;
                };
                let first = hex.split_whitespace().next().unwrap_or("");
                if let Some(c) = u32::from_str_radix(first, 16).ok().and_then(char::from_u32) {
                    m.insert(glyph, c);
                }
            }
            m
        })
        .get(name)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agl_ligature_compositions() {
        assert_eq!(glyph_to_char("f_i"), Some('\u{FB01}'));
        assert_eq!(glyph_to_char("f_f_l"), Some('\u{FB04}'));
        assert_eq!(glyph_to_char("f_f.alt"), Some('\u{FB00}'));
        assert_eq!(glyph_to_char("x_y"), None);
    }

    #[test]
    fn g_decimal_corpus_convention() {
        assert_eq!(glyph_to_char("G73"), Some('I'));
        assert_eq!(glyph_to_char("G109"), Some('m'));
        assert_eq!(glyph_to_char("G32"), Some(' '));
        assert_eq!(glyph_to_char("G46"), Some('.'));
    }

    #[test]
    fn uni_and_u_hex() {
        assert_eq!(glyph_to_char("uni0041"), Some('A'));
        assert_eq!(glyph_to_char("u1F600"), Some('😀'));
    }

    #[test]
    fn agl_standard_names() {
        assert_eq!(glyph_to_char("quoteleft"), Some('\u{2018}'));
        assert_eq!(glyph_to_char("Aacute"), Some('\u{00C1}'));
        assert_eq!(glyph_to_char("copyright"), Some('\u{00A9}'));
    }

    #[test]
    fn variant_suffix_strips() {
        assert_eq!(glyph_to_char("A.alt"), Some('A'));
    }

    #[test]
    fn full_agl_covers_beyond_the_old_subset() {
        assert_eq!(glyph_to_char("Delta"), Some('\u{2206}'));
        assert_eq!(glyph_to_char("afii10017"), Some('\u{0410}')); // Cyrillic A
        assert_eq!(glyph_to_char("ffi"), Some('\u{FB03}'));
        assert_eq!(glyph_to_char("dalethatafpatah"), Some('\u{05D3}')); // first scalar
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(glyph_to_char("zzznotreal"), None);
    }
}
