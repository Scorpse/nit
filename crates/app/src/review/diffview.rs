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
mod tests {
    use super::*;

    /// The large-file pipeline on a synthetic 200k-row patch (about 30 MB, like the audit's
    /// `large.rs`): bytes to text, rows, widest row. Each step against the code it replaced
    /// (kept here as the reference, and checked to give the same result).
    /// `cargo test -p app --release --bin nit large_patch_pipeline -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn large_patch_pipeline_bench() {
        use prcore::diff::parse_patch;
        use std::time::Instant;

        fn old_cols(text: &str) -> usize {
            super::super::columns::clusters(text)
                .last()
                .map_or(0, |c| c.col1)
        }
        let mut patch = String::from(
            "diff --git a/large.rs b/large.rs\n--- a/large.rs\n+++ b/large.rs\n@@ -0,0 +1,200000 @@\n",
        );
        for i in 0..200_000 {
            patch.push_str(&format!(
                "+    let value_{i:06} = compute_something(input.field_{i}, \"literal string {i}\", \
                 Some(&mut state), options.clone()).unwrap_or_default(); // note {i}\n"
            ));
        }
        let bytes = patch.clone().into_bytes();
        println!("patch: {} bytes", bytes.len());
        let best = |f: &mut dyn FnMut() -> usize| {
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(f());
                    t.elapsed()
                })
                .min()
                .unwrap()
        };
        let lossy = best(&mut || String::from_utf8_lossy(&bytes).into_owned().len());
        let zero = best(&mut || String::from_utf8(bytes.clone()).unwrap().len())
            .saturating_sub(best(&mut || bytes.clone().len()));
        // parse before/after: prcore's `parse_patch_matches_the_old_parser_and_is_faster`
        let parse = best(&mut || parse_patch(&patch).len());
        let rows = parse_patch(&patch);
        let cols_old = best(&mut || rows.iter().map(|r| old_cols(&r.text)).max().unwrap_or(0));
        let cols_new = best(&mut || rows.iter().map(|r| columns(&r.text)).max().unwrap_or(0));
        assert_eq!(
            rows.iter().map(|r| old_cols(&r.text)).max(),
            rows.iter().map(|r| columns(&r.text)).max()
        );
        println!("bytes -> text: lossy copy {lossy:?}, validate only {zero:?}");
        println!("parse (new): {parse:?}");
        println!("widest row: clusters {cols_old:?}, ascii fast path {cols_new:?}");
    }

    #[test]
    fn minimap_ticks_are_scaled_monotonic_and_clicks_map_back() {
        let mut o = Offsets::new(100, ROW_H);
        // a tall card under row 10 pushes the later rows down
        o.set_extras([(10, 50.0 * ROW_H)]);
        let ticks = minimap_ticks(
            &o,
            [
                (90, TickKind::Draft),
                (10, TickKind::Unresolved),
                (10, TickKind::Unresolved),
                (10, TickKind::Resolved),
                (50, TickKind::Resolved),
                (500, TickKind::Draft),
            ],
        );
        let rows: Vec<(usize, TickKind)> = ticks.iter().map(|t| (t.row, t.kind)).collect();
        assert_eq!(
            rows,
            [
                (10, TickKind::Unresolved),
                (10, TickKind::Resolved),
                (50, TickKind::Resolved),
                (90, TickKind::Draft)
            ],
            "deduplicated, sorted, rows outside the file dropped"
        );
        assert!(
            ticks.windows(2).all(|w| w[0].frac <= w[1].frac),
            "monotonic"
        );
        let total = o.total();
        assert_eq!(ticks[0].frac, 10.0 * ROW_H / total);
        assert_eq!(ticks[2].frac, o.top(50) / total);
        assert!(
            ticks[2].frac > 0.5,
            "scaled by content height, not row count"
        );
        assert!(ticks.iter().all(|t| (0.0..=1.0).contains(&t.frac)));
        assert_eq!(tick_at(&ticks, ticks[2].frac + 0.001, 0.01), Some(50));
        assert_eq!(tick_at(&ticks, ticks[3].frac - 0.005, 0.01), Some(90));
        assert_eq!(tick_at(&ticks, 0.0, 0.01), None, "nothing near");
        assert_eq!(tick_at(&[], 0.5, 1.0), None);
        assert!(minimap_ticks(&Offsets::new(0, ROW_H), [(0, TickKind::Draft)]).is_empty());
    }

    fn rows(n: usize) -> Arc<Vec<Row>> {
        Arc::new(
            (0..n)
                .map(|i| Row {
                    kind: RowKind::Context,
                    old_line: Some(i as u32 + 1),
                    new_line: Some(i as u32 + 1),
                    text: "x".repeat(i % 7),
                })
                .collect(),
        )
    }

    #[test]
    fn offsets_binary_search_finds_row_at_y() {
        let o = Offsets::new(100, ROW_H);
        assert_eq!(o.total(), 100.0 * ROW_H);
        assert_eq!(o.row_at(0.0), Some(0));
        assert_eq!(o.row_at(ROW_H - 0.1), Some(0));
        assert_eq!(o.row_at(ROW_H), Some(1));
        assert_eq!(o.row_at(42.5 * ROW_H), Some(42));
        assert_eq!(o.row_at(-5.0), Some(0), "clamped above");
        assert_eq!(o.row_at(1e9), Some(99), "clamped below");
        assert_eq!(Offsets::new(0, ROW_H).row_at(0.0), None);
    }

    #[test]
    fn variable_heights_shift_following_rows() {
        let mut o = Offsets::new(10, ROW_H);
        o.set_extra_height(3, 50.0);
        assert_eq!(o.top(3), 3.0 * ROW_H, "rows above are unchanged");
        assert_eq!(o.height(3), ROW_H + 50.0);
        assert_eq!(o.top(4), 4.0 * ROW_H + 50.0);
        assert_eq!(o.total(), 10.0 * ROW_H + 50.0);
        assert_eq!(
            o.row_at(o.top(3) + ROW_H + 10.0),
            Some(3),
            "inside the extra"
        );
        assert_eq!(o.row_at(o.top(4) + 1.0), Some(4));
        o.set_extra_height(1, 20.0);
        assert_eq!(o.top(4), 4.0 * ROW_H + 70.0);
        o.set_extra_height(3, 0.0);
        assert_eq!(o.top(4), 4.0 * ROW_H + 20.0);
        assert_eq!(o.height(3), ROW_H);
    }

    #[test]
    fn set_extras_matches_one_by_one_updates_and_replaces_old_extras() {
        let mut a = Offsets::new(20, ROW_H);
        let mut b = Offsets::new(20, ROW_H);
        a.set_extra_height(7, 99.0);
        a.set_extras([(3, 40.0), (12, 25.0), (50, 10.0), (5, 0.0)]);
        b.set_extra_height(3, 40.0);
        b.set_extra_height(12, 25.0);
        for i in 0..=20 {
            let (ya, yb) = (a.y[i], b.y[i]);
            assert!((ya - yb).abs() < 1e-3, "row {i}: {ya} vs {yb}");
        }
        assert_eq!(a.height(7), ROW_H, "previous extras are replaced");
        a.set_extra_height(3, 0.0);
        assert_eq!(a.total(), 20.0 * ROW_H + 25.0);
    }

    #[test]
    fn scroll_anchor_survives_height_changes_above_it() {
        let mut o = Offsets::new(100, ROW_H);
        o.set_extras([(60, 30.0)]);
        let y = o.top(50) + 5.0;
        let a = o.anchor_at(y).unwrap();
        assert_eq!(a, (50, 5.0));
        o.set_extras([(10, 200.0), (60, 30.0)]); // grew above
        assert_eq!(o.y_for(a), y + 200.0);
        o.set_extras([(70, 99.0)]); // below only: unchanged
        assert_eq!(o.y_for(a), 50.0 * ROW_H + 5.0);
        // an anchor inside a row's own extra stays inside that row when it shrinks
        o.set_extras([(50, 100.0)]);
        let deep = o.anchor_at(o.top(50) + 80.0).unwrap();
        o.set_extras([]);
        assert_eq!(o.y_for(deep), o.top(51), "clamped to the row's new height");
        assert_eq!(Offsets::new(0, ROW_H).anchor_at(0.0), None);
    }

    #[test]
    fn visible_range_for_viewport_is_bounded() {
        let mut v = DiffView::default();
        v.set_patch_computed("big.rs", rows(1_000_000), true);
        let top = 600_000.0 * ROW_H + 3.0;
        let r = v.rows_to_layout("big.rs", top, 1200.0);
        assert!(r.len() < 200, "{r:?}");
        assert!(r.start <= 600_000 && r.end > 600_000 + 60, "{r:?}");
        let end = v.rows_to_layout("big.rs", 1e12, 900.0);
        assert!(end.len() < 200 && end.end == 1_000_000, "{end:?}");
        assert!(v.rows_to_layout("other.rs", 0.0, 900.0).is_empty());
    }

    #[test]
    fn scroll_is_remembered_per_file_and_only_the_current_file_is_held() {
        let mut v = DiffView::default();
        v.set_patch_computed("a.rs", rows(50), false);
        v.set_scroll("a.rs", Vec2::new(0.0, 120.0));
        v.set_patch_computed("b.rs", rows(5), false);
        assert!(v.view("a.rs").is_none() && v.view("b.rs").is_some());
        assert_eq!(v.scroll("a.rs"), Vec2::new(0.0, 120.0));
        assert_eq!(v.scroll("b.rs"), Vec2::ZERO);
        v.set_extra_height("b.rs", 2, 30.0);
        assert_eq!(v.view("b.rs").unwrap().offsets.total(), 5.0 * ROW_H + 30.0);
    }

    #[test]
    fn max_cols_tracks_the_widest_expanded_row() {
        let mut v = DiffView::default();
        let r = vec![
            Row {
                kind: RowKind::Add,
                old_line: None,
                new_line: Some(1),
                text: "\tab".into(),
            },
            Row {
                kind: RowKind::Add,
                old_line: None,
                new_line: Some(2),
                text: "abc".into(),
            },
        ];
        v.set_patch_computed("t.rs", Arc::new(r), false);
        assert_eq!(v.view("t.rs").unwrap().max_cols, 6);
    }

    #[test]
    fn set_patch_takes_the_precomputed_layout_as_is() {
        let mut v = DiffView::default();
        let r = rows(7);
        // the layout comes from the worker; set_patch never recomputes it
        let layout = Arc::new(PatchLayout {
            max_cols: 99,
            partner: vec![NO_PARTNER; r.len()],
            plain: false,
            split: Default::default(),
        });
        v.set_patch("a.rs", r.clone(), false, layout.clone());
        assert_eq!(v.view("a.rs").unwrap().max_cols, 99);
        assert_eq!(PatchLayout::compute(&r, false).max_cols, 6);
        assert!(
            PatchLayout::compute(&r, true).partner.is_empty(),
            "plain: no word diff"
        );
        let pairs = Arc::new(vec![
            kind_row(RowKind::Del, "a"),
            kind_row(RowKind::Add, "b"),
        ]);
        assert_eq!(PatchLayout::compute(&pairs, false).partner, vec![1, 0]);
    }

    #[test]
    fn unchanged_extra_height_is_a_no_op() {
        let mut o = Offsets::new(10, ROW_H);
        assert!(o.set_extra_height(3, 50.0));
        assert!(!o.set_extra_height(3, 50.0), "same value");
        assert!(!o.set_extra_height(5, 0.0), "already none");
        assert!(!o.set_extra_height(99, 10.0), "out of range");
        assert!(o.set_extra_height(3, 0.0));
        assert_eq!(o.total(), 10.0 * ROW_H);
    }

    #[test]
    fn tabs_expand_to_four_columns_and_clip() {
        assert_eq!(columns("\tx"), 5);
        assert_eq!(columns("ab\tc"), 5);
        assert_eq!(columns("abc\r"), 3);
        assert_eq!(visible_columns("ab\tc", 0, 10), "ab  c");
        assert_eq!(visible_columns("ab\tcd", 3, 2), " c");
        assert_eq!(visible_columns("hello\r", 0, 10), "hello");
        assert_eq!(visible_columns("héllo wörld", 6, 3), "wör");
        assert_eq!(visible_columns("short", 10, 5), "");
        let long = "y".repeat(20_000);
        assert_eq!(visible_columns(&long, 19_998, 10), "yy");
    }

    fn kind_row(kind: RowKind, text: &str) -> Row {
        Row {
            kind,
            old_line: None,
            new_line: None,
            text: text.into(),
        }
    }

    #[test]
    fn styled_runs_match_visible_columns_with_tabs_and_multibyte() {
        let (red, blue) = (Color32::RED, Color32::BLUE);
        // bytes: \t=0 h=1 é=2..4 l=4 \t=5 w=6 ö=7..9 r=9 \r=10
        let text = "\thél\twör\r";
        let spans = vec![(0..4, red), (5..9, blue)];
        for (start, count) in [(0, 40), (0, 3), (2, 5), (4, 6), (7, 2), (30, 5)] {
            let st = styled_columns(text, &spans, &[], start, count);
            let joined: String = st.runs.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(
                joined,
                visible_columns(text, start, count),
                "{start} {count}"
            );
        }
        let st = styled_columns(text, &spans, &[], 0, 40);
        assert_eq!(st.runs[0], ("    hé".to_string(), Some(red)));
        assert_eq!(st.runs[1], ("l".to_string(), None));
        assert_eq!(st.runs[2], (" wö".to_string(), Some(blue)));
        assert_eq!(st.runs[3], ("r".to_string(), None));
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn styled_marks_are_columns_relative_to_start() {
        // "ab\tcd": columns a0 b1 tab2-3 c4 d5; marking bytes of "b\tc" = 1..4
        let st = styled_columns("ab\tcd", &[], &[1..4], 0, 10);
        assert_eq!(st.marks, [1..5]);
        let st = styled_columns("ab\tcd", &[], &[1..4], 3, 10);
        assert_eq!(st.marks, [0..2], "clipped at the left edge and shifted");
        let st = styled_columns("héllo", &[], &[1..3], 0, 10);
        assert_eq!(st.marks, [1..2], "a multi-byte char is one column");
        assert!(styled_columns("abc", &[], &[], 0, 10).marks.is_empty());
    }

    #[test]
    fn rows_pair_in_order_within_del_add_runs() {
        use RowKind::*;
        let rows: Vec<Row> = [
            (HunkHeader, "@@"),
            (Del, "a"),
            (Del, "b"),
            (Del, "c"),
            (Add, "A"),
            (Add, "B"),
            (Context, "x"),
            (Add, "lone"),
            (Del, "d"),
        ]
        .iter()
        .map(|(k, t)| kind_row(*k, t))
        .collect();
        let p = pair_rows(&rows);
        assert_eq!((p[1], p[4]), (4, 1));
        assert_eq!((p[2], p[5]), (5, 2));
        for i in [0, 3, 6, 7, 8] {
            assert_eq!(p[i], NO_PARTNER, "row {i}");
        }
    }

    #[test]
    fn no_newline_marker_does_not_split_a_del_add_pair() {
        use RowKind::*;
        let rows: Vec<Row> = [
            (Del, "old"),
            (NoNewline, "\\ No newline at end of file"),
            (Add, "new"),
            (NoNewline, "\\ No newline at end of file"),
        ]
        .iter()
        .map(|(k, t)| kind_row(*k, t))
        .collect();
        let p = pair_rows(&rows);
        assert_eq!((p[0], p[2]), (2, 0));
        assert_eq!((p[1], p[3]), (NO_PARTNER, NO_PARTNER));
    }

    #[test]
    fn word_ranges_are_lazy_memoized_and_skipped_for_plain_files() {
        let rows = Arc::new(vec![
            kind_row(RowKind::Del, "let x = 1;"),
            kind_row(RowKind::Add, "let x = 2;"),
        ]);
        let mut v = DiffView::default();
        v.set_patch_computed("a.rs", rows.clone(), false);
        let fv = v.view("a.rs").unwrap();
        assert!(fv.words.borrow().is_empty(), "nothing before drawing");
        assert_eq!(*fv.word_ranges(0).unwrap(), vec![8..9]);
        assert_eq!(fv.words.borrow().len(), 2, "the partner is memoized too");
        assert_eq!(*fv.word_ranges(1).unwrap(), vec![8..9]);
        v.set_patch_computed("p.rs", rows, true);
        assert!(v.view("p.rs").unwrap().word_ranges(0).is_none());
    }

    #[test]
    fn highlight_is_applied_only_to_the_matching_current_file() {
        let mut v = DiffView::default();
        v.set_patch_computed("a.rs", rows(2), false);
        v.set_highlight("b.rs", 2, 0..2, Arc::new(vec![vec![], vec![]]));
        assert!(v.view("a.rs").unwrap().spans.is_none());
        v.set_highlight("a.rs", 1, 0..1, Arc::new(vec![vec![]]));
        assert!(v.view("a.rs").unwrap().spans.is_none(), "wrong row count");
        v.set_highlight("a.rs", 2, 0..3, Arc::new(vec![vec![], vec![], vec![]]));
        assert!(
            v.view("a.rs").unwrap().spans.is_none(),
            "range past the rows"
        );
        v.set_highlight("a.rs", 2, 0..2, Arc::new(vec![vec![], vec![]]));
        assert!(v.view("a.rs").unwrap().spans.is_some());
    }

    #[test]
    fn highlight_steps_are_merged_into_the_current_file() {
        let (red, blue) = (Color32::RED, Color32::BLUE);
        let mut v = DiffView::default();
        v.set_patch_computed("a.rs", rows(5), false);
        v.set_highlight("a.rs", 5, 3..5, Arc::new(vec![vec![(0..1, red)]; 2]));
        let sp = v.view("a.rs").unwrap().spans.clone().unwrap();
        assert_eq!(sp.len(), 5);
        assert!(sp[..3].iter().all(|s| s.is_empty()), "not coloured yet");
        assert_eq!(sp[4], vec![(0..1, red)]);
        drop(sp);
        v.set_highlight("a.rs", 5, 0..3, Arc::new(vec![vec![(0..1, blue)]; 3]));
        let sp = v.view("a.rs").unwrap().spans.clone().unwrap();
        assert_eq!(sp[0], vec![(0..1, blue)]);
        assert_eq!(sp[3], vec![(0..1, red)], "earlier step kept");
        // a whole file's colours are taken as they are (no copy)
        let whole = Arc::new(vec![Vec::new(); 5]);
        v.set_highlight("a.rs", 5, 0..5, whole.clone());
        assert!(Arc::ptr_eq(
            v.view("a.rs").unwrap().spans.as_ref().unwrap(),
            &whole
        ));
        // a step for a plain file is ignored
        v.set_patch_computed("p.rs", rows(2), true);
        v.set_highlight("p.rs", 2, 0..1, Arc::new(vec![vec![]]));
        assert!(v.view("p.rs").unwrap().spans.is_none());
    }

    const MIXED: &str = "a\t日本e\u{301}😀x\u{200d}y\tz\r";

    #[test]
    fn styled_runs_equal_visible_columns_for_wide_combining_and_tabs() {
        let (red, blue) = (Color32::RED, Color32::BLUE);
        let spans = vec![(0..4, red), (7..12, blue)];
        let total = columns(MIXED);
        for start in 0..total + 2 {
            for count in 0..total + 2 {
                let st = styled_columns(MIXED, &spans, &[], start, count);
                let joined: String = st.runs.iter().map(|(t, _)| t.as_str()).collect();
                assert_eq!(
                    joined,
                    visible_columns(MIXED, start, count),
                    "{start} {count}"
                );
                assert!(display_cols(&joined) <= count, "{start} {count}");
            }
        }
    }

    #[test]
    fn scroll_offset_into_wide_chars_pads_cut_glyph() {
        // 日 = columns 0-1, 本 = 2-3
        assert_eq!(visible_columns("日本", 0, 4), "日本");
        assert_eq!(visible_columns("日本", 1, 3), " 本", "cut at the left edge");
        assert_eq!(
            visible_columns("日本", 0, 3),
            "日 ",
            "cut at the right edge"
        );
        assert_eq!(visible_columns("日本", 3, 5), " ");
        assert_eq!(visible_columns("a日", 1, 1), " ");
        // marks stay with their base and are never cut off from it
        assert_eq!(visible_columns("e\u{301}x", 0, 1), "e\u{301}");
        assert_eq!(visible_columns("e\u{301}x", 1, 1), "x");
        // a tab cut by the left edge keeps only its visible columns
        assert_eq!(visible_columns("a\tb", 2, 4), "  b");
    }

    #[test]
    fn max_cols_counts_wide_chars_as_two() {
        let mut v = DiffView::default();
        let r = |t: &str| Row {
            kind: RowKind::Add,
            old_line: None,
            new_line: Some(1),
            text: t.into(),
        };
        v.set_patch_computed(
            "w.rs",
            Arc::new(vec![r("日本"), r("abc"), r("\t日")]),
            false,
        );
        assert_eq!(
            v.view("w.rs").unwrap().max_cols,
            6,
            "tab 4 + one wide char 2"
        );
        assert_eq!(columns("日本語"), 6);
        assert_eq!(columns("e\u{301}"), 1, "a mark adds no column");
        assert_eq!(columns("😀"), 2);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn word_diff_marks_align_with_selection_columns() {
        use crate::review::columns::byte_to_col;
        use crate::review::selection::col_to_x;
        // the changed word "bb" starts after a tab, a wide char and a combining mark
        let text = "\t日e\u{301} bb c";
        let start_b = text.find("bb").unwrap();
        let c0 = byte_to_col(text, start_b);
        assert_eq!(c0, 4 + 2 + 1 + 1);
        let st = styled_columns(text, &[], &[start_b..start_b + 2], 0, 40);
        assert_eq!(st.marks, [c0..c0 + 2]);
        // the selection maps that column to the x where the mark is drawn
        assert_eq!(col_to_x(text, c0, 8.0), c0 as f32 * 8.0);
        // scrolled: marks are relative to the first visible column
        let st = styled_columns(text, &[], &[start_b..start_b + 2], 5, 40);
        assert_eq!(st.marks, [c0 - 5..c0 - 3]);
        // a mark over a wide char covers both of its columns
        let st = styled_columns("x日y", &[], &[1..4], 0, 10);
        assert_eq!(st.marks, [1..3]);
    }

    #[test]
    fn highlight_x_matches_drawn_x_for_wide_and_combining() {
        use crate::review::selection::col_to_x;
        let w = 8.0;
        let (red, blue) = (Color32::RED, Color32::BLUE);
        // colour changes inside tabs, wide chars and mark sequences
        let spans = vec![(0..1, red), (4..8, blue), (8..13, red), (13..20, blue)];
        for start in [0usize, 1, 4, 6] {
            let st = styled_columns(MIXED, &spans, &[], start, 60);
            assert!(st.runs.len() >= 3);
            for (col, text, _) in st.run_columns() {
                // the run is drawn at x(start + col); the selection highlight uses col_to_x
                let drawn_x = (start + col) as f32 * w;
                assert_eq!(
                    col_to_x(MIXED, start + col, w),
                    drawn_x,
                    "{start}+{col} {text:?}"
                );
            }
            let end = st
                .run_columns()
                .last()
                .map(|(c, t, _)| c + display_cols(t))
                .unwrap();
            assert_eq!(start + end, columns(MIXED), "runs reach the last column");
        }
    }
}
