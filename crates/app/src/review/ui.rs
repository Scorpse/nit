//! Review screen: top bar, file list (left) and the selected file's virtualized diff.
//!
//! `ReviewScreen` is the testable state (events in, patch requests out); the `draw_*`
//! functions only paint it. `ReviewHost` owns the service and is what `App` keeps.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::containers::scroll_area::{DragScroll, ScrollSource};
use eframe::egui::{
    self, text::LayoutJob, vec2, Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense,
    TextFormat, Ui, Vec2,
};
use prcore::comments::OutdatedReason;
use prcore::diff::{DiffFile, FileStatus, RowKind};
use prcore::model::{PullRequest, RepoRef};
use store::{Store, StoreWriter};

use super::actions_ui::{
    action_buttons, action_confirm, action_notes, open_pr_page, open_web_label, ActionAct,
    ReviewActions, ReviewContext,
};
use super::comments_ui::{
    is_flagged, is_open, measure_with, outdated_note, paint_card, reason_note, scope, title_rect,
    CardLayout, CardToggle, Comments, Scope, CARD_MARGIN,
};
use super::composer::{
    confirm_dialog, draft_card, draft_height, draft_widget, drafts_bar, editor_card, editor_height,
    editor_widget, reply_widgets, thread_buttons, toast, Act, Composer, MemoryPoster, Poster, Slot,
    Target,
};
use super::diffview::{
    minimap_ticks, styled_columns, tick_at, DiffCursor, DiffView, FileView, Offsets, Tick,
    TickKind, ROW_H,
};
use super::files::{
    comment_marks, ghost_entries, status_letter, CommentMark, Cursor, FileList, ListRow, Tri,
    ViewMode, VIEW_META_KEY,
};
use super::keymap::{help_rows, Action, Feed, Group, KeyIn, KeyState};
use super::search::{find_all, ignores_case, matching_rows, step_match};
use super::selection::{
    autoscroll_delta, col_to_x, hit_test, line_range, press_action, row_span, selected_text,
    x_to_col, Hit, Pos, Press, Selection, SelectionState,
};
use super::service::{ReviewEvent, ReviewService};
use super::split::{
    comment_target, selected_text_split, split_fits, DiffLayout, SplitMap, SplitSide,
    LAYOUT_META_KEY,
};
use crate::backend::Backend;
use crate::demo::DemoReview;
use crate::list::{reveal_offset, Nav};
use crate::lock::lock_ok;
use crate::ui::{App, Screen};
use prcore::draft::{DraftKind, Side};

const LIST_ROW_H: f32 = 24.0;
/// The comment marker strip right of the diff, and the gap before it.
const MINIMAP_W: f32 = 10.0;
const MINIMAP_GAP: f32 = 2.0;
const MONO: f32 = 13.0;

use crate::theme::{AMBER, GREEN, GREY, RED};
const DIM: Color32 = Color32::from_rgb(110, 110, 120);
/// `DIM` and `GREY` on a selected row: the selection fill is too light for the plain ones
/// (contrast about 1.7 and 2.8), these reach 3.0 or more on it.
const DIM_SELECTED: Color32 = Color32::from_rgb(170, 175, 190);
const GREY_SELECTED: Color32 = Color32::from_rgb(185, 190, 205);

/// The dimmed directory colour of a path, and the name colour of a viewed file, for a row
/// that is `selected` or not.
fn viewed_colors(selected: bool) -> (Color32, Color32) {
    if selected {
        (DIM_SELECTED, GREY_SELECTED)
    } else {
        (DIM, GREY)
    }
}
const DRAFT_BLUE: Color32 = crate::theme::DRAFT;
const SELECTION_BG: Color32 = Color32::from_rgba_premultiplied(40, 70, 120, 120);

pub const STALE_TEXT: &str = "PR changed, reload";

/// The title always keeps at least this much of the top bar (truncated with an ellipsis).
pub const TITLE_MIN_W: f32 = 160.0;
/// The title gets up to this much before the repository and branches are shown.
pub const TITLE_PREF_W: f32 = 420.0;

/// Top bar buttons that `plan_top_bar` places inline or in the More menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarButton {
    Refresh,
    Approve,
    RequestChanges,
    OpenWeb,
}

/// Natural widths (px, with their spacing) of the top bar's pieces.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BarWidths {
    /// Always shown: Back, the unresolved count, Approve, Request changes, More, separators.
    pub fixed: f32,
    pub title: f32,
    /// "#65 in workspace/repo".
    pub meta: f32,
    /// "source -> destination".
    pub branches: f32,
    pub refresh: f32,
    pub open_web: f32,
}

/// What the top bar shows at one width.
#[derive(Debug, Clone, PartialEq)]
pub struct BarPlan {
    /// Width given to the (truncated) title.
    pub title_w: f32,
    pub meta: bool,
    pub branches: bool,
    /// Buttons on the bar; Approve and Request changes are always here.
    pub inline: Vec<BarButton>,
    /// Buttons moved into the More menu.
    pub more: Vec<BarButton>,
}

/// Fits the top bar into `width`. Approve and Request changes always stay inline. Going
/// narrower, the branches are hidden first, then the repository, then Refresh comments and
/// Open in Bitbucket move into More; the title is truncated to what is left, never below
/// `TITLE_MIN_W` (and gets up to `TITLE_PREF_W` before the repository and branches show).
pub fn plan_top_bar(width: f32, w: &BarWidths) -> BarPlan {
    let title_min = w.title.min(TITLE_MIN_W);
    let mut room = width - w.fixed - title_min;
    let take = |need: f32, room: &mut f32| {
        let fits = need <= *room;
        if fits {
            *room -= need;
        }
        fits
    };
    let open_web = take(w.open_web, &mut room);
    let refresh = open_web && take(w.refresh, &mut room);
    let grow = room.clamp(0.0, (w.title.min(TITLE_PREF_W) - title_min).max(0.0));
    room -= grow;
    let meta = refresh && take(w.meta, &mut room);
    let branches = meta && take(w.branches, &mut room);
    let rest = room.clamp(0.0, (w.title - title_min - grow).max(0.0));
    let mut inline = vec![BarButton::Approve, BarButton::RequestChanges];
    let mut more = Vec::new();
    for (b, on) in [
        (BarButton::Refresh, refresh),
        (BarButton::OpenWeb, open_web),
    ] {
        if on {
            inline.push(b);
        } else {
            more.push(b);
        }
    }
    BarPlan {
        title_w: title_min + grow + rest,
        meta,
        branches,
        inline,
        more,
    }
}

/// The top bar's natural widths at the current style (see `plan_top_bar`).
fn bar_widths(ui: &Ui, s: &ReviewScreen, meta: &str, branches: &str) -> BarWidths {
    let style = ui.style();
    let gap = style.spacing.item_spacing.x;
    let pad = 2.0 * style.spacing.button_padding.x;
    let body = egui::TextStyle::Body.resolve(style);
    let text = |t: &str, font: FontId| {
        ui.painter()
            .layout_no_wrap(t.to_owned(), font, Color32::WHITE)
            .size()
            .x
    };
    let button = |t: &str| text(t, egui::TextStyle::Button.resolve(style)) + pad + gap;
    // a separator: its spacing plus the gap after it
    let sep = 6.0 + gap;
    let b = s.actions.buttons(&s.pr);
    let count = if s.comments.loading {
        text("Loading comments...", body.clone()) + gap + 2.0 * style.spacing.interact_size.y
    } else {
        let n = s.comments.unresolved();
        text(&format!("{n} unresolved"), body.clone()) + gap
    };
    let spinner = if s.actions.running() {
        style.spacing.interact_size.y + gap
    } else {
        0.0
    };
    BarWidths {
        fixed: button("Back")
            + 3.0 * sep
            + count
            + button(super::actions_ui::label(b.approve))
            + button(super::actions_ui::label(b.request))
            + spinner
            + button("More")
            + button(super::export_ui::COPY_LABEL)
            // rounding and frame margins
            + 12.0,
        title: text(&s.pr.title, FontId::proportional(15.0)) + gap,
        meta: text(meta, body) + gap,
        branches: text(branches, egui::TextStyle::Monospace.resolve(style)) + gap,
        refresh: button("Refresh comments"),
        open_web: button("Open in Bitbucket"),
    }
}

/// Opens the review screen for `pr` (the detail pane's Review button and its Enter
/// shortcut). The service starts on the next frame of the review screen. The button is
/// hidden for forks; should one get here anyway, the service refuses it with a visible
/// "Pull requests from forks are not supported" error. Only open pull requests are reviewed
/// (the button is disabled for the others); anything else is ignored here.
pub fn open_review(app: &mut App, repo: RepoRef, pr: PullRequest) {
    if pr.state != prcore::model::PrState::Open {
        return;
    }
    app.review.context = app.review_context(&repo, pr.id);
    app.review.pending = Some((repo, pr));
    app.screen = Screen::Review;
}

/// j/k/v act only when no text field has keyboard focus and no command modifier is held
/// (Ctrl+V, Alt+J and the like belong to someone else).
pub fn nav_keys_allowed(wants_keyboard: bool, m: egui::Modifiers) -> bool {
    !(wants_keyboard || m.ctrl || m.alt || m.command || m.mac_cmd)
}

/// Review shortcuts (j/k/v/c): as `nav_keys_allowed`, and never while a modal is open.
pub fn review_keys_allowed(wants_keyboard: bool, m: egui::Modifiers, modal_open: bool) -> bool {
    !modal_open && nav_keys_allowed(wants_keyboard, m)
}

/// File list keys (arrows, paging, Left / Right, Space, Enter): as `review_keys_allowed`,
/// and not while the pointer is over the central pane, whose scroll area keeps egui's own
/// arrow and page scrolling.
pub fn list_keys_allowed(
    wants_keyboard: bool,
    m: egui::Modifiers,
    modal_open: bool,
    pointer_over_center: bool,
) -> bool {
    !pointer_over_center && review_keys_allowed(wants_keyboard, m, modal_open)
}

/// A file list key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKey {
    Nav(Nav),
    Left,
    Right,
    /// Toggle viewed (a file) or the tri-state box (a directory).
    Space,
    /// On a file: show its diff.
    Enter,
}

impl ListKey {
    fn pressed(i: &egui::InputState) -> Option<ListKey> {
        if let Some(n) = Nav::pressed(i) {
            return Some(ListKey::Nav(n));
        }
        [
            (Key::ArrowLeft, ListKey::Left),
            (Key::ArrowRight, ListKey::Right),
            (Key::Space, ListKey::Space),
            (Key::Enter, ListKey::Enter),
        ]
        .into_iter()
        .find_map(|(k, v)| i.key_pressed(k).then_some(v))
    }
}

/// How often the open review asks the server whether the PR moved.
pub const RECHECK_EVERY: Duration = Duration::from_secs(60);

/// The periodic part of the head re-check: `RECHECK_EVERY` after the last one, and never while
/// the window is known to be unfocused (`focused` None: unknown, counts as focused).
pub fn recheck_due(now: Instant, last: Instant, focused: Option<bool>) -> bool {
    focused != Some(false) && now.duration_since(last) >= RECHECK_EVERY
}

/// When to re-check the PR's head while the review is open: when the window gains focus,
/// and every `RECHECK_EVERY` while it is focused (the service coalesces overlapping ones).
#[derive(Debug)]
pub struct RecheckTimer {
    last: Instant,
    focused: Option<bool>,
}

impl RecheckTimer {
    pub fn new(now: Instant) -> RecheckTimer {
        RecheckTimer {
            last: now,
            focused: None,
        }
    }

    /// True when a recheck is due this frame; `focused` is the window's focus now (None:
    /// unknown).
    pub fn due(&mut self, now: Instant, focused: Option<bool>) -> bool {
        let gained = focused == Some(true) && self.focused == Some(false);
        if focused.is_some() {
            self.focused = focused;
        }
        if gained || recheck_due(now, self.last, focused) {
            self.last = now;
            return true;
        }
        false
    }
}

/// State of one open review.
pub struct ReviewScreen {
    pub repo: RepoRef,
    pub pr: PullRequest,
    store: Arc<Mutex<Store>>,
    /// Viewed marks and the file view are stored here, off the UI thread, in click order;
    /// dropping the screen stores what is still queued.
    writes: StoreWriter,
    pub phase: Option<String>,
    pub failed: Option<String>,
    pub stale: bool,
    /// None until the file index arrived.
    pub files: Option<FileList>,
    /// The line counts could not be read (the list and diffs still work).
    pub counts_error: Option<String>,
    pub diff: DiffView,
    pub patch_errors: HashMap<String, String>,
    pub store_error: Option<String>,
    /// Paths to ask the service for (priority), drained each frame.
    requests: Vec<String>,
    /// Scroll offset to apply once to the diff area (the file just changed).
    restore_scroll: Option<Vec2>,
    /// File list scroll to force once (keyboard navigation), and the last list viewport.
    list_scroll: Option<f32>,
    /// Scroll the Conversation tab to its end once the comments are in (screenshot scene).
    conversation_to_end: bool,
    list_view: (f32, f32),
    /// First patch arrival and the time its colours took (bench output).
    first_patch_at: Option<Instant>,
    pub highlight_ms: Option<f32>,
    /// Milestones since the screen opened (bench output).
    pub timings: OpenTimings,
    /// The diff's first visible row, as last reported to the service.
    visible_sent: Option<(String, usize)>,
    /// The diff's first visible row this frame.
    visible_now: Option<(String, usize)>,
    pub comments: Comments,
    pub tab: Tab,
    /// "Refresh comments" was pressed; the host asks the service, drained each frame.
    comments_refresh: bool,
    /// Drafts, submit, reply and resolve.
    pub composer: Composer,
    /// Approve / Request changes and their confirmation.
    pub actions: ReviewActions,
    /// (threads version, drafts version) the file list's comment marks were made for.
    marks_key: Option<(u64, u64)>,
    /// The central pane (diff or conversation) of the last frame: arrow keys over it
    /// scroll it instead of moving the file list.
    center_rect: Option<Rect>,
    /// The selection changed outside the list (open, links, j/k): scroll it into view once
    /// the list's viewport is known.
    reveal_next: bool,
    /// The Conversation tab leaves out threads known to be outdated.
    pub hide_outdated: bool,
    /// A Conversation click asked for this thread (root id): the diff scrolls to its row
    /// once the file's patch is laid out.
    pending_reveal: Option<prcore::comments::CommentId>,
    /// Unified or split, as chosen (remembered in the store).
    pub layout: DiffLayout,
    /// Whether the last frame showed the split view (the choice, unless the window is too
    /// narrow): the coordinates the diff's scroll offset and selection are in.
    split_shown: bool,
    /// The vim-style keys are on (Settings, copied from the host every frame).
    pub vim: bool,
    keys: KeyState,
    pub help_open: bool,
    /// Visual line mode: the unified row it started on.
    visual: Option<usize>,
    pub search: SearchBar,
    /// The diff viewport of the last frame: (scroll offset, height).
    diff_viewport: (f32, f32),
}

/// The `/` search bar of the diff.
#[derive(Debug, Default)]
pub struct SearchBar {
    pub open: bool,
    /// Focus the text field on the next frame.
    focus: bool,
    pub query: String,
    /// Matching rows of `for_key` (path, query): found when a search is confirmed.
    matches: Vec<usize>,
    for_key: Option<(String, String)>,
}

/// What a key asked for that needs the UI (or the host) to carry out.
#[derive(Debug, Default)]
pub struct KeyEffects {
    pub acts: Vec<Act>,
    pub back: bool,
    pub copy: Option<String>,
    pub focus_filter: bool,
}

/// Milliseconds from the review screen's start to its milestones (`--bench-review`).
#[derive(Debug, Default, Clone)]
pub struct OpenTimings {
    start: Option<Instant>,
    /// The file list (`Opened`).
    pub file_list: Option<f32>,
    /// The line counts (`Counts`).
    pub counts: Option<f32>,
    /// The first syntax colours of any file.
    pub first_colour: Option<f32>,
}

impl OpenTimings {
    fn since_start(&self) -> f32 {
        self.start
            .map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0)
    }
}

/// The central pane: the selected file's diff, or the PR-level conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Diff,
    Conversation,
}

impl ReviewScreen {
    pub fn new(repo: RepoRef, pr: PullRequest, store: Arc<Mutex<Store>>) -> ReviewScreen {
        Self::with_in_flight(repo, pr, store, Default::default())
    }

    /// `new`, with the process-wide in-flight registry (drafts loaded with it in place).
    pub fn with_in_flight(
        repo: RepoRef,
        pr: PullRequest,
        store: Arc<Mutex<Store>>,
        in_flight: super::composer::InFlight,
    ) -> ReviewScreen {
        let composer = Composer::with_in_flight(repo.clone(), pr.id, store.clone(), in_flight);
        ReviewScreen {
            composer,
            repo,
            pr,
            writes: StoreWriter::new(store.clone()),
            store,
            phase: Some("Starting...".into()),
            failed: None,
            stale: false,
            files: None,
            counts_error: None,
            diff: DiffView::default(),
            patch_errors: HashMap::new(),
            store_error: None,
            requests: Vec::new(),
            restore_scroll: None,
            list_scroll: None,
            conversation_to_end: false,
            list_view: (0.0, 0.0),
            first_patch_at: None,
            highlight_ms: None,
            timings: OpenTimings {
                start: Some(Instant::now()),
                ..OpenTimings::default()
            },
            visible_sent: None,
            visible_now: None,
            comments: Comments::new(),
            tab: Tab::Diff,
            comments_refresh: false,
            actions: ReviewActions::new(ReviewContext::default()),
            marks_key: None,
            center_rect: None,
            reveal_next: false,
            hide_outdated: false,
            pending_reveal: None,
            layout: DiffLayout::Unified,
            split_shown: false,
            vim: true,
            keys: KeyState::default(),
            help_open: false,
            visual: None,
            search: SearchBar::default(),
            diff_viewport: (0.0, 0.0),
        }
    }

    pub fn apply(&mut self, ev: ReviewEvent) {
        match ev {
            ReviewEvent::Phase(p) => self.phase = Some(p),
            ReviewEvent::Opened { refs, files } => {
                let t = self.timings.since_start();
                self.timings.file_list.get_or_insert(t);
                self.composer.head = refs.head.clone();
                self.composer.revalidate_heads();
                // after a reload the inbox head is unknown: the fetched head is what is shown
                if self.pr.source_commit.is_empty() {
                    self.pr.source_commit = refs.head;
                }
                let (viewed, mode, layout) = {
                    let st = lock_ok(&self.store);
                    let mode = st.get_meta(VIEW_META_KEY).ok().flatten();
                    let layout = st.get_meta(LAYOUT_META_KEY).ok().flatten();
                    (st.viewed_map(&self.repo, self.pr.id), mode, layout)
                };
                self.layout = DiffLayout::parse(layout.as_deref());
                let viewed = viewed.unwrap_or_else(|_| {
                    self.store_error = Some("Could not read viewed files".into());
                    HashMap::new()
                });
                self.comments.set_files(&files);
                let mut list = FileList::new(files, viewed);
                list.set_view(ViewMode::parse(mode.as_deref()));
                let first = list.first_reviewable();
                self.files = Some(list);
                self.refresh_marks(true);
                self.phase = None;
                if let Some(i) = first {
                    self.select(i);
                }
            }
            ReviewEvent::Counts { counts } => {
                let t = self.timings.since_start();
                self.timings.counts.get_or_insert(t);
                if let Some(l) = self.files.as_mut() {
                    l.apply_counts(&counts);
                }
                self.counts_error = None;
                // binary flags take part in the comment marks (outdated line threads)
                self.refresh_marks(true);
            }
            ReviewEvent::CountsFailed(e) => self.counts_error = Some(e),
            ReviewEvent::Patch {
                path,
                rows,
                plain,
                layout,
            } => {
                if self.selected_path() == Some(path.as_str()) {
                    self.patch_errors.remove(&path);
                    self.diff.set_patch(&path, rows, plain, layout);
                    self.first_patch_at.get_or_insert_with(Instant::now);
                }
            }
            ReviewEvent::Highlighted {
                path,
                rows,
                range,
                spans,
            } => {
                self.diff.set_highlight(&path, rows, range, spans);
                if let Some(t) = self.first_patch_at {
                    self.highlight_ms
                        .get_or_insert(t.elapsed().as_secs_f32() * 1000.0);
                }
                let t = self.timings.since_start();
                self.timings.first_colour.get_or_insert(t);
            }
            ReviewEvent::PatchFailed { path, error } => {
                self.patch_errors.insert(path, error);
            }
            ReviewEvent::Failed(msg) => {
                self.failed = Some(msg);
                self.phase = None;
                self.comments.loading = false;
            }
            ReviewEvent::Stale => {
                self.stale = true;
                self.composer.stale = true;
            }
            ReviewEvent::Comments { threads, skipped } => {
                self.comments.set(threads, skipped);
                // resolves still in flight win over what the server sent before them
                self.composer.reapply_pending(&mut self.comments);
                // "Check comments" of unconfirmed drafts: compared with what just arrived
                self.composer
                    .comments_checked(&self.comments, self.actions.ctx.me.as_ref());
                // resolves whose answer was lost: settled against what just arrived
                self.composer.resolves_checked(&self.comments);
                self.refresh_marks(true);
            }
            ReviewEvent::CommentsFailed(e) => self.comments.fail(e),
        }
    }

