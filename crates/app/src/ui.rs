//! Application state and screens (login and inbox).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use prcore::advice::Advice;
use prcore::model::{AccountId, Forge, PullRequest, RepoRef, Reviewer, UserRef};
use sync::SyncEvent;

use crate::backend::{Backend, Msg, Snapshot, CACHE_READ_FAILED};
use crate::inbox::{build_inbox_for, scope_chips, stack_advice, Inbox, Scope, View};
use crate::list::{
    build_items, item_pos, nav_items, reveal_offset, row_click, show_detail, show_list, Detail,
    Item, Nav, RowClick, ROW_H,
};
use crate::lock::lock_ok;
use crate::repo_picker::{show_picker, PickerAction, RepoPicker, NONE_CHOSEN};
use crate::review::keymap::{vim_keys_on, vim_keys_value, VIM_META_KEY};
use crate::review::split::{DiffLayout, LAYOUT_META_KEY};
use crate::review::ui::nav_keys_allowed;
use crate::review::ui::{open_review, ReviewHost};
use crate::settings_ui::{
    self, default_download_dir, review_button_state, show_settings, NewAccount, ReviewButton,
    SettingsAction, SettingsState,
};
use crate::theme::{AMBER, RED};
use crate::welcome;

const REFRESH_EVERY: Duration = Duration::from_millis(500);
/// With no known user, "Mine" and "Needs me" would hide everything; show all instead.
pub fn effective_view(me_known: bool, view: View) -> View {
    match view {
        View::Mine | View::NeedsMe if !me_known => View::All,
        v => v,
    }
}

const BENCH_FRAMES: usize = 300;
/// Changed files in the `--bench-review` demo PR.
pub const BENCH_REVIEW_FILES: usize = 1000;
/// `--bench-review` gives up (error, exit 1) when the review screen is not ready by then.
const BENCH_REVIEW_MAX_FRAMES: usize = 3000;
const BENCH_REVIEW_MAX_WAIT: Duration = Duration::from_secs(120);

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Screen {
    Login,
    Inbox,
    Settings,
    /// The PR in `App::review`.
    Review,
    /// "Choose repositories" (`App::picker`).
    Repos,
}

/// Extra demo state for a screenshot.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    Normal,
    /// The login screen right after a 401.
    Rejected,
    /// A sync in which every repo answered 403.
    Denied,
    /// The review screen on the demo file with comment threads.
    ReviewComments,
    /// `ReviewComments` with sample drafts (one blocked) and an open composer.
    ReviewDrafts,
    /// The review screen with a few diff rows selected.
    ReviewSelection,
    /// The review screen with the Approve confirmation open (stack and comment warnings).
    ReviewActions,
    /// The review screen's file tree: nested directories, comment markers, viewed marks.
    ReviewTree,
    /// The review screen on the Conversation tab (long, threaded and unplaced comments).
    ReviewConversation,
    /// `ReviewConversation` scrolled to its end (the unplaced comments).
    ReviewConversationEnd,
    /// The review screen on the Commits tab: a commit after my last review is marked new.
    ReviewCommits,
    /// The review screen with a "not in diff" entry selected (comments on a file that is
    /// not in the current diff).
    ReviewOutdated,
    /// The commented demo file in the split (side-by-side) view.
    ReviewSplit,
    /// The review screen with the keyboard help (`?`) open.
    ReviewHelp,
    /// The review of a GitHub PR: read-only (no Approve, Request changes, Comment, Reply,
    /// Resolve; a "Read-only" badge instead).
    ReviewGitHub,
    /// The review of a GitLab MR (Approve, Request changes, Comment, Reply, Resolve), its
    /// comments open.
    ReviewGitLab,
    /// Settings > Add account with GitLab picked (self-managed address, Test connection
    /// answered).
    AddGitLab,
    /// Settings with edits that need attention (validation messages, Save blocked, old git).
    SettingsProblems,
    /// Several accounts: the Accounts card with "Add account" open, and the repository
    /// picker grouped by account.
    Accounts,
}

