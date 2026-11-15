//! The selected file's diff pane: scroll area, search bar, ghost files.

use super::*;

pub(super) fn draw_diff_pane(
    ui: &mut Ui,
    s: &mut ReviewScreen,
    toggles: &mut Vec<CardToggle>,
    acts: &mut Vec<Act>,
) {
    if let Some(g) = s.files.as_ref().and_then(|l| l.selected_ghost()) {
        draw_ghost_pane(ui, s, g, toggles, acts);
        return;
    }
    let Some(f) = s.selected_file().cloned() else {
        if s.files.is_some() {
            ui.label(RichText::new("Select a file").weak());
        }
        return;
    };
    // the split view needs room for two columns of code: otherwise unified, with a hint
    let fits = split_fits(
        ui.ctx().content_rect().width(),
        ui.available_width() - MINIMAP_W - MINIMAP_GAP,
    );
    let split = s.layout == DiffLayout::Split && fits;
    // sticky file header
    let viewed_now = s
        .files
        .as_ref()
        .and_then(|l| l.selected().map(|i| (i, l.is_viewed(i))));
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        chip(ui.painter(), rect.left(), rect.center().y, f.status);
        let title = match &f.old_path {
            Some(old) => format!("{old} -> {}", f.path),
            None => f.path.clone(),
        };
        ui.label(RichText::new(title).monospace().strong());
        if !f.counts_known {
            ui.label(RichText::new(COUNTS_PENDING).color(GREY))
                .on_hover_text("Counting changed lines");
        } else if !f.binary {
            ui.label(RichText::new(format!("+{}", f.additions)).color(GREEN));
            ui.label(RichText::new(format!("-{}", f.deletions)).color(RED));
        }
        if s.visual.is_some() {
            ui.label(RichText::new("VISUAL").monospace().color(AMBER))
                .on_hover_text("Rows from where v was pressed to the cursor; Esc or v ends it");
        }
        if let Some(p) = s.keys.pending() {
            ui.label(RichText::new(p).monospace().color(GREY));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let viewed_key = if s.vim { "r" } else { "v" };
            if let Some((i, mut v)) = viewed_now {
                if ui
                    .checkbox(&mut v, "Viewed")
                    .on_hover_text(viewed_key)
                    .changed()
                {
                    s.toggle_viewed(i);
                }
            }
            if !s.composer.read_only
                && ui
                    .add_enabled(!s.composer.running(), egui::Button::new("Comment on file"))
                    .on_hover_text("A comment on the whole file. Click a line number for a line.")
                    .clicked()
            {
                acts.push(Act::Open(Target::File(f.path.clone())));
            }
            ui.separator();
            // right to left: Split is drawn first
            let mut lay = s.layout;
            ui.selectable_value(&mut lay, DiffLayout::Split, "Split")
                .on_hover_text("Old and new file side by side (Alt+S)");
            ui.selectable_value(&mut lay, DiffLayout::Unified, "Unified")
                .on_hover_text("Old and new lines in one column (Alt+S)");
            s.set_layout(lay);
            if s.layout == DiffLayout::Split && !fits {
                ui.label(RichText::new("Too narrow for split").color(AMBER))
                    .on_hover_text(
                        "The split view needs a window at least 900 px wide and a diff pane of \
                         600 px: shown unified until there is room",
                    );
            }
        });
    });
    ui.separator();
    if s.search.open {
        search_bar(ui, s);
    }

    let sections_h = (ui.available_height() * 0.3).max(80.0);
    let file_level: Vec<usize> = s
        .comments
        .for_path(&f.path)
        .iter()
        .copied()
        .filter(|&i| matches!(scope(&s.comments.threads[i]), Scope::File(_)))
        .collect();
    let modal = s.modal_open();
    let mut dr = Drafting {
        composer: &mut s.composer,
        acts,
        modal,
    };
    if !file_level.is_empty() {
        let p = f.path.as_str();
        thread_section(
            ui,
            false,
            p,
            &s.comments,
            &file_level,
            sections_h,
            toggles,
            &mut dr,
        );
        ui.separator();
    }
    file_drafts_section(ui, &f.path, sections_h, &mut dr);

    if f.binary {
        // no rows to attach to: line and old-path threads are all outdated here
        let outdated: Vec<usize> = s
            .comments
            .for_path(&f.path)
            .iter()
            .chain(s.comments.on_old_path(&f.path))
            .copied()
            .filter(|&i| !matches!(scope(&s.comments.threads[i]), Scope::File(_)))
            .collect();
        if !outdated.is_empty() {
            let p = f.path.as_str();
            thread_section(
                ui,
                true,
                p,
                &s.comments,
                &outdated,
                sections_h,
                toggles,
                &mut dr,
            );
        }
        s.pending_reveal = None;
        placeholder(ui, "Binary file, not shown.");
        return;
    }
    if let Some(err) = s.patch_errors.get(&f.path).cloned() {
        ui.add_space(8.0);
        ui.colored_label(RED, err);
        if ui.button("Retry").clicked() {
            // `retry_patch`, by field (the composer is borrowed)
            s.patch_errors.remove(&f.path);
            s.requests.push(f.path.clone());
        }
        return;
    }
    let mut restore = s.restore_scroll.take();
    let mut scroll_now = restore.unwrap_or_else(|| s.diff.scroll(&f.path));
    let was_split = s.split_shown;
    let Some(view) = s.diff.view_mut(&f.path) else {
        s.restore_scroll = restore;
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Loading patch...");
        });
        return;
    };
    let relayout = split != was_split;
    if relayout && !view.rows.is_empty() {
        // unified <-> split: the row at the top stays at the top; the selection's rows and
        // side belong to the other layout
        let o = shown_offsets(view, was_split);
        let l = o.row_at(scroll_now.y).unwrap_or(0);
        let row = if was_split {
            view.split().lines[l].any()
        } else {
            l
        };
        let line = line_of(view, split, row);
        scroll_now.y = shown_offsets(view, split).top(line);
        restore = Some(scroll_now);
        view.sel.clear();
        view.sel.side = None;
        s.visual = None;
    }
    s.split_shown = split;
    // threads placed and row heights set only when something changed
    let geo = DiffGeometry::new(ui);
    let composer: &Composer = dr.composer;
    let (threads, path) = (&s.comments.threads, f.path.as_str());
    // the comments layout keeps the top row of the unified offsets; the split view keeps
    // its own line
    let split_anchor = split
        .then(|| view.split_offsets().anchor_at(scroll_now.y))
        .flatten();
    let re = s.comments.layout_with(
        view,
        geo.card_w(ui.available_width() - MINIMAP_W - MINIMAP_GAP),
        scroll_now.y,
        &mut measure_with(ui.painter()),
        composer.version(),
        &mut |rows, placement, width, measure| {
            composer.row_extras(path, rows, placement, threads, width, measure)
        },
    );
    if re.changed {
        ui.ctx().request_repaint();
    }
    if let Some(y) = re.scroll_y {
        // heights above the viewport changed: keep the same top row in view
        let y = match split_anchor {
            Some(a) => view.split_offsets().y_for(a),
            None if split => scroll_now.y,
            None => y,
        };
        restore = Some(Vec2::new(scroll_now.x, y));
    }
    if let Some(root) = s.pending_reveal.take() {
        // a Conversation click: the thread's row, or the top (its outdated section)
        let row = view
            .comments
            .placement
            .inline
            .iter()
            .find(|(_, ids)| ids.iter().any(|&t| threads[t].root.id == root))
            .map(|&(r, _)| r);
        let y = row.map_or(0.0, |r| {
            let line = line_of(view, split, r);
            (shown_offsets(view, split).top(line) - 2.0 * ROW_H).max(0.0)
        });
        restore = Some(Vec2::new(scroll_now.x, y));
    }
    let outdated = view.comments.placement.outdated.clone();
    if !outdated.is_empty() {
        let p = f.path.as_str();
        thread_section(
            ui,
            true,
            p,
            &s.comments,
            &outdated,
            sections_h,
            toggles,
            &mut dr,
        );
        ui.separator();
    }
    unplaced_drafts_section(ui, &f.path, sections_h, &mut dr);
    if view.rows.is_empty() {
        // a binary file looks like this too until numstat flags it
        let text = if f.counts_known {
            "No content changes (rename, mode change or empty file)."
        } else {
            "No text changes to show."
        };
        placeholder(ui, text);
        return;
    }
    if view.plain {
        let text = if f.counts_known {
            format!(
                "Large file ({} changed lines): shown without highlighting.",
                f.additions + f.deletions
            )
        } else {
            "Large file: shown without highlighting.".to_string()
        };
        ui.label(RichText::new(text).color(AMBER));
    }
    // comment markers: one tick per thread / draft row, O(threads + drafts)
    let ticks = {
        let threads = &s.comments.threads;
        let mut marks: Vec<(usize, TickKind)> = view
            .comments
            .placement
            .inline
            .iter()
            .flat_map(|(r, ids)| {
                ids.iter().map(move |&t| {
                    let kind = if is_open(&threads[t]) {
                        TickKind::Unresolved
                    } else {
                        TickKind::Resolved
                    };
                    (*r, kind)
                })
            })
            .collect();
        if let Some(p) = dr.composer.plan_for(&f.path) {
            for (r, slots) in &p.rows {
                if slots
                    .iter()
                    .any(|x| matches!(x, Slot::Draft(_) | Slot::Editor))
                {
                    marks.push((*r, TickKind::Draft));
                }
            }
        }
        if split {
            for m in &mut marks {
                m.0 = view.split().line_of(m.0);
            }
        }
        minimap_ticks(shown_offsets(view, split), marks)
    };
    let full = ui.available_rect_before_wrap();
    let strip = Rect::from_min_max(Pos2::new(full.right() - MINIMAP_W, full.top()), full.max);
    let diff_rect = Rect::from_min_max(
        full.min,
        Pos2::new(strip.left() - MINIMAP_GAP, full.bottom()),
    );
    let search = (s.search.open && !s.search.query.is_empty()).then_some(s.search.query.as_str());
    let opts = RowOpts {
        split,
        search,
        vim: s.vim,
    };
    let offset = ui
        .scope_builder(egui::UiBuilder::new().max_rect(diff_rect), |ui| {
            draw_rows(ui, view, restore, &geo, &s.comments, toggles, &mut dr, opts)
        })
        .inner;
    let offs = shown_offsets(view, split);
    let total = offs.total();
    if let Some(line) = minimap(ui, strip, &ticks, offset.y, diff_rect.height(), total) {
        let y = (offs.top(line) - 2.0 * ROW_H).max(0.0);
        s.restore_scroll = Some(Vec2::new(offset.x, y));
        ui.ctx().request_repaint();
    }
    let top = offs.row_at(offset.y).unwrap_or(0);
    // the service colours from the first visible unified row
    let top = if split {
        view.split().lines.get(top).map_or(0, |l| l.any())
    } else {
        top
    };
    s.visible_now = Some((f.path.clone(), top));
    s.diff_viewport = (offset.y, diff_rect.height());
    if relayout {
        s.diff.forget_other_scrolls(&f.path);
    }
    s.diff.set_scroll(&f.path, offset);
}