    /// "Refresh comments": shows the spinner and asks the host to refetch.
    pub fn refresh_comments(&mut self) {
        self.comments.loading = true;
        self.comments_refresh = true;
    }

    pub fn take_comment_refresh(&mut self) -> bool {
        std::mem::take(&mut self.comments_refresh)
    }

    fn update_counts(&mut self) {
        self.refresh_marks(true);
    }

    /// Recomputes the file list's comment marks when the threads or the drafts changed
    /// (or `force`); cheap to call every frame.
    pub fn refresh_marks(&mut self, force: bool) {
        let key = (self.comments.version(), self.composer.version());
        if !force && self.marks_key == Some(key) {
            return;
        }
        let Some(l) = self.files.as_mut() else {
            return;
        };
        self.marks_key = Some(key);
        l.set_marks(comment_marks(
            &self.comments,
            &self.composer.drafts,
            l.files(),
        ));
        l.set_ghosts(ghost_entries(&self.comments, &self.composer.drafts));
    }

    /// Tree / List; remembered in the store for the next review.
    pub fn set_file_view(&mut self, v: ViewMode) {
        let Some(l) = self.files.as_mut() else {
            return;
        };
        if l.view() == v {
            return;
        }
        l.set_view(v);
        self.writes
            .write("Could not save the file view", move |st| {
                st.set_meta(VIEW_META_KEY, v.as_str())
            });
        self.reveal_selected();
    }

    /// Unified / split (the diff's toggle and Alt+S); remembered in the store.
    pub fn set_layout(&mut self, l: DiffLayout) {
        if self.layout == l {
            return;
        }
        self.layout = l;
        self.writes
            .write("Could not save the diff layout", move |st| {
                st.set_meta(LAYOUT_META_KEY, l.as_str())
            });
    }

    /// Carries out a key of the keymap (see `keymap::KEYMAP`); `count` is its prefix.
    pub fn key_action(&mut self, a: Action, count: Option<u32>) -> KeyEffects {
        let mut fx = KeyEffects::default();
        let n = count.unwrap_or(1).max(1) as usize;
        match a {
            Action::NextFile | Action::PrevFile => {
                self.visual = None;
                for _ in 0..n {
                    self.step(a == Action::NextFile);
                }
            }
            Action::ToggleViewed => {
                if let Some(i) = self.files.as_ref().and_then(|l| l.selected()) {
                    self.toggle_viewed(i);
                }
            }
            Action::Open => self.list_key(ListKey::Enter),
            Action::Help => self.help_open = !self.help_open,
            Action::Back => fx.back = true,
            Action::FocusFilter => fx.focus_filter = true,
            Action::CopyPath => fx.copy = self.selected_path().map(str::to_string),
            Action::FileComment => {
                if let Some(p) = self.selected_path() {
                    fx.acts.push(Act::Open(Target::File(p.to_string())));
                }
            }
            Action::ToggleLayout => self.set_layout(self.layout.toggled()),
            Action::Search => {
                self.tab = Tab::Diff;
                self.search.open = true;
                self.search.focus = true;
            }
            Action::Cancel => {
                if self.help_open {
                    self.help_open = false;
                } else if self.search.open {
                    self.search.open = false;
                } else {
                    self.visual = None;
                }
            }
            Action::Comment => self.comment_at_cursor(&mut fx),
            Action::Visual => {
                if self.visual.take().is_none() {
                    if self.cursor().is_none() {
                        self.move_cursor(Action::Down, None);
                    }
                    self.visual = self.cursor().map(|c| c.row);
                    self.update_visual();
                }
            }
            Action::SearchNext | Action::SearchPrev => self.search_step(a == Action::SearchNext, n),
            Action::Down
            | Action::Up
            | Action::HalfDown
            | Action::HalfUp
            | Action::Top
            | Action::Bottom
            | Action::NextHunk
            | Action::PrevHunk
            | Action::SideLeft
            | Action::SideRight => self.move_cursor(a, count),
        }
        fx
    }

    /// The diff cursor of the shown file.
    pub fn cursor(&self) -> Option<DiffCursor> {
        self.diff.view(self.selected_path()?)?.cursor
    }

    /// The shown file's view, when its diff is on screen (Diff tab).
    fn shown_view(&mut self) -> Option<(String, &mut FileView)> {
        if self.tab != Tab::Diff {
            return None;
        }
        let path = self.selected_path()?.to_string();
        let v = self.diff.view_mut(&path)?;
        Some((path, v))
    }

    /// j / k / paging / gg / G / hunks / sides: moves the cursor (shown on the first press
    /// when there is none: at the top of the viewport) and scrolls it into view.
    fn move_cursor(&mut self, a: Action, count: Option<u32>) {
        let split = self.split_shown;
        let (scroll_y, view_h) = self.diff_viewport;
        let Some((path, v)) = self.shown_view() else {
            return;
        };
        let len = display_len(v, split);
        if len == 0 {
            return;
        }
        let n = count.unwrap_or(1).max(1) as usize;
        let start = v.cursor;
        let side = match a {
            Action::SideLeft => SplitSide::Left,
            Action::SideRight => SplitSide::Right,
            _ => start.map_or(SplitSide::Right, |c| c.side),
        };
        let line = match start {
            Some(c) => line_of(v, split, c.row),
            None => shown_offsets(v, split).row_at(scroll_y).unwrap_or(0),
        };
        let row = start.map_or_else(|| row_on(v, split, line, side), |c| c.row);
        let half = ((view_h / 2.0 / ROW_H).floor() as usize).max(1);
        // the first j / k only shows the cursor where the viewport is
        let step = if start.is_some() { n } else { 0 };
        let target = match (a, count) {
            (Action::Down, _) => line + step,
            (Action::Up, _) => line.saturating_sub(step),
            (Action::HalfDown, _) => line + half * n,
            (Action::HalfUp, _) => line.saturating_sub(half * n),
            (Action::Top | Action::Bottom, Some(k)) => line_of(v, split, goto_line(&v.rows, k)),
            (Action::Top, None) => 0,
            (Action::Bottom, None) => len - 1,
            (Action::NextHunk | Action::PrevHunk, _) => {
                let mut r = row;
                for _ in 0..n {
                    match next_hunk(&v.rows, r, a == Action::NextHunk) {
                        Some(h) => r = h,
                        None => break,
                    }
                }
                line_of(v, split, r)
            }
            _ => line,
        }
        .min(len - 1);
        let c = cursor_on(v, split, target, side);
        v.cursor = Some(c);
        let x = self.diff.scroll(&path).x;
        let Some(v) = self.diff.view_mut(&path) else {
            return;
        };
        let offs = shown_offsets(v, split);
        let top = offs.top(target);
        let y = if matches!(a, Action::HalfDown | Action::HalfUp) {
            // the page moves with the cursor: it stays where it was on screen
            Some(scroll_y + top - offs.top(line))
        } else {
            reveal_y(top, scroll_y, view_h)
        };
        self.scroll_to(x, y, scroll_y);
        self.update_visual();
    }

    /// Scrolls the diff to `y` (clamped at the top) unless it is already there.
    fn scroll_to(&mut self, x: f32, y: Option<f32>, now: f32) {
        if let Some(y) = y.map(|y| y.max(0.0)).filter(|y| (y - now).abs() > 0.5) {
            self.restore_scroll = Some(Vec2::new(x, y));
        }
    }

    /// Visual mode: the selection covers whole rows from where it started to the cursor.
    fn update_visual(&mut self) {
        let Some(anchor) = self.visual else {
            return;
        };
        let split = self.split_shown;
        let Some((_, v)) = self.shown_view() else {
            return;
        };
        let Some(c) = v.cursor else {
            return;
        };
        let (a, h) = (line_of(v, split, anchor), line_of(v, split, c.row));
        let side = split.then(|| v.split().side_of(c.row, c.side));
        v.sel.select_lines(a, h, side);
    }

    /// `c`: the composer on the cursor's line and side; without a cursor, the last selected
    /// row as before. On a forge that takes line ranges, a selection over several lines of
    /// the unified view (visual mode, or a mouse selection without a cursor) comments on
    /// that range; otherwise visual mode comments on the cursor row.
    fn comment_at_cursor(&mut self, fx: &mut KeyEffects) {
        let split = self.split_shown;
        let visual = self.visual.take().is_some();
        let multi = self.composer.multi_line();
        let Some((path, v)) = self.shown_view() else {
            return;
        };
        let range = (multi && !split && (visual || v.cursor.is_none()))
            .then(|| v.sel.sel.and_then(|s| line_range(&v.rows, &s)))
            .flatten()
            .map(|(start, (side, line))| Target::Inline {
                path: path.clone(),
                side,
                line,
                start: Some(start),
            });
        let target = match v.cursor {
            _ if range.is_some() => range,
            Some(c) if split => {
                let m = v.split();
                let line = m.lines[m.line_of(c.row)];
                comment_target(&v.rows, line, m.side_of(c.row, c.side)).map(|(side, line)| {
                    Target::Inline {
                        path: path.clone(),
                        side,
                        line,
                        start: None,
                    }
                })
            }
            Some(c) => c_key_target(&v.rows, &path, Some(c.row), None),
            None if !split => c_key_target(&v.rows, &path, None, v.sel.sel.map(|s| s.head.row)),
            None => None,
        };
        if let Some(t) = target {
            fx.acts.push(Act::Open(t));
        }
    }

    /// The rows matching the search query in the shown file, found again only when the file
    /// or the query changed (O(rows), on confirming a search or on n / N).
    fn refresh_matches(&mut self) {
        let Some(path) = self.selected_path().map(str::to_string) else {
            return;
        };
        let key = (path.clone(), self.search.query.clone());
        if self.search.for_key.as_ref() == Some(&key) {
            return;
        }
        self.search.matches = self
            .diff
            .view(&path)
            .map_or_else(Vec::new, |v| matching_rows(&v.rows, &self.search.query));
        self.search.for_key = Some(key);
    }

    /// `n` / `N` (and Enter in the search bar): the cursor to the next / previous match in
    /// screen order, wrapping around.
    fn search_step(&mut self, forward: bool, n: usize) {
        self.refresh_matches();
        let split = self.split_shown;
        let (scroll_y, view_h) = self.diff_viewport;
        let matches = std::mem::take(&mut self.search.matches);
        let Some((path, v)) = self.shown_view() else {
            self.search.matches = matches;
            return;
        };
        let top_line = shown_offsets(v, split).row_at(scroll_y).unwrap_or(0);
        let key = |r: usize| -> (usize, u8) {
            if split {
                let m = v.split();
                (m.line_of(r), m.side_of(r, SplitSide::Right) as u8)
            } else {
                (r, 0)
            }
        };
        let mut at = match v.cursor {
            Some(c) if split => {
                let m = v.split();
                (m.line_of(c.row), m.side_of(c.row, c.side) as u8)
            }
            Some(c) => (c.row, 0),
            // from the top of the viewport
            None => (top_line.saturating_sub(1), 1),
        };
        let mut found = None;
        for _ in 0..n {
            match step_match(&matches, at, forward, key) {
                Some(r) => {
                    found = Some(r);
                    at = key(r);
                }
                None => break,
            }
        }
        if let Some(r) = found {
            let side = if split {
                v.split().side_of(r, SplitSide::Right)
            } else {
                SplitSide::Right
            };
            v.cursor = Some(DiffCursor { row: r, side });
            let line = line_of(v, split, r);
            let top = shown_offsets(v, split).top(line);
            let x = self.diff.scroll(&path).x;
            self.search.matches = matches;
            self.scroll_to(x, reveal_y(top, scroll_y, view_h), scroll_y);
            return;
        }
        self.search.matches = matches;
    }

    /// A directory's tri-state box: every file below it viewed (or, when all are, none),
    /// stored in one transaction.
    pub fn toggle_dir_viewed(&mut self, d: usize) {
        let Some(l) = self.files.as_mut() else {
            return;
        };
        let changes = l.toggle_dir_viewed(d);
        let (repo, id) = (self.repo.clone(), self.pr.id);
        self.writes
            .write("Could not save the viewed marks", move |st| {
                let marks: Vec<(&str, &str, bool)> = changes
                    .iter()
                    .map(|c| (c.path.as_str(), c.oid.as_str(), c.viewed))
                    .collect();
                st.set_viewed_many(&repo, id, &marks)
            });
    }

    /// Arrow, paging, Left / Right in the file list; a file reached is selected.
    pub fn list_key(&mut self, key: ListKey) {
        let Some(l) = self.files.as_mut() else {
            return;
        };
        let before = l.selected();
        let page = ((self.list_view.1 / LIST_ROW_H).floor() as usize).max(1);
        match key {
            ListKey::Nav(n) => l.nav(n, page),
            ListKey::Left => l.left(),
            ListKey::Right => l.right(),
            ListKey::Space => match l.cursor() {
                Some(Cursor::Dir(d)) => return self.toggle_dir_viewed(d),
                Some(Cursor::File(i)) => return self.toggle_viewed(i),
                Some(Cursor::Ghost(_)) | None => return,
            },
            ListKey::Enter => {
                if matches!(l.cursor(), Some(Cursor::File(_) | Cursor::Ghost(_))) {
                    self.tab = Tab::Diff;
                }
                return;
            }
        }
        if l.selected() != before {
            self.after_selection_change();
        }
        self.reveal_selected();
    }

    /// Approve / Request changes clicked: opens the confirmation (with its warnings).
    pub fn request_action(&mut self, a: prcore::comments::ReviewAction) {
        let (unresolved, drafts) = (self.comments.unresolved(), self.composer.drafts.len());
        self.actions
            .request(a, &self.pr, self.stale, unresolved, drafts);
    }

    /// Applies a click of the action bar or its confirmation.
    pub fn action_act(&mut self, a: ActionAct) {
        match a {
            ActionAct::Request(r) => self.request_action(r),
            ActionAct::Confirm => {
                let head = self.composer.head.clone();
                self.actions
                    .confirm(&self.repo, &mut self.pr, &head, self.stale);
            }
            ActionAct::Cancel => self.actions.cancel(),
            ActionAct::DismissNote => {
                self.actions.error = None;
                self.actions.done = None;
                self.actions.unconfirmed = None;
            }
        }
    }

    /// A confirmation dialog (submit drafts, or a review action) is open: the review
    /// shortcuts (j/k/v/c) are off.
    pub fn modal_open(&self) -> bool {
        self.composer.confirm.is_some() || self.actions.confirm.is_some()
    }

    /// A post, resolve or review action is waiting for the server (Back and Reload wait).
    pub fn busy(&self) -> bool {
        self.composer.running() || self.composer.any_resolving() || self.actions.running()
    }

    /// Finished posts, resolves and review actions; a post the server could not echo
    /// triggers a refetch, a head that moved makes the screen stale.
    pub fn poll_composer(&mut self) {
        if self.actions.poll(&mut self.pr, &self.composer.head) {
            self.stale = true;
            self.composer.stale = true;
        }
        let out = self.composer.poll(&mut self.comments);
        if out.refresh {
            self.refresh_comments();
        }
        if out.stale {
            self.stale = true;
        }
        if out.threads_changed || out.refresh {
            self.update_counts();
        }
    }

    /// Applies a click collected while drawing.
    pub fn act(&mut self, a: Act) {
        let c = &mut self.composer;
        match a {
            Act::Open(t) => c.open_editor(t),
            Act::Edit(id) => c.edit_draft(id),
            Act::Delete(id) => c.delete_draft(id),
            Act::AsFile(id) => c.as_file_comment(id),
            Act::AsPr(id) => c.as_pr_comment(id),
            Act::Reveal(id) => self.reveal(id),
            Act::Save => {
                c.save_editor();
            }
            Act::Cancel => c.cancel_editor(),
            Act::Submit => c.request_submit(),
            Act::Confirm => {
                c.confirm_submit();
            }
            Act::CancelSubmit => c.cancel_submit(),
            Act::Resolve(root) => {
                c.toggle_resolved(&mut self.comments, &root);
                self.update_counts();
            }
            Act::Undo => {
                c.undo(&mut self.comments);
                self.update_counts();
            }
            Act::DismissError => {
                c.error = None;
                c.notice = None;
            }
            Act::StopBatch => c.cancel_batch(),
            Act::CheckComments(id) => {
                if c.check_comments(id) {
                    self.refresh_comments();
                }
            }
            Act::Rearm(id) => {
                c.rearm(id);
            }
            Act::Suggest => {
                let path = match c.editor.as_ref().map(|e| &e.target) {
                    Some(Target::Inline { path, .. }) => path.clone(),
                    _ => return,
                };
                match self.diff.view_mut(&path) {
                    Some(v) => c.insert_suggestion(&v.rows),
                    None => c.error = Some("The file's diff is not loaded".into()),
                }
            }
        }
    }

    /// Shows where a draft lives: its file's diff, or the conversation tab.
    fn reveal(&mut self, id: i64) {
        let Some(d) = self.composer.draft(id).cloned() else {
            return;
        };
        let path = match d.kind {
            DraftKind::Pr => None,
            DraftKind::File | DraftKind::Inline => d.path,
            DraftKind::Reply => self
                .comments
                .threads
                .iter()
                .find(|t| d.replies_to(t))
                .and_then(|t| t.root.anchor.as_ref().map(|a| a.path.clone())),
        };
        if !path.is_some_and(|p| self.open_path(&p)) {
            self.tab = Tab::Conversation;
        }
    }

    /// Selects the file at `path` (conversation tab links) and shows its diff.
    pub fn open_file(&mut self, path: &str) {
        let i = self
            .files
            .as_ref()
            .and_then(|l| l.files().iter().position(|f| f.path == path));
        if let Some(i) = i {
            self.select(i);
            self.tab = Tab::Diff;
        }
    }

    /// Shows `path`: its diff, or its "not in diff" entry. False when it is neither.
    pub fn open_path(&mut self, path: &str) -> bool {
        let Some(l) = self.files.as_mut() else {
            return false;
        };
        if l.files().iter().any(|f| f.path == path) {
            self.open_file(path);
            return true;
        }
        let Some(g) = l.ghost_of(path) else {
            return false;
        };
        l.select_ghost(g);
        self.reveal_next = true;
        self.tab = Tab::Diff;
        true
    }

    /// Shows thread `i` where it lives: its file (scrolled to its row once laid out), its
    /// "not in diff" entry, or the Conversation tab for a PR-level thread.
    pub fn jump_to_thread(&mut self, i: usize) {
        let Some(t) = self.comments.threads.get(i) else {
            return;
        };
        let root = t.root.id.clone();
        let path = t.root.anchor.as_ref().map(|a| a.path.clone());
        // a thread on a renamed file's old path is shown on the renamed file
        let shown = path.map(|p| {
            self.comments
                .paths()
                .into_iter()
                .find(|&f| self.comments.on_old_path(f).contains(&i))
                .map_or(p, str::to_string)
        });
        if shown.is_some_and(|p| self.open_path(&p)) {
            self.pending_reveal = Some(root);
        }
    }

