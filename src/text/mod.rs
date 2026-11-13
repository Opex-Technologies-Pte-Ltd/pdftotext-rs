//! Assemble readable Unicode text for a page.
//!
//! Pipeline per page:
//!   decoded content stream
//!     → `content::interpret` drives a [`Walker`] (the interpreter's `Sink`)
//!       → shown strings: decoded via the in-scope font, positioned in page
//!         space (CTM + text matrix tracked by the interpreter), advanced by the
//!         font's glyph widths
//!       → `Do`: Form XObjects are interpreted inline with their own /Resources
//!         and `form /Matrix × CTM`, so form text lands where it is drawn
//!   → raw (stream order) or layout (position order) assembly.
//!
//! Hostile-input guards, all enforced here: Form XObject cycles (active-set),
//! nesting depth, total form invocations per page, a per-document decoded-bytes
//! budget (`Shared`), per-page run/char caps.

pub mod agl;
mod cid2uni;
mod cjk_cmap;
mod cmap;
mod encoding;
pub mod font;
mod reading;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use lopdf::{Dictionary, Document as LoDoc, Object, ObjectId, Stream};

use crate::content::{interpret, is_sane, mul, Advance, Matrix, Sink, TextRun, IDENTITY};
use crate::reader::{decode_stream, page_content_raw, MAX_STREAM_BYTES};
use crate::{Error, Result};

use self::font::ResolvedFont;

/// Max Form-XObject nesting depth.
const MAX_DEPTH: usize = 16;

/// Max Form XObject invocations while extracting one page. Bounds the
/// "billion laughs" shape (forms that `Do` each other many times) that depth and
/// cycle checks alone do not.
const MAX_FORMS_PER_PAGE: usize = 50_000;

/// Max text runs / output characters kept per page; the rest is dropped.
const MAX_RUNS_PER_PAGE: usize = 200_000;
const MAX_CHARS_PER_PAGE: usize = 8 << 20;

/// Total decoded content bytes interpreted across one document (pages + forms).
/// ~1 GiB is minutes of lexing; beyond it the input is treated as a bomb.
const DOC_DECODE_BUDGET: usize = 1 << 30;

/// Per-document state shared by every page extraction: the resolved-font cache
/// (a font shared by N pages is parsed once) and the decode budget.
pub struct Shared {
    fonts: Mutex<HashMap<ObjectId, Arc<ResolvedFont>>>,
    /// Font dictionaries written inline in a Resources dictionary, keyed by
    /// their address in the parsed document (stable while it lives). Without
    /// this a form invoked N times would re-parse its inline font's ToUnicode
    /// N times.
    direct_fonts: Mutex<HashMap<usize, Arc<ResolvedFont>>>,
    decode_left: AtomicUsize,
}

impl Shared {
    pub fn new() -> Self {
        Shared {
            fonts: Mutex::new(HashMap::new()),
            direct_fonts: Mutex::new(HashMap::new()),
            decode_left: AtomicUsize::new(DOC_DECODE_BUDGET),
        }
    }

    /// Spend `n` bytes of decode budget; `false` (budget untouched) if it would
    /// overdraw.
    fn charge(&self, n: usize) -> bool {
        let mut left = self.decode_left.load(AtomicOrdering::Relaxed);
        loop {
            let Some(rest) = left.checked_sub(n) else {
                return false;
            };
            match self.decode_left.compare_exchange_weak(
                left,
                rest,
                AtomicOrdering::Relaxed,
                AtomicOrdering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => left = actual,
            }
        }
    }

    fn font(&self, id: ObjectId, make: impl FnOnce() -> ResolvedFont) -> Arc<ResolvedFont> {
        if let Some(f) = self.fonts.lock().ok().and_then(|m| m.get(&id).cloned()) {
            return f;
        }
        let f = Arc::new(make());
        if let Ok(mut m) = self.fonts.lock() {
            m.insert(id, Arc::clone(&f));
        }
        f
    }

    fn direct_font(&self, obj: &Object, make: impl FnOnce() -> ResolvedFont) -> Arc<ResolvedFont> {
        let key = obj as *const Object as usize;
        if let Some(f) = self
            .direct_fonts
            .lock()
            .ok()
            .and_then(|m| m.get(&key).cloned())
        {
            return f;
        }
        let f = Arc::new(make());
        if let Ok(mut m) = self.direct_fonts.lock() {
            m.insert(key, Arc::clone(&f));
        }
        f
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

/// One decoded, positioned text run in page space.
struct Run {
    text: String,
    size: f32,
    x: f32,
    y: f32,
    /// Where the run ends (advance-width based); next-run gap detection.
    end_x: f32,
    end_y: f32,
    /// Baseline is closer to vertical than horizontal (rotated text). Successive
    /// strings of one rotated line are merged into a single run so the line-based
    /// layout does not shred them across rows.
    vertical: bool,
    /// Minimum gap after which a following run on the line gets a space.
    gap: f32,
    /// A space glyph was drawn right before this run (and dropped as text):
    /// a word break, whatever its measured width (justified text shrinks
    /// spaces with a negative `Tw`), unless the line is letter-spaced.
    after_blank: bool,
}

/// Resource lookup for one content-stream scope (a page or a Form XObject).
/// Entries are resolved lazily so a shared Resources dictionary with thousands of
/// fonts costs nothing until a font is actually shown.
struct Scope<'a> {
    /// Resources dictionaries, nearest first (page, then inherited ancestors).
    resources: Vec<&'a Dictionary>,
    fonts: HashMap<String, Option<Arc<ResolvedFont>>>,
}

impl<'a> Scope<'a> {
    fn new(resources: Vec<&'a Dictionary>) -> Self {
        Scope {
            resources,
            fonts: HashMap::new(),
        }
    }

