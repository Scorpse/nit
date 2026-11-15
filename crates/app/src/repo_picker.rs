//! "Choose repositories": the discovery cache with a checkbox per repository. Only the
//! repositories chosen here are synced. The logic (filter, counts, workspace toggles, what a
//! Save changes) is plain Rust on `RepoPicker`; `show_picker` only draws it.
//!
//! With several accounts the list is grouped by account, then workspace. A repository two
//! accounts can see is listed under both and synced through the one it is ticked under
//! (decision D8): ticking it under one account unticks it under the other.

use std::collections::HashSet;

use eframe::egui::{self, Id, RichText};
use prcore::model::{AccountId, RepoRef};
use store::RepoChoice;

use crate::theme::{AMBER, GREY, RED};

/// Shown when no repository is chosen, here and in the inbox.
pub const NONE_CHOSEN: &str = "No repositories chosen yet. nit syncs only the repositories \
    you choose: their pull requests, sizes and comments. Until then it reads nothing but the \
    list of your repositories.";
pub const GONE_LABEL: &str = "no longer accessible";

/// What the caller must do after a frame of the picker.
#[derive(Debug, PartialEq, Eq)]
pub enum PickerAction {
    Back,
    /// List the workspaces and repositories again.
    Refresh,
    /// Write the selection (each repository with the account it syncs through); `purge`:
    /// repositories whose cached PRs are deleted.
    Save {
        selected: Vec<(AccountId, RepoRef)>,
        purge: Vec<RepoRef>,
    },
}

#[derive(Default)]
pub struct RepoPicker {
    /// The discovery cache, ordered by workspace and repository; `selected` is what the user
    /// ticked so far.
    pub rows: Vec<RepoChoice>,
    /// Selected when the list was first loaded (to tell what a Save stops syncing).
    initial: HashSet<RepoRef>,
    loaded: bool,
    pub query: String,
    pub discovering: bool,
    pub saving: bool,
    pub error: Option<String>,
    /// Repositories a Save would stop syncing, while the keep / delete question is open.
    pub confirm: Option<Vec<RepoRef>>,
    /// The name shown for each account's group (set by the caller; the id otherwise).
    pub account_labels: Vec<(AccountId, String)>,
}

/// Group order: the default (signed-in) account first, then by id.
fn account_rank(a: &AccountId) -> (bool, AccountId) {
    (*a != AccountId::default_bitbucket(), a.clone())
}

fn matches(c: &RepoChoice, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || c.repo.full_name().to_lowercase().contains(&q)
        || c.name
            .as_ref()
            .is_some_and(|n| n.to_lowercase().contains(&q))
}

impl RepoPicker {
    /// Waiting for the list (`load`).
    pub fn loading() -> Self {
        RepoPicker::default()
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// The list from the store. A reload (after a discovery) keeps what the user ticked
    /// meanwhile for repositories that are still listed.
    pub fn load(&mut self, mut choices: Vec<RepoChoice>) {
        // grouped by account; the store's workspace and repository order stays within each
        choices.sort_by_key(|c| account_rank(&c.account));
        if !self.loaded {
            self.initial = choices
                .iter()
                .filter(|c| c.selected)
                .map(|c| c.repo.clone())
                .collect();
            self.rows = choices;
            self.loaded = true;
            return;
        }
        let ticked: HashSet<(AccountId, RepoRef)> = self.selection().into_iter().collect();
        let known: HashSet<(AccountId, RepoRef)> = self
            .rows
            .iter()
            .map(|c| (c.account.clone(), c.repo.clone()))
            .collect();
        self.rows = choices
            .into_iter()
            .map(|mut c| {
                let key = (c.account.clone(), c.repo.clone());
                if known.contains(&key) {
                    c.selected = ticked.contains(&key);
                }
                c
            })
            .collect();
    }

    /// True when the rows come from more than one account (the list is grouped by account).
    pub fn multi_account(&self) -> bool {
        self.rows
            .first()
            .is_some_and(|f| self.rows.iter().any(|c| c.account != f.account))
    }

    /// The name of `account`'s group.
    pub fn account_label(&self, account: &AccountId) -> String {
        self.account_labels
            .iter()
            .find(|(a, _)| a == account)
            .map_or_else(|| account.as_str().to_string(), |(_, l)| l.clone())
    }

    /// Ticks or unticks row `i`; ticking unticks the same repository under every other
    /// account (it syncs through one account only).
    pub fn tick(&mut self, i: usize, on: bool) {
        self.rows[i].selected = on;
        if on {
            let (repo, account) = (self.rows[i].repo.clone(), self.rows[i].account.clone());
            for c in &mut self.rows {
                if c.repo == repo && c.account != account {
                    c.selected = false;
                }
            }
        }
    }

    /// Rows matching the search, as indexes into `rows`.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|&i| matches(&self.rows[i], &self.query))
            .collect()
    }

