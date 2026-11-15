//! Shared tone and small widgets used by every settings card.

use super::*;

// ---- pure summary strings (unit tested; the drawing code only shows them) -------------

/// How a status line or chip is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Bad,
    Busy,
    Muted,
}

// ---- drawing ----------------------------------------------------------------------------

/// The centred column all cards sit in.
pub(super) const COLUMN_W: f32 = 760.0;
pub(super) const CARD_PAD: i8 = 18;
/// Label column of the About rows.
pub(super) const LABEL_W: f32 = 104.0;
/// Fixed widths so the path field can take exactly the rest of the row.
pub(super) const BROWSE_W: f32 = 84.0;
pub(super) const OPEN_W: f32 = 60.0;
pub(super) const ROW_H: f32 = 28.0;

pub(super) fn tone_color(t: Tone) -> Color32 {
    match t {
        Tone::Good => theme::GREEN,
        Tone::Warn => theme::AMBER,
        Tone::Bad => theme::RED,
        Tone::Busy => theme::ACCENT,
        Tone::Muted => theme::MUTED,
    }
}

/// Lays `add` out in a column at most `COLUMN_W` wide, centred in the available width.
pub(super) fn column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let full = ui.available_rect_before_wrap();
    let w = COLUMN_W.min(full.width() - 32.0).max(240.0);
    let rect = egui::Rect::from_min_size(
        egui::pos2((full.center().x - w / 2.0).round(), full.top()),
        egui::vec2(w, full.height()),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.set_width(w);
            add(ui)
        },
    )
    .inner
}

/// A small rounded label with a tinted fill.
pub(super) fn chip(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let c = tone_color(tone);
    egui::Frame::new()
        .fill(c.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, c.gamma_multiply(0.55)))
        .corner_radius(10)
        .inner_margin(egui::Margin::symmetric(8, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(12.0).color(c));
        });
}

/// One status line: a coloured dot (a spinner while busy) and wrapped text.
pub(super) fn status_line(ui: &mut egui::Ui, tone: Tone, text: &str) {
    ui.horizontal(|ui| {
        if tone == Tone::Busy {
            ui.add(egui::Spinner::new().size(12.0));
        } else {
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 14.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(r.center(), 3.5, tone_color(tone));
        }
        let color = match tone {
            Tone::Good | Tone::Busy => theme::TEXT,
            Tone::Muted => theme::MUTED,
            t => tone_color(t),
        };
        ui.add(egui::Label::new(RichText::new(text).color(color)).wrap());
    });
}

pub(super) fn muted(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(theme::MUTED)).wrap());
}

/// The accent-filled button of a card (greyed when disabled).
pub(super) fn primary_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    let fill = if enabled {
        theme::ACCENT_FILL
    } else {
        ui.visuals().widgets.inactive.weak_bg_fill
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(fill)
            .min_size(egui::vec2(0.0, ROW_H)),
    )
}

pub(super) fn button(ui: &mut egui::Ui, enabled: bool, text: &str, width: f32) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(text).min_size(egui::vec2(width, ROW_H)),
    )
}

/// A card: rounded frame, title (with an optional chip on the right), one-line description,
/// then `body`.
pub(super) fn card<R>(
    ui: &mut egui::Ui,
    title: &str,
    desc: &str,
    badge: Option<(&str, Tone)>,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let r = egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(10)
        .inner_margin(egui::Margin::same(CARD_PAD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(title)
                        .size(16.0)
                        .strong()
                        .color(Color32::WHITE),
                );
                if let Some((text, tone)) = badge {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        chip(ui, text, tone)
                    });
                }
            });
            muted(ui, desc);
            ui.add_space(12.0);
            body(ui)
        })
        .inner;
    ui.add_space(14.0);
    r
}
