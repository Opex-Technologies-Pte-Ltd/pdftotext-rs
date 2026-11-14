//! `JBIG2Decode` images (bilevel scans) to PNG, for OCR (see `png.rs`).
//!
//! Scanned manuals run through Acrobat's "Paper Capture" / "Optimize scanned
//! PDF" (and many archival scans) store each page as a JBIG2 image, sometimes
//! with symbol dictionaries shared between pages in a `/JBIG2Globals` stream.
//! Like CCITT (`fax.rs`), the page is decoded and re-encoded as a 1-bit PNG,
//! which the vision model accepts as is.
//!
//! Decoding uses the `hayro-jbig2` crate (Apache-2.0 OR MIT), a standalone
//! ITU-T T.88 codec. The PDF embedding (ISO 32000-2, 7.4.7) is the "embedded"
//! organisation: segment headers and data in sequence, no file header, and the
//! globals' segments are those with page association 0.
//!
//! Hostile input: the codec allocates bitmaps and tables from sizes declared
//! in segment headers (up to 65535 × 65535 pixels, a million symbols) before
//! reading any coded data, and an allocation failure in the wasm is a trap.
//! [`prescan`] walks the segment headers first (T.88, 7.2 and 7.4) and refuses
//! a stream whose declared sizes exceed this module's limits, so the codec
//! only ever runs on bounded work.

use hayro_jbig2::{DecodeSettings, Decoder, Globals, Page};

use super::png::{BilevelPngWriter, PngError, MAX_PIXELS, MAX_SIDE};

/// Pixels across all regions of one image (a page is usually covered once or
/// twice: a text region plus generic regions for pictures).
const MAX_REGION_PIXELS: u64 = 2 * MAX_PIXELS;
/// Symbols declared by all symbol dictionaries of an image and its globals
/// (a dense page has a few thousand distinct glyph shapes).
const MAX_SYMBOLS: u64 = 1 << 18;
/// Symbol instances placed by all text regions of an image.
const MAX_INSTANCES: u64 = 1 << 22;
/// Halftone grid cells and patterns of an image.
const MAX_GRID_CELLS: u64 = 1 << 22;
const MAX_PATTERNS: u64 = 1 << 16;
/// Segments of an image and its globals.
const MAX_SEGMENTS: usize = 1 << 16;

/// Running totals of declared work, shared by the globals and the page.
#[derive(Default)]
struct Work {
    segments: usize,
    region_pixels: u64,
    symbols: u64,
    instances: u64,
    grid_cells: u64,
    patterns: u64,
}

struct Bytes<'a> {
    s: &'a [u8],
    i: usize,
}

impl Bytes<'_> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.s.get(self.i)?;
        self.i += 1;
        Some(b)
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.s.get(self.i..self.i + 2)?;
        self.i += 2;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.s.get(self.i..self.i + 4)?;
        self.i += 4;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.i = self.i.checked_add(n).filter(|&e| e <= self.s.len())?;
        Some(())
    }
}

fn region_ok(w: u32, h: u32) -> bool {
    w <= MAX_SIDE && h <= MAX_SIDE && u64::from(w) * u64::from(h) <= MAX_PIXELS
}

