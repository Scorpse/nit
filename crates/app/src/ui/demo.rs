//! Demo mode, screenshot scenes and the scripted bench.

use super::*;

impl App {
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

    pub(super) fn script(&mut self, ctx: &egui::Context) {
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
    pub(super) fn demo_scene(&mut self, scene: Scene) {
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
}
