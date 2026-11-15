//! Text selection in the diff: positions in display columns (tabs expanded, double-width
//! characters count 2), mapping between columns and x, the text a selection copies, word
//! picking and drag autoscroll. Pure; the pointer and painting code lives in `review::ui`.

use std::ops::Range;

use prcore::diff::{Row, RowKind};
use prcore::draft::Side;

use super::columns::{clusters, display_cols, Cluster};
use super::diffview::{Offsets, ROW_H};
use super::split::SplitSide;

/// A text position: a row and a display column in its expanded text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

/// Where a drag started (`anchor`) and where it is now (`head`); either may come first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Pos,
    pub head: Pos,
}

impl Selection {
    /// (start, end) in document order, whichever way it was dragged.
    pub fn range(&self) -> (Pos, Pos) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Every row; the end column is `usize::MAX` ("to the end"), so nothing per row is built.
    pub fn all(rows: usize) -> Option<Selection> {
        (rows > 0).then(|| Selection {
            anchor: Pos { row: 0, col: 0 },
            head: Pos {
                row: rows - 1,
                col: usize::MAX,
            },
        })
    }
}

/// x (from the start of the text) of display column `col`, snapped back to the start of the
/// character it falls in and clamped to the text's end.
pub fn col_to_x(text: &str, col: usize, char_w: f32) -> f32 {
    let mut at = 0;
    for c in clusters(text) {
        if c.col0 > col {
            break;
        }
        at = if col >= c.col1 { c.col1 } else { c.col0 };
    }
    at as f32 * char_w
}

/// The display column nearest to `x`, always on a character boundary (never inside a tab,
/// a double-width character or a base character with its combining marks).
pub fn x_to_col(text: &str, x: f32, char_w: f32) -> usize {
    let f = x / char_w;
    if f <= 0.0 || f.is_nan() {
        return 0;
    }
    let mut end = 0;
    for c in clusters(text) {
        end = c.col1;
        if f < c.col1 as f32 {
            let mid = c.col0 as f32 + (c.col1 - c.col0) as f32 / 2.0;
            return if f < mid { c.col0 } else { c.col1 };
        }
    }
    end
}

/// The text of display columns `[c0, c1)` as it is in the file (tabs stay tabs, CR is
/// dropped). A character cut by the range is included whole.
pub fn slice_cols(text: &str, c0: usize, c1: usize) -> String {
    if c0 >= c1 {
        return String::new();
    }
    let mut out = String::new();
    let keep = |s: &str, out: &mut String| out.extend(s.chars().filter(|&c| c != '\r'));
    if c0 == 0 && c1 == usize::MAX {
        keep(text, &mut out);
        return out;
    }
    for c in clusters(text) {
        if c.col0 >= c1 {
            break;
        }
        let inside = if c.col0 == c.col1 {
            c.col0 >= c0
        } else {
            c.col1 > c0
        };
        if inside {
            keep(&text[c.b0..c.b1], &mut out);
        }
    }
    out
}

#[derive(PartialEq, Clone, Copy)]
enum Class {
    Word,
    Space,
    /// Symbols stand alone.
    Other,
}