    fn font(&mut self, doc: &'a LoDoc, shared: &Shared, name: &str) -> Option<Arc<ResolvedFont>> {
        if let Some(f) = self.fonts.get(name) {
            return f.clone();
        }
        let found = self.resources.iter().find_map(|res| {
            let fonts = res.get(b"Font").ok().and_then(|o| deref_dict(doc, o))?;
            let v = fonts.get(name.as_bytes()).ok()?;
            Some(match v {
                Object::Reference(r) => {
                    shared.font(*r, || font::resolve(doc, deref_object(doc, v)))
                }
                other => shared.direct_font(other, || font::resolve(doc, other)),
            })
        });
        self.fonts.insert(name.to_owned(), found.clone());
        found
    }

    fn xobject(&self, doc: &'a LoDoc, name: &str) -> Option<ObjectId> {
        self.resources.iter().find_map(|res| {
            let xo = res.get(b"XObject").ok().and_then(|o| deref_dict(doc, o))?;
            as_ref_id(xo.get(name.as_bytes()).ok()?)
        })
    }
}

struct Walker<'a> {
    doc: &'a LoDoc,
    shared: &'a Shared,
    scopes: Vec<Scope<'a>>,
    /// Object ids of the Forms currently being interpreted (cycle guard; its
    /// length is the nesting depth).
    active: Vec<ObjectId>,
    forms_left: usize,
    runs: Vec<Run>,
    chars: usize,
    /// The last piece shown was a space glyph (see `Run::after_blank`).
    after_blank: bool,
    /// Opaque fills still checked for hiding earlier text on this page.
    fills_left: usize,
    /// Run/char cap reached: stop interpreting, keep what we have.
    full: bool,
    error: Option<Error>,
}

impl Walker<'_> {
    /// Record one decoded piece of a show string starting at (`x`, `y`). Returns
    /// its full advance along the baseline (glyph widths plus `Tc`/`Tw`).
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        decoded: String,
        x: f32,
        y: f32,
        adv: Advance,
        run: &TextRun,
        vertical: bool,
        gap: f32,
    ) -> f32 {
        let s = run.scale;
        let len = adv.em * s.em + adv.glyphs as f32 * s.glyph + adv.spaces as f32 * s.space;
        // Blank pieces only move the pen; the gap they leave is what counts.
        if decoded.trim().is_empty() && !vertical {
            // A space drawn on top of the previous word's ink (seen inside
            // ligatures) is not a word break.
            let overlaps = self
                .runs
                .last()
                .is_some_and(|l| (l.y - y).abs() < 0.2 * run.size.max(1.0) && x < l.end_x - 0.1);
            self.after_blank = !overlaps;
            return len;
        }
        let after_blank = std::mem::take(&mut self.after_blank);
        // Gaps are measured from where the last glyph's ink ends: positive
        // character spacing after it is blank space, not text.
        let ink = if adv.glyphs > 0 {
            len - s.glyph.max(0.0)
        } else {
            len
        };
        let finite = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        let end_x = finite(x + ink * s.dx, x);
        let end_y = finite(y + ink * s.dy, y);
        self.chars += decoded.len();
        let tail = self
            .runs
            .last_mut()
            .filter(|l| vertical && l.vertical && (l.x - x).abs() <= (0.5 * run.size).max(1.0));
        match tail {
            Some(last) => {
                let along = (y - last.end_y) * s.dy.signum();
                if along > gap {
                    last.text.push(' ');
                }
                last.text.push_str(&decoded);
                last.end_x = end_x;
                last.end_y = end_y;
            }
            None => self.runs.push(Run {
                text: decoded,
                size: run.size,
                x,
                y,
                end_x,
                end_y,
                vertical,
                gap,
                after_blank,
            }),
        }
        if self.runs.len() >= MAX_RUNS_PER_PAGE || self.chars >= MAX_CHARS_PER_PAGE {
            self.full = true;
        }
        len
    }
}

impl<'a> Sink for Walker<'a> {
    fn text(&mut self, run: TextRun) -> Advance {
        let doc = self.doc;
        let shared = self.shared;
        let font = match (run.font.as_deref(), self.scopes.last_mut()) {
            (Some(name), Some(scope)) => scope.font(doc, shared, name),
            _ => None,
        };
        let decode = |bytes: &[u8]| -> (String, Advance) {
            match &font {
                Some(f) => (f.decode(bytes), f.advance(bytes)),
                None => {
                    let n = bytes.len() as u32;
                    (
                        ascii_pass(bytes),
                        Advance {
                            em: n as f32 * 0.5,
                            glyphs: n,
                            spaces: bytes.iter().filter(|&&b| b == 0x20).count() as u32,
                        },
                    )
                }
            }
        };
        let (decoded, adv) = decode(&run.bytes);
        if self.full || decoded.is_empty() {
            return adv;
        }
        let known = font.as_ref().is_some_and(|f| f.widths_known());
        // Without real widths the run ends are per-letter-shape estimates
        // (see `font::estimated_width`), so ask for a slightly larger gap.
        // 0.1 em: condensed layouts set word spaces as kerning of ~0.12-0.15 em,
        // while kerning between letters stays well under 0.05 em.
        let frac = if known { 0.1 } else { 0.2 };
        let gap = (frac * run.size).max(1.0);
        let vertical = run.scale.dy.abs() > run.scale.dx.abs();
        // Letter-spaced text (a large `Tc`, typically pulled back glyph by glyph
        // with `TJ` kerning) leaves word gaps *inside* one show string, where a
        // per-string run cannot see them. Measure such strings glyph by glyph.
        let code_len = font.as_ref().map_or(1, |f| f.code_len());
        let per_glyph = !vertical
            && run.scale.glyph > 0.5 * gap
            && run.bytes.len() > code_len
            && run.bytes.len() <= 4096;
        // Space glyphs inside a string are not trusted as word breaks: in
        // letter-spaced text a space can be the gap after a ligature, and a
        // stray space can sit inside a word. Split the string at them and let
        // the gap rule decide, as it does between strings; a real word space
        // still leaves a gap of a space's width.
        let split_spaces = !per_glyph
            && !vertical
            && code_len == 1
            && run.bytes.len() <= 4096
            && run.bytes.contains(&0x20)
            && decode(&[0x20]).0.trim().is_empty();
        if per_glyph || split_spaces {
            let pieces: Vec<&[u8]> = if per_glyph {
                font.as_ref().map_or_else(
                    || run.bytes.chunks(1).collect(),
                    |f| f.split_codes(&run.bytes),
                )
            } else {
                run.bytes
                    .chunk_by(|a, b| (*a == 0x20) == (*b == 0x20))
                    .collect()
            };
            let (mut x, mut y) = (run.x, run.y);
            for piece in pieces {
                let (d, a) = decode(piece);
                let len = self.push(d, x, y, a, &run, vertical, gap);
                x += len * run.scale.dx;
                y += len * run.scale.dy;
                if self.full {
                    break;
                }
            }
        } else {
            self.push(decoded, run.x, run.y, adv, &run, vertical, gap);
        }
        adv
    }

