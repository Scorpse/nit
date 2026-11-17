//! The conversation tab and thread/draft cards in the central pane.

use super::*;

/// Tabs over the central pane, then the selected tab. Card clicks toggle afterwards.
pub(super) fn draw_center(ui: &mut Ui, s: &mut ReviewScreen, acts: &mut Vec<Act>) {
    s.center_rect = Some(ui.max_rect());
    ui.horizontal(|ui| {
        ui.selectable_value(&mut s.tab, Tab::Diff, "Diff");
        let n = s.comments.pr_level.len();
        ui.selectable_value(&mut s.tab, Tab::Conversation, format!("Conversation ({n})"));
        ui.selectable_value(&mut s.tab, Tab::Commits, commits_tab_label(&s.commits));
        if providers::provider::capabilities_for(s.pr.forge).tasks {
            let label = match s.tasks.open_count() {
                0 => "Tasks".to_string(),
                n => format!("Tasks ({n} open)"),
            };
            ui.selectable_value(&mut s.tab, Tab::Tasks, label);
        }
    });
    ui.separator();
    let mut toggles = Vec::new();
    match s.tab {
        Tab::Diff => draw_diff_pane(ui, s, &mut toggles, acts),
        Tab::Conversation => draw_conversation(ui, s, &mut toggles, acts),
        Tab::Commits => draw_commits(ui, s),
        Tab::Tasks => draw_tasks(ui, s),
    }
    if !toggles.is_empty() {
        // the new heights are laid out on the next frame
        ui.ctx().request_repaint();
    }
    for t in toggles {
        match t {
            CardToggle::Thread(id) => s.comments.toggle(&id),
            CardToggle::Body(id) => s.comments.toggle_full(&id),
        }
    }
}

/// Paints thread `t`'s card in `card` (laid out by `lay` at `card`'s width) with its
/// clickable title, "Show more" / "Show less" lines and Reply / Resolve buttons. `salt`
/// keeps the ids of one card apart from the same thread's card elsewhere.
#[allow(clippy::too_many_arguments)]
pub(super) fn thread_card(
    ui: &mut Ui,
    painter: &egui::Painter,
    card: Rect,
    t: &prcore::comments::Thread,
    lay: &CardLayout,
    note: Option<&str>,
    salt: &str,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) {
    let resp = ui.interact(
        title_rect(card),
        ui.scope_id().with((salt, &t.root.id)),
        Sense::click(),
    );
    paint_card(painter, card, t, lay, note, resp.hovered());
    thread_buttons(ui, card, t, dr.composer, dr.acts);
    if resp.clicked() {
        toggles.push(CardToggle::Thread(t.root.id.clone()));
    }
    for (id, r) in lay.more_rects(card) {
        let more = ui
            .interact(r, ui.scope_id().with((salt, "more", &id)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if more.clicked() {
            toggles.push(CardToggle::Body(id));
        }
    }
}

/// Composer state and the clicks collected while drawing.
pub(super) struct Drafting<'a> {
    pub(super) composer: &'a mut Composer,
    pub(super) acts: &'a mut Vec<Act>,
    /// A confirmation dialog is open (`ReviewScreen::modal_open`): no `c` shortcut.
    pub(super) modal: bool,
}

/// One thread card as a widget (sections outside the virtualized rows), with Reply and
/// Resolve on its title and its reply drafts / composer below.
pub(super) fn card_widget(
    ui: &mut Ui,
    comments: &Comments,
    i: usize,
    note: Option<String>,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) {
    let t = &comments.threads[i];
    // measured at the width it is painted at, then allocated whole: the next item starts
    // below the card's real bottom
    let w = ui.available_width().min(1000.0);
    let lay = comments.card(t, w, &mut measure_with(ui.painter()));
    let (rect, _) = ui.allocate_exact_size(vec2(w, lay.height), Sense::hover());
    let painter = ui.painter().clone();
    let note = note.as_deref();
    thread_card(
        ui,
        &painter,
        rect,
        t,
        &lay,
        note,
        "thread-card",
        toggles,
        dr,
    );
    ui.add_space(CARD_MARGIN);
    reply_widgets(ui, dr.composer, t, dr.acts);
}

/// A collapsible list of cards above the diff (file-level or outdated threads), at most
/// `max_h` tall.
#[allow(clippy::too_many_arguments)]
pub(super) fn thread_section(
    ui: &mut Ui,
    outdated: bool,
    file_path: &str,
    comments: &Comments,
    ids: &[usize],
    max_h: f32,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) {
    let (salt, title) = if outdated {
        (
            "outdated-comments",
            format!("Outdated comments ({})", ids.len()),
        )
    } else {
        ("file-comments", format!("File comments ({})", ids.len()))
    };
    egui::CollapsingHeader::new(RichText::new(title).color(AMBER).strong())
        .id_salt(salt)
        .default_open(true)
        .show_unindented(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt((salt, "scroll"))
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for &i in ids {
                        let t = &comments.threads[i];
                        let note = if outdated {
                            Some(outdated_note(t, file_path))
                        } else {
                            // a file comment Bitbucket flags keeps its place, with the badge
                            is_flagged(t).then(|| reason_note(t, OutdatedReason::Flagged))
                        };
                        card_widget(ui, comments, i, note, toggles, dr);
                    }
                });
        });
}

