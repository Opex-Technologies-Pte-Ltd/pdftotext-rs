//! `CCITTFaxDecode` images (fax-style scans) to PNG, for OCR (see `png.rs`).
//!
//! Old manuals scanned in black and white are usually stored as CCITT Group 4
//! (sometimes Group 3) images. A vision model cannot read the raw fax stream,
//! and decoding it to 8-bit pixels would cost a byte per pixel; a 1-bit PNG of
//! a 300-dpi page is a few hundred KB and goes to the model as is.
//!
//! Decoding uses the `fax` crate (MIT), a standalone ITU-T T.4/T.6 codec.

use lopdf::{Dictionary, Document as LoDoc, Object};

use super::png::{BilevelPngWriter, PngError, MAX_PIXELS, MAX_SIDE};

/// `/DecodeParms` of a `CCITTFaxDecode` filter (ISO 32000-2, 7.4.6).
pub struct FaxParams {
    /// < 0: Group 4; 0: Group 3 one-dimensional; > 0: Group 3 mixed.
    pub k: i64,
    pub columns: u32,
    pub rows: Option<u32>,
    pub black_is_1: bool,
}

impl FaxParams {
    /// Read the parameters for the stream's (single) CCITT filter. `width` and
    /// `height` are the image's, used when `/Columns`/`/Rows` are absent.
    pub fn read(doc: &LoDoc, dict: &Dictionary, width: u32, height: u32) -> Self {
        let parms = match dict.get(b"DecodeParms").ok().map(|o| deref(doc, o)) {
            Some(Object::Dictionary(d)) => Some(d),
            // One entry per filter; the CCITT one is the last.
            Some(Object::Array(a)) => a
                .last()
                .map(|o| deref(doc, o))
                .and_then(|o| o.as_dict().ok()),
            _ => None,
        };
        let int = |key: &[u8]| match parms.and_then(|d| d.get(key).ok()).map(|o| deref(doc, o)) {
            Some(Object::Integer(i)) => Some(*i),
            _ => None,
        };
        let black_is_1 = matches!(
            parms
                .and_then(|d| d.get(b"BlackIs1").ok())
                .map(|o| deref(doc, o)),
            Some(Object::Boolean(true))
        );
        FaxParams {
            k: int(b"K").unwrap_or(0),
            columns: int(b"Columns")
                .and_then(|c| u32::try_from(c).ok())
                .filter(|&c| c > 0)
                .unwrap_or(if width > 0 { width } else { 1728 }),
            rows: int(b"Rows")
                .and_then(|r| u32::try_from(r).ok())
                .filter(|&r| r > 0)
                .or((height > 0).then_some(height)),
            black_is_1,
        }
    }
}

fn deref<'a>(doc: &'a LoDoc, o: &'a Object) -> &'a Object {
    match o {
        Object::Reference(r) => doc.get_object(*r).unwrap_or(o),
        _ => o,
    }
}

/// Decode a CCITT stream to a 1-bit grayscale PNG with black ink as 0 (black)
/// on white, as the page shows it. `inverted` is the image's `/Decode [1 0]`
/// (or a stencil mask's equivalent). `Undecodable` if the stream does not
/// decode or is too large, `TooBig` if the PNG would exceed `max_out` bytes.
pub fn ccitt_to_png(
    data: &[u8],
    p: &FaxParams,
    inverted: bool,
    max_out: usize,
) -> Result<Vec<u8>, PngError> {
    let width = p.columns;
    if width == 0 || width > MAX_SIDE {
        return Err(PngError::Undecodable);
    }
    let max_rows = p.rows.unwrap_or(MAX_SIDE).min(MAX_SIDE);
    if u64::from(width) * u64::from(max_rows) > MAX_PIXELS {
        return Err(PngError::Undecodable);
    }
    let row_bytes = width.div_ceil(8) as usize;
    let mut png = BilevelPngWriter::new(width, max_out);
    let mut row = vec![0u8; row_bytes];
    // Fax "black" is ink. The filter emits ink as 0 unless /BlackIs1; the image
    // then shows sample 0 as black unless /Decode inverts it. Either flip turns
    // the page's ink white; both cancel.
    let ink_is_white = p.black_is_1 ^ inverted;
    let mut push_line = |transitions: &[u32]| {
        if png.height() >= max_rows {
            return;
        }
        row.fill(0);
        for (x, c) in fax::decoder::pels(transitions, width)
            .enumerate()
            .take(width as usize)
        {
            let ink = c == fax::Color::Black;
            // PNG 1-bit gray: 1 = white.
            if ink == ink_is_white {
                row[x / 8] |= 0x80 >> (x % 8);
            }
        }
        png.row(&row);
    };
    // No height hint: with one, the decoder pads missing lines with white,
    // and a stream that decodes nothing would pass as a blank page.
    let ok = if p.k < 0 {
        fax::decoder::decode_g4(data.iter().copied(), width, None, &mut push_line)
    } else {
        fax::decoder::decode_g3(data.iter().copied(), &mut push_line)
    };
    // A truncated stream still yields the lines decoded so far; a stream that
    // decodes (almost) nothing is a failure.
    let height = png.height();
    if height == 0 || (ok.is_none() && height < 8) {
        return Err(PngError::Undecodable);
    }
    // Encoders may omit trailing white lines; pad to the declared height.
    if p.rows.is_some() {
        row.fill(if ink_is_white { 0x00 } else { 0xFF });
        while png.height() < max_rows {
            png.row(&row);
        }
    }
    png.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::png::decode_rows;

    /// G4-encode a bitmap (`true` = ink) with the same crate's encoder.
    fn g4(rows: &[Vec<bool>]) -> Vec<u8> {
        let width = rows[0].len() as u32;
        let mut enc = fax::encoder::Encoder::new(fax::VecWriter::new());
        for r in rows {
            let pels = r.iter().map(|&ink| {
                if ink {
                    fax::Color::Black
                } else {
                    fax::Color::White
                }
            });
            enc.encode_line(pels, width).unwrap();
        }
        enc.finish().unwrap().finish()
    }

    #[test]
    fn group4_scan_decodes_to_the_same_ink() {
        let (w, h) = (40usize, 12usize);
        let page: Vec<Vec<bool>> = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| (x + y) % 7 == 0 || (5..9).contains(&y))
                    .collect()
            })
            .collect();
        let data = g4(&page);
        let p = FaxParams {
            k: -1,
            columns: w as u32,
            rows: Some(h as u32),
            black_is_1: false,
        };
        let png = ccitt_to_png(&data, &p, false, 1 << 20).unwrap();
        assert_eq!(decode_rows(&png), page);
        // /BlackIs1 or /Decode [1 0] alone flips what reaches the page; both cancel.
        let p1 = FaxParams {
            black_is_1: true,
            ..p
        };
        let flipped = decode_rows(&ccitt_to_png(&data, &p1, false, 1 << 20).unwrap());
        assert_eq!(flipped[0][0], !page[0][0]);
        let both = decode_rows(&ccitt_to_png(&data, &p1, true, 1 << 20).unwrap());
        assert_eq!(both, page);
    }

    #[test]
    fn garbage_and_oversize_are_refused() {
        let p = FaxParams {
            k: -1,
            columns: 50_000,
            rows: Some(10),
            black_is_1: false,
        };
        assert!(ccitt_to_png(&[0xFF; 64], &p, false, 1 << 20).is_err());
        let p = FaxParams {
            k: -1,
            columns: 100,
            rows: Some(10),
            black_is_1: false,
        };
        assert!(ccitt_to_png(&[], &p, false, 1 << 20).is_err());
    }
}
