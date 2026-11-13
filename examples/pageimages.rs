//! Dev tool: list the image XObjects of every page (what the OCR path receives)
//! and optionally write the payloads (JPEG, JPEG 2000, PNG) to a directory.
//!
//! Usage: `cargo run --release --example pageimages -- file.pdf [outdir]`

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: pageimages <file.pdf> [outdir]");
    let out = args.next();
    let bytes = std::fs::read(&path).expect("read");
    let doc = pdftotext::Document::parse_keeping_images(&bytes).expect("parse");
    let mut budget = usize::MAX;
    for idx in 0..doc.page_count() {
        let p = doc.page_images(idx, &mut budget).expect("page");
        for (k, im) in p.images.iter().enumerate() {
            println!(
                "p{} #{k} {}x{} {:?} cs={:?} codec={} cover={:.2} enc={}B data={} omitted={:?}",
                p.page_number,
                im.width,
                im.height,
                im.filters,
                im.color_space,
                im.codec.as_str(),
                im.coverage,
                im.encoded_len,
                im.data.as_ref().map_or(0, Vec::len),
                im.omitted.map(|o| o.as_str()),
            );
            if let (Some(dir), Some(data)) = (&out, &im.data) {
                let ext = match im.format {
                    Some("jpeg") => "jpg",
                    Some("png") => "png",
                    _ => "jp2",
                };
                std::fs::write(format!("{dir}/p{}-{k}.{ext}", p.page_number), data).expect("write");
            }
        }
    }
}