    fn xobject(&mut self, name: &str, ctm: &Matrix) {
        if self.full || self.error.is_some() || self.active.len() >= MAX_DEPTH {
            return;
        }
        let doc = self.doc;
        let Some(id) = self.scopes.last().and_then(|s| s.xobject(doc, name)) else {
            return;
        };
        // A form that (transitively) invokes itself would recurse forever.
        if self.active.contains(&id) {
            return;
        }
        let Some(form) = form_xobject(doc, id) else {
            return;
        };
        if self.forms_left == 0 {
            return;
        }
        self.forms_left -= 1;

        let content = decode_stream(form, MAX_STREAM_BYTES);
        if !self.shared.charge(content.len()) {
            self.error = Some(Error::Limit(
                "decoded content budget exhausted (decompression bomb or runaway XObject reuse)"
                    .into(),
            ));
            return;
        }

        // Form space → page space: `form /Matrix`, then the CTM at `Do`.
        let child_ctm = match form_matrix(doc, form) {
            Some(m) => {
                let c = mul(&m, ctm);
                if is_sane(&c) {
                    c
                } else {
                    *ctm
                }
            }
            None => *ctm,
        };
        let own_resources = form
            .dict
            .get(b"Resources")
            .ok()
            .and_then(|r| deref_dict(doc, r));
        if let Some(res) = own_resources {
            self.scopes.push(Scope::new(vec![res]));
        }
        self.active.push(id);
        interpret(&content, child_ctm, self);
        self.active.pop();
        if own_resources.is_some() {
            self.scopes.pop();
        }
    }

    fn halted(&self) -> bool {
        self.full || self.error.is_some()
    }

    fn opaque_fill(&mut self, rects: &[[f32; 4]]) {
        // Bounded: a page painting thousands of cells after its text would
        // otherwise cost fills × runs.
        if self.fills_left == 0 || self.runs.is_empty() {
            return;
        }
        self.fills_left -= 1;
        for &[x0, y0, x1, y1] in rects {
            // Only boxes big enough to cover a word; hairlines and dots are
            // decoration.
            if x1 - x0 < 4.0 || y1 - y0 < 4.0 {
                continue;
            }
            self.runs.retain(|r| {
                // The run's ink box: baseline to ~cap height, start to end.
                let (lx, hx) = (r.x.min(r.end_x), r.x.max(r.end_x));
                let (ly, hy) = (
                    r.y.min(r.end_y) - 0.2 * r.size,
                    r.y.max(r.end_y) + 0.7 * r.size,
                );
                let tol = 0.5;
                let covered = lx >= x0 - tol && hx <= x1 + tol && ly >= y0 - tol && hy <= y1 + tol;
                !covered
            });
        }
    }

    fn ext_gstate_transparent(&mut self, name: &str) -> Option<bool> {
        let doc = self.doc;
        let gs = self.scopes.last()?.resources.iter().find_map(|res| {
            let all = res
                .get(b"ExtGState")
                .ok()
                .and_then(|o| deref_dict(doc, o))?;
            all.get(name.as_bytes())
                .ok()
                .and_then(|o| deref_dict(doc, o))
        })?;
        let num = |k: &[u8]| match gs.get(k).ok().map(|o| deref_object(doc, o)) {
            Some(Object::Integer(i)) => Some(*i as f32),
            Some(Object::Real(r)) => Some(*r),
            _ => None,
        };
        let blend = match gs.get(b"BM").ok().map(|o| deref_object(doc, o)) {
            Some(Object::Name(n)) => {
                Some(n.as_slice() != b"Normal" && n.as_slice() != b"Compatible")
            }
            Some(Object::Array(_)) => Some(true),
            _ => None,
        };
        let smask = match gs.get(b"SMask").ok().map(|o| deref_object(doc, o)) {
            Some(Object::Name(n)) => Some(n.as_slice() != b"None"),
            Some(_) => Some(true),
            None => None,
        };
        let alpha = num(b"ca").map(|a| a < 0.99);
        if alpha.is_none() && blend.is_none() && smask.is_none() {
            return None;
        }
        Some(alpha.unwrap_or(false) || blend.unwrap_or(false) || smask.unwrap_or(false))
    }
}

/// How a page's text is arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Physical layout (`-layout`): lines top to bottom, each run near the
    /// column matching its position.
    Layout,
    /// Reading order (the default): lines top to bottom, columns of running
    /// text one after the other, single spaces.
    Reading,
    /// Content-stream order (`-raw`), spaced and broken by position.
    Raw,
}

impl Mode {
    /// `true` = [`Mode::Layout`], `false` = [`Mode::Reading`].
    pub fn from_layout(layout: bool) -> Self {
        if layout {
            Mode::Layout
        } else {
            Mode::Reading
        }
    }
}

