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
mod tests {
    use super::*;
    use crate::review::selection::Pos;

    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> Row {
        Row {
            kind,
            old_line: old,
            new_line: new,
            text: text.into(),
        }
    }

    fn del(n: u32, t: &str) -> Row {
        row(RowKind::Del, Some(n), None, t)
    }
    fn add(n: u32, t: &str) -> Row {
        row(RowKind::Add, None, Some(n), t)
    }
    fn ctx(o: u32, n: u32, t: &str) -> Row {
        row(RowKind::Context, Some(o), Some(n), t)
    }
    fn hunk(t: &str) -> Row {
        row(RowKind::HunkHeader, None, None, t)
    }
    fn nonl() -> Row {
        row(
            RowKind::NoNewline,
            None,
            None,
            "\\ No newline at end of file",
        )
    }

    /// Lines as (left, right) row indices.
    fn pairs(rows: &[Row]) -> Vec<(Option<usize>, Option<usize>)> {
        pair_lines(rows)
            .lines
            .iter()
            .map(|l| (l.left(), l.right()))
            .collect()
    }

    fn check_line_of(rows: &[Row]) {
        let m = pair_lines(rows);
        assert_eq!(m.line_of.len(), rows.len());
        for (r, &l) in m.line_of.iter().enumerate() {
            let line = m.lines[l as usize];
            assert!(
                line.left() == Some(r) || line.right() == Some(r),
                "row {r} is on its line {l}"
            );
        }
        // every row appears exactly once per side it is on
        let shown: usize = m
            .lines
            .iter()
            .map(|l| {
                if l.shared() {
                    1
                } else {
                    usize::from(l.left().is_some()) + usize::from(l.right().is_some())
                }
            })
            .sum();
        assert_eq!(shown, rows.len());
    }

    #[test]
    fn context_and_headers_are_on_both_sides() {
        let rows = vec![hunk("@@ -1,2 +1,2 @@"), ctx(1, 1, "a"), ctx(2, 2, "b")];
        assert_eq!(
            pairs(&rows),
            [(Some(0), Some(0)), (Some(1), Some(1)), (Some(2), Some(2))]
        );
        let m = pair_lines(&rows);
        assert!(m.lines.iter().all(|l| l.shared()));
        check_line_of(&rows);
    }

    #[test]
    fn unequal_runs_line_up_row_by_row_and_pad_the_shorter_side() {
        // 3 deletions, 1 addition, then 1 deletion, 3 additions
        let rows = vec![
            ctx(1, 1, "x"),
            del(2, "a"),
            del(3, "b"),
            del(4, "c"),
            add(2, "A"),
            ctx(5, 3, "y"),
            del(6, "d"),
            add(4, "D"),
            add(5, "E"),
            add(6, "F"),
        ];
        assert_eq!(
            pairs(&rows),
            [
                (Some(0), Some(0)),
                (Some(1), Some(4)),
                (Some(2), None),
                (Some(3), None),
                (Some(5), Some(5)),
                (Some(6), Some(7)),
                (None, Some(8)),
                (None, Some(9)),
            ]
        );
        check_line_of(&rows);
        let m = pair_lines(&rows);
        assert_eq!(
            m.line_of(4),
            1,
            "the addition sits on the first deletion's line"
        );
        assert_eq!(m.line_of(9), 7);
    }

    #[test]
    fn only_additions_or_only_deletions() {
        let adds = vec![hunk("@@ -0,0 +1,2 @@"), add(1, "a"), add(2, "b")];
        assert_eq!(
            pairs(&adds),
            [(Some(0), Some(0)), (None, Some(1)), (None, Some(2))]
        );
        check_line_of(&adds);
        let dels = vec![hunk("@@ -1,2 +0,0 @@"), del(1, "a"), del(2, "b")];
        assert_eq!(
            pairs(&dels),
            [(Some(0), Some(0)), (Some(1), None), (Some(2), None)]
        );
        check_line_of(&dels);
        assert!(pair_lines(&[]).lines.is_empty());
    }

    #[test]
    fn an_add_run_before_a_del_run_does_not_pair_with_it() {
        // the word diff pairs a Del run only with the Add run after it; so does the split
        let rows = vec![add(1, "A"), del(1, "a"), add(2, "B")];
        assert_eq!(pairs(&rows), [(None, Some(0)), (Some(1), Some(2))]);
        check_line_of(&rows);
    }

    #[test]
    fn no_newline_markers_stay_on_their_side() {
        // both ends lack a newline: the markers line up
        let both = vec![del(1, "old"), nonl(), add(1, "new"), nonl()];
        assert_eq!(pairs(&both), [(Some(0), Some(2)), (Some(1), Some(3))]);
        check_line_of(&both);
        // only the old file lacked it
        let old = vec![del(1, "old"), nonl(), add(1, "new")];
        assert_eq!(pairs(&old), [(Some(0), Some(2)), (Some(1), None)]);
        check_line_of(&old);
        // only the new file lacks it
        let new = vec![del(1, "old"), add(1, "new"), nonl()];
        assert_eq!(pairs(&new), [(Some(0), Some(1)), (None, Some(2))]);
        check_line_of(&new);
        // after a context row it is on both sides
        let c = vec![ctx(1, 1, "end"), nonl()];
        assert_eq!(pairs(&c), [(Some(0), Some(0)), (Some(1), Some(1))]);
        check_line_of(&c);
    }

