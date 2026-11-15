//! Painting: cards, editor, drafts bar, confirm dialog and toast.

use super::*;

// ---- painting ----

use crate::theme::{AMBER, DRAFT, RED};
pub(super) const QUIET: Color32 = crate::theme::GREY;
pub(super) const TEXT: Color32 = crate::theme::CARD_TEXT;
pub(super) const DRAFT_BG: Color32 = Color32::from_rgb(30, 36, 52);

pub const BUTTON_H: f32 = 20.0;
pub const EDITOR_TEXT_H: f32 = 92.0;
pub const EDITOR_ID: &str = "review-draft-editor";

/// Height of the composer card.
pub fn editor_height() -> f32 {
    2.0 * CARD_PAD + TITLE_H + COMMENT_GAP + EDITOR_TEXT_H + COMMENT_GAP + BUTTON_H
}

/// Height of a draft card at `width`.
pub fn draft_height(
    d: &Draft,
    state: &DraftState,
    width: f32,
    measure: &mut dyn FnMut(&str, f32) -> f32,
) -> f32 {
    let note = if state.note(d).is_some() {
        AUTHOR_H
    } else {
        0.0
    };
    2.0 * CARD_PAD + TITLE_H + note + COMMENT_GAP + measure(&d.text, body_width(width, false))
}

/// Where a draft sits, for its title: "new line 10", "file", "reply", "pull request".
pub(super) fn place_word(d: &Draft) -> String {
    match (d.kind, d.side, d.line) {
        (DraftKind::Inline, Some(s), Some(n)) => format!("{} line {n}", side_word(s)),
        (DraftKind::File, ..) => "file comment".into(),
        (DraftKind::Reply, ..) => "reply".into(),
        (DraftKind::Pr, ..) => "pull request comment".into(),
        _ => "incomplete".into(),
    }
}

/// Adds `widget` in `rect` inside a card without moving `ui`'s cursor. The caller
/// allocated the whole card; `Ui::put` would advance the cursor to the widget's bottom (a
/// title button's bottom is near the card's top), so the next item overlapped the card.
pub fn put_in_card(ui: &mut Ui, rect: Rect, widget: impl egui::Widget) -> egui::Response {
    ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(
        egui::Layout::centered_and_justified(egui::Direction::TopDown),
    ))
    .add(widget)
}

/// Small buttons right-aligned in the title row of `card`; returns which was clicked.
pub(super) fn title_buttons(ui: &mut Ui, card: Rect, labels: &[(&str, bool)]) -> Option<usize> {
    let mut right = card.right() - CARD_PAD;
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    let mut clicked = None;
    for (i, (label, enabled)) in labels.iter().enumerate() {
        let w = ui
            .painter()
            .layout_no_wrap(label.to_string(), FontId::proportional(12.0), TEXT)
            .size()
            .x
            + 2.0 * ui.spacing().button_padding.x
            + 6.0;
        let r = Rect::from_min_size(Pos2::new(right - w, cy - BUTTON_H / 2.0), vec2(w, BUTTON_H));
        let text = RichText::new(*label).size(12.0);
        let b = egui::Button::new(if *enabled { text } else { text.color(QUIET) })
            .wrap_mode(egui::TextWrapMode::Extend);
        if put_in_card(ui, r, b.sense_if(*enabled)).clicked() && *enabled {
            clicked = Some(i);
        }
        right = r.left() - 4.0;
    }
    clicked
}

pub(super) trait SenseIf {
    fn sense_if(self, on: bool) -> Self;
}

impl SenseIf for egui::Button<'_> {
    fn sense_if(self, on: bool) -> Self {
        if on {
            self
        } else {
            self.sense(Sense::hover())
        }
    }
}

