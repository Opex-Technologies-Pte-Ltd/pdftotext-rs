//! Content-stream interpreter.
//!
//! Tokenizes a decoded content stream and drives the graphics/text state
//! operators that matter for text extraction: `q`/`Q`/`cm` (CTM), the text
//! object and text-state operators (`BT Tf Tc Tw Tz TL Ts Td TD Tm T*`), and the
//! showing operators (`Tj TJ ' "`). Each shown string is reported to a [`Sink`]
//! with its start position in the *invoking space* (the CTM passed to
//! [`interpret`] is the space the coordinates are expressed in), and the sink
//! answers with the string's advance so the text matrix moves exactly as the
//! spec describes (ISO 32000-2 §9.4.4). `Do` is reported with the CTM in effect
//! so the caller can recurse into Form XObjects with the right transform.
//!
//! Everything here is total over arbitrary bytes: no panics, no recursion, all
//! arithmetic is guarded against NaN/inf, operand and graphics-state stacks are
//! bounded.

use std::rc::Rc;

/// 2D affine matrix `[a b c d e f]` in PDF order (row vector convention:
/// `[x y 1] × M`).
pub type Matrix = [f32; 6];

pub const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a × b` — apply `a` first, then `b` (PDF convention).
pub fn mul(a: &Matrix, b: &Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

/// Reject matrices that went non-finite or absurdly large (hostile operands).
pub fn is_sane(m: &Matrix) -> bool {
    m.iter().all(|v| v.is_finite() && v.abs() < 1.0e9)
}

/// Device-space distance moved per unit of each advance component, so a sink can
/// compute a run's end position without knowing the text state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scale {
    /// Per em of glyph width (i.e. per `Σ width/1000`), includes font size,
    /// horizontal scaling and the text/CTM x-scale.
    pub em: f32,
    /// Per glyph code (character spacing `Tc`).
    pub glyph: f32,
    /// Per single-byte space code (word spacing `Tw`).
    pub space: f32,
    /// Unit baseline direction in the invoking space (`(1, 0)` for upright text,
    /// `(0, 1)` for text rotated 90° counter-clockwise).
    pub dx: f32,
    pub dy: f32,
}

/// One `Tj`/`TJ`/`'`/`"` emission: the raw show bytes, the font name active
/// at the time (the operand of the most recent `Tf`, e.g. `"F1"`; `None` if no
/// `Tf` has been seen yet), the run's start position (`x`, `y`) in the
/// invoking space, the effective font `size` (Tf size × text-matrix × CTM
/// y-scale) and the advance [`Scale`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextRun {
    pub font: Option<String>,
    pub bytes: Vec<u8>,
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub scale: Scale,
}

/// How far a shown string advances the text position, before spacing: glyph
/// widths in em (`Σ width/1000`), the number of glyph codes, and how many of them
/// are the single-byte space code (32) that word spacing applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Advance {
    pub em: f32,
    pub glyphs: u32,
    pub spaces: u32,
}

/// Receiver for interpreter events.
pub trait Sink {
    /// A string was shown. Returns its advance.
    fn text(&mut self, run: TextRun) -> Advance;
    /// `Do /name` with the CTM in effect. Implementations recurse by calling
    /// [`interpret`] themselves (and own the depth/cycle/work budgets).
    fn xobject(&mut self, _name: &str, _ctm: &Matrix) {}
    /// Polled after every operator; `true` aborts interpretation of this stream.
    fn halted(&self) -> bool {
        false
    }
    /// An opaque fill of a path made only of rectangles, as page-space
    /// bounding boxes `[x0, y0, x1, y1]`. Text drawn earlier and lying inside
    /// one is hidden under it.
    fn opaque_fill(&mut self, _rects: &[[f32; 4]]) {}
    /// `gs /name`: whether that ExtGState makes later fills see-through
    /// (`/ca` < 1, a blend mode, a soft mask). `None` = no change.
    fn ext_gstate_transparent(&mut self, _name: &str) -> Option<bool> {
        None
    }
}

