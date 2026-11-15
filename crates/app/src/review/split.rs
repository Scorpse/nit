//! Side-by-side (split) diff: the old file on the left, the new file on the right. Pure: the
//! pairing of a file's unified rows into split lines, the mapping between the two, the side
//! and line a comment on a split line goes to, and the text a one-sided selection copies.
//! Painting lives in `review::ui`.
//!
//! The unified rows stay the one source of truth: comments, drafts, colours and word diff are
//! all keyed by unified row; a split line only names the row shown on each side.

use prcore::diff::{Row, RowKind};
use prcore::draft::Side;

use super::selection::{row_span, slice_cols, Selection};

/// No row on this side of a split line (the shorter side of a change is padded).
const PAD: u32 = u32::MAX;

/// Store meta key that remembers the diff layout.
pub const LAYOUT_META_KEY: &str = "review_diff_layout";

/// Below these widths the split view does not fit and the unified view is shown instead.
pub const SPLIT_MIN_WINDOW_W: f32 = 900.0;
pub const SPLIT_MIN_PANE_W: f32 = 600.0;

/// How the diff is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffLayout {
    #[default]
    Unified,
    Split,
}

impl DiffLayout {
    pub fn as_str(self) -> &'static str {
        match self {
            DiffLayout::Unified => "unified",
            DiffLayout::Split => "split",
        }
    }

    /// The stored value; anything unknown is the unified view.
    pub fn parse(s: Option<&str>) -> DiffLayout {
        match s {
            Some("split") => DiffLayout::Split,
            _ => DiffLayout::Unified,
        }
    }

    pub fn toggled(self) -> DiffLayout {
        match self {
            DiffLayout::Unified => DiffLayout::Split,
            DiffLayout::Split => DiffLayout::Unified,
        }
    }
}

/// Whether the split view fits a window `window_w` wide whose diff pane is `pane_w` wide.
pub fn split_fits(window_w: f32, pane_w: f32) -> bool {
    window_w >= SPLIT_MIN_WINDOW_W && pane_w >= SPLIT_MIN_PANE_W
}

/// A side of the split view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SplitSide {
    /// The old file.
    Left,
    /// The new file.
    #[default]
    Right,
}

impl SplitSide {
    pub fn other(self) -> SplitSide {
        match self {
            SplitSide::Left => SplitSide::Right,
            SplitSide::Right => SplitSide::Left,
        }
    }
}

/// One line of the split view: the unified row shown on each side. Hunk headers, context
/// rows and a "\ No newline" marker after a context row are the same row on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLine {
    left: u32,
    right: u32,
}

impl SplitLine {
    fn new(left: Option<usize>, right: Option<usize>) -> SplitLine {
        let enc = |r: Option<usize>| r.map_or(PAD, |r| r as u32);
        SplitLine {
            left: enc(left),
            right: enc(right),
        }
    }

    pub fn left(self) -> Option<usize> {
        (self.left != PAD).then_some(self.left as usize)
    }

    pub fn right(self) -> Option<usize> {
        (self.right != PAD).then_some(self.right as usize)
    }

    pub fn side(self, side: SplitSide) -> Option<usize> {
        match side {
            SplitSide::Left => self.left(),
            SplitSide::Right => self.right(),
        }
    }

    /// One row on both sides (hunk header, context).
    pub fn shared(self) -> bool {
        self.left == self.right && self.left != PAD
    }

    /// The first row of the line, left side first (for "which row is at the top").
    pub fn any(self) -> usize {
        self.left().or(self.right()).unwrap_or(0)
    }
}

/// A file's rows as split lines, and the split line of every row. O(rows) once per patch, on
/// the service worker (see `PatchLayout`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SplitMap {
    pub lines: Vec<SplitLine>,
    /// Per unified row: its split line.
    pub line_of: Vec<u32>,
}

impl SplitMap {
    pub fn line_of(&self, row: usize) -> usize {
        self.line_of.get(row).map_or(0, |&l| l as usize)
    }

    /// Accounted size in the patch cache.
    pub fn bytes(&self) -> usize {
        self.lines.len() * std::mem::size_of::<SplitLine>()
            + self.line_of.len() * std::mem::size_of::<u32>()
    }

