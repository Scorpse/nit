//! The Commits tab: the PR's commits, newest first, those after my last review marked new.

use super::*;

/// "Commits (7)", or "Commits (7, 3 new)".
pub(super) fn commits_tab_label(c: &Commits) -> String {
    let Some(list) = c.list.as_ref() else {
        return "Commits".into();
    };
    match c.new_count() {
        0 => format!("Commits ({})", list.len()),
        n => format!("Commits ({}, {n} new)", list.len()),
    }
}

pub(super) fn draw_commits(ui: &mut Ui, s: &mut ReviewScreen) {
    let c = &s.commits;
    if let Some(e) = &c.error {
        ui.colored_label(RED, e);
        return;
    }
    let Some(list) = c.list.as_ref() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Listing commits...");
        });
        return;
    };
    if let Some(note) = c.note() {
        ui.colored_label(if c.mark_error.is_some() { AMBER } else { GREY }, note);
        ui.add_space(4.0);
    }
    if list.is_empty() {
        ui.label("This pull request has no commits of its own.");
        return;
    }
    let new = c.new_flags();
    egui::ScrollArea::vertical()
        .id_salt("review-commits")
        .auto_shrink(false)
        .show(ui, |ui| {
            // newest first: what was added last is what a returning reviewer looks for
            for (commit, is_new) in list.iter().zip(&new).rev() {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [34.0, ui.spacing().interact_size.y],
                        egui::Label::new(if *is_new {
                            RichText::new("new").color(GREEN).strong()
                        } else {
                            RichText::new("")
                        }),
                    );
                    ui.label(
                        RichText::new(commit.id.get(..7).unwrap_or(&commit.id))
                            .monospace()
                            .color(GREY),
                    );
                    ui.add(egui::Label::new(&commit.subject).truncate());
                });
                ui.horizontal(|ui| {
                    ui.add_space(34.0 + ui.spacing().item_spacing.x);
                    ui.label(
                        RichText::new(format!(
                            "{}, {}",
                            commit.author,
                            commit.date.format("%Y-%m-%d %H:%M")
                        ))
                        .small()
                        .color(GREY),
                    );
                });
                ui.add_space(2.0);
            }
        });
}