    pub fn selected_file(&self) -> Option<&DiffFile> {
        let l = self.files.as_ref()?;
        l.selected().map(|i| &l.files()[i])
    }

    fn selected_path(&self) -> Option<&str> {
        self.selected_file().map(|f| f.path.as_str())
    }

    /// Selects file `i`, restores its scroll and asks for its patch unless it is loaded.
    pub fn select(&mut self, i: usize) {
        let Some(l) = self.files.as_mut() else {
            return;
        };
        l.select(i);
        self.reveal_next = true;
        self.after_selection_change();
    }

    fn after_selection_change(&mut self) {
        let Some(f) = self.selected_file().cloned() else {
            return;
        };
        self.restore_scroll = Some(self.diff.scroll(&f.path));
        let loaded = self.diff.view(&f.path).is_some();
        if !f.binary && !loaded && !self.patch_errors.contains_key(&f.path) {
            self.requests.push(f.path);
        }
    }

    /// `j` / `k`: moves the selection and keeps it in view in the list.
    pub fn step(&mut self, forward: bool) {
        let Some(l) = self.files.as_mut() else {
            return;
        };
        let before = l.selected();
        let after = if forward {
            l.select_next()
        } else {
            l.select_prev()
        };
        if after != before {
            self.after_selection_change();
            self.reveal_selected();
        }
    }

    fn reveal_selected(&mut self) {
        let Some(l) = self.files.as_ref() else {
            return;
        };
        // the keyboard focus (or the selected file), scrolled to only when out of view
        let Some(pos) = l.cursor_pos(&l.rows()) else {
            return;
        };
        if let Some(y) = reveal_offset(pos, LIST_ROW_H, self.list_view) {
            self.list_scroll = Some(y);
        }
    }

    /// Flips file `i`'s viewed mark and stores it against the file's current oid.
    pub fn toggle_viewed(&mut self, i: usize) {
        let Some(l) = self.files.as_mut() else {
            return;
        };
        // optimistic: the list shows the new mark at once, the write follows on the writer
        let c = l.toggle_viewed(i);
        let (repo, id) = (self.repo.clone(), self.pr.id);
        self.writes
            .write("Could not save the viewed mark", move |st| {
                st.set_viewed(&repo, id, &c.path, &c.oid, c.viewed)
            });
    }

    /// Shows a failed background write ("Could not save ..."); keeps repainting while
    /// writes are queued so a failure appears without waiting for input. Once per frame.
    pub fn poll_writes(&mut self, ctx: &egui::Context) {
        if let Some(e) = self.writes.take_failure() {
            self.store_error = Some(e);
        }
        if self.writes.pending() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    /// Waits until the queued store writes are done.
    #[cfg(test)]
    pub fn flush_writes(&mut self) {
        self.writes.flush();
        if let Some(e) = self.writes.take_failure() {
            self.store_error = Some(e);
        }
    }

    /// Clears a file's error and asks for its patch again.
    #[cfg(test)]
    pub fn retry_patch(&mut self, path: &str) {
        self.patch_errors.remove(path);
        self.requests.push(path.to_string());
    }

    /// What Reload (and Retry) reopens. The inbox copy of the source commit is what made
    /// the screen stale, so it is cleared: the service then keeps only the server probe as
    /// the judge (stale again only if the server reports a head other than the one fetched),
    /// and `Opened` records the freshly fetched head as the PR's source commit.
    pub fn reload_target(&self) -> (RepoRef, PullRequest) {
        let mut pr = self.pr.clone();
        pr.source_commit.clear();
        (self.repo.clone(), pr)
    }

    pub fn take_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.requests)
    }

    /// The diff's first visible row when it changed since last asked (file and row).
    pub fn take_visible_change(&mut self) -> Option<(String, usize)> {
        let now = self.visible_now.take()?;
        if self.visible_sent.as_ref() == Some(&now) {
            return None;
        }
        self.visible_sent = Some(now.clone());
        Some(now)
    }

    /// Opened and the selected file shows its final content (diff, placeholder or error).
    fn settled(&self) -> bool {
        if self.failed.is_some() {
            return true;
        }
        match self.selected_file() {
            None => self.files.is_some(),
            Some(f) => {
                f.binary
                    || self.diff.view(&f.path).is_some()
                    || self.patch_errors.contains_key(&f.path)
            }
        }
    }
}

/// What `App` keeps for the review screen.
#[derive(Default)]
pub struct ReviewHost {
    pub(crate) pending: Option<(RepoRef, PullRequest)>,
    screen: Option<ReviewScreen>,
    service: Option<Arc<ReviewService>>,
    /// Demo mode: the throwaway repository and an in-memory store, made on first open.
    demo: Option<(Arc<DemoReview>, Arc<Mutex<Store>>)>,
    /// Demo mode: one service (and worker pool) for every demo review.
    demo_service: Option<Arc<ReviewService>>,
    /// Screenshot scene: once opened, show a modified file and mark two files viewed.
    pub(crate) shot_scene: bool,
    /// Screenshot scene variant: show the demo file that carries comment threads.
    pub(crate) shot_comments: bool,
    /// Screenshot scene variant: preload sample drafts (one of them blocked).
    pub(crate) shot_drafts: bool,
    /// Screenshot scene variant: the file tree with a nested file selected.
    pub(crate) shot_tree: bool,
    /// Screenshot scene variant: open the Conversation tab.
    pub(crate) shot_conversation: bool,
    /// Screenshot scene variant: scroll the Conversation tab to its end.
    pub(crate) shot_conversation_end: bool,
    /// Screenshot scene variant: select a "not in diff" entry once the comments are in.
    pub(crate) shot_outdated: bool,
    /// Draft ids being posted by any screen of this process (screens outlive no batch).
    in_flight: super::composer::InFlight,
    /// Screenshot scene variant: preselect a few diff rows (cleared once applied).
    pub(crate) shot_selection: bool,
    /// Screenshot scene variant: open the Approve confirmation (cleared once applied).
    pub(crate) shot_actions: bool,
    /// Who I am and the PR's stack, from the inbox that opened the review.
    pub(crate) context: ReviewContext,
    /// `--demo`: reviews open the throwaway demo repository with an in-memory "server".
    /// Never inferred from a missing backend: a real-mode host without one shows an error.
    pub(crate) demo_mode: bool,
    /// When to ask the server again whether the open PR moved.
    recheck: Option<RecheckTimer>,
    /// Settings: the vim-style keys are turned off.
    pub(crate) vim_off: bool,
    /// Screenshot scene variant: the split view (cleared once applied).
    pub(crate) shot_split: bool,
    /// Screenshot scene variant: the keyboard help open.
    pub(crate) shot_help: bool,
}

impl ReviewHost {
    /// The open PR's reviewers after a review action went through, once (the inbox keeps
    /// its own copy of the PR).
    pub fn take_reviewers_update(
        &mut self,
    ) -> Option<(RepoRef, u64, Vec<prcore::model::Reviewer>)> {
        let s = self.screen.as_mut()?;
        let reviewers = s.actions.take_reviewers_update()?;
        Some((s.repo.clone(), s.pr.id, reviewers))
    }

    /// Demo mode: use an already built fixture (made before the window opened, so the UI
    /// thread does not freeze building it).
    pub fn preload_demo(&mut self, fixture: Arc<DemoReview>) {
        let store = Store::open_in_memory().expect("in-memory store");
        self.demo = Some((fixture, Arc::new(Mutex::new(store))));
        // the cached service reads the old store and repository
        self.demo_service = None;
    }

    /// Screenshots and `--bench-review`: the selected diff is on screen and its colours arrived.
    pub fn ready_highlighted(&self) -> bool {
        self.ready_for_shot()
            && self
                .screen
                .as_ref()
                .is_some_and(|s| match s.selected_file() {
                    Some(f) => s
                        .diff
                        .view(&f.path)
                        .is_some_and(|v| v.plain || v.spans.is_some() || v.rows.is_empty()),
                    // a "not in diff" entry has no diff to colour
                    None => s
                        .files
                        .as_ref()
                        .is_some_and(|l| l.selected_ghost().is_some()),
                })
    }

    /// `--bench-review`: the second half of the bench shows the split view.
    pub fn bench_split(&mut self) {
        if let Some(s) = self.screen.as_mut() {
            s.layout = DiffLayout::Split;
        }
    }

    /// Time from the first patch to its colours (bench output).
    pub fn highlight_ms(&self) -> Option<f32> {
        self.screen.as_ref()?.highlight_ms
    }

    /// Milestones of the open review (bench output).
    pub fn timings(&self) -> Option<OpenTimings> {
        Some(self.screen.as_ref()?.timings.clone())
    }

    /// For scripted screenshots: the screen has settled after opening.
    pub fn ready_for_shot(&self) -> bool {
        self.pending.is_none()
            && !self.shot_scene
            && !self.shot_selection
            && !self.shot_actions
            && self
                .screen
                .as_ref()
                .is_some_and(|s| s.settled() && !(self.shot_comments && s.comments.loading))
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        backend: Option<&Backend>,
        repo: RepoRef,
        pr: PullRequest,
    ) {
        self.service = None;
        self.recheck = Some(RecheckTimer::new(Instant::now()));
        let mut context = self.context.clone();
        // the demo opens its fixture PR for every row: it keeps the clicked row's forge
        let clicked_forge = pr.forge;
        let (service, store, repo, pr, poster) = match (backend, self.demo_mode) {
            (Some(b), false) => {
                let (svc, store) = b.review_service();
                (svc, store, repo, pr, b.comment_poster())
            }
            (None, false) => {
                // real mode without a backend: never fall back to demo data or a fake server
                let mut s = ReviewScreen::new(
                    repo,
                    pr,
                    Arc::new(Mutex::new(Store::open_in_memory().expect("memory store"))),
                );
                s.apply(ReviewEvent::Failed(
                    "Reviewing is unavailable: the local cache could not be opened".into(),
                ));
                self.screen = Some(s);
                return;
            }
            (_, true) => {
                // git is located by the service on its own thread ("Git is unavailable")
                let (fixture, store) = self.demo.get_or_insert_with(|| {
                    let store = Store::open_in_memory().expect("in-memory store");
                    (
                        Arc::new(crate::demo::review_fixture_demo()),
                        Arc::new(Mutex::new(store)),
                    )
                });
                let head = fixture.head.clone();
                // the demo "server": posts and resolves stay in memory
                let memory = Arc::new(MemoryPoster::new(&head));
                if self.shot_drafts || self.shot_actions || self.shot_tree {
                    let st = lock_ok(store);
                    for d in crate::demo::review_drafts(&fixture.repo, fixture.pr.id, &head) {
                        let _ = st.save_draft(&d);
                    }
                }
                let svc = self
                    .demo_service
                    .get_or_insert_with(|| {
                        Arc::new(ReviewService::with_local(
                            store.clone(),
                            fixture.repo_path.clone(),
                            ctx.clone(),
                            Arc::new(move |_, _| Ok(head.clone())),
                            Arc::new(|_, _| Ok(crate::demo::review_comments_demo())),
                        ))
                    })
                    .clone();
                // every demo PR opens the demo repository's PR, in a demo stack
                let (repo, mut pr) = (fixture.repo.clone(), fixture.pr.clone());
                pr.forge = clicked_forge;
                context = crate::demo::review_context(fixture, &mut pr);
                *lock_ok(&memory.pr) = Some(pr.clone());
                *lock_ok(&memory.me) = context.me.clone();
                let poster: Poster = memory;
                (svc, store.clone(), repo, pr, Some(poster))
            }
        };
        service.open(repo.clone(), pr.clone());
        let mut screen = ReviewScreen::with_in_flight(repo, pr, store, self.in_flight.clone());
        screen.actions.ctx = context;
        // a read-only forge (`Capabilities::read_only`; none at present) gets no poster at
        // all: nothing can be sent, and the write buttons are replaced by a "Read-only" badge
        let read_only = providers::provider::capabilities_for(screen.pr.forge).read_only;
        screen.actions.read_only = read_only;
        screen.composer.read_only = read_only;
        screen.composer.set_forge(screen.pr.forge);
        if let Some(p) = poster.filter(|_| !read_only) {
            screen.actions.connect(p.clone(), Some(ctx.clone()));
            screen
                .composer
                .connect(p, service.row_source(), Some(ctx.clone()));
        }
        if self.shot_drafts {
            shot_drafts_setup(&mut screen.composer);
        }
        self.screen = Some(screen);
        self.service = Some(service);
    }

    pub(crate) fn close(&mut self) {
        if let Some(s) = self.service.take() {
            s.close();
        }
        self.screen = None;
        self.pending = None;
    }

    fn shot_setup(&mut self) {
        let Some(s) = self.screen.as_mut() else {
            return;
        };
        let Some(l) = s.files.as_ref() else { return };
        if self.shot_outdated {
            // the entries come with the comments: wait for them
            let Some(g) = l.ghosts().first().map(|g| g.path.clone()) else {
                return;
            };
            let pick = l
                .ghost_of("build/module-bundles/modules.lock")
                .map_or(g, |i| l.ghosts()[i].path.clone());
            s.open_path(&pick);
            self.shot_scene = false;
            return;
        }
        if self.shot_tree {
            let pos = |p: &str| l.files().iter().position(|f| f.path == p);
            let pick = pos(crate::demo::DEMO_TREE_FILE);
            let viewed: Vec<usize> = ["packages/readers/test/a00.test.ts", "README.md"]
                .iter()
                .filter_map(|p| pos(p))
                .collect();
            let dir = l
                .tree()
                .dirs
                .iter()
                .position(|d| d.path == "packages/verifiers/test");
            if let Some(i) = pick {
                s.select(i);
            }
            for i in viewed {
                s.toggle_viewed(i);
            }
            if let (Some(d), Some(l)) = (dir, s.files.as_mut()) {
                l.set_dir_expanded(d, false);
            }
            self.shot_scene = false;
            return;
        }
        let modified = if self.shot_comments {
            l.files()
                .iter()
                .position(|f| f.path == crate::demo::DEMO_COMMENTS_FILE)
        } else {
            // the demo app's TypeScript component (what the owner's PRs mostly hold), else
            // the first modified file
            let modified = |f: &DiffFile| f.status == FileStatus::Modified && !f.binary;
            let files = l.files();
            files
                .iter()
                .position(|f| f.path == crate::demo::DEMO_TSX_FILE)
                .or_else(|| files.iter().position(modified))
        };
        if let Some(i) = modified {
            s.select(i);
        }
        s.toggle_viewed(0);
        s.toggle_viewed(1);
        if self.shot_conversation {
            s.tab = Tab::Conversation;
        }
        if self.shot_conversation_end {
            s.conversation_to_end = true;
        }
        if self.shot_split {
            s.layout = DiffLayout::Split;
        }
        s.help_open = self.shot_help;
        self.shot_scene = false;
    }
}

/// Drafts scene: the draft on a line outside the diff shows as blocked, and a composer is
/// open on a line of the commented file.
fn shot_drafts_setup(c: &mut Composer) {
    let far = c
        .drafts
        .iter()
        .find(|d| d.kind == DraftKind::Inline && d.line == Some(crate::demo::DEMO_FAR_LINE))
        .map(|d| d.id);
    if let Some(id) = far {
        c.mark_blocked(id, prcore::comments::InlineBlock::LineNotInDiff);
    }
    c.open_editor(Target::Inline {
        path: crate::demo::DEMO_COMMENTS_FILE.into(),
        side: Side::New,
        line: 8,
        start: None,
    });
    if let Some(e) = c.editor.as_mut() {
        e.text = "Maybe extract this into a helper?".into();
        e.comment_type = Some(prcore::comment_type::CommentType::Suggestion);
        e.focus = false;
    }
}

/// Screenshot scene: selects a few rows of the loaded diff; false while it is not loaded yet.
fn preset_shot_selection(s: &mut ReviewScreen) -> bool {
    let Some(path) = s.selected_file().map(|f| f.path.clone()) else {
        return false;
    };
    let Some(v) = s.diff.view_mut(&path) else {
        return false;
    };
    let n = v.rows.len();
    let h = v
        .rows
        .iter()
        .position(|r| r.kind == RowKind::HunkHeader)
        .unwrap_or(0);
    if n > h + 5 {
        v.sel.sel = Some(Selection {
            anchor: Pos { row: h + 2, col: 6 },
            head: Pos {
                row: h + 5,
                col: 18,
            },
        });
    }
    true
}

/// Draws the review screen; returns true when the user went back to the inbox.
pub fn show(host: &mut ReviewHost, root: &mut Ui, backend: Option<&Backend>) -> bool {
    let ctx = root.ctx().clone();
    if let Some((repo, pr)) = host.pending.take() {
        host.start(&ctx, backend, repo, pr);
    }
    let Some(screen) = host.screen.as_mut() else {
        return true;
    };
    if let Some(svc) = &host.service {
        for ev in svc.poll() {
            screen.apply(ev);
        }
    }
    // the scene picks a modified text file: wait for numstat's binary flags
    let counted =
        screen.counts_error.is_some() || screen.files.as_ref().is_some_and(|l| l.counts_known());
    if host.shot_scene && counted {
        host.shot_setup();
    }
    let Some(screen) = host.screen.as_mut() else {
        return true;
    };
    screen.poll_composer();
    screen.poll_writes(&ctx);
    // the PR may be pushed to while it is open: ask again on focus and once a minute
    if let (Some(svc), Some(t)) = (&host.service, host.recheck.as_mut()) {
        let focused = ctx.input(|i| i.viewport().focused);
        if !screen.stale && t.due(Instant::now(), focused) {
            svc.recheck();
        }
        ctx.request_repaint_after(RECHECK_EVERY);
    }
    if host.shot_selection {
        host.shot_selection = !preset_shot_selection(screen);
    }
    if host.shot_actions && screen.files.is_some() && !screen.comments.loading {
        screen.request_action(prcore::comments::ReviewAction::Approve);
        host.shot_actions = false;
    }

    let modal = screen.modal_open();
    if let Some(scope) = super::export_ui::shortcut(&ctx, modal) {
        super::export_ui::copy(&ctx, screen, scope);
    }
    screen.vim = !host.vim_off;
    let mut acts = Vec::new();
    // the keymap (vim keys, Alt+S); never while a text field or a dialog has the keyboard
    let mut key_back = false;
    if ctx.egui_wants_keyboard_input() || modal {
        screen.keys.reset();
    } else {
        let fx = handle_keys(&ctx, screen);
        acts.extend(fx.acts);
        key_back = fx.back;
        if let Some(t) = fx.copy {
            ctx.copy_text(t);
        }
        if fx.focus_filter {
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique(FILTER_ID)));
        }
    }
    // the original single keys, when the vim keys are off
    if !screen.vim
        && review_keys_allowed(
            ctx.egui_wants_keyboard_input(),
            ctx.input(|i| i.modifiers),
            modal,
        )
    {
        let (j, k, v) = ctx.input(|i| {
            (
                i.key_pressed(Key::J),
                i.key_pressed(Key::K),
                i.key_pressed(Key::V),
            )
        });
        if j {
            screen.step(true);
        }
        if k {
            screen.step(false);
        }
        if v {
            if let Some(i) = screen.files.as_ref().and_then(|l| l.selected()) {
                screen.toggle_viewed(i);
            }
        }
    }
    let over_center = screen
        .center_rect
        .zip(ctx.pointer_hover_pos())
        .is_some_and(|(r, p)| r.contains(p));
    if list_keys_allowed(
        ctx.egui_wants_keyboard_input(),
        ctx.input(|i| i.modifiers),
        modal,
        over_center,
    ) {
        if let Some(k) = ctx.input(ListKey::pressed) {
            screen.list_key(k);
        }
    }
    screen.refresh_marks(false);

    let mut bar = TopBar::default();
    egui::Panel::top("review-bar").show(root, |ui| {
        bar = draw_top_bar(ui, screen);
    });
    // q / Backspace: as the Back button, which waits for a post or action in flight
    let (back, reload) = (bar.back || (key_back && !screen.busy()), bar.reload);
    if bar.refresh {
        screen.refresh_comments();
    }
    if bar.comment_pr {
        screen.tab = Tab::Conversation;
        acts.push(Act::Open(Target::Pr));
    }
    if bar.open_web {
        if let Err(e) = open_pr_page(&screen.repo, &screen.pr) {
            screen.actions.error = Some(e);
        }
    }
    egui::Panel::bottom("review-drafts")
        .show(root, |ui| drafts_bar(ui, &screen.composer, &mut acts));
    egui::Panel::left("review-files")
        .default_size(360.0)
        .size_range(240.0..=700.0)
        .show(root, |ui| draw_file_panel(ui, screen));
    egui::CentralPanel::default().show(root, |ui| draw_center(ui, screen, &mut acts));
    if screen.help_open {
        help_overlay(&ctx, &mut screen.help_open, screen.vim);
    }
    confirm_dialog(&ctx, &screen.composer, &mut acts);
    action_confirm(&ctx, &mut screen.actions, &screen.pr, &mut bar.actions);
    toast(&ctx, &screen.composer, &mut acts);
    if !acts.is_empty() || !bar.actions.is_empty() {
        ctx.request_repaint();
    }
    for a in acts {
        screen.act(a);
    }
    for a in bar.actions {
        screen.action_act(a);
    }

    if let Some(svc) = &host.service {
        for path in screen.take_requests() {
            svc.request_patch(path, true);
        }
        // where the diff is scrolled: its colours start there
        if let Some(v) = screen.take_visible_change() {
            svc.set_visible(&v.0, v.1);
        }
        if screen.take_comment_refresh() {
            svc.refresh_comments();
        }
    }
    if reload {
        host.pending = Some(screen.reload_target());
    }
    if back {
        host.close();
    }
    back
}

