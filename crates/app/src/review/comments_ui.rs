//! Comment threads on the review screen: where each thread goes (under a diff row, in the
//! outdated section, in the file header, in the conversation tab), the card heights the
//! virtualized diff reserves for them, and card painting.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use eframe::egui::{
    vec2, Align2, Color32, FontId, Galley, Painter, Pos2, Rect, Shape, Stroke, StrokeKind,
};
use prcore::comment_type::{split_prefix, CommentType};
use prcore::comments::{Comment, CommentId, OutdatedReason, Thread};
use prcore::diff::{DiffFile, Row, RowKind};

use super::diffview::FileView;

/// Space above, between and below the cards under one row.
pub const CARD_MARGIN: f32 = 4.0;
pub const CARD_PAD: f32 = 8.0;
pub const TITLE_H: f32 = 18.0;
/// A collapsed card: its title line only.
pub const COLLAPSED_H: f32 = 2.0 * CARD_PAD + TITLE_H;

/// Where a thread belongs, from its root's anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// No anchor: a PR-level comment (conversation tab).
    Pr,
    /// Anchor without lines: a file-level comment (file header).
    File(&'a str),
    /// `to` is a line of the new file, else `from` one of the old file.
    Line {
        path: &'a str,
        from: Option<u32>,
        to: Option<u32>,
    },
}

pub fn scope(t: &Thread) -> Scope<'_> {
    match &t.root.anchor {
        None => Scope::Pr,
        Some(a) if a.from.is_none() && a.to.is_none() => Scope::File(&a.path),
        Some(a) => Scope::Line {
            path: &a.path,
            from: a.from,
            to: a.to,
        },
    }
}

/// Counts as unresolved: not resolved, and not a deleted comment with only deleted replies.
pub fn is_open(t: &Thread) -> bool {
    let gone = t.root.deleted && t.replies.iter().all(|r| r.deleted);
    !(t.root.resolved || gone)
}

fn path_of(t: &Thread) -> Option<&str> {
    t.root.anchor.as_ref().map(|a| a.path.as_str())
}

/// The root comment carries Bitbucket's outdated flag.
pub fn is_flagged(t: &Thread) -> bool {
    t.root.anchor.as_ref().is_some_and(|a| a.outdated)
}

/// Why thread `t`, shown in `file_path`'s outdated section, is there: its old path, the
/// Bitbucket flag, else its line is not in the diff.
pub fn outdated_reason(t: &Thread, file_path: &str) -> OutdatedReason {
    match path_of(t) {
        Some(p) if p != file_path => OutdatedReason::OldPath,
        _ if is_flagged(t) => OutdatedReason::Flagged,
        _ => OutdatedReason::LineNotInDiff,
    }
}

/// Threads of one file, by where they are shown.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Placement {
    /// (diff row, thread indices) sorted by row; the cards go under that row.
    pub inline: Vec<(usize, Vec<usize>)>,
    /// Inline threads whose line is not in the current patch.
    pub outdated: Vec<usize>,
    /// File-level threads.
    pub file_level: Vec<usize>,
}

/// Places the threads `ids` (indices into `all`) on one file's `rows`. An inline thread
/// goes under the row showing new line `to`; if `to` is unset or not in the patch, under
/// the row showing old line `from`; if neither is shown it is outdated.
pub fn place(rows: &[Row], all: &[Thread], ids: &[usize]) -> Placement {
    let mut out = Placement::default();
    if ids.is_empty() {
        return out;
    }
    // first row showing each line number on each side; O(rows) once per placement
    let (mut new_at, mut old_at) = (HashMap::new(), HashMap::new());
    for (i, r) in rows.iter().enumerate() {
        if matches!(r.kind, RowKind::Context | RowKind::Add) {
            if let Some(n) = r.new_line {
                new_at.entry(n).or_insert(i);
            }
        }
        if matches!(r.kind, RowKind::Context | RowKind::Del) {
            if let Some(n) = r.old_line {
                old_at.entry(n).or_insert(i);
            }
        }
    }
    let mut by_row: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &t in ids {
        let row = match scope(&all[t]) {
            Scope::Pr => continue,
            Scope::File(_) => {
                out.file_level.push(t);
                continue;
            }
            // flagged by Bitbucket: its line numbers belong to an older version of the
            // file and may point at other content here
            Scope::Line { .. } if is_flagged(&all[t]) => None,
            // the new side wins when its line is shown; otherwise the old side is tried
            // before the thread counts as outdated
            Scope::Line { from, to, .. } => to
                .and_then(|n| new_at.get(&n))
                .or_else(|| from.and_then(|n| old_at.get(&n))),
        };
        match row {
            Some(&r) => by_row.entry(r).or_default().push(t),
            None => out.outdated.push(t),
        }
    }
    out.inline = by_row.into_iter().collect();
    out
}