/// Scripted `--shot` run: which screen to capture.
pub struct Shot {
    pub path: String,
    pub screen: Screen,
    pub view: View,
    pub select: Option<(&'static str, u64)>,
    pub scene: Scene,
    /// `--shots`: the result lands here instead of the process exiting after the save.
    pub done: Option<ShotDone>,
}

/// Where a `--shots` scene reports its screenshot: `None` until written, then the outcome.
pub type ShotDone = Arc<Mutex<Option<Result<(), String>>>>;

#[derive(Default)]
struct Login {
    email: String,
    token: String,
    error: Option<String>,
    busy: bool,
    /// Credentials are already in the store (cached; the store is not polled per frame).
    has_saved: bool,
}

#[derive(Default)]
struct SyncStatus {
    connecting: bool,
    running: bool,
    total: usize,
    done: usize,
    diffstats: usize,
    /// Repos that failed with "access denied" in this sync.
    denied: usize,
    finished: bool,
    note: Option<String>,
}

pub struct App {
    backend: Option<Backend>,
    pub(crate) screen: Screen,
    pub(crate) review: ReviewHost,
    login: Login,
    me: UserRef,
    data: Snapshot,
    view: View,
    query: String,
    inbox: Inbox,
    items: Vec<Item>,
    dirty: bool,
    selected: Option<(RepoRef, u64)>,
    detail: Option<Detail>,
    sync: SyncStatus,
    status_label: Option<String>,
    show_errors: bool,
    refresh_pending: bool,
    last_refresh: Instant,
    shot: Option<Shot>,
    shot_taken: bool,
    /// The shot was taken at the frame cap, before the scene had settled.
    pub(crate) shot_timed_out: bool,
    frame: usize,
    bench: Option<Vec<f32>>,
    /// `--bench-review`: the bench runs on the review screen of the 1000-file demo PR.
    bench_review: bool,
    /// When `--bench-review` opened the review screen (for its wall-clock cap).
    bench_review_at: Option<Instant>,
    /// `--bench-review`: the unified frames are done, the split view is being measured.
    bench_split: bool,
    settings: Option<SettingsState>,
    /// `Git::locate()` result from the startup worker; unset until it finishes.
    git_ok: Arc<OnceLock<bool>>,
    /// The same worker's `git --version` line, for the welcome screen.
    git_version: GitVersion,
    /// PR list (scroll offset, viewport height) of the last frame, and a scroll offset to
    /// force once (keyboard navigation).
    list_view: (f32, f32),
    list_scroll: Option<f32>,
    /// Why a double-clicked PR could not be opened (status line), until the next click.
    review_hint: Option<&'static str>,
    /// The repository picker while `Screen::Repos` shows it (made on its first frame).
    picker: Option<RepoPicker>,
    /// Where the picker's Back returns to.
    picker_from: Screen,
    /// The notice about repositories the upgrade selected was dismissed this session.
    notice_dismissed: bool,
    /// The inbox's forge/account filter, its chips (none with one account) and every
    /// account's label (refreshed when accounts change).
    scope: Scope,
    chips: Vec<(Scope, String)>,
    account_names: Vec<(AccountId, String)>,
    /// Accounts paused by a rate limit: (forge, account, label, until), refreshed after sync
    /// events.
    paused: Vec<(Forge, AccountId, String, std::time::SystemTime)>,
    /// Each account's last "requested elsewhere" answer (unselected repositories, PRs).
    elsewhere: Vec<(AccountId, Vec<(RepoRef, usize)>)>,
    /// The hint text that was dismissed; a different hint shows again.
    elsewhere_dismissed: Option<String>,
}

mod accounts;
mod demo;
mod detail;
mod inbox;
mod notices;
mod screens;
mod session;
pub use notices::*;
mod review_button;
pub use review_button::*;

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let started = Instant::now();
        let ctx = root.ctx().clone();
        self.frame += 1;
        self.drain_backend(&ctx);
        self.script(&ctx);
        match self.screen {
            Screen::Login => self.login_screen(root),
            Screen::Inbox => self.inbox_screen(root),
            Screen::Settings => self.settings_screen(root),
            Screen::Repos => self.repos_screen(root),
            Screen::Review => {
                let back = crate::review::ui::show(&mut self.review, root, self.backend.as_ref());
                if let Some((repo, id, reviewers)) = self.review.take_reviewers_update() {
                    self.apply_reviewers(&repo, id, reviewers);
                }
                if back {
                    self.screen = Screen::Inbox;
                }
            }
        }
        if let Some(times) = &mut self.bench {
            // the review bench measures the settled screen: diff shown and highlighted
            let ready = !self.bench_review || self.review.ready_highlighted();
            if self.frame > 5 && ready {
                times.push(started.elapsed().as_secs_f32() * 1000.0);
            }
        }
    }
}

#[cfg(test)]
mod tests;