/// Paints one draft into `card` (sized by `draft_height`) with Edit / Delete and, for a
/// blocked inline draft, "Post as file comment instead".
pub fn draft_card(
    ui: &mut Ui,
    painter: &Painter,
    card: Rect,
    d: &Draft,
    state: &DraftState,
    c: &Composer,
    acts: &mut Vec<Act>,
) {
    let locked = c.running();
    let p = painter.with_clip_rect(card.intersect(painter.clip_rect()));
    p.rect_filled(card, 4.0, DRAFT_BG);
    p.rect_stroke(card, 4.0, Stroke::new(1.0, DRAFT), StrokeKind::Inside);
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    let badge = p.layout_no_wrap("draft".into(), FontId::proportional(11.0), Color32::WHITE);
    let pill = Rect::from_min_size(
        Pos2::new(card.left() + CARD_PAD, cy - 8.0),
        vec2(badge.size().x + 12.0, 16.0),
    );
    p.rect_filled(pill, 8.0, DRAFT.gamma_multiply(0.7));
    p.galley(pill.center() - badge.size() / 2.0, badge, Color32::WHITE);
    let mut x = pill.right() + 8.0;
    if let Some(t) = d.comment_type {
        x = super::super::types_ui::paint_badge(&p, Pos2::new(x, cy), t).right() + 8.0;
    }
    let mut piece = |text: &str, color: Color32| {
        let r = p.text(
            Pos2::new(x, cy),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(13.0),
            color,
        );
        x = r.right() + 8.0;
    };
    piece(&place_word(d), QUIET);
    if *state == DraftState::Posting {
        piece("Posting...", AMBER);
    }
    let mut y = card.top() + CARD_PAD + TITLE_H;
    if let Some((note, color)) = state.note(d) {
        p.text(
            Pos2::new(card.left() + CARD_PAD, y + AUTHOR_H / 2.0),
            Align2::LEFT_CENTER,
            note,
            FontId::proportional(12.0),
            color,
        );
        y += AUTHOR_H;
    }
    y += COMMENT_GAP;
    let body = body_galley(&p, &d.text, body_width(card.width(), false), TEXT);
    p.galley(Pos2::new(card.left() + CARD_PAD, y), body, TEXT);

    if !card.intersects(painter.clip_rect()) {
        return;
    }
    let editable = !locked && *state != DraftState::Posting;
    let blocked = matches!(state, DraftState::Blocked(_) | DraftState::Outdated)
        && d.kind == DraftKind::Inline;
    let mut labels = vec![(if blocked { "Discard" } else { "Delete" }, editable)];
    if *state != DraftState::PostedNotRemoved {
        labels.push(("Edit", editable));
    }
    let off = d.kind == DraftKind::Inline && !c.inline_enabled();
    if blocked || off {
        labels.push((AS_FILE, editable));
    }
    if off {
        labels.push((AS_PR, editable));
    }
    if let DraftState::Unconfirmed { checked, .. } = state {
        labels.push((POST_AGAIN, editable && *checked));
        labels.push((CHECK_COMMENTS, editable));
    }
    match title_buttons(ui, card, &labels).map(|i| labels[i].0) {
        Some("Delete" | "Discard") => acts.push(Act::Delete(d.id)),
        Some("Edit") => acts.push(Act::Edit(d.id)),
        Some(AS_FILE) => acts.push(Act::AsFile(d.id)),
        Some(AS_PR) => acts.push(Act::AsPr(d.id)),
        Some(POST_AGAIN) => acts.push(Act::Rearm(d.id)),
        Some(CHECK_COMMENTS) => acts.push(Act::CheckComments(d.id)),
        _ => {}
    }
}

pub(super) const AS_FILE: &str = "Post as file comment instead";
pub(super) const AS_PR: &str = "Post as PR comment instead";
pub(super) const CHECK_COMMENTS: &str = "Check comments";
pub(super) const POST_AGAIN: &str = "Post again anyway";

pub(super) fn editor_title(e: &Editor) -> String {
    let what = match &e.target {
        Target::Pr => "Comment on the pull request".to_string(),
        Target::File(p) => format!("Comment on file {p}"),
        Target::Inline {
            side,
            line,
            start: None,
            ..
        } => {
            format!("Comment on {} line {line}", side_word(*side))
        }
        Target::Inline {
            side, line, start, ..
        } => format!("Comment on lines {}", lines_word(*side, *line, *start)),
        Target::Reply { .. } => "Reply".to_string(),
    };
    if e.draft_id.is_some() {
        format!("Edit draft: {what}")
    } else {
        what
    }
}

