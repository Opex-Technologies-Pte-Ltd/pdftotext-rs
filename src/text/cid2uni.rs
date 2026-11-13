//! CID → Unicode for the Adobe CJK character collections (Japan1, GB1, CNS1,
//! Korea1), and the glyph-order fallback for `Identity` CID fonts.
//!
//! A composite font whose `/CIDSystemInfo` names an Adobe collection but which
//! has no `/ToUnicode` (common in Japanese manuals that embed full Kozuka or
//! Ryumin fonts) still identifies every glyph: its CIDs are the collection's,
//! and Adobe publishes the collection's Unicode mapping. The tables in
//! `src/data/cid2uni/` are generated from Adobe's cmap-resources by
//! `scripts/gen-cid2unicode.py` (BSD-3-Clause; licence alongside the data).

use std::sync::OnceLock;

/// The Adobe collections we carry tables for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    Japan1,
    Gb1,
    Cns1,
    Korea1,
}

impl Collection {
    /// `Registry`/`Ordering` from `/CIDSystemInfo` (or a CFF ROS).
    pub fn from_ros(registry: &str, ordering: &str) -> Option<Self> {
        if registry != "Adobe" {
            return None;
        }
        match ordering {
            "Japan1" => Some(Self::Japan1),
            "GB1" => Some(Self::Gb1),
            "CNS1" => Some(Self::Cns1),
            "Korea1" => Some(Self::Korea1),
            _ => None,
        }
    }

    fn table(self) -> &'static [u32] {
        match self {
            Self::Japan1 => table(&JAPAN1, include_bytes!("../data/cid2uni/Japan1.bin")),
            Self::Gb1 => table(&GB1, include_bytes!("../data/cid2uni/GB1.bin")),
            Self::Cns1 => table(&CNS1, include_bytes!("../data/cid2uni/CNS1.bin")),
            Self::Korea1 => table(&KOREA1, include_bytes!("../data/cid2uni/Korea1.bin")),
        }
    }

    /// The CID the collection uses for `c` (the lowest, when several do):
    /// the width lookup key for fonts whose codes are Unicode (`Uni*-UCS2`).
    pub fn cid_of(self, c: char) -> Option<u32> {
        static INV: [OnceLock<std::collections::HashMap<u32, u32>>; 4] = [
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
            OnceLock::new(),
        ];
        let slot = match self {
            Self::Japan1 => 0,
            Self::Gb1 => 1,
            Self::Cns1 => 2,
            Self::Korea1 => 3,
        };
        let inv = INV[slot].get_or_init(|| {
            let mut m = std::collections::HashMap::new();
            for (cid, &cp) in self.table().iter().enumerate() {
                if cp != 0 {
                    m.entry(cp).or_insert(cid as u32);
                }
            }
            m
        });
        inv.get(&(c as u32)).copied()
    }

    /// Unicode for `cid`, if the collection maps it.
    pub fn to_char(self, cid: u32) -> Option<char> {
        self.table()
            .get(cid as usize)
            .copied()
            .filter(|&cp| cp != 0)
            .and_then(char::from_u32)
    }
}

static JAPAN1: OnceLock<Vec<u32>> = OnceLock::new();
static GB1: OnceLock<Vec<u32>> = OnceLock::new();
static CNS1: OnceLock<Vec<u32>> = OnceLock::new();
static KOREA1: OnceLock<Vec<u32>> = OnceLock::new();

/// Expand the run-length table on first use: index = CID, value = code point
/// (0 = unmapped). ~100 KB per collection, built only when a document needs it.
fn table<'a>(cell: &'a OnceLock<Vec<u32>>, data: &[u8]) -> &'a [u32] {
    cell.get_or_init(|| {
        let mut t: Vec<u32> = Vec::new();
        for rec in data.as_chunks::<10>().0 {
            let cid = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as usize;
            let cp = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
            let n = u16::from_le_bytes([rec[8], rec[9]]) as usize;
            // Collections stay well under 70 000 CIDs; refuse anything else.
            if cid + n > 1 << 17 {
                continue;
            }
            if t.len() < cid + n {
                t.resize(cid + n, 0);
            }
            for k in 0..n {
                t[cid + k] = cp + k as u32;
            }
        }
        t
    })
}

/// Windows TrueType families whose glyph order starts with the standard
/// Macintosh set (`.notdef`, `.null`, `nonmarkingreturn`, then `space` …
/// `asciitilde`), so glyph ID = ASCII code − 29 for 0x20–0x7E. A CID font
/// built from one of them with `Ordering (Identity)` and no ToUnicode has CIDs
/// equal to those glyph IDs; other extractors print it as text shifted by three
/// letters (`wkh` for "the").
pub fn has_standard_ascii_glyph_order(base_font: &str) -> bool {
    // Strip a subset tag (`ABCDEF+`) and any CMap suffix (`-Identity-H`).
    let name =
        base_font.split_once('+').map_or(
            base_font,
            |(tag, rest)| if tag.len() == 6 { rest } else { base_font },
        );
    const FAMILIES: [&str; 12] = [
        "Arial",
        "TimesNewRoman",
        "Times New Roman",
        "CourierNew",
        "Courier New",
        "Tahoma",
        "Verdana",
        "Calibri",
        "Cambria",
        "Georgia",
        "SegoeUI",
        "TrebuchetMS",
    ];
    FAMILIES.iter().any(|f| name.starts_with(f))
}

/// ASCII for a glyph ID in a font with the standard glyph order (see above).
pub fn standard_order_ascii(gid: u32) -> Option<char> {
    (3..=97).contains(&gid).then(|| char::from(gid as u8 + 29))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japan1_maps_kana_kanji_and_ascii() {
        let j = Collection::Japan1;
        assert_eq!(j.to_char(34), Some('A')); // CID 34 = 'A' in Adobe-Japan1
        assert_eq!(j.to_char(843), Some('あ'));
        assert_eq!(j.to_char(1125), Some('亜'));
        assert_eq!(j.to_char(1_000_000), None);
    }

    #[test]
    fn other_collections_load() {
        assert!(Collection::Gb1.to_char(4000).is_some());
        assert!(Collection::Cns1.to_char(5000).is_some());
        assert!(Collection::Korea1.to_char(1000).is_some());
        assert_eq!(Collection::from_ros("Adobe", "Identity"), None);
    }

    #[test]
    fn standard_glyph_order() {
        assert!(has_standard_ascii_glyph_order(
            "RYZYQV+ArialUnicodeMS-KSCms-UHC-H"
        ));
        assert!(!has_standard_ascii_glyph_order("ABCDEF+KozGoPro-Medium"));
        assert_eq!(standard_order_ascii(3), Some(' '));
        assert_eq!(standard_order_ascii(87), Some('t'));
        assert_eq!(standard_order_ascii(98), None);
    }
}
