//! Demo scenes for screenshots: the one scene table behind `--screen` and `--shots`, and the
//! run loop that renders every scene in a single window (`--demo --shots <dir>`).
//!
//! To add a scene: add a `Scene` variant (ui.rs), then one row to `SCENES`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::demo::DemoReview;
use crate::inbox::View;
use crate::lock::lock_ok;
use crate::ui::{App, Scene, Screen, Shot, ShotDone};

pub const DEFAULT_WIDTH: u32 = 1400;
pub const DEFAULT_HEIGHT: u32 = 900;
/// The narrow scene checks that nothing is clipped at the smallest supported width.
pub const NARROW_WIDTH: u32 = 900;
/// The smallest supported window (`login-small`).
pub const SMALL_SIZE: (u32, u32) = (900, 600);
/// `settings-small`: narrower than the app allows, so the cards are checked with room to spare.
pub const SETTINGS_SMALL: (u32, u32) = (800, 600);
/// Minimum window size of a `--shots` run, so `settings-small` is not clamped to the app's.
pub const SHOTS_MIN_SIZE: [f32; 2] = [640.0, 480.0];
/// Frames to wait after a window resize before the next scene starts.
const RESIZE_FRAMES: u32 = 5;

pub struct SceneDef {
    pub name: &'static str,
    pub screen: Screen,
    pub view: View,
    pub select: Option<(&'static str, u64)>,
    pub scene: Scene,
    /// Window size (width, height) this scene needs instead of the run's size.
    pub size: Option<(u32, u32)>,
}

const fn def(
    name: &'static str,
    screen: Screen,
    view: View,
    select: Option<(&'static str, u64)>,
    scene: Scene,
) -> SceneDef {
    SceneDef {
        name,
        screen,
        view,
        select,
        scene,
        size: None,
    }
}

/// Every demo scene, in `--shots` order. The first-listed `inbox` is also what `--screen`
/// falls back to when absent.
pub const SCENES: &[SceneDef] = &[
    def("login", Screen::Login, View::All, None, Scene::Normal),
    // the same in the smallest supported window
    SceneDef {
        size: Some(SMALL_SIZE),
        ..def("login-small", Screen::Login, View::All, None, Scene::Normal)
    },
    def(
        "inbox",
        Screen::Inbox,
        View::All,
        Some(("acme/api", 102)),
        Scene::Normal,
    ),
    def(
        "stacked",
        Screen::Inbox,
        View::Stacked,
        Some(("acme/gateway", 402)),
        Scene::Normal,
    ),
    // A stack child whose parent moved: the debug line shows two different hashes.
    def(
        "parent-changed",
        Screen::Inbox,
        View::Stacked,
        Some(("acme/billing", 202)),
        Scene::Normal,
    ),
    def("rejected", Screen::Login, View::All, None, Scene::Rejected),
    def(
        "denied",
        Screen::Inbox,
        View::All,
        Some(("acme/api", 102)),
        Scene::Denied,
    ),
    def("settings", Screen::Settings, View::All, None, Scene::Normal),
    // the same below the app's minimum width: cards reflow, nothing is clipped
    SceneDef {
        size: Some(SETTINGS_SMALL),
        ..def(
            "settings-small",
            Screen::Settings,
            View::All,
            None,
            Scene::Normal,
        )
    },
    // unsaved edits with problems: low space, a relative path blocking Save, git too old
    def(
        "settings-problems",
        Screen::Settings,
        View::All,
        None,
        Scene::SettingsProblems,
    ),
    // "Choose repositories": three workspaces, one repository no longer accessible
    def("repos", Screen::Repos, View::All, None, Scene::Normal),
    // a second Bitbucket account and a GitHub one: the Accounts card with "Add account" open
    // (GitLab shown as coming soon), and the picker grouped by account
    def(
        "settings-accounts",
        Screen::Settings,
        View::All,
        None,
        Scene::Accounts,
    ),
    // Add account with GitLab picked: a self-managed address under a path, the help text,
    // and the Test connection answer (scopes, expiry)
    def(
        "settings-add-gitlab",
        Screen::Settings,
        View::All,
        None,
        Scene::AddGitLab,
    ),
    def(
        "repos-accounts",
        Screen::Repos,
        View::All,
        None,
        Scene::Accounts,
    ),
    // the inbox with several accounts: forge and account chips, a rate-limit pause
    def(
        "inbox-accounts",
        Screen::Inbox,
        View::All,
        None,
        Scene::Accounts,
    ),
    // the demo review repository (built on the fly, needs git)
    def("review", Screen::Review, View::All, None, Scene::Normal),
    // the demo review on the file that carries the sample comment threads
    def(
        "review-comments",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewComments,
    ),
    // the Conversation tab with long PR-level, reply and unplaced threads
    def(
        "review-conversation",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewConversation,
    ),
    // the Tasks tab: open and done tasks, and the box for a new one
    def(
        "review-tasks",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewTasks,
    ),
    // the same in a 900 px window, scrolled to the unplaced comments
    SceneDef {
        size: Some((NARROW_WIDTH, DEFAULT_HEIGHT)),
        ..def(
            "review-conversation-narrow",
            Screen::Review,
            View::All,
            None,
            Scene::ReviewConversationEnd,
        )
    },
    // the Commits tab: the demo commit is new since my last review
    def(
        "review-commits",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewCommits,
    ),
    // the same, with sample drafts (one blocked) and an open composer
    def(
        "review-drafts",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewDrafts,
    ),
    // the demo review with a few diff rows selected
    def(
        "review-selection",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewSelection,
    ),
    // the demo review with the Approve confirmation and its warnings open
    def(
        "review-actions",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewActions,
    ),
    // the demo review's file tree: nested directories, comment markers, viewed marks
    def(
        "review-tree",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewTree,
    ),
    // a "not in diff" entry selected: outdated threads on a file a later push removed
    def(
        "review-outdated",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewOutdated,
    ),
    // the commented file side by side: old left, new right, cards under their lines
    def(
        "review-split",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewSplit,
    ),
    // the keyboard help (`?`) over the review screen
    def(
        "review-help",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewHelp,
    ),
    // a GitHub account's PRs in the inbox (forge tag, a fork PR listed), one selected
    def(
        "inbox-github",
        Screen::Inbox,
        View::All,
        Some(("acme-oss/widgets", 31)),
        Scene::Normal,
    ),
    // "Needs me" with the GitHub team request and fork PR next to the Bitbucket ones
    def(
        "needs-me-github",
        Screen::Inbox,
        View::NeedsMe,
        Some(("acme-oss/widgets", 34)),
        Scene::Normal,
    ),
    // a GitHub PR's review: read-only, the write buttons replaced by a badge
    def(
        "review-github",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewGitHub,
    ),
    // a GitLab account's MRs (self-managed, nested group): forge tag, file-count sizes,
    // "1000+ files" over GitLab's default limits, a fork MR; one selected
    def(
        "inbox-gitlab",
        Screen::Inbox,
        View::All,
        Some(("acme-platform/infra/terraform-modules", 51)),
        Scene::Normal,
    ),
    // "Needs me" with the GitLab MRs next to the others
    def(
        "needs-me-gitlab",
        Screen::Inbox,
        View::NeedsMe,
        Some(("acme-platform/infra/terraform-modules", 51)),
        Scene::Normal,
    ),
    // a GitLab MR's review with its write buttons
    def(
        "review-gitlab",
        Screen::Review,
        View::All,
        None,
        Scene::ReviewGitLab,
    ),
];

pub fn lookup(name: &str) -> Option<&'static SceneDef> {
    SCENES.iter().find(|d| d.name == name)
}

pub fn scene_names() -> Vec<&'static str> {
    SCENES.iter().map(|d| d.name).collect()
}