/// Per-view comment layout state, kept in `FileView` (fresh for every new patch).
#[derive(Debug, Default, Clone)]
pub struct CommentLayout {
    pub placement: Placement,
    /// Threads version the placement was computed for.
    placed_for: Option<u64>,
    /// (threads version, card states version, width, extras key) the row extras were set for.
    sized_for: Option<(u64, u64, u32, u64)>,
    /// Card width the heights were measured at; cards are painted at this width.
    pub card_w: f32,
}

/// Every thread of the open PR plus the per-thread expanded state.
#[derive(Default)]
pub struct Comments {
    pub threads: Vec<Thread>,
    /// Comments the provider could not decode.
    pub skipped: usize,
    pub loading: bool,
    pub error: Option<String>,
    expanded: HashMap<CommentId, bool>,
    /// Comment ids whose long body is shown in full ("Show more").
    full: HashSet<CommentId>,
    /// Diff files: current paths, and old path -> current path for renames. None until
    /// the file index arrived (every anchored thread is then routed by its own path).
    files: Option<(HashSet<String>, HashMap<String, String>)>,
    /// Current file path -> threads anchored on that path.
    by_path: HashMap<String, Vec<usize>>,
    /// Current file path -> threads anchored on its old path (shown as outdated there).
    renamed: HashMap<String, Vec<usize>>,
    /// Threads anchored on a path that is not in the diff (outdated: a later push removed
    /// the file or reverted its changes).
    pub unplaced: Vec<usize>,
    /// The same threads by their path, sorted (the file list's "not in diff" entries).
    ghosts: BTreeMap<String, Vec<usize>>,
    pub pr_level: Vec<usize>,
    threads_ver: u64,
    heights_ver: u64,
}

/// More height per row for `Comments::layout_with`: (rows, placement, width, measure) ->
/// (row, px) pairs.
pub type ExtraFn<'a> = &'a mut dyn FnMut(
    &[Row],
    &Placement,
    f32,
    &mut dyn FnMut(&str, f32) -> f32,
) -> Vec<(usize, f32)>;

/// What `Comments::layout` did to a view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Relayout {
    /// Row heights changed (repaint).
    pub changed: bool,
    /// Scroll offset that keeps the top row where it was; None for a view laid out for
    /// the first time (its remembered offset already includes the cards).
    pub scroll_y: Option<f32>,
}

impl Comments {
    pub fn new() -> Comments {
        Comments {
            loading: true,
            ..Default::default()
        }
    }

    /// New threads from the service (open or refresh); card states are kept by root id.
    pub fn set(&mut self, threads: Vec<Thread>, skipped: usize) {
        self.threads = threads;
        self.skipped = skipped;
        self.loading = false;
        self.error = None;
        self.route();
    }

    /// The PR's file index (on open); threads are routed to these files.
    pub fn set_files(&mut self, files: &[DiffFile]) {
        let paths = files.iter().map(|f| f.path.clone()).collect();
        let old = files
            .iter()
            .filter_map(|f| Some((f.old_path.clone()?, f.path.clone())))
            .collect();
        self.files = Some((paths, old));
        self.route();
    }

