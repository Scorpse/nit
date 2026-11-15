//! Painting the diff rows, unified and split.

use super::*;

/// What the diff rows are drawn with this frame.
#[derive(Clone, Copy)]
pub(super) struct RowOpts<'a> {
    pub(super) split: bool,
    /// The search query whose matches are highlighted.
    pub(super) search: Option<&'a str>,
    /// The vim keys are on: the cursor is drawn and `c` is theirs.
    pub(super) vim: bool,
}

pub(super) const CURSOR_BG: Color32 = Color32::from_rgba_premultiplied(18, 18, 22, 18);
pub(super) const CURSOR_BAR: Color32 = Color32::from_rgb(110, 150, 220);
pub(super) const SEARCH_BG: Color32 = Color32::from_rgba_premultiplied(120, 100, 20, 150);
/// The empty side of a split line (the shorter side of a change).
pub(super) const PAD_BG: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 70);

/// Paints only the rows (and the thread cards under them) in the viewport; returns the
/// scroll offset to remember. Clicked card titles are pushed to `toggles`.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_rows(
    ui: &mut Ui,
    view: &mut FileView,
    restore: Option<Vec2>,
    geo: &DiffGeometry,
    comments: &Comments,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
    opts: RowOpts<'_>,
) -> Vec2 {
    // the selection state is taken out so painting can read the rest of the view
    let mut selection = std::mem::take(&mut view.sel);
    if opts.split {
        view.split_offsets();
    }
    let (offset, pressed) = if opts.split {
        paint_split_rows(
            ui,
            view,
            restore,
            geo,
            comments,
            toggles,
            dr,
            &mut selection,
            opts,
        )
    } else {
        paint_rows(
            ui,
            view,
            restore,
            geo,
            comments,
            toggles,
            dr,
            &mut selection,
            opts,
        )
    };
    view.sel = selection;
    if let Some((line, side)) = pressed {
        // a click on text moves the cursor there
        let side = side.unwrap_or_else(|| view.cursor.map_or(SplitSide::Right, |c| c.side));
        view.cursor = Some(cursor_on(view, opts.split, line, side));
    }
    offset
}

/// Background, +/- sign and text colour of a row kind.
pub(super) fn row_colors(kind: RowKind) -> (Color32, &'static str, Color32) {
    match kind {
        RowKind::HunkHeader => (Color32::from_rgb(30, 40, 70), "", Color32::LIGHT_BLUE),
        RowKind::Add => (Color32::from_rgb(20, 52, 30), "+", Color32::from_gray(225)),
        RowKind::Del => (Color32::from_rgb(62, 25, 25), "-", Color32::from_gray(225)),
        RowKind::Context => (Color32::TRANSPARENT, "", Color32::from_gray(200)),
        RowKind::NoNewline => (Color32::TRANSPARENT, "", GREY),
    }
}

/// One text column on screen: its clipped painter and where its columns are.
pub(super) struct TextCol {
    pub(super) clip: egui::Painter,
    /// x of display column `col0` (the first visible one).
    pub(super) left: f32,
    /// x of display column 0 (scrolled out of view when > 0).
    pub(super) left_abs: f32,
    pub(super) col0: usize,
    pub(super) ncols: usize,
    pub(super) char_w: f32,
}