/// Check the declared sizes in one segment's data against the limits.
fn check_segment(kind: u8, data: &[u8], work: &mut Work) -> Option<()> {
    let mut r = Bytes { s: data, i: 0 };
    match kind {
        // Symbol dictionary (7.4.2.1): flags, AT pixels, refinement AT pixels,
        // SDNUMEXSYMS, SDNUMNEWSYMS.
        0 => {
            let flags = r.u16()?;
            let huffman = flags & 1 != 0;
            let refagg = flags & 2 != 0;
            let template = (flags >> 10) & 3;
            let rtemplate = (flags >> 12) & 1;
            if !huffman {
                r.skip(if template == 0 { 8 } else { 2 })?;
            }
            if refagg && rtemplate == 0 {
                r.skip(4)?;
            }
            let exported = u64::from(r.u32()?);
            let new = u64::from(r.u32()?);
            work.symbols += exported.max(new);
        }
        // Pattern dictionary (7.4.4.1): flags, HDPW, HDPH, GRAYMAX.
        16 => {
            r.skip(1)?;
            let w = u64::from(r.u8()?);
            let h = u64::from(r.u8()?);
            let patterns = u64::from(r.u32()?) + 1;
            work.patterns += patterns;
            if work.patterns > MAX_PATTERNS || patterns * w * h > MAX_PIXELS {
                return None;
            }
        }
        // Text, halftone, generic and refinement regions (intermediate,
        // immediate, immediate lossless): region segment information (7.4.1).
        4 | 6 | 7 | 20 | 22 | 23 | 36 | 38 | 39 | 40 | 42 | 43 => {
            let (w, h) = (r.u32()?, r.u32()?);
            if !region_ok(w, h) {
                return None;
            }
            work.region_pixels += u64::from(w) * u64::from(h);
            r.skip(9)?; // x, y, flags
            match kind {
                // Text region (7.4.3.1): flags, Huffman flags, refinement AT
                // pixels, SBNUMINSTANCES.
                4 | 6 | 7 => {
                    let flags = r.u16()?;
                    if flags & 1 != 0 {
                        r.skip(2)?;
                    }
                    if flags & 2 != 0 && flags & 0x8000 == 0 {
                        r.skip(4)?;
                    }
                    work.instances += u64::from(r.u32()?);
                }
                // Halftone region (7.4.5.1): flags, HGW, HGH.
                20 | 22 | 23 => {
                    r.skip(1)?;
                    let cells = u64::from(r.u32()?) * u64::from(r.u32()?);
                    work.grid_cells += cells;
                }
                _ => {}
            }
        }
        // Page information (7.4.8): width, height (0xFFFFFFFF: striped, the
        // codec bounds it by the end-of-stripe rows checked after parsing).
        48 => {
            let (w, h) = (r.u32()?, r.u32()?);
            if !region_ok(w, if h == u32::MAX { 1 } else { h }) {
                return None;
            }
        }
        _ => {}
    }
    (work.region_pixels <= MAX_REGION_PIXELS
        && work.symbols <= MAX_SYMBOLS
        && work.instances <= MAX_INSTANCES
        && work.grid_cells <= MAX_GRID_CELLS)
        .then_some(())
}

/// Walk the segment headers of an embedded JBIG2 stream (T.88, 7.2) and add
/// the declared work to `work`. `None` if a header is malformed or a limit is
/// exceeded.
fn prescan(data: &[u8], work: &mut Work) -> Option<()> {
    let mut r = Bytes { s: data, i: 0 };
    while r.i < data.len() {
        work.segments += 1;
        if work.segments > MAX_SEGMENTS {
            return None;
        }
        let number = r.u32()?;
        let flags = r.u8()?;
        let kind = flags & 0x3F;
        let first = r.u8()?;
        let referred = match first >> 5 {
            c @ 0..=4 => usize::from(c),
            7 => {
                let rest = [r.u8()?, r.u8()?, r.u8()?];
                let n = u32::from_be_bytes([first & 0x1F, rest[0], rest[1], rest[2]]) as usize;
                r.skip(n.checked_add(1)?.div_ceil(8))?; // retention bits
                n
            }
            _ => return None,
        };
        let number_size = match number {
            0..=256 => 1,
            257..=65536 => 2,
            _ => 4,
        };
        r.skip(referred.checked_mul(number_size)?)?;
        r.skip(if flags & 0x40 != 0 { 4 } else { 1 })?; // page association
        let len = r.u32()?;
        let rest = &data[r.i..];
        if len == u32::MAX {
            // Unknown length (immediate generic region only): its region
            // information is at the start; the codec finds the end itself.
            return check_segment(kind, rest, work);
        }
        let len = len as usize;
        let seg = rest.get(..len)?;
        check_segment(kind, seg, work)?;
        r.i += len;
    }
    Some(())
}

/// Packs the codec's pixels into PNG rows.
struct Sink {
    png: BilevelPngWriter,
    row: Vec<u8>,
    x: usize,
    width: usize,
    /// The PNG bit (1 = white) written for a black pixel.
    black_bit: bool,
}

impl Sink {
    fn set(&mut self, x: usize, black: bool) {
        if black == self.black_bit {
            self.row[x / 8] |= 0x80 >> (x % 8);
        }
    }
}