    /// The side row `row` is shown on (a shared row is on both: `prefer` decides).
    pub fn side_of(&self, row: usize, prefer: SplitSide) -> SplitSide {
        match self.lines.get(self.line_of(row)) {
            Some(l) if l.shared() => prefer,
            Some(l) if l.left() == Some(row) => SplitSide::Left,
            _ => SplitSide::Right,
        }
    }
}

/// Pairs `rows` into split lines: a run of N deleted rows followed by a run of M added rows
/// lines up row by row on max(N, M) lines, the shorter side padded. A "\ No newline" marker
/// stays on the side of the row it follows (both sides after a context row). Like the word
/// diff's pairing, a Del run pairs only with the Add run right after it.
pub fn pair_lines(rows: &[Row]) -> SplitMap {
    let n = rows.len();
    let mut lines = Vec::with_capacity(n);
    let mut line_of = vec![0u32; n];
    // a "\ No newline" row belongs to the side of the row before it
    let run_end = |mut i: usize, kind: RowKind| {
        while i < n
            && (rows[i].kind == kind
                || (rows[i].kind == RowKind::NoNewline && i > 0 && rows[i - 1].kind == kind))
        {
            i += 1;
        }
        i
    };
    let mut i = 0;
    while i < n {
        match rows[i].kind {
            RowKind::Del | RowKind::Add => {
                let (d0, d1) = if rows[i].kind == RowKind::Del {
                    (i, run_end(i, RowKind::Del))
                } else {
                    (i, i)
                };
                let (a0, a1) = (d1, run_end(d1, RowKind::Add));
                let (nd, na) = (d1 - d0, a1 - a0);
                for k in 0..nd.max(na) {
                    let l = lines.len() as u32;
                    let left = (k < nd).then_some(d0 + k);
                    let right = (k < na).then_some(a0 + k);
                    for r in [left, right].into_iter().flatten() {
                        line_of[r] = l;
                    }
                    lines.push(SplitLine::new(left, right));
                }
                i = a1;
            }
            _ => {
                line_of[i] = lines.len() as u32;
                lines.push(SplitLine::new(Some(i), Some(i)));
                i += 1;
            }
        }
    }
    SplitMap { lines, line_of }
}

/// What a comment on `side` of `line` is anchored to: the old file's line on the left, the
/// new file's on the right. None for padding, hunk headers and "\ No newline" rows.
pub fn comment_target(rows: &[Row], line: SplitLine, side: SplitSide) -> Option<(Side, u32)> {
    let r = rows.get(line.side(side)?)?;
    match (r.kind, side) {
        (RowKind::Del | RowKind::Context, SplitSide::Left) => Some((Side::Old, r.old_line?)),
        (RowKind::Add | RowKind::Context, SplitSide::Right) => Some((Side::New, r.new_line?)),
        _ => None,
    }
}

/// The extra height (comment cards) of each split line, from the unified rows' extras: the
/// cards of both rows of a line stack under it, left row's first.
pub fn split_extras(
    map: &SplitMap,
    extras: impl IntoIterator<Item = (usize, f32)>,
) -> Vec<(usize, f32)> {
    let mut out: Vec<(usize, f32)> = extras
        .into_iter()
        .filter(|&(r, _)| r < map.line_of.len())
        .map(|(r, px)| (map.line_of(r), px))
        .collect();
    out.sort_unstable_by_key(|&(l, _)| l);
    out.dedup_by(|b, a| {
        let same = a.0 == b.0;
        if same {
            a.1 += b.1;
        }
        same
    });
    out
}

/// The text a selection on one `side` of the split view copies: that side's rows only (the
/// other side's text is never mixed in, padding is skipped), joined with '\n', following the
/// unified copy's rules for hunk headers and "\ No newline" rows. `sel` positions are split
/// lines.
pub fn selected_text_split(
    rows: &[Row],
    map: &SplitMap,
    side: SplitSide,
    sel: &Selection,
) -> String {
    let (lo, hi) = sel.range();
    if sel.is_empty() || lo.row >= map.lines.len() {
        return String::new();
    }
    let mut out = String::new();
    let mut first = true;
    let end = (hi.row + 1).min(map.lines.len());
    for (l, line) in map.lines[..end].iter().enumerate().skip(lo.row) {
        let Some(r) = line.side(side).and_then(|i| rows.get(i)) else {
            continue;
        };
        let Some(span) = row_span(r, l, lo, hi) else {
            continue;
        };
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(&slice_cols(&r.text, span.start, span.end));
    }
    out
}

#[cfg(test)]
mod tests;