/// `--screen` value to its scene; no `--screen` is the inbox.
pub fn screen_def(name: Option<&str>) -> Option<&'static SceneDef> {
    lookup(name.unwrap_or("inbox"))
}

impl SceneDef {
    pub fn shot(&self, path: String, done: Option<ShotDone>) -> Shot {
        Shot {
            path,
            screen: self.screen,
            view: self.view,
            select: self.select,
            scene: self.scene,
            done,
        }
    }
}

/// Parsed `--shots` options.
#[derive(Debug, PartialEq, Eq)]
pub struct ShotsArgs {
    pub dir: String,
    pub screens: Vec<&'static str>,
    pub width: u32,
    pub height: u32,
}

/// `--screens a,b,c`: names are trimmed; empty items, unknown names and duplicates are errors
/// (an unknown name lists the valid ones).
pub fn parse_screens(list: &str) -> Result<Vec<&'static str>, String> {
    let mut out: Vec<&'static str> = Vec::new();
    for item in list.split(',') {
        let name = item.trim();
        if name.is_empty() {
            return Err(format!("--screens has an empty item in {list:?}"));
        }
        let Some(d) = lookup(name) else {
            return Err(format!(
                "unknown screen {name:?}; valid screens: {}",
                scene_names().join(", ")
            ));
        };
        if out.contains(&d.name) {
            return Err(format!("--screens names {name:?} twice"));
        }
        out.push(d.name);
    }
    Ok(out)
}

