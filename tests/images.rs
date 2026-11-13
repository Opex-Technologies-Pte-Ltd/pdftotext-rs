//! Page image extraction (`Document::page_images`, the OCR path for scanned
//! pages): placement, codecs, budget, and the same hostile-input contract as
//! the text path (no panic, bounded work, typed errors). Fixtures are built
//! in-process from the spec.

use std::io::Write;
use std::time::{Duration, Instant};

use pdftotext::reader::images::{Codec, Omitted, MAX_IMAGES_PER_PAGE};
use pdftotext::{Document, Error};

#[derive(Default)]
struct Pdf {
    objs: Vec<(u32, Vec<u8>)>,
}

impl Pdf {
    fn obj(&mut self, n: u32, body: impl AsRef<[u8]>) -> &mut Self {
        self.objs.retain(|(k, _)| *k != n);
        self.objs.push((n, body.as_ref().to_vec()));
        self
    }

    fn stream(&mut self, n: u32, dict_extra: &str, data: &[u8]) -> &mut Self {
        let mut body = format!("<</Length {} {dict_extra}>>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.obj(n, body)
    }

    fn build(&self) -> Vec<u8> {
        let size = self.objs.iter().map(|(n, _)| *n).max().unwrap_or(0) + 1;
        let mut offsets = vec![None; size as usize];
        let mut buf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
        for (n, body) in &self.objs {
            offsets[*n as usize] = Some(buf.len());
            writeln!(buf, "{n} 0 obj").unwrap();
            buf.extend_from_slice(body);
            buf.extend_from_slice(b"\nendobj\n");
        }
        let xref_off = buf.len();
        write!(buf, "xref\n0 {size}\n").unwrap();
        buf.extend_from_slice(b"0000000000 65535 f \n");
        for off in offsets.iter().skip(1) {
            match off {
                Some(o) => writeln!(buf, "{o:010} 00000 n ").unwrap(),
                None => buf.extend_from_slice(b"0000000000 65535 f \n"),
            }
        }
        write!(
            buf,
            "trailer\n<</Size {size}/Root 1 0 R>>\nstartxref\n{xref_off}\n%%EOF"
        )
        .unwrap();
        buf
    }
}

/// A minimal baseline JPEG header: SOI, SOF0 (8-bit, 2×1, `nf` components), EOI.
fn fake_jpeg(nf: u8) -> Vec<u8> {
    let mut v = vec![
        0xFF,
        0xD8,
        0xFF,
        0xC0,
        0x00,
        8 + 3 * nf,
        0x08,
        0x00,
        0x01,
        0x00,
        0x02,
        nf,
    ];
    for c in 0..nf {
        v.extend_from_slice(&[c + 1, 0x11, 0x00]);
    }
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn zlib(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// Page (612×792) with content `content` and XObjects `Im1` (obj 6), `Im2`
/// (obj 7), `Fm1` (obj 8) available in its resources.
fn page(content: &[u8]) -> Pdf {
    let mut p = Pdf::default();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</XObject<</Im1 6 0 R/Im2 7 0 R/Fm1 8 0 R>>>>>>",
        )
        .stream(4, "", content);
    p
}

fn image_dict(w: u32, h: u32, extra: &str) -> String {
    format!("/Type/XObject/Subtype/Image/Width {w}/Height {h}/BitsPerComponent 8/ColorSpace/DeviceRGB {extra}")
}

fn within(limit: Duration, f: impl FnOnce()) {
    let t = Instant::now();
    f();
    assert!(t.elapsed() < limit, "took {:?}", t.elapsed());
}

#[test]
fn full_page_jpeg_is_returned_as_stored_with_placement() {
    let jpg = fake_jpeg(3);
    let mut p = page(b"q 612 0 0 792 0 0 cm /Im1 Do Q");
    p.stream(6, &image_dict(2550, 3300, "/Filter/DCTDecode"), &jpg);
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let mut budget = usize::MAX;
    let imgs = doc.page_images(0, &mut budget).unwrap();
    assert_eq!(imgs.page_number, 1);
    assert_eq!(imgs.images.len(), 1);
    let im = &imgs.images[0];
    assert_eq!((im.width, im.height), (2550, 3300));
    assert_eq!(im.codec, Codec::Jpeg);
    assert_eq!(im.color_space.as_deref(), Some("DeviceRGB"));
    assert_eq!(im.data.as_deref(), Some(jpg.as_slice()));
    assert!((im.coverage - 1.0).abs() < 1e-9, "{}", im.coverage);
    assert_eq!((im.placed.x1, im.placed.y1), (612.0, 792.0));
    assert_eq!(budget, usize::MAX - jpg.len());
    assert_eq!(
        pdftotext::reader::images::jpeg_components(im.data.as_ref().unwrap()),
        Some(3)
    );
}

#[test]
fn default_parse_reports_images_without_payload() {
    let mut p = page(b"612 0 0 792 0 0 cm /Im1 Do");
    p.stream(6, &image_dict(10, 10, "/Filter/DCTDecode"), &fake_jpeg(3));
    let doc = Document::parse(&p.build()).unwrap();
    let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
    assert_eq!(imgs.images[0].omitted, Some(Omitted::NotLoaded));
    assert!(imgs.images[0].data.is_none());
}

#[test]
fn undecodable_ccitt_is_metadata_only_and_jpx_is_flagged() {
    let mut p = page(b"q 300 0 0 300 0 0 cm /Im1 Do Q q 100 0 0 100 0 400 cm /Im2 Do Q");
    p.stream(
        6,
        "/Subtype/Image/Width 1704/Height 2208/BitsPerComponent 1/ColorSpace/DeviceGray/Filter/CCITTFaxDecode/DecodeParms<</K -1/Columns 1704>>",
        b"\x00\x01\x02",
    )
    .stream(
        7,
        "/Subtype/Image/Width 4/Height 4/Filter/JPXDecode",
        b"\xFF\x4F\xFF\x51rest",
    );
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
    let ccitt = &imgs.images[0];
    assert_eq!(ccitt.codec, Codec::Ccitt);
    assert_eq!(ccitt.omitted, Some(Omitted::NotEncoded));
    assert_eq!(ccitt.bits_per_component, Some(1));
    assert_eq!(ccitt.encoded_len, 3);
    let jpx = &imgs.images[1];
    assert_eq!(jpx.codec, Codec::Jpeg2000);
    assert!(jpx.data.is_some());
    assert!(jpx.coverage < 0.03);
}

/// An embedded JBIG2 stream (T.88 7.2, 7.4): page information plus one
/// immediate lossless generic region, MMR-coded from `rows` (`true` = ink).
fn jbig2_page(rows: &[Vec<bool>]) -> Vec<u8> {
    let (w, h) = (rows[0].len() as u32, rows.len() as u32);
    let seg = |number: u32, kind: u8, data: &[u8]| {
        let mut s = number.to_be_bytes().to_vec();
        s.extend_from_slice(&[kind, 0, 1]);
        s.extend_from_slice(&(data.len() as u32).to_be_bytes());
        s.extend_from_slice(data);
        s
    };
    let mut info = w.to_be_bytes().to_vec();
    info.extend_from_slice(&h.to_be_bytes());
    info.extend_from_slice(&[0; 11]);
    let mut region = w.to_be_bytes().to_vec();
    region.extend_from_slice(&h.to_be_bytes());
    region.extend_from_slice(&[0; 9]);
    region.push(1); // MMR
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
    region.extend_from_slice(&enc.finish().unwrap().finish());
    let mut s = seg(0, 48, &info);
    s.extend(seg(1, 39, &region));
    s.extend(seg(2, 49, &[]));
    s
}

/// Rows of "is black" of a 1-bit grayscale PNG from `page_images`.
fn png_ink(png: &[u8]) -> Vec<Vec<bool>> {
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let width = u32::from_be_bytes(png[16..20].try_into().unwrap()) as usize;
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap()) as usize;
    assert_eq!(&png[24..26], &[1, 0], "1-bit grayscale");
    let idat_len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
    let mut raw = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::ZlibDecoder::new(&png[41..41 + idat_len]),
        &mut raw,
    )
    .unwrap();
    let rb = width.div_ceil(8);
    (0..height)
        .map(|y| {
            let row = &raw[y * (rb + 1) + 1..(y + 1) * (rb + 1)];
            (0..width)
                .map(|x| row[x / 8] & (0x80 >> (x % 8)) == 0)
                .collect()
        })
        .collect()
}