/// The composer in `card` (sized by `editor_height`): Ctrl+Enter saves, Esc cancels.
pub fn editor_card(
    ui: &mut Ui,
    painter: &Painter,
    card: Rect,
    e: &mut Editor,
    acts: &mut Vec<Act>,
) {
    let p = painter.with_clip_rect(card.intersect(painter.clip_rect()));
    p.rect_filled(card, 4.0, DRAFT_BG);
    p.rect_stroke(card, 4.0, Stroke::new(1.5, DRAFT), StrokeKind::Inside);
    let cy = card.top() + CARD_PAD + TITLE_H / 2.0;
    p.text(
        Pos2::new(card.left() + CARD_PAD, cy),
        Align2::LEFT_CENTER,
        editor_title(e),
        FontId::proportional(13.0),
        Color32::WHITE,
    );
    if !card.intersects(painter.clip_rect()) {
        return;
    }
    let id = Id::unique(EDITOR_ID);
    // handled before the text field sees them (it would insert a newline / drop focus)
    if ui.memory(|m| m.has_focus(id)) {
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
            acts.push(Act::Save);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            acts.push(Act::Cancel);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Tab)) {
            e.comment_type = CommentType::cycle(e.comment_type, false);
        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            e.comment_type = CommentType::cycle(e.comment_type, true);
        }
    }
    let chips_w = super::super::types_ui::chips_width(ui);
    let chips = Rect::from_min_size(
        Pos2::new(card.right() - CARD_PAD - chips_w, cy - BUTTON_H / 2.0),
        vec2(chips_w, BUTTON_H),
    );
    if let Some(t) = super::super::types_ui::chips(ui, chips, e.comment_type) {
        e.comment_type = t;
        e.focus = true;
    }
    let top = card.top() + CARD_PAD + TITLE_H + COMMENT_GAP;
    let text_rect = Rect::from_min_size(
        Pos2::new(card.left() + CARD_PAD, top),
        vec2(card.width() - 2.0 * CARD_PAD, EDITOR_TEXT_H),
    );
    let edit = egui::TextEdit::multiline(&mut e.text)
        .id(id)
        .desired_width(text_rect.width())
        // Tab cycles the comment type (handled above) instead of leaving the field
        .lock_focus(true)
        .hint_text(format!(
            "Write a comment. {} saves the draft, Esc cancels, Tab sets the type.",
            super::super::keymap::CMD_ENTER
        ));
    let resp = put_in_card(ui, text_rect, edit);
    if e.focus {
        resp.request_focus();
        e.focus = false;
    }
    let by = text_rect.bottom() + COMMENT_GAP;
    let save = Rect::from_min_size(Pos2::new(card.left() + CARD_PAD, by), vec2(90.0, BUTTON_H));
    let cancel = Rect::from_min_size(Pos2::new(save.right() + 6.0, by), vec2(64.0, BUTTON_H));
    let empty = e.text.trim().is_empty();
    if put_in_card(ui, save, egui::Button::new("Save draft").sense_if(!empty))
        .on_hover_text(super::super::keymap::CMD_ENTER)
        .clicked()
        && !empty
    {
        acts.push(Act::Save);
    }
    if put_in_card(ui, cancel, egui::Button::new("Cancel"))
        .on_hover_text("Esc")
        .clicked()
    {
        acts.push(Act::Cancel);
    }
    let mut last = cancel;
    if e.suggestions.is_some()
        && matches!(
            e.target,
            Target::Inline {
                side: Side::New,
                ..
            }
        )
    {
        last = Rect::from_min_size(Pos2::new(cancel.right() + 6.0, by), vec2(110.0, BUTTON_H));
        if put_in_card(ui, last, egui::Button::new("Suggest change"))
            .on_hover_text("Insert the commented lines as a suggested change")
            .clicked()
        {
            acts.push(Act::Suggest);
        }
    }
    p.text(
        Pos2::new(last.right() + 10.0, cancel.center().y),
        Align2::LEFT_CENTER,
        "Saved locally; nothing is posted until you submit.",
        FontId::proportional(12.0),
        QUIET,
    );
}

