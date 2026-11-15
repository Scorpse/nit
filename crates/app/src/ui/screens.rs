//! The login, repository picker and settings screens, and the status line.

use super::*;

impl App {
    /// The welcome / sign-in screen (`welcome.rs` draws it; the actions are applied here).
    pub(super) fn login_screen(&mut self, root: &mut egui::Ui) {
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

    pub(super) fn repos_screen(&mut self, root: &mut egui::Ui) {
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

    pub(super) fn status_line(&mut self, ui: &mut egui::Ui) {
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

    pub(super) fn settings_screen(&mut self, root: &mut egui::Ui) {
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
}