    /// (selected, accessible) repositories: what the sync will cover, out of how many.
    pub fn counts(&self) -> (usize, usize) {
        let live = self.rows.iter().filter(|c| !c.gone);
        let total = live.clone().count();
        (live.filter(|c| c.selected).count(), total)
    }

    /// Ticks (or unticks) every visible repository of `account`'s `workspace`; "all" never
    /// ticks one that is no longer accessible.
    pub fn set_workspace(&mut self, account: &AccountId, workspace: &str, on: bool) {
        for i in self.visible() {
            let c = &self.rows[i];
            if c.account == *account && c.repo.namespace == workspace && !(on && c.gone) {
                self.tick(i, on);
            }
        }
    }

    /// The ticked repositories, each with the account it syncs through.
    pub fn selection(&self) -> Vec<(AccountId, RepoRef)> {
        self.rows
            .iter()
            .filter(|c| c.selected)
            .map(|c| (c.account.clone(), c.repo.clone()))
            .collect()
    }

    /// The ticked repositories, whatever their account.
    pub fn selected(&self) -> Vec<RepoRef> {
        self.rows
            .iter()
            .filter(|c| c.selected)
            .map(|c| c.repo.clone())
            .collect()
    }

    /// Repositories selected at load time that a Save would stop syncing.
    pub fn removed(&self) -> Vec<RepoRef> {
        let now: HashSet<RepoRef> = self.selected().into_iter().collect();
        let mut v: Vec<RepoRef> = self
            .initial
            .iter()
            .filter(|r| !now.contains(r))
            .cloned()
            .collect();
        v.sort_by_key(|r| r.full_name());
        v
    }

    /// Save pressed: asks first when repositories stop being synced, else saves.
    pub fn save(&mut self) -> Option<PickerAction> {
        let removed = self.removed();
        if !removed.is_empty() {
            self.confirm = Some(removed);
            return None;
        }
        Some(self.save_now(Vec::new()))
    }

    /// The answer to the keep / delete question (`None`: cancelled).
    pub fn answer(&mut self, delete_cached: Option<bool>) -> Option<PickerAction> {
        let removed = self.confirm.take()?;
        let delete = delete_cached?;
        Some(self.save_now(if delete { removed } else { Vec::new() }))
    }

    fn save_now(&mut self, purge: Vec<RepoRef>) -> PickerAction {
        self.saving = true;
        self.error = None;
        PickerAction::Save {
            selected: self.selection(),
            purge,
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        "y"
    } else {
        "ies"
    }
}

/// The keep / delete question for repositories a Save stops syncing.
fn confirm_ui(ctx: &egui::Context, removed: &[RepoRef]) -> Option<Option<bool>> {
    let mut answer = None;
    let n = removed.len();
    let resp = egui::Modal::new(Id::unique("repo-picker-confirm")).show(ctx, |ui| {
        ui.set_min_width(440.0);
        ui.set_max_width(560.0);
        ui.heading(format!("Stop syncing {n} repositor{}?", plural(n)));
        ui.add_space(4.0);
        for r in removed.iter().take(6) {
            ui.label(format!("\u{2022} {}", r.full_name()));
        }
        if n > 6 {
            ui.label(RichText::new(format!("and {} more", n - 6)).color(GREY));
        }
        ui.add_space(6.0);
        ui.label(
            "Their pull requests leave the inbox. The cached copy can stay on this computer \
             (it is shown again if you choose them again) or be deleted now.",
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .button(RichText::new("Keep cached data").strong())
                .clicked()
            {
                answer = Some(Some(false));
            }
            if ui.button("Delete cached data").clicked() {
                answer = Some(Some(true));
            }
            if ui.button("Cancel").clicked() {
                answer = Some(None);
            }
        });
    });
    if answer.is_none() && resp.should_close() {
        answer = Some(None);
    }
    answer
}

