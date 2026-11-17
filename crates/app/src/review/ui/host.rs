//! The review host: service plumbing, recheck timer and the per-frame `show`.

use super::*;

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
            && (!self.shot_commits
                || self
                    .screen
                    .as_ref()
                    .is_some_and(|s| s.commits.patch.is_some()))
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

    pub(super) fn start(
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
                        Arc::new(ReviewService::with_local_reviewed(
                            store.clone(),
                            fixture.repo_path.clone(),
                            ctx.clone(),
                            Arc::new(move |_, _| Ok(head.clone())),
                            Arc::new(|_, _| Ok(crate::demo::review_comments_demo())),
                            Arc::new(|_, _| Ok(Some(crate::demo::demo_last_review()))),
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

    pub(super) fn shot_setup(&mut self) {
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
        if self.shot_commits {
            s.tab = Tab::Commits;
        }
        if self.shot_conversation_end {
            s.conversation_to_end = true;
        }
        if self.shot_tasks {
            s.tab = Tab::Tasks;
        }
        if self.shot_split {
            s.layout = DiffLayout::Split;
        }
        s.help_open = self.shot_help;
        self.shot_scene = false;
    }
}

/// Commits scene: chooses the newest commit and its first file as soon as each is known.
fn shot_commits_step(s: &mut ReviewScreen) {
    let c = &mut s.commits;
    if c.selected.is_none() {
        if let Some(last) = c.list.as_ref().and_then(|l| l.last()) {
            c.select(super::super::commits_ui::commit_range(last));
        }
        return;
    }
    if c.open_file.is_none() {
        let range = c.selected.clone().unwrap_or_else(|| unreachable!());
        if let Some(Ok(files)) = c.files.get(&range) {
            if let Some(f) = files.first().cloned() {
                c.open(&f);
            }
        }
    }
}

/// Drafts scene: the draft on a line outside the diff shows as blocked, and a composer is
/// open on a line of the commented file.
pub(super) fn shot_drafts_setup(c: &mut Composer) {
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
pub(super) fn preset_shot_selection(s: &mut ReviewScreen) -> bool {
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
    let shot_commits = host.shot_commits;
    let Some(screen) = host.screen.as_mut() else {
        return true;
    };
    if shot_commits {
        shot_commits_step(screen);
    }
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
    if let Some(scope) = super::super::export_ui::shortcut(&ctx, modal) {
        super::super::export_ui::copy(&ctx, screen, scope);
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
        for want in screen.commits.take_wants() {
            match want {
                Want::Files(range) => svc.request_range_files(range),
                Want::Patch(range, file) => svc.request_range_patch(range, file),
            }
        }
        // where the diff is scrolled: its colours start there
        if let Some(v) = screen.take_visible_change() {
            svc.set_visible(&v.0, v.1);
        }
        if screen.take_comment_refresh() {
            svc.refresh_comments();
        }
        for content in screen.tasks.take_wants() {
            svc.create_task(content);
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