/// Extract readable text for a page AND its section headers in one pass.
/// Headers are detected by larger-than-body font size (the rule
/// `pdf-extraction.ts` applies to unpdf's per-item heights). Returns
/// `(text, headers)`. This is what the wasm bridge surfaces to the Cloudflare
/// Worker so its `sectionHeaders` match what unpdf produced (the chunker titles
/// chunks off them).
///
/// `Error::Limit` when the document-wide decode budget is exhausted.
pub fn page_text_with_headers(
    doc: &LoDoc,
    shared: &Shared,
    page_id: ObjectId,
    mode: Mode,
) -> Result<(String, Vec<String>)> {
    let content = page_content_raw(doc, page_id);
    if !shared.charge(content.len()) {
        return Err(Error::Limit(
            "decoded content budget exhausted (decompression bomb?)".into(),
        ));
    }
    let mut walker = Walker {
        doc,
        shared,
        scopes: vec![Scope::new(page_resources(doc, page_id))],
        active: Vec::new(),
        forms_left: MAX_FORMS_PER_PAGE,
        runs: Vec::new(),
        chars: 0,
        after_blank: false,
        fills_left: 2_000,
        full: false,
        error: None,
    };
    interpret(&content, IDENTITY, &mut walker);
    if let Some(e) = walker.error {
        return Err(e);
    }
    clip_to_page(&mut walker.runs, page_box(doc, page_id));
    walker.runs = drop_overdraw(std::mem::take(&mut walker.runs));
    let headers = compute_section_headers(&walker.runs);
    let text = match mode {
        Mode::Layout => layout_runs(&walker.runs),
        Mode::Reading => reading::reading_runs(&walker.runs),
        Mode::Raw => raw(&walker.runs),
    };
    Ok((text, headers))
}

/// Extract readable text for a page (no header detection).
pub fn page_text(doc: &LoDoc, shared: &Shared, page_id: ObjectId, mode: Mode) -> Result<String> {
    Ok(page_text_with_headers(doc, shared, page_id, mode)?.0)
}

/// Detect section headers from per-run font sizes: a run ≥130% of the body
/// median (and >2 non-space chars) is a header candidate. Mirrors the heuristic
/// in OpexMX's previous pdf.js-based extractor so wasm-extracted headers match
/// what unpdf produced.
fn compute_section_headers(runs: &[Run]) -> Vec<String> {
    // Body baseline = median of positive font sizes (0.0 = unknown size → skip).
    let mut sizes: Vec<f32> = runs.iter().map(|r| r.size).filter(|s| *s > 0.0).collect();
    if sizes.is_empty() {
        return Vec::new();
    }
    sizes.sort_by(|a, b| a.total_cmp(b));
    let median = sizes[sizes.len() / 2];
    let threshold = median * 1.3;

    let mut headers: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev: Option<&Run> = None;
    for r in runs {
        let big = r.size >= threshold;
        // A heading broken into several strings by kerning (`TJ`) or per-word
        // positioning continues on the same baseline: join it, with a space
        // only where there is a gap.
        let continues = big
            && !current.is_empty()
            && prev.is_some_and(|p| {
                (r.y - p.y).abs() <= 0.5 * r.size.min(p.size) && r.x >= p.end_x - r.gap
            });
        if continues {
            let p = prev.expect("checked above");
            if r.x > p.end_x + r.gap && !current.ends_with(' ') {
                current.push(' ');
            }
            current.push_str(&r.text);
        } else if big && r.text.trim().len() > 2 {
            if !current.is_empty() {
                headers.push(current.trim().to_string());
            }
            current = r.text.clone();
        } else if !current.is_empty() && !r.text.trim().is_empty() {
            headers.push(current.trim().to_string());
            current.clear();
        }
        prev = Some(r);
    }
    if !current.is_empty() {
        headers.push(current.trim().to_string());
    }
    // Dedupe + drop empties (mirrors `dedupe` + filter in pdf-extraction.ts).
    let mut seen = std::collections::HashSet::new();
    headers
        .into_iter()
        .filter(|h| !h.is_empty())
        .filter(|h| seen.insert(h.clone()))
        .collect()
}

/// Best-effort ASCII passthrough for a run whose font is unknown — emits only
/// printable bytes so we never produce glyph-code garbage.
fn ascii_pass(bytes: &[u8]) -> String {
    bytes
        .iter()
        .filter(|&&b| (0x20..0x7f).contains(&b))
        .map(|&b| b as char)
        .collect()
}

/// Drop text drawn more than once at (almost) the same spot: fake bold (the
/// same string overprinted with a sub-point offset), drop shadows, and pages
/// that paint their text layer twice. Without this the layout sort interleaves
/// the copies letter by letter ("informaormationtion").
fn drop_overdraw(runs: Vec<Run>) -> Vec<Run> {
    // (text, cell) -> indices into `kept`
    let mut seen: HashMap<(String, i32, i32), Vec<usize>> = HashMap::new();
    let mut kept: Vec<Run> = Vec::with_capacity(runs.len());
    for r in runs {
        // Never merge distinct glyphs of tiny text: the tolerance stays well
        // under one glyph width.
        let tol = (0.2 * r.size).clamp(0.1, 1.5);
        let (cx, cy) = ((r.x / tol).floor() as i32, (r.y / tol).floor() as i32);
        let mut dup = None;
        'search: for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(ids) = seen.get(&(r.text.clone(), cx + dx, cy + dy)) {
                    if let Some(&k) = ids.iter().find(|&&k| {
                        (kept[k].x - r.x).abs() <= tol && (kept[k].y - r.y).abs() <= tol
                    }) {
                        dup = Some(k);
                        break 'search;
                    }
                }
            }
        }
        if let Some(k) = dup {
            // The copy left ink up to its own end (a doubled hyphen drawn as
            // one long dash): the kept run covers that too, so no gap opens.
            let kr = &mut kept[k];
            if r.end_x > kr.end_x {
                kr.end_x = r.end_x;
                kr.end_y = r.end_y;
            }
            continue;
        }
        seen.entry((r.text.clone(), cx, cy))
            .or_default()
            .push(kept.len());
        kept.push(r);
    }
    kept
}

