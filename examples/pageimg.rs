//! Dev tool: write the images `page_images` returns for one page.
//! Usage: `cargo run --release --example pageimg -- file.pdf <page> <out-prefix>`
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let doc = pdftotext::Document::parse_keeping_images(&std::fs::read(&a[1]).unwrap()).unwrap();
    let page: usize = a[2].parse().unwrap();
    let mut budget = 64usize << 20;
    let pi = doc.page_images(page - 1, &mut budget).unwrap();
    for (k, im) in pi.images.iter().enumerate() {
        println!(
            "{k}: {}x{} codec={} format={:?} omitted={:?} coverage={:.2} bytes={}",
            im.width,
            im.height,
            im.codec.as_str(),
            im.format,
            im.omitted.map(|o| o.as_str()),
            im.coverage,
            im.data.as_ref().map_or(0, |d| d.len())
        );
        if let (Some(d), Some(f)) = (&im.data, im.format) {
            std::fs::write(format!("{}-{k}.{f}", a[3]), d).unwrap();
        }
    }
}
