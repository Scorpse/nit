//! The `ReviewScreen` behaviour: events, key actions, selection, search, writes.

use super::*;

impl ReviewScreen {
    pub fn new(repo: RepoRef, pr: PullRequest, store: Arc<Mutex<Store>>) -> ReviewScreen {
        Self::with_in_flight(repo, pr, store, Default::default())
    }

    /// `new`, with the process-wide in-flight registry (drafts loaded with it in place).
    pub fn with_in_flight(
        repo: RepoRef,
        pr: PullRequest,
        store: Arc<Mutex<Store>>,
        in_flight: super::super::composer::InFlight,
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
            commits: Commits::default(),
            tab: Tab::Diff,
            tasks: Tasks::default(),
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
                self.commits.pr_base = refs.base.clone();
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
            ReviewEvent::Commits(list) => {
                self.commits.set_list(list);
                self.save_caught_up();
            }
            ReviewEvent::CommitsFailed(e) => self.commits.fail(e),
            ReviewEvent::LastReview(mark) => {
                self.commits.set_mark(mark);
                self.save_caught_up();
            }
            ReviewEvent::LastReviewFailed(e) => self.commits.fail_mark(e),
            ReviewEvent::RangeFiles { range, result } => self.commits.set_files(range, result),
            ReviewEvent::RangePatch {
                range,
                path,
                result,
            } => self.commits.set_patch(range, path, result),
            ReviewEvent::Tasks(list) => self.tasks.set_list(list),
            ReviewEvent::TaskCreated(result) => self.tasks.created(result),
        }
    }

    /// Remembers the head I am caught up to (as of my last review), so the inbox can flag
    /// the PR once it moves on. Nothing is saved without a review of mine.
    fn save_caught_up(&mut self) {
        if let Some(head) = self.commits.caught_up_head() {
            self.write_caught_up(head);
        }
    }

    fn write_caught_up(&self, head: String) {
        let key = caught_up_key(&self.repo, self.pr.id);
        self.writes
            .write("Could not save the review point", move |st| {
                st.set_meta(&key, &head)
            });
    }

    /// "Refresh comments": shows the spinner and asks the host to refetch.
    pub fn refresh_comments(&mut self) {
        self.comments.loading = true;
        self.comments_refresh = true;
    }

    pub fn take_comment_refresh(&mut self) -> bool {
        std::mem::take(&mut self.comments_refresh)
    }

    pub(super) fn update_counts(&mut self) {
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
    pub(super) fn shown_view(&mut self) -> Option<(String, &mut FileView)> {
        if self.tab != Tab::Diff {
            return None;
        }
        let path = self.selected_path()?.to_string();
        let v = self.diff.view_mut(&path)?;
        Some((path, v))
    }

    /// j / k / paging / gg / G / hunks / sides: moves the cursor (shown on the first press
    /// when there is none: at the top of the viewport) and scrolls it into view.
    pub(super) fn move_cursor(&mut self, a: Action, count: Option<u32>) {
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
    pub(super) fn scroll_to(&mut self, x: f32, y: Option<f32>, now: f32) {
        if let Some(y) = y.map(|y| y.max(0.0)).filter(|y| (y - now).abs() > 0.5) {
            self.restore_scroll = Some(Vec2::new(x, y));
        }
    }

    /// Visual mode: the selection covers whole rows from where it started to the cursor.
    pub(super) fn update_visual(&mut self) {
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
    pub(super) fn comment_at_cursor(&mut self, fx: &mut KeyEffects) {
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
    pub(super) fn refresh_matches(&mut self) {
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
    pub(super) fn search_step(&mut self, forward: bool, n: usize) {
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
        let acted = self.actions.done.is_some();
        if self.actions.poll(&mut self.pr, &self.composer.head) {
            self.stale = true;
            self.composer.stale = true;
        }
        // a review action just went through: I am caught up to the head it was made on
        if !acted && self.actions.done.is_some() && !self.composer.head.is_empty() {
            self.write_caught_up(self.composer.head.clone());
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
    pub(super) fn reveal(&mut self, id: i64) {
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

    pub(super) fn selected_path(&self) -> Option<&str> {
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

    pub(super) fn after_selection_change(&mut self) {
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

    pub(super) fn reveal_selected(&mut self) {
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
    pub(super) fn settled(&self) -> bool {
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