/// A draft card as a widget (sections outside the virtualized rows).
pub fn draft_widget(ui: &mut Ui, c: &Composer, d: &Draft, acts: &mut Vec<Act>) {
    let w = ui.available_width().min(1000.0);
    let state = c.state(d.id);
    let h = {
        let painter = ui.painter().clone();
        let mut m = super::super::comments_ui::measure_with(&painter);
        draft_height(d, &state, w, &mut m)
    };
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter().clone();
    draft_card(ui, &painter, rect, d, &state, c, acts);
    ui.add_space(CARD_MARGIN);
}

/// The composer as a widget, when it targets `target`.
pub fn editor_widget(ui: &mut Ui, c: &mut Composer, target: &Target, acts: &mut Vec<Act>) {
    let Some(e) = c.editor.as_mut().filter(|e| &e.target == target) else {
        return;
    };
    let w = ui.available_width().min(1000.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, editor_height()), Sense::hover());
    let painter = ui.painter().clone();
    editor_card(ui, &painter, rect, e, acts);
    ui.add_space(CARD_MARGIN);
}

/// Reply drafts and the reply composer of thread `t`, under its card.
pub fn reply_widgets(ui: &mut Ui, c: &mut Composer, t: &Thread, acts: &mut Vec<Act>) {
    let ids: Vec<i64> = c.replies_to(t).map(|d| d.id).collect();
    for id in ids {
        if let Some(d) = c.draft(id).cloned() {
            draft_widget(ui, c, &d, acts);
        }
    }
    if let Some(thread) = t.id() {
        let target = Target::Reply {
            thread: thread.clone(),
        };
        editor_widget(ui, c, &target, acts);
    }
}

/// Reply and Resolve / Unresolve on a thread card's title row.
pub fn thread_buttons(ui: &mut Ui, card: Rect, t: &Thread, c: &Composer, acts: &mut Vec<Act>) {
    if c.read_only {
        // no Resolve / Reply: nothing is written to this forge in this version
        return;
    }
    let busy = c.is_resolving(&t.root.id);
    let resolve = if t.root.resolved {
        "Unresolve"
    } else {
        "Resolve"
    };
    // GitHub says per thread whether I may (un)resolve it
    let labels = [
        (resolve, !busy && c.can_post() && t.root.can_resolve),
        ("Reply", !c.running()),
    ];
    match title_buttons(ui, card, &labels) {
        Some(0) => acts.push(Act::Resolve(t.root.id.clone())),
        Some(_) => {
            if let Some(thread) = t.id() {
                acts.push(Act::Open(Target::Reply {
                    thread: thread.clone(),
                }))
            }
        }
        None => {}
    }
}

/// Why nothing can be written on a read-only forge (the tooltip of its "Read-only" badge).
/// Forge-neutral: which forges are read-only is `Capabilities::read_only`'s call.
pub const READ_ONLY_TIP: &str = "read-only for this forge in this version: nothing is posted";

/// The tooltip of the "experimental" badge next to Submit while inline posting is on. Only
/// Bitbucket's switch is the environment variable; other forges have their own gate.
pub fn inline_experiment_tip(forge: Forge) -> &'static str {
    match forge {
        Forge::BitbucketCloud => {
            "NIT_INLINE_EXPERIMENT=1: inline comments are enabled for the anchoring \
             experiment; placement is not verified."
        }
        Forge::GitHub | Forge::GitLab => {
            "Inline comments on this forge are new: check where each one lands in the \
             browser after posting."
        }
    }
}

/// A disabled "Read-only" button whose tooltip says why (in place of the write buttons).
pub fn read_only_badge(ui: &mut Ui) {
    ui.add_enabled(false, egui::Button::new("Read-only"))
        .on_disabled_hover_text(READ_ONLY_TIP);
}