    /// Puts every thread in exactly one bucket: PR-level, a file's own path, a renamed
    /// file's old path, or unplaced.
    fn route(&mut self) {
        self.by_path.clear();
        self.renamed.clear();
        self.unplaced.clear();
        self.ghosts.clear();
        self.pr_level.clear();
        for (i, t) in self.threads.iter().enumerate() {
            let Some(p) = path_of(t) else {
                self.pr_level.push(i);
                continue;
            };
            match &self.files {
                Some((paths, _)) if paths.contains(p) => {
                    self.by_path.entry(p.to_string()).or_default().push(i)
                }
                Some((_, old)) => match old.get(p) {
                    Some(new) => self.renamed.entry(new.clone()).or_default().push(i),
                    None => {
                        self.unplaced.push(i);
                        self.ghosts.entry(p.to_string()).or_default().push(i);
                    }
                },
                None => self.by_path.entry(p.to_string()).or_default().push(i),
            }
        }
        self.threads_ver += 1;
    }

    /// Resolve / Unresolve shown before (or reverted after) the server answered.
    pub fn set_resolved(&mut self, root_id: &CommentId, resolved: bool) {
        if let Some(t) = self.threads.iter_mut().find(|t| t.root.id == *root_id) {
            t.root.resolved = resolved;
            self.threads_ver += 1;
        }
    }

    /// A comment just posted from a draft, shown without refetching.
    pub fn add_posted(&mut self, c: Comment) {
        let mut all: Vec<Comment> = std::mem::take(&mut self.threads)
            .into_iter()
            .flat_map(|t| std::iter::once(t.root).chain(t.replies))
            .filter(|x| x.id != c.id)
            .collect();
        all.push(c);
        self.threads = prcore::comments::build_threads(all);
        self.route();
    }

    /// Fetching failed; threads already shown stay.
    pub fn fail(&mut self, error: String) {
        self.loading = false;
        self.error = Some(error);
    }

    /// Thread indices anchored on file `path` itself (file-level and inline).
    pub fn for_path(&self, path: &str) -> &[usize] {
        self.by_path.get(path).map_or(&[], Vec::as_slice)
    }

    /// Threads anchored on the old path of the renamed file `path`.
    pub fn on_old_path(&self, path: &str) -> &[usize] {
        self.renamed.get(path).map_or(&[], Vec::as_slice)
    }

    /// Paths with threads that are not in the diff, sorted.
    pub fn ghost_paths(&self) -> Vec<&str> {
        self.ghosts.keys().map(String::as_str).collect()
    }

    /// Threads on `path` when it is not in the diff.
    pub fn on_ghost(&self, path: &str) -> &[usize] {
        self.ghosts.get(path).map_or(&[], Vec::as_slice)
    }

    /// Why thread `i` is outdated, as far as is known without the file's diff rows: its
    /// file is not in the diff, it is on a renamed file's old path, or Bitbucket flags its
    /// line. A line missing from the patch is only known once the patch is placed.
    pub fn known_outdated(&self, i: usize) -> Option<OutdatedReason> {
        let t = &self.threads[i];
        let p = path_of(t)?;
        if self.on_ghost(p).contains(&i) {
            Some(OutdatedReason::FileNotInDiff)
        } else if self.renamed.values().any(|v| v.contains(&i)) {
            Some(OutdatedReason::OldPath)
        } else if is_flagged(t) && matches!(scope(t), Scope::Line { .. }) {
            Some(OutdatedReason::Flagged)
        } else {
            None
        }
    }

    /// `known_outdated`, else whether `view` (the thread's file, when its patch is laid out
    /// for the current threads) placed thread `i` in its outdated section.
    pub fn outdated_in(&self, i: usize, view: Option<&FileView>) -> Option<OutdatedReason> {
        self.known_outdated(i).or_else(|| {
            let lay = &view?.comments;
            (lay.placed_for == Some(self.threads_ver) && lay.placement.outdated.contains(&i))
                .then_some(OutdatedReason::LineNotInDiff)
        })
    }