/// Does `r` start inside the ink of the run before it on the same line? A
/// whitespace-only run that does is a stray space glyph drawn over a word (seen
/// inside ligatures in InDesign exports), not a word break.
fn overlaps_prev(r: &Run, prev_end_x: f32) -> bool {
    r.text.trim().is_empty() && r.x < prev_end_x - 0.1
}

/// Raw (bare, no `-layout`) emission: runs in content-stream order, with a
/// space only where there is a real gap on the baseline (or the stream jumps
/// backwards), and a newline where the baseline changes. Many manuals draw every
/// glyph as its own positioned string, so joining runs blindly with spaces
/// would print "T h e s t r u c t u r e".
fn raw(runs: &[Run]) -> String {
    let seq: Vec<usize> = (0..runs.len())
        .filter(|&i| !runs[i].text.is_empty())
        .collect();
    let thresholds = word_gap_thresholds(runs, &seq);
    let mut out = String::new();
    let mut prev: Option<&Run> = None;
    for (k, &i) in seq.iter().enumerate() {
        let r = &runs[i];
        if let Some(p) = prev {
            let line_tol = (0.5 * r.size.min(p.size)).max(1.0);
            let same_line = !r.vertical && !p.vertical && (r.y - p.y).abs() <= line_tol;
            if !same_line {
                out.push('\n');
            } else if overlaps_prev(r, p.end_x) {
                continue;
            } else if r.x > p.end_x + thresholds[k] || r.x < p.x {
                let last_ws = out.chars().next_back().is_some_and(|c| c.is_whitespace());
                let next_ws = r.text.starts_with(char::is_whitespace);
                if !last_ws && !next_ws {
                    out.push(' ');
                }
            }
        }
        out.push_str(&r.text);
        prev = Some(r);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    if out.chars().any(is_rtl) {
        out = out
            .lines()
            .map(visual_to_logical)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
    }
    out
}

/// Reconstruct lines from positioned runs: group by y (top of page first — PDF y
/// grows up), sort each line by x, insert a space where the advance-width-based
/// gap to the previous run exceeds a fraction of the font size, and indent by
/// the line's x. Heuristic — not full -layout column reconstruction, but yields
/// readable, structured, non-concatenated text.
fn layout_runs(runs: &[Run]) -> String {
    if runs.is_empty() {
        return String::new();
    }
    let lines = group_lines(runs);
    // Physical layout, as `pdftotext -layout` promises: each run starts near
    // the text column matching its x, so table cells and side-by-side columns
    // stay visibly apart instead of running together. One column = the page's
    // typical character width.
    let cw = char_width(runs);
    let left = runs
        .iter()
        .map(|r| r.x)
        .filter(|x| x.is_finite())
        .fold(f32::INFINITY, f32::min);
    let left = if left.is_finite() { left } else { 0.0 };
    let column = |x: f32| -> usize {
        let c = ((x - left) / cw).round();
        if c.is_finite() {
            c.clamp(0.0, 400.0) as usize
        } else {
            0
        }
    };
    let mut out = String::new();
    for line in lines {
        let thresholds = word_gap_thresholds(runs, &line);
        let mut prev_end_x: Option<f32> = None;
        let mut col = 0usize; // characters written on this line
        let line_start = out.len();
        for (pos, &i) in line.iter().enumerate() {
            let r = &runs[i];
            let pad = if pos == 0 {
                column(r.x)
            } else if let Some(pe) = prev_end_x {
                if overlaps_prev(r, pe) {
                    continue;
                }
                if r.x <= pe + thresholds[pos] {
                    0
                } else if r.x - pe < 2.0 * cw {
                    1 // an ordinary word space
                } else {
                    column(r.x).saturating_sub(col).max(1)
                }
            } else {
                0
            };
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(&r.text);
            col += pad + r.text.chars().count();
            prev_end_x = Some(r.end_x);
        }
        let logical = visual_to_logical(&out[line_start..]);
        out.truncate(line_start);
        out.push_str(&logical);
        out.push('\n');
    }
    out
}

/// Group runs into visual lines, top to bottom, each sorted left to right
/// (stable: runs at the same x keep stream order).
fn group_lines(runs: &[Run]) -> Vec<Vec<usize>> {
    let mut idx: Vec<usize> = (0..runs.len()).collect();
    // Sort by y descending (top first), then x ascending. Positions are finite
    // (the interpreter rejects non-finite matrices), and `total_cmp` is a total
    // order regardless, so the sort can never trip on NaN.
    idx.sort_by(|&a, &b| {
        runs[b]
            .y
            .total_cmp(&runs[a].y)
            .then(runs[a].x.total_cmp(&runs[b].x))
    });

    // Same line when the baselines are within half the smaller font size:
    // keeps tight small-type lines apart, tolerates baseline jitter in big
    // type, and does not pull the body line under a large heading up into it.
    // A run clearly smaller than the line (sub/superscript, an index like
    // the "2" in mm2) may sit further off the baseline: up to 0.6 of the
    // larger size. The line's baseline is that of its largest text.
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut cur: Option<(f32, f32)> = None; // (line baseline, line font size)
    for &i in &idx {
        let (y, size) = (runs[i].y, runs[i].size.max(1.0));
        let same = cur.is_some_and(|(cy, cs)| {
            let (small, big) = (cs.min(size), cs.max(size));
            let tol = if small < 0.85 * big {
                0.6 * big
            } else {
                0.5 * small
            };
            (y - cy).abs() <= tol
        });
        match (same, lines.last_mut()) {
            (true, Some(line)) => {
                line.push(i);
                if cur.is_some_and(|(_, cs)| size > cs) {
                    cur = Some((y, size));
                }
            }
            _ => {
                lines.push(vec![i]);
                cur = Some((y, size));
            }
        }
    }

    let mut lines: Vec<Vec<usize>> = lines
        .into_iter()
        .flat_map(|line| split_overlapping(runs, line))
        .collect();
    for line in &mut lines {
        line.sort_by(|&a, &b| runs[a].x.total_cmp(&runs[b].x));
    }
    lines
}

/// Right-to-left letters (Hebrew, Arabic and their presentation forms).
fn is_rtl(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF)
}