/// The diff's search bar (`/`): Enter goes to the next match, Esc closes it.
pub(super) fn search_bar(ui: &mut Ui, s: &mut ReviewScreen) {
    let (mut go, mut close) = (false, false);
    ui.horizontal(|ui| {
        ui.label(RichText::new("/").monospace().strong());
        let resp = ui.add(
            egui::TextEdit::singleline(&mut s.search.query)
                .id(egui::Id::unique(SEARCH_ID))
                .desired_width(280.0)
                .hint_text("Search in this diff"),
        );
        if std::mem::take(&mut s.search.focus) {
            resp.request_focus();
        }
        if resp.lost_focus() {
            go = ui.input(|i| i.key_pressed(Key::Enter));
            close = ui.input(|i| i.key_pressed(Key::Escape));
        }
        let path = s.selected_path().unwrap_or_default();
        let counted = s
            .search
            .for_key
            .as_ref()
            .is_some_and(|(p, q)| p == path && *q == s.search.query);
        let hint = if s.search.query.is_empty() {
            "Enter: first match, n / N: next / previous, Esc: close".to_string()
        } else if counted {
            match s.search.matches.len() {
                0 => "No match".to_string(),
                1 => "1 matching row".to_string(),
                n => format!("{n} matching rows (n / N)"),
            }
        } else {
            "Enter: find".to_string()
        };
        ui.label(RichText::new(hint).color(GREY));
        if ui.small_button("Close").on_hover_text("Esc").clicked() {
            close = true;
        }
    });
    if go {
        s.search_step(true, 1);
    }
    if close {
        s.search.open = false;
    }
}

