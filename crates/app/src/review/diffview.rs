//! Virtualized unified diff of one file: flat rows, prefix-sum y offsets with binary search,
//! variable row heights (inline comment threads), tab expansion and column clipping.
//! Pure state; drawing lives in `review::ui`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use eframe::egui::{Color32, Vec2};
use prcore::diff::{Row, RowKind};

use super::columns::{clusters, display_cols, Cluster};
use super::highlight::Spans;
use super::selection::SelectionState;
use super::split::{pair_lines, split_extras, SplitMap, SplitSide};
use super::worddiff::word_spans;

use super::comments_ui::CommentLayout;

pub const ROW_H: f32 = 18.0;
pub const TAB_WIDTH: usize = 4;

/// Row tops as a prefix sum: `y[i]` is the top of row `i`, `y[n]` the total height.
pub struct Offsets {
    y: Vec<f32>,
    row_h: f32,
    extra: HashMap<usize, f32>,
    /// Bumped whenever the extras change (views derived from these offsets rebuild then).
    gen: u64,
}

impl Offsets {
    pub fn new(rows: usize, row_h: f32) -> Offsets {
        Offsets {
            y: (0..=rows).map(|i| i as f32 * row_h).collect(),
            row_h,
            extra: HashMap::new(),
            gen: 0,
        }
    }

    /// The extra heights (row, px), in no particular order.
    pub fn extras(&self) -> impl Iterator<Item = (usize, f32)> + '_ {
        self.extra.iter().map(|(&r, &px)| (r, px))
    }

    pub fn gen(&self) -> u64 {
        self.gen
    }

    pub fn rows(&self) -> usize {
        self.y.len() - 1
    }

    pub fn total(&self) -> f32 {
        self.y[self.rows()]
    }

    pub fn top(&self, row: usize) -> f32 {
        self.y[row]
    }

    pub fn height(&self, row: usize) -> f32 {
        self.y[row + 1] - self.y[row]
    }

    /// Extra space below `row` (an inline comment, later); 0 removes it. Rebuilds the
    /// offsets from `row` down: O(rows below), only when the height changes. Returns
    /// whether anything changed (callers may call it every frame).
    #[cfg(test)]
    pub fn set_extra_height(&mut self, row: usize, px: f32) -> bool {
        let px = px.max(0.0);
        if row >= self.rows() || self.extra.get(&row).copied().unwrap_or(0.0) == px {
            return false;
        }
        if px > 0.0 {
            self.extra.insert(row, px);
        } else {
            self.extra.remove(&row);
        }
        self.gen += 1;
        for i in row..self.rows() {
            let h = self.row_h + self.extra.get(&i).copied().unwrap_or(0.0);
            self.y[i + 1] = self.y[i] + h;
        }
        true
    }

    /// Replaces every extra height at once and rebuilds the offsets in one O(rows) pass
    /// (for laying out many comment threads; `set_extra_height` per thread is quadratic).
    pub fn set_extras(&mut self, extras: impl IntoIterator<Item = (usize, f32)>) {
        let n = self.rows();
        self.extra = extras
            .into_iter()
            .filter(|&(row, px)| row < n && px > 0.0)
            .collect();
        self.gen += 1;
        let mut sorted: Vec<(usize, f32)> = self.extra.iter().map(|(&r, &px)| (r, px)).collect();
        sorted.sort_unstable_by_key(|&(r, _)| r);
        // plain multiples of the row height between rows that have an extra
        let mut added = 0.0;
        let mut next = sorted.iter().peekable();
        for i in 0..n {
            if let Some(&&(r, px)) = next.peek() {
                if r == i {
                    added += px;
                    next.next();
                }
            }
            self.y[i + 1] = (i + 1) as f32 * self.row_h + added;
        }
    }

    /// Scroll anchor for offset `y`: the row covering it and how far into that row it is.
    pub fn anchor_at(&self, y: f32) -> Option<(usize, f32)> {
        self.row_at(y).map(|r| (r, (y - self.top(r)).max(0.0)))
    }

    /// The offset of `anchor` under the current heights (clamped into its row), so the
    /// same row stays at the top when heights above it change.
    pub fn y_for(&self, (row, within): (usize, f32)) -> f32 {
        let row = row.min(self.rows().saturating_sub(1));
        if self.rows() == 0 {
            return 0.0;
        }
        self.top(row) + within.min(self.height(row))
    }

    /// The row covering `y` (binary search), clamped to the rows; None when there are none.
    pub fn row_at(&self, y: f32) -> Option<usize> {
        let n = self.rows();
        (n > 0).then(|| {
            self.y
                .partition_point(|&v| v <= y)
                .saturating_sub(1)
                .min(n - 1)
        })
    }

    /// Rows intersecting `[top, bottom)`.
    pub fn visible_range(&self, top: f32, bottom: f32) -> Range<usize> {
        let Some(first) = self.row_at(top) else {
            return 0..0;
        };
        let end = self.y.partition_point(|&v| v < bottom).min(self.rows());
        first..end.max(first + 1)
    }
}

