//! The Commits tab: the PR's commits, newest first, those after my last review marked new.
//! Choosing a commit (or all the new ones) shows its files and, for a chosen file, its patch.

use super::super::commits_ui::{commit_range, Commits};
use super::*;
use crate::review::CommitRange;

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

fn top_down() -> egui::Layout {
    egui::Layout::top_down(egui::Align::Min)
}

/// The colour of a patch line: added, removed, hunk header.
fn patch_color(line: &str) -> Color32 {
    match line.as_bytes().first() {
        Some(b'+') if !line.starts_with("+++") => GREEN,
        Some(b'-') if !line.starts_with("---") => RED,
        Some(b'@') => crate::theme::DRAFT,
        _ => GREY,
    }
}

pub(super) fn draw_commits(ui: &mut Ui, s: &mut ReviewScreen) {
    if let Some(e) = &s.commits.error {
        ui.colored_label(RED, e);
        return;
    }
    if s.commits.list.is_none() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Listing commits...");
        });
        return;
    }
    if let Some(note) = s.commits.note() {
        let c = if s.commits.mark_error.is_some() {
            AMBER
        } else {
            GREY
        };
        ui.colored_label(c, note);
        ui.add_space(4.0);
    }
    if s.commits.list.as_ref().is_some_and(Vec::is_empty) {
        ui.label("This pull request has no commits of its own.");
        return;
    }
    let total = ui.available_width();
    let left_w = (total * 0.42).clamp(260.0, 520.0).min(total);
    let height = ui.available_height();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(left_w, height), top_down(), |ui| {
            draw_commit_list(ui, s)
        });
        ui.separator();
        ui.allocate_ui_with_layout(
            vec2((total - left_w - 16.0).max(100.0), height),
            top_down(),
            |ui| draw_range(ui, s),
        );
    });
}

fn draw_commit_list(ui: &mut Ui, s: &mut ReviewScreen) {
    let c = &mut s.commits;
    let new = c.new_flags();
    let since = c.since_review();
    let list = c.list.clone().unwrap_or_default();
    let mut chosen: Option<CommitRange> = None;
    egui::ScrollArea::vertical()
        .id_salt("review-commits")
        .auto_shrink(false)
        .show(ui, |ui| {
            if let Some(range) = since {
                let on = c.selected.as_ref() == Some(&range);
                let n = new.iter().filter(|n| **n).count();
                let label = RichText::new(format!("All {n} new, together"))
                    .strong()
                    .color(GREEN);
                if ui.selectable_label(on, label).clicked() {
                    chosen = Some(range);
                }
                ui.separator();
            }
            // newest first: what was added last is what a returning reviewer looks for
            for (commit, is_new) in list.iter().zip(&new).rev() {
                let range = commit_range(commit);
                let on = c.selected.as_ref() == Some(&range);
                let when = commit.date.strftime("%Y-%m-%d %H:%M");
                let mut job = LayoutJob::default();
                let font = FontId::proportional(13.0);
                let fmt = |color| TextFormat {
                    font_id: font.clone(),
                    color,
                    ..Default::default()
                };
                if *is_new {
                    job.append("new  ", 0.0, fmt(GREEN));
                }
                job.append(
                    commit.id.get(..7).unwrap_or(&commit.id),
                    0.0,
                    TextFormat {
                        font_id: FontId::monospace(12.5),
                        color: GREY,
                        ..Default::default()
                    },
                );
                job.append("  ", 0.0, fmt(GREY));
                job.append(&commit.subject, 0.0, fmt(ui.visuals().text_color()));
                job.append(&format!("\n{}, {when}", commit.author), 0.0, fmt(GREY));
                job.wrap.max_width = ui.available_width();
                if ui.selectable_label(on, job).clicked() {
                    chosen = Some(range);
                }
            }
        });
    if let Some(range) = chosen {
        c.select(range);
    }
}

fn draw_range(ui: &mut Ui, s: &mut ReviewScreen) {
    let c = &mut s.commits;
    let Some(range) = c.selected.clone() else {
        ui.colored_label(GREY, "Choose a commit to see what it changes.");
        return;
    };
    let Some(files) = c.files.get(&range).cloned() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Listing the changed files...");
        });
        return;
    };
    let files = match files {
        Ok(f) => f,
        Err(e) => {
            ui.colored_label(RED, e);
            return;
        }
    };
    if files.is_empty() {
        ui.label("No file changes (for example a merge or an empty commit).");
        return;
    }
    let (adds, dels) = files
        .iter()
        .fold((0u32, 0u32), |(a, d), f| (a + f.additions, d + f.deletions));
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} files", files.len())).strong());
        ui.label(RichText::new(format!("+{adds}")).color(GREEN));
        ui.label(RichText::new(format!("-{dels}")).color(RED));
    });
    let mut open = None;
    let opened = c
        .open_file
        .as_ref()
        .filter(|(r, _)| *r == range)
        .map(|(_, p)| p.clone());
    egui::ScrollArea::vertical()
        .id_salt("review-commit-files")
        .max_height(ui.available_height() * 0.35)
        .show(ui, |ui| {
            for f in &files {
                let on = opened.as_deref() == Some(f.path.as_str());
                let mut job = LayoutJob::default();
                let font = FontId::proportional(13.0);
                let fmt = |color| TextFormat {
                    font_id: font.clone(),
                    color,
                    ..Default::default()
                };
                let (letter, color) = match f.status {
                    FileStatus::Added => ("A", GREEN),
                    FileStatus::Deleted => ("D", RED),
                    FileStatus::Modified => ("M", AMBER),
                    _ => ("R", GREY),
                };
                job.append(letter, 0.0, fmt(color));
                job.append(
                    &format!("  {}", f.path),
                    0.0,
                    fmt(ui.visuals().text_color()),
                );
                if f.binary {
                    job.append("  binary", 0.0, fmt(GREY));
                } else {
                    job.append(&format!("  +{}", f.additions), 0.0, fmt(GREEN));
                    job.append(&format!(" -{}", f.deletions), 0.0, fmt(RED));
                }
                if ui.selectable_label(on, job).clicked() {
                    open = Some(f.clone());
                }
            }
        });
    if let Some(f) = open {
        c.open(&f);
    }
    ui.separator();
    let Some((_, path)) = c.open_file.clone().filter(|(r, _)| *r == range) else {
        ui.colored_label(GREY, "Choose a file to see its changes.");
        return;
    };
    let Some(p) = c
        .patch
        .as_ref()
        .filter(|p| p.range == range && p.path == path)
    else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Building the diff...");
        });
        return;
    };
    match &p.lines {
        Err(e) => {
            ui.colored_label(RED, e);
        }
        Ok(lines) => {
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
            egui::ScrollArea::both()
                .id_salt("review-commit-patch")
                .auto_shrink(false)
                .show_rows(ui, row_h, lines.len(), |ui, rows| {
                    for line in &lines[rows] {
                        ui.add(
                            egui::Label::new(
                                RichText::new(line.as_str())
                                    .monospace()
                                    .color(patch_color(line)),
                            )
                            .extend(),
                        );
                    }
                });
            if p.cut {
                ui.colored_label(AMBER, "Long diff: only the first lines are shown.");
            }
        }
    }
}