/// Row `i`'s text at `y`: syntax colours, word-diff marks, search matches and the selected
/// columns `sel`, only the visible columns (same columns as plain text: no layout shift).
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_row_text(
    tc: &TextCol,
    view: &FileView,
    i: usize,
    y: f32,
    fg: Color32,
    font: &FontId,
    sel: Option<Range<usize>>,
    search: Option<&str>,
) {
    let r = &view.rows[i];
    let (char_w, col0, ncols) = (tc.char_w, tc.col0, tc.ncols);
    let cy = y + ROW_H / 2.0;
    let spans = view.spans.as_ref().map_or(&[][..], |s| s[i].as_slice());
    let words = view.word_ranges(i);
    let styled = styled_columns(
        &r.text,
        spans,
        words.as_deref().map_or(&[][..], |w| w.as_slice()),
        col0,
        ncols,
    );
    let mark_bg = match r.kind {
        RowKind::Add => Color32::from_rgb(32, 92, 50),
        _ => Color32::from_rgb(118, 42, 42),
    };
    let fill_cols = |m: &Range<usize>, color: Color32| {
        let x = tc.left + m.start as f32 * char_w;
        let rect = Rect::from_min_size(
            Pos2::new(x, y + 1.0),
            vec2(m.len() as f32 * char_w, ROW_H - 2.0),
        );
        tc.clip.rect_filled(rect, 2.0, color);
    };
    for m in &styled.marks {
        fill_cols(m, mark_bg);
    }
    if let Some(q) = search {
        // visible rows only, while the search bar is open
        let found = find_all(&r.text, q, ignores_case(q));
        if !found.is_empty() {
            for m in &styled_columns(&r.text, &[], &found, col0, ncols).marks {
                fill_cols(m, SEARCH_BG);
            }
        }
    }
    if let Some(span) = sel {
        // behind the text, only the visible columns (a minified line of 1 MB is never
        // walked to its end); a row selected through its end also marks the line break
        // with one extra column when the line ends in view
        let lim = col0 + ncols;
        let (c0, c1) = (span.start.max(col0), span.end.min(lim));
        if c0 < c1 {
            let x0 = tc.left_abs + col_to_x(&r.text, c0, char_w);
            let mut x1 = tc.left_abs + col_to_x(&r.text, c1, char_w);
            if span.end == usize::MAX && x1 < tc.left_abs + lim as f32 * char_w {
                x1 += char_w;
            }
            if x1 > x0 {
                let rect = Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + ROW_H));
                tc.clip.rect_filled(rect, 0.0, SELECTION_BG);
            }
        }
    }
    // one galley per colour run, each placed at its own display column: a glyph whose
    // natural advance differs from its cells never shifts the rest of the line
    for (col, t, color) in styled.run_columns() {
        let galley = tc.clip.layout(
            t.to_string(),
            font.clone(),
            color.unwrap_or(fg),
            f32::INFINITY,
        );
        let at = Align2::LEFT_CENTER
            .anchor_size(Pos2::new(tc.left + col as f32 * char_w, cy), galley.size())
            .min;
        tc.clip.galley(at, galley, fg);
    }
}

/// The comment threads, drafts and composer of unified row `row` in its extra height,
/// stacked from `top`; returns where the next card would start.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_cards(
    ui: &mut Ui,
    painter: &egui::Painter,
    view: &FileView,
    comments: &Comments,
    row: usize,
    mut top: f32,
    at_x: f32,
    vis: Rect,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
) -> f32 {
    let lay = &view.comments;
    // threads with their reply drafts, then inline drafts and the composer
    let planned = dr
        .composer
        .plan_for(&view.path)
        .and_then(|p| p.rows.get(&row).cloned());
    let slots = planned.unwrap_or_else(|| {
        let at = lay.placement.inline.binary_search_by_key(&row, |(r, _)| *r);
        let ids = at.map_or(&[][..], |k| &lay.placement.inline[k].1[..]);
        ids.iter().map(|&t| Slot::Thread(t)).collect()
    });
    let measure_painter = painter.clone();
    let mut measure = measure_with(&measure_painter);
    for slot in slots {
        match slot {
            Slot::Thread(t) => {
                // the same layout `Comments::layout_with` reserved the row for
                let th = &comments.threads[t];
                let card_lay = comments.card(th, lay.card_w, &mut measure);
                let h = card_lay.height;
                let card = Rect::from_min_size(Pos2::new(at_x, top), vec2(lay.card_w, h));
                if card.intersects(vis) {
                    thread_card(
                        ui,
                        painter,
                        card,
                        th,
                        &card_lay,
                        None,
                        "thread-row",
                        toggles,
                        dr,
                    );
                }
                top += h + CARD_MARGIN;
            }
            Slot::Draft(id) => {
                let Some(d) = dr.composer.draft(id) else {
                    continue;
                };
                let state = dr.composer.state(id);
                let h = draft_height(d, &state, lay.card_w, &mut measure);
                let card = Rect::from_min_size(Pos2::new(at_x, top), vec2(lay.card_w, h));
                if card.intersects(vis) {
                    draft_card(ui, painter, card, d, &state, dr.composer, dr.acts);
                }
                top += h + CARD_MARGIN;
            }
            Slot::Editor => {
                let h = editor_height();
                let card = Rect::from_min_size(Pos2::new(at_x, top), vec2(lay.card_w, h));
                if let Some(e) = dr.composer.editor.as_mut() {
                    if card.intersects(vis) {
                        editor_card(ui, painter, card, e, dr.acts);
                    }
                }
                top += h + CARD_MARGIN;
            }
        }
    }
    top
}