/// Draws the screen; returns what the caller must act on this frame.
pub fn show_picker(root: &mut egui::Ui, p: &mut RepoPicker) -> Option<PickerAction> {
    let mut action = None;
    let ctx = root.ctx().clone();
    let (selected, total) = p.counts();
    egui::Panel::top("repo-picker-bar").show(root, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("< Back").clicked() {
                action = Some(PickerAction::Back);
            }
            ui.heading("Choose repositories");
        });
        ui.label(
            RichText::new(
                "Only the repositories you choose are synced. The list itself comes from your \
                 workspaces and is refreshed only when you ask.",
            )
            .color(GREY),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut p.query)
                    .desired_width(300.0)
                    .hint_text("Search repositories"),
            );
            if ui
                .add_enabled(!p.discovering, egui::Button::new("Refresh list"))
                .on_hover_text("List your workspaces and repositories again")
                .clicked()
            {
                action = Some(PickerAction::Refresh);
            }
            if p.discovering {
                ui.spinner();
                ui.label(RichText::new("Listing repositories...").color(GREY));
            }
        });
        ui.add_space(4.0);
    });
    egui::Panel::bottom("repo-picker-save").show(root, |ui| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let save = ui.add_enabled(
                p.is_loaded() && !p.saving,
                egui::Button::new(RichText::new("Save").strong()),
            );
            if save
                .on_hover_text("Sync just the chosen repositories")
                .clicked()
            {
                action = p.save();
            }
            if ui.button("Cancel").clicked() {
                action = Some(PickerAction::Back);
            }
            ui.label(format!("{selected} of {total} selected"));
            if p.saving {
                ui.spinner();
            }
            if let Some(e) = &p.error {
                ui.colored_label(RED, e);
            }
        });
        if selected == 0 && p.is_loaded() {
            ui.label(RichText::new(NONE_CHOSEN).color(AMBER));
        }
        ui.add_space(6.0);
    });
    egui::CentralPanel::default().show(root, |ui| {
        if !p.is_loaded() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the repository list...");
            });
            return;
        }
        if p.rows.is_empty() {
            ui.label(
                RichText::new(if p.discovering {
                    "Listing your workspaces and repositories..."
                } else {
                    "No repositories listed yet. Press Refresh list."
                })
                .color(GREY),
            );
            return;
        }
        let visible = p.visible();
        if visible.is_empty() {
            ui.label(RichText::new("No repository matches the search.").color(GREY));
        }
        let mut toggles: Vec<(AccountId, String, bool)> = Vec::new();
        let mut ticks: Vec<(usize, bool)> = Vec::new();
        let multi = p.multi_account();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut ws: Option<(AccountId, String)> = None;
                for &i in &visible {
                    let account = p.rows[i].account.clone();
                    let workspace = p.rows[i].repo.namespace.clone();
                    if multi && ws.as_ref().map(|(a, _)| a) != Some(&account) {
                        ui.add_space(12.0);
                        ui.label(RichText::new(p.account_label(&account)).strong().size(16.0));
                    }
                    let group = (account.clone(), workspace.clone());
                    if ws.as_ref() != Some(&group) {
                        let in_ws = p
                            .rows
                            .iter()
                            .filter(|c| c.account == account && c.repo.namespace == workspace);
                        let n = in_ws.clone().filter(|c| !c.gone).count();
                        let on = in_ws.filter(|c| c.selected && !c.gone).count();
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&workspace).strong().size(15.0));
                            ui.label(RichText::new(format!("{on} of {n}")).color(GREY));
                            if ui.small_button("All").clicked() {
                                toggles.push((account.clone(), workspace.clone(), true));
                            }
                            if ui.small_button("None").clicked() {
                                toggles.push((account.clone(), workspace.clone(), false));
                            }
                        });
                        ui.separator();
                        ws = Some(group);
                    }
                    let c = &mut p.rows[i];
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        let slug = c.repo.slug.clone();
                        let mut on = c.selected;
                        if ui.checkbox(&mut on, slug).changed() {
                            ticks.push((i, on));
                        }
                        if let Some(name) = c.name.as_ref().filter(|n| **n != c.repo.slug) {
                            ui.label(RichText::new(name).color(GREY));
                        }
                        if c.private == Some(true) {
                            ui.label(RichText::new("private").small().color(GREY));
                        }
                        if c.gone {
                            ui.label(RichText::new(GONE_LABEL).color(AMBER))
                                .on_hover_text(
                                    "The last refresh of the list did not show it. It is not \
                                     synced; untick it to forget it.",
                                );
                        }
                    });
                }
            });
        for (i, on) in ticks {
            p.tick(i, on);
        }
        for (account, ws, on) in toggles {
            p.set_workspace(&account, &ws, on);
        }
    });
    if let Some(removed) = p.confirm.clone() {
        if let Some(answer) = confirm_ui(&ctx, &removed) {
            action = p.answer(answer).or(action);
        }
    }
    action
}

#[cfg(test)]
mod tests;