/// File name (without directory) for a scene's PNG. Scene names are fixed identifiers; anything
/// outside `[A-Za-z0-9_-]` is replaced so a name can never reach outside the output directory.
pub fn file_name(scene: &str) -> String {
    let stem: String = scene
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}.png")
}

pub fn shot_path(dir: &Path, scene: &str) -> PathBuf {
    dir.join(file_name(scene))
}

/// 0 when every scene was saved and settled, 1 otherwise.
pub fn exit_code(failed: &[String]) -> i32 {
    i32::from(!failed.is_empty())
}

/// Builds the app for one scene through the same path as `--shot --screen`.
fn build_app(
    def: &SceneDef,
    path: String,
    done: ShotDone,
    fixture: &Option<Arc<DemoReview>>,
) -> App {
    let mut app = App::demo(Some(def.shot(path, Some(done))), false);
    if def.screen == Screen::Review {
        if let Some(f) = fixture {
            app.preload_review_fixture(f.clone());
        }
    }
    app
}

/// Renders the scenes one after another in the one window, replacing the app state in place.
pub struct ShotsRun {
    dir: PathBuf,
    scenes: Vec<&'static SceneDef>,
    width: u32,
    height: u32,
    fixture: Option<Arc<DemoReview>>,
    idx: usize,
    app: App,
    done: ShotDone,
    cur_size: (u32, u32),
    /// Frames left to wait after a resize.
    pause: u32,
    saved: usize,
    failed: Vec<String>,
    finished: bool,
    exit: Arc<AtomicI32>,
}

impl ShotsRun {
    /// `fixture`: the demo review repository, built once for every review scene.
    pub fn new(
        args: &ShotsArgs,
        fixture: Option<Arc<DemoReview>>,
        exit: Arc<AtomicI32>,
    ) -> ShotsRun {
        let scenes: Vec<&'static SceneDef> =
            args.screens.iter().filter_map(|n| lookup(n)).collect();
        let dir = PathBuf::from(&args.dir);
        // A window that stops rendering (no frames, so no frame cap) must not hang the run.
        let limit = std::time::Duration::from_secs(60 + 20 * scenes.len() as u64);
        std::thread::spawn(move || {
            std::thread::sleep(limit);
            eprintln!("shots: no result after {}s, giving up", limit.as_secs());
            std::process::exit(1);
        });
        let done: ShotDone = Arc::new(Mutex::new(None));
        let first = scenes[0];
        let app = build_app(
            first,
            shot_path(&dir, first.name).to_string_lossy().into_owned(),
            done.clone(),
            &fixture,
        );
        ShotsRun {
            cur_size: first.size.unwrap_or((args.width, args.height)),
            dir,
            scenes,
            width: args.width,
            height: args.height,
            fixture,
            idx: 0,
            app,
            done,
            pause: 0,
            saved: 0,
            failed: Vec::new(),
            finished: false,
            exit,
        }
    }

    /// The first scene's window size (the window is created with it).
    pub fn first_size(args: &ShotsArgs) -> (u32, u32) {
        args.screens
            .first()
            .and_then(|n| lookup(n))
            .and_then(|d| d.size)
            .unwrap_or((args.width, args.height))
    }

