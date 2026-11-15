//! Review button and git version check.

use super::*;

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
pub(super) fn review_button_ui(
    ui: &mut egui::Ui,
    state: Option<ReviewButton>,
    forge: Forge,
) -> bool {
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
pub(super) type GitVersion = Arc<OnceLock<Option<String>>>;

/// Locates git once on a worker thread; the result is read by the Review button and the
/// version line by the welcome screen's footer.
pub(super) fn spawn_git_check(ctx: Option<egui::Context>) -> (Arc<OnceLock<bool>>, GitVersion) {
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