/// The bottom bar: "Drafts (N)", Submit, progress, errors and the list of drafts.
pub fn drafts_bar(ui: &mut Ui, c: &Composer, acts: &mut Vec<Act>) {
    ui.add_space(4.0);
    if c.read_only {
        read_only_badge(ui);
        return;
    }
    ui.horizontal(|ui| {
        let n = c.drafts.len();
        ui.label(RichText::new(format!("Drafts ({n})")).strong());
        let postable = c.postable().len();
        // refused inline drafts still enable Submit: the click explains the refusal
        let ok = !c.running() && (postable > 0 || c.refused_inline() > 0) && c.can_post();
        let resp = ui.add_enabled(ok, egui::Button::new("Submit drafts"));
        let resp = if c.can_post() {
            resp.on_disabled_hover_text("No drafts to post")
        } else {
            resp.on_disabled_hover_text("Posting needs a signed-in account")
        };
        if resp.clicked() {
            acts.push(Act::Submit);
        }
        if c.inline_enabled() {
            ui.label(
                RichText::new(" experimental ")
                    .size(11.0)
                    .color(Color32::BLACK)
                    .background_color(AMBER),
            )
            .on_hover_text(inline_experiment_tip(c.forge));
        }
        if let Some(p) = c.progress() {
            ui.spinner();
            ui.label(p);
            let label = if c.stopping() {
                "Stopping..."
            } else {
                "Stop after this comment"
            };
            if ui
                .add_enabled(!c.stopping(), egui::Button::new(label).small())
                .on_hover_text("The comment being posted finishes; the rest stay drafts")
                .clicked()
            {
                acts.push(Act::StopBatch);
            }
        }
        let held = c.held_inline();
        if held > 0 {
            ui.colored_label(
                AMBER,
                format!(
                    "{held} inline draft(s) held: the pull request changed, Reload to post them."
                ),
            );
        }
        if let Some(e) = &c.error {
            ui.colored_label(RED, e);
            if ui.small_button("Dismiss").clicked() {
                acts.push(Act::DismissError);
            }
        } else if let Some(n) = &c.notice {
            ui.colored_label(AMBER, n);
            if ui.small_button("Dismiss").clicked() {
                acts.push(Act::DismissError);
            }
        }
    });
    if c.drafts.is_empty() {
        ui.add_space(2.0);
        return;
    }
    egui::CollapsingHeader::new("Saved drafts")
        .id_salt("review-drafts-list")
        .default_open(true)
        .show_unindented(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("review-drafts-scroll")
                .max_height(96.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for d in &c.drafts {
                        draft_row(ui, c, d, acts);
                    }
                });
        });
}

pub(super) fn draft_row(ui: &mut Ui, c: &Composer, d: &Draft, acts: &mut Vec<Act>) {
    let state = c.state(d.id);
    let editable = !c.running() && state != DraftState::Posting;
    ui.horizontal(|ui| {
        ui.label(RichText::new(kind_label(d.kind)).color(DRAFT).strong());
        if let Some(t) = d.comment_type {
            super::super::types_ui::type_label(ui, t);
        }
        if ui
            .link(RichText::new(describe(d)).monospace())
            .on_hover_text("Show it")
            .clicked()
        {
            acts.push(Act::Reveal(d.id));
        }
        ui.label(RichText::new(preview(&d.text)).color(QUIET));
        if state == DraftState::Posting {
            ui.colored_label(AMBER, "Posting...");
        }
        if let Some((note, color)) = state.note(d) {
            ui.colored_label(color, note);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(editable, egui::Button::new("Delete").small())
                .clicked()
            {
                acts.push(Act::Delete(d.id));
            }
            if state != DraftState::PostedNotRemoved
                && ui
                    .add_enabled(editable, egui::Button::new("Edit").small())
                    .clicked()
            {
                acts.push(Act::Reveal(d.id));
                acts.push(Act::Edit(d.id));
            }
            let off = d.kind == DraftKind::Inline && !c.inline_enabled();
            if (off || matches!(state, DraftState::Blocked(_) | DraftState::Outdated))
                && d.kind == DraftKind::Inline
                && ui
                    .add_enabled(editable, egui::Button::new(AS_FILE).small())
                    .clicked()
            {
                acts.push(Act::AsFile(d.id));
            }
            if off
                && ui
                    .add_enabled(editable, egui::Button::new(AS_PR).small())
                    .clicked()
            {
                acts.push(Act::AsPr(d.id));
            }
            if let DraftState::Unconfirmed { checked, .. } = state {
                if ui
                    .add_enabled(editable && checked, egui::Button::new(POST_AGAIN).small())
                    .on_disabled_hover_text("Check comments first")
                    .clicked()
                {
                    acts.push(Act::Rearm(d.id));
                }
                if ui
                    .add_enabled(editable, egui::Button::new(CHECK_COMMENTS).small())
                    .on_hover_text("Fetch the comments again to see whether it was posted")
                    .clicked()
                {
                    acts.push(Act::CheckComments(d.id));
                }
            }
        });
    });
}