/// The cursor row's subtle highlight: a fill and a bar at its left edge.
pub(super) fn paint_cursor(painter: &egui::Painter, row: Rect) {
    painter.rect_filled(row, 0.0, CURSOR_BG);
    painter.rect_filled(
        Rect::from_min_size(row.min, vec2(2.0, row.height())),
        0.0,
        CURSOR_BAR,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_rows(
    ui: &mut Ui,
    view: &FileView,
    restore: Option<Vec2>,
    geo: &DiffGeometry,
    comments: &Comments,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
    selection: &mut SelectionState,
    opts: RowOpts<'_>,
) -> (Vec2, Option<(usize, Option<SplitSide>)>) {
    let font = FontId::monospace(MONO);
    let (char_w, num_w) = (geo.char_w, geo.num_w);
    let text_x = geo.text_x();
    let content_w = text_x + (view.max_cols as f32 + 2.0) * char_w;
    let total_h = view.offsets.total();
    let mut pressed = None;

    // drag moves the selection, not the scroll position
    let mut area = egui::ScrollArea::both()
        .id_salt("review-diff")
        .auto_shrink(false)
        .scroll_source(ScrollSource {
            drag: DragScroll::Never,
            mouse_wheel: true,
            scroll_bar: true,
        });
    if let Some(off) = restore {
        area = area.scroll_offset(off);
    }
    let out = area.show_viewport(ui, |ui, vp| {
        ui.set_height(total_h);
        ui.set_width(content_w.max(ui.available_width()));
        let origin = ui.max_rect().min;
        let vis = Rect::from_min_size(origin + vp.min.to_vec2(), vp.size());
        let painter = ui.painter_at(vis);
        let col0 = (vp.min.x / char_w).floor().max(0.0) as usize;
        let tc = TextCol {
            clip: painter.with_clip_rect(Rect::from_min_max(
                Pos2::new(vis.left() + text_x, vis.top()),
                vis.max,
            )),
            left: vis.left() + text_x - (vp.min.x - col0 as f32 * char_w),
            left_abs: vis.left() + text_x - vp.min.x,
            col0,
            ncols: ((vp.width() - text_x).max(0.0) / char_w).ceil() as usize + 2,
            char_w,
        };

        let g = SelGeo {
            offsets: &view.offsets,
            split: None,
            text_x,
        };
        pressed = handle_selection(
            ui, &view.rows, &g, selection, vis, vp.min.x, origin.y, char_w,
        );
        // `c` (vim keys off): comment on the row under the pointer, else on the last
        // selected row
        let ctx = ui.ctx().clone();
        let typing = ctx.egui_wants_keyboard_input();
        if !opts.vim
            && review_keys_allowed(typing, ctx.input(|i| i.modifiers), dr.modal)
            && ctx.input(|i| i.key_pressed(Key::C))
        {
            let hovered = ctx
                .input(|i| i.pointer.hover_pos())
                .filter(|p| vis.contains(*p))
                .and_then(|p| {
                    match hit_test(&view.offsets, p.x - vis.left(), p.y - origin.y, text_x) {
                        Some(Hit::Gutter(r) | Hit::Text(r)) => Some(r),
                        _ => None,
                    }
                });
            let last = selection.sel.map(|s| s.head.row);
            if let Some(t) = c_key_target(&view.rows, &view.path, hovered, last) {
                dr.acts.push(Act::Open(t));
            }
        }
        let sel_range = selection.sel.filter(|s| !s.is_empty()).map(|s| s.range());
        // rows with threads (any unresolved?) and rows with drafts, for the gutter dots;
        // O(threads + drafts), looked up per visible row
        let thread_rows: Vec<(usize, bool)> = view
            .comments
            .placement
            .inline
            .iter()
            .map(|(r, ids)| (*r, ids.iter().any(|&t| is_open(&comments.threads[t]))))
            .collect();
        let draft_rows: std::collections::HashSet<usize> = dr
            .composer
            .plan_for(&view.path)
            .map(|p| {
                p.rows
                    .iter()
                    .filter(|(_, sl)| sl.iter().any(|x| matches!(x, Slot::Draft(_))))
                    .map(|(r, _)| *r)
                    .collect()
            })
            .unwrap_or_default();
        let cursor = opts.vim.then_some(view.cursor).flatten();

        for i in view.offsets.visible_range(vp.min.y, vp.max.y) {
            let r = &view.rows[i];
            let y = origin.y + view.offsets.top(i);
            let row = Rect::from_min_size(Pos2::new(vis.left(), y), vec2(vis.width(), ROW_H));
            let cy = y + ROW_H / 2.0;
            let (bg, sign, fg) = row_colors(r.kind);
            painter.rect_filled(row, 0.0, bg);
            if cursor.is_some_and(|c| c.row == i) {
                paint_cursor(&painter, row);
            }
            // gutters stay put while the text scrolls sideways
            if r.kind != RowKind::HunkHeader {
                let gut = Rect::from_min_size(row.min, vec2(2.0 * num_w + 4.0, ROW_H));
                painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                gutter_comment(ui, &painter, gut, num_w, i, r, &view.path, dr, None);
                // a dot for threads / drafts under this row, visible with the card collapsed
                if let Ok(k) = thread_rows.binary_search_by_key(&i, |x| x.0) {
                    let c = if thread_rows[k].1 { AMBER } else { GREY };
                    painter.circle_filled(Pos2::new(gut.right() - 4.0, cy), 2.5, c);
                }
                if draft_rows.contains(&i) {
                    painter.circle_filled(Pos2::new(gut.right() - 4.0, cy + 5.0), 2.0, DRAFT_BLUE);
                }
            }
            let num = |n: Option<u32>, right: f32| {
                if let Some(n) = n {
                    painter.text(
                        Pos2::new(right, cy),
                        Align2::RIGHT_CENTER,
                        n.to_string(),
                        font.clone(),
                        DIM,
                    );
                }
            };
            num(r.old_line, vis.left() + num_w - 4.0);
            num(r.new_line, vis.left() + 2.0 * num_w - 4.0);
            if !sign.is_empty() {
                painter.text(
                    Pos2::new(vis.left() + 2.0 * num_w + 0.6 * char_w, cy),
                    Align2::LEFT_CENTER,
                    sign,
                    font.clone(),
                    fg,
                );
            }
            let span = sel_range.and_then(|(lo, hi)| row_span(r, i, lo, hi));
            paint_row_text(&tc, view, i, y, fg, &font, span, opts.search);
            // comment threads anchored on this row, in its extra height
            let extra = view.offsets.height(i) - ROW_H;
            if extra > 0.0 {
                let gut = Rect::from_min_size(
                    Pos2::new(vis.left(), y + ROW_H),
                    vec2(geo.gutter_w(), extra),
                );
                painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                let at_x = vis.left() + geo.card_x();
                let top = y + ROW_H + CARD_MARGIN;
                paint_cards(ui, &painter, view, comments, i, top, at_x, vis, toggles, dr);
            }
        }
    });
    (out.state.clamped_offset(), pressed)
}

/// Which side of the split view the gutter dot of thread `t` on row `r` goes to: the side
/// the row is on, and for a row on both sides (context) the side of the thread's anchor.
pub(super) fn thread_side(
    map: &SplitMap,
    rows: &[prcore::diff::Row],
    r: usize,
    t: &prcore::comments::Thread,
) -> SplitSide {
    match scope(t) {
        Scope::Line { to: Some(n), .. } if rows[r].new_line == Some(n) => {
            map.side_of(r, SplitSide::Right)
        }
        _ => map.side_of(r, SplitSide::Left),
    }
}

/// The split view: old file left, new file right, a change's deletions and additions line by
/// line (see `split::pair_lines`). Virtualized like the unified view: only the lines in the
/// viewport are painted, found by binary search in the split line offsets.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_split_rows(
    ui: &mut Ui,
    view: &FileView,
    restore: Option<Vec2>,
    geo: &DiffGeometry,
    comments: &Comments,
    toggles: &mut Vec<CardToggle>,
    dr: &mut Drafting<'_>,
    selection: &mut SelectionState,
    opts: RowOpts<'_>,
) -> (Vec2, Option<(usize, Option<SplitSide>)>) {
    let font = FontId::monospace(MONO);
    let (char_w, num_w) = (geo.char_w, geo.num_w);
    let map = view.split();
    let Some(offs) = view.split_offsets_ref() else {
        return (Vec2::ZERO, None);
    };
    // each side: one number column, the sign, then the text
    let text_x = num_w + 2.0 * char_w;
    let total_h = offs.total();
    let mut pressed = None;

    let mut area = egui::ScrollArea::both()
        .id_salt("review-diff")
        .auto_shrink(false)
        .scroll_source(ScrollSource {
            drag: DragScroll::Never,
            mouse_wheel: true,
            scroll_bar: true,
        });
    if let Some(off) = restore {
        area = area.scroll_offset(off);
    }
    let out = area.show_viewport(ui, |ui, vp| {
        let half = (vp.width() / 2.0).floor();
        // scrolled sideways, both sides move together until the widest line's end shows
        let content_w = half + text_x + (view.max_cols as f32 + 2.0) * char_w;
        ui.set_height(total_h);
        ui.set_width(content_w.max(ui.available_width()));
        let origin = ui.max_rect().min;
        let vis = Rect::from_min_size(origin + vp.min.to_vec2(), vp.size());
        let painter = ui.painter_at(vis);
        let col0 = (vp.min.x / char_w).floor().max(0.0) as usize;
        let side_col = |x0: f32, x1: f32| TextCol {
            clip: painter.with_clip_rect(Rect::from_min_max(
                Pos2::new(x0 + text_x, vis.top()),
                Pos2::new(x1, vis.bottom()),
            )),
            left: x0 + text_x - (vp.min.x - col0 as f32 * char_w),
            left_abs: x0 + text_x - vp.min.x,
            col0,
            ncols: ((x1 - x0 - text_x).max(0.0) / char_w).ceil() as usize + 2,
            char_w,
        };
        let mid = vis.left() + half;
        let cols = [
            (SplitSide::Left, vis.left(), side_col(vis.left(), mid - 1.0)),
            (SplitSide::Right, mid, side_col(mid, vis.right())),
        ];
        // hunk headers span both sides
        let header_col = side_col(vis.left(), vis.right());

        let g = SelGeo {
            offsets: offs,
            split: Some((map, half)),
            text_x,
        };
        pressed = handle_selection(
            ui, &view.rows, &g, selection, vis, vp.min.x, origin.y, char_w,
        );
        // `c` (vim keys off): the line and side under the pointer, else the selection's end
        let ctx = ui.ctx().clone();
        let typing = ctx.egui_wants_keyboard_input();
        if !opts.vim
            && review_keys_allowed(typing, ctx.input(|i| i.modifiers), dr.modal)
            && ctx.input(|i| i.key_pressed(Key::C))
        {
            let hovered = ctx
                .input(|i| i.pointer.hover_pos())
                .filter(|p| vis.contains(*p))
                .and_then(|p| match g.hit(p.x - vis.left(), p.y - origin.y) {
                    Some(Hit::Gutter(l) | Hit::Text(l)) => {
                        Some((l, g.side_at(p.x - vis.left()).unwrap_or_default()))
                    }
                    _ => None,
                });
            let last = selection
                .sel
                .map(|s| (s.head.row, selection.side.unwrap_or_default()));
            let target = hovered
                .or(last)
                .and_then(|(l, side)| comment_target(&view.rows, *map.lines.get(l)?, side));
            if let Some((side, line)) = target {
                dr.acts.push(Act::Open(Target::Inline {
                    path: view.path.clone(),
                    side,
                    line,
                    start: None,
                }));
            }
        }
        let sel_range = selection.sel.filter(|s| !s.is_empty()).map(|s| s.range());
        // gutter dots by (row, side); O(threads + drafts), looked up per visible row
        let mut thread_marks: Vec<(usize, SplitSide, bool)> = view
            .comments
            .placement
            .inline
            .iter()
            .flat_map(|(r, ids)| {
                ids.iter().map(move |&t| {
                    let th = &comments.threads[t];
                    (*r, thread_side(map, &view.rows, *r, th), is_open(th))
                })
            })
            .collect();
        thread_marks.sort_unstable_by_key(|&(r, s, open)| (r, s as u8, !open));
        let draft_marks: std::collections::HashSet<(usize, SplitSide)> = dr
            .composer
            .plan_for(&view.path)
            .map(|p| {
                p.rows
                    .iter()
                    .flat_map(|(r, sl)| {
                        sl.iter().filter_map(move |x| match x {
                            Slot::Draft(id) => Some((*r, *id)),
                            _ => None,
                        })
                    })
                    .map(|(r, id)| {
                        let prefer = match dr.composer.draft(id).and_then(|d| d.side) {
                            Some(Side::Old) => SplitSide::Left,
                            _ => SplitSide::Right,
                        };
                        (r, map.side_of(r, prefer))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cursor = opts
            .vim
            .then_some(view.cursor)
            .flatten()
            .map(|c| (map.line_of(c.row), map.side_of(c.row, c.side)));
        let divider = egui::Stroke::new(1.0, Color32::from_gray(60));

        for l in offs.visible_range(vp.min.y, vp.max.y) {
            let line = map.lines[l];
            let y = origin.y + offs.top(l);
            let cy = y + ROW_H / 2.0;
            let shared = line.shared();
            if shared && view.rows[line.any()].kind == RowKind::HunkHeader {
                let i = line.any();
                let (bg, _, fg) = row_colors(RowKind::HunkHeader);
                let row = Rect::from_min_size(Pos2::new(vis.left(), y), vec2(vis.width(), ROW_H));
                painter.rect_filled(row, 0.0, bg);
                if cursor.is_some_and(|c| c.0 == l) {
                    paint_cursor(&painter, row);
                }
                // a header is not split: it is selected (and copied) from either side
                let span = sel_range.and_then(|(lo, hi)| row_span(&view.rows[i], l, lo, hi));
                paint_row_text(&header_col, view, i, y, fg, &font, span, opts.search);
            } else {
                for (side, x0, tc) in &cols {
                    let (side, x0) = (*side, *x0);
                    let w = if side == SplitSide::Left {
                        half
                    } else {
                        vis.right() - x0
                    };
                    let cell = Rect::from_min_size(Pos2::new(x0, y), vec2(w, ROW_H));
                    let Some(i) = line.side(side) else {
                        painter.rect_filled(cell, 0.0, PAD_BG);
                        continue;
                    };
                    let r = &view.rows[i];
                    let (bg, sign, fg) = row_colors(r.kind);
                    painter.rect_filled(cell, 0.0, bg);
                    if cursor == Some((l, side)) {
                        paint_cursor(&painter, cell);
                    }
                    let gut = Rect::from_min_size(cell.min, vec2(num_w + 4.0, ROW_H));
                    painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                    let forced = match side {
                        SplitSide::Left => Side::Old,
                        SplitSide::Right => Side::New,
                    };
                    gutter_comment(ui, &painter, gut, num_w, i, r, &view.path, dr, Some(forced));
                    let k = thread_marks.partition_point(|m| m.0 < i);
                    if let Some(m) = thread_marks[k..]
                        .iter()
                        .take_while(|m| m.0 == i)
                        .find(|m| m.1 == side)
                    {
                        let c = if m.2 { AMBER } else { GREY };
                        painter.circle_filled(Pos2::new(gut.right() - 4.0, cy), 2.5, c);
                    }
                    if draft_marks.contains(&(i, side)) {
                        painter.circle_filled(
                            Pos2::new(gut.right() - 4.0, cy + 5.0),
                            2.0,
                            DRAFT_BLUE,
                        );
                    }
                    let n = match side {
                        SplitSide::Left => r.old_line,
                        SplitSide::Right => r.new_line,
                    };
                    if let Some(n) = n {
                        painter.text(
                            Pos2::new(x0 + num_w - 4.0, cy),
                            Align2::RIGHT_CENTER,
                            n.to_string(),
                            font.clone(),
                            DIM,
                        );
                    }
                    if !sign.is_empty() {
                        painter.text(
                            Pos2::new(x0 + num_w + 0.6 * char_w, cy),
                            Align2::LEFT_CENTER,
                            sign,
                            font.clone(),
                            fg,
                        );
                    }
                    let span = sel_range
                        .filter(|_| selection.side.unwrap_or_default() == side)
                        .and_then(|(lo, hi)| row_span(r, l, lo, hi));
                    paint_row_text(tc, view, i, y, fg, &font, span, opts.search);
                }
                painter.line_segment([Pos2::new(mid, y), Pos2::new(mid, y + ROW_H)], divider);
            }
            // the cards of both rows, full width under the line (left row's first)
            let extra = offs.height(l) - ROW_H;
            if extra > 0.0 {
                let gut = Rect::from_min_size(
                    Pos2::new(vis.left(), y + ROW_H),
                    vec2(geo.gutter_w(), extra),
                );
                painter.rect_filled(gut, 0.0, Color32::from_black_alpha(60));
                let at_x = vis.left() + geo.card_x();
                let mut top = y + ROW_H;
                let right = line.right().filter(|_| !shared);
                for i in [line.left(), right].into_iter().flatten() {
                    let own = view.offsets.height(i) - ROW_H;
                    if own > 0.0 {
                        paint_cards(
                            ui,
                            &painter,
                            view,
                            comments,
                            i,
                            top + CARD_MARGIN,
                            at_x,
                            vis,
                            toggles,
                            dr,
                        );
                        top += own;
                    }
                }
            }
        }
    });
    (out.state.clamped_offset(), pressed)
}