fn is_ltr_strong(c: char) -> bool {
    c.is_alphanumeric() && !is_rtl(c)
}

/// PDFs place right-to-left text in visual order (glyphs left to right as
/// they appear), so read off the page an Arabic line comes out reversed.
/// Reorder a line to logical order with a minimal bidi pass: a line whose
/// letters are mostly right-to-left is read right to left, with embedded
/// left-to-right stretches (numbers, Latin model codes) kept in their own
/// order; in a left-to-right line only the right-to-left stretches are
/// reversed. Lines without right-to-left letters are returned unchanged.
fn visual_to_logical(line: &str) -> String {
    if !line.chars().any(is_rtl) {
        return line.to_owned();
    }
    let indent_len = line.len() - line.trim_start().len();
    let (indent, body) = line.split_at(indent_len);
    let body = body.trim_end();
    let rtl = body.chars().filter(|&c| is_rtl(c)).count();
    let ltr = body.chars().filter(|&c| is_ltr_strong(c)).count();
    // Segments of one direction; neutrals (spaces, punctuation) join the
    // segment they sit in, or the right-to-left one between two of them.
    let mut segs: Vec<(bool, String)> = Vec::new();
    for c in body.chars() {
        let dir = if is_rtl(c) {
            Some(true)
        } else if is_ltr_strong(c) {
            Some(false)
        } else {
            None
        };
        match (dir, segs.last_mut()) {
            (Some(d), Some((sd, s))) if *sd == d => s.push(c),
            (None, Some((_, s))) => s.push(c),
            (Some(d), _) => segs.push((d, c.to_string())),
            (None, None) => segs.push((rtl >= ltr, c.to_string())),
        }
    }
    // Spaces and punctuation at the edges of an embedded stretch belong to the
    // line's own direction (they sit between the two directions).
    let para = rtl >= ltr;
    let is_neutral = |c: char| !is_rtl(c) && !is_ltr_strong(c);
    let mut fixed: Vec<(bool, String)> = Vec::new();
    for (d, s) in segs {
        if d == para {
            fixed.push((d, s));
            continue;
        }
        let core = s.trim_matches(is_neutral);
        let lead = &s[..s.len() - s.trim_start_matches(is_neutral).len()];
        let trail = &s[s.trim_end_matches(is_neutral).len()..];
        for (dir, part) in [(para, lead), (d, core), (para, trail)] {
            if part.is_empty() {
                continue;
            }
            match fixed.last_mut() {
                Some((ld, ls)) if *ld == dir => ls.push_str(part),
                _ => fixed.push((dir, part.to_owned())),
            }
        }
    }
    let segs = fixed;
    // Reversed right-to-left text also swaps paired brackets, which are drawn
    // mirrored (Unicode bidi mirroring).
    let mirror = |c: char| match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        c => c,
    };
    let flip = |s: &str| s.chars().rev().map(mirror).collect::<String>();
    let mut out = String::from(indent);
    if rtl >= ltr {
        for (d, s) in segs.iter().rev() {
            out.push_str(&if *d { flip(s) } else { s.clone() });
        }
    } else {
        for (d, s) in &segs {
            out.push_str(&if *d { flip(s) } else { s.clone() });
        }
    }
    out
}

/// A grouped line whose runs sit on clearly different baselines AND overlap
/// horizontally is really two lines drawn close together (a label over a rule
/// of underscores, stacked tabs in a catalogue margin); sorting it by x would
/// interleave them letter by letter. Split it by baseline, top first. Runs on
/// different baselines that do not overlap (sub/superscripts, inline indices)
/// stay one line.
fn split_overlapping(runs: &[Run], mut line: Vec<usize>) -> Vec<Vec<usize>> {
    if line.len() < 2 || line.len() > 2000 {
        return vec![line];
    }
    line.sort_by(|&a, &b| runs[b].y.total_cmp(&runs[a].y));
    let mut bands: Vec<Vec<usize>> = Vec::new();
    for &i in &line {
        let r = &runs[i];
        match bands.last_mut() {
            Some(band)
                if {
                    let first = &runs[band[0]];
                    (first.y - r.y).abs() <= 0.2 * first.size.min(r.size).max(1.0)
                } =>
            {
                band.push(i)
            }
            _ => bands.push(vec![i]),
        }
    }
    if bands.len() < 2 {
        return vec![line];
    }
    let span = |i: usize| {
        let r = &runs[i];
        (r.x.min(r.end_x), r.x.max(r.end_x), r.size.max(1.0))
    };
    let solid = |i: usize| !runs[i].text.trim().is_empty();
    let overlap = bands.iter().enumerate().any(|(k, a)| {
        bands[k + 1..].iter().any(|b| {
            a.iter().filter(|&&i| solid(i)).any(|&i| {
                let (al, ah, asz) = span(i);
                b.iter().filter(|&&j| solid(j)).any(|&j| {
                    let (bl, bh, bsz) = span(j);
                    al.max(bl) < ah.min(bh) - 0.3 * asz.min(bsz)
                })
            })
        })
    });
    if overlap {
        bands
    } else {
        vec![line]
    }
}