/// Id of the file list's filter field (`t` focuses it).
const FILTER_ID: &str = "review-file-filter";
/// Id of the diff's search field.
const SEARCH_ID: &str = "review-diff-search";

/// A key event as the keymap sees it: typed characters as typed (any layout), Ctrl / Alt
/// with a letter, and Enter / Backspace / Esc without modifiers. Alt+letter text is not a
/// typed character.
fn key_in(e: &egui::Event, alt: bool) -> Option<KeyIn> {
    match e {
        egui::Event::Text(t) if !alt => {
            let mut cs = t.chars();
            let c = cs.next()?;
            (cs.next().is_none() && !c.is_whitespace() && !c.is_control()).then_some(KeyIn::Char(c))
        }
        egui::Event::Key {
            key,
            pressed: true,
            modifiers: m,
            ..
        } => {
            let letter = || {
                let mut n = key.name().chars();
                match (n.next(), n.next()) {
                    (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
                    _ => None,
                }
            };
            if m.ctrl && !m.alt && !m.shift {
                letter().map(KeyIn::Ctrl)
            } else if m.alt && !m.ctrl && !m.command && !m.shift {
                letter().map(KeyIn::Alt)
            } else if m.is_none() {
                match key {
                    Key::Enter => Some(KeyIn::Enter),
                    Key::Backspace => Some(KeyIn::Backspace),
                    Key::Escape => Some(KeyIn::Escape),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Feeds this frame's key events to the keymap and carries out what fired. The events it
/// used are removed, so a text field focused by a key (`t`, `/`) never types that key.
/// Esc and Enter stay for the selection and the file list.
fn handle_keys(ctx: &egui::Context, s: &mut ReviewScreen) -> KeyEffects {
    let now = Instant::now();
    let (events, alt) = ctx.input(|i| (i.events.clone(), i.modifiers.alt));
    let mut fx = KeyEffects::default();
    let mut used = Vec::new();
    for (k, e) in events.iter().enumerate() {
        let Some(key) = key_in(e, alt) else {
            continue;
        };
        match s.keys.feed(key, now, s.vim) {
            Feed::Ignored => continue,
            Feed::Pending => {}
            Feed::Fired { action, count } => {
                let f = s.key_action(action, count);
                fx.acts.extend(f.acts);
                fx.back |= f.back;
                fx.copy = f.copy.or(fx.copy);
                fx.focus_filter |= f.focus_filter;
            }
        }
        if !matches!(key, KeyIn::Escape | KeyIn::Enter) {
            used.push(k);
        }
    }
    if !used.is_empty() {
        ctx.input_mut(|i| {
            let mut k = 0;
            i.events.retain(|_| {
                k += 1;
                !used.contains(&(k - 1))
            });
        });
        ctx.request_repaint();
    }
    s.keys.expire(now);
    if let Some(d) = s.keys.deadline() {
        // the pending `g` is shown until it times out
        ctx.request_repaint_after(d.saturating_duration_since(now));
    }
    fx
}

/// The `?` overlay: every shortcut from the keymap table, by section.
fn help_overlay(ctx: &egui::Context, open: &mut bool, vim: bool) {
    let mut close = false;
    let resp = egui::Modal::new(egui::Id::unique("review-keyboard-help")).show(ctx, |ui| {
        ui.label(RichText::new("Keyboard shortcuts").strong().size(16.0));
        if !vim {
            ui.label(RichText::new("Vim keys are off (Settings): only these work.").color(AMBER));
        }
        ui.add_space(6.0);
        // one grid per column, so the descriptions of all its sections line up
        ui.horizontal_top(|ui| {
            for (c, groups) in [&Group::ALL[..2], &Group::ALL[2..]].into_iter().enumerate() {
                ui.vertical(|ui| {
                    egui::Grid::new(("help", c))
                        .num_columns(2)
                        .spacing(vec2(14.0, 3.0))
                        .show(ui, |ui| {
                            for g in groups {
                                let rows = help_rows(*g, vim);
                                if rows.is_empty() {
                                    continue;
                                }
                                ui.label(RichText::new(g.title()).strong().color(AMBER));
                                ui.end_row();
                                for (keys, text) in rows {
                                    ui.label(RichText::new(keys).monospace().color(Color32::WHITE));
                                    ui.label(text);
                                    ui.end_row();
                                }
                                ui.label("");
                                ui.end_row();
                            }
                        });
                });
                ui.add_space(18.0);
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("? or Esc closes this help.").color(GREY));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("Close").clicked();
            });
        });
    });
    if resp.should_close() || close {
        *open = false;
    }
}

/// What the top bar was asked for this frame.
#[derive(Default)]
struct TopBar {
    back: bool,
    reload: bool,
    refresh: bool,
    /// "Comment on PR" (bar or overflow menu).
    comment_pr: bool,
    open_web: bool,
    actions: Vec<ActionAct>,
}

fn draw_top_bar(ui: &mut Ui, s: &ReviewScreen) -> TopBar {
    let mut bar = TopBar::default();
    ui.add_space(4.0);
    // a batch or a review action keeps running from this screen: let it finish first
    let busy = s.busy();
    let busy_why = "Posting to Bitbucket: wait, or stop after the current comment";
    ui.horizontal(|ui| {
        bar.back = ui
            .add_enabled(!busy, egui::Button::new("Back"))
            .on_hover_text("Back to the inbox")
            .on_disabled_hover_text(busy_why)
            .clicked();
        ui.separator();
        // what fits on one row at this width: the rest goes into More (buttons) or is hidden
        // (repository and branches); the title is truncated to its share
        let forge = s.pr.forge;
        let meta = format!("{} in {}", forge.change_ref(s.pr.id), s.repo.full_name());
        let branches = format!("{} -> {}", s.pr.source_branch, s.pr.dest_branch);
        let plan = plan_top_bar(ui.available_width(), &bar_widths(ui, s, &meta, &branches));
        ui.scope(|ui| {
            ui.set_max_width(plan.title_w);
            ui.add(egui::Label::new(RichText::new(&s.pr.title).strong().size(15.0)).truncate())
                .on_hover_text(&s.pr.title);
        });
        if plan.meta {
            ui.add(egui::Label::new(RichText::new(meta).color(GREY)).extend());
        }
        if plan.branches {
            ui.add(egui::Label::new(RichText::new(branches).monospace().color(GREY)).extend());
        }
        let inline = |b: BarButton| plan.inline.contains(&b);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("More", |ui| {
                let comment_on = format!("Comment on {}", forge.change_noun());
                if s.composer.read_only {
                    ui.add_enabled(false, egui::Button::new(comment_on))
                        .on_disabled_hover_text(super::composer::READ_ONLY_TIP);
                } else if ui.button(comment_on).clicked() {
                    bar.comment_pr = true;
                    ui.close();
                }
                if !inline(BarButton::Refresh)
                    && ui
                        .add_enabled(!s.comments.loading, egui::Button::new("Refresh comments"))
                        .clicked()
                {
                    bar.refresh = true;
                    ui.close();
                }
                if ui.button(open_web_label(&s.pr)).clicked() {
                    bar.open_web = true;
                    ui.close();
                }
            });
            super::export_ui::menu(ui, s);
            if inline(BarButton::OpenWeb)
                && ui
                    .button(open_web_label(&s.pr))
                    .on_hover_text(format!("Open this {} in the browser", forge.change_name()))
                    .clicked()
            {
                bar.open_web = true;
            }
            ui.separator();
            action_buttons(ui, &s.actions, &s.pr, s.stale, &mut bar.actions);
            ui.separator();
            if inline(BarButton::Refresh)
                && ui
                    .add_enabled(!s.comments.loading, egui::Button::new("Refresh comments"))
                    .clicked()
            {
                bar.refresh = true;
            }
            let c = &s.comments;
            if c.loading {
                ui.label(RichText::new("Loading comments...").color(GREY));
                ui.spinner();
            } else {
                let n = c.unresolved();
                let color = if n > 0 { AMBER } else { GREY };
                ui.label(
                    RichText::new(format!("{n} unresolved"))
                        .color(color)
                        .strong(),
                )
                .on_hover_text(format!(
                    "Unresolved comment threads in this {}",
                    forge.change_name()
                ));
            }
        });
    });
    action_notes(ui, &s.actions, s.stale, &mut bar.actions);
    if let Some(e) = &s.comments.error {
        ui.colored_label(RED, e);
    }
    if s.comments.skipped > 0 {
        ui.colored_label(
            AMBER,
            format!(
                "{} comments could not be read and are not shown.",
                s.comments.skipped
            ),
        );
    }
    if let Some(p) = &s.phase {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(p);
        });
    }
    if let Some(e) = &s.failed {
        banner(ui, Color32::from_rgb(70, 28, 28), RED, |ui| {
            ui.label(RichText::new(e).color(RED));
            bar.reload |= ui.add_enabled(!busy, egui::Button::new("Retry")).clicked();
        });
    }
    if s.stale {
        banner(ui, Color32::from_rgb(52, 46, 24), AMBER, |ui| {
            ui.label(RichText::new(STALE_TEXT).color(AMBER).strong());
            bar.reload |= ui
                .add_enabled(!busy, egui::Button::new("Reload"))
                .on_hover_text(format!(
                    "Fetch the {} again; drafts are kept",
                    s.pr.forge.change_name()
                ))
                .on_disabled_hover_text(busy_why)
                .clicked();
        });
    }
    if let Some(e) = &s.store_error {
        ui.colored_label(RED, e);
    }
    ui.add_space(4.0);
    bar
}

fn banner(ui: &mut Ui, fill: Color32, stroke: Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style())
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(add);
        });
}

fn status_color(s: FileStatus) -> Color32 {
    match s {
        FileStatus::Added => GREEN,
        FileStatus::Deleted => RED,
        FileStatus::Renamed | FileStatus::Copied => Color32::from_rgb(170, 130, 230),
        FileStatus::Modified | FileStatus::TypeChanged => AMBER,
    }
}

/// Status chip at `x` (left edge), vertically centred on `cy`; returns its right edge.
fn chip(p: &egui::Painter, x: f32, cy: f32, s: FileStatus) -> f32 {
    let rect = Rect::from_min_size(Pos2::new(x, cy - 8.0), vec2(16.0, 16.0));
    p.rect_filled(rect, 3.0, status_color(s).gamma_multiply(0.25));
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        status_letter(s),
        FontId::monospace(11.0),
        status_color(s),
    );
    rect.right()
}

fn path_job(path: &str, size: f32, color: Color32, dim: Color32) -> LayoutJob {
    let (dir, name) = match path.rfind('/') {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    };
    let mut job = LayoutJob::default();
    let fmt = |c| TextFormat {
        font_id: FontId::proportional(size),
        color: c,
        ..Default::default()
    };
    job.append(dir, 0.0, fmt(dim));
    job.append(name, 0.0, fmt(color));
    job
}

/// Indentation per tree level, and the room of a directory's chevron.
const TREE_INDENT: f32 = 14.0;
/// Fixed width of the "+a -d" column and of the comment marker column.
const COUNTS_W: f32 = 64.0;
const CHIP_W: f32 = 32.0;

/// What a click in the file panel asked for (applied after drawing).
#[derive(Default)]
struct PanelClicks {
    select: Option<usize>,
    toggle: Option<usize>,
    focus_dir: Option<usize>,
    toggle_dir: Option<usize>,
    dir_viewed: Option<usize>,
    flip_generated: bool,
    open_diff: bool,
    view: Option<ViewMode>,
    /// A "not in diff" entry was clicked.
    ghost: Option<usize>,
}

