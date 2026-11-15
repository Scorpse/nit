//! The minimap and small row/line helpers of the diff.

use super::*;

pub(super) fn tick_color(k: TickKind) -> Color32 {
    match k {
        TickKind::Unresolved => AMBER,
        TickKind::Resolved => GREY,
        TickKind::Draft => DRAFT_BLUE,
    }
}

/// The comment marker strip: the viewport as a faint box, one tick per thread / draft
/// row. Returns the row of a clicked tick.
pub(super) fn minimap(
    ui: &mut Ui,
    strip: Rect,
    ticks: &[Tick],
    scroll_y: f32,
    view_h: f32,
    total: f32,
) -> Option<usize> {
    let resp = ui.interact(strip, ui.scope_id().with("diff-minimap"), Sense::click());
    let p = ui.painter_at(strip);
    p.rect_filled(strip, 2.0, Color32::from_black_alpha(50));
    let h = strip.height().max(1.0);
    // content shorter than the viewport: rows and ticks line up
    let used = if total > 0.0 {
        h * (total / total.max(view_h)).min(1.0)
    } else {
        h
    };
    if total > 0.0 {
        let y0 = strip.top() + (scroll_y / total).clamp(0.0, 1.0) * used;
        let y1 = strip.top() + ((scroll_y + view_h) / total).clamp(0.0, 1.0) * used;
        p.rect_filled(
            Rect::from_min_max(Pos2::new(strip.left(), y0), Pos2::new(strip.right(), y1)),
            2.0,
            Color32::from_white_alpha(14),
        );
    }
    for t in ticks {
        let y = strip.top() + t.frac * used;
        let r = Rect::from_min_size(
            Pos2::new(strip.left() + 1.0, y - 1.5),
            vec2(strip.width() - 2.0, 3.0),
        );
        p.rect_filled(r, 1.0, tick_color(t.kind));
    }
    let tol = 5.0 / h;
    let at = |pos: Pos2| tick_at(ticks, (pos.y - strip.top()) / used.max(1.0), tol);
    if resp.hover_pos().and_then(at).is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        return resp.interact_pointer_pos().and_then(at);
    }
    None
}

/// Lines on screen: unified rows, or split lines.
pub(super) fn display_len(v: &FileView, split: bool) -> usize {
    if split {
        v.split().lines.len()
    } else {
        v.rows.len()
    }
}

/// The screen line of unified row `row`.
pub(super) fn line_of(v: &FileView, split: bool, row: usize) -> usize {
    if split {
        v.split().line_of(row)
    } else {
        row
    }
}

/// The unified row shown on `line` on `side` (the other side's on padding).
pub(super) fn row_on(v: &FileView, split: bool, line: usize, side: SplitSide) -> usize {
    if !split {
        return line;
    }
    let l = v.split().lines[line];
    l.side(side).or(l.side(side.other())).unwrap_or(0)
}

/// The cursor on `line`, keeping the side it prefers.
pub(super) fn cursor_on(v: &FileView, split: bool, line: usize, side: SplitSide) -> DiffCursor {
    DiffCursor {
        row: row_on(v, split, line, side),
        side,
    }
}

/// The line tops of the shown layout.
pub(super) fn shown_offsets(v: &mut FileView, split: bool) -> &Offsets {
    if split {
        v.split_offsets()
    } else {
        &v.offsets
    }
}

/// `{N}G`: the first row showing the new file's line N or a later one, else the last row.
pub(super) fn goto_line(rows: &[prcore::diff::Row], n: u32) -> usize {
    rows.iter()
        .position(|r| r.new_line.is_some_and(|l| l >= n))
        .unwrap_or(rows.len().saturating_sub(1))
}

/// The next (or previous) hunk header after (before) row `from`.
pub(super) fn next_hunk(rows: &[prcore::diff::Row], from: usize, forward: bool) -> Option<usize> {
    let header = |i: &usize| rows[*i].kind == RowKind::HunkHeader;
    if forward {
        (from + 1..rows.len()).find(header)
    } else {
        (0..from.min(rows.len())).rev().find(header)
    }
}

/// The scroll offset that brings a row at `top` into a viewport at `scroll` of height `h`,
/// with a margin of two rows; None when it is already in view (no jitter).
pub(super) fn reveal_y(top: f32, scroll: f32, h: f32) -> Option<f32> {
    if h <= 0.0 {
        return None;
    }
    let margin = (2.0 * ROW_H).min(h / 4.0);
    if top < scroll + margin {
        Some(top - margin)
    } else if top + ROW_H > scroll + h - margin {
        Some(top + ROW_H + margin - h)
    } else {
        None
    }
}

/// What `c` comments on: the hovered row, else the last selected row; the new side when
/// the row has a new line (added and context lines), the old side for deleted lines.
pub(super) fn c_key_target(
    rows: &[prcore::diff::Row],
    path: &str,
    hovered: Option<usize>,
    last_selected: Option<usize>,
) -> Option<Target> {
    let r = rows.get(hovered.or(last_selected)?)?;
    let (side, line) = match r.kind {
        RowKind::Add | RowKind::Context => (Side::New, r.new_line?),
        RowKind::Del => (Side::Old, r.old_line?),
        RowKind::HunkHeader | RowKind::NoNewline => return None,
    };
    Some(Target::Inline {
        path: path.to_string(),
        side,
        line,
        start: None,
    })
}

/// The line-number gutter of one row: hovering shows "+", a click opens the composer on
/// that line. Added lines are new side, deleted lines old side; on context lines the
/// clicked number column decides and Alt picks the other side. In the split view each side
/// has its own gutter: `forced` is that side.
#[allow(clippy::too_many_arguments)]
pub(super) fn gutter_comment(
    ui: &mut Ui,
    painter: &egui::Painter,
    gut: Rect,
    num_w: f32,
    i: usize,
    r: &prcore::diff::Row,
    path: &str,
    dr: &mut Drafting<'_>,
    forced: Option<Side>,
) {
    if !matches!(r.kind, RowKind::Context | RowKind::Add | RowKind::Del) {
        return;
    }
    let id = match forced {
        None => ui.scope_id().with(("gutter", i)),
        Some(s) => ui.scope_id().with(("gutter", i, s == Side::Old)),
    };
    let resp = ui
        .interact(gut, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.hovered() && !dr.composer.running() {
        let plus = Rect::from_min_size(gut.min + vec2(1.0, 2.0), vec2(14.0, 14.0));
        painter.rect_filled(plus, 3.0, DRAFT_BLUE);
        painter.text(
            plus.center(),
            Align2::CENTER_CENTER,
            "+",
            FontId::monospace(12.0),
            Color32::WHITE,
        );
    }
    let resp = if r.kind == RowKind::Context && forced.is_none() {
        resp.on_hover_text("Comment on this line (Alt-click: the other side)")
    } else {
        resp.on_hover_text("Comment on this line")
    };
    if !resp.clicked() {
        return;
    }
    let alt = ui.input(|inp| inp.modifiers.alt);
    let old_col = resp
        .interact_pointer_pos()
        .is_some_and(|p| p.x < gut.left() + num_w);
    let side = match (forced, r.kind) {
        (Some(s), _) => s,
        (None, RowKind::Add) => Side::New,
        (None, RowKind::Del) => Side::Old,
        _ if old_col != alt => Side::Old,
        _ => Side::New,
    };
    let line = match side {
        Side::New => r.new_line,
        Side::Old => r.old_line,
    };
    if let Some(line) = line {
        dr.acts.push(Act::Open(Target::Inline {
            path: path.to_string(),
            side,
            line,
            start: None,
        }));
    }
}