fn class(ch: char) -> Class {
    if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else if ch == '\t' || ch.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// The word (or run of blanks, or single symbol) under display column `col`, as columns.
pub fn word_at(text: &str, col: usize) -> Range<usize> {
    let cl: Vec<Cluster> = clusters(text).collect();
    let Some(last) = cl.last() else {
        return 0..0;
    };
    let Some(i) = cl.iter().position(|c| c.col0 <= col && col < c.col1) else {
        return last.col1..last.col1;
    };
    let kind = class(cl[i].base);
    if kind == Class::Other {
        return cl[i].col0..cl[i].col1;
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && class(cl[a - 1].base) == kind {
        a -= 1;
    }
    while b + 1 < cl.len() && class(cl[b + 1].base) == kind {
        b += 1;
    }
    cl[a].col0..cl[b].col1
}

/// What row `i` contributes to the selection `lo..=hi`, as display columns (end may be
/// `usize::MAX`): None for rows outside it, "\ No newline" rows and hunk headers that are
/// only partly inside.
pub fn row_span(row: &Row, i: usize, lo: Pos, hi: Pos) -> Option<Range<usize>> {
    if i < lo.row || i > hi.row {
        return None;
    }
    let c0 = if i == lo.row { lo.col } else { 0 };
    let c1 = if i == hi.row { hi.col } else { usize::MAX };
    match row.kind {
        RowKind::NoNewline => None,
        RowKind::HunkHeader => (c0 == 0 && (c1 == usize::MAX || c1 >= display_cols(&row.text)))
            .then_some(0..usize::MAX),
        _ => Some(c0..c1),
    }
}

/// The copied text: the rows' text joined with '\n', without gutters or +/- markers.
/// Comment cards are not rows, so they are never part of it. Built only when copying.
pub fn selected_text(rows: &[Row], sel: &Selection) -> String {
    let (lo, hi) = sel.range();
    if sel.is_empty() || lo.row >= rows.len() {
        return String::new();
    }
    let mut out = String::new();
    let mut first = true;
    for (i, row) in rows.iter().enumerate().take(hi.row + 1).skip(lo.row) {
        let Some(span) = row_span(row, i, lo, hi) else {
            continue;
        };
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(&slice_cols(&row.text, span.start, span.end));
    }
    out
}

/// The lines a selection over several code rows comments on: (first, last), each as (side,
/// line), new side for added and context rows, old side for deleted rows. None when it
/// covers fewer than two code rows or crosses a hunk header (a range stays in one hunk).
pub fn line_range(rows: &[Row], sel: &Selection) -> Option<((Side, u32), (Side, u32))> {
    let (lo, hi) = sel.range();
    let mut code = Vec::new();
    for r in rows.iter().take(hi.row + 1).skip(lo.row) {
        match r.kind {
            RowKind::HunkHeader if !code.is_empty() => return None,
            RowKind::Add | RowKind::Context => code.push((Side::New, r.new_line?)),
            RowKind::Del => code.push((Side::Old, r.old_line?)),
            RowKind::HunkHeader | RowKind::NoNewline => {}
        }
    }
    match code[..] {
        [first, .., last] => Some((first, last)),
        _ => None,
    }
}

const EDGE: f32 = 16.0;
const AUTOSCROLL_BASE: f32 = 120.0;
const AUTOSCROLL_PER_PX: f32 = 15.0;
const AUTOSCROLL_MAX: f32 = 2400.0;

/// Pixels to scroll this frame while dragging with the pointer at `pointer_y` and the
/// viewport spanning `[top, bottom]`: negative up, positive down, 0 away from the edges.
/// The speed grows with how far the pointer is past (or into) the edge band, and is capped.
pub fn autoscroll_delta(pointer_y: f32, top: f32, bottom: f32, dt: f32) -> f32 {
    let (past, dir) = if pointer_y < top + EDGE {
        (top + EDGE - pointer_y, -1.0)
    } else if pointer_y > bottom - EDGE {
        (pointer_y - (bottom - EDGE), 1.0)
    } else {
        return 0.0;
    };
    dir * (AUTOSCROLL_BASE + AUTOSCROLL_PER_PX * past).min(AUTOSCROLL_MAX) * dt.max(0.0)
}

/// What the pointer is over in the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// The line-number gutter (and sign column) of a row: reserved for the comment composer.
    Gutter(usize),
    /// The text of a row.
    Text(usize),
    /// The space below a row that holds its comment cards.
    Card(usize),
}

/// `x` is measured from the left edge of the diff pane (gutters at 0, not scrolled), `y` from
/// the top of the content; `text_x` is where the text column starts.
pub fn hit_test(offsets: &Offsets, x: f32, y: f32, text_x: f32) -> Option<Hit> {
    let row = offsets.row_at(y)?;
    Some(if y - offsets.top(row) >= ROW_H && y < offsets.total() {
        Hit::Card(row)
    } else if x < text_x {
        Hit::Gutter(row)
    } else {
        Hit::Text(row)
    })
}

/// What a mouse press at a hit does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// Starts (or extends) a text selection.
    Begin(usize),
    /// A gutter press: not ours, the comment composer takes it.
    GutterClick(usize),
    /// Comment cards and empty space.
    Ignore,
}

