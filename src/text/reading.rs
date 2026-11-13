//! Reading order: the default (non-`-layout`, non-`-raw`) text mode.
//!
//! Physical layout prints a two-column page line by line across both columns,
//! with the columns kept apart by spaces. That is right for a terminal and
//! wrong for anything that reads the text as text (search, chunking for an AI
//! assistant): once the spaces are collapsed, every line of the left column is
//! glued to its neighbour from the right column. This pass finds column
//! gutters and emits each column in turn, while keeping full-width lines
//! (headings, captions spanning both columns) in place between them.
//!
//! Tables are the hard case: their columns are separated by gutters too, and
//! reading a troubleshooting table column by column would separate every fault
//! from its remedy. A gutter is therefore only accepted between two blocks of
//! *running text*: on each side at least [`MIN_LINES`] lines, a block at least
//! [`MIN_SIDE_SHARE`] of the region wide and comparable to the other side, and
//! lines that do not behave like table rows across the gutter (cells pair up
//! on shared baselines and one side skips the rows where the other wraps;
//! row ends line up on both sides; text columns break independently).
//! Anything else stays in rows.

use super::{group_lines, overlaps_prev, visual_to_logical, word_gap_thresholds, Run};

/// Lines needed on each side of a gutter.
const MIN_LINES: usize = 5;
/// Share of one side's vertical breaks (paragraph ends, gaps before a
/// heading) that may coincide with a break on the other side. In a table the
/// rows end together on both sides of a column gutter; in running text the two
/// columns break independently.
const MAX_ALIGNED_BREAKS: f32 = 0.5;
/// Each side's width as a share of the region's, and the most one side may be
/// wider than the other: text columns are wide and comparable, while table
/// columns (part numbers, labels, short values) are typically narrow.
const MIN_SIDE_SHARE: f32 = 0.3;
const MAX_SIDE_RATIO: f32 = 2.0;
/// Cuts nest at most this deep (column splits and horizontal cuts).
const MAX_DEPTH: usize = 8;

/// A stretch of one line between large gaps (a cell, a column's part).
struct Seg {
    line: usize,
    /// Baseline of the segment's line.
    y: f32,
    x0: f32,
    x1: f32,
    text: String,
}

/// Text of a page in reading order.
pub(super) fn reading_runs(runs: &[Run]) -> String {
    if runs.is_empty() {
        return String::new();
    }
    let lines = group_lines(runs);
    let segs = segments(runs, &lines);
    let all: Vec<usize> = (0..segs.len()).collect();
    let order = order(&segs, all, 0);
    let mut out = String::new();
    let mut line_buf = String::new();
    let mut prev_line: Option<usize> = None;
    for id in order {
        let s = &segs[id];
        if prev_line.is_some_and(|l| l != s.line) {
            out.push_str(&visual_to_logical(line_buf.trim_end()));
            out.push('\n');
            line_buf.clear();
        }
        if !line_buf.is_empty() {
            line_buf.push(' ');
        }
        line_buf.push_str(&s.text);
        prev_line = Some(s.line);
    }
    if !line_buf.is_empty() {
        out.push_str(&visual_to_logical(line_buf.trim_end()));
        out.push('\n');
    }
    out
}

/// Split each line into segments at gaps much wider than a word space; words
/// inside a segment are joined like the other modes do.
fn segments(runs: &[Run], lines: &[Vec<usize>]) -> Vec<Seg> {
    let mut segs = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let thresholds = word_gap_thresholds(runs, line);
        let mut cur: Option<Seg> = None;
        let mut prev_end: Option<f32> = None;
        for (pos, &i) in line.iter().enumerate() {
            let r = &runs[i];
            if let Some(pe) = prev_end {
                if overlaps_prev(r, pe) {
                    continue;
                }
            }
            let gap = prev_end.map(|pe| r.x - pe);
            let big = gap.is_some_and(|g| g > (1.2 * r.size).max(6.0));
            if big {
                if let Some(s) = cur.take() {
                    segs.push(s);
                }
            }
            match cur.as_mut() {
                Some(s) => {
                    if gap.is_some_and(|g| g > thresholds[pos]) && !s.text.ends_with(' ') {
                        s.text.push(' ');
                    }
                    s.text.push_str(&r.text);
                    s.x1 = s.x1.max(r.end_x);
                }
                None => {
                    cur = Some(Seg {
                        line: li,
                        y: runs[line[0]].y,
                        x0: r.x.min(r.end_x),
                        x1: r.x.max(r.end_x),
                        text: r.text.clone(),
                    })
                }
            }
            prev_end = Some(r.end_x);
        }
        if let Some(s) = cur {
            segs.push(s);
        }
    }
    for s in &mut segs {
        let t = s.text.trim().to_owned();
        s.text = t;
    }
    segs.retain(|s| !s.text.is_empty());
    segs
}

