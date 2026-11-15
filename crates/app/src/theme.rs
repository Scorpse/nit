//! App-wide look: one refined dark palette, spacing, text sizes, the logo mark (painted
//! on the welcome screen), the window icon, and the status colours the screens share. Screen
//! code keeps its own diff colours; only egui's global style is set here.

use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontFamily, FontId, Painter, Rect, Stroke,
    TextStyle,
};

/// Panel background (the base layer).
pub const BG: Color32 = Color32::from_rgb(24, 26, 31);
/// Text fields and other recessed areas.
pub const BG_DEEP: Color32 = Color32::from_rgb(15, 16, 20);
/// Windows, popups and cards (one step above the panel).
pub const SURFACE: Color32 = Color32::from_rgb(32, 35, 42);
/// Separators and window edges.
pub const BORDER: Color32 = Color32::from_rgb(52, 56, 66);
/// Body text.
pub const TEXT: Color32 = Color32::from_rgb(204, 208, 216);
/// The single accent: primary buttons, focus rings, the logo.
pub const ACCENT: Color32 = Color32::from_rgb(76, 142, 235);
/// Fill of a primary button (the accent, a little deeper so white text reads well).
pub const ACCENT_FILL: Color32 = Color32::from_rgb(46, 104, 196);
/// Selected items and text selection.
pub const SELECTION: Color32 = Color32::from_rgb(38, 76, 134);
const SELECTION_TEXT: Color32 = Color32::from_rgb(200, 222, 255);
/// Error callouts.
pub const ERROR: Color32 = Color32::from_rgb(235, 87, 87);

// Status colours shared by the screens.
/// Failures and deletions.
pub const RED: Color32 = Color32::from_rgb(235, 87, 87);
/// Success and additions.
pub const GREEN: Color32 = Color32::from_rgb(98, 190, 120);
/// Warnings and open threads.
pub const AMBER: Color32 = Color32::from_rgb(230, 170, 60);
/// Muted text.
pub const GREY: Color32 = Color32::from_rgb(150, 150, 160);
/// Descriptions and secondary lines on cards (the welcome screen's muted tone).
pub const MUTED: Color32 = Color32::from_rgb(150, 156, 168);
/// Text on comment and draft cards.
pub const CARD_TEXT: Color32 = Color32::from_gray(215);
/// Drafts (not posted yet).
pub const DRAFT: Color32 = Color32::from_rgb(110, 150, 230);

/// Applies the palette, spacing and text styles to the dark theme (the only one used).
pub fn apply(ctx: &egui::Context) {
    ctx.style_mut_of(egui::Theme::Dark, style);
}

fn style(s: &mut egui::Style) {
    use FontFamily::{Monospace, Proportional};
    s.text_styles = [
        (TextStyle::Small, FontId::new(10.0, Proportional)),
        (TextStyle::Body, FontId::new(13.5, Proportional)),
        (TextStyle::Button, FontId::new(13.5, Proportional)),
        (TextStyle::Heading, FontId::new(20.0, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, Monospace)),
    ]
    .into();

    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 4.0);
    sp.button_padding = egui::vec2(10.0, 3.0);
    sp.interact_size.y = 22.0;
    sp.menu_margin = egui::Margin::same(8);
    sp.window_margin = egui::Margin::same(10);
    // floating (takes no width, so no layout shifts), and slimmer when expanded
    sp.scroll = egui::style::ScrollStyle {
        bar_width: 8.0,
        ..egui::style::ScrollStyle::floating()
    };

    let v = &mut s.visuals;
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.extreme_bg_color = BG_DEEP;
    v.code_bg_color = Color32::from_rgb(44, 48, 58);
    v.faint_bg_color = Color32::from_rgb(28, 30, 36);
    v.hyperlink_color = Color32::from_rgb(110, 165, 245);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(6);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(80),
    };
    v.selection.bg_fill = SELECTION;
    // also the focus ring of a text field and the text of a selected label
    v.selection.stroke = Stroke::new(1.0, SELECTION_TEXT);
    v.text_cursor.stroke = Stroke::new(2.0, SELECTION_TEXT);

    let w = &mut v.widgets;
    let r4 = CornerRadius::same(4);
    let r5 = CornerRadius::same(5);
    w.noninteractive.bg_fill = BG;
    w.noninteractive.weak_bg_fill = BG;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r4;
    w.inactive.weak_bg_fill = Color32::from_rgb(44, 48, 57);
    w.inactive.bg_fill = Color32::from_rgb(54, 58, 68);
    w.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(222, 225, 232));
    w.inactive.corner_radius = r5;
    w.hovered.weak_bg_fill = Color32::from_rgb(56, 61, 72);
    w.hovered.bg_fill = Color32::from_rgb(64, 69, 80);
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(92, 104, 128));
    w.hovered.fg_stroke = Stroke::new(1.5, Color32::from_rgb(242, 244, 248));
    w.hovered.corner_radius = r5;
    w.active.weak_bg_fill = Color32::from_rgb(40, 44, 53);
    w.active.bg_fill = Color32::from_rgb(50, 55, 65);
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.active.corner_radius = r5;
    w.open.weak_bg_fill = Color32::from_rgb(48, 52, 62);
    w.open.bg_fill = BG;
    w.open.bg_stroke = Stroke::new(1.0, BORDER);
    w.open.corner_radius = r5;
}