fn draw_file_panel(ui: &mut Ui, s: &mut ReviewScreen) {
    // the panel remembers the width its content used; keep the default while loading
    ui.set_min_width(ui.available_width());
    ui.add_space(6.0);
    if s.reveal_next && s.list_view.1 > 0.0 {
        s.reveal_next = false;
        s.reveal_selected();
    }
    let Some(list) = s.files.as_mut() else {
        ui.label(RichText::new("Loading files...").weak());
        return;
    };
    let mut c = PanelClicks::default();
    ui.add(
        egui::TextEdit::singleline(&mut list.filter)
            .id(egui::Id::unique(FILTER_ID))
            .desired_width(f32::INFINITY)
            .hint_text("Filter files"),
    );
    ui.horizontal_wrapped(|ui| {
        for ch in ['A', 'M', 'D', 'R'] {
            let on = list.statuses.contains(&ch);
            if ui
                .selectable_label(on, RichText::new(ch.to_string()).monospace())
                .on_hover_text("Show only files with this status")
                .clicked()
            {
                if on {
                    list.statuses.remove(&ch);
                } else {
                    list.statuses.insert(ch);
                }
            }
        }
        if ui
            .selectable_label(list.commented_only, "Commented")
            .on_hover_text("Show only files with comment threads or drafts")
            .clicked()
        {
            list.commented_only = !list.commented_only;
        }
        ui.separator();
        ui.checkbox(&mut list.hide_viewed, "Hide viewed");
    });
    ui.horizontal(|ui| {
        let tree = list.view() == ViewMode::Tree;
        if ui
            .selectable_label(tree, "Tree")
            .on_hover_text("Changed files as a directory tree")
            .clicked()
        {
            c.view = Some(ViewMode::Tree);
        }
        if ui
            .selectable_label(!tree, "List")
            .on_hover_text("Changed files as a flat list")
            .clicked()
        {
            c.view = Some(ViewMode::List);
        }
        if tree {
            ui.separator();
            if ui
                .small_button("Expand all")
                .on_hover_text("Expand every directory")
                .clicked()
            {
                list.expand_all();
            }
            if ui
                .small_button("Collapse all")
                .on_hover_text("Collapse every directory")
                .clicked()
            {
                list.collapse_all();
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(list.counter()).color(GREY));
        if let Some(e) = &s.counts_error {
            ui.label(RichText::new("line counts unavailable").color(AMBER))
                .on_hover_text(e);
        } else if !list.counts_known() {
            ui.label(RichText::new("counting lines…").color(GREY));
        }
    });
    ui.separator();

    let rows = list.rows();
    if rows.is_empty() {
        let msg = if list.len() == 0 {
            "No changed files."
        } else {
            "No files match."
        };
        ui.label(RichText::new(msg).weak());
        apply_panel_clicks(s, c);
        return;
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut area = egui::ScrollArea::vertical()
        .id_salt("review-file-list")
        .auto_shrink(false);
    if let Some(y) = s.list_scroll.take() {
        area = area.vertical_scroll_offset(y);
    }
    let (selected, cursor) = (list.selected(), list.cursor());
    let stats = list.dir_stats();
    let list: &FileList = list;
    let out = area.show_rows(ui, LIST_ROW_H, rows.len(), |ui, range| {
        for row in &rows[range] {
            match *row {
                ListRow::Generated { count, expanded } => {
                    if generated_row(ui, count, expanded).clicked() {
                        c.flip_generated = true;
                    }
                }
                ListRow::Dir(d) => {
                    let focused = cursor == Some(Cursor::Dir(d));
                    let (r, chevron, cb) = dir_row(ui, list, d, &stats[d], focused);
                    if cb {
                        c.dir_viewed = Some(d);
                    } else if chevron || r.double_clicked() {
                        c.toggle_dir = Some(d);
                        c.focus_dir = Some(d);
                    } else if r.clicked() {
                        c.focus_dir = Some(d);
                    }
                }
                ListRow::File(i) => {
                    let focused = cursor == Some(Cursor::File(i));
                    let (r, cb) = file_row(ui, list, i, selected == Some(i), focused);
                    if cb {
                        c.toggle = Some(i);
                    } else if r.double_clicked() {
                        c.select = Some(i);
                        c.open_diff = true;
                    } else if r.clicked() {
                        c.select = Some(i);
                    }
                }
                ListRow::GhostHeader { count } => ghost_header_row(ui, count),
                ListRow::Ghost(g) => {
                    let focused = cursor == Some(Cursor::Ghost(g));
                    let r = ghost_row(ui, list, g, list.selected_ghost() == Some(g), focused);
                    if r.clicked() || r.double_clicked() {
                        c.ghost = Some(g);
                        c.open_diff = true;
                    }
                }
            }
        }
    });
    s.list_view = (out.state.clamped_offset().y, out.inner_rect.height());
    apply_panel_clicks(s, c);
}

fn apply_panel_clicks(s: &mut ReviewScreen, c: PanelClicks) {
    if let Some(v) = c.view {
        s.set_file_view(v);
    }
    if let Some(l) = s.files.as_mut() {
        if c.flip_generated {
            l.show_generated = !l.show_generated;
        }
        if let Some(d) = c.toggle_dir {
            l.toggle_dir(d);
        }
        if let Some(d) = c.focus_dir {
            l.focus_dir(d);
        }
        if let Some(g) = c.ghost {
            l.select_ghost(g);
        }
    }
    if let Some(d) = c.dir_viewed {
        s.toggle_dir_viewed(d);
    }
    if let Some(i) = c.toggle {
        s.toggle_viewed(i);
    }
    if let Some(i) = c.select {
        if s.files.as_ref().and_then(|l| l.selected()) != Some(i) {
            s.select(i);
        }
    }
    if c.open_diff {
        s.tab = Tab::Diff;
    }
}

fn generated_row(ui: &mut Ui, count: usize, expanded: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    if resp.hovered() {
        p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let verb = if expanded { "Hide" } else { "Show" };
    p.text(
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{verb} {count} generated files"),
        FontId::proportional(13.0),
        GREY,
    );
    resp
}

/// "Not in this diff (n)": the header of the paths with outdated comments.
fn ghost_header_row(ui: &mut Ui, count: usize) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::hover());
    let p = ui.painter_at(rect);
    p.hline(
        rect.x_range(),
        rect.top() + 1.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    p.text(
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        format!("Not in this diff ({count})"),
        FontId::proportional(13.0),
        GREY,
    );
}

/// A path with comments that is not in the diff: greyed full path, a "not in diff" tag and
/// its (dimmed) comment marker.
fn ghost_row(
    ui: &mut Ui,
    list: &FileList,
    g: usize,
    selected: bool,
    focused: bool,
) -> egui::Response {
    let ghost = &list.ghosts()[g];
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    row_bg(ui, &p, rect, selected, focused, resp.hovered());
    let cy = rect.center().y;
    let (dim, grey) = viewed_colors(selected);
    let tag = p.text(
        Pos2::new(rect.right() - 8.0, cy),
        Align2::RIGHT_CENTER,
        "not in diff",
        FontId::proportional(11.0),
        grey,
    );
    p.rect_stroke(
        tag.expand2(vec2(4.0, 1.0)),
        3.0,
        egui::Stroke::new(1.0, grey),
        egui::StrokeKind::Inside,
    );
    let right = paint_comment_chip(&p, tag.left() - 10.0, cy, &ghost.mark);
    paint_path(
        &p,
        &ghost.path,
        rect.left() + 6.0,
        cy,
        right - 8.0,
        (grey, dim),
    );
    if resp.hovered() {
        resp.on_hover_text(format!(
            "{}\nNot in this pull request's current diff (removed, or its changes reverted, \
             by a later push). Its comments are outdated.\n{}",
            ghost.path,
            ghost.mark.tooltip()
        ))
    } else {
        resp
    }
}

/// Row background: selection, keyboard focus outline, hover.
fn row_bg(ui: &Ui, p: &egui::Painter, rect: Rect, selected: bool, focused: bool, hovered: bool) {
    if selected {
        p.rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
    } else if hovered {
        p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if focused {
        p.rect_stroke(
            rect.shrink(1.0),
            2.0,
            egui::Stroke::new(1.0, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
}

/// The viewed box (tri-state for directories) centred in `r`.
fn paint_viewed_box(p: &egui::Painter, r: Rect, state: Tri, hovered: bool) {
    let stroke = if hovered { Color32::WHITE } else { GREY };
    p.rect_stroke(
        r,
        3.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    let (c, s) = (r.center(), egui::Stroke::new(2.0, Color32::WHITE));
    match state {
        Tri::Off => {}
        Tri::On => {
            p.rect_filled(r.shrink(1.0), 3.0, Color32::from_rgb(52, 90, 140));
            // a tick drawn with lines: the default fonts have no check mark glyph
            p.line_segment([c + vec2(-4.5, 0.0), c + vec2(-1.5, 3.5)], s);
            p.line_segment([c + vec2(-1.5, 3.5), c + vec2(4.5, -3.5)], s);
        }
        Tri::Mixed => {
            p.rect_filled(r.shrink(1.0), 3.0, Color32::from_rgb(40, 62, 92));
            p.line_segment([c + vec2(-4.0, 0.0), c + vec2(4.0, 0.0)], s);
        }
    }
}

/// Shown instead of "+a -d" while the line counts are not in yet.
const COUNTS_PENDING: &str = "…";

/// "…" right-aligned at `right`; returns the left edge.
fn paint_counts_pending(p: &egui::Painter, right: f32, cy: f32) -> f32 {
    p.text(
        Pos2::new(right, cy),
        Align2::RIGHT_CENTER,
        COUNTS_PENDING,
        FontId::proportional(12.0),
        GREY,
    )
    .left()
}

/// "+a -d" right-aligned at `right`; returns the left edge.
fn paint_counts(p: &egui::Painter, right: f32, cy: f32, add: u64, del: u64) -> f32 {
    let d = p.text(
        Pos2::new(right, cy),
        Align2::RIGHT_CENTER,
        format!("-{del}"),
        FontId::proportional(12.0),
        RED,
    );
    p.text(
        Pos2::new(d.left() - 4.0, cy),
        Align2::RIGHT_CENTER,
        format!("+{add}"),
        FontId::proportional(12.0),
        GREEN,
    )
    .left()
}

/// The comment marker right-aligned at `right`: a speech bubble with the thread count
/// (amber: unresolved, grey: all resolved; dimmer when every thread is outdated) and a blue
/// dot for drafts. Returns the left edge (`right` when there is nothing to show).
fn paint_comment_chip(p: &egui::Painter, right: f32, cy: f32, m: &CommentMark) -> f32 {
    let mut x = right;
    if m.drafts > 0 {
        let c = Pos2::new(x - 4.0, cy);
        p.circle_filled(c, 3.5, DRAFT_BLUE);
        x -= 10.0;
    }
    if let Some((n, open)) = m.chip() {
        let color = if open { AMBER } else { GREY };
        let color = if m.all_outdated() {
            color.gamma_multiply(0.55)
        } else {
            color
        };
        let g = p.layout_no_wrap(n.to_string(), FontId::proportional(11.0), color);
        let tx = x - g.size().x;
        p.galley(Pos2::new(tx, cy - g.size().y / 2.0), g, color);
        // the bubble: a rounded body with a tail at its lower left
        let body = Rect::from_min_size(Pos2::new(tx - 15.0, cy - 6.0), vec2(12.0, 9.0));
        p.rect_filled(body, 3.0, color);
        p.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(body.left() + 2.0, body.bottom() - 1.0),
                Pos2::new(body.left() + 6.0, body.bottom() - 1.0),
                Pos2::new(body.left() + 2.0, body.bottom() + 3.5),
            ],
            color,
            egui::Stroke::NONE,
        ));
        x = body.left();
    }
    x
}

/// `text` on one line from `x`, cut with an ellipsis before `max_x`.
fn one_line(p: &egui::Painter, mut job: LayoutJob, x: f32, cy: f32, max_x: f32, color: Color32) {
    job.wrap = egui::text::TextWrapping {
        max_width: (max_x - x).max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('\u{2026}'),
    };
    let g = p.layout_job(job);
    p.galley(Pos2::new(x, cy - g.size().y / 2.0), g, color);
}

/// A path on one line from `x` to `max_x`: the file name is always shown whole (cut at its
/// end only when it alone does not fit); the dimmed directory is cut at its start
/// ("...test/") when both do not fit.
fn paint_path(
    p: &egui::Painter,
    path: &str,
    x: f32,
    cy: f32,
    max_x: f32,
    (color, dim): (Color32, Color32),
) {
    let (dir, name) = match path.rfind('/') {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    };
    let font = FontId::proportional(13.0);
    let avail = max_x - x;
    let name_g = p.layout_no_wrap(name.to_string(), font.clone(), color);
    let room = avail - name_g.size().x;
    if dir.is_empty() || room <= 0.0 {
        one_line(p, path_job(name, 13.0, color, dim), x, cy, max_x, color);
        return;
    }
    let width = |s: String| p.layout_no_wrap(s, font.clone(), dim).size().x;
    let shown = if width(dir.to_string()) <= room {
        dir.to_string()
    } else {
        // the longest tail of the directory that fits behind an ellipsis (binary search)
        let starts: Vec<usize> = dir.char_indices().map(|(i, _)| i).collect();
        let tail = |k: usize| format!("\u{2026}{}", &dir[starts[k]..]);
        let (mut lo, mut hi) = (1, starts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if width(tail(mid)) <= room {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        if lo < starts.len() && width(tail(lo)) <= room {
            tail(lo)
        } else {
            String::new()
        }
    };
    let dir_g = p.layout_no_wrap(shown, font, dim);
    let dw = dir_g.size().x;
    p.galley(Pos2::new(x, cy - dir_g.size().y / 2.0), dir_g, dim);
    p.galley(Pos2::new(x + dw, cy - name_g.size().y / 2.0), name_g, color);
}

/// Left edge of a row's content at tree `depth`.
fn indent_x(rect: Rect, depth: u16) -> f32 {
    rect.left() + 6.0 + f32::from(depth) * TREE_INDENT
}

/// Returns the row response and whether the viewed box was clicked.
fn file_row(
    ui: &mut Ui,
    list: &FileList,
    i: usize,
    selected: bool,
    focused: bool,
) -> (egui::Response, bool) {
    let f = &list.files()[i];
    let tree = list.view() == ViewMode::Tree;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let cb_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 16.0, rect.center().y),
        vec2(16.0, 16.0),
    );
    let cb = ui.interact(cb_rect, resp.id.with(("viewed", i)), Sense::click());
    let p = ui.painter_at(rect);
    row_bg(ui, &p, rect, selected, focused, resp.hovered());
    let cy = rect.center().y;
    let viewed = list.is_viewed(i);

    // right side, right to left: viewed box, +/- counts, comment marker
    let state = if viewed { Tri::On } else { Tri::Off };
    paint_viewed_box(&p, cb_rect, state, cb.hovered());
    // fixed columns: names never run into the counts or the box
    let counts_right = cb_rect.left() - 8.0;
    if f.binary || !f.counts_known {
        // "…": numstat has not answered yet
        p.text(
            Pos2::new(counts_right, cy),
            Align2::RIGHT_CENTER,
            if f.binary { "bin" } else { COUNTS_PENDING },
            FontId::proportional(12.0),
            GREY,
        );
    } else {
        let (a, d) = (u64::from(f.additions), u64::from(f.deletions));
        paint_counts(&p, counts_right, cy, a, d);
    }
    let mut right = counts_right - COUNTS_W;
    let mark = list.mark(i);
    if list.has_marks() {
        paint_comment_chip(&p, right, cy, &mark);
        right -= CHIP_W;
    }

    // left side, cut before the right side
    let depth = list.depth(ListRow::File(i));
    let x0 = if tree && list.tree().file_dir[i].is_some() {
        // files line up with their directory's name, past the chevron
        indent_x(rect, depth) + TREE_INDENT
    } else {
        rect.left() + 6.0
    };
    let x = chip(&p, x0, cy, f.status) + 8.0;
    let (dim, grey) = viewed_colors(selected);
    let color = if viewed {
        grey
    } else {
        ui.visuals().text_color()
    };
    if tree && list.tree().file_dir[i].is_some() {
        // the tree shows the name only (the full path is in the tooltip)
        let job = path_job(super::files::file_name(&f.path), 13.0, color, dim);
        one_line(&p, job, x, cy, right - 8.0, color);
    } else {
        paint_path(&p, &f.path, x, cy, right - 8.0, (color, dim));
    }
    let resp = if resp.hovered() {
        let mut tip = match &f.old_path {
            Some(old) => format!("{old} -> {}", f.path),
            None => f.path.clone(),
        };
        if mark.any() {
            tip.push('\n');
            tip.push_str(&mark.tooltip());
        }
        resp.on_hover_text(tip)
    } else {
        resp
    };
    (resp, cb.clicked())
}

/// Returns the row response, whether its chevron and whether its viewed box was clicked.
fn dir_row(
    ui: &mut Ui,
    list: &FileList,
    d: usize,
    st: &super::files::DirStats,
    focused: bool,
) -> (egui::Response, bool, bool) {
    let node = &list.tree().dirs[d];
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let cy = rect.center().y;
    let x = indent_x(rect, node.depth);
    let chev_rect = Rect::from_center_size(Pos2::new(x + 5.0, cy), vec2(TREE_INDENT, LIST_ROW_H));
    let chev = ui.interact(chev_rect, resp.id.with(("chevron", d)), Sense::click());
    let cb_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, cy), vec2(16.0, 16.0));
    let cb = ui.interact(cb_rect, resp.id.with(("dir-viewed", d)), Sense::click());
    let p = ui.painter_at(rect);
    row_bg(ui, &p, rect, false, focused, resp.hovered());

    // right side: tri-state box, viewed/total, +/- counts, comment marker (descendants)
    paint_viewed_box(&p, cb_rect, st.tri(), cb.hovered());
    // the same fixed columns as the files, then "viewed/total"
    let counts_right = cb_rect.left() - 8.0;
    // directory totals may be wider than the files' column: never under the marker
    // partial totals would read as final: "…" until every file below is counted
    let counts_left = if st.counts_unknown > 0 {
        paint_counts_pending(&p, counts_right, cy)
    } else {
        paint_counts(&p, counts_right, cy, st.additions, st.deletions)
    };
    let mut right = (counts_right - COUNTS_W).min(counts_left - 4.0);
    if list.has_marks() {
        paint_comment_chip(&p, right, cy, &st.marks);
        right -= CHIP_W;
    }
    right = p
        .text(
            Pos2::new(right, cy),
            Align2::RIGHT_CENTER,
            format!("{}/{}", st.viewed, st.files),
            FontId::proportional(12.0),
            GREY,
        )
        .left();

    // chevron (painted triangle), folder glyph, name
    let open = list.dir_expanded(d);
    let cc = Pos2::new(x + 5.0, cy);
    let tri = if open {
        vec![
            cc + vec2(-4.0, -2.5),
            cc + vec2(4.0, -2.5),
            cc + vec2(0.0, 3.0),
        ]
    } else {
        vec![
            cc + vec2(-2.5, -4.0),
            cc + vec2(3.0, 0.0),
            cc + vec2(-2.5, 4.0),
        ]
    };
    let chev_color = if chev.hovered() { Color32::WHITE } else { GREY };
    p.add(egui::Shape::convex_polygon(
        tri,
        chev_color,
        egui::Stroke::NONE,
    ));
    let fx = x + TREE_INDENT;
    let folder = Color32::from_rgb(120, 140, 175);
    p.rect_filled(
        Rect::from_min_size(Pos2::new(fx, cy - 6.0), vec2(6.0, 3.0)),
        1.0,
        folder,
    );
    p.rect_filled(
        Rect::from_min_size(Pos2::new(fx, cy - 4.0), vec2(14.0, 10.0)),
        2.0,
        folder,
    );
    let color = if st.tri() == Tri::On {
        GREY
    } else {
        ui.visuals().strong_text_color()
    };
    let mut job = LayoutJob::default();
    job.append(
        &node.name,
        0.0,
        TextFormat {
            font_id: FontId::proportional(13.0),
            color,
            ..Default::default()
        },
    );
    one_line(&p, job, fx + 20.0, cy, right - 8.0, color);
    let resp = if resp.hovered() {
        let counts = if st.counts_unknown > 0 {
            "lines being counted".to_string()
        } else {
            format!("+{} -{}", st.additions, st.deletions)
        };
        let mut tip = format!(
            "{}/\n{} files, {} viewed, {counts}",
            node.path, st.files, st.viewed
        );
        if st.marks.any() {
            tip.push('\n');
            tip.push_str(&st.marks.tooltip());
        }
        resp.on_hover_text(tip)
    } else {
        resp
    };
    (resp, chev.clicked(), cb.clicked())
}

/// Tabs over the central pane, then the selected tab. Card clicks toggle afterwards.
fn draw_center(ui: &mut Ui, s: &mut ReviewScreen, acts: &mut Vec<Act>) {
    s.center_rect = Some(ui.max_rect());
    ui.horizontal(|ui| {
        ui.selectable_value(&mut s.tab, Tab::Diff, "Diff");
        let n = s.comments.pr_level.len();
        ui.selectable_value(&mut s.tab, Tab::Conversation, format!("Conversation ({n})"));
    });
    ui.separator();
    let mut toggles = Vec::new();
    match s.tab {
        Tab::Diff => draw_diff_pane(ui, s, &mut toggles, acts),
        Tab::Conversation => draw_conversation(ui, s, &mut toggles, acts),
    }
    if !toggles.is_empty() {
        // the new heights are laid out on the next frame
        ui.ctx().request_repaint();
    }
    for t in toggles {
        match t {
            CardToggle::Thread(id) => s.comments.toggle(&id),
            CardToggle::Body(id) => s.comments.toggle_full(&id),
        }
    }
}

/// Paints thread `t`'s card in `card` (laid out by `lay` at `card`'s width) with its
/// clickable title, "Show more" / "Show less" lines and Reply / Resolve buttons. `salt`
/// keeps the ids of one card apart from the same thread's card elsewhere.
#[allow(clippy::too_many_arguments)]
fn thread_card(
    ui: &mut Ui,
    painter: &egui::Painter,
    card: Rect,
    t: &prcore::comments::Thread,
    lay: &CardLayout,
    note: Option<&str>,
    salt: &str,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) {
    let resp = ui.interact(
        title_rect(card),
        ui.scope_id().with((salt, &t.root.id)),
        Sense::click(),
    );
    paint_card(painter, card, t, lay, note, resp.hovered());
    thread_buttons(ui, card, t, dr.composer, dr.acts);
    if resp.clicked() {
        toggles.push(CardToggle::Thread(t.root.id.clone()));
    }
    for (id, r) in lay.more_rects(card) {
        let more = ui
            .interact(r, ui.scope_id().with((salt, "more", &id)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if more.clicked() {
            toggles.push(CardToggle::Body(id));
        }
    }
}

/// Composer state and the clicks collected while drawing.
struct Drafting<'a> {
    composer: &'a mut Composer,
    acts: &'a mut Vec<Act>,
    /// A confirmation dialog is open (`ReviewScreen::modal_open`): no `c` shortcut.
    modal: bool,
}

/// One thread card as a widget (sections outside the virtualized rows), with Reply and
/// Resolve on its title and its reply drafts / composer below.
fn card_widget(
    ui: &mut Ui,
    comments: &Comments,
    i: usize,
    note: Option<String>,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) {
    let t = &comments.threads[i];
    // measured at the width it is painted at, then allocated whole: the next item starts
    // below the card's real bottom
    let w = ui.available_width().min(1000.0);
    let lay = comments.card(t, w, &mut measure_with(ui.painter()));
    let (rect, _) = ui.allocate_exact_size(vec2(w, lay.height), Sense::hover());
    let painter = ui.painter().clone();
    let note = note.as_deref();
    thread_card(
        ui,
        &painter,
        rect,
        t,
        &lay,
        note,
        "thread-card",
        toggles,
        dr,
    );
    ui.add_space(CARD_MARGIN);
    reply_widgets(ui, dr.composer, t, dr.acts);
}

/// A collapsible list of cards above the diff (file-level or outdated threads), at most
/// `max_h` tall.
#[allow(clippy::too_many_arguments)]
fn thread_section(
    ui: &mut Ui,
    outdated: bool,
    file_path: &str,
    comments: &Comments,
    ids: &[usize],
    max_h: f32,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) {
    let (salt, title) = if outdated {
        (
            "outdated-comments",
            format!("Outdated comments ({})", ids.len()),
        )
    } else {
        ("file-comments", format!("File comments ({})", ids.len()))
    };
    egui::CollapsingHeader::new(RichText::new(title).color(AMBER).strong())
        .id_salt(salt)
        .default_open(true)
        .show_unindented(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt((salt, "scroll"))
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for &i in ids {
                        let t = &comments.threads[i];
                        let note = if outdated {
                            Some(outdated_note(t, file_path))
                        } else {
                            // a file comment Bitbucket flags keeps its place, with the badge
                            is_flagged(t).then(|| reason_note(t, OutdatedReason::Flagged))
                        };
                        card_widget(ui, comments, i, note, toggles, dr);
                    }
                });
        });
}