/// Segment ids in reading order: top to bottom, and where a prose gutter
/// splits a stretch of lines, its left column before its right one.
fn order(segs: &[Seg], ids: Vec<usize>, depth: usize) -> Vec<usize> {
    let by_line = |ids: &[usize]| -> Vec<Vec<usize>> {
        let mut lines: Vec<Vec<usize>> = Vec::new();
        for &id in ids {
            match lines.last_mut() {
                Some(l) if segs[l[0]].line == segs[id].line => l.push(id),
                _ => lines.push(vec![id]),
            }
        }
        lines
    };
    let mut ids = ids;
    // Lines top to bottom (line indices are already in that order), segments
    // left to right.
    ids.sort_by(|&a, &b| {
        segs[a]
            .line
            .cmp(&segs[b].line)
            .then(segs[a].x0.total_cmp(&segs[b].x0))
    });
    if depth >= MAX_DEPTH {
        return ids;
    }
    let lines = by_line(&ids);
    let Some((g0, g1)) = gutter(segs, &lines, 2) else {
        // No gutter across the whole region: a page often stacks text columns
        // above a full-width table or figure whose rows cross the gutter. Cut
        // at the widest vertical gap and look again in each part.
        return match horizontal_cut(segs, &lines) {
            Some(k) => {
                let top: Vec<usize> = lines[..k].iter().flatten().copied().collect();
                let bottom: Vec<usize> = lines[k..].iter().flatten().copied().collect();
                let mut out = order(segs, top, depth + 1);
                out.extend(order(segs, bottom, depth + 1));
                out
            }
            None => ids,
        };
    };
    // Split every line at the gutter; a line with a segment across it spans
    // both columns and is kept whole, between the column blocks.
    let mut out = Vec::with_capacity(ids.len());
    let (mut left, mut right): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    let flush = |left: &mut Vec<usize>, right: &mut Vec<usize>, out: &mut Vec<usize>| {
        out.extend(order(segs, std::mem::take(left), depth + 1));
        out.extend(order(segs, std::mem::take(right), depth + 1));
    };
    for line in &lines {
        let spans = line.iter().any(|&id| segs[id].x0 < g0 && segs[id].x1 > g1);
        if spans {
            flush(&mut left, &mut right, &mut out);
            out.extend(line);
        } else {
            for &id in line {
                if segs[id].x1 <= g1 {
                    left.push(id);
                } else {
                    right.push(id);
                }
            }
        }
    }
    flush(&mut left, &mut right, &mut out);
    out
}

/// Where to cut a region horizontally: before the line that follows the widest
/// vertical gap, if that gap is clearly wider than the usual line spacing and
/// both parts keep enough lines to hold a column block.
fn horizontal_cut(segs: &[Seg], lines: &[Vec<usize>]) -> Option<usize> {
    if lines.len() < 2 * MIN_LINES {
        return None;
    }
    let ys: Vec<f32> = lines.iter().map(|l| segs[l[0]].y).collect();
    let gaps: Vec<f32> = ys.windows(2).map(|w| (w[0] - w[1]).abs()).collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let leading = sorted[sorted.len() / 2].max(1.0);
    let (k, widest) = gaps
        .iter()
        .enumerate()
        .filter(|&(k, _)| k + 1 >= MIN_LINES && lines.len() - (k + 1) >= MIN_LINES)
        .max_by(|a, b| a.1.total_cmp(b.1))?;
    (*widest > 1.5 * leading).then_some(k + 1)
}