// ---- logo mark ----

/// One rounded bar of the mark, in unit coordinates of its square: (x0, y0, x1, y1, radius).
type Bar = (f32, f32, f32, f32, f32);

/// The tile behind the bars.
const TILE: Bar = (0.0, 0.0, 1.0, 1.0, 0.22);
/// Three diff-like lines: the middle one shorter, the last one indented.
const BARS: [Bar; 3] = [
    (0.20, 0.26, 0.80, 0.39, 0.065),
    (0.20, 0.44, 0.60, 0.57, 0.065),
    (0.36, 0.62, 0.80, 0.75, 0.065),
];
const BAR_ALPHA: [u8; 3] = [255, 170, 255];

/// Paints the logo mark into the square `rect`.
pub fn paint_logo(painter: &Painter, rect: Rect) {
    let s = rect.width().min(rect.height());
    let at = |b: Bar| {
        let r = Rect::from_min_max(
            rect.min + egui::vec2(b.0, b.1) * s,
            rect.min + egui::vec2(b.2, b.3) * s,
        );
        (r, b.4 * s)
    };
    let (tile, radius) = at(TILE);
    painter.rect_filled(tile, radius, ACCENT);
    for (b, a) in BARS.iter().zip(BAR_ALPHA) {
        let (r, radius) = at(*b);
        painter.rect_filled(r, radius, Color32::from_white_alpha(a));
    }
}

/// The window icon (Windows title bar and taskbar, X11): the app icon of the installers.
/// Wayland and macOS take the icon from the desktop file or the app bundle instead.
pub fn window_icon() -> egui::IconData {
    const PNG: &[u8] = include_bytes!("../../../packaging/icons/nit-256.png");
    let img = image::load_from_memory_with_format(PNG, image::ImageFormat::Png)
        .expect("the bundled icon is a valid PNG")
        .into_rgba8();
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(c: u8) -> f64 {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(c: Color32) -> f64 {
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    /// WCAG 2 contrast ratio of two opaque colours (1 to 21).
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn body_text_and_accent_have_enough_contrast() {
        for bg in [BG, SURFACE] {
            assert!(contrast(TEXT, bg) >= 4.5, "body text on {bg:?}");
            assert!(contrast(ACCENT, bg) >= 3.0, "accent on {bg:?}");
        }
        assert!(
            contrast(Color32::WHITE, ACCENT_FILL) >= 4.5,
            "primary button text"
        );
        assert!(
            contrast(SELECTION_TEXT, SELECTION) >= 4.5,
            "selected label text"
        );
        assert!(contrast(TEXT, BG_DEEP) >= 4.5, "text field text");
    }

    #[test]
    fn contrast_of_black_and_white_is_21() {
        assert!((contrast(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast(BG, BG) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_window_icon_has_the_right_size_and_a_visible_mark() {
        let icon = window_icon();
        let size = 256;
        assert_eq!((icon.width, icon.height), (size, size));
        assert_eq!(icon.rgba.len(), (size * size * 4) as usize);
        let alpha: Vec<u8> = icon.rgba.chunks(4).map(|p| p[3]).collect();
        // the rounded corners are transparent, the centre is opaque
        assert_eq!(alpha[0], 0, "corner");
        let centre = (size / 2 * size + size / 2) as usize;
        assert_eq!(alpha[centre], 255, "centre");
        let opaque = alpha.iter().filter(|&&a| a == 255).count();
        assert!(opaque > (size * size / 2) as usize);
        // the amber comment bubble on the dark tile
        assert!(icon
            .rgba
            .chunks(4)
            .any(|p| p[0] > 240 && p[1] > 170 && p[2] < 60 && p[3] == 255));
    }

    #[test]
    fn apply_sets_the_palette_on_the_dark_style() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let s = ctx.style_of(egui::Theme::Dark);
        assert_eq!(s.visuals.panel_fill, BG);
        assert_eq!(s.visuals.widgets.noninteractive.fg_stroke.color, TEXT);
        assert_eq!(s.text_styles[&TextStyle::Body].size, 13.5);
    }
}
