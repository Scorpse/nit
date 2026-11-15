//! Session lifecycle: construction, backend messages, sync and sign-out.

use super::*;

impl App {
    pub(super) fn blank(screen: Screen) -> App {
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
    pub(super) fn apply_reviewers(&mut self, repo: &RepoRef, id: u64, reviewers: Vec<Reviewer>) {
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

    pub(super) fn drain_backend(&mut self, ctx: &egui::Context) {
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
    pub(super) fn apply_msg(&mut self, m: Msg) -> bool {
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
    pub(super) fn sync_busy(&self) -> bool {
        self.login.busy || self.sync.connecting || self.sync.running
    }

    /// Marks a reconnect as started; false (and no change) when one is already running.
    pub(super) fn begin_sync(&mut self) -> bool {
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
    pub(super) fn clear_session(&mut self) {
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
    pub(super) fn sign_out(&mut self) {
        let result = match self.backend.as_mut() {
            Some(b) => b.sign_out(),
            None => Ok(()),
        };
        self.clear_session();
        self.login.error = result.err();
    }

    pub(super) fn me_known(&self) -> bool {
        !self.me.id.is_empty()
    }

    /// Reconnects and re-syncs with the stored credentials (Retry and Refresh); never twice.
    pub(super) fn retry(&mut self) {
        if self.backend.is_some() && self.begin_sync() {
            if let Some(b) = &self.backend {
                b.connect_stored();
            }
        }
    }

    pub(super) fn connect(&mut self) {
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
    pub(super) fn resync(&mut self) {
        let Some(b) = &self.backend else { return };
        self.login.error = None;
        self.sync.note = None;
        self.sync.connecting = true;
        b.connect_stored();
    }
}