pub fn press_action(hit: Option<Hit>) -> Press {
    match hit {
        Some(Hit::Text(row)) => Press::Begin(row),
        Some(Hit::Gutter(row)) => Press::GutterClick(row),
        _ => Press::Ignore,
    }
}

/// Selection and drag state of the current file.
#[derive(Debug, Default)]
pub struct SelectionState {
    pub sel: Option<Selection>,
    pub dragging: bool,
    /// The split view side the selection is on (its rows are split lines then); None in the
    /// unified view. A selection never spans both sides.
    pub side: Option<SplitSide>,
}

impl SelectionState {
    /// Mouse press on text at `at`: starts a selection there, or with `extend` moves the head
    /// of the existing one.
    pub fn begin(&mut self, at: Pos, extend: bool) {
        self.dragging = true;
        match self.sel.as_mut() {
            Some(s) if extend => s.head = at,
            _ => {
                self.sel = Some(Selection {
                    anchor: at,
                    head: at,
                })
            }
        }
    }

    pub fn drag_to(&mut self, at: Pos) {
        if let Some(s) = self.sel.as_mut() {
            s.head = at;
        }
    }

    pub fn end_drag(&mut self) {
        self.dragging = false;
    }

    /// A click without drag: shift extends, otherwise it clears.
    pub fn click(&mut self, at: Pos, extend: bool) {
        match self.sel.as_mut() {
            Some(s) if extend => s.head = at,
            _ => self.sel = None,
        }
    }

    /// Mouse press on `side` (None: unified) at `at`: a press on the other side than the
    /// current selection's starts a new one there (sides are never mixed).
    pub fn begin_on(&mut self, at: Pos, extend: bool, side: Option<SplitSide>) {
        let extend = extend && self.side == side;
        self.side = side;
        self.begin(at, extend);
    }

    /// A click without drag on `side`: as `click`, never extending across sides.
    pub fn click_on(&mut self, at: Pos, extend: bool, side: Option<SplitSide>) {
        self.click(at, extend && self.side == side);
    }

    #[cfg(test)]
    pub fn select_word(&mut self, rows: &[Row], at: Pos) {
        let Some(r) = rows.get(at.row) else { return };
        self.select_word_in(&r.text, at);
    }

    /// The word under `at` in `text` (the text of row `at.row` on the selection's side).
    pub fn select_word_in(&mut self, text: &str, at: Pos) {
        let w = word_at(text, at.col);
        self.sel = Some(Selection {
            anchor: Pos {
                row: at.row,
                col: w.start,
            },
            head: Pos {
                row: at.row,
                col: w.end,
            },
        });
    }

    #[cfg(test)]
    pub fn select_row(&mut self, rows: &[Row], row: usize) {
        let Some(r) = rows.get(row) else { return };
        self.select_row_in(&r.text, row);
    }

    /// Row `row` whole, `text` being its text on the selection's side.
    pub fn select_row_in(&mut self, text: &str, row: usize) {
        self.sel = Some(Selection {
            anchor: Pos { row, col: 0 },
            head: Pos {
                row,
                col: display_cols(text),
            },
        });
    }

    /// Whole rows from `anchor` to `head` (visual line mode), on `side`.
    pub fn select_lines(&mut self, anchor: usize, head: usize, side: Option<SplitSide>) {
        let (a, h) = if head >= anchor {
            (0, usize::MAX)
        } else {
            (usize::MAX, 0)
        };
        self.side = side;
        self.dragging = false;
        self.sel = Some(Selection {
            anchor: Pos {
                row: anchor,
                col: a,
            },
            head: Pos { row: head, col: h },
        });
    }