pub(super) fn draw_conversation(
    ui: &mut Ui,
    s: &mut ReviewScreen,
    toggles: &mut Vec<CardToggle>,
    acts: &mut Vec<Act>,
) {
    let (mut jump_path, mut jump_thread) = (None, None);
    let mut hide_outdated = s.hide_outdated;
    let modal = s.modal_open();
    let forge = s.pr.forge;
    let mut dr = Drafting {
        composer: &mut s.composer,
        acts,
        modal,
    };
    let to_end = s.conversation_to_end && !s.comments.loading;
    s.conversation_to_end &= !to_end;
    egui::ScrollArea::vertical()
        .id_salt("review-conversation")
        .auto_shrink(false)
        .show(ui, |ui| {
            let c = &s.comments;
            if c.loading && c.threads.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading comments...");
                });
                return;
            }
            ui.horizontal(|ui| {
                let name = forge.change_name();
                let title = format!("{}{} comments", name[..1].to_uppercase(), &name[1..]);
                ui.label(RichText::new(title).strong().size(15.0));
                let open = dr.composer.editor_is(&Target::Pr);
                let read_only = dr.composer.read_only;
                if ui
                    .add_enabled(
                        !dr.composer.running() && !open && !read_only,
                        egui::Button::new(
                            RichText::new(format!("Comment on {}", forge.change_noun())).strong(),
                        ),
                    )
                    .on_hover_text(format!("A comment on the whole {name}, saved as a draft"))
                    .on_disabled_hover_text(if read_only {
                        super::super::composer::READ_ONLY_TIP
                    } else {
                        "A comment is being written or posted"
                    })
                    .clicked()
                {
                    dr.acts.push(Act::Open(Target::Pr));
                }
            });
            ui.add_space(4.0);
            // the PR-level composer and drafts first, where the button is
            editor_widget(ui, dr.composer, &Target::Pr, dr.acts);
            let ids: Vec<i64> = dr
                .composer
                .of_kind(DraftKind::Pr, None)
                .map(|d| d.id)
                .collect();
            for id in ids {
                if let Some(d) = dr.composer.draft(id).cloned() {
                    draft_widget(ui, dr.composer, &d, dr.acts);
                }
            }
            if c.pr_level.is_empty() {
                ui.label(
                    RichText::new(format!("No {} comments.", forge.change_name())).color(GREY),
                );
            }
            for &i in &c.pr_level {
                card_widget(ui, c, i, None, toggles, &mut dr);
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Comments on files").strong().size(15.0));
                ui.checkbox(&mut hide_outdated, "Hide outdated")
                    .on_hover_text(
                        "Leave out threads whose file or line changed since they were made",
                    );
            });
            ui.add_space(4.0);
            // one group per path: the files of the diff, then the paths that are not in it
            let groups: Vec<(&str, bool)> = c
                .paths()
                .into_iter()
                .map(|p| (p, false))
                .chain(c.ghost_paths().into_iter().map(|p| (p, true)))
                .collect();
            if groups.is_empty() {
                ui.label(RichText::new("No file comments.").color(GREY));
            }
            let mut hidden = 0;
            for (path, ghost) in groups {
                let ids: Vec<usize> = if ghost {
                    c.on_ghost(path).to_vec()
                } else {
                    c.for_path(path)
                        .iter()
                        .chain(c.on_old_path(path))
                        .copied()
                        .collect()
                };
                let view = (!ghost).then(|| s.diff.view(path)).flatten();
                let all: Vec<(usize, Option<OutdatedReason>)> =
                    ids.iter().map(|&i| (i, c.outdated_in(i, view))).collect();
                let outdated = all.iter().filter(|(_, r)| r.is_some()).count();
                let shown: Vec<(usize, Option<OutdatedReason>)> = all
                    .into_iter()
                    .filter(|(_, r)| !(hide_outdated && r.is_some()))
                    .collect();
                hidden += ids.len() - shown.len();
                if shown.is_empty() {
                    continue;
                }
                let open = ids.iter().filter(|&&i| is_open(&c.threads[i])).count();
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let tip = if ghost {
                        "Show its comments (the file is not in this diff)"
                    } else {
                        "Show this file's diff"
                    };
                    if ui
                        .link(RichText::new(path).monospace())
                        .on_hover_text(tip)
                        .clicked()
                    {
                        jump_path = Some(path.to_string());
                    }
                    if ghost {
                        ui.label(RichText::new("not in diff").color(GREY).small());
                    }
                    let n = ids.len();
                    let plural = if n == 1 { "" } else { "s" };
                    let mut text = format!("{n} thread{plural}, {open} unresolved");
                    if outdated > 0 {
                        text.push_str(&format!(", {outdated} outdated"));
                    }
                    ui.label(RichText::new(text).color(GREY));
                });
                for (i, reason) in shown {
                    let t = &c.threads[i];
                    let target = match scope(t) {
                        Scope::Line { to: Some(n), .. } => format!("Go to line {n}"),
                        Scope::Line { from: Some(n), .. } => format!("Go to old line {n}"),
                        _ => "Go to file".into(),
                    };
                    if ui
                        .link(RichText::new(target).small())
                        .on_hover_text("Show this thread in the review")
                        .clicked()
                    {
                        jump_thread = Some(i);
                    }
                    let note = reason.map(|r| reason_note(t, r));
                    card_widget(ui, c, i, note, toggles, &mut dr);
                }
            }
            if hidden > 0 {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{hidden} outdated threads hidden")).color(GREY));
            }
            if to_end {
                ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
            }
        });
    s.hide_outdated = hide_outdated;
    if let Some(i) = jump_thread {
        s.jump_to_thread(i);
    } else if let Some(p) = jump_path {
        s.open_path(&p);
    }
}