/// The widest vertical gap, crossed by few lines, with a block of prose on
/// each side. `None` if there is no such gutter (single column, or a table).
fn gutter(segs: &[Seg], lines: &[Vec<usize>], nest: u8) -> Option<(f32, f32)> {
    if lines.len() < 2 * MIN_LINES {
        return None;
    }
    // Sweep segment extents along x, counting how many lines cover each x.
    let mut ev: Vec<(f32, i32)> = Vec::new();
    for line in lines {
        for &id in line {
            ev.push((segs[id].x0, 1));
            ev.push((segs[id].x1, -1));
        }
    }
    ev.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let lo = ev.first()?.0;
    let hi = ev.last()?.0;
    // Lines allowed to cross: headings and spanning captions, not body text.
    let allow = (lines.len() / 12).max(1) as i32;
    let min_width = 8.0;
    let mut best: Option<(f32, f32)> = None;
    let mut count = 0i32;
    let mut open: Option<f32> = None;
    for (x, d) in ev {
        let was_low = count <= allow;
        count += d;
        let low = count <= allow;
        match (was_low, low) {
            (false, true) => open = Some(x),
            (true, false) => {
                if let Some(o) = open.take() {
                    if x - o >= min_width && o > lo && x < hi {
                        let wider = best.is_none_or(|(b0, b1)| x - o > b1 - b0);
                        if wider && prose_both_sides(segs, lines, o, x, hi - lo, nest) {
                            best = Some((o, x));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    best
}

/// Both sides of a candidate gutter hold running text, not table cells.
fn prose_both_sides(
    segs: &[Seg],
    lines: &[Vec<usize>],
    g0: f32,
    g1: f32,
    region: f32,
    nest: u8,
) -> bool {
    // Per side: (width, baselines top to bottom).
    let side = |left: bool| -> Option<(f32, Vec<f32>)> {
        let mut x0 = f32::INFINITY;
        let mut x1 = f32::NEG_INFINITY;
        let mut ys = Vec::new();
        for line in lines {
            let parts: Vec<&Seg> = line
                .iter()
                .map(|&id| &segs[id])
                .filter(|s| if left { s.x1 <= g0 } else { s.x0 >= g1 })
                .collect();
            if let Some(first) = parts.first() {
                ys.push(first.y);
                for p in &parts {
                    x0 = x0.min(p.x0);
                    x1 = x1.max(p.x1);
                }
            }
        }
        let width = x1 - x0;
        (ys.len() >= MIN_LINES && width >= 60.0 && width >= MIN_SIDE_SHARE * region)
            .then_some((width, ys))
    };
    let (Some((lw, ly)), Some((rw, ry))) = (side(true), side(false)) else {
        return false;
    };
    if lw.max(rw) > MAX_SIDE_RATIO * lw.min(rw) {
        return false;
    }
    if table_rows(&ly, &ry) || table_rows(&ry, &ly) {
        return false;
    }
    if cells(segs, lines, |s| s.x1 <= g0, nest) || cells(segs, lines, |s| s.x0 >= g1, nest) {
        return false;
    }
    let (lb, rb) = (breaks(&ly), breaks(&ry));
    let (few, many) = if lb.len() <= rb.len() {
        (&lb, &rb)
    } else {
        (&rb, &lb)
    };
    if few.len() < 2 {
        // Too few breaks to tell: a single run of text on each side reads as
        // two columns.
        return true;
    }
    let aligned = few
        .iter()
        .filter(|&&(y, tol)| many.iter().any(|&(m, _)| (m - y).abs() <= tol))
        .count();
    (aligned as f32) <= MAX_ALIGNED_BREAKS * few.len() as f32
}

/// One side of a candidate gutter is made of table cells: most of its lines
/// are several pieces apart ("0 | Reserved", "6232 | 160 | 290"), and those
/// pieces do not form text columns of their own (a side holding two text
/// columns, as on a three-column page, is fine).
fn cells(segs: &[Seg], lines: &[Vec<usize>], on_side: impl Fn(&Seg) -> bool, nest: u8) -> bool {
    let side: Vec<Vec<usize>> = lines
        .iter()
        .map(|l| {
            l.iter()
                .copied()
                .filter(|&id| on_side(&segs[id]))
                .collect::<Vec<_>>()
        })
        .filter(|l| !l.is_empty())
        .collect();
    let multi = side.iter().filter(|l| l.len() >= 2).count();
    // Whether the side holds text columns of its own is asked a bounded number
    // of levels deep (dense tables would otherwise repeat the search at every
    // level); past that, a cell-like side counts as table cells.
    multi as f32 >= 0.4 * side.len() as f32
        && (nest == 0 || gutter(segs, &side, nest - 1).is_none())
}

/// Table rows seen from one side: most lines of `side` sit on a
/// baseline the other side also has (a cell beside its neighbour cell), and
/// `side` often skips baselines the other side uses (its neighbour cell wraps
/// onto more lines). Text columns sharing a baseline grid also pair up, but
/// skip only at their own paragraph breaks.
fn table_rows(side: &[f32], other: &[f32]) -> bool {
    if side.len() < 3 {
        return false;
    }
    let tol = 1.0;
    let paired = side
        .iter()
        .filter(|&&y| other.iter().any(|&o| (o - y).abs() <= tol))
        .count();
    // Not every row has a cell on both sides ("Reserved" bits, blank remedy
    // cells): half the lines pairing up is already a table's pattern.
    if (paired as f32) < 0.5 * side.len() as f32 {
        return false;
    }
    let skips = side
        .windows(2)
        .filter(|w| {
            let (hi, lo) = (w[0].max(w[1]), w[0].min(w[1]));
            other.iter().any(|&o| o < hi - tol && o > lo + tol)
        })
        .count();
    skips as f32 >= 0.3 * (side.len() - 1) as f32
}

/// Vertical breaks in a column: the midpoints of gaps between consecutive
/// baselines that are clearly wider than the usual line spacing, with a
/// matching tolerance of half that spacing.
fn breaks(ys: &[f32]) -> Vec<(f32, f32)> {
    let mut gaps: Vec<f32> = ys.windows(2).map(|w| (w[0] - w[1]).abs()).collect();
    if gaps.is_empty() {
        return Vec::new();
    }
    gaps.sort_by(|a, b| a.total_cmp(b));
    let leading = gaps[gaps.len() / 2].max(1.0);
    ys.windows(2)
        .filter(|w| (w[0] - w[1]).abs() > 1.4 * leading)
        .map(|w| ((w[0] + w[1]) / 2.0, 0.5 * leading))
        .collect()
}