fn draw_conversation(
    ui: &mut Ui,
    s: &mut ReviewScreen,
    toggles: &mut Vec<CardToggle>,
    acts: &mut Vec<Act>,
) {
    let (mut jump_path, mut jump_thread) = (None, None);
    let mut hide_outdated = s.hide_outdated;
    let modal = s.modal_open();
    let forge = s.pr.forge;
    let mut dr = Drafting {
        composer: &mut s.composer,
        acts,
        modal,
    };
    let to_end = s.conversation_to_end && !s.comments.loading;
    s.conversation_to_end &= !to_end;
    egui::ScrollArea::vertical()
        .id_salt("review-conversation")
        .auto_shrink(false)
        .show(ui, |ui| {
            let c = &s.comments;
            if c.loading && c.threads.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading comments...");
                });
                return;
            }
            ui.horizontal(|ui| {
                let name = forge.change_name();
                let title = format!("{}{} comments", name[..1].to_uppercase(), &name[1..]);
                ui.label(RichText::new(title).strong().size(15.0));
                let open = dr.composer.editor_is(&Target::Pr);
                let read_only = dr.composer.read_only;
                if ui
                    .add_enabled(
                        !dr.composer.running() && !open && !read_only,
                        egui::Button::new(
                            RichText::new(format!("Comment on {}", forge.change_noun())).strong(),
                        ),
                    )
                    .on_hover_text(format!("A comment on the whole {name}, saved as a draft"))
                    .on_disabled_hover_text(if read_only {
                        super::composer::READ_ONLY_TIP
                    } else {
                        "A comment is being written or posted"
                    })
                    .clicked()
                {
                    dr.acts.push(Act::Open(Target::Pr));
                }
            });
            ui.add_space(4.0);
            // the PR-level composer and drafts first, where the button is
            editor_widget(ui, dr.composer, &Target::Pr, dr.acts);
            let ids: Vec<i64> = dr
                .composer
                .of_kind(DraftKind::Pr, None)
                .map(|d| d.id)
                .collect();
            for id in ids {
                if let Some(d) = dr.composer.draft(id).cloned() {
                    draft_widget(ui, dr.composer, &d, dr.acts);
                }
            }
            if c.pr_level.is_empty() {
                ui.label(
                    RichText::new(format!("No {} comments.", forge.change_name())).color(GREY),
                );
            }
            for &i in &c.pr_level {
                card_widget(ui, c, i, None, toggles, &mut dr);
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Comments on files").strong().size(15.0));
                ui.checkbox(&mut hide_outdated, "Hide outdated")
                    .on_hover_text(
                        "Leave out threads whose file or line changed since they were made",
                    );
            });
            ui.add_space(4.0);
            // one group per path: the files of the diff, then the paths that are not in it
            let groups: Vec<(&str, bool)> = c
                .paths()
                .into_iter()
                .map(|p| (p, false))
                .chain(c.ghost_paths().into_iter().map(|p| (p, true)))
                .collect();
            if groups.is_empty() {
                ui.label(RichText::new("No file comments.").color(GREY));
            }
            let mut hidden = 0;
            for (path, ghost) in groups {
                let ids: Vec<usize> = if ghost {
                    c.on_ghost(path).to_vec()
                } else {
                    c.for_path(path)
                        .iter()
                        .chain(c.on_old_path(path))
                        .copied()
                        .collect()
                };
                let view = (!ghost).then(|| s.diff.view(path)).flatten();
                let all: Vec<(usize, Option<OutdatedReason>)> =
                    ids.iter().map(|&i| (i, c.outdated_in(i, view))).collect();
                let outdated = all.iter().filter(|(_, r)| r.is_some()).count();
                let shown: Vec<(usize, Option<OutdatedReason>)> = all
                    .into_iter()
                    .filter(|(_, r)| !(hide_outdated && r.is_some()))
                    .collect();
                hidden += ids.len() - shown.len();
                if shown.is_empty() {
                    continue;
                }
                let open = ids.iter().filter(|&&i| is_open(&c.threads[i])).count();
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let tip = if ghost {
                        "Show its comments (the file is not in this diff)"
                    } else {
                        "Show this file's diff"
                    };
                    if ui
                        .link(RichText::new(path).monospace())
                        .on_hover_text(tip)
                        .clicked()
                    {
                        jump_path = Some(path.to_string());
                    }
                    if ghost {
                        ui.label(RichText::new("not in diff").color(GREY).small());
                    }
                    let n = ids.len();
                    let plural = if n == 1 { "" } else { "s" };
                    let mut text = format!("{n} thread{plural}, {open} unresolved");
                    if outdated > 0 {
                        text.push_str(&format!(", {outdated} outdated"));
                    }
                    ui.label(RichText::new(text).color(GREY));
                });
                for (i, reason) in shown {
                    let t = &c.threads[i];
                    let target = match scope(t) {
                        Scope::Line { to: Some(n), .. } => format!("Go to line {n}"),
                        Scope::Line { from: Some(n), .. } => format!("Go to old line {n}"),
                        _ => "Go to file".into(),
                    };
                    if ui
                        .link(RichText::new(target).small())
                        .on_hover_text("Show this thread in the review")
                        .clicked()
                    {
                        jump_thread = Some(i);
                    }
                    let note = reason.map(|r| reason_note(t, r));
                    card_widget(ui, c, i, note, toggles, &mut dr);
                }
            }
            if hidden > 0 {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{hidden} outdated threads hidden")).color(GREY));
            }
            if to_end {
                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
            }
        });
    s.hide_outdated = hide_outdated;
    if let Some(i) = jump_thread {
        s.jump_to_thread(i);
    } else if let Some(p) = jump_path {
        s.open_path(&p);
    }
}

fn draw_diff_pane(
    ui: &mut Ui,
    s: &mut ReviewScreen,
    toggles: &mut Vec<CardToggle>,
    acts: &mut Vec<Act>,
) {
    if let Some(g) = s.files.as_ref().and_then(|l| l.selected_ghost()) {
        draw_ghost_pane(ui, s, g, toggles, acts);
        return;
    }
    let Some(f) = s.selected_file().cloned() else {
        if s.files.is_some() {
            ui.label(RichText::new("Select a file").weak());
        }
        return;
    };
    // the split view needs room for two columns of code: otherwise unified, with a hint
    let fits = split_fits(
        ui.ctx().content_rect().width(),
        ui.available_width() - MINIMAP_W - MINIMAP_GAP,
    );
    let split = s.layout == DiffLayout::Split && fits;
    // sticky file header
    let viewed_now = s
        .files
        .as_ref()
        .and_then(|l| l.selected().map(|i| (i, l.is_viewed(i))));
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        chip(ui.painter(), rect.left(), rect.center().y, f.status);
        let title = match &f.old_path {
            Some(old) => format!("{old} -> {}", f.path),
            None => f.path.clone(),
        };
        ui.label(RichText::new(title).monospace().strong());
        if !f.counts_known {
            ui.label(RichText::new(COUNTS_PENDING).color(GREY))
                .on_hover_text("Counting changed lines");
        } else if !f.binary {
            ui.label(RichText::new(format!("+{}", f.additions)).color(GREEN));
            ui.label(RichText::new(format!("-{}", f.deletions)).color(RED));
        }
        if s.visual.is_some() {
            ui.label(RichText::new("VISUAL").monospace().color(AMBER))
                .on_hover_text("Rows from where v was pressed to the cursor; Esc or v ends it");
        }
        if let Some(p) = s.keys.pending() {
            ui.label(RichText::new(p).monospace().color(GREY));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let viewed_key = if s.vim { "r" } else { "v" };
            if let Some((i, mut v)) = viewed_now {
                if ui
                    .checkbox(&mut v, "Viewed")
                    .on_hover_text(viewed_key)
                    .changed()
                {
                    s.toggle_viewed(i);
                }
            }
            if !s.composer.read_only
                && ui
                    .add_enabled(!s.composer.running(), egui::Button::new("Comment on file"))
                    .on_hover_text("A comment on the whole file. Click a line number for a line.")
                    .clicked()
            {
                acts.push(Act::Open(Target::File(f.path.clone())));
            }
            ui.separator();
            // right to left: Split is drawn first
            let mut lay = s.layout;
            ui.selectable_value(&mut lay, DiffLayout::Split, "Split")
                .on_hover_text("Old and new file side by side (Alt+S)");
            ui.selectable_value(&mut lay, DiffLayout::Unified, "Unified")
                .on_hover_text("Old and new lines in one column (Alt+S)");
            s.set_layout(lay);
            if s.layout == DiffLayout::Split && !fits {
                ui.label(RichText::new("Too narrow for split").color(AMBER))
                    .on_hover_text(
                        "The split view needs a window at least 900 px wide and a diff pane of \
                         600 px: shown unified until there is room",
                    );
            }
        });
    });
    ui.separator();
    if s.search.open {
        search_bar(ui, s);
    }

    let sections_h = (ui.available_height() * 0.3).max(80.0);
    let file_level: Vec<usize> = s
        .comments
        .for_path(&f.path)
        .iter()
        .copied()
        .filter(|&i| matches!(scope(&s.comments.threads[i]), Scope::File(_)))
        .collect();
    let modal = s.modal_open();
    let mut dr = Drafting {
        composer: &mut s.composer,
        acts,
        modal,
    };
    if !file_level.is_empty() {
        let p = f.path.as_str();
        thread_section(
            ui,
            false,
            p,
            &s.comments,
            &file_level,
            sections_h,
            toggles,
            &mut dr,
        );
        ui.separator();
    }
    file_drafts_section(ui, &f.path, sections_h, &mut dr);

    if f.binary {
        // no rows to attach to: line and old-path threads are all outdated here
        let outdated: Vec<usize> = s
            .comments
            .for_path(&f.path)
            .iter()
            .chain(s.comments.on_old_path(&f.path))
            .copied()
            .filter(|&i| !matches!(scope(&s.comments.threads[i]), Scope::File(_)))
            .collect();
        if !outdated.is_empty() {
            let p = f.path.as_str();
            thread_section(
                ui,
                true,
                p,
                &s.comments,
                &outdated,
                sections_h,
                toggles,
                &mut dr,
            );
        }
        s.pending_reveal = None;
        placeholder(ui, "Binary file, not shown.");
        return;
    }
    if let Some(err) = s.patch_errors.get(&f.path).cloned() {
        ui.add_space(8.0);
        ui.colored_label(RED, err);
        if ui.button("Retry").clicked() {
            // `retry_patch`, by field (the composer is borrowed)
            s.patch_errors.remove(&f.path);
            s.requests.push(f.path.clone());
        }
        return;
    }
    let mut restore = s.restore_scroll.take();
    let mut scroll_now = restore.unwrap_or_else(|| s.diff.scroll(&f.path));
    let was_split = s.split_shown;
    let Some(view) = s.diff.view_mut(&f.path) else {
        s.restore_scroll = restore;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading patch...");
        });
        return;
    };
    let relayout = split != was_split;
    if relayout && !view.rows.is_empty() {
        // unified <-> split: the row at the top stays at the top; the selection's rows and
        // side belong to the other layout
        let o = shown_offsets(view, was_split);
        let l = o.row_at(scroll_now.y).unwrap_or(0);
        let row = if was_split {
            view.split().lines[l].any()
        } else {
            l
        };
        let line = line_of(view, split, row);
        scroll_now.y = shown_offsets(view, split).top(line);
        restore = Some(scroll_now);
        view.sel.clear();
        view.sel.side = None;
        s.visual = None;
    }
    s.split_shown = split;
    // threads placed and row heights set only when something changed
    let geo = DiffGeometry::new(ui);
    let composer: &Composer = dr.composer;
    let (threads, path) = (&s.comments.threads, f.path.as_str());
    // the comments layout keeps the top row of the unified offsets; the split view keeps
    // its own line
    let split_anchor = split
        .then(|| view.split_offsets().anchor_at(scroll_now.y))
        .flatten();
    let re = s.comments.layout_with(
        view,
        geo.card_w(ui.available_width() - MINIMAP_W - MINIMAP_GAP),
        scroll_now.y,
        &mut measure_with(ui.painter()),
        composer.version(),
        &mut |rows, placement, width, measure| {
            composer.row_extras(path, rows, placement, threads, width, measure)
        },
    );
    if re.changed {
        ui.ctx().request_repaint();
    }
    if let Some(y) = re.scroll_y {
        // heights above the viewport changed: keep the same top row in view
        let y = match split_anchor {
            Some(a) => view.split_offsets().y_for(a),
            None if split => scroll_now.y,
            None => y,
        };
        restore = Some(Vec2::new(scroll_now.x, y));
    }
    if let Some(root) = s.pending_reveal.take() {
        // a Conversation click: the thread's row, or the top (its outdated section)
        let row = view
            .comments
            .placement
            .inline
            .iter()
            .find(|(_, ids)| ids.iter().any(|&t| threads[t].root.id == root))
            .map(|&(r, _)| r);
        let y = row.map_or(0.0, |r| {
            let line = line_of(view, split, r);
            (shown_offsets(view, split).top(line) - 2.0 * ROW_H).max(0.0)
        });
        restore = Some(Vec2::new(scroll_now.x, y));
    }
    let outdated = view.comments.placement.outdated.clone();
    if !outdated.is_empty() {
        let p = f.path.as_str();
        thread_section(
            ui,
            true,
            p,
            &s.comments,
            &outdated,
            sections_h,
            toggles,
            &mut dr,
        );
        ui.separator();
    }
    unplaced_drafts_section(ui, &f.path, sections_h, &mut dr);
    if view.rows.is_empty() {
        // a binary file looks like this too until numstat flags it
        let text = if f.counts_known {
            "No content changes (rename, mode change or empty file)."
        } else {
            "No text changes to show."
        };
        placeholder(ui, text);
        return;
    }
    if view.plain {
        let text = if f.counts_known {
            format!(
                "Large file ({} changed lines): shown without highlighting.",
                f.additions + f.deletions
            )
        } else {
            "Large file: shown without highlighting.".to_string()
        };
        ui.label(RichText::new(text).color(AMBER));
    }
    // comment markers: one tick per thread / draft row, O(threads + drafts)
    let ticks = {
        let threads = &s.comments.threads;
        let mut marks: Vec<(usize, TickKind)> = view
            .comments
            .placement
            .inline
            .iter()
            .flat_map(|(r, ids)| {
                ids.iter().map(move |&t| {
                    let kind = if is_open(&threads[t]) {
                        TickKind::Unresolved
                    } else {
                        TickKind::Resolved
                    };
                    (*r, kind)
                })
            })
            .collect();
        if let Some(p) = dr.composer.plan_for(&f.path) {
            for (r, slots) in &p.rows {
                if slots
                    .iter()
                    .any(|x| matches!(x, Slot::Draft(_) | Slot::Editor))
                {
                    marks.push((*r, TickKind::Draft));
                }
            }
        }
        if split {
            for m in &mut marks {
                m.0 = view.split().line_of(m.0);
            }
        }
        minimap_ticks(shown_offsets(view, split), marks)
    };
    let full = ui.available_rect_before_wrap();
    let strip = Rect::from_min_max(Pos2::new(full.right() - MINIMAP_W, full.top()), full.max);
    let diff_rect = Rect::from_min_max(
        full.min,
        Pos2::new(strip.left() - MINIMAP_GAP, full.bottom()),
    );
    let search = (s.search.open && !s.search.query.is_empty()).then_some(s.search.query.as_str());
    let opts = RowOpts {
        split,
        search,
        vim: s.vim,
    };
    let offset = ui
        .scope_builder(egui::UiBuilder::new().max_rect(diff_rect), |ui| {
            draw_rows(ui, view, restore, &geo, &s.comments, toggles, &mut dr, opts)
        })
        .inner;
    let offs = shown_offsets(view, split);
    let total = offs.total();
    if let Some(line) = minimap(ui, strip, &ticks, offset.y, diff_rect.height(), total) {
        let y = (offs.top(line) - 2.0 * ROW_H).max(0.0);
        s.restore_scroll = Some(Vec2::new(offset.x, y));
        ui.ctx().request_repaint();
    }
    let top = offs.row_at(offset.y).unwrap_or(0);
    // the service colours from the first visible unified row
    let top = if split {
        view.split().lines.get(top).map_or(0, |l| l.any())
    } else {
        top
    };
    s.visible_now = Some((f.path.clone(), top));
    s.diff_viewport = (offset.y, diff_rect.height());
    if relayout {
        s.diff.forget_other_scrolls(&f.path);
    }
    s.diff.set_scroll(&f.path, offset);
}

/// The diff's search bar (`/`): Enter goes to the next match, Esc closes it.
fn search_bar(ui: &mut Ui, s: &mut ReviewScreen) {
    let (mut go, mut close) = (false, false);
    ui.horizontal(|ui| {
        ui.label(RichText::new("/").monospace().strong());
        let resp = ui.add(
            egui::TextEdit::singleline(&mut s.search.query)
                .id(egui::Id::unique(SEARCH_ID))
                .desired_width(280.0)
                .hint_text("Search in this diff"),
        );
        if std::mem::take(&mut s.search.focus) {
            resp.request_focus();
        }
        if resp.lost_focus() {
            go = ui.input(|i| i.key_pressed(Key::Enter));
            close = ui.input(|i| i.key_pressed(Key::Escape));
        }
        let path = s.selected_path().unwrap_or_default();
        let counted = s
            .search
            .for_key
            .as_ref()
            .is_some_and(|(p, q)| p == path && *q == s.search.query);
        let hint = if s.search.query.is_empty() {
            "Enter: first match, n / N: next / previous, Esc: close".to_string()
        } else if counted {
            match s.search.matches.len() {
                0 => "No match".to_string(),
                1 => "1 matching row".to_string(),
                n => format!("{n} matching rows (n / N)"),
            }
        } else {
            "Enter: find".to_string()
        };
        ui.label(RichText::new(hint).color(GREY));
        if ui.small_button("Close").on_hover_text("Esc").clicked() {
            close = true;
        }
    });
    if go {
        s.search_step(true, 1);
    }
    if close {
        s.search.open = false;
    }
}

/// The threads of "not in diff" entry `g`: no diff, every card marked outdated. Replying
/// and resolving still work (a reply needs no line anchor).
fn draw_ghost_pane(
    ui: &mut Ui,
    s: &mut ReviewScreen,
    g: usize,
    toggles: &mut Vec<CardToggle>,
    acts: &mut Vec<Act>,
) {
    let Some(path) = s.files.as_ref().map(|l| l.ghosts()[g].path.clone()) else {
        return;
    };
    s.pending_reveal = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&path).monospace().strong().color(GREY));
        ui.label(RichText::new("not in diff").color(GREY).small());
    });
    ui.label(
        RichText::new(
            "This file is not in the pull request's current diff: a later push removed it or \
             reverted its changes. Its comments are outdated; you can still read, reply to \
             and resolve them.",
        )
        .color(AMBER),
    );
    ui.separator();
    let ids = s.comments.on_ghost(&path).to_vec();
    let modal = s.modal_open();
    let mut dr = Drafting {
        composer: &mut s.composer,
        acts,
        modal,
    };
    let c = &s.comments;
    egui::ScrollArea::vertical()
        .id_salt(("ghost-threads", &path))
        .auto_shrink(false)
        .show(ui, |ui| {
            for i in ids {
                let note = reason_note(&c.threads[i], OutdatedReason::FileNotInDiff);
                card_widget(ui, c, i, Some(note), toggles, &mut dr);
            }
        });
}

/// Draft file comments of `path` and the file composer, above the diff.
fn file_drafts_section(ui: &mut Ui, path: &str, max_h: f32, dr: &mut Drafting<'_>) {
    let target = Target::File(path.to_string());
    let ids: Vec<i64> = dr
        .composer
        .of_kind(DraftKind::File, Some(path))
        .map(|d| d.id)
        .collect();
    let editing = dr.composer.editor_is(&target);
    if ids.is_empty() && !editing {
        return;
    }
    egui::CollapsingHeader::new(
        RichText::new(format!("Draft file comments ({})", ids.len()))
            .color(DRAFT_BLUE)
            .strong(),
    )
    .id_salt("file-drafts")
    .default_open(true)
    .show_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("file-drafts", "scroll"))
            .max_height(max_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for id in ids {
                    if let Some(d) = dr.composer.draft(id).cloned() {
                        draft_widget(ui, dr.composer, &d, dr.acts);
                    }
                }
                editor_widget(ui, dr.composer, &target, dr.acts);
            });
    });
    ui.separator();
}

/// Inline drafts of `path` whose line is not shown in this diff (and a composer on such a
/// line), so none of them is ever hidden.
fn unplaced_drafts_section(ui: &mut Ui, path: &str, max_h: f32, dr: &mut Drafting<'_>) {
    let (ids, editor) = match dr.composer.plan_for(path) {
        Some(p) => (p.unplaced.clone(), p.editor_unplaced),
        None => return,
    };
    if ids.is_empty() && !editor {
        return;
    }
    egui::CollapsingHeader::new(
        RichText::new(format!("Drafts on lines not in this diff ({})", ids.len()))
            .color(AMBER)
            .strong(),
    )
    .id_salt("unplaced-drafts")
    .default_open(true)
    .show_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("unplaced-drafts", "scroll"))
            .max_height(max_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for id in ids {
                    if let Some(d) = dr.composer.draft(id).cloned() {
                        draft_widget(ui, dr.composer, &d, dr.acts);
                    }
                }
                if editor {
                    if let Some(t) = dr.composer.editor.as_ref().map(|e| e.target.clone()) {
                        editor_widget(ui, dr.composer, &t, dr.acts);
                    }
                }
            });
    });
    ui.separator();
}