    /// Diff files with comments (own or old path), sorted.
    pub fn paths(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .by_path
            .keys()
            .chain(self.renamed.keys())
            .map(String::as_str)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// "3 unresolved" for the top bar; None while loading, or after a failure with no
    /// threads to count (the error is shown instead).
    #[cfg(test)]
    pub fn unresolved_label(&self) -> Option<String> {
        if self.loading || (self.error.is_some() && self.threads.is_empty()) {
            return None;
        }
        Some(format!("{} unresolved", self.unresolved()))
    }

    /// Unresolved threads start expanded, resolved ones collapsed, until toggled.
    pub fn is_expanded(&self, t: &Thread) -> bool {
        self.expanded
            .get(&t.root.id)
            .copied()
            .unwrap_or(!t.root.resolved)
    }

    pub fn toggle(&mut self, root_id: &CommentId) {
        let Some(t) = self.threads.iter().find(|t| t.root.id == *root_id) else {
            return;
        };
        let open = !self.is_expanded(t);
        self.expanded.insert(root_id.clone(), open);
        self.heights_ver += 1;
    }

    /// "Show more" / "Show less" on the long body of comment `id`.
    pub fn toggle_full(&mut self, id: &CommentId) {
        if !self.full.remove(id) {
            self.full.insert(id.clone());
        }
        self.heights_ver += 1;
    }

    /// The layout of thread `t`'s card at `width` in its current state; the one source of
    /// card heights for the diff rows, the sections and the Conversation tab.
    pub fn card(
        &self,
        t: &Thread,
        width: f32,
        measure: &mut dyn FnMut(&str, f32) -> f32,
    ) -> CardLayout {
        card_layout(
            t,
            self.is_expanded(t),
            &|id| self.full.contains(id),
            width,
            measure,
        )
    }

    /// Lays out `view`'s threads at `width`; does nothing unless the threads, a card's
    /// state or the width changed since the last call for this view. A change costs one
    /// placement (only when the threads changed), one measure per card on this file and
    /// one O(rows) offsets rebuild. `scroll_y` is the diff's current scroll offset; the
    /// result says how to move it so the top row stays put.
    #[cfg(test)]
    pub fn layout(
        &self,
        view: &mut FileView,
        width: f32,
        scroll_y: f32,
        measure: &mut dyn FnMut(&str, f32) -> f32,
    ) -> Relayout {
        self.layout_with(view, width, scroll_y, measure, 0, &mut |_, _, _, _| {
            Vec::new()
        })
    }

    /// `layout`, plus more space per row from `extra(rows, placement, width, measure)`
    /// (drafts and the composer: each item's height plus `CARD_MARGIN`), recomputed when
    /// `extra_key` changes.
    pub fn layout_with(
        &self,
        view: &mut FileView,
        width: f32,
        scroll_y: f32,
        measure: &mut dyn FnMut(&str, f32) -> f32,
        extra_key: u64,
        extra: ExtraFn<'_>,
    ) -> Relayout {
        // coarse widths: a window resize re-measures every few pixels, not every pixel
        let width = ((width / WIDTH_STEP).floor() * WIDTH_STEP).max(MIN_CARD_W);
        let FileView {
            rows,
            path,
            offsets,
            comments: lay,
            ..
        } = view;
        let first = lay.placed_for.is_none();
        if lay.placed_for != Some(self.threads_ver) {
            lay.placement = place(rows, &self.threads, self.for_path(path));
            // an old path's lines belong to another version of the file
            lay.placement
                .outdated
                .extend_from_slice(self.on_old_path(path));
            lay.placed_for = Some(self.threads_ver);
        }
        let key = (self.threads_ver, self.heights_ver, width as u32, extra_key);
        if lay.sized_for == Some(key) {
            return Relayout {
                changed: false,
                scroll_y: None,
            };
        }
        lay.sized_for = Some(key);
        let anchor = if first {
            None
        } else {
            offsets.anchor_at(scroll_y)
        };
        lay.card_w = width;
        let mut extras: HashMap<usize, f32> = lay
            .placement
            .inline
            .iter()
            .map(|(row, ids)| {
                let cards = ids
                    .iter()
                    .map(|&i| self.card(&self.threads[i], width, measure).height);
                (*row, stack_height(cards))
            })
            .collect();
        for (row, h) in extra(rows.as_slice(), &lay.placement, width, measure) {
            match extras.get_mut(&row) {
                Some(x) => *x += h,
                None => {
                    extras.insert(row, h + CARD_MARGIN);
                }
            }
        }
        offsets.set_extras(extras);
        Relayout {
            changed: true,
            scroll_y: anchor.map(|a| offsets.y_for(a)),
        }
    }

    /// Unresolved threads per diff file (own path and, for renames, old path). Together
    /// with the PR-level and unplaced ones this counts every thread exactly once.
    #[cfg(test)]
    pub fn unresolved_by_path(&self) -> HashMap<String, usize> {
        let mut out: HashMap<String, usize> = HashMap::new();
        for (p, ids) in self.by_path.iter().chain(self.renamed.iter()) {
            let n = ids.iter().filter(|&&i| is_open(&self.threads[i])).count();
            if n > 0 {
                *out.entry(p.clone()).or_default() += n;
            }
        }
        out
    }

    /// Bumped whenever the threads or their resolved state change (file list marks).
    pub fn version(&self) -> u64 {
        self.threads_ver
    }

    /// Unresolved threads in the whole PR (top bar).
    pub fn unresolved(&self) -> usize {
        self.threads.iter().filter(|t| is_open(t)).count()
    }
}

const WIDTH_STEP: f32 = 8.0;
const MIN_CARD_W: f32 = 160.0;
pub const AUTHOR_H: f32 = 18.0;
pub const COMMENT_GAP: f32 = 6.0;
pub const REPLY_INDENT: f32 = 16.0;

/// Shown instead of a deleted comment's body.
pub const DELETED: &str = "[deleted]";

/// The text a comment shows: its body without a leading `**type:** ` prefix (shown as a
/// badge, see `comment_type`), or the deleted placeholder.
pub fn body_text(c: &Comment) -> &str {
    if c.deleted {
        DELETED
    } else {
        split_prefix(&c.body).1
    }
}

/// The type of a comment, from its leading `**type:** ` prefix (also others' comments).
pub fn comment_type(c: &Comment) -> Option<CommentType> {
    if c.deleted {
        None
    } else {
        split_prefix(&c.body).0
    }
}

/// Wrap width of a body in a card of width `card_w`.
pub fn body_width(card_w: f32, reply: bool) -> f32 {
    let indent = if reply { REPLY_INDENT } else { 0.0 };
    (card_w - 2.0 * CARD_PAD - indent).max(40.0)
}

/// Root first, then the replies.
pub fn comments_of(t: &Thread) -> impl Iterator<Item = &Comment> {
    std::iter::once(&t.root).chain(t.replies.iter())
}

/// Bodies longer than this (chars) start collapsed to their first lines, with "Show more".
pub const LONG_BODY: usize = 3000;
/// A collapsed long body keeps at most this many of its lines...
pub const COLLAPSED_LINES: usize = 12;
/// ...and at most this many chars (long lines wrap into several rows).
pub const COLLAPSED_CHARS: usize = 1200;
/// The "Show more" / "Show less" line under a long body.
pub const MORE_H: f32 = 20.0;

/// A body long enough to be collapsed.
pub fn is_long(c: &Comment) -> bool {
    !c.deleted && c.body.chars().count() > LONG_BODY
}

/// The text a comment's card shows: `body_text`, or for a long body not shown in `full`
/// its first `COLLAPSED_LINES` lines (at most `COLLAPSED_CHARS` chars) and an ellipsis.
pub fn shown_body(c: &Comment, full: bool) -> Cow<'_, str> {
    let text = body_text(c);
    if full || !is_long(c) {
        return Cow::Borrowed(text);
    }
    let mut end = text.len();
    if let Some((i, _)) = text.match_indices('\n').nth(COLLAPSED_LINES - 1) {
        end = i;
    }
    if let Some((i, _)) = text.char_indices().nth(COLLAPSED_CHARS) {
        end = end.min(i);
    }
    Cow::Owned(format!("{}\u{2026}", text[..end].trim_end()))
}