/// The part of cluster `c` inside columns `[start, end)`: the cluster's own text when it fits
/// whole, otherwise (a tab, or a wide character cut at either edge) one space per visible column.
fn visible_part(
    text: &str,
    c: &Cluster,
    start: usize,
    end: usize,
) -> Option<(String, Range<usize>)> {
    if c.col0 == c.col1 {
        // zero width on its own (leading mark, CR): kept when its column is visible
        let t: String = text[c.b0..c.b1].chars().filter(|&ch| ch != '\r').collect();
        return (c.col0 >= start && c.col0 < end && !t.is_empty()).then_some((t, c.col0..c.col0));
    }
    let (v0, v1) = (c.col0.max(start), c.col1.min(end));
    if v0 >= v1 {
        return None;
    }
    let whole = v0 == c.col0 && v1 == c.col1 && c.base != '\t';
    let t = if whole {
        text[c.b0..c.b1].chars().filter(|&ch| ch != '\r').collect()
    } else {
        " ".repeat(v1 - v0)
    };
    Some((t, v0..v1))
}

/// `text` with tabs expanded to `TAB_WIDTH` stops and CR dropped, display columns
/// `[start, start + count)` only (see the `columns` module). A wide character cut by an edge
/// is padded with spaces. Work is bounded by `start + count`, not the line length.
#[cfg(test)]
pub fn visible_columns(text: &str, start: usize, count: usize) -> String {
    let end = start.saturating_add(count);
    let mut out = String::new();
    for c in clusters(text) {
        if c.col0 >= end && c.col1 > c.col0 {
            break;
        }
        if let Some((t, _)) = visible_part(text, &c, start, end) {
            out.push_str(&t);
        }
    }
    out
}

/// The visible columns of one row, split into runs of one colour (`None`: the row's own
/// colour) plus the column ranges, relative to `start`, that the word diff marks.
#[derive(Debug, Default, PartialEq)]
pub struct Styled {
    pub runs: Vec<(String, Option<Color32>)>,
    pub marks: Vec<Range<usize>>,
}

impl Styled {
    /// Each run with the display column (relative to `start`) it is drawn at: runs are placed
    /// by column, so a glyph whose width differs from its cells cannot shift later runs.
    pub fn run_columns(&self) -> impl Iterator<Item = (usize, &str, Option<Color32>)> {
        self.runs.iter().scan(0usize, |col, (t, c)| {
            let at = *col;
            *col += display_cols(t);
            Some((at, t.as_str(), *c))
        })
    }
}

