//! Top bar: the width plan (`plan_top_bar`) and its drawing.

use super::*;

/// The title always keeps at least this much of the top bar (truncated with an ellipsis).
pub const TITLE_MIN_W: f32 = 160.0;
/// The title gets up to this much before the repository and branches are shown.
pub const TITLE_PREF_W: f32 = 420.0;

/// Top bar buttons that `plan_top_bar` places inline or in the More menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarButton {
    Refresh,
    Approve,
    RequestChanges,
    OpenWeb,
}

/// Natural widths (px, with their spacing) of the top bar's pieces.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BarWidths {
    /// Always shown: Back, the unresolved count, Approve, Request changes, More, separators.
    pub fixed: f32,
    pub title: f32,
    /// "#65 in workspace/repo".
    pub meta: f32,
    /// "source -> destination".
    pub branches: f32,
    pub refresh: f32,
    pub open_web: f32,
}

/// What the top bar shows at one width.
#[derive(Debug, Clone, PartialEq)]
pub struct BarPlan {
    /// Width given to the (truncated) title.
    pub title_w: f32,
    pub meta: bool,
    pub branches: bool,
    /// Buttons on the bar; Approve and Request changes are always here.
    pub inline: Vec<BarButton>,
    /// Buttons moved into the More menu.
    pub more: Vec<BarButton>,
}

/// Fits the top bar into `width`. Approve and Request changes always stay inline. Going
/// narrower, the branches are hidden first, then the repository, then Refresh comments and
/// Open in Bitbucket move into More; the title is truncated to what is left, never below
/// `TITLE_MIN_W` (and gets up to `TITLE_PREF_W` before the repository and branches show).
pub fn plan_top_bar(width: f32, w: &BarWidths) -> BarPlan {
    let title_min = w.title.min(TITLE_MIN_W);
    let mut room = width - w.fixed - title_min;
    let take = |need: f32, room: &mut f32| {
        let fits = need <= *room;
        if fits {
            *room -= need;
        }
        fits
    };
    let open_web = take(w.open_web, &mut room);
    let refresh = open_web && take(w.refresh, &mut room);
    let grow = room.clamp(0.0, (w.title.min(TITLE_PREF_W) - title_min).max(0.0));
    room -= grow;
    let meta = refresh && take(w.meta, &mut room);
    let branches = meta && take(w.branches, &mut room);
    let rest = room.clamp(0.0, (w.title - title_min - grow).max(0.0));
    let mut inline = vec![BarButton::Approve, BarButton::RequestChanges];
    let mut more = Vec::new();
    for (b, on) in [
        (BarButton::Refresh, refresh),
        (BarButton::OpenWeb, open_web),
    ] {
        if on {
            inline.push(b);
        } else {
            more.push(b);
        }
    }
    BarPlan {
        title_w: title_min + grow + rest,
        meta,
        branches,
        inline,
        more,
    }
}

/// The top bar's natural widths at the current style (see `plan_top_bar`).
pub(super) fn bar_widths(ui: &Ui, s: &ReviewScreen, meta: &str, branches: &str) -> BarWidths {
    let style = ui.style();
    let gap = style.spacing.item_spacing.x;
    let pad = 2.0 * style.spacing.button_padding.x;
    let body = egui::TextStyle::Body.resolve(style);
    let text = |t: &str, font: FontId| {
        ui.painter()
            .layout_no_wrap(t.to_owned(), font, Color32::WHITE)
            .size()
            .x
    };
    let button = |t: &str| text(t, egui::TextStyle::Button.resolve(style)) + pad + gap;
    // a separator: its spacing plus the gap after it
    let sep = 6.0 + gap;
    let b = s.actions.buttons(&s.pr);
    let count = if s.comments.loading {
        text("Loading comments...", body.clone()) + gap + 2.0 * style.spacing.interact_size.y
    } else {
        let n = s.comments.unresolved();
        text(&format!("{n} unresolved"), body.clone()) + gap
    };
    let spinner = if s.actions.running() {
        style.spacing.interact_size.y + gap
    } else {
        0.0
    };
    BarWidths {
        fixed: button("Back")
            + 3.0 * sep
            + count
            + button(super::super::actions_ui::label(b.approve))
            + button(super::super::actions_ui::label(b.request))
            + spinner
            + button("More")
            + button(super::super::export_ui::COPY_LABEL)
            // rounding and frame margins
            + 12.0,
        title: text(&s.pr.title, FontId::proportional(15.0)) + gap,
        meta: text(meta, body) + gap,
        branches: text(branches, egui::TextStyle::Monospace.resolve(style)) + gap,
        refresh: button("Refresh comments"),
        open_web: button("Open in Bitbucket"),
    }
}

/// What the top bar was asked for this frame.
#[derive(Default)]
pub(super) struct TopBar {
    pub(super) back: bool,
    pub(super) reload: bool,
    pub(super) refresh: bool,
    /// "Comment on PR" (bar or overflow menu).
    pub(super) comment_pr: bool,
    pub(super) open_web: bool,
    pub(super) actions: Vec<ActionAct>,
}