/// Horizontal layout of the diff: number gutters, text column, comment cards.
struct DiffGeometry {
    char_w: f32,
    num_w: f32,
}

impl DiffGeometry {
    fn new(ui: &Ui) -> DiffGeometry {
        let char_w = ui
            .painter()
            .layout_no_wrap("M".into(), FontId::monospace(MONO), Color32::WHITE)
            .size()
            .x
            .max(1.0);
        DiffGeometry {
            char_w,
            num_w: 6.0 * char_w,
        }
    }

    /// Where the text column starts, from the left edge of the pane.
    fn text_x(&self) -> f32 {
        2.0 * self.num_w + 2.0 * self.char_w
    }

    fn gutter_w(&self) -> f32 {
        2.0 * self.num_w + 4.0
    }

    /// Cards start right of the gutters and stop short of the scroll bar.
    fn card_x(&self) -> f32 {
        self.gutter_w() + 10.0
    }

    fn card_w(&self, pane_w: f32) -> f32 {
        (pane_w - self.card_x() - 24.0).min(1000.0)
    }
}

/// How the selection maps the pointer to text in the shown layout.
struct SelGeo<'a> {
    offsets: &'a Offsets,
    /// Split view: the lines, and where the right side starts (from the pane's left edge).
    split: Option<(&'a SplitMap, f32)>,
    /// Where the text starts, from the left edge of its side (of the pane, when unified).
    text_x: f32,
}

impl SelGeo<'_> {
    /// The side under pane x `x`; None in the unified view.
    fn side_at(&self, x: f32) -> Option<SplitSide> {
        self.split.map(|(_, half)| {
            if x < half {
                SplitSide::Left
            } else {
                SplitSide::Right
            }
        })
    }

    /// Where `side` starts, from the pane's left edge.
    fn x0(&self, side: Option<SplitSide>) -> f32 {
        match (self.split, side) {
            (Some((_, half)), Some(SplitSide::Right)) => half,
            _ => 0.0,
        }
    }

    /// Screen lines: rows, or split lines.
    fn len(&self, rows: &[prcore::diff::Row]) -> usize {
        self.split.map_or(rows.len(), |(m, _)| m.lines.len())
    }

    /// The text of screen line `line` on `side` (None: padding).
    fn text<'r>(
        &self,
        rows: &'r [prcore::diff::Row],
        line: usize,
        side: Option<SplitSide>,
    ) -> Option<&'r str> {
        let row = match (self.split, side) {
            (Some((m, _)), Some(sd)) => m.lines.get(line)?.side(sd)?,
            _ => line,
        };
        rows.get(row).map(|r| r.text.as_str())
    }

    /// What pane x `x`, content y `y` is over.
    fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        hit_test(self.offsets, x - self.x0(self.side_at(x)), y, self.text_x)
    }
}

/// Pointer and keyboard input for the text selection of the current diff: press-drag,
/// shift-click, double click (word), triple click (row), Esc, Ctrl+A, Ctrl+C, and autoscroll
/// at the edges. Presses in the gutter or on comment cards never start a selection (the
/// gutter is left to the comment composer). In the split view a selection stays on the side
/// it started on. Returns the screen line and side of a press on text (it moves the cursor).
#[allow(clippy::too_many_arguments)]
fn handle_selection(
    ui: &mut Ui,
    rows: &[prcore::diff::Row],
    g: &SelGeo<'_>,
    st: &mut SelectionState,
    vis: Rect,
    scroll_x: f32,
    origin_y: f32,
    char_w: f32,
) -> Option<(usize, Option<SplitSide>)> {
    let hit = |p: Pos2| g.hit(p.x - vis.left(), p.y - origin_y);
    let side_at = |p: Pos2| g.side_at(p.x - vis.left());
    let pos_at = |p: Pos2, side: Option<SplitSide>| -> Option<Pos> {
        let row = g.offsets.row_at(p.y - origin_y)?;
        let x = p.x - vis.left() - g.x0(side) - g.text_x + scroll_x;
        Some(Pos {
            row,
            col: x_to_col(g.text(rows, row, side).unwrap_or(""), x, char_w),
        })
    };
    let mut pressed = None;
    let resp = ui.interact(
        vis,
        ui.scope_id().with("diff-select"),
        Sense::click_and_drag(),
    );
    let (shift, press, latest, down, dt) = ui.input(|i| {
        (
            i.modifiers.shift,
            i.pointer.press_origin(),
            i.pointer.latest_pos(),
            i.pointer.primary_down(),
            i.stable_dt.min(0.1),
        )
    });
    if resp.hovered()
        && resp
            .hover_pos()
            .is_some_and(|p| matches!(hit(p), Some(Hit::Text(_))))
    {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }
    if resp.drag_started() {
        // a gutter press (Press::GutterClick) is left to the comment composer
        if let Some(o) = press.filter(|&o| matches!(press_action(hit(o)), Press::Begin(_))) {
            let side = side_at(o);
            if let Some(at) = pos_at(o, side) {
                st.begin_on(at, shift, side);
                pressed = Some((at.row, side));
            }
        }
    }
    if st.dragging {
        if !down || resp.drag_stopped() {
            st.end_drag();
        } else if let Some(p) = latest {
            // the pointer may wander to the other side: the selection stays on its own
            if let Some(at) = pos_at(p, st.side) {
                st.drag_to(at);
            }
            let dy = autoscroll_delta(p.y, vis.top(), vis.bottom(), dt);
            if dy != 0.0 {
                ui.scroll_with_delta_animation(
                    vec2(0.0, -dy),
                    egui::style::ScrollAnimation::none(),
                );
            }
            ui.ctx().request_repaint();
        }
    }
    if let Some(p) = resp.interact_pointer_pos() {
        let side = side_at(p);
        if let (Press::Begin(row), Some(at)) = (press_action(hit(p)), pos_at(p, side)) {
            let text = g.text(rows, row, side).unwrap_or("");
            if resp.triple_clicked() {
                st.side = side;
                st.select_row_in(text, row);
            } else if resp.double_clicked() {
                st.side = side;
                st.select_word_in(text, at);
            } else if resp.clicked() {
                st.click_on(at, shift, side);
                pressed = Some((row, side));
            }
        }
    }

    let ctx = ui.ctx().clone();
    if ctx.egui_wants_keyboard_input() {
        return pressed;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::A)) {
        st.sel = Selection::all(g.len(rows));
        st.side = g.split.map(|_| st.side.unwrap_or_default());
    }
    if ctx.input(|i| i.key_pressed(Key::Escape)) {
        st.clear();
    }
    if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
        // built here, on demand: never per frame
        if let Some(sel) = st.sel {
            let text = match g.split {
                Some((m, _)) => selected_text_split(rows, m, st.side.unwrap_or_default(), &sel),
                None => selected_text(rows, &sel),
            };
            if !text.is_empty() {
                ctx.copy_text(text);
            }
        }
    }
    pressed
}

fn placeholder(ui: &mut Ui, text: &str) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| ui.label(RichText::new(text).color(GREY)));
}

/// What the diff rows are drawn with this frame.
#[derive(Clone, Copy)]
struct RowOpts<'a> {
    split: bool,
    /// The search query whose matches are highlighted.
    search: Option<&'a str>,
    /// The vim keys are on: the cursor is drawn and `c` is theirs.
    vim: bool,
}

const CURSOR_BG: Color32 = Color32::from_rgba_premultiplied(18, 18, 22, 18);
const CURSOR_BAR: Color32 = Color32::from_rgb(110, 150, 220);
const SEARCH_BG: Color32 = Color32::from_rgba_premultiplied(120, 100, 20, 150);
/// The empty side of a split line (the shorter side of a change).
const PAD_BG: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 70);

/// Paints only the rows (and the thread cards under them) in the viewport; returns the
/// scroll offset to remember. Clicked card titles are pushed to `toggles`.
#[allow(clippy::too_many_arguments)]
fn draw_rows(
    ui: &mut Ui,
    view: &mut FileView,
    restore: Option<Vec2>,
    geo: &DiffGeometry,
    comments: &Comments,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
    opts: RowOpts<'_>,
) -> Vec2 {
    // the selection state is taken out so painting can read the rest of the view
    let mut selection = std::mem::take(&mut view.sel);
    if opts.split {
        view.split_offsets();
    }
    let (offset, pressed) = if opts.split {
        paint_split_rows(
            ui,
            view,
            restore,
            geo,
            comments,
            toggles,
            dr,
            &mut selection,
            opts,
        )
    } else {
        paint_rows(
            ui,
            view,
            restore,
            geo,
            comments,
            toggles,
            dr,
            &mut selection,
            opts,
        )
    };
    view.sel = selection;
    if let Some((line, side)) = pressed {
        // a click on text moves the cursor there
        let side = side.unwrap_or_else(|| view.cursor.map_or(SplitSide::Right, |c| c.side));
        view.cursor = Some(cursor_on(view, opts.split, line, side));
    }
    offset
}

/// Background, +/- sign and text colour of a row kind.
fn row_colors(kind: RowKind) -> (Color32, &'static str, Color32) {
    match kind {
        RowKind::HunkHeader => (Color32::from_rgb(30, 40, 70), "", Color32::LIGHT_BLUE),
        RowKind::Add => (Color32::from_rgb(20, 52, 30), "+", Color32::from_gray(225)),
        RowKind::Del => (Color32::from_rgb(62, 25, 25), "-", Color32::from_gray(225)),
        RowKind::Context => (Color32::TRANSPARENT, "", Color32::from_gray(200)),
        RowKind::NoNewline => (Color32::TRANSPARENT, "", GREY),
    }
}

/// One text column on screen: its clipped painter and where its columns are.
struct TextCol {
    clip: egui::Painter,
    /// x of display column `col0` (the first visible one).
    left: f32,
    /// x of display column 0 (scrolled out of view when > 0).
    left_abs: f32,
    col0: usize,
    ncols: usize,
    char_w: f32,
}

/// Row `i`'s text at `y`: syntax colours, word-diff marks, search matches and the selected
/// columns `sel`, only the visible columns (same columns as plain text: no layout shift).
#[allow(clippy::too_many_arguments)]
fn paint_row_text(
    tc: &TextCol,
    view: &FileView,
    i: usize,
    y: f32,
    fg: Color32,
    font: &FontId,
    sel: Option<Range<usize>>,
    search: Option<&str>,
) {
    let r = &view.rows[i];
    let (char_w, col0, ncols) = (tc.char_w, tc.col0, tc.ncols);
    let cy = y + ROW_H / 2.0;
    let spans = view.spans.as_ref().map_or(&[][..], |s| s[i].as_slice());
    let words = view.word_ranges(i);
    let styled = styled_columns(
        &r.text,
        spans,
        words.as_deref().map_or(&[][..], |w| w.as_slice()),
        col0,
        ncols,
    );
    let mark_bg = match r.kind {
        RowKind::Add => Color32::from_rgb(32, 92, 50),
        _ => Color32::from_rgb(118, 42, 42),
    };
    let fill_cols = |m: &Range<usize>, color: Color32| {
        let x = tc.left + m.start as f32 * char_w;
        let rect = Rect::from_min_size(
            Pos2::new(x, y + 1.0),
            vec2(m.len() as f32 * char_w, ROW_H - 2.0),
        );
        tc.clip.rect_filled(rect, 2.0, color);
    };
    for m in &styled.marks {
        fill_cols(m, mark_bg);
    }
    if let Some(q) = search {
        // visible rows only, while the search bar is open
        let found = find_all(&r.text, q, ignores_case(q));
        if !found.is_empty() {
            for m in &styled_columns(&r.text, &[], &found, col0, ncols).marks {
                fill_cols(m, SEARCH_BG);
            }
        }
    }
    if let Some(span) = sel {
        // behind the text, only the visible columns (a minified line of 1 MB is never
        // walked to its end); a row selected through its end also marks the line break
        // with one extra column when the line ends in view
        let lim = col0 + ncols;
        let (c0, c1) = (span.start.max(col0), span.end.min(lim));
        if c0 < c1 {
            let x0 = tc.left_abs + col_to_x(&r.text, c0, char_w);
            let mut x1 = tc.left_abs + col_to_x(&r.text, c1, char_w);
            if span.end == usize::MAX && x1 < tc.left_abs + lim as f32 * char_w {
                x1 += char_w;
            }
            if x1 > x0 {
                let rect = Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + ROW_H));
                tc.clip.rect_filled(rect, 0.0, SELECTION_BG);
            }
        }
    }
    // one galley per colour run, each placed at its own display column: a glyph whose
    // natural advance differs from its cells never shifts the rest of the line
    for (col, t, color) in styled.run_columns() {
        let galley = tc.clip.layout(
            t.to_string(),
            font.clone(),
            color.unwrap_or(fg),
            f32::INFINITY,
        );
        let at = Align2::LEFT_CENTER
            .anchor_size(Pos2::new(tc.left + col as f32 * char_w, cy), galley.size())
            .min;
        tc.clip.galley(at, galley, fg);
    }
}

/// The comment threads, drafts and composer of unified row `row` in its extra height,
/// stacked from `top`; returns where the next card would start.
#[allow(clippy::too_many_arguments)]
fn paint_cards(
    ui: &mut Ui,
    painter: &egui::Painter,
    view: &FileView,
    comments: &Comments,
    row: usize,
    mut top: f32,
    at_x: f32,
    vis: Rect,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) -> f32 {
    let lay = &view.comments;
    // threads with their reply drafts, then inline drafts and the composer
    let planned = dr
        .composer
        .plan_for(&view.path)
        .and_then(|p| p.rows.get(&row).cloned());
    let slots = planned.unwrap_or_else(|| {
        let at = lay.placement.inline.binary_search_by_key(&row, |(r, _)| *r);
        let ids = at.map_or(&[][..], |k| &lay.placement.inline[k].1[..]);
        ids.iter().map(|&t| Slot::Thread(t)).collect()
    });
    let measure_painter = painter.clone();
    let mut measure = measure_with(&measure_painter);
    for slot in slots {
        match slot {
            Slot::Thread(t) => {
                // the same layout `Comments::layout_with` reserved the row for
                let th = &comments.threads[t];
                let card_lay = comments.card(th, lay.card_w, &mut measure);
                let h = card_lay.height;
                let card = Rect::from_min_size(Pos2::new(at_x, top), vec2(lay.card_w, h));
                if card.intersects(vis) {
                    thread_card(
                        ui,
                        painter,
                        card,
                        th,
                        &card_lay,
                        None,
                        "thread-row",
                        toggles,
                        dr,
                    );
                }
                top += h + CARD_MARGIN;
            }
            Slot::Draft(id) => {
                let Some(d) = dr.composer.draft(id) else {
                    continue;
                };
                let state = dr.composer.state(id);
                let h = draft_height(d, &state, lay.card_w, &mut measure);
                let card = Rect::from_min_size(Pos2::new(at_x, top), vec2(lay.card_w, h));
                if card.intersects(vis) {
                    draft_card(ui, painter, card, d, &state, dr.composer, dr.acts);
                }
                top += h + CARD_MARGIN;
            }
            Slot::Editor => {
                let h = editor_height();
                let card = Rect::from_min_size(Pos2::new(at_x, top), vec2(lay.card_w, h));
                if let Some(e) = dr.composer.editor.as_mut() {
                    if card.intersects(vis) {
                        editor_card(ui, painter, card, e, dr.acts);
                    }
                }
                top += h + CARD_MARGIN;
            }
        }
    }
    top
}