/// Like `visible_columns`, but keeps the colour of the syntax `spans` and the `words` ranges
/// (both byte ranges into `text`, sorted), looked up by each character's first byte. The
/// concatenated run text equals `visible_columns(text, start, count)`.
pub fn styled_columns(
    text: &str,
    spans: &[(Range<usize>, Color32)],
    words: &[Range<usize>],
    start: usize,
    count: usize,
) -> Styled {
    let end = start.saturating_add(count);
    let mut out = Styled::default();
    let (mut si, mut wi) = (0usize, 0usize);
    for c in clusters(text) {
        if c.col0 >= end && c.col1 > c.col0 {
            break;
        }
        while si < spans.len() && spans[si].0.end <= c.b0 {
            si += 1;
        }
        while wi < words.len() && words[wi].end <= c.b0 {
            wi += 1;
        }
        let Some((shown, cols)) = visible_part(text, &c, start, end) else {
            continue;
        };
        let color = spans
            .get(si)
            .filter(|(r, _)| r.start <= c.b0)
            .map(|(_, c)| *c);
        let marked = words.get(wi).is_some_and(|r| r.start <= c.b0);
        match out.runs.last_mut() {
            Some((t, rc)) if *rc == color => t.push_str(&shown),
            _ => out.runs.push((shown, color)),
        }
        if marked && cols.start < cols.end {
            let (m0, m1) = (cols.start - start, cols.end - start);
            match out.marks.last_mut() {
                Some(m) if m.end == m0 => m.end = m1,
                _ => out.marks.push(m0..m1),
            }
        }
    }
    out
}

/// Width in display columns (see the `columns` module).
pub fn columns(text: &str) -> usize {
    display_cols(text)
}

/// The selected file's rows and layout.
pub struct FileView {
    pub path: String,
    pub rows: Arc<Vec<Row>>,
    /// Over the plain-render limit: shown without highlighting.
    pub plain: bool,
    pub offsets: Offsets,
    /// Widest row in columns, for the horizontal scroll extent.
    pub max_cols: usize,
    /// Comment threads placed on these rows and the state their heights were laid out for.
    pub comments: CommentLayout,
    /// Syntax colours per row, once the worker has produced them.
    pub spans: Option<Arc<Spans>>,
    /// Text selection of this file (fresh for every new patch, so cleared on file switch).
    pub sel: SelectionState,
    /// Widest row and word-diff pairs, computed by the service worker.
    layout: Arc<PatchLayout>,
    /// Word-diff ranges per row, computed when a row is first drawn.
    words: RefCell<HashMap<usize, Rc<Vec<Range<usize>>>>>,
    /// Keyboard cursor (vim keys); None until a key or click places it.
    pub cursor: Option<DiffCursor>,
    /// Split view line tops and the `offsets` generation they were derived from; made when
    /// the split view is first shown.
    split_offsets: Option<(u64, Offsets)>,
}

const NO_PARTNER: u32 = u32::MAX;
/// Memoized word-diff rows kept before the memo is dropped (only visible rows are added).
const WORDS_MEMO_MAX: usize = 20_000;

/// What the diff pane derives from a file's rows: O(rows) work done once, on the service
/// worker that built the patch, so showing a file never scans its rows on the UI thread.
#[derive(Debug, Default, PartialEq, Clone)]
pub struct PatchLayout {
    /// Widest row in display columns, for the horizontal scroll extent.
    pub max_cols: usize,
    /// Per row: the row it is paired with for the word diff (`NO_PARTNER` if none); empty
    /// for plain files (no word diff).
    pub partner: Vec<u32>,
    /// Rendered plain (over the plain limit): no highlight, no word diff.
    pub plain: bool,
    /// The rows as split (side-by-side) lines.
    pub split: SplitMap,
}

impl PatchLayout {
    pub fn compute(rows: &[Row], plain: bool) -> PatchLayout {
        PatchLayout {
            max_cols: rows.iter().map(|r| columns(&r.text)).max().unwrap_or(0),
            partner: if plain { Vec::new() } else { pair_rows(rows) },
            plain,
            split: pair_lines(rows),
        }
    }

    /// Accounted size in the patch cache.
    pub fn bytes(&self) -> usize {
        self.partner.len() * std::mem::size_of::<u32>() + self.split.bytes()
    }
}

/// The keyboard cursor of the diff (vim keys): a unified row, and in the split view the side
/// it prefers (a row of the other side is used on lines where its side is padding).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffCursor {
    pub row: usize,
    pub side: SplitSide,
}

