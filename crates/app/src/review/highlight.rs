//! Syntax highlighting of diff rows with per-hunk parse state.
//!
//! Hunks are independent (the parse state restarts at each hunk header), so a file is
//! highlighted hunk by hunk, starting with the hunk on screen, in steps of a few hundred
//! rows; each step's colours are handed out at once and a job can stop between any two rows.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use eframe::egui::Color32;
use prcore::diff::{Row, RowKind};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Colour spans of one row: byte ranges of its text with their colour.
pub type RowSpans = Vec<(Range<usize>, Color32)>;
/// One span list per row.
pub type Spans = Vec<RowSpans>;

/// Colours of at most this many recently highlighted files are kept, so going back to a file
/// does not parse it again (the file on screen and its prefetched neighbours fit).
const REMEMBERED_FILES: usize = 16;
/// ... and at most this many (estimated) bytes of colours in total.
const REMEMBERED_BYTES: usize = 64 << 20;
/// A file whose colours are bigger than this is not remembered.
const REMEMBERED_FILE_MAX_BYTES: usize = 16 << 20;
/// Highlight jobs for the file on screen waiting at once: only the files the user asked for
/// last still matter, older ones were scrolled past by switching files.
const MAX_QUEUED: usize = 2;
/// Background jobs (prefetched neighbours) waiting at once.
const MAX_BACKGROUND: usize = 6;

/// Rows per step: colours are handed out (and "is this file still wanted?" asked) after
/// at most this many rows.
pub const CHUNK_ROWS: usize = 400;
/// Wall time one file may take; the rest of a file over it stays plain.
pub const HIGHLIGHT_BUDGET: Duration = Duration::from_secs(3);