impl Decoder for Sink {
    fn push_pixel(&mut self, black: bool) {
        if self.x < self.width {
            let x = self.x;
            self.set(x, black);
            self.x += 1;
        }
    }

    fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
        // Byte-aligned runs of 8 pixels.
        let start = self.x / 8;
        let end = (start + chunk_count as usize).min(self.row.len());
        if black == self.black_bit {
            self.row[start..end].fill(0xFF);
        }
        self.x = (self.x + 8 * chunk_count as usize).min(self.width);
    }

    fn next_line(&mut self) {
        self.png.row(&self.row);
        self.row.fill(0);
        self.x = 0;
    }
}

/// Decode an embedded JBIG2 stream (with its `/JBIG2Globals` stream, if any)
/// to a 1-bit grayscale PNG with the page's ink black on white. JBIG2 codes ink
/// as 1 and the filter hands it to the page as sample 0, which an image shows
/// as black and a stencil mask paints; `inverted` is the image's
/// `/Decode [1 0]`, which turns that around. `Undecodable` if the stream does
/// not decode or declares more work than this module allows, `TooBig` if the
/// PNG would exceed `max_out` bytes.
pub fn jbig2_to_png(
    data: &[u8],
    globals: Option<&[u8]>,
    inverted: bool,
    max_out: usize,
) -> Result<Vec<u8>, PngError> {
    let bad = PngError::Undecodable;
    let mut work = Work::default();
    if let Some(g) = globals {
        prescan(g, &mut work).ok_or(bad)?;
    }
    prescan(data, &mut work).ok_or(bad)?;
    let settings = DecodeSettings::default();
    let globals = match globals {
        Some(g) => Some(Globals::new(g, &settings).map_err(|_| bad)?),
        None => None,
    };
    let page = Page::new_embedded(data, globals.as_ref(), &settings).map_err(|_| bad)?;
    let (width, height) = (page.width(), page.height());
    if width == 0 || height == 0 || !region_ok(width, height) {
        return Err(bad);
    }
    let mut sink = Sink {
        png: BilevelPngWriter::new(width, max_out),
        row: vec![0; width.div_ceil(8) as usize],
        x: 0,
        width: width as usize,
        // Ink (1) is black on the page unless /Decode inverts it.
        black_bit: inverted,
    };
    page.decode(&mut sink).map_err(|_| bad)?;
    if sink.png.height() != height {
        return Err(bad);
    }
    sink.png.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::png::decode_rows;

    /// Segment header (short form, 1-byte page association) + data.
    fn segment(number: u32, kind: u8, page: u8, data: &[u8]) -> Vec<u8> {
        let mut s = number.to_be_bytes().to_vec();
        s.extend_from_slice(&[kind, 0, page]);
        s.extend_from_slice(&(data.len() as u32).to_be_bytes());
        s.extend_from_slice(data);
        s
    }

    fn page_info(w: u32, h: u32) -> Vec<u8> {
        let mut d = w.to_be_bytes().to_vec();
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&[0; 8]); // resolution unknown
        d.extend_from_slice(&[0, 0, 0]); // flags, striping
        d
    }

    /// Immediate lossless generic region, MMR-coded (T.6) from `rows`.
    fn mmr_region(rows: &[Vec<bool>]) -> Vec<u8> {
        let (w, h) = (rows[0].len() as u32, rows.len() as u32);
        let mut d = w.to_be_bytes().to_vec();
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&[0; 8]); // x, y
        d.push(0); // external combination operator OR
        d.push(1); // generic region flags: MMR
        let mut enc = fax::encoder::Encoder::new(fax::VecWriter::new());
        for r in rows {
            let pels = r.iter().map(|&ink| {
                if ink {
                    fax::Color::Black
                } else {
                    fax::Color::White
                }
            });
            enc.encode_line(pels, w).unwrap();
        }
        d.extend_from_slice(&enc.finish().unwrap().finish());
        d
    }

    fn sample(w: usize, h: usize) -> Vec<Vec<bool>> {
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| (x * 3 + y) % 11 == 0 || (4..7).contains(&y) || x == w - 1)
                    .collect()
            })
            .collect()
    }

    fn stream(rows: &[Vec<bool>]) -> Vec<u8> {
        let (w, h) = (rows[0].len() as u32, rows.len() as u32);
        let mut s = segment(0, 48, 1, &page_info(w, h));
        s.extend(segment(1, 39, 1, &mmr_region(rows)));
        s.extend(segment(2, 49, 1, &[])); // end of page
        s
    }

    #[test]
    fn mmr_generic_region_decodes_to_the_same_ink() {
        // Odd width: exercises the partial last byte of each row.
        let page = sample(45, 13);
        let data = stream(&page);
        let png = jbig2_to_png(&data, None, false, 1 << 20).unwrap();
        assert_eq!(decode_rows(&png), page);
        let inv = decode_rows(&jbig2_to_png(&data, None, true, 1 << 20).unwrap());
        let flipped: Vec<Vec<bool>> = page
            .iter()
            .map(|r| r.iter().map(|&b| !b).collect())
            .collect();
        assert_eq!(inv, flipped);
        // Byte-aligned width: the codec's 8-pixel chunk path.
        let page = sample(64, 9);
        let png = jbig2_to_png(&stream(&page), None, false, 1 << 20).unwrap();
        assert_eq!(decode_rows(&png), page);
        // Output budget.
        assert_eq!(jbig2_to_png(&data, None, false, 80), Err(PngError::TooBig));
    }

    #[test]
    fn garbage_is_refused() {
        assert!(jbig2_to_png(&[], None, false, 1 << 20).is_err());
        assert!(jbig2_to_png(&[0xFF; 64], None, false, 1 << 20).is_err());
        let junk: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
            .collect();
        assert!(jbig2_to_png(&junk, None, false, 1 << 20).is_err());
        // A valid page with garbage globals.
        let page = stream(&sample(16, 4));
        assert!(jbig2_to_png(&page, Some(&junk), false, 1 << 20).is_err());
    }

    #[test]
    fn declared_sizes_are_bounded_before_decoding() {
        let rows = sample(16, 4);
        // Page larger than MAX_PIXELS (within the codec's own 65535 limit).
        let mut s = segment(0, 48, 1, &page_info(30_000, 30_000));
        s.extend(segment(1, 39, 1, &mmr_region(&rows)));
        assert!(jbig2_to_png(&s, None, false, 1 << 20).is_err());
        // Small page, huge region.
        let mut region = mmr_region(&rows);
        region[..8].copy_from_slice(&[0, 0, 0xFF, 0, 0, 0, 0xFF, 0]);
        let mut s = segment(0, 48, 1, &page_info(16, 4));
        s.extend(segment(1, 39, 1, &region));
        assert!(jbig2_to_png(&s, None, false, 1 << 20).is_err());
        // A symbol dictionary declaring 2^24 symbols (Huffman, no AT bytes).
        let mut dict = vec![0, 1];
        dict.extend_from_slice(&(1u32 << 24).to_be_bytes());
        dict.extend_from_slice(&(1u32 << 24).to_be_bytes());
        let mut work = Work::default();
        assert!(prescan(&segment(0, 0, 0, &dict), &mut work).is_none());
        // Halftone grid of 2^32 cells.
        let mut ht = vec![0, 0, 0, 16, 0, 0, 0, 16];
        ht.extend_from_slice(&[0; 9]);
        ht.push(0);
        ht.extend_from_slice(&[0, 1, 0, 0, 0, 1, 0, 0]);
        let mut work = Work::default();
        assert!(prescan(&segment(1, 22, 1, &ht), &mut work).is_none());
        // Truncated header / data length past the end.
        assert!(prescan(&[0, 0, 0, 0, 48], &mut Work::default()).is_none());
        let mut s = segment(0, 48, 1, &page_info(16, 4));
        s.truncate(s.len() - 1);
        assert!(prescan(&s, &mut Work::default()).is_none());
        // The valid stream passes.
        assert!(prescan(&stream(&rows), &mut Work::default()).is_some());
    }

    #[test]
    fn mutated_streams_never_panic() {
        let base = stream(&sample(40, 20));
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..3000 {
            let mut d = base.clone();
            for _ in 0..1 + (seed % 4) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed as usize) % d.len();
                d[i] = (seed >> 32) as u8;
            }
            let _ = jbig2_to_png(&d, None, false, 1 << 20);
        }
    }
}
