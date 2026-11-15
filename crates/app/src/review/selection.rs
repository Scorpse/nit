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
mod tests;