/// Pairs the i-th row of each Del run with the i-th row of the Add run right after it.
fn pair_rows(rows: &[Row]) -> Vec<u32> {
    let mut partner = vec![NO_PARTNER; rows.len()];
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind != RowKind::Del {
            i += 1;
            continue;
        }
        let dels = i;
        while i < rows.len() && rows[i].kind == RowKind::Del {
            i += 1;
        }
        // "\ No newline" after the last old line sits between the two runs
        while i < rows.len() && rows[i].kind == RowKind::NoNewline {
            i += 1;
        }
        let adds = i;
        while i < rows.len() && rows[i].kind == RowKind::Add {
            i += 1;
        }
        for k in 0..(adds - dels).min(i - adds) {
            partner[dels + k] = (adds + k) as u32;
            partner[adds + k] = (dels + k) as u32;
        }
    }
    partner
}

impl FileView {
    /// The rows as split lines (computed with the rest of the layout, on the worker).
    pub fn split(&self) -> &SplitMap {
        &self.layout.split
    }

    /// Tops of the split lines, rebuilt (O(lines)) only when the unified extras changed:
    /// a line's extra height is the cards of both of its rows.
    pub fn split_offsets(&mut self) -> &Offsets {
        let gen = self.offsets.gen();
        let stale = self.split_offsets.as_ref().is_none_or(|(g, _)| *g != gen);
        if stale {
            let map = &self.layout.split;
            let mut o = match self.split_offsets.take() {
                Some((_, o)) => o,
                None => Offsets::new(map.lines.len(), ROW_H),
            };
            o.set_extras(split_extras(map, self.offsets.extras()));
            self.split_offsets = Some((gen, o));
        }
        &self.split_offsets.as_ref().expect("just set").1
    }

    /// The split offsets as last built (`split_offsets` builds them).
    pub fn split_offsets_ref(&self) -> Option<&Offsets> {
        self.split_offsets.as_ref().map(|(_, o)| o)
    }

    /// Changed byte ranges of row `i` against its paired row; None when it has no partner.
    pub fn word_ranges(&self, i: usize) -> Option<Rc<Vec<Range<usize>>>> {
        let p = *self.layout.partner.get(i)?;
        if p == NO_PARTNER {
            return None;
        }
        if let Some(w) = self.words.borrow().get(&i) {
            return Some(w.clone());
        }
        let p = p as usize;
        let (d, a) = if self.rows[i].kind == RowKind::Del {
            (i, p)
        } else {
            (p, i)
        };
        let (del, add) = word_spans(&self.rows[d].text, &self.rows[a].text);
        let mut memo = self.words.borrow_mut();
        if memo.len() > WORDS_MEMO_MAX {
            memo.clear();
        }
        let (del, add) = (Rc::new(del), Rc::new(add));
        memo.insert(d, del.clone());
        memo.insert(a, add.clone());
        Some(if i == d { del } else { add })
    }
}

/// The diff pane: the current file plus remembered scroll positions per file. Only the
/// current file's rows are held, so the service's patch cache stays the memory bound.
#[derive(Default)]
pub struct DiffView {
    current: Option<FileView>,
    scroll: HashMap<String, Vec2>,
}

impl DiffView {
    /// Shows `rows` with the `layout` the worker computed for them (no scan of the rows).
    pub fn set_patch(
        &mut self,
        path: &str,
        rows: Arc<Vec<Row>>,
        plain: bool,
        layout: Arc<PatchLayout>,
    ) {
        // a layout made without its split lines (never by the worker): add them here
        let layout = if layout.split.line_of.len() == rows.len() {
            layout
        } else {
            Arc::new(PatchLayout {
                split: pair_lines(&rows),
                ..(*layout).clone()
            })
        };
        self.current = Some(FileView {
            path: path.to_string(),
            offsets: Offsets::new(rows.len(), ROW_H),
            rows,
            plain,
            max_cols: layout.max_cols,
            comments: CommentLayout::default(),
            spans: None,
            sel: SelectionState::default(),
            layout,
            words: RefCell::default(),
            cursor: None,
            split_offsets: None,
        });
    }

