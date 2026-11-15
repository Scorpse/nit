//! About card.

use super::*;

/// One row of the About card: a muted label on the left, `value` on the right.
pub(super) fn about_row(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, 18.0),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                ui.set_min_width(LABEL_W);
                ui.label(RichText::new(label).color(theme::MUTED));
            },
        );
        ui.vertical(|ui| value(ui));
    });
    ui.add_space(10.0);
}

pub(super) fn about_card(ui: &mut egui::Ui, state: &mut SettingsState) {
    card(
        ui,
        "About",
        "Version and what nit runs on; useful when reporting a problem.",
        None,
        |ui| {
            about_row(ui, "Version", |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} {}",
                        crate::brand::NAME,
                        env!("CARGO_PKG_VERSION")
                    ))
                    .strong(),
                );
            });
            let (value, badge, hint) = git_row(&git_info(state.git.as_ref()));
            about_row(ui, "Git", |ui| {
                ui.horizontal(|ui| {
                    ui.label(value);
                    if let Some((text, tone)) = badge {
                        chip(ui, text, tone);
                    }
                });
                if let Some(h) = hint {
                    muted(ui, h);
                }
            });
            about_row(ui, "Graphics", |ui| {
                match adapter_parts(crate::gpu::adapter_line()) {
                    Some((name, backend)) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(name);
                            ui.label(RichText::new(format!("via {backend}")).color(theme::MUTED));
                        });
                    }
                    None => muted(ui, "not started"),
                }
            });
            about_row(ui, "Data folder", |ui| match state.data_dir.clone() {
                Some(dir) => {
                    ui.add(egui::Label::new(dir.display().to_string()).wrap());
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("Open")
                            .on_hover_text(format!("Show it in {}", crate::open::FILE_MANAGER))
                            .clicked()
                        {
                            state.open_folder("data", dir.clone());
                        }
                        if ui.small_button("Copy path").clicked() {
                            ui.ctx().copy_text(dir.display().to_string());
                            state.copied = Some("data");
                        }
                        if state.copied == Some("data") {
                            ui.label(RichText::new("Copied").color(theme::GREEN));
                        }
                    });
                    if let Some(("data", e)) = &state.open_error {
                        status_line(ui, Tone::Bad, e);
                    }
                }
                None => muted(ui, crate::brand::DATA_DIR_UNSET),
            });
            about_row(ui, "Token check", |ui| {
                muted(
                    ui,
                    "Checks the saved token against Bitbucket and git without changing \
                     anything, and saves a report (token-check.txt) in the data folder. Run it \
                     in a terminal:",
                );
                ui.horizontal(|ui| {
                    egui::Frame::new()
                        .fill(theme::BG_DEEP)
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .corner_radius(4)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.label(RichText::new(CHECK_TOKEN_CMD).monospace());
                        });
                    if ui.small_button("Copy command").clicked() {
                        ui.ctx().copy_text(CHECK_TOKEN_CMD.to_string());
                        state.copied = Some("cmd");
                    }
                    if state.copied == Some("cmd") {
                        ui.label(RichText::new("Copied").color(theme::GREEN));
                    }
                });
            });
            about_row(ui, "Licence", |ui| {
                ui.label("MIT OR Apache-2.0");
            });
        },
    );
}
