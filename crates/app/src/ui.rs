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

/// Time left before the next throttled refresh; never underflows.
pub fn repaint_delay(elapsed: Duration) -> Duration {
    REFRESH_EVERY.saturating_sub(elapsed)
}

/// Enter opens the selected PR's review only when no widget had focus as the frame began
/// and none lost it this frame (Enter in the search box ends the edit instead).
pub fn enter_opens_review(enter: bool, focused_at_start: bool, lost_focus: bool) -> bool {
    enter && !focused_at_start && !lost_focus
}

/// State of the detail pane's Review button; None when no PR is selected.
pub fn detail_review_button(app: &App) -> Option<ReviewButton> {
    let d = app.detail.as_ref()?;
    // demo mode needs no credentials; real mode once credentials are stored
    let has_creds = app.backend.is_none() || app.login.has_saved;
    let git_ok = app.git_ok.get().copied().unwrap_or(false)
        || app.settings.as_ref().is_some_and(|s| s.git_ok());
    Some(review_button_state(
        // fork PRs are reviewed on GitHub and GitLab (D2), not on Bitbucket
        d.pr.is_fork() && d.pr.forge == Forge::BitbucketCloud,
        d.pr.state.clone(),
        has_creds,
        git_ok,
    ))
}

/// Draws the Review button (nothing when hidden); returns true when it was clicked.
fn review_button_ui(ui: &mut egui::Ui, state: Option<ReviewButton>, forge: Forge) -> bool {
    let reason = match state {
        None | Some(ReviewButton::Hidden) => return false,
        Some(ReviewButton::Enabled) => None,
        Some(ReviewButton::Disabled(why)) => Some(why),
    };
    ui.add_space(6.0);
    let text = format!("Review this {}: files, diff, comments", forge.change_noun());
    let button = ui.add_enabled(
        reason.is_none(),
        egui::Button::new(RichText::new(text).strong())
            .fill(Color32::from_rgb(40, 90, 150))
            .min_size(egui::vec2(ui.available_width(), 34.0)),
    );
    match reason {
        Some(why) => {
            button.on_disabled_hover_text(why);
            ui.label(RichText::new(why).small().color(Color32::GRAY));
            false
        }
        None => button.on_hover_text("Open the diff (Enter)").clicked(),
    }
}

/// `git --version` output when git can be used (None otherwise), set by `spawn_git_check`.
type GitVersion = Arc<OnceLock<Option<String>>>;

