use pdftotext::Document;
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let doc = Document::parse(&std::fs::read(&p).unwrap()).unwrap();
    for i in 0..doc.page_count().min(5) {
        let t = doc.extract_text(i, true).unwrap_or_default();
        let n = t.chars().count();
        let g = t
            .chars()
            .filter(|c| {
                let u = *c as u32;
                u == 0xFFFD
                    || (0xE000..=0xF8FF).contains(&u)
                    || (c.is_control() && *c != '\n' && *c != '\t' && *c != '\r')
            })
            .count();
        let pct = if n > 0 {
            g as f64 * 100.0 / n as f64
        } else {
            0.0
        };
        println!("page {i}: {n} chars, {:.1}% garbage", pct);
        if pct > 5.0 {
            println!("  sample: {:?}", &t[..t.len().min(120)]);
        }
    }
}