#[test]
fn jbig2_images_decode_to_png_with_globals_masks_and_decode_arrays() {
    let rows: Vec<Vec<bool>> = (0..20)
        .map(|y| (0..37).map(|x| (x + 2 * y) % 9 == 0 || y == 10).collect())
        .collect();
    let data = jbig2_page(&rows);
    let inverted: Vec<Vec<bool>> = rows
        .iter()
        .map(|r| r.iter().map(|&b| !b).collect())
        .collect();
    let mut p = page(b"q 300 0 0 300 0 0 cm /Im1 Do Q q 100 0 0 100 0 400 cm /Im2 Do Q");
    // Im1: Flate in front of JBIG2, with an (empty) globals stream.
    p.stream(
        6,
        "/Subtype/Image/Width 37/Height 20/BitsPerComponent 1/ColorSpace/DeviceGray/Filter[/FlateDecode/JBIG2Decode]/DecodeParms[null<</JBIG2Globals 9 0 R>>]",
        &zlib(&data),
    )
    // Im2: a stencil mask with /Decode [1 0].
    .stream(
        7,
        "/Subtype/Image/Width 37/Height 20/ImageMask true/Decode[1 0]/Filter/JBIG2Decode",
        &data,
    )
    .stream(9, "", b"");
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let mut budget = usize::MAX;
    let imgs = doc.page_images(0, &mut budget).unwrap();
    let im = &imgs.images[0];
    assert_eq!(im.codec, Codec::Jbig2);
    assert_eq!(im.format, Some("png"));
    assert_eq!(png_ink(im.data.as_ref().unwrap()), rows);
    let mask = &imgs.images[1];
    assert!(mask.image_mask && mask.decode_inverted);
    assert_eq!(png_ink(mask.data.as_ref().unwrap()), inverted);
    assert_eq!(
        budget,
        usize::MAX - im.data.as_ref().unwrap().len() - mask.data.as_ref().unwrap().len()
    );
    // A budget smaller than the PNG.
    let imgs = doc.page_images(0, &mut 40).unwrap();
    assert_eq!(imgs.images[0].omitted, Some(Omitted::Budget));
}