/// Where one comment sits in its card (offsets from the card's top edge).
#[derive(Debug, Clone, PartialEq)]
pub struct CommentBox {
    pub id: CommentId,
    /// Top of the author line; the body starts `AUTHOR_H` below.
    pub top: f32,
    /// Height of the body wrapped at `body_width(card width, reply)`.
    pub body_h: f32,
    pub reply: bool,
    /// Some(shown in full) for a long body: a "Show more" / "Show less" line follows it.
    pub more: Option<bool>,
}

/// A thread card measured at one width: its height and where each comment goes. Both the
/// space reserved for the card and its painting come from this, so they always agree.
#[derive(Debug, Clone, PartialEq)]
pub struct CardLayout {
    pub height: f32,
    /// Empty for a collapsed card.
    pub comments: Vec<CommentBox>,
}

impl CardLayout {
    /// The clickable "Show more" / "Show less" lines of the card painted in `card`.
    pub fn more_rects(&self, card: Rect) -> Vec<(CommentId, Rect)> {
        self.comments
            .iter()
            .filter(|b| b.more.is_some())
            .map(|b| {
                let x = card.left() + CARD_PAD + if b.reply { REPLY_INDENT } else { 0.0 };
                let y = card.top() + b.top + AUTHOR_H + b.body_h;
                (
                    b.id.clone(),
                    Rect::from_min_size(Pos2::new(x, y), vec2(90.0, MORE_H)),
                )
            })
            .collect()
    }
}