pub(super) fn draw_top_bar(ui: &mut Ui, s: &ReviewScreen) -> TopBar {
    let mut bar = TopBar::default();
    ui.add_space(4.0);
    // a batch or a review action keeps running from this screen: let it finish first
    let busy = s.busy();
    let busy_why = "Posting to Bitbucket: wait, or stop after the current comment";
    ui.horizontal(|ui| {
        bar.back = ui
            .add_enabled(!busy, egui::Button::new("Back"))
            .on_hover_text("Back to the inbox")
            .on_disabled_hover_text(busy_why)
            .clicked();
        ui.separator();
        // what fits on one row at this width: the rest goes into More (buttons) or is hidden
        // (repository and branches); the title is truncated to its share
        let forge = s.pr.forge;
        let meta = format!("{} in {}", forge.change_ref(s.pr.id), s.repo.full_name());
        let branches = format!("{} -> {}", s.pr.source_branch, s.pr.dest_branch);
        let plan = plan_top_bar(ui.available_width(), &bar_widths(ui, s, &meta, &branches));
        ui.scope(|ui| {
            ui.set_max_width(plan.title_w);
            ui.add(egui::Label::new(RichText::new(&s.pr.title).strong().size(15.0)).truncate())
                .on_hover_text(&s.pr.title);
        });
        if plan.meta {
            ui.add(egui::Label::new(RichText::new(meta).color(GREY)).extend());
        }
        if plan.branches {
            ui.add(egui::Label::new(RichText::new(branches).monospace().color(GREY)).extend());
        }
        let inline = |b: BarButton| plan.inline.contains(&b);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("More", |ui| {
                let comment_on = format!("Comment on {}", forge.change_noun());
                if s.composer.read_only {
                    ui.add_enabled(false, egui::Button::new(comment_on))
                        .on_disabled_hover_text(super::super::composer::READ_ONLY_TIP);
                } else if ui.button(comment_on).clicked() {
                    bar.comment_pr = true;
                    ui.close();
                }
                if !inline(BarButton::Refresh)
                    && ui
                        .add_enabled(!s.comments.loading, egui::Button::new("Refresh comments"))
                        .clicked()
                {
                    bar.refresh = true;
                    ui.close();
                }
                if ui.button(open_web_label(&s.pr)).clicked() {
                    bar.open_web = true;
                    ui.close();
                }
            });
            super::super::export_ui::menu(ui, s);
            if inline(BarButton::OpenWeb)
                && ui
                    .button(open_web_label(&s.pr))
                    .on_hover_text(format!("Open this {} in the browser", forge.change_name()))
                    .clicked()
            {
                bar.open_web = true;
            }
            ui.separator();
            action_buttons(ui, &s.actions, &s.pr, s.stale, &mut bar.actions);
            ui.separator();
            if inline(BarButton::Refresh)
                && ui
                    .add_enabled(!s.comments.loading, egui::Button::new("Refresh comments"))
                    .clicked()
            {
                bar.refresh = true;
            }
            let c = &s.comments;
            if c.loading {
                ui.label(RichText::new("Loading comments...").color(GREY));
                ui.spinner();
            } else {
                let n = c.unresolved();
                let color = if n > 0 { AMBER } else { GREY };
                ui.label(
                    RichText::new(format!("{n} unresolved"))
                        .color(color)
                        .strong(),
                )
                .on_hover_text(format!(
                    "Unresolved comment threads in this {}",
                    forge.change_name()
                ));
            }
        });
    });
    action_notes(ui, &s.actions, s.stale, &mut bar.actions);
    if let Some(e) = &s.comments.error {
        ui.colored_label(RED, e);
    }
    if s.comments.skipped > 0 {
        ui.colored_label(
            AMBER,
            format!(
                "{} comments could not be read and are not shown.",
                s.comments.skipped
            ),
        );
    }
    if let Some(p) = &s.phase {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(p);
        });
    }
    if let Some(e) = &s.failed {
        banner(ui, Color32::from_rgb(70, 28, 28), RED, |ui| {
            ui.label(RichText::new(e).color(RED));
            bar.reload |= ui.add_enabled(!busy, egui::Button::new("Retry")).clicked();
        });
    }
    if s.stale {
        banner(ui, Color32::from_rgb(52, 46, 24), AMBER, |ui| {
            ui.label(RichText::new(STALE_TEXT).color(AMBER).strong());
            bar.reload |= ui
                .add_enabled(!busy, egui::Button::new("Reload"))
                .on_hover_text(format!(
                    "Fetch the {} again; drafts are kept",
                    s.pr.forge.change_name()
                ))
                .on_disabled_hover_text(busy_why)
                .clicked();
        });
    }
    if let Some(e) = &s.store_error {
        ui.colored_label(RED, e);
    }
    ui.add_space(4.0);
    bar
}

pub(super) fn banner(ui: &mut Ui, fill: Color32, stroke: Color32, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style())
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(add);
        });
}