/// "Post N comments to Bitbucket?" with every listed draft; nothing posts without it.
pub fn confirm_dialog(ctx: &egui::Context, c: &Composer, acts: &mut Vec<Act>) {
    let Some(ids) = &c.confirm else { return };
    let n = ids.len();
    let resp = egui::Modal::new(Id::unique("review-submit-confirm")).show(ctx, |ui| {
        ui.set_max_width(720.0);
        let noun = if n == 1 { "comment" } else { "comments" };
        ui.heading(format!("Post {n} {noun} to Bitbucket?"));
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                for d in ids.iter().filter_map(|id| c.draft(*id)) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(kind_label(d.kind)).color(DRAFT).strong());
                        if let Some(t) = d.comment_type {
                            super::super::types_ui::type_label(ui, t);
                        }
                        ui.label(RichText::new(describe(d)).monospace());
                        ui.label(RichText::new(preview(&d.text)).color(QUIET));
                    });
                    if d.kind == DraftKind::Inline && !INLINE_VERIFIED && c.forge != Forge::GitHub {
                        ui.label(
                            RichText::new(INLINE_UNVERIFIED_NOTE)
                                .color(AMBER)
                                .size(12.0),
                        );
                    }
                    ui.add_space(2.0);
                }
            });
        let held = c.held_inline();
        if held > 0 {
            ui.colored_label(
                AMBER,
                format!("{held} inline draft(s) are held until Reload: the pull request changed."),
            );
        }
        let refused = c.refused_inline();
        if refused > 0 {
            ui.colored_label(AMBER, inline_disabled_message(refused));
        }
        ui.label(
            RichText::new(
                "Comments are posted one at a time; posting stops at the first failure and \
                 unposted drafts stay saved.",
            )
            .color(QUIET),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(format!("Post {n} {noun}")).clicked() {
                acts.push(Act::Confirm);
            }
            if ui.button("Cancel").clicked() {
                acts.push(Act::CancelSubmit);
            }
        });
    });
    if resp.should_close() && !acts.contains(&Act::Confirm) {
        acts.push(Act::CancelSubmit);
    }
}

/// "Thread resolved. Undo" for `UNDO_FOR`.
pub fn toast(ctx: &egui::Context, c: &Composer, acts: &mut Vec<Act>) {
    let Some(t) = &c.toast else { return };
    let left = UNDO_FOR.saturating_sub(t.at.elapsed());
    ctx.request_repaint_after(left.min(Duration::from_millis(500)));
    egui::Area::new(Id::unique("review-resolve-toast"))
        .anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, -70.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let what = if t.resolved {
                        "Thread resolved."
                    } else {
                        "Thread unresolved."
                    };
                    ui.label(what);
                    if c.is_resolving(&t.root_id) {
                        ui.spinner();
                    }
                    if t.can_undo
                        && ui
                            .add_enabled(!c.is_resolving(&t.root_id), egui::Button::new("Undo"))
                            .clicked()
                    {
                        acts.push(Act::Undo);
                    }
                });
            });
        });
}