/// The cursor row's subtle highlight: a fill and a bar at its left edge.
fn paint_cursor(painter: &egui::Painter, row: Rect) {
    painter.rect_filled(row, 0.0, CURSOR_BG);
    painter.rect_filled(
        Rect::from_min_size(row.min, vec2(2.0, row.height())),
        0.0,
        CURSOR_BAR,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_rows(
    ui: &mut Ui,
    view: &FileView,
    restore: Option<Vec2>,
    geo: &DiffGeometry,
    comments: &Comments,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
    selection: &mut SelectionState,
    opts: RowOpts<'_>,
) -> (Vec2, Option<(usize, Option<SplitSide>)>) {
    let font = FontId::monospace(MONO);
    let (char_w, num_w) = (geo.char_w, geo.num_w);
    let text_x = geo.text_x();
    let content_w = text_x + (view.max_cols as f32 + 2.0) * char_w;
    let total_h = view.offsets.total();
    let mut pressed = None;

    // drag moves the selection, not the scroll position
    let mut area = egui::ScrollArea::both()
        .id_salt("review-diff")
        .auto_shrink(false)
        .scroll_source(ScrollSource {
            drag: DragScroll::Never,
            mouse_wheel: true,
            scroll_bar: true,
        });
    if let Some(off) = restore {
        area = area.scroll_offset(off);
    }
    let out = area.show_viewport(ui, |ui, vp| {
        ui.set_height(total_h);
        ui.set_width(content_w.max(ui.available_width()));
        let origin = ui.max_rect().min;
        let vis = Rect::from_min_size(origin + vp.min.to_vec2(), vp.size());
        let painter = ui.painter_at(vis);
        let col0 = (vp.min.x / char_w).floor().max(0.0) as usize;
        let tc = TextCol {
            clip: painter.with_clip_rect(Rect::from_min_max(
                Pos2::new(vis.left() + text_x, vis.top()),
                vis.max,
            )),
            left: vis.left() + text_x - (vp.min.x - col0 as f32 * char_w),
            left_abs: vis.left() + text_x - vp.min.x,
            col0,
            ncols: ((vp.width() - text_x).max(0.0) / char_w).ceil() as usize + 2,
            char_w,
        };

        let g = SelGeo {
            offsets: &view.offsets,
            split: None,
            text_x,
        };
        pressed = handle_selection(
            ui, &view.rows, &g, selection, vis, vp.min.x, origin.y, char_w,
        );
        // `c` (vim keys off): comment on the row under the pointer, else on the last
        // selected row
        let ctx = ui.ctx().clone();
        let typing = ctx.egui_wants_keyboard_input();
        if !opts.vim
            && review_keys_allowed(typing, ctx.input(|i| i.modifiers), dr.modal)
            && ctx.input(|i| i.key_pressed(Key::C))
        {
            let hovered = ctx
                .input(|i| i.pointer.hover_pos())
                .filter(|p| vis.contains(*p))
                .and_then(|p| {
                    match hit_test(&view.offsets, p.x - vis.left(), p.y - origin.y, text_x) {
                        Some(Hit::Gutter(r) | Hit::Text(r)) => Some(r),
                        _ => None,
                    }
                });
            let last = selection.sel.map(|s| s.head.row);
            if let Some(t) = c_key_target(&view.rows, &view.path, hovered, last) {
                dr.acts.push(Act::Open(t));
            }
        }
        let sel_range = selection.sel.filter(|s| !s.is_empty()).map(|s| s.range());
        // rows with threads (any unresolved?) and rows with drafts, for the gutter dots;
        // O(threads + drafts), looked up per visible row
        let thread_rows: Vec<(usize, bool)> = view
            .comments
            .placement
            .inline
            .iter()
            .map(|(r, ids)| (*r, ids.iter().any(|&t| is_open(&comments.threads[t]))))
            .collect();
        let draft_rows: std::collections::HashSet<usize> = dr
            .composer
            .plan_for(&view.path)
            .map(|p| {
                p.rows
                    .iter()
                    .filter(|(_, sl)| sl.iter().any(|x| matches!(x, Slot::Draft(_))))
                    .map(|(r, _)| *r)
                    .collect()
            })
            .unwrap_or_default();
        let cursor = opts.vim.then_some(view.cursor).flatten();

        for i in view.offsets.visible_range(vp.min.y, vp.max.y) {
            let r = &view.rows[i];
            let y = origin.y + view.offsets.top(i);
            let row = Rect::from_min_size(Pos2::new(vis.left(), y), vec2(vis.width(), ROW_H));
            let cy = y + ROW_H / 2.0;
            let (bg, sign, fg) = row_colors(r.kind);
            painter.rect_filled(row, 0.0, bg);
            if cursor.is_some_and(|c| c.row == i) {
                paint_cursor(&painter, row);
            }
            // gutters stay put while the text scrolls sideways
            if r.kind != RowKind::HunkHeader {
                let gut = Rect::from_min_size(row.min, vec2(2.0 * num_w + 4.0, ROW_H));
                painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                gutter_comment(ui, &painter, gut, num_w, i, r, &view.path, dr, None);
                // a dot for threads / drafts under this row, visible with the card collapsed
                if let Ok(k) = thread_rows.binary_search_by_key(&i, |x| x.0) {
                    let c = if thread_rows[k].1 { AMBER } else { GREY };
                    painter.circle_filled(Pos2::new(gut.right() - 4.0, cy), 2.5, c);
                }
                if draft_rows.contains(&i) {
                    painter.circle_filled(Pos2::new(gut.right() - 4.0, cy + 5.0), 2.0, DRAFT_BLUE);
                }
            }
            let num = |n: Option<u32>, right: f32| {
                if let Some(n) = n {
                    painter.text(
                        Pos2::new(right, cy),
                        Align2::RIGHT_CENTER,
                        n.to_string(),
                        font.clone(),
                        DIM,
                    );
                }
            };
            num(r.old_line, vis.left() + num_w - 4.0);
            num(r.new_line, vis.left() + 2.0 * num_w - 4.0);
            if !sign.is_empty() {
                painter.text(
                    Pos2::new(vis.left() + 2.0 * num_w + 0.6 * char_w, cy),
                    Align2::LEFT_CENTER,
                    sign,
                    font.clone(),
                    fg,
                );
            }
            let span = sel_range.and_then(|(lo, hi)| row_span(r, i, lo, hi));
            paint_row_text(&tc, view, i, y, fg, &font, span, opts.search);
            // comment threads anchored on this row, in its extra height
            let extra = view.offsets.height(i) - ROW_H;
            if extra > 0.0 {
                let gut = Rect::from_min_size(
                    Pos2::new(vis.left(), y + ROW_H),
                    vec2(geo.gutter_w(), extra),
                );
                painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                let at_x = vis.left() + geo.card_x();
                let top = y + ROW_H + CARD_MARGIN;
                paint_cards(ui, &painter, view, comments, i, top, at_x, vis, toggles, dr);
            }
        }
    });
    (out.state.clamped_offset(), pressed)
}

/// Which side of the split view the gutter dot of thread `t` on row `r` goes to: the side
/// the row is on, and for a row on both sides (context) the side of the thread's anchor.
fn thread_side(
    map: &SplitMap,
    rows: &[prcore::diff::Row],
    r: usize,
    t: &prcore::comments::Thread,
) -> SplitSide {
    match scope(t) {
        Scope::Line { to: Some(n), .. } if rows[r].new_line == Some(n) => {
            map.side_of(r, SplitSide::Right)
        }
        _ => map.side_of(r, SplitSide::Left),
    }
}

/// The split view: old file left, new file right, a change's deletions and additions line by
/// line (see `split::pair_lines`). Virtualized like the unified view: only the lines in the
/// viewport are painted, found by binary search in the split line offsets.
#[allow(clippy::too_many_arguments)]
fn paint_split_rows(
    ui: &mut Ui,
    view: &FileView,
    restore: Option<Vec2>,
    geo: &DiffGeometry,
    comments: &Comments,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
    selection: &mut SelectionState,
    opts: RowOpts<'_>,
) -> (Vec2, Option<(usize, Option<SplitSide>)>) {
    let font = FontId::monospace(MONO);
    let (char_w, num_w) = (geo.char_w, geo.num_w);
    let map = view.split();
    let Some(offs) = view.split_offsets_ref() else {
        return (Vec2::ZERO, None);
    };
    // each side: one number column, the sign, then the text
    let text_x = num_w + 2.0 * char_w;
    let total_h = offs.total();
    let mut pressed = None;

    let mut area = egui::ScrollArea::both()
        .id_salt("review-diff")
        .auto_shrink(false)
        .scroll_source(ScrollSource {
            drag: DragScroll::Never,
            mouse_wheel: true,
            scroll_bar: true,
        });
    if let Some(off) = restore {
        area = area.scroll_offset(off);
    }
    let out = area.show_viewport(ui, |ui, vp| {
        let half = (vp.width() / 2.0).floor();
        // scrolled sideways, both sides move together until the widest line's end shows
        let content_w = half + text_x + (view.max_cols as f32 + 2.0) * char_w;
        ui.set_height(total_h);
        ui.set_width(content_w.max(ui.available_width()));
        let origin = ui.max_rect().min;
        let vis = Rect::from_min_size(origin + vp.min.to_vec2(), vp.size());
        let painter = ui.painter_at(vis);
        let col0 = (vp.min.x / char_w).floor().max(0.0) as usize;
        let side_col = |x0: f32, x1: f32| TextCol {
            clip: painter.with_clip_rect(Rect::from_min_max(
                Pos2::new(x0 + text_x, vis.top()),
                Pos2::new(x1, vis.bottom()),
            )),
            left: x0 + text_x - (vp.min.x - col0 as f32 * char_w),
            left_abs: x0 + text_x - vp.min.x,
            col0,
            ncols: ((x1 - x0 - text_x).max(0.0) / char_w).ceil() as usize + 2,
            char_w,
        };
        let mid = vis.left() + half;
        let cols = [
            (SplitSide::Left, vis.left(), side_col(vis.left(), mid - 1.0)),
            (SplitSide::Right, mid, side_col(mid, vis.right())),
        ];
        // hunk headers span both sides
        let header_col = side_col(vis.left(), vis.right());

        let g = SelGeo {
            offsets: offs,
            split: Some((map, half)),
            text_x,
        };
        pressed = handle_selection(
            ui, &view.rows, &g, selection, vis, vp.min.x, origin.y, char_w,
        );
        // `c` (vim keys off): the line and side under the pointer, else the selection's end
        let ctx = ui.ctx().clone();
        let typing = ctx.egui_wants_keyboard_input();
        if !opts.vim
            && review_keys_allowed(typing, ctx.input(|i| i.modifiers), dr.modal)
            && ctx.input(|i| i.key_pressed(Key::C))
        {
            let hovered = ctx
                .input(|i| i.pointer.hover_pos())
                .filter(|p| vis.contains(*p))
                .and_then(|p| match g.hit(p.x - vis.left(), p.y - origin.y) {
                    Some(Hit::Gutter(l) | Hit::Text(l)) => {
                        Some((l, g.side_at(p.x - vis.left()).unwrap_or_default()))
                    }
                    _ => None,
                });
            let last = selection
                .sel
                .map(|s| (s.head.row, selection.side.unwrap_or_default()));
            let target = hovered
                .or(last)
                .and_then(|(l, side)| comment_target(&view.rows, *map.lines.get(l)?, side));
            if let Some((side, line)) = target {
                dr.acts.push(Act::Open(Target::Inline {
                    path: view.path.clone(),
                    side,
                    line,
                    start: None,
                }));
            }
        }
        let sel_range = selection.sel.filter(|s| !s.is_empty()).map(|s| s.range());
        // gutter dots by (row, side); O(threads + drafts), looked up per visible row
        let mut thread_marks: Vec<(usize, SplitSide, bool)> = view
            .comments
            .placement
            .inline
            .iter()
            .flat_map(|(r, ids)| {
                ids.iter().map(move |&t| {
                    let th = &comments.threads[t];
                    (*r, thread_side(map, &view.rows, *r, th), is_open(th))
                })
            })
            .collect();
        thread_marks.sort_unstable_by_key(|&(r, s, open)| (r, s as u8, !open));
        let draft_marks: std::collections::HashSet<(usize, SplitSide)> = dr
            .composer
            .plan_for(&view.path)
            .map(|p| {
                p.rows
                    .iter()
                    .flat_map(|(r, sl)| {
                        sl.iter().filter_map(move |x| match x {
                            Slot::Draft(id) => Some((*r, *id)),
                            _ => None,
                        })
                    })
                    .map(|(r, id)| {
                        let prefer = match dr.composer.draft(id).and_then(|d| d.side) {
                            Some(Side::Old) => SplitSide::Left,
                            _ => SplitSide::Right,
                        };
                        (r, map.side_of(r, prefer))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cursor = opts
            .vim
            .then_some(view.cursor)
            .flatten()
            .map(|c| (map.line_of(c.row), map.side_of(c.row, c.side)));
        let divider = egui::Stroke::new(1.0, Color32::from_gray(60));

        for l in offs.visible_range(vp.min.y, vp.max.y) {
            let line = map.lines[l];
            let y = origin.y + offs.top(l);
            let cy = y + ROW_H / 2.0;
            let shared = line.shared();
            if shared && view.rows[line.any()].kind == RowKind::HunkHeader {
                let i = line.any();
                let (bg, _, fg) = row_colors(RowKind::HunkHeader);
                let row = Rect::from_min_size(Pos2::new(vis.left(), y), vec2(vis.width(), ROW_H));
                painter.rect_filled(row, 0.0, bg);
                if cursor.is_some_and(|c| c.0 == l) {
                    paint_cursor(&painter, row);
                }
                // a header is not split: it is selected (and copied) from either side
                let span = sel_range.and_then(|(lo, hi)| row_span(&view.rows[i], l, lo, hi));
                paint_row_text(&header_col, view, i, y, fg, &font, span, opts.search);
            } else {
                for (side, x0, tc) in &cols {
                    let (side, x0) = (*side, *x0);
                    let w = if side == SplitSide::Left {
                        half
                    } else {
                        vis.right() - x0
                    };
                    let cell = Rect::from_min_size(Pos2::new(x0, y), vec2(w, ROW_H));
                    let Some(i) = line.side(side) else {
                        painter.rect_filled(cell, 0.0, PAD_BG);
                        continue;
                    };
                    let r = &view.rows[i];
                    let (bg, sign, fg) = row_colors(r.kind);
                    painter.rect_filled(cell, 0.0, bg);
                    if cursor == Some((l, side)) {
                        paint_cursor(&painter, cell);
                    }
                    let gut = Rect::from_min_size(cell.min, vec2(num_w + 4.0, ROW_H));
                    painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                    let forced = match side {
                        SplitSide::Left => Side::Old,
                        SplitSide::Right => Side::New,
                    };
                    gutter_comment(ui, &painter, gut, num_w, i, r, &view.path, dr, Some(forced));
                    let k = thread_marks.partition_point(|m| m.0 < i);
                    if let Some(m) = thread_marks[k..]
                        .iter()
                        .take_while(|m| m.0 == i)
                        .find(|m| m.1 == side)
                    {
                        let c = if m.2 { AMBER } else { GREY };
                        painter.circle_filled(Pos2::new(gut.right() - 4.0, cy), 2.5, c);
                    }
                    if draft_marks.contains(&(i, side)) {
                        painter.circle_filled(
                            Pos2::new(gut.right() - 4.0, cy + 5.0),
                            2.0,
                            DRAFT_BLUE,
                        );
                    }
                    let n = match side {
                        SplitSide::Left => r.old_line,
                        SplitSide::Right => r.new_line,
                    };
                    if let Some(n) = n {
                        painter.text(
                            Pos2::new(x0 + num_w - 4.0, cy),
                            Align2::RIGHT_CENTER,
                            n.to_string(),
                            font.clone(),
                            DIM,
                        );
                    }
                    if !sign.is_empty() {
                        painter.text(
                            Pos2::new(x0 + num_w + 0.6 * char_w, cy),
                            Align2::LEFT_CENTER,
                            sign,
                            font.clone(),
                            fg,
                        );
                    }
                    let span = sel_range
                        .filter(|_| selection.side.unwrap_or_default() == side)
                        .and_then(|(lo, hi)| row_span(r, l, lo, hi));
                    paint_row_text(tc, view, i, y, fg, &font, span, opts.search);
                }
                painter.line_segment([Pos2::new(mid, y), Pos2::new(mid, y + ROW_H)], divider);
            }
            // the cards of both rows, full width under the line (left row's first)
            let extra = offs.height(l) - ROW_H;
            if extra > 0.0 {
                let gut = Rect::from_min_size(
                    Pos2::new(vis.left(), y + ROW_H),
                    vec2(geo.gutter_w(), extra),
                );
                painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                let at_x = vis.left() + geo.card_x();
                let mut top = y + ROW_H;
                let right = line.right().filter(|_| !shared);
                for i in [line.left(), right].into_iter().flatten() {
                    let own = view.offsets.height(i) - ROW_H;
                    if own > 0.0 {
                        paint_cards(
                            ui,
                            &painter,
                            view,
                            comments,
                            i,
                            top + CARD_MARGIN,
                            at_x,
                            vis,
                            toggles,
                            dr,
                        );
                        top += own;
                    }
                }
            }
        }
    });
    (out.state.clamped_offset(), pressed)
}

fn tick_color(k: TickKind) -> Color32 {
    match k {
        TickKind::Unresolved => AMBER,
        TickKind::Resolved => GREY,
        TickKind::Draft => DRAFT_BLUE,
    }
}

/// The comment marker strip: the viewport as a faint box, one tick per thread / draft
/// row. Returns the row of a clicked tick.
fn minimap(
    ui: &mut Ui,
    strip: Rect,
    ticks: &[Tick],
    scroll_y: f32,
    view_h: f32,
    total: f32,
) -> Option<usize> {
    let resp = ui.interact(strip, ui.scope_id().with("diff-minimap"), Sense::click());
    let p = ui.painter_at(strip);
    p.rect_filled(strip, 2.0, Color32::from_black_alpha(50));
    let h = strip.height().max(1.0);
    // content shorter than the viewport: rows and ticks line up
    let used = if total > 0.0 {
        h * (total / total.max(view_h)).min(1.0)
    } else {
        h
    };
    if total > 0.0 {
        let y0 = strip.top() + (scroll_y / total).clamp(0.0, 1.0) * used;
        let y1 = strip.top() + ((scroll_y + view_h) / total).clamp(0.0, 1.0) * used;
        p.rect_filled(
            Rect::from_min_max(Pos2::new(strip.left(), y0), Pos2::new(strip.right(), y1)),
            2.0,
            Color32::from_white_alpha(14),
        );
    }
    for t in ticks {
        let y = strip.top() + t.frac * used;
        let r = Rect::from_min_size(
            Pos2::new(strip.left() + 1.0, y - 1.5),
            vec2(strip.width() - 2.0, 3.0),
        );
        p.rect_filled(r, 1.0, tick_color(t.kind));
    }
    let tol = 5.0 / h;
    let at = |pos: Pos2| tick_at(ticks, (pos.y - strip.top()) / used.max(1.0), tol);
    if resp.hover_pos().and_then(at).is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        return resp.interact_pointer_pos().and_then(at);
    }
    None
}

/// Lines on screen: unified rows, or split lines.
fn display_len(v: &FileView, split: bool) -> usize {
    if split {
        v.split().lines.len()
    } else {
        v.rows.len()
    }
}

/// The screen line of unified row `row`.
fn line_of(v: &FileView, split: bool, row: usize) -> usize {
    if split {
        v.split().line_of(row)
    } else {
        row
    }
}

/// The unified row shown on `line` on `side` (the other side's on padding).
fn row_on(v: &FileView, split: bool, line: usize, side: SplitSide) -> usize {
    if !split {
        return line;
    }
    let l = v.split().lines[line];
    l.side(side).or(l.side(side.other())).unwrap_or(0)
}

/// The cursor on `line`, keeping the side it prefers.
fn cursor_on(v: &FileView, split: bool, line: usize, side: SplitSide) -> DiffCursor {
    DiffCursor {
        row: row_on(v, split, line, side),
        side,
    }
}

/// The line tops of the shown layout.
fn shown_offsets(v: &mut FileView, split: bool) -> &Offsets {
    if split {
        v.split_offsets()
    } else {
        &v.offsets
    }
}

/// `{N}G`: the first row showing the new file's line N or a later one, else the last row.
fn goto_line(rows: &[prcore::diff::Row], n: u32) -> usize {
    rows.iter()
        .position(|r| r.new_line.is_some_and(|l| l >= n))
        .unwrap_or(rows.len().saturating_sub(1))
}

/// The next (or previous) hunk header after (before) row `from`.
fn next_hunk(rows: &[prcore::diff::Row], from: usize, forward: bool) -> Option<usize> {
    let header = |i: &usize| rows[*i].kind == RowKind::HunkHeader;
    if forward {
        (from + 1..rows.len()).find(header)
    } else {
        (0..from.min(rows.len())).rev().find(header)
    }
}

/// The scroll offset that brings a row at `top` into a viewport at `scroll` of height `h`,
/// with a margin of two rows; None when it is already in view (no jitter).
fn reveal_y(top: f32, scroll: f32, h: f32) -> Option<f32> {
    if h <= 0.0 {
        return None;
    }
    let margin = (2.0 * ROW_H).min(h / 4.0);
    if top < scroll + margin {
        Some(top - margin)
    } else if top + ROW_H > scroll + h - margin {
        Some(top + ROW_H + margin - h)
    } else {
        None
    }
}

/// What `c` comments on: the hovered row, else the last selected row; the new side when
/// the row has a new line (added and context lines), the old side for deleted lines.
fn c_key_target(
    rows: &[prcore::diff::Row],
    path: &str,
    hovered: Option<usize>,
    last_selected: Option<usize>,
) -> Option<Target> {
    let r = rows.get(hovered.or(last_selected)?)?;
    let (side, line) = match r.kind {
        RowKind::Add | RowKind::Context => (Side::New, r.new_line?),
        RowKind::Del => (Side::Old, r.old_line?),
        RowKind::HunkHeader | RowKind::NoNewline => return None,
    };
    Some(Target::Inline {
        path: path.to_string(),
        side,
        line,
        start: None,
    })
}

/// The line-number gutter of one row: hovering shows "+", a click opens the composer on
/// that line. Added lines are new side, deleted lines old side; on context lines the
/// clicked number column decides and Alt picks the other side. In the split view each side
/// has its own gutter: `forced` is that side.
#[allow(clippy::too_many_arguments)]
fn gutter_comment(
    ui: &mut Ui,
    painter: &egui::Painter,
    gut: Rect,
    num_w: f32,
    i: usize,
    r: &prcore::diff::Row,
    path: &str,
    dr: &mut Drafting<'_>,
    forced: Option<Side>,
) {
    if !matches!(r.kind, RowKind::Context | RowKind::Add | RowKind::Del) {
        return;
    }
    let id = match forced {
        None => ui.scope_id().with(("gutter", i)),
        Some(s) => ui.scope_id().with(("gutter", i, s == Side::Old)),
    };
    let resp = ui
        .interact(gut, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.hovered() && !dr.composer.running() {
        let plus = Rect::from_min_size(gut.min + vec2(1.0, 2.0), vec2(14.0, 14.0));
        painter.rect_filled(plus, 3.0, DRAFT_BLUE);
        painter.text(
            plus.center(),
            Align2::CENTER_CENTER,
            "+",
            FontId::monospace(12.0),
            Color32::WHITE,
        );
    }
    let resp = if r.kind == RowKind::Context && forced.is_none() {
        resp.on_hover_text("Comment on this line (Alt-click: the other side)")
    } else {
        resp.on_hover_text("Comment on this line")
    };
    if !resp.clicked() {
        return;
    }
    let alt = ui.input(|inp| inp.modifiers.alt);
    let old_col = resp
        .interact_pointer_pos()
        .is_some_and(|p| p.x < gut.left() + num_w);
    let side = match (forced, r.kind) {
        (Some(s), _) => s,
        (None, RowKind::Add) => Side::New,
        (None, RowKind::Del) => Side::Old,
        _ if old_col != alt => Side::Old,
        _ => Side::New,
    };
    let line = match side {
        Side::New => r.new_line,
        Side::Old => r.old_line,
    };
    if let Some(line) = line {
        dr.acts.push(Act::Open(Target::Inline {
            path: path.to_string(),
            side,
            line,
            start: None,
        }));
    }
}

#[cfg(test)]
mod tests;
