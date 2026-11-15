//! The inbox screen.

use super::*;

impl App {
    pub(super) fn inbox_screen(&mut self, root: &mut egui::Ui) {
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