    /// `set_patch` with the layout computed here (tests).
    #[cfg(test)]
    pub fn set_patch_computed(&mut self, path: &str, rows: Arc<Vec<Row>>, plain: bool) {
        let layout = Arc::new(PatchLayout::compute(&rows, plain));
        self.set_patch(path, rows, plain, layout);
    }

    /// Installs syntax colours of rows `range` of `path` (a file of `rows` rows) if it is
    /// still the current file and they fit its rows; steps of one file are merged (rows not
    /// coloured yet stay plain). A whole file's colours are kept as they are, shared.
    pub fn set_highlight(
        &mut self,
        path: &str,
        rows: usize,
        range: Range<usize>,
        spans: Arc<Spans>,
    ) {
        let Some(v) = self.view_mut(path) else {
            return;
        };
        let n = v.rows.len();
        if v.plain || rows != n || range.end > n || spans.len() != range.len() {
            return;
        }
        if range == (0..n) {
            v.spans = Some(spans);
            return;
        }
        let all = v.spans.get_or_insert_with(|| Arc::new(vec![Vec::new(); n]));
        // the only owner (steps are never shared): no copy
        Arc::make_mut(all)[range].clone_from_slice(&spans);
    }

    /// The loaded view of `path`, if it is the current one.
    pub fn view(&self, path: &str) -> Option<&FileView> {
        self.current.as_ref().filter(|v| v.path == path)
    }

    pub fn view_mut(&mut self, path: &str) -> Option<&mut FileView> {
        self.current.as_mut().filter(|v| v.path == path)
    }

    #[cfg(test)]
    pub fn set_extra_height(&mut self, path: &str, row: usize, px: f32) -> bool {
        self.view_mut(path)
            .is_some_and(|v| v.offsets.set_extra_height(row, px))
    }

    pub fn scroll(&self, path: &str) -> Vec2 {
        self.scroll.get(path).copied().unwrap_or(Vec2::ZERO)
    }

    pub fn set_scroll(&mut self, path: &str, offset: Vec2) {
        self.scroll.insert(path.to_string(), offset);
    }

    /// The layout changed (unified / split): remembered offsets of other files are in the
    /// other layout's coordinates, so they are dropped (those files open at the top).
    pub fn forget_other_scrolls(&mut self, path: &str) {
        self.scroll.retain(|p, _| p == path);
    }

    /// Rows of `path` to lay out for a viewport `[top, top + height)`.
    #[cfg(test)]
    pub fn rows_to_layout(&self, path: &str, top: f32, height: f32) -> Range<usize> {
        self.view(path)
            .map_or(0..0, |v| v.offsets.visible_range(top, top + height))
    }
}

/// What a comment marker beside the diff stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TickKind {
    Unresolved,
    Resolved,
    Draft,
}

/// A comment marker on the strip beside the diff: the row it belongs to and where that
/// row starts, as a fraction of the file's content height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    pub row: usize,
    pub frac: f32,
    pub kind: TickKind,
}

/// One tick per (row, kind) of `marks`, sorted by row. O(marks log marks): never a walk
/// over the rows.
pub fn minimap_ticks(
    offsets: &Offsets,
    marks: impl IntoIterator<Item = (usize, TickKind)>,
) -> Vec<Tick> {
    let total = offsets.total();
    let mut m: Vec<(usize, TickKind)> = marks
        .into_iter()
        .filter(|&(r, _)| r < offsets.rows())
        .collect();
    m.sort_unstable();
    m.dedup();
    m.into_iter()
        .map(|(row, kind)| Tick {
            row,
            frac: if total > 0.0 {
                offsets.top(row) / total
            } else {
                0.0
            },
            kind,
        })
        .collect()
}

/// The row of the tick nearest to `frac` (a click on the strip), if it is within `tol`.
pub fn tick_at(ticks: &[Tick], frac: f32, tol: f32) -> Option<usize> {
    ticks
        .iter()
        .map(|t| ((t.frac - frac).abs(), t.row))
        .filter(|&(d, _)| d <= tol)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, r)| r)
}

#[cfg(test)]
mod tests;