/// Lays out thread `t`'s card at `width`; `full(id)` says whether comment `id`'s long body
/// is shown in full; `measure(text, width)` is the wrapped height of a body.
pub fn card_layout(
    t: &Thread,
    expanded: bool,
    full: &dyn Fn(&CommentId) -> bool,
    width: f32,
    measure: &mut dyn FnMut(&str, f32) -> f32,
) -> CardLayout {
    if !expanded {
        return CardLayout {
            height: COLLAPSED_H,
            comments: Vec::new(),
        };
    }
    let mut y = CARD_PAD + TITLE_H;
    let mut comments = Vec::new();
    for (i, c) in comments_of(t).enumerate() {
        let reply = i > 0;
        y += COMMENT_GAP;
        let shown_full = full(&c.id);
        let body_h = measure(&shown_body(c, shown_full), body_width(width, reply));
        let more = is_long(c).then_some(shown_full);
        comments.push(CommentBox {
            id: c.id.clone(),
            top: y,
            body_h,
            reply,
            more,
        });
        y += AUTHOR_H + body_h + if more.is_some() { MORE_H } else { 0.0 };
    }
    CardLayout {
        height: y + CARD_PAD,
        comments,
    }
}

/// A click on a card, applied after drawing (`Comments::toggle` / `toggle_full`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardToggle {
    /// The title: expand or collapse the thread (root comment id).
    Thread(CommentId),
    /// "Show more" / "Show less" under a long body (comment id).
    Body(CommentId),
}

/// Height of one card with every long body collapsed (tests).
#[cfg(test)]
pub fn card_height(
    t: &Thread,
    expanded: bool,
    width: f32,
    measure: &mut dyn FnMut(&str, f32) -> f32,
) -> f32 {
    card_layout(t, expanded, &|_| false, width, measure).height
}

/// Space reserved under a row for a stack of cards.
pub fn stack_height(cards: impl Iterator<Item = f32>) -> f32 {
    cards.map(|h| h + CARD_MARGIN).sum::<f32>() + CARD_MARGIN
}

/// "Outdated: new line 20 is not in this diff" for an inline thread placed as outdated.
/// A thread on a renamed file's old path says so instead.
pub fn outdated_note(t: &Thread, file_path: &str) -> String {
    reason_note(t, outdated_reason(t, file_path))
}