    pub fn clear(&mut self) {
        self.sel = None;
        self.dragging = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 8.0;

    fn row(kind: RowKind, text: &str) -> Row {
        Row {
            kind,
            old_line: None,
            new_line: None,
            text: text.into(),
        }
    }

    fn p(row: usize, col: usize) -> Pos {
        Pos { row, col }
    }

    #[test]
    fn a_selection_over_several_lines_comments_on_its_first_and_last_line() {
        let r = |kind, old, new| Row {
            kind,
            old_line: old,
            new_line: new,
            text: "x".into(),
        };
        let rows = vec![
            r(RowKind::HunkHeader, None, None),
            r(RowKind::Context, Some(4), Some(4)),
            r(RowKind::Del, Some(5), None),
            r(RowKind::Add, None, Some(5)),
            r(RowKind::Context, Some(6), Some(6)),
            r(RowKind::HunkHeader, None, None),
            r(RowKind::Context, Some(20), Some(21)),
        ];
        let sel = |a: Pos, h: Pos| Selection { anchor: a, head: h };
        // dragged upwards from the context line to the deleted one: old 5 to new 6
        assert_eq!(
            line_range(&rows, &sel(p(4, 2), p(2, 0))),
            Some(((Side::Old, 5), (Side::New, 6)))
        );
        // from the hunk header on: its first code line
        assert_eq!(
            line_range(&rows, &sel(p(0, 0), p(3, 1))),
            Some(((Side::New, 4), (Side::New, 5)))
        );
        // one line, or across two hunks: no range
        assert_eq!(line_range(&rows, &sel(p(3, 0), p(3, 9))), None);
        assert_eq!(line_range(&rows, &sel(p(4, 0), p(6, 1))), None);
    }

    #[test]
    fn x_col_roundtrip_with_tabs_and_wide_chars() {
        // columns: a0 \t1-3 日4-5 b6 e7(+U+0301 mark) c8
        let t = "a\t日be\u{301}c";
        assert_eq!(display_cols(t), 9);
        for col in [0, 1, 4, 6, 7, 8, 9] {
            assert_eq!(x_to_col(t, col_to_x(t, col, W), W), col, "col {col}");
        }
        // inside the tab / the wide char snaps to a boundary, never in the middle
        assert_eq!(x_to_col(t, 2.2 * W, W), 1);
        assert_eq!(x_to_col(t, 2.8 * W, W), 4);
        assert_eq!(x_to_col(t, 4.9 * W, W), 4, "left half of the wide char");
        assert_eq!(x_to_col(t, 5.1 * W, W), 6, "right half");
        assert_eq!(col_to_x(t, 5, W), 4.0 * W, "mid-char column snaps back");
        assert_eq!(x_to_col(t, -10.0, W), 0);
        assert_eq!(x_to_col(t, 1e6, W), 9);
        assert_eq!(col_to_x(t, usize::MAX, W), 9.0 * W);
        // a tab after wide chars lands on the next multiple of 4
        assert_eq!(display_cols("日日\tx"), 9);
        assert_eq!(display_cols("ab\r"), 2);
    }

    #[test]
    fn selection_text_across_rows_excludes_markers() {
        let rows = vec![
            row(RowKind::Context, "let a = 1;"),
            row(RowKind::Del, "let b = 2;"),
            row(RowKind::Add, "let b = 3;"),
            row(RowKind::Context, "end"),
        ];
        let s = Selection {
            anchor: p(0, 4),
            head: p(3, 2),
        };
        assert_eq!(
            selected_text(&rows, &s),
            "a = 1;\nlet b = 2;\nlet b = 3;\nen"
        );
        let one = Selection {
            anchor: p(1, 4),
            head: p(1, 5),
        };
        assert_eq!(selected_text(&rows, &one), "b");
        let none = Selection {
            anchor: p(1, 4),
            head: p(1, 4),
        };
        assert_eq!(selected_text(&rows, &none), "");
        // tabs are copied as tabs, CR dropped
        let tabs = vec![row(RowKind::Add, "\tx\r")];
        let all = Selection::all(1).unwrap();
        assert_eq!(selected_text(&tabs, &all), "\tx");
    }

    #[test]
    fn reversed_drag_normalizes() {
        let fwd = Selection {
            anchor: p(1, 2),
            head: p(4, 7),
        };
        let back = Selection {
            anchor: p(4, 7),
            head: p(1, 2),
        };
        assert_eq!(fwd.range(), (p(1, 2), p(4, 7)));
        assert_eq!(back.range(), fwd.range());
        let same_row = Selection {
            anchor: p(2, 9),
            head: p(2, 3),
        };
        assert_eq!(same_row.range(), (p(2, 3), p(2, 9)));
        let rows: Vec<Row> = (0..5)
            .map(|i| row(RowKind::Add, &format!("line{i}")))
            .collect();
        assert_eq!(selected_text(&rows, &fwd), selected_text(&rows, &back));
    }

    #[test]
    fn word_at_pos_unicode() {
        let t = "foo_bar  baz->x";
        assert_eq!(word_at(t, 2), 0..7);
        assert_eq!(word_at(t, 8), 7..9, "run of blanks");
        assert_eq!(word_at(t, 11), 9..12);
        assert_eq!(word_at(t, 12), 12..13, "symbols one by one");
        assert_eq!(word_at(t, 99), 15..15, "past the end: empty at the end");
        // combining mark stays with its base: "he\u{301}llo wörld" columns h0 e1 l2 l3 o4
        let c = "he\u{301}llo w";
        assert_eq!(word_at(c, 1), 0..5);
        // emoji (2 columns) and an emoji sequence with a joiner
        let e = "a 😀 b";
        assert_eq!(word_at(e, 2), 2..4);
        assert_eq!(word_at(e, 3), 2..4, "second half of the emoji");
        let fam = "x 👨\u{200d}👩 y";
        assert_eq!(word_at(fam, 3), 2..4, "joined emoji is one cluster");
        // CJK ideographs are word characters: a run selects together (日本語 = cols 0..6)
        let k = "日本語 test";
        assert_eq!(word_at(k, 1), 0..6);
        assert_eq!(word_at(k, 6), 6..7);
        assert_eq!(word_at(k, 8), 7..11);
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn autoscroll_speed_grows_with_distance() {
        let (top, bottom, dt) = (100.0, 700.0, 0.016);
        assert_eq!(autoscroll_delta(400.0, top, bottom, dt), 0.0);
        let near_top = autoscroll_delta(top + 5.0, top, bottom, dt);
        let at_top = autoscroll_delta(top, top, bottom, dt);
        let above = autoscroll_delta(top - 50.0, top, bottom, dt);
        let far = autoscroll_delta(top - 5000.0, top, bottom, dt);
        assert!(
            near_top < 0.0 && at_top < near_top && above < at_top,
            "{near_top} {at_top} {above}"
        );
        let cap = autoscroll_delta(top - 1e6, top, bottom, dt);
        assert_eq!(far, cap, "capped");
        assert!(cap > -3000.0 * dt && cap < 0.0);
        let below = autoscroll_delta(bottom + 50.0, top, bottom, dt);
        assert!(below > 0.0 && (below + above).abs() < 1e-3, "symmetric");
        assert_eq!(autoscroll_delta(top - 50.0, top, bottom, 0.0), 0.0);
    }

    #[test]
    fn copy_text_follows_hunk_header_and_no_newline_rules() {
        let rows = vec![
            row(RowKind::HunkHeader, "@@ -1,2 +1,2 @@"),
            row(RowKind::Context, "one"),
            row(RowKind::Del, "two"),
            row(RowKind::NoNewline, "\\ No newline at end of file"),
            row(RowKind::Add, "three"),
            row(RowKind::HunkHeader, "@@ -9 +9 @@"),
            row(RowKind::Context, "nine"),
        ];
        // header fully inside (from its column 0) is copied; no-newline rows are skipped
        let all = Selection::all(rows.len()).unwrap();
        assert_eq!(
            selected_text(&rows, &all),
            "@@ -1,2 +1,2 @@\none\ntwo\nthree\n@@ -9 +9 @@\nnine"
        );
        // header only partly selected at its start: skipped
        let part = Selection {
            anchor: p(0, 3),
            head: p(2, 3),
        };
        assert_eq!(selected_text(&rows, &part), "one\ntwo");
        // header cut at its end: skipped; fully covered at the end row: copied
        let cut = Selection {
            anchor: p(4, 0),
            head: p(5, 4),
        };
        assert_eq!(selected_text(&rows, &cut), "three");
        let full = Selection {
            anchor: p(4, 0),
            head: p(5, 11),
        };
        assert_eq!(selected_text(&rows, &full), "three\n@@ -9 +9 @@");
        // a drag that only touches the no-newline row copies nothing
        let nn = Selection {
            anchor: p(3, 0),
            head: p(3, 5),
        };
        assert_eq!(selected_text(&rows, &nn), "");
        // comment cards are not rows (they live in a row's extra height, see `hit_test`), so
        // the text can only come from rows
        assert_eq!(row_span(&rows[3], 3, p(0, 0), p(6, 0)), None);
        assert_eq!(row_span(&rows[1], 1, p(2, 0), p(6, 0)), None);
    }

    #[test]
    fn select_all_text_large_file() {
        let rows: Vec<Row> = (0..100_000)
            .map(|i| {
                let k = match i % 4 {
                    0 => RowKind::Context,
                    1 => RowKind::Add,
                    2 => RowKind::Del,
                    _ => RowKind::HunkHeader,
                };
                row(k, &format!("\tline number {i} with some text 日本"))
            })
            .collect();
        let t0 = std::time::Instant::now();
        let text = selected_text(&rows, &Selection::all(rows.len()).unwrap());
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(text.lines().count(), 100_000);
        assert!(text.starts_with("\tline number 0 "));
        assert!(Selection::all(0).is_none());
    }

    #[test]
    fn gutter_click_does_not_start_selection() {
        let mut offsets = Offsets::new(10, ROW_H);
        offsets.set_extra_height(2, 40.0);
        let text_x = 100.0;
        assert_eq!(
            hit_test(&offsets, 10.0, 3.0 * ROW_H + 40.0 + 2.0, text_x),
            Some(Hit::Gutter(3))
        );
        assert_eq!(
            hit_test(&offsets, text_x - 0.1, 5.0, text_x),
            Some(Hit::Gutter(0))
        );
        assert_eq!(hit_test(&offsets, text_x, 5.0, text_x), Some(Hit::Text(0)));
        assert_eq!(
            hit_test(&offsets, 400.0, 2.0 * ROW_H + ROW_H + 10.0, text_x),
            Some(Hit::Card(2))
        );
        assert_eq!(
            hit_test(&offsets, 400.0, 1e9, text_x),
            Some(Hit::Text(9)),
            "clamped"
        );
        assert_eq!(hit_test(&Offsets::new(0, ROW_H), 0.0, 0.0, text_x), None);
        // only a text hit may begin a selection; the gutter is handed to the composer
        let at = |x, y| press_action(hit_test(&offsets, x, y, text_x));
        assert_eq!(at(10.0, 5.0), Press::GutterClick(0));
        assert_eq!(at(400.0, 5.0), Press::Begin(0));
        assert_eq!(at(400.0, 3.0 * ROW_H + 40.0 + 2.0), Press::Begin(3));
        assert_eq!(
            at(400.0, 2.0 * ROW_H + ROW_H + 10.0),
            Press::Ignore,
            "card area"
        );
        assert_eq!(press_action(None), Press::Ignore);
    }

    #[test]
    fn a_press_on_the_other_side_starts_a_new_selection_there() {
        let mut st = SelectionState::default();
        st.begin_on(p(1, 0), false, Some(SplitSide::Left));
        st.drag_to(p(3, 2));
        st.end_drag();
        // shift-press on the right side: not an extension across sides
        st.begin_on(p(5, 1), true, Some(SplitSide::Right));
        assert_eq!(st.side, Some(SplitSide::Right));
        assert_eq!(st.sel.unwrap().anchor, p(5, 1));
        // shift-press on the same side extends
        st.begin_on(p(7, 0), true, Some(SplitSide::Right));
        assert_eq!(st.sel.unwrap().anchor, p(5, 1));
        assert_eq!(st.sel.unwrap().head, p(7, 0));
        st.click_on(p(2, 0), true, Some(SplitSide::Left));
        assert!(st.sel.is_none(), "a shift-click on the other side clears");
    }

    #[test]
    fn visual_lines_cover_whole_rows_in_either_direction() {
        let rows: Vec<Row> = (0..5)
            .map(|i| row(RowKind::Add, &format!("r{i}")))
            .collect();
        let mut st = SelectionState::default();
        st.select_lines(1, 3, None);
        assert_eq!(selected_text(&rows, &st.sel.unwrap()), "r1\nr2\nr3");
        st.select_lines(3, 1, None);
        assert_eq!(selected_text(&rows, &st.sel.unwrap()), "r1\nr2\nr3");
        st.select_lines(2, 2, Some(SplitSide::Left));
        assert_eq!(selected_text(&rows, &st.sel.unwrap()), "r2");
        assert_eq!(st.side, Some(SplitSide::Left));
    }

    #[test]
    fn selection_starting_inside_an_expanded_tab() {
        // "a\tb": a0 tab1-3 b4; column 2 is inside the tab, which is copied whole
        let rows = vec![row(RowKind::Add, "a\tb")];
        let s = Selection {
            anchor: p(0, 2),
            head: p(0, 5),
        };
        assert_eq!(selected_text(&rows, &s), "\tb");
        assert_eq!(slice_cols("a\tb", 4, 5), "b");
        assert_eq!(slice_cols("a\tb", 3, 4), "\t", "a tab's last column");
        let s = Selection {
            anchor: p(0, 0),
            head: p(0, 2),
        };
        assert_eq!(selected_text(&rows, &s), "a\t");
    }

    #[test]
    fn click_extend_word_and_row_selection() {
        let rows = vec![row(RowKind::Add, "foo bar"), row(RowKind::Add, "baz")];
        let mut st = SelectionState::default();
        st.begin(p(0, 1), false);
        assert!(st.dragging);
        st.drag_to(p(1, 2));
        assert_eq!(
            st.sel,
            Some(Selection {
                anchor: p(0, 1),
                head: p(1, 2)
            })
        );
        st.end_drag();
        st.click(p(1, 3), true);
        assert_eq!(st.sel.unwrap().head, p(1, 3), "shift-click moves the head");
        assert_eq!(st.sel.unwrap().anchor, p(0, 1));
        st.click(p(0, 0), false);
        assert!(st.sel.is_none());
        st.select_word(&rows, p(0, 5));
        assert_eq!(selected_text(&rows, &st.sel.unwrap()), "bar");
        st.select_row(&rows, 1);
        assert_eq!(selected_text(&rows, &st.sel.unwrap()), "baz");
        st.begin(p(1, 0), false);
        st.begin(p(0, 0), true);
        assert_eq!(
            st.sel.unwrap().anchor,
            p(1, 0),
            "shift-press keeps the anchor"
        );
        st.clear();
        assert!(st.sel.is_none());
    }
}