/// Rough size of a file's colours: a span and a row's vector header are about 24 bytes each.
fn spans_bytes(spans: &Spans) -> usize {
    spans.iter().map(|r| 24 + r.len() * 24).sum()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlPriority {
    /// The file on screen: before prefetched patches.
    Visible,
    /// A prefetched neighbour: only when nothing else waits.
    Background,
}

pub struct HlJob {
    pub path: String,
    pub rows: Arc<Vec<Row>>,
    pub priority: HlPriority,
    /// Set when the file was shown again while this job runs: the job sends every row it
    /// has coloured so far again (the new view of the file has none of them).
    pub resend: Arc<AtomicBool>,
}

/// Pending highlight jobs plus the colours of the last few files.
#[derive(Default)]
pub struct HighlightQueue {
    /// Jobs for the file on screen, newest last.
    queue: VecDeque<HlJob>,
    /// Jobs for prefetched files, newest last.
    background: VecDeque<HlJob>,
    /// Queued or running paths, so one file is never highlighted twice at once.
    pending: HashSet<String>,
    /// Resend flags of the running jobs.
    running: HashMap<String, Arc<AtomicBool>>,
    done: VecDeque<(String, Arc<Spans>)>,
}

impl HighlightQueue {
    pub fn clear(&mut self) {
        self.queue.clear();
        self.background.clear();
        self.pending.clear();
        self.running.clear();
        self.done.clear();
    }

    fn remembered(&self, path: &str, rows: &[Row]) -> Option<Arc<Spans>> {
        self.done
            .iter()
            .find(|(p, _)| p == path)
            .filter(|(_, s)| s.len() == rows.len())
            .map(|(_, s)| s.clone())
    }

    fn job(path: &str, rows: &Arc<Vec<Row>>, priority: HlPriority) -> HlJob {
        HlJob {
            path: path.to_string(),
            rows: rows.clone(),
            priority,
            resend: Arc::default(),
        }
    }

    /// The file on screen: its remembered colours if they fit `rows`. Otherwise a running
    /// job for it is asked to send what it has again, a queued one (also a background one)
    /// becomes the newest job for the screen, or a new job is queued as the newest one
    /// (older ones beyond `MAX_QUEUED` are dropped); then None.
    pub fn request(&mut self, path: &str, rows: &Arc<Vec<Row>>) -> Option<Arc<Spans>> {
        if let Some(spans) = self.remembered(path, rows) {
            return Some(spans);
        }
        if let Some(flag) = self.running.get(path) {
            flag.store(true, Ordering::SeqCst);
            return None;
        }
        let queued = match self.queue.iter().position(|j| j.path == path) {
            Some(pos) => self.queue.remove(pos),
            None => self
                .background
                .iter()
                .position(|j| j.path == path)
                .and_then(|pos| self.background.remove(pos)),
        };
        let job = match queued {
            Some(mut job) => {
                job.priority = HlPriority::Visible;
                job
            }
            None if self.pending.insert(path.to_string()) => {
                Self::job(path, rows, HlPriority::Visible)
            }
            None => return None,
        };
        self.queue.push_back(job);
        while self.queue.len() > MAX_QUEUED {
            if let Some(stale) = self.queue.pop_front() {
                self.pending.remove(&stale.path);
            }
        }
        None
    }

    /// A prefetched file: queued behind everything else unless it is remembered, queued or
    /// running already. Only the newest `MAX_BACKGROUND` wait.
    pub fn request_background(&mut self, path: &str, rows: &Arc<Vec<Row>>) {
        if self.remembered(path, rows).is_some() || !self.pending.insert(path.to_string()) {
            return;
        }
        self.background
            .push_back(Self::job(path, rows, HlPriority::Background));
        while self.background.len() > MAX_BACKGROUND {
            if let Some(stale) = self.background.pop_front() {
                self.pending.remove(&stale.path);
            }
        }
    }

    fn start(&mut self, job: HlJob) -> HlJob {
        self.running.insert(job.path.clone(), job.resend.clone());
        job
    }

    /// The newest job for the screen; its path stays pending until `finish`.
    pub fn pop(&mut self) -> Option<HlJob> {
        let job = self.queue.pop_back()?;
        Some(self.start(job))
    }

    /// The newest background job; its path stays pending until `finish`.
    pub fn pop_background(&mut self) -> Option<HlJob> {
        let job = self.background.pop_back()?;
        Some(self.start(job))
    }

    #[cfg(test)]
    pub fn has_visible(&self) -> bool {
        !self.queue.is_empty()
    }

    /// A job ended: remembers its colours (None: it failed or was abandoned, the file stays
    /// plain until asked for again). Returns whether the file was shown again since the
    /// job's last step (its colours are then sent whole).
    pub fn finish(&mut self, path: &str, spans: Option<Arc<Spans>>) -> bool {
        self.pending.remove(path);
        let resend = self
            .running
            .remove(path)
            .is_some_and(|f| f.load(Ordering::SeqCst));
        self.done.retain(|(p, _)| p != path);
        let Some(spans) = spans else { return resend };
        if spans_bytes(&spans) > REMEMBERED_FILE_MAX_BYTES {
            return resend;
        }
        self.done.push_back((path.to_string(), spans));
        let mut total: usize = self.done.iter().map(|(_, s)| spans_bytes(s)).sum();
        while self.done.len() > REMEMBERED_FILES || total > REMEMBERED_BYTES {
            let Some((_, old)) = self.done.pop_front() else {
                break;
            };
            total -= spans_bytes(&old);
        }
        resend
    }
}

/// Loads the syntax set and theme and compiles the TypeScript and Rust syntaxes, so the
/// first real file does not pay for it (run once, on its own thread, per service).
pub fn warm() {
    let row = |text: &str| Row {
        kind: RowKind::Add,
        old_line: None,
        new_line: Some(1),
        text: text.into(),
    };
    let ts = "import type { A } from \"a\"; const f = <T,>(x: T): T => x; `${f(1)}`;";
    let _ = highlight_rows("warm.ts", &[row(ts)]);
    let _ = highlight_rows("warm.rs", &[row("fn main() { let x = \"s\"; }")]);
}

/// Lines longer than this (chars) are not highlighted (minified code: slow, and unreadable
/// in colour too).
const MAX_LINE_CHARS: usize = 2_000;

/// bat's syntax set (a superset of syntect's defaults), loaded once on first use.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
/// Dark theme that sits well on the diff's red/green row backgrounds.
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let mut themes = ThemeSet::load_defaults().themes;
    themes
        .remove("base16-ocean.dark")
        .or_else(|| themes.into_values().next())
        .unwrap_or_default()
});

/// The parse state of one hunk: the old file (Context and Del rows) and the new file
/// (Context and Add rows).
struct Sides {
    syntax: &'static SyntaxReference,
    old: HighlightLines<'static>,
    new: HighlightLines<'static>,
}

impl Sides {
    fn new(syntax: &'static SyntaxReference) -> Sides {
        Sides {
            syntax,
            old: HighlightLines::new(syntax, &THEME),
            new: HighlightLines::new(syntax, &THEME),
        }
    }

    /// Colours of one row, advancing the state; a hunk header restarts it.
    fn row(&mut self, row: &Row) -> RowSpans {
        match row.kind {
            RowKind::HunkHeader => {
                *self = Sides::new(self.syntax);
                Vec::new()
            }
            RowKind::NoNewline => Vec::new(),
            RowKind::Context => {
                let (text, long) = clip(&row.text);
                let line = format!("{text}\n");
                let found = run(&mut self.new, &line, text.len());
                // the old file has this line too; its result is not needed
                let _ = self.old.highlight_line(&line, &SYNTAXES);
                if long {
                    Vec::new()
                } else {
                    found
                }
            }
            RowKind::Add | RowKind::Del => {
                let (text, long) = clip(&row.text);
                let side = if row.kind == RowKind::Add {
                    &mut self.new
                } else {
                    &mut self.old
                };
                let found = run(side, &format!("{text}\n"), text.len());
                if long {
                    Vec::new()
                } else {
                    found
                }
            }
        }
    }
}