    #[test]
    fn comment_target_is_the_old_line_on_the_left_and_the_new_line_on_the_right() {
        let rows = vec![
            hunk("@@ -10,3 +20,2 @@"),
            ctx(10, 20, "same"),
            del(11, "gone"),
            del(12, "also gone"),
            add(21, "new"),
            nonl(),
        ];
        let m = pair_lines(&rows);
        let t = |l: usize, s| comment_target(&rows, m.lines[l], s);
        assert_eq!(t(0, SplitSide::Left), None, "hunk header");
        assert_eq!(t(1, SplitSide::Left), Some((Side::Old, 10)));
        assert_eq!(t(1, SplitSide::Right), Some((Side::New, 20)));
        assert_eq!(t(2, SplitSide::Left), Some((Side::Old, 11)));
        assert_eq!(t(2, SplitSide::Right), Some((Side::New, 21)));
        assert_eq!(t(3, SplitSide::Left), Some((Side::Old, 12)));
        assert_eq!(t(3, SplitSide::Right), None, "the no-newline marker");
        assert_eq!(m.lines.len(), 4);
        let padded = vec![del(1, "a"), del(2, "b"), add(1, "A")];
        let p = pair_lines(&padded);
        assert_eq!(
            comment_target(&padded, p.lines[1], SplitSide::Right),
            None,
            "padding"
        );
        assert_eq!(
            comment_target(&padded, p.lines[1], SplitSide::Left),
            Some((Side::Old, 2))
        );
        // the side a row is shown on
        assert_eq!(m.side_of(2, SplitSide::Right), SplitSide::Left);
        assert_eq!(m.side_of(4, SplitSide::Left), SplitSide::Right);
        assert_eq!(
            m.side_of(1, SplitSide::Left),
            SplitSide::Left,
            "shared: preferred"
        );
        assert_eq!(m.side_of(1, SplitSide::Right), SplitSide::Right);
    }

    #[test]
    fn extras_of_both_rows_of_a_line_add_up() {
        let rows = vec![ctx(1, 1, "x"), del(2, "a"), add(2, "A"), ctx(3, 3, "y")];
        let m = pair_lines(&rows);
        let e = split_extras(&m, [(2, 30.0), (0, 10.0), (1, 5.0), (9, 99.0)]);
        assert_eq!(e, [(0, 10.0), (1, 35.0)]);
        assert!(split_extras(&m, []).is_empty());
    }

    #[test]
    fn a_selection_copies_one_side_only() {
        let rows = vec![
            hunk("@@ -1,3 +1,2 @@"),
            ctx(1, 1, "keep"),
            del(2, "old two"),
            del(3, "old three"),
            add(2, "new two"),
            nonl(),
        ];
        let m = pair_lines(&rows);
        let all = Selection::all(m.lines.len()).unwrap();
        assert_eq!(
            selected_text_split(&rows, &m, SplitSide::Left, &all),
            "@@ -1,3 +1,2 @@\nkeep\nold two\nold three"
        );
        assert_eq!(
            selected_text_split(&rows, &m, SplitSide::Right, &all),
            "@@ -1,3 +1,2 @@\nkeep\nnew two"
        );
        // columns apply to the side's own text
        let part = Selection {
            anchor: Pos { row: 2, col: 4 },
            head: Pos { row: 3, col: 3 },
        };
        assert_eq!(
            selected_text_split(&rows, &m, SplitSide::Left, &part),
            "two\nold"
        );
        assert_eq!(
            selected_text_split(&rows, &m, SplitSide::Right, &part),
            "two",
            "padding is skipped"
        );
        let empty = Selection {
            anchor: Pos { row: 1, col: 0 },
            head: Pos { row: 1, col: 0 },
        };
        assert_eq!(selected_text_split(&rows, &m, SplitSide::Left, &empty), "");
    }

    #[test]
    fn layout_is_stored_and_toggled_and_narrow_windows_fall_back() {
        for l in [DiffLayout::Unified, DiffLayout::Split] {
            assert_eq!(DiffLayout::parse(Some(l.as_str())), l);
            assert_eq!(l.toggled().toggled(), l);
        }
        assert_eq!(DiffLayout::parse(None), DiffLayout::Unified);
        assert_eq!(DiffLayout::parse(Some("weird")), DiffLayout::Unified);
        assert!(split_fits(1400.0, 1000.0));
        assert!(!split_fits(899.0, 1000.0), "narrow window");
        assert!(!split_fits(1400.0, 500.0), "narrow pane (wide file list)");
    }

    /// 200k rows (as the audit's large file) pair in a few milliseconds and stay compact.
    #[test]
    fn pairing_a_large_file_is_linear() {
        let rows: Vec<Row> = (0..200_000u32)
            .map(|i| match i % 5 {
                0 => ctx(i, i, "c"),
                1 | 2 => del(i, "d"),
                _ => add(i, "a"),
            })
            .collect();
        let t = std::time::Instant::now();
        let m = pair_lines(&rows);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(m.lines.len(), 120_000, "each 2 + 2 change is 2 lines");
        assert_eq!(m.bytes(), 120_000 * 8 + 200_000 * 4);
    }
}
