//! PNG writer for decoded bilevel scans (CCITT fax in `fax.rs`, JBIG2 in
//! `jbig2.rs`). A scan up to `MAX_OUT_WIDTH` wide is written as 1-bit gray (a
//! set bit is white). A wider one, such as a 600-dpi page, is shrunk by a whole
//! factor to at most `MAX_OUT_WIDTH`, averaging each block of pixels into 8-bit
//! gray: small text stays legible (a vision model would otherwise shrink the
//! 1-bit image itself), the file is smaller, and the image stays within the
//! pixel limits vision APIs set.

use std::io::Write;

/// Largest bilevel image decoded: 120 million pixels (a 600-dpi A3 page is
/// ~70 M). Its page bitmap is 15 MB.
pub const MAX_PIXELS: u64 = 120_000_000;
/// Widest / tallest bilevel image decoded.
pub const MAX_SIDE: u32 = 40_000;
/// Widest PNG written: about 340 dpi across a Letter or A4 page (a 600-dpi
/// Letter scan, 5100 px wide, comes out 2550 px wide at 300 dpi).
pub const MAX_OUT_WIDTH: u32 = 4000;

/// Why no PNG came out of a bilevel decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngError {
    /// The stream did not decode, or declares more than the decoder allows.
    Undecodable,
    /// The PNG would exceed the caller's `max_out` bytes.
    TooBig,
}

/// Streams rows into the zlib-compressed `IDAT`, so a page never sits in
/// memory twice (decoder bitmap + PNG rows). Stops compressing once the
/// compressed size passes `max_out`.
pub struct BilevelPngWriter {
    width: u32,
    /// Input rows received.
    height: u32,
    /// Shrink factor: 1 writes 1-bit rows as given; k > 1 averages k x k
    /// blocks into 8-bit gray.
    factor: u32,
    /// Per output column, white pixels seen in the current band of rows.
    white: Vec<u32>,
    band_rows: u32,
    out_height: u32,
    z: flate2::write::ZlibEncoder<Vec<u8>>,
    max_out: usize,
    failed: bool,
}

impl BilevelPngWriter {
    pub fn new(width: u32, max_out: usize) -> Self {
        let factor = width.div_ceil(MAX_OUT_WIDTH).max(1);
        let out_w = if factor > 1 {
            width.div_ceil(factor) as usize
        } else {
            0
        };
        BilevelPngWriter {
            width,
            height: 0,
            factor,
            white: vec![0; out_w],
            band_rows: 0,
            out_height: 0,
            z: flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default()),
            max_out,
            failed: false,
        }
    }

    /// Append one row of `ceil(width / 8)` packed bytes (MSB first, 1 = white).
    pub fn row(&mut self, packed: &[u8]) {
        // Rows are counted even past the cap, so callers can still tell a
        // complete image from a truncated one.
        self.height += 1;
        if self.failed {
            return;
        }
        if self.factor == 1 {
            self.out_height += 1;
            let ok = self.z.write_all(&[0]).is_ok() // PNG filter type: none
                && self.z.write_all(packed).is_ok();
            self.check(ok);
            return;
        }
        let k = self.factor as usize;
        let width = self.width as usize;
        for (i, &byte) in packed.iter().enumerate().take(width.div_ceil(8)) {
            if byte == 0 {
                continue;
            }
            for bit in 0..8 {
                let x = i * 8 + bit;
                if x >= width {
                    break;
                }
                if byte & (0x80 >> bit) != 0 {
                    self.white[x / k] += 1;
                }
            }
        }
        self.band_rows += 1;
        if self.band_rows == self.factor {
            self.flush_band();
        }
    }

    /// Write the averaged gray row for the rows gathered so far.
    fn flush_band(&mut self) {
        if self.band_rows == 0 {
            return;
        }
        let k = self.factor;
        let mut out = Vec::with_capacity(self.white.len() + 1);
        out.push(0); // PNG filter type: none
        let last = self.white.len().saturating_sub(1);
        for (c, w) in self.white.iter_mut().enumerate() {
            // The last column may be narrower than k pixels.
            let cols = if c == last {
                self.width - k * c as u32
            } else {
                k
            };
            let n = cols * self.band_rows;
            out.push(((*w * 255 + n / 2) / n) as u8);
            *w = 0;
        }
        self.band_rows = 0;
        self.out_height += 1;
        let ok = self.z.write_all(&out).is_ok();
        self.check(ok);
    }

    fn check(&mut self, ok: bool) {
        if !ok || self.z.get_ref().len() + 64 > self.max_out {
            self.failed = true;
        }
    }

    /// Input rows received.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The PNG: `TooBig` if it would exceed `max_out` bytes, `Undecodable` if
    /// it has no rows.
    pub fn finish(mut self) -> Result<Vec<u8>, PngError> {
        if !self.failed && self.factor > 1 {
            self.flush_band();
        }
        if self.failed {
            return Err(PngError::TooBig);
        }
        if self.height == 0 || self.width == 0 {
            return Err(PngError::Undecodable);
        }
        let idat = self.z.finish().map_err(|_| PngError::Undecodable)?;
        if idat.len() + 64 > self.max_out {
            return Err(PngError::TooBig);
        }
        let (out_w, depth) = if self.factor > 1 {
            (self.white.len() as u32, 8)
        } else {
            (self.width, 1)
        };
        let mut out = Vec::with_capacity(idat.len() + 64);
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&out_w.to_be_bytes());
        ihdr.extend_from_slice(&self.out_height.to_be_bytes());
        ihdr.extend_from_slice(&[depth, 0, 0, 0, 0]); // gray, deflate, filter 0, no interlace
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &idat);
        chunk(&mut out, b"IEND", &[]);
        Ok(out)
    }
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut h = crc32fast::Hasher::new();
    h.update(kind);
    h.update(data);
    out.extend_from_slice(&h.finalize().to_be_bytes());
}