/// Highlights each row's text, the whole file in order (the reference for
/// `highlight_chunked`). A hunk is parsed as the two files it came from: the old side is fed
/// Context and Del rows, the new side Context and Add rows, and each state restarts at a
/// hunk header. Unknown file types, over-long lines (they still advance the parse state) and
/// rows without code (hunk headers, "no newline" markers) get no spans; syntect errors only
/// drop the affected row.
pub fn highlight_rows(path: &str, rows: &[Row]) -> Spans {
    let Some(syntax) = syntax_for(path) else {
        return vec![Vec::new(); rows.len()];
    };
    let mut sides = Sides::new(syntax);
    rows.iter().map(|r| sides.row(r)).collect()
}

/// What a job does after handing out a step's colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Continue,
    /// Hand out every row coloured so far again, then continue.
    Resend,
    /// Abandon the job.
    Stop,
}

/// How a chunked highlight ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlEnd {
    Done,
    /// The time budget ran out: the rows not reached stay plain.
    OverBudget,
    /// `cancelled` or a `Next::Stop`: the colours are incomplete.
    Cancelled,
}

/// Controls of `highlight_chunked`.
pub struct Chunking<'a> {
    /// Most rows per step.
    pub rows: usize,
    /// Wall time for the whole file.
    pub budget: Duration,
    /// Checked before every row: true stops at once.
    pub cancelled: &'a dyn Fn() -> bool,
    /// The first row on screen when this file is shown (read before each hunk is chosen).
    pub on_screen: &'a dyn Fn() -> Option<usize>,
    /// A step's colours: rows `range` (contiguous) and their spans.
    pub emit: &'a mut dyn FnMut(Range<usize>, &[RowSpans]) -> Next,
}

/// The row ranges that start with fresh parse state: one per hunk (from its header), and the
/// rows before the first header, if any.
fn segments(rows: &[Row]) -> Vec<Range<usize>> {
    let mut starts: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == RowKind::HunkHeader)
        .map(|(i, _)| i)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let mut out: Vec<Range<usize>> = starts.windows(2).map(|w| w[0]..w[1]).collect();
    if let Some(&last) = starts.last() {
        out.push(last..rows.len());
    }
    out.retain(|r| !r.is_empty());
    out
}

/// The next hunk to colour: the one holding `row`, else the nearest one below it, else the
/// nearest one above it.
fn pick(segs: &[Range<usize>], done: &[bool], row: usize) -> Option<usize> {
    let open = |k: &usize| !done[*k];
    let at = segs.partition_point(|s| s.end <= row);
    (at..segs.len())
        .find(open)
        .or_else(|| (0..at.min(segs.len())).rev().find(open))
}

/// Like `highlight_rows` (the same colours for every row it reaches), but hunk by hunk from
/// the one on screen outward, in steps of at most `c.rows` rows whose colours go to
/// `c.emit` as soon as they are done. Stops between rows when `c.cancelled` says so, when
/// `c.emit` says `Stop`, or when `c.budget` is spent (the rows not reached stay plain).
pub fn highlight_chunked(path: &str, rows: &[Row], c: Chunking<'_>) -> (Spans, HlEnd) {
    let mut out: Spans = vec![Vec::new(); rows.len()];
    let Some(syntax) = syntax_for(path) else {
        return (out, HlEnd::Done);
    };
    let started = Instant::now();
    let segs = segments(rows);
    let mut done = vec![false; segs.len()];
    let mut have = vec![false; rows.len()];
    // (hunk, next row, its state)
    let mut cur: Option<(usize, usize, Sides)> = None;
    loop {
        if cur.is_none() {
            let row = (c.on_screen)().unwrap_or(0);
            let Some(k) = pick(&segs, &done, row) else {
                return (out, HlEnd::Done);
            };
            cur = Some((k, segs[k].start, Sides::new(syntax)));
        }
        let step_start = cur.as_ref().map_or(0, |(_, at, _)| *at);
        let mut end = step_start;
        let mut over = false;
        while end - step_start < c.rows {
            let Some((k, at, sides)) = cur.as_mut() else {
                break;
            };
            if *at == segs[*k].end {
                done[*k] = true;
                // the next hunk starts where this one ended: same step
                let next = *k + 1;
                if next < segs.len() && !done[next] {
                    cur = Some((next, segs[next].start, Sides::new(syntax)));
                    continue;
                }
                cur = None;
                break;
            }
            if (c.cancelled)() {
                return (out, HlEnd::Cancelled);
            }
            if started.elapsed() > c.budget {
                over = true;
                break;
            }
            out[*at] = sides.row(&rows[*at]);
            have[*at] = true;
            *at += 1;
            end = *at;
        }
        if let Some((k, at, _)) = &cur {
            if *at == segs[*k].end {
                done[*k] = true;
                cur = None;
            }
        }
        if end > step_start {
            match (c.emit)(step_start..end, &out[step_start..end]) {
                Next::Continue => {}
                Next::Stop => return (out, HlEnd::Cancelled),
                Next::Resend => {
                    let mut i = 0;
                    while i < have.len() {
                        if !have[i] {
                            i += 1;
                            continue;
                        }
                        let j = (i..have.len()).find(|&j| !have[j]).unwrap_or(have.len());
                        if (c.emit)(i..j, &out[i..j]) == Next::Stop {
                            return (out, HlEnd::Cancelled);
                        }
                        i = j;
                    }
                }
            }
        }
        if over {
            return (out, HlEnd::OverBudget);
        }
        // a step that ended at a hunk boundary: the next hunk is chosen again, from where
        // the screen is now
        if let Some((k, at, _)) = &cur {
            if *at == segs[*k].start {
                cur = None;
            }
        }
    }
}

