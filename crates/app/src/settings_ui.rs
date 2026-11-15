//! Settings screen: where repo mirrors go (`download_dir`) and which folder is scanned for
//! existing clones (`clone_root`). The logic (validation, save, review-button rules) is plain
//! Rust in `SettingsState` and the free functions; `show_settings` only draws it.
//!
//! Nothing here touches the filesystem on the UI thread: string checks (absolute path, quotes,
//! nesting) run per keystroke, while checks that look at the disk, folder pickers, git and the
//! clone scan run on worker threads and report back through a channel. Results carry a
//! generation number; a result whose generation is no longer current is dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, Color32, RichText, Stroke};
use gitlocal::{discover_clones, Git, LocalClone, SkippedRepo, NO_HTTPS_REMOTE};
use prcore::model::{AccountId, Forge, PrState};
use providers::github::GitHubBases;
use providers::gitlab::GitLabBases;
use store::{Settings, StoreWriter};

use crate::review::split::DiffLayout;
use crate::theme;

/// How many levels below `clone_root` the clone scan looks.
const SCAN_DEPTH: usize = 4;
const GIT_MISSING: &str = "git not found / too old (need 2.31 or newer)";
/// An example download folder and clones folder in the platform's path syntax.
const EXAMPLE_DOWNLOAD: &str = if cfg!(windows) {
    r"D:\nit\mirrors"
} else if cfg!(target_os = "macos") {
    "/Users/you/nit/mirrors"
} else {
    "/home/you/nit/mirrors"
};
const EXAMPLE_CLONES: &str = if cfg!(windows) {
    r"D:\code (optional)"
} else if cfg!(target_os = "macos") {
    "/Users/you/code (optional)"
} else {
    "/home/you/code (optional)"
};
const FULL_PATH_HINT: &str = if cfg!(windows) {
    r"Use a full path, e.g. D:\nit\mirrors"
} else if cfg!(target_os = "macos") {
    "Use a full path, e.g. /Users/you/nit/mirrors"
} else {
    "Use a full path, e.g. /home/you/nit/mirrors"
};

pub const REVIEW_NEEDS_SIGN_IN: &str = "Sign in to review this pull request";
pub const REVIEW_NEEDS_GIT: &str = "Git 2.31 or newer is needed to review (install it and restart)";
pub const REVIEW_NEEDS_OPEN: &str = "Only open pull requests can be reviewed here.";

mod storage;
pub use storage::*;
mod widgets;
pub use widgets::*;
mod repositories;
pub use repositories::*;
mod accounts;
pub use accounts::*;
mod review;
pub use review::*;
mod about;
use about::*;
mod state;
pub use state::*;

/// Draws the screen. Returns what the caller must act on this frame.
pub fn show_settings(root: &mut egui::Ui, state: &mut SettingsState) -> Option<SettingsAction> {
    let ctx = root.ctx().clone();
    // read before any widget runs: Esc in a text field only ends the edit
    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape))
        && ctx.memory(|m| m.focused().is_none())
        && !state.confirm_sign_out
        && state.confirm_remove.is_none();
    let mut action = state.poll().map(SettingsAction::Persist);
    if esc {
        action = Some(SettingsAction::Back);
    }
    egui::Panel::top("settings-bar").show(root, |ui| {
        ui.add_space(6.0);
        column(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("< Back")
                    .on_hover_text("Back to the inbox (Esc)")
                    .clicked()
                {
                    action = Some(SettingsAction::Back);
                }
                ui.add_space(4.0);
                ui.heading("Settings");
            });
        });
        ui.add_space(6.0);
    });
    egui::CentralPanel::default().show(root, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                column(ui, |ui| {
                    ui.add_space(14.0);
                    if let Some(e) = &state.load_error {
                        status_line(
                            ui,
                            Tone::Bad,
                            &format!(
                                "Could not read the saved settings ({e}). Saving is disabled."
                            ),
                        );
                        ui.add_space(8.0);
                    }
                    if let Some(a) = account_card(ui, state) {
                        action = Some(a);
                    }
                    repos_card(ui, state, &mut action);
                    storage_card(ui, state);
                    review_card(ui, state);
                    about_card(ui, state);
                    ui.add_space(10.0);
                });
            });
    });
    if let Some(other) = state.confirm_remove.clone() {
        match remove_confirm(&ctx, &other, state.removal_counts.as_ref()) {
            Some(true) => {
                state.confirm_remove = None;
                state.removal_counts = None;
                action = Some(SettingsAction::RemoveAccount(other.id));
            }
            Some(false) => {
                state.confirm_remove = None;
                state.removal_counts = None;
            }
            None => {}
        }
    }
    if state.confirm_sign_out {
        match sign_out_confirm(&ctx) {
            Some(true) => {
                state.confirm_sign_out = false;
                action = Some(SettingsAction::SignOut);
            }
            Some(false) => state.confirm_sign_out = false,
            None => {}
        }
    }
    action
}

#[cfg(test)]
mod tests;
