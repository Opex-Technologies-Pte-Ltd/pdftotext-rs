//! Minimal 1-bit grayscale PNG writer for decoded bilevel scans (CCITT fax in
//! `fax.rs`, JBIG2 in `jbig2.rs`). PNG 1-bit gray: a set bit is white.

use std::io::Write;

/// Largest bilevel image decoded: 120 million pixels (a 600-dpi A3 page is
/// ~70 M). Its page bitmap is 15 MB.
pub const MAX_PIXELS: u64 = 120_000_000;
/// Widest / tallest bilevel image decoded.
pub const MAX_SIDE: u32 = 40_000;

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
pub struct Gray1Writer {
    width: u32,
    height: u32,
    z: flate2::write::ZlibEncoder<Vec<u8>>,
    max_out: usize,
    failed: bool,
}

impl Gray1Writer {
    pub fn new(width: u32, max_out: usize) -> Self {
        Gray1Writer {
            width,
            height: 0,
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
        let ok = self.z.write_all(&[0]).is_ok() // PNG filter type: none
            && self.z.write_all(packed).is_ok();
        if !ok || self.z.get_ref().len() + 64 > self.max_out {
            self.failed = true;
        }
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The PNG: `TooBig` if it would exceed `max_out` bytes, `Undecodable` if
    /// it has no rows.
    pub fn finish(self) -> Result<Vec<u8>, PngError> {
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
        let mut out = Vec::with_capacity(idat.len() + 64);
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        ihdr.extend_from_slice(&[1, 0, 0, 0, 0]); // depth 1, gray, deflate, filter 0, no interlace
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
        let mut w = Gray1Writer::new(3, 1 << 20);
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
    fn output_cap_is_enforced() {
        let mut w = Gray1Writer::new(8, 70);
        // Incompressible-ish rows push the IDAT past the cap.
        for i in 0..2000u32 {
            w.row(&[(i.wrapping_mul(2654435761) >> 7) as u8]);
        }
        assert_eq!(w.finish(), Err(PngError::TooBig));
        assert_eq!(
            Gray1Writer::new(8, 1 << 20).finish(),
            Err(PngError::Undecodable)
        );
    }
}