#[test]
fn hostile_jbig2_is_metadata_only_and_bounded() {
    let rows = vec![vec![true, false, true, false, true, false, true, false]; 4];
    let good = jbig2_page(&rows);
    // Page information declaring 60000 × 60000 pixels (3.6 G): refused before decoding.
    let mut huge = good.clone();
    huge[11..19].copy_from_slice(&[0, 0, 0xEA, 0x60, 0, 0, 0xEA, 0x60]);
    let junk: Vec<u8> = (0..8192u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 9) as u8)
        .collect();
    let mut p = page(b"q 300 0 0 300 0 0 cm /Im1 Do Q /Im2 Do /Fm1 Do");
    p.stream(6, "/Subtype/Image/Width 60000/Height 60000/BitsPerComponent 1/Filter/JBIG2Decode", &huge)
        .stream(7, "/Subtype/Image/Width 8/Height 4/BitsPerComponent 1/Filter/JBIG2Decode", &junk)
        // A valid page whose globals are garbage.
        .stream(
            8,
            "/Subtype/Image/Width 8/Height 4/BitsPerComponent 1/Filter/JBIG2Decode/DecodeParms<</JBIG2Globals 9 0 R>>",
            &good,
        )
        .stream(9, "", &junk);
    // Fm1 is obj 8 here: it is an image, painted through the Fm1 name.
    within(Duration::from_secs(5), || {
        let doc = Document::parse_keeping_images(&p.build()).unwrap();
        let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
        assert_eq!(imgs.images.len(), 3);
        for im in &imgs.images {
            assert_eq!(im.codec, Codec::Jbig2);
            assert_eq!(im.omitted, Some(Omitted::NotEncoded), "{im:?}");
        }
    });
}

