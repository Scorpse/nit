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
use prcore::history::caught_up_key;
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
use super::commits_ui::{Commits, Want};
use super::composer::{
    confirm_dialog, draft_card, draft_height, draft_widget, drafts_bar, editor_card, editor_height,
    editor_widget, reply_widgets, thread_buttons, toast, Act, Composer, MemoryPoster, Poster, Slot,
    Target,
};
use super::conflicts_ui::Conflicts;
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
use super::tasks_ui::Tasks;
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
    pub commits: Commits,
    pub tab: Tab,
    /// The PR's tasks and the one being added (Bitbucket).
    pub tasks: Tasks,
    /// Whether the PR merges into its destination, and the conflicted files.
    pub conflicts: Conflicts,
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
    Commits,
    Tasks,
    Conflicts,
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
    /// Screenshot scene variant: open the Commits tab.
    pub(crate) shot_commits: bool,
    /// Screenshot scene variant: open the Tasks tab.
    pub(crate) shot_tasks: bool,
    /// Screenshot scene variant: open the Conflicts tab with sample conflicts.
    pub(crate) shot_conflicts: bool,
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

mod commits;
mod conflicts;
mod conversation;
mod diffpane;
mod filelist;
mod host;
mod keys;
mod minimap;
mod rows;
mod screen;
mod selecting;
mod tasks;
mod topbar;

use self::commits::*;
use self::conflicts::*;
use self::conversation::*;
use self::diffpane::*;
use self::filelist::*;
pub use self::host::*;
pub use self::keys::*;
use self::minimap::*;
use self::rows::*;
use self::selecting::*;
use self::tasks::*;
pub use self::topbar::*;

#[cfg(test)]
mod tests;
