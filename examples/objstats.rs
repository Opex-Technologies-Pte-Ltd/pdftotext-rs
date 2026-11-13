//! Dev tool: what a fully parsed document holds, by object kind (nodes =
//! numbers, names, array and dictionary members; lopdf stores each as a
//! 120-byte value). Loads unfiltered, to show what the load filter can drop.
//!
//! Usage: `cargo run --release --example objstats -- file.pdf`
use lopdf::{Document, Object};
use std::collections::BTreeMap;
fn walk(o: &Object, n: &mut usize, bytes: &mut usize) {
    *n += 1;
    match o {
        Object::Array(a) => a.iter().for_each(|x| walk(x, n, bytes)),
        Object::Dictionary(d) => d.iter().for_each(|(_, x)| walk(x, n, bytes)),
        Object::Stream(s) => {
            *bytes += s.content.len();
            s.dict.iter().for_each(|(_, x)| walk(x, n, bytes))
        }
        Object::String(s, _) | Object::Name(s) => *bytes += s.len(),
        _ => {}
    }
}
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let doc = Document::load_mem(&std::fs::read(&p).unwrap()).unwrap();
    let mut kinds: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for o in doc.objects.values() {
        let d = match o {
            Object::Dictionary(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        };
        let key = match d {
            Some(d) => {
                let n = |k: &[u8]| {
                    d.get(k)
                        .ok()
                        .and_then(|o| o.as_name().ok())
                        .map(|n| String::from_utf8_lossy(n).into_owned())
                        .unwrap_or_default()
                };
                let mut k = format!(
                    "{}/{}{}",
                    n(b"Type"),
                    n(b"Subtype"),
                    if matches!(o, Object::Stream(_)) {
                        " [stream]"
                    } else {
                        ""
                    }
                );
                if k.starts_with('/') && n(b"Subtype").is_empty() {
                    k = format!(
                        "keys:{} {}",
                        d.iter()
                            .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(","),
                        k
                    );
                }
                k
            }
            None => format!("{:?}", std::mem::discriminant(o)),
        };
        let (mut n, mut b) = (0, 0);
        walk(o, &mut n, &mut b);
        let e = kinds.entry(key).or_default();
        e.0 += 1;
        e.1 += n;
        e.2 += b;
    }
    let mut v: Vec<_> = kinds.into_iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1 .1 * 120 + x.1 .2));
    for (k, (c, n, b)) in v.into_iter().take(12) {
        eprintln!(
            "  {k:50} objs={c:6} nodes={n:8} bytes={:7.1}MB",
            b as f64 / 1e6
        );
    }
}