/// The gap before each run of `seq` (in order) that counts as a word break.
/// Normally the run's own threshold; but in letter-spaced text (captions and
/// headings set with wide tracking, glyph by glyph) every glyph is followed by
/// a similar sizeable gap, and only a clearly larger one is a word break.
/// Detected locally: among the letter-to-letter gaps of nearby one- or
/// two-glyph runs on the same baseline (at least 3 within 6 runs either side,
/// gaps after a space glyph excluded), if even the lower-quartile gap exceeds
/// the normal threshold, a break needs 1.6 times that gap.
fn word_gap_thresholds(runs: &[Run], seq: &[usize]) -> Vec<f32> {
    let n = seq.len();
    // gap[k] = gap before seq[k], when it is a letter-to-letter gap.
    let gap: Vec<Option<f32>> = (0..n)
        .map(|k| {
            if k == 0 {
                return None;
            }
            let (p, r) = (&runs[seq[k - 1]], &runs[seq[k]]);
            let short = |t: &str| {
                let c = t.trim().chars().count();
                (1..=2).contains(&c)
            };
            let same_line = (r.y - p.y).abs() <= 0.2 * r.size.max(1.0);
            let g = r.x - p.end_x;
            // Only gaps between letters say anything about letter spacing; the
            // gap after a space glyph is a word space.
            (same_line
                && !r.after_blank
                && short(&p.text)
                && short(&r.text)
                && g.is_finite()
                && g > -0.5 * r.size
                && g < 2.0 * r.size)
                .then_some(g)
        })
        .collect();
    // Stretches of one text block: a new one starts at a change of baseline
    // or a column-sized jump, so the neighbourhood never mixes columns.
    let mut block = vec![0usize; n];
    for k in 1..n {
        let (p, r) = (&runs[seq[k - 1]], &runs[seq[k]]);
        let jump = (r.y - p.y).abs() > 0.2 * r.size.max(1.0)
            || r.x - p.end_x > 3.0 * r.size.max(1.0)
            || r.x < p.x;
        block[k] = block[k - 1] + usize::from(jump);
    }
    (0..n)
        .map(|k| {
            let base = runs[seq[k]].gap;
            let mut lo = k.saturating_sub(6);
            let mut hi = (k + 6).min(n.saturating_sub(1));
            while block[lo] != block[k] {
                lo += 1;
            }
            while block[hi] != block[k] {
                hi -= 1;
            }
            let mut near: Vec<f32> = (lo..=hi)
                .filter_map(|j| gap.get(j).copied().flatten())
                .collect();
            if near.len() < 3 {
                return if runs[seq[k]].after_blank {
                    f32::NEG_INFINITY
                } else {
                    base
                };
            }
            near.sort_by(|a, b| a.total_cmp(b));
            // Letter-spaced text has every letter gap wide; ordinary text set
            // glyph by glyph has most of them near zero, with only its word
            // gaps wide. The lower quartile tells the two apart.
            let low = near[near.len() / 4];
            if low > base {
                base.max(1.6 * low)
            } else if runs[seq[k]].after_blank {
                f32::NEG_INFINITY
            } else {
                base
            }
        })
        .collect()
}

/// Typical character width on the page (median over runs of a few glyphs),
/// the unit of the physical layout grid. Clamped to sane values.
fn char_width(runs: &[Run]) -> f32 {
    let mut w: Vec<f32> = runs
        .iter()
        .filter(|r| !r.vertical)
        .filter_map(|r| {
            let n = r.text.chars().count();
            let span = r.end_x - r.x;
            (n >= 2 && span > 0.0 && span.is_finite()).then(|| span / n as f32)
        })
        .collect();
    if w.is_empty() {
        return 5.0;
    }
    w.sort_by(|a, b| a.total_cmp(b));
    w[w.len() / 2].clamp(2.0, 20.0)
}

/// The page's Resources dictionaries, nearest first: its own, then those
/// inherited from ancestor `Pages` nodes (direct or indirect). Walks `/Parent`
/// iteratively with a visited set, so a cyclic or absurdly deep tree cannot loop.
fn page_resources(doc: &LoDoc, page_id: ObjectId) -> Vec<&Dictionary> {
    const MAX_ANCESTORS: usize = 64;
    let mut out = Vec::new();
    let mut seen = vec![page_id];
    let mut node = doc.get_dictionary(page_id).ok();
    while let Some(d) = node {
        if let Some(res) = d.get(b"Resources").ok().and_then(|r| deref_dict(doc, r)) {
            out.push(res);
        }
        node = match d.get(b"Parent") {
            Ok(Object::Reference(p)) if !seen.contains(p) && seen.len() < MAX_ANCESTORS => {
                seen.push(*p);
                doc.get_dictionary(*p).ok()
            }
            _ => None,
        };
    }
    out
}

/// The visible area of a page: `/CropBox`, else `/MediaBox`, inherited from
/// ancestor `Pages` nodes, normalised to `[x0, y0, x1, y1]`.
fn page_box(doc: &LoDoc, page_id: ObjectId) -> Option<[f32; 4]> {
    const MAX_ANCESTORS: usize = 64;
    for key in [&b"CropBox"[..], &b"MediaBox"[..]] {
        let mut seen = vec![page_id];
        let mut node = doc.get_dictionary(page_id).ok();
        while let Some(d) = node {
            if let Some(arr) = d
                .get(key)
                .ok()
                .map(|o| deref_object(doc, o))
                .and_then(|o| o.as_array().ok())
            {
                let v: Vec<f32> = arr
                    .iter()
                    .filter_map(|o| match deref_object(doc, o) {
                        Object::Integer(i) => Some(*i as f32),
                        Object::Real(r) => Some(*r),
                        _ => None,
                    })
                    .filter(|v| v.is_finite())
                    .collect();
                if let [a, b, c, e] = v[..] {
                    return Some([a.min(c), b.min(e), a.max(c), b.max(e)]);
                }
            }
            node = match d.get(b"Parent") {
                Ok(Object::Reference(p)) if !seen.contains(p) && seen.len() < MAX_ANCESTORS => {
                    seen.push(*p);
                    doc.get_dictionary(*p).ok()
                }
                _ => None,
            };
        }
    }
    None
}