    fn finish_scene(&mut self, ctx: &egui::Context, saved: Result<(), String>) {
        let def = self.scenes[self.idx];
        let path = shot_path(&self.dir, def.name);
        let timed_out = self.app.shot_timed_out;
        match &saved {
            Ok(()) => {
                self.saved += 1;
                let note = if timed_out {
                    " (TIMED OUT: not settled at the frame cap)"
                } else {
                    ""
                };
                eprintln!("shot {} -> {}{note}", def.name, path.display());
            }
            Err(e) => eprintln!("shot {} FAILED: {e}", def.name),
        }
        if saved.is_err() || timed_out {
            self.failed.push(def.name.to_string());
        }
        // Stop the scene's background work before the next one starts, so none of its
        // events can reach the next scene.
        self.app.review.close();
        self.idx += 1;
        if self.idx >= self.scenes.len() {
            self.finish(ctx);
            return;
        }
        let next = self.scenes[self.idx];
        *lock_ok(&self.done) = None;
        self.app = build_app(
            next,
            shot_path(&self.dir, next.name)
                .to_string_lossy()
                .into_owned(),
            self.done.clone(),
            &self.fixture,
        );
        let want = next.size.unwrap_or((self.width, self.height));
        if want != self.cur_size {
            self.cur_size = want;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                want.0 as f32,
                want.1 as f32,
            )));
            self.pause = RESIZE_FRAMES;
        }
    }

    fn finish(&mut self, ctx: &egui::Context) {
        self.finished = true;
        let code = exit_code(&self.failed);
        eprintln!(
            "shots: {} of {} saved in {}{}",
            self.saved,
            self.scenes.len(),
            self.dir.display(),
            if self.failed.is_empty() {
                String::new()
            } else {
                format!("; FAILED: {}", self.failed.join(", "))
            }
        );
        self.exit.store(code, Ordering::SeqCst);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for ShotsRun {
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        if self.finished {
            return;
        }
        if self.pause > 0 {
            self.pause -= 1;
            ctx.request_repaint();
            egui::CentralPanel::default().show(root, |_| {});
            return;
        }
        self.app.ui(root, frame);
        let saved = lock_ok(&self.done).take();
        if let Some(saved) = saved {
            self.finish_scene(&ctx, saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_table_has_unique_valid_names() {
        let names = scene_names();
        for (i, n) in names.iter().enumerate() {
            assert!(!names[..i].contains(n), "duplicate scene {n}");
            assert!(!n.is_empty());
            // every name resolves through --screen parsing
            assert_eq!(screen_def(Some(n)).map(|d| d.name), Some(*n));
        }
        assert_eq!(screen_def(None).map(|d| d.name), Some("inbox"));
        assert!(screen_def(Some("nope")).is_none());
    }

    #[test]
    fn file_names_never_contain_separators() {
        for n in scene_names() {
            let f = file_name(n);
            assert_eq!(f, format!("{n}.png"), "scene names are already safe");
            assert!(!f.contains(['/', '\\', ':']));
        }
        let f = file_name("../a\\b:c");
        assert_eq!(f, "___a_b_c.png");
        assert!(!f.contains(['/', '\\', ':']));
        assert_eq!(
            shot_path(Path::new("out"), "login"),
            Path::new("out").join("login.png")
        );
    }

    #[test]
    fn parse_screens_trims_and_rejects_bad_items() {
        assert_eq!(
            parse_screens(" login , inbox ").unwrap(),
            vec!["login", "inbox"]
        );
        for bad in ["", "login,,inbox", "login,", " , ", "login,login"] {
            assert!(parse_screens(bad).is_err(), "{bad:?}");
        }
        let err = parse_screens("login,nope").unwrap_err();
        assert!(err.contains("\"nope\""));
        for n in scene_names() {
            assert!(err.contains(n), "error lists {n}: {err}");
        }
    }

    #[test]
    fn exit_code_is_zero_only_when_nothing_failed() {
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&["review".to_string()]), 1);
    }

    #[test]
    fn only_the_narrow_scenes_override_the_size() {
        for d in SCENES {
            assert_eq!(
                d.size.is_some(),
                matches!(
                    d.name,
                    "review-conversation-narrow" | "login-small" | "settings-small"
                ),
                "{}",
                d.name
            );
        }
    }
}