/// The first `MAX_LINE_CHARS` chars of `s`, and whether it was longer. A long line gets no
/// spans, but its prefix still advances the parse state so later lines stay consistent.
fn clip(s: &str) -> (&str, bool) {
    match s.char_indices().nth(MAX_LINE_CHARS) {
        Some((i, _)) => (&s[..i], true),
        None => (s, false),
    }
}

/// Syntax names for file names and extensions the set does not map (or maps elsewhere).
const BY_NAME: &[(&str, &str)] = &[
    ("Dockerfile", "Dockerfile"),
    ("Containerfile", "Dockerfile"),
    (".gitignore", "Git Ignore"),
    (".dockerignore", "Git Ignore"),
    (".npmignore", "Git Ignore"),
    (".prettierignore", "Git Ignore"),
    (".eslintignore", "Git Ignore"),
];
const BY_EXT: &[(&str, &str)] = &[
    ("ts", "TypeScript"),
    ("mts", "TypeScript"),
    ("cts", "TypeScript"),
    ("tsx", "TypeScriptReact"),
    ("js", "JavaScript (Babel)"),
    ("jsx", "JavaScript (Babel)"),
    ("mjs", "JavaScript (Babel)"),
    ("cjs", "JavaScript (Babel)"),
    ("json", "JSON"),
    ("jsonc", "JSON"),
    ("json5", "JSON"),
    ("yml", "YAML"),
    ("yaml", "YAML"),
    ("toml", "TOML"),
    ("md", "Markdown"),
    ("sh", "Bourne Again Shell (bash)"),
    ("bash", "Bourne Again Shell (bash)"),
    ("ps1", "PowerShell"),
    ("psm1", "PowerShell"),
    ("dockerfile", "Dockerfile"),
];

/// The syntax for `path` by file name, then extension (the table above first, then the
/// set's own extension list); None (plain) for anything unknown.
fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let named = |n: &str| SYNTAXES.find_syntax_by_name(n);
    if let Some(&(_, syn)) = BY_NAME.iter().find(|(n, _)| *n == name) {
        return named(syn);
    }
    let ext = name.rsplit_once('.')?.1;
    if ext.is_empty() {
        return None;
    }
    let lower = ext.to_ascii_lowercase();
    match BY_EXT.iter().find(|(e, _)| *e == lower) {
        Some(&(_, syn)) => named(syn),
        None => SYNTAXES.find_syntax_by_extension(ext),
    }
}

/// Spans of `line` (text plus the `\n` syntect wants), clipped to the first `text_len` bytes.
fn run(h: &mut HighlightLines, line: &str, text_len: usize) -> Vec<(Range<usize>, Color32)> {
    let Ok(parts) = h.highlight_line(line, &SYNTAXES) else {
        return Vec::new();
    };
    let mut spans: Vec<(Range<usize>, Color32)> = Vec::new();
    let mut at = 0;
    for (style, s) in parts {
        let range = at..(at + s.len()).min(text_len);
        at += s.len();
        if range.start >= range.end {
            continue;
        }
        let c = style.foreground;
        // lifted toward white so dim theme colours (comments) stay readable on the red/green
        // row and word-diff backgrounds
        let lift = |v: u8| (f32::from(v) * 0.7 + 255.0 * 0.3).round() as u8;
        let color = Color32::from_rgb(lift(c.r), lift(c.g), lift(c.b));
        match spans.last_mut() {
            Some((last, lc)) if *lc == color && last.end == range.start => last.end = range.end,
            _ => spans.push((range, color)),
        }
    }
    spans
}

#[cfg(test)]
mod tests;