/// Drop text that lies entirely outside the visible page: printer's slugs,
/// crop-mark labels and job tickets in the pasteboard of DTP exports (an
/// InDesign file name and print date on every page of a manual). A run that
/// touches the page, even partly, stays. A degenerate box is ignored. When no
/// run at all is visible (a blank page carrying only its slug, or coordinates
/// that do not match the box), only text in the pasteboard margin around the
/// page is dropped; text far outside it means the box is not to be trusted.
fn clip_to_page(runs: &mut Vec<Run>, page: Option<[f32; 4]>) {
    let Some([x0, y0, x1, y1]) = page else {
        return;
    };
    if x1 - x0 < 10.0 || y1 - y0 < 10.0 || runs.is_empty() {
        return;
    }
    let within = |r: &Run, margin: f32| {
        let (lx, hx) = (r.x.min(r.end_x), r.x.max(r.end_x));
        let (ly, hy) = (r.y.min(r.end_y), r.y.max(r.end_y));
        hx >= x0 - margin && lx <= x1 + margin && hy >= y0 - margin && ly <= y1 + margin
    };
    // A glyph whose baseline sits just outside the box still shows its
    // ascenders inside it; a quarter of the size covers that.
    let visible = |r: &Run| within(r, 0.25 * r.size.max(1.0));
    if runs.iter().any(visible) {
        runs.retain(visible);
    } else {
        const PASTEBOARD: f32 = 72.0; // one inch
        runs.retain(|r| !within(r, PASTEBOARD));
    }
}

fn deref_dict<'a>(doc: &'a LoDoc, obj: &'a Object) -> Option<&'a Dictionary> {
    deref_object(doc, obj).as_dict().ok()
}

fn deref_object<'a>(doc: &'a LoDoc, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(r) => doc.get_object(*r).unwrap_or(obj),
        _ => obj,
    }
}

fn as_ref_id(obj: &Object) -> Option<ObjectId> {
    match obj {
        Object::Reference(r) => Some(*r),
        _ => None,
    }
}

fn form_xobject(doc: &LoDoc, obj_id: ObjectId) -> Option<&Stream> {
    match doc.get_object(obj_id).ok()? {
        Object::Stream(s) if matches!(s.dict.get(b"Subtype").ok(), Some(Object::Name(n)) if n == b"Form") => {
            Some(s)
        }
        _ => None,
    }
}

/// The form's `/Matrix` (default identity), if it is a well-formed 6-number array.
fn form_matrix(doc: &LoDoc, form: &Stream) -> Option<Matrix> {
    let arr = deref_object(doc, form.dict.get(b"Matrix").ok()?)
        .as_array()
        .ok()?;
    if arr.len() != 6 {
        return None;
    }
    let mut m = [0f32; 6];
    for (slot, o) in m.iter_mut().zip(arr) {
        *slot = match deref_object(doc, o) {
            Object::Integer(i) => *i as f32,
            Object::Real(r) => *r,
            _ => return None,
        };
    }
    is_sane(&m).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, x: f32, y: f32, end_x: f32) -> Run {
        Run {
            text: text.into(),
            size: 10.0,
            x,
            y,
            end_x,
            end_y: y,
            vertical: false,
            gap: 1.5,
            after_blank: false,
        }
    }

    #[test]
    fn layout_inserts_space_only_for_real_gaps() {
        // "He" + "llo" abut (no space); "world" starts 5pt after "llo" ends.
        let runs = [
            run("He", 0.0, 100.0, 10.0),
            run("llo", 10.0, 100.0, 25.0),
            run("world", 30.0, 100.0, 60.0),
        ];
        assert_eq!(layout_runs(&runs).trim(), "Hello world");
    }

    #[test]
    fn rtl_lines_are_reordered_logically() {
        // Visual order as drawn: "ملاس" is سلام reversed; digits stay LTR.
        let visual: String = "سلام".chars().rev().collect();
        assert_eq!(visual_to_logical(&visual), "سلام");
        assert_eq!(
            visual_to_logical(&format!("  CR 15 {visual}")),
            "  سلام CR 15"
        );
        // Brackets are extracted as drawn, "(" leftmost; logically the
        // group opens first.
        assert_eq!(visual_to_logical(&format!("({visual})")), "(سلام)");
        assert_eq!(visual_to_logical("plain text"), "plain text");
    }

    #[test]
    fn layout_orders_lines_top_down() {
        let runs = [run("low", 0.0, 10.0, 5.0), run("high", 0.0, 90.0, 5.0)];
        assert_eq!(layout_runs(&runs), "high\nlow\n");
    }

    #[test]
    fn layout_survives_non_finite_positions() {
        // Defense in depth: even if a NaN reached the sort it must not panic.
        let runs = [
            run("a", f32::NAN, f32::NAN, 0.0),
            run("b", 1.0, 2.0, 3.0),
            run("c", f32::INFINITY, f32::NEG_INFINITY, 0.0),
        ];
        let _ = layout_runs(&runs);
    }

    #[test]
    fn section_headers_follow_font_size() {
        let mut big = run("Chapter One", 0.0, 100.0, 50.0);
        big.size = 20.0;
        let body = run("body text here", 0.0, 80.0, 50.0);
        let more = run("more body text", 0.0, 60.0, 50.0);
        let h = compute_section_headers(&[big, body, more]);
        assert_eq!(h, vec!["Chapter One".to_string()]);
    }

    #[test]
    fn budget_overdraw_is_refused_without_spending() {
        let s = Shared::new();
        assert!(s.charge(DOC_DECODE_BUDGET - 10));
        assert!(!s.charge(11));
        assert!(s.charge(10));
        assert!(!s.charge(1));
    }
}
