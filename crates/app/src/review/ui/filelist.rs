//! The file list pane (left): tree rows, counts, chips, viewed boxes.

use super::*;

pub(super) fn status_color(s: FileStatus) -> Color32 {
    match s {
        FileStatus::Added => GREEN,
        FileStatus::Deleted => RED,
        FileStatus::Renamed | FileStatus::Copied => Color32::from_rgb(170, 130, 230),
        FileStatus::Modified | FileStatus::TypeChanged => AMBER,
    }
}

/// Status chip at `x` (left edge), vertically centred on `cy`; returns its right edge.
pub(super) fn chip(p: &egui::Painter, x: f32, cy: f32, s: FileStatus) -> f32 {
    let rect = Rect::from_min_size(Pos2::new(x, cy - 8.0), vec2(16.0, 16.0));
    p.rect_filled(rect, 3.0, status_color(s).gamma_multiply(0.25));
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        status_letter(s),
        FontId::monospace(11.0),
        status_color(s),
    );
    rect.right()
}

pub(super) fn path_job(path: &str, size: f32, color: Color32, dim: Color32) -> LayoutJob {
    let (dir, name) = match path.rfind('/') {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    };
    let mut job = LayoutJob::default();
    let fmt = |c| TextFormat {
        font_id: FontId::proportional(size),
        color: c,
        ..Default::default()
    };
    job.append(dir, 0.0, fmt(dim));
    job.append(name, 0.0, fmt(color));
    job
}

/// Indentation per tree level, and the room of a directory's chevron.
pub(super) const TREE_INDENT: f32 = 14.0;
/// Fixed width of the "+a -d" column and of the comment marker column.
pub(super) const COUNTS_W: f32 = 64.0;
pub(super) const CHIP_W: f32 = 32.0;

/// What a click in the file panel asked for (applied after drawing).
#[derive(Default)]
pub(super) struct PanelClicks {
    pub(super) select: Option<usize>,
    pub(super) toggle: Option<usize>,
    pub(super) focus_dir: Option<usize>,
    pub(super) toggle_dir: Option<usize>,
    pub(super) dir_viewed: Option<usize>,
    pub(super) flip_generated: bool,
    pub(super) open_diff: bool,
    pub(super) view: Option<ViewMode>,
    /// A "not in diff" entry was clicked.
    pub(super) ghost: Option<usize>,
}