/// Locates git once on a worker thread; the result is read by the Review button and the
/// version line by the welcome screen's footer.
fn spawn_git_check(ctx: Option<egui::Context>) -> (Arc<OnceLock<bool>>, GitVersion) {
    let slot = Arc::new(OnceLock::new());
    let version: GitVersion = Arc::new(OnceLock::new());
    let (s, v) = (slot.clone(), version.clone());
    let _ = std::thread::Builder::new()
        .name("git-check".into())
        .spawn(move || {
            // locates the process-wide git once; reviews and Settings reuse it
            let git = gitlocal::Git::shared().ok();
            let line = git.as_ref().map(|g| {
                g.run(std::path::Path::new("."), &["--version"], None)
                    .map(|out| String::from_utf8_lossy(&out).trim().to_string())
                    .unwrap_or_default()
            });
            let _ = v.set(line);
            let _ = s.set(git.is_some());
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
    (slot, version)
}

const BENCH_FRAMES: usize = 300;
/// Changed files in the `--bench-review` demo PR.
pub const BENCH_REVIEW_FILES: usize = 1000;
/// `--bench-review` gives up (error, exit 1) when the review screen is not ready by then.
const BENCH_REVIEW_MAX_FRAMES: usize = 3000;
const BENCH_REVIEW_MAX_WAIT: Duration = Duration::from_secs(120);

pub const AUTH_REJECTED: &str = "Email or token rejected. Re-enter them or sign out.";
pub const ACCESS_DENIED_HINT: &str = "Access denied on all repos: the token may be missing scopes";
const DENIED: &str = "access denied";

/// True when a finished sync could read nothing because of 403s: every repo failed with
/// "access denied" or, with no repos found at all, every workspace listing did.
pub fn access_denied_everywhere(total: usize, denied: usize, errors: &[(RepoRef, String)]) -> bool {
    if total > 0 {
        return denied >= total;
    }
    let mut listings = errors.iter().filter(|(r, _)| r.slug == "*").peekable();
    listings.peek().is_some() && listings.all(|(_, e)| e == DENIED)
}

/// Short commit hash for the debug line; empty means the API did not report one.
fn short_hash(h: &str) -> &str {
    if h.is_empty() {
        "unknown"
    } else {
        h.get(..7).unwrap_or(h)
    }
}

/// Debug details for a stack member, so the owner can compare raw hashes with the advice.
pub fn stack_debug_line(pr: &PullRequest, parent: Option<&PullRequest>) -> String {
    match parent {
        Some(p) => format!(
            "debug: destination.commit {} | parent #{} source.commit {}",
            short_hash(&pr.dest_commit),
            p.id,
            short_hash(&p.source_commit)
        ),
        None => format!(
            "debug: source.commit {} | destination.commit {}",
            short_hash(&pr.source_commit),
            short_hash(&pr.dest_commit)
        ),
    }
}

/// Plain-language sentence for one piece of stack advice.
pub fn advice_text(advice: &Advice) -> String {
    match advice {
        Advice::ReviewBottomUp => "Review this stack from the bottom up: start with the first PR \
            and work toward the last, because each PR builds on the one before it."
            .into(),
        Advice::ParentChanged { parent, child } => format!(
            "#{parent} changed after #{child} was opened on top of it, so the diff and comments \
             of #{child} may be stale. Ask the author to rebase #{child}, then re-review it."
        ),
        Advice::ParentMergedRetarget { parent, child } => format!(
            "#{parent} is already merged but #{child} still targets its branch. Ask the author to \
             retarget #{child} to the new base before you review it."
        ),
        Advice::HoldChildApproval { parent, child } => format!(
            "Hold off approving #{child}: #{parent} has requested changes or open tasks, and \
             #{child} may change with it."
        ),
        Advice::MergeOrderViolation { parent, child } => format!(
            "#{child} is approved but #{parent} is not. #{parent} must be merged first, so do not \
             merge #{child} yet."
        ),
        Advice::ReviewablePerPr => "Together these PRs are over Bitbucket's diff limits, but each \
            one fits. Review them one PR at a time in Bitbucket."
            .into(),
    }
}

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

impl App {
    fn blank(screen: Screen) -> App {
        App {
            backend: None,
            screen,
            review: ReviewHost::default(),
            login: Login::default(),
            me: UserRef {
                id: String::new(),
                display_name: String::new(),
                login: None,
            },
            data: Snapshot::default(),
            view: View::NeedsMe,
            query: String::new(),
            inbox: Inbox {
                rows: vec![],
                stacks: vec![],
            },
            items: vec![],
            dirty: true,
            selected: None,
            detail: None,
            sync: SyncStatus::default(),
            status_label: None,
            show_errors: false,
            refresh_pending: false,
            last_refresh: Instant::now(),
            shot: None,
            shot_taken: false,
            shot_timed_out: false,
            frame: 0,
            bench: None,
            bench_review: false,
            bench_review_at: None,
            bench_split: false,
            settings: None,
            git_ok: Arc::new(OnceLock::new()),
            git_version: Arc::new(OnceLock::new()),
            list_view: (0.0, 0.0),
            list_scroll: None,
            review_hint: None,
            picker: None,
            picker_from: Screen::Inbox,
            notice_dismissed: false,
            scope: Scope::All,
            chips: Vec::new(),
            account_names: Vec::new(),
            paused: Vec::new(),
            elsewhere: Vec::new(),
            elsewhere_dismissed: None,
        }
    }

    /// The "requested elsewhere" hint over every account's last answer, unless dismissed.
    fn elsewhere_text(&self) -> Option<String> {
        let mut all: std::collections::BTreeMap<String, (RepoRef, usize)> = Default::default();
        for (r, n) in self.elsewhere.iter().flat_map(|(_, v)| v) {
            let e = all.entry(r.key()).or_insert_with(|| (r.clone(), 0));
            // the same PR seen through two accounts counts once
            e.1 = e.1.max(*n);
        }
        let repos: Vec<(RepoRef, usize)> = all.into_values().collect();
        crate::backend::elsewhere_hint(&repos)
            .filter(|t| self.elsewhere_dismissed.as_ref() != Some(t))
    }

    /// Demo bench of the review screen instead of the inbox; `preload_review_fixture` gives
    /// it the `BENCH_REVIEW_FILES` demo PR.
    pub fn demo_bench_review() -> App {
        let mut app = App::demo(None, true);
        app.bench_review = true;
        app
    }

    /// Demo: the review fixture, built before the window opened.
    pub fn preload_review_fixture(&mut self, fixture: Arc<crate::demo::DemoReview>) {
        self.review.preload_demo(fixture);
    }

    /// No network, no secrets, no store: fixed demo data in memory.
    pub fn demo(shot: Option<Shot>, bench: bool) -> App {
        let (me, prs, stats) = crate::demo::demo_data();
        let mut app = App::blank(Screen::Inbox);
        let repo_count = prs
            .iter()
            .map(|p| &p.repo)
            .collect::<std::collections::HashSet<_>>()
            .len();
        app.status_label = Some(format!(
            "Demo data, no network: {} PRs in {repo_count} repos",
            prs.len()
        ));
        app.data = Snapshot {
            prs,
            stats,
            errors: vec![
                (
                    RepoRef::parse_bitbucket("acme/payments").unwrap(),
                    "access denied".into(),
                ),
                (
                    RepoRef::parse_bitbucket("acme/reports").unwrap(),
                    "rate limited".into(),
                ),
            ],
            repo_count,
            known_repos: crate::demo::demo_repo_choices()
                .iter()
                .filter(|c| !c.gone)
                .count(),
            preselected: None,
            account: None,
            account_users: crate::demo::demo_account_users(),
        };
        app.me = me;
        app.view = View::All;
        app.review.demo_mode = true;
        app.shot = shot;
        app.refresh_accounts();
        if bench {
            app.bench = Some(Vec::new());
        }
        (app.git_ok, app.git_version) = spawn_git_check(None);
        app
    }

    /// Real mode: connects automatically when credentials are already stored.
    pub fn real(ctx: egui::Context, backend: Result<Backend, String>) -> App {
        let mut app = App::blank(Screen::Login);
        match backend {
            Ok(b) => {
                b.request_snapshot();
                if b.with_store(|s| s.wal_unavailable()) {
                    app.sync.note = Some(store::StoreError::WalUnavailable.to_string());
                }
                if let Some(me) = b.stored_me() {
                    app.me = me;
                }
                let vim = b.with_store(|s| s.get_meta(VIM_META_KEY).ok().flatten());
                app.review.vim_off = !vim_keys_on(vim.as_deref());
                app.login.has_saved = b.has_stored_credentials();
                // Pre-filled so a later 401 returns to a login screen that keeps the email.
                app.login.email = b.stored_email().unwrap_or_default();
                if app.login.has_saved {
                    app.screen = Screen::Inbox;
                    app.sync.connecting = true;
                    b.connect_stored();
                }
                app.backend = Some(b);
                app.refresh_accounts();
            }
            Err(e) => app.login.error = Some(e),
        }
        (app.git_ok, app.git_version) = spawn_git_check(Some(ctx));
        app
    }

    /// A review action of mine went through: the inbox row and the cache show the new
    /// reviewers before the next sync.
    fn apply_reviewers(&mut self, repo: &RepoRef, id: u64, reviewers: Vec<Reviewer>) {
        if let Some(p) = self
            .data
            .prs
            .iter_mut()
            .find(|p| p.repo == *repo && p.id == id)
        {
            p.reviewers = reviewers.clone();
            self.dirty = true;
        }
        if let Some(b) = &self.backend {
            let repo = repo.clone();
            b.writer()
                .write("Could not save the review state", move |s| {
                    s.set_pr_reviewers(&repo, id, &reviewers).map(|_| ())
                });
        }
    }

    fn drain_backend(&mut self, ctx: &egui::Context) {
        let Some(b) = self.backend.as_mut() else {
            return;
        };
        let mut msgs = Vec::new();
        let mut newest = None;
        while let Ok(m) = b.rx.try_recv() {
            match m {
                // only the newest snapshot is applied; older ones are dropped
                Msg::Snapshot(s) => newest = Some(Msg::Snapshot(s)),
                m => msgs.push(m),
            }
        }
        msgs.extend(newest);
        let mut refresh_now = false;
        for m in msgs {
            refresh_now |= self.apply_msg(m);
        }
        let Some(b) = self.backend.as_ref() else {
            return;
        };
        if self.refresh_pending {
            let due = self.last_refresh.elapsed() >= REFRESH_EVERY;
            if refresh_now || due {
                // built on the backend's runtime; arrives as `Msg::Snapshot`
                b.request_snapshot();
                self.last_refresh = Instant::now();
                self.refresh_pending = false;
                self.dirty = true;
            } else {
                ctx.request_repaint_after(repaint_delay(self.last_refresh.elapsed()));
            }
        }
    }

    /// State transition for one backend message; returns true when the snapshot must be
    /// re-read now. Touches no credentials: a 401 keeps the stored email and token.
    fn apply_msg(&mut self, m: Msg) -> bool {
        match m {
            Msg::Connected(me) => {
                if me.id != self.me.id {
                    // another account: nothing of the previous one stays on screen
                    self.data = Snapshot::default();
                    self.selected = None;
                    self.detail = None;
                }
                self.me = me;
                self.sync.connecting = false;
                // The sync starts right after `me()`; stay busy until it finishes.
                self.sync.running = true;
                self.login.busy = false;
                // connecting always uses the stored credentials
                self.login.has_saved = true;
                // a reconnect (Refresh, a saved repository choice) leaves other screens open
                if self.screen == Screen::Login {
                    self.screen = Screen::Inbox;
                }
                self.refresh_paused();
                self.dirty = true;
            }
            Msg::Discovery { first, result } => {
                // the first sign-in found repositories, none chosen: the picker comes first
                let pick = first
                    && matches!(&result, Ok(d) if d.selected == 0)
                    && self.screen == Screen::Inbox;
                // Settings' "Refresh list" ends here too
                if let Some(s) = self.settings.as_mut() {
                    s.discovering = false;
                    s.repos_error = result.as_ref().err().cloned();
                }
                match (self.picker.as_mut(), result) {
                    (Some(p), Ok(_)) => {
                        p.discovering = false;
                        if let Some(b) = &self.backend {
                            b.request_repo_choices();
                        }
                    }
                    (Some(p), Err(e)) => {
                        p.discovering = false;
                        p.error = Some(e);
                    }
                    (None, Err(e)) => self.sync.note = Some(e),
                    (None, Ok(_)) => {}
                }
                if pick {
                    self.open_picker(Screen::Inbox);
                }
                self.refresh_pending = true;
            }
            Msg::RepoChoices(r) => {
                if let Some(p) = self.picker.as_mut() {
                    match r {
                        Ok(c) => p.load(c),
                        Err(e) => p.error = Some(e),
                    }
                }
            }
            Msg::SelectionSaved(Ok(())) => {
                self.picker = None;
                self.settings = None;
                self.screen = Screen::Inbox;
                self.refresh_pending = true;
                // a sync of just the chosen repositories (one in flight is replaced)
                self.resync();
                return true;
            }
            Msg::AccountAdded(r) => {
                self.refresh_accounts();
                let others = self.other_accounts();
                if let Some(st) = self.settings.as_mut() {
                    st.add.busy = false;
                    if r.is_ok() {
                        st.add.open = false;
                        st.add.email.clear();
                    }
                    st.add.result = Some(r.map(|name| format!("Added {name}")));
                    st.others = others;
                }
            }
            Msg::ConnectionTested(r) => {
                if let Some(st) = self.settings.as_mut() {
                    st.add.busy = false;
                    st.add.tested = Some(r);
                }
            }
            Msg::AccountRemoved(r) => {
                self.refresh_accounts();
                let known: Vec<AccountId> =
                    self.account_names.iter().map(|(a, _)| a.clone()).collect();
                self.elsewhere.retain(|(a, _)| known.contains(a));
                let others = self.other_accounts();
                if let Some(st) = self.settings.as_mut() {
                    st.add.result = Some(r);
                    st.others = others;
                }
                self.refresh_pending = true;
                return true;
            }
            Msg::RequestedElsewhere { account, repos } => {
                self.elsewhere.retain(|(a, _)| *a != account);
                self.elsewhere.push((account, repos));
            }
            Msg::AccountUpdated(r) => {
                self.refresh_accounts();
                let others = self.other_accounts();
                if let Some(st) = self.settings.as_mut() {
                    st.add.result = Some(r);
                    st.others = others;
                }
                self.refresh_pending = true;
                return true;
            }
            Msg::SelectionSaved(Err(e)) => {
                if let Some(p) = self.picker.as_mut() {
                    p.saving = false;
                    p.error = Some(e);
                }
            }
            Msg::AuthRejected => {
                self.login.token.clear();
                self.login.error = Some(AUTH_REJECTED.into());
                self.login.has_saved = true;
                self.login.busy = false;
                self.sync = SyncStatus::default();
                self.screen = Screen::Login;
            }
            Msg::Failed(e) => {
                self.sync.connecting = false;
                self.login.busy = false;
                self.login.has_saved = self
                    .backend
                    .as_ref()
                    .is_some_and(|b| b.has_stored_credentials());
                if self.screen == Screen::Inbox {
                    self.sync.note = Some(e);
                } else {
                    self.login.error = Some(e);
                }
            }
            Msg::SyncFailed(e) => {
                self.sync.running = false;
                self.sync.note = Some(e);
                self.refresh_paused();
            }
            // a snapshot of another account's cache (built before a sign-in switched it, or
            // read after a sign-out) is never shown
            Msg::Snapshot(Ok(s)) if s.account.as_ref().is_some_and(|a| *a != self.me.id) => {}
            Msg::Snapshot(Ok(s)) => {
                self.data = s;
                self.dirty = true;
                if self.sync.note.as_deref() == Some(CACHE_READ_FAILED) {
                    self.sync.note = None;
                }
            }
            // the last good data stays on screen, with the reason it is not refreshed
            Msg::Snapshot(Err(e)) => self.sync.note = Some(e),
            Msg::Sync(ev) => {
                self.refresh_pending = true;
                match ev {
                    SyncEvent::Started => {
                        self.sync = SyncStatus {
                            running: true,
                            ..SyncStatus::default()
                        }
                    }
                    SyncEvent::Discovered { repos } => self.sync.total = repos,
                    SyncEvent::RepoDone { .. } => self.sync.done += 1,
                    SyncEvent::RepoFailed { error, .. } => {
                        self.sync.done += 1;
                        if error == DENIED {
                            self.sync.denied += 1;
                        }
                        // a rate limit pauses its account's client
                        if self.paused.is_empty() && error.contains("rate") {
                            self.refresh_paused();
                        }
                    }
                    SyncEvent::DiffstatDone { .. } => self.sync.diffstats += 1,
                    SyncEvent::Finished => {
                        self.sync.running = false;
                        self.sync.finished = true;
                        self.refresh_paused();
                        return true;
                    }
                }
            }
        }
        false
    }

    /// A connect or sync is in flight; Refresh, Retry and Connect must not start another.
    fn sync_busy(&self) -> bool {
        self.login.busy || self.sync.connecting || self.sync.running
    }

    /// Marks a reconnect as started; false (and no change) when one is already running.
    fn begin_sync(&mut self) -> bool {
        if self.sync_busy() {
            return false;
        }
        self.login.error = None;
        self.sync.note = None;
        if self.screen == Screen::Login {
            self.login.busy = true;
        } else {
            self.sync.connecting = true;
        }
        true
    }

    /// Back to an empty login screen: no credentials, no user, no data in memory.
    fn clear_session(&mut self) {
        self.screen = Screen::Login;
        self.login = Login::default();
        self.me = UserRef {
            id: String::new(),
            display_name: String::new(),
            login: None,
        };
        self.data = Snapshot::default();
        self.inbox = Inbox {
            rows: vec![],
            stacks: vec![],
        };
        self.items.clear();
        self.selected = None;
        self.detail = None;
        self.sync = SyncStatus::default();
        self.status_label = None;
        self.show_errors = false;
        self.refresh_pending = false;
        self.picker = None;
        self.notice_dismissed = false;
        self.elsewhere.clear();
        self.elsewhere_dismissed = None;
        self.dirty = true;
    }

    /// Deletes the stored email, token and user, then returns to the login screen.
    fn sign_out(&mut self) {
        let result = match self.backend.as_mut() {
            Some(b) => b.sign_out(),
            None => Ok(()),
        };
        self.clear_session();
        self.login.error = result.err();
    }

    /// Selects a PR of the list; its detail pane is rebuilt at once.
    fn select_pr(&mut self, sel: (RepoRef, u64)) {
        if self.selected.as_ref() != Some(&sel) {
            self.selected = Some(sel);
            self.dirty = true;
            self.recompute();
        }
    }

    /// Rebuilds rows and the detail pane. Runs only after data, view, query or selection changed.
    fn recompute(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        self.chips = scope_chips(&self.data.prs, &self.account_names);
        if !self.chips.iter().any(|(s, _)| *s == self.scope) {
            self.scope = Scope::All;
        }
        let scoped: Vec<PullRequest>;
        let prs: &[PullRequest] = if self.scope == Scope::All {
            &self.data.prs
        } else {
            scoped = self
                .data
                .prs
                .iter()
                .filter(|p| self.scope.matches(p))
                .cloned()
                .collect();
            &scoped
        };
        self.inbox = build_inbox_for(
            prs,
            &self.data.stats,
            &self.me,
            &self.data.account_users,
            effective_view(self.me_known(), self.view),
            &self.query,
        );
        self.items = build_items(&self.inbox);
        self.detail = self
            .selected
            .as_ref()
            .and_then(|(repo, id)| self.make_detail(repo, *id));
    }

    /// Who I am, the stack PR `id` belongs to and its repository's PRs, for the review
    /// screen's actions and warnings.
    pub(crate) fn review_context(
        &self,
        repo: &RepoRef,
        id: u64,
    ) -> crate::review::actions_ui::ReviewContext {
        let stack = self
            .inbox
            .stacks
            .iter()
            .find(|s| s.repo == *repo && s.members.contains(&id))
            .cloned();
        let prs = self
            .data
            .prs
            .iter()
            .filter(|p| p.repo == *repo)
            .map(|p| (p.id, p.clone()))
            .collect();
        crate::review::actions_ui::ReviewContext {
            me: self.me_known().then(|| self.me.clone()),
            stack,
            prs,
        }
    }

    fn make_detail(&self, repo: &RepoRef, id: u64) -> Option<Detail> {
        let pr: &PullRequest = self
            .data
            .prs
            .iter()
            .find(|p| p.repo == *repo && p.id == id)?;
        let slot = self
            .inbox
            .stacks
            .iter()
            .position(|s| s.repo == *repo && s.members.contains(&id));
        let (stack, advice, debug) = match slot {
            Some(si) => {
                let s = &self.inbox.stacks[si];
                let pos = s.members.iter().position(|&m| m == id).unwrap_or(0) + 1;
                let texts = stack_advice(&self.inbox, &self.data.prs, &self.data.stats, si)
                    .iter()
                    .map(advice_text)
                    .collect();
                let parent = s.edges.iter().find(|&&(_, c)| c == id).and_then(|&(p, _)| {
                    self.data.prs.iter().find(|x| x.repo == *repo && x.id == p)
                });
                let debug = stack_debug_line(pr, parent);
                (Some((pos, s.members.len())), texts, Some(debug))
            }
            None => (None, Vec::new(), None),
        };
        Some(Detail {
            pr: pr.clone(),
            stat: self.data.stats.get(&(repo.clone(), id)).cloned(),
            stack,
            advice,
            debug,
        })
    }

    fn script(&mut self, ctx: &egui::Context) {
        if self.shot.is_none() && self.bench.is_none() {
            return;
        }
        ctx.request_repaint();
        if let Some(times) = &self.bench {
            if times.len() >= BENCH_FRAMES {
                let avg = times.iter().sum::<f32>() / times.len() as f32;
                let max = times.iter().cloned().fold(0.0, f32::max);
                let label = if self.bench_split { "split view: " } else { "" };
                println!(
                    "{label}ui() over {} frames: avg {avg:.3} ms, max {max:.3} ms",
                    times.len()
                );
                if self.bench_review && !self.bench_split {
                    // the same screen again in the split view
                    self.bench_split = true;
                    self.review.bench_split();
                    self.bench = Some(Vec::new());
                    return;
                }
                if let Some(ms) = self.review.highlight_ms() {
                    println!("cold highlight of the first file: {ms:.1} ms");
                }
                if let Some(t) = self.review.timings() {
                    let ms = |v: Option<f32>| v.map_or("-".into(), |v| format!("{v:.0} ms"));
                    println!(
                        "since the review opened: file list {}, line counts {}, first colours {}",
                        ms(t.file_list),
                        ms(t.counts),
                        ms(t.first_colour)
                    );
                }
                std::process::exit(0);
            }
        }
        if self.bench_review && self.frame == 3 {
            let pr = self.data.prs[0].clone();
            open_review(self, pr.repo.clone(), pr);
            // select a modified (highlightable) file, as the screenshot does
            self.review.shot_scene = true;
            self.bench_review_at = Some(Instant::now());
        }
        if let (true, Some(at)) = (self.bench_review, self.bench_review_at) {
            if self.frame > BENCH_REVIEW_MAX_FRAMES || at.elapsed() > BENCH_REVIEW_MAX_WAIT {
                eprintln!(
                    "bench-review: the review screen was not ready after {} frames / {:.0} s",
                    self.frame,
                    at.elapsed().as_secs_f32()
                );
                std::process::exit(1);
            }
        }
        let Some(shot) = &self.shot else { return };
        if self.frame == 3 {
            self.screen = shot.screen;
            self.view = shot.view;
            // by full name on any forge (the GitHub demo repo is on github.com)
            self.selected = shot.select.map(|(r, id)| {
                let found = self
                    .data
                    .prs
                    .iter()
                    .find(|p| p.repo.full_name() == r && p.id == id);
                match found {
                    Some(p) => (p.repo.clone(), id),
                    None => (RepoRef::parse_bitbucket(r).unwrap(), id),
                }
            });
            self.dirty = true;
            let scene = shot.scene;
            self.demo_scene(scene);
            if self.screen == Screen::Review {
                // the GitHub scene opens a GitHub PR: the review is read-only
                let pr = match scene {
                    Scene::ReviewGitHub => self.data.prs.iter().find(|p| p.forge == Forge::GitHub),
                    Scene::ReviewGitLab => self.data.prs.iter().find(|p| p.forge == Forge::GitLab),
                    _ => None,
                }
                .unwrap_or(&self.data.prs[0])
                .clone();
                open_review(self, pr.repo.clone(), pr);
                self.review.shot_scene = true;
                self.review.shot_comments = matches!(
                    scene,
                    Scene::ReviewComments
                        | Scene::ReviewDrafts
                        | Scene::ReviewActions
                        | Scene::ReviewTree
                        | Scene::ReviewConversation
                        | Scene::ReviewConversationEnd
                        | Scene::ReviewOutdated
                        | Scene::ReviewSplit
                        | Scene::ReviewGitLab
                );
                self.review.shot_split = scene == Scene::ReviewSplit;
                self.review.shot_help = scene == Scene::ReviewHelp;
                self.review.shot_tree = scene == Scene::ReviewTree;
                self.review.shot_outdated = scene == Scene::ReviewOutdated;
                self.review.shot_conversation = matches!(
                    scene,
                    Scene::ReviewConversation | Scene::ReviewConversationEnd
                );
                self.review.shot_conversation_end = scene == Scene::ReviewConversationEnd;
                self.review.shot_actions = scene == Scene::ReviewActions;
                self.review.shot_drafts = scene == Scene::ReviewDrafts;
                self.review.shot_selection = scene == Scene::ReviewSelection;
            }
        }
        let Some(shot) = &self.shot else { return };
        // the review screen loads in the background: wait for it, at most until frame 600
        let settled = match self.screen {
            Screen::Review => self.review.ready_highlighted(),
            // the welcome footer shows the git check's result
            Screen::Login => self.git_version.get().is_some(),
            _ => true,
        };
        if !self.shot_taken && self.frame >= 12 && (settled || self.frame >= 600) {
            self.shot_taken = true;
            self.shot_timed_out = !settled;
            let path = shot.path.clone();
            let done = shot.done.clone();
            ctx.request_screenshot(move |image| {
                let img = image::RgbaImage::from_raw(
                    image.width() as u32,
                    image.height() as u32,
                    image.pixels.iter().flat_map(|c| c.to_array()).collect(),
                )
                .expect("screenshot buffer size");
                if let Some(done) = done {
                    let saved = img.save(&path).map_err(|e| e.to_string());
                    *lock_ok(&done) = Some(saved);
                    return;
                }
                img.save(&path).expect("write screenshot");
                println!("saved {path}");
                std::process::exit(0);
            });
        }
    }

    /// Puts demo data into the state a screenshot scene needs (no network involved).
    fn demo_scene(&mut self, scene: Scene) {
        match scene {
            Scene::Normal
            | Scene::ReviewComments
            | Scene::ReviewDrafts
            | Scene::ReviewSelection
            | Scene::ReviewActions
            | Scene::ReviewTree
            | Scene::ReviewConversation
            | Scene::ReviewConversationEnd
            | Scene::ReviewOutdated
            | Scene::ReviewSplit
            | Scene::ReviewHelp
            | Scene::SettingsProblems
            | Scene::ReviewGitHub
            | Scene::ReviewGitLab
            | Scene::AddGitLab => {}
            Scene::Accounts => {
                // the GitHub account's search found review requests outside the selection
                let r = |n: &str| RepoRef::parse("github.com", n).unwrap();
                self.apply_msg(Msg::RequestedElsewhere {
                    account: AccountId(crate::demo::DEMO_GITHUB_ACCOUNT.into()),
                    repos: vec![(r("acme-oss/docs-site"), 2), (r("acme-oss/sdk-js"), 1)],
                });
            }
            Scene::Rejected => {
                self.login.email = "you@example.com".into();
                self.apply_msg(Msg::AuthRejected);
            }
            Scene::Denied => {
                let mut repos: Vec<RepoRef> =
                    self.data.prs.iter().map(|p| p.repo.clone()).collect();
                repos.sort_by_key(|r| r.full_name());
                repos.dedup();
                self.data.errors = repos.iter().map(|r| (r.clone(), DENIED.into())).collect();
                self.apply_msg(Msg::Sync(SyncEvent::Started));
                self.apply_msg(Msg::Sync(SyncEvent::Discovered { repos: repos.len() }));
                for repo in repos {
                    self.apply_msg(Msg::Sync(SyncEvent::RepoFailed {
                        repo,
                        error: DENIED.into(),
                    }));
                }
                self.apply_msg(Msg::Sync(SyncEvent::Finished));
            }
        }
    }

    /// The welcome / sign-in screen (`welcome.rs` draws it; the actions are applied here).
    fn login_screen(&mut self, root: &mut egui::Ui) {
        let form = welcome::Form {
            email: &mut self.login.email,
            token: &mut self.login.token,
            error: self.login.error.as_deref(),
            busy: self.login.busy,
            has_saved: self.login.has_saved,
            git: self.git_version.get().map(Option::as_deref),
        };
        match welcome::show(root, form) {
            Some(welcome::Action::Connect) => self.connect(),
            Some(welcome::Action::Retry) => self.retry(),
            Some(welcome::Action::SignOut) => self.sign_out(),
            Some(welcome::Action::OpenTokenPage) => {
                if let Err(e) = crate::review::actions_ui::open_url(welcome::TOKEN_PAGE) {
                    self.login.error = Some(e);
                }
            }
            None => {}
        }
    }

    fn me_known(&self) -> bool {
        !self.me.id.is_empty()
    }

    /// Reconnects and re-syncs with the stored credentials (Retry and Refresh); never twice.
    fn retry(&mut self) {
        if self.backend.is_some() && self.begin_sync() {
            if let Some(b) = &self.backend {
                b.connect_stored();
            }
        }
    }

    fn connect(&mut self) {
        let Some(b) = &self.backend else {
            self.login.error = Some("Connecting is not available in this mode".into());
            return;
        };
        let email = welcome::clean_email(&self.login.email);
        match b.save_and_connect(email, welcome::clean_token(&self.login.token)) {
            Ok(()) => {
                // The token now lives only in the credential store; drop our copy.
                self.login.token = String::new();
                self.login.error = None;
                self.login.busy = true;
            }
            Err(e) => self.login.error = Some(e),
        }
    }

    /// Reconnects and syncs now, replacing a connect or sync in flight (after the repository
    /// choice changed, the old sync covers the wrong repositories).
    fn resync(&mut self) {
        let Some(b) = &self.backend else { return };
        self.login.error = None;
        self.sync.note = None;
        self.sync.connecting = true;
        b.connect_stored();
    }

    /// Shows "Choose repositories"; Back returns to `from`.
    /// Re-reads every account's label (from the store: only on startup and after an account
    /// change, never after a sync event), then the pauses.
    fn refresh_accounts(&mut self) {
        self.account_names = match &self.backend {
            Some(b) => b.account_labels(),
            None => crate::demo::demo_accounts()
                .iter()
                .map(|a| (a.id.clone(), crate::backend::default_label(a)))
                .collect(),
        };
        self.refresh_paused();
    }

    /// Which accounts are paused by a rate limit, from the providers only (no store read, so
    /// a sync holding the store never blocks the UI); also names the signed-in account by
    /// its user before its row knows one.
    fn refresh_paused(&mut self) {
        let default = AccountId::default_bitbucket();
        for (id, name) in &mut self.account_names {
            if *id == default
                && name.as_str() == default.as_str()
                && !self.me.display_name.is_empty()
            {
                *name = self.me.display_name.clone();
            }
        }
        let paused = match &self.backend {
            Some(b) => b.paused_status(),
            None if self.shot.as_ref().map(|s| s.scene) == Some(Scene::Accounts) => {
                let accounts = crate::demo::demo_accounts();
                crate::demo::demo_paused()
                    .into_iter()
                    .filter_map(|(id, t)| {
                        let a = accounts.iter().find(|a| a.id == id)?;
                        Some((a.forge, id, t))
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        self.paused = paused
            .into_iter()
            .map(|(forge, id, until)| {
                let label = self
                    .account_names
                    .iter()
                    .find(|(a, _)| *a == id)
                    .map_or_else(|| id.as_str().to_string(), |(_, l)| l.clone());
                (forge, id, label, until)
            })
            .collect();
        self.dirty = true;
    }

    /// The accounts besides the signed-in one (none in demo mode), by label, with their
    /// rate-limit pause.
    fn other_accounts(&self) -> Vec<settings_ui::OtherAccount> {
        let mut out = self.other_accounts_raw();
        for o in &mut out {
            if let Some((_, l)) = self.account_names.iter().find(|(id, _)| *id == o.id) {
                o.name = l.clone();
            }
            o.paused_until = self
                .paused
                .iter()
                .find(|(_, id, _, _)| *id == o.id)
                .map(|p| p.3);
        }
        out
    }

    fn other_accounts_raw(&self) -> Vec<settings_ui::OtherAccount> {
        match &self.backend {
            Some(b) => settings_ui::other_accounts_with(&b.accounts(), |id| {
                b.with_store(|s| crate::backend::token_expiry(s, id))
            }),
            None if matches!(
                self.shot.as_ref().map(|s| s.scene),
                Some(Scene::Accounts | Scene::AddGitLab)
            ) =>
            {
                settings_ui::other_accounts_with(
                    &crate::demo::demo_accounts(),
                    crate::demo::demo_token_expiry,
                )
            }
            None => Vec::new(),
        }
    }

    /// The picker's group name of every account: forge, host and user.
    fn account_labels(&self) -> Vec<(prcore::model::AccountId, String)> {
        let accounts = match &self.backend {
            Some(b) => b.accounts(),
            None => crate::demo::demo_accounts(),
        };
        accounts
            .into_iter()
            .map(|a| {
                let label = self.account_names.iter().find(|(id, _)| *id == a.id);
                let name = match label {
                    Some((_, l)) => l.clone(),
                    None if a.display_name.is_empty() => self.me.display_name.clone(),
                    None => a.display_name.clone(),
                };
                let label = format!(
                    "{name} \u{00b7} {} \u{00b7} {}",
                    a.forge.label(),
                    a.git_host
                );
                (a.id, label)
            })
            .collect()
    }

    pub(crate) fn open_picker(&mut self, from: Screen) {
        self.picker_from = from;
        self.picker = None;
        self.screen = Screen::Repos;
    }

    fn repos_screen(&mut self, root: &mut egui::Ui) {
        if self.picker.is_none() {
            let mut p = RepoPicker::loading();
            p.account_labels = self.account_labels();
            match &self.backend {
                Some(b) => b.request_repo_choices(),
                None if self.shot.as_ref().map(|s| s.scene) == Some(Scene::Accounts) => {
                    p.load(crate::demo::demo_repo_choices_two_accounts())
                }
                None => p.load(crate::demo::demo_repo_choices()),
            }
            self.picker = Some(p);
        }
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        match show_picker(root, p) {
            Some(PickerAction::Back) => {
                self.screen = self.picker_from;
                self.picker = None;
            }
            Some(PickerAction::Refresh) => match &self.backend {
                Some(b) => {
                    p.discovering = true;
                    p.error = None;
                    b.discover();
                }
                None => p.error = Some("Demo mode: the list is not refreshed".into()),
            },
            Some(PickerAction::Save { selected, purge }) => match &self.backend {
                Some(b) => b.save_selection(selected, purge),
                None => {
                    p.saving = false;
                    p.error = Some("Demo mode: the choice is not saved".into());
                }
            },
            None => {}
        }
    }

    fn status_line(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(label) = &self.status_label {
                ui.label(label);
            } else if self.sync.connecting {
                ui.spinner();
                ui.label("Connecting...");
            } else if self.sync.running {
                ui.spinner();
                ui.label(format!(
                    "Syncing: {}/{} repos, {} diffstats",
                    self.sync.done, self.sync.total, self.sync.diffstats
                ));
            } else {
                ui.label(format!(
                    "{} repos selected, {} PRs cached",
                    self.data.repo_count,
                    self.data.prs.len()
                ));
            }
            if let Some(n) = self.sync.note.clone() {
                ui.colored_label(RED, n);
                if !self.sync.connecting && ui.button("Retry").clicked() {
                    self.retry();
                }
            }
            if let Some(why) = self.review_hint {
                ui.colored_label(Color32::from_rgb(230, 170, 60), why);
            }
            let paused: Vec<_> = self
                .paused
                .iter()
                .map(|(f, _, l, t)| (*f, l.clone(), *t))
                .collect();
            if let Some(line) = crate::backend::paused_line(&paused) {
                ui.colored_label(AMBER, line);
            }
            if !self.me_known() && !self.sync.connecting && self.status_label.is_none() {
                ui.label(RichText::new("Offline: showing cached data, my PRs unknown").weak());
            }
            if self.sync.finished
                && access_denied_everywhere(self.sync.total, self.sync.denied, &self.data.errors)
            {
                ui.colored_label(RED, ACCESS_DENIED_HINT);
            }
            let errs = self.data.errors.len();
            if errs > 0 {
                let label = format!("{errs} repos with errors");
                if ui
                    .selectable_label(self.show_errors, RichText::new(label).color(RED))
                    .clicked()
                {
                    self.show_errors = !self.show_errors;
                }
            }
        });
    }

    fn settings_screen(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        if self.settings.is_none() {
            let state = match &self.backend {
                Some(b) => {
                    let default = default_download_dir();
                    let loaded = b
                        .with_store(|s| s.settings(&default))
                        .map_err(|e| e.to_string());
                    let mut st =
                        SettingsState::new(ctx, loaded.as_ref().map_err(Clone::clone), default);
                    let layout = b.with_store(|s| s.get_meta(LAYOUT_META_KEY).ok().flatten());
                    st.layout = DiffLayout::parse(layout.as_deref());
                    st.start();
                    st
                }
                None if self.shot.as_ref().map(|s| s.scene) == Some(Scene::SettingsProblems) => {
                    SettingsState::demo_problems(ctx)
                }
                None => SettingsState::demo(ctx),
            };
            let others = self.other_accounts();
            let scene = self.shot.as_ref().map(|s| s.scene);
            self.settings = Some(state);
            if let Some(s) = self.settings.as_mut() {
                s.vim_keys = !self.review.vim_off;
                s.others = others;
                if scene == Some(Scene::Accounts) {
                    // the form as a user sees it while filling it in (no token shown)
                    s.add.open = true;
                    s.add.email = "bo@example.com".into();
                }
                if scene == Some(Scene::AddGitLab) {
                    // a self-managed GitLab under a relative URL root, after Test connection
                    s.add.open = true;
                    s.add.forge = Forge::GitLab;
                    s.add.web_url = format!("https://{}", crate::demo::DEMO_GITLAB_HOST);
                    s.add.tested = Some(Ok("Signed in as alex; scopes [read_api, \
                        read_repository]: read-only; comments and approvals need the api \
                        scope; expires on \
                        2027-03-01; 14 projects visible"
                        .into()));
                }
            }
        }
        let account = settings_ui::Account {
            name: self.me.display_name.clone(),
            email: match &self.backend {
                Some(_) => welcome::clean_email(&self.login.email).to_string(),
                None => "you@example.com".into(),
            },
            conn: settings_ui::connection(
                self.backend.is_none(),
                self.sync.connecting,
                self.sync.running,
                (self.sync.done, self.sync.total),
                self.sync.note.as_deref(),
                self.me_known(),
            ),
        };
        let Some(state) = self.settings.as_mut() else {
            return;
        };
        state.repos = (self.data.repo_count, self.data.known_repos);
        state.account = account;
        // the removal confirmation says what goes (the store's own count)
        if let (None, Some(id)) = (&state.removal_counts, state.pending_removal().cloned()) {
            state.removal_counts = Some(match &self.backend {
                Some(b) => b
                    .with_store(|s| s.removal_counts(&id))
                    .map_err(|_| "Could not count the account's cached data".to_string()),
                None => Ok(store::RemovedCounts {
                    prs: 12,
                    drafts_kept: 2,
                    ..Default::default()
                }),
            });
        }
        let layout_before = state.layout;
        let action = show_settings(root, state);
        // the default diff layout applies at once, like "Vim keys" (demo mode: not saved)
        if state.layout != layout_before {
            let l = state.layout;
            if let Some(b) = &self.backend {
                b.writer()
                    .write("Could not save the diff layout", move |st| {
                        st.set_meta(LAYOUT_META_KEY, l.as_str())
                    });
            }
        }
        // "Vim keys" applies at once (demo mode: for this run only)
        if state.vim_keys == self.review.vim_off {
            let on = state.vim_keys;
            self.review.vim_off = !on;
            if let Some(b) = &self.backend {
                b.writer()
                    .write("Could not save the Vim keys setting", move |st| {
                        st.set_meta(VIM_META_KEY, vim_keys_value(on))
                    });
            }
        }
        match action {
            Some(SettingsAction::Back) => {
                self.screen = Screen::Inbox;
                self.settings = None;
            }
            Some(SettingsAction::ChooseRepos) => self.open_picker(Screen::Settings),
            Some(SettingsAction::RefreshRepos) => match &self.backend {
                Some(b) => b.discover(),
                None => {
                    state.discovering = false;
                    state.repos_error = Some("Demo mode: the list is not refreshed".into());
                }
            },
            Some(SettingsAction::SignOut) => {
                self.settings = None;
                self.sign_out();
            }
            Some(SettingsAction::AddAccount(new)) => match &self.backend {
                Some(b) => match new {
                    NewAccount::Bitbucket { email, token } => b.add_bitbucket_account(email, token),
                    NewAccount::GitHub { bases, token } => b.add_github_account(bases, token),
                    NewAccount::GitLab { bases, token } => b.add_gitlab_account(bases, token),
                },
                None => {
                    state.add.busy = false;
                    state.add.result = Some(Err("Demo mode: accounts are not added".into()));
                }
            },
            Some(SettingsAction::TestConnection(new)) => match (&self.backend, new) {
                (Some(b), NewAccount::GitHub { bases, token }) => {
                    b.test_github_connection(bases, token)
                }
                (Some(b), NewAccount::GitLab { bases, token }) => {
                    b.test_gitlab_connection(bases, token)
                }
                (Some(_), NewAccount::Bitbucket { .. }) => state.add.busy = false,
                (None, _) => {
                    state.add.busy = false;
                    state.add.tested = Some(Err("Demo mode: nothing is sent".into()));
                }
            },
            Some(SettingsAction::RemoveAccount(id)) => match &self.backend {
                Some(b) => b.remove_account(id),
                None => state.add.result = Some(Err("Demo mode: accounts are not removed".into())),
            },
            Some(SettingsAction::SetEnabled(id, on)) => match &self.backend {
                Some(b) => b.set_account_enabled(id, on),
                None => state.add.result = Some(Err("Demo mode: accounts are not changed".into())),
            },
            Some(SettingsAction::RenameAccount(id, label)) => match &self.backend {
                Some(b) => b.rename_account(id, label),
                None => state.add.result = Some(Err("Demo mode: accounts are not renamed".into())),
            },
            Some(SettingsAction::Persist(settings)) => match &self.backend {
                Some(b) => state.persist(&settings, b.writer()),
                None => state.message = Some(Err("Demo mode: settings are not saved".into())),
            },
            None => {}
        }
    }

    fn inbox_screen(&mut self, root: &mut egui::Ui) {
        self.recompute();
        let mut changed = false;
        // read before any widget runs: Enter in the search box must not open a review
        let focused_at_start = root.ctx().memory(|m| m.focused().is_some());
        let mut lost_focus = false;
        let (mut choose, mut dismiss) = (false, false);
        egui::Panel::top("bar").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                for (v, name) in [
                    (View::Mine, "Mine"),
                    (View::NeedsMe, "Needs me"),
                    (View::All, "All"),
                    (View::TooBig, "Too big"),
                    (View::Stacked, "Stacked"),
                ] {
                    if ui.selectable_value(&mut self.view, v, name).changed() {
                        changed = true;
                    }
                }
                ui.separator();
                let q = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .desired_width(300.0)
                        .hint_text("Search title, repo, branch, author"),
                );
                changed |= q.changed();
                lost_focus |= q.lost_focus();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Sign out")
                        .on_hover_text("Delete the saved email and token")
                        .clicked()
                    {
                        self.sign_out();
                    }
                    if ui
                        .button("Settings")
                        .on_hover_text("Repositories to sync, download and clone folders")
                        .clicked()
                    {
                        self.screen = Screen::Settings;
                    }
                    if ui
                        .add_enabled(!self.sync_busy(), egui::Button::new("Refresh"))
                        .on_hover_text("Sync again with Bitbucket")
                        .clicked()
                    {
                        self.retry();
                    }
                    if !self.me.display_name.is_empty() {
                        ui.label(RichText::new(&self.me.display_name).weak());
                    }
                });
            });
            if !self.chips.is_empty() {
                ui.horizontal(|ui| {
                    for (scope, name) in &self.chips {
                        let on = self.scope == *scope;
                        if ui
                            .selectable_label(on, name)
                            .on_hover_text("Show only this forge's or account's pull requests")
                            .clicked()
                            && !on
                        {
                            self.scope = scope.clone();
                            changed = true;
                        }
                    }
                });
            }
            self.status_line(ui);
            if let (Some(n), false) = (self.data.preselected, self.notice_dismissed) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(preselected_notice(n)).color(AMBER));
                    if ui.small_button("Choose repositories").clicked() {
                        choose = true;
                    }
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = true;
                    }
                });
            }
            // a hint only (decision D4): choosing repositories stays the user's move
            if let Some(hint) = self.elsewhere_text() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&hint).color(AMBER));
                    if ui.small_button("Choose repositories").clicked() {
                        choose = true;
                    }
                    if ui.small_button("Dismiss").clicked() {
                        self.elsewhere_dismissed = Some(hint.clone());
                    }
                });
            }
            ui.add_space(4.0);
        });
        if dismiss {
            self.notice_dismissed = true;
            if let Some(b) = &self.backend {
                b.dismiss_preselected_notice();
            }
        }
        if self.show_errors && !self.data.errors.is_empty() {
            egui::Panel::bottom("errors").show(root, |ui| {
                ui.add_space(4.0);
                ui.strong("Repos that failed to sync");
                egui::ScrollArea::vertical()
                    .max_height(120.0)
                    .show(ui, |ui| {
                        for (repo, msg) in &self.data.errors {
                            ui.label(format!("{}: {msg}", repo.full_name()));
                        }
                    });
            });
        }
        let mut clicked = None;
        let button = detail_review_button(self);
        let mut review_clicked = false;
        egui::Panel::right("detail")
            .default_size(460.0)
            .size_range(320.0..=700.0)
            .show(root, |ui| match &self.detail {
                Some(d) => {
                    review_clicked = review_button_ui(ui, button, d.pr.forge);
                    show_detail(ui, d);
                }
                None => {
                    ui.add_space(8.0);
                    ui.label(RichText::new("Select a pull request").weak());
                }
            });
        egui::CentralPanel::default().show(root, |ui| {
            if self.items.is_empty() && self.data.repo_count == 0 && self.backend.is_some() {
                ui.add_space(8.0);
                ui.label(NONE_CHOSEN);
                ui.add_space(6.0);
                if ui
                    .button(RichText::new("Choose repositories").strong())
                    .clicked()
                {
                    choose = true;
                }
            } else if self.items.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("No pull requests match.").weak());
            } else {
                let scroll = self.list_scroll.take();
                let out = show_list(ui, &self.inbox, &self.items, self.selected.as_ref(), scroll);
                self.list_view = out.view;
                clicked = out.clicked;
            }
        });
        let mut open_now = false;
        if let Some((sel, double)) = clicked {
            self.review_hint = None;
            self.select_pr(sel);
            // the detail (and so the Review button) of the PR just selected
            match row_click(double, detail_review_button(self)) {
                RowClick::Open => open_now = true,
                RowClick::ShowReason(why) => self.review_hint = Some(why),
                RowClick::Select => {}
            }
        }
        // Up / Down / PageUp / PageDown / Home / End move the selection (not while typing)
        if nav_keys_allowed(
            root.ctx().egui_wants_keyboard_input(),
            root.input(|i| i.modifiers),
        ) {
            if let Some(k) = root.input(Nav::pressed) {
                let page = ((self.list_view.1 / ROW_H).floor() as usize).max(1);
                let to = nav_items(&self.inbox, &self.items, self.selected.as_ref(), k, page);
                if let Some(sel) = to {
                    self.review_hint = None;
                    if let Some(pos) = item_pos(&self.inbox, &self.items, &sel) {
                        if let Some(y) = reveal_offset(pos, ROW_H, self.list_view) {
                            self.list_scroll = Some(y);
                            root.ctx().request_repaint();
                        }
                    }
                    self.select_pr(sel);
                }
            }
        }
        // The Review button, or Enter as its shortcut (only when the button is enabled).
        let enter = enter_opens_review(
            root.input(|i| i.key_pressed(egui::Key::Enter)),
            focused_at_start,
            lost_focus,
        );
        if review_clicked || open_now || (enter && button == Some(ReviewButton::Enabled)) {
            if let Some(pr) = self.detail.as_ref().map(|d| d.pr.clone()) {
                open_review(self, pr.repo.clone(), pr);
            }
        }
        if changed {
            self.dirty = true;
        }
        if choose {
            self.open_picker(Screen::Inbox);
        }
    }
}

/// The one-line notice after the upgrade that started syncing only chosen repositories.
pub fn preselected_notice(n: usize) -> String {
    let s = if n == 1 { "y" } else { "ies" };
    format!(
        "nit now syncs only repositories you choose; the {n} repositor{s} you already had \
         pull requests from stay chosen."
    )
}

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
