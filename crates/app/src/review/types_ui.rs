//! Comment type badges ("nit", "issue", ...) on cards and drafts, and the composer's type
//! chips. The types and their prefix live in `prcore::comment_type`.

use eframe::egui::{self, vec2, Align2, Color32, FontId, Painter, Pos2, Rect, RichText, Ui};
use prcore::comment_type::{CommentType, COMMENT_TYPES};

use crate::theme::{DRAFT, GREEN, GREY, RED};

const SUGGESTION: Color32 = Color32::from_rgb(170, 130, 230);

/// The colour of a type's badge.
pub fn type_color(t: CommentType) -> Color32 {
    match t {
        CommentType::Issue => RED,
        CommentType::Nit => GREY,
        CommentType::Question => DRAFT,
        CommentType::Suggestion => SUGGESTION,
        CommentType::Praise => GREEN,
    }
}

const BADGE_FONT: f32 = 11.0;
const BADGE_H: f32 = 16.0;

/// Paints a small coloured pill with the type's word, its left edge at `left`, centred on
/// `left.y`; returns its rect.
pub fn paint_badge(p: &Painter, left: Pos2, t: CommentType) -> Rect {
    let color = type_color(t);
    let g = p.layout_no_wrap(t.as_str().into(), FontId::proportional(BADGE_FONT), color);
    let r = Rect::from_min_size(
        Pos2::new(left.x, left.y - BADGE_H / 2.0),
        vec2(g.size().x + 10.0, BADGE_H),
    );
    p.rect_filled(r, BADGE_H / 2.0, color.gamma_multiply(0.18));
    p.rect_stroke(
        r,
        BADGE_H / 2.0,
        egui::Stroke::new(1.0, color.gamma_multiply(0.8)),
        egui::StrokeKind::Inside,
    );
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        t.as_str(),
        FontId::proportional(BADGE_FONT),
        color,
    );
    r
}

/// The type as a coloured label (lists and dialogs).
pub fn type_label(ui: &mut Ui, t: CommentType) {
    ui.label(
        RichText::new(t.as_str())
            .color(type_color(t))
            .size(BADGE_FONT + 1.0),
    );
}

/// Width of the composer's chips row.
pub fn chips_width(ui: &Ui) -> f32 {
    let pad = 2.0 * ui.spacing().button_padding.x + 4.0;
    let w = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.into(), FontId::proportional(12.0), Color32::WHITE)
            .size()
            .x
            + pad
    };
    w("none") + COMMENT_TYPES.iter().map(|(_, s)| w(s)).sum::<f32>()
}

/// The composer's type chips in `rect` (right-aligned): none, then every type. Returns the
/// type clicked (`Some(None)`: "none").
pub fn chips(ui: &mut Ui, rect: Rect, current: Option<CommentType>) -> Option<Option<CommentType>> {
    let mut clicked = None;
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    child.spacing_mut().item_spacing.x = 4.0;
    // right to left: the last type first
    for (t, word) in COMMENT_TYPES.iter().rev() {
        let on = current == Some(*t);
        let text =
            RichText::new(*word)
                .size(12.0)
                .color(if on { Color32::WHITE } else { type_color(*t) });
        let b = egui::Button::new(text)
            .selected(on)
            .fill(if on {
                type_color(*t).gamma_multiply(0.45)
            } else {
                Color32::TRANSPARENT
            })
            .small();
        if child
            .add(b)
            .on_hover_text("Comment type (Tab cycles)")
            .clicked()
        {
            clicked = Some(if on { None } else { Some(*t) });
        }
    }
    let none = egui::Button::new(
        RichText::new("none")
            .size(12.0)
            .color(if current.is_none() {
                Color32::WHITE
            } else {
                GREY
            }),
    )
    .selected(current.is_none())
    .small();
    if child.add(none).on_hover_text("No comment type").clicked() {
        clicked = Some(None);
    }
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_has_its_own_colour() {
        let colors: Vec<Color32> = COMMENT_TYPES.iter().map(|(t, _)| type_color(*t)).collect();
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
