//! Review button state and the review card.

use super::*;

// ---- review button ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewButton {
    Enabled,
    /// Shown greyed out with this hint.
    Disabled(&'static str),
    Hidden,
}

/// Whether the PR detail pane's "Review" button is usable. Forks are not supported (hidden);
/// merged, declined and superseded pull requests cannot be reviewed.
pub fn review_button_state(
    is_fork: bool,
    state: PrState,
    has_creds: bool,
    git_ok: bool,
) -> ReviewButton {
    if is_fork {
        ReviewButton::Hidden
    } else if state != PrState::Open {
        ReviewButton::Disabled(REVIEW_NEEDS_OPEN)
    } else if !has_creds {
        ReviewButton::Disabled(REVIEW_NEEDS_SIGN_IN)
    } else if !git_ok {
        ReviewButton::Disabled(REVIEW_NEEDS_GIT)
    } else {
        ReviewButton::Enabled
    }
}

pub(super) fn review_card(ui: &mut egui::Ui, state: &mut SettingsState) {
    card(
        ui,
        "Review",
        "How the review screen behaves.",
        Some(("Applies at once", Tone::Muted)),
        |ui| {
            ui.checkbox(
                &mut state.vim_keys,
                RichText::new("Vim-style keys in the diff").strong(),
            );
            ui.indent("vim-help", |ui| {
                muted(
                    ui,
                    "j / k move by row, gg / G go to the top / bottom, ] / [ jump between \
                     hunks, / searches and c comments. Off: j / k move between files and v \
                     marks a file Viewed.",
                );
                ui.label(
                    RichText::new("See ? in review for the key list.")
                        .color(ui.visuals().hyperlink_color),
                );
            });
            ui.add_space(12.0);
            ui.label(RichText::new("Default diff layout").strong());
            ui.add_space(2.0);
            // a segmented control: both choices in one recessed frame
            egui::Frame::new()
                .fill(theme::BG_DEEP)
                .stroke(Stroke::new(1.0, theme::BORDER))
                .corner_radius(6)
                .inner_margin(egui::Margin::same(2))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.horizontal(|ui| {
                        for (l, name) in [
                            (DiffLayout::Unified, "Unified"),
                            (DiffLayout::Split, "Split"),
                        ] {
                            if ui
                                .add(
                                    egui::Button::selectable(state.layout == l, name)
                                        .min_size(egui::vec2(84.0, ROW_H - 4.0)),
                                )
                                .clicked()
                            {
                                state.layout = l;
                            }
                        }
                    });
                });
            ui.add_space(2.0);
            muted(
                ui,
                "Split shows the old file left and the new one right, in windows 900 px or \
                 wider. Alt+S switches while reviewing.",
            );
        },
    );
}