/// Max operands kept between operators; extra operands are dropped. Real `TJ`
/// arrays are far smaller; this only bounds hostile streams.
const MAX_OPERANDS: usize = 1 << 16;

/// Max nesting of `q`; deeper pushes are ignored (and their `Q` pops balance
/// against the clamp, so a hostile stream cannot grow the stack).
const MAX_GS_DEPTH: usize = 256;

#[derive(Debug, Clone)]
enum Tok<'a> {
    Str(Vec<u8>),
    Name(&'a [u8]),
    Number(f32),
    ArrayStart,
    ArrayEnd,
    Op(&'a [u8]),
}

fn is_delim(c: u8) -> bool {
    matches!(
        c,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Lexer<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() || c == 0 {
                self.i += 1;
            } else if c == b'%' {
                // comment runs to end of line
                while let Some(c) = self.peek() {
                    self.i += 1;
                    if c == b'\n' || c == b'\r' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn next(&mut self) -> Option<Tok<'a>> {
        loop {
            self.skip_ws();
            let c = *self.b.get(self.i)?;
            let tok = match c {
                b'(' => self.literal_string(),
                b'<' => {
                    if self.b.get(self.i + 1) == Some(&b'<') {
                        self.i += 2;
                        Tok::Op(b"<<")
                    } else {
                        self.hex_string()
                    }
                }
                b'>' => {
                    if self.b.get(self.i + 1) == Some(&b'>') {
                        self.i += 2;
                        Tok::Op(b">>")
                    } else {
                        self.i += 1;
                        Tok::Op(b">")
                    }
                }
                b'[' => {
                    self.i += 1;
                    Tok::ArrayStart
                }
                b']' => {
                    self.i += 1;
                    Tok::ArrayEnd
                }
                b'/' => self.name(),
                b'{' | b'}' => {
                    self.i += 1;
                    Tok::Op(&self.b[self.i - 1..self.i])
                }
                // A stray `)` has no opener: skip it (iteratively — a long run of
                // them must not recurse).
                b')' => {
                    self.i += 1;
                    continue;
                }
                c if c == b'+' || c == b'-' || c == b'.' || c.is_ascii_digit() => self.number(),
                _ => self.operator(),
            };
            return Some(tok);
        }
    }

    fn literal_string(&mut self) -> Tok<'a> {
        self.i += 1; // opening (
        let mut out = Vec::new();
        let mut depth = 1usize;
        while depth > 0 {
            let Some(c) = self.peek() else {
                break;
            };
            match c {
                b'\\' => {
                    self.i += 1;
                    let Some(n) = self.peek() else {
                        break;
                    };
                    match n {
                        b'n' => {
                            out.push(b'\n');
                            self.i += 1;
                        }
                        b'r' => {
                            out.push(b'\r');
                            self.i += 1;
                        }
                        b't' => {
                            out.push(b'\t');
                            self.i += 1;
                        }
                        b'b' => {
                            out.push(0x08);
                            self.i += 1;
                        }
                        b'f' => {
                            out.push(0x0C);
                            self.i += 1;
                        }
                        b'(' | b')' | b'\\' => {
                            out.push(n);
                            self.i += 1;
                        }
                        // line continuation: `\<LF>`, `\<CR>`, `\<CR><LF>`
                        b'\n' => {
                            self.i += 1;
                        }
                        b'\r' => {
                            self.i += 1;
                            if self.peek() == Some(b'\n') {
                                self.i += 1;
                            }
                        }
                        d if (b'0'..=b'7').contains(&d) => {
                            let mut val = 0u32;
                            let mut count = 0;
                            while count < 3 {
                                match self.peek() {
                                    Some(d2) if (b'0'..=b'7').contains(&d2) => {
                                        val = val * 8 + (d2 - b'0') as u32;
                                        self.i += 1;
                                        count += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push((val & 0xFF) as u8);
                        }
                        _ => {
                            self.i += 1;
                        } // unknown escape: drop the backslash
                    }
                }
                b'(' => {
                    depth += 1;
                    out.push(b'(');
                    self.i += 1;
                }
                b')' => {
                    depth -= 1;
                    self.i += 1;
                    if depth > 0 {
                        out.push(b')');
                    }
                }
                _ => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        Tok::Str(out)
    }

    fn hex_string(&mut self) -> Tok<'a> {
        self.i += 1; // opening <
        let mut out = Vec::new();
        let mut hi: Option<u8> = None;
        while let Some(c) = self.peek() {
            if c == b'>' {
                self.i += 1;
                break;
            }
            if let Some(v) = (c as char).to_digit(16) {
                match hi {
                    None => hi = Some(v as u8),
                    Some(h) => {
                        out.push((h << 4) | (v as u8));
                        hi = None;
                    }
                }
            }
            self.i += 1;
        }
        if let Some(h) = hi {
            out.push(h << 4);
        } // odd trailing digit → pad low nibble 0
        Tok::Str(out)
    }

    fn name(&mut self) -> Tok<'a> {
        self.i += 1; // leading /
        let start = self.i;
        while let Some(c) = self.peek() {
            if is_delim(c) || c.is_ascii_whitespace() || c == 0 {
                break;
            }
            self.i += 1;
        }
        Tok::Name(&self.b[start..self.i])
    }

    fn number(&mut self) -> Tok<'a> {
        let start = self.i;
        if matches!(self.peek(), Some(b'+') | Some(b'-')) {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' {
                self.i += 1;
            } else {
                break;
            }
        }
        let v = std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(0.0);
        Tok::Number(v)
    }

    /// Scan a keyword. The caller guarantees the current byte is a regular
    /// character (not whitespace/delimiter), so this always consumes ≥ 1 byte.
    fn operator(&mut self) -> Tok<'a> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if is_delim(c) || c.is_ascii_whitespace() || c == 0 {
                break;
            }
            self.i += 1;
        }
        let op = &self.b[start..self.i];
        // Inline image: `BI … ID <single-ws><binary>EI`. When we see the `ID`
        // operator, skip its binary payload up to `EI` so a `(` inside the image
        // data can't start a literal string that swallows the operators after it
        // (correctness) or allocate a huge bogus string on hostile uploads (DoS).
        if op == b"ID" {
            self.skip_inline_image();
        }
        Tok::Op(op)
    }

    /// Scan forward to the `EI` that ends an inline image. `EI` is recognized
    /// only when preceded by whitespace and followed by whitespace/EOF/delimiter
    /// (avoids a false match on bytes like `EIC`). Bounded by the buffer length.
    fn skip_inline_image(&mut self) {
        while self.i + 3 <= self.b.len() {
            let c = self.b[self.i];
            let is_ei =
                c.is_ascii_whitespace() && self.b.get(self.i + 1..self.i + 3) == Some(b"EI");
            if is_ei {
                let after = self.b.get(self.i + 3).copied();
                let ends = after.is_none()
                    || after.is_some_and(|a| a.is_ascii_whitespace() || is_delim(a));
                if ends {
                    self.i += 3;
                    return;
                }
            }
            self.i += 1;
        }
        self.i = self.b.len();
    }
}