#[test]
fn flate_wrapped_jpeg_is_unwrapped_and_bad_signatures_rejected() {
    let jpg = fake_jpeg(1);
    let mut p = page(b"/Im1 Do /Im2 Do");
    p.stream(
        6,
        &image_dict(2, 1, "/Filter[/FlateDecode/DCTDecode]"),
        &zlib(&jpg),
    )
    .stream(7, &image_dict(2, 1, "/Filter/DCT"), b"not a jpeg");
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
    assert_eq!(imgs.images[0].data.as_deref(), Some(jpg.as_slice()));
    assert_eq!(imgs.images[0].filters, vec!["FlateDecode", "DCTDecode"]);
    assert_eq!(imgs.images[1].omitted, Some(Omitted::BadSignature));
    assert_eq!(imgs.images[1].filters, vec!["DCTDecode"]); // abbreviation normalised
}

#[test]
fn images_inside_forms_get_the_form_matrix_and_cycles_terminate() {
    let mut p = page(b"q 2 0 0 2 0 0 cm /Fm1 Do Q");
    p.stream(6, &image_dict(5, 5, "/Filter/DCTDecode"), &fake_jpeg(3))
        .stream(
            8,
            "/Subtype/Form/Matrix[1 0 0 1 10 10]/Resources<</XObject<</Im1 6 0 R/Fm1 8 0 R>>>>",
            b"q 100 0 0 50 0 0 cm /Im1 Do Q /Fm1 Do",
        );
    within(Duration::from_secs(5), || {
        let doc = Document::parse_keeping_images(&p.build()).unwrap();
        let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
        assert_eq!(imgs.images.len(), 1);
        let r = imgs.images[0].placed;
        // unit square × [100 0 0 50 0 0] × form [1 0 0 1 10 10] × page [2 0 0 2 0 0]
        assert_eq!((r.x0, r.y0, r.x1, r.y1), (20.0, 20.0, 220.0, 120.0));
    });
}

#[test]
fn repeated_image_is_reported_once_with_its_largest_placement() {
    let mut p = page(b"q 10 0 0 10 0 0 cm /Im1 Do Q q 612 0 0 792 0 0 cm /Im1 Do Q /Im1 Do");
    p.stream(6, &image_dict(5, 5, "/Filter/DCTDecode"), &fake_jpeg(3));
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
    assert_eq!(imgs.images.len(), 1);
    assert!((imgs.images[0].coverage - 1.0).abs() < 1e-9);
}

#[test]
fn budget_limits_returned_bytes() {
    let jpg = fake_jpeg(3);
    let mut p = page(b"/Im1 Do /Im2 Do");
    p.stream(6, &image_dict(5, 5, "/Filter/DCTDecode"), &jpg)
        .stream(7, &image_dict(5, 5, "/Filter/DCTDecode"), &jpg);
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let mut budget = jpg.len() + 1;
    let imgs = doc.page_images(0, &mut budget).unwrap();
    assert!(imgs.images[0].data.is_some());
    assert_eq!(imgs.images[1].omitted, Some(Omitted::Budget));
    assert_eq!(budget, 1);
}