/// The threads of "not in diff" entry `g`: no diff, every card marked outdated. Replying
/// and resolving still work (a reply needs no line anchor).
pub(super) fn draw_ghost_pane(
    ui: &mut Ui,
    s: &mut ReviewScreen,
    g: usize,
    toggles: &mut Vec<CardToggle>,
    acts: &mut Vec<Act>,
) {
    let Some(path) = s.files.as_ref().map(|l| l.ghosts()[g].path.clone()) else {
        return;
    };
    s.pending_reveal = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&path).monospace().strong().color(GREY));
        ui.label(RichText::new("not in diff").color(GREY).small());
    });
    ui.label(
        RichText::new(
            "This file is not in the pull request's current diff: a later push removed it or \
             reverted its changes. Its comments are outdated; you can still read, reply to \
             and resolve them.",
        )
        .color(AMBER),
    );
    ui.separator();
    let ids = s.comments.on_ghost(&path).to_vec();
    let modal = s.modal_open();
    let mut dr = Drafting {
        composer: &mut s.composer,
        acts,
        modal,
    };
    let c = &s.comments;
    egui::ScrollArea::vertical()
        .id_salt(("ghost-threads", &path))
        .auto_shrink(false)
        .show(ui, |ui| {
            for i in ids {
                let note = reason_note(&c.threads[i], OutdatedReason::FileNotInDiff);
                card_widget(ui, c, i, Some(note), toggles, &mut dr);
            }
        });
}