/// Decode a PNG written here back to rows of "is black" (tests only).
#[cfg(test)]
pub fn decode_rows(png: &[u8]) -> Vec<Vec<bool>> {
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap()) as usize;
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap()) as usize;
    let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
    assert_eq!(&png[37..41], b"IDAT");
    let mut raw = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::ZlibDecoder::new(&png[41..41 + idat_len]),
        &mut raw,
    )
    .unwrap();
    let rb = width.div_ceil(8);
    assert_eq!(raw.len(), height * (rb + 1));
    (0..height)
        .map(|y| {
            let row = &raw[y * (rb + 1) + 1..(y + 1) * (rb + 1)];
            (0..width)
                .map(|x| row[x / 8] & (0x80 >> (x % 8)) == 0)
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_structure_is_valid() {
        let mut w = BilevelPngWriter::new(3, 1 << 20);
        w.row(&[0b1010_0000]);
        w.row(&[0b0100_0000]);
        let png = w.finish().unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..20], &3u32.to_be_bytes());
        assert_eq!(&png[20..24], &2u32.to_be_bytes());
        assert!(png.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]));
        assert_eq!(
            decode_rows(&png),
            vec![vec![false, true, false], vec![true, false, true]]
        );
    }

    #[test]
    fn wide_scans_are_averaged_to_gray() {
        // 9000 px wide: factor 3 -> 3000 px, 8-bit gray.
        let width = 9000u32;
        let rb = width.div_ceil(8) as usize;
        let mut w = BilevelPngWriter::new(width, 1 << 22);
        let white = vec![0xFFu8; rb];
        let black = vec![0u8; rb];
        // Rows 0-2: one band, two of three rows white -> 170.
        w.row(&white);
        w.row(&black);
        w.row(&white);
        // Row 3: a partial last band, all black -> 0.
        w.row(&black);
        assert_eq!(w.height(), 4);
        let png = w.finish().unwrap();
        assert_eq!(&png[16..20], &3000u32.to_be_bytes());
        assert_eq!(&png[20..24], &2u32.to_be_bytes());
        assert_eq!(png[24], 8);
        let rows = gray8_rows(&png);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].iter().all(|&v| v == 170));
        assert!(rows[1].iter().all(|&v| v == 0));

        // A width that is not a multiple: 8002 px -> factor 3 -> 2668 px; the
        // last column covers one pixel only, and that pixel is white.
        let width = 8002u32;
        let rb = width.div_ceil(8) as usize;
        let mut row = vec![0u8; rb];
        row[1000] = 0x40; // pixel 8001
        let mut w = BilevelPngWriter::new(width, 1 << 22);
        w.row(&row);
        let rows = gray8_rows(&w.finish().unwrap());
        assert_eq!(rows[0].len(), 2668);
        assert_eq!(rows[0][2667], 255);
        assert_eq!(rows[0][2666], 0);
    }

    fn gray8_rows(png: &[u8]) -> Vec<Vec<u8>> {
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap()) as usize;
        let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        let mut raw = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::ZlibDecoder::new(&png[41..41 + idat_len]),
            &mut raw,
        )
        .unwrap();
        raw.chunks(width + 1).map(|r| r[1..].to_vec()).collect()
    }

    #[test]
    fn output_cap_is_enforced() {
        let mut w = BilevelPngWriter::new(8, 70);
        // Incompressible-ish rows push the IDAT past the cap.
        for i in 0..2000u32 {
            w.row(&[(i.wrapping_mul(2654435761) >> 7) as u8]);
        }
        assert_eq!(w.finish(), Err(PngError::TooBig));
        assert_eq!(
            BilevelPngWriter::new(8, 1 << 20).finish(),
            Err(PngError::Undecodable)
        );
    }
}