/// Last `N` numeric operands (in order), or `None` if fewer than `N` exist.
fn last_nums<const N: usize>(stack: &[Tok]) -> Option<[f32; N]> {
    let mut out = [0f32; N];
    let mut found = 0;
    for t in stack.iter().rev() {
        if let Tok::Number(v) = t {
            out[N - 1 - found] = *v;
            found += 1;
            if found == N {
                return Some(out);
            }
        }
    }
    None
}

fn last_name<'a>(stack: &[Tok<'a>]) -> Option<&'a [u8]> {
    stack.iter().rev().find_map(|t| match t {
        Tok::Name(n) => Some(*n),
        _ => None,
    })
}

#[derive(Clone)]
struct GState {
    ctm: Matrix,
    /// Fills are see-through (from `gs`): they hide nothing.
    transparent: bool,
    font: Option<Rc<str>>,
    size: f32,
    tc: f32,
    tw: f32,
    th: f32,
    leading: f32,
    rise: f32,
}

struct Interp<'s, S: Sink> {
    sink: &'s mut S,
    gs: GState,
    tm: Matrix,
    tlm: Matrix,
}

impl<S: Sink> Interp<'_, S> {
    fn move_line(&mut self, tx: f32, ty: f32) {
        let m = mul(&[1.0, 0.0, 0.0, 1.0, tx, ty], &self.tlm);
        if is_sane(&m) {
            self.tlm = m;
            self.tm = m;
        }
    }

    fn advance_x(&mut self, tx: f32) {
        let m = mul(&[1.0, 0.0, 0.0, 1.0, tx, 0.0], &self.tm);
        if is_sane(&m) {
            self.tm = m;
        }
    }

    fn show(&mut self, bytes: Vec<u8>) {
        let m = mul(&self.tm, &self.gs.ctm);
        let th = self.gs.th;
        let size = self.gs.size;
        let xs = m[0].hypot(m[1]);
        let run = TextRun {
            font: self.gs.font.as_deref().map(str::to_owned),
            bytes,
            x: m[4] + self.gs.rise * m[2],
            y: m[5] + self.gs.rise * m[3],
            size: (size * m[2].hypot(m[3])).abs(),
            scale: Scale {
                em: (size * th * xs).abs(),
                glyph: self.gs.tc * th * xs,
                space: self.gs.tw * th * xs,
                dx: if xs > 0.0 { m[0] / xs } else { 1.0 },
                dy: if xs > 0.0 { m[1] / xs } else { 0.0 },
            },
        };
        let adv = self.sink.text(run);
        let tx =
            (adv.em * size + adv.glyphs as f32 * self.gs.tc + adv.spaces as f32 * self.gs.tw) * th;
        if tx.is_finite() {
            self.advance_x(tx);
        }
    }

    fn show_stack_strings(&mut self, stack: &mut [Tok]) {
        // TJ: strings are shown, numbers shift the text position by
        // `-n/1000 × size × Th`.
        for t in stack.iter_mut() {
            match t {
                Tok::Str(b) => {
                    let bytes = std::mem::take(b);
                    self.show(bytes);
                }
                Tok::Number(n) => {
                    let tx = -*n / 1000.0 * self.gs.size * self.gs.th;
                    if tx.is_finite() {
                        self.advance_x(tx);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Tokenize `content` and drive `sink`. `ctm` is the matrix mapping the stream's
/// user space into the space the sink wants coordinates in (identity for a
/// page; `form /Matrix × CTM at Do` for a Form XObject).
/// A content-stream name as text, with `#xx` escapes decoded (ISO 32000 7.3.5):
/// `/F#31` names the font resource `F1`.
fn name_string(n: &[u8]) -> String {
    if !n.contains(&b'#') {
        return String::from_utf8_lossy(n).into_owned();
    }
    let mut out = Vec::with_capacity(n.len());
    let mut i = 0;
    while i < n.len() {
        let hex = |k: usize| n.get(k).and_then(|&c| (c as char).to_digit(16));
        match (n[i], hex(i + 1), hex(i + 2)) {
            (b'#', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn interpret<S: Sink>(content: &[u8], ctm: Matrix, sink: &mut S) {
    let mut it = Interp {
        sink,
        gs: GState {
            ctm,
            transparent: false,
            font: None,
            size: 0.0,
            tc: 0.0,
            tw: 0.0,
            th: 1.0,
            leading: 0.0,
            rise: 0.0,
        },
        tm: IDENTITY,
        tlm: IDENTITY,
    };
    let mut gs_stack: Vec<GState> = Vec::new();
    // The current path, while it consists only of rectangles (page space).
    let mut rects: Vec<[f32; 4]> = Vec::new();
    let mut rects_only = true;
    // Number of `q` pushes dropped by the depth clamp; their `Q`s are ignored.
    let mut skipped_q = 0usize;
    let mut stack: Vec<Tok> = Vec::new();
    let mut lex = Lexer::new(content);
    while let Some(tok) = lex.next() {
        let Tok::Op(op) = tok else {
            if stack.len() < MAX_OPERANDS {
                stack.push(tok);
            }
            continue;
        };
        match op {
            b"q" => {
                if gs_stack.len() < MAX_GS_DEPTH {
                    gs_stack.push(it.gs.clone());
                } else {
                    skipped_q += 1;
                }
            }
            b"Q" => {
                if skipped_q > 0 {
                    skipped_q -= 1;
                } else if let Some(g) = gs_stack.pop() {
                    it.gs = g;
                }
            }
            b"cm" => {
                if let Some(m) = last_nums::<6>(&stack) {
                    let c = mul(&m, &it.gs.ctm);
                    if is_sane(&c) {
                        it.gs.ctm = c;
                    }
                }
            }
            b"BT" => {
                it.tm = IDENTITY;
                it.tlm = IDENTITY;
            }
            b"Tf" => {
                if let Some(n) = last_name(&stack) {
                    it.gs.font = Some(Rc::from(name_string(n).as_str()));
                }
                if let Some([s]) = last_nums::<1>(&stack) {
                    it.gs.size = s;
                }
            }
            b"Tc" => {
                if let Some([v]) = last_nums::<1>(&stack) {
                    it.gs.tc = v;
                }
            }
            b"Tw" => {
                if let Some([v]) = last_nums::<1>(&stack) {
                    it.gs.tw = v;
                }
            }
            b"Tz" => {
                if let Some([v]) = last_nums::<1>(&stack) {
                    it.gs.th = v / 100.0;
                }
            }
            b"TL" => {
                if let Some([v]) = last_nums::<1>(&stack) {
                    it.gs.leading = v;
                }
            }
            b"Ts" => {
                if let Some([v]) = last_nums::<1>(&stack) {
                    it.gs.rise = v;
                }
            }
            b"Tm" => {
                if let Some(m) = last_nums::<6>(&stack) {
                    if is_sane(&m) {
                        it.tm = m;
                        it.tlm = m;
                    }
                }
            }
            b"Td" => {
                if let Some([tx, ty]) = last_nums::<2>(&stack) {
                    it.move_line(tx, ty);
                }
            }
            b"TD" => {
                if let Some([tx, ty]) = last_nums::<2>(&stack) {
                    it.gs.leading = -ty;
                    it.move_line(tx, ty);
                }
            }
            b"T*" => {
                let l = it.gs.leading;
                it.move_line(0.0, -l);
            }
            b"Tj" => {
                if let Some(Tok::Str(b)) = stack.last_mut() {
                    let bytes = std::mem::take(b);
                    it.show(bytes);
                }
            }
            b"'" | b"\"" => {
                if op == b"\"" {
                    // aw ac string "
                    if let Some([aw, ac]) = last_nums::<2>(&stack) {
                        it.gs.tw = aw;
                        it.gs.tc = ac;
                    }
                }
                let l = it.gs.leading;
                it.move_line(0.0, -l);
                if let Some(Tok::Str(b)) = stack.last_mut() {
                    let bytes = std::mem::take(b);
                    it.show(bytes);
                }
            }
            b"TJ" => it.show_stack_strings(&mut stack),
            b"Do" => {
                if let Some(n) = last_name(&stack) {
                    let name = name_string(n);
                    let ctm = it.gs.ctm;
                    it.sink.xobject(&name, &ctm);
                }
            }
            b"gs" => {
                if let Some(n) = last_name(&stack) {
                    if let Some(t) = it.sink.ext_gstate_transparent(&name_string(n)) {
                        it.gs.transparent = t;
                    }
                }
            }
            b"re" => {
                if let Some([x, y, w, h]) = last_nums::<4>(&stack) {
                    if rects_only && rects.len() < 1024 {
                        let m = it.gs.ctm;
                        let pts =
                            [(x, y), (x + w, y), (x, y + h), (x + w, y + h)].map(|(px, py)| {
                                (m[0] * px + m[2] * py + m[4], m[1] * px + m[3] * py + m[5])
                            });
                        let xs = pts.map(|p| p.0);
                        let ys = pts.map(|p| p.1);
                        let bb = [
                            xs.iter().copied().fold(f32::INFINITY, f32::min),
                            ys.iter().copied().fold(f32::INFINITY, f32::min),
                            xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                            ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                        ];
                        // Rotated or skewed rectangles are not axis-aligned boxes.
                        let axis_aligned = m[1].abs() < 1e-3 && m[2].abs() < 1e-3;
                        if axis_aligned && bb.iter().all(|v| v.is_finite()) {
                            rects.push(bb);
                        } else {
                            rects_only = false;
                        }
                    }
                }
            }
            b"m" | b"l" | b"c" | b"v" | b"y" => rects_only = false,
            b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"S" | b"s" | b"n" => {
                let fills = !matches!(op, b"S" | b"s" | b"n");
                if fills && rects_only && !rects.is_empty() && !it.gs.transparent {
                    it.sink.opaque_fill(&rects);
                }
                rects.clear();
                rects_only = true;
            }
            _ => {}
        }
        stack.clear();
        if it.sink.halted() {
            return;
        }
    }
}

/// Sink that records every run and does not advance the text position (all
/// strings of one `TJ` share a start position). Used by [`collect_runs`].
struct Collect(Vec<TextRun>);

impl Sink for Collect {
    fn text(&mut self, run: TextRun) -> Advance {
        self.0.push(run);
        Advance::default()
    }
}

/// Collect text runs from decoded content-stream bytes, in stream order.
/// Convenience wrapper over [`interpret`] for callers that only want raw show
/// strings (no font decoding, no Form XObject recursion).
pub fn collect_runs(content: &[u8]) -> Vec<TextRun> {
    let mut sink = Collect(Vec::new());
    interpret(content, IDENTITY, &mut sink);
    sink.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_escapes_are_decoded() {
        assert_eq!(name_string(b"F#31"), "F1");
        assert_eq!(name_string(b"A#20B"), "A B");
        assert_eq!(name_string(b"Bad#zz#4"), "Bad#zz#4");
    }

    #[test]
    fn tj_captures_font_and_bytes() {
        let runs = collect_runs(b"BT /F1 12 Tf (Hello) Tj ET");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].font.as_deref(), Some("F1"));
        assert_eq!(runs[0].bytes, b"Hello");
    }

    #[test]
    fn tj_array_emits_each_string() {
        // [(He)-10(llo)] TJ  → two runs "He", "llo"
        let runs = collect_runs(b"[(He)-10(llo)]TJ");
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].bytes, b"He");
        assert_eq!(runs[1].bytes, b"llo");
    }

    #[test]
    fn literal_string_escapes() {
        let runs = collect_runs(b"(a\\nb\\tc\\065d) Tj");
        assert_eq!(runs[0].bytes, b"a\nb\tc5d"); // \065 octal = 53 = '5'
    }

    #[test]
    fn hex_string_operand() {
        let runs = collect_runs(b"<48656C6C6F> Tj"); // "Hello"
        assert_eq!(runs[0].bytes, b"Hello");
    }

    #[test]
    fn quote_operators_show_string() {
        let runs = collect_runs(b"2 3 (X) '");
        assert_eq!(runs[0].bytes, b"X");
        let runs2 = collect_runs(b"1 2 3 (Y) \"");
        assert_eq!(runs2[0].bytes, b"Y");
    }

    #[test]
    fn no_tf_means_unknown_font() {
        let runs = collect_runs(b"(NoFont) Tj");
        assert_eq!(runs[0].font, None);
    }

    #[test]
    fn td_and_tm_position_runs() {
        let runs = collect_runs(b"BT 1 0 0 1 100 700 Tm (A) Tj 10 -14 Td (B) Tj ET");
        assert_eq!((runs[0].x, runs[0].y), (100.0, 700.0));
        assert_eq!((runs[1].x, runs[1].y), (110.0, 686.0));
    }

    #[test]
    fn cm_and_q_transform_text_position() {
        // A flipped page CTM (common from Cairo/Skia exporters): y' = 792 - y.
        let runs =
            collect_runs(b"q 1 0 0 -1 0 792 cm BT 1 0 0 1 50 100 Tm (A) Tj ET Q BT (B) Tj ET");
        assert_eq!((runs[0].x, runs[0].y), (50.0, 692.0));
        // Q restored the identity CTM.
        assert_eq!((runs[1].x, runs[1].y), (0.0, 0.0));
    }

    #[test]
    fn tm_scale_feeds_effective_font_size() {
        // `/F1 1 Tf` with a 12× text matrix is a 12pt font.
        let runs = collect_runs(b"BT /F1 1 Tf 12 0 0 12 10 10 Tm (A) Tj ET");
        assert!((runs[0].size - 12.0).abs() < 1e-4);
    }

    #[test]
    fn sink_advance_moves_text_position() {
        struct Fixed(Vec<(f32, f32)>);
        impl Sink for Fixed {
            fn text(&mut self, run: TextRun) -> Advance {
                self.0.push((run.x, run.y));
                Advance {
                    em: 0.5,
                    glyphs: 0,
                    spaces: 0,
                }
            }
        }
        let mut s = Fixed(Vec::new());
        // 10pt font, each string advances 5pt; the TJ number -1000 adds 1 em.
        interpret(
            b"BT /F1 10 Tf 0 0 Td [(a)(b) -1000 (c)] TJ ET",
            IDENTITY,
            &mut s,
        );
        assert_eq!(s.0, vec![(0.0, 0.0), (5.0, 0.0), (20.0, 0.0)]);
    }

    // --- robustness: hostile streams must be handled in bounded time/stack ---

    #[test]
    fn stray_close_parens_do_not_recurse() {
        let mut data = vec![b')'; 2_000_000];
        data.extend_from_slice(b" BT (ok) Tj ET");
        let runs = collect_runs(&data);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].bytes, b"ok");
    }

    #[test]
    fn overflowing_numbers_never_poison_positions() {
        // 1e40 parses to +inf in f32; inf - inf would become NaN.
        let big = "9".repeat(60);
        let content = format!("BT {big} {big} Td -{big} -{big} Td (a) Tj 1 0 0 1 5 5 Tm (b) Tj ET");
        let runs = collect_runs(content.as_bytes());
        assert!(runs.iter().all(|r| r.x.is_finite() && r.y.is_finite()));
        assert_eq!((runs[1].x, runs[1].y), (5.0, 5.0));
    }

    #[test]
    fn unbalanced_q_and_deep_q_are_bounded() {
        let mut data = Vec::new();
        for _ in 0..100_000 {
            data.extend_from_slice(b"q ");
        }
        data.extend_from_slice(b"BT (x) Tj ET");
        for _ in 0..200_000 {
            data.extend_from_slice(b"Q ");
        }
        assert_eq!(collect_runs(&data).len(), 1);
    }

    #[test]
    fn operand_flood_is_capped() {
        let mut data = Vec::new();
        for _ in 0..1_000_000 {
            data.extend_from_slice(b"1 ");
        }
        data.extend_from_slice(b"Tj BT (z) Tj ET");
        assert_eq!(collect_runs(&data).len(), 1);
    }

    #[test]
    fn unterminated_string_and_inline_image_terminate() {
        assert_eq!(collect_runs(b"BT (never closed Tj").len(), 0);
        assert_eq!(collect_runs(b"BI /W 1 ID \xff\xfe(((").len(), 0);
    }
}