/// Draft file comments of `path` and the file composer, above the diff.
pub(super) fn file_drafts_section(ui: &mut Ui, path: &str, max_h: f32, dr: &mut Drafting<'_>) {
    let target = Target::File(path.to_string());
    let ids: Vec<i64> = dr
        .composer
        .of_kind(DraftKind::File, Some(path))
        .map(|d| d.id)
        .collect();
    let editing = dr.composer.editor_is(&target);
    if ids.is_empty() && !editing {
        return;
    }
    egui::CollapsingHeader::new(
        RichText::new(format!("Draft file comments ({})", ids.len()))
            .color(DRAFT_BLUE)
            .strong(),
    )
    .id_salt("file-drafts")
    .default_open(true)
    .show_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("file-drafts", "scroll"))
            .max_height(max_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for id in ids {
                    if let Some(d) = dr.composer.draft(id).cloned() {
                        draft_widget(ui, dr.composer, &d, dr.acts);
                    }
                }
                editor_widget(ui, dr.composer, &target, dr.acts);
            });
    });
    ui.separator();
}

/// Inline drafts of `path` whose line is not shown in this diff (and a composer on such a
/// line), so none of them is ever hidden.
pub(super) fn unplaced_drafts_section(ui: &mut Ui, path: &str, max_h: f32, dr: &mut Drafting<'_>) {
    let (ids, editor) = match dr.composer.plan_for(path) {
        Some(p) => (p.unplaced.clone(), p.editor_unplaced),
        None => return,
    };
    if ids.is_empty() && !editor {
        return;
    }
    egui::CollapsingHeader::new(
        RichText::new(format!("Drafts on lines not in this diff ({})", ids.len()))
            .color(AMBER)
            .strong(),
    )
    .id_salt("unplaced-drafts")
    .default_open(true)
    .show_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(("unplaced-drafts", "scroll"))
            .max_height(max_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for id in ids {
                    if let Some(d) = dr.composer.draft(id).cloned() {
                        draft_widget(ui, dr.composer, &d, dr.acts);
                    }
                }
                if editor {
                    if let Some(t) = dr.composer.editor.as_ref().map(|e| e.target.clone()) {
                        editor_widget(ui, dr.composer, &t, dr.acts);
                    }
                }
            });
    });
    ui.separator();
}