#[test]
fn hostile_matrices_dimensions_and_missing_resources_are_survived() {
    let mut p = page(
        b"1e308 0 0 1e308 0 0 cm NaN 0 0 1 0 0 cm /Im1 Do Q Q Q q q /Nope Do /Im2 Do (unterminated",
    );
    p.stream(
        6,
        "/Subtype/Image/Width -5/Height 99999999999/Filter/DCTDecode",
        &fake_jpeg(3),
    )
    .obj(7, "<</Subtype/Image>>"); // not a stream at all
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
    let im = &imgs.images[0];
    assert_eq!(im.width, 0); // negative width is not a size
    assert!(im.placed.x1.is_finite() && im.coverage.is_finite());
    assert_eq!(imgs.images[1].omitted, Some(Omitted::NotLoaded));
}

#[test]
fn image_count_and_inline_images_are_bounded() {
    let mut content = Vec::new();
    // Inline image whose data contains "Do" and a fake "EI" that is not standalone.
    content.extend_from_slice(b"BI /W 1 /H 1 /BPC 8 /CS /G ID \x00/Im1 DoEI\x01 EI ");
    let mut p = Pdf::default();
    let mut names = String::new();
    for i in 0..(MAX_IMAGES_PER_PAGE + 10) {
        let n = 10 + i as u32;
        names.push_str(&format!("/I{i} {n} 0 R"));
        content.extend_from_slice(format!("/I{i} Do ").as_bytes());
        p.stream(n, &image_dict(1, 1, "/Filter/FlateDecode"), &zlib(b"abc"));
    }
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            format!("<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</XObject<<{names}>>>>>>"),
        )
        .stream(4, "", &content);
    let doc = Document::parse_keeping_images(&p.build()).unwrap();
    let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
    assert_eq!(imgs.images.len(), MAX_IMAGES_PER_PAGE);
    assert!(imgs.truncated);
    assert!(imgs.images.iter().all(|i| i.codec == Codec::Deflate));
}

#[test]
fn form_fan_out_is_bounded() {
    // Fm1 invokes Fm2 ten times, Fm2 invokes Fm3 ten times, … (10^8 paths).
    let mut p = Pdf::default();
    p.obj(1, "<</Type/Catalog/Pages 2 0 R>>")
        .obj(2, "<</Type/Pages/Kids[3 0 R]/Count 1>>")
        .obj(
            3,
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]/Contents 4 0 R/Resources<</XObject<</F 10 0 R>>>>>>",
        )
        .stream(4, "", b"/F Do");
    for level in 0..8u32 {
        let next = 11 + level;
        p.stream(
            10 + level,
            &format!("/Subtype/Form/Resources<</XObject<</F {next} 0 R>>>>"),
            "/F Do ".repeat(10).as_bytes(),
        );
    }
    p.stream(18, &image_dict(1, 1, "/Filter/DCTDecode"), &fake_jpeg(3));
    within(Duration::from_secs(5), || {
        let doc = Document::parse_keeping_images(&p.build()).unwrap();
        let imgs = doc.page_images(0, &mut usize::MAX.clone()).unwrap();
        assert!(imgs.truncated);
    });
}

#[test]
fn out_of_range_page_is_a_typed_error() {
    let doc = Document::parse_keeping_images(&page(b"").build()).unwrap();
    assert!(matches!(
        doc.page_images(5, &mut 0),
        Err(Error::Unsupported(_))
    ));
}

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn mutated_pdfs_never_panic_or_hang() {
    let mut p = page(b"q 612 0 0 792 0 0 cm /Im1 Do Q q 1 0 0 1 5 5 cm /Fm1 Do Q /Im2 Do");
    p.stream(6, &image_dict(5, 5, "/Filter/DCTDecode"), &fake_jpeg(3))
        .stream(
            7,
            &image_dict(5, 5, "/Filter[/FlateDecode/DCTDecode]"),
            &zlib(&fake_jpeg(1)),
        )
        .stream(
            8,
            "/Subtype/Form/Matrix[1 0 0 1 5 5]/Resources<</XObject<</Im1 6 0 R/Fm1 8 0 R>>>>",
            b"/Im1 Do /Fm1 Do",
        );
    let base = p.build();
    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    within(Duration::from_secs(60), || {
        for round in 0..400 {
            let mut m = base.clone();
            for _ in 0..(1 + rng.next() % 12) {
                let at = (rng.next() as usize) % m.len();
                match rng.next() % 4 {
                    0 => m[at] = rng.next() as u8,
                    1 => m[at] ^= 1 << (rng.next() % 8),
                    2 => m.truncate(at.max(8)),
                    _ => {
                        let end = (at + (rng.next() as usize % 32)).min(m.len());
                        m.drain(at..end);
                    }
                }
                if m.is_empty() {
                    break;
                }
            }
            let r = std::panic::catch_unwind(|| {
                if let Ok(doc) = Document::parse_keeping_images(&m) {
                    for i in 0..doc.page_count() {
                        let _ = doc.page_images(i, &mut 1_000_000);
                    }
                }
            });
            assert!(r.is_ok(), "panic on mutation round {round}");
        }
    });
}