/// "Outdated: <why>" for thread `t` outdated for `reason`; the text before ": " is the
/// card's badge.
pub fn reason_note(t: &Thread, reason: OutdatedReason) -> String {
    let line = match scope(t) {
        Scope::Line { to: Some(n), .. } => format!("new line {n}"),
        Scope::Line { from: Some(n), .. } => format!("old line {n}"),
        _ => String::new(),
    };
    match reason {
        OutdatedReason::FileNotInDiff => "Outdated: file not in this diff".into(),
        OutdatedReason::OldPath => {
            format!("Outdated: on the old path {}", path_of(t).unwrap_or(""))
        }
        OutdatedReason::Flagged if line.is_empty() => "Outdated: changed since".into(),
        OutdatedReason::Flagged => format!("Outdated: {line} changed since this comment"),
        OutdatedReason::LineNotInDiff if line.is_empty() => "Outdated".into(),
        OutdatedReason::LineNotInDiff => format!("Outdated: {line} is not in this diff"),
    }
}

fn replies_text(n: usize) -> String {
    match n {
        0 => "no replies".into(),
        1 => "1 reply".into(),
        n => format!("{n} replies"),
    }
}

// ---- painting ----

pub const BODY_SIZE: f32 = 13.0;
const CARD_BG: Color32 = Color32::from_rgb(34, 37, 46);
const CARD_BG_HOVER: Color32 = Color32::from_rgb(42, 46, 58);
const OPEN: Color32 = crate::theme::AMBER;
const QUIET: Color32 = crate::theme::GREY;
const TEXT: Color32 = crate::theme::CARD_TEXT;
const RULE: Color32 = Color32::from_rgb(70, 74, 88);
const LINK: Color32 = Color32::from_rgb(110, 160, 240);

/// A comment body laid out at `width`; the same call measures and paints, so the
/// reserved height always matches what is drawn.
pub fn body_galley(painter: &Painter, text: &str, width: f32, color: Color32) -> Arc<Galley> {
    painter.layout(
        text.to_owned(),
        FontId::proportional(BODY_SIZE),
        color,
        width,
    )
}

/// `measure` for `card_height` / `Comments::layout`, backed by egui's text layout.
pub fn measure_with(painter: &Painter) -> impl FnMut(&str, f32) -> f32 + '_ {
    move |text, width| body_galley(painter, text, width, TEXT).size().y
}

/// The clickable title row of a card occupying `card`.
pub fn title_rect(card: Rect) -> Rect {
    Rect::from_min_size(card.min, vec2(card.width(), COLLAPSED_H))
}

