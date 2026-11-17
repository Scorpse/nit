//! The Conflicts tab and the banner over the central pane: the files the PR conflicts in with
//! its destination branch, and what each side did to the conflicted lines.

use prcore::conflicts::summary_markdown;

use super::super::conflicts_ui::{lines_of, Kind};
use super::*;

/// The destination's side: blue. The PR's side is green, as added lines are.
const OURS: Color32 = Color32::from_rgb(110, 170, 240);
const BANNER_FILL: Color32 = Color32::from_rgb(120, 35, 35);

/// A red strip over the tabs while the PR conflicts with its destination; a quiet amber
/// line when the test could not run. Nothing when it merges cleanly or is not known yet.
pub(super) fn draw_conflict_banner(ui: &mut Ui, s: &mut ReviewScreen) {
    let dest = s.pr.dest_branch.clone();
    let n = s.conflicts.count();
    if n == 0 {
        if let Some(e) = s.conflicts.error() {
            ui.label(RichText::new(format!("Could not check for conflicts: {e}")).color(AMBER));
        }
        return;
    }
    let mut show = false;
    let mut copy = None;
    egui::Frame::new()
        .fill(BANNER_FILL)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let files = if n == 1 {
                    "1 file".to_string()
                } else {
                    format!("{n} files")
                };
                ui.label(
                    RichText::new(format!("Merge conflicts with {dest} in {files}"))
                        .color(Color32::WHITE)
                        .strong(),
                );
                if ui.button("Show conflicts").clicked() {
                    show = true;
                }
                if ui
                    .button("Copy summary")
                    .on_hover_text("The files and the commands to reproduce it, as text")
                    .clicked()
                {
                    if let Some(r) = s.conflicts.report() {
                        copy = Some(summary_markdown(&s.pr.title, &s.pr.source_branch, &dest, r));
                    }
                }
            });
        });
    ui.add_space(4.0);
    if show {
        s.tab = Tab::Conflicts;
        s.conflicts.select_first();
    }
    if let Some(text) = copy {
        ui.ctx().copy_text(text);
    }
}

pub(super) fn conflicts_tab_label(s: &ReviewScreen) -> String {
    format!("Conflicts ({})", s.conflicts.count())
}

pub(super) fn draw_conflicts(ui: &mut Ui, s: &mut ReviewScreen) {
    let dest = s.pr.dest_branch.clone();
    let Some(report) = s.conflicts.report().cloned() else {
        ui.colored_label(GREY, "No conflicts.");
        return;
    };
    let total = ui.available_width();
    let left_w = (total * 0.32).clamp(220.0, 420.0).min(total);
    let height = ui.available_height();
    let mut chosen = None;
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            vec2(left_w, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("review-conflict-files")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for f in &report.files {
                            let on = s.conflicts.selected.as_deref() == Some(f.path.as_str());
                            let mut job = LayoutJob::default();
                            let font = FontId::proportional(13.0);
                            job.append(
                                &f.path,
                                0.0,
                                TextFormat {
                                    font_id: font.clone(),
                                    color: ui.visuals().text_color(),
                                    ..Default::default()
                                },
                            );
                            job.append(
                                &format!("\n{}", f.kind.describe(&dest)),
                                0.0,
                                TextFormat {
                                    font_id: font,
                                    color: GREY,
                                    ..Default::default()
                                },
                            );
                            job.wrap.max_width = ui.available_width();
                            if ui.selectable_label(on, job).clicked() {
                                chosen = Some(f.path.clone());
                            }
                        }
                    });
            },
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            vec2((total - left_w - 16.0).max(100.0), height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| draw_conflicted_file(ui, s, &dest),
        );
    });
    if let Some(path) = chosen {
        s.conflicts.select(&path);
    }
}

fn draw_conflicted_file(ui: &mut Ui, s: &ReviewScreen, dest: &str) {
    let Some(path) = s.conflicts.selected.as_deref() else {
        ui.colored_label(GREY, "Choose a file to see what conflicts.");
        return;
    };
    ui.label(RichText::new(path).strong());
    let Some(text) = s.conflicts.text(path) else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Reading the merged file...");
        });
        return;
    };
    let segments = match text {
        Ok(s) => s,
        Err(e) => {
            ui.colored_label(RED, e);
            return;
        }
    };
    let lines = lines_of(segments, dest);
    if lines.is_empty() {
        let why = s
            .conflicts
            .report()
            .and_then(|r| r.files.iter().find(|f| f.path == path))
            .map_or(String::new(), |f| f.kind.describe(dest));
        ui.colored_label(
            GREY,
            format!("No conflicting lines to show ({why}). Resolve it in your checkout."),
        );
        return;
    }
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt("review-conflict-text")
        .auto_shrink(false)
        .show_rows(ui, row_h, lines.len(), |ui, rows| {
            for l in &lines[rows] {
                let (color, strong) = match l.kind {
                    Kind::Skip | Kind::Context | Kind::Base => (GREY, false),
                    Kind::Head => (AMBER, true),
                    Kind::OursLabel | Kind::Ours => (OURS, matches!(l.kind, Kind::OursLabel)),
                    Kind::BaseLabel => (GREY, true),
                    Kind::TheirsLabel | Kind::Theirs => {
                        (GREEN, matches!(l.kind, Kind::TheirsLabel))
                    }
                };
                let mut t = RichText::new(l.text.as_str()).monospace().color(color);
                if strong {
                    t = t.strong();
                }
                ui.add(egui::Label::new(t).extend());
            }
        });
}