/// Real scans, when present locally (`/tmp/manuals`, not in the repo).
#[test]
fn real_scanned_manuals_when_available() {
    let scan = std::path::Path::new("/tmp/manuals/sears-compressor-scan.pdf");
    if let Ok(bytes) = std::fs::read(scan) {
        let doc = Document::parse_keeping_images(&bytes).unwrap();
        let mut budget = usize::MAX;
        for i in 0..doc.page_count() {
            let p = doc.page_images(i, &mut budget).unwrap();
            let im = &p.images[0];
            assert_eq!(im.codec, Codec::Jpeg);
            assert!(im.coverage > 0.99);
            assert!(im.data.as_ref().unwrap().starts_with(&[0xFF, 0xD8]));
        }
    }
    let fax = std::path::Path::new("/tmp/manuals/sears-compressor-imgonly.pdf");
    if let Ok(bytes) = std::fs::read(fax) {
        let doc = Document::parse_keeping_images(&bytes).unwrap();
        for i in 0..doc.page_count() {
            let p = doc.page_images(i, &mut usize::MAX.clone()).unwrap();
            // Fax scans come back decoded, as 1-bit PNGs of the full page.
            let im = &p.images[0];
            assert_eq!(im.codec, Codec::Ccitt);
            assert_eq!(im.format, Some("png"), "page {}", i + 1);
            assert!(im.data.as_ref().unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        }
    }
}

/// A real JBIG2 scan from the public corpus (`corpus/manuals.tsv`, fetched by
/// `scripts/fetch-corpus.sh` into `corpus/pdfs/`), when present.
#[test]
fn real_jbig2_manual_when_available() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/pdfs/acdc-rsf-jbig2.pdf");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let doc = Document::parse_keeping_images(&bytes).unwrap();
    assert_eq!(doc.page_count(), 50);
    let mut budget = usize::MAX;
    // 34 Mpixel pages: every page in release, a sample (portrait, landscape,
    // rotated drawing) in a debug build.
    let pages: Vec<usize> = if cfg!(debug_assertions) {
        vec![1, 19, 42]
    } else {
        (0..doc.page_count()).collect()
    };
    for i in pages {
        let p = doc.page_images(i, &mut budget).unwrap();
        // Each page is one 600-dpi JBIG2 scan (plus a few 1×1 inline stencils
        // that `page_images` does not list).
        let im = &p.images[0];
        assert_eq!(im.codec, Codec::Jbig2, "page {}", i + 1);
        assert_eq!(im.format, Some("png"), "page {}", i + 1);
        assert!(im.coverage > 0.95, "page {}", i + 1);
        let png = im.data.as_ref().unwrap();
        assert_eq!(&png[16..20], &im.width.to_be_bytes());
        assert_eq!(&png[20..24], &im.height.to_be_bytes());
        if i == 1 {
            // The safety-precautions page: mostly paper, some ink.
            let ink = png_ink(png);
            let black: usize = ink.iter().map(|r| r.iter().filter(|&&b| b).count()).sum();
            let frac = black as f64 / (im.width as f64 * im.height as f64);
            assert!((0.01..0.3).contains(&frac), "ink fraction {frac}");
        }
    }
}