/// Paints thread `t` into `card`, laid out by `lay` (`card_layout` at `card`'s width).
/// `note` is an extra badge.
pub fn paint_card(
    painter: &Painter,
    card: Rect,
    t: &Thread,
    lay: &CardLayout,
    note: Option<&str>,
    hovered: bool,
) {
    let expanded = !lay.comments.is_empty();
    let p = painter.with_clip_rect(card.intersect(painter.clip_rect()));
    let open = is_open(t);
    let fill = if hovered { CARD_BG_HOVER } else { CARD_BG };
    p.rect_filled(card, 4.0, fill);
    let edge = if open { OPEN.gamma_multiply(0.6) } else { RULE };
    p.rect_stroke(card, 4.0, Stroke::new(1.0, edge), StrokeKind::Inside);

    // title: disclosure triangle, status, replies, note
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    let tx = card.left() + CARD_PAD + 5.0;
    let tri = if expanded {
        vec![
            Pos2::new(tx - 4.5, cy - 2.5),
            Pos2::new(tx + 4.5, cy - 2.5),
            Pos2::new(tx, cy + 3.0),
        ]
    } else {
        vec![
            Pos2::new(tx - 2.5, cy - 4.5),
            Pos2::new(tx + 3.0, cy),
            Pos2::new(tx - 2.5, cy + 4.5),
        ]
    };
    p.add(Shape::convex_polygon(tri, QUIET, Stroke::NONE));
    let font = FontId::proportional(BODY_SIZE);
    let mut x = card.left() + CARD_PAD + 16.0;
    let mut piece = |text: &str, color: Color32| {
        let r = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            text,
            font.clone(),
            color,
        );
        x = r.right() + 8.0;
    };
    let replies = replies_text(t.replies.len());
    let status = if t.root.resolved {
        ("Resolved", QUIET)
    } else {
        ("Unresolved", OPEN)
    };
    if expanded {
        piece(status.0, status.1);
        piece(&replies, QUIET);
    } else {
        let who = &crate::review::authors::author_label(&t.root).text;
        let text = if t.root.resolved {
            format!("Resolved \u{00b7} thread by {who}, {replies} \u{2014} click to expand")
        } else {
            format!("Thread by {who}, {replies} \u{2014} click to expand")
        };
        piece(&text, QUIET);
    }
    if let Some(n) = note {
        // "Outdated: why": the part before ": " as a boxed badge, the reason after it
        let (badge, why) = n.split_once(": ").unwrap_or((n, ""));
        let r = p.text(
            Pos2::new(x + 4.0, cy),
            Align2::LEFT_CENTER,
            badge,
            FontId::proportional(11.0),
            OPEN,
        );
        p.rect_stroke(
            r.expand2(vec2(4.0, 2.0)),
            3.0,
            Stroke::new(1.0, OPEN),
            StrokeKind::Inside,
        );
        if !why.is_empty() {
            p.text(
                Pos2::new(r.right() + 12.0, cy),
                Align2::LEFT_CENTER,
                why,
                font.clone(),
                OPEN,
            );
        }
    }

    // the comments at the offsets `card_layout` measured, bodies wrapped at the same width
    let more = lay.more_rects(card);
    for (c, b) in comments_of(t).zip(&lay.comments) {
        let y = card.top() + b.top;
        let x0 = card.left() + CARD_PAD + if b.reply { REPLY_INDENT } else { 0.0 };
        let color = if c.deleted { QUIET } else { TEXT };
        let text = shown_body(c, b.more.unwrap_or(true));
        let body = body_galley(&p, &text, body_width(card.width(), b.reply), color);
        if b.reply {
            let lx = x0 - REPLY_INDENT / 2.0;
            p.line_segment(
                [
                    Pos2::new(lx, y + 2.0),
                    Pos2::new(lx, y + AUTHOR_H + b.body_h),
                ],
                Stroke::new(2.0, RULE),
            );
        }
        let label = crate::review::authors::author_label(c);
        let mut name = p.text(
            Pos2::new(x0, y + AUTHOR_H / 2.0),
            Align2::LEFT_CENTER,
            &label.text,
            FontId::proportional(BODY_SIZE),
            Color32::WHITE,
        );
        if label.bot {
            let tag = p.text(
                Pos2::new(name.right() + 6.0, y + AUTHOR_H / 2.0),
                Align2::LEFT_CENTER,
                "bot",
                FontId::proportional(10.0),
                QUIET,
            );
            p.rect_stroke(
                tag.expand2(eframe::egui::vec2(3.0, 1.0)),
                3.0,
                Stroke::new(1.0, QUIET),
                eframe::egui::StrokeKind::Inside,
            );
            name = tag.expand2(eframe::egui::vec2(3.0, 0.0));
        }
        let mut when = c.created_on.format("%Y-%m-%d %H:%M").to_string();
        if c.updated_on != c.created_on && !c.deleted {
            when.push_str(" (edited)");
        }
        let when = p.text(
            Pos2::new(name.right() + 8.0, y + AUTHOR_H / 2.0),
            Align2::LEFT_CENTER,
            when,
            FontId::proportional(12.0),
            QUIET,
        );
        if let Some(t) = comment_type(c) {
            super::types_ui::paint_badge(&p, Pos2::new(when.right() + 8.0, y + AUTHOR_H / 2.0), t);
        }
        p.galley(Pos2::new(x0, y + AUTHOR_H), body, color);
        if let (Some(full), Some((_, r))) = (b.more, more.iter().find(|(id, _)| *id == c.id)) {
            let label = if full { "Show less" } else { "Show more" };
            p.text(
                r.left_center(),
                Align2::LEFT_CENTER,
                label,
                FontId::proportional(12.0),
                LINK,
            );
        }
    }
}

#[cfg(test)]
mod tests;