pub(super) fn draw_file_panel(ui: &mut Ui, s: &mut ReviewScreen) {
    // the panel remembers the width its content used; keep the default while loading
    ui.set_min_width(ui.available_width());
    ui.add_space(6.0);
    if s.reveal_next && s.list_view.1 > 0.0 {
        s.reveal_next = false;
        s.reveal_selected();
    }
    let Some(list) = s.files.as_mut() else {
        ui.label(RichText::new("Loading files...").weak());
        return;
    };
    let mut c = PanelClicks::default();
    ui.add(
        egui::TextEdit::singleline(&mut list.filter)
            .id(egui::Id::unique(FILTER_ID))
            .desired_width(f32::INFINITY)
            .hint_text("Filter files"),
    );
    ui.horizontal_wrapped(|ui| {
        for ch in ['A', 'M', 'D', 'R'] {
            let on = list.statuses.contains(&ch);
            if ui
                .selectable_label(on, RichText::new(ch.to_string()).monospace())
                .on_hover_text("Show only files with this status")
                .clicked()
            {
                if on {
                    list.statuses.remove(&ch);
                } else {
                    list.statuses.insert(ch);
                }
            }
        }
        if ui
            .selectable_label(list.commented_only, "Commented")
            .on_hover_text("Show only files with comment threads or drafts")
            .clicked()
        {
            list.commented_only = !list.commented_only;
        }
        ui.separator();
        ui.checkbox(&mut list.hide_viewed, "Hide viewed");
    });
    ui.horizontal(|ui| {
        let tree = list.view() == ViewMode::Tree;
        if ui
            .selectable_label(tree, "Tree")
            .on_hover_text("Changed files as a directory tree")
            .clicked()
        {
            c.view = Some(ViewMode::Tree);
        }
        if ui
            .selectable_label(!tree, "List")
            .on_hover_text("Changed files as a flat list")
            .clicked()
        {
            c.view = Some(ViewMode::List);
        }
        if tree {
            ui.separator();
            if ui
                .small_button("Expand all")
                .on_hover_text("Expand every directory")
                .clicked()
            {
                list.expand_all();
            }
            if ui
                .small_button("Collapse all")
                .on_hover_text("Collapse every directory")
                .clicked()
            {
                list.collapse_all();
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new(list.counter()).color(GREY));
        if let Some(e) = &s.counts_error {
            ui.label(RichText::new("line counts unavailable").color(AMBER))
                .on_hover_text(e);
        } else if !list.counts_known() {
            ui.label(RichText::new("counting lines…").color(GREY));
        }
    });
    ui.separator();

    let rows = list.rows();
    if rows.is_empty() {
        let msg = if list.len() == 0 {
            "No changed files."
        } else {
            "No files match."
        };
        ui.label(RichText::new(msg).weak());
        apply_panel_clicks(s, c);
        return;
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    let mut area = egui::ScrollArea::vertical()
        .id_salt("review-file-list")
        .auto_shrink(false);
    if let Some(y) = s.list_scroll.take() {
        area = area.vertical_scroll_offset(y);
    }
    let (selected, cursor) = (list.selected(), list.cursor());
    let conflicted = s.conflicts.paths();
    let stats = list.dir_stats();
    let list: &FileList = list;
    let out = area.show_rows(ui, LIST_ROW_H, rows.len(), |ui, range| {
        for row in &rows[range] {
            match *row {
                ListRow::Generated { count, expanded } => {
                    if generated_row(ui, count, expanded).clicked() {
                        c.flip_generated = true;
                    }
                }
                ListRow::Dir(d) => {
                    let focused = cursor == Some(Cursor::Dir(d));
                    let (r, chevron, cb) = dir_row(ui, list, d, &stats[d], focused);
                    if cb {
                        c.dir_viewed = Some(d);
                    } else if chevron || r.double_clicked() {
                        c.toggle_dir = Some(d);
                        c.focus_dir = Some(d);
                    } else if r.clicked() {
                        c.focus_dir = Some(d);
                    }
                }
                ListRow::File(i) => {
                    let focused = cursor == Some(Cursor::File(i));
                    let conflicted = conflicted.contains(list.files()[i].path.as_str());
                    let (r, cb) = file_row(ui, list, i, selected == Some(i), focused, conflicted);
                    if cb {
                        c.toggle = Some(i);
                    } else if r.double_clicked() {
                        c.select = Some(i);
                        c.open_diff = true;
                    } else if r.clicked() {
                        c.select = Some(i);
                    }
                }
                ListRow::GhostHeader { count } => ghost_header_row(ui, count),
                ListRow::Ghost(g) => {
                    let focused = cursor == Some(Cursor::Ghost(g));
                    let r = ghost_row(ui, list, g, list.selected_ghost() == Some(g), focused);
                    if r.clicked() || r.double_clicked() {
                        c.ghost = Some(g);
                        c.open_diff = true;
                    }
                }
            }
        }
    });
    s.list_view = (out.state.clamped_offset().y, out.inner_rect.height());
    apply_panel_clicks(s, c);
}

pub(super) fn apply_panel_clicks(s: &mut ReviewScreen, c: PanelClicks) {
    if let Some(v) = c.view {
        s.set_file_view(v);
    }
    if let Some(l) = s.files.as_mut() {
        if c.flip_generated {
            l.show_generated = !l.show_generated;
        }
        if let Some(d) = c.toggle_dir {
            l.toggle_dir(d);
        }
        if let Some(d) = c.focus_dir {
            l.focus_dir(d);
        }
        if let Some(g) = c.ghost {
            l.select_ghost(g);
        }
    }
    if let Some(d) = c.dir_viewed {
        s.toggle_dir_viewed(d);
    }
    if let Some(i) = c.toggle {
        s.toggle_viewed(i);
    }
    if let Some(i) = c.select {
        if s.files.as_ref().and_then(|l| l.selected()) != Some(i) {
            s.select(i);
        }
    }
    if c.open_diff {
        s.tab = Tab::Diff;
    }
}

pub(super) fn generated_row(ui: &mut Ui, count: usize, expanded: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    if resp.hovered() {
        p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let verb = if expanded { "Hide" } else { "Show" };
    p.text(
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{verb} {count} generated files"),
        FontId::proportional(13.0),
        GREY,
    );
    resp
}

/// "Not in this diff (n)": the header of the paths with outdated comments.
pub(super) fn ghost_header_row(ui: &mut Ui, count: usize) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::hover());
    let p = ui.painter_at(rect);
    p.hline(
        rect.x_range(),
        rect.top() + 1.0,
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    p.text(
        rect.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        format!("Not in this diff ({count})"),
        FontId::proportional(13.0),
        GREY,
    );
}

/// A path with comments that is not in the diff: greyed full path, a "not in diff" tag and
/// its (dimmed) comment marker.
pub(super) fn ghost_row(
    ui: &mut Ui,
    list: &FileList,
    g: usize,
    selected: bool,
    focused: bool,
) -> egui::Response {
    let ghost = &list.ghosts()[g];
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    row_bg(ui, &p, rect, selected, focused, resp.hovered());
    let cy = rect.center().y;
    let (dim, grey) = viewed_colors(selected);
    let tag = p.text(
        Pos2::new(rect.right() - 8.0, cy),
        Align2::RIGHT_CENTER,
        "not in diff",
        FontId::proportional(11.0),
        grey,
    );
    p.rect_stroke(
        tag.expand2(vec2(4.0, 1.0)),
        3.0,
        egui::Stroke::new(1.0, grey),
        egui::StrokeKind::Inside,
    );
    let right = paint_comment_chip(&p, tag.left() - 10.0, cy, &ghost.mark);
    paint_path(
        &p,
        &ghost.path,
        rect.left() + 6.0,
        cy,
        right - 8.0,
        (grey, dim),
    );
    if resp.hovered() {
        resp.on_hover_text(format!(
            "{}\nNot in this pull request's current diff (removed, or its changes reverted, \
             by a later push). Its comments are outdated.\n{}",
            ghost.path,
            ghost.mark.tooltip()
        ))
    } else {
        resp
    }
}

/// Row background: selection, keyboard focus outline, hover.
pub(super) fn row_bg(
    ui: &Ui,
    p: &egui::Painter,
    rect: Rect,
    selected: bool,
    focused: bool,
    hovered: bool,
) {
    if selected {
        p.rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
    } else if hovered {
        p.rect_filled(rect, 0.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    if focused {
        p.rect_stroke(
            rect.shrink(1.0),
            2.0,
            egui::Stroke::new(1.0, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
}

/// The viewed box (tri-state for directories) centred in `r`.
pub(super) fn paint_viewed_box(p: &egui::Painter, r: Rect, state: Tri, hovered: bool) {
    let stroke = if hovered { Color32::WHITE } else { GREY };
    p.rect_stroke(
        r,
        3.0,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    let (c, s) = (r.center(), egui::Stroke::new(2.0, Color32::WHITE));
    match state {
        Tri::Off => {}
        Tri::On => {
            p.rect_filled(r.shrink(1.0), 3.0, Color32::from_rgb(52, 90, 140));
            // a tick drawn with lines: the default fonts have no check mark glyph
            p.line_segment([c + vec2(-4.5, 0.0), c + vec2(-1.5, 3.5)], s);
            p.line_segment([c + vec2(-1.5, 3.5), c + vec2(4.5, -3.5)], s);
        }
        Tri::Mixed => {
            p.rect_filled(r.shrink(1.0), 3.0, Color32::from_rgb(40, 62, 92));
            p.line_segment([c + vec2(-4.0, 0.0), c + vec2(4.0, 0.0)], s);
        }
    }
}

/// Shown instead of "+a -d" while the line counts are not in yet.
pub(super) const COUNTS_PENDING: &str = "…";

/// "…" right-aligned at `right`; returns the left edge.
pub(super) fn paint_counts_pending(p: &egui::Painter, right: f32, cy: f32) -> f32 {
    p.text(
        Pos2::new(right, cy),
        Align2::RIGHT_CENTER,
        COUNTS_PENDING,
        FontId::proportional(12.0),
        GREY,
    )
    .left()
}

/// "+a -d" right-aligned at `right`; returns the left edge.
pub(super) fn paint_counts(p: &egui::Painter, right: f32, cy: f32, add: u64, del: u64) -> f32 {
    let d = p.text(
        Pos2::new(right, cy),
        Align2::RIGHT_CENTER,
        format!("-{del}"),
        FontId::proportional(12.0),
        RED,
    );
    p.text(
        Pos2::new(d.left() - 4.0, cy),
        Align2::RIGHT_CENTER,
        format!("+{add}"),
        FontId::proportional(12.0),
        GREEN,
    )
    .left()
}

/// The comment marker right-aligned at `right`: a speech bubble with the thread count
/// (amber: unresolved, grey: all resolved; dimmer when every thread is outdated) and a blue
/// dot for drafts. Returns the left edge (`right` when there is nothing to show).
pub(super) fn paint_comment_chip(p: &egui::Painter, right: f32, cy: f32, m: &CommentMark) -> f32 {
    let mut x = right;
    if m.drafts > 0 {
        let c = Pos2::new(x - 4.0, cy);
        p.circle_filled(c, 3.5, DRAFT_BLUE);
        x -= 10.0;
    }
    if let Some((n, open)) = m.chip() {
        let color = if open { AMBER } else { GREY };
        let color = if m.all_outdated() {
            color.gamma_multiply(0.55)
        } else {
            color
        };
        let g = p.layout_no_wrap(n.to_string(), FontId::proportional(11.0), color);
        let tx = x - g.size().x;
        p.galley(Pos2::new(tx, cy - g.size().y / 2.0), g, color);
        // the bubble: a rounded body with a tail at its lower left
        let body = Rect::from_min_size(Pos2::new(tx - 15.0, cy - 6.0), vec2(12.0, 9.0));
        p.rect_filled(body, 3.0, color);
        p.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(body.left() + 2.0, body.bottom() - 1.0),
                Pos2::new(body.left() + 6.0, body.bottom() - 1.0),
                Pos2::new(body.left() + 2.0, body.bottom() + 3.5),
            ],
            color,
            egui::Stroke::NONE,
        ));
        x = body.left();
    }
    x
}

/// `text` on one line from `x`, cut with an ellipsis before `max_x`.
pub(super) fn one_line(
    p: &egui::Painter,
    mut job: LayoutJob,
    x: f32,
    cy: f32,
    max_x: f32,
    color: Color32,
) {
    job.wrap = egui::text::TextWrapping {
        max_width: (max_x - x).max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('\u{2026}'),
    };
    let g = p.layout_job(job);
    p.galley(Pos2::new(x, cy - g.size().y / 2.0), g, color);
}

/// A path on one line from `x` to `max_x`: the file name is always shown whole (cut at its
/// end only when it alone does not fit); the dimmed directory is cut at its start
/// ("...test/") when both do not fit.
pub(super) fn paint_path(
    p: &egui::Painter,
    path: &str,
    x: f32,
    cy: f32,
    max_x: f32,
    (color, dim): (Color32, Color32),
) {
    let (dir, name) = match path.rfind('/') {
        Some(i) => path.split_at(i + 1),
        None => ("", path),
    };
    let font = FontId::proportional(13.0);
    let avail = max_x - x;
    let name_g = p.layout_no_wrap(name.to_string(), font.clone(), color);
    let room = avail - name_g.size().x;
    if dir.is_empty() || room <= 0.0 {
        one_line(p, path_job(name, 13.0, color, dim), x, cy, max_x, color);
        return;
    }
    let width = |s: String| p.layout_no_wrap(s, font.clone(), dim).size().x;
    let shown = if width(dir.to_string()) <= room {
        dir.to_string()
    } else {
        // the longest tail of the directory that fits behind an ellipsis (binary search)
        let starts: Vec<usize> = dir.char_indices().map(|(i, _)| i).collect();
        let tail = |k: usize| format!("\u{2026}{}", &dir[starts[k]..]);
        let (mut lo, mut hi) = (1, starts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if width(tail(mid)) <= room {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        if lo < starts.len() && width(tail(lo)) <= room {
            tail(lo)
        } else {
            String::new()
        }
    };
    let dir_g = p.layout_no_wrap(shown, font, dim);
    let dw = dir_g.size().x;
    p.galley(Pos2::new(x, cy - dir_g.size().y / 2.0), dir_g, dim);
    p.galley(Pos2::new(x + dw, cy - name_g.size().y / 2.0), name_g, color);
}

/// Left edge of a row's content at tree `depth`.
pub(super) fn indent_x(rect: Rect, depth: u16) -> f32 {
    rect.left() + 6.0 + f32::from(depth) * TREE_INDENT
}

/// Returns the row response and whether the viewed box was clicked.
pub(super) fn file_row(
    ui: &mut Ui,
    list: &FileList,
    i: usize,
    selected: bool,
    focused: bool,
    conflicted: bool,
) -> (egui::Response, bool) {
    let f = &list.files()[i];
    let tree = list.view() == ViewMode::Tree;
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let cb_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 16.0, rect.center().y),
        vec2(16.0, 16.0),
    );
    let cb = ui.interact(cb_rect, resp.id.with(("viewed", i)), Sense::click());
    let p = ui.painter_at(rect);
    row_bg(ui, &p, rect, selected, focused, resp.hovered());
    let cy = rect.center().y;
    let viewed = list.is_viewed(i);

    // right side, right to left: viewed box, +/- counts, comment marker
    let state = if viewed { Tri::On } else { Tri::Off };
    paint_viewed_box(&p, cb_rect, state, cb.hovered());
    // fixed columns: names never run into the counts or the box
    let counts_right = cb_rect.left() - 8.0;
    if f.binary || !f.counts_known {
        // "…": numstat has not answered yet
        p.text(
            Pos2::new(counts_right, cy),
            Align2::RIGHT_CENTER,
            if f.binary { "bin" } else { COUNTS_PENDING },
            FontId::proportional(12.0),
            GREY,
        );
    } else {
        let (a, d) = (u64::from(f.additions), u64::from(f.deletions));
        paint_counts(&p, counts_right, cy, a, d);
    }
    let mut right = counts_right - COUNTS_W;
    let mark = list.mark(i);
    if list.has_marks() {
        paint_comment_chip(&p, right, cy, &mark);
        right -= CHIP_W;
    }
    if conflicted {
        p.text(
            Pos2::new(right, cy),
            Align2::RIGHT_CENTER,
            "!",
            FontId::proportional(14.0),
            RED,
        );
        right -= 14.0;
    }

    // left side, cut before the right side
    let depth = list.depth(ListRow::File(i));
    let x0 = if tree && list.tree().file_dir[i].is_some() {
        // files line up with their directory's name, past the chevron
        indent_x(rect, depth) + TREE_INDENT
    } else {
        rect.left() + 6.0
    };
    let x = chip(&p, x0, cy, f.status) + 8.0;
    let (dim, grey) = viewed_colors(selected);
    let color = if viewed {
        grey
    } else {
        ui.visuals().text_color()
    };
    if tree && list.tree().file_dir[i].is_some() {
        // the tree shows the name only (the full path is in the tooltip)
        let job = path_job(super::super::files::file_name(&f.path), 13.0, color, dim);
        one_line(&p, job, x, cy, right - 8.0, color);
    } else {
        paint_path(&p, &f.path, x, cy, right - 8.0, (color, dim));
    }
    let resp = if resp.hovered() {
        let mut tip = match &f.old_path {
            Some(old) => format!("{old} -> {}", f.path),
            None => f.path.clone(),
        };
        if mark.any() {
            tip.push('\n');
            tip.push_str(&mark.tooltip());
        }
        if conflicted {
            tip.push_str("\nConflicts with the destination branch");
        }
        resp.on_hover_text(tip)
    } else {
        resp
    };
    (resp, cb.clicked())
}

/// Returns the row response, whether its chevron and whether its viewed box was clicked.
pub(super) fn dir_row(
    ui: &mut Ui,
    list: &FileList,
    d: usize,
    st: &super::super::files::DirStats,
    focused: bool,
) -> (egui::Response, bool, bool) {
    let node = &list.tree().dirs[d];
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    let cy = rect.center().y;
    let x = indent_x(rect, node.depth);
    let chev_rect = Rect::from_center_size(Pos2::new(x + 5.0, cy), vec2(TREE_INDENT, LIST_ROW_H));
    let chev = ui.interact(chev_rect, resp.id.with(("chevron", d)), Sense::click());
    let cb_rect = Rect::from_center_size(Pos2::new(rect.right() - 16.0, cy), vec2(16.0, 16.0));
    let cb = ui.interact(cb_rect, resp.id.with(("dir-viewed", d)), Sense::click());
    let p = ui.painter_at(rect);
    row_bg(ui, &p, rect, false, focused, resp.hovered());

    // right side: tri-state box, viewed/total, +/- counts, comment marker (descendants)
    paint_viewed_box(&p, cb_rect, st.tri(), cb.hovered());
    // the same fixed columns as the files, then "viewed/total"
    let counts_right = cb_rect.left() - 8.0;
    // directory totals may be wider than the files' column: never under the marker
    // partial totals would read as final: "…" until every file below is counted
    let counts_left = if st.counts_unknown > 0 {
        paint_counts_pending(&p, counts_right, cy)
    } else {
        paint_counts(&p, counts_right, cy, st.additions, st.deletions)
    };
    let mut right = (counts_right - COUNTS_W).min(counts_left - 4.0);
    if list.has_marks() {
        paint_comment_chip(&p, right, cy, &st.marks);
        right -= CHIP_W;
    }
    right = p
        .text(
            Pos2::new(right, cy),
            Align2::RIGHT_CENTER,
            format!("{}/{}", st.viewed, st.files),
            FontId::proportional(12.0),
            GREY,
        )
        .left();

    // chevron (painted triangle), folder glyph, name
    let open = list.dir_expanded(d);
    let cc = Pos2::new(x + 5.0, cy);
    let tri = if open {
        vec![
            cc + vec2(-4.0, -2.5),
            cc + vec2(4.0, -2.5),
            cc + vec2(0.0, 3.0),
        ]
    } else {
        vec![
            cc + vec2(-2.5, -4.0),
            cc + vec2(3.0, 0.0),
            cc + vec2(-2.5, 4.0),
        ]
    };
    let chev_color = if chev.hovered() { Color32::WHITE } else { GREY };
    p.add(egui::Shape::convex_polygon(
        tri,
        chev_color,
        egui::Stroke::NONE,
    ));
    let fx = x + TREE_INDENT;
    let folder = Color32::from_rgb(120, 140, 175);
    p.rect_filled(
        Rect::from_min_size(Pos2::new(fx, cy - 6.0), vec2(6.0, 3.0)),
        1.0,
        folder,
    );
    p.rect_filled(
        Rect::from_min_size(Pos2::new(fx, cy - 4.0), vec2(14.0, 10.0)),
        2.0,
        folder,
    );
    let color = if st.tri() == Tri::On {
        GREY
    } else {
        ui.visuals().strong_text_color()
    };
    let mut job = LayoutJob::default();
    job.append(
        &node.name,
        0.0,
        TextFormat {
            font_id: FontId::proportional(13.0),
            color,
            ..Default::default()
        },
    );
    one_line(&p, job, fx + 20.0, cy, right - 8.0, color);
    let resp = if resp.hovered() {
        let counts = if st.counts_unknown > 0 {
            "lines being counted".to_string()
        } else {
            format!("+{} -{}", st.additions, st.deletions)
        };
        let mut tip = format!(
            "{}/\n{} files, {} viewed, {counts}",
            node.path, st.files, st.viewed
        );
        if st.marks.any() {
            tip.push('\n');
            tip.push_str(&st.marks.tooltip());
        }
        resp.on_hover_text(tip)
    } else {
        resp
    };
    (resp, chev.clicked(), cb.clicked())
}
